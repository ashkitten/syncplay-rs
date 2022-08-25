use std::{
    io,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use syncplay_rs::{Packet, StreamId, TimeSyncer};
use tokio::{net::UdpSocket, sync::RwLock, time};
use webrtc_sctp::{
    association::{Association, Config},
    stream::{PollStream, ReliabilityType, Stream},
};
use webrtc_util::{conn::conn_disconnected_packet::DisconnectedPacketConn, Conn};

#[tokio::main]
async fn main() -> io::Result<()> {
    let server = Arc::new(Server::new().await?);

    while let Some(stream) = server.association.accept_stream().await {
        println!("accepted stream");
        match stream.stream_identifier().into() {
            StreamId::Control => {
                let server = Arc::clone(&server);
                tokio::spawn(async move {
                    server.run_control_stream(stream).await.unwrap();
                });
            }
            StreamId::Sync => {
                let server = Arc::clone(&server);
                tokio::spawn(async move {
                    server.run_sync_stream(stream).await.unwrap();
                });
            }
        }
    }

    Ok(())
}

struct Server {
    association: Association,
    syncer: Arc<RwLock<TimeSyncer>>,
    playback_start: Arc<RwLock<Instant>>,
}

impl Server {
    async fn new() -> io::Result<Self> {
        let sock = Arc::new(UdpSocket::bind("0.0.0.0:8998").await?);
        let conn = Arc::new(DisconnectedPacketConn::new(sock));
        println!("listening on {}", conn.local_addr().await.unwrap());
        let config = Config {
            net_conn: conn,
            max_receive_buffer_size: 0,
            max_message_size: 0,
            name: "server".to_owned(),
        };
        let association = Association::server(config).await?;
        println!("created server");

        Ok(Self {
            association,
            syncer: Arc::new(RwLock::new(TimeSyncer::new())),
            playback_start: Arc::new(RwLock::new(Instant::now())),
        })
    }

    async fn run_control_stream(&self, stream: Arc<Stream>) -> io::Result<()> {
        stream.set_reliability_params(true, ReliabilityType::Reliable, 0);
        println!("accepted control stream");

        let mut poll_stream = PollStream::new(stream);
        while let Ok(packet) = Packet::read_from(&mut poll_stream).await {
            match packet {
                Packet::PlaybackControl {
                    timestamp,
                    paused,
                    elapsed,
                } => {
                    let latency = self.syncer.read().await.since(timestamp);
                    *self.playback_start.write().await = Instant::now() - (elapsed + latency);
                }
                _ => (),
            }
        }
        Ok(())
    }

    async fn run_sync_stream(&self, stream: Arc<Stream>) -> io::Result<()> {
        stream.set_reliability_params(true, ReliabilityType::Rexmit, 0);
        println!("accepted sync stream");

        let mut poll_stream = PollStream::new(Arc::clone(&stream));
        tokio::spawn(async move {
            while let Ok(packet) = Packet::read_from(&mut poll_stream).await {
                match packet {
                    Packet::TimeSyncRequest => {
                        Packet::TimeSyncResponse(SystemTime::UNIX_EPOCH.elapsed().unwrap())
                            .write_into(&mut poll_stream)
                            .await
                            .unwrap();
                    }
                    _ => (),
                }
            }
        });

        let mut poll_stream = PollStream::new(Arc::clone(&stream));
        loop {
            let packet = Packet::PlaybackUpdate {
                timestamp: self.syncer.read().await.now(),
                elapsed: self.playback_start.read().await.elapsed(),
            };
            packet.write_into(&mut poll_stream).await.unwrap();
            time::sleep(Duration::from_secs(1)).await;
        }
    }
}
