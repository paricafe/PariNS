# Configuration

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md) · [Architecture](architecture.md)

The web console and file mode share one TOML schema. In managed mode, edit settings in the console pages or under **Advanced (TOML)**. In file mode, start from [parins.example.toml](../parins.example.toml). PariNS rejects unknown keys and invalid values before applying any change. Relative file paths resolve against the configuration file's directory in file mode and against the state directory in managed mode.

Seven keys have no default and must be set: `listen`, `query_timeout_ms`, `tcp_io_timeout_ms`, `shutdown_grace_ms`, `max_inflight`, `max_tcp_connections`, and `upstreams.servers`. Every other key is optional and takes the default listed below. Sizes are in bytes, where 1 MiB equals 1,048,576 bytes.

Jump to a section: [Upstreams](#upstreams) · [Resolution mode and recursion](#resolution-mode-and-recursion) · [Listeners and encryption](#listeners-and-encryption) · [Cache and ECS sharing](#cache-and-ecs-sharing) · [Prefetch and serve stale](#prefetch-and-serve-stale) · [Query coalescing](#query-coalescing) · [Filtering and subscriptions](#filtering-and-subscriptions) · [Limits](#limits) · [Storage and logs](#storage-and-logs) · [Updates](#updates).

## Upstreams

`[upstreams]` lists the resolvers PariNS forwards to. Configure endpoints that return equivalent answers, because they share one cache.

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `servers` | Required | 1 to 32 entries | Upstream endpoints, one per entry. See endpoint syntax below. |
| `mode` | `"weighted"` | `weighted`, `parallel` | `weighted` selects one endpoint per query by smooth weighted round-robin. `parallel` queries all endpoints and returns the first NOERROR or NXDOMAIN answer. |
| `bootstrap` | `[]` | Up to 8 `IP:port` | Plain DNS servers used only to resolve hostname endpoints. Required when any endpoint uses a hostname. |
| `prefer_h3` | `false` | Boolean | Try HTTP/3 first for `https://` endpoints and fall back to verified HTTP/2 before sending. |
| `max_parallel` | `32` | 1 to 32 | Maximum endpoints queried at once in `parallel` mode. Must be at least the number of entries in `servers`. |
| `max_extra_inflight` | `128` | 1 to 65536 | Extra concurrent upstream requests beyond the first of each query in `parallel` mode. When exhausted, PariNS queries fewer endpoints. |
| `ca_file` | None | File path | PEM root bundle that replaces built-in WebPKI roots for encrypted upstreams. Must resolve to a regular file of at most 1 MiB. |

`[upstreams.dot_pool]` reuses authenticated connections for each `tls://` endpoint:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | Boolean | Reuse DoT connections instead of opening one connection per query. |
| `max_connections` | `8` | 1 to 256 | Pooled connections per DoT endpoint. Each connection carries one query at a time. |
| `idle_timeout_ms` | `30000` | 1 to 600000 | Maximum idle age before a pooled connection is discarded when a query requests one. |

A closed reused DoT connection is the only case where PariNS resends a sent query. It retries once on a fresh connection within the query deadline.

### Endpoint syntax

Put one endpoint in each entry, optionally followed by ` weight=N` where `N` is 1 to 1000 and defaults to 1. Wrap IPv6 literals in brackets.

| Form | Transport | Default port |
| --- | --- | --- |
| `192.0.2.53` or `udp://HOST[:PORT]` | UDP, with TCP fallback for truncated answers | 53 |
| `tcp://HOST[:PORT]` | TCP | 53 |
| `tls://HOST[:PORT]` | DNS-over-TLS | 853 |
| `https://HOST[:PORT][/PATH]` | DNS-over-HTTPS, defaulting to `/dns-query` | 443 |
| `quic://HOST[:PORT]` | DNS-over-QUIC | 853 |

```toml
[upstreams]
servers = ["https://dns.example/dns-query weight=2", "tls://192.0.2.53"]
bootstrap = ["192.0.2.1:53"]
```

### Upstream behavior

- **Hostname bootstrap and validation.** PariNS resolves hostname endpoints only through `bootstrap`, never through the host resolver. It rejects credentials, query strings, fragments, duplicate endpoints, and any endpoint or bootstrap address that points at a PariNS listener. Loops routed through NAT or another alias cannot be detected automatically.
- **Strict TLS verification.** Encrypted transports verify the certificate for the hostname or IP in the endpoint URL and never fall back to plaintext.
- **No resend after sending.** A hostname endpoint tries its next bootstrap address only if nothing was sent, such as a failed connect, handshake, or stream open. Once a query may have reached an upstream, PariNS does not resend it, with one exception. If a reused DoT pool connection was closed by the server before the first response byte with EOF, reset, abort, broken pipe, or not-connected error, PariNS retries once on a new connection within the query deadline.
- **HTTP/3 fallback and cooldown.** With `prefer_h3 = true`, connecting and sending request headers gets 250 ms, or half the remaining query deadline if shorter. The body and response share the remaining deadline. Failures before sending fall back to verified HTTP/2. Failures after sending count as upstream failures without an HTTP/2 retry. Pre-send and connection failures pause HTTP/3 for that endpoint for 2 seconds, doubling up to 30 seconds on consecutive failures, and a successful HTTP/3 answer resets the cooldown. A stream reset or HTTP 5xx or 429 on an open connection does not trigger a cooldown. A definitive HTTP 4xx refusal such as 405 disables HTTP/3 for that endpoint until the next DNS restart rebuilds the pool. Alternate ports from `Alt-Svc` or HTTPS and SVCB records are ignored.
- **DoH method selection.** DoH uses RFC 8484 GET when the encoded query fits in the client URI and POST otherwise. An upstream with a lower URL or header limit may still reject GET with HTTP 414 or 431.
- **EDNS normalization.** Upstream queries advertise an EDNS UDP size of at most 1232 bytes. PariNS strips EDNS TCP keepalive from client queries and upstream answers on all transports. Encrypted upstream hops add 128-byte EDNS padding blocks when the client requested padding, while plaintext hops carry no padding.
- **Shared deadline.** All upstream work for one query shares `query_timeout_ms`. A failed or timed-out exchange returns SERVFAIL unless [serve stale](#prefetch-and-serve-stale) applies.

## Resolution mode and recursion

`[resolution] mode` sets the resolution source. `"forward"` is the default and uses `[upstreams]`. `"recursive"` resolves from the built-in root servers with DNSSEC validation. `[upstreams]` remains configured in both modes. Switching modes restarts DNS with an empty cache, because forwarded and recursive answers never share a cache or snapshot.

`[recursion]` applies only in recursive mode:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `ipv6` | `false` | Boolean | Query authoritative servers over IPv6 as well as IPv4. Enable only with working IPv6 egress. |
| `max_outbound_per_second` | `500` | 1 to 10000 | Global queries per second across all authoritative servers. |
| `max_outbound_inflight` | `256` | 1 to 4096 | Authoritative queries admitted at once, including queries waiting for a rate token. |
| `max_depth` | `24` | 4 to 64 | Maximum referral and CNAME depth per resolution. |
| `max_queries_per_request` | `200` | 1 to 1000 | Authoritative queries per lookup across delegations, name server addresses, and CNAME targets. Each DS or DNSKEY lookup has its own budget, bounded by the global outbound limits. |
| `cache_entries` | `65536` | 1024 to 1048576 | Count of delegation, name server, and DNSSEC key responses held in recursor memory. |

### Recursive behavior

- **Plaintext outbound queries and ECS.** Only PariNS filtering rules apply in recursive mode. Queries to authoritative servers use plaintext UDP and TCP on port 53. When ECS is on, PariNS sends the client subnet only on answer queries for the question and its CNAME targets, never on delegation, name server address, or DNSSEC key lookups.
- **Authoritative ECS scope.** An authoritative answer without an ECS option does not depend on client network and is shared within its address family. Learned ECS sharing does not run in recursive mode.
- **DNSSEC validation.** PariNS validates from the built-in root trust anchor. DNSSEC Bogus responses return SERVFAIL with Extended DNS Error 6 and never serve stale data. PariNS sets AD only for Secure data, only when the client set DO or AD, and never after filtering rewrote the answer. Clients that set CD receive unchecked data without AD from a separate cache namespace.
- **Wildcard denial proofs.** When a client sets DO, positive answers keep NSEC or NSEC3 records and their RRSIGs in the authority section so the client can verify wildcard answers. Other authority records on positive answers are dropped.
- **Timeouts and minimisation.** Each authoritative server gets 1.5 seconds before the recursor tries the next server, and every lookup stays inside `query_timeout_ms`. Timeouts and unreachable servers count as upstream failures for serve stale. QNAME minimisation is relaxed because some authoritative servers return NXDOMAIN for non-empty terminals. Root hints and the root trust anchor are built into the binary and update with PariNS releases.

### Recursive limitations

- Zone cuts are discovered by probing level by level, and referrals returned on the final answer query are not followed. If probing stops above the real zone cut because a server refuses, fails, or disagrees with peers, the lookup fails and increments `parins_recursion_failed_referral_total`.
- If a DS or DNSKEY lookup required for validation times out or is unreachable, PariNS treats the result as Bogus with SERVFAIL and Extended DNS Error 6 until the recursor validation cache entry expires.
- Delegation, name server address, and DNSSEC key caches live only in memory, so lookups warm up from the root after a restart.
- In both forwarding and recursive modes, unknown algorithm numbers in a client EDNS DAU option are dropped when PariNS re-encodes the query.

## Listeners and encryption

PariNS listens for plain DNS on one address and optionally for DoT, DoH over HTTP/2 and HTTP/3, and DoQ. Assign each listener its own address and port.

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `listen` | Required | `IP:port` | Plain DNS over UDP and TCP on the same address and port. Port `0` chooses a free port and prints it at startup. |
| `admin_listen` | None | Loopback `IP:port` | Local `/metrics` and `/healthz` HTTP listener. See [Metrics](operations.md#metrics). |
| `[dot]` `listen`, `cert_file`, `key_file` | Off | Address and PEM paths | DNS-over-TLS on TCP. |
| `[doh]` `listen`, `cert_file`, `key_file` | Off | Address and PEM paths | DNS-over-HTTPS over HTTP/2 on TCP. Also enables console HTTPS in managed mode. |
| `[doh]` `http3` | `false` | Boolean | Serve DoH over HTTP/3 on UDP at the `[doh]` address and port. |
| `[doq]` `listen`, `cert_file`, `key_file` | Off | Address and PEM paths | DNS-over-QUIC on UDP. May reuse the DoT port number because DoT uses TCP. |
| `[web]` `public_host` | None | One domain or IP | Browser hostname for console HTTPS. Required in managed mode when `[doh]` is set. See [Console HTTPS](deployment.md#console-https). |

```toml
[doq]
listen = "0.0.0.0:853"
cert_file = "/path/to/fullchain.pem"
key_file = "/path/to/privkey.pem"
```

### Listener behavior

- **UDP replies.** UDP answers follow the client EDNS payload size up to 1232 bytes, or 512 bytes without EDNS. Larger answers set TC so the client retries over TCP. On Linux and macOS, UDP replies use the arrival destination address and interface even on wildcard binds. Other operating systems require a specific bind address.
- **Query validation.** PariNS answers only standard single-question queries. ANY, AXFR, IXFR, and signed queries receive REFUSED, other opcodes receive NOTIMP, and malformed queries receive FORMERR.
- **DoH endpoints.** DoH serves `/dns-query` via GET with the `dns` parameter or POST with `application/dns-message` over HTTP/2 or HTTP/3 only, with `Cache-Control: no-store`. Responses advertise `Alt-Svc: h3` only while the DoH3 listener is running.
- **QUIC address validation.** DoQ and DoH3 answer unvalidated client addresses with a stateless QUIC Retry before consuming a connection slot or source limit. 0-RTT and connection migration are disabled.
- **Encrypted padding and client identity.** Encrypted listeners add 468-byte EDNS padding blocks when the client requested padding, bounded by the maximum DNS message size. Client identity always comes from the socket peer. `X-Forwarded-For` and `Forwarded` headers are ignored.
- **Independent supervision and certificates.** Each listener has its own supervisor. If one listener fails to bind or exits, the others keep serving while it retries. See [DNS degraded](operations.md#dns-degraded). Certificates are validated before use and reload without restarting. See [Certificates](operations.md#certificates).

## Cache and ECS sharing

PariNS caches complete DNS responses. With EDNS Client Subnet enabled, answers stay separated by client network so each subnet receives the answer returned for it.

`[ecs]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | Boolean | Send a client subnet derived from the socket peer to upstreams. When false, PariNS removes ECS from outgoing queries. |
| `ipv4_prefix` | `24` | 0 to 32 | Maximum IPv4 prefix length sent upstream. |
| `ipv6_prefix` | `56` | 0 to 128 | Maximum IPv6 prefix length sent upstream. |

`[cache]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `true` | Boolean | Enable or disable the cache. Overrides all `[[cache.rules]]`. |
| `max_entries` | `262144` | 1 to 16777216 | Maximum cached answers, counting each ECS variant. |
| `max_bytes` | `268435456` (256 MiB) | 512 B to 64 GiB | Charged size of cached answers and per-entry overhead. Not process RSS. |
| `max_variants` | `64` | 1 to 4096, at most `max_entries` | Maximum ECS variants kept per question key. |
| `max_ttl_secs` | `86400` | 1 to 604800 | Upper cap on positive answer TTLs. |
| `negative_ttl_cap_secs` | `900` | 1 to 604800 | Upper cap on NXDOMAIN and NODATA TTLs. |
| `servfail_ttl_secs` | `30` | 0 to 300 | Cache duration for upstream SERVFAIL responses. Set `0` to disable. |
| `shards` | `16` | 1 to 256 | Number of independently locked cache shards. |
| `negative_percent` | `20` | 0 to 90 | Percentage of entries and bytes reserved for negative answers. |

`[cache.persistence]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `true` | Boolean | Save fresh answers on clean shutdown and restore them on next start. |
| `max_bytes` | `268435456` (256 MiB) | 1 MiB to 64 GiB | Maximum snapshot file size. |

`[cache.ecs_sharing]` applies only in forwarding mode:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | Boolean | Learn names that return identical answers across networks and store one copy per address family. |
| `min_networks` | `3` | 2 to 64 | Minimum disjoint networks that must observe the same answer before sharing begins. |
| `revoke_cooldown_secs` | `3600` | 60 to 2592000 | Cooldown after a revocation before PariNS can learn sharing for that name again. |

`[[cache.rules]]` allows up to 4096 per-name overrides. Each query evaluates rules in order, so large rule lists add lookup cost:

| Key | Default | Meaning |
| --- | --- | --- |
| `name` | Required | Domain name to match. |
| `suffix` | `false` | Also match subdomains of `name`. |
| `qtype` | Any | Optional record type such as `"A"`. |
| `bypass` | `false` | Skip cache lookup and storage for matching queries. |
| `max_ttl_secs`, `negative_ttl_cap_secs` | Global value | Per-rule TTL caps from 1 to 604800 seconds. |
| `prefetch`, `stale` | Global value | Override prefetch or serve stale for matching answers. |

Rule precedence selects exact names before suffixes, then more labels, then rules with `qtype` before rules without, then the first declared rule.

### Cache behavior

- **Semantic keys.** Each question name, type, and class together with DO, CD, RD, and EDNS presence forms its own key. IPv4 ECS, IPv6 ECS, no-ECS, and privacy `/0` traffic never share entries.
- **ECS response scope.** Clients reuse an answer only within the scope prefix returned by the upstream. If an upstream omits ECS in reply to a nonzero outgoing subnet, PariNS reuses that answer only for that exact outgoing subnet. A newer answer supersedes overlapping older entries even if the newer answer cannot be cached.
- **Client-supplied ECS.** Clients may send their own ECS option. A nonzero prefix must contain the client address or PariNS refuses the query. A `/0` prefix requests privacy. If an upstream returns REFUSED for a nonzero subnet, PariNS retries once with `/0` and skips caching that reply.
- **Learned ECS sharing.** When an upstream omits ECS, PariNS compares normalized answers across networks using response code, answer records, DNSSEC authority proofs, and negative SOA metadata, ignoring TTL and record order. Once `min_networks` disjoint networks spanning at least two `/16` IPv4 prefixes or two `/32` IPv6 prefixes receive the same valid answer, one entry serves all nonzero-ECS queries in that address family. Any different answer or explicit nonzero scope revokes sharing immediately and blocks relearning for `revoke_cooldown_secs`. Shared evidence expires after one hour and must be confirmed again. The learning table holds at most 65,536 names, is excluded from snapshots, and clears with the cache. The **Cache** page displays each tracked name as observing, shared, or revoked.
- **Eligibility.** Truncated answers, REFUSED, zero-TTL answers, negative answers without a SOA, and queries carrying Cookie or unknown EDNS options are not cached. Negative TTLs take the minimum of the SOA TTL, the SOA MINIMUM field, and `negative_ttl_cap_secs`. A CNAME chain ending in NXDOMAIN or NODATA is cached only when the chain is unambiguous and backed by matching covering SOAs. PariNS never extends a TTL.
- **Shards and memory.** Each shard keeps separate positive and negative budgets, so negative answers never evict positive ones. Memory allocates as entries arrive rather than at startup. In a desktop measurement of typical answers with one CNAME and two to four addresses at roughly 590 charged bytes each, process memory averaged 300 to 600 bytes per entry, or about 95 MiB for 262,144 entries.
- **Persistence.** PariNS writes a snapshot only at clean, quiescent shutdown and consumes it once before serving on the next start. Crashes and forced stops start with an empty cache. Restored entries keep their remaining TTL. A damaged snapshot or a snapshot from an incompatible cache build loads nothing. Both saving and restoring process entries from most recently used downward and stop at their deadlines: 8 seconds for saving, which publishes everything written so far, and 4.5 seconds for restoring, which keeps everything loaded so far and reports skipped entries with reason `deadline`.
- **Cached failures.** Upstream SERVFAIL answers are cached for `servfail_ttl_secs` per [RFC 9520](https://www.rfc-editor.org/rfc/rfc9520), capped by `negative_ttl_cap_secs`. In recursive mode this also covers DNSSEC Bogus answers, referral loops, and exhausted query budgets. Timeouts and transport failures are never cached. A cached failure applies only to the exact outgoing network, never replaces a cached answer, and is replaced at once by any newer answer. While active, PariNS serves a usable stale answer if available except for DNSSEC Bogus, or returns SERVFAIL with the upstream's Extended DNS Errors without querying upstream. Cached failures use the negative budget, are never prefetched, are excluded from snapshots, and appear in the query log as **Cached failure**.
- **Hot reload.** Changing only `[cache]` settings replaces the cache without restarting listeners.

## Prefetch and serve stale

Prefetch refreshes popular answers shortly before they expire. Serve stale answers from expired entries when the upstream fails per [RFC 8767](https://www.rfc-editor.org/rfc/rfc8767). Both are off by default.

`[cache.prefetch]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | Boolean | Refresh hot positive answers in the background. |
| `min_hits` | `1` | 1 to 1000000 | Minimum cache hits before an entry qualifies for prefetch. |
| `remaining_percent` | `25` | 1 to 90 | Trigger refresh when less than this share of the TTL remains. |
| `max_inflight` | `32` | 1 to 4096 | Concurrent background refreshes, including background cache fills from cancelled queries. |
| `rate_per_sec` | `100` | 1 to 100000 | Maximum refreshes and cache fills started per second. |
| `backoff_secs` | `5` | 1 to 86400 | Wait time before retrying refresh for the same entry after a failure. |

`[cache.stale]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | Boolean | Answer from expired positive entries when an upstream fails. |
| `retention_secs` | `300` | 1 to 2592000 | Seconds past expiry an entry remains eligible for stale fallback. |
| `reply_ttl_secs` | `30` | 1 to 3600 | TTL written onto stale replies. |

### Refresh and stale rules

- Serve stale applies after a timeout, transport failure, or SERVFAIL, and in recursive mode after any failure except DNSSEC Bogus. REFUSED, negative answers, and ECS privacy retries never fall back to stale data. Stale answers never cross ECS namespaces and never carry AD.
- Cache replacement and DNS shutdown cancel active refreshes. When coalescing is enabled, foreground queries for an entry already being refreshed join that refresh.
- If the last client cancels a cache miss after PariNS already sent the upstream query, PariNS continues the query in the background to populate the cache using the `max_inflight` and `rate_per_sec` limits above, even when prefetch is off. Queries over that budget cancel with the client.

## Query coalescing

Identical cache misses share one upstream request while keeping each client's transaction ID, ECS echo, and response filtering separate.

`[coalescing]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `true` | Boolean | Share upstream work across identical concurrent misses. |
| `max_groups` | `128` | 1 to 65536 | Maximum distinct in-flight shared queries. |
| `max_waiters` | `64` | 1 to 65536 | Maximum callers per group, including the first caller. |

When either limit is full, extra queries receive SERVFAIL without sending upstream traffic. Queries with Cookie or unknown EDNS options or with a class other than IN are never coalesced. Cancelling one caller leaves other callers running. When the last caller leaves, the upstream query cancels unless it continues in the background cache-fill budget described in [Prefetch and serve stale](#prefetch-and-serve-stale). A continuing query holds its `max_groups` slot until it finishes.

## Filtering and subscriptions

Filtering blocks queries by domain name using local rules and online subscriptions. Blocked replies are never cached and never carry AD. Filtering never modifies cached upstream answers.

`[filter]` configures local rules:

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Apply the local rules below. |
| `block_response` | `"nodata"` | Default block action: `nodata`, `null_ip`, `custom_ip`, `nxdomain`, or `refused`. |
| `block_ipv4`, `block_ipv6` | None | Unicast IPv4 and IPv6 addresses for `custom_ip`. At least one is required with `custom_ip`, and both are rejected with other methods. |
| `blocked_ttl_secs` | `10` | TTL of block answers and their SOA MINIMUM, from 1 to 86400 seconds. |
| `block_exact` | `[]` | Block these exact domain names. |
| `block_suffix` | `[]` | Block these domain names and their subdomains. |
| `allow_exact` | `[]` | Never block these exact domain names. |
| `allow_suffix` | `[]` | Never block these domain names and their subdomains. |
| `filter_file` | None | Top-level key pointing to a TOML file with the same keys. When set, it replaces inline `[filter]`. |

### Block answer methods

| Method | Response |
| --- | --- |
| `nodata` | NOERROR with no answer records and a SOA. |
| `null_ip` | `0.0.0.0` for A and `::` for AAAA. |
| `custom_ip` | `block_ipv4` for A and `block_ipv6` for AAAA. |
| `nxdomain` | NXDOMAIN with a SOA. |
| `refused` | REFUSED. |

Only A and AAAA queries return addresses. Other query types such as HTTPS, SVCB, or MX, as well as address families missing from `custom_ip`, return `nodata`. The SOA owner is the matching rule name when the rule matched the question name, or the question name otherwise. EDNS clients also receive Extended DNS Error 15 for Blocked.

Any block entry may override the default method after the domain name by appending a method name or one or two unicast IP addresses, at most one per address family. Allow entries take no method.

```toml
block_suffix = [
  "ads.example",
  "track.example nxdomain",
  "tele.example null_ip",
  "page.example 192.0.2.10 2001:db8::10",
]
```

When multiple block rules match, precedence picks local exact, then longest local suffix, then subscription exact, then longest subscription suffix, then the first configured subscription source. The query log records the matched rule and method.

### Local rule constraints

- Rule names must be ASCII labels containing letters, digits, `_`, or `-`, using punycode for IDNs. Matching compares labels case-insensitively and ignores a trailing dot.
- Wildcards, Adblock syntax, hosts lines, URLs, and regular expressions are rejected.
- Local rules are capped at 100,000 entries and 8 MiB of text, including `filter_file`.
- A matching allow rule wins for that name. PariNS also filters CNAME targets in upstream answers, so allowing a query name does not allow a blocked CNAME target.
- If saved local rules fail to compile, such as an unreadable `filter_file`, DNS stays stopped on startup, retry, and rollback, and status displays the error. `filter_file` errors report line and column or list and entry position without exposing file contents. In managed mode, `filter_file` cannot point to files or directories owned by PariNS inside the state directory.

### Filter subscriptions

`[filter_subscriptions]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `true` | Boolean | Apply enabled subscription sources. With no sources configured, nothing downloads. |
| `max_rules` | `1000000` | 1 to 5000000 | Total rules across all sources before deduplication. |
| `max_memory_bytes` | `134217728` (128 MiB) | Up to 512 MiB | Memory budget for building and holding subscription rule sets. |
| `max_disk_bytes` | `268435456` (256 MiB) | 256 KiB to 2 GiB | Disk space budget for downloaded lists. |

Each `[[filter_subscriptions.sources]]` entry defines one list, up to 16 sources:

| Key | Default | Meaning |
| --- | --- | --- |
| `id` | Required | Unique identifier of 1 to 32 lowercase letters, digits, or `-`. |
| `name` | Required | Display name up to 80 characters. |
| `url` | Required | HTTPS URL of the blocklist. |
| `format` | Required | `domain_list` or `hosts_blocklist`. |
| `enabled` | `true` | Enable this source. |
| `auto_update` | `true` | Refresh this source on a schedule. |
| `update_interval_hours` | `24` | Refresh interval from 1 to 168 hours. |
| `block_response`, `block_ipv4`, `block_ipv6` | Inherit `[filter]` | Per-source block method using the same values as `[filter]`. Changing it rebuilds rules from the cached list without redownloading. |

- `domain_list` puts one domain per line, where `example.org` matches exactly and `.example.org` also matches subdomains. `hosts_blocklist` parses hosts lines pointing to `0.0.0.0`, `127.0.0.1`, or their IPv6 equivalents, ignoring the file IP and applying the source block method. Adblock syntax is not supported.
- Single lists are capped at 16 MiB, all lists combined at 32 MiB, and each line at 4096 bytes. Downloads use verified HTTPS to public IP addresses only and ignore proxy environment variables.
- When you add an enabled source in the console, PariNS downloads and validates it before saving. The console generates `id` from the name, editable under **More options**. Failed scheduled updates keep the previous rule set. Enabled local allow rules override subscription blocks.
- Downloaded files and the catalog live under `filter-subscriptions/` in the state or data directory. If the catalog is damaged, keep the directory for diagnosis and restore it from a backup or disable subscriptions instead of editing files directly.

## Limits

Global limits and timeouts are required top-level keys:

| Key | Example | Range | Meaning |
| --- | --- | --- | --- |
| `query_timeout_ms` | `2000` | 1 to 60000 | Total deadline for one query, including all upstream or recursive work. |
| `tcp_io_timeout_ms` | `5000` | 1 to 60000 | Limit for transport reads, writes, idle connections, and encrypted handshakes. |
| `shutdown_grace_ms` | `3000` | 1 to 60000 | Drain window for active queries when DNS stops. |
| `max_inflight` | `256` | 1 to 65536 | Total queries in progress across all transports. Over the limit, UDP queries drop and TCP queries receive SERVFAIL. |
| `max_tcp_connections` | `64` | 1 to 65536 | Total concurrent TCP, DoT, DoH, and QUIC connections, including handshakes. |

`[source_limits]` applies per-client budgets shared across all DNS listeners and keyed by socket peer:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | Boolean | Enable per-client limits. |
| `rate_per_sec` | `100` | 1 to 1000000 | Sustained queries per second per client prefix. |
| `burst` | `200` | 1 to 1000000 | Burst queries allowed per client prefix. |
| `max_inflight` | `32` | 1 to 65536 | Concurrent queries in progress per client prefix. |
| `max_connections` | `8` | 1 to 65536 | Concurrent connections per client prefix, including handshakes. |
| `max_sources` | `4096` | 1 to 65536 | Maximum tracked client prefixes. New clients are rejected when full. |
| `ipv4_prefix` | `32` | 0 to 32 | IPv4 addresses within one prefix share a budget. |
| `ipv6_prefix` | `64` | 0 to 128 | IPv6 addresses within one prefix share a budget. |

- Cache hits and blocked queries count toward source limits. Rejected UDP queries drop silently. Reliable transports return SERVFAIL, and DoH returns HTTP 200 with a SERVFAIL DNS payload.
- Source limits protect process resources and do not replace firewall or network DDoS controls. Clients behind NAT share one budget.
- PariNS raises its soft open-file limit to its hard limit at startup. `deploy/parins.service` sets `LimitNOFILE=524288`, while the managed unit uses systemd's hard limit. If file descriptors run out, listeners pause accepting briefly, keep serving existing traffic, and log `raise the open-file limit or lower max_tcp_connections` at most once per minute.

## Storage and logs

PariNS stores statistics and the optional query log in a private SQLite database under `runtime/` in the managed state directory or the file-mode data directory. See [Query log and storage](operations.md#query-log-and-storage).

`[storage]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `max_database_bytes` | `134217728` (128 MiB) | 16 MiB to 4 GiB | Database size limit including its WAL sidecars. |
| `flush_interval_ms` | `1000` | 100 to 5000 | Write flush interval for queued records. |
| `cleanup_interval_secs` | `60` | 10 to 3600 | Cleanup interval for expired and over-limit data. |
| `queue_max_entries` | `4096` | 128 to 65536 | Maximum records buffered in memory before new ones drop and count. |
| `queue_max_bytes` | `8388608` (8 MiB) | 1 MiB to 64 MiB | Maximum memory charged to queued records. |

`[query_log]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | Boolean | Record each query with client address, name, result, timing, and upstream. |
| `max_entries` | `100000` | 1 to 1000000 | Maximum stored query records. |
| `max_bytes` | `67108864` (64 MiB) | 1 MiB to 2 GiB, at most `max_database_bytes` | Space budget for query records. |
| `retention_secs` | `86400` | 60 to 2592000 | Maximum age of stored query records. |

`[statistics]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `retention_secs` | `604800` (7 days) | 3600 to 31536000 | Maximum age of trend samples. |
| `max_samples` | `10080` | 60 to 525600 | Maximum one-minute trend samples kept. |
| `reset_interval_days` | `0` | 0 to 3650 | Reset cumulative totals every N days. `0` disables automatic reset and never affects trends. |

`[metrics]`:

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `interval_secs` | `0` | 0 to 3600 | Print a JSON metrics snapshot to stderr every N seconds and at shutdown. `0` disables output. |

## Updates

`[updates]` controls software update checks in managed mode. File mode never checks for updates.

| Key | Default | Range | Meaning |
| --- | --- | --- | --- |
| `auto_check` | `true` | Boolean | Check official GitHub releases for newer versions. Updates never install automatically. |
| `check_interval_hours` | `6` | 1 to 168 | Hours between automatic checks. |

Changing only `[updates]` keeps DNS, caches, and certificates running, and leaves a stopped DNS service stopped. See [Online updates and recovery](operations.md#online-updates-and-recovery).
