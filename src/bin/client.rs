use anyhow::{bail, Result};
use bytes::Bytes;
use log::{error, info};
use quinn::{
    ClientConfig, Connection, ConnectionError, Datagrams, Endpoint, IncomingBiStreams,
    NewConnection, RecvStream, SendStream,
};
use rkyv::{
    ser::{serializers::BufferSerializer, Serializer},
    AlignedBytes, Archived,
};
use std::{mem, sync::Arc, time::Duration};
use syncplay_rs::{Packet, TimeSyncer};
use tokio::{sync::RwLock, time};
use tokio_stream::StreamExt;

#[tokio::main]
async fn main() -> Result<()> {
    // Command::new("mpv")
    //     .arg("--input-ipc-server=/tmp/mpvsocket")
    //     .arg("--idle")
    //     .spawn()?
    //     .wait()
    //     .await?;

    let client_config = {
        let crypto = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth();

        ClientConfig::new(Arc::new(crypto))
    };
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())?;
    endpoint.set_default_client_config(client_config);

    let NewConnection {
        connection,
        bi_streams,
        datagrams,
        ..
    } = endpoint
        .connect("127.0.0.1:8998".parse()?, "localhost")?
        .await?;

    let state = Arc::new(ClientState::new());

    let fut = handle_bi_streams(Arc::clone(&state), bi_streams);
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

    let (mut send, _recv) = connection.open_bi().await?;
    Packet::Version(69).write_into(&mut send).await?;

    let packet = Packet::PlaybackControl {
        timestamp: state.syncer.read().await.now(),
        paused: false,
        elapsed: Duration::ZERO,
    };
    packet.write_into(&mut send).await?;

    {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let buf = Box::from([0u8; { mem::size_of::<Archived<Packet>>() }]);
                let mut serializer = BufferSerializer::new(buf);
                serializer
                    .serialize_value(&Packet::TimeSyncRequest)
                    .unwrap();
                state.syncer.write().await.start_sync();
                connection
                    .send_datagram(Bytes::from(serializer.into_inner()))
                    .unwrap();

                time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    loop {
        time::sleep(Duration::from_secs(1)).await;
        *state.playback_elapsed.write().await += Duration::from_secs(1);
    }
}

struct ClientState {
    syncer: Arc<RwLock<TimeSyncer>>,
    playback_elapsed: Arc<RwLock<Duration>>,
}

impl ClientState {
    fn new() -> Self {
        Self {
            syncer: Arc::new(RwLock::new(TimeSyncer::new())),
            playback_elapsed: Arc::new(RwLock::new(Duration::ZERO)),
        }
    }
}

async fn handle_bi_streams(
    state: Arc<ClientState>,
    mut bi_streams: IncomingBiStreams,
) -> Result<()> {
    while let Some(stream) = bi_streams.next().await {
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
    state: Arc<ClientState>,
    send: SendStream,
    mut recv: RecvStream,
) -> Result<()> {
    while let Ok(packet) = Packet::read_from(&mut recv).await {
        match packet {
            Packet::PlaybackControl {
                timestamp,
                paused,
                elapsed,
            } => {}
            _ => (),
        }
    }

    Ok(())
}

async fn handle_datagrams(
    state: Arc<ClientState>,
    mut datagrams: Datagrams,
    connection: Connection,
) -> Result<()> {
    while let Some(Ok(datagram)) = datagrams.next().await {
        dbg!(&datagram);
        let mut bytes = AlignedBytes([0u8; mem::size_of::<Archived<Packet>>()]);
        bytes.copy_from_slice(&datagram);
        let packet = unsafe { rkyv::from_bytes_unchecked::<Packet>(&bytes[..]).unwrap() };
        match packet {
            Packet::PlaybackUpdate { timestamp, elapsed } => {
                let latency = state.syncer.read().await.since(timestamp);
                dbg!(latency);

                let drift_ms = elapsed.as_millis() as i128
                    - (*state.playback_elapsed.read().await).as_millis() as i128
                    + latency.as_millis() as i128;
                dbg!(drift_ms);
            }
            Packet::TimeSyncResponse(resp_time) => {
                state.syncer.write().await.finish_sync(resp_time);
            }
            _ => (),
        }
    }

    Ok(())
}

/// Dummy certificate verifier that treats any certificate as valid.
/// NOTE, such verification is vulnerable to MITM attacks, but convenient for testing.
struct SkipServerVerification;

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl rustls::client::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}
