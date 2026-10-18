// POST /api/{group}/{action}: one registry, flat params (JSON or form), one envelope
// {success, status, message, data, error, meta} for every answer.

pub mod actions;
pub mod registry;

use std::future::Future;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::app::AppState;
use crate::auth::{self, Principal, Via};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Auth {
    /// Anyone.
    Public,
    /// An admin session or an API key.
    Admin,
    /// An admin in the browser; API keys are refused.
    Session,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "validation_failed", message)
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Sign in as an admin first.",
        )
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn internal(error: impl std::fmt::Display) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            error.to_string(),
        )
    }
}

impl From<crate::store::Error> for ApiError {
    fn from(e: crate::store::Error) -> Self {
        Self::internal(e)
    }
}

/// An action result plus what goes next to it in the envelope.
pub struct Out<T> {
    pub data: T,
    pub message: Option<String>,
    pub meta: Map<String, Value>,
    pub cookies: Vec<String>,
}

impl<T> From<T> for Out<T> {
    fn from(data: T) -> Self {
        Self {
            data,
            message: None,
            meta: Map::new(),
            cookies: Vec::new(),
        }
    }
}

impl<T> Out<T> {
    #[must_use]
    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    #[must_use]
    pub fn meta(mut self, key: &str, value: impl Serialize) -> Self {
        self.meta.insert(
            key.into(),
            serde_json::to_value(value).unwrap_or(Value::Null),
        );
        self
    }

    #[must_use]
    pub fn cookie(mut self, cookie: String) -> Self {
        self.cookies.push(cookie);
        self
    }
}

pub struct Ctx {
    pub app: AppState,
    pub principal: Principal,
    pub headers: HeaderMap,
}

impl Ctx {
    /// Store work off the async runtime.
    pub async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&AppState) -> crate::store::Result<T> + Send + 'static,
    ) -> ApiResult<T> {
        let app = self.app.clone();
        tokio::task::spawn_blocking(move || work(&app))
            .await
            .map_err(ApiError::internal)?
            .map_err(ApiError::from)
    }

    pub fn email(&self) -> &str {
        self.principal.email().unwrap_or_default()
    }
}

/// One API action: a flat params struct that runs against a `Ctx`.
pub trait Action: DeserializeOwned + schemars::JsonSchema + Send + 'static {
    /// `group/action`.
    const PATH: &'static str;
    const DOC: &'static str;
    const AUTH: Auth;
    /// False for actions that change nothing; read-only API keys may only call those.
    const WRITE: bool;
    type Output: Serialize + schemars::JsonSchema;

    fn run(self, ctx: &Ctx) -> impl Future<Output = ApiResult<Out<Self::Output>>> + Send;
}

pub enum Params {
    Json(Value),
    Form(String),
}

impl Params {
    fn from_body(headers: &HeaderMap, body: &[u8]) -> ApiResult<Self> {
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Self::Json(json!({})));
        }
        if content_type.starts_with("application/x-www-form-urlencoded") {
            return Ok(Self::Form(String::from_utf8_lossy(body).into_owned()));
        }
        serde_json::from_slice(body)
            .map(Self::Json)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON: {e}")))
    }

    pub fn parse<T: DeserializeOwned>(self) -> ApiResult<T> {
        match self {
            Self::Json(value) => {
                serde_json::from_value(value).map_err(|e| ApiError::invalid(e.to_string()))
            }
            Self::Form(form) => {
                serde_html_form::from_str(&form).map_err(|e| ApiError::invalid(e.to_string()))
            }
        }
    }
}

fn envelope(status: StatusCode, body: &Value, cookies: &[String]) -> Response {
    let mut response = (status, axum::Json(body)).into_response();
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    for c in cookies {
        if let Ok(value) = HeaderValue::from_str(c) {
            headers.append(SET_COOKIE, value);
        }
    }
    response
}

fn meta(path: &str, started: Instant, extra: Map<String, Value>) -> Value {
    let mut meta = Map::new();
    meta.insert("action".into(), json!(path));
    meta.insert(
        "request_id".into(),
        json!(hex::encode(auth::random_bytes::<6>())),
    );
    meta.insert(
        "time_ms".into(),
        json!(started.elapsed().as_millis() as u64),
    );
    meta.extend(extra);
    Value::Object(meta)
}

fn failure(path: &str, started: Instant, error: &ApiError) -> Response {
    if error.status.is_server_error() {
        tracing::error!(action = %path, error = %error.message, "api error");
    }
    let body = json!({
        "success": false,
        "status": error.status.as_u16(),
        "message": error.message,
        "data": null,
        "error": { "code": error.code, "message": error.message },
        "meta": meta(path, started, Map::new()),
    });
    envelope(error.status, &body, &[])
}

/// Checks who may call `entry`.
fn authorize(entry: &registry::Entry, principal: &Principal, headers: &HeaderMap) -> ApiResult<()> {
    if principal.via_session() && !auth::same_origin(headers) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "bad_origin",
            "Cross-site request refused.",
        ));
    }
    match (entry.auth, principal) {
        (Auth::Public, _) => Ok(()),
        (_, Principal::Anonymous) => Err(ApiError::unauthorized()),
        (
            Auth::Session,
            Principal::Admin {
                via: Via::Key { .. },
                ..
            },
        ) => Err(ApiError::forbidden("API keys cannot call this action.")),
        (
            _,
            Principal::Admin {
                via: Via::Key {
                    read_only: true, ..
                },
                ..
            },
        ) if entry.write => Err(ApiError::forbidden("This API key is read-only.")),
        _ => Ok(()),
    }
}

pub async fn dispatch(
    State(app): State<AppState>,
    Path((group, action)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let path = format!("{group}/{action}");
    let Some(entry) = registry::find(&path) else {
        return failure(
            &path,
            started,
            &ApiError::not_found(format!("Unknown action {path}.")),
        );
    };
    let Ok(principal) = auth::resolve(&app, &headers) else {
        return failure(
            &path,
            started,
            &ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "Unknown API key."),
        );
    };
    if let Err(e) = authorize(entry, &principal, &headers) {
        return failure(&path, started, &e);
    }
    let params = match Params::from_body(&headers, &body) {
        Ok(params) => params,
        Err(e) => return failure(&path, started, &e),
    };
    let ctx = Ctx {
        app,
        principal,
        headers,
    };
    match (entry.call)(&ctx, params).await {
        Ok(out) => {
            let body = json!({
                "success": true,
                "status": 200,
                "message": out.message,
                "data": out.data,
                "error": null,
                "meta": meta(&path, started, out.meta),
            });
            envelope(StatusCode::OK, &body, &out.cookies)
        }
        Err(e) => failure(&path, started, &e),
    }
}
