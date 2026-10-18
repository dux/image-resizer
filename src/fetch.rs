// Source downloads: browser-like headers, size cap, and the SSRF guard (DNS answers and every redirect
// hop are checked against private ranges, IP literals included).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT,
};

use crate::jobs::JobError;

const MAX_REDIRECTS: usize = 5;

pub fn blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return blocked_v4(v4);
            }
            let seg = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (seg[0] & 0xffc0) == 0xfe80 // link local fe80::/10
                || (seg[0] == 0x64 && seg[1] == 0xff9b) // NAT64 can reach private v4
                || (seg[0] == 0x2002) // 6to4 embeds any v4
        }
    }
}

fn blocked_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // CGNAT
        || (a == 192 && b == 0) // 192.0.0.0/24 and 192.0.2.0/24
        || (a == 198 && (18..20).contains(&b)) // benchmarking
        || a >= 240
}

/// Hosts refused before any lookup: `localhost` names and private IP literals.
pub fn blocked_host(host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_lowercase();
    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(blocked_ip)
}

/// Resolves through the system resolver and keeps only public addresses.
struct GuardResolver;

impl Resolve for GuardResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> =
                addrs.into_iter().filter(|a| !blocked_ip(a.ip())).collect();
            if public.is_empty() {
                return Err(
                    format!("blocked-address; {host} resolves only to private addresses").into(),
                );
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

#[derive(Clone)]
pub struct Fetcher {
    client: reqwest::Client,
    max_bytes: u64,
}

impl Fetcher {
    pub fn new(guard: bool, max_bytes: u64) -> Self {
        let mut headers = HeaderMap::new();
        let set = |headers: &mut HeaderMap, name, value: &'static str| {
            headers.insert(name, HeaderValue::from_static(value));
        };
        set(
            &mut headers,
            USER_AGENT,
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:132.0) Gecko/20100101 Firefox/132.0",
        );
        set(
            &mut headers,
            ACCEPT,
            "image/avif,image/webp,image/png,image/jpeg,image/svg+xml,image/*;q=0.8,*/*;q=0.5",
        );
        set(&mut headers, ACCEPT_LANGUAGE, "en-US,en;q=0.5");
        headers.insert("dnt", HeaderValue::from_static("1"));
        headers.insert("upgrade-insecure-requests", HeaderValue::from_static("1"));

        let redirect = reqwest::redirect::Policy::custom(move |attempt| {
            let url = attempt.url();
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if !matches!(url.scheme(), "http" | "https") {
                attempt.error("redirect to a non-http URL")
            } else if guard && url.host_str().is_none_or(blocked_host) {
                attempt.error("blocked-address; redirect to a private host")
            } else {
                attempt.follow()
            }
        });
        let mut builder = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(redirect)
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90));
        if guard {
            builder = builder.dns_resolver(Arc::new(GuardResolver));
        }
        Self {
            client: builder.build().expect("http client"),
            max_bytes,
        }
    }

    /// Body bytes and Content-Type of a 200 answer.
    pub async fn download(&self, url: &str) -> Result<(Bytes, String), JobError> {
        let fail = |e: reqwest::Error| {
            let text = error_chain(&e);
            if e.is_timeout() {
                JobError::retry(format!("fetch-failed; timeout; {text}"))
            } else {
                JobError::new(format!("fetch-failed; {text}"))
            }
        };
        let response = self.client.get(url).send().await.map_err(fail)?;
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            return Err(JobError::new(format!(
                "fetch-failed; status={}",
                status.as_u16()
            )));
        }
        let declared = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if declared.is_some_and(|len| len > self.max_bytes) {
            return Err(JobError::new("fetch-failed; source too large"));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();

        let mut body = BytesMut::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                if e.is_timeout() {
                    JobError::retry(format!("read-failed; timeout; {}", error_chain(&e)))
                } else {
                    JobError::new(format!("read-failed; {}", error_chain(&e)))
                }
            })?;
            if (body.len() + chunk.len()) as u64 > self.max_bytes {
                return Err(JobError::new("fetch-failed; source too large"));
            }
            body.extend_from_slice(&chunk);
        }
        if body.is_empty() {
            return Err(JobError::new("empty-data"));
        }
        Ok((body.freeze(), content_type))
    }

    /// HEAD request for the demo's random image picker.
    pub async fn reachable(&self, url: &str) -> bool {
        self.client
            .head(url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }
}

/// reqwest hides the useful part (DNS refusal, connect error) in the source chain.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        let part = inner.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        source = inner.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges() {
        for ip in [
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "172.31.255.255",
            "127.0.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
        ] {
            assert!(blocked_ip(ip.parse().unwrap()), "{ip} must be blocked");
        }
        for ip in [
            "8.8.8.8",
            "172.32.0.1",
            "1.1.1.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(!blocked_ip(ip.parse().unwrap()), "{ip} must pass");
        }
    }

    #[test]
    fn private_hosts() {
        assert!(blocked_host("localhost"));
        assert!(blocked_host("app.localhost"));
        assert!(blocked_host("[::1]"));
        assert!(blocked_host("127.0.0.1"));
        assert!(blocked_host(""));
        assert!(!blocked_host("example.com"));
        assert!(!blocked_host("8.8.8.8"));
    }

    #[tokio::test]
    async fn guard_refuses_private_names() {
        let fetcher = Fetcher::new(true, 1024);
        let err = fetcher
            .download("http://localhost.:9/x.png")
            .await
            .unwrap_err();
        assert!(err.message.contains("blocked-address"), "{}", err.message);
    }
}
