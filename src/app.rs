// Shared state and the one route table.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use parking_lot::Mutex;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::auth::authcog::{self, LoginStates};
use crate::auth::keys::Keys;
use crate::auth::session::SessionKey;
use crate::config::{AppEnv, Config};
use crate::fetch::Fetcher;
use crate::jobs::{Flights, Workers};
use crate::referers::Referers;
use crate::resize::handler::{self, text};
use crate::resize::params::FORCED_FORMATS;
use crate::resize::work::{self, RawResult, SourceResult, VariantResult};
use crate::store::Store;

pub struct App {
    pub config: Config,
    pub store: Arc<Store>,
    pub fetcher: Fetcher,
    pub workers: Workers,
    pub variants: Flights<VariantResult>,
    pub sources: Flights<SourceResult>,
    pub steps: Flights<RawResult>,
    pub referers: Referers,
    /// Entry ids served since the last flush, for LRU eviction.
    pub hits: Mutex<HashSet<i64>>,
    pub session: SessionKey,
    pub logins: LoginStates,
    pub keys: Keys,
    /// For the `AuthCog` exchange: no redirects, short timeout.
    pub auth_http: reqwest::Client,
    pub started: Instant,
}

pub type AppState = Arc<App>;

impl App {
    pub fn new(config: Config) -> Result<AppState, String> {
        let store =
            Arc::new(Store::open(&config.data_dir).map_err(|e| format!("cache store: {e}"))?);
        let report = store
            .reconcile()
            .map_err(|e| format!("cache reconcile: {e}"))?;
        if report != crate::store::ReconcileReport::default() {
            tracing::warn!(?report, "cache reconciled");
        }
        let referers = Referers::load(&store).map_err(|e| format!("referers: {e}"))?;
        let keys = Keys::load(&store).map_err(|e| format!("api keys: {e}"))?;
        let session = SessionKey::load_or_create(&config.data_dir.join("session.key"))?;
        Ok(Arc::new(Self {
            fetcher: Fetcher::new(!config.allow_private_sources, config.max_source_bytes),
            workers: Workers::new(config.workers),
            variants: Flights::new(work::cancelled),
            sources: Flights::new(work::cancelled),
            steps: Flights::new(work::cancelled),
            referers,
            hits: Mutex::default(),
            session,
            logins: authcog::login_states(),
            keys,
            auth_http: authcog::http_client(),
            started: Instant::now(),
            store,
            config,
        }))
    }
}

const FAVICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32" viewBox="0 0 32 32">
    <rect width="32" height="32" fill="#2563eb" rx="6"/>
    <path d="M8 10h3l2-2h6l2 2h3c1.1 0 2 .9 2 2v10c0 1.1-.9 2-2 2H8c-1.1 0-2-.9-2-2V12c0-1.1.9-2 2-2z" fill="white"/>
    <circle cx="16" cy="17" r="4" fill="#2563eb"/>
    <circle cx="16" cy="17" r="2" fill="white"/>
    <rect x="20" y="13" width="2" height="1" fill="#2563eb"/>
  </svg>"##;

async fn favicon(axum::extract::State(app): axum::extract::State<AppState>) -> Response {
    let mut response = FAVICON.into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"));
    if let Ok(value) = HeaderValue::from_str(&format!(
        "public, max-age={}, immutable",
        app.config.max_age
    )) {
        headers.insert(CACHE_CONTROL, value);
    }
    response
}

async fn not_found() -> Response {
    text(StatusCode::NOT_FOUND, "404 page not found")
}

pub fn router(app: AppState) -> Router {
    let index = || {
        tower::ServiceBuilder::new()
            .layer(SetResponseHeaderLayer::overriding(
                CACHE_CONTROL,
                HeaderValue::from_static("no-cache"),
            ))
            .service(ServeFile::new("web/index.html"))
    };
    let mut router = Router::new()
        .route_service("/", index())
        .route_service("/api", index())
        .route("/api/{group}/{action}", post(crate::api::dispatch))
        .route("/login", get(authcog::start))
        .route("/authcog", get(authcog::callback))
        .route("/admin/blob/{id}", get(crate::admin::blob))
        .route("/admin/logs", get(crate::admin::logs_socket))
        .route("/r/", get(handler::resize))
        .route("/r/{*params}", get(handler::resize))
        .route("/resize", get(handler::resize))
        .route("/favicon.ico", get(favicon));
    for ext in FORCED_FORMATS {
        router = router.route(&format!("/r.{ext}"), get(handler::resize));
    }
    if app.config.env != AppEnv::Production {
        router = router.route("/dev/login", get(authcog::dev));
    }
    router
        .nest_service("/static", ServeDir::new("static"))
        .nest_service(
            "/web",
            tower::ServiceBuilder::new()
                .layer(SetResponseHeaderLayer::overriding(
                    CACHE_CONTROL,
                    HeaderValue::from_static("no-cache"),
                ))
                .service(ServeDir::new("web")),
        )
        .fallback(not_found)
        .with_state(app)
}
