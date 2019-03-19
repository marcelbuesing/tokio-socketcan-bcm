use futures::{Future, Stream};
use std::time;
use std::io;
use tokio_socketcan_bcm::*;

fn main() -> io::Result<()> {
    let socket = BCMSocket::open_nb("vcan0")?;
    let ival = time::Duration::from_millis(0);
    let f = socket
        .filter_id_incoming_frames(0x123.into(), ival, ival)?
        .for_each(|frame| {
            println!("Frame {:?}", frame);
            Ok(())
        })
        .map_err(|err| eprintln!("IO error {:?}", err));
    tokio::run(f);

    Ok(())
}
