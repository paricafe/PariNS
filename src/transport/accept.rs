//! Socket error recovery for DNS datagrams and TCP listener accepts.

use std::{
    io,
    net::SocketAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use tokio::net::{TcpListener, TcpStream};

pub(crate) const EXHAUSTED_PAUSE: Duration = Duration::from_millis(250);
const EXHAUSTED_LOG_INTERVAL: Duration = Duration::from_secs(60);
static EXHAUSTED_LOGGED: Mutex<Option<Instant>> = Mutex::new(None);

pub(crate) fn log_exhausted(operation: &str, error: &io::Error) {
    let mut logged = EXHAUSTED_LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if logged.is_none_or(|at| now.duration_since(at) >= EXHAUSTED_LOG_INTERVAL) {
        *logged = Some(now);
        if operation == "TCP accept" {
            eprintln!(
                "PariNS accept paused: {error}; raise the open-file limit or lower max_tcp_connections"
            );
        } else {
            eprintln!("PariNS {operation} paused: {error}");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recovery {
    Retry,
    Pause,
    Fatal,
}

pub(crate) fn recovery(error: &io::Error) -> Recovery {
    use io::ErrorKind::*;
    if matches!(
        error.kind(),
        ConnectionAborted
            | ConnectionReset
            | ConnectionRefused
            | NetworkDown
            | NetworkUnreachable
            | HostUnreachable
            | TimedOut
            | Interrupted
            | WouldBlock
    ) {
        return Recovery::Retry;
    }
    if error.kind() == OutOfMemory {
        return Recovery::Pause;
    }
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error() {
        use nix::errno::Errno;
        match Errno::from_raw(code) {
            Errno::EMFILE | Errno::ENFILE | Errno::ENOBUFS => {
                return Recovery::Pause;
            }
            Errno::EPROTO | Errno::EHOSTDOWN => return Recovery::Retry,
            #[cfg(target_os = "linux")]
            Errno::ENONET => return Recovery::Retry,
            _ => {}
        }
    }
    Recovery::Fatal
}

fn accept_recovery(error: &io::Error) -> Recovery {
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error() {
        use nix::errno::Errno;
        if matches!(
            Errno::from_raw(code),
            Errno::ENOPROTOOPT | Errno::EOPNOTSUPP | Errno::EPERM
        ) {
            return Recovery::Retry;
        }
    }
    recovery(error)
}

pub(crate) struct Acceptor<'a> {
    listener: &'a TcpListener,
    retry_at: Option<tokio::time::Instant>,
}

impl<'a> Acceptor<'a> {
    pub(crate) fn new(listener: &'a TcpListener) -> Self {
        Self {
            listener,
            retry_at: None,
        }
    }

    pub(crate) async fn accept(&mut self) -> io::Result<(TcpStream, SocketAddr)> {
        accept_with(&mut self.retry_at, || self.listener.accept()).await
    }
}

async fn accept_with<T, F: Future<Output = io::Result<T>>>(
    retry_at: &mut Option<tokio::time::Instant>,
    mut next: impl FnMut() -> F,
) -> io::Result<T> {
    loop {
        if let Some(at) = *retry_at {
            tokio::time::sleep_until(at).await;
            *retry_at = None;
        }
        let error = match next().await {
            Ok(accepted) => return Ok(accepted),
            Err(error) => error,
        };
        match accept_recovery(&error) {
            Recovery::Retry => {}
            Recovery::Pause => {
                log_exhausted("TCP accept", &error);
                *retry_at = Some(tokio::time::Instant::now() + EXHAUSTED_PAUSE);
            }
            Recovery::Fatal => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_and_resource_errors_keep_the_listener_serving() {
        for kind in [
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::NetworkDown,
            io::ErrorKind::Interrupted,
        ] {
            assert_eq!(recovery(&kind.into()), Recovery::Retry, "{kind:?}");
        }
        assert_eq!(
            recovery(&io::ErrorKind::OutOfMemory.into()),
            Recovery::Pause
        );
        #[cfg(unix)]
        {
            use nix::errno::Errno;
            for errno in [Errno::EMFILE, Errno::ENFILE, Errno::ENOBUFS] {
                let error = io::Error::from_raw_os_error(errno as i32);
                assert_eq!(recovery(&error), Recovery::Pause, "{errno}");
            }
            for errno in [Errno::ECONNABORTED, Errno::EPROTO, Errno::EPERM] {
                let error = io::Error::from_raw_os_error(errno as i32);
                assert_eq!(accept_recovery(&error), Recovery::Retry, "{errno}");
            }
            for errno in [Errno::EBADF, Errno::EINVAL, Errno::ENOTSOCK] {
                let error = io::Error::from_raw_os_error(errno as i32);
                assert_eq!(recovery(&error), Recovery::Fatal, "{errno}");
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn accept_errors_are_skipped_or_paused_until_a_connection_or_fatal_error() {
        fn errors(kinds: &[io::ErrorKind]) -> std::collections::VecDeque<io::Result<u8>> {
            kinds.iter().map(|kind| Err((*kind).into())).collect()
        }
        let started = tokio::time::Instant::now();
        let mut script = errors(&[
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::OutOfMemory,
            io::ErrorKind::ConnectionReset,
        ]);
        script.push_back(Ok(7));
        let accepted = accept_with(&mut None, || {
            std::future::ready(script.pop_front().unwrap())
        })
        .await;
        assert_eq!(accepted.unwrap(), 7);
        assert_eq!(started.elapsed(), EXHAUSTED_PAUSE);

        let mut script = errors(&[
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::InvalidInput,
        ]);
        script.push_back(Ok(7));
        let failed = accept_with(&mut None, || {
            std::future::ready(script.pop_front().unwrap())
        })
        .await;
        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(script.len(), 1, "a fatal error ends accepting");
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_accept_preserves_the_remaining_exhaustion_pause() {
        let mut retry_at = None;
        let mut script = [Err(io::ErrorKind::OutOfMemory.into()), Ok(7)].into_iter();
        let start = tokio::time::Instant::now();
        {
            let attempt = accept_with(&mut retry_at, || std::future::ready(script.next().unwrap()));
            tokio::pin!(attempt);
            assert!(futures_util::poll!(&mut attempt).is_pending());
        }
        assert_eq!(retry_at, Some(start + EXHAUSTED_PAUSE));
        tokio::time::advance(EXHAUSTED_PAUSE / 2).await;
        let accepted =
            accept_with(&mut retry_at, || std::future::ready(script.next().unwrap())).await;
        assert_eq!(accepted.unwrap(), 7);
        assert_eq!(start.elapsed(), EXHAUSTED_PAUSE);
    }
}
