use ::time::{Duration, Instant};
use anyhow::{bail, Result};
use bytes::Bytes;
use log::{error, info};
use quinn::{
    ClientConfig, ConnectionError, Datagrams, Endpoint, IncomingBiStreams, NewConnection,
    RecvStream, SendStream,
};
use rkyv::{
    ser::{serializers::BufferSerializer, Serializer},
    AlignedBytes, Archived,
};
use std::{mem, sync::Arc};
use syncplay_rs::mpv::{Command, Event, PropertyChange, SeekOptions};
use syncplay_rs::{mpv::Mpv, Packet, TimeSyncer};
use tokio::{
    select,
    sync::{
        mpsc::{self, Sender},
        RwLock,
    },
    time,
};
use tokio_stream::StreamExt;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

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

    let (state, mut rx) = ClientState::new();
    let state = Arc::new(state);

    let fut = handle_incoming_streams(Arc::clone(&state), bi_streams);
    tokio::spawn(async move {
        if let Err(e) = fut.await {
            error!("{}", e);
        }
    });

    let fut = handle_datagrams(Arc::clone(&state), datagrams);
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
        let connection = connection.clone();
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

                time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
    }

    let mut mpv = loop {
        if let Ok(mpv) = Mpv::spawn().await {
            break mpv;
        }
    };

    let mut events = mpv.listen_events();

    mpv.send_command(Command::Observe(1, "playback-time"))
        .await?;

    loop {
        println!("select loop");
        select! {
            event = events.recv() => {
                dbg!(&event);
                match event {
                    Ok(Event::PlaybackRestart) => {
                        info!("seeking");
                        let time = mpv
                            .send_command(Command::GetProperty("playback-time"))
                            .await?
                            .as_f64()
                            .map(Duration::seconds_f64)
                            .unwrap();
                        if let Err(e) = state.tx.send(Message::Seek(time)).await {
                            error!("error trying to seek to time {}: {}", time, e);
                        }
                    }
                    Ok(Event::PropertyChange { id, property_change }) => {
                        match property_change {
                            PropertyChange::PlaybackTime(time) => {
                                *state.playback_start.write().await =
                                    Instant::now() - Duration::seconds_f64(time);
                            }
                        }
                    }
                    Err(e) => {
                        error!("error receiving event: {}", e);
                    }
                }
            },
            message = rx.recv() => {
                dbg!(&message);
                if let Some(message) = message {
                    match message {
                        Message::Seek(elapsed) => {
                            let (mut send, _) = connection.open_bi().await?;
                            Packet::PlaybackControl {
                                timestamp: state.syncer.read().await.now(),
                                paused: false,
                                elapsed,
                            }
                            .write_into(&mut send)
                            .await?;
                        }
                        Message::Sync(elapsed) => {
                            let cmd = Command::Seek(elapsed.as_seconds_f64(), SeekOptions::Absolute);
                            if let Err(e) = mpv.send_command(cmd).await {
                                error!("{}", e);
                            };
                        }
                    }
                }
            },
        }
    }
}

struct ClientState {
    tx: Sender<Message>,
    syncer: Arc<RwLock<TimeSyncer>>,
    playback_start: Arc<RwLock<Instant>>,
}

impl ClientState {
    fn new() -> (Self, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::channel(16);
        (
            Self {
                tx,
                syncer: Arc::new(RwLock::new(TimeSyncer::new())),
                playback_start: Arc::new(RwLock::new(Instant::now())),
            },
            rx,
        )
    }
}

#[derive(Debug, Clone)]
enum Message {
    Seek(Duration),
    Sync(Duration),
}

async fn handle_incoming_streams(
    state: Arc<ClientState>,
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

async fn handle_datagrams(state: Arc<ClientState>, mut datagrams: Datagrams) -> Result<()> {
    while let Some(Ok(datagram)) = datagrams.next().await {
        let mut bytes = AlignedBytes([0u8; mem::size_of::<Archived<Packet>>()]);
        bytes.copy_from_slice(&datagram);
        let packet = unsafe { rkyv::from_bytes_unchecked::<Packet>(&bytes[..]).unwrap() };
        match packet {
            Packet::PlaybackUpdate { timestamp, elapsed } => {
                let latency = state.syncer.read().await.since(timestamp);
                println!("latency: {}", latency.as_seconds_f64());

                let client_elapsed =
                    (Instant::now() - *state.playback_start.read().await) - latency;

                let drift = elapsed - client_elapsed - latency;
                println!("drift: {}", drift.as_seconds_f64());

                if drift.abs() > Duration::seconds(5) {
                    state
                        .tx
                        .send(Message::Sync(client_elapsed - latency))
                        .await?;
                }
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
