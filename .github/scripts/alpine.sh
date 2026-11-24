#!/bin/sh
# Build (and optionally test) the musl binary inside Alpine. Runs in node:22-alpine, repo at /w.
# usage: alpine.sh <rust-target> [test]
set -e
apk add --no-cache bash curl build-base git >/dev/null
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -q
. "$HOME/.cargo/env"
corepack enable
cd /w
pnpm install --frozen-lockfile
pnpm build:native --target "$1"
if [ "$2" = test ]; then
  pnpm build:ts
  pnpm test
fi
