// Background upkeep: hit and referer flushes, source expiry, LRU eviction and hourly reconcile.

use std::time::Duration;

use crate::app::AppState;
use crate::store;

const SOURCE_TTL_SECS: i64 = 86_400;
const EVICT_BATCH: usize = 500;

/// Writes the served-entry ids and the referer counters.
pub fn flush(app: &AppState) {
    let ids: Vec<i64> = std::mem::take(&mut *app.hits.lock()).into_iter().collect();
    if let Err(e) = app.store.touch(&ids) {
        tracing::error!(error = %e, "hit flush failed");
    }
    if let Err(e) = app.referers.flush(&app.store) {
        tracing::error!(error = %e, "referer flush failed");
    }
    if let Err(e) = app.keys.flush(&app.store) {
        tracing::error!(error = %e, "api key flush failed");
    }
}

/// Expires sources, then evicts least recently hit entries until the cache is under 90% of its limit.
pub fn cleanup(app: &AppState) -> store::Result<()> {
    let expired = app.store.expire_sources(SOURCE_TTL_SECS)?;
    if expired > 0 {
        tracing::info!(
            expired,
            "source cache cleanup: removed entries older than 24h"
        );
    }
    let limit = app.config.max_cache_bytes;
    let start = app.store.total_bytes()?;
    if start <= limit {
        return Ok(());
    }
    tracing::info!(
        mb = start / 1_048_576,
        limit_mb = limit / 1_048_576,
        "cache over limit, evicting"
    );
    let target = limit / 10 * 9;
    let mut evicted = 0;
    while app.store.total_bytes()? > target {
        let n = app.store.evict_lru(EVICT_BATCH)?;
        if n == 0 {
            break;
        }
        evicted += n;
    }
    tracing::info!(
        evicted,
        mb = app.store.total_bytes()? / 1_048_576,
        "cache eviction done"
    );
    Ok(())
}

pub fn spawn(app: AppState) {
    let every = |period: Duration, app: AppState, task: fn(&AppState)| {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await;
            loop {
                interval.tick().await;
                let app = app.clone();
                let _ = tokio::task::spawn_blocking(move || task(&app)).await;
            }
        });
    };
    every(Duration::from_secs(5), app.clone(), |app| {
        if let Err(e) = app.referers.flush(&app.store) {
            tracing::error!(error = %e, "referer flush failed");
        }
    });
    every(Duration::from_secs(30), app.clone(), flush);
    every(Duration::from_secs(60), app.clone(), |app| {
        if let Err(e) = cleanup(app) {
            tracing::error!(error = %e, "cache cleanup failed");
        }
    });
    every(Duration::from_secs(3600), app, |app| {
        match app.store.reconcile() {
            Ok(report) if report != store::ReconcileReport::default() => {
                tracing::warn!(?report, "cache reconciled");
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "cache reconcile failed"),
        }
    });
}
