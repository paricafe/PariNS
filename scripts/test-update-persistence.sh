#!/bin/sh
# Real v0.1.4 → current → v0.1.4 data fixture, never an installer or system service.
# This proves the explicit enrollment baseline only. A future same-epoch release
# gate must pin its actual prior release, run the same two-CLI read/write sequence,
# and require success after saving every new persisted config/payload field too.
# The expected v0.1.4 rejection below must NOT be counted as that gate passing.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
baseline=d33ee4612f10719e036d885edf2851e68bdd7ca9
[ "$(git -C "$repo" rev-parse 'v0.1.4^{commit}')" = "$baseline" ] || { echo 'Unexpected v0.1.4 source identity' >&2; exit 1; }
fixture=$(mktemp -d "${TMPDIR:-/tmp}/parins-persistence.XXXXXX")
printf 'Isolated fixture (retained for inspection): %s\n' "$fixture"
mkdir "$fixture/old-source"
git -C "$repo" archive "$baseline" | tar -xf - -C "$fixture/old-source"
# Reuse dependency tooling only when its authoritative lock is byte-identical.
# The old UI is built from the archived old source, never copied from current dist.
cmp "$repo/web/package-lock.json" "$fixture/old-source/web/package-lock.json"
[ -d "$repo/web/node_modules" ] || { echo 'Run the project web dependency setup first' >&2; exit 1; }
ln -s "$repo/web/node_modules" "$fixture/old-source/web/node_modules"
npm --prefix "$fixture/old-source/web" run build
chmod -R a-w "$fixture/old-source/src"
CARGO_TARGET_DIR="$fixture/old-target" cargo build --offline --locked --manifest-path "$fixture/old-source/Cargo.toml" --bin parins
cp "$fixture/old-target/debug/parins" "$fixture/old-parins"
cargo build --offline --locked --manifest-path "$repo/Cargo.toml" --target-dir "$repo/target" --bin parins
cp "$repo/target/debug/parins" "$fixture/current-parins"
node "$repo/scripts/test-update-persistence.mjs" "$fixture" "$baseline"
