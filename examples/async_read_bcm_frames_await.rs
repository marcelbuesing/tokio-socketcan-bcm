//! You must include the following dependencies for this example and build with nightly:
//! [dependencies-dev]
//! tokio = {version = "0.1.16", features = ["async-await-preview"]}
//!

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
