# Development

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md) · [Architecture](architecture.md)

Run commands from the repository root using the toolchain pinned in [rust-toolchain.toml](../rust-toolchain.toml) and locked dependencies. Contributor rules and module ownership live in [AGENTS.md](../AGENTS.md), and module-level invariants live in each module's `//!` documentation. [ci.yml](../.github/workflows/ci.yml) defines the full CI gate.

See [Architecture](architecture.md) for request flow, state lifetimes, management observation, configuration transactions, and terminal shutdown boundaries.

In management test fixtures, bind an ephemeral `TcpListener` and transfer it directly to `manage::serve`. Keep that socket open through startup instead of releasing and rebinding its port, which races with other fixtures and outgoing connections. Treat a successful management HTTP response as readiness, not a bare TCP connect.

## Build and test

The web console is a React and TypeScript app under `web/`. Rust embeds `web/dist/` at compile time, so build the console before compiling Rust. Production runs only the Rust binary.

`npm run build` produces Brotli and gzip copies of `app.js` and `app.css`, inserts a content version into each asset URL in `index.html`, and rejects extra chunks, remote resources, and inline scripts so every production asset matches a fixed Rust route under the strict CSP. Only requests for the exact embedded version receive one-year caching. The HTML document and mismatched versions return `no-store`. API, WebSocket, and DoH responses skip asset compression.

Console browser storage keeps only presentation preferences, the query-log refresh interval, and custom background images in IndexedDB. Unavailable storage falls back to defaults. Credentials, session bindings, private keys, query logs, and configuration drafts never enter browser storage. Shared style tokens drive both console and page styles. Apply backdrop blur and rim styling once on the outer surface of a joined group, and honor `prefers-reduced-motion`.

Run the checks that cover your change:

| Change | Checks |
| --- | --- |
| Rust | `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked --all-targets` |
| Build or configuration | `cargo build --locked --release`, `./target/release/parins --config parins.example.toml --check` |
| Web | In `web/`: `npm ci --ignore-scripts`, `npm run typecheck`, `npm test`, `npm run build`, and `npm audit --registry https://registry.npmjs.org --audit-level=high`. Verify interaction changes in a real browser across errors, session transitions, keyboard use, and narrow screens. |
| Installer or bootstrap | `sh scripts/test-install.sh`, `sh scripts/test-bootstrap.sh`. See [Installer and bootstrap checks](#installer-and-bootstrap-checks). |
| Service, UDP, subscriptions, or updates | Run the `--ephemeral-ci` suites in [Acceptance workflows](#acceptance-workflows) only on disposable runners. |
| Dependencies | `cargo install cargo-audit --version 0.22.2 --locked`, then `cargo audit --deny warnings` |
| Documentation only | Verify referenced paths, commands, and links, review the diff, and run `git diff --check`. |

Run the standard full local gate:

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

Unit and integration tests use loopback upstreams and never query public DNS. To run the managed console locally without systemd or root:

```sh
./target/release/parins --manage --state-dir parins-state --web-listen 127.0.0.1:3000
```

The binary uses Tokio's multi-threaded runtime with one worker per CPU by default. Set `TOKIO_WORKER_THREADS=2` to select two workers.

## Console end-to-end acceptance

Test the built, embedded console against its real Rust API, a disposable private state directory, loopback listeners, and a synthetic DNS upstream. Disable automatic update checks and use a unique `.localhost` management hostname to isolate cookies. Never modify host DNS, certificate trust, or system services.

Verify each step separately and record the browser, binary, and observed result:

1. Submit an incorrect login and confirm rejection, then sign in with the keyboard.
2. Save a cache-only edit in the TOML editor. Confirm the value updates while the DNS listener and running generation stay in place.
3. Hold an accepted DNS query at the mock upstream, save a restart-requiring edit, and refresh **Overview** in a second authenticated tab. Confirm it shows stopping and not ready, then release the query within `shutdown_grace_ms` and confirm both its reply and the new generation's readiness.
4. In the disposable fixture only, induce a configuration persistence failure after candidate preparation. Confirm the error, retained draft, unchanged saved revision, and restored DNS, then restore the fixture.
5. Discard the failed draft and trigger **Undo last save**. Confirm the restored settings and a working DNS listener.
6. Change the management hostname. Confirm the destination notice, automatic navigation, and fresh sign-in requirement where the old session cannot authorize the new origin.
7. Send real DNS queries and verify **Overview** totals and **Query log** entries, including live refresh. Sign out and verify the login screen.
8. Repeat navigation and error handling on a narrow viewport with the keyboard, checking focus, reachable controls, and horizontal overflow. Separate application console errors from browser extension errors.

Component mocks do not substitute for this acceptance pass. HTTP-to-HTTPS transition testing also requires a browser-trusted certificate without bypassing browser warnings. The Rust management integration suite separately exercises TLS with pinned test roots alongside cross-site navigation, API rejection, and session revocation.

## Benchmarks

Run the in-process microbenchmarks as needed:

```sh
cargo run --locked --release --example bench -- 1000 32
cargo run --locked --release --example bench_dot -- 1000 8
cargo run --locked --release --example bench_cache -- --seconds 1 --repeats 3
cargo test --locked --release --lib flight::bench::registry -- --exact --ignored --nocapture
cargo test --locked --release --lib flight::bench::concurrent_registry -- --exact --ignored --nocapture
```

- `bench` outputs JSON for warm cache, cold single upstream, and cold parallel upstreams against loopback mocks.
- `bench_dot` compares fresh and pooled DoT transactions against one loopback TLS peer.
- `bench_cache` benchmarks cache operations with 1, 2, and 4 workers plus correctness checks. Pass `--case Multi --workers 2` to run one case, or set `PARINS_BENCH_SHARDS`, `PARINS_BENCH_MAX_ENTRIES`, and `PARINS_BENCH_MAX_BYTES`.
- `flight::bench::registry` runs five rounds of active-key joins and distinct-key turnover at 128, 1024, and 4096 groups with no network IO, printing JSON join timings and verifying group roles and capacity.
- `flight::bench::concurrent_registry` runs five rounds at 128 and 4096 groups with 1, 4, and 8 callers across one hot key, uniform active keys, and distinct-key turnover, reporting join and lock percentiles. Preallocated thread-local timing buffers exist only in tests and are included in measurements, and wall throughput includes query construction and holder turnover.

These are in-process microbenchmarks, not wire DNS QPS. For any performance change, keep a baseline, repeat the same workload on a quiet host, and report variance and regressions.

### UDP wire baseline

`scripts/bench-dns.mjs` launches the compiled binary with private fixture data, kernel-assigned loopback DNS and admin ports, and a synthetic UDP upstream. It reuses the fixed-rate sender and answer oracle from the lifecycle suites. The driver and mock upstream run in one Node process while PariNS runs separately. It requires Node.js, `ps`, and a built binary, and never changes host DNS or system services.

```sh
node --test scripts/test-wire-load.mjs
node scripts/bench-dns.mjs --binary target/release/parins --output docs/wire-baseline --seconds 5 --repeats 5 --rates 1000,4000 --workers 4
```

The benchmark runs five cases: a direct-upstream driver control, warm cache, unique names with 20 ms upstream delay, one shared uncached name with 20 ms delay, and unique names with 200 ms delay to retain active flight groups. Every reply must match its DNS ID, question, answer, flags, and TTL, and cache and upstream counters verify the intended path. Each round rotates case order and retains failed trials. Correct QPS uses the fixed offered-load duration. Scheduled latency includes driver lag, whereas wire latency starts at send time. A passing trial proves complete correct delivery at the offered rate, not a production SLA.

The output directory records provenance including binary and script digests, source diff, tool versions, and host details, plus per-trial JSON and logs and a repeated-run summary. Parent directories are created automatically, and an existing output directory is rejected. CPU time comes from `ps` and RSS samples once per second, which are unconstrained local process observations rather than Linux cgroup limits or lifetime memory peaks.

On macOS, pass `--profile` with at least five seconds per trial to capture a separate three-second `sample` pass under the many-active workload. That pass is excluded from the baseline summary. Stack samples point to wait paths but do not measure lock percentiles. Check contention hypotheses against correctness, throughput, driver-control, and repeated latency data before altering locks. CI runs all five cases once at 200 QPS as a correctness smoke test and does not treat shared-runner timings as a baseline.

## Packaging and releases

Build a host-native development archive:

```sh
sh scripts/package.sh
```

The script builds the web console first and writes `target/packages/parins-REVISION-OS-ARCH.tar.gz` with its `.sha256` file. Each archive contains `parins`, `parins-updater`, `install-build-info.json`, `install.sh`, the `deploy/` unit files, `parins.example.toml`, licenses, `README.md`, `CHANGELOG.md`, and `SHA256SUMS`, with no keys or private configuration.

Build a release archive on its native Linux architecture:

```sh
sh scripts/package.sh --version vX.Y.Z --target x86_64-unknown-linux-musl
```

Release builds require a clean tracked worktree, matching `Cargo.toml` version, and fully static binaries.

[release.yml](../.github/workflows/release.yml) runs on `v*` tags and manual dispatch, publishing only on tags. For both `x86_64` and `aarch64`, it runs web checks, `cargo fmt`, Clippy, musl tests, installer and bootstrap tests, and dependency audits on `x86_64`. It then builds each package, stages it through `scripts/bootstrap.sh`, and verifies it under the managed systemd acceptance suite. Next it validates both packages, builds `parins-update.json` via `scripts/update-manifest.mjs`, extracts notes via `scripts/release-notes.mjs`, uploads assets to a draft release, publishes it without overwriting existing releases, and verifies that `releases/latest/download/bootstrap.sh` installs the tagged version.

Tagged commits must sit on `main`. A patch release on an older line keeps the highest published version marked as `latest`.

To prepare a release:

1. Update the version in `Cargo.toml` and both the `version=` default and usage text in `scripts/bootstrap.sh` together.
2. Move `Unreleased` entries in `CHANGELOG.md` into a new heading for that version.
3. Run `sh scripts/test-bootstrap.sh` to verify the usage text and default version contract.

## Installer and bootstrap checks

`sh scripts/test-install.sh` and `sh scripts/test-bootstrap.sh` run unprivileged without systemd using `--root` staging directories. Installer fixtures also require `jq` and a native `cc` compiler. These suites enforce:

- `bootstrap.sh` defaults to the `Cargo.toml` version and keeps its entry point at the end of the file so a truncated pipe cannot start an install. Pass `--package ARCHIVE.tar.gz` to `scripts/test-bootstrap.sh` to stage a real native package.
- Extracted binaries become executable only after checksum verification so live preflight can run the helper safely.
- Archives with unexpected paths, links, special files, or bad checksums fail before extraction or installation.
- Older, modified, symlinked, or unknown host layouts fail before any write and reject the removed enrollment flag.
- Fresh-install guidance prints the tunnel command, local URL, and valid token only from a regular single-link token file. Initialized reinstalls proceed only after read-only preflight succeeds.
- Staging and `--dry-run` never start services or execute candidate binaries.
- Failed replacements restore prior files. Recovery fixtures run the real `EXIT` handler with distinct packaged and restored helpers, verifying that the packaged helper waits without committing, the restored helper checks its unit contract, and either failure leaves readiness unconfirmed.
- Service readiness fixtures wait for stable, matching-generation DNS readiness before UDP and TCP probes, independently of management HTTP readiness. Real failures report a named stage and fixed status fields without credentials. The official updater control preserves the immutable `v0.1.5` status contract, while current builds must populate every readiness field. Preflight rejection uses a fixture-built native executable with a fixed exit code.

The first line of `deploy/parins-managed.service` identifies the installer contract. The installer compares the installed managed unit against the package template, and the updater compares all four managed unit files against its embedded copies. Preserve their exact bytes during formatting edits.

During live preflight, the installer pins the target mount namespace and passes a descriptor path to the trusted helper. The helper drops privileges and capabilities before running the candidate, captures stdout and stderr in bounded files, and preserves the candidate exit code. Registration runs under the installer lock, which releases before restarting `parins-updater.path` so queued requests reach an idle helper.

Root executor launch-loop tests use Tokio's paused monotonic clock to verify storage becoming ready past one minute, restored identities, the ten-second fresh stability window, process replacement, missing or stalled health, and blocked observations. Unit timeout checks keep systemd candidate, rollback, and recovery timeouts above the corresponding launch budgets. The real systemd suite also tests `install-wait`, byte-identical journal and public status files, and `install-verify`.

## Acceptance workflows

Suites that install software or alter host state require `--ephemeral-ci` and refuse to run outside disposable GitHub-hosted Linux runners. Never run them on a workstation or live server, and never remove their guards.

Re-run filter acceptance manually for each release candidate. Path-triggered runs check harness edits only. The public DNS workflow performs one operator-directed probe and never gates CI or measures capacity. The QEMU recovery suite keeps raw serial and cloud-init output private, publishing only fixed status categories and counts.

| Workflow | Command | Scope |
| --- | --- | --- |
| `ci.yml`, `release.yml` | `sh scripts/test-systemd.sh --ephemeral-ci` with `--binary-path` or `--package` | Real installer and systemd lifecycle, updater contract, and certificate renewal. |
| `ci.yml` | `sh scripts/test-udp.sh --ephemeral-ci` | IPv4 and IPv6 UDP reply source addresses in isolated network namespaces. |
| `filter-acceptance.yml` (manual or harness edits) | `scripts/test-systemd.sh --ephemeral-ci --package DIR --filter-subscriptions` | Subscriptions against a real HTTPS fixture, including offline restart, update freeze, and disk-full recovery. |
| `filter-lifecycle.yml` (manual) | `scripts/test-systemd.sh --ephemeral-ci --package DIR --filter-lifecycle` | Subscription refresh and restart in a fixed 2 CPU and 2 GiB cgroup with a pinned load generator. Keep workload and targets fixed across all rounds. |
| `update-check.yml` (manual) | `scripts/test-systemd.sh --ephemeral-ci --package DIR --official-updater-check` | Update check against a real published release without forging an official build identity. |
| `update-recovery.yml` (manual or harness edits) | `python3 -B scripts/test-update-reboot.py --ephemeral-ci ...` | Update recovery across reboot, hard reset, and journal disk-full inside a disposable QEMU guest using a development package. |
| `public-dns-acceptance.yml` (manual) | `scripts/test-public-dns.py` | One verified DoQ or HTTP/3 query against an operator-specified public target with no hardcoded deployment targets or credentials. |

## Hickory fork

DNS wire parsing uses Hickory. Recursive resolution uses the `parins-ecs` branch of the `Natsuki-Kaede/hickory-dns` fork, which adds per-request ECS plus CNAME and DNSSEC fixes. `Cargo.toml` pins `hickory-resolver` to a Git revision and patches `hickory-proto` from crates.io to the same revision under `[patch.crates-io]`. The first build fetches this fork via Git. To update the fork, bump both revisions together, commit `Cargo.lock`, and run the full Rust test suite including `tests/recursion.rs`.
