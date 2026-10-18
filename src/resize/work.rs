// Jobs behind a cache miss: source once per URL (download or STEP render), then one variant per key.
// Both levels are coalesced, and results are written to the store before the flight is released.

use bytes::Bytes;

use super::params::ResizeParams;
use super::pipeline::{self, Accepts};
use super::svg;
use crate::app::AppState;
use crate::jobs::JobError;
use crate::step;
use crate::store::{self, Kind, NewEntry};

#[derive(Clone, Debug)]
pub struct Rendered {
    pub data: Bytes,
    pub content_type: String,
    pub format: String,
    /// Format of the source the output came from, kept on the cache entry.
    pub origin: String,
    pub info: String,
    pub hash: String,
}

pub type VariantResult = Result<Rendered, JobError>;

#[derive(Clone, Debug)]
pub struct Source {
    pub data: Bytes,
    /// Format of the cached bytes.
    pub format: String,
    /// Format of the original download.
    pub origin: String,
}

impl Source {
    pub fn is_svg(&self) -> bool {
        self.format == "svg"
    }
}

pub type SourceResult = Result<Source, JobError>;
pub type RawResult = Result<Bytes, JobError>;

pub fn cancelled<T>() -> Result<T, JobError> {
    Err(JobError::retry("job canceled"))
}

pub struct VariantJob {
    pub src: String,
    pub params: ResizeParams,
    pub cache_key: String,
    pub accepts: Accepts,
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> store::Result<T> + Send + 'static,
) -> Result<T, JobError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(JobError::internal)?
        .map_err(JobError::internal)
}

/// Cached bytes of `url`/`key`, `None` on a miss (or when its file vanished).
async fn cached(
    app: &AppState,
    url: &str,
    key: &str,
) -> Result<Option<(store::Entry, Bytes)>, JobError> {
    let (store, url, key) = (app.store.clone(), url.to_string(), key.to_string());
    blocking(move || {
        let Some(entry) = store.get(&url, &key)? else {
            return Ok(None);
        };
        Ok(store
            .read_bytes(&entry)?
            .map(|data| (entry, Bytes::from(data))))
    })
    .await
}

struct Pending {
    url: String,
    key: String,
    kind: Kind,
    content_type: String,
    format: String,
    origin: String,
    data: Bytes,
}

/// Store write that runs after the waiters got their result; failures are logged, the image is still served.
async fn save(app: AppState, p: Pending) {
    let store = app.store.clone();
    let saved = tokio::task::spawn_blocking(move || {
        let entry = NewEntry {
            url: &p.url,
            key: &p.key,
            kind: p.kind,
            content_type: &p.content_type,
            format: &p.format,
            origin_format: &p.origin,
        };
        store.put(&entry, &p.data)
    })
    .await;
    match saved {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::error!(error = %e, "cache write failed"),
        Err(e) => tracing::error!(error = %e, "cache write panicked"),
    }
}

/// Starts (or joins) the variant job for `src|cache_key`.
pub fn submit(
    app: &AppState,
    job: VariantJob,
) -> futures_util::future::Shared<futures_util::future::BoxFuture<'static, VariantResult>> {
    let key = format!("{}|{}", job.src, job.cache_key);
    let app2 = app.clone();
    let (flight, joined) = app.variants.run(&key, move || async move {
        let timeout = app2.config.job_timeout;
        let result = tokio::time::timeout(timeout, render(&app2, &job))
            .await
            .unwrap_or_else(|_| Err(JobError::retry("job-timeout; deadline exceeded")));
        let p = &job.params;
        let cache = result
            .as_ref()
            .ok()
            .filter(|r| r.format != "svg" && (p.width > 0 || p.height > 0 || !p.format.is_empty()))
            .cloned();
        let after = async move {
            if let Some(r) = cache {
                let (url, key, kind) = (job.src, job.cache_key, Kind::Variant);
                save(
                    app2,
                    Pending {
                        url,
                        key,
                        kind,
                        content_type: r.content_type,
                        format: r.format,
                        origin: r.origin,
                        data: r.data,
                    },
                )
                .await;
            }
        };
        (result, after)
    });
    if joined {
        tracing::info!(%key, "coalescing request");
    }
    flight
}

async fn render(app: &AppState, job: &VariantJob) -> VariantResult {
    if job.params.format == "glb" {
        return glb(app, &job.src).await;
    }
    let source = ensure_source(app, &job.src, &job.params).await?;
    if source.is_svg() && job.params.format.is_empty() {
        return Ok(Rendered {
            hash: store::hash(&source.data),
            data: source.data,
            content_type: "image/svg+xml".into(),
            format: "svg".into(),
            origin: "svg".into(),
            info: "source-cache; format=svg; no-manipulation".into(),
        });
    }
    let (params, accepts, quality, max) = (
        job.params.clone(),
        job.accepts,
        app.config.quality,
        app.config.max_size,
    );
    let origin = source.origin.clone();
    let (encoded, hash) = app
        .workers
        .cpu(move || {
            let encoded = pipeline::render_variant(
                &source.data,
                &source.origin,
                &params,
                accepts,
                quality,
                max,
            )?;
            let hash = store::hash(&encoded.data);
            Ok((encoded, hash))
        })
        .await?;
    Ok(Rendered {
        info: format!(
            "source-cache; params={}; input={}; output={}",
            job.params.cache_key, origin, encoded.format
        ),
        data: encoded.data.into(),
        content_type: encoded.mime.into(),
        format: encoded.format.into(),
        origin,
        hash,
    })
}

/// The cached source of a URL: from the store, or downloaded (or STEP-rendered) once for all variants.
pub async fn ensure_source(app: &AppState, src: &str, params: &ResizeParams) -> SourceResult {
    let is_step = step::is_step_source(src);
    let key = if is_step {
        step::source_key(&params.cam_key, &params.bg_key)
    } else {
        "source".to_string()
    };
    if let Some((entry, data)) = cached(app, src, &key).await? {
        return Ok(Source {
            data,
            format: entry.format,
            origin: entry.origin_format,
        });
    }

    let (app2, src2, key2, params) = (app.clone(), src.to_string(), key.clone(), params.clone());
    let (flight, joined) = app.sources.run(&format!("{src}|{key}"), move || async move {
        // a flight that finished between our lookup and this job already stored the source
        let result = match cached(&app2, &src2, &key2).await {
            Ok(Some((entry, data))) => Ok((Source { data, format: entry.format, origin: entry.origin_format }, false)),
            Ok(None) if is_step => render_step_source(&app2, &src2, &params).await.map(|s| (s, true)),
            Ok(None) => fetch_source(&app2, &src2).await.map(|s| (s, true)),
            Err(e) => Err(e),
        };
        let save_it = result.as_ref().ok().filter(|(_, fresh)| *fresh).map(|(s, _)| s.clone());
        let after = async move {
            if let Some(s) = save_it {
                tracing::info!(src = %src2, key = %key2, origin = %s.origin, stored = %s.format, kb = s.data.len() / 1024, "source cached");
                let content_type = pipeline::mime_of(&s.format).into();
                save(app2, Pending { url: src2, key: key2, kind: Kind::Source, content_type, format: s.format, origin: s.origin, data: s.data }).await;
            }
        };
        (result.map(|(s, _)| s), after)
    });
    if joined {
        tracing::info!(%src, "source fetch coalescing");
    }
    flight.await
}

async fn fetch_source(app: &AppState, src: &str) -> SourceResult {
    let (body, content_type) = app.fetcher.download(src).await?;
    // SVG is passed through; raster bytes always go through the pipeline
    if svg::is_svg_source(&content_type, src, &body) {
        return Ok(Source {
            data: body,
            format: "svg".into(),
            origin: "svg".into(),
        });
    }
    // STEP without a .step extension: render the default view and treat it as a PNG download
    let body = if step::is_step_data(&body) {
        app.workers
            .slot(step::render_png(
                &app.config.f3d_bin,
                &body,
                "",
                true,
                app.config.max_size,
            ))
            .await?
    } else {
        body.to_vec()
    };
    let max = app.config.max_size;
    let built = app
        .workers
        .cpu(move || pipeline::build_source(body, max))
        .await?;
    Ok(Source {
        data: built.data.into(),
        format: built.format.into(),
        origin: built.origin.into(),
    })
}

async fn render_step_source(app: &AppState, src: &str, params: &ResizeParams) -> SourceResult {
    let raw = ensure_step_raw(app, src).await?;
    let png = app
        .workers
        .slot(step::render_png(
            &app.config.f3d_bin,
            &raw,
            &params.cam_dir,
            params.bg_transparent,
            app.config.max_size,
        ))
        .await?;
    let (transparent, max) = (params.bg_transparent, app.config.max_size);
    let data = app
        .workers
        .cpu(move || pipeline::build_step_source(&png, transparent, max))
        .await?;
    // fallback encoding treats renders as PNG-origin
    Ok(Source {
        data: data.into(),
        format: "webp".into(),
        origin: "png".into(),
    })
}

/// Raw STEP bytes of a URL, downloaded once and cached under `step`.
async fn ensure_step_raw(app: &AppState, src: &str) -> RawResult {
    if let Some((_, data)) = cached(app, src, "step").await? {
        return Ok(data);
    }
    let (app2, src2) = (app.clone(), src.to_string());
    let (flight, _) = app.steps.run(&format!("{src}|step"), move || async move {
        let result = match cached(&app2, &src2, "step").await {
            Ok(Some((_, data))) => Ok((data, false)),
            Ok(None) => match app2.fetcher.download(&src2).await {
                Ok((data, _)) if step::is_step_data(&data) => Ok((data, true)),
                Ok(_) => Err(JobError::new("not-a-step-file")),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        let save_it = result
            .as_ref()
            .ok()
            .filter(|(_, fresh)| *fresh)
            .map(|(d, _)| d.clone());
        let after = async move {
            if let Some(data) = save_it {
                let (key, content_type, format, origin) = (
                    "step".into(),
                    "application/step".into(),
                    "step".into(),
                    "step".into(),
                );
                save(
                    app2,
                    Pending {
                        url: src2,
                        key,
                        kind: Kind::Step,
                        content_type,
                        format,
                        origin,
                        data,
                    },
                )
                .await;
            }
        };
        (result.map(|(d, _)| d), after)
    });
    flight.await
}

async fn glb(app: &AppState, src: &str) -> VariantResult {
    let raw = ensure_step_raw(app, src).await?;
    let data = app
        .workers
        .slot(step::convert_glb(&app.config.step2glb_bin, &raw))
        .await?;
    Ok(Rendered {
        info: format!("step-to-glb; {:.1} KB", data.len() as f64 / 1024.0),
        hash: store::hash(&data),
        data: data.into(),
        content_type: "model/gltf-binary".into(),
        format: "glb".into(),
        origin: "step".into(),
    })
}
