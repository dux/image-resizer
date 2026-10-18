#!/usr/bin/env bash
# local_before.sh - runs on this machine before any remote work.
#
# lux-deploy rsyncs `src:` with a fixed exclude list, which would ship the local cargo target/
# (gigabytes). Stage exactly what git would carry - tracked plus new, non-ignored files, with
# their working-tree content - into tmp/deploy-src, the `src:` in .yaml.
set -euo pipefail

dest=tmp/deploy-src
rm -rf "$dest"
mkdir -p "$dest"
git ls-files -z --cached --others --exclude-standard \
  | rsync -a --from0 --files-from=- ./ "$dest/"
