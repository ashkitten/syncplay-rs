use num_enum::{FromPrimitive, IntoPrimitive};
use rkyv::{AlignedBytes, Archive, Archived, Deserialize, Serialize};
use std::{
    io, mem,
    time::{Duration, Instant, SystemTime},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(FromPrimitive, IntoPrimitive)]
#[repr(u16)]
pub enum StreamId {
    #[num_enum(default)]
    Control,
    Sync,
}

#[derive(Serialize, Deserialize, Archive, Debug)]
pub enum Packet {
    Version(u8),
    TimeSyncRequest,
    TimeSyncResponse(Duration),
    MediaInfo {
        name: String,
        duration: Duration,
    },
    PlaybackUpdate {
        timestamp: Duration,
        elapsed: Duration,
    },
    PlaybackControl {
        timestamp: Duration,
        paused: bool,
        elapsed: Duration,
    },
}

impl Packet {
    pub async fn read_from(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Self> {
        let mut buf = AlignedBytes([0u8; mem::size_of::<Archived<Self>>()]);
        let n = reader.read(buf.as_mut_slice()).await?;
        let packet = unsafe { rkyv::from_bytes_unchecked::<Self>(&buf[..n]).unwrap() };
        dbg!(&packet);
        Ok(packet)
    }

    pub async fn write_into(&self, writer: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        let buf = rkyv::to_bytes::<_, { mem::size_of::<Archived<Packet>>() }>(self).unwrap();
        writer.write(&buf).await?;
        Ok(())
    }
}

pub struct TimeSyncer {
    epoch: Instant,
    offset: Duration,
    sync_start: Instant,
    rtt_samples: Vec<Duration>,
}

impl TimeSyncer {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now() - SystemTime::UNIX_EPOCH.elapsed().unwrap(),
            offset: Duration::ZERO,
            sync_start: Instant::now(),
            rtt_samples: Vec::with_capacity(10),
        }
    }

    pub fn start_sync(&mut self) {
        self.sync_start = Instant::now();
        self.epoch = self.sync_start - SystemTime::UNIX_EPOCH.elapsed().unwrap();
    }

    pub fn finish_sync(&mut self, since_epoch: Duration) {
        self.rtt_samples.insert(0, self.sync_start.elapsed());
        self.rtt_samples.truncate(10);
        let avg_rtt: Duration =
            self.rtt_samples.iter().sum::<Duration>() / self.rtt_samples.len() as u32;
        self.offset =
            since_epoch.saturating_sub(self.sync_start.duration_since(self.epoch)) + avg_rtt / 2;
    }

    pub fn since(&self, timestamp: Duration) -> Duration {
        dbg!(self.offset, &self.rtt_samples);
        self.epoch.elapsed() + self.offset - timestamp
    }

    pub fn now(&self) -> Duration {
        self.epoch.elapsed() + self.offset
    }
}
