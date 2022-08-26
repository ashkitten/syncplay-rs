use ::time::Duration;
use anyhow::{bail, Result};
use bytes::Bytes;
use log::{error, info};
use mpvipc::{Event, Mpv, Property};
use quinn::{
    ClientConfig, Connection, ConnectionError, Datagrams, Endpoint, IncomingBiStreams,
    NewConnection, RecvStream, SendStream,
};
use rkyv::{
    ser::{serializers::BufferSerializer, Serializer},
    AlignedBytes, Archived,
};
use std::process::Command;
use std::{mem, sync::Arc, thread};
use syncplay_rs::{Packet, TimeSyncer};
use tokio::{sync::RwLock, time};
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

    let state = Arc::new(ClientState::new());

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

    let mpv_thread = thread::spawn(move || {
        if let Err(e) = run_mpv(state, connection) {
            error!("error in mpv thread: {}", e);
        }
    });

    #[tokio::main]
    async fn run_mpv(state: Arc<ClientState>, connection: Connection) -> Result<()> {
        Command::new("mpv")
        .arg("--input-ipc-server=/tmp/mpv.sock")
        .arg("--idle")
        .arg("--no-terminal")
        .arg("https://jelly.kity.wtf/Items/9c05b3823ac404f6f05c2e7afbebdf49/Download?api_key=18c4a6f7b0a1401c983af4d8afd53207")
        .spawn()?;

        let mut mpv = loop {
            if let Ok(mpv) = Mpv::connect("/tmp/mpv.sock") {
                break mpv;
            }
        };

        mpv.observe_property(1, "playback-time")?;

        while let Ok(event) = mpv.event_listen() {
            match event {
                Event::Seek => {
                    let time = Duration::seconds_f64(mpv.get_property::<f64>("playback-time")?);
                    *state.playback_elapsed.write().await = time;

                    let (mut send, _) = connection.open_bi().await?;
                    Packet::PlaybackControl {
                        timestamp: state.syncer.read().await.now(),
                        paused: false,
                        elapsed: time,
                    }
                    .write_into(&mut send)
                    .await?;
                }
                Event::PropertyChange { id: _, property } => match property {
                    Property::PlaybackTime(Some(time)) => {
                        *state.playback_elapsed.write().await = Duration::seconds_f64(time);
                    }
                    property => {
                        dbg!(property);
                    }
                },
                event => {
                    dbg!(event);
                }
            }
        }

        Ok(())
    }

    loop {
        if mpv_thread.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }

    Ok(())
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

async fn handle_datagrams(
    state: Arc<ClientState>,
    mut datagrams: Datagrams,
    connection: Connection,
) -> Result<()> {
    while let Some(Ok(datagram)) = datagrams.next().await {
        let mut bytes = AlignedBytes([0u8; mem::size_of::<Archived<Packet>>()]);
        bytes.copy_from_slice(&datagram);
        let packet = unsafe { rkyv::from_bytes_unchecked::<Packet>(&bytes[..]).unwrap() };
        match packet {
            Packet::PlaybackUpdate { timestamp, elapsed } => {
                let latency = state.syncer.read().await.since(timestamp);
                dbg!(latency);

                let drift = elapsed - *state.playback_elapsed.read().await + latency;
                dbg!(drift);
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
