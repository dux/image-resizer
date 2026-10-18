# AGENTS.md

Image Resizer: resize/crop/convert images by URL with two cache layers, STEP renders, AuthCog admin.
Rust (axum + libvips via rs-vips + rusqlite) backend, fez frontend in `web/` with no build step.
Rewrite of `../image-resizer-golang`; `doc/plan.md` is the design and the list of deliberate changes.

## Read first

* `README.md` - URL format, config, how the cache works.
* `doc/api.md` - the API contract; code must match it (a test checks every action is listed).
* `~/dev/libs/fez/AGENTS.md` - before touching any `.fez` file.

## Rules

* Public image URLs (`/r/*`, `/r.{ext}`, `/resize`, STEP params, cache keys, response headers) must answer exactly as the Go service does. `src/resize/params.rs` and `tests/resize.rs` pin the behavior; change them only on purpose.
* Every API action is `POST /api/{group}/{action}`: a struct in `src/api/actions/`, registered in `src/api/registry.rs`, listed in `doc/api.md`. Actions that change nothing set `WRITE = false` (read-only keys depend on it); `Auth::Session` refuses API keys.
* Responses use the envelope `{success, status, message, data, error, meta}`; paging goes in `meta`.
* Cache bytes are only written and removed through `Store` (`src/store.rs`): it owns the files-equal-rows invariant. Never touch `DATA_DIR/blobs` elsewhere.
* libvips work runs in `Workers::cpu` (blocking pool, `WORKERS` slots); downloads never hold a slot.
* Admin = AuthCog session from `auth.authcog.com` with an email in `ADMIN_EMAILS` (required env, set to `rejotl@gmail.com` in `dboss.yaml`), or a Bearer API key.
* SVG responses go through `resize::handler::harden` (sandbox CSP).
* Sign-in is tried by hand against live `auth.authcog.com`; tests mock the realm.
* The user starts the dev server; do not start it. For e2e checks run the server only for the length of one command and kill it.

## Commands

* `bin/check` - must pass before any commit.
* `cargo run -- start` / `dboss s` - dev server.
* `bin/icons` - after using a new icon name; names chosen at runtime must appear literally as `icon: 'name'` or in a `<ui-icon name={...}>` expression.
* There is no live data: schema changes edit `SCHEMA` in `src/store.rs` in place; delete `tmp/cache.db*` and `tmp/blobs` locally.

## Gotchas

* `rs-vips` savers take flags (`keep`) as ints and enums as nick strings (`"down"`, `"av1"`).
* `thumbnail_buffer` output is sequential: `copy_memory()` before reading it twice (trim, then save).
* `web/index.html` wraps the app in `<div class="pjax" id="app">`, so `Fez.pjax` handles path links (`/api`, `/#/demo`) and keeps the `app-shell` instance: route state must be re-read on `pjax:render`, not only on `hashchange`. Only `#/...` hashes are routes; `#anchor` is an anchor.
* Scoped `.fez` styles reach into child components: give page and shell classes unique names (`shell-*`).
* The demo measures output by fetching it again with an AVIF/WebP `Accept`, the one every current browser sends for `<img>`.
