// End-to-end resize behaviour, ported from the Go suite (test/resize_test.go, resize_e2e_test.go).

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{AVIF, dims, env, env_with, image};

#[tokio::test]
async fn formats_follow_accept_and_origin() {
    let env = env().await;
    let cases = [
        ("/r/w100", "/test.jpeg", AVIF, "image/avif"),
        ("/r/w100", "/test.jpeg", "image/webp,image/*", "image/webp"),
        ("/r/w100", "/test.jpeg", "image/*", "image/jpeg"),
        ("/r/c100x80", "/test.jpeg", AVIF, "image/avif"),
        (
            "/r/c100x80",
            "/test.jpeg",
            "image/webp,image/*",
            "image/webp",
        ),
        ("/r/h100", "/test.jpeg", "image/webp,image/*", "image/webp"),
        ("/r/c80", "/test.png", "image/webp,image/*", "image/webp"),
        ("/r/w100", "/test.gif", AVIF, "image/gif"),
        ("/r/w100", "/test.gif", "image/*", "image/gif"),
        ("/r/w100", "/test.svg", AVIF, "image/svg+xml"),
        ("/r/w100", "/Flag.svg.png", "image/*", "image/png"),
        ("/r/w100", "/test.png", "image/*", "image/png"),
        ("/r/w100", "/test.png", AVIF, "image/avif"),
        ("/r/w100", "/test.avif", AVIF, "image/avif"),
        ("/r/w100", "/test.avif", "image/webp,image/*", "image/webp"),
        ("/r/w100", "/test.webp", "image/webp,image/*", "image/webp"),
        ("/r/w100", "/test.webp", "image/*", "image/jpeg"),
    ];
    for (path, file, accept, want) in cases {
        let r = env.resize(path, file, accept).await;
        let what = format!("{path} {file} {accept}: {}", r.header("x-info"));
        assert_eq!(r.status, 200, "{what}");
        assert_eq!(r.header("content-type"), want, "{what}");
        assert!(r.header("cache-control").contains("immutable"), "{what}");
        assert_eq!(
            r.header("content-length"),
            r.body.len().to_string(),
            "{what}"
        );
        assert!(!r.body.is_empty(), "{what}");
        assert!(
            !r.header("x-cache").is_empty() && !r.header("x-info").is_empty(),
            "{what}"
        );
        assert_eq!(r.header("vary"), "Accept", "{what}");
    }
}

#[tokio::test]
async fn resize_geometry_matches_go() {
    let env = env().await;
    let cases = [
        ("/r/w100", (100, 75)),
        ("/r/h100", (133, 100)),
        ("/r/w100x100", (100, 75)),
        ("/r/c100x80", (100, 80)),
        ("/r/c80", (80, 80)),
        ("/r/w300", (200, 150)),
        ("/r/c300x100", (300, 100)),
    ];
    for (path, want) in cases {
        let r = env.resize(path, "/test.jpeg", "image/*").await;
        assert_eq!(dims(&r.body), want, "{path}");
    }
    let big = env.resize("/r/w5000", "/big.jpeg", "image/*").await;
    assert_eq!(dims(&big.body), (1600, 1200), "clamped to MAX_SIZE");
}

#[tokio::test]
async fn cache_keys_are_separate_per_format() {
    let env = env().await;
    let url = format!("{}/test.jpeg", env.origin);
    assert_eq!(
        env.resize("/r/w90", "/test.jpeg", AVIF)
            .await
            .header("content-type"),
        "image/avif"
    );
    env.wait_cached(&url, "w_90_avif").await;
    assert_eq!(
        env.resize("/r/w90", "/test.jpeg", "image/webp,image/*")
            .await
            .header("content-type"),
        "image/webp"
    );
    env.wait_cached(&url, "w_90_webp").await;
    assert_eq!(
        env.resize("/r/w90", "/test.jpeg", "image/*")
            .await
            .header("content-type"),
        "image/jpeg"
    );
    env.wait_cached(&url, "w_90_jpg").await;
}

#[tokio::test]
async fn cache_hit_ignores_no_cache_and_answers_304() {
    let env = env().await;
    let url = format!("{}/test.jpeg", env.origin);
    let miss = env.resize("/r/w80", "/test.jpeg", AVIF).await;
    assert_eq!(miss.header("x-cache"), "MISS");
    let etag = miss.header("etag").to_string();
    assert!(etag.starts_with('"') && etag.len() == 34, "{etag}");
    env.wait_cached(&url, "w_80_avif").await;

    let hit = env
        .get(
            &format!("/r/w80?{url}"),
            &[("accept", AVIF), ("cache-control", "no-cache")],
        )
        .await;
    assert_eq!(hit.status, 200);
    assert_eq!(hit.header("x-cache"), "HIT");
    assert!(hit.header("cache-control").contains("immutable"));
    assert_eq!(hit.header("content-length"), hit.body.len().to_string());
    assert_eq!(hit.body, miss.body);
    assert_eq!(hit.header("etag"), etag);
    assert!(
        hit.header("x-info")
            .starts_with("from-cache; params=w_80; format=avif")
    );

    let not_modified = env
        .get(
            &format!("/r/w80?{url}"),
            &[("accept", AVIF), ("if-none-match", &etag)],
        )
        .await;
    assert_eq!(not_modified.status, 304);
    assert!(not_modified.body.is_empty());
    assert_eq!(not_modified.header("etag"), etag);
    assert_eq!(not_modified.header("vary"), "Accept");
}

#[tokio::test]
async fn hard_errors_answer_the_error_svg() {
    let env = env().await;
    for file in ["/notfound", "/error500", "/empty"] {
        let r = env.resize("/r/w100", file, AVIF).await;
        assert_eq!(r.status, 200, "{file}");
        assert_eq!(r.header("content-type"), "image/svg+xml");
        assert!(
            r.header("cache-control").contains("no-cache")
                && r.header("cache-control").contains("max-age=60")
        );
        assert_eq!(r.header("content-length"), r.body.len().to_string());
        assert!(
            r.text().contains("#fff8f8") && !r.text().contains("animateTransform"),
            "{file}"
        );
        assert!(r.header("x-info").starts_with("error; "));
    }
    let dns = env
        .get("/r/w100?http://nonexistent.test.invalid/image.jpg", &[])
        .await;
    assert!(dns.text().contains("#fff8f8"));
    let retry = env.resize("/r/w100", "/toomany", AVIF).await;
    assert!(
        retry.text().contains("animateTransform"),
        "429 shows the spinner"
    );
}

#[tokio::test]
async fn bad_requests() {
    let env = env().await;
    let r = env.get("/resize", &[]).await;
    assert_eq!(
        (r.status.as_u16(), r.text().as_str()),
        (400, "Missing src parameter\n")
    );
    let r = env
        .get("/r/c100x200x300?http://example.com/img.jpg", &[])
        .await;
    assert_eq!(r.status, 400);
    assert!(r.text().starts_with("Invalid parameters"));
    assert_eq!(env.get("/r/w32&f=bmp?x.com/a.png", &[]).await.status, 400);
    assert_eq!(env.get("/unknown", &[]).await.status, 404);
}

#[tokio::test]
async fn slow_source_answers_the_spinner_and_finishes_in_background() {
    let env = env_with(|c| c.wait_timeout = Duration::from_millis(500)).await;
    let r = env.resize("/r/w100", "/slow.jpeg", AVIF).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), "image/svg+xml");
    assert_eq!(r.header("x-cache"), "QUEUED");
    assert_eq!(r.header("retry-after"), "10");
    assert!(r.header("cache-control").contains("max-age=10"));
    assert!(r.text().contains("animateTransform"));

    env.wait_cached(&format!("{}/slow.jpeg", env.origin), "w_100_avif")
        .await;
    let again = env.resize("/r/w100", "/slow.jpeg", AVIF).await;
    assert_eq!(again.header("x-cache"), "HIT");
    assert_eq!(again.header("content-type"), "image/avif");
}

#[tokio::test]
async fn same_request_is_coalesced() {
    let env = env().await;
    let futures = (0..5).map(|_| env.resize("/r/w42", "/count/coalesce.jpeg", AVIF));
    for r in futures_util::future::join_all(futures).await {
        assert_eq!(r.status, 200);
        assert_eq!(r.header("content-type"), "image/avif");
    }
    assert_eq!(env.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn different_sizes_share_one_source_fetch() {
    let env = env().await;
    let futures = ["/r/w61", "/r/w62", "/r/w63", "/r/w64", "/r/w65"]
        .into_iter()
        .map(|path| env.resize(path, "/count/source.jpeg", AVIF));
    for r in futures_util::future::join_all(futures).await {
        assert_eq!(r.status, 200, "{}", r.header("x-info"));
    }
    assert_eq!(env.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn source_is_cached_and_reused() {
    let env = env().await;
    let url = format!("{}/count/reuse.jpeg", env.origin);
    assert_eq!(
        env.resize("/r/w71", "/count/reuse.jpeg", AVIF).await.status,
        200
    );
    let source = env.wait_cached(&url, "source").await;
    assert_eq!(
        (source.kind.as_str(), source.origin_format.as_str()),
        ("source", "jpeg")
    );
    assert_eq!(
        source.format, "jpeg",
        "a small upright JPEG is stored as downloaded"
    );
    env.wait_cached(&url, "w_71_avif").await;

    assert_eq!(
        env.resize("/r/w72", "/count/reuse.jpeg", AVIF).await.status,
        200
    );
    assert_eq!(env.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn large_sources_are_scaled_down_before_caching() {
    let env = env().await;
    let url = format!("{}/big.jpeg", env.origin);
    env.resize("/r/w100", "/big.jpeg", AVIF).await;
    let source = env.wait_cached(&url, "source").await;
    assert_eq!(source.format, "webp");
    let bytes = env.app.store.read_bytes(&source).unwrap().unwrap();
    assert_eq!(dims(&bytes), (1600, 1200));
}

#[tokio::test]
async fn forced_formats() {
    let env = env().await;
    let r = env.resize("/r/w32.png", "/test.png", AVIF).await;
    assert_eq!(
        (r.status.as_u16(), r.header("content-type")),
        (200, "image/png")
    );
    assert_eq!(r.header("vary"), "", "forced format must not set Vary");
    assert_eq!(dims(&r.body), (32, 24));

    let r = env.resize("/r.png", "/test.png", AVIF).await;
    assert_eq!(r.header("content-type"), "image/png");
    assert_eq!(dims(&r.body), (200, 150));

    assert_eq!(
        env.resize("/r/w32&f=png", "/test.png", AVIF)
            .await
            .header("content-type"),
        "image/png"
    );
    assert_eq!(
        env.resize("/r/w32.jpeg", "/test.png", AVIF)
            .await
            .header("content-type"),
        "image/jpeg"
    );
    assert_eq!(
        env.resize("/r/w32.webp", "/test.png", "image/*")
            .await
            .header("content-type"),
        "image/webp"
    );
    assert_eq!(
        env.resize("/r/w32.gif", "/test.png", "image/*")
            .await
            .header("content-type"),
        "image/gif"
    );
    assert_eq!(
        env.resize("/r.avif", "/test.png", "image/*")
            .await
            .header("content-type"),
        "image/avif"
    );
    env.wait_cached(&format!("{}/test.png", env.origin), "w_32_jpg")
        .await;

    let svg = env.resize("/r/w50.png", "/test.svg", AVIF).await;
    assert_eq!(
        svg.header("content-type"),
        "image/png",
        "forced format rasterizes SVG"
    );
    assert_eq!(dims(&svg.body), (50, 38));

    let legacy = env
        .get(
            &format!("/resize?src={}/test.png&w=20&f=png", env.origin),
            &[],
        )
        .await;
    assert_eq!(legacy.header("content-type"), "image/png");
    assert_eq!(dims(&legacy.body), (20, 15));
}

#[tokio::test]
async fn svg_passthrough_is_not_cached_as_a_variant() {
    let env = env().await;
    let url = format!("{}/test.svg", env.origin);
    let r = env.resize("/r/w100", "/test.svg", AVIF).await;
    assert!(r.text().starts_with("<svg"));
    assert_eq!(
        r.header("x-info"),
        "source-cache; format=svg; no-manipulation"
    );
    assert!(
        r.header("content-security-policy").contains("sandbox"),
        "remote SVG must not run script on our origin"
    );
    assert_eq!(r.header("x-content-type-options"), "nosniff");
    env.wait_cached(&url, "source").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(env.app.store.get(&url, "w_100_avif").unwrap().is_none());
}

#[tokio::test]
async fn exif_rotation_is_applied() {
    let env = env().await;
    // 200x100 landscape pixels tagged "rotate 90": upright it is 100x200
    let jpg = image("jpeg", 200, 100);
    let mut img = rs_vips::VipsImage::new_from_buffer(&jpg, "")
        .unwrap()
        .copy_memory()
        .unwrap();
    img.set_int("orientation", 6).unwrap();
    let tagged = img.jpegsave_buffer().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("rot.jpg"), &tagged).unwrap();
    let router =
        axum::Router::new().fallback_service(tower_http::services::ServeDir::new(dir.path()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let r = env
        .get(
            &format!("/r/w50?http://{addr}/rot.jpg"),
            &[("accept", "image/*")],
        )
        .await;
    assert_eq!(
        r.header("content-type"),
        "image/jpeg",
        "{} {}",
        r.status,
        r.header("x-info")
    );
    assert_eq!(dims(&r.body), (50, 100));
}

#[tokio::test]
async fn ssrf_guard_blocks_private_sources() {
    let env = env_with(|c| c.allow_private_sources = false).await;
    for src in [
        "http://127.0.0.1:9/x.jpg",
        "http://localhost/x.jpg",
        "http://[::1]/x.jpg",
        "http://10.1.2.3/a.png",
    ] {
        let r = env.get(&format!("/r/w100?{src}"), &[]).await;
        assert_eq!(
            (r.status.as_u16(), r.text().as_str()),
            (403, "Source URL not allowed\n"),
            "{src}"
        );
    }
    let dns = env.get("/r/w100?http://localhost./x.jpg", &[]).await;
    assert!(
        dns.header("x-info").contains("blocked-address"),
        "{}",
        dns.header("x-info")
    );
}

#[tokio::test]
async fn download_size_is_capped() {
    let env = env_with(|c| c.max_source_bytes = 1000).await;
    let r = env.resize("/r/w100", "/test.jpeg", AVIF).await;
    assert!(
        r.header("x-info").contains("too large"),
        "{}",
        r.header("x-info")
    );
}

#[tokio::test]
async fn allowed_domains_and_disabled_referers() {
    let env = env_with(|c| c.allowed_domains = vec!["*.example.com".into()]).await;
    let r = env.resize("/r/w100", "/test.jpeg", AVIF).await;
    assert_eq!(
        (r.status.as_u16(), r.text().as_str()),
        (403, "Domain '127.0.0.1' is not allowed\n")
    );

    let env = common::env().await;
    env.app.referers.toggle(&env.app.store, "bad.com").unwrap();
    let r = env
        .get(
            &format!("/r/w100?{}/test.jpeg", env.origin),
            &[("referer", "https://www.bad.com/page")],
        )
        .await;
    assert_eq!(r.status, 403);
    assert_eq!(
        r.text(),
        "Domain 'bad.com' is forbidden from using this resize service\n"
    );
    let ok = env
        .get(
            &format!("/r/w100?{}/test.jpeg", env.origin),
            &[("referer", "https://good.com/")],
        )
        .await;
    assert_eq!(ok.status, 200);
}

#[tokio::test]
async fn eviction_keeps_the_cache_under_its_limit() {
    let env = env_with(|c| c.max_cache_bytes = 1).await;
    env.resize("/r/w100", "/test.jpeg", AVIF).await;
    env.wait_cached(&format!("{}/test.jpeg", env.origin), "w_100_avif")
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let app = env.app.clone();
    tokio::task::spawn_blocking(move || image_resizer::housekeeping::cleanup(&app))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(env.app.store.count().unwrap(), 0);
    assert!(env.app.store.files_on_disk().unwrap().is_empty());
}

#[tokio::test]
async fn favicon_and_static() {
    let env = env().await;
    let r = env.get("/favicon.ico", &[]).await;
    assert_eq!(r.header("content-type"), "image/svg+xml");
    assert!(r.header("cache-control").contains("immutable"));
    assert!(r.text().contains("#2563eb"));
    assert_eq!(env.get("/static/test.svg", &[]).await.status, 200);
}
