use std::future::Future;
use tokio_util::sync::CancellationToken;

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Cancel the root lifecycle whenever a supervised future completes or unwinds.
pub async fn cancel_root_when_complete<T>(
    cancel: CancellationToken,
    future: impl Future<Output = T>,
) -> T {
    let _cancel_on_drop = CancelOnDrop(cancel);
    future.await
}
