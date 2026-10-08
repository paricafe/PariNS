# Operations

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md)

This guide describes the current main branch; features marked unreleased are not
part of the published v0.1.6 packages. See [CHANGELOG](../CHANGELOG.md) for release-specific changes.

## Management console

Use [Deployment](deployment.md) for first-run setup and the HTTP-to-HTTPS transition.

### Authentication and sessions

The console accepts literal IPv4/IPv6 hosts (including a public IP mapped by NAT)
at its listening port, localhost, and a specific validated `[web].public_host`
when configured. It rejects other hostnames, ports and cross-origin browser
requests; forwarding headers are not trusted. This is not a reverse-proxy trust
configuration. Passwords use Argon2id. HTTP login uses an eight-hour HttpOnly,
SameSite=Strict Cookie; HTTPS uses a Secure, HttpOnly, SameSite=Strict `__Host-`
Cookie. Refreshing restores the session until it expires, is revoked by logout,
or the management process restarts. Logout revokes that session server-side; it
does not promise to erase the browser's now-unusable Cookie value. The browser
never stores the session credential or binding in local/session storage. The
management origin is tied to the exact scheme, host and port; each protected request also
uses a per-session binding to prevent stale tabs from writing under a new login.
There are no external frontend assets. Opt-in query history is held in private
server-side SQLite storage, not browser storage. An unsaved configuration draft does not survive a
full page refresh.

### Administrator credentials (unreleased)

This feature is on the main branch and is **not included in published v0.1.6 packages**.

Use **Admin account** in the console to change the username and password.
Enter the current password and the desired username/new password; confirm the new
password in the form. Usernames use 1–64 ASCII letters, digits, hyphens or underscores;
new passwords use the existing 12–256 byte limit. Save or explicitly discard any
configuration draft first. Account changes are separate from DNS configuration.

A successful change revokes every existing session, including the current tab,
and requires signing in with the new credentials. It does not clear the browser
Cookie or issue a replacement session. A wrong current password keeps the valid
session open. If the connection fails before the result is known, do not repeat
the change automatically: sign in explicitly with the intended credentials to
confirm the outcome. Passwords and session bindings are neither stored in browser
storage nor sent through cross-tab notifications.

Credential rotation preserves configuration revision and rollback history, DNS
listeners and caches, runtime SQLite, certificates, subscriptions and software-update
state. Rolling back a DNS configuration does not roll back the account credentials.
There is no supported forgotten-password/offline recovery flow in this change.

### Language and appearance

The React console supports Simplified Chinese and English, plus light, dark and
system-following appearance. Use the selectors at the top of any page, including
sign-in and setup.
The initial language follows a supported browser language, falling back to
Simplified Chinese. Manual language and appearance choices are remembered locally
in that browser; switching works even if storage is blocked. Only these display
preferences are persisted.
Switching updates labels, help, messages, dates and charts in place, preserving
drafts and expanded query details. Configuration keys, user-entered values and
raw server diagnostic details keep their original text.

### Configuration changes and statistics

After setup, the console provides visual traffic statistics, grouped settings
forms, and an advanced TOML editor with validation, change preview, export, and
rollback. DNS, ECS, cache, filtering, encrypted listeners and resource budgets
use the same canonical configuration schema as CLI mode. Form changes are merged
by the server into the current draft, preserving unedited settings; TOML comments
and formatting may be normalized. File-backed filtering is clearly separated
from inline rules.

The dashboard shows persistent totals, cache/blocking rates, average latency,
response codes, latency distribution, and bounded aggregate trends.
History is sampled once per minute and defaults to seven days/10080 samples;
completed checkpoints survive process restarts. Missing intervals appear as gaps,
not zero traffic. Process metrics and persistent totals are separate scopes.
No query names or client IPs are collected for these charts;
there are no per-domain/client rankings. The separate opt-in [query log](#query-logs)
records per-request data; aggregate charts remain free of query identities.

With DNS running, changes limited to cache, storage, query logging, statistics,
filtering/subscriptions or update settings preserve listeners. A cache-policy
change replaces cache/refresh state; the other listed settings, including
cache-persistence-only changes, preserve the memory cache. Updates-only changes
also leave a stopped DNS instance stopped. Other configuration changes, and
reapplying an unchanged document, drain/restart DNS and clear cache, but preserve
process metrics and runtime history. These hot changes do not implicitly reload
certificates. Binding or persistence failures restore
the prior configuration when possible, and an unavailable DNS instance is shown
as an error. Stale edits are rejected by revision; after a disconnected save,
reload the current configuration to determine its result before retrying.

## State and backups

Managed `state.json` is authoritative: it contains the administrator hash,
current/previous TOML and revision, protected by 0700/0600 permissions, an
exclusive process lock and atomic replacement. Exported TOML excludes account
data. Relative rule/certificate paths resolve against the state directory;
provision those files separately with permissions readable by the service.
Do not manually edit live state or use `--config` with `--manage`. Back up the
whole private state directory while the service is stopped. Managed SIGHUP only
reloads the current certificate paths; rules and configuration still use apply.
Forgotten-password/offline recovery is not supported; keep your password and
private state backup safe. Online
[credential rotation](#administrator-credentials-unreleased) is an unreleased
main-branch feature.

## Service commands

Useful commands:

```sh
sudo systemctl status parins-managed.service
sudo journalctl -u parins-managed.service
sudo systemctl restart parins-managed.service
sudo systemctl disable --now parins-managed.service  # retains state and backups
# Local development, no systemd or root:
./target/release/parins --manage --state-dir parins-state --web-listen 127.0.0.1:3000
```

## Software updates

Starting with v0.1.5, managed Linux installations can check for software
updates and use a restricted updater. This is the first updater-capable official
version. Official same-epoch update, rollback and two-version data round-trip acceptance
must be verified separately. Isolated development-build recovery checks do not
prove that official path.

`[updates]` defaults to `auto_check = true` and `check_interval_hours = 6`
(1–168 hours). Checks begin only after managed setup, contact the official GitHub
repository without credentials, and never install automatically. File mode does
not run this scheduler. Changing these settings does not restart DNS or clear its
cache. `parins --build-info=json` reports the current source/official build identity;
`parins --manage --check --state-dir PATH` checks saved materials without starting
services, opening the runtime database, or consuming a cache snapshot.

One-click installation requires an official Linux managed installation and the
trusted updater components, not just a new console. The initial-development
v0.1.4 has no updater: first enrollment requires a verified newer installation
package and an explicit administrator action. A software update interrupts DNS
briefly and requires signing in again. Only matching durable-data contracts can
use automatic binary rollback; it does not restore an old database or discard
new query history. HTTPS and SHA256 establish transport and content integrity,
not an independent publisher signature.

The current installer accepts `--enable-updater` only for the exact official
v0.1.4 managed unit and fixed installation paths. Run it from a verified **newer**
package; the v0.1.4 download does not implement this option. The installer keeps
the same state directory and certificate access, performs read-only preflight as
the service user, and registers the new main binary, helper and systemd units
together. It does not translate old settings or migrate the database. Unknown or
custom layouts require a manual installation plan. If the first enrollment fails
after the new program ran, the old files can be restored but v0.1.4 is not started
automatically against potentially changed data: it has no verified rollback epoch.

For an interrupted update, inspect the console's operation reason and the
read-only service status before taking manual action:

```sh
sudo systemctl status parins-managed.service parins-updater.service parins-update-recovery.service
sudo journalctl -u parins-managed.service -u parins-updater.service -u parins-update-recovery.service
sudo cat /var/lib/parins-updater/status.json
```

Retain the private updater journal, reported backup, and business state when
diagnosing a `manual_required` result. Do not delete the journal to force a new
attempt, reset the database, or blindly start an older binary. Resolve the stated
cause and verify that the selected binary can read the retained data before a
manual reinstall. Root's journal is authoritative; the public status is derived.
The release reader requires public GitHub addresses and does not use proxy
environment variables. DNS interception that returns private or synthetic
addresses is rejected rather than silently bypassed.

## Certificates

### Import through the console

Under **Security and encryption**, enable the target DNS listener and paste its
PEM certificate chain and private key. Import validates key matching, accepts up
to 64 KiB per field, and stores a private immutable combined identity (0700
directory, 0600 file). Only file references return to the draft; keys are never
returned by the API or included in TOML/export. Save/apply uses the existing
configuration transaction. Certificate expiry, hostname and trust must still be
checked by clients. At most 32 identities are retained; normalized repeats reuse
the same file. Remove unused files manually only after checking current/rollback
configurations. Enabling inbound DoH with a validated `[web].public_host`
also chooses the identity for management HTTPS; importing PEM alone does not
switch the console protocol.

### Certificate renewal

- Certificate preparation validates bounded regular PEM files, matching keys and
  current dates; the DoH role also checks the management public host. One atomic
  CertificateSet publication updates the DoH slot shared by Web/H2/H3 and the
  independent DoT/DoQ slots. Any failure keeps every previous identity. New full
  handshakes see the new roles; established connections remain usable.
- Managed `POST /api/certificates/reload` uses the saved revision and existing
  authenticated mutation boundary. Managed SIGHUP uses that same serialized
  operation, only for certificates; file-mode SIGHUP also reloads rules. Neither
  reload moves listeners, resets cache/history, changes config revision nor logs
  out users. `status.certificates` reports active fingerprints and the last result.

After your external issuer atomically publishes a complete key/chain pair, a
new-build renewal hook can run `systemctl reload parins-managed.service`.
**Do not use this hook on managed v0.1.3.** Signal delivery is not completion:
check the authenticated reload result and a new handshake's public fingerprint.
Configure read-only service access to external TLS files yourself; the installer
does not chown or relocate them. No private key is returned by status.

## Query logs

The **Query log** page provides manual refresh, search, status filtering,
newest-first cursor pagination, and explicit clearing. Enable it under Runtime
settings (disabled by default):

```toml
[query_log]
enabled = true
max_entries = 1000 # independently limits actual coverage
max_bytes = 67108864
retention_secs = 86400 # 60..2592000; an upper bound, not promised coverage
```

Logs contain client IP, name/type, transport, response code, duration, actual
cache/filter path, winning upstream (if any), input/output ECS and EDNS flags,
and bounded answer details (16 records, capped strings). They are authenticated,
persisted asynchronously, periodically expired, and retained across DNS/process restart.
Logs expose the actual oldest/latest record times, coverage and cleanup/drop reasons;
count/byte/database limits may shorten retention. Requests cancelled
after entering the resolver are recorded as dropped; malformed transport requests
that never reach the DNS resolver are not DNS query entries. Background prefetch
is not counted as a client request. Clearing rejects old in-flight log writes;
future requests can create new entries. No query data is emitted to metrics or
stderr. Treat logs as private browsing metadata, not a durable audit database.

## Runtime storage and diagnostics

Managed runtime data lives under `STATE_DIR/runtime`; file mode uses `--data-dir`.
A private single-writer lock protects each directory. SQLite uses one worker,
bounded queues, DELETE journaling and FULL synchronization. DNS never waits for
log writes; overload drops new records and exposes finite drop reasons. A clear
or reset succeeds only after commit. Logs, totals and trend history have separate
epochs, so clearing one does not reset the others or admit pre-clear queued work.

```toml
[storage]
max_database_bytes = 134217728
flush_interval_ms = 1000
cleanup_interval_secs = 60
queue_max_entries = 4096
queue_max_bytes = 8388608

[statistics]
retention_secs = 604800
max_samples = 10080
reset_interval_days = 0 # disabled; reset totals and clear trends separately
```

The database limit excludes rollback journal and temporary cache-snapshot peaks.
The console shows configured/applied settings, actual committed data and bounded
pending work. Filesystem capacity is sampled at most once per minute off the DNS
path; unavailable or stale observations are explicit, never zero free space.
Low-space warnings use less than 10% available, or less than the database budget
plus twice the snapshot budget plus 64 MiB. This is an early warning, not a promise
that writes will succeed. No other application's files are cleaned up.

The `dns_health` field of authenticated `GET /api/status` reports managed DNS
readiness; a reachable management API alone is not proof that DNS is running.
Unexpected listener exits
publish a generation-scoped failure also used by statistics sampling. Reapply
configuration after diagnosing the failure; there is no automatic restart loop
inside the manager. Process-local typed cache/upstream/QUIC counters remain usable
with query logging disabled. Upstream endpoint slots belong to their displayed
pool generation, and bounded query traces contain at most eight attempts, keeping
the winning or final failure result. Caller cancellation, parallel race losers and
shutdown are separate from network failures. Public `/healthz` stays liveness only.

## Runtime metrics

`Resolver::metrics()` and `Server::metrics()` expose fixed counters, inflight gauges
and cumulative request/upstream latency histograms. Collection is always active;
`[metrics] interval_secs = 10` enables periodic and shutdown JSON snapshots on
stderr. The default `0` disables output. An optional top-level `admin_listen`
starts a loopback-only HTTP endpoint: `GET /metrics` exposes Prometheus counters,
gauges and histograms (seconds); `GET /healthz` is process liveness, not upstream
readiness. No administrative writes or query logs are exposed. Snapshots have `event=parins_metrics`, `entry_point=server`, a per-run
`run_id`, `reason`, `uptime_secs` and `metrics`. Counters reset on restart.

- `requests/completed/cancelled` refer to resolver calls; completed means returned,
  not DNS success or delivery to the client. Response counters classify RCODEs.
  Admission drops/rejections are separate and do not enter resolver counts.
- `cache_hits/misses` count the initial lookup (miss includes disabled/bypassed
  cache); the admission-race recheck may avoid IO without an initial hit.
  Filtered queries before lookup do not count as hits or misses.
- `flight_leaders/joined/rejected/bypassed` distinguish sharing from overload.
  `upstream_operations` counts whole operations, not packets: TCP fallback and
  ECS retry remain inside one operation. `upstream_failures` counts exchange
  failures/timeouts, not DNS error RCODEs; timeouts are also a subset counter.
- UDP datagrams/TCP complete frames, encrypted DNS messages reaching shared
  admission (`encrypted_received/rejected`), admission drops, query rejections and
  connection rejections distinguish ingress saturation. Final gauges reach zero
  after orderly drain or cancellation.
- Latency buckets use inclusive bounds 1/5/10/50/100/500/1000/5000 ms plus infinity
  (`upper_bound_micros: null`), with cumulative counts and `sum_micros`. Durations
  include cancelled operations. Snapshots are concurrent approximations, not
  transactions; use bucket deltas for approximate percentile analysis.

No query names, client addresses or unbounded labels are emitted. Existing
startup/shutdown messages remain plain text; select JSON events when ingesting
metrics. No OpenTelemetry tracing or alert setup is included.

## Deployment acceptance

Before deployment, choose management/DNS network access rules, provision readable
external certificates, inspect capacity/backups, and validate independent encrypted
clients. The installer never changes host DNS, firewall, trust stores or other
applications. No measured local loopback result proves public DoQ reachability or
capacity on a 2 vCPU/2 GiB VPS.

For an independent public DoQ check, manually run **Public DNS acceptance** in
GitHub Actions with your server's public IP, certificate hostname and UDP port.
It sends one `example.com. A` query, verifies the public CA, hostname, ALPN and DNS
response, and retains a small JSON result. No server credentials are needed. A
pass proves that address family and network path at that time, not capacity or
reachability from every client; ordinary CI does not contact your deployment.

**Update boot recovery** runs a separate, disposable Linux VM controlled by a
GitHub runner. It checks normal reboot and interrupted precommit recovery using
a development package. It does not reboot the runner or a deployment, and does
not establish official-version upgrade/rollback, power-loss recovery or capacity.
