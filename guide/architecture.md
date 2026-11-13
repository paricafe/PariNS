# Architecture

[Development](development.md) · [Configuration](configuration.md) · [Operations](operations.md)

PariNS is a modular Rust service. Its embedded React console and management API
control the same process that owns DNS, while a separate fixed-purpose updater
performs privileged installation work on Linux. Production needs no Node server.

## Request path

```mermaid
flowchart LR
    Client --> Listener[Server and transport adapters]
    Listener --> Resolver
    Resolver --> Policy[Request-held policy generation]
    Resolver --> Cache
    Resolver --> Flights[In-flight sharing and bounded refresh]
    Flights --> Source{Resolution mode}
    Source --> Pool[Upstream pool]
    Source --> Recursor[Hickory recursor]
    Resolver --> Delivery[Transport delivery acknowledgement]
    Delivery --> Runtime[Process metrics and bounded storage queue]
```

`server` owns listeners, their supervisors and shutdown. Plain transports handle
UDP packet information and TCP framing; encrypted adapters share admission through
`ingress`. `protocol` and `ecs` validate and normalize the request at their owning
boundaries.

`resolver` captures the cache and policy owners for each request. It applies query
filtering, looks up the cache, shares eligible misses through `flight`, and runs
one source operation with one deadline. Forwarding and recursive modes share this
outer behavior. `upstreams` owns endpoint scheduling and verified transport
connections. `recursion` owns the pinned Hickory recursor, DNSSEC result mapping,
infrastructure caches and global outbound bounds.

The cache keeps upstream answers before response filtering. Filtering, per-client
ECS and AD handling run before delivery. Stale fallback belongs to each foreground
consumer. A transport confirms delivery only after its write succeeds; dropping an
unconfirmed delivery records cancellation. Cache code performs no network IO.

An answer that cannot be stored can still supersede an older overlapping
answer. This includes complete, transaction-independent answers with zero TTL
or Extended DNS Errors that make replay ineligible. The cache checks the epoch
under the shard lock before learning ECS sharing or admitting a response.
Capacity diagnostics copy bounded occupancy and eviction counters without
scanning or touching entries. Resident occupancy includes entries beyond
their fresh lifetime.

### Upstream transport

DoH selects GET while the encoded URI fits and POST otherwise. Both HTTP
versions account for HTTP `Age` in response TTLs before returning the DNS
message. HTTP/3 can fall back to verified HTTP/2 only before sending the query.
After the HEADERS frame is written, the attempt keeps the resolver deadline
and any failure goes through normal upstream failure handling. The pool can
advance to another bootstrap address only after an unsent attempt. DoT's
bounded retry on a closed reused connection is described in
[Configuration](configuration.md#upstreams).

HTTP/3 pre-send and connection failures start a cooldown from 2 to 30 seconds.
A stream failure on an open connection leaves that cooldown alone, and an
outcome from a replaced connection cannot modify its successor. An HTTP/3
answer resets the cooldown. A definitive HTTP 4xx response disables HTTP/3
until pool replacement. Statuses 408, 414, 421, 425, 429 and 431 are excluded
from that rule.

Recursive resolution acquires a global rate token and concurrency permit
for every authoritative attempt before its server timeout starts. The
recursor owns bounded delegation, name server and DNSSEC key caches. The
Resolver's cache remains the answer cache for both resolution modes.

## State ownership and lifetime

| State | Owner | Lifetime or change boundary |
| --- | --- | --- |
| Saved configuration revision | Management Store and Manager | Each committed configuration save or rollback; rejects stale writes. |
| DNS generation | Manager/Running and Server | Listener, Resolver and upstream lifecycle; health publications carry this generation. |
| Cache instance | Resolver's cache generation | Replaced independently for cache behavior changes; old work retains its own instance. |
| Cache epoch | Cache | Invalidation fences admission by older work inside that instance. |
| Cache invalidation token | Cache | Opaque instance identity plus epoch; old tokens cannot clear a replacement instance. |
| Policy generation | PolicyHandle and subscription service | Immutable publication; each request pins its rules, with at most one retained old generation. |
| Metrics, log queue, SQLite and health | RuntimeServices | Shared across DNS generations in the process. |
| Log, totals and history epochs | Storage worker | Independent durable clear/reset boundaries. |
| Sessions and management transport | Active | Process-owned; credential or transport-realm changes revoke the appropriate sessions. |

SQLite has one worker that owns the WAL database. DNS producers enqueue bounded
structured entries without waiting for disk or serialization. Status reads use
cached storage observations. Per-request logs are opt-in and bounded separately
from aggregate metrics.

The subscription catalog is the durable selection authority. Validators bind
the final URL, request representation and accepted content. The worker
persists source objects before selecting them and never reconstructs a
missing or corrupt catalog by scanning objects. An uncertain directory sync
after a rename keeps both catalog versions and pauses collection until
reconciliation. Background work rechecks the saved configuration, selected
content, local rules and update freeze before publishing. Configuration apply
persists its configuration before the policy swap, while background refresh
commits catalog selection before publishing its aggregate.

Management observation has no configuration or credential authority. Manager
publishes committed lifecycle metadata and weak Resolver references to a short
read lock; status, statistics and query-log reads do not acquire the configuration
mutex. Metrics, health, listeners, cache and storage are read from their owners.
Status combines these observations rather than promising one atomic snapshot of
all counters. It cannot keep a stopped Resolver alive or authorize a mutation.
`lifecycle_phase` distinguishes stable state from starting and stopping. Status
readiness additionally requires a stable matching generation; runtime readiness
and history retain their existing health owner. A generation mismatch means a
handover, not proof that a task exited. One status render pins one cache instance.

## Configuration transactions

The console keeps one in-memory draft. Rust owns parsing, preview, validation and
the persisted configuration. A save carries the saved revision. The existing
mutation permit serializes accepted changes; accepted transactions outlive their
initiating HTTP connection. Network uncertainty is confirmed through reads rather
than replaying a mutation.

The task takes a short Manager lock to copy preparation input, releases it while
reading material and building the candidate, and reacquires it to commit. The
mutation permit remains held throughout. Commit rechecks the saved revision and
the live hot-update target; failed preparation publishes no candidate state.

```mermaid
flowchart LR
    Accepted[Accepted task and mutation permit] --> Input[Manager: copy input]
    Input --> Prepare[Prepare without Manager lock]
    Prepare --> Candidate[PreparedCandidate]
    Candidate --> Commit[Manager: check revision and live target]
    Commit --> Persist[Bind if needed, then persist]
    Persist --> Publish[Publish committed state and lifecycle phase]
```

`ChangeImpact` names updates-only, hot (with explicit cache replacement), and
restart changes. Validation and application use the same source-document
classification; a stopped or exited runtime cannot receive a hot change.

Updates-only changes preserve a stopped or running DNS service. Supported cache,
storage, statistics and filtering changes preserve listeners, replacing only the
state whose behavior changed. Candidate material is prepared before stopping DNS.
Changes requiring a restart then stop the old generation, bind every candidate
listener, persist the configuration, and start the new generation. Failure attempts
to restore the saved configuration. Startup and
recovery may serve degraded while individual listeners retry; a newly applied
candidate must bind every listener before persistence.

Read-only observation remains available during preparation and persistence. Its
configuration revision advances only after the transaction commits. Credentials,
session checks and update freeze keep their existing owners. Session and saved
configuration reads remain available during candidate preparation; persistence
and runtime replacement remain serialized by Manager.

## Startup and terminal shutdown

File mode loads TOML directly and owns its subscription scheduler. The managed
entry point takes ownership of a bound socket without releasing its address,
opens the private Store and RuntimeServices, and starts the
saved DNS configuration; process-owned schedulers manage subscriptions, startup
retry and software checks.

A clean cache snapshot is durably consumed before serving. Its contents are
restored only when the configuration and policy fingerprint matches. Only the
terminal shutdown owner may publish another clean snapshot, after DNS and
background work are quiescent. Configuration apply and rollback do not save one.
Cache save and storage flush have separate bounded budgets; storage releases its
directory lock only after the save boundary.

During listener shutdown, only streams that received a complete DNS query
can force a stop. This requires the end of the request stream for DoH and FIN
for DoQ. Unfinished handshakes and incomplete queries are cancelled promptly.
Peer close notification uses at most half the shutdown grace. Outside a
generation stop, an adapter exit drains independently and its supervisor
rebinds the affected listener while the shared resolver and upstream pool stay
available.

TCP admission pauses briefly on descriptor or buffer exhaustion. An error
limited to a pending connection leaves the listener available. QUIC
admission sends a stateless Retry to an unvalidated address before taking
a connection slot or a source budget.

## Updates and security boundaries

The authenticated management API applies Host/Origin checks and an HttpOnly Cookie
plus an in-memory session binding. The browser never persists that binding,
credentials or configuration drafts. Live-log sockets have their own admission,
authentication, resource limits and shutdown owner.

The live log endpoint requires Origin and the session Cookie before upgrade.
The first message supplies the in-memory binding. Failure to authenticate
closes with 4401, session revocation or expiration closes with 4403, and idle
timeout closes with 4408. The management shutdown drain closes sockets with
1001. The storage worker owns polls, and each accepted subscription receives
a `subscribed` acknowledgement before its entries. Independent `state`
updates cannot acknowledge a change of subscription.

Authentication attempt budgets use the socket peer, grouping IPv6 addresses
by /64 and treating IPv4-mapped peers as IPv4. Password hashing has a
separate global concurrency budget. A cancelled caller's blocking hash
holds its permit until the computation finishes. Credential changes
preserve the saved configuration revision and revoke sessions at commit.

Changing the management origin navigates to the new console document and revokes
existing sessions. Cross-site entry permits only a top-level `GET /` navigation
(`Sec-Fetch-Mode: navigate`, `Sec-Fetch-Dest: document`) after Host/Origin checks.
API, asset and embedded requests remain blocked across sites; the new document
loads its assets and signs in through same-origin requests.

The application update coordinator persists intent and uses the existing mutation
permit for the update freeze. The root updater accepts only fixed operations and
fixed installation paths; its journal is the installed-identity authority. It
does not own DNS, configuration or runtime databases and never executes a
candidate as root. Changes to this helper contract require separate release and
installation compatibility decisions.

A matching terminal fence and installed identity resolve outstanding commit
intent. A Stage refusal is recorded under its operation ID and phase nonce,
including refusals before an operation begins. This lets the application
reconcile without replaying Stage. A matching Commit that fails a
pre-installation check records failure before returning. Boot rollback
publishes the restored launch for readiness verification. A
`manual_required` operation remains open until a reinstallation proves
readiness, and the journal keeps its terminal fences across registration.
