//! Own Quinn's background work until the listening socket is actually released.
use std::{
    future::{Future, poll_fn},
    io::{self, IoSliceMut},
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use quinn::{AsyncTimer, AsyncUdpSocket, Runtime, TokioRuntime, UdpPoller};
use tokio::{sync::watch, task::JoinSet};

#[derive(Debug)]
struct Tasks {
    stopping: bool,
    running: JoinSet<()>,
}

#[derive(Debug)]
pub(crate) struct ListenerRuntime {
    tasks: Mutex<Tasks>,
    released: watch::Sender<bool>,
}

impl ListenerRuntime {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            tasks: Mutex::new(Tasks {
                stopping: false,
                running: JoinSet::new(),
            }),
            released: watch::channel(true).0,
        })
    }

    fn abort(&self) {
        let mut tasks = self.tasks.lock().expect("QUIC tasks");
        tasks.stopping = true;
        tasks.running.abort_all();
    }

    pub(crate) async fn shutdown(&self) {
        self.abort();
        // Keep joins in the owner, so cancelling this wait cannot lose cleanup.
        while poll_fn(|cx| {
            self.tasks
                .lock()
                .expect("QUIC tasks")
                .running
                .poll_join_next(cx)
        })
        .await
        .is_some()
        {}
        // Cancelled application connection/stream tasks can still hold sockets
        // until their destructors run; driver completion alone is insufficient.
        let mut released = self.released.subscribe();
        while !*released.borrow_and_update() {
            released.changed().await.expect("QUIC socket release owner");
        }
    }
}

impl Runtime for ListenerRuntime {
    fn new_timer(&self, at: std::time::Instant) -> Pin<Box<dyn AsyncTimer>> {
        TokioRuntime.new_timer(at)
    }

    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        let mut tasks = self.tasks.lock().expect("QUIC tasks");
        while tasks.running.try_join_next().is_some() {}
        if !tasks.stopping {
            tasks.running.spawn(future);
        }
    }

    fn wrap_udp_socket(&self, socket: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        let inner = TokioRuntime.wrap_udp_socket(socket)?;
        self.released.send_replace(false);
        Ok(Arc::new(Socket {
            inner,
            _release: Release(self.released.clone()),
        }))
    }
}

pub(super) struct Owner(pub(super) Arc<ListenerRuntime>);

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug)]
struct Release(watch::Sender<bool>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

#[derive(Debug)]
struct Socket {
    // Field order ensures the real socket is dropped before release is signalled.
    inner: Arc<dyn AsyncUdpSocket>,
    _release: Release,
}

#[derive(Debug)]
struct Poller {
    // The delegate may retain the real socket. Drop it before the notifying owner.
    inner: Pin<Box<dyn UdpPoller>>,
    _owner: Arc<Socket>,
}

impl UdpPoller for Poller {
    fn poll_writable(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.as_mut().poll_writable(cx)
    }
}

impl AsyncUdpSocket for Socket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(Poller {
            inner: self.inner.clone().create_io_poller(),
            _owner: self,
        })
    }
    fn try_send(&self, transmit: &quinn::udp::Transmit<'_>) -> io::Result<()> {
        self.inner.try_send(transmit)
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_recv(cx, bufs, meta)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }
    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }
    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}
