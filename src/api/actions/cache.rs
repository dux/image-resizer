use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::{Action, ApiError, ApiResult, Auth, Ctx, Out};
use crate::store::Entry;

const MAX_PER_PAGE: i64 = 200;

#[derive(Deserialize, JsonSchema)]
pub struct List {
    /// 1-based, clamped to the last page.
    pub page: Option<i64>,
    /// Default 50, at most 200.
    pub per_page: Option<i64>,
    /// `source`, `step` or `variant`.
    pub kind: Option<String>,
    /// Substring of the source URL.
    pub url: Option<String>,
}

impl Action for List {
    const PATH: &'static str = "cache/list";
    const DOC: &'static str =
        "Cached entries, newest first. Paging is in `meta`: page, per_page, total, total_pages.";
    const AUTH: Auth = Auth::Admin;
    const WRITE: bool = false;
    type Output = Vec<Entry>;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<Vec<Entry>>> {
        let per_page = self.per_page.unwrap_or(50).clamp(1, MAX_PER_PAGE);
        let kind = self.kind.filter(|k| !k.is_empty());
        if kind
            .as_deref()
            .is_some_and(|k| !matches!(k, "source" | "step" | "variant"))
        {
            return Err(ApiError::invalid("kind must be source, step or variant."));
        }
        let url = self
            .url
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty());
        let page = self.page.unwrap_or(1);
        let (items, total, page) = ctx
            .blocking(move |app| {
                app.store
                    .list(page, per_page, kind.as_deref(), url.as_deref())
            })
            .await?;
        let total_pages = ((total + per_page - 1) / per_page).max(1);
        Ok(Out::from(items)
            .meta("page", page)
            .meta("per_page", per_page)
            .meta("total", total)
            .meta("total_pages", total_pages))
    }
}

#[derive(Serialize, JsonSchema)]
pub struct Deleted {
    pub deleted: usize,
}

#[derive(Deserialize, JsonSchema)]
pub struct Delete {
    pub id: i64,
}

impl Action for Delete {
    const PATH: &'static str = "cache/delete";
    const DOC: &'static str = "Deletes one cached entry by id.";
    const AUTH: Auth = Auth::Admin;
    const WRITE: bool = true;
    type Output = Deleted;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<Deleted>> {
        let id = self.id;
        let deleted = ctx.blocking(move |app| app.store.delete_id(id)).await?;
        if deleted == 0 {
            return Err(ApiError::not_found(format!("No cache entry {id}.")));
        }
        tracing::info!(id, by = %ctx.email(), "deleted cached image");
        Ok(Deleted { deleted }.into())
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct Clear {
    /// `1h` / `24h`: added in that window; `7d`: older than 7 days; `all` (default).
    pub period: Option<String>,
}

impl Action for Clear {
    const PATH: &'static str = "cache/clear";
    const DOC: &'static str = "Deletes cached entries by period: `1h` and `24h` (added within), `7d` (older than), `all`.";
    const AUTH: Auth = Auth::Admin;
    const WRITE: bool = true;
    type Output = Deleted;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<Deleted>> {
        let period = self.period.unwrap_or_else(|| "all".into());
        if !matches!(period.as_str(), "1h" | "24h" | "7d" | "all") {
            return Err(ApiError::invalid("period must be 1h, 24h, 7d or all."));
        }
        let p = period.clone();
        let deleted = ctx.blocking(move |app| app.store.clear(&p)).await?;
        tracing::info!(%period, deleted, by = %ctx.email(), "cache cleared");
        Ok(Out::from(Deleted { deleted }).message(format!("{deleted} cached images removed")))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct Purge {
    /// Source URL exactly as cached (after normalization, e.g. with `https://`).
    pub url: String,
}

impl Action for Purge {
    const PATH: &'static str = "cache/purge";
    const DOC: &'static str = "Deletes every entry of one source URL (source, STEP data and all variants), e.g. after the image changed.";
    const AUTH: Auth = Auth::Admin;
    const WRITE: bool = true;
    type Output = Deleted;

    async fn run(self, ctx: &Ctx) -> ApiResult<Out<Deleted>> {
        let url = crate::resize::params::normalize_src(self.url.trim().to_string());
        if url.is_empty() {
            return Err(ApiError::invalid("url is required."));
        }
        let u = url.clone();
        let deleted = ctx.blocking(move |app| app.store.purge_url(&u)).await?;
        tracing::info!(%url, deleted, by = %ctx.email(), "cache purged");
        Ok(Out::from(Deleted { deleted }).meta("url", json!(url)))
    }
}
