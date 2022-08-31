use futures::{stream::SplitSink, SinkExt, StreamExt};
use log::{error, info, trace};
use serde::{
    de,
    de::{MapAccess, Visitor},
    ser::SerializeMap,
    Deserialize, Deserializer, Serialize,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fmt, io, process};
use thiserror::Error;
use tokio::{
    net::UnixStream,
    sync::{broadcast, mpsc, oneshot},
};
use tokio_util::codec::{Decoder, Encoder, Framed, LinesCodec, LinesCodecError};

#[derive(Error, Debug)]
pub enum Error {
    #[error("failed to start process: {0}")]
    StartProcessError(#[from] io::Error),
    #[error("ipc codec error: {0}")]
    CodecError(#[from] CodecError),
    #[error(transparent)]
    CommandError(#[from] CommandError),
}

#[derive(Error, Debug)]
pub enum CodecError {
    #[error("lines codec error: {0}")]
    LinesCodec(#[from] LinesCodecError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("fmt error: {0}")]
    Fmt(#[from] fmt::Error),
}

#[derive(Error, Debug)]
pub enum CommandError {
    #[error("error sending command: {0}")]
    SendError(#[from] mpsc::error::SendError<(i64, oneshot::Sender<MpvResult>)>),
    #[error("error receiving result: {0}")]
    RecvError(#[from] oneshot::error::RecvError),
    #[error("mpv responded with error: {0}")]
    MpvError(String),
}

pub struct Mpv {
    tx: SplitSink<Framed<UnixStream, MpvCodec>, TaggedCommand>,
    listen: mpsc::Sender<(i64, oneshot::Sender<MpvResult>)>,
    events: broadcast::Sender<Event>,
    next_request_id: i64,
}

impl Mpv {
    pub async fn spawn() -> Result<Self, Error> {
        let sock = format!("/tmp/mpv.{}.sock", process::id());

        tokio::process::Command::new("mpv")
            .arg(format!("--input-ipc-server={}", sock))
            .arg("--idle")
            .arg("--no-terminal")
            .spawn()?;

        let (tx, mut rx) = loop {
            if let Ok(stream) = UnixStream::connect(&sock).await {
                let framed = Framed::new(stream, MpvCodec::new());
                break framed.split();
            }
        };

        let (listen, mut listen_recv) = mpsc::channel(64);
        let (events, _) = broadcast::channel(64);
        {
            let events = events.clone();
            tokio::spawn(async move {
                let mut listeners: BTreeMap<i64, oneshot::Sender<MpvResult>> = BTreeMap::new();

                while let Some(Ok(message)) = rx.next().await {
                    while let Ok((id, listener)) = listen_recv.try_recv() {
                        listeners.insert(id, listener);
                    }

                    match message {
                        Message::Response { request_id, result } => {
                            if let Some(listener) = listeners.remove(&request_id) {
                                if let Err(e) = listener.send(result) {
                                    error!("listener disconnected: {:?}", e);
                                }
                            } else {
                                error!("no listener for response");
                            }
                        }
                        Message::Event(event) => {
                            if let Err(e) = events.send(event) {
                                error!("error sending event: {}", e);
                            }
                        }
                    }
                }
            });
        }

        Ok(Self {
            tx,
            listen,
            events,
            next_request_id: 0,
        })
    }

    pub async fn send_command(&mut self, command: Command) -> Result<Value, Error> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;

        let command = TaggedCommand {
            command,
            request_id,
        };

        self.tx.send(command).await?;

        let (tx, rx) = oneshot::channel();
        self.listen
            .send((request_id, tx))
            .await
            .map_err(CommandError::SendError)?;
        let res = rx
            .await
            .map_err(CommandError::RecvError)?
            .map_err(CommandError::MpvError)?;
        Ok(res)
    }

    pub fn listen_events(&mut self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
}

struct MpvCodec(LinesCodec);

impl MpvCodec {
    fn new() -> Self {
        Self(LinesCodec::new())
    }
}

impl Decoder for MpvCodec {
    type Error = CodecError;
    type Item = Message;

    fn decode(&mut self, src: &mut bytes::BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if let Some(line) = self.0.decode(src)? {
            trace!("[< mpv] {}", &line);
            Ok(serde_json::from_str(&line).unwrap_or_else(|_| {
                info!("unhandled mpv message: {}", line);
                None
            }))
        } else {
            Ok(None)
        }
    }
}

impl Encoder<TaggedCommand> for MpvCodec {
    type Error = CodecError;

    fn encode(
        &mut self,
        item: TaggedCommand,
        dst: &mut bytes::BytesMut,
    ) -> Result<(), Self::Error> {
        let line = serde_json::to_string(&item)?;
        trace!("[> mpv] {}", &line);
        self.0.encode(line, dst).map_err(CodecError::LinesCodec)
    }
}

type MpvResult = Result<Value, String>;

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
enum Message {
    Event(Event),
    Response {
        request_id: i64,
        #[serde(flatten, deserialize_with = "deserialize_response_result")]
        result: MpvResult,
    },
}

fn deserialize_response_result<'de, D>(deserializer: D) -> Result<MpvResult, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(field_identifier, rename_all = "kebab-case")]
    enum Field {
        Data,
        Error,
    }

    struct ResultVisitor;

    impl<'de> Visitor<'de> for ResultVisitor {
        type Value = MpvResult;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("Result")
        }

        fn visit_map<V>(self, mut map: V) -> Result<Self::Value, V::Error>
        where
            V: MapAccess<'de>,
        {
            let mut data: Option<Value> = None;
            let mut error: Option<&str> = None;

            while let Some(key) = map.next_key()? {
                match key {
                    Field::Data => {
                        if data.is_some() {
                            return Err(de::Error::duplicate_field("data"));
                        }
                        data = Some(map.next_value()?);
                    }
                    Field::Error => {
                        if error.is_some() {
                            return Err(de::Error::duplicate_field("error"));
                        }
                        error = Some(map.next_value()?);
                    }
                }
            }

            match (data, error) {
                (Some(data), Some("success")) => Ok(Ok(data)),
                (None, Some("success")) => Ok(Ok(Value::Null)),
                (None, Some(error)) => Ok(Err(error.to_string())),
                _ => Err(de::Error::missing_field("error")),
            }
        }
    }

    const FIELDS: &'static [&'static str] = &["data", "error"];
    deserializer.deserialize_struct("Result", FIELDS, ResultVisitor)
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    PlaybackRestart,
    PropertyChange {
        id: i64,
        #[serde(flatten)]
        property_change: PropertyChange,
    },
    Pause,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "name", content = "data", rename_all = "kebab-case")]
pub enum PropertyChange {
    PlaybackTime(f64),
}

#[derive(Serialize, Debug, Clone)]
struct TaggedCommand {
    #[serde(flatten)]
    command: Command,
    request_id: i64,
}

#[derive(Debug, Clone)]
pub enum Command {
    Seek(f64, SeekOptions),
    Observe(i64, &'static str),
    GetProperty(&'static str),
    SetProperty(&'static str, Value),
}

impl Serialize for Command {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let cmd = match self {
            Command::Seek(time, options) => {
                json!(["seek", time, options])
            }
            Command::Observe(id, property) => {
                json!(["observe_property", id, property])
            }
            Command::GetProperty(property) => {
                json!(["get_property", property])
            }
            Command::SetProperty(property, value) => {
                json!(["set_property", property, value])
            }
        };

        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("command", &cmd)?;
        map.end()
    }
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub enum SeekOptions {
    Relative,
    Absolute,
    RelativePercent,
    AbsolutePercent,
}
