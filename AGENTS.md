# Working on PariNS

## Scope and workflow

PariNS is a self-hosted DNS forwarding, filtering, caching, and management service.
Do not expand it into a full recursive resolver without an explicit design decision.

- Read the relevant implementation and tests before changing behavior. Trace the
  request flow and identify the owner of each affected contract.
- Preserve unrelated working-tree and staged changes. Make focused changes and
  stage explicit paths; do not include another task's work in a commit.
- Reviews, explanations, and plans do not authorize implementation. Reuse existing
  authorization, but distinguish committing, pushing, releasing, and deploying.

## Current-version design and proportional safeguards

- Target the current PariNS design, not backward compatibility with older PariNS
  versions. Do not retain or add legacy configuration aliases, API adapters,
  parallel old/new implementations, automatic migrations, or silent legacy fallbacks.
  When replacing a contract, update its callers, UI, examples, and tests together;
  remove superseded paths within that change and document required user actions.
  Reject obsolete input clearly rather than guessing how to translate it.
- This is a project-version policy, not permission to break supported DNS/transport
  standards or remove required protocol negotiation and operational fallbacks.
- Prefer existing mechanisms and dependencies. Before adding validation, guards,
  retries, fallbacks, or abstractions, identify the concrete requirement or reachable
  failure, explain why existing guarantees are insufficient, and choose the simplest
  complete solution. Do not build protection for hypothetical future scenarios.
- Validate at the owning trust boundary and rely on established internal contracts.
  Avoid repeated checks across layers, catch-all error suppression, and defensive
  defaults that conceal broken contracts. Preserve necessary DNS correctness,
  authentication, TLS verification, resource bounds, and persistence integrity.
- Apply these rules to the requested change; they do not authorize unrelated
  compatibility removal or a repository-wide defensive-code cleanup.

## Ownership

| Area | Responsibility |
| --- | --- |
| `src/config.rs` | Authoritative configuration schema, defaults, and validation. |
| `src/protocol.rs`, `src/ecs.rs` | DNS protocol checks, ECS validation, scope, and response normalization. |
| `src/cache.rs` | Cache eligibility, semantic keys, TTLs, budgets, eviction, policy matching, inspection, and invalidation. No network IO. |
| `src/resolver.rs`, `src/flight.rs`, `src/resolver/refresh.rs` | Resolution orchestration, shared upstream work, bounded refresh, and stale fallback. |
| `src/upstream.rs`, `src/upstreams/`, `src/upstreams.rs` | Upstream protocol exchange, explicit bootstrap, and weighted/parallel scheduling; preserve resolver deadlines and cancellation. |
| `src/query_log.rs` | Opt-in bounded per-request history, retention, pagination and clear epochs; never an aggregate metrics label store. |
| `src/server.rs` and listener/transport modules | Listener lifecycle, transport framing, connection limits, and shutdown. |
| `src/policy.rs`, `src/limits.rs` | Filtering decisions and source resource budgets, respectively. |
| `src/manage/` | Authenticated management APIs, configuration transactions, persistence, and managed runtime ownership. |
| `src/update/` | Official release/build contracts and bounded HTTPS reader; the fixed root executor owns its installation journal, never DNS or business data. |
| `src/filter_subscriptions/` | Process-owned bounded subscription work, private source catalog/index, and immutable policy generations. Manager/file runtime own final configuration, freeze, reload and shutdown gates. |
| `src/https_reader.rs`, `src/private_files.rs` | Shared pinned HTTPS transport and private-file checks; callers retain source authorization, transactions, and lifecycle ownership. |
| `web/` | Embedded UI, forms, drafts, and presentation. No independent DNS policy engine or authoritative configuration state. |
| `scripts/`, `deploy/` | Installation, packaging, and service lifecycle; not DNS business logic. |

Keep checks at the boundary that owns them. UI validation helps users, but Rust
validation remains authoritative. Reuse protocol and configuration helpers rather
than implementing different rules for file mode, management APIs, and the browser.

Console translations and field metadata live in typed modules under `web/src/i18n/`
and `web/src/model/`. React owns only presentation and one in-memory configuration
draft; Rust owns parsing, validation, and apply. Language and theme changes must
preserve controls, drafts, focus and session ownership. Persist only language and
theme preferences, never credentials, session bindings, PEM, logs or drafts.
Keep both languages complete; configuration values and raw diagnostics stay intact.
Incomplete numeric and listener edits remain raw drafts until preview/validation.

## DNS and cache invariants

- Preserve complete query semantics when caching or coalescing. ECS address
  families, subnets, no-ECS traffic, and privacy fallback must not leak answers
  across incompatible namespaces. Respect response scope when reusing entries.
- Match DNS names by labels, including escaped labels; presentation-string suffix
  matching is not a substitute for DNS name semantics.
- Preserve positive and negative eligibility and TTL rules. Fresh lifetime, stale
  retention, and stale reply TTL are separate concepts; do not silently extend TTLs.
- Keep stale serving conditional on the supported upstream failures. Do not use
  negative answers or a different privacy namespace as a stale fallback.
- Invalidation and cache replacement must prevent older in-flight work from
  refilling invalidated state. Newer successful answers must not resurrect older
  overlapping answers merely because the newer answer cannot be cached.
  Known EDE diagnostics may establish supersession without being admitted for
  cache replay; client-specific options retain their existing isolation.
- Inspection and explanation are read-only: no upstream requests, recency changes,
  or hit-counter updates. Filtering must not contaminate cached upstream answers.

## Concurrency, configuration, and security

- Keep shared locks short. Never hold a cache lock during network IO; reconstruct
  responses outside shard locks. Bound queues, cache state, and background work.
- Give spawned tasks an owner and explicit shutdown/cancellation behavior. Refresh
  and foreground coalescing must share compatible work without crossing generations.
- QUIC listener ownership includes Quinn drivers and final socket release. Server
  retains cleanup across adapter cancellation; peer close notification stays within
  the existing shutdown grace, and a completed shutdown permits immediate rebinding.
- Keep cache byte accounting distinct from process RSS. Preserve separate positive
  and negative budgets; document capacity/utilization trade-offs when changing them.
- Management mutations must retain authentication, Host/Origin checks, and relevant
  revision/epoch checks. Preserve atomic persistence and failure recovery.
- Management sessions use HttpOnly, SameSite=Strict Cookies and an in-memory
  binding on protected APIs: `parins_session_http` for HTTP, or Secure
  `__Host-parins_session` for HTTPS. No Bearer compatibility. Logout revokes only
  its specified server-side session, without clearing the Cookie or relying on
  Web Locks for cross-tab ordering. Never persist or broadcast the binding.
  An unknown configuration result is not proof of failure and must not be
  silently replayed.
- Authentication attempt budgets use the socket peer, never forwarding headers.
  Bound and reclaim source state separately from global password-hash concurrency;
  blocking hash work owns its permit until it finishes, even after caller cancellation.
- Cache-only configuration changes should not restart DNS listeners. Validate and
  persist before publishing replacement state; do not promise cache retention after
  arbitrary policy changes. Report restart requirements accurately for other changes.
- Management defaults to HTTP on its one listener; enabling inbound DoH selects
  its validated identity for management HTTPS. `[doh].http3` shares the DoH TCP
  address and actual port over UDP; there is no independent `[doh3]` configuration.
  Share the active CertificateSet's DoH slot, not DNS TLS routing/ALPN.
  A failed candidate or TLS fault never silently downgrades HTTPS to HTTP;
  explicit DoH disablement needs downgrade confirmation. Rust owns validated
  `[web].public_host`, management origin and protocol transaction state.
- Preserve configured HTTPS and certificate verification. Never commit credentials, private
  keys, runtime state, or private configuration. Do not add sensitive logging by
  default; query/client data needs explicit privacy and retention decisions.
- Pasted DNS identities must be validated before private persistence; return only
  file references, never private keys. Management and DNS listeners have separate
  ports, ALPN and routes even when they reuse one validated inbound DoH identity.
- Pool endpoints share cache semantics: require equivalent policies, explicit
  hostname bootstrap and authenticated encrypted transports. Parallel losers must
  cancel with the caller; failed DNS responses must not preempt usable answers.
- `[upstreams]` is the only upstream configuration. Do not reintroduce retired
  single-upstream fields, delayed-replica scheduling, compatibility adapters or
  automatic migrations. DoT pool controls belong to `[upstreams.dot_pool]`.
- H3 preference applies only to HTTPS endpoints, within the resolver deadline.
  Preserve verified H2 fallback, bounded cooldown and reusable connection ownership;
  cancelling a request must release its stream without breaking other requests.
- DoH transports normalize HTTP Age into RR TTLs before handing answers to Resolver;
  cache code has no HTTP policy. DoQ in-flight requests retain their endpoint owner
  independently of the current bootstrap-address cache entry.
- Missing ECS in a response to nonzero outgoing ECS uses only ExactSource reuse;
  Padding is stripped from cache and regenerated at the final encrypted hop.
  Cookie/unknown options keep their nonshared boundary. Flight and refresh use
  one canonical key that ignores Padding bytes while preserving request intent.
- RuntimeServices owns process metrics and a bounded SQLite worker across DNS
  generations. Logs, totals and trends have independent durable epochs; status
  is a cached observation, not a hot-path disk query. Cache/storage-only applies
  must preserve listeners and may not publish candidate certificate changes.
- Consume a clean cache snapshot durably before serving. Publish one only at
  terminal, quiescent shutdown; apply/rollback and abnormal termination never
  create a restorable cache snapshot. Bound storage work and total shutdown.
- Manager/Active owns the atomic CertificateSet even when DNS is stopped. API
  and managed SIGHUP reload current certificates under the existing mutation
  permit; file-mode SIGHUP also reloads policy. Preparation cannot mutate active
  material. Reload does not alter config revision, DNS/cache ownership or sessions.
- Software checks belong to one managed-process scheduler after setup, never a
  DNS generation or API GET. Updates-only settings preserve listeners, caches,
  RuntimeServices, certificates and sessions. File mode has no update scheduler.
- Update commit intent is persisted before a freeze and fixed inbox request.
  Check the freeze under the existing mutation permit; release that permit
  before root waits for normal SIGTERM. Unknown helper outcomes never unlock by
  timeout. Only a matching terminal fence and installed identity resolve intent.
- Managed `--check` is read-only and never opens Store/runtime, SQLite, cache
  persistence or listeners. Candidate preflight runs as the existing app user;
  root never executes the candidate. Root journal is the sole installed anchor,
  with bounded derived status and pending-launch health, not a second database.
- Subscription validators bind the final exact URL, request representation and
  accepted content. Source objects precede catalog selection; initialize a durable
  empty catalog before the first object. Never reconstruct missing/corrupt authority
  by scanning objects. Post-rename directory-sync uncertainty retains both versions
  and pauses GC until reconciliation. Preparation cannot refresh an active source
  or bypass future aggregate compilation, revision, local-basis and UP-freeze gates.
- One aggregate radix policy serves local and subscription rules. Config retains
  local source rules, not an index; each request holds its generation across cache/CNAME/stale
  handling. One worker owns construction and at most one request-held retired
  generation. Preserve raw cached answers and semantic cache restoration checks.
- Subscription status GETs do no IO. Background download/compile stays outside Manager and mutation
  locks, then recheck captured configuration/content/local basis and update freeze.
  Config candidates use try-admission under Manager; never await the worker there.
  Persist Config before its infallible policy swap; background content selects
  catalog before policy publication. Synchronize current/previous GC roots before
  new collection; do not invent a cross-file atomic transaction or legacy migration.
- Publish DNS terminal health once per generation to both readiness and storage
  sampling. Attempt diagnostics use bounded endpoint slots and finite typed
  dimensions; cancellation/shutdown are not upstream faults. Management API 200
  alone is not DNS readiness.
- Do not change host DNS, firewall rules, certificate trust, or system services as
  a side effect of development tests. Use isolated fixtures for local acceptance.

## Verification

Use the toolchain in `rust-toolchain.toml` and locked dependencies. Choose checks
that cover the changed behavior; `.github/workflows/ci.yml` defines the full CI gate.
Reuse existing tests and tools. Add tests or verification scripts only for a
meaningful behavior or regression gap, not duplicate coverage or speculative risks.
Run the smallest sufficient checks plus required project gates. Stop when acceptance
criteria pass; expand or repeat verification only for changes, failures, or new evidence.

- Rust: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`,
  and `cargo test --locked --all-targets`.
- Build/configuration: `cargo build --locked --release`, then
  `./target/release/parins --config parins.example.toml --check`.
- Web: from `web/`, run `npm ci --ignore-scripts`, `npm run typecheck`,
  `npm test`, and `npm run build` before compiling Rust; the binary embeds
  the exact checked `web/dist/` output. Run `npm audit --registry
  https://registry.npmjs.org --audit-level=high`. For interaction changes,
  verify the affected flow in a real browser, including errors, session
  transitions, keyboard use, and small screens.
- Installer/bootstrap: `sh scripts/test-install.sh` and
  `sh scripts/test-bootstrap.sh`. Run `scripts/test-systemd.sh --ephemeral-ci`
  only on a disposable Linux environment, not a developer's host.
- Dependency/security changes: use the dependency audit configured in CI.
- Documentation-only changes: check referenced paths/commands, review the diff, and
  run `git diff --check`; a full application build is not required.

For performance changes, retain a baseline and repeat the same workload with
correctness checks. Use `examples/bench_cache.rs` for cache measurements; report
regressions and variance as well as improvements. Cache operations per second are
not wire DNS QPS or evidence of capacity on a particular VPS. Do not remove safety
checks or add complex optimizations solely for a microbenchmark score.

## Documentation and delivery

- Write focused English commit messages, such as `docs: clarify cache ownership`.
  Report what changed, what was verified, and remaining limitations.
- Keep user-facing behavior and compatibility notes in `README.md` and
  `CHANGELOG.md` as appropriate. Distinguish unreleased main changes from releases.
- `docs/` contains intentionally ignored local plans and progress. Do not force-add
  it; essential contributor rules must remain available in tracked files.
- Maintain this file in the same change that alters an ownership boundary, durable
  invariant, or development workflow. Keep it concise and repository-portable:
  no personal paths, task history, test counts, temporary metrics, or copied defaults.
- When guidance conflicts with code or tests, investigate and explain the mismatch;
  do not silently weaken a contract or preserve an obsolete design by habit.
