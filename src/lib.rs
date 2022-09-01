use anyhow::Error;
use async_stream::stream;
use futures::{Stream, StreamExt};
use quinn::{Connection, NewConnection};
use rkyv::{
    to_archived,
    with::{ArchiveWith, DeserializeWith, SerializeWith},
    AlignedBytes, Archive, Archived, Deserialize, Fallible, Serialize,
};
use std::{io, mem, ptr::addr_of_mut};
use time::{Duration, Instant, OffsetDateTime};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    select,
};
use tokio_stream::StreamMap;
use tokio_util::codec::{Decoder, FramedRead};

pub mod mpv;

#[derive(Serialize, Deserialize, Archive, Debug)]
pub enum Packet {
    Version(u8),
    TimeSyncRequest,
    TimeSyncResponse(#[with(ArchivedDuration)] Duration),
    MediaInfo {
        name: String,
        #[with(ArchivedDuration)]
        duration: Duration,
    },
    PlaybackUpdate {
        #[with(ArchivedDuration)]
        timestamp: Duration,
        #[with(ArchivedDuration)]
        elapsed: Duration,
    },
    PlaybackControl {
        #[with(ArchivedDuration)]
        timestamp: Duration,
        paused: bool,
        #[with(ArchivedDuration)]
        elapsed: Duration,
    },
}

impl Packet {
    pub async fn read_from(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Self> {
        let mut buf = AlignedBytes([0xffu8; mem::size_of::<Archived<Self>>()]);
        let n = reader.read(buf.as_mut_slice()).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "stream closed"));
        }
        let packet = unsafe { rkyv::from_bytes_unchecked::<Self>(&buf[..n]).unwrap() };
        Ok(packet)
    }

    pub async fn write_into(&self, writer: &mut (impl AsyncWrite + Unpin)) -> io::Result<()> {
        let buf = rkyv::to_bytes::<_, { mem::size_of::<Archived<Packet>>() }>(self).unwrap();
        writer.write_all(&buf).await?;
        Ok(())
    }
}

pub struct PacketDecoder;

impl Decoder for PacketDecoder {
    type Error = Error;
    type Item = Packet;

    fn decode(&mut self, src: &mut bytes::BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < mem::size_of::<Archived<Packet>>() {
            return Ok(None);
        }

        let src = src.split_to(mem::size_of::<Archived<Packet>>());
        let mut bytes = AlignedBytes([0u8; mem::size_of::<Archived<Packet>>()]);
        bytes.copy_from_slice(&src);
        Ok(Some(unsafe {
            rkyv::from_bytes_unchecked::<Packet>(&bytes[..]).unwrap()
        }))
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
            epoch: Instant::now() - (OffsetDateTime::now_utc() - OffsetDateTime::UNIX_EPOCH),
            offset: Duration::ZERO,
            sync_start: Instant::now(),
            rtt_samples: Vec::with_capacity(10),
        }
    }

    pub fn start_sync(&mut self) {
        self.sync_start = Instant::now();
        self.epoch = self.sync_start - (OffsetDateTime::now_utc() - OffsetDateTime::UNIX_EPOCH);
    }

    pub fn finish_sync(&mut self, since_epoch: Duration) {
        self.rtt_samples.insert(0, self.sync_start.elapsed());
        self.rtt_samples.truncate(10);
        let avg_rtt: Duration =
            self.rtt_samples.iter().sum::<Duration>() / self.rtt_samples.len() as u32;
        self.offset = since_epoch - (self.sync_start - self.epoch) + avg_rtt / 2;
    }

    pub fn since(&self, timestamp: Duration) -> Duration {
        self.epoch.elapsed() + self.offset - timestamp
    }

    pub fn now(&self) -> Duration {
        self.epoch.elapsed() + self.offset
    }
}

pub struct ArchivedDuration {
    seconds: i64,
    nanoseconds: i32,
}

impl ArchiveWith<Duration> for ArchivedDuration {
    type Archived = ArchivedDuration;
    type Resolver = ();

    #[inline]
    unsafe fn resolve_with(
        field: &Duration,
        _pos: usize,
        _resolver: Self::Resolver,
        out: *mut Self::Archived,
    ) {
        addr_of_mut!((*out).seconds).write(to_archived!(field.whole_seconds()));
        addr_of_mut!((*out).nanoseconds).write(to_archived!(field.subsec_nanoseconds()));
    }
}

impl<S: Fallible + ?Sized> SerializeWith<Duration, S> for ArchivedDuration {
    fn serialize_with(_field: &Duration, _serializer: &mut S) -> Result<Self::Resolver, S::Error> {
        Ok(())
    }
}

impl<D: Fallible + ?Sized> DeserializeWith<ArchivedDuration, Duration, D> for ArchivedDuration {
    fn deserialize_with(
        field: &ArchivedDuration,
        _deserializer: &mut D,
    ) -> Result<Duration, D::Error> {
        Ok(Duration::new(field.seconds, field.nanoseconds))
    }
}

// TODO: when StreamMap gets a `.get()` method, create SyncplayConnection
// wrapper that provides a Stream and Sink of Packets and use that instead
pub async fn run_connection(
    connection: NewConnection,
) -> (impl Stream<Item = Packet> + Unpin, Connection) {
    let NewConnection {
        connection,
        mut uni_streams,
        mut datagrams,
        ..
    } = connection;

    let stream = Box::pin(stream! {
        let mut streams = StreamMap::new();

        loop {
            select! {
                Some(Ok(recv)) = uni_streams.next() => {
                    streams.insert(recv.id(), FramedRead::new(recv, PacketDecoder));
                }
                Some((_, Ok(packet))) = streams.next() => {
                    yield packet;
                }
                Some(Ok(datagram)) = datagrams.next() => {
                    let mut bytes = AlignedBytes([0u8; mem::size_of::<Archived<Packet>>()]);
                    bytes.copy_from_slice(&datagram);
                    let packet = unsafe { rkyv::from_bytes_unchecked::<Packet>(&bytes[..]).unwrap() };
                    yield packet;
                }
                else => break,
            }
        }
    });

    (stream, connection)
}

pub enum PlaybackState {
    Stopped,
    Playing { start: Instant },
    Paused { elapsed: Duration },
}
