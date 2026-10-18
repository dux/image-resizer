#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use image_resizer::app::{self, App, AppState};
use image_resizer::config::Config;
use rs_vips::VipsImage;
use rs_vips::voption::{Setter, VOption};
use tower::ServiceExt;

static VIPS: Once = Once::new();

pub fn init() {
    VIPS.call_once(|| image_resizer::resize::pipeline::init().unwrap());
}

/// A 200x150 test image in `format`, drawn from the static JPEG.
pub fn image(format: &str, width: i32, height: i32) -> Vec<u8> {
    init();
    let jpg = std::fs::read("static/test.jpg").unwrap();
    let img = VipsImage::thumbnail_buffer_with_opts(
        &jpg,
        width,
        VOption::new().set("height", height).set("size", "force"),
    )
    .unwrap();
    match format {
        "png" => img.pngsave_buffer().unwrap(),
        "gif" => img.gifsave_buffer().unwrap(),
        "webp" => img.webpsave_buffer().unwrap(),
        "avif" => img
            .heifsave_buffer_with_opts(VOption::new().set("compression", "av1").set("effort", 0))
            .unwrap(),
        _ => img.jpegsave_buffer().unwrap(),
    }
}

/// Width and height of encoded bytes.
pub fn dims(data: &[u8]) -> (i32, i32) {
    init();
    let img = VipsImage::new_from_buffer(data, "").unwrap();
    (img.get_width(), img.get_height())
}

#[derive(Clone)]
struct Origin {
    hits: Arc<AtomicUsize>,
    jpeg: Arc<Vec<u8>>,
}

fn reply(content_type: &'static str, data: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, content_type)], data).into_response()
}

/// Local image server: fixed test images, error endpoints, slow and counted endpoints.
pub async fn origin() -> (String, Arc<AtomicUsize>) {
    init();
    let state = Origin {
        hits: Arc::default(),
        jpeg: Arc::new(image("jpeg", 200, 150)),
    };
    let hits = state.hits.clone();
    let router = Router::new()
        .route("/test.jpeg", get(|| async { reply("image/jpeg", image("jpeg", 200, 150)) }))
        .route("/test.png", get(|| async { reply("image/png", image("png", 200, 150)) }))
        .route("/test.gif", get(|| async { reply("image/gif", image("gif", 200, 150)) }))
        .route("/test.webp", get(|| async { reply("image/webp", image("webp", 200, 150)) }))
        .route("/test.avif", get(|| async { reply("image/avif", image("avif", 200, 150)) }))
        .route(
            "/test.svg",
            get(|| async {
                reply(
                    "image/svg+xml",
                    br#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="150"><rect width="200" height="150" fill="red"/></svg>"#.to_vec(),
                )
            }),
        )
        .route("/Flag.svg.png", get(|| async { reply("image/png", image("png", 200, 150)) }))
        .route("/big.jpeg", get(|| async { reply("image/jpeg", image("jpeg", 3200, 2400)) }))
        .route("/notfound", get(|| async { StatusCode::NOT_FOUND }))
        .route("/error500", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
        .route("/toomany", get(|| async { StatusCode::TOO_MANY_REQUESTS }))
        .route("/empty", get(|| async { reply("image/jpeg", Vec::new()) }))
        .route(
            "/slow.jpeg",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                reply("image/jpeg", image("jpeg", 200, 150))
            }),
        )
        .route(
            "/box.step",
            get(|| async { reply("application/step", std::fs::read("tests/fixtures/box.step").unwrap()) }),
        )
        .route(
            "/model",
            get(|| async { reply("application/octet-stream", std::fs::read("tests/fixtures/box.step").unwrap()) }),
        )
        .route(
            "/count/{*name}",
            get(|State(s): State<Origin>| async move {
                s.hits.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(300)).await;
                reply("image/jpeg", s.jpeg.to_vec())
            }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, hits)
}

pub struct Env {
    pub app: AppState,
    pub origin: String,
    pub hits: Arc<AtomicUsize>,
    pub _dir: tempfile::TempDir,
}

pub async fn env() -> Env {
    env_with(|_| {}).await
}

pub async fn env_with(tweak: impl FnOnce(&mut Config)) -> Env {
    init();
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::test(dir.path());
    tweak(&mut config);
    let app = App::new(config).unwrap();
    let (origin, hits) = origin().await;
    Env {
        app,
        origin,
        hits,
        _dir: dir,
    }
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub const AVIF: &str = "image/avif,image/webp,image/*";

impl Env {
    pub async fn get(&self, uri: &str, headers: &[(&str, &str)]) -> Reply {
        let mut request = Request::get(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }

    pub async fn send(&self, request: Request<Body>) -> Reply {
        let response = app::router(self.app.clone())
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    /// `GET {path}?{origin}{file}` with an Accept header.
    pub async fn resize(&self, path: &str, file: &str, accept: &str) -> Reply {
        self.get(
            &format!("{path}?{}{file}", self.origin),
            &[("accept", accept)],
        )
        .await
    }

    /// Waits until the background cache write for `url`/`key` landed.
    pub async fn wait_cached(&self, url: &str, key: &str) -> image_resizer::store::Entry {
        for _ in 0..100 {
            if let Some(entry) = self.app.store.get(url, key).unwrap() {
                return entry;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{url} {key} never cached");
    }
}
