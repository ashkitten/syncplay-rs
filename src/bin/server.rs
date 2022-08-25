use anyhow::{bail, Result};
use bytes::Bytes;
use log::{error, info};
use quinn::{
    Connecting, Connection, ConnectionError, Datagrams, Endpoint, IncomingBiStreams, RecvStream,
    SendStream, ServerConfig,
};
use rkyv::ser::Serializer;
use rkyv::{ser::serializers::BufferSerializer, AlignedBytes, Archived};
use std::{
    mem,
    sync::Arc,
    time::{Duration, Instant},
};
use syncplay_rs::{Packet, TimeSyncer};
use tokio::{sync::RwLock, time};
use tokio_stream::StreamExt;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let config = {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = cert.serialize_der().unwrap();
        let priv_key = cert.serialize_private_key_der();
        let priv_key = rustls::PrivateKey(priv_key);
        let cert_chain = vec![rustls::Certificate(cert_der)];
        ServerConfig::with_single_cert(cert_chain, priv_key).unwrap()
    };
    let addr = "0.0.0.0:8998".parse().unwrap();
    let (_endpoint, mut incoming) = Endpoint::server(config, addr)?;

    let state = Arc::new(ServerState::new());

    while let Some(connection) = incoming.next().await {
        println!("connection incoming");

        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = run_connection(Arc::clone(&state), connection).await {
                error!("connection failed: {}", e);
            }
        });
    }

    Ok(())
}

struct ServerState {
    syncer: Arc<RwLock<TimeSyncer>>,
    playback_start: Arc<RwLock<Instant>>,
}

impl ServerState {
    fn new() -> Self {
        Self {
            syncer: Arc::new(RwLock::new(TimeSyncer::new())),
            playback_start: Arc::new(RwLock::new(Instant::now())),
        }
    }
}

async fn run_connection(state: Arc<ServerState>, connection: Connecting) -> Result<()> {
    let quinn::NewConnection {
        connection,
        bi_streams,
        datagrams,
        ..
    } = connection.await.unwrap();

    let fut = handle_incoming_streams(Arc::clone(&state), bi_streams);
    tokio::spawn(async move {
        if let Err(e) = fut.await {
            error!("{}", e);
        }
    });

    let fut = handle_datagrams(Arc::clone(&state), datagrams, connection.clone());
    tokio::spawn(async move {
        if let Err(e) = fut.await {
            error!("{}", e);
        }
    });

    loop {
        let packet = Packet::PlaybackUpdate {
            timestamp: state.syncer.read().await.now(),
            elapsed: state.playback_start.read().await.elapsed(),
        };
        let buf = Box::from([0u8; { mem::size_of::<Archived<Packet>>() }]);
        let mut serializer = BufferSerializer::new(buf);
        serializer.serialize_value(&packet)?;
        connection.send_datagram(Bytes::from(serializer.into_inner()))?;
        time::sleep(Duration::from_secs(1)).await;
    }
}

async fn handle_incoming_streams(
    state: Arc<ServerState>,
    mut streams: IncomingBiStreams,
) -> Result<()> {
    while let Some(stream) = streams.next().await {
        let (send, recv) = match stream {
            Err(ConnectionError::ApplicationClosed { .. }) => {
                info!("connection closed");
                return Ok(());
            }
            Err(e) => {
                bail!(e);
            }
            Ok(s) => s,
        };

        let fut = handle_stream(Arc::clone(&state), send, recv);
        tokio::spawn(async move {
            if let Err(e) = fut.await {
                error!("stream handler failed: {}", e.to_string());
            }
        });

        let state = Arc::clone(&state);
    }

    Ok(())
}

async fn handle_stream(
    state: Arc<ServerState>,
    send: SendStream,
    mut recv: RecvStream,
) -> Result<()> {
    while let Ok(packet) = Packet::read_from(&mut recv).await {
        match packet {
            Packet::PlaybackControl {
                timestamp,
                paused,
                elapsed,
            } => {
                let latency = state.syncer.read().await.since(timestamp);
                *state.playback_start.write().await = Instant::now() - (elapsed + latency);
            }
            _ => (),
        }
    }

    Ok(())
}

async fn handle_datagrams(
    state: Arc<ServerState>,
    mut datagrams: Datagrams,
    connection: Connection,
) -> Result<()> {
    while let Some(Ok(datagram)) = datagrams.next().await {
        let mut bytes = AlignedBytes([0u8; mem::size_of::<Archived<Packet>>()]);
        bytes.copy_from_slice(&datagram);
        let packet = unsafe { rkyv::from_bytes_unchecked::<Packet>(&bytes[..]).unwrap() };
        match packet {
            Packet::TimeSyncRequest => {
                let packet = Packet::TimeSyncResponse(state.syncer.read().await.now());
                let buf = Box::from([0u8; { mem::size_of::<Archived<Packet>>() }]);
                let mut serializer = BufferSerializer::new(buf);
                serializer.serialize_value(&packet)?;
                connection.send_datagram(Bytes::from(serializer.into_inner()))?;
            }
            _ => (),
        }
    }

    Ok(())
}
