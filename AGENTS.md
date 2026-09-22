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
- Prefer existing mechanisms and dependencies. Add abstractions, retries, caches,
  or fallbacks only for a concrete requirement or reachable failure.

## Ownership

| Area | Responsibility |
| --- | --- |
| `src/config.rs` | Authoritative configuration schema, defaults, and validation. |
| `src/protocol.rs`, `src/ecs.rs` | DNS protocol checks, ECS validation, scope, and response normalization. |
| `src/cache.rs` | Cache eligibility, semantic keys, TTLs, budgets, eviction, policy matching, inspection, and invalidation. No network IO. |
| `src/resolver.rs`, `src/flight.rs`, `src/resolver/refresh.rs` | Resolution orchestration, shared upstream work, bounded refresh, and stale fallback. |
| `src/upstream.rs`, `src/upstreams/`, `src/upstreams.rs`, `src/scheduler.rs` | Upstream protocol exchange, explicit bootstrap, and scheduling; preserve resolver deadlines and cancellation. |
| `src/query_log.rs` | Opt-in bounded per-request history, retention, pagination and clear epochs; never an aggregate metrics label store. |
| `src/server.rs` and listener/transport modules | Listener lifecycle, transport framing, connection limits, and shutdown. |
| `src/policy.rs`, `src/limits.rs` | Filtering decisions and source resource budgets, respectively. |
| `src/manage/` | Authenticated management APIs, configuration transactions, persistence, and managed runtime ownership. |
| `web/` | Embedded UI, forms, drafts, and presentation. No independent DNS policy engine or authoritative configuration state. |
| `scripts/`, `deploy/` | Installation, packaging, and service lifecycle; not DNS business logic. |

Keep checks at the boundary that owns them. UI validation helps users, but Rust
validation remains authoritative. Reuse protocol and configuration helpers rather
than implementing different rules for file mode, management APIs, and the browser.

Console translations live in `web/locales-*.js`; `web/i18n.js` updates text and
accessible labels in place. Language changes must preserve controls, drafts and
session ownership. Persist only the locale preference, never credentials or drafts.
Keep both languages complete; configuration values and raw diagnostics stay intact.

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
- Inspection and explanation are read-only: no upstream requests, recency changes,
  or hit-counter updates. Filtering must not contaminate cached upstream answers.

## Concurrency, configuration, and security

- Keep shared locks short. Never hold a cache lock during network IO; reconstruct
  responses outside shard locks. Bound queues, cache state, and background work.
- Give spawned tasks an owner and explicit shutdown/cancellation behavior. Refresh
  and foreground coalescing must share compatible work without crossing generations.
- Keep cache byte accounting distinct from process RSS. Preserve separate positive
  and negative budgets; document capacity/utilization trade-offs when changing them.
- Management mutations must retain authentication, Host/Origin checks, and relevant
  revision/epoch checks. Preserve atomic persistence and failure recovery.
- Cache-only configuration changes should not restart DNS listeners. Validate and
  persist before publishing replacement state; do not promise cache retention after
  arbitrary policy changes. Report restart requirements accurately for other changes.
- Preserve HTTPS and certificate verification. Never commit credentials, private
  keys, runtime state, or private configuration. Do not add sensitive logging by
  default; query/client data needs explicit privacy and retention decisions.
- Pasted DNS identities must be validated before private persistence; return only
  file references, never private keys. Console TLS and DNS TLS remain independent.
- Pool endpoints share cache semantics: require equivalent policies, explicit
  hostname bootstrap and authenticated encrypted transports. Parallel losers must
  cancel with the caller; failed DNS responses must not preempt usable answers.
- `[upstreams]` is the canonical upstream configuration. Keep legacy TLS/SNI and
  hedging semantics until an explicit replacement; unrelated form edits must not
  migrate legacy settings. The console exposes only the canonical editor.
- H3 preference applies only to HTTPS endpoints, within the resolver deadline.
  Preserve verified H2 fallback, bounded cooldown and reusable connection ownership;
  cancelling a request must release its stream without breaking other requests.
- Do not change host DNS, firewall rules, certificate trust, or system services as
  a side effect of development tests. Use isolated fixtures for local acceptance.

## Verification

Use the toolchain in `rust-toolchain.toml` and locked dependencies. Choose checks
that cover the changed behavior; `.github/workflows/ci.yml` defines the full CI gate.

- Rust: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`,
  and `cargo test --locked --all-targets`.
- Build/configuration: `cargo build --locked --release`, then
  `./target/release/parins --config parins.example.toml --check`.
- Web: `for file in web/*.js; do node --check "$file"; done` and
  `node --test scripts/test-web.mjs`. For interaction changes, verify the affected
  flow in a real browser, including errors, session transitions, and small screens.
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
