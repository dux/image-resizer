// Work coalescing (one job per key, every caller shares its result) and the CPU worker limit.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use tokio::sync::{Semaphore, oneshot};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobError {
    pub message: String,
    /// Work still running or worth retrying (timeouts, cancellations, upstream 429): show the spinner.
    pub retryable: bool,
}

impl JobError {
    /// Retryable when the text says so, the same test Go applies to its error strings.
    pub fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        let lower = message.to_lowercase();
        let retryable = ["timeout", "deadline", "canceled", "cancelled", "status=429"]
            .iter()
            .any(|needle| lower.contains(needle));
        Self { message, retryable }
    }

    pub fn retry(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }

    pub fn internal(error: impl fmt::Display) -> Self {
        Self::new(format!("internal; {error}"))
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JobError {}

type Flight<V> = Shared<BoxFuture<'static, V>>;

/// Singleflight: the first caller of a key spawns the job, later callers join it. The job runs to the
/// end even when every caller stops waiting, so the next request finds its result in the cache.
pub struct Flights<V: Clone + Send + Sync + 'static> {
    map: Arc<Mutex<HashMap<String, Flight<V>>>>,
    cancelled: fn() -> V,
}

impl<V: Clone + Send + Sync + 'static> Flights<V> {
    pub fn new(cancelled: fn() -> V) -> Self {
        Self {
            map: Arc::default(),
            cancelled,
        }
    }

    /// Returns the shared result of the job for `key` and whether this call joined a running one.
    /// `job` returns the value for the waiters and a follow-up (e.g. the cache write) that runs
    /// before the key is released, so callers arriving in between still join instead of redoing the work.
    pub fn run<F, A>(&self, key: &str, job: impl FnOnce() -> F) -> (Flight<V>, bool)
    where
        F: Future<Output = (V, A)> + Send + 'static,
        A: Future<Output = ()> + Send + 'static,
    {
        let mut map = self.map.lock();
        if let Some(flight) = map.get(key) {
            return (flight.clone(), true);
        }
        let (tx, rx) = oneshot::channel();
        let cancelled = self.cancelled;
        let flight = async move { rx.await.unwrap_or_else(|_| cancelled()) }
            .boxed()
            .shared();
        map.insert(key.to_string(), flight.clone());
        drop(map);

        let (map, key, work) = (self.map.clone(), key.to_string(), job());
        tokio::spawn(async move {
            let (value, after) = work.await;
            let _ = tx.send(value);
            after.await;
            map.lock().remove(&key);
        });
        (flight, false)
    }

    pub fn len(&self) -> usize {
        self.map.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.lock().is_empty()
    }
}

/// Limits concurrent CPU work (libvips, external tools) to `WORKERS`; downloads do not take a slot.
#[derive(Clone)]
pub struct Workers {
    permits: Arc<Semaphore>,
    size: usize,
}

impl Workers {
    pub fn new(size: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(size)),
            size,
        }
    }

    /// Runs blocking `work` on the blocking pool once a worker slot is free.
    pub async fn cpu<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T, JobError> + Send + 'static,
    ) -> Result<T, JobError> {
        let _permit = self.permits.acquire().await.map_err(JobError::internal)?;
        tokio::task::spawn_blocking(work)
            .await
            .map_err(JobError::internal)?
    }

    /// Holds a worker slot around async work, e.g. an external tool.
    pub async fn slot<T>(&self, work: impl Future<Output = T>) -> T {
        let _permit = self.permits.acquire().await;
        work.await
    }

    /// Jobs waiting for a slot plus running ones, for the stats page.
    pub fn busy(&self) -> usize {
        self.size - self.permits.available_permits()
    }

    pub fn size(&self) -> usize {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[test]
    fn retryable_errors() {
        assert!(JobError::new("fetch-failed; timeout").retryable);
        assert!(JobError::new("context deadline exceeded").retryable);
        assert!(JobError::new("fetch-failed; status=429").retryable);
        assert!(JobError::new("request Canceled").retryable);
        assert!(!JobError::new("fetch-failed; status=404").retryable);
        assert!(!JobError::new("decode-failed; bad").retryable);
    }

    #[tokio::test]
    async fn coalesces_and_finishes_without_waiters() {
        let flights: Flights<usize> = Flights::new(|| 0);
        let runs = Arc::new(AtomicUsize::new(0));
        let job = |runs: Arc<AtomicUsize>| {
            move || async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                (runs.fetch_add(1, Ordering::SeqCst) + 1, async {})
            }
        };
        let (a, joined_a) = flights.run("k", job(runs.clone()));
        let (b, joined_b) = flights.run("k", job(runs.clone()));
        assert!(!joined_a && joined_b);
        assert_eq!((a.await, b.await), (1, 1));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(flights.len(), 0);

        let (c, _) = flights.run("k", job(runs.clone()));
        drop(c);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "dropped waiter must not cancel the job"
        );
    }
}
