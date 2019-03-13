#![deny(clippy::all)]
//! The Broadcast Manager protocol provides a command based configuration
//! interface to filter and send (e.g. cyclic) CAN messages in kernel space.
//! Filtering messages in kernel space may significantly reduce the load in an application.
//!
//! A BCM socket is not intended for sending individual CAN frames.
//! To send invidiual frames use the [tokio-socketcan](https://crates.io/crates/tokio-socketcan) crate.
//!
//! # Example 1
//! ```
//! use futures::stream::Stream;
//! use std::time;
//! use tokio_socketcan_bcm::*;
//!
//! fn main() {
//!     let socket = BCMSocket::open_nb("vcan0").unwrap();
//!     // Throttle messages in kernel space to max every 5 seconds
//!     let ival = time::Duration::from_secs(5);
//!     let f = socket
//!         .filter_id_incoming_frames(0x123.into(), ival, ival)
//!         .unwrap()
//!         .map_err(|err| eprintln!("IO error {:?}", err))
//!         .for_each(|frame| {
//!             println!("Frame {:?}", frame);
//!             Ok(())
//!         });
//!     tokio::run(f);
//! }
//! ```
//!
//! # Example 2 (async/await)
//! Notice: async/await currently requires nightly rust and the tokio `async-await-preview` feature.
//! ```
//! #![feature(await_macro, async_await, futures_api)]
//!
//! #[macro_use]
//! extern crate tokio;
//!
//! use std::time;
//! use tokio::prelude::*;
//! use tokio_socketcan_bcm::*;
//!
//! fn main() {
//!     tokio::run_async(
//!         async {
//!             let socket = BCMSocket::open_nb("vcan0").unwrap();
//!             let ival = time::Duration::from_millis(0);
//!
//!             // create a stream of messages that filters by the can frame id 0x123
//!             let mut can_frame_stream = socket
//!                 .filter_id_incoming_frames(0x123.into(), ival, ival)
//!                 .unwrap();
//!
//!             while let Some(frame) = await!(can_frame_stream.next()) {
//!                 println!("Frame {:?}", frame);
//!                 ()
//!             }
//!         },
//!     );
//! }
//! ```

use libc::{
    c_int, c_short, c_uint, c_void, close, connect, fcntl, read, sockaddr, socket, suseconds_t,
    time_t, timeval, write, F_SETFL, O_NONBLOCK,
};

use bitflags::bitflags;

#[cfg(feature = "try_from")]
use core::convert::TryFrom;
use futures;
use futures::try_ready;
use futures::Stream;
use mio::unix::EventedFd;
use mio::{Evented, Poll, PollOpt, Ready, Token};
use nix::net::if_::if_nametoindex;
use std::collections::VecDeque;
use std::fmt;
use std::io::{Error, ErrorKind};
use std::mem::size_of;
use std::{io, slice, time};
use socketcan::EFF_FLAG;
use tokio::reactor::PollEvented2;

// reexport socketcan CANFrame
pub use socketcan::CANFrame;

#[cfg(feature = "try_from")]
use socketcan::{SFF_MASK, EFF_MASK};

#[cfg(test)]
mod tests {

    use super::*;
    use std::convert::TryFrom;

    #[test]
    fn eff_with_eff_bit_is_stripped_of_bit() {
        let can_id = CANMessageId::try_from(0x98FE_F5EBu32);
        assert_eq!(Ok(CANMessageId::EFF(0x18FE_F5EB)), can_id);
    }
}

/// defined in socket.h
pub const AF_CAN: c_int = 29;
/// defined in socket.h
pub const PF_CAN: c_int = AF_CAN;

pub const CAN_BCM: c_int = 2;

/// datagram (connection less) socket
pub const SOCK_DGRAM: c_int = 2;

const SFF_MASK_U16: u16 = 0x07ff;

pub const MAX_NFRAMES: u32 = 256;

/// OpCodes
///
/// create (cyclic) transmission task
pub const TX_SETUP: u32 = 1;
/// remove (cyclic) transmission task
pub const TX_DELETE: u32 = 2;
/// read properties of (cyclic) transmission task
pub const TX_READ: u32 = 3;
/// send one CAN frame
pub const TX_SEND: u32 = 4;
/// create RX content filter subscription
pub const RX_SETUP: u32 = 5;
/// remove RX content filter subscription
pub const RX_DELETE: u32 = 6;
/// read properties of RX content filter subscription
pub const RX_READ: u32 = 7;
/// reply to TX_READ request
pub const TX_STATUS: u32 = 8;
/// notification on performed transmissions (count=0)
pub const TX_EXPIRED: u32 = 9;
/// reply to RX_READ request
pub const RX_STATUS: u32 = 10;
/// cyclic message is absent
pub const RX_TIMEOUT: u32 = 11;
/// sent if the first or a revised CAN message was received
pub const RX_CHANGED: u32 = 12;

/// Flags
///
/// set the value of ival1, ival2 and count
pub const SETTIMER: u32 = 0x0001;
/// start the timer with the actual value of ival1, ival2 and count.
/// Starting the timer leads simultaneously to emit a can_frame.
pub const STARTTIMER: u32 = 0x0002;
/// create the message TX_EXPIRED when count expires
pub const TX_COUNTEVT: u32 = 0x0004;
/// A change of data by the process is emitted immediatly.
/// (Requirement of 'Changing Now' - BAES)
pub const TX_ANNOUNCE: u32 = 0x0008;
/// Copies the can_id from the message header to each subsequent frame
/// in frames. This is intended only as usage simplification.
pub const TX_CP_CAN_ID: u32 = 0x0010;
/// Filter by can_id alone, no frames required (nframes=0)
pub const RX_FILTER_ID: u32 = 0x0020;
/// A change of the DLC leads to an RX_CHANGED.
pub const RX_CHECK_DLC: u32 = 0x0040;
/// If the timer ival1 in the RX_SETUP has been set equal to zero, on receipt
/// of the CAN message the timer for the timeout monitoring is automatically
/// started. Setting this flag prevents the automatic start timer.
pub const RX_NO_AUTOTIMER: u32 = 0x0080;
/// refers also to the time-out supervision of the management RX_SETUP.
/// By setting this flag, when an RX-outs occours, a RX_CHANGED will be
/// generated when the (cyclic) receive restarts. This will happen even if the
/// user data have not changed.
pub const RX_ANNOUNCE_RESUM: u32 = 0x0100;
/// forces a reset of the index counter from the update to be sent by multiplex
/// message even if it would not be necessary because of the length.
pub const TX_RESET_MULTI_ID: u32 = 0x0200;
/// the filter passed is used as CAN message to be sent when receiving an RTR frame.
pub const RX_RTR_FRAME: u32 = 0x0400;
pub const CAN_FD_FRAME: u32 = 0x0800;

/// BcmMsgHead
///
/// Head of messages to and from the broadcast manager
#[repr(C)]
pub struct BcmMsgHead {
    _opcode: u32,
    _flags: u32,
    /// number of frames to send before changing interval
    _count: u32,
    /// interval for the first count frames
    _ival1: timeval,
    /// interval for the following frames
    _ival2: timeval,
    _can_id: u32,
    /// number of can frames appended to the message head
    _nframes: u32,
    // TODO figure out how why C adds a padding here?
    #[cfg(all(target_pointer_width = "32"))]
    _pad: u32,
    // TODO figure out how to allocate only nframes instead of MAX_NFRAMES
    /// buffer of CAN frames
    _frames: [CANFrame; MAX_NFRAMES as usize],
}

impl BcmMsgHead {
    pub fn can_id(&self) -> u32 {
        self._can_id
    }

    #[inline]
    pub fn frames(&self) -> &[CANFrame] {
        unsafe { slice::from_raw_parts(self._frames.as_ptr(), self._nframes as usize) }
    }
}

impl fmt::Debug for BcmMsgHead {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "BcmMsgHead {{ _opcode: {}, _flags: {} , _count: {}, _ival1: {:?}, _ival2: {:?}, _can_id: {}, _nframes: {}}}", self._opcode, self._flags,              self._count, self._ival1.tv_sec, self._ival2.tv_sec, self._can_id, self._nframes)
    }
}

/// BcmMsgHeadFrameLess
///
/// Head of messages to and from the broadcast manager see _pad fields for differences
/// to BcmMsgHead
#[repr(C)]
pub struct BcmMsgHeadFrameLess {
    _opcode: u32,
    _flags: u32,
    /// number of frames to send before changing interval
    _count: u32,
    /// interval for the first count frames
    _ival1: timeval,
    /// interval for the following frames
    _ival2: timeval,
    _can_id: u32,
    /// number of can frames appended to the message head
    _nframes: u32,
    // Workaround Rust ZST has a size of 0 for frames, in
    // C the BcmMsgHead struct contains an Array that although it has
    // a length of zero still takes n (4) bytes.
    #[cfg(all(target_pointer_width = "32"))]
    _pad: usize,
}

#[repr(C)]
pub struct TxMsg {
    _msg_head: BcmMsgHeadFrameLess,
    _frames: [CANFrame; MAX_NFRAMES as usize],
}

#[derive(Debug)]
/// Errors opening socket
pub enum CANBCMSocketOpenError {
    /// Device could not be found
    LookupError(nix::Error),

    /// System error while trying to look up device name
    IOError(io::Error),
}

impl fmt::Display for CANBCMSocketOpenError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            CANBCMSocketOpenError::LookupError(ref e) => write!(f, "CAN Device not found: {}", e),
            CANBCMSocketOpenError::IOError(ref e) => write!(f, "IO: {}", e),
        }
    }
}

impl std::error::Error for CANBCMSocketOpenError {
    fn description(&self) -> &str {
        match *self {
            CANBCMSocketOpenError::LookupError(_) => "can device not found",
            CANBCMSocketOpenError::IOError(ref e) => e.description(),
        }
    }

    fn cause(&self) -> Option<&std::error::Error> {
        match *self {
            CANBCMSocketOpenError::LookupError(ref e) => Some(e),
            CANBCMSocketOpenError::IOError(ref e) => Some(e),
        }
    }
}

impl From<nix::Error> for CANBCMSocketOpenError {
    fn from(e: nix::Error) -> CANBCMSocketOpenError {
        CANBCMSocketOpenError::LookupError(e)
    }
}

impl From<io::Error> for CANBCMSocketOpenError {
    fn from(e: io::Error) -> CANBCMSocketOpenError {
        CANBCMSocketOpenError::IOError(e)
    }
}

/// A socket for a CAN device, specifically for broadcast manager operations.
#[derive(Debug)]
pub struct BCMSocket {
    pub fd: c_int,
}

pub struct BcmFrameStream {
    io: PollEvented2<BCMSocket>,
    frame_buffer: VecDeque<CANFrame>,
}

impl BcmFrameStream {
    pub fn new(socket: BCMSocket) -> BcmFrameStream {
        BcmFrameStream {
            io: PollEvented2::new(socket),
            frame_buffer: VecDeque::new(),
        }
    }
}

impl Stream for BcmFrameStream {
    type Item = CANFrame;
    type Error = io::Error;

    fn poll(&mut self) -> futures::Poll<Option<Self::Item>, Self::Error> {
        let ready = Ready::readable();

        // Buffer still contains frames
        // after testing this it looks like the recv_msg will never contain
        // more than one msg, therefore the buffer is basically never filled
        if let Some(frame) = self.frame_buffer.pop_front() {
            return Ok(futures::Async::Ready(Some(frame)));
        }

        try_ready!(self.io.poll_read_ready(ready));

        match self.io.get_ref().read_msg() {
            Ok(n) => {
                let mut frames = n.frames().to_vec();
                if let Some(frame) = frames.pop() {
                    if !frames.is_empty() {
                        for frame in n.frames() {
                            self.frame_buffer.push_back(*frame)
                        }
                    }
                    Ok(futures::Async::Ready(Some(frame)))
                } else {
                    // This happens e.g. when a timed out msg is received
                    self.io.clear_read_ready(ready)?;
                    Ok(futures::Async::NotReady)
                }
            }
            Err(e) => {
                if e.kind() == io::ErrorKind::WouldBlock {
                    self.io.clear_read_ready(ready)?;
                    return Ok(futures::Async::NotReady);
                }
                Err(e)
            }
        }
    }
}

impl Evented for BcmFrameStream {
    fn register(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        self.io.get_ref().register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        self.io.get_ref().reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        self.io.get_ref().deregister(poll)
    }
}

impl BCMSocket {
    /// Open a named CAN device non blocking.
    ///
    /// Usually the more common case, opens a socket can device by name, such
    /// as "vcan0" or "socan0".
    pub fn open_nb(ifname: &str) -> Result<BCMSocket, CANBCMSocketOpenError> {
        let if_index = if_nametoindex(ifname)?;
        BCMSocket::open_if_nb(if_index)
    }

    /// Open CAN device by interface number non blocking.
    ///
    /// Opens a CAN device by kernel interface number.
    pub fn open_if_nb(if_index: c_uint) -> Result<BCMSocket, CANBCMSocketOpenError> {
        // open socket
        let sock_fd;
        unsafe {
            sock_fd = socket(PF_CAN, SOCK_DGRAM, CAN_BCM);
        }

        if sock_fd == -1 {
            return Err(CANBCMSocketOpenError::from(io::Error::last_os_error()));
        }

        let fcntl_resp = unsafe { fcntl(sock_fd, F_SETFL, O_NONBLOCK) };

        if fcntl_resp == -1 {
            return Err(CANBCMSocketOpenError::from(io::Error::last_os_error()));
        }

        let addr = CANAddr {
            _af_can: AF_CAN as c_short,
            if_index: if_index as c_int,
            rx_id: 0, // ?
            tx_id: 0, // ?
        };

        let sockaddr_ptr = &addr as *const CANAddr;

        let connect_res;
        unsafe {
            connect_res = connect(
                sock_fd,
                sockaddr_ptr as *const sockaddr,
                size_of::<CANAddr>() as u32,
            );
        }

        if connect_res != 0 {
            return Err(CANBCMSocketOpenError::from(io::Error::last_os_error()));
        }

        Ok(BCMSocket { fd: sock_fd })
    }

    fn close(&mut self) -> io::Result<()> {
        unsafe {
            let rv = close(self.fd);
            if rv != -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Create a content filter subscription, filtering can frames by can_id.
    pub fn filter_id(
        &self,
        can_id: CANMessageId,
        ival1: time::Duration,
        ival2: time::Duration,
    ) -> io::Result<()> {
        let _ival1 = c_timeval_new(ival1);
        let _ival2 = c_timeval_new(ival2);

        let frames = [CANFrame::new(0x0, &[], false, false).unwrap(); MAX_NFRAMES as usize];
        let msg = BcmMsgHeadFrameLess {
            _opcode: RX_SETUP,
            _flags: SETTIMER | RX_FILTER_ID,
            _count: 0,
            #[cfg(all(target_pointer_width = "32"))]
            _pad: 0,
            _ival1,
            _ival2,
            _can_id: can_id.with_eff_bit(),
            _nframes: 0,
        };

        let tx_msg = &TxMsg {
            _msg_head: msg,
            _frames: frames,
        };

        let tx_msg_ptr = tx_msg as *const TxMsg;

        let write_rv = unsafe { write(self.fd, tx_msg_ptr as *const c_void, size_of::<TxMsg>()) };

        if write_rv < 0 {
            return Err(Error::new(ErrorKind::WriteZero, io::Error::last_os_error()));
        }

        Ok(())
    }

    ///
    /// Combination of `BCMSocket::filter_id` and `BCMSocket::incoming_frames`.
    /// ```
    /// extern crate futures;
    /// extern crate tokio;
    /// extern crate socketcan;
    ///
    /// use futures::stream::Stream;
    /// use tokio::prelude::*;
    /// use std::time;
    /// use tokio_socketcan_bcm::*;
    ///
    /// let ival = time::Duration::from_millis(1);
    /// let socket = BCMSocket::open_nb("vcan0").unwrap();
    /// let f = socket.filter_id_incoming_frames(0x123.into(), ival, ival).unwrap()
    ///       .map_err(|_| ())
    ///       .for_each(|frame| {
    ///          println!("Frame {:?}", frame);
    ///          Ok(())
    ///        });
    /// tokio::run(f);
    /// ```
    ///
    pub fn filter_id_incoming_frames(
        self,
        can_id: CANMessageId,
        ival1: time::Duration,
        ival2: time::Duration,
    ) -> io::Result<BcmFrameStream> {
        self.filter_id(can_id, ival1, ival2)?;
        Ok(self.incoming_frames())
    }

    ///
    /// Stream of incoming BcmMsgHeads that apply to the filter criteria.
    /// ```
    /// extern crate futures;
    /// extern crate tokio;
    /// extern crate socketcan;
    ///
    /// use futures::stream::Stream;
    /// use tokio::prelude::*;
    /// use std::time;
    /// use tokio_socketcan_bcm::*;
    ///
    /// let socket = BCMSocket::open_nb("vcan0").unwrap();
    /// let ival = time::Duration::from_millis(1);
    /// socket.filter_id(0x123.into(), ival, ival).unwrap();
    /// let f = socket.incoming_msg()
    ///        .map_err(|err| {
    ///            eprintln!("IO error {:?}", err)
    ///        })
    ///       .for_each(|bcm_msg_head| {
    ///          println!("BcmMsgHead {:?}", bcm_msg_head);
    ///          Ok(())
    ///        });
    /// tokio::run(f);
    /// ```
    ///
    pub fn incoming_msg(self) -> BcmStream {
        BcmStream::from(self)
    }

    ///
    /// Stream of incoming frames that apply to the filter criteria.
    /// ```
    /// extern crate futures;
    /// extern crate tokio;
    /// extern crate socketcan;
    ///
    /// use futures::stream::Stream;
    /// use tokio::prelude::*;
    /// use std::time;
    /// use tokio_socketcan_bcm::*;
    ///
    /// let socket = BCMSocket::open_nb("vcan0").unwrap();
    /// let ival = time::Duration::from_millis(1);
    /// socket.filter_id(0x123.into(), ival, ival).unwrap();
    /// let f = socket.incoming_frames()
    ///        .map_err(|err| {
    ///            eprintln!("IO error {:?}", err)
    ///        })
    ///       .for_each(|frame| {
    ///          println!("Frame {:?}", frame);
    ///          Ok(())
    ///        });
    /// tokio::run(f);
    /// ```
    ///
    pub fn incoming_frames(self) -> BcmFrameStream {
        BcmFrameStream::new(self)
    }

    /// Remove a content filter subscription.
    pub fn filter_delete(&self, can_id: CANMessageId) -> io::Result<()> {
        let frames = [CANFrame::new(0x0, &[], false, false).unwrap(); MAX_NFRAMES as usize];

        let msg = &BcmMsgHead {
            _opcode: RX_DELETE,
            _flags: 0,
            _count: 0,
            _ival1: c_timeval_new(time::Duration::new(0, 0)),
            _ival2: c_timeval_new(time::Duration::new(0, 0)),
            _can_id: can_id.with_eff_bit(),
            _nframes: 0,
            #[cfg(all(target_pointer_width = "32"))]
            _pad: 0,
            _frames: frames,
        };

        let msg_ptr = msg as *const BcmMsgHead;
        let write_rv = unsafe { write(self.fd, msg_ptr as *const c_void, size_of::<BcmMsgHead>()) };

        let expected_size = size_of::<BcmMsgHead>() - size_of::<[CANFrame; MAX_NFRAMES as usize]>();
        if write_rv as usize != expected_size {
            let msg = format!("Wrote {} but expected {}", write_rv, expected_size);
            return Err(Error::new(ErrorKind::WriteZero, msg));
        }

        Ok(())
    }

    /// Read a single bcm message.
    pub fn read_msg(&self) -> io::Result<BcmMsgHead> {
        let ival1 = c_timeval_new(time::Duration::from_millis(0));
        let ival2 = c_timeval_new(time::Duration::from_millis(0));
        let frames = [CANFrame::new(0x0, &[], false, false).unwrap(); MAX_NFRAMES as usize];
        let mut msg = BcmMsgHead {
            _opcode: 0,
            _flags: 0,
            _count: 0,
            _ival1: ival1,
            _ival2: ival2,
            _can_id: 0,
            _nframes: 0,
            #[cfg(all(target_pointer_width = "32"))]
            _pad: 0,
            _frames: frames,
        };

        let msg_ptr = &mut msg as *mut BcmMsgHead;
        let count = unsafe { read(self.fd, msg_ptr as *mut c_void, size_of::<BcmMsgHead>()) };

        let last_error = io::Error::last_os_error();
        if count < 0 {
            Err(last_error)
        } else {
            Ok(msg)
        }
    }
}

impl Evented for BCMSocket {
    fn register(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.fd).register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        EventedFd(&self.fd).reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        EventedFd(&self.fd).deregister(poll)
    }
}

impl Drop for BCMSocket {
    fn drop(&mut self) {
        self.close().ok(); // ignore result
    }
}

pub struct BcmStream {
    io: PollEvented2<BCMSocket>,
}

pub trait IntoBcmStream {
    type Stream: futures::stream::Stream;
    type Error;

    fn into_bcm(self) -> Result<Self::Stream, Self::Error>;
}

impl BcmStream {
    pub fn from(bcm_socket: BCMSocket) -> BcmStream {
        let io = PollEvented2::new(bcm_socket);
        BcmStream { io }
    }
}

impl Stream for BcmStream {
    type Item = BcmMsgHead;
    type Error = io::Error;

    fn poll(&mut self) -> futures::Poll<Option<Self::Item>, Self::Error> {
        let ready = Ready::readable();

        try_ready!(self.io.poll_read_ready(ready));

        match self.io.get_ref().read_msg() {
            Ok(n) => Ok(futures::Async::Ready(Some(n))),
            Err(e) => {
                if e.kind() == io::ErrorKind::WouldBlock {
                    self.io.clear_read_ready(ready)?;
                    return Ok(futures::Async::NotReady);
                }
                Err(e)
            }
        }
    }
}

bitflags! {
    #[derive(Default)]
    pub struct FrameFlags: u32 {
        /// if set, indicate 29 bit extended format
        const EFF_FLAG = 0x8000_0000;

        /// remote transmission request flag
        const RTR_FLAG = 0x4000_0000;

        /// error flag
        const ERR_FLAG = 0x2000_0000;
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct CANAddr {
    pub _af_can: c_short,
    pub if_index: c_int, // address familiy,
    pub rx_id: u32,
    pub tx_id: u32,
}

/// 11-bit or 29-bit identifier of can frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Hash)]
pub enum CANMessageId {
    /// Standard Frame Format (11-bit identifier)
    SFF(u16),
    /// Extended Frame Format (29-bit identifier)
    EFF(u32),
}

impl fmt::Display for CANMessageId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            CANMessageId::SFF(id) => write!(f, "{}", id),
            CANMessageId::EFF(id) => write!(f, "{}", id),
        }
    }
}

impl CANMessageId {
    pub fn with_eff_bit(self) -> u32 {
        match self {
            CANMessageId::SFF(id) => u32::from(id),
            CANMessageId::EFF(id) => id | FrameFlags::EFF_FLAG.bits(),
        }
    }
}

impl From<u16> for CANMessageId {
    fn from(id: u16) -> CANMessageId {
        match id {
            0...SFF_MASK_U16 => CANMessageId::SFF(id),
            SFF_MASK_U16...std::u16::MAX => CANMessageId::EFF(u32::from(id)),
        }
    }
}

#[cfg(feature = "try_from")]
impl TryFrom<u32> for CANMessageId {
    type Error = ConstructionError;

    fn try_from(id: u32) -> Result<CANMessageId, ConstructionError> {
        match id {
            0...SFF_MASK => Ok(CANMessageId::SFF(id as u16)),
            SFF_MASK...EFF_MASK => Ok(CANMessageId::EFF(id)),
            _ => {
                // might be the EFF flag is set
                if id & EFF_FLAG != 0 {
                    let without_flag = id & EFF_MASK;
                    Ok(CANMessageId::EFF(without_flag))
                } else {
                    Err(ConstructionError::IDTooLarge)
                }
            },
        }
    }
}

impl From<CANMessageId> for u32 {
    fn from(id: CANMessageId) -> u32 {
        match id {
            CANMessageId::SFF(id) => u32::from(id),
            CANMessageId::EFF(id) => id,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
/// Error that occurs when creating CAN packets
pub enum ConstructionError {
    /// CAN ID was outside the range of valid IDs
    IDTooLarge,
}

impl fmt::Display for ConstructionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ConstructionError::IDTooLarge => write!(f, "CAN ID too large"),
        }
    }
}

impl std::error::Error for ConstructionError {
    fn description(&self) -> &str {
        match *self {
            ConstructionError::IDTooLarge => "can id too large",
        }
    }
}

fn c_timeval_new(t: time::Duration) -> timeval {
    timeval {
        tv_sec: t.as_secs() as time_t,
        tv_usec: i64::from(t.subsec_micros()) as suseconds_t,
    }
}
