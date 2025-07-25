use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, VecDeque},
    fmt::Debug,
    io::{Error, ErrorKind, Result},
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::Instant,
};

use futures::{AsyncRead, AsyncWrite};
use n3io::{mio::Token, reactor::Reactor};
use quiche::{RecvInfo, SendInfo};

pub(crate) struct QuicConnState {
    /// reactor for IOs.
    reactor: Reactor,
    /// underlying quiche connection object.
    pub(crate) quiche_conn: quiche::Connection,
    /// generator for outbound bidirectional stream id.
    outbound_bidi_stream_id_next: u64,
    /// The biggest inbound stream ID currently seen.
    inbound_stream_id_current: u64,
    /// fifo queue for first seen inbound stream IDs.
    incoming_stream_id_fifo: VecDeque<u64>,
    /// wakers for stream reading events.
    stream_readable_wakers: HashMap<u64, Waker>,
    /// wakers for stream writting events.
    stream_writable_wakers: HashMap<u64, Waker>,
    /// waker for `poll_send`
    send_waker: Option<Waker>,
    /// waker for fifo receiver.
    fifo_waker: Option<Waker>,
    /// open stream waker.
    open_stream_waker: Option<Waker>,
    /// wait for calling on_timeout.
    on_timeout_timer: Option<Token>,
    /// closing stream set.
    closing_stream_set: HashMap<u64, Instant>,
    /// pre-allocated recv buf for closing stream receiving.
    closing_recv_buf: Vec<u8>,
}

impl QuicConnState {
    fn closing_recv(&mut self, id: u64) -> bool {
        loop {
            match self.quiche_conn.stream_recv(id, &mut self.closing_recv_buf) {
                Ok((_, fin)) => {
                    if fin {
                        log::trace!(
                            "QuicConn({}): closed stream, stream_id={}, trace_id={}",
                            self.quiche_conn.is_server(),
                            id,
                            self.quiche_conn.trace_id(),
                        );
                        assert!(self.quiche_conn.stream_finished(id));
                        return true;
                    }
                }
                Err(quiche::Error::Done) => {
                    log::trace!(
                        "QuicConn({}): re-append stream to closing queue, stream_id={}, trace_id={}",
                        self.quiche_conn.is_server(),
                        id,
                        self.quiche_conn.trace_id(),
                    );

                    return false;
                }
                Err(err) => {
                    log::error!(
                        "QuicConn({}): clear up closed stream, stream_id={}, trace_id={}, err={}",
                        self.quiche_conn.is_server(),
                        id,
                        self.quiche_conn.trace_id(),
                        err
                    );

                    return true;
                }
            }
        }
    }

    fn poll_conn_stat_events(&mut self) -> Vec<Waker> {
        let mut wakers = vec![];
        let mut ordering_readable_id_set = BinaryHeap::new();

        while let Some(event) = self.quiche_conn.path_event_next() {
            log::info!(
                "path event, scid={}, event={:?}",
                self.quiche_conn.trace_id(),
                event
            );
        }

        while let Some(id) = self.quiche_conn.stream_readable_next() {
            ordering_readable_id_set.push(Reverse(id));
        }

        while let Some(Reverse(id)) = ordering_readable_id_set.pop() {
            log::trace!(
                "QuicConn({}): stream readable, stream_id={}, trace_id={}",
                self.quiche_conn.is_server(),
                id,
                self.quiche_conn.trace_id()
            );

            if is_bidi(id)
                && !is_local(id, self.quiche_conn.is_server())
                && self.inbound_stream_id_current < id
            {
                self.inbound_stream_id_current = id;
                self.incoming_stream_id_fifo.push_back(id);

                log::trace!(
                    "QuicConn({}): new incoming stream, id={}, trace_id={}",
                    self.quiche_conn.is_server(),
                    id,
                    self.quiche_conn.trace_id()
                );

                continue;
            }

            if let Some(waker) = self.stream_readable_wakers.remove(&id) {
                log::trace!(
                    "QuicConn({}): wakeup stream readable, id={},trace_id={}",
                    self.quiche_conn.is_server(),
                    id,
                    self.quiche_conn.trace_id()
                );

                wakers.push(waker);

                continue;
            }

            // clear closing stream.
            if let Some(closing_timestamp) = self.closing_stream_set.remove(&id) {
                if !self.closing_recv(id) {
                    self.closing_stream_set.insert(id, closing_timestamp);
                }
            }
        }

        if !self.incoming_stream_id_fifo.is_empty() {
            if let Some(waker) = self.fifo_waker.take() {
                log::trace!(
                    "Wakeup stream incoming, trace_id={}",
                    self.quiche_conn.trace_id()
                );
                wakers.push(waker);
            }
        }

        while let Some(id) = self.quiche_conn.stream_writable_next() {
            if let Some(waker) = self.stream_writable_wakers.remove(&id) {
                log::trace!(
                    "Wakeup stream writable, id={},trace_id={}",
                    id,
                    self.quiche_conn.trace_id()
                );
                wakers.push(waker);
            }
        }

        log::trace!(
            "QuicConn({}): poll_conn_stat_events, peer_streams_left_bidi={}, trace_id={}",
            self.quiche_conn.is_server(),
            self.quiche_conn.peer_streams_left_bidi(),
            self.quiche_conn.trace_id(),
        );

        if self.quiche_conn.peer_streams_left_bidi() > 0 {
            if let Some(waker) = self.open_stream_waker.take() {
                log::trace!(
                    "QuicConn({}): wakeup open stream, peer_streams_left_bidi={}, trace_id={}",
                    self.quiche_conn.is_server(),
                    self.quiche_conn.peer_streams_left_bidi(),
                    self.quiche_conn.trace_id(),
                );
                wakers.push(waker);
            }
        }

        wakers
    }

    fn finalize_events(&mut self) -> Vec<Waker> {
        let mut wakers = vec![];
        let trace_id = self.quiche_conn.trace_id().to_owned();
        if let Some(waker) = self.fifo_waker.take() {
            log::trace!(
                "QuicConn({}): finalize wake up `incoming` task, trace_id={}",
                self.quiche_conn.is_server(),
                trace_id,
            );
            wakers.push(waker);
        }

        for (stream_id, waker) in self.stream_readable_wakers.drain() {
            log::trace!(
                "QuicConn({}): finalize wake up stream reading task, stream_id={}, trace_id={}",
                self.quiche_conn.is_server(),
                stream_id,
                trace_id,
            );

            wakers.push(waker);
        }

        for (stream_id, waker) in self.stream_writable_wakers.drain() {
            log::trace!(
                "QuicConn({}): finalize wake up stream writting task, stream_id={}, trace_id={}",
                self.quiche_conn.is_server(),
                stream_id,
                trace_id,
            );

            wakers.push(waker);
        }

        wakers
    }
}

/// Returns true if the stream was created locally.
fn is_local(stream_id: u64, is_server: bool) -> bool {
    (stream_id & 0x1) == (is_server as u64)
}

/// Returns true if the stream is bidirectional.
fn is_bidi(stream_id: u64) -> bool {
    (stream_id & 0x2) == 0
}

/// Quic connection api.
#[derive(Clone)]
pub struct QuicConnDispatcher(pub(crate) Arc<Mutex<QuicConnState>>);

impl QuicConnDispatcher {
    /// Create new `QuicConn` from raw `quiche::Connection.`
    pub fn new(quiche_conn: quiche::Connection, reactor: Reactor) -> QuicConnDispatcher {
        let outbound_bidi_stream_id_next = if quiche_conn.is_server() { 5 } else { 4 };

        let state = Arc::new(Mutex::new(QuicConnState {
            reactor,
            quiche_conn,
            outbound_bidi_stream_id_next,
            inbound_stream_id_current: 0,
            incoming_stream_id_fifo: Default::default(),
            stream_readable_wakers: Default::default(),
            stream_writable_wakers: Default::default(),
            send_waker: Default::default(),
            fifo_waker: Default::default(),
            on_timeout_timer: Default::default(),
            open_stream_waker: Default::default(),
            closing_stream_set: Default::default(),
            closing_recv_buf: vec![0; 1200],
        }));

        QuicConnDispatcher(state)
    }

    /// Return true if the connection handshake is complete.
    pub(crate) fn is_established(&self) -> bool {
        self.0.lock().unwrap().quiche_conn.is_established()
    }
    /// Writes a single QUIC packet to be sent to the peer.
    ///
    /// This func transfer error [`quiche::Error::Done`] to [`Poll::Pending`]
    pub fn poll_send(
        &self,
        cx: &mut Context<'_>,
        out: &mut [u8],
    ) -> Poll<Result<(usize, SendInfo)>> {
        log::trace!("poll_send");
        let mut state = self.0.lock().unwrap();

        if let Some(timer) = state.on_timeout_timer.take() {
            match state.reactor.poll_timeout(cx, timer) {
                Poll::Ready(_) => {
                    log::trace!(
                        "QuicConn({}) call on_timeout, trace_id={}, timer={:?}",
                        state.quiche_conn.is_server(),
                        state.quiche_conn.trace_id(),
                        timer,
                    );
                    state.quiche_conn.on_timeout();
                }
                Poll::Pending => {}
            }

            state.reactor.deregister_timer(timer);
            state.send_waker = None;
        }

        loop {
            match state.quiche_conn.send(out) {
                Ok((send_size, send_info)) => {
                    log::trace!(
                        "QuicConn({}) send, send_size={}, send_info={:?}, trace_id={}, is_draining={}, is_closed={}",
                        state.quiche_conn.is_server(),
                        send_size,
                        send_info,
                        state.quiche_conn.trace_id(),
                        state.quiche_conn.is_draining(),
                        state.quiche_conn.is_closed()
                    );

                    let wakers = state.poll_conn_stat_events();

                    drop(state);

                    for waker in wakers {
                        waker.wake();
                    }

                    return Poll::Ready(Ok((send_size, send_info)));
                }
                Err(quiche::Error::Done) => {
                    log::trace!(
                        "QuicConn({}): send data `Done`, trace_id={:?}, is_draining={}, is_closed={}",
                        state.quiche_conn.is_server(),
                        state.quiche_conn.trace_id(),
                        state.quiche_conn.is_draining(),
                        state.quiche_conn.is_closed(),
                    );

                    if state.quiche_conn.is_closed() {
                        log::trace!(
                            "QuicConn(send, {}) is closed, trace_id={}",
                            state.quiche_conn.is_server(),
                            state.quiche_conn.trace_id()
                        );

                        for waker in state.finalize_events() {
                            waker.wake();
                        }

                        return Poll::Ready(Err(Error::new(
                            ErrorKind::BrokenPipe,
                            format!("QuicConn({}) is closed", state.quiche_conn.is_server()),
                        )));
                    }

                    if let Some(timeout) = state.quiche_conn.timeout_instant() {
                        let timer = state.reactor.deadline(timeout);

                        match state.reactor.poll_timeout(cx, timer) {
                            Poll::Ready(_) => {
                                // The deadline has expired.
                                state.quiche_conn.on_timeout();

                                state.reactor.deregister_timer(timer);

                                let now = Instant::now();

                                if now < timeout {
                                    log::trace!(
                                        "QuicConn({}): send data directly on_timout, trace_id={:?}, timeout={:?}",
                                        state.quiche_conn.is_server(),
                                        state.quiche_conn.trace_id(),
                                        timeout - now
                                    );
                                } else {
                                    log::trace!(
                                        "QuicConn({}): send data directly on_timout, trace_id={:?}, timeout=-{:?}",
                                        state.quiche_conn.is_server(),
                                        state.quiche_conn.trace_id(),
                                        now - timeout
                                    );
                                }

                                continue;
                            }
                            Poll::Pending => {}
                        }

                        let now = Instant::now();

                        if now < timeout {
                            log::trace!(
                                "QuicConn({}): send data pending, trace_id={:?}, timeout={:?}, timer={:?}",
                                state.quiche_conn.is_server(),
                                state.quiche_conn.trace_id(),
                                timeout - now,
                                timer
                            );
                        } else {
                            log::trace!(
                                "QuicConn({}): send data pending, trace_id={:?}, timeout=-{:?}, timer={:?}",
                                state.quiche_conn.is_server(),
                                state.quiche_conn.trace_id(),
                                now - timeout,
                                timer
                            );
                        }

                        state.on_timeout_timer = Some(timer);
                    }

                    state.send_waker = Some(cx.waker().clone());

                    return Poll::Pending;
                }
                Err(err) => {
                    log::error!(
                        "QuicConn({}): send data, trace_id={:?}, err={}",
                        state.quiche_conn.is_server(),
                        state.quiche_conn.trace_id(),
                        err
                    );
                    return Poll::Ready(Err(Error::other(err)));
                }
            }
        }
    }

    /// Processes QUIC packets received from the peer.
    pub fn poll_recv(
        &self,
        _cx: &mut Context<'_>,
        buf: &mut [u8],
        info: RecvInfo,
    ) -> Poll<Result<usize>> {
        log::trace!("poll_recv");
        let mut state = self.0.lock().unwrap();

        let poll = match state.quiche_conn.recv(buf, info) {
            Ok(recv_size) => {
                log::trace!(
                    "QuicConn({}): recv data, len={}, is_closed={}, is_draining={}",
                    state.quiche_conn.is_server(),
                    recv_size,
                    state.quiche_conn.is_closed(),
                    state.quiche_conn.is_draining(),
                );

                Poll::Ready(Ok(recv_size))
            }
            Err(err) => {
                log::error!(
                    "QuicConn({}): recv data, trace_id={}, err={}",
                    state.quiche_conn.is_server(),
                    state.quiche_conn.trace_id(),
                    err
                );
                Poll::Ready(Err(Error::other(err)))
            }
        };

        let mut wakers = state.poll_conn_stat_events();

        if let Some(waker) = state.send_waker.take() {
            log::trace!(
                "QuicConn({}): wake up sending task, trace_id={}",
                state.quiche_conn.is_server(),
                state.quiche_conn.trace_id(),
            );
            wakers.push(waker);
        }

        drop(state);

        for waker in wakers {
            waker.wake();
        }

        poll
    }
}

/// An extension trait for `QuicConnDispatcher` that provides a variety of convenient combinator functions.
pub trait QuicConnDispatcherExt {
    fn recv<'a>(&'a self, buf: &'a mut [u8], info: RecvInfo) -> ConnRecv<'a>;
    fn send<'a>(&'a self, buf: &'a mut [u8]) -> ConnSend<'a>;
}

impl QuicConnDispatcherExt for QuicConnDispatcher {
    fn recv<'a>(&'a self, buf: &'a mut [u8], info: RecvInfo) -> ConnRecv<'a> {
        ConnRecv {
            dispatcher: self,
            buf,
            info,
        }
    }

    fn send<'a>(&'a self, buf: &'a mut [u8]) -> ConnSend<'a> {
        ConnSend {
            dispatcher: self,
            buf,
        }
    }
}

/// A future created by [`recv`](QuicConnExt::recv) func.
pub struct ConnRecv<'a> {
    dispatcher: &'a QuicConnDispatcher,
    buf: &'a mut [u8],
    info: RecvInfo,
}

impl<'a> Future for ConnRecv<'a> {
    type Output = Result<usize>;

    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let info = self.info.clone();

        self.dispatcher.poll_recv(cx, self.buf, info)
    }
}

/// A future created by [`send`](QuicConnExt::send) func.
pub struct ConnSend<'a> {
    dispatcher: &'a QuicConnDispatcher,
    buf: &'a mut [u8],
}

impl<'a> Future for ConnSend<'a> {
    type Output = Result<(usize, SendInfo)>;

    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.dispatcher.poll_send(cx, self.buf)
    }
}

/// Quic connection api.
pub struct QuicConn(pub(crate) Arc<Mutex<QuicConnState>>);

impl Debug for QuicConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.0.lock().unwrap();

        f.debug_struct("QuicConn")
            .field("is_server", &state.quiche_conn.is_server())
            .field("trace_id", &state.quiche_conn.trace_id())
            .finish()
    }
}

impl Drop for QuicConn {
    fn drop(&mut self) {
        _ = self.close(0x0, b"");
    }
}

impl QuicConn {
    /// Return the number of active streams.
    pub fn active_outbound_streams(&self) -> Option<u64> {
        self.quiche_conn(|conn| {
            conn.peer_transport_params()
                .map(|params| params.initial_max_streams_bidi - conn.peer_streams_left_bidi())
        })
    }

    /// Access the inner quiche::Connection instance.
    pub fn quiche_conn<F, O>(&self, f: F) -> O
    where
        F: FnOnce(&quiche::Connection) -> O,
    {
        f(&self.0.lock().unwrap().quiche_conn)
    }

    /// Returns true if the connection is closed.
    ///
    /// If this returns true, the connection object can be dropped.
    pub fn is_closed(&self) -> bool {
        self.quiche_conn(|conn| conn.is_closed())
    }

    /// Close this connection.
    pub fn close(&self, err: u64, reason: &[u8]) -> Result<()> {
        let mut state = self.0.lock().unwrap();

        match state.quiche_conn.close(false, err, reason) {
            Ok(_) => {
                log::trace!(
                    "QuicConn({}): close, trace_id={}",
                    state.quiche_conn.is_server(),
                    state.quiche_conn.trace_id()
                );
            }
            Err(quiche::Error::Done) => {
                log::trace!(
                    "QuicConn({}): already closed, trace_id={}",
                    state.quiche_conn.is_server(),
                    state.quiche_conn.trace_id()
                );
                return Ok(());
            }
            Err(err) => {
                log::trace!(
                    "QuicConn({}): failed to close, trace_id={}, err={}",
                    state.quiche_conn.is_server(),
                    state.quiche_conn.trace_id(),
                    err
                );
                return Err(Error::other(err));
            }
        }

        let mut wakers = vec![];

        // make sure to wake up sending task.
        if let Some(waker) = state.send_waker.take() {
            log::trace!(
                "QuicConn({}): wake up sending task, trace_id={}",
                state.quiche_conn.is_server(),
                state.quiche_conn.trace_id()
            );

            wakers.push(waker);
        }

        drop(state);

        for waker in wakers {
            waker.wake();
        }

        Ok(())
    }

    /// Accepts a new `QUIC` stream.
    pub fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<Result<QuicStream>> {
        let mut state = self.0.lock().unwrap();

        if state.quiche_conn.is_closed() {
            return Poll::Ready(Err(Error::new(
                ErrorKind::BrokenPipe,
                format!(
                    "quic connection is closed, id={}",
                    state.quiche_conn.trace_id()
                ),
            )));
        }

        if let Some(stream_id) = state.incoming_stream_id_fifo.pop_front() {
            return Poll::Ready(Ok(QuicStream::new(stream_id, self.0.clone())));
        }

        log::trace!(
            "Accept new incoming, trace_id={}, pending=true",
            state.quiche_conn.trace_id()
        );

        state.fifo_waker = Some(cx.waker().clone());

        Poll::Pending
    }

    /// Try to open a outbound stream without waiting.
    ///
    /// Returns error, if `peer_streams_left_bidi == 0`
    pub fn try_open(&self) -> Result<QuicStream> {
        let mut state = self.0.lock().unwrap();

        if state.quiche_conn.peer_streams_left_bidi() > 0 {
            let stream_id = state.outbound_bidi_stream_id_next;
            state.outbound_bidi_stream_id_next += 4;

            // this a trick, func `stream_priority` will created the target if did not exist.
            state
                .quiche_conn
                .stream_priority(stream_id, 255, true)
                .map_err(|err| Error::other(err))?;

            log::trace!(
                "QuicConn({}) open new outbound stream, stream_id={}, trace_id={}",
                state.quiche_conn.is_server(),
                stream_id,
                state.quiche_conn.trace_id()
            );

            // if let Some(waker) = state.send_waker.take() {
            //     drop(state);
            //     waker.wake();
            // }

            return Ok(QuicStream::new(stream_id, self.0.clone()));
        }

        return Err(Error::new(
            ErrorKind::WouldBlock,
            "peer_streams_left_bidi == 0",
        ));
    }

    /// Open a new outbound stream.
    pub fn poll_stream_open(&self, cx: &mut Context<'_>) -> Poll<Result<QuicStream>> {
        let mut state = self.0.lock().unwrap();

        if state.quiche_conn.peer_streams_left_bidi() > 0 {
            let stream_id = state.outbound_bidi_stream_id_next;
            state.outbound_bidi_stream_id_next += 4;

            // this a trick, func `stream_priority` will created the target if did not exist.
            state
                .quiche_conn
                .stream_priority(stream_id, 255, true)
                .map_err(|err| Error::other(err))?;

            log::trace!(
                "QuicConn({}) open new outbound stream, stream_id={}, trace_id={}",
                state.quiche_conn.is_server(),
                stream_id,
                state.quiche_conn.trace_id()
            );

            // if let Some(waker) = state.send_waker.take() {
            //     drop(state);
            //     waker.wake();
            // }

            Poll::Ready(Ok(QuicStream::new(stream_id, self.0.clone())))
        } else {
            log::trace!(
                "Open new outbound, trace_id={}, pending=true",
                state.quiche_conn.trace_id()
            );
            state.open_stream_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// An extension trait for `QuicConn` that provides a variety of convenient combinator functions.
pub trait QuicConnExt {
    /// Accept a new incoming QuicStream.
    fn accept(&self) -> AcceptStream<'_>;

    /// Open a new outbound stream.
    fn open(&self) -> OpenStream<'_>;
}

impl QuicConnExt for QuicConn {
    fn accept(&self) -> AcceptStream<'_> {
        AcceptStream(self)
    }

    fn open(&self) -> OpenStream<'_> {
        OpenStream(self)
    }
}

/// A future created by [`accept`](QuicConnExt::accept) func.
pub struct AcceptStream<'a>(&'a QuicConn);

impl<'a> Future for AcceptStream<'a> {
    type Output = Result<QuicStream>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.poll_accept(cx)
    }
}

/// A future created by [`open`](QuicConnExt::open) func.
pub struct OpenStream<'a>(&'a QuicConn);

impl<'a> Future for OpenStream<'a> {
    type Output = Result<QuicStream>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.poll_stream_open(cx)
    }
}

/// The quic stream socket.
pub struct QuicStream {
    stream_id: u64,
    state: Arc<Mutex<QuicConnState>>,
}

impl QuicStream {
    fn new(stream_id: u64, state: Arc<Mutex<QuicConnState>>) -> Self {
        QuicStream { stream_id, state }
    }
}

impl Drop for QuicStream {
    fn drop(&mut self) {
        _ = self.close_stream();
    }
}

impl QuicStream {
    fn close_stream(&self) -> Result<()> {
        log::trace!("close_stream, id={}", self.stream_id);
        let mut state = self.state.lock().unwrap();

        log::trace!(
            "QuiConn({}): close stream, stream_id={}, conn_id={}, is_draining={}, is_closed={}",
            state.quiche_conn.is_server(),
            self.stream_id,
            state.quiche_conn.trace_id(),
            state.quiche_conn.is_draining(),
            state.quiche_conn.is_closed(),
        );

        if state.quiche_conn.is_closed() {
            return Ok(());
        }

        if let Err(err) = state.quiche_conn.stream_send(self.stream_id, b"", true) {
            log::error!(
                "QuiConn({}): failed to close stream, id={}, trace_id={}, err={}",
                state.quiche_conn.is_server(),
                self.stream_id,
                state.quiche_conn.trace_id(),
                err
            );
        }

        if !state.quiche_conn.stream_finished(self.stream_id) {
            log::trace!(
                "QuiConn({}): append stream to closing queue, id={}, trace_id={}",
                state.quiche_conn.is_server(),
                self.stream_id,
                state.quiche_conn.trace_id(),
            );

            state
                .closing_stream_set
                .insert(self.stream_id, Instant::now());
        } else {
            // force to collect complete streams.
            state.closing_recv(self.stream_id);
        }

        if let Some(waker) = state.send_waker.take() {
            drop(state);
            waker.wake();
        }

        Ok(())
    }

    /// Returns id value of this stream.
    pub fn id(&self) -> u64 {
        self.stream_id
    }

    /// Returns true if all the data has been read from the specified stream.
    pub fn is_finished(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .quiche_conn
            .stream_finished(self.stream_id)
    }

    /// Attempt to write bytes from `buf` into the `stream_id`.
    pub fn poll_stream_write(
        &self,
        cx: &mut Context<'_>,
        buf: &[u8],
        fin: bool,
    ) -> Poll<Result<usize>> {
        log::trace!("poll_stream_write, id={}", self.stream_id);

        let mut state = self.state.lock().unwrap();

        match state.quiche_conn.stream_send(self.stream_id, buf, fin) {
            Ok(written_size) => {
                log::trace!(
                    "Stream write, stream_id={}, trace_id={}, send_size={}, fin={}, is_server={}",
                    self.stream_id,
                    state.quiche_conn.trace_id(),
                    written_size,
                    fin,
                    state.quiche_conn.is_server()
                );

                if let Some(waker) = state.send_waker.take() {
                    drop(state);
                    waker.wake();
                }

                return Poll::Ready(Ok(written_size));
            }
            Err(quiche::Error::Done) => {
                if state.quiche_conn.is_draining() || state.quiche_conn.is_closed() {
                    log::error!(
                        "QuicConn({}): failed to read from stream, stream_id={}, trace_id={}, err=connection is closed or is draining",
                        state.quiche_conn.is_server(),
                        self.stream_id,
                        state.quiche_conn.trace_id()
                    );
                    return Poll::Ready(Err(Error::new(
                        ErrorKind::BrokenPipe,
                        "Connection is closed or is draining.",
                    )));
                }

                log::trace!(
                    "QuicConn({}): stream write, stream_id={}, trace_id={}, pending",
                    state.quiche_conn.is_server(),
                    self.stream_id,
                    state.quiche_conn.trace_id()
                );

                state
                    .stream_writable_wakers
                    .insert(self.stream_id, cx.waker().clone());

                return Poll::Pending;
            }
            Err(err) => {
                return Poll::Ready(Err(Error::other(err)));
            }
        }
    }

    /// Attempt to read bytes the `stream_id` into the `buf`.
    pub fn poll_stream_read(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<(usize, bool)>> {
        log::trace!("poll_stream_read, id={}", self.stream_id);

        let mut state = self.state.lock().unwrap();

        match state.quiche_conn.stream_recv(self.stream_id, buf) {
            Ok((read_size, fin)) => {
                log::trace!(
                    "Stream read, stream_id={}, trace_id={}, read_size={}, fin={}, is_server={}",
                    self.stream_id,
                    state.quiche_conn.trace_id(),
                    read_size,
                    fin,
                    state.quiche_conn.is_server()
                );

                if let Some(waker) = state.send_waker.take() {
                    drop(state);
                    waker.wake();
                }

                return Poll::Ready(Ok((read_size, fin)));
            }
            Err(quiche::Error::Done) => {
                if state.quiche_conn.is_draining() || state.quiche_conn.is_closed() {
                    log::trace!(
                        "QuicConn({}): failed to read from stream, stream_id={}, trace_id={}, err=connection is closed or is draining",
                        state.quiche_conn.is_server(),
                        self.stream_id,
                        state.quiche_conn.trace_id()
                    );
                    return Poll::Ready(Ok((0, true)));
                }

                log::trace!(
                    "QuicConn({}): stream read, stream_id={}, trace_id={}, pending",
                    state.quiche_conn.is_server(),
                    self.stream_id,
                    state.quiche_conn.trace_id()
                );

                state
                    .stream_readable_wakers
                    .insert(self.stream_id, cx.waker().clone());

                return Poll::Pending;
            }
            Err(err) => {
                return Poll::Ready(Err(Error::other(err)));
            }
        }
    }

    /// Helper method for splitting the quic stream into two halves.
    ///
    /// The two halves returned implement the AsyncRead and AsyncWrite traits, respectively.
    pub fn split(self) -> (QuicStreamWriter, QuicStreamReader) {
        let this = Arc::new(self);

        (QuicStreamWriter(this.clone()), QuicStreamReader(this))
    }
}

impl AsyncWrite for &QuicStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        self.poll_stream_write(cx, buf, false)
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.poll_stream_write(cx, b"", true).map_ok(|_| ())
    }
}

impl AsyncWrite for QuicStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        self.poll_stream_write(cx, buf, false)
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.poll_stream_write(cx, b"", true).map_ok(|_| ())
    }
}

impl AsyncRead for &QuicStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize>> {
        self.poll_stream_read(cx, buf).map_ok(|(len, _)| len)
    }
}

impl AsyncRead for QuicStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize>> {
        self.poll_stream_read(cx, buf).map_ok(|(len, _)| len)
    }
}

/// Readable half of one quic stream.
pub struct QuicStreamReader(Arc<QuicStream>);

impl AsyncRead for QuicStreamReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize>> {
        self.0.poll_stream_read(cx, buf).map_ok(|(len, _)| len)
    }
}

/// Writable half of one quic stream.
pub struct QuicStreamWriter(Arc<QuicStream>);

impl AsyncWrite for QuicStreamWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        self.0.poll_stream_write(cx, buf, false)
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.0.poll_stream_write(cx, b"", true).map_ok(|_| ())
    }
}
