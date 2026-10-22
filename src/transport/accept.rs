//! TCP accept shared by the DNS TCP, DoT, DoH, admin and management listeners.
//!
//! An accept error usually concerns one pending connection (the peer aborted
//! it, or the network failed for it) or a passing resource shortage, so the
//! listener keeps serving. Descriptor and buffer exhaustion first pause briefly,
//! because accepting again at once would fail the same way in a busy loop; the
//! pending connections wait in the kernel backlog meanwhile. Only an error that
//! concerns the listening socket itself ends the listener, which its supervisor
//! then rebinds. Exhaustion is logged at most once a minute across listeners
//! (per-connection errors are not logged), so an operator sees it without a flood.

use std::{
    io,
    net::SocketAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use tokio::net::{TcpListener, TcpStream};

/// Pause after descriptor or buffer exhaustion before accepting again.
const EXHAUSTED_PAUSE: Duration = Duration::from_millis(250);
const EXHAUSTED_LOG_INTERVAL: Duration = Duration::from_secs(60);
static EXHAUSTED_LOGGED: Mutex<Option<Instant>> = Mutex::new(None);

fn log_exhausted(error: &io::Error) {
    let mut logged = EXHAUSTED_LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if logged.is_none_or(|at| now.duration_since(at) >= EXHAUSTED_LOG_INTERVAL) {
        *logged = Some(now);
        eprintln!(
            "PariNS accept paused: {error}; raise the open-file limit or lower max_tcp_connections"
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recovery {
    /// One connection failed; accept the next one at once.
    Retry,
    /// Process or system resources ran out; accept again after a pause.
    Pause,
    /// The listening socket failed.
    Fatal,
}

fn recovery(error: &io::Error) -> Recovery {
    use io::ErrorKind::*;
    // accept(2): network errors pending on the new socket are reported by accept
    // and should be treated like EAGAIN.
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
            Errno::EMFILE | Errno::ENFILE | Errno::ENOBUFS | Errno::ENOMEM => {
                return Recovery::Pause;
            }
            // EPERM: a firewall rule refused this connection (Linux).
            Errno::EPROTO
            | Errno::ENOPROTOOPT
            | Errno::EHOSTDOWN
            | Errno::EOPNOTSUPP
            | Errno::EPERM => return Recovery::Retry,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Errno::ENONET => return Recovery::Retry,
            _ => {}
        }
    }
    Recovery::Fatal
}

/// Accepts the next connection, riding out errors that do not concern the
/// listening socket. Cancel-safe like [`TcpListener::accept`]: the caller may
/// race it against shutdown, including during an exhaustion pause.
pub(crate) async fn accept(listener: &TcpListener) -> io::Result<(TcpStream, SocketAddr)> {
    accept_with(|| listener.accept()).await
}

async fn accept_with<T, F: Future<Output = io::Result<T>>>(
    mut next: impl FnMut() -> F,
) -> io::Result<T> {
    loop {
        let error = match next().await {
            Ok(accepted) => return Ok(accepted),
            Err(error) => error,
        };
        match recovery(&error) {
            Recovery::Retry => {}
            Recovery::Pause => {
                log_exhausted(&error);
                tokio::time::sleep(EXHAUSTED_PAUSE).await;
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
                assert_eq!(recovery(&error), Recovery::Retry, "{errno}");
            }
            for errno in [Errno::EBADF, Errno::EINVAL, Errno::ENOTSOCK] {
                let error = io::Error::from_raw_os_error(errno as i32);
                assert_eq!(recovery(&error), Recovery::Fatal, "{errno}");
            }
        }
    }

    /// Aborted connections are skipped at once, exhaustion pauses before the
    /// next accept, and only a listening-socket error is returned.
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
        let accepted = accept_with(|| std::future::ready(script.pop_front().unwrap())).await;
        assert_eq!(accepted.unwrap(), 7);
        assert_eq!(started.elapsed(), EXHAUSTED_PAUSE);

        let mut script = errors(&[
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::InvalidInput,
        ]);
        script.push_back(Ok(7));
        let failed = accept_with(|| std::future::ready(script.pop_front().unwrap())).await;
        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(script.len(), 1, "a fatal error ends accepting");
    }
}
