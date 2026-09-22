# PariNS

Self-hosted DNS with subnet-aware caching, local filtering, encrypted transports,
and an integrated HTTPS management console.

PariNS is a standalone Rust service for running your own DNS forwarder on a local
machine, home network, private network, or VPS. Choose your upstream resolver,
configure your own filtering and cache policies, and manage the service through
the browser or a TOML configuration file. It is not tied to a particular DNS
provider or hosted service.

## Features

- **DNS transports:** UDP, TCP, DoT, DoH over HTTP/2 and HTTP/3, and DoQ listeners.
- **Upstream forwarding:** UDP with TCP fallback, or authenticated DoT with
  optional bounded connection reuse and equivalent-replica hedging.
- **Subnet-aware caching:** independent ECS subnet answers, TTL and negative
  caching, bounded LRU eviction, and in-flight request coalescing. ECS is opt-in.
- **Local filtering:** exact-name and suffix rules, allow exceptions, and CNAME
  response checks; no external rule service is required.
- **Web management:** first-run setup, administrator authentication, status and
  metrics, TOML validation/editing, export, and one-generation rollback.
- **Self-hosted operations:** persistent self-signed HTTPS by default, optional
  custom certificates, a Linux systemd installer, aggregate metrics, and
  configurable resource and source-subnet budgets.

## Status

Early development. The features above are implemented, with automated Linux and
macOS tests and isolated Linux systemd installation checks. Production deployment
and target-machine capacity acceptance have not been performed.

PariNS forwards to existing resolvers; it is not an authoritative DNS server or
a standalone iterative resolver, and does not perform DNSSEC validation. DNS
listener certificates are configured separately from the management console's
self-signed certificate. See [Behavior and limits](#behavior-and-limits) before
deploying.

## Quick start

Install Rust with [rustup](https://rust-lang.org/tools/install/); the repository
pins its toolchain in `rust-toolchain.toml`. Build and start the management console:

```sh
git clone https://github.com/paricafe/PariNS.git
cd PariNS
cargo build --locked --release
./target/release/parins --manage
```

Open `https://SERVER_IP:3000` (or <https://127.0.0.1:3000> on the same machine).
The console listens on all IPv4 interfaces by default. Verify the generated
self-signed certificate before trusting it. Read its fingerprint on the host
(over a trusted SSH connection when remote):

```sh
openssl x509 -in parins-state/https-cert.pem -noout -sha256 -fingerprint
```

See the certificate limitations below, including public-IP name matching.
Restrict inbound TCP 3000 to your administrator IPs when accessing remotely.

Read the token from `parins-state/setup-token`, then use the setup wizard to
create an administrator and choose the DNS listen address and upstream IP:port.
DNS starts only after setup succeeds. For local-only management, add
`--web-listen 127.0.0.1:3000`.

For a persistent Linux service, follow [Managed mode](#managed-mode-install-then-initialize-in-the-browser).
For a headless deployment without the console, use [File-configured mode](#file-configured-mode).

## Deployment choices

- **Local machine:** keep DNS and management bound to loopback.
- **Home or private network:** bind DNS to a LAN address and allow only intended
  clients through the firewall; configure those clients or your router to use it.
- **VPS or remote clients:** configure encrypted DNS listeners and their
  certificates, restrict DNS and management access at the network boundary, and
  measure capacity on the target host. Installing PariNS does not automatically
  make an unrestricted public resolver safe to operate.

## Run

### Managed mode: install, then initialize in the browser

On Linux with systemd, build from this checkout and install the managed service:

```sh
cargo build --locked --release
sudo sh scripts/install.sh
```

Alternatively, build a host-native package with `sh scripts/package.sh`, verify
its adjacent `.sha256` and extracted `SHA256SUMS`, and run `sudo sh install.sh`
inside the extracted Linux package. No public release/download endpoint is
assumed; macOS packages cannot be installed as Linux services. Linux x86_64 and
aarch64 ELF binaries are accepted, with architecture checked before installation.
Use `--dry-run` to inspect planned targets without writes.

The installer enables and starts `parins-managed.service`, using
`/opt/parins-managed/parins` and private state under `/var/lib/parins`. It does not
stop `systemd-resolved`, change host DNS, open firewall ports, or modify the legacy
`parins.service`. Re-running upgrades the managed binary/unit while retaining
state and keeping a private prior binary/unit backup. An installation/startup
failure attempts to restore the prior service; inspect the reported backup and
`journalctl -u parins-managed.service` if recovery itself fails.

The first start serves only the console; **DNS starts after successful setup**.
The CLI and installed service now default to **`0.0.0.0:3000`, HTTPS only**.
Open `https://SERVER_PUBLIC_IP:3000` after allowing inbound TCP 3000 in the host
firewall and cloud security group for your intended administrator IPs. The
installer does not change those rules, configure NAT, or prove Internet routing.
An upgrade from the original console changes both HTTP to HTTPS and the installed
service's loopback default to all IPv4 interfaces; keep an explicit loopback
override if that is what you want.

For IPv6 use `--web-listen '[::]:3000'` and access `https://[SERVER_IPV6]:3000`.
IPv4 acceptance on an IPv6 socket depends on the operating system; the default
IPv4 socket does not claim IPv6 coverage. `--web-listen 127.0.0.1:3000` retains
local-only access. An optional SSH tunnel still works, using HTTPS locally:

```sh
ssh -N -L 3000:127.0.0.1:3000 USER@HOST
```

Read `sudo cat /var/lib/parins/setup-token` on the server. If using the tunnel,
open <https://127.0.0.1:3000>. Enter the one-time token, choose an
administrator name and a password of at least 12 bytes, and set the DNS listen
address and your upstream's literal IP:port. The wizard defaults to loopback DNS;
set port 53 explicitly if wanted and free. The service has only the capability
needed to bind low ports. Port conflicts reject setup/application; no conflicting
service is automatically stopped. Never share the token or put it in a URL.

The console accepts literal IPv4/IPv6 hosts (including a public IP mapped by NAT)
at the listening port, plus localhost. It rejects arbitrary domain names, other
ports and cross-origin browser requests; forwarding headers are not trusted.
This is direct IP access, not a domain-name/reverse-proxy configuration feature.
Use the same local tunnel or NAT port (3000). Passwords use
Argon2id. Eight-hour bearer sessions remain only in page memory: refreshing
requires login, and logout revokes the session. There are no external frontend
assets, cookies, browser-persisted tokens, or query logs.

On first start, PariNS generates a self-signed HTTPS identity in
`/var/lib/parins/https-identity.pem` (certificate **and private key**, 0600) and
exports only its public certificate to `/var/lib/parins/https-cert.pem`.
The identity is atomically stored, reused across restart/upgrades and independent
of DNS certificates/configuration. A corrupt or insecure existing identity fails
startup instead of silently replacing a certificate you may have pinned.

**Self-signed means encrypted, not automatically trusted.** Verify the certificate
fingerprint over a trusted channel such as SSH before trusting it in your browser:

```sh
sudo openssl x509 -in /var/lib/parins/https-cert.pem -noout -sha256 -fingerprint
```

Only `https-cert.pem` may be copied to clients; never export `https-identity.pem`.
The generated certificate is valid for approximately ten years and covers
localhost, loopback IPs and an explicitly bound IP. With a wildcard bind, PariNS
cannot infer the public/NAT IP, so direct public access can also show a hostname
mismatch. For a matching identity, supply a certificate whose IP SAN includes
the accessed public IP. Custom certificate/key paths must be provided together:

```sh
./target/release/parins --manage --web-cert /path/to/cert.pem --web-key /path/to/key.pem
```

There is no automatic renewal/ACME or HTTP fallback/redirect on port 3000.
Replace an expiring certificate explicitly and restart the process. Custom
certificates must be readable by the service and do not overwrite the generated
identity. Certificate changes for the management listener require a full process
restart, not the DNS configuration's apply button. Existing setup-token and
administrator state are retained when upgrading from HTTP.

After setup, the console provides a status/metrics dashboard and a full TOML
editor with validation, change preview, export, and rollback. Advanced ECS,
cache, filtering, encrypted listener and budget options use the same canonical
configuration schema as CLI mode, rather than a second set of UI defaults.
Applying a configuration drains/restarts DNS and clears its in-memory cache and
metrics; it is not zero-downtime reload. Binding or persistence failures restore
the prior configuration when possible, and an unavailable DNS instance is shown
as an error. Stale edits are rejected by revision; after a disconnected save,
reload the current configuration to determine its result before retrying.

Managed `state.json` is authoritative: it contains the administrator hash,
current/previous TOML and revision, protected by 0700/0600 permissions, an
exclusive process lock and atomic replacement. Exported TOML excludes account
data. Relative rule/certificate paths resolve against the state directory;
provision those files separately with permissions readable by the service.
Do not manually edit live state or use `--config` with `--manage`. Back up the
whole private state directory while the service is stopped. Managed mode uses
console reapplication for DNS file/certificate changes, not SIGHUP. Password changes
and account recovery are not exposed in this first console version; keep your
password and private state backup safe.

Useful commands:

```sh
sudo systemctl status parins-managed.service
sudo journalctl -u parins-managed.service
sudo systemctl restart parins-managed.service
sudo systemctl disable --now parins-managed.service  # retains state and backups
# Local development, no systemd or root:
./target/release/parins --manage --state-dir parins-state --web-listen 127.0.0.1:3000
```

### File-configured mode

```sh
cargo build --locked --release
cp parins.example.toml parins.toml
# Edit parins.toml and set upstream to your chosen resolver's IP:port.
./target/release/parins --check
./target/release/parins
```

In another terminal:

```sh
dig @127.0.0.1 -p 5353 example.com A
dig @127.0.0.1 -p 5353 example.com A +tcp
```

Use `--config PATH` to select a different configuration file. `listen` selects
the same address and port for UDP and TCP; port zero selects a shared ephemeral
port, printed on startup. `upstream` must be a literal IP:port, reachable over
both UDP and TCP (or TCP/TLS when `upstream_tls` is configured). Do not point it back at PariNS, including through a local
interface alias that configuration validation cannot identify.

Ctrl-C or SIGTERM (Unix) stops accepting traffic, closes idle TCP clients, and
allows active queries up to `shutdown_grace_ms` to finish before cancellation.

## Behavior and limits

- Cache misses use an upstream UDP transaction with an independent socket and random
  ID. Only responses matching the upstream endpoint, ID, opcode, and question
  are accepted. Upstream truncation triggers TCP fallback under the same
  `query_timeout_ms` deadline. Failures return SERVFAIL.
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
defaults separately to `0.0.0.0:3000` over HTTPS. This release is not yet accepted
as a production public resolver: production capacity validation and network-level abuse protection are
not implemented. Logs contain startup/shutdown/reload events and
optional aggregate metrics, not query names or client IP addresses.

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

## Cache policy

Caching is enabled by default; ECS remains opt-in (`[ecs] enabled = true`).
The implementation is PariNS-owned Rust code, not an embedded resolver cache:
Hickory decodes/encodes DNS, `ipnet` represents subnets, and `lru` manages eviction.
Subnet reuse follows [RFC 7871](https://www.rfc-editor.org/rfc/rfc7871.html),
and negative TTL calculation follows [RFC 2308](https://www.rfc-editor.org/rfc/rfc2308.html).

- Each normalized name/type/class and DO/CD/RD/EDNS combination owns separate
  subnet answers. The longest covering upstream **scope** wins, provided the
  query's source prefix is sufficient. Different subnets cannot overwrite one
  another. IPv4, IPv6, no-ECS, and explicit privacy `/0` are isolated; a normal
  scope `/0` answer may be shared across ordinary queries of its address family.
- Defaults: 4096 answers, 8 MiB of serialized-answer and key bytes, 64 subnet
  variants per query. These are configurable under `[cache]`; the byte limit is
  not an RSS limit. Eviction is LRU across query buckets and within each bucket.
  Each resolver instance owns its cache and immutable upstream configuration.
- This is a whole-response cache, not an RRset cache. All section TTLs age;
  the earliest RR expiry invalidates the answer. Positive TTLs are capped at
  3600 seconds by default. NXDOMAIN/NODATA require a covering SOA and use the
  minimum of SOA TTL, SOA MINIMUM, and the 300-second negative cap.
  Negative answers remain query-type-specific; CNAME plus negative answers
  are not cached. These are conservative limits, not full RFC cache conformance.
- Truncated answers, errors such as SERVFAIL/REFUSED, zero TTL, missing SOA for
  negative answers, missing expected ECS, and scope longer than source bypass
  caching. Non-ECS EDNS options (including cookies) also bypass caching to avoid
  replaying client-specific state.
- Hits restore the current request's ID/question and original ECS, with aged
  TTLs and normalized EDNS. There is no stale serving, prefetch or persistence.

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
On Unix, SIGHUP reloads this file and configured listener certificates. Every
candidate is checked before publication; errors retain the previous generation.
Each DNS request keeps its starting policy snapshot. Inline configuration changes
require restart. There is no rule download or third-party list import.

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

## Request coalescing

Identical eligible cache misses share one upstream operation, keyed by the actual
outbound ECS subnet and DNS semantics (not the eventual response scope). `[coalescing]`
defaults to enabled, 128 groups and 64 callers per group, including the first caller.
At either limit, new requests return SERVFAIL without extra upstream IO. Requests
with non-ECS EDNS options or non-IN class bypass sharing; ingress budgets still apply.
Waiters share the first operation's timeout. Cancelling one does not cancel others;
cancelling the last releases IO without a detached background task. Each caller
still receives independent policy checks, ID/question and ECS restoration.

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

## Encrypted transports and reload

Uncomment individual `[dot]`, `[doh]`, `[doq]`, `[doh3]` sections in the example.
Certificate, key, CA and rule paths resolve relative to the configuration file.
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
  The legacy `tcp_io_timeout_ms` also bounds encrypted handshakes
  and complete HTTP/QUIC request work. HTTP/2 headers are limited to 8 KiB;
  HTTP/3 field sections to 128 KiB; DNS payloads to 65535 bytes. Large DNS GET URLs
  may exceed header limits; use POST. These budgets are not an RSS guarantee.
- H2 stream reset, DoQ response cancellation and H3 connection closure cancel
  their active DNS waiter. H3 **single-stream** cancellation is currently observed
  on response write or at the finite request deadline: the selected H3 library
  does not expose an earlier response-stream cancellation notification.
- Optional `[upstream_tls]` authenticates the fixed upstream IP using `server_name`.
  `ca_file` replaces built-in WebPKI roots. Certificate failures never downgrade
  to plaintext. Upstream DoT opens a fresh connection per transaction by default;
  optional bounded reuse is described below. There is no DoH/DoQ upstream client.
- SIGHUP validates all new rule/certificate candidates before replacing them.
  Each listener's new full handshakes see its atomic certificate replacement;
  established connections and resumed sessions may retain previous TLS identity
  context. Publication is not one global transaction across all listeners and
  rules. Listener addresses, resource limits, upstream/CA settings and inline
  configuration require restart, which creates fresh resolver/cache ownership.

## Optional DoT upstream reuse

With `[upstream_tls]` configured, enable `[upstream_pool] enabled = true` to reuse
authenticated connections, following the connection-lifecycle guidance in
[RFC 7858 §3.4](https://www.rfc-editor.org/rfc/rfc7858.html#section-3.4).
`max_connections` defaults to 8 (range 1..256), shared by clones and both replica
endpoints within one immutable TLS profile. Connections never cross endpoints
or authentication profiles. Each connection handles one query at a time; DNS
pipelining is not implemented. Pool waiting, handshake, exchange and reconnect
all remain inside the Resolver's original `query_timeout_ms` deadline.

`idle_timeout_ms` defaults to 30000 (range 1..600000). Expiry is checked on checkout,
not by a background reaper: without new traffic, idle sockets can remain until
the owning upstream is dropped, still within the connection cap. Cancellation,
partial responses and invalid replies discard the borrowed connection. A reused
connection closed by the peer gets at most one authenticated reconnect; protocol
and certificate errors do not trigger retries or plaintext fallback. Leave pool
capacity headroom for replica hedges; a one-connection cap serializes them.

## Optional replica scheduling

`[scheduler]` enables exactly one secondary **semantically equivalent** upstream.
The operator must establish equal ECS, DNSSEC and answer policies. Both replicas
use the same transport and TLS server name. The primary starts first; after
`hedge_after_ms`, or an IO error/SERVFAIL, at most one secondary starts.
`max_extra_inflight` is a process-resolver-wide non-queuing extra-operation budget.
The first non-SERVFAIL response wins, including NXDOMAIN, NODATA and REFUSED.
REFUSED remains available for the resolver's bounded anonymous ECS retry.
The overall query deadline is unchanged and losing futures are dropped, not
left running in background tasks. Saturation falls back to primary-only behavior.

Changing upstream semantics requires a new Resolver instance (restart in the
binary), with an independent cache and singleflight table. No automatic
non-ECS emergency profile or health-based routing is enabled. Hedging is disabled
by default and trades additional upstream traffic for latency; measure first.

## Development, testing and packaging

```sh
cargo run --locked -- --config parins.example.toml --check
cargo test --locked --all-targets
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
```

The example uses loopback addresses and an upstream on port 5354. Set `upstream`
to your chosen resolver's IP and port before sending queries. Unknown configuration
keys and invalid resource limits are rejected at startup.

The binary uses Tokio's multi-thread runtime, normally one worker per available
CPU; `TOKIO_WORKER_THREADS=2` can explicitly select two workers. This is not CPU
affinity or a memory limit. Cache and coalescing state use short shared locks;
UDP has one receive loop followed by spawned query tasks. More workers do not
guarantee linear scaling. Default cache byte limits account for payload/key bytes,
not process RSS. Measure on the target machine before increasing resource budgets.

```sh
cargo run --locked --release --example bench -- 1000 32
cargo run --locked --release --example bench_dot -- 1000 8
cargo install cargo-audit --version 0.22.2 --locked
cargo audit --deny warnings
sh scripts/package.sh
```

The benchmark runs only synthetic loopback upstreams and emits JSON for warm
cache, cold single-upstream and cold hedged scenarios. It is a Resolver baseline,
not network/TLS throughput, RSS measurement or production capacity evidence.
`bench_dot` separately compares fresh and pooled authenticated DoT transactions
against one synthetic loopback TLS peer, without response caching or artificial
delay. JSON includes checked answers, errors, connection/handshake counts and
latency percentiles. Full handshakes are counted separately from TLS session
resumption; compare repeated runs with the same query count and concurrency.
These transport microbenchmarks are not production capacity guarantees.
Packaging creates a host-native archive under `target/packages`, containing no
keys or private configuration. `deploy/parins.service` is a Linux systemd template,
not an installed service; review paths, file permissions, firewall and source
limits before deployment. The template defaults to unprivileged ports. CI runs
formatting, Clippy, tests, release build, configuration check, audit and packaging;
Linux/macOS CI results must be checked separately from local macOS acceptance.

## Module boundaries

| Module | Responsibility |
| --- | --- |
| `config` | Parse and validate startup configuration |
| `protocol` | DNS parsing, request rules, response correlation, UDP encoding |
| `ecs` | Peer provenance, subnet selection, wire validation, scope and client echo |
| `cache` | Independent subnet answers, TTL/negative policy, bounded LRU eviction |
| `policy` | Immutable local rules, label matching, allow precedence, CNAME filtering |
| `flight` | Bounded in-flight sharing and cancellation ownership |
| `metrics` | Fixed counters, RAII lifecycle gauges and cumulative latency buckets |
| `resolver` | Compose policy, ECS, cache, upstream deadline and response restoration |
| `upstream` | Independent UDP exchange and validated TCP fallback |
| `scheduler` | Opt-in equivalent-replica race, shared extra budget, loser cancellation |
| `tls` / `doh` / `quic` | Authenticated transport, framing, HTTP/stream lifecycle |
| `ingress` | Shared encrypted-query admission and response serialization |
| `limits` | Bounded socket-subnet token buckets and RAII query/connection quotas |
| `admin` | Loopback read-only metrics and process liveness |
| `manage` | HTTPS API, authentication, private state, and transactional DNS configuration changes |
| `web` | Embedded setup wizard, dashboard, and configuration editor |
| `transport::tcp` | Length-prefixed framing used on both sides |
| `server` | Listener ownership, admission budgets, client tasks, shutdown |
| `main` | CLI arguments, startup, OS signals |

Tests use controlled loopback upstreams and do not rely on public DNS answers.

## Planned scope

- Explicit emergency-profile product policy and health-driven scheduling.
- Licensed third-party rule import and additional filtering response modes.
- DoT pipelining and additional authenticated upstream protocols.
- Measured public-service capacity, network abuse protection and deployment/rollback acceptance.
- Zero-downtime full configuration replacement; console application currently restarts DNS.

The initial design focuses on forwarding to existing resolvers. A standalone
iterative resolver is outside the initial scope. The implementation uses Rust,
Tokio for asynchronous IO, and Hickory for DNS message parsing and encoding.

## License

Copyright 2026 Natsuki-Kaede and PariNS contributors.

Licensed under the [Apache License, Version 2.0](LICENSE).
