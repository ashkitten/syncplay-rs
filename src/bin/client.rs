use std::{
    io,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use syncplay_rs::{Packet, StreamId, TimeSyncer};
use tokio::{net::UdpSocket, process::Command, sync::RwLock, time};
use webrtc_sctp::{
    association::{Association, Config},
    chunk::chunk_payload_data::PayloadProtocolIdentifier,
    stream::{PollStream, ReliabilityType},
};

#[tokio::main]
async fn main() -> io::Result<()> {
    // Command::new("mpv")
    //     .arg("--input-ipc-server=/tmp/mpvsocket")
    //     .arg("--idle")
    //     .spawn()?
    //     .wait()
    //     .await?;

    let client = Arc::new(Client::new().await?);
    {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.run_control_stream().await.unwrap() });
    }
    {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.run_sync_stream().await.unwrap() });
    }

    loop {
        time::sleep(Duration::from_secs(1)).await;
        *client.playback_elapsed.write().await += Duration::from_secs(1);
    }
}

struct Client {
    association: Association,
    syncer: Arc<RwLock<TimeSyncer>>,
    playback_elapsed: Arc<RwLock<Duration>>,
}

impl Client {
    async fn new() -> io::Result<Self> {
        let conn = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        conn.connect("127.0.0.1:8998").await?;
        println!("connected on {}", conn.local_addr().unwrap());
        let config = Config {
            net_conn: conn,
            max_receive_buffer_size: 0,
            max_message_size: 0,
            name: "client".to_owned(),
        };
        let association = Association::client(config).await?;
        println!("created client");

        Ok(Self {
            association,
            syncer: Arc::new(RwLock::new(TimeSyncer::new())),
            playback_elapsed: Arc::new(RwLock::new(Duration::ZERO)),
        })
    }

    async fn run_control_stream(&self) -> io::Result<()> {
        let stream = self
            .association
            .open_stream(StreamId::Control.into(), PayloadProtocolIdentifier::Unknown)
            .await?;
        stream.set_reliability_params(true, ReliabilityType::Reliable, 0);
        let mut poll_stream = PollStream::new(Arc::clone(&stream));
        println!("control stream opened");

        Packet::Version(69).write_into(&mut poll_stream).await?;

        let packet = Packet::PlaybackControl {
            timestamp: self.syncer.read().await.now(),
            paused: false,
            elapsed: Duration::ZERO,
        };
        packet.write_into(&mut poll_stream).await?;

        loop {
            while let Ok(packet) = Packet::read_from(&mut poll_stream).await {
                match packet {
                    Packet::PlaybackControl {
                        timestamp,
                        paused,
                        elapsed,
                    } => {}
                    _ => (),
                }
            }
        }
    }

    async fn run_sync_stream(&self) -> io::Result<()> {
        let stream = self
            .association
            .open_stream(StreamId::Sync.into(), PayloadProtocolIdentifier::Unknown)
            .await?;
        stream.set_reliability_params(true, ReliabilityType::Rexmit, 0);
        let mut poll_stream = PollStream::new(Arc::clone(&stream));
        println!("sync stream opened");

        {
            let mut poll_stream = PollStream::new(Arc::clone(&stream));
            let syncer = Arc::clone(&self.syncer);
            tokio::spawn(async move {
                loop {
                    syncer.write().await.start_sync();
                    Packet::TimeSyncRequest
                        .write_into(&mut poll_stream)
                        .await
                        .unwrap();
                    time::sleep(Duration::from_secs(5)).await;
                }
            });
        }

        loop {
            while let Ok(packet) = Packet::read_from(&mut poll_stream).await {
                match packet {
                    Packet::PlaybackUpdate { timestamp, elapsed } => {
                        let latency = self.syncer.read().await.since(timestamp);
                        dbg!(latency);

                        let drift_ms: i128 = (elapsed - *self.playback_elapsed.read().await
                            + latency)
                            .as_millis() as i128;
                        dbg!(drift_ms);
                    }
                    Packet::TimeSyncResponse(resp_time) => {
                        self.syncer.write().await.finish_sync(resp_time);
                    }
                    _ => (),
                }
            }
        }
    }
}
