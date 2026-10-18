// AuthCog sign-in for admins, ported from dboss internal/authcog and MailCog src/auth:
// GET /login -> AuthCog -> GET /authcog?callback=..&state=.. -> profile exchange -> session cookie.

use std::fmt::Write;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::header::{LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use subtle::ConstantTimeEq;

use super::{Origin, clear_session_cookie, cookie, request_origin, session_cookie, set_cookie};
use crate::app::AppState;
use crate::config::AppEnv;
use crate::resize::handler::text;

const STATE_COOKIE: &str = "ir_login_state";
const CALLBACK_PATH: &str = "/authcog";
const STATE_TTL: Duration = Duration::from_secs(300);
const MAX_RESPONSE_BYTES: usize = 1 << 20;

#[derive(Clone)]
pub struct PendingLogin {
    destination: String,
    redirect_to: String,
}

pub type LoginStates = moka::sync::Cache<String, PendingLogin>;

pub fn login_states() -> LoginStates {
    moka::sync::Cache::builder()
        .max_capacity(4096)
        .time_to_live(STATE_TTL)
        .build()
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .expect("http client")
}

/// `/d:<host>[/p:<port>][/s:http]`: where `AuthCog` sends the browser back to.
pub fn destination(origin: &Origin) -> String {
    let mut path = format!("/d:{}", origin.host);
    if !origin.default_port() {
        let _ = write!(path, "/p:{}", origin.port);
    }
    if !origin.https {
        path.push_str("/s:http");
    }
    path
}

/// Keeps only same-origin relative paths; anything else becomes `/`.
pub fn safe_redirect(target: Option<&str>) -> String {
    match target {
        Some(t)
            if t.starts_with('/')
                && !t.starts_with("//")
                && !t.contains('\\')
                && !t.starts_with(CALLBACK_PATH) =>
        {
            t.into()
        }
        _ => "/".into(),
    }
}

fn redirect(status: StatusCode, location: &str, cookies: &[String]) -> Response {
    let mut response = status.into_response();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(location) {
        headers.insert(LOCATION, value);
    }
    for c in cookies {
        if let Ok(value) = HeaderValue::from_str(c) {
            headers.append(SET_COOKIE, value);
        }
    }
    response
}

#[derive(Deserialize)]
pub struct LoginQuery {
    redirect_to: Option<String>,
}

pub async fn start(
    State(app): State<AppState>,
    Query(query): Query<LoginQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(origin) = request_origin(&app.config, &headers) else {
        return text(
            StatusCode::BAD_REQUEST,
            "invalid authentication destination",
        );
    };
    let state = URL_SAFE_NO_PAD.encode(super::random_bytes::<32>());
    let destination = destination(&origin);
    let pending = PendingLogin {
        destination: destination.clone(),
        redirect_to: safe_redirect(query.redirect_to.as_deref()),
    };
    app.logins.insert(state.clone(), pending);
    let location = format!("{}{destination}?state={state}", app.config.authcog_url);
    let state_cookie = set_cookie(
        STATE_COOKIE,
        &state,
        CALLBACK_PATH,
        STATE_TTL.as_secs() as i64,
        origin.https,
    );
    redirect(StatusCode::FOUND, &location, &[state_cookie])
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    callback: Option<String>,
    state: Option<String>,
}

#[derive(Deserialize)]
struct Profile {
    email: String,
}

enum ExchangeError {
    Expired,
    Unavailable(String),
}

pub async fn callback(
    State(app): State<AppState>,
    Query(query): Query<CallbackQuery>,
    headers: HeaderMap,
) -> Response {
    let origin = request_origin(&app.config, &headers);
    let https = origin.as_ref().is_some_and(|o| o.https);
    let clear_state = set_cookie(STATE_COOKIE, "", CALLBACK_PATH, 0, https);
    let fail = |reason: &str| {
        redirect(
            StatusCode::SEE_OTHER,
            &format!("/?login_error={reason}"),
            std::slice::from_ref(&clear_state),
        )
    };

    let (Some(callback), Some(state), Some(cookie_state), Some(origin)) = (
        query.callback,
        query.state,
        cookie(&headers, STATE_COOKIE),
        origin,
    ) else {
        return fail("expired");
    };
    if !bool::from(state.as_bytes().ct_eq(cookie_state.as_bytes())) {
        return fail("expired");
    }
    let Some(pending) = app.logins.remove(&state) else {
        return fail("expired");
    };
    if destination(&origin) != pending.destination {
        return fail("expired");
    }

    let email = match exchange(&app, &pending.destination, &callback).await {
        Ok(email) => email,
        Err(ExchangeError::Expired) => return fail("expired"),
        Err(ExchangeError::Unavailable(error)) => {
            tracing::warn!(%error, "AuthCog exchange failed");
            return fail("unavailable");
        }
    };
    if !app.config.is_admin(&email) {
        tracing::info!(%email, "sign-in refused: not an admin");
        return fail("no_access");
    }
    tracing::info!(%email, "admin signed in");
    redirect(
        StatusCode::SEE_OTHER,
        &pending.redirect_to,
        &[session_cookie(&app, &email, https), clear_state],
    )
}

/// Trades the one-time callback for the profile email (lowercased).
async fn exchange(
    app: &AppState,
    destination: &str,
    callback: &str,
) -> Result<String, ExchangeError> {
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user", callback)
        .finish();
    let url = format!("{}{destination}?{query}", app.config.authcog_url);
    let mut response = app
        .auth_http
        .get(&url)
        .send()
        .await
        .map_err(|e| ExchangeError::Unavailable(e.to_string()))?;
    match response.status().as_u16() {
        200 => {}
        400 | 404 | 410 => return Err(ExchangeError::Expired),
        status => {
            return Err(ExchangeError::Unavailable(format!(
                "AuthCog returned {status}"
            )));
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ExchangeError::Unavailable(e.to_string()))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(ExchangeError::Unavailable(
                "AuthCog response too large".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let profile: Profile = serde_json::from_slice(&body)
        .map_err(|e| ExchangeError::Unavailable(format!("bad AuthCog JSON: {e}")))?;
    let email = profile.email.trim().to_lowercase();
    if email.is_empty() {
        return Err(ExchangeError::Unavailable(
            "AuthCog response has no email".into(),
        ));
    }
    Ok(email)
}

#[derive(Deserialize)]
pub struct DevQuery {
    email: String,
    redirect_to: Option<String>,
}

/// Outside production: sign in as an admin email without `AuthCog`. Not routed in production.
pub async fn dev(
    State(app): State<AppState>,
    Query(query): Query<DevQuery>,
    headers: HeaderMap,
) -> Response {
    if app.config.env == AppEnv::Production {
        return text(StatusCode::NOT_FOUND, "404 page not found");
    }
    let email = query.email.trim().to_lowercase();
    if !app.config.is_admin(&email) {
        return text(StatusCode::FORBIDDEN, "not an admin (ADMIN_EMAILS)");
    }
    let https = request_origin(&app.config, &headers).is_some_and(|o| o.https);
    tracing::info!(%email, "dev sign-in");
    redirect(
        StatusCode::SEE_OTHER,
        &safe_redirect(query.redirect_to.as_deref()),
        &[session_cookie(&app, &email, https)],
    )
}

pub fn logout_cookie(app: &AppState, headers: &HeaderMap) -> String {
    clear_session_cookie(request_origin(&app.config, headers).is_some_and(|o| o.https))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destinations() {
        let o = |https, host: &str, port| Origin {
            https,
            host: host.into(),
            port,
        };
        assert_eq!(
            destination(&o(false, "lvh.me", 3000)),
            "/d:lvh.me/p:3000/s:http"
        );
        assert_eq!(
            destination(&o(true, "img.sohospot.com", 443)),
            "/d:img.sohospot.com"
        );
        assert_eq!(
            destination(&o(true, "img.x.com", 8443)),
            "/d:img.x.com/p:8443"
        );
        assert_eq!(destination(&o(false, "lvh.me", 80)), "/d:lvh.me/s:http");
    }

    #[test]
    fn redirects_stay_on_the_app() {
        assert_eq!(safe_redirect(Some("https://evil.com")), "/");
        assert_eq!(safe_redirect(Some("//evil.com")), "/");
        assert_eq!(safe_redirect(Some("javascript:alert(1)")), "/");
        assert_eq!(safe_redirect(Some("/\\evil.com")), "/");
        assert_eq!(safe_redirect(Some("/authcog?callback=x")), "/");
        assert_eq!(safe_redirect(None), "/");
        assert_eq!(safe_redirect(Some("/#/cache?page=2")), "/#/cache?page=2");
    }
}
