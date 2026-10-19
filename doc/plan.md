# image-resizer-rust - plan

A Rust rewrite of `../image-resizer-golang`.
Everything an image URL can do stays the same: every resize URL form, format negotiation, STEP renders and GLB, caching layers, spinner and error placeholders, response headers.
Everything around it is rebuilt: admin login through AuthCog, a MailCog-style API layer with API keys and an explorer, and a fez frontend for all pages.

## Status

| Layer | State |
|---|---|
| 1. Public resize parity | done; Go resize tests ported (`tests/resize.rs`) |
| 2. STEP | done; render and GLB tests against `tests/fixtures/box.step` |
| 3. Housekeeping | done |
| 4. API + auth | done; AuthCog flow tested against a mock realm (`tests/api.rs`) |
| 5. fez SPA | done; every page checked in a browser, light, dark and mobile |
| 6. Deploy and cutover | deployed to deb2 under dboss (`WORKERS=2`); STEP tools copied from the Go box |

Open:

* Try the AuthCog sign-in by hand against live `auth.authcog.com` (dev: `http://lvh.me:<port>/login`).
* Load test with `oha` on deb2 (both hosts switched 2026-10-18, the Go service is retired).

Found while building, beyond the plan:

* Remote SVG was served as `image/svg+xml` from our own origin (Go too), so a hostile SVG opened in a tab could run script and call the admin API with the admin's cookie. Every SVG response now carries a sandbox `Content-Security-Policy` and `nosniff`.
* Go repaired `https:/x.com` only after prefixing `https://`, which made it `https://https:/x.com`; the repair now runs first.
* Downloads no longer hold a worker slot (Go's 5 workers could all sit on slow downloads); only libvips work and the STEP tools do.
* The demo page measured a second `fetch()` sent with a different `Accept` than the `<img>`, so it reported JPEG sizes for AVIF images; it now asks for the same format the image got.

## Decisions

| Topic | Decision |
|---|---|
| Public image routes | Identical to Go, STEP included (see [Public routes](#public-routes-kept-exactly)) |
| Other routes | Free to change; admin pages move to a fez SPA plus `/api` |
| Image engine | libvips through Rust bindings (`rs-vips` or `libvips-rs`, both generated from libvips introspection) |
| HTTP | axum 0.8 + tokio, reqwest (rustls) for fetching |
| Admin auth | AuthCog central realm `https://auth.authcog.com`, admins from the required `ADMIN_EMAILS` env (`dboss.yaml`: `rejotl@gmail.com`) |
| Admin session | Stateless signed cookie (HMAC-SHA256), like dboss `internal/authcog` |
| API | MailCog style: `POST /api/{group}/{action}`, one registry, envelope `{success, status, message, data, error, meta}` |
| API extras | Bearer API keys (read-only or read-write) and a `/api` explorer page |
| Frontend | fez from CDN, no build step; `.fez` files served from `web/` and loaded at runtime |
| Pages | All pages are fez: Home, Demo, Config, Cache, Logs, API keys, API explorer |
| Storage | Fresh schema: SQLite holds metadata only, blobs are content-addressed files on disk |
| Basic Auth | Removed (`HTTP_USER_AND_PASS` is gone) |
| Docker | Dropped; production runs under dboss and deploys with lux-deploy, as today |

## Public routes kept exactly

These must answer exactly as Go does; the Go test suite is ported to prove it.

* `GET /r/{params}?{src}` - params segment, e.g. `w300`, `w_300`, `w=300`, `h200`, `w300x200`, `c300`, `c300x200`, `cam=top`, `bg=white`, `f=png`, `to=glb`, joined with `&`.
* `GET /r/{params}.{ext}?{src}` - forced format, ext one of `png`, `jpg`, `jpeg`, `webp`, `avif`, `gif`, `glb`; only known extensions strip, so `cam=-1,1,-0.5` survives.
* `GET /r.{ext}?{src}` - format only, no resize.
* `GET /resize?src=...&w=&h=&c=&crop=&width=&height=&f=&to=&cam=&bg=` - legacy query form.
* `&amp;` in the raw query is rewritten to `&`.
* Source normalization: URL-decode, prepend `https://` when there is no scheme and the value has a dot, repair `https:/` and `http:/`.
* `GET /favicon.ico` (same inline SVG) and `GET /static/*`.

Behavior kept:

* Accept negotiation AVIF > WebP > original (JPEG/PNG, everything else JPEG), `Vary: Accept` only when negotiating.
* GIF source keeps GIF output (first frame), SVG source is passthrough unless a format is forced, then rasterized.
* Resize rules: no upscaling, fit within bounds for `wXxY`, cover + crop for `c` with 50% horizontal and 30% vertical focus, clamp to `MAX_SIZE`, JPEG flattens alpha onto white.
* STEP: `.step`/`.stp` URL or `ISO-10303-21` sniff, `cam` presets (`iso`, `front`, `back`, `top`, `bottom`, `left`, `right`) or `x,y,z`, `bg` `transparent`/`none`/`white`, f3d render at `MAX_SIZE`, margin trim (alpha or white), lossless WebP render cache, GLB through `scripts/step2glb` with mesh validation, `Access-Control-Allow-Origin: *`, 422 on failure, 202 while converting.
* Cache keys keep the Go strings: `w_300_avif`, `c_300x200_webp`, `h_200_jpg`, `cam-iso_bg-transparent_w_600_avif`, `source`, `source_cam-iso_bg-transparent`, `step`, `glb`.
* Variants are cached only when a size or a format was requested; SVG passthrough is never cached as a variant.
* Client `Cache-Control: no-cache` never bypasses the cache.
* Worker pool of `WORKERS` (5), handler waits 10s, job runs up to 90s, STEP tools 45s.
* Two-level coalescing: same URL + key shares one job, same URL shares one source fetch (and one STEP download).
* Response headers:

| Response | Headers |
|---|---|
| HIT | `Cache-Control: public, max-age={MAX_AGE}, immutable`, `X-Cache: HIT`, `X-Info: from-cache; params=..; format=..`, `ETag` (new) |
| MISS | same with `X-Cache: MISS`, `X-Info: source-cache; params=..; input=..; output=..`, `ETag` (new) |
| Spinner | `Cache-Control: no-cache, max-age=10`, `Retry-After: 10`, `X-Cache: QUEUED` |
| Error SVG | `Cache-Control: no-cache, max-age=60`, `X-Cache: MISS`, `X-Info: error; ..` |
| 304 (new) | `ETag`, `Cache-Control`, `Vary` as the 200 would send |

* Timeouts, cancellations and upstream 429 show the spinner, other failures the error SVG; both SVGs are byte-for-byte the Go markup.
* `ALLOWED_DOMAINS` with `*.example.com` wildcards and the auto-allowed sibling domains of the service host.
* Referer tracking per base domain and day, disabled referer domains answer 403 with the Go message.

Dropped: `/i` (static file info), `/c`, `/config*`, `/cache*`, `/logs`, `/ws/logs`, `/demo` (now `/#/demo?src=`).

## Optimizations adopted

### 1. thumbnail shrink-on-load

* Source creation and every variant use `vips_thumbnail_buffer` instead of a full decode followed by `resize`.
* JPEG decodes at 1/2, 1/4 or 1/8 scale, WebP and SVG load at the target size; memory and CPU drop sharply on large sources.
* thumbnail premultiplies alpha around the resample, which replaces Go's manual premultiply step.
* thumbnail applies EXIF orientation; Go loses it (the AVIF re-encode strips metadata), so rotated phone photos most likely come out sideways today.
* Resize math stays in a pure function `plan_resize(orig_w, orig_h, &params) -> ResizePlan` (skip, fit, scale or cover + crop box), unit tested against the Go rules; vips only executes the plan.
* Crop: thumbnail to the cover size, then `extract_area` with the 30% vertical focus.

### 2. Cheaper source cache

* Go re-encodes every source to lossy AVIF: the slowest step in the pipeline, and a second lossy generation before the variant encode.
* New rule: a JPEG, PNG, WebP or AVIF source already within `MAX_SIZE` with no EXIF rotation is stored as the original bytes (no encode at all).
* Otherwise the thumbnail at `MAX_SIZE` is stored as WebP q95 for lossy origins and lossless WebP for lossless origins (PNG, BMP, TIFF).
* GIF keeps the original bytes and STEP renders keep lossless WebP, as in Go.
* The entry records the origin format, so the JPEG/PNG fallback encode still follows the original.

### 3. Blobs on disk, no stale data

SQLite keeps metadata only; bytes live in `tmp/blobs/ab/cd/<blake3-hex>`.
The path is the hash of the bytes, so a file can never disagree with its row, and identical outputs (no-op resizes, passthroughs) share one file.

Invariant: the set of files under `tmp/blobs` equals the set of `blobs` rows, and every row has `refs = number of entries pointing at it`.

How it holds:

* One store writer: all inserts, deletes and file unlinks run under one write lock (a single SQLite write connection in a blocking task), so a GC can never race an insert of the same hash.
* `refs` is kept by SQLite triggers on `entries` insert, delete and hash update, so every delete path (single item, clear by period, eviction, purge by URL, replace) stays correct without code in each path.
* Write: hash bytes; if the file is missing, write `tmp/blobs/incoming/<random>`, then `rename` into place (atomic on one filesystem); then one transaction upserts the blob row and the entry.
* Delete: one transaction deletes entries and the `refs = 0` blob rows it produced, commits, then unlinks those files.
* A crash can only leave an orphan file (renamed but not committed, or committed but not unlinked); it can never leave a row without a file.
* Reconcile at startup and hourly: empty `incoming/`, unlink every file whose hash has no row, delete rows whose file is gone (only possible through outside deletion), repair `refs` with one `UPDATE ... SET refs = (SELECT COUNT(*) ...)`.
* Read path self-heals: a row whose file is missing is deleted and treated as a miss.
* A store invariant test runs random put/replace/delete/clear/evict sequences, injected crashes between steps, then reconcile, and asserts files == rows and refs == counts.

Wins: the DB stays a few MB, no multi-GB `VACUUM` stalls, size accounting is `SUM(size)`, HIT responses stream from the file, admin listing never touches blob pages.

### 4. ETag + 304

* `ETag` is the quoted first 32 hex chars of the blob hash, set on HIT and MISS.
* `If-None-Match` on a HIT is answered `304` from the row alone, without opening the file.
* Spinner and error SVGs get no ETag.

### 5. Batched referer stats

* Go runs one UPSERT goroutine and one SELECT per resize request.
* New: a `DashMap<(domain, date), u64>` counter flushed every 5s in one transaction, and the disabled-domain set held in memory (`ArcSwap<HashSet>`), loaded at start and updated by the toggle action.
* Disabled state moves to its own `domains` table; Go flips `is_disabled` per day row, so a domain disabled yesterday gets a new enabled row today and the next toggle inverts both.

### 6. Real SSRF guard

* Always on in production (Go checks only when `ALLOWED_DOMAINS` is set, and only by hostname string prefix, which DNS names, decimal IPs and redirects bypass).
* A custom reqwest DNS resolver drops private, loopback, link-local, CGNAT, multicast, unspecified, ULA and IPv4-mapped private addresses; a name that resolves only to blocked addresses fails with `blocked-address`.
* IP-literal hosts skip DNS, so the URL and every redirect hop are checked explicitly in the redirect policy (max 5 hops, http/https only).
* Download cap `MAX_SOURCE_MB` (50), enforced while streaming.
* Pixel cap from the vips header before decode (100 MP) against decompression bombs.
* `ALLOW_PRIVATE_SOURCES` defaults to true outside production so local test servers and dev work.

### 7. Smarter cleanup

* Every minute: drop `source` and `step` entries older than 24h (by `kind`, so STEP `source_cam-*` and `step` rows are cleaned too; Go only matches `cache_key = 'source'`, so STEP sources never expire).
* When `SUM(blobs.size) + db file > MAX_CACHE_SIZE`, evict least recently hit entries in batches until under 90% (Go drops the oldest 50% by creation time).
* Hits are recorded in memory and flushed every 30s (`UPDATE entries SET hit_at = ? WHERE id IN (...)`).
* `auto_vacuum = INCREMENTAL` and `PRAGMA incremental_vacuum` after large deletes; no full `VACUUM`.

### 8. Non-blocking live logs

* A tracing layer writes each line to a 1000-line ring buffer and a `tokio::sync::broadcast` channel.
* Each WebSocket client reads at its own pace; a lagging client gets `... skipped N lines` instead of blocking the process (Go writes to every socket synchronously inside the logger).
* The socket checks `Origin` (Go accepts any origin, which allows cross-site WebSocket hijacking with cached Basic Auth credentials).

### Also by design

* `cache_set_max(0)` for libvips: we cache results ourselves, and the libvips operation cache only holds on to source buffers.
* Image work runs in `spawn_blocking` behind a `WORKERS` semaphore; the async runtime never blocks on vips.
* One SQLite file for everything (`tmp/cache.db`) with WAL, a pool of read connections and one writer; Go serialized every read and write through `MaxOpenConns(1)` with `cache=shared`.

### Later options (not in this plan)

* In-memory hot cache of response bytes (moka, size bounded) in front of the disk.
* Animated GIF/WebP output instead of first frame only.
* libvips concurrency and AVIF effort tuning per output size, measured with the load test.
* Cloudflare cache purge from `cache/purge`.

## Architecture

```
request ──► axum router (src/app.rs, every route in one place)
             │
             ├─ /r/*, /r.{ext}, /resize ─► resize::handler
             │     parse params ─► allowed/blocked checks ─► referer count
             │     store.get(url, key) ── HIT ─► 304 or stream file
             │        └─ MISS ─► jobs.variant(url|key)  (singleflight)
             │                    └─ semaphore(WORKERS) ─► spawn_blocking
             │                         source = jobs.source(url|source_key)  (singleflight)
             │                           └─ store.get or fetch (SSRF-safe) ─► thumbnail ─► store.put
             │                         thumbnail(source, plan) ─► encode ─► store.put
             │     wait 10s ─► result, error SVG, or spinner (job keeps running)
             │
             ├─ /api/{group}/{action} ─► registry ─► auth (cookie or Bearer) ─► action ─► envelope
             ├─ /login, /authcog, /dev/login ─► AuthCog flow
             ├─ /admin/blob/{id}, /admin/logs (WebSocket) ─► admin only
             └─ /, /api ─► web/index.html (fez SPA), /web/* ─► .fez files
```

### Layout

```
Cargo.toml, build.rs         libvips found through pkg-config
dboss.yaml
mise.toml                    rust toolchain
bin/check                    fmt, clippy, tests, fez compile, icon set
bin/icons                    Phosphor SVGs used by ui-icon -> web/lib/icons.js
config/deploy/                .yaml, local_before.sh, remote_before.sh
scripts/step2glb             copied from Go
static/                      test images, random-images.txt
web/
  index.html                 fez CDN + <script fez="..."> per component, <app-shell>
  lib/icons.js, format.js    window.ICONS, window.Fmt
  components/app/            app-shell (window.App: router, API client, nav), api-explorer
  components/tpl/            tpl-home, tpl-demo, tpl-config, tpl-cache, tpl-logs, tpl-keys
  components/ui/             ui-theme (tokens + .card/.btn/.badge/.input/.table classes), ui-icon, ui-toast, ui-dialog
  components/demo/           img-preview, step-viewer
src/
  main.rs                    clap: start, help
  config.rs                  env -> Config, validated once, fail fast
  app.rs                     App state, the route table
  resize/params.rs           URL forms -> ResizeParams + src (pure)
  resize/plan.rs             plan_resize (pure)
  resize/pipeline.rs         vips: source build, variant, encoders
  resize/svg.rs              spinner/error SVG, SVG and raster sniffing
  resize/work.rs             source, variant, STEP and GLB jobs
  resize/handler.rs          axum handlers, headers, 304, ALLOWED_DOMAINS, SVG hardening
  step.rs                    cam/bg, f3d, step2glb, GLB validation
  jobs.rs                    Flights (singleflight), Workers (CPU slots), JobError
  fetch.rs                   reqwest client, SSRF resolver, redirect policy, size cap
  store.rs                   SQLite index + blob files, reconcile, eviction
  housekeeping.rs            timers: flushes, expiry, eviction, reconcile
  referers.rs                batched counters, disabled set
  logs.rs                    tracing writer, ring buffer, broadcast
  admin.rs                   /admin/blob/{id}, /admin/logs WebSocket
  auth/mod.rs                Principal, cookies, request origin, Origin check
  auth/authcog.rs            login start, callback, exchange, dev login
  auth/session.rs            signed cookie
  auth/keys.rs               API keys
  api/mod.rs                 Action trait, Auth, Ctx, envelope, dispatch
  api/registry.rs            action table, sys/schema docs
  api/actions/*.rs
tests/                       resize.rs, step.rs, api.rs (+ common/, fixtures/box.step)
doc/                         plan.md, api.md
```

### Crates

axum (ws), tokio, tower-http (fs), reqwest (rustls, stream), rs-vips 0.7 (the layer 1 spike confirmed `thumbnail_buffer` with size modes, the savers' options, `flatten`, `find_trim` on an extracted band and `extract_area`), rusqlite (bundled) + r2d2_sqlite, blake3, dashmap, arc-swap, moka (login states), parking_lot, serde, serde_json, serde_html_form, schemars, hmac, sha2, subtle, base64, rand, chrono, dotenvy, clap, tracing, tracing-subscriber, futures-util, url, percent-encoding, tempfile.

### Schema (`tmp/cache.db`)

```sql
PRAGMA auto_vacuum = INCREMENTAL;  -- set before the first table

CREATE TABLE blobs (
  hash TEXT PRIMARY KEY,
  size INTEGER NOT NULL,
  refs INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

CREATE TABLE entries (
  id INTEGER PRIMARY KEY,
  url TEXT NOT NULL,
  key TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('source', 'step', 'variant')),
  hash TEXT NOT NULL REFERENCES blobs (hash),
  content_type TEXT NOT NULL,
  format TEXT NOT NULL,
  origin_format TEXT NOT NULL,
  size INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  hit_at INTEGER NOT NULL,
  UNIQUE (url, key)
);
CREATE INDEX entries_kind_created ON entries (kind, created_at);
CREATE INDEX entries_created ON entries (created_at);
CREATE INDEX entries_hit ON entries (hit_at);
-- triggers: entries AFTER INSERT / AFTER DELETE / AFTER UPDATE OF hash keep blobs.refs

CREATE TABLE referer_daily (
  domain TEXT NOT NULL,
  day TEXT NOT NULL,
  count INTEGER NOT NULL,
  PRIMARY KEY (domain, day)
) WITHOUT ROWID;

CREATE TABLE domains (
  domain TEXT PRIMARY KEY,
  disabled INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

CREATE TABLE api_keys (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  prefix TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  read_only INTEGER NOT NULL,
  created_by TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER
);
```

The schema is created on start; there is no live data, so changes edit it in place.

## Admin auth (AuthCog)

Ported from dboss `internal/authcog/authcog.go` and MailCog `src/auth/`.

* `GET /login?redirect_to=/#/config` - random state kept 5 min in memory (state -> destination, redirect_to) and in a `ir_login_state` cookie (HttpOnly, Lax, Secure on https, path `/authcog`); 302 to `https://auth.authcog.com/d:<host>[/p:<port>][/s:http]?state=...`.
* `GET /authcog?callback=..&state=..` - state must match the cookie (constant time) and a pending login; destination recomputed from the request must match; exchange `GET https://auth.authcog.com/<destination>?user=<callback>` (no redirects, 10s, 1 MB cap) returns `{email, name, avatar, provider}`.
* The lowercased email must be in `ADMIN_EMAILS` (required; `dboss.yaml` sets `rejotl@gmail.com`), else back to `/?login_error=no_access`; expired or unreachable map to `expired` / `unavailable`.
* Session cookie `ir_session` = `base64url(json {email, exp}) . base64url(hmac_sha256(key, payload))`, 30 days, HttpOnly, Lax (the return from AuthCog is a cross-site navigation), Secure on https.
* The key is 32 random bytes in `tmp/session.key` (0600), created on first start; `tmp/` survives releases. Deleting it signs everyone out.
* Each request re-checks the email against `ADMIN_EMAILS`, so removing an admin from the env revokes access on restart.
* Destination host comes from `Host`/`X-Forwarded-Host` and must be in `APP_HOSTS` (production: `img.sohospot.com,resizer.sohotasks.com`); dev accepts `lvh.me`, its subdomains and `localhost`. Scheme from `X-Forwarded-Proto`.
* `safe_redirect` keeps only same-origin relative paths that do not start with `//` or `/authcog`.
* CSRF: a cookie-authenticated `POST /api/*` and the logs WebSocket need an `Origin` equal to the request origin; Bearer calls are exempt.
* `GET /dev/login?email=&redirect_to=` signs in an admin email without AuthCog; the route is not registered when `APP_ENV=production`.
* Sign-in is tried by hand against live `auth.authcog.com`; tests replace the exchange base URL with a local mock.

### API keys

* Created in the Keys page by a signed-in admin: name and read-only flag; the token `irk_<32 random bytes base62>` is shown once.
* Stored as SHA-256; all hashes held in memory and refreshed on create/delete; `last_used_at` flushed in batches.
* `Authorization: Bearer <token>` acts as admin; a read-only key may only call actions with `WRITE = false`.
* A wrong Bearer token is 401, with no cookie fallback.
* Keys cannot manage keys (`Auth::Session` actions).

## API layer

* Every action is `POST /api/{group}/{action}`, one struct per action in `src/api/actions/`, registered in `src/api/registry.rs`.
* Flat params, accepted as JSON or form; validated, errors in the envelope.
* Envelope `{success, status, message, data, error, meta}`; lists put paging in `meta`.
* Each action declares `Auth` (`Public`, `Admin` = session or key, `Session` = browser admin only) and `WRITE`.
* `sys/schema` returns the registry with params and output schemas (schemars); the `/api` explorer renders it and can call actions.
* A test fails when an action is missing from `doc/api.md`.

| Action | Auth | Write | Purpose |
|---|---|---|---|
| `sys/health` | Public | no | status, version, uptime |
| `sys/schema` | Public | no | registry for the explorer |
| `auth/me` | Public | no | signed-in admin or null |
| `auth/logout` | Session | yes | clears the cookie |
| `demo/random` | Public | no | a reachable URL from `static/random-images.txt` (3s HEAD checks, as Go) |
| `stats/show` | Admin | no | settings, cache size and count, average size, usage %, queue depth, in-flight jobs |
| `referer/list` | Admin | no | domains with totals and disabled flag |
| `referer/toggle` | Admin | yes | `{domain}`; `direct` and `hidden` refused |
| `cache/list` | Admin | no | `{page, per_page, kind, url}`, 50 per page by default |
| `cache/delete` | Admin | yes | `{id}` |
| `cache/clear` | Admin | yes | `{period: 1h \| 24h \| 7d \| all}` (added in the last 1h / 24h, older than 7d, all), returns deleted count |
| `cache/purge` | Admin | yes | `{url}`: every entry of one source URL (the scripted use case for keys) |
| `key/list` | Session | no | keys without tokens |
| `key/create` | Session | yes | `{name, read_only}`, returns the token once |
| `key/delete` | Session | yes | `{id}` |

Non-envelope admin routes: `GET /admin/blob/{id}` (cache thumbnails and previews, `Cache-Control: private, max-age=3600`) and `GET /admin/logs` (WebSocket: history, then live lines).

## Frontend (fez, no build)

* Read `~/dev/libs/fez/AGENTS.md` before writing any `.fez` file.
* `web/index.html` loads `https://dux.github.io/fez/dist/fez.min.js`, then one `<script fez="/web/components/...fez">` per component, and mounts `<app-shell>`.
* `/web/*` is served from disk with `Cache-Control: no-cache` and ETags, so a deploy shows at once.
* Shared code (API client, hash router) lives in `app-shell` as the `App` global, so nothing depends on module imports in runtime-loaded `.fez` (verify what the runtime supports in layer 5 before relying on imports).
* `app-shell`: boots with `auth/me`, routes `#/<page>?k=v` through `<fez-component name="tpl-<page>">`, shows "Page not found" for unknown pages, shows guests only a sign-in screen ("Image Resizer by @dux", "Sign in with AuthCog"; outside production `/dev/login?email=` signs in by URL) at any address, with no top bar or footer, maps `login_error` to a toast.
* `App.api(path, params)` returns `data`, throws on error, toasts unless silent; `unauthorized` returns to the sign-in prompt.
* Nav (admins only): Home, Demo, Config, Cache, Logs, Keys, API; theme toggle (system, light, dark); email and Sign out.
* `ui-theme.fez` holds the design tokens and the shared classes (`.card`, `.btn`, `.badge`, `.input`, `.table`, `.segmented`); the Tailwind CDN goes away. Without a bundler, plain elements with these classes beat wrapper components (no attribute forwarding, no extra DOM).

Pages:

* `tpl-home` - landing with the URL format reference and live examples; for a STEP source the URL builder previews the model in 3D (`step-viewer bare`) with a 3D / Image switch.
* `tpl-demo` - source URL input, Random image (`demo/random`), original plus `img-preview` cards for w100/200/300, c300, c300x200, c200x300, h300/200/100, each with spinner, fade-in, size in KB, click to open.
  A `.step`/`.stp` source (`App.isStep`: the URL path extension, as the server's `step::is_step_source`, so `a.step?v=1` counts) shows `step-viewer` instead of the original: `/r.glb` in `<model-viewer>` (pinned, from jsdelivr, loaded on first use), retrying while the answer is 202.
* `tpl-config` - settings, cache size with usage bar, count, average size, referer table with enable/disable toggles, clear cache.
* `tpl-cache` - paged table (thumbnail, URL, key badge, format badge, size, age, delete), source rows marked, hover preview up to 500px with dimensions, click opens, clear by period, filter by kind and URL.
* `tpl-logs` - live stream, connection status, pause, clear, auto-scroll, line count, keyword highlighting, 5000-line cap.
* `tpl-keys` - list, create (token shown once with copy), delete.
* `api-explorer` at `/api` - actions grouped, params and output schemas, try-it form.

`bin/check` runs `fez compile 'web/components/**/*.fez'`; every page is then checked in a browser, pixel by pixel.

## Config

| Var | Default | Notes |
|---|---|---|
| `APP_ENV` | `development` | `development`, `test`, `production` |
| `PORT` | `8080` | |
| `APP_HOSTS` | dev: `lvh.me` | required in production; AuthCog destinations |
| `ADMIN_EMAILS` | required | comma separated; `dboss.yaml` sets `rejotl@gmail.com` |
| `WORKERS` | `5` | |
| `QUALITY` | `90` | 10-100 |
| `MAX_SIZE` | `1600` | 100-10000 |
| `MAX_AGE` | `86400` | |
| `MAX_CACHE_SIZE` | `1000` | MB, blobs + db (replaces `MAX_DB_SIZE`) |
| `MAX_SOURCE_MB` | `50` | download cap |
| `ALLOWED_DOMAINS` | all | `*.example.com` wildcards |
| `ALLOW_PRIVATE_SOURCES` | true outside production | SSRF guard off for local sources |
| `F3D_BIN` | `f3d` | |
| `STEP2GLB_BIN` | `scripts/step2glb` | honors `DRAWEXE_BIN` |
| `DATA_DIR` | `tmp` | db, blobs, session key |

`.env` is loaded with dotenvy.
Invalid values stop the start with a clear message (Go logged and fell back to defaults).

## Build layers

Each layer ends with `bin/check` green and a working product.

1. **Public resize parity (no STEP).**
   Config, router, fetch with SSRF guard, store with blobs + reconcile, singleflight jobs, vips pipeline with thumbnail and the cheaper source cache, negotiation, spinner/error SVGs, ETag/304, favicon, static.
   Port the Go resize tests (`resize_test.go`, `format_test.go`, `resize_e2e_test.go`) and the store invariant test.
2. **STEP.**
   cam/bg parsing, f3d render + trim, GLB conversion and validation, STEP coalescing and cache keys; port `step_test.go`, keep the render tests as `#[ignore]` when f3d is missing.
3. **Housekeeping.**
   Cleanup and LRU eviction, hit flush, referer batching and disabled domains, logs ring + broadcast.
4. **API + auth.**
   Registry, envelope, actions, AuthCog flow, signed cookie, Origin check, dev login, API keys, `sys/schema`, `doc/api.md`; everything testable with curl.
5. **fez SPA.**
   index.html, app-shell, theme and ui primitives, all pages, API explorer; browser pass on every page.
6. **Deploy and cutover.**
   `dboss.yaml` (`command: exec ./bin/server start`, dev: `watchexec -r -e rs -- cargo run -- start`, health `/favicon.ico`, `MAX_CACHE_SIZE: 500`, `tmp_clean: false`), `remote_before.sh` (`cargo build --release` against the box's libvips with `CARGO_TARGET_DIR=tmp/cargo-target` so builds stay incremental across releases), `mise.toml` with Rust, README and AGENTS.md.
   Load test Go vs Rust with `oha` on the same URLs (cold source, warm variant, HIT, 304), then switch the hosts to the Rust app; the cache refills on demand.

## Tests

* Unit: param grammar and cache keys, format ext split, src normalization, allowed domains, `plan_resize`, SVG/raster/STEP sniffing, cam/bg, GLB validation, retryable errors, spinner/error SVG markup, referer base domain, SSRF address ranges, `safe_redirect`, destination, cookie sign/verify/expiry.
* Integration (in-process app + local axum image server): every URL form, Accept negotiation, forced formats, SVG passthrough and rasterize, GIF, HIT/MISS headers, no-cache ignored, spinner on a slow source (wait timeout set per test), job and source coalescing (request counter on the image server), source reuse across variants, error SVG, ETag/304, disabled referer 403, cleanup and eviction, reconcile after injected crashes, AuthCog callback against a mock exchange, admin gating, read-only keys, Origin check, WebSocket auth.
* `cargo test` needs libvips installed; STEP render tests need f3d and DRAWEXE.

## Bugs in the Go version this design removes

* STEP source and raw entries (`source_cam-*`, `step`) never expire.
* Referer disable state is per day row, so toggles drift across days.
* SSRF check is skipped without `ALLOWED_DOMAINS` and is bypassable by DNS, numeric IPs and redirects.
* `/ws/logs` accepts any Origin.
* One slow logs WebSocket blocks every log call.
* Full `VACUUM` on a blob-heavy database stalls the single shared connection, and with it every request.
* Source re-encode drops EXIF orientation; the Rust pipeline applies it (`exif_rotation_is_applied` in `tests/resize.rs`).
* Remote SVG served from our own origin could run script against the admin API (see Status).

## Risks

* Rust libvips bindings are young; layer 1 starts with a spike that exercises every vips call we need (thumbnail options, savers with effort/lossless/strip, flatten, find_trim on an extracted band, extract_area) before the pipeline is written.
* thumbnail rounding can differ from Go's resize by 1px; ported tests assert the Go dimensions, and any difference is fixed in `plan_resize`, not by loosening tests.
* fez from the CDN is a rolling build; a breaking fez release reaches production without a deploy.
* Building with LTO on the box is slow; the persistent target dir keeps repeat deploys fast.
