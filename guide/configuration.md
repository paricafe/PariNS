# Configuration

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md)

The console and file mode use the same TOML schema. In managed mode you edit it in
the console's settings pages or under **Advanced (TOML)**; in file mode, start from
[parins.example.toml](../parins.example.toml). Unknown keys and invalid values are
rejected before anything is applied. Relative file paths resolve against the
configuration file's directory in file mode and against the state directory in
managed mode.

These keys have no default and must be present: `listen`, `query_timeout_ms`,
`tcp_io_timeout_ms`, `shutdown_grace_ms`, `max_inflight`, `max_tcp_connections` and
`upstreams.servers`. Every other key may be omitted and takes the default shown.
Sizes are in bytes (MiB = 1,048,576 bytes).

Sections: [Upstreams](#upstreams) · [Resolution mode and recursion](#resolution-mode-and-recursion)
· [Listeners and encryption](#listeners-and-encryption) · [Cache and ECS sharing](#cache-and-ecs-sharing)
· [Prefetch and serve stale](#prefetch-and-serve-stale) · [Query coalescing](#query-coalescing)
· [Filtering and subscriptions](#filtering-and-subscriptions) · [Limits](#limits)
· [Storage and logs](#storage-and-logs) · [Updates](#updates)

## Upstreams

`[upstreams]` lists the resolvers PariNS forwards to. All endpoints should give
equivalent answers, because they share one cache.

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `servers` | (required) | 1–32 entries | Upstream endpoints, one per entry (syntax below). |
| `mode` | `"weighted"` | `weighted`, `parallel` | Weighted picks one endpoint per query by smooth weighted round-robin. Parallel sends to all endpoints and uses the first NOERROR or NXDOMAIN answer. |
| `bootstrap` | `[]` | up to 8 `IP:port` | Plain DNS servers used only to resolve hostname endpoints. Required when any endpoint is a hostname. |
| `prefer_h3` | `false` | | Try HTTP/3 first for `https://` endpoints, with verified HTTP/2 as fallback. |
| `max_parallel` | `32` | 1–32 | Parallel mode only: endpoints queried at once; must be at least the number of servers. |
| `max_extra_inflight` | `128` | 1–65536 | Parallel mode only: extra requests (beyond the first of each query) in flight across all queries. When used up, fewer endpoints are queried. |
| `ca_file` | none | path | PEM roots that replace the built-in WebPKI roots for encrypted upstreams. |

`[upstreams.dot_pool]` reuses authenticated connections to each `tls://` endpoint:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | | Reuse DoT connections instead of one connection per query. |
| `max_connections` | `8` | 1–256 | Connections per DoT endpoint; each carries one query at a time. |
| `idle_timeout_ms` | `30000` | 1–600000 | An idle connection older than this is not reused (checked when a query needs one). |

A reused connection that the server has already closed is the only case in which a
sent query is tried again: once, on a new connection, within the query deadline.

**Endpoint syntax.** One endpoint per entry, optionally followed by ` weight=N`
(1–1000, default 1). IPv6 literals go in brackets.

| Form | Transport | Default port |
| --- | --- | --- |
| `192.0.2.53` or `udp://HOST[:PORT]` | UDP, with TCP for truncated answers | 53 |
| `tcp://HOST[:PORT]` | TCP | 53 |
| `tls://HOST[:PORT]` | DNS-over-TLS | 853 |
| `https://HOST[:PORT][/PATH]` | DNS-over-HTTPS (path defaults to `/dns-query`) | 443 |
| `quic://HOST[:PORT]` | DNS-over-QUIC | 853 |

```toml
[upstreams]
servers = ["https://dns.example/dns-query weight=2", "tls://192.0.2.53"]
bootstrap = ["192.0.2.1:53"]
```

Behavior:

- Hostname endpoints are resolved only through `bootstrap`, never the host's system
  resolver. Credentials, query strings, fragments and duplicate endpoints are
  rejected, as is an endpoint or bootstrap address that points at a PariNS listener.
  A loop through NAT or another alias cannot be detected; avoid it.
- Encrypted transports verify the certificate for the hostname or IP in the URL and
  never fall back to plaintext.
- **A query that may have reached an upstream is never sent again**, with one
  exception. A hostname endpoint moves to its next bootstrap address only when
  nothing was sent (connect, TLS or QUIC handshake, or stream open failed). The
  exception: when a reused DoT pool connection turns out to have been closed by the
  server (end of stream, reset, aborted, broken pipe or not connected), the query is
  retried once on a new connection within its deadline. Every other sent query
  fails without a resend.
- **HTTP/3** (`prefer_h3`): connecting and sending over HTTP/3 has a budget of
  250 ms, or half the remaining query time if that is shorter. A failure before the
  query was sent falls back to verified HTTP/2. A failure after sending is that
  query's upstream failure, with no HTTP/2 resend. Any HTTP/3 failure pauses HTTP/3
  for that endpoint for 30 seconds. A definite refusal such as 405 stops HTTP/3 for
  that endpoint until the upstream pool is rebuilt by the next DNS restart. Alternate
  ports from Alt-Svc or HTTPS/SVCB records are not used.
- DoH sends RFC 8484 GET requests. An unusually large query can exceed an upstream's
  URL or header limits and fail with 414 or 431; that affects only that query.
- Queries to upstreams advertise an EDNS UDP size of at most 1232 bytes. The EDNS TCP
  keepalive option is removed from client queries and from upstream answers on every
  transport. Encrypted hops add EDNS padding when the client asked for it (128-byte
  blocks upstream); plaintext hops carry none.
- All upstream work of a query shares one deadline, `query_timeout_ms`. A failed or
  timed-out exchange returns SERVFAIL, unless [serve stale](#prefetch-and-serve-stale)
  applies.

## Resolution mode and recursion

`[resolution] mode` chooses where answers come from: `"forward"` (default) uses
`[upstreams]`; `"recursive"` resolves from the root servers with DNSSEC validation.
`[upstreams]` stays configured in both modes. Changing the mode restarts DNS and
starts with an empty cache, because forwarded and recursive answers never share a
cache or snapshot.

`[recursion]` applies only in recursive mode:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `ipv6` | `false` | | Also query authoritative servers over IPv6. Enable only with working IPv6 egress. |
| `max_outbound_per_second` | `500` | 1–10000 | Queries per second to all authoritative servers together. |
| `max_outbound_inflight` | `256` | 1–4096 | Authoritative queries admitted at once, including those waiting for a rate token. |
| `max_depth` | `24` | 4–64 | Referral and CNAME depth of one resolution. |
| `max_queries_per_request` | `200` | 1–1000 | Authoritative queries one lookup may send, including delegations, name server addresses and CNAME targets. Each DS or DNSKEY lookup has its own budget; the outbound limits cap the total. |
| `cache_entries` | `65536` | 1024–1048576 | Delegation, name server and DNSSEC key responses kept by the recursor (entries, not bytes). |

Behavior:

- Only PariNS's own filtering applies. Queries to authoritative servers are plaintext
  UDP and TCP on port 53. With ECS on, the client subnet is sent only with queries
  for the answer itself (the question and its CNAME targets), never with delegation,
  name server address or DNSSEC key lookups.
- An authoritative answer without an ECS option does not depend on the client
  network and is shared within its address family. Learned ECS sharing does not
  apply in this mode.
- Answers are validated from the built-in root trust anchor. DNSSEC Bogus data is
  answered with SERVFAIL and Extended DNS Error 6, never with stale data. AD is set
  only for Secure data, only for clients that set DO or AD, and never after filtering
  rewrote the answer. A client that sets CD gets unchecked data without AD, cached
  separately.
- For clients that set DO, positive answers keep the NSEC or NSEC3 records and their
  RRSIGs in the authority section, so the client can validate a wildcard answer.
  Other authority records of positive answers are dropped.
- Each authoritative server gets 1.5 seconds before the next one is tried, and every
  resolution stays within `query_timeout_ms`. Timeouts and unreachable servers are
  upstream failures for serve stale.
- QNAME minimisation is relaxed, because some authoritative servers answer NXDOMAIN
  for names that only have descendants. Root hints and the trust anchor are built in
  and change only with a PariNS release.

## Listeners and encryption

PariNS listens for plain DNS on one address and, optionally, for DoT, DoH (HTTP/2
and HTTP/3) and DoQ. Each listener needs its own address and port.

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `listen` | (required) | `IP:port` | Plain DNS over UDP and TCP on the same address and port. Port 0 picks a free port, printed at startup. |
| `admin_listen` | none | loopback `IP:port` | Local `/metrics` and `/healthz` endpoint; see [Metrics](operations.md#metrics). |
| `[dot]` `listen`, `cert_file`, `key_file` | off | | DNS-over-TLS (TCP). |
| `[doh]` `listen`, `cert_file`, `key_file` | off | | DNS-over-HTTPS over HTTP/2 (TCP). Enables console HTTPS in managed mode. |
| `[doh]` `http3` | `false` | | Also serve DoH over HTTP/3, on UDP at the same address and port. |
| `[doq]` `listen`, `cert_file`, `key_file` | off | | DNS-over-QUIC (UDP). May share a port number with DoT, since one is TCP and the other UDP. |
| `[web]` `public_host` | none | one domain or IP | The host name the browser uses for console HTTPS; required in managed mode when `[doh]` is set. See [Console HTTPS](deployment.md#console-https). |

```toml
[doq]
listen = "0.0.0.0:853"
cert_file = "/path/to/fullchain.pem"
key_file = "/path/to/privkey.pem"
```

Behavior:

- UDP answers follow the client's EDNS payload size, capped at 1232 bytes (512
  without EDNS); a larger answer returns a truncated reply so the client retries over
  TCP. On Linux and macOS, UDP replies leave from the address and interface the query
  arrived on, including on wildcard binds; other platforms need a specific address.
- Only ordinary single-question queries are served. ANY, AXFR, IXFR and signed
  queries get REFUSED, other opcodes NOTIMP, malformed queries FORMERR.
- DoH serves `/dns-query` with GET (`dns=` parameter) or POST
  (`application/dns-message`), over HTTP/2 or HTTP/3 only, with `Cache-Control:
  no-store`. Responses advertise `Alt-Svc: h3` only while the DoH3 listener is
  actually running.
- DoQ and DoH3 answer a client whose address QUIC has not yet validated with a
  stateless Retry (one extra round trip) before it takes a connection slot or source
  budget. 0-RTT and connection migration are off.
- Encrypted listeners add EDNS padding (468-byte blocks) when the client asked for
  it, bounded only by the maximum DNS message size.
- Client identity is always the socket peer; `X-Forwarded-For` and `Forwarded` are
  ignored.
- Each listener runs on its own. If one cannot bind or stops, the others keep serving
  while it is retried; see [DNS degraded](operations.md#dns-degraded).
- Certificates are checked before use and can be replaced without restarting; see
  [Certificates](operations.md#certificates).

## Cache and ECS sharing

PariNS caches whole responses. With ECS (EDNS Client Subnet) on, answers are kept
per client network so each network gets the answer the upstream gave for it.

`[ecs]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | | Send a client subnet derived from the socket peer to upstreams. Off: ECS is removed from queries. |
| `ipv4_prefix` | `24` | 0–32 | Longest IPv4 prefix sent upstream. |
| `ipv6_prefix` | `56` | 0–128 | Longest IPv6 prefix sent upstream. |

`[cache]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `true` | | Turn the cache on or off. Overrides every cache rule. |
| `max_entries` | `32768` | 1–262144 | Cached answers (each ECS variant counts). |
| `max_bytes` | `33554432` (32 MiB) | 512–1073741824 | Charged size of cached answers plus per-entry overhead. Not process memory. |
| `max_variants` | `64` | 1–256, at most `max_entries` | ECS variants kept for one question. |
| `max_ttl_secs` | `3600` | 1–86400 | Cap on positive answer TTLs. |
| `negative_ttl_cap_secs` | `300` | 1–86400 | Cap on NXDOMAIN and NODATA TTLs. |
| `shards` | `4` | 1–64 | Independently locked cache partitions. |
| `negative_percent` | `20` | 0–90 | Share of entries and bytes reserved for negative answers. |

`[cache.persistence]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `true` | | Save fresh answers at a clean shutdown and restore them at the next start. |
| `max_bytes` | `33554432` (32 MiB) | 1 MiB–512 MiB | Size limit of the snapshot file. |

`[cache.ecs_sharing]` (learned ECS sharing, forwarding mode only):

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | | Learn which answers are the same for every network and store them once per address family. |
| `min_networks` | `3` | 2–8 | Disjoint networks that must receive the same answer. |
| `revoke_cooldown_secs` | `3600` | 60–604800 | After a revocation, how long the name is not learned again. |

`[[cache.rules]]` (up to 256) overrides cache behavior per name:

| Key | Default | Meaning |
| --- | --- | --- |
| `name` | (required) | Domain name. |
| `suffix` | `false` | Also match subdomains. |
| `qtype` | any | Record type, such as `"A"`. |
| `bypass` | `false` | Do not cache matching answers. |
| `max_ttl_secs`, `negative_ttl_cap_secs` | global value | TTL caps for matching answers (1–86400). |
| `prefetch`, `stale` | global value | Turn prefetch or serve stale on or off for matching answers. |

The most specific rule wins: exact names before suffixes, then more labels, then a
rule with `qtype` before one without, then the first declared.

Behavior:

- **Keys.** Each question (name, type, class) with its DO, CD and RD flags and EDNS
  presence has its own entries. IPv4 ECS, IPv6 ECS, no-ECS queries and privacy `/0`
  queries never share answers.
- **ECS scope.** An answer is reused by clients inside the scope the upstream
  returned. When an upstream omits ECS in its answer to a query that carried a
  nonzero subnet, the answer is reused only for that exact outgoing subnet. A newer
  answer replaces overlapping older ones, even if the newer one cannot be cached.
- **Client ECS.** A client may send its own ECS option. A nonzero subnet must contain
  the client's address, or the query is refused; `/0` keeps the client private. If an
  upstream refuses a query with a nonzero subnet, PariNS retries once with `/0` and
  does not cache that result.
- **Learned ECS sharing.** For answers returned without ECS, PariNS compares the
  normalized answer (response code and answer records, ignoring TTL and order) that
  different networks receive. Once `min_networks` disjoint networks, spanning at
  least two /16 (IPv6 /32) prefixes, received the same answer while it was valid,
  one copy serves every nonzero-ECS query of that family. Any different answer, or
  an answer with an explicit nonzero scope, revokes it at once: networks are cached
  separately again and the name is not relearned for `revoke_cooldown_secs`. Shared
  evidence lasts one hour, then must be confirmed again. The learning table holds at
  most 65,536 names, is not saved in snapshots, and is cleared with the cache.
  The **Cache** page shows each name as observing, shared or revoked.
- **Eligibility.** Truncated answers, SERVFAIL and REFUSED, zero TTLs, negative
  answers without a SOA, and queries with Cookie or unknown EDNS options are not
  cached. Negative TTLs are the smallest of the SOA TTL, the SOA MINIMUM and
  `negative_ttl_cap_secs`. A CNAME chain ending in NXDOMAIN or NODATA is cached only
  when the chain is unambiguous and backed by a matching SOA. Lower TTLs are never
  raised.
- **Budgets.** Each shard has separate positive and negative budgets; negative
  answers never evict positive ones. Memory grows with use; the budget is not
  reserved at startup.
- **Persistence.** The snapshot is written only at a clean, quiescent shutdown and is
  consumed before DNS starts serving, so it is used once and a crash or forced stop
  starts with an empty cache. Restored answers keep their remaining TTL. A snapshot from a build
  with different cache semantics is skipped.
- Changing only cache settings replaces the cache without restarting listeners.

## Prefetch and serve stale

Prefetch refreshes popular answers shortly before they expire. Serve stale answers
from an expired entry when the upstream fails ([RFC 8767](https://www.rfc-editor.org/rfc/rfc8767)).
Both are off by default.

`[cache.prefetch]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | | Refresh hot positive answers in the background. |
| `min_hits` | `1` | 1–1000000 | Cache hits an entry needs before it is prefetched. |
| `remaining_percent` | `25` | 1–90 | Refresh when less than this share of the TTL remains. |
| `max_inflight` | `8` | 1–256 | Refreshes running at once. |
| `rate_per_sec` | `20` | 1–10000 | Refreshes started per second. |
| `backoff_secs` | `5` | 1–3600 | Wait before refreshing the same entry again after a failure. |

`[cache.stale]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | | Answer from expired entries when the upstream fails. |
| `retention_secs` | `300` | 1–604800 | How long after expiry an answer is kept for this. |
| `reply_ttl_secs` | `30` | 1–300 | TTL of a stale answer. |

Behavior:

- Serve stale applies after a timeout, transport failure or SERVFAIL (in recursive
  mode, any failure except DNSSEC Bogus). REFUSED, negative answers and ECS privacy
  retries are never replaced by stale data, and stale data never crosses ECS
  namespaces.
- Refreshes are cancelled when the cache is replaced or DNS stops. With coalescing
  on, a client query for an entry being refreshed joins that refresh.

## Query coalescing

Identical cache misses share one upstream request. Each caller still gets its own
ID, ECS echo and filtering.

`[coalescing]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `true` | | Share upstream work between identical queries. |
| `max_groups` | `128` | 1–65536 | Distinct queries being shared at once. |
| `max_waiters` | `64` | 1–65536 | Callers per group, including the first. |

At either limit, new queries get SERVFAIL without upstream traffic. Queries with
Cookie or unknown EDNS options, or a class other than IN, are not shared. Cancelling
one caller does not cancel the others; the request ends when the last caller leaves.

## Filtering and subscriptions

Filtering blocks queries by name, from local rules and from online subscriptions.
A blocked query gets NOERROR with an empty answer (no SOA). Blocked answers are never
cached, and cached upstream answers are never altered by filtering.

`[filter]` (local rules):

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Apply the local rules below. |
| `block_exact` | `[]` | Block exactly these names. |
| `block_suffix` | `[]` | Block these names and their subdomains. |
| `allow_exact` | `[]` | Never block exactly these names. |
| `allow_suffix` | `[]` | Never block these names and their subdomains. |
| `filter_file` (top level) | none | A TOML file with the same five keys; when set, it replaces the inline `[filter]`. |

Rules are ASCII names (letters, digits, `_`, `-`; punycode for IDNs), matched by DNS
labels, ignoring case and a final dot. Wildcards, Adblock syntax, hosts lines, URLs
and regular expressions are rejected. Limits: 100,000 rules and 8 MiB of rule text
(also for `filter_file`). A matching allow rule wins for that name. The answer's
CNAME chain is checked too, so allowing a name does not allow a blocked CNAME target.

If saved local rules cannot be compiled (for example, an unreadable `filter_file`),
DNS stays stopped on every start path (startup, retry, and rollback after a failed
apply) and the status shows the reason. Fix the rules and apply the configuration
again.

`[filter_subscriptions]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `true` | | Apply enabled subscription sources. With no sources, nothing is downloaded. |
| `max_rules` | `1000000` | 1–5000000 | Rules across all sources, counted before deduplication. |
| `max_memory_bytes` | `134217728` (128 MiB) | up to 512 MiB | Memory for building and holding rule sets. |
| `max_disk_bytes` | `268435456` (256 MiB) | 256 KiB–2 GiB | Disk space for downloaded lists. |

Each `[[filter_subscriptions.sources]]` (up to 16):

| Key | Default | Meaning |
| --- | --- | --- |
| `id` | (required) | 1–32 lowercase letters, digits or `-`; unique. |
| `name` | (required) | Display name, up to 80 characters. |
| `url` | (required) | HTTPS URL of the list. |
| `format` | (required) | `domain_list` or `hosts_blocklist`. |
| `enabled` | `true` | Use this source. |
| `auto_update` | `true` | Download updates on a schedule. |
| `update_interval_hours` | `24` | 1–168. |

- `domain_list`: one domain per line; `example.org` matches exactly and
  `.example.org` also matches subdomains. `hosts_blocklist`: hosts lines with
  `0.0.0.0` or `127.0.0.1` (or their IPv6 equivalents). Neither is Adblock syntax.
- Each list may be 16 MiB, all lists together 32 MiB, and each line 4096 bytes.
  Downloads use verified HTTPS to public addresses only and ignore proxy settings.
- In the console, add a source, verify the download, then enable it and save.
  Verification alone does not activate rules. A failed update keeps the previous
  rules. Enabled local allow rules also override subscription blocks.
- Downloaded lists and the catalog live under `filter-subscriptions/` in the state or
  data directory. If the catalog is damaged, keep the directory for diagnosis and
  restore it from a backup, or turn subscriptions off; do not edit it.

## Limits

Global limits and timeouts are top-level keys (all required):

| Key | Example value | Range | Meaning |
| --- | --- | --- | --- |
| `query_timeout_ms` | `2000` | 1–60000 | Deadline of one query, including all upstream or recursive work. |
| `tcp_io_timeout_ms` | `5000` | 1–60000 | Limit for each TCP frame read or write, idle time, and encrypted handshakes and requests. |
| `shutdown_grace_ms` | `3000` | 1–60000 | Time active queries get to finish when DNS stops. |
| `max_inflight` | `256` | 1–65536 | Queries in progress across all transports. At the limit, UDP queries are dropped and TCP queries get SERVFAIL. |
| `max_tcp_connections` | `64` | 1–65536 | TCP, DoT, DoH and QUIC connections, including handshakes. |

`[source_limits]` sets per-client limits, shared by all DNS listeners and keyed only
by the socket peer address:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | | Turn per-client limits on. |
| `rate_per_sec` | `100` | 1–1000000 | Sustained queries per second per client. |
| `burst` | `200` | 1–1000000 | Queries a client may send at once. |
| `max_inflight` | `32` | 1–65536 | Queries in progress per client. |
| `max_connections` | `8` | 1–65536 | Connections per client, including pending handshakes. |
| `max_sources` | `4096` | 1–65536 | Clients tracked at once; new clients are rejected when the table is full. |
| `ipv4_prefix` | `32` | 0–32 | IPv4 addresses in one prefix share a budget. |
| `ipv6_prefix` | `64` | 0–128 | IPv6 addresses in one prefix share a budget. |

- Cache hits and blocked queries still count. Rejected UDP queries are dropped;
  reliable transports answer SERVFAIL (DoH keeps HTTP 200 with a DNS body).
- These are per-process admission limits, not a DDoS defense. Clients behind NAT
  share a budget. Keep firewall and provider protection in place.
- PariNS raises its soft open-file limit to the hard limit at startup.
  `deploy/parins.service` sets `LimitNOFILE=524288`; the managed unit relies on
  systemd's default hard limit. When descriptors run out, listeners pause accepting
  briefly and keep serving, and the log says so at most once a minute (`raise the
  open-file limit or lower max_tcp_connections`).

## Storage and logs

PariNS keeps statistics and the optional query log in a private SQLite database
(`runtime/` in the managed state directory, or the file-mode data directory). See
[Query log and storage](operations.md#query-log-and-storage).

`[storage]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `max_database_bytes` | `134217728` (128 MiB) | 16 MiB–4 GiB | Database size limit, not counting its journal. |
| `flush_interval_ms` | `1000` | 100–5000 | How often queued records are written. |
| `cleanup_interval_secs` | `60` | 10–3600 | How often expired and over-limit data is removed. |
| `queue_max_entries` | `4096` | 128–65536 | Records waiting to be written; more are dropped and counted. |
| `queue_max_bytes` | `8388608` (8 MiB) | 1–64 MiB | Size of records waiting to be written. |

`[query_log]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | | Record each query (client address, name, result, timing, upstream). |
| `max_entries` | `100000` | 1–1000000 | Records kept. |
| `max_bytes` | `67108864` (64 MiB) | 1 MiB–512 MiB, at most `max_database_bytes` | Space for records. |
| `retention_secs` | `86400` | 60–2592000 | Maximum record age. |

`[statistics]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `retention_secs` | `604800` (7 days) | 3600–31536000 | Age of the oldest trend point. |
| `max_samples` | `10080` | 60–525600 | Trend points kept (one per minute). |
| `reset_interval_days` | `0` | 0–3650 | Reset totals every N days; 0 never does. Trends are not affected. |

`[metrics]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `interval_secs` | `0` | 0–3600 | Print a JSON metrics snapshot to stderr every N seconds and at shutdown; 0 turns it off. |

## Updates

`[updates]` controls the software update check in managed mode. File mode never
checks.

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `auto_check` | `true` | | Check the official GitHub releases for a newer version. Updates are never installed automatically. |
| `check_interval_hours` | `6` | 1–168 | Time between checks. |

Changing only `[updates]` keeps DNS, caches and certificates as they are, and a
stopped DNS stays stopped. See [Online updates and recovery](operations.md#online-updates-and-recovery).
