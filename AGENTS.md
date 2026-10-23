# Working on PariNS

## Scope and workflow

PariNS is a self-hosted DNS forwarding, filtering, caching, and management service
with optional validating recursive resolution (`[resolution] mode = "recursive"`).
Recursion builds on the pinned hickory recursor fork; do not grow it into a general
recursive server (RPZ, DNS64, local zones, forward/recursive mixing) without an
explicit design decision.

- Read the relevant implementation, tests and module `//!` documentation before
  changing behavior. Trace the request flow and identify each affected contract's owner.
- Preserve unrelated working-tree and staged changes. Make focused changes and
  stage explicit paths; do not include another task's work in a commit.
- Committing, pushing, releasing and deploying are separate steps.

## Design principles

- Target the current design, not compatibility with older PariNS versions: no legacy
  aliases, API adapters, parallel old/new implementations, automatic migrations, or
  silent legacy fallbacks. When replacing a contract, update callers, UI, examples
  and tests together, remove the superseded path, document required user actions,
  and reject obsolete input clearly. DNS/transport standards, protocol negotiation
  and their operational fallbacks still apply.
- Prefer existing mechanisms. Validate at the owning trust boundary and rely on
  established internal contracts; add a guard, retry, fallback or abstraction only
  for a named requirement or reachable failure. Preserve DNS correctness,
  authentication, TLS verification, resource bounds and persistence integrity.

## Ownership

| Area | Responsibility |
| --- | --- |
| `src/config.rs` | Authoritative configuration schema, defaults, and validation. |
| `src/protocol.rs`, `src/ecs.rs` | DNS protocol checks, ECS validation, scope, and response normalization. |
| `src/cache.rs`, `src/cache/` | Eligibility, semantic keys, TTLs, budgets, eviction, policy matching, inspection, invalidation, snapshot codec. No network IO. |
| `src/cache_persistence.rs` | One-use clean snapshot file lifecycle; only the terminal shutdown owner saves. |
| `src/resolver.rs`, `src/resolver/`, `src/flight.rs` | Resolution orchestration, one upstream deadline, shared upstream work, bounded refresh, stale fallback. |
| `src/upstream.rs`, `src/upstreams.rs`, `src/upstreams/` | Upstream exchange, explicit bootstrap, weighted/parallel scheduling, H2/H3 connections, attempt diagnostics; preserve resolver deadlines and cancellation. |
| `src/recursion.rs`, `src/recursion/` | Recursive source: the pinned hickory recursor (DNSSEC validation, relaxed QNAME minimisation, infrastructure caches), result mapping (Bogus to SERVFAIL + EDE 6, AD, authority ECS scope), global outbound rate and concurrency bounds. The Resolver keeps the deadline, cache, filtering and stale fallback. |
| `src/server.rs`, `src/transport/` | Listener lifecycle, UDP/TCP framing, local reply addresses, connection limits, shutdown. |
| `src/ingress.rs` | Shared admission and DNS handling for encrypted transport adapters. |
| `src/tls.rs`, `src/tls/` | Verified TLS configuration, DoT adapters and pool, the inbound CertificateSet. |
| `src/doh.rs`, `src/quic.rs`, `src/quic/` | DoH (HTTP/2), DoQ and HTTP/3 listeners, including Quinn driver and socket lifecycle. |
| `src/policy.rs`, `src/policy/`, `src/limits.rs` | Filtering decisions, rule parsing and the radix index; source budgets keyed by peer subnet. |
| `src/metrics.rs`, `src/admin.rs` | Fixed-cardinality process metrics; read-only loopback `/metrics` and `/healthz` with their own small connection budget. |
| `src/runtime_services.rs`, `src/runtime_health.rs`, `src/runtime_lifecycle.rs` | Metrics and storage across DNS generations, generation-scoped DNS health, startup/terminal boundaries for both modes. |
| `src/storage/`, `src/query_log.rs` | Private runtime SQLite with one owner worker: statistics and opt-in bounded per-request history with clear epochs; never an aggregate metrics label store. |
| `src/manage/` | Authenticated management APIs, sessions and accounts, sign-in Turnstile, configuration transactions, persistence, managed runtime ownership. |
| `src/update/`, `src/bin/` | Official release/build contracts and bounded HTTPS reader; the fixed root executor owns its installation journal, never DNS or business data. |
| `src/filter_subscriptions/` | Bounded subscription work, private source catalog/index, immutable policy generations. Manager/file runtime own final configuration, freeze, reload and shutdown gates. |
| `src/https_reader.rs`, `src/private_files.rs` | Pinned HTTPS transport and private-file checks; callers keep authorization, transactions and lifecycle. |
| `web/` | Embedded UI, forms, drafts, and presentation. No independent DNS policy engine or authoritative configuration state. |
| `scripts/`, `deploy/` | Installation, packaging, and service lifecycle; not DNS business logic. |

UI validation helps users, but Rust validation is authoritative; reuse protocol and
configuration helpers across file mode, management APIs, and the browser.
Console translations and field metadata live in typed modules under `web/src/i18n/`
and `web/src/model/`. React owns only presentation and one in-memory configuration
draft. Language and appearance changes preserve controls, drafts, focus and session
ownership; persist only language, appearance (theme, background) and the query log
refresh interval in the browser, never credentials, session bindings, PEM, logs or drafts. Keep both languages complete; configuration values and raw
diagnostics stay intact. Incomplete numeric and listener edits remain raw drafts
until preview/validation.

## DNS and cache invariants

- Preserve complete query semantics when caching or coalescing. ECS families,
  subnets, no-ECS traffic and privacy fallback never share answers; respect response
  scope. Missing ECS in a response to nonzero outgoing ECS defaults to ExactSource
  reuse. Only when one cache key received the same normalized answer from at least
  K disjoint networks (spanning two or more /16, IPv6 /32) with no contradiction may
  `[cache.ecs_sharing]` learn sharing within that ECS family; any different answer or
  explicit nonzero scope revokes it at once, with a cooldown. No-ECS, privacy
  fallback and the other family never share. Details: `src/cache/sharing.rs`.
  These rules describe upstream recursive resolvers. In recursive mode the scope
  comes from authoritative servers: an answer without an ECS option does not depend
  on the client network and is shared within its family (echoed as scope 0); learned
  sharing never applies. Forwarded and recursive answers never share a cache or
  snapshot.
- AD is answered only for Secure data from the validating recursor, only to clients
  that set DO or AD, and never after filtering rewrote the answer. The cache keeps
  the source's AD bit with the entry and never replays it as stored.
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
- Padding is stripped from cache and regenerated at the final encrypted hop, bounded
  by the maximum DNS message size, never the EDNS UDP size. EDNS TCP keepalive is
  hop-local: ingress removes it from queries and the pool from answers. Upstream
  queries advertise at most 1232 bytes. Cookie/unknown options stay nonshared. Flight and refresh share one canonical key
  that ignores Padding bytes but preserves request intent.
- Inspection and explanation are read-only: no upstream requests, recency or
  hit-counter changes. Filtering never contaminates cached upstream answers.
- One aggregate radix policy serves local and subscription rules; Config keeps local
  rules, not an index. Each request holds its policy generation across cache, CNAME
  and stale handling. Preserve semantic cache restoration checks.
- A blocked name is answered with the method of its most specific block rule (local
  exact, local suffix, subscription exact, subscription suffix; longest suffix;
  configured source order); slots carry the methods, and the index keeps a covered
  block rule only when it can answer differently. Block answers carry EDE 15 for
  EDNS clients. Details: `src/policy.rs`, `src/policy/response.rs`.
- Cache byte accounting is not process RSS. Keep separate positive and negative
  budgets; document capacity/utilization trade-offs when changing them.
- Consume a clean cache snapshot durably before serving; publish one only at
  terminal, quiescent shutdown, never after apply/rollback or abnormal termination.
  Save and restore run most recently used first and stop at their deadline; a save
  publishes a complete snapshot of what it wrote, a restore keeps what it loaded,
  and a damaged snapshot loads nothing.

## Concurrency, listeners and upstreams

- Keep shared locks short; never hold a cache lock during network IO, and
  reconstruct responses outside shard locks. Bound queues,
  cache state, background work and total shutdown. Every spawned task has an owner
  and explicit cancellation; refresh and foreground coalescing never cross generations.
- Shutdown: only streams with a complete query can force a stop; others are
  cancelled at once. Peer close notification uses at most half the grace; a forced
  stop records its first bounded cause and logs it when no clean snapshot is
  published. Details: `src/quic.rs`, `src/doh.rs`.
- UDP replies use the datagram's local destination and interface (packet info on
  Linux/macOS); other platforms reject wildcard binds. Resolver and cache never
  select addresses.
- `[upstreams]` is the only upstream configuration; DoT pool controls are under
  `[upstreams.dot_pool]`. Pool endpoints need equivalent policies, explicit hostname
  bootstrap and authenticated encrypted transports. Parallel losers cancel with the
  caller; failed DNS responses never preempt usable answers.
- Recursion admits every authoritative query attempt through one global rate bucket
  and concurrency limit before the server's attempt timeout starts, so queueing never
  counts against or penalizes a server; a token is taken only when it is used, and
  shutdown closes them so waiting queries fail. Each resolution stays within the
  Resolver deadline.
  Recursion failures are upstream failures for stale fallback, except DNSSEC Bogus,
  which is always answered as SERVFAIL with EDE 6.
- H3 preference applies only to HTTPS endpoints within the resolver deadline. Only
  an H3 failure before the query was sent falls back to verified H2; a failure after
  sending is that query's upstream failure. Any H3 failure starts a bounded cooldown.
  A hostname endpoint tries its next address only when nothing was sent. A sent
  query is never resent or replayed, with one exception: when a reused idle DoT pool
  connection turns out to be closed by the server (EOF, reset, aborted, broken pipe
  or not connected), the query is retried once on a new connection within its
  deadline; cancelling a request releases only its stream. DoH transports normalize
  HTTP Age into RR TTLs; cache code has no HTTP policy. Details:
  `src/upstreams/h3.rs`, `src/upstreams/transport.rs`.
- DNS terminal health is published once per generation to readiness and storage
  sampling. Attempt diagnostics use bounded slots and finite typed dimensions
  (endpoint labels and reached addresses only in the authenticated status API, never
  in query logs or metric labels);
  cancellation/shutdown are not upstream faults. Management API 200 is not DNS readiness.
- Server supervises each listener separately in both modes: a bind failure or an
  adapter exit rebinds only that listener (capped exponential backoff, rate-limited
  logs) while the others, the cache and the upstream pool keep serving. Health is
  ready only with every listener up, otherwise degraded and not ready; an adapter's
  drain forces a stop (and the upstream pool down) only when the generation is
  stopping. Per-connection accept errors keep a listener (and the management
  listener) serving, and descriptor exhaustion pauses it briefly. QUIC answers
  unvalidated Initials with a stateless Retry; only validated connections take the
  global slot and source budget. DoH advertises Alt-Svc h3 only while DoH3 serves.
  Supervisors belong to the generation and stop with it. An applied configuration
  must bind every listener before it is persisted; only (re)starts of the saved
  configuration run degraded. A generation that fails
  to start for another reason is retried by one process-owned managed task that
  yields to mutations and restarts only the saved revision. Saved local rules that
  fail to compile keep DNS stopped on every start path (startup, retry, apply
  rollback); only an applied configuration whose rules compile starts it. Details:
  `src/server.rs`, `src/manage/dns_retry.rs`.

## Management, runtime and security

- Mutations keep authentication, Host/Origin checks, atomic persistence and failure
  recovery. Configuration saves and rollback carry the saved revision as an
  optimistic-concurrency check; the console keeps it internal, never shows it, and on
  a conflict reloads while keeping the draft (non-overlapping edits merge, otherwise
  the administrator chooses). Other operations (subscription work, certificate
  reload and import, cache invalidation, storage clears, software updates) act on
  the server's current saved state and take no client revision; epochs still guard
  data the administrator saw. Attempt budgets use the socket peer, never forwarding
  headers. Details: `src/manage/auth_budget.rs`.
- Sessions are HttpOnly, SameSite=Strict Cookies plus an in-memory binding on
  protected APIs: `parins_session_http` (HTTP) or Secure `__Host-parins_session`
  (HTTPS). Logout revokes only its own server-side session, without clearing the
  Cookie. Never persist or broadcast the binding. An unknown mutation result is
  not proof of failure: the console confirms it by reading server state and never
  replays it.
- Credential changes are separate Store transactions that revoke all sessions and
  require explicit login. Details: `src/manage/account.rs`.
- Turnstile is optional; its keys live only in the private `turnstile.json`, and
  the secret never enters the TOML, logs, API responses or exports. Sign-in checks
  it (siteverify through the pinned HTTPS reader: success, host name, `login`
  action) before the password, charged to the peer's attempt budget (IPv6 peers by
  /64); failure refuses. Only a loopback socket peer skips it (a same-host proxy or
  tunnel makes everyone loopback; documented and warned in the console). Cloudflare
  test keys only as a pair. The relaxed CSP for its script and frame applies only to
  a signed-out console document that shows it. Setting or clearing the keys needs
  the current password, revokes all sessions and voids sign-ins admitted under the
  old setting. Details: `src/manage/turnstile.rs`.
- The live query log WebSocket (`/api/query-log/live`) belongs to the management
  process: Host/Origin (Origin required) and session Cookie are checked before the
  upgrade; the binding arrives only as the first message. Sockets leave the HTTP
  connection lifetime but stay bounded per session and in total, with a small
  inbound message limit and an idle timeout. Removing the session from memory
  closes them (4403), and shutdown closes them (1001) within its drain. Polls run
  on the storage worker; Manager is never held across socket IO. Details:
  `src/manage/live.rs`.
- Validate and persist before publishing replacement state. Cache-only, storage-only
  and updates-only changes preserve listeners; updates-only changes also keep caches,
  RuntimeServices, certificates and sessions and leave stopped DNS stopped. One
  updates-only classification drives validation, restart impact and apply, keeping
  schema validation without reloading unchanged DNS material. Do not promise cache
  retention after arbitrary policy changes; report restart requirements accurately.
- RuntimeServices owns process metrics and the bounded SQLite worker across DNS
  generations. Logs, totals and trends have independent durable epochs; status is a
  cached observation, not a hot-path disk query. Cache/storage-only applies never
  publish candidate certificate changes. The worker opens and integrity-checks an
  existing database in the background (`checking`), so DNS startup never waits for
  it; only shutdown interrupts the check, and writes meanwhile use the bounded queue.
  Cleanup runs per `storage.cleanup_interval_secs`, after a write over a capacity
  bound, and on explicit flush or shutdown. At terminal shutdown the cache save and
  the storage drain run in parallel with separate budgets; storage closes (releasing
  the runtime lock) only after the save.
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
  journal is the sole installed anchor. Root records an application-initiated Abort
  as `aborted`; the application keeps its own abort reason. Details: `src/update/executor/mod.rs`.
- Managed `--check` is read-only and never opens Store/runtime, SQLite, cache
  persistence or listeners; it also validates the saved rollback copy. Candidate preflight runs as the app user; root never
  executes the candidate. The preflight compares only digests of version-independent
  inputs (saved revision and TOML, rule files, selected subscription content and
  catalog revision, certificates, upstream CA), never encodings a version derives,
  and logs which categories differ.
- The installer stops an active `parins-updater.path` right after taking its lock and
  restores it on rollback or enables it after commit; a post-commit enable failure
  is reported without rollback.
- Subscription content is bound to verified validators and selected through a
  durable catalog, never reconstructed by scanning objects. Status GETs do no IO;
  download/compile runs outside Manager and mutation locks and is never awaited under
  Manager. Details: `src/filter_subscriptions/mod.rs`.
- A process started frozen never opens the subscription Store; reconcile is the only
  thaw, and the update loop then opens it once (outside Manager, under the mutation
  permit). Any other component that defers work during a freeze must resume after it.

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
- For performance changes, keep a baseline, repeat the same workload with
  correctness checks (`examples/bench_cache.rs` for the cache), and report
  regressions and variance; cache operations per second are not wire DNS QPS.

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
- When guidance and code disagree, investigate and report the mismatch before
  changing either.
