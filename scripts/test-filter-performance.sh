#!/bin/sh
# Fixed static-index comparison; not FS7 dynamic lifecycle or a capacity test.
set -eu
umask 077
[ "$#" -eq 1 ] && [ "${1:-}" = --ephemeral-ci ] && \
    [ "${GITHUB_ACTIONS:-}" = true ] && \
    [ "${RUNNER_ENVIRONMENT:-}" = github-hosted ] && \
    [ "${RUNNER_OS:-}" = Linux ] && [ "$(uname -s)" = Linux ] || {
    printf '%s\n' 'Refusing: requires --ephemeral-ci on a disposable GitHub Linux runner.' >&2
    exit 1
}
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
cd "$repo"
for command in node cargo rustc sudo systemctl systemd-run taskset curl sha256sum lscpu; do
    command -v "$command" >/dev/null 2>&1 || { printf 'Missing: %s\n' "$command" >&2; exit 1; }
done
[ -x /usr/bin/time ] && [ -f /sys/fs/cgroup/cgroup.controllers ]
sudo -n true
output="$repo/artifacts/filter-performance"
# Never mix a prior partial/complete matrix into a new run.
[ ! -e "$output" ] || { printf 'Refusing existing result directory: %s\n' "$output" >&2; exit 1; }
mkdir -p "$output"
export TMPDIR="$output"
node --input-type=module - "$output" <<'JS'
import assert from 'node:assert/strict';
import { readFile, writeFile } from 'node:fs/promises';
import { cpuList } from './scripts/lib/wire-bench.mjs';
const output = process.argv[2];
const status = await readFile('/proc/self/status', 'utf8');
const cpus = cpuList(status.match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1]);
assert(cpus.length >= 4, 'need at least four available logical CPUs: two service, at least two driver');
const memory = BigInt((await readFile('/proc/meminfo', 'utf8')).match(/^MemTotal:\s*(\d+) kB$/m)[1]) * 1024n;
assert(memory >= 6n * 1024n ** 3n, 'need host memory for 4 GiB service plus driver/build');
await writeFile(`${output}/server-cpus`, cpus.slice(0, 2).join(','));
await writeFile(`${output}/driver-cpus`, cpus.slice(2).join(','));
await writeFile(`${output}/host-proc-status`, status);
await writeFile(`${output}/host-meminfo`, await readFile('/proc/meminfo'));
JS
PARINS_FS_SERVER_CPUS=$(cat "$output/server-cpus")
PARINS_FS_DRIVER_CPUS=$(cat "$output/driver-cpus")
export PARINS_FS_SERVER_CPUS PARINS_FS_DRIVER_CPUS
lscpu > "$output/lscpu.txt"
systemd-run --version > "$output/systemd-version.txt"
rustc --version --verbose > "$output/rustc.txt"
node --version > "$output/node-version.txt"
git rev-parse HEAD > "$output/commit.txt"
git diff --binary > "$output/source-diff.patch"
node --check scripts/bench-filter-wire.mjs
node --check scripts/lib/wire-bench.mjs

# The workflow must build web/dist first, matching the normal embedded-Web gate.
cargo test --locked --release --lib policy::wire:: --no-run --message-format=json > "$output/build.jsonl"
binary=$(node --input-type=module - "$output/build.jsonl" <<'JS'
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
const messages = (await readFile(process.argv[2], 'utf8')).trim().split('\n').map(JSON.parse);
const binaries = messages.filter(x => x.reason === 'compiler-artifact' && x.target?.name === 'parins' && x.target.kind.includes('lib') && x.profile.test && x.executable).map(x => x.executable);
assert.equal(binaries.length, 1, 'expected exactly one release libtest');
console.log(binaries[0]);
JS
)
"$binary" policy::wire::tests:: --quiet > "$output/wire-fixture-tests.txt"
corpus="$output/natsuki-list.list"
curl --fail --location --silent --show-error --proto '=https' --proto-redir '=https' --max-time 60 \
    https://raw.githubusercontent.com/Natsuki-Kaede/Natsuki-List/d1e0e168589302c62373256855b0d8542f058bdf/natsuki-list.list \
    --output "$corpus"
printf '%s  %s\n' d68e37b2a861e6e8ef85568db4237bb3e18d1a9f2476323b3dba977fe850af09 "$corpus" | sha256sum --check
sha256sum "$binary" "$corpus" > "$output/input-sha256.txt"
# No retries or best-of selection: failed smoke stops before the fixed matrix.
taskset --cpu-list "$PARINS_FS_DRIVER_CPUS" node scripts/bench-filter-wire.mjs "$binary" "$corpus" \
    --native-acceptance --linux-isolated --smoke > "$output/smoke.jsonl" 2> "$output/smoke.stderr"
taskset --cpu-list "$PARINS_FS_DRIVER_CPUS" node scripts/bench-filter-wire.mjs "$binary" "$corpus" \
    --native-acceptance --linux-isolated > "$output/measurement.jsonl" 2> "$output/measurement.stderr"
printf '%s\n' 'Fixed Linux static matrix completed. Performance judgement and FS7 lifecycle remain separate.'
