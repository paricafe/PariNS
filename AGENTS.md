# Working on PariNS

## Scope and workflow

PariNS is a self-hosted DNS forwarding, filtering, caching, and management service.
Do not expand it into a full recursive resolver without an explicit design decision.

- Read the relevant implementation, tests and module `//!` documentation before
  changing behavior. Trace the request flow and identify each affected contract's owner.
- Preserve unrelated working-tree and staged changes. Make focused changes and
  stage explicit paths; do not include another task's work in a commit.
- Reviews, explanations, and plans do not authorize implementation. Reuse existing
  authorization, but distinguish committing, pushing, releasing, and deploying.

## Design principles

- Target the current design, not compatibility with older PariNS versions: no legacy
  aliases, API adapters, parallel old/new implementations, automatic migrations, or
  silent legacy fallbacks. When replacing a contract, update callers, UI, examples
  and tests together, remove the superseded path, document required user actions,
  and reject obsolete input clearly. This never permits breaking supported
  DNS/transport standards or required protocol negotiation and operational fallbacks.
- Prefer existing mechanisms. Before adding validation, guards, retries, fallbacks
  or abstractions, name the concrete requirement or reachable failure, why existing
  guarantees are insufficient, and the simplest complete fix.
- Validate at the owning trust boundary and rely on established internal contracts;
  avoid repeated checks, catch-all suppression and defaults that conceal broken
  contracts. Preserve DNS correctness, authentication, TLS verification, resource
  bounds and persistence integrity. These rules apply to the requested change, not
  to unrelated repository-wide cleanup.

## Ownership

| Area | Responsibility |
| --- | --- |
| `src/config.rs` | Authoritative configuration schema, defaults, and validation. |
| `src/protocol.rs`, `src/ecs.rs` | DNS protocol checks, ECS validation, scope, and response normalization. |
| `src/cache.rs`, `src/cache/` | Eligibility, semantic keys, TTLs, budgets, eviction, policy matching, inspection, invalidation, snapshot codec. No network IO. |
| `src/cache_persistence.rs` | One-use clean snapshot file lifecycle; only the terminal shutdown owner saves. |
| `src/resolver.rs`, `src/resolver/`, `src/flight.rs` | Resolution orchestration, one upstream deadline, shared upstream work, bounded refresh, stale fallback. |
| `src/upstream.rs`, `src/upstreams.rs`, `src/upstreams/` | Upstream exchange, explicit bootstrap, weighted/parallel scheduling, H2/H3 connections, attempt diagnostics; preserve resolver deadlines and cancellation. |
| `src/server.rs`, `src/transport/` | Listener lifecycle, UDP/TCP framing, local reply addresses, connection limits, shutdown. |
| `src/ingress.rs` | Shared admission and DNS handling for encrypted transport adapters. |
| `src/tls.rs`, `src/tls/` | Verified TLS configuration, DoT adapters and pool, the inbound CertificateSet. |
| `src/doh.rs`, `src/quic.rs`, `src/quic/` | DoH (HTTP/2), DoQ and HTTP/3 listeners, including Quinn driver and socket lifecycle. |
| `src/policy.rs`, `src/policy/`, `src/limits.rs` | Filtering decisions, rule parsing and the radix index; source budgets keyed by peer subnet. |
| `src/metrics.rs`, `src/admin.rs` | Fixed-cardinality process metrics; read-only loopback `/metrics` and `/healthz`. |
| `src/runtime_services.rs`, `src/runtime_health.rs`, `src/runtime_lifecycle.rs` | Metrics and storage across DNS generations, generation-scoped DNS health, startup/terminal boundaries for both modes. |
| `src/storage/`, `src/query_log.rs` | Private runtime SQLite with one owner worker: statistics and opt-in bounded per-request history with clear epochs; never an aggregate metrics label store. |
| `src/manage/` | Authenticated management APIs, sessions and accounts, configuration transactions, persistence, managed runtime ownership. |
| `src/update/`, `src/bin/` | Official release/build contracts and bounded HTTPS reader; the fixed root executor owns its installation journal, never DNS or business data. |
| `src/filter_subscriptions/` | Bounded subscription work, private source catalog/index, immutable policy generations. Manager/file runtime own final configuration, freeze, reload and shutdown gates. |
| `src/https_reader.rs`, `src/private_files.rs` | Pinned HTTPS transport and private-file checks; callers keep authorization, transactions and lifecycle. |
| `web/` | Embedded UI, forms, drafts, and presentation. No independent DNS policy engine or authoritative configuration state. |
| `scripts/`, `deploy/` | Installation, packaging, and service lifecycle; not DNS business logic. |

UI validation helps users, but Rust validation is authoritative; reuse protocol and
configuration helpers across file mode, management APIs, and the browser.
Console translations and field metadata live in typed modules under `web/src/i18n/`
and `web/src/model/`. React owns only presentation and one in-memory configuration
draft. Language and theme changes preserve controls, drafts, focus and session
ownership; persist only those two preferences, never credentials, session bindings,
PEM, logs or drafts. Keep both languages complete; configuration values and raw
diagnostics stay intact. Incomplete numeric and listener edits remain raw drafts
until preview/validation.

## DNS and cache invariants

- Preserve complete query semantics when caching or coalescing. ECS families,
  subnets, no-ECS traffic and privacy fallback never share answers; respect response
  scope. Missing ECS in a response to nonzero outgoing ECS uses only ExactSource reuse.
- Match DNS names by labels, including escaped labels, never presentation suffixes.
- Preserve positive and negative eligibility and TTL rules. Fresh lifetime, stale
  retention and stale reply TTL are separate; never silently extend TTLs. Stale
  serving applies only to supported upstream failures, never from negative answers
  or another privacy namespace.
- Classify responses once for admission and snapshot restoration. A nonempty negative
  answer needs an unambiguous same-class CNAME chain and equivalent covering SOAs for
  its terminal name; it keeps the question, ECS namespace, negative budget and
  bounded TTL, and is never stale or prefetch eligible.
- Invalidation and replacement stop older in-flight work from refilling invalidated
  state. A newer successful answer, even uncacheable or carrying known EDE
  diagnostics (not admitted for replay), supersedes older overlapping answers.
- Padding is stripped from cache and regenerated at the final encrypted hop.
  Cookie/unknown options stay nonshared. Flight and refresh share one canonical key
  that ignores Padding bytes but preserves request intent.
- Inspection and explanation are read-only: no upstream requests, recency or
  hit-counter changes. Filtering never contaminates cached upstream answers.
- One aggregate radix policy serves local and subscription rules; Config keeps local
  rules, not an index. Each request holds its policy generation across cache, CNAME
  and stale handling. Preserve semantic cache restoration checks.
- Cache byte accounting is not process RSS. Keep separate positive and negative
  budgets; document capacity/utilization trade-offs when changing them.
- Consume a clean cache snapshot durably before serving; publish one only at
  terminal, quiescent shutdown, never after apply/rollback or abnormal termination.

## Concurrency, listeners and upstreams

- Keep shared locks short; never hold a cache lock during network IO, and
  reconstruct responses outside shard locks. Bound queues,
  cache state, background work and total shutdown. Every spawned task has an owner
  and explicit cancellation; refresh and foreground coalescing never cross generations.
- Shutdown: only accepted request streams can force a stop; peer close notification
  uses at most half the grace; a forced stop records its first bounded cause and logs
  it when no clean snapshot is published. Details: `src/quic.rs`.
- UDP replies use the datagram's local destination and interface (packet info on
  Linux/macOS); other platforms reject wildcard binds. Resolver and cache never
  select addresses.
- `[upstreams]` is the only upstream configuration; DoT pool controls are under
  `[upstreams.dot_pool]`. Pool endpoints need equivalent policies, explicit hostname
  bootstrap and authenticated encrypted transports. Parallel losers cancel with the
  caller; failed DNS responses never preempt usable answers.
- H3 preference applies only to HTTPS endpoints within the resolver deadline, with
  verified H2 fallback and bounded cooldown. A sent query is never resent or
  replayed; cancelling a request releases only its stream. DoH transports normalize
  HTTP Age into RR TTLs; cache code has no HTTP policy. Details:
  `src/upstreams/h3.rs`, `src/upstreams/transport.rs`.
- DNS terminal health is published once per generation to readiness and storage
  sampling. Attempt diagnostics use bounded slots and finite typed dimensions;
  cancellation/shutdown are not upstream faults. Management API 200 is not DNS readiness.

## Management, runtime and security

- Mutations keep authentication, Host/Origin and revision/epoch checks, atomic
  persistence and failure recovery. Attempt budgets use the socket peer, never
  forwarding headers. Details: `src/manage/auth_budget.rs`.
- Sessions are HttpOnly, SameSite=Strict Cookies plus an in-memory binding on
  protected APIs: `parins_session_http` (HTTP) or Secure `__Host-parins_session`
  (HTTPS). Logout revokes only its own server-side session, without clearing the
  Cookie. Never persist or broadcast the binding. An unknown configuration result
  is not proof of failure and is never silently replayed.
- Credential changes are separate Store transactions that revoke all sessions and
  require explicit login. Details: `src/manage/account.rs`.
- Validate and persist before publishing replacement state. Cache-only, storage-only
  and updates-only changes preserve listeners; updates-only changes also keep caches,
  RuntimeServices, certificates and sessions and leave stopped DNS stopped. One
  updates-only classification drives validation, restart impact and apply, keeping
  schema validation without reloading unchanged DNS material. Do not promise cache
  retention after arbitrary policy changes; report restart requirements accurately.
- RuntimeServices owns process metrics and the bounded SQLite worker across DNS
  generations. Logs, totals and trends have independent durable epochs; status is a
  cached observation, not a hot-path disk query. Cache/storage-only applies never
  publish candidate certificate changes.
- Management uses HTTP on its one listener until inbound DoH is enabled, which
  selects the validated DoH identity for HTTPS; `[doh].http3` shares the DoH address
  and actual port over UDP. Management and DNS keep separate ports, ALPN and routes.
  A failed candidate or TLS fault never downgrades HTTPS to HTTP; disabling DoH needs
  downgrade confirmation. Rust owns `[web].public_host`, origin and protocol state.
- Manager/Active owns the atomic CertificateSet, reloaded by API or SIGHUP without
  changing revision, DNS/cache ownership or sessions. Pasted identities are validated
  before private persistence; only file references are returned, never private keys.
  Details: `src/tls/certificates.rs`.
- Preserve configured HTTPS and certificate verification. Never commit credentials,
  keys, runtime state or private configuration. No sensitive logging by default;
  query/client data needs explicit privacy and retention decisions.

## Updates and subscriptions

- Software checks belong to one managed-process scheduler after setup, never a DNS
  generation or API GET; file mode has none.
- Update commit intent is persisted before a freeze and the fixed inbox request. The
  freeze is checked under the existing mutation permit, released before root waits
  for normal SIGTERM. Unknown helper outcomes never unlock by timeout. The root
  journal is the sole installed anchor. Details: `src/update/executor/mod.rs`.
- Managed `--check` is read-only and never opens Store/runtime, SQLite, cache
  persistence or listeners. Candidate preflight runs as the app user; root never
  executes the candidate.
- The installer stops an active `parins-updater.path` right after taking its lock and
  restores it on rollback or enables it after commit; a post-commit enable failure
  is reported without rollback.
- Subscription content is bound to verified validators and selected through a
  durable catalog, never reconstructed by scanning objects. Status GETs do no IO;
  download/compile runs outside Manager and mutation locks and is never awaited under
  Manager. Details: `src/filter_subscriptions/mod.rs`.

## Verification

Use the toolchain in `rust-toolchain.toml` and locked dependencies;
`.github/workflows/ci.yml` is the full CI gate. Run the smallest checks covering the
change plus required gates, and stop when they pass. Add tests or scripts only for a
meaningful behavior or regression gap.

| Change | Checks |
| --- | --- |
| Rust | `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked --all-targets` |
| Build/configuration | `cargo build --locked --release`, `./target/release/parins --config parins.example.toml --check` |
| Web | In `web/`: `npm ci --ignore-scripts`, `npm run typecheck`, `npm test`, `npm run build` before compiling Rust (the binary embeds `web/dist/`); `npm audit --registry https://registry.npmjs.org --audit-level=high`. For interaction changes, test the flow in a real browser, including errors, session transitions, keyboard use and small screens. |
| Installer/bootstrap | `sh scripts/test-install.sh`, `sh scripts/test-bootstrap.sh`; see [installer contracts](guide/development.md#installer-and-bootstrap-checks) |
| Service, UDP, subscriptions, updates | `--ephemeral-ci` suites in [Acceptance workflows](guide/development.md#acceptance-workflows), only on disposable runners |
| Dependencies | The dependency audit configured in CI |
| Documentation only | Referenced paths, commands and links; review the diff; `git diff --check` |

- Keep every `--ephemeral-ci` guard. Tests never change host DNS, firewall rules,
  certificate trust or system services; use isolated fixtures for local acceptance.
- A check proves only its declared fixture and scope. Local, CI, isolated or
  development-build results do not establish production capacity, public
  reachability, official upgrade/rollback, or behavior on a particular host.
- For performance changes, keep a baseline and repeat the same workload with
  correctness checks; use `examples/bench_cache.rs` for the cache and report
  regressions and variance. Cache operations per second are not wire DNS QPS. Never
  remove safety checks or add complex optimizations for a microbenchmark score.

## Documentation and delivery

- Write focused English commit messages, such as `docs: clarify cache ownership`.
  Report what changed, what was verified, and remaining limitations.
- Keep `README.md` to the overview, features, quick start and license. Public guides
  live in `guide/`; release and compatibility notes in `CHANGELOG.md`. Distinguish
  unreleased main changes from releases.
- `docs/` holds intentionally ignored local plans; never force-add it. Essential
  contributor rules stay in tracked files.
- Update this file and the affected module documentation in the same change that
  alters an ownership boundary, durable invariant, or workflow. Keep it concise and
  repository-portable: no personal paths, task history, test counts, temporary
  metrics, or copied defaults.
- When guidance conflicts with code or tests, investigate and explain the mismatch;
  do not silently weaken a contract or preserve an obsolete design by habit.
