use anyhow::Result;
use bytes::Bytes;
use log::error;
use quinn::{Endpoint, ServerConfig, VarInt};
use rkyv::{
    ser::{serializers::BufferSerializer, Serializer},
    Archived,
};
use std::{collections::HashMap, mem, time::Duration};
use syncplay_rs::{run_connection, Packet, PlaybackState, TimeSyncer};
use time::Instant;
use tokio::{select, time::MissedTickBehavior};
use tokio_stream::{StreamExt, StreamMap};

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

    let mut packets = StreamMap::new();
    let mut connections = HashMap::new();

    let syncer = TimeSyncer::new();

    let mut playback_state = PlaybackState::Stopped;

    let mut sync_interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
    sync_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        select! {
            Some(connection) = incoming.next() => {
                let (stream, connection) = run_connection(connection.await?).await;
                packets.insert(connection.stable_id(), stream);
                let send = connection.open_uni().await?;
                connections.insert(connection.stable_id(), (connection, send));
            }

            Some((id, packet)) = packets.next() => {
                dbg!(&packet);
                match packet {
                    Packet::Version(version) => {
                        // TODO: actual versions lol
                        if version != 69 {
                            connections[&id].0.close(VarInt::from_u32(69), "wrong version".as_bytes())
                        }

                        let packet = match playback_state {
                            PlaybackState::Stopped => continue,
                            PlaybackState::Playing { start } => Packet::PlaybackControl {
                                timestamp: syncer.now(),
                                paused: false,
                                elapsed: start.elapsed(),
                            },
                            PlaybackState::Paused { elapsed } => Packet::PlaybackControl {
                                timestamp: syncer.now(),
                                paused: true,
                                elapsed: elapsed,
                            },
                        };
                        packet.write_into(&mut connections.get_mut(&id).unwrap().1).await?;
                    }

                    Packet::PlaybackControl {
                        timestamp,
                        paused,
                        elapsed,
                    } => {
                        let latency = syncer.since(timestamp);
                        dbg!(&latency);

                        playback_state = match paused {
                            false => PlaybackState::Playing {
                                start: (Instant::now() - (elapsed + latency)).try_into().unwrap(),
                            },
                            true => PlaybackState::Paused {
                                elapsed: (elapsed + latency).try_into().unwrap_or_default(),
                            },
                        };

                        let packet = match playback_state {
                            PlaybackState::Stopped => continue,
                            PlaybackState::Playing { start } => Packet::PlaybackControl {
                                timestamp: syncer.now(),
                                paused: false,
                                elapsed: start.elapsed(),
                            },
                            PlaybackState::Paused { elapsed } => Packet::PlaybackControl {
                                timestamp: syncer.now(),
                                paused: true,
                                elapsed: elapsed,
                            },
                        };

                        let mut to_remove = Vec::new();
                        for (connection_id, (_, send)) in connections.iter_mut() {
                            if id != *connection_id {
                                if let Err(e) = packet.write_into(send).await {
                                    error!("error writing to stream: {}", e);
                                    to_remove.push(*connection_id);
                                }
                            }
                        }
                        for id in to_remove {
                            connections.remove(&id);
                        }
                    }

                    Packet::TimeSyncRequest => {
                        let packet = Packet::TimeSyncResponse(syncer.now());
                        let buf = Box::from([0u8; { mem::size_of::<Archived<Packet>>() }]);
                        let mut serializer = BufferSerializer::new(buf);
                        serializer.serialize_value(&packet)?;
                        connections[&id].0.send_datagram(Bytes::from(serializer.into_inner()))?;
                    }

                    _ => {
                        dbg!(packet);
                    }
                }
            }

            _ = sync_interval.tick() => {
                if let PlaybackState::Playing { start } = playback_state {
                    let packet = Packet::PlaybackUpdate {
                        timestamp: syncer.now(),
                        elapsed: start.elapsed(),
                    };
                    let buf = Box::from([0u8; { mem::size_of::<Archived<Packet>>() }]);
                    let mut serializer = BufferSerializer::new(buf);
                    serializer.serialize_value(&packet)?;
                    let bytes = Bytes::from(serializer.into_inner());

                    let mut to_remove = Vec::new();
                    for (connection_id, (connection, _)) in connections.iter() {
                        if let Err(e) = connection.send_datagram(bytes.clone()) {
                            error!("error sending datagram: {}", e);
                            to_remove.push(*connection_id);
                        }
                    }
                    for id in to_remove {
                        connections.remove(&id);
                    }
                }
            }
        }

        // reset after all connections drop
        if connections.len() == 0 {
            playback_state = PlaybackState::Stopped;
        }
    }
}
