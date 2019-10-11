// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

//! The guts of the underlying network communication protocol.
//!
//! Only the bits of the protocol that are relevant to consumers of this crate
//! are documented here. To learn more about the inner workings of the
//! communication protocol, refer to the non-documentation comments within
//! this module.

// NOTE(benesch): it is about time to split this module apart, but doing so
// is rather painful, as the pieces are all interdependent. Logical boundaries
// are marked with section headers throughout.

use futures::sink::SinkFromErr;
use futures::stream::{FromErr, Fuse};
use futures::{future, try_ready, Async, AsyncSink, Future, Poll, Sink, StartSend, Stream};
use num_enum::{IntoPrimitive, TryFromPrimitive};
use ore::future::{FutureExt, SinkExt, StreamExt};
use ore::netio::{SniffedStream, SniffingStream};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::convert::TryInto;
use std::fmt;
use std::hash::Hash;
use std::net::SocketAddr;
use std::thread;
use tokio::codec::LengthDelimitedCodec;
use tokio::executor::Executor;
use tokio::io::{self, AsyncRead, AsyncWrite};
use tokio::net::unix::UnixStream;
use tokio::net::TcpStream;
use tokio::runtime::TaskExecutor;
use tokio_serde_bincode::{ReadBincode, WriteBincode};
use uuid::Uuid;

use crate::switchboard::Switchboard;

// === Connections and connection pools ===
//
// This module declares a `Connection` trait, which abstracts over network
// streams, so that the rest of the `comm` package can be generic with respect
// to whether commmunication is happening over TCP streams, Unix streams, and so
// on. It also provides a simple connection pool implementation, as opening too
// many outgoing TCP connections will exhaust the client's outgoing ports.

/// A trait for objects that can serve as the underlying transport layer for
/// `comm` traffic.
///
/// Only [`TcpStream`] and [`SniffedStream`] support is provided at the moment,
/// but support for any owned, thread-safe type which implements [`AsyncRead`]
/// and [`AsyncWrite`] can be added trivially, i.e., by implementing this trait.
pub trait Connection: AsyncRead + AsyncWrite + Send + 'static {
    /// The type that identifies the endpoint when establishing a connection of
    /// this type.
    type Addr: fmt::Debug
        + Eq
        + PartialEq
        + Hash
        + Send
        + Sync
        + Clone
        + Serialize
        + for<'de> Deserialize<'de>
        + Into<Addr>;

    /// Connects to the specified `addr`.
    fn connect(addr: &Self::Addr) -> Box<dyn Future<Item = Self, Error = io::Error> + Send>;

    /// Returns the address of the peer that this connection is connected to.
    fn addr(&self) -> Self::Addr;

    /// Returns a thread-local pool for this connection type, if one exists.
    /// For use only within this crate.
    #[doc(hidden)]
    fn pool() -> Option<Pool<Self>>
    where
        Self: Sized,
    {
        None
    }
}

impl Connection for TcpStream {
    type Addr = SocketAddr;

    fn connect(addr: &Self::Addr) -> Box<dyn Future<Item = Self, Error = io::Error> + Send> {
        Box::new(TcpStream::connect(&addr).map(|conn| {
            conn.set_nodelay(true).expect("set_nodelay call failed");
            conn
        }))
    }

    fn addr(&self) -> Self::Addr {
        self.peer_addr().unwrap()
    }

    fn pool() -> Option<Pool<Self>>
    where
        Self: Sized,
    {
        Some(Pool(&TCP_POOL))
    }
}

impl<C> Connection for SniffedStream<C>
where
    C: Connection,
{
    type Addr = C::Addr;

    fn connect(addr: &Self::Addr) -> Box<dyn Future<Item = Self, Error = io::Error> + Send> {
        Box::new(C::connect(addr).map(|conn| SniffingStream::new(conn).into_sniffed()))
    }

    fn addr(&self) -> Self::Addr {
        self.get_ref().addr()
    }
}

impl Connection for UnixStream {
    type Addr = std::path::PathBuf;

    fn connect(addr: &Self::Addr) -> Box<dyn Future<Item = Self, Error = io::Error> + Send> {
        Box::new(UnixStream::connect(addr))
    }

    fn addr(&self) -> Self::Addr {
        self.peer_addr()
            .unwrap()
            .as_pathname()
            .unwrap()
            .to_path_buf()
    }
}

pub(crate) type Framed<C> = tokio::codec::Framed<C, LengthDelimitedCodec>;

/// Frames `conn` using a length-delimited codec. In other words, it transforms
/// a connection which implements [`AsyncRead`] and [`AsyncWrite`] into an
/// combination [`Sink`] and [`Stream`] which produces/emits byte chunks.
pub(crate) fn framed<C>(conn: C) -> Framed<C>
where
    C: Connection,
{
    let mut codec = LengthDelimitedCodec::new();
    codec.set_max_frame_length(1 << 30 /* 1GiB */);
    Framed::new(conn, codec)
}

/// All known address types for [`Connection`]s.
///
/// The existence of this type is a bit unfortunate. It exists so that
/// [`mpsc::Sender`] does not need to be generic over [`Connection`], as
/// MPSC transmitters are meant to be lightweight and easy to stash in places
/// where a generic parameter might be a hassle. Ideally we'd make an `Addr`
/// trait and store a `Box<dyn Addr>`, but Rust does not currently permit
/// serializing and deserializing trait objects.
#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub enum Addr {
    /// The address type for [`TcpStream`].
    Tcp(<TcpStream as Connection>::Addr),
    /// The address type for [`UnixStream`].
    Unix(<UnixStream as Connection>::Addr),
}

impl From<<TcpStream as Connection>::Addr> for Addr {
    fn from(addr: <TcpStream as Connection>::Addr) -> Addr {
        Addr::Tcp(addr)
    }
}

impl From<<UnixStream as Connection>::Addr> for Addr {
    fn from(addr: <UnixStream as Connection>::Addr) -> Addr {
        Addr::Unix(addr)
    }
}

thread_local! {
    static TCP_POOL: RefCell<BasicPool<TcpStream>> = RefCell::new(BasicPool::default());
}

/// A simple pool for `Connection`s. This pool is not threadsafe, and highly
/// specific to the needs of this crate. It is unlikely to be suitable for use
/// as a generic connection pool.
///
/// The pool is keyed by address, as you might expect, and connections are
/// recycled in FIFO order. Note, however, that connections (`C`s) are not
/// stored directly, but rather framed connections (`Framed<C>`s) are stored.
/// This is because `Framed` has internal buffers that would be discarded when
/// calling `Framed::into_inner()`, potentially leaving the connection in an
/// unknown state.
pub struct BasicPool<C>(HashMap<C::Addr, VecDeque<Framed<C>>>)
where
    C: Connection;

impl<C> BasicPool<C>
where
    C: Connection,
{
    /// Constructs an empty `BasicPool`.
    pub fn default() -> BasicPool<C> {
        BasicPool(HashMap::new())
    }
}

impl<C> BasicPool<C>
where
    C: Connection,
{
    /// Attempts to retrieve an existing connection that is connected to `addr`.
    fn get(&mut self, addr: C::Addr) -> Option<Framed<C>> {
        self.0
            .entry(addr)
            .or_insert_with(|| VecDeque::new())
            .pop_front()
    }

    /// Returns a framed connection to the pool so that it can be reused.
    fn put(&mut self, conn: Framed<C>) {
        let addr = conn.get_ref().addr();
        self.0
            .entry(addr)
            .or_insert_with(|| VecDeque::new())
            .push_back(conn)
    }
}

/// A newtype wrapper for a thread-local `BasicPool`. It simply delegates all
/// methods to the internal `BasicPool` on the currently running thread.
pub struct Pool<C>(&'static thread::LocalKey<RefCell<BasicPool<C>>>)
where
    C: Connection;

impl<C> Pool<C>
where
    C: Connection,
{
    /// Attempts to retrieve an existing connection that is connected to `addr`.
    fn get(&self, addr: C::Addr) -> Option<Framed<C>> {
        self.0.with(|cell| cell.borrow_mut().get(addr))
    }

    /// Returns a framed connection to the pool so that it can be reused.
    fn put(&self, conn: Framed<C>) {
        self.0.with(|cell| cell.borrow_mut().put(conn))
    }
}

// === Protocol guts ===
//
// The following functions handle the details of establishing a new `comm`
// connection. For simplicity, each stream is used unidirectionally. Only the
// side that establishes the connection will send any messages.
//
// A connection begins with a simple handshake that identifies the type of
// traffic. There are two types of connections: rendezvous connections and
// channel connections. Rendezvous connections are created by
// `Switchboard::rendezvous` and are used only to validate that the node is
// alive and well; once the rendezvous is complete, any rendezvous connections
// are handed back to the caller of `Switchboard::rendezvous` and are no longer
// managed by this crate. Channel connections are created as necessary to
// support new MPSC and broadcast channels. Channel connections are pooled, and
// so can outlive the channel they were created to service; that is, a given
// channel connection may carry traffic for any number of channels over its
// lifetime, but will only carry traffic for one channel at a time.
//
// The handshake begins with the eight byte `PROTOCOL_MAGIC` magic number, which
// makes it easy to sniff out `comm` traffic from other traffic flowing over the
// same port. Then comes one byte identifying the connection type, whose value
// is determined by the `TrafficType` enum. The remainder of the handshake is
// depends on the connection type.
//
// For rendezvous connections, the 64-bit node ID of the sender is sent along in
// big-endian order, completing the handshake. The connection is then suitable
// for use by other protocols. The rendezvous handshake is designed so that only
// exactly the bytes in the handshake are read from the underlying connection.
// If the handshake were to require framing via `Framed`, for example, it would
// be very difficult to reuse the connection for other purposes, as the `Framed`
// wrapper prefetches on reads and stores extra bytes in an inaccessible buffer.
//
// For channel connections, length-prefixed framing begins immediately. The UUID
// of the desired endpoint is sent along in the first length-prefixed frame.
// After that point, the handshake is considered completed, and future frames on
// the stream are bincode `Message<D>`s that are managed by the `Encoder` and
// `Decoder`. If the channel is closed via a `Message::Hangup`, than the
// connection can be reused via an abbreviated handshake that consists of only
// the channel portion (i.e., sending the framed UUID for the new channel).
// Channels can also be closed by simply closing the connection, which indicates
// that the client does not wish to reuse the connection for a future channel.
//
// Note that there are no backwards/forwards compatibility requirements on this
// protocol whatsoever. Timely already assumes that any nodes in the cluster are
// running exactly the same version of the code, and will panic loudly if not.
// We're therefore free to make the same simplifying assumption.

/// A magic number that is sent along at the beginning of each network
/// connection. The intent is to make it easy to sniff out `comm` traffic when
/// multiple protocols are multiplexed on the same port.
pub const PROTOCOL_MAGIC: [u8; 8] = [0x5f, 0x65, 0x44, 0x90, 0xaf, 0x4b, 0x3c, 0xfc];

/// Reports whether the connection handshake is `comm` traffic by sniffing out
/// whether the first bytes of `buf` match [`PROTOCOL_MAGIC`].
///
/// See [`crate::Switchboard::handle_connection`] for a usage example.
pub fn match_handshake(buf: &[u8]) -> bool {
    if buf.len() < 8 {
        return false;
    }
    buf[..8] == PROTOCOL_MAGIC
}

#[repr(u8)]
#[derive(IntoPrimitive, TryFromPrimitive)]
enum TrafficType {
    Channel,
    Rendezvous,
}

pub(crate) fn send_channel_handshake<C>(
    conn: C,
    uuid: Uuid,
) -> impl Future<Item = Framed<C>, Error = io::Error>
where
    C: Connection,
{
    let mut buf = [0; 9];
    (&mut buf[..8]).copy_from_slice(&PROTOCOL_MAGIC);
    buf[8] = TrafficType::Channel.into();
    io::write_all(conn, buf).and_then(move |(conn, _buf)| {
        let conn = framed(conn);
        conn.send(uuid.as_bytes()[..].into())
    })
}

pub(crate) fn send_rendezvous_handshake<C>(
    conn: C,
    id: u64,
) -> impl Future<Item = C, Error = io::Error>
where
    C: Connection,
{
    let mut buf = [0; 17];
    (&mut buf[..8]).copy_from_slice(&PROTOCOL_MAGIC);
    buf[8] = TrafficType::Rendezvous.into();
    (&mut buf[9..]).copy_from_slice(&id.to_be_bytes());
    io::write_all(conn, buf).map(|(conn, _buf)| conn)
}

pub(crate) enum RecvHandshake<C> {
    Channel(Uuid, Framed<C>),
    Rendezvous(u64, C),
}

pub(crate) fn recv_handshake<C>(conn: C) -> impl Future<Item = RecvHandshake<C>, Error = io::Error>
where
    C: Connection,
{
    io::read_exact(conn, [0; 9]).and_then(|(conn, buf)| {
        assert_eq!(&buf[..8], PROTOCOL_MAGIC);
        match buf[8].try_into().unwrap() {
            TrafficType::Channel => recv_channel_handshake(framed(conn)).left(),
            TrafficType::Rendezvous => recv_rendezvous_handshake(conn).right(),
        }
    })
}

pub(crate) fn recv_channel_handshake<C>(
    conn: Framed<C>,
) -> impl Future<Item = RecvHandshake<C>, Error = io::Error>
where
    C: Connection,
{
    conn.recv().map(move |(bytes, conn)| {
        assert_eq!(bytes.len(), 16);
        let uuid = Uuid::from_slice(&bytes).unwrap();
        RecvHandshake::Channel(uuid, conn)
    })
}

pub(crate) fn recv_rendezvous_handshake<C>(
    conn: C,
) -> impl Future<Item = RecvHandshake<C>, Error = io::Error>
where
    C: Connection,
{
    io::read_exact(conn, [0; 8]).map(move |(conn, buf)| {
        let id = u64::from_be_bytes(buf);
        RecvHandshake::Rendezvous(id, conn)
    })
}

/// === Channel traffic handling ===
///
/// The remaining types and functions handle sending messages across a channel
/// once the connection has been established via the handshake described above.
///
/// Most of the complexity here lies in the interface with the connection pool.
/// At its core, the `Encoder` is just a sink that takes arbitrary Rust datums,
/// bincodes them, and sends them over the wire, while the `Decoder` is just a
/// stream that does the reverse.
///
/// The `Encoder`, however, goes to great pains to look for an existing
/// connection in the thread-local pool, if such a pool exists, and return it to
/// the pool upon completion when dropped. Similarly, the `Decoder` will route
/// closed connections through the `Switchboard` when dropped, in case the
/// client reuses the connection for another channel.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Message<D> {
    Data(D),
    Hangup,
}

pub(crate) type SendSink<D> = Box<dyn Sink<SinkItem = D, SinkError = bincode::Error> + Send>;

/// Creates a new channel directed at the specified `addr` and `uuid`. Returns a
/// [`Sink`] which encodes incoming `D`s using [bincode] and sends them over the
/// connection `conn` with a length prefix.
///
/// [bincode]: https://crates.io/crates/bincode
pub(crate) fn connect_channel<C, D>(
    addr: &C::Addr,
    uuid: Uuid,
) -> impl Future<Item = SendSink<D>, Error = io::Error>
where
    C: Connection,
    D: Serialize + for<'de> Deserialize<'de> + Send + 'static,
{
    let pool = C::pool();
    if let Some(conn) = pool.as_ref().and_then(|pool| pool.get(addr.clone())) {
        conn.send(uuid.as_bytes()[..].into())
            .map(|conn| encoder(conn, pool).boxed())
            .left()
    } else {
        C::connect(addr)
            .and_then(move |conn| send_channel_handshake(conn, uuid))
            .map(|conn| encoder(conn, pool).boxed())
            .right()
    }
}

fn encoder<C, D>(
    framed: Framed<C>,
    pool: Option<Pool<C>>,
) -> impl Sink<SinkItem = D, SinkError = bincode::Error>
where
    C: Connection,
    D: Serialize + for<'de> Deserialize<'de> + Send + 'static,
{
    Encoder {
        inner: Some(WriteBincode::new(framed.sink_from_err::<bincode::Error>())),
        hangup_started: false,
        pool,
    }
}

struct Encoder<C, D>
where
    C: Connection,
    D: Serialize + Send + 'static,
{
    inner: Option<WriteBincode<SinkFromErr<Framed<C>, bincode::Error>, Message<D>>>,
    hangup_started: bool,
    pool: Option<Pool<C>>,
}

impl<C, D> Encoder<C, D>
where
    C: Connection,
    D: Serialize + Send + 'static,
{
    fn inner_mut(&mut self) -> &mut impl Sink<SinkItem = Message<D>, SinkError = bincode::Error> {
        self.inner.as_mut().unwrap()
    }
}

impl<C, D> Sink for Encoder<C, D>
where
    C: Connection,
    D: Serialize + Send + 'static,
{
    type SinkItem = D;
    type SinkError = bincode::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<D, bincode::Error> {
        match self.inner_mut().start_send(Message::Data(item)) {
            Ok(AsyncSink::NotReady(Message::Data(d))) => Ok(AsyncSink::NotReady(d)),
            Ok(AsyncSink::NotReady(Message::Hangup)) => unreachable!(),
            Ok(AsyncSink::Ready) => Ok(AsyncSink::Ready),
            Err(err) => Err(err),
        }
    }

    fn poll_complete(&mut self) -> Poll<(), bincode::Error> {
        self.inner_mut().poll_complete()
    }

    fn close(&mut self) -> Poll<(), bincode::Error> {
        // Don't bother hanging up this connection if there's no pool, since
        // we won't be reusing it.
        if self.pool.is_some() && !self.hangup_started {
            match self.inner_mut().start_send(Message::Hangup) {
                Ok(AsyncSink::Ready) => {
                    self.hangup_started = true;
                }
                Ok(AsyncSink::NotReady(_)) => return Ok(Async::NotReady),
                Err(err) => return Err(err),
            }
        }
        match self.inner_mut().poll_complete() {
            Ok(Async::Ready(())) => {
                let inner = self.inner.take().unwrap();
                if let Some(pool) = &mut self.pool {
                    let inner = inner.into_inner(); // unwrap WriteBincode
                    let inner = inner.into_inner(); // unwrap SinkFromErr
                    pool.put(inner);
                }
                Ok(Async::Ready(()))
            }
            other => other,
        }
    }
}

impl<C, D> Drop for Encoder<C, D>
where
    C: Connection,
    D: Serialize + Send + 'static,
{
    fn drop(&mut self) {
        // NOTE(benesch): it is conceivable that not everyone will want to
        // attempt a synchronous hangup when dropping an encoder, but for now
        // it's convenient.
        if self.inner.is_some() {
            future::poll_fn(|| self.close()).wait().unwrap();
        }
    }
}

/// Constructs a [`Stream`] which decodes bincoded, length-prefixed `D`s from
/// the connection `conn`.
pub(crate) fn decoder<C, D>(
    conn: Framed<C>,
    switchboard: Switchboard<C>,
) -> impl Stream<Item = D, Error = bincode::Error>
where
    C: Connection,
    D: Serialize + for<'de> Deserialize<'de> + Send + 'static,
{
    let decoder = Decoder {
        inner: Some(ReadBincode::new(conn.from_err())),
        switchboard: switchboard.clone(),
    };
    DrainOnDrop::new(decoder, switchboard.executor().clone())
}

struct Decoder<C, D>
where
    C: Connection,
    D: Serialize + for<'de> Deserialize<'de> + Send,
{
    inner: Option<ReadBincode<FromErr<Framed<C>, bincode::Error>, Message<D>>>,
    switchboard: Switchboard<C>,
}

impl<C, D> Stream for Decoder<C, D>
where
    C: Connection,
    D: Serialize + for<'de> Deserialize<'de> + Send,
{
    type Item = D;
    type Error = bincode::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match try_ready!(self.inner.as_mut().unwrap().poll()) {
            None => Ok(Async::Ready(None)),
            Some(Message::Data(d)) => Ok(Async::Ready(Some(d))),
            Some(Message::Hangup) => {
                let inner = self.inner.take().unwrap();
                let conn = inner.into_inner().into_inner();
                let recycle = self.switchboard.recycle_connection(conn).map_err(|_| ());
                try_spawn(&mut self.switchboard.executor().clone(), recycle);
                Ok(Async::Ready(None))
            }
        }
    }
}

struct DrainOnDrop<S>
where
    S: Stream + Send + 'static,
    S::Error: fmt::Display,
{
    inner: Option<Fuse<S>>,
    executor: TaskExecutor,
}

impl<S> DrainOnDrop<S>
where
    S: Stream + Send + 'static,
    S::Error: fmt::Display,
{
    fn new(inner: S, executor: TaskExecutor) -> DrainOnDrop<S> {
        DrainOnDrop {
            inner: Some(inner.fuse()),
            executor,
        }
    }
}

impl<S> Stream for DrainOnDrop<S>
where
    S: Stream + Send + 'static,
    S::Error: fmt::Display,
{
    type Item = S::Item;
    type Error = S::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.inner.as_mut().unwrap().poll()
    }
}

impl<S> Drop for DrainOnDrop<S>
where
    S: Stream + Send + 'static,
    S::Error: fmt::Display,
{
    fn drop(&mut self) {
        let inner = self.inner.take().unwrap();
        let drain = inner.map_err(|_| ()).drain();
        try_spawn(&mut self.executor, drain);
    }
}

// Attempt to spawn `future` on `executor`, but don't panic if the executor
// is shut down.
fn try_spawn<E, F>(executor: &mut E, future: F)
where
    E: Executor,
    F: Future<Item = (), Error = ()> + Send + 'static,
{
    let _ = executor.spawn(future.boxed());
}
