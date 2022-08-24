use bytes::Bytes;
use std::{io, sync::Arc};
use syncplay_rs::{Packet, StreamId};
use tokio::{io::AsyncWriteExt, net::UdpSocket};
use webrtc_sctp::{
    association::{Association, Config},
    chunk::chunk_payload_data::PayloadProtocolIdentifier,
    stream::{PollStream, ReliabilityType},
};

#[tokio::main]
async fn main() -> io::Result<()> {
    let conn = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    conn.connect("127.0.0.1:8998").await?;
    println!("connected on {}", conn.local_addr().unwrap());
    let config = Config {
        net_conn: conn,
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    };
    let client = Association::client(config).await?;
    println!("created client");

    let stream = client
        .open_stream(StreamId::Control.into(), PayloadProtocolIdentifier::Unknown)
        .await?;
    stream.set_reliability_params(true, ReliabilityType::Reliable, 0);
    let mut stream = PollStream::new(stream);

    let buf = rkyv::to_bytes::<_, 1024>(&Packet::Version(69)).unwrap();
    stream.write(&Bytes::from(buf.into_boxed_slice())).await?;

    Ok(())
}
