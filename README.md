# Image Resizer (Rust)

An image resizing service: put a size in front of an image URL and get it back resized, cropped or converted, in the smallest format the browser accepts, cached twice.
It is the Rust rewrite of [image-resizer-golang](../image-resizer-golang): every public image URL behaves the same, the admin side is new (AuthCog sign-in, an API with keys, a fez UI).

## Features

* **AVIF first** - AVIF > WebP > JPEG/PNG from the `Accept` header, or a forced format (`/r/w300.png`).
* **Two cache layers** - each source is downloaded once and cached; every size is cut from the cached source.
* **Coalescing** - concurrent requests for one image share one job, different sizes of one URL share one download.
* **Spinner fallback** - a request waits 10s, then gets an animated SVG placeholder while the job finishes in the background.
* **STEP (CAD)** - `.step` / `.stp` sources render to images per camera and background, or convert to GLB for three.js.
* **Admin UI** - config and referer domains, cache explorer, live logs, API keys, API explorer; sign-in through AuthCog.
* **SSRF guard** - DNS answers and every redirect hop are checked against private ranges; downloads are size-capped.

## Quick start

libvips >= 8.15 must be installed (it does all decoding, resizing and encoding).

```sh
brew install vips                 # macOS
sudo apt install libvips-dev      # Debian/Ubuntu, plus libheif-plugin-aomenc on Debian 13 for AVIF output

ADMIN_EMAILS=you@example.com cargo run -- start   # http://localhost:8080
dboss s                           # or the dev stack from ./dboss.yaml (watchexec restart on change)
```

Open `http://lvh.me:8080/#/demo`.
Outside production, `/dev/login?email=rejotl@gmail.com&redirect_to=/%23/config` signs in an admin without AuthCog.

## URL format

```
/r/{params}?{source_url}
```

A source without a scheme gets `https://`.

| URL | Result |
|---|---|
| `/r/w300?example.com/a.jpg` | width 300, height follows (`w_300` and `w=300` work too) |
| `/r/h200?example.com/a.jpg` | height 200 |
| `/r/w300x200?example.com/a.jpg` | fit inside 300x200 |
| `/r/c300x200?example.com/a.jpg` | crop to exactly 300x200, centered horizontally, 30% from the top |
| `/r/c300?example.com/a.jpg` | square crop |
| `/r/w300.png?example.com/a.jpg` | forced PNG (`png`, `jpg`, `jpeg`, `webp`, `avif`, `gif`); also `f=png` |
| `/r.webp?example.com/a.jpg` | format only, original size |
| `/resize?src=https://example.com/a.jpg&w=200` | legacy query form (`w`, `h`, `c`, `width`, `height`, `crop`, `f`) |

Images are never enlarged by a fit, and nothing exceeds `MAX_SIZE`.
GIF sources stay GIF (first frame), SVG sources are passed through unless a format is forced.

### STEP models

| URL | Result |
|---|---|
| `/r/w600?example.com/part.step` | snapshot render, iso camera, transparent background |
| `/r/w600&cam=top?example.com/part.step` | `iso`, `front`, `back`, `top`, `bottom`, `left`, `right` or an `x,y,z` view vector |
| `/r/w600&bg=white.png?example.com/part.step` | opaque white background |
| `/r.glb?example.com/part.step` | binary glTF for three.js (alias `/r/to=glb`), `Access-Control-Allow-Origin: *` |

Rendering needs [f3d](https://f3d.app) 3.x (`--force-reader`), GLB needs OpenCascade `DRAWEXE` 7.4+ (see `./scripts/step2glb`).
Without them STEP requests answer the error SVG (or 422 for GLB).

### Response headers

| Response | Headers |
|---|---|
| Cache hit | `Cache-Control: public, max-age={MAX_AGE}, immutable`, `X-Cache: HIT`, `ETag` |
| Cache miss | same with `X-Cache: MISS` |
| `If-None-Match` matches | `304`, answered from the cache index without reading the file |
| Still working after 10s | spinner SVG, `Cache-Control: no-cache, max-age=10`, `Retry-After: 10`, `X-Cache: QUEUED` |
| Failure | error SVG, `Cache-Control: no-cache, max-age=60` |

`X-Info` says where the answer came from (`from-cache; params=w_300; format=avif`, `error; fetch-failed; status=404`).
Negotiated answers carry `Vary: Accept`; forced formats do not.
SVG answers carry a sandbox `Content-Security-Policy`, so a remote SVG cannot run script on this origin.

## Admin

Every page of the UI (home and demo included) needs an AuthCog sign-in; guests see only a sign-in screen.
It uses the central realm (`auth.authcog.com`) and accepts only an email in `ADMIN_EMAILS` (`./dboss.yaml` sets `rejotl@gmail.com`).
The session is a signed cookie; the key is `DATA_DIR/session.key`, and deleting it signs everyone out.

Scripts use API keys created on the Keys page:

```sh
curl -X POST https://img.sohospot.com/api/cache/purge \
  -H "Authorization: Bearer irk_..." -H "Content-Type: application/json" \
  -d '{"url": "https://example.com/photo.jpg"}'
```

Every action is listed in [doc/api.md](doc/api.md) and in the explorer at `/api`.

## Configuration

Read from the environment and `.env`; an invalid value stops the start.

| Variable | Default | |
|---|---|---|
| `APP_ENV` | `development` | `development`, `test`, `production` |
| `PORT` | `8080` | |
| `APP_HOSTS` | - | hosts AuthCog may return to; required in production |
| `ADMIN_EMAILS` | required | comma-separated admin emails; `./dboss.yaml` sets `rejotl@gmail.com` |
| `WORKERS` | `5` | parallel image jobs (libvips, f3d, DRAWEXE) |
| `QUALITY` | `90` | AVIF/WebP/JPEG quality, 10-100 |
| `MAX_SIZE` | `1600` | max output and cached source dimension, 100-10000 |
| `MAX_AGE` | `86400` | `Cache-Control` max-age of images |
| `MAX_CACHE_SIZE` | `1000` | MB of cache (files + database) before LRU eviction |
| `MAX_SOURCE_MB` | `50` | download size limit |
| `ALLOWED_DOMAINS` | all | source whitelist, `*.example.com` wildcards; siblings of the service host always pass |
| `ALLOW_PRIVATE_SOURCES` | `true` outside production | allow sources on private addresses |
| `DATA_DIR` | `tmp` | `cache.db`, `blobs/`, `session.key` |
| `F3D_BIN` | `f3d` | |
| `STEP2GLB_BIN` | `scripts/step2glb` | honors `DRAWEXE_BIN` |

## How it works

* **Cache** - SQLite (`DATA_DIR/cache.db`) holds the index; bytes live in content-addressed files `DATA_DIR/blobs/ab/cd/<blake3>`.
  A file is written before its row commits and removed only after its row is gone, triggers keep reference counts, and a reconcile at start and every hour removes anything a crash left behind.
* **Sources** - a small upright JPEG/PNG/WebP/AVIF is stored as downloaded; larger ones are scaled to `MAX_SIZE` with shrink-on-load and stored as WebP (lossless for lossless origins). EXIF rotation is applied.
  Sources and raw STEP files expire after 24 hours; variants stay until evicted.
* **Eviction** - every minute, when the cache is over `MAX_CACHE_SIZE`, the least recently served entries go until it is under 90%.
* **Referers** - counted per base domain and day in memory, flushed every 5 seconds; disabled domains answer 403.
* **Logs** - every log line goes to stdout and a 1000-line buffer streamed to the Logs page.

See [doc/plan.md](doc/plan.md) for the design and what changed from the Go version.

## Development

```sh
bin/check              # fmt, clippy -D warnings, all tests, fez compile, icon set
cargo test             # needs libvips; STEP tests skip without f3d / DRAWEXE
bin/icons              # after using a new ui-icon name (fetches Phosphor SVGs)
```

The frontend has no build step: `./web/index.html` loads fez from its CDN and each `.fez` file from `./web/components/`.

## Deploy

Production runs under [dboss](https://github.com/dux/dboss) and deploys with lux-deploy (`lux-deploy up`).

* `./dboss.yaml` - the process, its hosts and env; `dboss s` runs the same file locally.
* `./config/deploy/.yaml` - the box (`deb2`, an ssh alias); server-only env such as `WORKERS` goes in its `.env.local` (`lux-deploy env:set`).
* `./config/deploy/local_before.sh` - stages the git-visible files in `tmp/deploy-src`, so the local cargo `target/` never ships.
* `./config/deploy/remote_before.sh` - builds `bin/server` on the box against its libvips; Rust comes from `./mise.toml`, the cargo target dir lives in the shared `tmp/` so builds stay incremental.
* The cache lives in `tmp/`, which lux-deploy links to a shared dir that survives releases.

The box needs `libvips-dev`, `pkg-config` and a C toolchain, plus the STEP tools from above.
