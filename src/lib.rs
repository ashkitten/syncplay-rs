use num_enum::{FromPrimitive, IntoPrimitive};
use rkyv::{AlignedBytes, Archive, Archived, Deserialize, Serialize};
use std::{io, mem, time::Duration};
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
