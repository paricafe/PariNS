# Development

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md)

Commands run from the repository root. Use the toolchain pinned in
[rust-toolchain.toml](../rust-toolchain.toml) and locked dependencies. Contributor
rules and module ownership are in [AGENTS.md](../AGENTS.md); detailed invariants are
in each module's `//!` documentation. [ci.yml](../.github/workflows/ci.yml) is the
full CI gate.

See [Architecture](architecture.md) for request flow, state lifetimes, management
observation and the configuration and terminal shutdown boundaries.

Management fixtures bind an ephemeral `TcpListener` and transfer it directly to
`manage::serve`. Keep that socket open through startup; releasing its port before
rebinding races with other fixtures and outgoing connections. Readiness is a
successful management response, not a bare TCP connection.

## Build and test

The console is a React and TypeScript app in `web/`. Its build output in `web/dist/`
is embedded into the Rust binary, so build it before compiling Rust. Production runs
only the Rust binary. `npm run build` also writes Brotli and gzip copies of
`app.js` and `app.css` and adds a content version to each address in `index.html`.
Only requests for the embedded resource's exact version receive year-long
caching. The console document and mismatched resource versions use
`no-store`. API, WebSocket and DoH responses are outside asset compression.

The console stores presentation preferences and the query-log refresh interval
locally. Custom background images live in IndexedDB. Unavailable browser
storage falls back to defaults. Credentials, session bindings, private keys,
query records and configuration drafts stay out of both storage mechanisms.
Shared style tokens serve the console and page styles. Apply backdrop blur
and rim styling once on the outer surface of a joined group, and honor
`prefers-reduced-motion` in motion treatments.

Run the checks that match your change:

| Change | Checks |
| --- | --- |
| Rust | `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked --all-targets` |
| Build or configuration | `cargo build --locked --release`, `./target/release/parins --config parins.example.toml --check` |
| Web | In `web/`: `npm ci --ignore-scripts`, `npm run typecheck`, `npm test`, `npm run build`; `npm audit --registry https://registry.npmjs.org --audit-level=high`. Test interaction changes in a real browser, including errors, session changes, keyboard use and small screens. |
| Installer or bootstrap | `sh scripts/test-install.sh`, `sh scripts/test-bootstrap.sh` (see [below](#installer-and-bootstrap-checks)) |
| Service, UDP, subscriptions, updates | The `--ephemeral-ci` suites in [Acceptance workflows](#acceptance-workflows), only on disposable runners |
| Dependencies | `cargo install cargo-audit --version 0.22.2 --locked`, then `cargo audit --deny warnings` |
| Documentation only | Check referenced paths, commands and links; review the diff; `git diff --check` |

```sh
cd web
npm ci --ignore-scripts
npm run typecheck
npm test
npm run build
cd ..
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Tests use loopback upstreams and never depend on public DNS. `npm run build` rejects
extra chunks, remote resources and inline scripts, so every production asset has a
fixed Rust route under the strict CSP. To run the console locally without systemd or
root:

```sh
./target/release/parins --manage --state-dir parins-state --web-listen 127.0.0.1:3000
```

The binary uses Tokio's multi-threaded runtime with one worker per CPU by default;
`TOKIO_WORKER_THREADS=2` selects two.

## Console end-to-end acceptance

Run the built, embedded console against its real Rust API, a disposable private
state directory, loopback listeners and a synthetic DNS upstream. Disable automatic
update checks and use a unique `.localhost` management hostname to isolate browser
Cookies. Do not change host DNS, certificate trust or system services.

Verify each boundary separately and record the browser, binary and observed result:

1. Reject an incorrect login, then sign in through the keyboard.
2. Save a cache-only edit from the TOML editor. Confirm that the saved value changes
   while the DNS listener and running generation remain in place.
3. Hold an accepted DNS query at the fixture upstream, save a restart-requiring
   edit, and refresh Overview in a second authenticated tab. It must show stopping
   and not ready; release the query within the shutdown grace and confirm its
   answer and the replacement generation's readiness.
4. In the disposable fixture only, make configuration persistence fail after
   candidate preparation. Confirm an error, retained draft, unchanged saved
   revision and restored DNS. Restore the fixture before continuing.
5. Discard the failed draft and use Undo last save. Confirm the restored values
   and a usable DNS listener.
6. Change the management hostname. Confirm the stated destination, automatic
   navigation and a new login; the old session must not authorize the new realm.
7. Send real DNS queries and confirm Overview totals and query-log entries,
   including a new entry through live refresh. Sign out and confirm the login page.
8. Repeat navigation and error handling at a narrow viewport with the keyboard;
   check focus, reachable controls and horizontal overflow. Inspect application
   console errors separately from browser extension errors.

Do not count component mocks as this acceptance. HTTP/HTTPS transition coverage
also needs a certificate the browser already trusts; never bypass its warnings.
The management integration suite separately exercises TLS with pinned test roots
and checks cross-site navigation, API rejection and session invalidation.

## Benchmarks

```sh
cargo run --locked --release --example bench -- 1000 32
cargo run --locked --release --example bench_dot -- 1000 8
cargo run --locked --release --example bench_cache -- --seconds 1 --repeats 3
cargo test --locked --release --lib flight::bench::registry -- --exact --ignored --nocapture
cargo test --locked --release --lib flight::bench::concurrent_registry -- --exact --ignored --nocapture
```

- `bench`: resolver scenarios (warm cache, cold single upstream, cold parallel
  upstreams) against synthetic loopback upstreams, as JSON.
- `bench_dot`: fresh versus pooled DoT transactions against one loopback TLS peer.
- `bench_cache`: cache operations with 1, 2 and 4 workers, with correctness checks.
  `--case Multi --workers 2` picks one case; `PARINS_BENCH_SHARDS`,
  `PARINS_BENCH_MAX_ENTRIES` and `PARINS_BENCH_MAX_BYTES` override the cache shape.
- `flight::bench::registry`: an opt-in registry CPU microbenchmark, with five
  rounds of active-key joins and distinct-key turnover at 128, 1024 and 4096 groups.
  It emits JSON with per-join timings and checks group roles and capacity; it does
  no network IO and measures no wire DNS QPS.
- `flight::bench::concurrent_registry`: five rounds at 128/4096 groups with 1/4/8
  callers, comparing one hot key, uniform active keys and distinct-key turnover.
  It checks roles and capacity and emits join and lock-acquisition percentiles.
  Preallocated thread-local timing buffers exist only in tests; their overhead is
  included. Wall throughput also includes query construction and holder turnover.

These are in-process microbenchmarks; `bench_cache` counts cache operations, which
are not wire DNS queries. For a performance change, keep a baseline, repeat the same
workload on a quiet host, and report variance and regressions.

### UDP wire baseline

`scripts/bench-dns.mjs` starts the production binary with private fixture data,
kernel-assigned loopback DNS/admin ports and a synthetic UDP upstream. It reuses
the lifecycle suites' fixed-rate sender and answer oracle. The driver and mock
upstream share one Node process; the server runs separately. It requires Node,
`ps`, and an already built binary; it changes no host DNS or service settings.

```sh
node --test scripts/test-wire-load.mjs
node scripts/bench-dns.mjs --binary target/release/parins --output docs/wire-baseline --seconds 5 --repeats 5 --rates 1000,4000 --workers 4
```

The five cases are a direct-upstream driver control, warm cache, unique names with
20 ms upstream delay, one shared uncached name with 20 ms delay, and unique names
with 200 ms delay to retain more active groups. Every response must match its DNS
ID, question, answer, flags and TTL. Cache/upstream counters verify the intended
path. Each round rotates case order and retains failed trials. Correct QPS uses
the fixed offered-load duration; scheduled latency includes driver lag, while
wire latency starts at actual send time. A passed trial means complete correct
delivery at the offered rate, not a production capacity or latency SLA.

The new output directory contains provenance (binary and harness digests, source
diff, tool versions and host), per-trial JSON/logs and a repeated-run summary.
Parent directories are created as needed; an existing output directory is refused.
CPU comes from process `ps` time; RSS is sampled once per second. These are
co-located, unconstrained process observations, not Linux cgroup/OOM evidence or
lifetime memory peaks. The managed lifecycle wrapper retains its Linux resource
gates separately.

On macOS, add `--profile` with at least five seconds per trial to capture a separate
three-second `sample` pass under the many-active workload. That pass is excluded
from the baseline summary. Stack samples can locate waiting paths; they do not
measure mutex acquisition percentiles. Compare a contention hypothesis with the
correctness, throughput, driver-control and repeated latency evidence before
changing shared locks. `examples/bench.rs` remains an in-process Resolver benchmark.
CI runs the five cases once at 200 QPS as a correctness smoke test, without treating
the shared runner's timings as a performance baseline.

## Packaging and releases

```sh
sh scripts/package.sh
```

Without options this builds a host-native development package,
`target/packages/parins-REVISION-OS-ARCH.tar.gz` with its `.sha256`. It always builds
the console from the checkout first. The archive holds `parins`, `parins-updater`,
`install-build-info.json`, `install.sh`, the `deploy/` units, `parins.example.toml`,
licenses, `README.md`, `CHANGELOG.md` and a `SHA256SUMS` manifest; no keys or private
configuration.

A release package is built on its native Linux architecture:

```sh
sh scripts/package.sh --version vX.Y.Z --target x86_64-unknown-linux-musl
```

The version must match `Cargo.toml`, tracked files must be clean, and the binaries
must be fully static.

Releases come from [release.yml](../.github/workflows/release.yml). It runs on `v*`
tags and on manual dispatch, but only a tag publishes. For each of x86_64 and aarch64
it runs the web checks, formatting, Clippy and tests on the musl target, and the
installer and bootstrap fixtures (plus the dependency audits on x86_64), then builds
the package,
stages it through the default bootstrap entry point and installs it with the managed
service acceptance suite. It then verifies both packages, writes the update manifest
`parins-update.json` (`scripts/update-manifest.mjs`), and publishes the archives,
checksums, raw binaries, manifest and `scripts/bootstrap.sh`, with notes from
`scripts/release-notes.mjs`. An existing release is never overwritten. Finally it
downloads `releases/latest/download/bootstrap.sh` and checks that it installs the
tagged version.

The tagged commit must be on `main`. Assets are uploaded to a draft before
publication. A patch to an older line keeps the highest published version as
`latest`.

To prepare a release, change the version in `Cargo.toml` and the default version in
`scripts/bootstrap.sh` (its `version=` default and usage text) together;
`sh scripts/test-bootstrap.sh` checks the usage text, and the package staging checks
the default. Release notes come from the one `CHANGELOG.md` section headed with that
version, so move the Unreleased entries there.

## Installer and bootstrap checks

`sh scripts/test-install.sh` and `sh scripts/test-bootstrap.sh` run without root or
systemd in staged directories (`--root`). Installer fixtures also require `jq`
and a native `cc` compiler for service fixture checks. They cover these contracts:

- The bootstrap uses the Cargo version and the default, unpinned entry point.
  `sh scripts/test-bootstrap.sh --package ARCHIVE.tar.gz` stages a real native package
  the same way. Keep the bootstrap entry point last so an incomplete download
  piped to the shell cannot start an install.
- Extracted programs receive executable permissions after checksum verification,
  so the installer can run the helper during live preflight.
- Archives with unexpected paths, links, special files or checksum mismatches are
  rejected before anything is extracted or installed.
- Older, modified, symlinked or unknown layouts are rejected before any write, with a
  hint to handle them by hand; the removed enrollment option is refused.
- Fresh-install output prints the tunnel command, the local URL and the exact token,
  and never prints a token from a symlinked, hard-linked or malformed file. An
  initialized reinstall is recognized only after its read-only preflight succeeds.
- Staging and `--dry-run` start no service and run no candidate binary.
- A failed replacement restores the previous files. Recovery fixtures exercise
  the actual EXIT handler with distinct packaged/restored helpers: the packaged
  helper waits without committing, the restored helper verifies its own unit
  contract, and either failure leaves readiness unconfirmed.
- Service readiness fixtures wait for stable, matching-generation DNS readiness
  before UDP/TCP probes, independently of management HTTP readiness. Their
  unprivileged regression cases run through `test-install.sh`; real service
  failures report a named stage and fixed status fields without credentials.
  The explicit official updater control retains the immutable v0.1.5 status
  contract and real DNS probes; current builds must supply every readiness field.
  Preflight rejection uses a fixture-owned native executable with a fixed exit
  status, independent of host utility packaging.

The installed managed unit's first line identifies its installer contract.
The installer compares the existing managed unit to the package template,
and the updater compares all four managed unit files to its embedded copies.
Preserve their exact bytes during comment or formatting changes.

Live preflight pins the target mount namespace in the installer and gives the
trusted helper a path to that descriptor. The helper drops credentials and
capabilities before executing the candidate. Separate bounded output files
limit stdout and stderr while preserving the candidate's exit status.
Registration uses the held installer lock. Release it before restarting the
update request path so queued requests reach an idle helper.

The root executor's launch-loop tests use Tokio's paused monotonic clock. They
cover storage becoming ready after the former one-minute deadline, restored
identities, the fresh ten-second stability window, process replacement, missing
or stalled health and a blocked observation. Unit timeout checks keep systemd's
candidate/rollback and recovery budgets above the corresponding launch budgets.
The real systemd suite also invokes `install-wait` against an initialized process,
compares the journal and public status byte for byte, and requires `install-verify`
to complete the pending installation.

## Acceptance workflows

Suites that install software or change a system refuse to run without
`--ephemeral-ci` on a disposable GitHub-hosted Linux runner. Never run them on a
developer machine or an existing installation, and keep their guards.

Re-run filter acceptance manually for the exact release candidate. Its
path-triggered runs check harness changes. The public DNS workflow is a
single operator-directed probe. It does not gate routine CI or measure
capacity. The QEMU suite keeps raw serial and cloud-init output private and
publishes only fixed status categories and counts.

| Workflow | Command | What it covers |
| --- | --- | --- |
| `ci.yml`, `release.yml` | `sh scripts/test-systemd.sh --ephemeral-ci` (`--binary-path` or `--package`) | Installs through the real installer and systemd, including the updater contract and certificate renewal. |
| `ci.yml` | `sh scripts/test-udp.sh --ephemeral-ci` | IPv4 and IPv6 UDP reply source addresses in isolated network namespaces. |
| `filter-acceptance.yml` (manual, or on harness changes) | `scripts/test-systemd.sh --ephemeral-ci --package DIR --filter-subscriptions` | Subscriptions against a real HTTPS fixture, including offline restart, update freeze and disk-full recovery. |
| `filter-lifecycle.yml` (manual) | `scripts/test-systemd.sh --ephemeral-ci --package DIR --filter-lifecycle` | Subscription refresh and restart under a fixed 2 CPU / 2 GiB cgroup with a pinned load generator. Fix the workload and targets before running and keep all rounds. |
| `update-check.yml` (manual) | `scripts/test-systemd.sh --ephemeral-ci --package DIR --official-updater-check` | The update check of a real published release. Never rebuild with a forged official identity. |
| `update-recovery.yml` (manual, or on harness changes) | `python3 -B scripts/test-update-reboot.py --ephemeral-ci ...` | Update recovery across normal reboot, hard reset and journal-volume disk-full in a disposable QEMU guest. Uses a development package; does not cover official upgrades or power loss. |
| `public-dns-acceptance.yml` (manual) | `scripts/test-public-dns.py` | One verified DoQ or HTTP/3 DNS query to an operator-chosen public address, host name and port. Never hardcode deployment targets or add production credentials. |

## Hickory fork

DNS messages use Hickory. Recursive resolution uses the Hickory recursor from a
fork, `Natsuki-Kaede/hickory-dns` (branch `parins-ecs`, which adds per-request ECS
and CNAME and DNSSEC fixes). `Cargo.toml` pins `hickory-resolver` to a Git revision
and patches `hickory-proto` from crates.io to the same revision under
`[patch.crates-io]`. The first build fetches it with Git. To update it, change both
revisions together, commit the resulting `Cargo.lock` change, and run the full Rust
checks, including the recursion tests (`tests/recursion.rs`).
