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

# Example 1

```Rust
use futures::stream::Stream;
use std::time;
use tokio_socketcan_bcm::*;
fn main() {
    let socket = BCMSocket::open_nb("vcan0").unwrap();
    // Throttle messages in kernel space to max every 5 seconds
    let ival = time::Duration::from_secs(5);
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
```

# Example 2 (async/await)
Notice: async/await currently requires nightly rust and the tokio `async-await-preview` feature.

```Rust
#![feature(await_macro, async_await, futures_api)]
#[macro_use]
extern crate tokio;
use std::time;
use tokio::prelude::*;
use tokio_socketcan_bcm::*;
fn main() {
    tokio::run_async(
        async {
            let socket = BCMSocket::open_nb("vcan0").unwrap();
            let ival = time::Duration::from_millis(0);
            // create a stream of messages that filters by the can frame id 0x123
            let mut can_frame_stream = socket
                .filter_id_incoming_frames(0x123.into(), ival, ival)
                .unwrap();
            while let Some(frame) = await!(can_frame_stream.next()) {
                println!("Frame {:?}", frame);
                ()
            }
        },
    );
}
```