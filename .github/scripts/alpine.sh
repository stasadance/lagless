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
# native build: with --target, napi points the linker at <arch>-linux-musl-gcc, which Alpine
# doesn't have (its gcc is already the musl one), so only pass it when cross-compiling
if [ "$1" = "$(rustc -vV | sed -n 's/^host: //p')" ]; then
  pnpm build:native
else
  pnpm build:native --target "$1"
fi
if [ "$2" = test ]; then
  pnpm build:ts
  pnpm test
fi
