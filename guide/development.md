# Development

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md)

Commands below run from the repository root. Use the toolchain in
[rust-toolchain.toml](../rust-toolchain.toml), locked dependencies and the current
[contributor rules](../AGENTS.md). The full CI gate is defined in
[ci.yml](../.github/workflows/ci.yml).

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

The example uses loopback addresses and an upstream on port 5354. Set
`upstreams.servers` to your chosen resolver(s) before sending queries. Unknown configuration
keys and invalid resource limits are rejected at startup.

The binary uses Tokio's multi-thread runtime, normally one worker per available
CPU; `TOKIO_WORKER_THREADS=2` can explicitly select two workers. This is not CPU
affinity or a memory limit. Cache and coalescing state use short shared locks;
UDP has one receive loop followed by spawned query tasks. More workers do not
guarantee linear scaling. Cache byte limits use conservative entry charges,
not process RSS. Measure on the target machine before increasing resource budgets.

```sh
cargo run --locked --release --example bench -- 1000 32
cargo run --locked --release --example bench_dot -- 1000 8
cargo run --locked --release --example bench_cache -- --seconds 1 --repeats 3
cargo install cargo-audit --version 0.22.2 --locked
cargo audit --deny warnings
sh scripts/package.sh
```

The benchmark runs only synthetic loopback upstreams and emits JSON for warm
cache, cold single-upstream and cold parallel-upstream scenarios. It is a Resolver baseline,
not network/TLS throughput, RSS measurement or production capacity evidence.
`bench_dot` separately compares fresh and pooled authenticated DoT transactions
against one synthetic loopback TLS peer, without response caching or artificial
delay. JSON includes checked answers, errors, connection/handshake counts and
latency percentiles. Full handshakes are counted separately from TLS session
resumption; compare repeated runs with the same query count and concurrency.
These transport microbenchmarks are not production capacity guarantees.
`bench_cache` separately measures hot-key, multi-key, ECS and negative churn with
1/2/4 OS workers; fixed workloads check IDs, questions, TTLs and scopes. It reports
sampled p99 and correctness failures. Repeat under a quiet host and compare the
same workload (`--case Multi --workers 2` selects a case; `PARINS_BENCH_SHARDS=4`
selects shards). Positive/negative partition changes affect hit rates, so churn
throughput alone is not an equal-work speed comparison. These are cache operations,
not wire DNS QPS or proof of capacity on a 2-vCPU/4-GiB VPS.
Packaging creates a host-native archive under `target/packages`, containing no
keys or private configuration. `deploy/parins.service` is a Linux systemd template,
not an installed service; review paths, file permissions, firewall and source
limits before deployment. The template defaults to unprivileged ports. CI runs
formatting, Clippy, tests, release build, configuration check, audit and packaging;
Linux/macOS CI results must be checked separately from local macOS acceptance.

## Module boundaries

| Module | Responsibility |
| --- | --- |
| `config` | Parse and validate startup configuration |
| `protocol` | DNS parsing, request rules, response correlation, UDP encoding |
| `ecs` | Peer provenance, subnet selection, wire validation, scope and client echo |
| `cache` | Independent subnet answers, TTL/negative policy, bounded LRU eviction |
| `policy` | Immutable local rules, label matching, allow precedence, CNAME filtering |
| `flight` | Bounded in-flight sharing and cancellation ownership |
| `metrics` | Fixed counters, RAII lifecycle gauges and cumulative latency buckets |
| `resolver` | Compose policy, ECS, cache, upstream deadline and response restoration |
| `upstream` | Independent UDP exchange and validated TCP fallback |
| `upstreams` | Unified endpoints, explicit bootstrap, weighted/parallel scheduling, H3 preference |
| `tls` / `doh` / `quic` | Authenticated transport, framing, HTTP/stream lifecycle |
| `ingress` | Shared encrypted-query admission and response serialization |
| `limits` | Bounded socket-subnet token buckets and RAII query/connection quotas |
| `admin` | Loopback read-only metrics and process liveness |
| `manage` | HTTP/HTTPS management transport, authentication, private state, and transactional DNS configuration changes |
| `web` | Embedded setup wizard, dashboard, and configuration editor |
| `transport::tcp` | Length-prefixed framing used on both sides |
| `server` | Listener ownership, admission budgets, client tasks, shutdown |
| `main` | CLI arguments, startup, OS signals |

Tests use controlled loopback upstreams and do not rely on public DNS answers.

## Planned scope

- Explicit emergency-profile product policy and health-driven scheduling.
- Licensed third-party rule import and additional filtering response modes.
- DoT pipelining and alternate HTTP/3 endpoint discovery.
- Measured public-service capacity, network abuse protection and deployment/rollback acceptance.
- Zero-downtime full configuration replacement; console application currently restarts DNS.

The initial design focuses on forwarding to existing resolvers. A standalone
iterative resolver is outside the initial scope. The implementation uses Rust,
Tokio for asynchronous IO, and Hickory for DNS message parsing and encoding.
