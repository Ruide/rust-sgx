/* Copyright (c) Fortanix, Inc.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#[cfg(all(unix, not(target_abi = "musl")))]
extern crate libc;
#[cfg(all(unix, not(target_abi = "musl")))]
extern crate nix;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, ErrorKind as IoErrorKind, Read, Result as IoResult};
use std::result::Result as StdResult;
use std::str;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::{cmp, fmt};
use std::pin::Pin;

use std::sync::Arc;
use std::thread;
use std::time;
use std::task::{Poll, Context, Waker};

use failure;
use fnv::FnvHashMap;

use futures::StreamExt;
use futures::lock::Mutex;
use futures::future::{Either, FutureExt, Future, poll_fn};
use tokio::prelude::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc as async_mpsc;
use tokio::stream::Stream as TokioStream;

use fortanix_sgx_abi::*;
use sgxs::loader::Tcs as SgxsTcs;
lazy_static! {
    static ref DEBUGGER_TOGGLE_SYNC: Mutex<()> = Mutex::new(());
}

pub(crate) mod abi;
mod interface;

use self::abi::dispatch;
use self::interface::{Handler, OutputBuffer};
#[cfg(all(unix, not(target_abi = "musl")))]
use self::libc::{c_int, c_void, siginfo_t, ucontext_t};
#[cfg(all(unix, not(target_abi = "musl")))]
use self::nix::sys::signal;
use crate::loader::{EnclavePanic, ErasedTcs};
use crate::tcs;
use crate::tcs::{CoResult, ThreadResult};
use std::thread::JoinHandle;

const EV_ABORT: u64 = 0b0000_0000_0000_1000;

type UsercallSendData = (ThreadResult<ErasedTcs>, RunningTcs, RefCell<[u8; 1024]>);

struct ReadOnly<R>(Pin<Box<R>>);
struct WriteOnly<W>(Pin<Box<W>>);

macro_rules! forward {
    (fn $n:ident(mut self: Pin<&mut Self> $(, $p:ident : $t:ty)*) -> $ret:ty) => {
        fn $n(mut self: Pin<&mut Self> $(, $p: $t)*) -> $ret {
            self.0.as_mut().$n($($p),*)
        }
    }
}

impl<R: std::marker::Unpin + AsyncRead> AsyncRead for ReadOnly<R> {
    forward!(fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context, buf: &mut [u8]) -> Poll<tokio::io::Result<usize>>);
}

impl<T> AsyncRead for WriteOnly<T> {
    fn poll_read(self: Pin<&mut Self>, _cx: &mut Context, _buf: &mut [u8]) -> Poll<tokio::io::Result<usize>> {
        Poll::Ready(Err(IoErrorKind::BrokenPipe.into()))
    }
}

impl<T> AsyncWrite for ReadOnly<T> {
    fn poll_write(self: Pin<&mut Self>, _cx: &mut Context, _buf: &[u8]) -> Poll<tokio::io::Result<usize>> {
        Poll::Ready(Err(IoErrorKind::BrokenPipe.into()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<tokio::io::Result<()>> {
        Poll::Ready(Err(IoErrorKind::BrokenPipe.into()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<tokio::io::Result<()>> {
        Poll::Ready(Err(IoErrorKind::BrokenPipe.into()))
    }
}

impl<W: std::marker::Unpin + AsyncWrite> AsyncWrite for WriteOnly<W> {
    forward!(fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<tokio::io::Result<usize>>);
    forward!(fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<tokio::io::Result<()>>);
    forward!(fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<tokio::io::Result<()>>);
}

struct Stdin;

impl AsyncRead for Stdin {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut [u8]) -> Poll<tokio::io::Result<usize>> {
        const BUF_SIZE: usize = 8192;

        trait AsIoResult<T> {
            fn as_io_result(self) -> io::Result<T>;
        }

        impl<T> AsIoResult<T> for Poll<T> {
            fn as_io_result(self) -> io::Result<T> {
                match self {
                    Poll::Ready(v) => Ok(v),
                    Poll::Pending => Err(io::ErrorKind::WouldBlock.into()),
                }
            }
        }

        struct AsyncStdin {
            rx: async_mpsc::Receiver<VecDeque<u8>>,
            buf: VecDeque<u8>,
        }

        lazy_static::lazy_static! {
            static ref STDIN: Mutex<AsyncStdin> = {
                let (mut tx, rx) = async_mpsc::channel(8);
                thread::spawn(move || {
                    let mut buf = [0u8; BUF_SIZE];
                    while let Ok(len) = io::stdin().read(&mut buf) {
                        if len == 0 {
                            continue
                        }

                        if tx.try_send(buf[..len].to_vec().into()).is_err() {
                            return
                        };
                    }
                });
                Mutex::new(AsyncStdin { rx, buf: VecDeque::new() })
            };
        }

        match Pin::new(&mut STDIN.lock()).poll(cx) {
            Poll::Ready(mut stdin) => {
                if stdin.buf.is_empty() {
                    let pipeerr = tokio::io::Error::new(tokio::io::ErrorKind::BrokenPipe, "broken pipe");
                    stdin.buf = match Pin::new(&mut stdin.rx).poll_next(cx) {
                        Poll::Ready(Some(vec)) => vec,
                        Poll::Ready(None) => return Poll::Ready(Err(pipeerr)),
                        _ => return Poll::Pending,
                    };
                }
                let inbuf = match stdin.buf.as_slices() {
                    (&[], inbuf) => inbuf,
                    (inbuf, _) => inbuf,
                };
                let len = cmp::min(buf.len(), inbuf.len());
                buf[..len].copy_from_slice(&inbuf[..len]);
                stdin.buf.drain(..len);
                Poll::Ready(Ok(len))
            }
            Poll::Pending => Poll::Pending
        }
    }
}

pub trait AsyncStream: AsyncRead + AsyncWrite + 'static + Send + Sync {
    fn poll_read_alloc(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<Vec<u8>>>
    {
        let mut v: Vec<u8> = vec![0; 8192];
        self.poll_read(cx, v.as_mut_slice()).map(move |res| {
            res.map(|size| { v.truncate(size); v })
        })
    }
}

impl<S: AsyncRead + AsyncWrite + Sync + Send + 'static> AsyncStream for S {}

/// AsyncListener lets an implementation implement a slightly modified form of `std::net::TcpListener::accept`.
pub trait AsyncListener: 'static + Send {
    /// The enclave may optionally request the local or peer addresses
    /// be returned in `local_addr` or `peer_addr`, respectively.
    /// If `local_addr` and/or `peer_addr` are not `None`, they will point to an empty `String`.
    /// On success, user-space can fill in the strings as appropriate.
    ///
    /// The enclave must not make any security decisions based on the local address received.
    fn poll_accept(
        self: Pin<&mut Self>,
        cx: &mut Context,
        local_addr: Option<&mut String>,
        peer_addr: Option<&mut String>,
    ) -> Poll<tokio::io::Result<Option<Box<dyn AsyncStream>>>>;
}

struct AsyncStreamAdapter {
    stream: Pin<Box<dyn AsyncStream>>,
    read_queue: VecDeque<Waker>,
    write_queue: VecDeque<Waker>,
    flush_queue: VecDeque<Waker>,
}

fn notify_other_tasks(cx: &mut Context, queue: &mut VecDeque<Waker>) {
    for task in queue.drain(..) {
        if !task.will_wake(&cx.waker()) {
            task.wake();
        }
    }
}

impl AsyncStreamAdapter {
    fn poll_read_alloc(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<tokio::io::Result<Vec<u8>>> {
        match self.stream.as_mut().poll_read_alloc(cx) {
            Poll::Pending => {
                self.read_queue.push_back(cx.waker().clone());
                Poll::Pending
            }
            Poll::Ready(Ok(ret)) => {
                notify_other_tasks(cx, &mut self.read_queue);
                Poll::Ready(Ok(ret))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        }
    }

    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context, buf: &mut [u8]) -> Poll<tokio::io::Result<usize>> {
        match self.stream.as_mut().poll_read(cx, buf) {
            Poll::Pending => {
                self.read_queue.push_back(cx.waker().clone());
                Poll::Pending
            }
            Poll::Ready(Ok(ret)) => {
                notify_other_tasks(cx, &mut self.read_queue);
                Poll::Ready(Ok(ret))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        }
    }

    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<tokio::io::Result<usize>> {
        match self.stream.as_mut().poll_write(cx, buf) {
            Poll::Pending => {
                self.write_queue.push_back(cx.waker().clone());
                Poll::Pending
            }
            Poll::Ready(Ok(ret)) => {
                notify_other_tasks(cx, &mut self.write_queue);
                Poll::Ready(Ok(ret))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<tokio::io::Result<()>> {
        match self.stream.as_mut().poll_flush(cx) {
            Poll::Pending => {
                self.flush_queue.push_back(cx.waker().clone());
                Poll::Pending
            }
            Poll::Ready(Ok(ret)) => {
                notify_other_tasks(cx, &mut self.flush_queue);
                Poll::Ready(Ok(ret))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        }
    }
}

struct AsyncStreamContainer {
    inner: Mutex<Pin<Box<AsyncStreamAdapter>>>,
}

impl AsyncStreamContainer {
    fn new(s: Box<dyn AsyncStream>) -> Self {
        AsyncStreamContainer {
            inner: Mutex::new(Box::pin(AsyncStreamAdapter {
                stream: s.into(),
                read_queue: VecDeque::new(),
                write_queue: VecDeque::new(),
                flush_queue: VecDeque::new(),
            })),
        }
    }

    async fn async_read(&self, buf: &mut [u8]) -> IoResult<usize> {
        poll_fn(|cx| {
            let inner_ref = &mut self.inner.lock();
            let mut inner = Pin::new(inner_ref);
            match inner.as_mut().poll(cx) {
                Poll::Ready(mut adapter) => adapter.as_mut().poll_read(cx, buf),
                Poll::Pending => Poll::Pending,
            }
        }).await
    }

    async fn async_read_alloc(&self) -> IoResult<Vec<u8>> {
        poll_fn(|cx| {
            let inner_ref = &mut self.inner.lock();
            let mut inner = Pin::new(inner_ref);
            match inner.as_mut().poll(cx) {
                Poll::Ready(mut adapter) => adapter.as_mut().poll_read_alloc(cx),
                Poll::Pending => Poll::Pending,
            }
        }).await
    }

    async fn async_write(&self, buf: &[u8]) -> IoResult<usize> {
        poll_fn(|cx| {
            let inner_ref = &mut self.inner.lock();
            let mut inner = Pin::new(inner_ref);
            match inner.as_mut().poll(cx) {
                Poll::Ready(mut adapter) => adapter.as_mut().poll_write(cx, buf),
                Poll::Pending => Poll::Pending,
            }
        }).await
    }

    async fn async_flush(&self) -> IoResult<()> {
        poll_fn(|cx| {
            let inner_ref = &mut self.inner.lock();
            let mut inner = Pin::new(inner_ref);
            match inner.as_mut().poll(cx) {
                Poll::Ready(mut adapter) => adapter.as_mut().poll_flush(cx),
                Poll::Pending => Poll::Pending,
            }
        }).await
    }
}

struct AsyncListenerAdapter {
    listener: Pin<Box<dyn AsyncListener>>,
    accept_queue: VecDeque<Waker>,
}

impl AsyncListenerAdapter {
    fn poll_accept(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        local_addr: Option<&mut String>,
        peer_addr: Option<&mut String>
    ) -> Poll<tokio::io::Result<Option<Box<dyn AsyncStream>>>> {
        match self.listener.as_mut().poll_accept(cx, local_addr, peer_addr) {
            Poll::Pending => {
                self.accept_queue.push_back(cx.waker().clone());
                Poll::Pending
            }
            Poll::Ready(Ok(ret)) => {
                notify_other_tasks(cx, &mut self.accept_queue);
                Poll::Ready(Ok(ret))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        }
    }
}

struct AsyncListenerContainer {
    inner: Mutex<Pin<Box<AsyncListenerAdapter>>>,
}

impl AsyncListenerContainer {
    fn new(l: Box<dyn AsyncListener>) -> Self {
        AsyncListenerContainer {
            inner: Mutex::new(Pin::new(Box::new(AsyncListenerAdapter {
                listener: l.into(),
                accept_queue: VecDeque::new(),
            }))),
        }
    }

    async fn async_accept(&self, local_addr: Option<&mut String>, peer_addr: Option<&mut String>) -> IoResult<Option<Box<dyn AsyncStream>>> {
        let mut local_addr_owned: Option<String> = if local_addr.is_some() { Some(String::new()) } else { None };
        let mut peer_addr_owned: Option<String> = if peer_addr.is_some() { Some(String::new()) } else { None };
        let res = poll_fn(|cx| {
            let inner_ref = &mut self.inner.lock();
            let mut inner = Pin::new(inner_ref);
            match inner.as_mut().poll(cx) {
                Poll::Ready(mut adapter) => adapter.as_mut().poll_accept(cx, local_addr_owned.as_mut(), peer_addr_owned.as_mut()),
                Poll::Pending => Poll::Pending,
            }
        }).await;

        if let Some(local_addr) = local_addr {
            *local_addr = local_addr_owned.unwrap();
        }
        if let Some(peer_addr) = peer_addr {
            *peer_addr = peer_addr_owned.unwrap();
        }
        res
    }
}

impl AsyncListener for tokio::net::TcpListener {
    fn poll_accept(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        local_addr: Option<&mut String>,
        peer_addr: Option<&mut String>,
    ) -> Poll<tokio::io::Result<Option<Box<dyn AsyncStream>>>> {
        let mut incoming = self.incoming();
        let inner = Pin::new(&mut incoming);
        match inner.poll_next(cx) {
            Poll::Ready(Some(Ok(stream))) => {
                if let Some(local_addr) = local_addr {
                    *local_addr = stream.local_addr().map(|addr| addr.to_string()).unwrap_or_else(|_err| "error".to_owned());
                }
                if let Some(peer_addr) = peer_addr {
                    *peer_addr = stream.peer_addr().map(|addr| addr.to_string()).unwrap_or_else(|_err| "error".to_owned());
                }
                Poll::Ready(Ok(Some(Box::new(stream))))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Err(e)),
            Poll::Ready(None) => Poll::Ready(Ok(None)),
            Poll::Pending => Poll::Pending,
        }
    }
}

enum AsyncFileDesc {
    Stream(AsyncStreamContainer),
    Listener(AsyncListenerContainer),
}

impl AsyncFileDesc {
    fn stream(s: Box<dyn AsyncStream>) -> AsyncFileDesc {
        AsyncFileDesc::Stream(AsyncStreamContainer::new(s))
    }

    fn listener(l: Box<dyn AsyncListener>) -> AsyncFileDesc {
        AsyncFileDesc::Listener(AsyncListenerContainer::new(l))
    }

    fn as_stream(&self) -> IoResult<&AsyncStreamContainer> {
        if let AsyncFileDesc::Stream(ref s) = self {
            Ok(s)
        } else {
            Err(IoErrorKind::InvalidInput.into())
        }
    }

    fn as_listener(&self) -> IoResult<&AsyncListenerContainer> {
        if let AsyncFileDesc::Listener(ref l) = self {
            Ok(l)
        } else {
            Err(IoErrorKind::InvalidInput.into())
        }
    }
}

#[derive(Debug)]
pub(crate) enum EnclaveAbort<T> {
    Exit {
        panic: T,
    },
    /// Secondary threads exiting due to an abort
    Secondary,
    IndefiniteWait,
    InvalidUsercall(u64),
    MainReturned,
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
struct TcsAddress(usize);

impl ErasedTcs {
    fn address(&self) -> TcsAddress {
        TcsAddress(SgxsTcs::address(self) as _)
    }
}

impl fmt::Pointer for TcsAddress {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        (self.0 as *const u8).fmt(f)
    }
}

struct StoppedTcs {
    tcs: ErasedTcs,
    event_queue: futures::channel::mpsc::UnboundedReceiver<u8>,
}

struct IOHandlerInput<'tcs> {
    tcs: &'tcs mut RunningTcs,
    enclave: Arc<EnclaveState>,
    work_sender: &'tcs crossbeam::crossbeam_channel::Sender<Work>,
}

struct RunningTcs {
    pending_event_set: u8,
    pending_events: VecDeque<u8>,
    event_queue: futures::channel::mpsc::UnboundedReceiver<u8>,
    mode: EnclaveEntry,
}

enum EnclaveKind {
    Command(Command),
    Library(Library),
}

struct PanicReason {
    primary_panic_reason: Option<EnclaveAbort<EnclavePanic>>,
    other_reasons: Vec<EnclaveAbort<EnclavePanic>>,
}

struct Command {
    panic_reason: Mutex<PanicReason>,
}

struct Library {}

impl EnclaveKind {
    fn as_command(&self) -> Option<&Command> {
        match self {
            EnclaveKind::Command(c) => Some(c),
            _ => None,
        }
    }

    fn _as_library(&self) -> Option<&Library> {
        match self {
            EnclaveKind::Library(l) => Some(l),
            _ => None,
        }
    }
}

pub(crate) struct EnclaveState {
    kind: EnclaveKind,
    event_queues: FnvHashMap<TcsAddress, futures::channel::mpsc::UnboundedSender<u8>>,
    fds: Mutex<FnvHashMap<Fd, Arc<AsyncFileDesc>>>,
    last_fd: AtomicUsize,
    exiting: AtomicBool,
    usercall_ext: Box<dyn UsercallExtension>,
    threads_queue: crossbeam::queue::SegQueue<StoppedTcs>,
    forward_panics: bool,
}

struct Work {
    tcs: RunningTcs,
    entry: CoEntry,
}

enum CoEntry {
    Initial(ErasedTcs, u64, u64, u64, u64, u64),
    Resume(tcs::Usercall<ErasedTcs>, (u64, u64)),
}

impl Work {
    fn do_work(self, io_send_queue: &mut tokio::sync::mpsc::UnboundedSender<UsercallSendData>) {
        let buf = RefCell::new([0u8; 1024]);
        let usercall_send_data = match self.entry {
            CoEntry::Initial(erased_tcs, p1, p2, p3, p4, p5) => {
                let coresult = tcs::coenter(erased_tcs, p1, p2, p3, p4, p5, Some(&buf));
                (coresult, self.tcs, buf)
            }
            CoEntry::Resume(usercall, coresult) => {
                let coresult = usercall.coreturn(coresult, Some(&buf));
                (coresult, self.tcs, buf)
            }
        };
        // if there is an error do nothing, as it means that the main thread has exited
        let _ = io_send_queue.send(usercall_send_data);
    }
}

impl EnclaveState {
    fn event_queue_add_tcs(
        event_queues: &mut FnvHashMap<TcsAddress, futures::channel::mpsc::UnboundedSender<u8>>,
        tcs: ErasedTcs,
    ) -> StoppedTcs {
        let (send, recv) = futures::channel::mpsc::unbounded();
        if event_queues.insert(tcs.address(), send).is_some() {
            panic!("duplicate TCS address: {:p}", tcs.address())
        }
        StoppedTcs {
            tcs,
            event_queue: recv,
        }
    }

    fn new(
        kind: EnclaveKind,
        mut event_queues: FnvHashMap<TcsAddress, futures::channel::mpsc::UnboundedSender<u8>>,
        usercall_ext: Option<Box<dyn UsercallExtension>>,
        threads_vector: Vec<ErasedTcs>,
        forward_panics: bool,
    ) -> Arc<Self> {
        let mut fds = FnvHashMap::default();

        fds.insert(
            FD_STDIN,
            Arc::new(AsyncFileDesc::stream(Box::new(ReadOnly(
                Box::pin(Stdin)
            )))),
        );
        fds.insert(
            FD_STDOUT,
            Arc::new(AsyncFileDesc::stream(Box::new(WriteOnly(
                Box::pin(tokio::io::stdout()),
            )))),
        );
        fds.insert(
            FD_STDERR,
            Arc::new(AsyncFileDesc::stream(Box::new(WriteOnly(
                Box::pin(tokio::io::stderr()),
            )))),
        );
        let last_fd = AtomicUsize::new(fds.keys().cloned().max().unwrap() as _);

        let usercall_ext = usercall_ext.unwrap_or_else(|| Box::new(UsercallExtensionDefault));

        let threads_queue = crossbeam::queue::SegQueue::new();

        for thread in threads_vector {
            threads_queue.push(Self::event_queue_add_tcs(&mut event_queues, thread));
        }

        Arc::new(EnclaveState {
            kind,
            event_queues,
            fds: Mutex::new(fds),
            last_fd,
            exiting: AtomicBool::new(false),
            usercall_ext,
            threads_queue,
            forward_panics,
        })
    }

    fn syscall_loop(
        enclave: Arc<EnclaveState>,
        io_queue_receive: tokio::sync::mpsc::UnboundedReceiver<UsercallSendData>,
        work_sender: crossbeam::crossbeam_channel::Sender<Work>,
    ) -> StdResult<(u64, u64), EnclaveAbort<EnclavePanic>> {
        let (tx_return_channel, mut rx_return_channel) = tokio::sync::mpsc::unbounded_channel();
        let enclave_clone = enclave.clone();
        let mut rt = tokio::runtime::Builder::new()
            .basic_scheduler()
            .enable_all()
            .build()
            .expect("failed to create tokio Runtime");
        let local_set = tokio::task::LocalSet::new();

        let return_future = async move {
            while let (Some(work), stream) = rx_return_channel.into_future().await {
                rx_return_channel = stream;
                let (my_result, mode) = work;
                let res = match (my_result, mode) {
                    (e, EnclaveEntry::Library)
                    | (e, EnclaveEntry::ExecutableMain)
                    | (e @ Err(EnclaveAbort::Secondary), EnclaveEntry::ExecutableNonMain) => e,
                    (Ok(_), EnclaveEntry::ExecutableNonMain) => {
                        continue;
                    }
                    (Err(e @ EnclaveAbort::Exit { .. }), EnclaveEntry::ExecutableNonMain)
                    | (
                        Err(e @ EnclaveAbort::InvalidUsercall(_)),
                        EnclaveEntry::ExecutableNonMain,
                    ) => {
                        let cmd = enclave_clone.kind.as_command().unwrap();
                        let mut cmddata = cmd.panic_reason.lock().await;

                        if cmddata.primary_panic_reason.is_none() {
                            cmddata.primary_panic_reason = Some(e)
                        } else {
                            cmddata.other_reasons.push(e)
                        }
                        Err(EnclaveAbort::Secondary)
                    }
                    (Err(e), EnclaveEntry::ExecutableNonMain) => {
                        let cmd = enclave_clone.kind.as_command().unwrap();
                        let mut cmddata = cmd.panic_reason.lock().await;
                        cmddata.other_reasons.push(e);
                        continue;
                    }
                };
                return res;
            }
            unreachable!();
        };
        let enclave_clone = enclave.clone();
        let io_future = async move {
            let mut recv_queue = io_queue_receive.into_future();
            while let (Some(work), stream) = recv_queue.await {
                let work_sender = work_sender.clone();
                let tx_return_channel = tx_return_channel.clone();
                let enclave_clone = enclave_clone.clone();
                recv_queue = stream.into_future();
                let (coresult, mut state, buf) = work;
                match coresult {
                    CoResult::Yield(usercall) => {
                        let fut = async move {
                            let mut input = IOHandlerInput {
                                enclave: enclave_clone.clone(),
                                tcs: &mut state,
                                work_sender: &work_sender,
                            };
                            let handler = Handler(&mut input);
                            let (_handler, result) = {
                                let (p1, p2, p3, p4, p5) = usercall.parameters();
                                dispatch(handler, p1, p2, p3, p4, p5).await
                            };
                            let ret = match result {
                                Ok(ret) => {
                                    work_sender
                                        .send(Work {
                                            tcs: state,
                                            entry: CoEntry::Resume(usercall, ret),
                                        })
                                        .expect("Work sender couldn't send data to receiver");
                                    return;
                                }
                                Err(EnclaveAbort::Exit { panic: true }) => {
                                    println!("Attaching debugger");
                                    #[cfg(all(unix, not(target_abi = "musl")))]
                                    trap_attached_debugger(usercall.tcs_address() as _).await;
                                    let panic = EnclavePanic::from(buf.into_inner());
                                    if enclave_clone.forward_panics {
                                        panic!("{}", &panic);
                                    }
                                    Err(EnclaveAbort::Exit{ panic })

                                }
                                Err(EnclaveAbort::Exit { panic: false }) => Ok((0, 0)),
                                Err(EnclaveAbort::IndefiniteWait) => {
                                    Err(EnclaveAbort::IndefiniteWait)
                                }
                                Err(EnclaveAbort::InvalidUsercall(n)) => {
                                    Err(EnclaveAbort::InvalidUsercall(n))
                                }
                                Err(EnclaveAbort::MainReturned) => Err(EnclaveAbort::MainReturned),
                                Err(EnclaveAbort::Secondary) => Err(EnclaveAbort::Secondary),
                            };
                            let _ = tx_return_channel.send((ret, state.mode));
                        };
                        tokio::task::spawn_local(fut);
                    }
                    CoResult::Return((tcs, v1, v2)) => {
                        let fut = async move {
                            let ret = match state.mode {
                                EnclaveEntry::Library => {
                                    enclave_clone.threads_queue.push(StoppedTcs {
                                        tcs,
                                        event_queue: state.event_queue,
                                    });
                                    Ok((v1, v2))
                                }
                                EnclaveEntry::ExecutableMain => Err(EnclaveAbort::MainReturned),
                                EnclaveEntry::ExecutableNonMain => {
                                    assert_eq!(
                                        (v1, v2),
                                        (0, 0),
                                        "Expected enclave thread entrypoint to return zero"
                                    );
                                    // If the enclave is in the exit-state, threads are no
                                    // longer able to be launched
                                    if !enclave_clone.exiting.load(Ordering::SeqCst) {
                                        enclave_clone.threads_queue.push(StoppedTcs {
                                            tcs,
                                            event_queue: state.event_queue,
                                        });
                                    }
                                    Ok((0, 0))
                                }
                            };
                            let _ = tx_return_channel.send((ret, state.mode));
                        };
                        tokio::task::spawn_local(fut);
                    }
                };
            }
            unreachable!();
        };

        // Note that:
        // - io_future will never return, its job is to spawn new futures that handle I/O.
        // - return_future returns in certain cases (see above) and in such cases we want to
        //   terminate the syscall loop.
        let select_fut =
            futures::future::select(return_future.boxed_local(), io_future.boxed_local()).map( |either| {
                match either {
                    Either::Left((x, _)) => x,
                    _ => unreachable!(),
                }
            });

        local_set.block_on(&mut rt, select_fut.unit_error()).unwrap()
    }

    fn run(
        enclave: Arc<EnclaveState>,
        num_of_worker_threads: usize,
        start_work: Work,
    ) -> StdResult<(u64, u64), EnclaveAbort<EnclavePanic>> {
        fn create_worker_threads(
            num_of_worker_threads: usize,
            work_receiver: crossbeam::crossbeam_channel::Receiver<Work>,
            io_queue_send: tokio::sync::mpsc::UnboundedSender<UsercallSendData>,
        ) -> Vec<JoinHandle<()>> {
            let mut thread_handles = vec![];
            for _ in 0..num_of_worker_threads {
                let work_receiver = work_receiver.clone();
                let mut io_queue_send = io_queue_send.clone();

                thread_handles.push(thread::spawn(move || {
                    while let Ok(work) = work_receiver.recv() {
                        work.do_work(&mut io_queue_send);
                    }
                }));
            }
            thread_handles
        }

        let (io_queue_send, io_queue_receive) = tokio::sync::mpsc::unbounded_channel();

        let (work_sender, work_receiver) = crossbeam::crossbeam_channel::unbounded();
        work_sender
            .send(start_work)
            .expect("Work sender couldn't send data to receiver");

        let join_handlers =
            create_worker_threads(num_of_worker_threads, work_receiver, io_queue_send);
        // main syscall polling loop
        let main_result =
            EnclaveState::syscall_loop(enclave.clone(), io_queue_receive, work_sender);

        for handler in join_handlers {
            let _ = handler.join();
        }
        return main_result;
    }

    pub(crate) fn main_entry(
        main: ErasedTcs,
        threads: Vec<ErasedTcs>,
        usercall_ext: Option<Box<dyn UsercallExtension>>,
        forward_panics: bool,
    ) -> StdResult<(), failure::Error> {
        let mut event_queues =
            FnvHashMap::with_capacity_and_hasher(threads.len() + 1, Default::default());
        let main = Self::event_queue_add_tcs(&mut event_queues, main);

        let main_work = Work {
            tcs: RunningTcs {
                event_queue: main.event_queue,
                pending_event_set: 0,
                pending_events: Default::default(),
                mode: EnclaveEntry::ExecutableMain,
            },
            entry: CoEntry::Initial(main.tcs, 0, 0, 0, 0, 0),
        };

        let num_of_worker_threads = num_cpus::get();

        let kind = EnclaveKind::Command(Command {
            panic_reason: Mutex::new(PanicReason {
                primary_panic_reason: None,
                other_reasons: vec![],
            }),
        });
        let enclave = EnclaveState::new(kind, event_queues, usercall_ext, threads, forward_panics);

        let main_result = EnclaveState::run(enclave.clone(), num_of_worker_threads, main_work);

        let main_panicking = match main_result {
            Err(EnclaveAbort::MainReturned)
            | Err(EnclaveAbort::InvalidUsercall(_))
            | Err(EnclaveAbort::Exit { .. }) => true,
            Err(EnclaveAbort::IndefiniteWait) | Err(EnclaveAbort::Secondary) | Ok(_) => false,
        };

        let mut rt = tokio::runtime::Builder::new()
            .basic_scheduler()
            .enable_all()
            .build()
            .expect("failed to create tokio Runtime");

        rt.block_on(async move {
            enclave.abort_all_threads();
            //clear the threads_queue
            while enclave.threads_queue.pop().is_ok() {}

            let cmd = enclave.kind.as_command().unwrap();
            let mut cmddata = cmd.panic_reason.lock().await;
            let main_result = match (main_panicking, cmddata.primary_panic_reason.take()) {
                (false, Some(reason)) => Err(reason),
                // TODO: interpret other_reasons
                _ => main_result,
            };
            match main_result {
                Err(EnclaveAbort::Exit { panic }) => Err(panic.into()),
                Err(EnclaveAbort::IndefiniteWait) => {
                    bail!("All enclave threads are waiting indefinitely without possibility of wakeup")
                }
                Err(EnclaveAbort::InvalidUsercall(n)) => {
                    bail!("The enclave performed an invalid usercall 0x{:x}", n)
                }
                Err(EnclaveAbort::MainReturned) => bail!(
                    "The enclave returned from the main entrypoint in violation of the specification."
                ),
                // Should always be able to return the real exit reason
                Err(EnclaveAbort::Secondary) => unreachable!(),
                Ok(_) => Ok(()),
            }
        }.boxed_local())
    }

    pub(crate) fn library(
        threads: Vec<ErasedTcs>,
        usercall_ext: Option<Box<dyn UsercallExtension>>,
        forward_panics: bool,
    ) -> Arc<Self> {
        let event_queues = FnvHashMap::with_capacity_and_hasher(threads.len(), Default::default());

        let kind = EnclaveKind::Library(Library {});

        let enclave = EnclaveState::new(kind, event_queues, usercall_ext, threads, forward_panics);
        return enclave;
    }

    pub(crate) fn library_entry(
        enclave: &Arc<Self>,
        p1: u64,
        p2: u64,
        p3: u64,
        p4: u64,
        p5: u64,
    ) -> StdResult<(u64, u64), failure::Error> {
        let thread = enclave.threads_queue.pop().expect("threads queue empty");
        let work = Work {
            tcs: RunningTcs {
                event_queue: thread.event_queue,
                mode: EnclaveEntry::Library,
                pending_event_set: 0,
                pending_events: Default::default(),
            },
            entry: CoEntry::Initial(thread.tcs, p1, p2, p3, p4, p5),
        };
        // As currently we are not supporting spawning threads let the number of threads be 2
        // one for usercall handling the other for actually running
        let num_of_worker_threads = 1;

        let library_result = EnclaveState::run(enclave.clone(), num_of_worker_threads, work);

        match library_result {
            Err(EnclaveAbort::Exit { panic }) => Err(panic.into()),
            Err(EnclaveAbort::IndefiniteWait) => {
                bail!("This thread is waiting indefinitely without possibility of wakeup")
            }
            Err(EnclaveAbort::InvalidUsercall(n)) => {
                bail!("The enclave performed an invalid usercall 0x{:x}", n)
            }
            Err(EnclaveAbort::Secondary) => {
                bail!("This thread exited because another thread aborted")
            }
            Err(EnclaveAbort::MainReturned) => unreachable!(),
            Ok(result) => Ok(result),
        }
    }

    fn abort_all_threads(&self) {
        self.exiting.store(true, Ordering::SeqCst);
        // wake other threads
        for queue in self.event_queues.values() {
            let _ = queue.unbounded_send(EV_ABORT as _);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EnclaveEntry {
    ExecutableMain,
    ExecutableNonMain,
    Library,
}

#[repr(C)]
#[allow(unused)]
enum Greg {
    R8 = 0,
    R9,
    R10,
    R11,
    R12,
    R13,
    R14,
    R15,
    RDI,
    RSI,
    RBP,
    RBX,
    RDX,
    RAX,
    RCX,
    RSP,
    RIP,
    EFL,
    CSGSFS, /* Actually short cs, gs, fs, __pad0. */
    ERR,
    TRAPNO,
    OLDMASK,
    CR2,
}

#[cfg(all(unix, not(target_abi = "musl")))]
/* Here we are passing control to debugger `fixup` style by raising Sigtrap.
 * If there is no debugger attached, this function, would skip the `int3` instructon
 * and resume execution.
 */
extern "C" fn handle_trap(_signo: c_int, _info: *mut siginfo_t, context: *mut c_void) {
    unsafe {
        let context = &mut *(context as *mut ucontext_t);
        let rip = &mut context.uc_mcontext.gregs[Greg::RIP as usize];
        let inst: *const u8 = *rip as _;
        if *inst == 0xcc {
            *rip += 1;
        }
    }
    return;
}

#[cfg(all(unix, not(target_abi = "musl")))]
/* Raising Sigtrap to allow debugger to take control.
 * Here, we also store tcs in rbx, so that the debugger could read it, to
 * set sgx state and correctly map the enclave symbols.
 */
async fn trap_attached_debugger(tcs: usize) {
    let _g = DEBUGGER_TOGGLE_SYNC.lock().await;
    let hdl = self::signal::SigHandler::SigAction(handle_trap);
    let sig_action = signal::SigAction::new(hdl, signal::SaFlags::empty(), signal::SigSet::empty());
    // Synchronized
    unsafe {
        let old = signal::sigaction(signal::SIGTRAP, &sig_action).unwrap();
        asm!("int3" : /* No output */
                    : /*input */ "{rbx}"(tcs)
                    :/* No clobber */
                    :"volatile");
        signal::sigaction(signal::SIGTRAP, &old).unwrap();
    }
}

/// Provides a mechanism for the enclave code to interface with an external service via a modified runner.
///
/// An implementation of `UsercallExtension` can be registered while [building](../struct.EnclaveBuilder.html#method.usercall_extension) the enclave.
pub trait UsercallExtension: 'static + Send + Sync + std::fmt::Debug {
    /// Override the connection target for connect calls by the enclave. The runner should determine the service that the enclave is trying to connect to by looking at addr.
    /// If `connect_stream` returns None, the default implementation of [`connect_stream`](../../fortanix_sgx_abi/struct.Usercalls.html#method.connect_stream) is used.
    /// The enclave may optionally request the local or peer addresses
    /// be returned in `local_addr` or `peer_addr`, respectively.
    /// If `local_addr` and/or `peer_addr` are not `None`, they will point to an empty `String`.
    /// On success, user-space can fill in the strings as appropriate.
    ///
    /// The enclave must not make any security decisions based on the local or
    /// peer address received.
    #[allow(unused)]
    fn connect_stream<'future>(
        &'future self,
        addr: &'future str,
        local_addr: Option<&'future mut String>,
        peer_addr: Option<&'future mut String>,
    ) -> std::pin::Pin<Box<dyn Future<Output = IoResult<Option<Box<dyn AsyncStream>>>> +'future>> {
        async {
            Ok(None)
        }.boxed_local()
    }

    /// Override the target for bind calls by the enclave. The runner should determine the service that the enclave is trying to bind to by looking at addr.
    /// If `bind_stream` returns None, the default implementation of [`bind_stream`](../../fortanix_sgx_abi/struct.Usercalls.html#method.bind_stream) is used.
    /// The enclave may optionally request the local address be returned in `local_addr`.
    /// If `local_addr` is not `None`, it will point to an empty `String`.
    /// On success, user-space can fill in the string as appropriate.
    ///
    /// The enclave must not make any security decisions based on the local address received.
    #[allow(unused)]
    fn bind_stream<'future>(
        &'future self,
        addr: &'future str,
        local_addr: Option<&'future mut String>,
    ) -> std::pin::Pin<Box<dyn Future<Output = IoResult<Option<Box<dyn AsyncListener>>>> + 'future>> {
        async {
            Ok(None)
        }.boxed_local()
    }
}

impl<T: UsercallExtension> From<T> for Box<dyn UsercallExtension> {
    fn from(value: T) -> Box<dyn UsercallExtension> {
        Box::new(value)
    }
}

#[derive(Debug)]
struct UsercallExtensionDefault;
impl UsercallExtension for UsercallExtensionDefault {}

impl<'tcs> IOHandlerInput<'tcs> {
    async fn lookup_fd(&self, fd: Fd) -> IoResult<Arc<AsyncFileDesc>> {
        match self.enclave.fds.lock().await.get(&fd) {
            Some(stream) => Ok(stream.clone()),
            None => Err(IoErrorKind::BrokenPipe.into()), // FIXME: Rust normally maps Unix EBADF to `Other`
        }
    }

    async fn alloc_fd(&self, stream: AsyncFileDesc) -> Fd {
        let fd = (self
            .enclave
            .last_fd
            .fetch_add(1, Ordering::Relaxed)
            .checked_add(1)
            .expect("FD overflow")) as Fd;
        let prev = self.enclave.fds.lock().await.insert(fd, Arc::new(stream));
        debug_assert!(prev.is_none());
        fd
    }

    #[inline(always)]
    fn is_exiting(&self) -> bool {
        self.enclave.exiting.load(Ordering::SeqCst)
    }

    #[inline(always)]
    async fn read(&self, fd: Fd, buf: &mut [u8]) -> IoResult<usize> {
        let file_desc = self.lookup_fd(fd).await?;
        file_desc.as_stream()?.async_read(buf).await
    }

    #[inline(always)]
    async fn read_alloc(&self, fd: Fd, buf: &mut OutputBuffer<'tcs>) -> IoResult<()> {
        let file_desc = self.lookup_fd(fd).await?;
        let v = file_desc.as_stream()?.async_read_alloc().await?;
        buf.set(v);
        Ok(())
    }

    #[inline(always)]
    async fn write(&self, fd: Fd, buf: &[u8]) -> IoResult<usize> {
        let file_desc = self.lookup_fd(fd).await?;
        return file_desc.as_stream()?.async_write(buf).await;
    }

    #[inline(always)]
    async fn flush(&self, fd: Fd) -> IoResult<()> {
        let file_desc = self.lookup_fd(fd).await?;
        file_desc.as_stream()?.async_flush().await
    }

    #[inline(always)]
    async fn close(&self, fd: Fd) {
        self.enclave.fds.lock().await.remove(&fd);
    }

    #[inline(always)]
    async fn bind_stream(
        &self,
        addr: &[u8],
        local_addr: Option<&mut OutputBuffer<'tcs>>,
    ) -> IoResult<Fd> {
        let addr = str::from_utf8(addr).map_err(|_| IoErrorKind::ConnectionRefused)?;
        let mut local_addr_str = local_addr.as_ref().map(|_| String::new());
        if let Some(stream_ext) = self
            .enclave
            .usercall_ext
            .bind_stream(addr, local_addr_str.as_mut()).await?
        {
            if let Some(local_addr) = local_addr {
                local_addr.set(local_addr_str.unwrap().into_bytes());
            }
            return Ok(self.alloc_fd(AsyncFileDesc::listener(stream_ext)).await);
        }

        let socket = tokio::net::TcpListener::bind(addr).await?;
        if let Some(local_addr) = local_addr {
            local_addr.set(socket.local_addr()?.to_string().into_bytes());
        }
        Ok(self.alloc_fd(AsyncFileDesc::listener(Box::new(socket))).await)
    }

    #[inline(always)]
    async fn accept_stream(
        &self,
        fd: Fd,
        local_addr: Option<&mut OutputBuffer<'tcs>>,
        peer_addr: Option<&mut OutputBuffer<'tcs>>,
    ) -> IoResult<Fd> {
        let mut local_addr_str = local_addr.as_ref().map(|_| String::new());
        let mut peer_addr_str = peer_addr.as_ref().map(|_| String::new());

        let file_desc = self.lookup_fd(fd).await?;
        let stream = file_desc.as_listener()?.async_accept(local_addr_str.as_mut(), peer_addr_str.as_mut()).await?.unwrap();

        if let Some(local_addr) = local_addr {
            local_addr.set(&local_addr_str.unwrap().into_bytes()[..])
        }
        if let Some(peer_addr) = peer_addr {
            peer_addr.set(&peer_addr_str.unwrap().into_bytes()[..])
        }
        Ok(self.alloc_fd(AsyncFileDesc::stream(stream)).await)
    }

    #[inline(always)]
    async fn connect_stream(
        &self,
        addr: &[u8],
        local_addr: Option<&mut OutputBuffer<'tcs>>,
        peer_addr: Option<&mut OutputBuffer<'tcs>>,
    ) -> IoResult<Fd> {
        let addr = str::from_utf8(addr).map_err(|_| IoErrorKind::ConnectionRefused)?;
        let mut local_addr_str = local_addr.as_ref().map(|_| String::new());
        let mut peer_addr_str = peer_addr.as_ref().map(|_| String::new());
        if let Some(stream_ext) = self.enclave.usercall_ext.connect_stream(
            addr,
            local_addr_str.as_mut(),
            peer_addr_str.as_mut(),
        ).await? {
            if let Some(local_addr) = local_addr {
                local_addr.set(local_addr_str.unwrap().into_bytes());
            }
            if let Some(peer_addr) = peer_addr {
                peer_addr.set(peer_addr_str.unwrap().into_bytes());
            }
            return Ok(self.alloc_fd(AsyncFileDesc::stream(stream_ext)).await);
        }

        let stream = tokio::net::TcpStream::connect(addr).await?;

        if let Some(local_addr) = local_addr {
            match stream.local_addr() {
                Ok(local) => local_addr.set(local.to_string().into_bytes()),
                Err(_) => local_addr.set(&b"error"[..]),
            }
        }
        if let Some(peer_addr) = peer_addr {
            match stream.peer_addr() {
                Ok(peer) => peer_addr.set(peer.to_string().into_bytes()),
                Err(_) => peer_addr.set(&b"error"[..]),
            }
        }
        Ok(self.alloc_fd(AsyncFileDesc::stream(Box::new(stream))).await)
    }

    #[inline(always)]
    fn launch_thread(&self) -> IoResult<()> {
        // check if enclave is of type command
        self.enclave
            .kind
            .as_command()
            .ok_or(IoErrorKind::InvalidInput)?;
        let new_tcs = match self.enclave.threads_queue.pop() {
            Ok(tcs) => tcs,
            Err(_) => {
                return Err(IoErrorKind::WouldBlock.into());
            }
        };

        let ret = self.work_sender.send(Work {
            tcs: RunningTcs {
                pending_events: Default::default(),
                pending_event_set: 0,
                event_queue: new_tcs.event_queue,
                mode: EnclaveEntry::ExecutableNonMain,
            },
            entry: CoEntry::Initial(new_tcs.tcs, 0, 0, 0, 0, 0),
        });
        match ret {
            Ok(()) => Ok(()),
            Err(e) => {
                let event_queue = e.0.tcs.event_queue;
                let entry = e.0.entry;
                match entry {
                    CoEntry::Initial(tcs, _, _ ,_, _, _) => {
                        self.enclave.threads_queue.push(StoppedTcs {
                            tcs,
                            event_queue,
                        });
                    },
                    _ => unreachable!(),
                };
                Err(std::io::Error::new(
                    IoErrorKind::NotConnected,
                    "Work Sender: send error",
                ))
            }
        }
    }

    #[inline(always)]
    fn exit(&mut self, panic: bool) -> EnclaveAbort<bool> {
        self.enclave.abort_all_threads();
        EnclaveAbort::Exit { panic }
    }

    fn check_event_set(set: u64) -> IoResult<u8> {
        const EV_ALL: u64 = EV_USERCALLQ_NOT_FULL | EV_RETURNQ_NOT_EMPTY | EV_UNPARK;
        if (set & !EV_ALL) != 0 {
            return Err(IoErrorKind::InvalidInput.into());
        }

        assert!((EV_ALL | EV_ABORT) <= u8::max_value().into());
        assert!((EV_ALL & EV_ABORT) == 0);
        Ok(set as u8)
    }

    #[inline(always)]
    async fn wait(&mut self, event_mask: u64, timeout: u64) -> IoResult<u64> {
        let wait = match timeout {
            WAIT_NO => false,
            WAIT_INDEFINITE => true,
            _ => return Err(IoErrorKind::InvalidInput.into()),
        };

        let event_mask = Self::check_event_set(event_mask)?;

        let mut ret = None;

        if (self.tcs.pending_event_set & event_mask) != 0 {
            if let Some(pos) = self
                .tcs
                .pending_events
                .iter()
                .position(|ev| (ev & event_mask) != 0)
            {
                ret = self.tcs.pending_events.remove(pos);
                self.tcs.pending_event_set = self.tcs.pending_events.iter().fold(0, |m, ev| m | ev);
            }
        }

        if ret.is_none() {
            loop {
                let ev = if wait {
                    Ok(self.tcs.event_queue.next().await.unwrap())
                } else {
                    match self.tcs.event_queue.try_next() {
                        Ok(Some(ev)) => Ok(ev),
                        Err(_) => break,
                        Ok(None) => Err(()),
                    }
                }
                .expect("TCS event queue disconnected unexpectedly");
                if (ev & (EV_ABORT as u8)) != 0 {
                    // dispatch will make sure this is not returned to enclave
                    return Err(IoErrorKind::Other.into());
                }

                if (ev & event_mask) != 0 {
                    ret = Some(ev);
                    break;
                } else {
                    self.tcs.pending_events.push_back(ev);
                    self.tcs.pending_event_set |= ev;
                }
            }
        }

        if let Some(ret) = ret {
            Ok(ret.into())
        } else {
            Err(IoErrorKind::WouldBlock.into())
        }
    }

    #[inline(always)]
    fn send(&self, event_set: u64, target: Option<Tcs>) -> IoResult<()> {
        let event_set = Self::check_event_set(event_set)?;

        if event_set == 0 {
            return Err(IoErrorKind::InvalidInput.into());
        }

        if let Some(tcs) = target {
            let tcs = TcsAddress(tcs.as_ptr() as _);
            let queue = self
                .enclave
                .event_queues
                .get(&tcs)
                .ok_or(IoErrorKind::InvalidInput)?;
            queue
                .unbounded_send(event_set)
                .expect("TCS event queue disconnected");
        } else {
            for queue in self.enclave.event_queues.values() {
                let _ = queue.unbounded_send(event_set);
            }
        }

        Ok(())
    }

    #[inline(always)]
    fn insecure_time(&mut self) -> u64 {
        let time = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap();
        (time.subsec_nanos() as u64) + time.as_secs() * 1_000_000_000
    }

    #[inline(always)]
    fn alloc(&self, size: usize, alignment: usize) -> IoResult<*mut u8> {
        unsafe {
            let layout =
                Layout::from_size_align(size, alignment).map_err(|_| IoErrorKind::InvalidInput)?;
            if layout.size() == 0 {
                return Err(IoErrorKind::InvalidInput.into());
            }
            let ptr = System.alloc(layout);
            if ptr.is_null() {
                Err(IoErrorKind::Other.into())
            } else {
                Ok(ptr)
            }
        }
    }

    #[inline(always)]
    fn free(&self, ptr: *mut u8, size: usize, alignment: usize) -> IoResult<()> {
        unsafe {
            let layout =
                Layout::from_size_align(size, alignment).map_err(|_| IoErrorKind::InvalidInput)?;
            if size == 0 {
                return Ok(());
            }
            Ok(System.dealloc(ptr, layout))
        }
    }

    #[inline(always)]
    fn async_queues(
        &self,
        _usercall_queue: &mut FifoDescriptor<Usercall>,
        _return_queue: &mut FifoDescriptor<Return>,
    ) -> IoResult<()> {
        Err(IoErrorKind::Other.into())
    }
}
