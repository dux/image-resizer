// HTTP side of /r/*, /r.{ext} and /resize: checks, cache lookup, job wait, and the Go response headers.

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{
    ACCEPT, ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, HOST,
    IF_NONE_MATCH, REFERER, RETRY_AFTER, VARY,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tokio_util::io::ReaderStream;

use super::params::{Parsed, parse_request};
use super::pipeline::Accepts;
use super::svg;
use super::work::{self, Rendered, VariantJob};
use crate::app::AppState;
use crate::fetch::blocked_host;
use crate::referers::base_domain;
use crate::step;
use crate::store::Entry;

static X_CACHE: HeaderName = HeaderName::from_static("x-cache");
static X_INFO: HeaderName = HeaderName::from_static("x-info");

/// Plain-text error the way Go's `http.Error` writes it.
pub fn text(status: StatusCode, message: &str) -> Response {
    let mut response = (status, format!("{message}\n")).into_response();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Remote SVG is served from our origin, so it must never run script there: an admin opening a
/// hostile SVG would otherwise drive the admin API with their session. `sandbox` gives it an opaque
/// origin; `<img>` rendering is unaffected.
pub fn harden(headers: &mut HeaderMap, content_type: &str) {
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    if content_type.starts_with("image/svg") {
        headers.insert(
            "content-security-policy",
            HeaderValue::from_static(
                "default-src 'none'; style-src 'unsafe-inline'; img-src data:; sandbox",
            ),
        );
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &HeaderName) -> &'a str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

fn set(headers: &mut HeaderMap, name: &HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name.clone(), value);
    }
}

/// Lowercase host of a source URL without IPv6 brackets.
pub fn source_host(src: &str) -> Option<String> {
    let url = url::Url::parse(src).ok()?;
    Some(
        url.host_str()?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_lowercase(),
    )
}

/// `ALLOWED_DOMAINS` check (`*.x.com` wildcards); siblings of the service's own domain always pass.
pub fn allowed_source(allowed: &[String], src: &str, service_host: &str) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let Some(host) = source_host(src) else {
        return false;
    };
    let service = service_host.to_lowercase();
    let service = service
        .rsplit_once(':')
        .map_or(service.as_str(), |(h, _)| h);
    if let Some((_, base)) = service.split_once('.')
        && (host == base || host.ends_with(&format!(".{base}")))
    {
        return true;
    }
    allowed.iter().any(|a| match a.strip_prefix("*.") {
        Some(root) => host == root || host.ends_with(&format!(".{root}")),
        None => host == *a,
    })
}

fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    header_str(headers, &IF_NONE_MATCH)
        .split(',')
        .map(str::trim)
        .any(|tag| tag == "*" || tag.trim_start_matches("W/") == etag)
}

pub async fn resize(State(app): State<AppState>, uri: Uri, headers: HeaderMap) -> Response {
    let path = percent_encoding::percent_decode_str(uri.path())
        .decode_utf8_lossy()
        .into_owned();
    let Parsed { params, src } = match parse_request(&path, uri.query().unwrap_or_default()) {
        Ok(parsed) => parsed,
        Err(message) => return text(StatusCode::BAD_REQUEST, &message),
    };

    if !app.config.allow_private_sources && source_host(&src).is_none_or(|h| blocked_host(&h)) {
        return text(StatusCode::FORBIDDEN, "Source URL not allowed");
    }
    if !allowed_source(
        &app.config.allowed_domains,
        &src,
        header_str(&headers, &HOST),
    ) {
        let host = source_host(&src).unwrap_or_default();
        return text(
            StatusCode::FORBIDDEN,
            &format!("Domain '{host}' is not allowed"),
        );
    }

    let domain = base_domain(header_str(&headers, &REFERER));
    app.referers.count(&domain);
    if app.referers.is_disabled(&domain) {
        return text(
            StatusCode::FORBIDDEN,
            &format!("Domain '{domain}' is forbidden from using this resize service"),
        );
    }

    if params.format == "glb" {
        return serve_glb(&app, src, params, &headers).await;
    }

    let accept = header_str(&headers, &ACCEPT);
    let negotiated = params.format.is_empty();
    let (suffix, accepts) = if negotiated {
        let accepts = Accepts {
            avif: accept.contains("image/avif"),
            webp: accept.contains("image/webp"),
        };
        let suffix = if accepts.avif {
            "avif"
        } else if accepts.webp {
            "webp"
        } else {
            "jpg"
        };
        (suffix.to_string(), accepts)
    } else {
        (params.format.clone(), Accepts::default())
    };
    let mut cache_key = format!("{}_{suffix}", params.cache_key);
    if step::is_step_source(&src) {
        cache_key = format!(
            "cam-{}_{cache_key}",
            step::cam_bg_token(&params.cam_key, &params.bg_key)
        );
    }
    let vary = |mut response: Response| {
        if negotiated {
            response
                .headers_mut()
                .insert(VARY, HeaderValue::from_static("Accept"));
        }
        response
    };

    if let Some(entry) = lookup(&app, &src, &cache_key).await {
        let info = format!(
            "from-cache; params={}; format={}",
            params.cache_key, entry.format
        );
        if let Some(response) = serve_entry(&app, &entry, &headers, &info, None).await {
            tracing::info!(%src, params = %params.cache_key, "serving cached image");
            return vary(response);
        }
    }

    let (width, height) = (params.width, params.height);
    let flight = work::submit(
        &app,
        VariantJob {
            src: src.clone(),
            params,
            cache_key: cache_key.clone(),
            accepts,
        },
    );
    let response = match tokio::time::timeout(app.config.wait_timeout, flight).await {
        Ok(Ok(rendered)) => rendered_response(&app, &rendered, &headers, None),
        Ok(Err(e)) if e.retryable => {
            tracing::info!(%src, error = %e, "resize still pending");
            spinner(width, height)
        }
        Ok(Err(e)) => {
            tracing::warn!(%src, error = %e, "resize failed");
            error_svg(width, height, &e.message)
        }
        Err(_) => {
            tracing::info!(%src, key = %cache_key, "worker timeout, returning spinner");
            spinner(width, height)
        }
    };
    vary(response)
}

async fn lookup(app: &AppState, src: &str, key: &str) -> Option<Entry> {
    let (store, src, key) = (app.store.clone(), src.to_string(), key.to_string());
    match tokio::task::spawn_blocking(move || store.get(&src, &key)).await {
        Ok(Ok(entry)) => entry,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "cache lookup failed");
            None
        }
        Err(_) => None,
    }
}

fn cache_headers(app: &AppState, headers: &mut HeaderMap, x_cache: &str, etag: &str) {
    set(
        headers,
        &CACHE_CONTROL,
        &format!("public, max-age={}, immutable", app.config.max_age),
    );
    set(headers, &X_CACHE, x_cache);
    set(headers, &ETAG, etag);
}

/// A cache HIT: 304 from the row alone, else the file streamed. `None` when the file is gone.
async fn serve_entry(
    app: &AppState,
    entry: &Entry,
    request: &HeaderMap,
    info: &str,
    cors: Option<()>,
) -> Option<Response> {
    let etag = entry.etag();
    app.hits.lock().insert(entry.id);
    let mut response = if if_none_match(request, &etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let Ok(file) = tokio::fs::File::open(app.store.blob_path(&entry.hash)).await else {
            let (store, entry) = (app.store.clone(), entry.clone());
            let _ = tokio::task::spawn_blocking(move || store.forget_missing(&entry)).await;
            return None;
        };
        let mut response = Body::from_stream(ReaderStream::new(file)).into_response();
        let headers = response.headers_mut();
        set(headers, &CONTENT_TYPE, &entry.content_type);
        set(headers, &CONTENT_LENGTH, &entry.size.to_string());
        harden(headers, &entry.content_type);
        response
    };
    let headers = response.headers_mut();
    cache_headers(app, headers, "HIT", &etag);
    set(headers, &X_INFO, info);
    if cors.is_some() {
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    }
    Some(response)
}

fn rendered_response(
    app: &AppState,
    rendered: &Rendered,
    request: &HeaderMap,
    cors: Option<()>,
) -> Response {
    let etag = crate::store::etag(&rendered.hash);
    let mut response = if if_none_match(request, &etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let mut response = Body::from(rendered.data.clone()).into_response();
        let headers = response.headers_mut();
        set(headers, &CONTENT_TYPE, &rendered.content_type);
        set(headers, &CONTENT_LENGTH, &rendered.data.len().to_string());
        harden(headers, &rendered.content_type);
        response
    };
    let headers = response.headers_mut();
    cache_headers(app, headers, "MISS", &etag);
    set(headers, &X_INFO, &rendered.info);
    if cors.is_some() {
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    }
    response
}

/// Placeholder while the job keeps going; the browser re-fetches after 10s.
fn spinner(width: i32, height: i32) -> Response {
    let body = svg::spinner(width, height);
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"));
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-cache, max-age=10"),
    );
    headers.insert(RETRY_AFTER, HeaderValue::from_static("10"));
    headers.insert(X_CACHE.clone(), HeaderValue::from_static("QUEUED"));
    headers.insert(
        X_INFO.clone(),
        HeaderValue::from_static("processing; queued; retry-after-10s"),
    );
    response
}

fn error_svg(width: i32, height: i32, message: &str) -> Response {
    let mut response = svg::error(width, height).into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"));
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-cache, max-age=60"),
    );
    headers.insert(X_CACHE.clone(), HeaderValue::from_static("MISS"));
    let info: String = format!("error; {message}")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    set(headers, &X_INFO, &info);
    response
}

/// STEP to GLB, cached under `glb`; CORS is open because three.js loads models with fetch.
async fn serve_glb(
    app: &AppState,
    src: String,
    params: super::params::ResizeParams,
    request: &HeaderMap,
) -> Response {
    if let Some(entry) = lookup(app, &src, "glb").await
        && let Some(response) = serve_entry(app, &entry, request, "from-cache; glb", Some(())).await
    {
        return response;
    }
    let flight = work::submit(
        app,
        VariantJob {
            src: src.clone(),
            params,
            cache_key: "glb".into(),
            accepts: Accepts::default(),
        },
    );
    match tokio::time::timeout(app.config.wait_timeout, flight).await {
        Ok(Ok(rendered)) => rendered_response(app, &rendered, request, Some(())),
        Ok(Err(e)) => text(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("STEP to GLB failed: {e}"),
        ),
        Err(_) => {
            tracing::info!(%src, "worker timeout, GLB still converting");
            let mut response = text(
                StatusCode::ACCEPTED,
                "GLB conversion in progress, retry shortly",
            );
            let headers = response.headers_mut();
            headers.insert(RETRY_AFTER, HeaderValue::from_static("10"));
            headers.insert(
                CACHE_CONTROL,
                HeaderValue::from_static("no-cache, max-age=10"),
            );
            headers.insert(X_CACHE.clone(), HeaderValue::from_static("QUEUED"));
            response
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_domains() {
        let allowed = vec!["example.com".to_string(), "*.cdn.net".to_string()];
        assert!(allowed_source(&[], "https://any.org/a", "img.x.com"));
        assert!(allowed_source(
            &allowed,
            "https://example.com/a",
            "img.x.com"
        ));
        assert!(!allowed_source(
            &allowed,
            "https://sub.example.com/a",
            "img.x.com"
        ));
        assert!(allowed_source(&allowed, "https://a.cdn.net/a", "img.x.com"));
        assert!(allowed_source(&allowed, "https://cdn.net/a", "img.x.com"));
        assert!(!allowed_source(
            &allowed,
            "https://evilcdn.net/a",
            "img.x.com"
        ));
        assert!(allowed_source(
            &allowed,
            "https://static.x.com/a",
            "img.x.com:8080"
        ));
        assert!(allowed_source(&allowed, "https://x.com/a", "img.x.com"));
        assert!(!allowed_source(&allowed, "not a url", "img.x.com"));
    }

    #[test]
    fn etag_matching() {
        let mut h = HeaderMap::new();
        h.insert(IF_NONE_MATCH, HeaderValue::from_static("\"a\", W/\"abc\""));
        assert!(if_none_match(&h, "\"abc\""));
        assert!(!if_none_match(&h, "\"zzz\""));
        assert!(!if_none_match(&HeaderMap::new(), "\"abc\""));
    }
}
