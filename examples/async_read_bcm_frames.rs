use futures::stream::Stream;
use std::time;
use tokio_socketcan_bcm::*;

fn main() {
    let socket = BCMSocket::open_nb("vcan0").unwrap();
    let ival = time::Duration::from_millis(0);
    let f = socket
        .filter_id_incoming_frames(0x123.into(), ival, ival)
        .unwrap()
        .map_err(|err| eprintln!("IO error {:?}", err))
        .for_each(|frame| {
            println!("Frame {:?}", frame);
            Ok(())
        });
    tokio::run(f);
}
