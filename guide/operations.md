# Operations

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md)

This guide covers a running managed installation. Service commands:

```sh
sudo systemctl status parins-managed.service
sudo journalctl -u parins-managed.service -n 50 --no-pager
sudo systemctl restart parins-managed.service
```

## Console and accounts

**Access.** The console answers on its listening port for literal IP addresses
(including a public IP mapped by NAT), `localhost`, and `[web].public_host` when it is
set. Other host names, other ports and cross-origin requests are rejected.
Forwarding headers are ignored, so PariNS is not designed to sit behind a reverse
proxy.

**Sessions.** A sign-in lasts eight hours in an HttpOnly, SameSite=Strict cookie:
`parins_session_http` over HTTP, or a Secure `__Host-parins_session` over HTTPS. It
ends at expiry, at sign-out (which revokes that session on the server) or when the
service restarts. Sign-in and setup attempts are limited to 5 per minute per client
address. Passwords are stored as Argon2id hashes.

**Admin account.** The **Admin account** page changes the username (1–64 ASCII
letters, digits, `-` or `_`) and password (12–256 bytes); it asks for the current
password. Save or discard configuration drafts first. A successful change signs out
every session, including the current one. If the connection drops before you see
the result, sign in with the intended new credentials to check; do not repeat the
change. Account changes are separate from configuration, so a configuration
rollback never restores an old password. There is no password recovery: keep the
password and a [backup](#backups-and-the-state-directory) safe.

**Language and appearance.** Chinese and English, and light, dark or system
appearance, can be switched on every page, including sign-in and setup. The browser
remembers only these two choices; credentials, drafts and logs are never stored in
the browser.

**Applying configuration.** Settings pages and **Advanced (TOML)** edit one draft.
Saving validates it, shows the changes, and applies it with a revision check, so a
stale tab cannot overwrite newer settings. What a change restarts:

| Change | Effect |
| --- | --- |
| Only `[storage]`, `[query_log]`, `[statistics]`, `[updates]`, `[cache.persistence]`, filtering or subscriptions | Listeners and the cache stay. |
| Other `[cache]` settings | Listeners stay; the cache is replaced (emptied). |
| Anything else, or re-applying an unchanged configuration | DNS restarts and the cache is emptied. Statistics and logs are kept. |

The first two rows apply while DNS is running; otherwise saving starts DNS with the
new configuration, except that an `[updates]`-only change leaves a stopped DNS
stopped.

A new configuration must bind every listener before it is saved. If it cannot, or
saving fails, the previous configuration keeps running and the console shows the
error. If the connection drops during a save, reload the page to see whether it was
applied before trying again. Hot changes do not reload certificates.

## Online updates and recovery

The **Software updates** page shows the installed version and checks the official
GitHub releases (every `check_interval_hours`, or with **Check for updates**). Checks
start only after setup, send no credentials, and never install anything by
themselves. Online updates are offered only to official release installations made
by the installer.

**Update now** replaces the program through the restricted root helper:

1. The release is downloaded over HTTPS from public GitHub addresses only (proxy
   settings are ignored) and checked against its SHA256 manifest.
2. The new version checks the saved configuration and the saved rollback
   configuration, read-only, as the service user (`parins --manage --check`). If
   either fails, the update stops with `preflight_failed` and nothing changes. If
   the current configuration is fine, save any configuration change: the current
   configuration then becomes the rollback copy.
3. DNS stops briefly, the new program starts, and the update completes only when DNS
   (every listener) and storage are ready. Otherwise the previous program is put
   back automatically. Sign in again afterwards.

Settings are locked while an update runs. Only the program is replaced. A release
that changes the updater itself says **This release must be installed manually with
the installer**; run the [install command](deployment.md#install) again. Automatic
rollback restores the program, not data: it never restores an old database or
removes new history.

If PariNS cancels an update before the program was replaced, the console shows its
own reason and the root journal records the update as `aborted`.

**Subscriptions after an update.** While an update is in progress, the subscription
store stays closed and DNS uses the rules it already has. Once the update finishes,
the store opens automatically; no restart is needed. If it cannot open, the
**Blocking rules** page shows `subscription_storage_unavailable` and the opening is
retried.

**Manual recovery.** If an update and its automatic rollback both fail to finish,
the console shows **Manual recovery needed** and configuration changes stay locked.
Check DNS first, then read the status:

```sh
sudo systemctl status parins-managed.service parins-updater.service parins-update-recovery.service
sudo journalctl -u parins-managed.service -u parins-updater.service -u parins-update-recovery.service
sudo cat /var/lib/parins-updater/status.json
```

Do not edit or delete the updater journal or the update state, reset the database,
or start an older program by hand. Fix the stated cause, then run the
[install command](deployment.md#install) for the current or a newer release. When
that installation passes its readiness check, the unfinished update is closed as
superseded and settings unlock without a restart. The installer refuses if the
installed program is not the one the updater recorded; that case needs a restore
from backup.

The installer pauses the update request path while it runs. If it reports that the
program is installed but the request path is not enabled, the installation is
complete; run the command it prints:

```sh
sudo systemctl reset-failed parins-updater.path parins-updater.service && sudo systemctl enable --now parins-updater.path
```

## Certificates

**Import.** Under **Encryption & certificates**, enable a listener and paste its PEM
certificate chain and private key (up to 64 KiB each). PariNS checks that they match
and stores them as a private file under `certificates/` in the state directory; only
the file reference goes into the configuration, and the key is never shown again or
exported. Up to 32 imported identities are kept; delete unused ones by hand only
after checking the current and rollback configurations. Clients still check expiry,
host name and trust.

**External files.** You can reference certificate files elsewhere instead. The
service runs as a dynamic user, so grant read access through a group: the only
drop-in the installer accepts is one that sets `SupplementaryGroups=` under
`[Service]`, for example `/etc/systemd/system/parins-managed.service.d/certs.conf`.

**Renewal.** PariNS has no ACME client and does not watch files. After your issuer
has replaced the certificate and key, reload them with **Reload certificates**, or:

```sh
sudo systemctl reload parins-managed.service
```

A reload checks every configured certificate first and swaps them all together; on
any failure the previous ones stay. It keeps listeners, caches, sessions and the
configuration revision. New handshakes use the new certificate; open connections
continue. The console shows the result and the active fingerprints; check them,
because a sent signal does not mean the reload succeeded. In managed mode SIGHUP
reloads only certificates; in file mode it also reloads `filter_file`.

## Backups and the state directory

Managed state lives in `/var/lib/parins-managed` (stored under
`/var/lib/private/parins-managed`), owned by the service's dynamic user with private
permissions:

| Path | Content |
| --- | --- |
| `state.json` | Administrator hash, current and previous (rollback) configuration, revision. |
| `setup-token` | One-time setup token, before setup. |
| `certificates/` | Imported certificate identities. |
| `filter-subscriptions/` | Downloaded lists and their catalog. |
| `runtime/` | Statistics and query log database, cache snapshot. |
| `update-state.json` | Online update progress. |

Back up the whole directory while the service is stopped:

```sh
sudo systemctl stop parins-managed.service
sudo tar -C /var/lib/private -czf parins-managed-backup.tar.gz parins-managed
sudo systemctl start parins-managed.service
```

Do not edit live state, and do not start the managed service with `--config`.
Exported TOML contains no account data. Restore a backup only onto the same or a
newer PariNS version.

## Query log and storage

**Query log.** Off by default; turn it on under **Logs & storage** (`[query_log]`).
Each record has the client address, name and type, transport, response code,
duration, cache and filter result, the upstream used, ECS in and out, EDNS flags and
up to 16 answer records. The **Query log** page offers search, status filters,
paging, clearing, and timed refresh (1, 3, 5 or 10 seconds) over an authenticated
WebSocket (`/api/query-log/live`). Records appear after they are written, up to
`storage.flush_interval_ms` late. Each session may hold 2 live connections and the
service 16; idle ones close after 30 seconds. Treat the log as private browsing data.

The log keeps records until `max_entries`, `max_bytes`, `retention_secs` or the
database limit is reached, whichever comes first; the page shows the actual oldest
record and why older ones were removed. Queries cancelled after reaching the
resolver are recorded as dropped. Prefetch is not logged. Query data never appears
in metrics or the service log.

**Statistics.** The **Overview** page shows totals, cache and block rates, latency
and response codes, plus trends sampled once a minute (7 days by default). Totals and
trends survive restarts and contain no names or client addresses.

**Storage.** One worker owns the SQLite database; DNS never waits for it. When the
write queue is full, new records are dropped and counted. Cleanup runs every
`cleanup_interval_secs`, right after a write that exceeds a capacity limit, and at an
explicit flush or shutdown. At shutdown, the cache snapshot save and the storage
drain run in parallel, each with its own time budget. Clearing logs, totals or trends
affects only that kind of data.

The storage status shows a low-space warning when free space falls below 10% or
below the database limit plus twice the snapshot limit plus 64 MiB. It is an early
warning, not a guarantee.

## Metrics

Set `admin_listen` to a loopback address (for example `127.0.0.1:9090`) to serve two
read-only HTTP endpoints, with their own small connection budget:

- `GET /healthz` returns `ok` while the process runs. It does not mean DNS is ready.
- `GET /metrics` returns Prometheus text. All values restart from zero with the
  process; no names, client addresses or other unbounded labels are exported.

For DNS readiness, use `dns_health` in the console's authenticated `GET /api/status`
(`ready`, `state`, `code`); PariNS sends no alerts itself.

| Metric | Type | Meaning |
| --- | --- | --- |
| `parins_requests_total`, `parins_completed_total`, `parins_cancelled_total` | counter | Queries entering the resolver; those that returned (any response code); those cancelled. |
| `parins_responses_{noerror,nxdomain,servfail,refused,other}_total`, `parins_dropped_total` | counter | Responses by code; queries that ended without a response. |
| `parins_cache_hits_total`, `parins_cache_misses_total` | counter | First cache lookup of a query (a miss includes a disabled or bypassed cache). |
| `parins_cache_lookup_{fresh,stale,miss,bypass,disabled}_total` | counter | Cache lookups by outcome. |
| `parins_cache_store_{admitted,replaced,skipped,superseded_only}_total` | counter | Cache store decisions. |
| `parins_cache_reason_*_total` | counter | Why a lookup bypassed or a store skipped: `unsupported_query`, `unsupported_edns`, `ecs_unusable`, `ttl_zero`, `negative_without_soa`, `uncacheable_response`, `policy_disabled`, `capacity`, `variant_limit`, `epoch_changed`. |
| `parins_query_blocked_total`, `parins_response_blocked_total` | counter | Blocked by query name; blocked by a name in the answer's CNAME chain. |
| `parins_flight_{leaders,joined,rejected,bypassed}_total` | counter | Query coalescing: shared, joined, refused at a limit, not eligible. |
| `parins_upstream_operations_total`, `parins_upstream_failures_total`, `parins_upstream_timeouts_total`, `parins_ecs_retries_total` | counter | Upstream operations (TCP fallback and ECS retry stay inside one), failed exchanges (not DNS error codes), timeouts, ECS `/0` retries. |
| `parins_upstream_attempt_{succeeded,failed,cancelled,skipped}_total` | counter | Individual upstream attempts by outcome. |
| `parins_upstream_attempt_reason_*_total` | counter | Attempt end reasons: `deadline`, `connect_io`, `tls`, `http_status`, `protocol_invalid`, `peer_closed`, `caller_cancelled`, `shutdown`, `internal`. |
| `parins_udp_received_total`, `parins_tcp_received_total`, `parins_encrypted_received_total` | counter | Datagrams, TCP frames and encrypted DNS messages received. |
| `parins_udp_dropped_total`, `parins_tcp_rejected_total`, `parins_encrypted_rejected_total`, `parins_connections_rejected_total` | counter | Admission rejections at the global limits. |
| `parins_source_queries_rejected_total`, `parins_source_connections_rejected_total`, `parins_source_table_full_total` | counter | Per-client limit rejections; table-full is a subset. |
| `parins_quic_{doq,doh3}_*_total` | counter | DoQ and DoH3 connection, handshake, stream and HTTP outcomes (for example `incoming`, `admission_global_rejected`, `handshake_established`, `handshake_tls`, `stream_cancelled`, `http_response_4xx`). `incoming` counts only validated client addresses. |
| `parins_request_inflight`, `parins_upstream_inflight` | gauge | Queries and upstream operations in progress. |
| `parins_request_duration_seconds`, `parins_upstream_duration_seconds` | histogram | Latency, with buckets at 1, 5, 10, 50, 100, 500, 1000 and 5000 ms. |

In recursive mode `/metrics` adds:

| Metric | Type | Meaning |
| --- | --- | --- |
| `parins_recursion_started_total` | counter | Recursive resolutions started. |
| `parins_recursion_answered_total`, `parins_recursion_negative_total`, `parins_recursion_bogus_total` | counter | Resolutions with an answer; ending in NXDOMAIN or NODATA; with DNSSEC Bogus data. |
| `parins_recursion_deadline_total` | counter | Resolutions cancelled before finishing, by the query deadline or a cancelled request. |
| `parins_recursion_shutdown_total` | counter | Resolutions ended by shutdown. |
| `parins_recursion_failed_{timeout,referral,budget,other}_total` | counter | Failures: authoritative timeouts, an unfollowed referral, a query or depth budget, other causes. |
| `parins_recursion_outbound_sent_total` | counter | Queries sent to authoritative servers. |
| `parins_recursion_outbound_rate_waits_total`, `parins_recursion_outbound_rate_wait_ms_total` | counter | Queries that waited for a rate token, and the total wait in milliseconds. |
| `parins_recursion_outbound_inflight` | gauge | Authoritative queries admitted and not finished, including those waiting for a rate token. |

`[metrics] interval_secs` also prints the process counters, gauges and histograms
as JSON lines on stderr (`"event": "parins_metrics"`), periodically and at shutdown.

## Running recursive mode

Switching `[resolution] mode` restarts DNS with an empty cache, so answers are slower
until it warms up. Before switching:

- Allow outbound UDP and TCP port 53 to the internet, and IPv6 only if you set
  `[recursion] ipv6 = true`. These queries are plaintext and, with ECS on, carry the
  client subnet to the answer's authoritative servers.
- Size `max_outbound_per_second` and `max_outbound_inflight` for your cache-miss rate;
  one cold miss usually needs several authoritative queries.
- Only PariNS's own filtering applies; the old upstream's policies no longer do.

Rising `parins_recursion_outbound_rate_waits_total` or
`parins_recursion_deadline_total` means the outbound limits or `query_timeout_ms`
are too tight for the load. See [Resolution mode and recursion](configuration.md#resolution-mode-and-recursion).

## Troubleshooting

### Port 53 is in use

Setup or saving fails with `CONFIG_CONFLICT` and nothing is saved. Another service,
often the systemd-resolved stub, holds the port. See
[Port 53 conflicts](deployment.md#port-53-conflicts).

### DNS degraded

`dns_health.state` is `degraded` with code `listener_unavailable` and `ready` is
false: DNS is serving, but at least one listener is down. The `listeners` list in
`/api/status` and the **Overview** page show each listener's state, address and last
error, such as `Address in use`. The missing listener is rebound after 5 seconds,
then at doubling intervals up to once a minute, until it works or DNS stops. The
others, the cache and the upstreams keep working, and the journal repeats an
unchanged error at most every ten minutes. An online update needs every listener up,
so it rolls back while one is missing.

If DNS fails to start for another reason (code `startup_failed`, for example an
unreadable certificate), the service retries the saved configuration with the same
backoff until it starts or you apply a new configuration.

### DNS stopped after a rule problem

If the saved local rules cannot be compiled, such as an unreadable `filter_file`, DNS
stays stopped and the status shows `DNS unavailable:` with the reason. Fix the rules
and apply the configuration again.

### Subscriptions after an update

During an online update, subscriptions keep their current rules and cannot be
changed. They become available again automatically when the update finishes. If
`subscription_storage_unavailable` persists, check the disk that holds the state
directory.

### Storage shows "checking"

At startup an existing database is integrity-checked in the background, and the
storage status is `checking` meanwhile. DNS does not wait for it. New records wait in
the bounded queue (overflow is dropped and counted), and the check may take a while
for a large database. Only shutdown interrupts it. If the status ends as
`unavailable`, its error says why.

### Accepts paused

The log line `PariNS accept paused: ...; raise the open-file limit or lower
max_tcp_connections` means the process ran out of file descriptors. Listeners keep
serving after a short pause. Raise `LimitNOFILE` for the service or lower
`max_tcp_connections`.

### Cache snapshot not saved

`cache snapshot not saved: shutdown not quiescent (<cause>)` means queries were still
running when `shutdown_grace_ms` ran out, so the next start begins with an empty
cache. This is safe; raise `shutdown_grace_ms` if it happens often.
