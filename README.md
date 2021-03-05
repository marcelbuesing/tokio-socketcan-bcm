# tokio-socketcan-bcm
[![LICENSE](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![VERSION](https://img.shields.io/crates/v/tokio-socketcan-bcm.svg)](https://crates.io/crates/tokio-socketcan-bcm)
[![docs](https://docs.rs/tokio-socketcan-bcm/badge.svg)](https://docs.rs/tokio-socketcan-bcm)

 The Broadcast Manager protocol provides a command based configuration
 interface to filter and send (e.g. cyclic) CAN messages in kernel space.
 Filtering messages in kernel space may significantly reduce the load in an application.

 A BCM socket is not intended for sending individual CAN frames.
 To send invidiual frames use the [tokio-socketcan](https://crates.io/crates/tokio-socketcan) crate.

This crate would not have been possible without the [socketcan crate](https://github.com/mbr/socketcan-rs).

# Example

```Rust
use std::time;
use tokio_socketcan_bcm::*;
use futures_util::stream::StreamExt;

#[tokio::main]
async fn main() {
    let socket = BCMSocket::open_nb("vcan0").unwrap();
    let ival = time::Duration::from_millis(0);

    // create a stream of messages that filters by the can frame id 0x123
    let mut can_frame_stream = socket
        .filter(0x123.into(), ival, ival)
        .unwrap();

    while let Some(frame) = can_frame_stream.next().await {
        println!("Frame {:?}", frame);
        ()
    }
}
```