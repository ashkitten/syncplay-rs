use std::{io, sync::Arc};
use syncplay_rs::{Packet, StreamId};
use tokio::{io::AsyncReadExt, net::UdpSocket};
use webrtc_sctp::{
    association::{Association, Config},
    stream::{PollStream, ReliabilityType},
};
use webrtc_util::{conn::conn_disconnected_packet::DisconnectedPacketConn, Conn};

#[tokio::main]
async fn main() -> io::Result<()> {
    let sock = Arc::new(UdpSocket::bind("0.0.0.0:8998").await?);
    let conn = Arc::new(DisconnectedPacketConn::new(sock));
    println!("listening on {}", conn.local_addr().await.unwrap());
    let config = Config {
        net_conn: conn,
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "server".to_owned(),
    };
    let server = Association::server(config).await?;
    println!("created server");

    while let Some(stream) = server.accept_stream().await {
        println!("accepted stream");

        match stream.stream_identifier().into() {
            StreamId::Control => {
                stream.set_reliability_params(false, ReliabilityType::Reliable, 0);
            }
            StreamId::Sync => {
                stream.set_reliability_params(false, ReliabilityType::Rexmit, 0);
            }
        }

        let mut stream = PollStream::new(stream);

        let mut buf = [0u8; 1024];
        while let Ok(n) = stream.read(&mut buf).await {
            let packet = unsafe { rkyv::archived_root::<Packet>(&buf[..n]) };
            dbg!(packet);
        }
    }

    Ok(())
}
