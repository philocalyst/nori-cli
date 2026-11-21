//! Keeping a catalog current without blocking the async runtime.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::Catalog;
use super::CatalogCache;
use super::Roots;
use crate::EvaluatorOptions;

/// Loads the catalog once, publishes it, then re-checks the modules' mtimes every `every` and
/// publishes again only when the catalog changed. All evaluation and disk access runs on the
/// blocking pool; the task ends when every receiver is dropped.
///
/// The receiver starts with an empty catalog and is marked changed when the first load is
/// published, so `changed().await` waits for the first real value.
pub fn watch(
    roots: Roots,
    options: EvaluatorOptions,
    every: Duration,
) -> (watch::Receiver<Arc<Catalog>>, JoinHandle<()>) {
    let (sender, receiver) = watch::channel(Arc::new(Catalog::default()));
    let handle = tokio::spawn(async move {
        let roots = Arc::new(roots);
        let options = Arc::new(options);
        let mut cache = CatalogCache::default();
        let mut first = true;
        loop {
            let (task_roots, task_options) = (Arc::clone(&roots), Arc::clone(&options));
            let Ok((returned, catalog, changed)) = tokio::task::spawn_blocking(move || {
                let (catalog, changed) = cache.reload(&task_roots, &task_options);
                (cache, catalog, changed)
            })
            .await
            else {
                return;
            };
            cache = returned;
            if (first || changed) && sender.send(Arc::new(catalog)).is_err() {
                return;
            }
            first = false;
            tokio::select! {
                () = tokio::time::sleep(every) => {}
                () = sender.closed() => return,
            }
        }
    });
    (receiver, handle)
}
