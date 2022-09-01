use anyhow::Result;
use bytes::Bytes;
use clap::Parser;
use log::{error, info};
use quinn::{ClientConfig, Connection, Endpoint};
use rkyv::{
    ser::{serializers::BufferSerializer, Serializer},
    Archived,
};
use serde_json::Value;
use std::{mem, sync::Arc};
use syncplay_rs::{
    mpv::{Command, LoadFileOptions, Mpv, SeekOptions},
    run_connection, Packet, PlaybackState, TimeSyncer,
};
use time::{Duration, Instant};
use tokio::{select, time::MissedTickBehavior};
use tokio_stream::StreamExt;

#[derive(Parser)]
struct Args {
    server: String,
    filename: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let args = Args::parse();

    let client_config = {
        let crypto = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth();

        ClientConfig::new(Arc::new(crypto))
    };

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())?;
    endpoint.set_default_client_config(client_config);

    let connection = endpoint.connect(args.server.parse()?, "localhost")?.await?;
    let (mut stream, connection) = run_connection(connection).await;

    let mut syncer = TimeSyncer::new();
    let mut sync_interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
    sync_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut send = connection.open_uni().await?;
    Packet::Version(69).write_into(&mut send).await?;

    let mut mpv = loop {
        if let Ok(mpv) = Mpv::spawn().await {
            break mpv;
        }
    };

    let mut events = mpv.listen_events();

    mpv.send_command(Command::Observe(2, "pause")).await?;

    mpv.send_command(Command::LoadFile(args.filename, LoadFileOptions::Replace))
        .await?;

    async fn get_playback_state(mpv: &mut Mpv) -> PlaybackState {
        if let Ok(Value::Number(time)) = mpv
            .send_command(Command::GetProperty("playback-time"))
            .await
        {
            let time = Duration::seconds_f64(time.as_f64().unwrap());
            if let Ok(Value::Bool(paused)) = mpv.send_command(Command::GetProperty("pause")).await {
                if paused {
                    return PlaybackState::Paused { elapsed: time };
                } else {
                    return PlaybackState::Playing {
                        start: Instant::now() - time,
                    };
                }
            }
        }

        PlaybackState::Stopped
    }

    async fn transition_state(
        connection: &Connection,
        syncer: &TimeSyncer,
        state: &PlaybackState,
        new_state: &PlaybackState,
    ) -> Result<()> {
        let send_packet = match state {
            PlaybackState::Stopped => match new_state {
                PlaybackState::Stopped => false,
                PlaybackState::Playing { .. } => true,
                PlaybackState::Paused { .. } => true,
            },

            PlaybackState::Playing { start } => match new_state {
                PlaybackState::Stopped => true,
                PlaybackState::Playing { start: new_start } => {
                    (*start - *new_start).abs() > Duration::seconds(1)
                }
                PlaybackState::Paused { .. } => true,
            },

            PlaybackState::Paused { elapsed } => match new_state {
                PlaybackState::Stopped => true,
                PlaybackState::Playing { .. } => true,
                PlaybackState::Paused {
                    elapsed: new_elapsed,
                } => (*elapsed - *new_elapsed).abs() > Duration::seconds(1),
            },
        };

        if send_packet {
            let packet = match new_state {
                PlaybackState::Stopped => unimplemented!(),
                PlaybackState::Playing { start } => Packet::PlaybackControl {
                    timestamp: syncer.now(),
                    paused: false,
                    elapsed: start.elapsed(),
                },
                PlaybackState::Paused { elapsed } => Packet::PlaybackControl {
                    timestamp: syncer.now(),
                    paused: true,
                    elapsed: *elapsed,
                },
            };
            packet.write_into(&mut send).await?;
        }

        Ok(())
    }

    let mut playback_state = get_playback_state(&mut mpv).await;

    loop {
        select! {
            Some(packet) = stream.next() => {
                playback_state = get_playback_state(&mut mpv).await;

                match packet {
                    Packet::PlaybackControl { timestamp, paused, elapsed } => {
                        info!("PlaybackControl {{ paused: {paused}, elapsed: {elapsed} }}");
                        let latency = syncer.since(timestamp);
                        let time = elapsed - latency;

                        match playback_state {
                            PlaybackState::Stopped => {
                                while let Err(e) = mpv.send_command(Command::Seek(time.as_seconds_f64(), SeekOptions::Absolute)).await {
                                    error!("error seeking: {}", e);
                                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                                }
                            }
                            PlaybackState::Playing { start } => {
                                if (start.elapsed() - time).abs() < Duration::seconds(1) {
                                    if let Err(e) = mpv.send_command(Command::Seek(time.as_seconds_f64(), SeekOptions::Absolute)).await {
                                        error!("error seeking: {}", e);
                                    }
                                }
                            }
                            PlaybackState::Paused { .. } => {
                                if let Err(e) = mpv.send_command(Command::Seek(time.as_seconds_f64(), SeekOptions::Absolute)).await {
                                    error!("error seeking: {}", e);
                                }
                            }
                        }

                        while let Err(e) = mpv.send_command(Command::SetProperty("pause", Value::Bool(paused))).await {
                            error!("error setting pause state: {}", e);
                            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                        }
                    }

                    Packet::PlaybackUpdate { timestamp, elapsed } => {
                        if let PlaybackState::Playing { start } = playback_state {
                            let latency = syncer.since(timestamp);
                            println!("latency: {}", latency.as_seconds_f64());
                            let client_elapsed = start.elapsed() - latency;
                            let drift = elapsed - client_elapsed - latency;
                            println!("drift: {}", drift.as_seconds_f64());
                            let time = elapsed - latency;

                            // TODO: slow down/speed up to resync smaller intervals
                            if drift.abs() > Duration::seconds(2) {
                                if let Err(e) = mpv.send_command(Command::Seek(time.as_seconds_f64(), SeekOptions::Absolute)).await {
                                    error!("error seeking: {}", e);
                                }
                            }
                        }
                    }

                    Packet::TimeSyncResponse(resp_time) => {
                        syncer.finish_sync(resp_time);
                    }

                    packet => {
                        dbg!(packet);
                    }
                }
            }

            event = events.recv() => {
                dbg!(&event);
                let new_state = get_playback_state(&mut mpv).await;
                transition_state(&connection, &syncer, &playback_state, &new_state).await?;
                playback_state = new_state;
            },

            _ = sync_interval.tick() => {
                let buf = Box::from([0u8; { mem::size_of::<Archived<Packet>>() }]);
                let mut serializer = BufferSerializer::new(buf);
                serializer
                    .serialize_value(&Packet::TimeSyncRequest)
                    .unwrap();
                syncer.start_sync();
                connection
                    .send_datagram(Bytes::from(serializer.into_inner()))
                    .unwrap();
            }
        }
    }
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
