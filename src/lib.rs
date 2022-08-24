use num_enum::{FromPrimitive, IntoPrimitive};
use rkyv::{Archive, Serialize};
use std::time::Duration;

#[derive(FromPrimitive, IntoPrimitive)]
#[repr(u16)]
pub enum StreamId {
    #[num_enum(default)]
    Control,
    Sync,
}

#[derive(Serialize, Archive, Debug)]
#[archive_attr(derive(Debug))]
pub enum Packet {
    Version(u8),
    MediaInfo {
        name: String,
        duration: Duration,
    },
    PlaybackUpdate {
        elapsed: Duration,
        timestamp: Duration,
    },
    PlaybackControl {
        paused: bool,
        elapsed: Duration,
        timestamp: Duration,
    },
}
