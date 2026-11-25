# Operations

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md) · [Architecture](architecture.md)

This guide covers operating a managed PariNS installation. Use standard systemd commands to inspect or restart the service:

```sh
sudo systemctl status parins-managed.service
sudo journalctl -u parins-managed.service -n 50 --no-pager
sudo systemctl restart parins-managed.service
```

## Console and accounts

- **Access.** The console answers on its listening port for literal IP addresses including a NAT-mapped public IP, `localhost`, and `[web].public_host` when configured. Other hostnames, ports, and cross-origin requests are rejected. Forwarding headers are ignored, so do not put a reverse proxy in front of PariNS. Connections are capped per socket peer with IPv6 grouped by `/64`, and request body reads have a separate deadline.
- **Sessions.** Sign-in sessions last eight hours in an `HttpOnly`, `SameSite=Strict` cookie named `parins_session_http` over HTTP or Secure `__Host-parins_session` over HTTPS. A session ends at expiry, on sign-out, or when the service restarts. Sign-in and setup attempts are limited to 5 per minute per client address with IPv6 counted by `/64`, including failed [Turnstile checks](#human-verification-cloudflare-turnstile). Passwords are stored as Argon2id hashes.
- **Admin account.** Open **Account & security** from the person icon to change the username or password. Usernames accept 1 to 64 ASCII letters, digits, `-`, or `_`, and passwords accept 12 to 256 bytes. Saving requires the current password, revokes every session including yours, and discards unsaved configuration drafts after asking you to confirm. If the connection drops before you see the response, test signing in with the new credentials instead of submitting again. Account credentials live outside configuration, so configuration rollback never restores an old password. There is no password recovery. Keep the password and a [state backup](#backups-and-the-state-directory) safe.
- **Language and appearance.** Switch between Chinese and English or light, dark, and system themes on any page, including sign-in and setup. The browser saves only language, theme, background choice, overlay darkness, and query-log refresh interval. Uploaded custom background images stay in IndexedDB until you delete them. Credentials, session bindings, private keys, drafts, and query logs are never stored in browser storage.
- **Applying configuration.** Settings pages and **Advanced (TOML)** edit one shared draft. **Save** validates and applies the draft at once and reports whether DNS restarted. The console asks for confirmation only when a change moves the console to a new address or protocol, which signs you out, or when disabling DoH downgrades the console from HTTPS to HTTP. Use **Advanced (TOML)** to compare changes against saved state, and **Undo last save** to roll back to the previous saved configuration.
- **Concurrent edits and dropped connections.** Every save checks the saved revision so a stale tab cannot overwrite newer settings. On a conflict, the console reloads saved state while keeping your draft. Non-overlapping edits merge for review, and conflicting fields let you choose which value to keep. Actions such as subscription updates, certificate reloads, or clearing logs act on saved state without waiting for your draft. Starting a software update asks you to save or discard the draft first because updates restart PariNS. If the connection drops mid-action, the console reads server state to verify the outcome instead of replaying the request.

Every save falls into one of three restart tiers:

| Change | Effect |
| --- | --- |
| Only `[storage]`, `[query_log]`, `[statistics]`, `[updates]`, `[cache.persistence]`, filtering, or subscriptions | Listeners and the cache stay in place. |
| Other `[cache]` settings | Listeners stay in place and the cache is replaced empty. |
| Any other change, or re-applying an unchanged configuration | DNS restarts with an empty cache. Statistics and logs are preserved. |

The first two rows apply hot while DNS is running. When DNS is stopped, saving starts DNS except for an `[updates]`-only edit, which leaves DNS stopped. Every newly applied configuration must bind all configured listeners before PariNS saves it. If binding or saving fails, the previous configuration keeps running and the console displays the error. Hot changes do not reload certificates.

## Human verification (Cloudflare Turnstile)

Open **Account & security** to enable Cloudflare Turnstile on the sign-in page. It is off by default. Create a Turnstile widget for your console hostname in the Cloudflare dashboard, then enter its site key and secret key. Setting, changing, or clearing keys requires the current password, saves independently of configuration, and revokes all active sessions.

PariNS keeps the secret key only in the private state directory. It is never displayed again, exported in TOML, or written to logs. When enabled, the sign-in page loads the widget and PariNS verifies the token with Cloudflare for success, hostname, and the `login` action before checking the password. Verification failures or unreachable Cloudflare endpoints reject the sign-in and count toward the attempt limit. Connections from loopback addresses such as an SSH tunnel to `localhost` skip Turnstile so you can always recover from wrong keys or Cloudflare outages. Only the socket peer counts because forwarding headers are ignored.

Do not place a local proxy or tunnel such as `cloudflared`, an nginx `stream` proxy, or `sslh` on the same server in front of the console. That makes every remote client appear as loopback, bypassing Turnstile and forcing all clients onto one attempt budget. The console shows a warning when Turnstile is enabled and you connect from the same host.

Only the signed-out login document allows Cloudflare's script and frame from `challenges.cloudflare.com`. After sign-in, the console reloads to drop that permission. Cloudflare test keys such as `1x00000000000000000000AA` and `1x0000000000000000000000000000000AA` are accepted only as a pair. They always pass or always fail without protection and display a warning banner in the console.

## Online updates and recovery

The **Software updates** page displays the installed version and checks official GitHub releases every `check_interval_hours` or on demand with **Check for updates**. Checks start after first-run setup, send no credentials, and never install updates automatically. Online updates are available only for official release installations created by the installer.

Clicking **Update now** replaces the binary through the restricted root updater:

1. PariNS downloads the release over verified HTTPS from public GitHub IP addresses only, ignoring proxy settings, and verifies its SHA256 manifest.
2. The candidate binary runs read-only as the service user via `parins --manage --check` against both the current configuration and the rollback copy. It must read the same saved configuration, rule files, downloaded subscription lists, certificates, and upstream CA as the running version. If either check fails, the update stops with `preflight_failed`, changes nothing, and logs which input categories differed. Fix the flagged inputs, or if the current configuration is valid, save a change once to refresh the rollback copy and retry.
3. DNS pauses briefly while the new binary starts. The update commits only after all DNS listeners and storage report ready, with up to three minutes per launch including ten continuous seconds of fresh health reports from the verified process. A large database still running its startup check prevents early commit even if DNS is already answering. If readiness fails, the updater automatically restores the previous binary under its own launch budget. Sign in again after the update completes.

Settings stay locked during an update, and only the main binary is replaced. If a release changes the root updater itself, the console reports **This release must be installed manually with the installer**, and you must run the [install command](deployment.md#install) again. Automatic rollback restores only the binary, never older database state or deleted history.

Update recovery and installer preflight require helper protocol 7. Upgrade older helpers with the full installer, which updates the binary, root helper, and systemd unit files together. During installer rollback, the packaged helper waits for storage without committing, and the restored helper verifies the restored unit files and fresh health before completing recovery. If PariNS cancels an update before replacing the binary, the console shows the reason and the root journal records `aborted`.

While an update runs, the subscription store stays closed and DNS keeps serving its loaded rules. When the update finishes, the store reopens automatically without a restart. If reopening fails, **Blocking rules** displays `subscription_storage_unavailable` and retries in the background.

### Manual recovery

If an update and its automatic rollback both fail, the console shows **Manual recovery needed** and keeps configuration writes locked. Check DNS first, then inspect service and updater status:

```sh
sudo systemctl status parins-managed.service parins-updater.service parins-update-recovery.service
sudo journalctl -u parins-managed.service -u parins-updater.service -u parins-update-recovery.service
sudo cat /var/lib/parins-updater/status.json
```

Do not edit or delete the updater journal, reset the database, or launch an older binary manually. Fix the reported cause, then run the [install command](deployment.md#install) for the current or a newer release. Once that installation passes readiness, the open operation closes as superseded and settings unlock without another restart. If the live binary digest differs from the recorded digest, the installer refuses to run until you restore the matching binary as shown below.

### Update stuck in progress

If the updater cannot read its journal when handling a request, it cannot record an outcome. The console stays stuck in progress, configuration stays locked across restarts, and the journal logs `PariNS updater: journal unreadable`. Recover with these steps:

1. Inspect the cause:
   ```sh
   sudo journalctl -u parins-updater.service | grep 'journal unreadable'
   ```
2. Fix the underlying problem, such as a full disk or wrong permissions on `/var/lib/parins-updater`.
3. Leave `/var/lib/parins-updater/private/journal.json` untouched.
4. Run the [install command](deployment.md#install) for the current or a newer release.

Once the installer passes readiness, the stuck update closes and settings unlock. If the installer still cannot read the journal, restore `/var/lib/parins-updater` from backup.

### Service fails to start after a reboot or crash

Check `parins-update-recovery.service` logs and the phase in `/var/lib/parins-updater/status.json` before touching any binary:

- If the phase is a non-terminal `staged` operation, boot recovery marks it interrupted and starts the recorded installed binary. Wait for `parins-update-recovery.service` to finish before starting `parins-managed.service`.
- If the phase is `manual_required`, recovery checks the live binary against `installed.sha256`. When they match, fix the stated cause and run the full installer.
- If recovery reports `unknown installed binary`, restore a binary matching `installed.sha256`. When `last_operation.operation_id` is present, check `sudo sha256sum /opt/parins-managed/.update/ID/previous`, replacing `ID` with that operation ID. Use that copy only if its digest equals `installed.sha256`, or restore a matching binary from your backup.

To restore a verified `previous` copy and re-run recovery checks:

```sh
sudo install -m 0755 /opt/parins-managed/.update/ID/previous /opt/parins-managed/.parins.restore
sudo mv -f /opt/parins-managed/.parins.restore /opt/parins-managed/parins
sudo systemctl restart parins-update-recovery.service
sudo systemctl start parins-managed.service
```

If the journal already recorded `manual_required`, restoring the binary allows the service to start, and running the full installer closes the operation and unlocks settings.

The installer pauses `parins-updater.path` while running. If it reports that installation succeeded but the request path failed to re-enable, run the printed recovery command:

```sh
sudo systemctl reset-failed parins-updater.path parins-updater.service && sudo systemctl enable --now parins-updater.path
```

## Certificates

- **Import in the console.** Under **Encryption & certificates**, enable a listener and paste its PEM certificate chain and private key, up to 64 KiB each. PariNS verifies that they match and writes them privately under `certificates/` in the state directory. Configuration stores only the file reference, and the private key is never shown again or exported. PariNS keeps up to 32 imported identities. Delete unused files manually only after confirming neither the current nor rollback configuration references them. Clients still enforce expiry, hostname match, and trust chain.
- **Extended Key Usage check.** Inbound validation accepts a leaf certificate with no Extended Key Usage extension. If the extension is present, it must allow server authentication. This check applies on load and reload.
- **External certificate files.** You may reference files outside the state directory instead. Because the managed service runs as a dynamic user, grant read access through a group using a systemd drop-in that sets only `SupplementaryGroups=` under `[Service]`, such as `/etc/systemd/system/parins-managed.service.d/certs.conf`. That is the only drop-in the installer permits.
- **Live renewal.** PariNS has no ACME client and does not watch files on disk. After your renewal tool replaces the files, click **Reload certificates** in the console or run:

  ```sh
  sudo systemctl reload parins-managed.service
  ```

  Reload validates every configured certificate first and swaps all listeners atomically. On any error, the previous certificates remain active. Listeners, caches, sessions, and saved configuration stay intact, while new handshakes present the updated certificates. Confirm the result and active fingerprints in the console, because sending `SIGHUP` alone does not guarantee reload succeeded. In managed mode `SIGHUP` reloads only certificates, whereas in file mode it also reloads `filter_file`.

## Backups and the state directory

Managed state lives in `/var/lib/parins-managed`, backed by `/var/lib/private/parins-managed`, owned by systemd's dynamic user with private permissions:

| Path | Content |
| --- | --- |
| `state.json` | Admin password hash, current and previous rollback configurations, and saved revision. If the rollback copy is unreadable, status reports `rollback_unavailable` and rollback stays disabled until the next save. |
| `turnstile.json` | Turnstile site key and secret key when human verification is enabled. |
| `setup-token` | One-time setup token before first-run setup completes. |
| `certificates/` | Imported certificate identities. |
| `filter-subscriptions/` | Downloaded blocklists and subscription catalog. |
| `runtime/` | Statistics and query-log SQLite database plus the cache snapshot. |
| `update-state.json` | Online update progress state. |

Back up the entire state directory while the service is stopped:

```sh
sudo systemctl stop parins-managed.service
sudo tar -C /var/lib/private -czf parins-managed-backup.tar.gz parins-managed
sudo systemctl start parins-managed.service
```

Never edit live state files, and never start the managed service with `--config`. Exported TOML excludes account data. Restore a backup only onto the same or a newer PariNS version.

## Query log and storage

- **Query log.** Disabled by default. Enable it under **Logs & storage** or `[query_log]`. Each entry records client address, query name and type, transport, response code, latency, cache and filter outcomes, upstream used, inbound and outbound ECS, EDNS flags, and up to 16 answer records. The **Query log** page supports search, status filters, paging, clearing, and live refresh every 1, 3, 5, or 10 seconds over an authenticated WebSocket at `/api/query-log/live`. New entries appear after flush, up to `storage.flush_interval_ms` later. Each session allows 2 live sockets up to 16 total, and idle sockets close after 30 seconds. Treat query logs as private browsing data.
- **Sampling and cancellation.** Rejected UDP queries are sampled in the query log while `parins_udp_dropped_total` counts every drop. Queries cancelled after reaching the resolver are logged as dropped with path **cancelled** and their prior cache lookup result, including transport write failures. Prefetch traffic is never logged, and query details never enter metrics or service logs.
- **Retention and sizing.** Records remain until `max_entries`, `max_bytes`, `retention_secs`, or `storage.max_database_bytes` is hit first. The console shows the oldest retained record and cleanup reasons. `query_log.max_bytes` accepts 1 MiB to 2 GiB and cannot exceed `storage.max_database_bytes`. Treat `retention_secs` as an upper age limit rather than guaranteed coverage, and size byte limits from observed query rate, per-record size, indexes, WAL files, and free disk headroom. Before downgrading to a version capped at 512 MiB, lower `query_log.max_bytes` into that range.
- **Statistics.** **Overview** shows totals, cache hit rate, blocked counts, latency, and one-minute trends for 7 days by default. Hit rate counts fresh hits, stale hits, and queries that joined an in-flight coalesced query. **Cache** breaks those components out separately. Totals and trends survive restarts and store no domain names or client addresses.
- **Storage worker and WAL.** One background worker serializes writes to SQLite after DNS sends each reply. When the memory queue fills, new entries drop and increment drop counters. Row-limit eviction shares the insert transaction, while periodic cleanup, lowered limits, and explicit flush reclaim unused pages. Name searches have a 500 ms budget and yield on timeout so writes continue. At shutdown, cache snapshot saving and storage draining run in parallel under separate budgets. Clearing logs, totals, or trends resets only that category.
- **Database files.** SQLite runs in WAL mode with `FULL` synchronization. `observability.sqlite3-wal` and `observability.sqlite3-shm` count toward `max_database_bytes`. Keep them alongside the main database while running and never copy or delete them separately. Clean shutdown checkpoints and truncates the WAL.
- **Cache diagnostics.** Refresh diagnostics reset on each cache generation and stay out of persisted statistics and snapshots. Cache inspection does not mark entries used, and unused expired entries are counted when lookup or cleanup encounters them. Authenticated `/api/status` includes `diagnostics.cache.capacity.shards` with per-shard positive and negative resident entry and byte counts, limits, and admission evictions by `entry_limit`, `byte_limit`, `entry_and_byte_limit`, or `variant_limit`. Evictions distinguish `fresh`, `retained`, and `expired` victims and exclude expiry cleanup, replacement, and manual invalidation. Compare counter deltas only within the same `since_ms`. Resident counts include expired entries not yet reaped, and byte charges stay alive while a reader holds an evicted entry.
- **Low-space warning.** Storage status warns when free disk space drops below 10% or below `storage.max_database_bytes` plus twice `cache.persistence.max_bytes` plus 64 MiB.

## Metrics

Set `admin_listen` to a loopback address such as `127.0.0.1:9090` to expose two read-only HTTP endpoints with their own small connection limit:

- `GET /healthz` returns `ok` while the process is alive. It does not check DNS readiness.
- `GET /metrics` returns Prometheus text format. All counters reset to zero on process start, and no domain names, client addresses, or unbounded labels are exported.

For DNS readiness, read `dns_health` (`ready`, `state`, and `code`) from the authenticated console endpoint `GET /api/status`. PariNS does not send alerts itself.

| Metric | Type | Meaning |
| --- | --- | --- |
| `parins_requests_total`, `parins_completed_total`, `parins_cancelled_total` | counter | Queries entering the resolver, queries whose answer wrote to the transport, and queries cancelled before write completion including write failures. |
| `parins_responses_{noerror,nxdomain,servfail,refused,other}_total`, `parins_dropped_total` | counter | Responses by RCODE and queries that ended without a response. |
| `parins_cache_hits_total`, `parins_cache_misses_total` | counter | First cache lookup per query, counting disabled or bypassed cache as a miss. |
| `parins_cache_lookup_{fresh,stale,miss,bypass,disabled}_total` | counter | Cache lookup outcomes. |
| `parins_cache_store_{admitted,replaced,skipped,superseded_only}_total` | counter | Cache admission outcomes. |
| `parins_cache_reason_*_total` | counter | Bypass or skip reasons: `unsupported_query`, `unsupported_edns`, `ecs_unusable`, `ttl_zero`, `negative_without_soa`, `uncacheable_response`, `policy_disabled`, `capacity`, `variant_limit`, and `epoch_changed`. |
| `parins_query_blocked_total`, `parins_response_blocked_total` | counter | Queries blocked by question name and answers blocked by a CNAME target. |
| `parins_flight_{leaders,joined,rejected,bypassed}_total` | counter | Coalescing leaders, joiners, rejections at limits, and non-coalescable queries. |
| `parins_refresh_active` | gauge | Background refreshes and cache fills running in the current cache generation. |
| `parins_refresh_{scheduled,success,failure}_total` | counter | Background refreshes started, stored, and failed to store. |
| `parins_refresh_rejected_{duplicate,cooldown,budget}_total`, `parins_refresh_cancelled_fill_rejected_total` | counter | Prefetch rejections by reason, plus sent queries whose departed caller could not hand them off to the background budget. |
| `parins_refresh_{useful,unused}_total` | counter | Refreshed entries settled once as useful on a first fresh foreground hit, or unused if served stale or removed before a foreground hit. |
| `parins_upstream_operations_total`, `parins_upstream_failures_total`, `parins_upstream_timeouts_total`, `parins_ecs_retries_total` | counter | Upstream operations including TCP fallback and ECS retry, failed exchanges, timeouts, and ECS `/0` retries. |
| `parins_upstream_attempt_{succeeded,failed,cancelled,skipped}_total` | counter | Individual upstream attempt outcomes. |
| `parins_upstream_attempt_reason_*_total` | counter | Attempt failure reasons: `deadline`, `connect_io`, `tls`, `http_status`, `protocol_invalid`, `peer_closed`, `caller_cancelled`, `shutdown`, and `internal`. |
| `parins_udp_received_total`, `parins_tcp_received_total`, `parins_encrypted_received_total` | counter | Inbound UDP datagrams, TCP frames, and encrypted DNS messages received. |
| `parins_udp_dropped_total`, `parins_tcp_rejected_total`, `parins_encrypted_rejected_total`, `parins_connections_rejected_total` | counter | Global admission rejections. |
| `parins_source_queries_rejected_total`, `parins_source_connections_rejected_total`, `parins_source_table_full_total` | counter | Per-client limit rejections, where table-full is a subset. |
| `parins_quic_{doq,doh3}_*_total` | counter | DoQ and DoH3 lifecycle counters such as `incoming`, `admission_global_rejected`, `handshake_established`, `handshake_tls`, `stream_cancelled`, `stream_peer_cancelled`, `http_request_failed`, and `http_response_4xx`. `incoming` counts only validated client addresses. |
| `parins_request_inflight`, `parins_upstream_inflight` | gauge | Active DNS requests and upstream operations. |
| `parins_request_duration_seconds`, `parins_upstream_duration_seconds` | histogram | Latency histograms with buckets at 1, 5, 10, 50, 100, 500, 1000, and 5000 ms. |

In recursive mode, `/metrics` adds these metrics:

| Metric | Type | Meaning |
| --- | --- | --- |
| `parins_recursion_started_total` | counter | Recursive resolutions started. |
| `parins_recursion_answered_total`, `parins_recursion_negative_total`, `parins_recursion_bogus_total` | counter | Answered resolutions, NXDOMAIN or NODATA outcomes, and DNSSEC Bogus outcomes. |
| `parins_recursion_deadline_total` | counter | Resolutions stopped by query deadline or client cancellation. |
| `parins_recursion_shutdown_total` | counter | Resolutions stopped by shutdown. |
| `parins_recursion_failed_{timeout,referral,budget,other}_total` | counter | Recursive failures by cause: authoritative timeout, unfollowed referral, query or depth budget, or other failure. |
| `parins_recursion_outbound_sent_total` | counter | Authoritative queries sent. |
| `parins_recursion_outbound_rate_waits_total`, `parins_recursion_outbound_rate_wait_ms_total` | counter | Authoritative queries that waited for a rate token and total wait time in milliseconds. |
| `parins_recursion_outbound_inflight` | gauge | Admitted authoritative queries not yet finished, including those waiting for a rate token. |

Setting `[metrics] interval_secs` also prints periodic and shutdown JSON snapshots to stderr with `"event": "parins_metrics"`.

## Running recursive mode

Changing `[resolution] mode` restarts DNS with an empty cache, so initial queries are slower until caches warm up. Before switching:

- Allow outbound UDP and TCP port 53 to the internet, and allow IPv6 egress only if you enable `[recursion] ipv6 = true`. Authoritative queries are plaintext and carry the client subnet when ECS is on.
- Size `max_outbound_per_second` and `max_outbound_inflight` for your cache-miss rate, because a cold lookup issues several authoritative queries.
- Confirm that PariNS's own filter rules cover what you need, because upstream resolver filtering no longer applies.

If `parins_recursion_outbound_rate_waits_total` or `parins_recursion_deadline_total` rises under load, increase the outbound limits or `query_timeout_ms`. See [Resolution mode and recursion](configuration.md#resolution-mode-and-recursion).

## Troubleshooting

### Port 53 is in use

Setup or save fails with `CONFIG_CONFLICT` and saves nothing because another process, often `systemd-resolved`, holds port 53. Follow [Port 53 conflicts](deployment.md#port-53-conflicts).

### DNS degraded

`dns_health.state` is `degraded` with code `listener_unavailable` and `ready` is `false` when at least one listener is down while others still serve. Inspect `listeners` in `/api/status` or on **Overview** for each listener's address and error, such as `Address in use`. PariNS retries the failed listener after 5 seconds and doubles the interval up to 60 seconds, logging repeated errors at most once every ten minutes. Online updates require every listener up and roll back while degraded.

If DNS fails to start with code `startup_failed`, such as an unreadable certificate, PariNS retries the saved configuration on the same backoff until it succeeds or you apply a fixed configuration.

### DNS stopped after a rule problem

If saved local rules cannot compile, such as an unreadable `filter_file`, DNS stays stopped and status displays `DNS unavailable` with the reason. Fix the rules and apply the configuration again.

### Subscriptions after an update

Subscriptions pause while an update runs and reopen automatically when it completes. If `subscription_storage_unavailable` persists on **Blocking rules**, check free space and permissions on the state directory disk.

### Storage shows "checking"

On startup, PariNS runs an integrity check on an existing database in the background while DNS serves immediately. During the check, storage reports `checking` and buffers new records in the bounded memory queue, where overflow drops and counts. Only shutdown cancels the check. If the check ends in `unavailable`, read the reported error.

### Accepts paused

The log line `PariNS accept paused: ...; raise the open-file limit or lower max_tcp_connections` means the process exhausted file descriptors. Listeners pause accepting briefly and keep serving. Raise `LimitNOFILE` for the service or lower `max_tcp_connections`.

### Cache snapshot not saved

`cache snapshot not saved: shutdown not quiescent (<cause>)` means active queries outlived `shutdown_grace_ms` at shutdown, so the next start begins with an empty cache. Increase `shutdown_grace_ms` if this happens often.
