# Development

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md)

Commands run from the repository root. Use the toolchain pinned in
[rust-toolchain.toml](../rust-toolchain.toml) and locked dependencies. Contributor
rules and module ownership are in [AGENTS.md](../AGENTS.md); detailed invariants are
in each module's `//!` documentation. [ci.yml](../.github/workflows/ci.yml) is the
full CI gate.

## Build and test

The console is a React and TypeScript app in `web/`. Its build output in `web/dist/`
is embedded into the Rust binary, so build it before compiling Rust. Production runs
only the Rust binary.

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

## Benchmarks

```sh
cargo run --locked --release --example bench -- 1000 32
cargo run --locked --release --example bench_dot -- 1000 8
cargo run --locked --release --example bench_cache -- --seconds 1 --repeats 3
```

- `bench`: resolver scenarios (warm cache, cold single upstream, cold parallel
  upstreams) against synthetic loopback upstreams, as JSON.
- `bench_dot`: fresh versus pooled DoT transactions against one loopback TLS peer.
- `bench_cache`: cache operations with 1, 2 and 4 workers, with correctness checks.
  `--case Multi --workers 2` picks one case; `PARINS_BENCH_SHARDS`,
  `PARINS_BENCH_MAX_ENTRIES` and `PARINS_BENCH_MAX_BYTES` override the cache shape.

These are microbenchmarks. Cache operations per second are not wire DNS QPS, and
none of them measures network or TLS throughput, process memory or capacity on a
deployment host. For a performance change, keep a baseline, repeat the same workload
on a quiet host, and report variance and regressions.

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

To prepare a release, change the version in `Cargo.toml` and the default version in
`scripts/bootstrap.sh` (its `version=` default and usage text) together;
`sh scripts/test-bootstrap.sh` checks the usage text, and the package staging checks
the default. Release notes come from the one `CHANGELOG.md` section headed with that
version, so move the Unreleased entries there.

## Installer and bootstrap checks

`sh scripts/test-install.sh` and `sh scripts/test-bootstrap.sh` run without root or
systemd in staged directories (`--root`). They cover these contracts:

- The bootstrap uses the Cargo version and the default, unpinned entry point.
  `sh scripts/test-bootstrap.sh --package ARCHIVE.tar.gz` stages a real native package
  the same way.
- Archives with unexpected paths, links, special files or checksum mismatches are
  rejected before anything is extracted or installed.
- Older, modified, symlinked or unknown layouts are rejected before any write, with a
  hint to handle them by hand; the removed enrollment option is refused.
- Fresh-install output prints the tunnel command, the local URL and the exact token,
  and never prints a token from a symlinked, hard-linked or malformed file. An
  initialized reinstall is recognized only after its read-only preflight succeeds.
- Staging and `--dry-run` start no service and run no candidate binary.
- A failed replacement restores the previous files.

## Acceptance workflows

Suites that install software or change a system refuse to run without
`--ephemeral-ci` on a disposable GitHub-hosted Linux runner. Never run them on a
developer machine or an existing installation, and keep their guards. Each result
proves only its own fixture.

| Workflow | Command | What it covers |
| --- | --- | --- |
| `ci.yml`, `release.yml` | `sh scripts/test-systemd.sh --ephemeral-ci` (`--binary-path` or `--package`) | Installs through the real installer and systemd, including the updater contract and certificate renewal. |
| `ci.yml` | `sh scripts/test-udp.sh --ephemeral-ci` | IPv4 and IPv6 UDP reply source addresses in isolated network namespaces. |
| `filter-acceptance.yml` (manual, or on harness changes) | `scripts/test-systemd.sh --ephemeral-ci --package DIR --filter-subscriptions` | Subscriptions against a real HTTPS fixture, including offline restart, update freeze and disk-full recovery. |
| `filter-lifecycle.yml` (manual) | `scripts/test-systemd.sh --ephemeral-ci --package DIR --filter-lifecycle` | Subscription refresh and restart under a fixed 2 CPU / 2 GiB cgroup with a pinned load generator. Fix the workload and targets before running and keep all rounds. |
| `update-check.yml` (manual) | `scripts/test-systemd.sh --ephemeral-ci --package DIR --official-updater-check` | The update check of a real published release. Never rebuild with a forged official identity. |
| `update-recovery.yml` (manual, or on harness changes) | `python3 -B scripts/test-update-reboot.py --ephemeral-ci ...` | Update recovery across normal reboot, hard reset and journal-volume disk-full in a disposable QEMU guest. Uses a development package; does not cover official upgrades or power loss. |
| `public-dns-acceptance.yml` (manual) | `scripts/test-public-doq.py` | One verified DoQ query to an operator-chosen public address, host name and port. Never hardcode deployment targets or add production credentials. |

These suites never change the host's DNS, firewall, certificate trust or system
services outside the disposable runner. Local, CI and development-build results do
not prove production capacity, public reachability or official upgrade and rollback
on a particular host.

## Hickory fork

DNS messages use Hickory. Recursive resolution uses the Hickory recursor from a
fork, `Natsuki-Kaede/hickory-dns` (branch `parins-ecs`, which adds per-request ECS
and CNAME and DNSSEC fixes). `Cargo.toml` pins `hickory-resolver` to a Git revision
and patches `hickory-proto` from crates.io to the same revision under
`[patch.crates-io]`. The first build fetches it with Git. To update it, change both
revisions together, commit the resulting `Cargo.lock` change, and run the full Rust
checks, including the recursion tests (`tests/recursion.rs`).
