# API

Every action is `POST /api/{group}/{action}`, registered in `./src/api/registry.rs`.
Params are one flat object, sent as JSON (`application/json`) or a form (`application/x-www-form-urlencoded`); an empty body is `{}`.
`POST /api/sys/schema` returns every action with its JSON schemas, and `/api` renders them as an explorer.

## Envelope

Every answer, success or not:

```json
{
  "success": true,
  "status": 200,
  "message": null,
  "data": {},
  "error": null,
  "meta": { "action": "cache/list", "request_id": "1a2b3c4d5e6f", "time_ms": 3 }
}
```

* `status` equals the HTTP status.
* On failure `data` is null and `error` is `{code, message}`; `message` repeats the human text.
* Lists put paging in `meta` (`page`, `per_page`, `total`, `total_pages`).

| Error code | Status | When |
|---|---|---|
| `bad_request` | 400 | Body is not valid JSON |
| `validation_failed` | 400 | Params do not parse or are out of range |
| `unauthorized` | 401 | Admin action without a session, or an unknown Bearer key |
| `forbidden` | 403 | Read-only key on a write action, key on a session-only action |
| `bad_origin` | 403 | Cookie-authenticated call from another site |
| `not_found` | 404 | Unknown action or record |
| `internal` | 500 | Bug or storage failure |

## Auth

* `Public` - anyone.
* `Admin` - an AuthCog admin session (cookie `ir_session`) or `Authorization: Bearer irk_...`.
* `Session` - an admin in the browser only; API keys are refused.
* A cookie-authenticated call must come from the same site: `Origin` must name the request host (or `Sec-Fetch-Site: same-origin`).
* A read-only key may only call actions marked read.
* A Bearer header that is not a valid key is `401`; there is no cookie fallback.

## Actions

| Action | Auth | Write | Params | Data |
|---|---|---|---|---|
| `sys/health` | Public | no | - | `{status, version, uptime_secs}` |
| `sys/schema` | Public | no | - | `[{path, doc, auth, write, params, output}]` |
| `auth/me` | Public | no | - | `{email, is_admin, via, dev_login}` |
| `auth/logout` | Session | yes | - | `true`, clears the cookie |
| `demo/random` | Public | no | - | `{url}`: a reachable image from `static/random-images.txt` |
| `stats/show` | Admin | no | - | `{version, uptime_secs, settings, cache, jobs}` |
| `referer/list` | Admin | no | - | `[{domain, total, disabled}]`, busiest first |
| `referer/toggle` | Admin | yes | `domain` | `{domain, disabled}`; `direct` and `hidden` refused |
| `cache/list` | Admin | no | `page`, `per_page` (max 200), `kind` (`source`, `step`, `variant`), `url` (substring) | entries, newest first; paging in `meta` |
| `cache/delete` | Admin | yes | `id` | `{deleted}` |
| `cache/clear` | Admin | yes | `period`: `1h`, `24h` (added within), `7d` (older than), `all` (default) | `{deleted}` |
| `cache/purge` | Admin | yes | `url` | `{deleted}`: every entry of that source URL |
| `key/list` | Session | no | - | `[{id, name, prefix, read_only, created_by, created_at, last_used_at}]` |
| `key/create` | Session | yes | `name`, `read_only` | `{key, token}`; the token is shown only once |
| `key/delete` | Session | yes | `id` | `{deleted}` |

A cache entry is `{id, url, key, kind, hash, content_type, format, origin_format, size, created_at, hit_at}`; times are Unix seconds.

## Other admin routes

* `GET /admin/blob/{id}` - the stored bytes of a cache entry (thumbnails in the cache page), `Cache-Control: private, max-age=3600`.
* `GET /admin/logs` - WebSocket: the last 1000 log lines as one message, then one message per line; a slow reader gets `... skipped N lines`.

## Sign-in routes

* `GET /login?redirect_to=/#/config` - starts the AuthCog sign-in.
* `GET /authcog?callback=..&state=..` - AuthCog's return; sets the session and redirects, or redirects to `/?login_error=expired|unavailable|no_access`.
* `GET /dev/login?email=..&redirect_to=..` - outside production only: signs in an admin email without AuthCog.

## Example

```sh
curl -s -X POST https://img.sohospot.com/api/cache/purge \
  -H "Authorization: Bearer $IMG_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"url": "https://example.com/photo.jpg"}'
```
