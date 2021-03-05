use futures_util::stream::StreamExt;
use std::time;
use tokio_socketcan_bcm::*;

#[tokio::main]
async fn main() {
    let socket = BCMSocket::open_nb("vcan0").unwrap();
    let ival = time::Duration::from_millis(0);

    // create a stream of frames that filters by the can frame id 0x123
    let mut can_frame_stream = socket.filter(0x123.into(), ival, ival).unwrap();

    while let Some(frame) = can_frame_stream.next().await {
        println!("Frame {:?}", frame);
        ()
    }
}
