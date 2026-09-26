//! Bounded staging IO whose directory lease lasts until the actual write ends.
use super::store::Staging;
use std::{
    fs::File,
    io::{self, Write},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    task::JoinHandle,
};

const WRITE_BUFFER: usize = 16 * 1024;

pub(super) struct StagingWriter {
    file: Option<File>,
    lease: Arc<File>,
    pending: Option<JoinHandle<(File, io::Result<usize>)>>,
    #[cfg(test)]
    before_write: Option<Box<dyn FnOnce() + Send>>,
}

impl StagingWriter {
    pub(super) fn new(stage: &Staging) -> io::Result<Self> {
        Ok(Self {
            file: Some(stage.file.try_clone()?),
            lease: stage.lease.clone(),
            pending: None,
            #[cfg(test)]
            before_write: None,
        })
    }

    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let result = ready!(Pin::new(self.pending.as_mut().expect("pending write")).poll(cx));
        self.pending = None;
        match result {
            Ok((file, result)) => {
                self.file = Some(file);
                Poll::Ready(result)
            }
            Err(error) => Poll::Ready(Err(io::Error::other(error))),
        }
    }

    pub(super) async fn finish(mut self) -> io::Result<File> {
        self.flush().await?;
        self.file
            .take()
            .ok_or_else(|| io::Error::other("staging writer failed"))
    }
}

impl AsyncWrite for StagingWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.pending.is_none() {
            if bytes.is_empty() {
                return Poll::Ready(Ok(0));
            }
            let Some(mut file) = self.file.take() else {
                return Poll::Ready(Err(io::Error::other("staging writer failed")));
            };
            let bytes = bytes[..bytes.len().min(WRITE_BUFFER)].to_vec();
            let lease = self.lease.clone();
            #[cfg(test)]
            let before_write = self.before_write.take();
            self.pending = Some(tokio::task::spawn_blocking(move || {
                // A dropped JoinHandle does not stop blocking IO. Retain ownership
                // here, not only on the cancellable async caller or Store.
                let _lease = lease;
                #[cfg(test)]
                if let Some(before_write) = before_write {
                    before_write();
                }
                let result = file.write(&bytes);
                (file, result)
            }));
        }
        self.poll_pending(cx)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.pending.is_some() {
            ready!(self.poll_pending(cx))?;
        }
        // std::fs::File has no userspace buffering; Store owns fsync/commit.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::Store;
    use super::*;
    use std::io::{Read, Seek, SeekFrom};

    #[tokio::test]
    async fn writes_are_bounded_and_finished_before_releasing_staging() {
        let dir = tempfile::tempdir().unwrap();
        let mut store =
            Store::open(&dir.path().join("subscriptions"), 64 * 1024 * 1024, 1).unwrap();
        let stage = store.begin_staging(100_000, 1).unwrap();
        let mut writer = StagingWriter::new(&stage).unwrap();
        let bytes = vec![b'x'; 50_000];
        assert_eq!(writer.write(&bytes).await.unwrap(), WRITE_BUFFER);
        writer.write_all(&bytes[WRITE_BUFFER..]).await.unwrap();
        let mut file = writer.finish().await.unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut actual = Vec::new();
        file.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, bytes);
        store.discard(stage).unwrap();
    }

    #[tokio::test]
    async fn cancelled_write_keeps_directory_locked_after_store_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subscriptions");
        let mut store = Store::open(&path, 64 * 1024 * 1024, 1).unwrap();
        let stage = store.begin_staging(100_000, 1).unwrap();
        let mut writer = StagingWriter::new(&stage).unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        writer.before_write = Some(Box::new(move || {
            let _ = started_tx.send(());
            // Even a failed assertion cannot strand the runtime's blocking pool.
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(5));
        }));
        let mut write = Box::pin(writer.write_all(b"cancelled.test\n"));
        assert!(futures_util::poll!(&mut write).is_pending());
        started_rx.await.unwrap();
        drop(write);
        let job = writer.pending.take().unwrap();
        drop(writer);
        drop(stage);
        drop(store);
        assert!(Store::open(&path, 64 * 1024 * 1024, 2).is_err());
        release_tx.send(()).unwrap();
        let _ = job.await.unwrap();
        let _reopened = Store::open(&path, 64 * 1024 * 1024, 2).unwrap();
        assert_eq!(std::fs::read_dir(path.join("staging")).unwrap().count(), 0);
    }
}
