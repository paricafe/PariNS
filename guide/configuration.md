# Configuration

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md)

Start with [parins.example.toml](../parins.example.toml) or the managed setup wizard.
The console and file mode share the same configuration schema. `[upstreams]` is
required; obsolete single-upstream fields and unknown keys are rejected. The example
uses a loopback test upstream on port 5354: replace it before sending real queries.
See [Deployment](deployment.md#file-configured-mode) for validation and startup commands.

- [Upstreams](#upstream-dns-settings) and [DoT connection reuse](#optional-dot-upstream-reuse)
- [Encrypted listeners](#encrypted-listeners)
- [Cache and ECS](#cache-policy) and [request coalescing](#request-coalescing)
- [Filtering and subscriptions](#local-filtering)
- [Source budgets](#optional-source-budgets) and [protocol limits](#behavior-and-limits)

## Upstream DNS settings

**Upstream DNS settings** is the sole visual upstream editor, for one or more
servers. Enter one endpoint per line. The first-run wizard uses the same
list, query modes, explicit bootstrap and H3 preference.
Supported syntax is IP with optional port, `udp://`, `tcp://`, `tls://`,
`https://host/dns-query`, and `quic://`; bracket IPv6 literals. A trailing
`weight=N` (1..1000) sets a static weight. This is smooth weighted round-robin,
not AdGuard Home's adaptive latency/failure weighting. Parallel mode starts all
configured endpoints when the global extra-operation budget allows, returning
the first NOERROR/NXDOMAIN response and cancelling remaining requests. DNS errors
are held as fallback while other requests remain pending; all share the caller's
deadline. At saturation, fewer parallel requests run. Weighting selects one
endpoint per operation; it does not promise retries/failover on failure.

```toml
[upstreams]
servers = ["udp://192.0.2.53:53 weight=2", "tcp://192.0.2.54:53 weight=1"]
mode = "weighted" # or "parallel"
prefer_h3 = false # HTTPS only; opt in to HTTP/3 with verified HTTP/2 fallback
bootstrap = [] # hostname endpoints require explicit DNS IP:port entries
max_parallel = 32 # parallel mode must cover all configured servers (max 32)
max_extra_inflight = 128
# ca_file = "private-ca.pem" # otherwise built-in WebPKI roots
```

The example addresses are documentation-only: replace them with your actual
resolvers. Hostnames require explicit bootstrap servers; system DNS is never
used for bootstrap. Encrypted transports validate certificate identity and never
downgrade to plaintext. DoH uses POST over HTTP/2 by default. Enable **Prefer
HTTP/3 for HTTPS upstreams** (`prefer_h3 = true`) to try QUIC on the same host,
port and path first, then fall back to verified HTTP/2 on failure or timeout.
The H3 attempt has a budget of at most 250 ms (half the configured query timeout
if shorter), inside the original query deadline. Connections are reused; failed
H3 attempts trigger a 30-second per-endpoint cooldown before retrying H3. Normal
DNS error replies still follow the selected upstream scheduling policy. This
does not discover alternate ports through Alt-Svc or HTTPS/SVCB records. Direct
HTTPS-origin probing and TCP fallback follow [RFC 9114 §3.1](https://www.rfc-editor.org/rfc/rfc9114.html#section-3.1).
Unsupported schemes,
credentials, URL query strings, fragments and duplicate endpoints are rejected.
All pool endpoints should have equivalent resolution,
filtering and ECS policies because they share the resolver cache. There is no
domain-routing syntax or DNSCrypt support. Do not point endpoints/bootstrap back
at PariNS (including NAT aliases); direct matching listeners are rejected, but
arbitrary network hairpin routes cannot be inferred by local validation.

### Client request forwarding

Forwarding preserves the question and EDNS options, DO/CD/RD flags and payload
advertisement, with deliberate exceptions: transaction IDs are hop-local; DoQ
requires ID zero and removes TCP keepalive. ECS is policy-controlled, not blindly
copied: disabled ECS removes the client subnet; enabled ECS validates it against
the socket peer and caps the prefix (explicit /0 remains private). Invalid or
spoofed subnets are rejected. ECS privacy retry and response normalization remain
in effect; AD is cleared because PariNS does not validate DNSSEC. Requests with
Cookie/unknown EDNS options bypass shared cache and request coalescing. Padding is
regenerated only for encrypted hops: 128-byte upstream request blocks and 468-byte
client response blocks when requested, bounded by the advertised payload limit.
Plaintext hops remove Padding; non-EDNS requests do not gain an OPT record. These boundaries
follow [RFC 6891](https://www.rfc-editor.org/rfc/rfc6891),
[RFC 7871](https://www.rfc-editor.org/rfc/rfc7871) and
[RFC 9250](https://www.rfc-editor.org/rfc/rfc9250); forwarding is not byte-for-byte
packet replay.

## Optional DoT upstream reuse

For `tls://` endpoints, enable `[upstreams.dot_pool] enabled = true` to reuse
authenticated connections, following the connection-lifecycle guidance in
[RFC 7858 §3.4](https://www.rfc-editor.org/rfc/rfc7858.html#section-3.4).
`max_connections` defaults to 8 (range 1..256), independently per DoT endpoint
and shared by its concurrent queries. Connections never cross endpoints
or authentication profiles. Each connection handles one query at a time; DNS
pipelining is not implemented. Pool waiting, handshake, exchange and reconnect
all remain inside the Resolver's original `query_timeout_ms` deadline.

`idle_timeout_ms` defaults to 30000 (range 1..600000). Expiry is checked on checkout,
not by a background reaper: without new traffic, idle sockets can remain until
the owning upstream is dropped, still within the connection cap. Cancellation,
partial responses and invalid replies discard the borrowed connection. A reused
connection closed by the peer gets at most one authenticated reconnect; protocol
and certificate errors do not trigger retries or plaintext fallback. A
one-connection cap serializes queries to that endpoint.

## Encrypted listeners

Uncomment individual `[dot]`, `[doh]`, `[doq]` sections in the example.
Set `[doh].http3 = true` for HTTP/3 on the same address and actual port as HTTP/2
(UDP plus TCP, including port 0). Allow both protocols at your network boundary.
DNS responses advertise `Alt-Svc: h3=":PORT"` when enabled and `clear` when disabled;
the management HTTP listener does not advertise DNS H3. Outbound `prefer_h3` is independent.
In file mode, certificate, key, CA and rule paths resolve relative to the
configuration file; managed mode resolves them against the state directory.
All sockets and certificates must initialize successfully before traffic is served;
`--check` validates files without opening listeners. PEM private keys and local
configuration are ignored by Git. Provision certificates outside this repository.

- DoT uses length-prefixed DNS, TLS 1.2/1.3 and sequential connection reuse.
- DoH uses `/dns-query`, GET with unpadded base64url `dns=`, or POST with
  `application/dns-message`. HTTP/2 requires `h2` ALPN; HTTP/1 DNS is not supported.
  Replies use `Cache-Control: no-store` to prevent shared HTTP cache subnet leaks.
- DoQ uses TLS 1.3 / `doq` ALPN, one zero-ID DNS frame per bidirectional stream,
  followed by FIN. HTTP/3 uses `h3` ALPN and the same DoH contract, not DoQ framing.
  QUIC migration and 0-RTT are disabled. Peer identity always comes from the socket;
  X-Forwarded-For/Forwarded headers are not trusted.
- Connections/handshakes share `max_tcp_connections`; streams and bodies have
  finite bounds. Per-connection streams are capped at `min(max_inflight, 1024)`.
  `tcp_io_timeout_ms` also bounds encrypted handshakes
  and complete HTTP/QUIC request work. HTTP/2 headers are limited to 8 KiB;
  HTTP/3 field sections to 128 KiB; DNS payloads to 65535 bytes. Large DNS GET URLs
  may exceed header limits; use POST. These budgets are not an RSS guarantee.
- H2 stream reset, DoQ response cancellation and H3 connection closure cancel
  their active DNS waiter. H3 **single-stream** cancellation is currently observed
  on response write or at the finite request deadline: the selected H3 library
  does not expose an earlier response-stream cancellation notification.
- Encrypted upstreams authenticate the hostname or IP in their URL.
  `[upstreams].ca_file` replaces built-in WebPKI roots. Certificate failures never
  downgrade to plaintext. DoT opens a fresh connection per transaction by default;
  optional bounded reuse is configured under `[upstreams.dot_pool]`.

See [certificate import and renewal](operations.md#certificates) for identity
validation, atomic reload and managed HTTPS behavior.

## Cache policy

Caching is enabled by default; ECS remains opt-in (`[ecs] enabled = true`).
The implementation is PariNS-owned Rust code, not an embedded resolver cache:
Hickory decodes/encodes DNS, `ipnet` represents subnets, and `lru` manages eviction.
Subnet reuse follows [RFC 7871](https://www.rfc-editor.org/rfc/rfc7871.html),
and negative TTL calculation follows [RFC 2308](https://www.rfc-editor.org/rfc/rfc2308.html).

- Each normalized name/type/class and DO/CD/RD/EDNS combination owns separate
  subnet answers. The longest covering upstream **scope** wins, provided the
  query's source prefix is sufficient. Disjoint subnets cannot overwrite one
  another. Successful responses conservatively replace overlapping scopes so a
  superseded broad answer cannot revive after a narrow answer expires, even when
  the new response has TTL zero or cannot be admitted. This can cause neighboring
  clients to miss a previously broad entry. IPv4, IPv6, no-ECS, and privacy `/0`
  are isolated; a normal
  scope `/0` answer may be shared across ordinary queries of its address family.
  If an upstream omits ECS after a nonzero ECS request, `ExactSource` permits reuse
  only for the identical actual outgoing family/network/source-prefix. It does
  not invent broader scope or mix privacy and no-ECS namespaces.
- Defaults: 32768 answers, 32 MiB charged bytes, 64 subnet variants per query,
  four concurrent shards and a 20% negative-cache reservation. Each shard has
  separate positive/negative entry and byte budgets; unused partitions are not
  borrowed. Tiny capacities can round negative capacity down to zero. Eviction
  removes individual variants, not entire query buckets. Negative churn cannot
  evict positive entries. Serialized response decoding and TTL restoration run
  outside the shard lock.
- Byte charge includes serialized data and conservative per-entry metadata,
  including entries temporarily retained by readers after eviction. It is not
  process RSS: allocator slack and empty index capacity are not tracked. Fixed
  shard partitions may reach capacity before the aggregate limit. Resolver/cache
  generations never share entries across upstream configurations.
- Cache storage grows on demand, not by reserving the full byte budget at startup.
  Rust owns defaults for omitted settings; the setup template mirrors them, and
  the console displays the server's parsed values. Existing explicit limits are
  not changed by an upgrade. Tune entry and byte limits together using interval
  hit/miss and eviction deltas: more capacity helps a capacity-bound working set,
  but cannot make expired or ineligible answers reusable. These limits are not a
  process memory ceiling or a measured DNS QPS rating.
- This is a whole-response cache, not an RRset cache. All section TTLs age;
  the earliest RR expiry invalidates the answer. Positive TTLs are capped at
  3600 seconds by default. NXDOMAIN/NODATA require a covering SOA and use the
  minimum of SOA TTL, SOA MINIMUM, and the 300-second negative cap.
  Negative answers remain query-type-specific. Since v0.1.7, PariNS also caches a strict
  CNAME-only chain ending in NXDOMAIN/NODATA when it is continuous, unambiguous and
  backed by equivalent, same-class SOAs covering the terminal name. Missing or
  conflicting proof is not cached. Chain and all retained record TTLs also bound
  the lifetime; these entries use the negative budget and never stale or prefetch.
  Direct NOERROR CNAME answers and NOERROR CNAME plus the requested type remain positive.
  These are conservative limits, not full RFC cache conformance.
- Truncated answers, errors such as SERVFAIL/REFUSED, zero TTL, missing SOA for
  negative answers, unusable ECS, and scope longer than source bypass caching.
  Cookie/unknown EDNS options bypass caching to avoid replaying client-specific
  state. Padding is allowed but removed from the stored canonical answer.
- Hits restore the current request's ID/question and original ECS, with aged
  TTLs and normalized EDNS. There is no local DNSSEC validation.
  `[cache.persistence]` defaults to enabled with a 32 MiB snapshot budget: only
  fresh entries are saved at terminal, quiescent shutdown. The snapshot budget
  is independent of cache accounting; its serialized format can reach the limit
  before every fresh entry is saved. Startup ages all TTLs
  and consumes the file durably before serving. Incompatible/corrupt snapshots
  are consumed and skipped; a failed consume prevents DNS startup. No periodic
  snapshot can resurrect entries after clear, TTL0, EDE or a crash. Forced drain,
  SIGKILL and abnormal exit start cold. Normal configuration apply is not restart
  restoration; stale-only entries are never restored. A stop forced by requests
  still running past `shutdown_grace_ms` logs
  `cache snapshot not saved: shutdown not quiescent (<cause>)`; idle client
  connections do not delay or force a stop.
  The v0.1.7 CNAME-negative change advances the cache semantic fingerprint,
  not the snapshot wire format. Earlier-semantic snapshots are rejected, so the
  first start after upgrading begins cold; there is no compatibility migration.
- `[[cache.rules]]` selects exact names before suffixes, then most labels,
  record-type-specific before wildcard type, then first declaration. DNS labels
  (including escaped labels) determine boundaries; suffix includes the named
  domain itself. Rules can bypass caching, set TTL caps and override prefetch/stale
  defaults. No minimum TTL is forced. The global cache switch always wins.
- Prefetch is disabled by default. Hot positive records near expiration may
  trigger bounded, deduplicated background refresh. Limits cover concurrent work,
  starts per second and failure backoff. Refresh tasks are cancelled on cache
  replacement and shutdown; they do not survive their owning generation. With
  coalescing enabled, foreground misses can join pending refreshes across expiry;
  explicitly disabling coalescing permits independent foreground operations.
- Stale serving is disabled by default. When enabled, normal upstream resolution
  runs first; timeout, transport failure or SERVFAIL may fall back to a retained
  positive answer. REFUSED, negative answers and ECS privacy-retry results are not
  stale fallback sources. Retention and returned TTL are separate (defaults 300s
  and 30s); this trades freshness for availability, as in
  [RFC 8767](https://www.rfc-editor.org/rfc/rfc8767.html).
- The cache page shows charged usage and fresh/stale/miss/bypass/eviction counters,
  explains a name/type/outgoing-subnet/flag lookup without contacting upstream,
  and clears all entries or an exact name/type/scope selection. Inspections are
  bounded to 256 variants and do not affect recency or hit counts. Clearing
  advances an epoch so older in-flight queries cannot refill cleared state;
  new queries may repopulate it. These authenticated operations expose no query log.

Example opt-in policy (also editable through the console):

```toml
[cache.prefetch]
enabled = true
min_hits = 3
remaining_percent = 10
max_inflight = 2
rate_per_sec = 10
backoff_secs = 5

[cache.stale]
enabled = false
retention_secs = 300
reply_ttl_secs = 30

[[cache.rules]]
name = "dynamic.example"
suffix = true
max_ttl_secs = 60
stale = false

[[cache.rules]]
name = "uncached.example"
bypass = true
```

## Request coalescing

Identical eligible cache misses share one upstream operation, keyed by the actual
outbound ECS subnet and DNS semantics (not the eventual response scope). `[coalescing]`
defaults to enabled, 128 groups and 64 callers per group, including the first caller.
At either limit, new requests return SERVFAIL without extra upstream IO. Requests
with Cookie/unknown EDNS options or non-IN class bypass sharing; ingress budgets still apply.
The shared flight/refresh key ignores Padding bytes and length, but preserves
whether Padding was requested. Cache keys ignore that intent; each final client
response independently restores ID/ECS and generates padding when appropriate.
Waiters share the first operation's timeout. Cancelling one does not cancel others;
cancelling the last releases IO without a detached background task. Each caller
still receives independent policy checks, ID/question and ECS restoration.

## Local filtering

Filtering is disabled by default. Opt in with local rules in the config:

```toml
[filter]
enabled = true
block_exact = ["ads.example"]
block_suffix = ["tracking.example"]
allow_exact = ["status.tracking.example"]
allow_suffix = []
```

Exact rules match only that name; suffix rules include that name and subdomains
at DNS label boundaries. ASCII case and a final dot are ignored. Any matching
allow rule wins for that name, regardless of specificity. Names use ASCII
letters, digits, underscores and hyphens; use punycode for IDNs. Wildcards,
Adblock syntax, hosts lines, URLs and regex are rejected, including when disabled.
Limits are 100000 rules and 8 MiB of rule text. Set the top-level `filter_file`
to use a standalone TOML file with these same fields (without `[filter]`). It
becomes authoritative instead of inline rules and is limited to 8 MiB total.
In file-configured mode on Unix, SIGHUP reloads this file and configured listener certificates. Every
candidate is checked before publication; errors retain the previous generation.
Each DNS request keeps its starting policy snapshot. Inline configuration changes
require restart in file mode. Managed rule-only changes retain listeners and the
raw-answer cache.

Query-name filtering runs after protocol/ECS validation and before cache lookup
for every supported query type (including AAAA, HTTPS and SVCB). Blocking returns
NOERROR with empty RR sections, no AA/AD/TC flags and no synthetic SOA. These
answers are not inserted into PariNS's cache and do not specify a downstream
negative-cache TTL. Unsupported ANY/AXFR/IXFR queries still receive REFUSED.

Answer CNAME chains are followed from the original question by owner and class,
independent of record order, with cycle detection. Allowing the query name does
not exempt a different blocked target. Unrelated answer/additional names do not
trigger a block. This check runs on both upstream responses and cache hits;
the cache retains the original upstream answer, never a filtered replacement.
DNAME and HTTPS/SVCB TargetName traversal are not implemented; query-name and
CNAME checks alone are not a complete DNS/application firewall.

### Online subscriptions (v0.1.5)

The console supports HTTPS rule subscriptions alongside local rules. Automated
fixed-resource performance and isolated Linux checks do not establish capacity
on a target machine; do not interpret the compact index as a capacity claim.
No lists are bundled, and the default empty source list makes no network requests.
The local filter switch controls only local rules; enabled local allow rules also
take precedence over subscription blocks.

Add a source in Filtering, choose its explicit format, verify the download, then
enable it and save the configuration. Verification alone does not activate rules.
Disabled sources can be saved before downloading. Update now uses the saved source;
editing a URL or format requires verification of the new identity. An unknown
operation result is reconciled through status, never silently retried.

```toml
[filter_subscriptions]
enabled = true
max_rules = 1000000
max_memory_bytes = 134217728
max_disk_bytes = 268435456

[[filter_subscriptions.sources]]
id = "example"
name = "My domain list"
url = "https://example.org/block.list"
format = "domain_list"
enabled = false
auto_update = true
update_interval_hours = 24
```

`domain_list` accepts one ASCII domain per line: `example.org` matches exactly,
while `.example.org` includes its root and subdomains. `hosts_blocklist` accepts
standard sinkhole hosts entries with `0.0.0.0` or `127.0.0.1` addresses (and their
supported IPv6 sinkholes). These are not ABP, wildcard or regex formats. For
Natsuki List use its `.list` domain-list source, not the unsupported Legacy file.
Third-party list licensing and content remain the administrator's responsibility.

At most 16 sources are configured. Rule counts are charged before deduplication;
the memory budget includes construction and retained generations, not process RSS.
Each source is bounded to 16 MiB, all source input to 32 MiB, and each line to
4096 bytes. Automatic intervals are 1–168 hours with bounded jitter and failure
backoff. Downloads use verified HTTPS and public pinned addresses; proxy environment
variables and private/synthetic DNS results cannot bypass these restrictions.

Private content and the authoritative catalog live under `filter-subscriptions/`
in the managed state directory or file-mode data directory. A failed update keeps
the accepted rules. An invalid derived index can be rebuilt from verified content;
missing or corrupt selected content does not silently remove protection. Existing
sources start offline from verified content. `--check` only reads material and
fails when effective sources are unavailable; it never downloads or writes an index.

If the catalog is corrupt or missing from an existing store, preserve the directory
for diagnosis. Restore a verified backup of the catalog and its selected objects,
or explicitly disable subscriptions before arranging a fresh store. Do not select
objects by filename, edit the catalog to guess a version, or copy state into an
older binary. The management console remains available to diagnose rule failures.

## Optional source budgets

`[source_limits] enabled = true` enables a token bucket and concurrent query/connection
limits keyed only by the socket peer. Defaults: `rate_per_sec = 100`, `burst = 200`,
`max_inflight = 32`, `max_connections = 8`, `max_sources = 4096`, `ipv4_prefix = 32`,
`ipv6_prefix = 64`. IPv4-mapped IPv6 is normalized to IPv4. All six DNS listeners
share these budgets within one Server; changing source ports, protocols, ECS or
forwarding headers does not create a new identity. NAT/proxy users share a budget.

Cache hits, filtered queries and coalesced callers still consume query admission.
UDP source rejection is silent; reliable transports return DNS SERVFAIL for an
otherwise forwardable query (DoH keeps HTTP 200 with a DNS body). Existing invalid
request handling is retained. Source connection caps include pending handshakes:
TCP/TLS is closed and incoming QUIC refused before application handshake work.
Normal completion, timeout and cancellation release concurrency, not rate tokens.
Global query/connection limits remain separate and may reject earlier.

State has a hard source-count cap. At capacity, admission scans at most 16 entries
and only reclaims inactive sources whose tokens have fully replenished. If no
eligible entry is found, new sources are rejected rather than resetting existing
token debt. This can conservatively reject a new source while another reclaimable
entry is outside that scan. IP/subnet state stays in memory until reclamation or
shutdown and is neither logged nor persisted. Disabled mode stores no source state.
`source_queries_rejected`, `source_connections_rejected` and `source_table_full`
are aggregate metrics; table-full is a subset of the corresponding rejections.

These are per-process DNS admission controls, not distributed limits, a bound on
all HTTP/QUIC parsing work, handshake-attempt rate, bandwidth or a DDoS defense.
They do not authenticate UDP source IPs. Keep infrastructure firewall/source
validation and provider protection. Configuration changes require restart.

## Behavior and limits

- UDP upstream cache misses use a transaction with an independent socket and random
  ID. Only responses matching the upstream endpoint, ID, opcode, and question
  are accepted. Upstream truncation triggers TCP fallback under the same
  `query_timeout_ms` deadline. Failures return SERVFAIL.
- Linux/macOS UDP listeners retain the query's destination IP and interface for
  replies, including wildcard and dual-stack binds. Other platforms require a
  specific local listen address; wildcard UDP binds are rejected.
- UDP replies respect the client's payload limit, capped at 1232 bytes (512
  without EDNS). Larger replies return a question-only TC response; retry over
  TCP for the complete answer. Downstream TCP connections support sequential
  reuse; requests on the same connection are processed in order.
- `max_inflight` is shared across all DNS transports. At capacity, new UDP
  queries are dropped and valid TCP queries receive SERVFAIL. Excess TCP
  connections are closed. `tcp_io_timeout_ms` bounds each complete frame read
  or write, including idle and partial-frame reads.
- Only ordinary single-question QUERY messages are supported. AXFR, IXFR, ANY,
  and signed queries are refused; other opcodes return NOTIMP. Malformed queries
  with a readable DNS header return FORMERR; unreadable packets and response
  packets sent to the listener are dropped (TCP connections are closed).
- EDNS/DO/CD are forwarded. ECS is stripped by default; enable `[ecs]` to derive
  it from the socket peer, capped by `ipv4_prefix`/`ipv6_prefix`. Client `/0`
  requests retain their privacy; nonzero client ECS must cover the peer or is
  refused. Trusted forwarding of third-party subnets is not implemented.
  Downstream ECS echoes the original client option and is omitted if the client
  did not supply one. A nonzero ECS REFUSED triggers one anonymous `/0` retry
  within the original deadline; that retry's result is not cached. This
  release does not perform DNSSEC validation or authenticate the plaintext
  upstream; the AD bit is cleared in client responses.

The example and setup wizard default to loopback **DNS**; the management console
defaults separately to `0.0.0.0:3000` over HTTP until inbound DoH is enabled.
PariNS is not yet accepted as a production public resolver: production capacity
validation and network-level abuse protection are not implemented. Process logs contain
startup/shutdown/reload events and optional aggregate metrics, not query names
or client IP addresses.
