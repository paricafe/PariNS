# Changelog

## Unreleased

- Add managed-mode official release checks and a restricted Linux updater with
  read-only preflight, durable update intent and bounded recovery.
- Add bilingual software-update controls and update-only hot settings without
  restarting DNS or clearing caches.
- Add official build manifests and explicit first-time updater enrollment.
- Add optional HTTPS filtering subscriptions with explicit domain-list and
  sinkhole-hosts formats, verified last-known-good content, bounded updates,
  and per-source status in the bilingual console.
- Use one compact radix policy for local and online rules. Keep local allow
  priority, request-consistent generations and unfiltered upstream cache data.
- Apply filtering settings without restarting managed DNS listeners; verify
  effective source material before saving and include it in read-only preflight.
- Reject local rule changes before configuration or certificate publication when
  old requests still hold a retired policy, including with unavailable source storage.
- Avoid hashing the executable at startup for independent management instances
  that cannot participate in the Linux updater installation protocol.
- Fix successful managed reinstalls reporting a cleanup error after releasing
  their installation lock.
- Keep the service mount namespace pinned during both low-privilege reinstall
  preflights when the installer runs under dash.
- Performance and disposable Linux lifecycle acceptance remain release gates.
  These development changes are not included in v0.1.4.

## v0.1.4 — 2026-09-24

### Upgrade notice

- This initial-development release intentionally changes the managed state
  directory and DoH configuration. The installer rejects the old managed layout
  before changing files. Back up the old state and explicitly retire the old
  managed unit and its state markers, retaining operator-owned TLS files. Let
  the new installer create `/var/lib/parins-managed`, then initialize again and
  re-enter the updated configuration. Do not pre-create the target directory or
  copy the old `state.json` into it; there is no automatic migration.
- Replace `[doh3]` with `[doh]` plus `http3 = true`. Keep a valid DoH certificate
  and matching `[web].public_host` for management HTTPS. Do not downgrade to an
  old binary against the new runtime database; retain a separate pre-upgrade backup.

### Changed

- Use an exclusive `/var/lib/parins-managed` directory for the managed installer.
  Existing old-layout managed state and unknown target directories are rejected
  before installation; no automatic migration is provided. External certificate
  directories remain operator-owned and are never moved or changed by installation.
- Replace independent `[doh3]` configuration with `[doh].http3 = true`, serving
  HTTP/2 and HTTP/3 on the same address/actual port and certificate. Update TOML
  explicitly; retired configuration is rejected.
- Persist opt-in query history, cumulative totals and aggregate trends in bounded
  SQLite storage. Separate clear/reset actions preserve their independent epochs.
  File mode now owns `--data-dir` (default `parins-data`); `--check` creates no state.

### Added

- Restore fresh cache entries only after a clean, quiescent process shutdown.
  Startup consumes the snapshot before serving; crash/forced shutdown starts cold.
- Cache exact outgoing subnets when an upstream omits ECS, without sharing answers
  across prefixes. Support EDNS Padding without storing or replaying padding bytes.
- Typed upstream attempt, cache decision and QUIC diagnostics, generation-scoped
  DNS readiness, actual query-history coverage and cached filesystem capacity.
- Atomic certificate reload for Web, DoH H2/H3, DoT and DoQ using the current saved
  paths: management API or managed SIGHUP. Failures retain every previous identity;
  unchanged material does not advance the certificate generation. Configuration,
  sessions, DNS listeners, cache and history are preserved.
- Bilingual runtime/storage controls, certificate status and diagnostics in the
  existing console, with reduced-motion support.

### Fixed

- Preserve the total upstream deadline through bootstrap, connection waits and
  HTTP/3 fallback; distinguish cancellation from actual transport failures.
- Preserve current totals and new history during wall-clock rollback, and keep
  DNS shutdown responsive even when a file-mode reload is blocked on IO.
- Show only the selected version's changes on its GitHub Release page.

### Operational notes

- Certificate reload is available starting with v0.1.4; do not send SIGHUP to a
  managed v0.1.3 process expecting this behavior.
- Query logs use bounded asynchronous storage and can drop records under overload.
  Retention time is a maximum, not guaranteed coverage or a lossless audit trail.
- Local tests and release CI are not a production capacity guarantee. Public
  DoQ paths, external renewal hooks and target-VPS resources need operator acceptance.

## v0.1.3 — 2026-09-24

### Upgrade notice

- This release changes the management listener from self-signed HTTPS to HTTP
  unless inbound DoH or DoH3 is enabled with a valid certificate and
  `[web].public_host`. On an existing installation without that configuration,
  upgrading changes port 3000 to plaintext HTTP. Restrict it to administrator
  IPs and use local access or an SSH tunnel for credentials and private-key
  entry; configure inbound DoH/DoH3 to restore management HTTPS. Saved state is
  retained, but old generated self-signed files are no longer used.
- An existing inbound DoH/DoH3 configuration now also needs a concrete
  `[web].public_host` covered by its certificate. Without one, managed startup
  rejects the configuration; update it before upgrading. The installer attempts
  to restore the prior binary and service if the new one fails to start.

### Changed

- Replace the native-script management page with a React 19/TypeScript console,
  including responsive navigation, light/dark/system appearance, bilingual views,
  per-request query details, and a single in-memory configuration draft.
- Replace Bearer tokens with eight-hour HttpOnly/SameSite=Strict Cookie
  sessions (Secure and `__Host-` under HTTPS). Refresh restores a valid login;
  protected APIs require a session binding. Logout revokes only its session,
  without relying on Web Locks or deleting the browser Cookie.
- Serve the management console over HTTP on port 3000 by default, without
  generating a self-signed certificate. Enabling inbound DoH or DoH3 with a
  validated `[web].public_host` switches that port to HTTPS using the same
  verified DNS identity; explicit confirmation is required to return to HTTP.
- Require exact same-origin checks for management writes, retire all old Web
  assets and Bearer callers, and embed checked static build output in the Rust
  binary. Unsaved drafts still do not persist across a full refresh.

## v0.1.2 — 2026-09-23

### Improved

- Split encrypted DNS listener forms into listening address and port fields,
  preserving IPv6, unsaved input and the existing `listen` configuration format.

### Fixed

- Isolate management login/setup attempt limits by socket peer, with bounded
  source tracking and a separate password-hashing concurrency budget.
- Invalidate overlapping obsolete cached answers when a new successful response
  carries EDE diagnostics, without caching those diagnostics for replay.
- Account for HTTP Age in DoH HTTP/2 and HTTP/3 answers and cache lifetimes.
- Keep in-flight DoQ queries alive when bootstrap rotates an upstream address.
- Preserve invalid cache-rule TTL drafts instead of treating incomplete numeric
  input as clearing an override; focus the affected field with a translated error.

## v0.1.1 — 2026-09-22

### Added

- Unified upstream DNS settings and first-run setup: one list for one or more
  servers, with a single configuration model for the console and TOML files.
- Optional HTTP/3 preference for HTTPS upstreams, with reusable QUIC connections,
  bounded attempts, verified HTTP/2 fallback and a cooldown after H3 failure.
- Simplified Chinese/English console switcher with a local language preference,
  clearer instructions and translated forms, query details, charts and messages.
  Switching languages preserves unsaved drafts and expanded query details.
- Opt-in bounded query log with per-request answers, client/transport, actual
  upstream/cache/filter paths, ECS flags, search, pagination and clearing.
- Multiline UDP/TCP/DoT/DoH/DoQ upstream pools with static weighted round-robin
  or bounded parallel racing; explicit hostname bootstrap and verified TLS.
- PEM certificate/key paste for encrypted DNS listeners: validate key matching,
  store privately and return only file references to the configuration draft.
- Removed the management-page footer slogan; added forwarding regressions for
  client EDNS options, ECS and DNS flags across the supported upstream protocols.
- Concurrent cache shards with response construction outside shard
  locks, individual ECS-variant LRU eviction and positive/negative budgets.
- Domain/type policies, explicit bypass and TTL caps, opt-in bounded prefetch
  and failure-only positive stale responses; both remain disabled by default.
- Authenticated cache usage, lookup explanation, and exact name/type/scope or
  whole-cache invalidation, with epochs rejecting old in-flight fills.
- Visual rule editing and cache controls; cache-only configuration updates and
  rollback preserve DNS listeners while replacing cache/refresh state.
- Reproducible cache contention benchmarks and lifecycle/management regressions.

Byte accounting now includes conservative per-entry metadata; the same byte
budget may hold fewer responses. Budgets are partitioned, not a process RSS cap.

### Removed

- Retired root `upstream`, `upstream_tls`, `upstream_pool` and `scheduler`
  configuration, delayed-replica scheduling, and automatic migration logic.
  `[upstreams]` is now required; DoT reuse belongs to `[upstreams.dot_pool]`.
  This is an intentional configuration break during initial development.
  Use the current example or setup wizard rather than old development configs.

### Fixed

- Cancelling an HTTP/3 upstream request while awaiting a response now releases
  its stream without a dependency panic and keeps other queries usable.
- Linux bootstrap defaults to v0.1.1, matching the versioned release packages.

## v0.1.0 — 2026-09-22

First public release of PariNS, a self-hosted caching and filtering DNS forwarder.

### Included

- Linux x86_64 and ARM64 static binaries, checksummed release packages, and a
  download-and-install script for systemd hosts; no Rust toolchain required.
- HTTPS management on port 3000 with a persistent self-signed certificate,
  first-run setup token, administrator login, configuration validation and
  application, export, and one-generation rollback.
- Visual management dashboard with bounded 24-hour aggregate history, request
  and cache trends, response-code and latency distributions; grouped forms for
  DNS, cache/ECS, filtering, encrypted listeners and runtime limits, sharing a
  validated draft with the advanced TOML editor.
- UDP/TCP, DoT, DoH over HTTP/2 and HTTP/3, and DoQ DNS listeners; UDP/TCP or
  authenticated DoT upstream forwarding.
- Bounded caching with independent ECS subnet answers, opt-in ECS, local
  name/CNAME filtering, request coalescing, and aggregate metrics.
- Optional source budgets, DoT upstream connection reuse, equivalent-replica
  hedging, and graceful shutdown.
- State-preserving installation upgrades with prior binary/unit backups and
  attempted rollback if service startup fails.

### Deployment notes

- Linux installation requires systemd and root privileges. The installer does
  not change system DNS, firewall rules, or existing conflicting services.
- Management defaults to `0.0.0.0:3000` over HTTPS. Verify the self-signed
  certificate fingerprint; wildcard-generated certificates cannot infer a
  public/NAT IP for name matching. Restrict management access to administrators.
- DNS starts after setup. Select a reachable upstream and an appropriate DNS
  listen address/port; the wizard starts with loopback DNS on port 5353.
- Configuration application restarts DNS and clears in-memory caches/metrics.
  Self-signed HTTPS identities and administrator/configuration state survive
  restarts and reinstalls.
- Existing source installations using the original HTTP console move to HTTPS;
  the managed unit now listens on all IPv4 interfaces. Use an explicit systemd
  override if loopback-only management is required.

### Known limits

- Early release, not a production capacity guarantee or an unrestricted public
  resolver deployment. Infrastructure abuse protection remains the operator's
  responsibility.
- Not an authoritative or iterative DNS server; no local DNSSEC validation,
  automatic certificate renewal, external blocklist downloads, or DoH/DoQ
  upstream clients.
- Management supports direct IP access, not arbitrary domain/reverse-proxy
  configuration. Password changes/account recovery and zero-downtime full
  configuration replacement are not yet provided.
- Release SHA256 files detect corruption; they are not independent signatures.
- Aggregate history is memory-only; no query logs, domain rankings or client-IP
  tracking are collected by the management dashboard.
