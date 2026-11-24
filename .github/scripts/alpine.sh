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
# napi sets the linker to <arch>-linux-musl-gcc on musl targets; Alpine's own gcc already is
# the musl one but has no such name, so give it that name
ln -sf "$(command -v gcc)" "/usr/local/bin/$(uname -m)-linux-musl-gcc"
pnpm build:native --target "$1"
if [ "$2" = test ]; then
  pnpm build:ts
  pnpm test
fi
