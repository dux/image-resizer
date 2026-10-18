use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::{Action, ApiResult, Auth, Ctx, Out};

#[derive(Deserialize, JsonSchema)]
pub struct Show {}

#[derive(Serialize, JsonSchema)]
pub struct Settings {
    pub port: u16,
    pub workers: usize,
    pub quality: i32,
    pub max_size: i32,
    pub max_age: u64,
    pub max_cache_bytes: u64,
    pub max_source_bytes: u64,
    pub allowed_domains: Vec<String>,
    pub admin_emails: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct CacheStats {
    pub entries: i64,
    /// Blobs plus database files.
    pub bytes: u64,
    pub blob_bytes: u64,
    pub db_bytes: u64,
    pub average_bytes: u64,
    /// Whole percent of `max_cache_bytes`.
    pub usage_percent: u64,
}

#[derive(Serialize, JsonSchema)]
pub struct JobStats {
    pub workers_busy: usize,
    pub variants_in_flight: usize,
    pub sources_in_flight: usize,
}

#[derive(Serialize, JsonSchema)]
pub struct StatsOut {
    pub version: &'static str,
    pub uptime_secs: u64,
    pub settings: Settings,
    pub cache: CacheStats,
    pub jobs: JobStats,
}

impl Action for Show {
    const PATH: &'static str = "stats/show";
    const DOC: &'static str = "Settings, cache size and usage, and running jobs.";
    const AUTH: Auth = Auth::Admin;
    const WRITE: bool = false;
    type Output = StatsOut;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<StatsOut>> {
        let (entries, blob_bytes, db_bytes) = ctx
            .blocking(|app| {
                Ok((
                    app.store.count()?,
                    app.store.blob_bytes()?,
                    app.store.db_bytes(),
                ))
            })
            .await?;
        let c = &ctx.app.config;
        let bytes = blob_bytes + db_bytes;
        Ok(StatsOut {
            version: env!("CARGO_PKG_VERSION"),
            uptime_secs: ctx.app.started.elapsed().as_secs(),
            settings: Settings {
                port: c.port,
                workers: c.workers,
                quality: c.quality,
                max_size: c.max_size,
                max_age: c.max_age,
                max_cache_bytes: c.max_cache_bytes,
                max_source_bytes: c.max_source_bytes,
                allowed_domains: c.allowed_domains.clone(),
                admin_emails: c.admin_emails.clone(),
            },
            cache: CacheStats {
                entries,
                bytes,
                blob_bytes,
                db_bytes,
                average_bytes: if entries > 0 {
                    blob_bytes / entries as u64
                } else {
                    0
                },
                usage_percent: (bytes * 100 + c.max_cache_bytes / 2) / c.max_cache_bytes,
            },
            jobs: JobStats {
                workers_busy: ctx.app.workers.busy(),
                variants_in_flight: ctx.app.variants.len(),
                sources_in_flight: ctx.app.sources.len(),
            },
        }
        .into())
    }
}
