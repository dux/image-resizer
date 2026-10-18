// API envelope, admin gating, API keys, CSRF and the AuthCog flow against a local mock realm.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use axum::response::IntoResponse;
use common::{AVIF, Env, env, env_with};
use parking_lot::Mutex;
use serde_json::{Value, json};

const HOST: &str = "lvh.me:3000";
const ORIGIN: &str = "http://lvh.me:3000";

async fn call(
    env: &Env,
    path: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> (u16, Value, Vec<String>) {
    let mut request = Request::post(format!("/api/{path}"))
        .header("host", HOST)
        .header("content-type", "application/json");
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    let r = env
        .send(request.body(Body::from(body.to_string())).unwrap())
        .await;
    let cookies = r
        .headers
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    (
        r.status.as_u16(),
        serde_json::from_slice(&r.body).unwrap(),
        cookies,
    )
}

/// Signs in through /dev/login and returns the Cookie header value.
async fn sign_in(env: &Env) -> String {
    let r = env
        .get(
            "/dev/login?email=rejotl@gmail.com&redirect_to=/%23/config",
            &[("host", HOST)],
        )
        .await;
    assert_eq!(r.status, 303);
    assert_eq!(r.header("location"), "/#/config");
    let set = r.header("set-cookie");
    assert!(
        set.contains("HttpOnly") && set.contains("SameSite=Lax"),
        "{set}"
    );
    set.split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn envelope_and_public_actions() {
    let env = env().await;
    let (status, body, _) = call(&env, "sys/health", json!({}), &[]).await;
    assert_eq!(status, 200);
    assert_eq!(body["success"], true);
    assert_eq!(body["data"]["status"], "ok");
    assert_eq!(body["meta"]["action"], "sys/health");
    assert!(body["meta"]["request_id"].is_string());

    let (_, me, _) = call(&env, "auth/me", json!({}), &[]).await;
    assert_eq!(me["data"]["is_admin"], false);
    assert_eq!(me["data"]["email"], Value::Null);

    let (status, body, _) = call(&env, "nope/nope", json!({}), &[]).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (404, Some("not_found"))
    );

    let (_, schema, _) = call(&env, "sys/schema", json!({}), &[]).await;
    let list = schema["data"].as_array().unwrap();
    assert!(
        list.iter()
            .any(|a| a["path"] == "cache/list" && a["auth"] == "admin" && a["write"] == false)
    );
}

#[tokio::test]
async fn admin_actions_need_a_session() {
    let env = env().await;
    let (status, body, _) = call(&env, "stats/show", json!({}), &[]).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (401, Some("unauthorized"))
    );

    let cookie = sign_in(&env).await;
    let (status, body, _) = call(
        &env,
        "stats/show",
        json!({}),
        &[("cookie", &cookie), ("origin", ORIGIN)],
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["settings"]["quality"], 90);
    let (_, me, _) = call(
        &env,
        "auth/me",
        json!({}),
        &[("cookie", &cookie), ("origin", ORIGIN)],
    )
    .await;
    assert_eq!(me["data"]["email"], "rejotl@gmail.com");
    assert_eq!(me["data"]["via"], "session");

    // a cookie riding a cross-site request is refused
    let (status, body, _) = call(
        &env,
        "stats/show",
        json!({}),
        &[("cookie", &cookie), ("origin", "https://evil.com")],
    )
    .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (403, Some("bad_origin"))
    );
    let (status, _, _) = call(&env, "stats/show", json!({}), &[("cookie", &cookie)]).await;
    assert_eq!(status, 403);

    // only admins get a dev session
    let r = env
        .get("/dev/login?email=someone@else.com", &[("host", HOST)])
        .await;
    assert_eq!(r.status, 403);

    let (status, _, cookies) = call(
        &env,
        "auth/logout",
        json!({}),
        &[("cookie", &cookie), ("origin", ORIGIN)],
    )
    .await;
    assert_eq!(status, 200);
    assert!(cookies[0].starts_with("ir_session=;") && cookies[0].contains("Max-Age=0"));
}

#[tokio::test]
async fn former_admins_lose_access() {
    let env = env().await;
    let cookie = sign_in(&env).await;
    let mut config = env.app.config.clone();
    config.admin_emails = vec!["other@x.com".into()];
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        env.app.config.data_dir.join("session.key"),
        dir.path().join("session.key"),
    )
    .unwrap();
    config.data_dir = dir.path().into();
    let other = Env {
        app: image_resizer::app::App::new(config).unwrap(),
        origin: env.origin.clone(),
        hits: env.hits.clone(),
        _dir: dir,
    };
    let (status, _, _) = call(
        &other,
        "stats/show",
        json!({}),
        &[("cookie", &cookie), ("origin", ORIGIN)],
    )
    .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn api_keys() {
    let env = env().await;
    let cookie = sign_in(&env).await;
    let session = [("cookie", cookie.as_str()), ("origin", ORIGIN)];
    let (status, body, _) = call(
        &env,
        "key/create",
        json!({"name": "ci", "read_only": true}),
        &session,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let read_only = format!("Bearer {}", body["data"]["token"].as_str().unwrap());
    let (_, body, _) = call(&env, "key/create", json!({"name": "cms"}), &session).await;
    let writer = format!("Bearer {}", body["data"]["token"].as_str().unwrap());
    let writer_id = body["data"]["key"]["id"].as_i64().unwrap();

    let (status, _, _) = call(
        &env,
        "cache/list",
        json!({}),
        &[("authorization", &read_only)],
    )
    .await;
    assert_eq!(status, 200);
    let (status, body, _) = call(
        &env,
        "cache/clear",
        json!({}),
        &[("authorization", &read_only)],
    )
    .await;
    assert_eq!(
        (status, body["error"]["message"].as_str()),
        (403, Some("This API key is read-only."))
    );
    let (status, _, _) = call(
        &env,
        "cache/clear",
        json!({"period": "all"}),
        &[("authorization", &writer)],
    )
    .await;
    assert_eq!(status, 200);
    let (status, _, _) = call(&env, "key/list", json!({}), &[("authorization", &writer)]).await;
    assert_eq!(status, 403, "keys cannot manage keys");
    let (status, _, _) = call(
        &env,
        "cache/list",
        json!({}),
        &[("authorization", "Bearer irk_nope")],
    )
    .await;
    assert_eq!(status, 401);

    let (_, list, _) = call(&env, "key/list", json!({}), &session).await;
    assert_eq!(list["data"].as_array().unwrap().len(), 2);
    assert!(list["data"][0].get("token").is_none());
    call(&env, "key/delete", json!({"id": writer_id}), &session).await;
    let (status, _, _) = call(&env, "cache/list", json!({}), &[("authorization", &writer)]).await;
    assert_eq!(status, 401, "deleted key stops at once");
}

#[tokio::test]
async fn cache_actions() {
    let env = env().await;
    env.resize("/r/w50", "/test.jpeg", AVIF).await;
    env.resize("/r/w60", "/test.jpeg", AVIF).await;
    let url = format!("{}/test.jpeg", env.origin);
    env.wait_cached(&url, "w_60_avif").await;
    env.wait_cached(&url, "w_50_avif").await;
    let cookie = sign_in(&env).await;
    let session = [("cookie", cookie.as_str()), ("origin", ORIGIN)];

    let (_, list, _) = call(&env, "cache/list", json!({"per_page": 2}), &session).await;
    assert_eq!(list["meta"]["total"], 3);
    assert_eq!(list["meta"]["total_pages"], 2);
    assert_eq!(list["data"].as_array().unwrap().len(), 2);
    let (_, sources, _) = call(&env, "cache/list", json!({"kind": "source"}), &session).await;
    assert_eq!(sources["meta"]["total"], 1);
    let (status, _, _) = call(&env, "cache/list", json!({"kind": "bogus"}), &session).await;
    assert_eq!(status, 400);

    let id = list["data"][0]["id"].as_i64().unwrap();
    let blob = env
        .get(
            &format!("/admin/blob/{id}"),
            &[("cookie", &cookie), ("host", HOST)],
        )
        .await;
    assert_eq!(blob.status, 200);
    assert_eq!(blob.header("content-type"), "image/avif");
    assert_eq!(env.get(&format!("/admin/blob/{id}"), &[]).await.status, 401);

    let (_, deleted, _) = call(&env, "cache/delete", json!({"id": id}), &session).await;
    assert_eq!(deleted["data"]["deleted"], 1);
    let (status, _, _) = call(&env, "cache/delete", json!({"id": id}), &session).await;
    assert_eq!(status, 404);

    let (_, purged, _) = call(&env, "cache/purge", json!({"url": url}), &session).await;
    assert_eq!(purged["data"]["deleted"], 2);
    assert!(env.app.store.files_on_disk().unwrap().is_empty());

    // form params work like JSON
    let form = Request::post("/api/cache/clear")
        .header("host", HOST)
        .header("cookie", &cookie)
        .header("origin", ORIGIN)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("period=7d"))
        .unwrap();
    let r = env.send(form).await;
    assert_eq!(r.status, 200, "{}", r.text());
}

#[tokio::test]
async fn referer_actions() {
    let env = env().await;
    env.get(
        &format!("/r/w50?{}/test.jpeg", env.origin),
        &[("referer", "https://news.com/a")],
    )
    .await;
    let cookie = sign_in(&env).await;
    let session = [("cookie", cookie.as_str()), ("origin", ORIGIN)];
    let (_, list, _) = call(&env, "referer/list", json!({}), &session).await;
    assert_eq!(
        list["data"][0],
        json!({"domain": "news.com", "total": 1, "disabled": false})
    );
    let (_, toggled, _) = call(
        &env,
        "referer/toggle",
        json!({"domain": "news.com"}),
        &session,
    )
    .await;
    assert_eq!(toggled["data"]["disabled"], true);
    let (status, _, _) = call(
        &env,
        "referer/toggle",
        json!({"domain": "direct"}),
        &session,
    )
    .await;
    assert_eq!(status, 400);
    let r = env
        .get(
            &format!("/r/w50?{}/test.jpeg", env.origin),
            &[("referer", "https://news.com/b")],
        )
        .await;
    assert_eq!(r.status, 403);
}

/// A local stand-in for auth.authcog.com: `/d:<host>/p:<port>/s:http?user=<callback>` answers a profile.
async fn mock_realm(email: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    let router = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let log = log.clone();
        async move {
            log.lock().push(uri.to_string());
            if uri.query().is_some_and(|q| q.contains("user=good")) {
                axum::Json(json!({"email": email, "name": "Dux", "provider": "google"}))
                    .into_response()
            } else {
                (axum::http::StatusCode::GONE, "expired").into_response()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, seen)
}

async fn start_login(env: &Env) -> (String, String) {
    let r = env
        .get("/login?redirect_to=/%23/cache", &[("host", HOST)])
        .await;
    assert_eq!(r.status, 302);
    let location = r.header("location").to_string();
    let state = location.split("state=").nth(1).unwrap().to_string();
    let cookie = r
        .header("set-cookie")
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert!(r.header("set-cookie").contains("Path=/authcog"));
    (state, cookie)
}

#[tokio::test]
async fn authcog_sign_in() {
    let (realm, seen) = mock_realm("RejOtl@Gmail.com").await;
    let env = env_with(|c| c.authcog_url = realm.clone()).await;

    let (state, state_cookie) = start_login(&env).await;
    let r = env
        .get(
            &format!("/authcog?callback=good&state={state}"),
            &[("host", HOST), ("cookie", &state_cookie)],
        )
        .await;
    assert_eq!(r.status, 303, "{}", r.header("location"));
    assert_eq!(r.header("location"), "/#/cache");
    assert_eq!(seen.lock()[0], "/d:lvh.me/p:3000/s:http?user=good");
    let session = r
        .headers
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|c| c.starts_with("ir_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let (status, me, _) = call(
        &env,
        "auth/me",
        json!({}),
        &[("cookie", &session), ("origin", ORIGIN)],
    )
    .await;
    assert_eq!(
        (status, me["data"]["email"].as_str()),
        (200, Some("rejotl@gmail.com"))
    );

    // a state works once
    let again = env
        .get(
            &format!("/authcog?callback=good&state={state}"),
            &[("host", HOST), ("cookie", &state_cookie)],
        )
        .await;
    assert_eq!(again.header("location"), "/?login_error=expired");

    // state cookie must match
    let (state, _) = start_login(&env).await;
    let r = env
        .get(
            &format!("/authcog?callback=good&state={state}"),
            &[("host", HOST), ("cookie", "ir_login_state=other")],
        )
        .await;
    assert_eq!(r.header("location"), "/?login_error=expired");

    // AuthCog says the callback is used up
    let (state, cookie) = start_login(&env).await;
    let r = env
        .get(
            &format!("/authcog?callback=bad&state={state}"),
            &[("host", HOST), ("cookie", &cookie)],
        )
        .await;
    assert_eq!(r.header("location"), "/?login_error=expired");

    // unknown host is not a destination
    assert_eq!(env.get("/login", &[("host", "evil.com")]).await.status, 400);
}

#[tokio::test]
async fn authcog_refuses_non_admins() {
    let (realm, _) = mock_realm("someone@else.com").await;
    let env = env_with(|c| c.authcog_url = realm.clone()).await;
    let (state, cookie) = start_login(&env).await;
    let r = env
        .get(
            &format!("/authcog?callback=good&state={state}"),
            &[("host", HOST), ("cookie", &cookie)],
        )
        .await;
    assert_eq!(r.header("location"), "/?login_error=no_access");
    assert!(
        r.headers
            .get_all("set-cookie")
            .iter()
            .all(|c| !c.to_str().unwrap().starts_with("ir_session="))
    );
}

#[tokio::test]
async fn logs_socket_needs_an_admin() {
    let env = env().await;
    let r = env.get("/admin/logs", &[]).await;
    assert_eq!(r.status, 401);
}
