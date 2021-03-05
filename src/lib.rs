#![deny(clippy::all)]
//! The Broadcast Manager protocol provides a command based configuration
//! interface to filter and send (e.g. cyclic) CAN messages in kernel space.
//! Filtering messages in kernel space may significantly reduce the load in an application.
//!
//! A BCM socket is not intended for sending individual CAN frames.
//! To send invidiual frames use the [tokio-socketcan](https://crates.io/crates/tokio-socketcan) crate.
//!
//! # Example
//!
//! ```no_run
//! use std::time;
//! use tokio_socketcan_bcm::*;
//! use futures_util::stream::StreamExt;
//!
//! #[tokio::main]
//! async fn main() {
//!     let socket = BCMSocket::open_nb("vcan0").unwrap();
//!     let ival = time::Duration::from_millis(0);
//!
//!     // create a stream of messages that filters by the can frame id 0x123
//!     let id = Id::Standard(StandardId::new(0x123).unwrap());
//!     let mut can_frame_stream = socket
//!         .filter(id, ival, ival)
//!         .unwrap();
//!
//!     while let Some(frame) = can_frame_stream.next().await {
//!         println!("Frame {:?}", frame);
//!         ()
//!     }
//! }
//! ```

use bitflags::bitflags;
pub use embedded_can::{ExtendedId, Id, StandardId};
use futures::prelude::*;
use futures::ready;
use futures::task::Context;
use libc::{
    c_int, c_short, c_uint, c_void, close, connect, fcntl, read, sockaddr, socket, suseconds_t,
    time_t, timeval, write, F_SETFL, O_NONBLOCK,
};
use mio::{event, unix::SourceFd, Interest, Registry, Token};
use nix::net::if_::if_nametoindex;
use std::io::{Error, ErrorKind};
use std::mem::size_of;
use std::pin::Pin;
use std::task::Poll;
use std::{
    fmt,
    os::unix::prelude::{AsRawFd, RawFd},
};
use std::{io, slice, time};
use tokio::io::unix::AsyncFd;

// reexport socketcan CANFrame
pub use socketcan::CANFrame;

/// defined in socket.h
pub const AF_CAN: c_int = 29;
/// defined in socket.h
pub const PF_CAN: c_int = AF_CAN;

pub const CAN_BCM: c_int = 2;

/// datagram (connection less) socket
pub const SOCK_DGRAM: c_int = 2;

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

pub struct BcmFrameStream {
    socket: BCMSocket,
}

impl BcmFrameStream {
    pub fn new(socket: BCMSocket) -> io::Result<BcmFrameStream> {
        Ok(BcmFrameStream { socket })
    }
}

impl Stream for BcmFrameStream {
    type Item = io::Result<CANFrame>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        loop {
            let mut ready_guard = ready!(self.socket.fd.poll_read_ready(cx))?;

            match ready_guard.try_io(|_inner| self.socket.read_msg()) {
                Ok(Ok(msg_head)) => {
                    if let Some(frame) = msg_head.frames().iter().next() {
                        return Poll::Ready(Some(Ok(*frame)));
                    } else {
                        // This happens e.g. when a timed out msg is received
                        return Poll::Pending;
                    }
                }
                Ok(Err(io_err)) => return Poll::Ready(Some(Err(io_err))),
                Err(_would_block) => continue,
            }
        }
    }
}

impl event::Source for BcmFrameStream {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.socket.as_raw_fd()).register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.socket.as_raw_fd()).reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        SourceFd(&self.socket.as_raw_fd()).deregister(registry)
    }
}

/// A socket for a CAN device, specifically for broadcast manager operations.
#[derive(Debug)]
pub struct BCMSocket {
    pub(crate) fd: AsyncFd<c_int>,
}

impl BCMSocket {
    /// Open a named CAN device non blocking.
    ///
    /// Usually the more common case, opens a socket can device by name, such
    /// as "vcan0" or "socan0".
    pub fn open_nb(ifname: &str) -> io::Result<BCMSocket> {
        let if_index = if_nametoindex(ifname).map_err(|nix_error| {
            if let nix::Error::Sys(err_no) = nix_error {
                io::Error::from(err_no)
            } else {
                panic!("unexpected nix error type: {:?}", nix_error)
            }
        })?;
        BCMSocket::open_if_nb(if_index)
    }

    /// Open CAN device by interface number non blocking.
    ///
    /// Opens a CAN device by kernel interface number.
    pub fn open_if_nb(if_index: c_uint) -> io::Result<BCMSocket> {
        // open socket
        let sock_fd;
        unsafe {
            sock_fd = socket(PF_CAN, SOCK_DGRAM, CAN_BCM);
        }

        if sock_fd == -1 {
            return Err(io::Error::last_os_error());
        }

        let fcntl_resp = unsafe { fcntl(sock_fd, F_SETFL, O_NONBLOCK) };

        if fcntl_resp == -1 {
            return Err(io::Error::last_os_error());
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
            return Err(io::Error::last_os_error());
        }

        let sock_fd = AsyncFd::new(sock_fd)?;

        Ok(BCMSocket { fd: sock_fd })
    }

    fn close(&mut self) -> io::Result<()> {
        unsafe {
            let rv = close(self.fd.as_raw_fd());
            if rv != -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    ///
    /// Filter CAN frames by CAN identifier.
    /// ```no_run
    /// use std::time;
    /// use tokio_socketcan_bcm::*;
    /// use futures_util::stream::StreamExt;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let socket = BCMSocket::open_nb("vcan0").unwrap();
    ///     let ival = time::Duration::from_millis(0);
    ///
    ///     // create a stream of messages that filters by the can frame id 0x123
    ///     let id = Id::Standard(StandardId::new(0x123).unwrap());
    ///     let mut can_frame_stream = socket
    ///         .filter(id, ival, ival)
    ///         .unwrap();
    ///
    ///     while let Some(frame) = can_frame_stream.next().await {
    ///         println!("Frame {:?}", frame);
    ///         ()
    ///     }
    /// }
    /// ```
    ///
    pub fn filter(
        self,
        can_id: Id,
        ival1: time::Duration,
        ival2: time::Duration,
    ) -> io::Result<impl Stream<Item = io::Result<CANFrame>>> {
        self.filter_id_internal(can_id, ival1, ival2)?;
        BcmFrameStream::new(self)
    }

    ///
    /// Stream of incoming `BcmMsgHead` that apply to the filter criteria.
    /// ```no_run
    /// use std::time;
    /// use tokio_socketcan_bcm::*;
    /// use futures_util::stream::StreamExt;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let socket = BCMSocket::open_nb("vcan0").unwrap();
    ///     let ival = time::Duration::from_millis(0);
    ///
    ///     // create a stream of messages that filters by the can frame id 0x123
    ///     let mut can_frame_stream = socket
    ///         .incoming_msg()
    ///         .unwrap();
    ///
    ///     while let Some(frame) = can_frame_stream.next().await {
    ///         println!("Frame {:?}", frame);
    ///         ()
    ///     }
    /// }
    /// ```
    ///
    pub fn filter_message(
        self,
        can_id: Id,
        ival1: time::Duration,
        ival2: time::Duration,
    ) -> io::Result<impl Stream<Item = io::Result<BcmMsgHead>>> {
        self.filter_id_internal(can_id, ival1, ival2)?;
        BcmStream::from(self)
    }

    /// Create a content filter subscription, filtering can frames by can_id.
    fn filter_id_internal(
        &self,
        can_id: Id,
        ival1: time::Duration,
        ival2: time::Duration,
    ) -> io::Result<()> {
        let _ival1 = c_timeval_new(ival1);
        let _ival2 = c_timeval_new(ival2);

        let frames = [CANFrame::new(0x0, &[], false, false).unwrap(); MAX_NFRAMES as usize];
        let can_id = match can_id {
            Id::Standard(sid) => sid.as_raw().into(),
            Id::Extended(eid) => eid.as_raw() | FrameFlags::EFF_FLAG.bits(),
        };

        let msg = BcmMsgHeadFrameLess {
            _opcode: RX_SETUP,
            _flags: SETTIMER | RX_FILTER_ID,
            _count: 0,
            #[cfg(all(target_pointer_width = "32"))]
            _pad: 0,
            _ival1,
            _ival2,
            _can_id: can_id,
            _nframes: 0,
        };

        let tx_msg = &TxMsg {
            _msg_head: msg,
            _frames: frames,
        };

        let tx_msg_ptr = tx_msg as *const TxMsg;

        let write_rv = unsafe {
            write(
                self.fd.as_raw_fd(),
                tx_msg_ptr as *const c_void,
                size_of::<TxMsg>(),
            )
        };

        if write_rv < 0 {
            return Err(Error::new(ErrorKind::WriteZero, io::Error::last_os_error()));
        }

        Ok(())
    }

    /// Create a content filter subscription, filtering can frames by can_id.
    #[deprecated(
        since = "2.0.0",
        note = "Will be removed in the future. Please use the `filter` function instead"
    )]
    pub fn filter_id(
        &self,
        can_id: Id,
        ival1: time::Duration,
        ival2: time::Duration,
    ) -> io::Result<()> {
        self.filter_id_internal(can_id, ival1, ival2)
    }

    ///
    /// Combination of `BCMSocket::filter_id` and `BCMSocket::incoming_frames`.
    /// ```no_run
    /// use std::time;
    /// use tokio_socketcan_bcm::*;
    /// use futures_util::stream::StreamExt;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let socket = BCMSocket::open_nb("vcan0").unwrap();
    ///     let ival = time::Duration::from_millis(0);
    ///
    ///     // create a stream of messages that filters by the can frame id 0x123
    ///     let id = Id::Standard(StandardId::new(0x123).unwrap());
    ///     let mut can_frame_stream = socket
    ///         .filter_id_incoming_frames(id, ival, ival)
    ///         .unwrap();
    ///
    ///     while let Some(frame) = can_frame_stream.next().await {
    ///         println!("Frame {:?}", frame);
    ///         ()
    ///     }
    /// }
    /// ```
    ///
    #[deprecated(
        since = "2.0.0",
        note = "Renamed, please use the `filter` function instead"
    )]
    pub fn filter_id_incoming_frames(
        self,
        can_id: Id,
        ival1: time::Duration,
        ival2: time::Duration,
    ) -> io::Result<impl Stream<Item = io::Result<CANFrame>>> {
        self.filter(can_id, ival1, ival2)
    }

    ///
    /// Stream of incoming BcmMsgHeads that apply to the filter criteria.
    /// ```no_run
    /// use std::time;
    /// use tokio_socketcan_bcm::*;
    /// use futures_util::stream::StreamExt;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let socket = BCMSocket::open_nb("vcan0").unwrap();
    ///     let ival = time::Duration::from_millis(0);
    ///
    ///     // create a stream of messages that filters by the can frame id 0x123
    ///     let mut can_frame_stream = socket
    ///         .incoming_msg()
    ///         .unwrap();
    ///
    ///     while let Some(frame) = can_frame_stream.next().await {
    ///         println!("Frame {:?}", frame);
    ///         ()
    ///     }
    /// }
    /// ```
    ///
    #[deprecated(
        since = "2.0.0",
        note = "Please use the `filter_message` function instead"
    )]
    pub fn incoming_msg(self) -> io::Result<impl Stream<Item = io::Result<BcmMsgHead>>> {
        BcmStream::from(self)
    }

    ///
    /// Stream of incoming frames that apply to the filter criteria.
    /// ```no_run
    /// use std::time;
    /// use tokio_socketcan_bcm::*;
    /// use futures_util::stream::StreamExt;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let socket = BCMSocket::open_nb("vcan0").unwrap();
    ///     let ival = time::Duration::from_millis(0);
    ///
    ///     // create a stream of messages that filters by the can frame id 0x123
    ///     let mut can_frame_stream = socket
    ///         .incoming_frames()
    ///         .unwrap();
    ///
    ///     while let Some(frame) = can_frame_stream.next().await {
    ///         println!("Frame {:?}", frame);
    ///         ()
    ///     }
    /// }
    /// ```
    ///
    #[deprecated(since = "2.0.0", note = "Please use the `filter` function instead")]
    pub fn incoming_frames(self) -> io::Result<impl Stream<Item = io::Result<CANFrame>>> {
        BcmFrameStream::new(self)
    }

    /// Remove a content filter subscription.
    #[deprecated(since = "2.0.0", note = "Will be removed in future versions.")]
    pub fn filter_delete(&self, can_id: Id) -> io::Result<()> {
        let frames = [CANFrame::new(0x0, &[], false, false).unwrap(); MAX_NFRAMES as usize];

        let can_id = match can_id {
            Id::Standard(sid) => sid.as_raw().into(),
            Id::Extended(eid) => eid.as_raw() | FrameFlags::EFF_FLAG.bits(),
        };

        let msg = &BcmMsgHead {
            _opcode: RX_DELETE,
            _flags: 0,
            _count: 0,
            _ival1: c_timeval_new(time::Duration::new(0, 0)),
            _ival2: c_timeval_new(time::Duration::new(0, 0)),
            _can_id: can_id,
            _nframes: 0,
            #[cfg(all(target_pointer_width = "32"))]
            _pad: 0,
            _frames: frames,
        };

        let msg_ptr = msg as *const BcmMsgHead;
        let write_rv = unsafe {
            write(
                self.fd.as_raw_fd(),
                msg_ptr as *const c_void,
                size_of::<BcmMsgHead>(),
            )
        };

        let expected_size = size_of::<BcmMsgHead>() - size_of::<[CANFrame; MAX_NFRAMES as usize]>();
        if write_rv as usize != expected_size {
            let msg = format!("Wrote {} but expected {}", write_rv, expected_size);
            return Err(Error::new(ErrorKind::WriteZero, msg));
        }

        Ok(())
    }

    /// Read a single bcm message.
    fn read_msg(&self) -> io::Result<BcmMsgHead> {
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
        let count = unsafe {
            read(
                self.fd.as_raw_fd(),
                msg_ptr as *mut c_void,
                size_of::<BcmMsgHead>(),
            )
        };

        let last_error = io::Error::last_os_error();
        if count < 0 {
            Err(last_error)
        } else {
            Ok(msg)
        }
    }
}

impl AsRawFd for BCMSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl event::Source for BCMSocket {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.fd.as_raw_fd()).register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.fd.as_raw_fd()).reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        SourceFd(&self.fd.as_raw_fd()).deregister(registry)
    }
}

impl Drop for BCMSocket {
    fn drop(&mut self) {
        self.close().ok(); // ignore result
    }
}

pub struct BcmStream {
    socket: BCMSocket,
}

pub trait IntoBcmStream {
    type Stream: futures::stream::Stream;
    type Error;

    fn into_bcm(self) -> Result<Self::Stream, Self::Error>;
}

impl BcmStream {
    pub fn from(socket: BCMSocket) -> io::Result<BcmStream> {
        Ok(BcmStream { socket })
    }
}

impl Stream for BcmStream {
    type Item = io::Result<BcmMsgHead>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        loop {
            let mut ready_guard = ready!(self.socket.fd.poll_read_ready(cx))?;
            match ready_guard.try_io(|_inner| self.socket.read_msg()) {
                Ok(result) => return Poll::Ready(Some(result)),
                Err(_would_block) => continue,
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

fn c_timeval_new(t: time::Duration) -> timeval {
    timeval {
        tv_sec: t.as_secs() as time_t,
        tv_usec: i64::from(t.subsec_micros()) as suseconds_t,
    }
}
