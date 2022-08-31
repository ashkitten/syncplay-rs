use anyhow::{bail, Result};
use bytes::Bytes;
use log::{error, info};
use quinn::{ClientConfig, Endpoint};
use rkyv::{
    ser::{serializers::BufferSerializer, Serializer},
    Archived,
};
use serde_json::Value;
use std::{mem, sync::Arc};
use syncplay_rs::{
    mpv::{Command, Event, Mpv, PropertyChange, SeekOptions},
    run_connection, Packet, TimeSyncer,
};
use time::{Duration, Instant};
use tokio::{select, time::MissedTickBehavior};
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

    let connection = endpoint
        .connect("127.0.0.1:8998".parse()?, "localhost")?
        .await?;
    let (mut stream, connection) = run_connection(connection).await;

    let mut syncer = TimeSyncer::new();

    let mut playback_start = Instant::now();
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

    mpv.send_command(Command::Observe(1, "playback-time"))
        .await?;

    loop {
        select! {
            Some(packet) = stream.next() => {
                match packet {
                    Packet::PlaybackStart { timestamp, paused, elapsed } => {
                        let latency = syncer.since(timestamp);
                        let time = elapsed - latency;

                        while let Err(e) = mpv.send_command(Command::SetProperty("pause", Value::Bool(paused))).await {
                            error!("error setting pause state: {}", e);
                            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                        }

                        while let Err(e) = mpv.send_command(Command::Seek(time.as_seconds_f64(), SeekOptions::Absolute)).await {
                            error!("error seeking: {}", e);
                            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                        }
                    }

                    Packet::PlaybackControl { timestamp, paused, elapsed } => {
                        info!("PlaybackControl {{ paused: {paused}, elapsed: {elapsed} }}");
                        let latency = syncer.since(timestamp);
                        let time = elapsed - latency;

                        while let Err(e) = mpv.send_command(Command::SetProperty("pause", Value::Bool(paused))).await {
                            error!("error setting pause state: {}", e);
                            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                        }

                        if (time - playback_start.elapsed()).abs() > Duration::seconds(1) {
                            while let Err(e) = mpv.send_command(Command::Seek(time.as_seconds_f64(), SeekOptions::Absolute)).await {
                                error!("error seeking: {}", e);
                                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                            }
                        }
                    }

                    Packet::PlaybackUpdate { timestamp, elapsed } => {
                        let latency = syncer.since(timestamp);
                        println!("latency: {}", latency.as_seconds_f64());
                        let client_elapsed = playback_start.elapsed() - latency;
                        let drift = elapsed - client_elapsed - latency;
                        println!("drift: {}", drift.as_seconds_f64());
                        let time = elapsed - latency;

                        // TODO: slow down/speed up to resync smaller intervals
                        if drift.abs() > Duration::seconds(5) {
                            if let Err(e) = mpv.send_command(Command::Seek(time.as_seconds_f64(), SeekOptions::Absolute)).await {
                                error!("error seeking: {}", e);
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
                match event {
                    Ok(Event::PlaybackRestart) => {
                        let time = loop {
                            if let Ok(time) = mpv.send_command(Command::GetProperty("playback-time")).await {
                                break time.as_f64().map(Duration::seconds_f64).unwrap();
                            }
                            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                        };

                        let packet = Packet::PlaybackControl {
                            timestamp: syncer.now(),
                            paused: false,
                            elapsed: time,
                        };
                        let mut stream = connection.open_uni().await?;
                        packet.write_into(&mut stream).await?;
                    }

                    Ok(Event::PropertyChange { id: _, property_change }) => {
                        match property_change {
                            PropertyChange::PlaybackTime(time) => {
                                playback_start = Instant::now() - Duration::seconds_f64(time);
                            }
                        }
                    }

                    Ok(Event::Pause) => {
                        let time = loop {
                            if let Ok(time) = mpv.send_command(Command::GetProperty("playback-time")).await {
                                break time.as_f64().map(Duration::seconds_f64).unwrap();
                            }
                            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                        };

                        let packet = Packet::PlaybackControl {
                            timestamp: syncer.now(),
                            paused: true,
                            elapsed: time,
                        };
                        let mut stream = connection.open_uni().await?;
                        packet.write_into(&mut stream).await?;
                    }

                    Err(e) => {
                        bail!("error receiving event: {}", e);
                    }
                }
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
