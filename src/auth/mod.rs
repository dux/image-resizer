// Who is calling: an admin through the AuthCog session cookie or a Bearer API key, or nobody.

pub mod authcog;
pub mod keys;
pub mod session;

use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, COOKIE, HOST, ORIGIN};

use crate::app::AppState;
use crate::config::Config;
use session::{SESSION_COOKIE, SESSION_DAYS};

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    rand::fill(&mut bytes);
    bytes
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Via {
    Session,
    Key { id: i64, read_only: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    Anonymous,
    Admin { email: String, via: Via },
}

impl Principal {
    pub fn email(&self) -> Option<&str> {
        match self {
            Self::Admin { email, .. } => Some(email),
            Self::Anonymous => None,
        }
    }

    pub fn via_session(&self) -> bool {
        matches!(
            self,
            Self::Admin {
                via: Via::Session,
                ..
            }
        )
    }
}

/// A Bearer header that names no key.
#[derive(Debug)]
pub struct UnknownKey;

/// A Bearer header must be a valid key (no cookie fallback); otherwise the session cookie decides,
/// and a stale cookie or a former admin is just anonymous.
pub fn resolve(app: &AppState, headers: &HeaderMap) -> Result<Principal, UnknownKey> {
    if let Some(token) = bearer(headers) {
        let key = app.keys.find(&token).ok_or(UnknownKey)?;
        return Ok(Principal::Admin {
            email: format!("key #{}", key.id),
            via: Via::Key {
                id: key.id,
                read_only: key.read_only,
            },
        });
    }
    let Some(value) = cookie(headers, SESSION_COOKIE) else {
        return Ok(Principal::Anonymous);
    };
    match app.session.verify(&value, crate::store::now()) {
        Some(email) if app.config.is_admin(&email) => Ok(Principal::Admin {
            email,
            via: Via::Session,
        }),
        _ => Ok(Principal::Anonymous),
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_string())
}

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

/// `Set-Cookie` value; `max_age` 0 removes the cookie.
pub fn set_cookie(name: &str, value: &str, path: &str, max_age: i64, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{name}={value}; Path={path}; Max-Age={max_age}; HttpOnly; SameSite=Lax{secure}")
}

pub fn session_cookie(app: &AppState, email: &str, secure: bool) -> String {
    let max_age = SESSION_DAYS * 86_400;
    let value = app.session.sign(email, crate::store::now() + max_age);
    set_cookie(SESSION_COOKIE, &value, "/", max_age, secure)
}

pub fn clear_session_cookie(secure: bool) -> String {
    set_cookie(SESSION_COOKIE, "", "/", 0, secure)
}

/// Scheme, host and port the browser used, from the proxy headers when present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    pub https: bool,
    pub host: String,
    pub port: u16,
}

impl Origin {
    pub fn default_port(&self) -> bool {
        self.port == if self.https { 443 } else { 80 }
    }

    /// `host[:port]` as browsers write it in `Origin`.
    pub fn authority(&self) -> String {
        if self.default_port() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn first(headers: &HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?;
    Some(value.split(',').next()?.trim().to_lowercase()).filter(|v| !v.is_empty())
}

/// The request's public origin, `None` when its host is not one of ours.
pub fn request_origin(config: &Config, headers: &HeaderMap) -> Option<Origin> {
    let authority = first(headers, "x-forwarded-host").or_else(|| first(headers, HOST.as_str()))?;
    let https = first(headers, "x-forwarded-proto").is_some_and(|p| p == "https");
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            (host.to_string(), port.parse().ok()?)
        }
        _ => (authority.clone(), if https { 443 } else { 80 }),
    };
    config
        .own_host(&host)
        .then_some(Origin { https, host, port })
}

/// CSRF guard for cookie-authenticated calls: the browser's `Origin` must name this host.
pub fn same_origin(headers: &HeaderMap) -> bool {
    let host = first(headers, "x-forwarded-host").or_else(|| first(headers, HOST.as_str()));
    let origin = headers.get(ORIGIN).and_then(|v| v.to_str().ok());
    match (origin, host) {
        (Some(origin), Some(host)) => url::Url::parse(origin).is_ok_and(|u| {
            let authority = match (u.host_str(), u.port()) {
                (Some(h), Some(p)) => format!("{h}:{p}"),
                (Some(h), None) => h.to_string(),
                _ => return false,
            };
            authority == host
        }),
        // fetch() always sends Origin on POST; browsers without it still send Sec-Fetch-Site
        (None, _) => headers
            .get("sec-fetch-site")
            .is_some_and(|v| v == "same-origin"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn cookies() {
        let h = headers(&[("cookie", "a=1; ir_session=abc.def"), ("cookie", "b=2")]);
        assert_eq!(cookie(&h, "ir_session").as_deref(), Some("abc.def"));
        assert_eq!(cookie(&h, "b").as_deref(), Some("2"));
        assert_eq!(cookie(&h, "c"), None);
        assert_eq!(
            set_cookie("x", "v", "/", 5, true),
            "x=v; Path=/; Max-Age=5; HttpOnly; SameSite=Lax; Secure"
        );
    }

    #[test]
    fn origins() {
        let mut config = Config::test("tmp");
        let o = request_origin(&config, &headers(&[("host", "lvh.me:3000")])).unwrap();
        assert_eq!(
            o,
            Origin {
                https: false,
                host: "lvh.me".into(),
                port: 3000
            }
        );
        let o = request_origin(
            &config,
            &headers(&[("host", "img.lvh.me"), ("x-forwarded-proto", "https")]),
        )
        .unwrap();
        assert_eq!((o.port, o.authority().as_str()), (443, "img.lvh.me"));
        assert!(request_origin(&config, &headers(&[("host", "evil.com")])).is_none());
        config.env = crate::config::AppEnv::Production;
        config.app_hosts = vec!["img.sohospot.com".into()];
        assert!(request_origin(&config, &headers(&[("host", "lvh.me")])).is_none());
        assert!(request_origin(&config, &headers(&[("host", "img.sohospot.com")])).is_some());
    }

    #[test]
    fn same_origin_check() {
        assert!(same_origin(&headers(&[
            ("host", "lvh.me:3000"),
            ("origin", "http://lvh.me:3000")
        ])));
        assert!(same_origin(&headers(&[
            ("host", "img.x.com"),
            ("origin", "https://img.x.com")
        ])));
        assert!(!same_origin(&headers(&[
            ("host", "img.x.com"),
            ("origin", "https://evil.com")
        ])));
        assert!(!same_origin(&headers(&[("host", "img.x.com")])));
        assert!(same_origin(&headers(&[
            ("host", "img.x.com"),
            ("sec-fetch-site", "same-origin")
        ])));
        assert!(!same_origin(&headers(&[
            ("host", "img.x.com"),
            ("origin", "null")
        ])));
    }
}
