//! Signals response completion once the final HTTP/2 frame is flushed to TLS.

use std::{
    collections::HashMap,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::oneshot,
};

#[derive(Clone, Default)]
pub(super) struct Responses(Arc<Mutex<HashMap<u32, Pending>>>);

struct Pending {
    flushed: oneshot::Sender<()>,
    written: bool,
}

impl Responses {
    pub(super) fn track(&self, id: h2::StreamId) -> oneshot::Receiver<()> {
        let (flushed, receiver) = oneshot::channel();
        let mut pending = self.0.lock().unwrap();
        pending.retain(|_, response| !response.flushed.is_closed());
        pending.insert(
            id.as_u32(),
            Pending {
                flushed,
                written: false,
            },
        );
        receiver
    }
}

pub(super) struct Io<T> {
    tls: T,
    responses: Responses,
    frames: Frames,
}

impl<T> Io<T> {
    pub(super) fn new(tls: T, responses: Responses) -> Self {
        Self {
            tls,
            responses,
            frames: Frames::default(),
        }
    }
}

impl<T> Drop for Io<T> {
    fn drop(&mut self) {
        self.responses.0.lock().unwrap().clear();
    }
}

#[derive(Default)]
struct Frames {
    header: [u8; 9],
    header_len: usize,
    remaining: usize,
    end_stream: Option<u32>,
}

impl Frames {
    fn written(&mut self, mut bytes: &[u8], mut finished: impl FnMut(u32)) {
        while !bytes.is_empty() {
            if self.header_len < self.header.len() {
                let count = bytes.len().min(self.header.len() - self.header_len);
                self.header[self.header_len..self.header_len + count]
                    .copy_from_slice(&bytes[..count]);
                self.header_len += count;
                bytes = &bytes[count..];
                if self.header_len < self.header.len() {
                    break;
                }
                let [high, mid, low, kind, flags, a, b, c, d] = self.header;
                self.remaining = u32::from_be_bytes([0, high, mid, low]) as usize;
                self.end_stream = (matches!(kind, 0 | 1) && flags & 1 != 0)
                    .then(|| u32::from_be_bytes([a & 0x7f, b, c, d]));
            }
            let count = self.remaining.min(bytes.len());
            self.remaining -= count;
            bytes = &bytes[count..];
            if self.remaining == 0 {
                if let Some(id) = self.end_stream.take() {
                    finished(id);
                }
                self.header_len = 0;
            }
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Io<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.tls).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Io<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.as_mut().get_mut();
        let count = std::task::ready!(Pin::new(&mut this.tls).poll_write(cx, buf))?;
        this.frames.written(&buf[..count], |id| {
            if let Some(response) = this.responses.0.lock().unwrap().get_mut(&id) {
                response.written = true;
            }
        });
        Poll::Ready(Ok(count))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        std::task::ready!(Pin::new(&mut self.tls).poll_flush(cx))?;
        let mut pending = self.responses.0.lock().unwrap();
        pending
            .extract_if(|_, response| response.written)
            .for_each(|(_, response)| {
                let _ = response.flushed.send(());
            });
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.tls).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn only_complete_flushed_frames_signal_delivery() {
        let (writer, _reader) = tokio::io::duplex(64);
        let responses = Responses::default();
        let (flushed, mut delivered) = oneshot::channel();
        responses.0.lock().unwrap().insert(
            1,
            Pending {
                flushed,
                written: false,
            },
        );
        let mut io = Io::new(writer, responses);
        let frame = [0, 0, 3, 0, 1, 0, 0, 0, 1, 42, 43, 44];
        io.write_all(&frame[..5]).await.unwrap();
        io.write_all(&frame[5..11]).await.unwrap();
        io.flush().await.unwrap();
        assert_eq!(
            delivered.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );
        io.write_all(&frame[11..]).await.unwrap();
        assert_eq!(
            delivered.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );
        io.flush().await.unwrap();
        assert_eq!(delivered.try_recv(), Ok(()));
    }
}
