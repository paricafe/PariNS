# Development

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md)

Commands below run from the repository root. Use the toolchain in
[rust-toolchain.toml](../rust-toolchain.toml), locked dependencies and the current
[contributor rules](../AGENTS.md). Module responsibilities are listed under
[Ownership](../AGENTS.md#ownership); detailed invariants live in each module's `//!`
documentation. The full CI gate is defined in [ci.yml](../.github/workflows/ci.yml).

## Build and test

The management UI is a React 19/TypeScript application built into four fixed
same-origin assets under `web/dist/`. Production runs only the Rust binary; it
does not run Node.js. Build the UI before compiling Rust because the binary
embeds the generated files:

```sh
cd web
npm ci --ignore-scripts
npm run typecheck
npm test
npm run build
cd ..
```

```sh
cargo run --locked -- --config parins.example.toml --check
cargo test --locked --all-targets
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
```

The frontend lockfile is pinned. `npm run build` rejects extra chunks, remote
resources, and inline scripts so every referenced production asset has an exact
Rust route. The repository's strict CSP is retained. The adapted beUI motion
components are free MIT-licensed source with their notice shipped in release
archives; see [the beUI notice](../web/src/components/beui/README.md).

The example configuration uses a loopback test upstream; see
[Configuration](configuration.md) before sending real queries. Tests use controlled
loopback upstreams and do not rely on public DNS answers.

The binary uses Tokio's multi-thread runtime, normally one worker per available
CPU; `TOKIO_WORKER_THREADS=2` can explicitly select two workers. This is not CPU
affinity or a memory limit. Cache and coalescing state use short shared locks;
UDP has one receive loop followed by spawned query tasks. More workers do not
guarantee linear scaling.

## Benchmarks and packaging

```sh
cargo run --locked --release --example bench -- 1000 32
cargo run --locked --release --example bench_dot -- 1000 8
cargo run --locked --release --example bench_cache -- --seconds 1 --repeats 3
cargo install cargo-audit --version 0.22.2 --locked
cargo audit --deny warnings
sh scripts/package.sh
```

- `bench` runs synthetic loopback upstreams and emits JSON for warm cache, cold
  single-upstream and cold parallel-upstream Resolver scenarios.
- `bench_dot` compares fresh and pooled authenticated DoT transactions against one
  synthetic loopback TLS peer, without caching or artificial delay. JSON includes
  checked answers, errors, connection/handshake counts (full handshakes separately
  from session resumption) and latency percentiles.
- `bench_cache` measures hot-key, multi-key, ECS and negative churn with 1/2/4 OS
  workers; fixed workloads check IDs, questions, TTLs and scopes, and report sampled
  p99 and correctness failures. `--case Multi --workers 2` selects a case and
  `PARINS_BENCH_SHARDS=4` selects shards. `PARINS_BENCH_MAX_ENTRIES` and
  `PARINS_BENCH_MAX_BYTES` override capacity for an interleaved old/new-budget
  comparison with the same binary; the output records the selected budgets.
  Positive/negative partition changes affect hit rates, so churn throughput alone
  is not an equal-work comparison.

Compare repeated runs with the same workload on a quiet host. These are
microbenchmarks: they do not measure network/TLS throughput, process RSS, wire DNS
QPS or capacity on any deployment host.

Packaging creates a host-native archive under `target/packages`, containing no
keys or private configuration. `deploy/parins.service` is a Linux systemd template
for file-configured mode, not an installed service; review paths, file permissions,
firewall and source limits before using it. The template defaults to unprivileged
ports.

CI runs the web checks, formatting, Clippy, tests, release build, configuration
check, audit, installer fixtures, managed-service acceptance and packaging on Linux;
macOS runs Clippy and the Rust tests. Check CI results separately from local runs.

## Installer and bootstrap checks

`sh scripts/test-install.sh` and `sh scripts/test-bootstrap.sh` run in staged roots
without systemd. They cover these installer contracts:

- Bootstrap fixtures use the Cargo release version and exercise the default,
  unpinned entry point. Each actual native release package is staged through that
  entry point before publication, and the public quick-start check asserts the exact
  published version.
- Success guidance distinguishes first setup from an initialized installation only
  after the existing identity-preserving preflight succeeds. It never parses private
  state or infers initialization from a token file. Staging and `--dry-run` start no
  service.
- Unknown, legacy and symlinked target layouts are rejected before any write.

## Acceptance workflows

Suites that install or change a real system refuse to run without `--ephemeral-ci`
on a disposable GitHub-hosted Linux runner. Never run them on a developer host or
an existing installation, and keep their guards intact. Release and public DNS
acceptance do not change the host. Each result proves only its declared fixture.

- **Managed service** (`ci.yml`, `release.yml`): `scripts/test-systemd.sh
  --ephemeral-ci` installs native musl binaries or a release package through the real
  installer and systemd, including the managed updater's installation contract.
- **UDP source address** (`ci.yml`): `sh scripts/test-udp.sh --ephemeral-ci` checks
  IPv4/IPv6 reply source addresses in isolated network namespaces.
- **Release** (`release.yml`): runs on version tags and manual dispatch. It builds
  and verifies both native packages and the update manifest; only a tag publishes.
- **Filter acceptance** (`filter-acceptance.yml`): `scripts/test-systemd.sh
  --ephemeral-ci --package DIR --filter-subscriptions` runs the isolated
  subscription service suite against a real HTTPS fixture, including offline
  restart, update freeze and ENOSPC recovery.
- **Filter lifecycle** (`filter-lifecycle.yml`, manual): the real managed service
  runs in a verified 2 CPU/2 GiB, no-swap cgroup with a separately pinned load
  generator. Its synthetic HTTPS source branch changes only fixture domains; keep CI
  read-only and public certificate validation normal. Fix the workload and SLO
  before running, retain all three rounds, distinguish CDN propagation from the
  measured refresh window, and prove request-held old-generation release separately
  from latency samples. Measure restart with derived indexes and rebuild from
  unchanged selected source objects separately; preserve the catalog and record CPU,
  RSS and disk IO without claiming that the host page cache was cold.
- **Official update check** (`update-check.yml`, manual): installs the actual
  published first updater-capable release; never rebuild with a forged official
  identity. Pin the installed version/source to its tag and independently require
  latest to equal it. Three fixed idle/check wire rounds use the authenticated
  managed check endpoint, honor cooldown, and observe a new success without
  replaying POST. Same-version checks do not download an update or prove A-to-B
  replacement.
- **Public DNS acceptance** (`public-dns-acceptance.yml`, manual): takes an
  explicitly selected public address, certificate hostname and DoQ port, sends one
  `example.com. A` query within one deadline, and verifies the public CA, hostname,
  ALPN and response with normal certificate verification. Never hardcode deployment
  targets or add production credentials to CI; ordinary CI does not contact your
  deployment. A pass covers that address family and network path at that time.
- **Update boot recovery** (`update-recovery.yml`): a hosted runner controls a
  disposable QEMU guest. Keep the host guard, pinned image digest, pinned guest SSH
  identity and private cleanup; never reboot the runner or fake hosted-runner flags
  inside a guest. Normal reboot, virtual hard reset and bounded journal-volume ENOSPC
  are separate fresh-guest scenarios. Disk-fault acceptance must observe the real
  helper's failed journal write and recovery on that same volume after space is
  freed, preserving the installed binary and managed configuration. This uses a
  development package: it does not cover official upgrade/rollback, physical
  power loss or full update-disk faults.

### Deployment acceptance

Before deployment, choose management/DNS network access rules, provision readable
external certificates, review backups, measure capacity on the target host, and
validate independent encrypted clients. Use **Public DNS acceptance** for an
independent public DoQ check; local loopback results do not prove public
reachability.

## Planned scope

- Explicit emergency-profile product policy and health-driven scheduling.
- Licensed third-party rule import and additional filtering response modes.
- DoT pipelining and alternate HTTP/3 endpoint discovery.
- Measured public-service capacity, network abuse protection and deployment/rollback acceptance.
- Zero-downtime replacement for changes that currently restart DNS (anything outside
  the [listener-preserving settings](operations.md#configuration-changes-and-statistics)).

PariNS forwards to existing resolvers by default; optional recursive resolution
is a validating resolver for its own clients, not a general recursive server
(no RPZ, DNS64, local zones or mixed forward/recursive resolution). The
implementation uses Rust, Tokio for asynchronous IO, and Hickory for DNS message
parsing and encoding. Recursion uses the Hickory recursor from a fork
(`Natsuki-Kaede/hickory-dns`, branch `parins-ecs`) pinned by revision in
`Cargo.toml`, with `hickory-proto` patched to the same revision; the first build
fetches it with Git, and updating it means changing both revisions together.
