use super::{
    model::{OwnerError, OwnerErrorKind, RecipientId},
    private_dir::PrivateStore,
};
use std::path::PathBuf;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

enum DiskCommand {
    Write(Vec<u8>, oneshot::Sender<Result<(), OwnerError>>),
    Sync(oneshot::Sender<Result<(), OwnerError>>),
    #[cfg(all(test, unix))]
    Fail(Option<super::private_dir::Failure>, oneshot::Sender<()>),
}

pub(super) struct Disk {
    sender: mpsc::Sender<DiskCommand>,
    worker: JoinHandle<()>,
}
impl Disk {
    pub(super) async fn open(
        root: PathBuf,
        recipient: RecipientId,
        maximum: usize,
    ) -> Result<(Self, Option<Vec<u8>>), OwnerError> {
        let (sender, mut receiver) = mpsc::channel(4);
        let (ready, opened) = oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let mut store = match PrivateStore::open(&root, recipient.as_str()) {
                Ok(store) => store,
                Err(error) => {
                    let _ = ready.send(Err(OwnerError::from(error)));
                    return;
                }
            };
            let bytes = store.read(maximum).map_err(OwnerError::from);
            let failed = bytes.is_err();
            if ready.send(bytes).is_err() || failed {
                return;
            }
            while let Some(command) = receiver.blocking_recv() {
                match command {
                    DiskCommand::Write(bytes, reply) => {
                        let _ = reply.send(store.write(&bytes).map_err(OwnerError::from));
                    }
                    DiskCommand::Sync(reply) => {
                        let _ = reply.send(store.sync_pending().map_err(OwnerError::from));
                    }
                    #[cfg(all(test, unix))]
                    DiskCommand::Fail(failure, reply) => {
                        store.fail_at(failure);
                        let _ = reply.send(());
                    }
                }
            }
        });
        Self::finish_open(sender, worker, opened).await
    }
    async fn finish_open(
        sender: mpsc::Sender<DiskCommand>,
        worker: JoinHandle<()>,
        opened: oneshot::Receiver<Result<Option<Vec<u8>>, OwnerError>>,
    ) -> Result<(Self, Option<Vec<u8>>), OwnerError> {
        match opened.await {
            Ok(Ok(bytes)) => Ok((Self { sender, worker }, bytes)),
            Ok(Err(error)) => {
                drop(sender);
                if let Err(join) = worker.await {
                    tracing::warn!(
                        panicked = join.is_panic(),
                        cancelled = join.is_cancelled(),
                        "disk worker failed after an earlier open error"
                    );
                }
                Err(error)
            }
            Err(_) => {
                drop(sender);
                worker.await.map_err(OwnerError::worker_join)?;
                Err(OwnerError::new(OwnerErrorKind::Unavailable))
            }
        }
    }
    pub(super) async fn write(&self, bytes: Vec<u8>) -> Result<(), OwnerError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(DiskCommand::Write(bytes, reply))
            .await
            .map_err(|_| OwnerError::new(OwnerErrorKind::Unavailable))?;
        result
            .await
            .map_err(|_| OwnerError::new(OwnerErrorKind::Unavailable))?
    }
    pub(super) async fn sync(&self) -> Result<(), OwnerError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(DiskCommand::Sync(reply))
            .await
            .map_err(|_| OwnerError::new(OwnerErrorKind::Unavailable))?;
        result
            .await
            .map_err(|_| OwnerError::new(OwnerErrorKind::Unavailable))?
    }
    pub(super) async fn close(self) -> Result<(), OwnerError> {
        drop(self.sender);
        self.worker.await.map_err(OwnerError::worker_join)
    }
    #[cfg(all(test, unix))]
    pub(super) async fn fail(
        &self,
        failure: Option<super::private_dir::Failure>,
    ) -> Result<(), OwnerError> {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(DiskCommand::Fail(failure, reply))
            .await
            .map_err(|_| OwnerError::new(OwnerErrorKind::Unavailable))?;
        result
            .await
            .map_err(|_| OwnerError::new(OwnerErrorKind::Unavailable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_join(error: &OwnerError) {
        let OwnerErrorKind::WorkerJoin(cause) = &error.kind else {
            panic!("disk worker panic must retain typed join cause")
        };
        assert!(cause.is_panic());
        assert!(!error.to_string().contains("private-disk-sentinel"));
    }

    #[tokio::test]
    async fn disk_open_ready_loss_retains_panicking_worker_join() {
        let (sender, _receiver) = mpsc::channel(4);
        let (ready, opened) = oneshot::channel();
        let worker = tokio::spawn(async move {
            drop(ready);
            panic!("private-disk-sentinel");
        });
        let error = Disk::finish_open(sender, worker, opened)
            .await
            .err()
            .unwrap();
        assert_join(&error);
    }

    #[tokio::test]
    async fn disk_open_clean_ready_loss_remains_unavailable() {
        let (sender, _receiver) = mpsc::channel(4);
        let (ready, opened) = oneshot::channel();
        let worker = tokio::spawn(async move {
            drop(ready);
        });
        let error = Disk::finish_open(sender, worker, opened)
            .await
            .err()
            .unwrap();
        assert!(matches!(error.kind, OwnerErrorKind::Unavailable));
    }

    #[tokio::test]
    async fn disk_open_primary_error_survives_later_worker_panic() {
        let (sender, _receiver) = mpsc::channel(4);
        let (ready, opened) = oneshot::channel();
        let worker = tokio::spawn(async move {
            let _ = ready.send(Err(OwnerError::new(OwnerErrorKind::Capacity)));
            panic!("private-disk-sentinel");
        });
        let error = Disk::finish_open(sender, worker, opened)
            .await
            .err()
            .unwrap();
        assert!(matches!(error.kind, OwnerErrorKind::Capacity));
    }

    #[tokio::test]
    async fn disk_close_retains_panicking_worker_join() {
        let (sender, _receiver) = mpsc::channel(4);
        let worker = tokio::spawn(async {
            panic!("private-disk-sentinel");
        });
        let error = Disk { sender, worker }.close().await.unwrap_err();
        assert_join(&error);
    }
}
