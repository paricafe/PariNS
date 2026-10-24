# Changelog

## v0.1.27 — 2026-10-24

### Changed

- When a client cancels a cache miss after PariNS sent the upstream query (in
  recursive mode, after resolution started), the query finishes in the
  background within its deadline and its answer can fill the cache. These fills
  share the prefetch limits `max_inflight` and `rate_per_sec`, also when prefetch
  is off; over the limits, or when DNS stops or the cache is replaced, the
  upstream query is cancelled with the client.
- A DoH3 answer that fails to write because the client stopped or reset the
  stream counts as cancelled, not completed, and as `stream_peer_cancelled`, not
  `http_request_failed`. A connection loss at that point counts as
  `stream_connection_lost`.
- A cancelled query in the query log keeps the cache lookup result made before
  the cancellation, so log rates can count it as a cache miss or hit.
- The operations guide now states that `upstream_failures` includes SERVFAIL
  answers from the upstream.

### Security

- Update tokio to 1.53.3 for RUSTSEC-2026-0386 (`AsyncFd` constructors violate
  I/O safety).
- Update the console's test dependency undici to 8.12.3 for GHSA-xjj6-f33q-929c,
  GHSA-4qv8-2p9j-2685 and GHSA-f72v-6w45-q472. The shipped console is not
  affected.

## v0.1.26 — 2026-10-23

### Fixed

- The online update preflight compares only the inputs both versions read (saved
  configuration, rule files, downloaded subscription lists, certificates and the
  upstream CA), not digests derived by one version, so an update no longer fails
  because a new version encodes its rules differently. A failed preflight logs
  which inputs differed. The comparison runs in the installed version, so an
  installation of 0.1.25 or earlier needs one offline upgrade with the install
  script to reach this version; later online updates work again.

## v0.1.25 — 2026-10-23

### Added

- Blocked queries can be answered with an empty answer (the default, now with a
  SOA), `0.0.0.0` and `::`, chosen addresses, NXDOMAIN or REFUSED, set globally in
  `[filter]`, per local block rule, or per subscription source. Block answers carry
  Extended DNS Error 15 for EDNS clients and a TTL of 10 seconds by default
  (`blocked_ttl_secs`). When several rules match, the most specific one chooses;
  the query log shows the rule and the method.

### Changed

- New cache defaults: 262,144 entries and 256 MiB (was 32,768 and 32 MiB),
  positive TTL cap 1 day (was 1 hour), negative TTL cap 15 minutes (was 5), 16
  shards (was 4), prefetch 32 at once and 100 per second (was 8 and 20), snapshot
  limit 256 MiB (was 32 MiB). Installations whose configuration omits these keys use the new
  values after upgrading.
- Cache settings accept larger values, up to 16,777,216 entries, 64 GiB, 7-day TTL
  caps, 256 shards and 4096 cache rules; the configuration guide lists every range
  and how memory and snapshot time grow with the entry count.
- The cache snapshot is written through a buffer, so a large cache saves several
  times faster at shutdown. Saving and restoring start from the most recently used
  entries and stop on time (8 s at shutdown, 4.5 s at startup), keeping what they
  wrote or loaded; before, a restore that ran out of time failed that DNS start.
- The query log's refresh interval starts at **Manual** and is remembered in the
  browser.


### Fixed

- The overview trend chart's tooltip no longer moves the page while it is shown.

## v0.1.24 — 2026-10-22

### Changed

- The management listener keeps serving after a failed connection accept and
  pauses briefly when file descriptors run out, like the DNS listeners. Before,
  such an error ended the process, including DNS in managed mode.
- In the query log, **Manual** closes the live connection; **Refresh** reloads the
  newest page.
- During a software update, subscriptions show that updates resume after it
  finishes.
- The sign-in page shows an error and **Retry** when the sign-in options cannot be
  loaded. The live query log reconnects after a message it cannot read.
- Query log list requests with invalid parameters are answered with 400
  `INVALID_REQUEST`; the console shows the server's message for errors it has no
  translation for, and translates the "already set up" error.
- Diagnostic times (`since_ms`) are always numbers.
- The session, transport and certificate import responses drop fields the console
  never read: `certificate_source`, `transport_change.from`,
  `transport_change.reauthenticate`, `expires_in_seconds`, `key_matches` and
  `certificate_count`.
- Certificate import checks the owner of the private directory and refuses
  symbolic links, like the other private files.
- The console no longer uses Tailwind. Its stylesheet is smaller and relies on CSS
  `color-mix()` (Chrome 111, Safari 16.2, Firefox 113 or later).
- The service log names the cause when the subscription store cannot be opened,
  when an update step fails (once per new error), and when a directory sync fails
  after the Turnstile keys are cleared. A management bind failure names the address.
- The installer's last lines tell you to point clients at the server and open the
  DNS ports you enable; staging reports where it staged.
- The configuration guide lists the known limitations of recursive mode.

## v0.1.23 — 2026-10-21

v0.1.22 was tagged but never published: its release build failed the dependency
audit. v0.1.23 contains all of its changes plus the dependency update below.

### Security

- Updated `rustls-webpki` to 0.103.16 and `webpki-roots` to 1.0.10
  (RUSTSEC-2026-0382, RUSTSEC-2026-0381), so certificate verification no longer
  accepts distrusted intermediates.

### Added

- Optional Cloudflare Turnstile on the sign-in page, set in **Account &
  security**. It is checked before the password and counts against the sign-in
  attempt limit; sign-ins from loopback (an SSH tunnel) skip it. The secret key is
  kept in a private state file, never in the configuration, and is never shown
  again. Sign-in attempts from IPv6 clients are counted per /64 network.
- Upstream diagnostics name each upstream and the address it was reached at.

### Changed

- Saving applies at once without a confirmation, except for a change of the
  console address or protocol. The console no longer shows configuration
  revisions; a save that meets a change made elsewhere reloads it and keeps your
  edits. The save bar has no preview; **Show changes** and **Check
  configuration** are on the Advanced (TOML) page. Rollback is **Undo last save**.
- Certificate reload and import, cache clearing, query log and statistics
  clearing, subscription work and updates no longer take a configuration
  revision, and none of them wait for unsaved settings.
- Saving a new subscription source downloads and checks it first; its ID is
  filled in from the name.
- A lost response to a change is confirmed by reading the server state again
  instead of asking you to check.
- The overview shows one cache hit rate that includes queries joined to an
  in-flight lookup.
- Account changes no longer require saving settings first; they ask before
  discarding unsaved edits.
- Console text and diagnostics no longer show internal identifiers.
- A software update stays available after an unrelated settings save.

### Fixed

- After an online update, the overview no longer keeps offering the version that
  is already installed.

## v0.1.21 — 2026-10-19

### Removed

- The installer option `--enable-updater` and adoption of v0.1.4 installations.
  An older layout is refused with instructions to back it up and remove it by
  hand before installing.

### Changed

- One-command install: `curl -fsSL https://github.com/paricafe/PariNS/releases/latest/download/bootstrap.sh | sudo sh`.
  A fresh install prints three steps: an SSH tunnel command, the local console
  address and the one-time setup token itself.
- First-run setup asks for the token, the administrator account, a listening
  mode (local testing on 127.0.0.1:5353, or LAN and internet on 0.0.0.0:53) and an
  upstream preset (Cloudflare, Google, Quad9 or AliDNS over DoH) or custom
  upstreams. An address already in use is reported as such (`CONFIG_CONFLICT`).
- Console text uses one set of standard terms with short help lines, in both
  languages; update failure reasons are translated.
- QUIC listeners (DoQ, DoH3) answer clients whose address is not yet validated
  with a stateless Retry before taking connection slots, which costs such clients
  one extra round trip.
- An HTTP/3 upstream failure falls back to HTTP/2 only when the query was not
  sent; once sent, the failure is that query's failure and the query is never
  resent. A hostname upstream moves to its next address only when nothing was
  sent. Upstream queries advertise an EDNS UDP size of at most 1232 bytes.
- `/healthz` and `/metrics` have their own small connection budget.
- PariNS raises its soft open-file limit to the hard limit at startup, and the
  file-mode service sets `LimitNOFILE=524288`.
- Storage cleanup runs on its interval or when a limit is exceeded instead of on
  every flush. At shutdown the cache snapshot and the storage drain run in
  parallel with separate time limits.
- The setup preview counts against the sign-in attempt budget.
- README and guides are rewritten for the current design.

### Fixed

- Saved local rules that cannot be read no longer let DNS start with no local
  rules after a failed configuration change rolled back; DNS stays stopped and
  status shows the reason.
- EDNS TCP keepalive is removed from client queries and upstream answers, so such
  queries are cached and coalesced, UDP upstreams no longer reject them, and DoQ
  clients receive answers that carried it.
- A connection error or a temporary lack of file descriptors while accepting a
  TCP, DoT, DoH or metrics connection no longer stops the listener for up to a
  minute; exhaustion is logged at most once a minute.
- Encrypted replies are padded up to the maximum DNS message size instead of the
  client's UDP size.
- In recursive mode, DNSSEC-aware clients receive the NSEC/NSEC3 proofs a
  wildcard answer needs.
- The cache snapshot keeps the most recently used entries across all shards.
- A listener that recovers and fails again logs the new failure at once; one
  listener's failure during shutdown no longer cuts short the others' drain.
- DoH advertises HTTP/3 only while the DoH3 listener is running.
- A new runtime database is created atomically, and opening it no longer holds
  status locks during queries.
- Managed `--check` also validates the saved rollback configuration, and an
  update cancelled before installation is reported as cancelled.
- Committing an update no longer blocks other console requests during the
  preflight check.
- Console fields with units (sizes and durations) reported an out-of-range value
  as a whole number of bytes or seconds, such as "1048576 to 536870912", although
  they accept exact decimals in any unit. The message now shows the bounds in
  units, such as "1 MB to 512 MB". The unit choices no longer offer a unit larger
  than the field's maximum, such as GB for the 512 MB cache snapshot limit.

## v0.1.20 — 2026-10-19

### Changed

- Dark mode glass is flatter: a near-uniform hairline edge instead of a bright
  top edge, no drop shadow under panels, no top sheen, a neutral fill close to
  the background instead of a blue-grey one (a denser neutral fill over patterns
  and images), and no glow on the status dot. Light mode is unchanged.

### Fixed

- After an online software update, the new process left filter subscription
  storage closed even once the update finished, so saving any configuration with
  active subscriptions and refreshing subscriptions failed with
  `subscription_storage_unavailable` until the next restart. Storage now opens when
  the update completes. Filtering itself was not affected.

## v0.1.19 — 2026-10-19

### Changed

- Console settings use standard DNS terms as titles (for example 最大 TTL /
  Maximum TTL, 预取 / Prefetch, 过期缓存（Serve stale）, DoT 连接池 / DoT
  connection pool), with plain-language explanations in the help text, in both
  languages. `cache.max_variants`, `cache.ecs_sharing.min_networks` and
  `upstreams.max_extra_inflight` gained help text. Configuration keys and values
  are unchanged.
- Console glass surfaces are calmer: the light that followed the pointer and the
  hover lift on overview cards are gone, every glass surface (including popups)
  has the same symmetric edge highlight and a light neutral shadow, and the
  primary button is a solid colour. Text on glass keeps WCAG AA contrast over
  every background, including custom images.
- Query log rows no longer expand in place. Each record has a details button: on
  a mouse, hovering it opens a detail card that you can move into; a click, tap,
  Enter or Space keeps the card open until it is pressed again, Escape is pressed
  or you click elsewhere. Live insertion pauses while a card is open.
- `[recursion] max_queries_per_request` is documented as a per-lookup budget: each
  DS or DNSKEY lookup for DNSSEC validation has its own, and the outbound rate
  and in-flight limits cap the total. The `parins_recursion_deadline_total` help
  now says it also counts cancelled requests, and
  `parins_recursion_outbound_inflight` includes queries waiting for a rate token.
- README and guides describe optional recursive resolution and learned ECS
  sharing; the operations guide adds a checklist for switching to recursive mode.

### Fixed

- Cache inspection reported `shared` for a name whose other address family was
  still observing, and kept reporting `shared` after the one-hour evidence had
  expired; it now reports the most cautious state.
- In recursive mode, a client served by the cache while another request for the
  same name was being admitted received no AD bit for validated data.

## v0.1.18 — 2026-10-18

### Added

- Optional recursive resolution with DNSSEC validation:
  `[resolution] mode = "recursive"` (default `"forward"`) resolves from the root
  servers instead of `[upstreams]`. Only PariNS filter rules apply, and queries to
  authoritative servers are plaintext. With ECS enabled, the client subnet goes
  only to the answer's authoritative servers; answers are cached by the scope
  they return, and answers without ECS are shared within the address family.
  Bogus data returns SERVFAIL with Extended DNS Error 6 (never stale data); AD
  is set for validated data when the client sets DO or AD, and CD returns
  unchecked data. `[recursion]` bounds outbound queries per second and in
  flight, queries per resolution, referral depth and the infrastructure cache.
  Status adds `resolution_mode` and `diagnostics.recursion`;
  `diagnostics.upstreams` is null in recursive mode, and `/metrics` adds
  `parins_recursion_*` counters. Changing the mode restarts DNS with a cold
  cache.

### Changed

- The appearance sheet no longer repeats the theme choice; the top-bar button is
  the only theme control.
- A custom background image is checked by its pixel size instead of its file
  size: images over 100 megapixels, or whose size cannot be read from the file,
  are refused before decoding.
- Optional learned ECS sharing, `[cache.ecs_sharing]` (off by default). When an
  upstream answers ECS queries without an ECS option, PariNS normally caches the
  answer separately for each client network. With sharing enabled, a name whose
  answer was identical for at least `min_networks` (default 3) networks across two
  or more /16 (IPv6 /32) prefixes is cached once for its address family (scope
  `learned_v4`/`learned_v6`). Any different answer revokes this immediately and
  blocks relearning for `revoke_cooldown_secs` (default 3600). No-ECS, privacy and
  the other address family are never shared. Cache statistics add
  `ecs_shared_hits`, `ecs_shared_entries` and `ecs_sharing_revocations`, and cache
  inspection reports `ecs_sharing` (`observing`, `shared`, `revoked` or null).
- The cache semantic version is now 4: the first start after upgrading skips the
  previous cache snapshot and begins cold.

## v0.1.17 — 2026-10-16

### Changed

- The query log page can refresh itself every 1, 3, 5 or 10 seconds over a new
  authenticated WebSocket, `/api/query-log/live`, instead of only manually. New
  records arrive in batches once storage commits them (about
  `storage.flush_interval_ms`, 1 s by default). Each browser session may hold 2
  live connections and the process 16; they close on logout, credential change,
  a management address change, 30 s without client messages, or shutdown.
- Query log: new records are inserted in batches and pause while you read a row,
  page back or filter (a pill shows how many are waiting); details now open inline
  under the hovered or focused row instead of in a side sheet, with a plain-language
  note beside each raw cache scope code.
- The overview trend chart shows each point's values on hover, tap or arrow keys,
  and marks periods when the service was not running.
- One theme button cycles system, light and dark. The background is a neutral
  solid color by default, with color presets, patterns and an optional custom
  image that is resized in the browser and kept only there (IndexedDB), with an
  overlay strength setting. Glass panels are clearer, brighter on HDR screens and
  have larger, continuous corners. The browser saves only language and appearance.

## v0.1.16 — 2026-10-16

### Changed

- Listeners are supervised one by one, in file and managed mode. If one cannot
  bind (for example a port taken by another program after a reboot) or stops while
  running, the other protocols keep serving and only that listener is rebound after
  5 seconds, doubling to at most once a minute. DNS health is then `degraded` (not
  ready), `/api/status` lists each listener's state and last error, and the console
  overview names the missing listeners. Before, one failed listener stopped every
  protocol. Saving a configuration still requires all of its listeners to bind.
- When DNS stops, a DoH, DoH over HTTP/3 or DoQ stream that has not delivered a
  complete query yet is cancelled at once instead of holding the shutdown until
  the grace period ends. A slow client no longer prevents the clean cache snapshot.
- A clean shutdown logs the cache snapshot it wrote: entries saved and skipped,
  bytes and time taken.
- The cache page notes that its entry count includes expired entries not yet
  removed.

## v0.1.15 — 2026-10-15

### Fixed

- A saved configuration that fails to start, for example because another program
  took a listener port after a reboot, no longer leaves DNS silently stopped.
  PariNS logs the failing protocol and address (such as
  `bind DoH 0.0.0.0:443: Address in use`), shows it on the console overview,
  and retries the same configuration after 5 seconds, doubling to at most once a
  minute, until it starts or another configuration is applied. Repeated identical
  errors are logged at most every ten minutes. All listeners still start together.
- The startup integrity check of an existing runtime database no longer gives up
  after 5 seconds. It runs in the background while DNS starts, the storage status
  shows "checking" meanwhile, and only shutdown interrupts it. Before, a large
  database on a cold disk cache could leave statistics and query logs unavailable
  until the next restart.

### Changed

- Monitoring guidance: use `dns_health` from `GET /api/status`; see
  [Operations](guide/operations.md#query-log-and-storage).

## v0.1.14 — 2026-10-15

### Changed

- Internal simplification with no intended change to DNS, cache, filtering or
  management behavior: the unused filter-index prototypes and benchmarks were
  removed (the radix index remains the only filter engine), HTTP/2 and HTTP/3
  share one DoH answer check, and duplicated validation and test helpers were
  consolidated.
- TLS client settings are built only for HTTPS and QUIC upstreams. An unreadable
  `[upstreams].ca_file` is still rejected for every upstream type; the error now
  reads "open TLS CA file".
- The guides were reorganized and shortened: first setup is one section in
  [Deployment](guide/deployment.md), and acceptance workflows moved to
  [Development](guide/development.md#acceptance-workflows).

## v0.1.13 — 2026-10-15

### Changed

- DoH over HTTP/2 now also sends RFC 8484 GET requests; POST is no longer used.
- The storage status shows when a query log entry was last dropped, next to the
  dropped count.

### Fixed

- With HTTP/3 preferred, a slow upstream answer no longer counts as an HTTP/3
  failure. The 250 ms HTTP/3 budget now covers only connecting and sending;
  after that PariNS waits for the answer until the query deadline instead of
  giving up, cooling HTTP/3 down for 30 seconds and asking again over HTTP/2.
  Connection, transport and protocol failures still fall back and cool down.
  An HTTP/3 connection now sends a keep-alive every second and closes after
  3 seconds without a reply, so a silently dead path stops costing queries
  quickly; an unused connection still closes after 10 seconds.
- A 414 or 431 answer to one oversized query no longer stops HTTP/3 for the
  whole upstream.
- Some servers answered HTTP/2 POST queries with 405; GET avoids this.

### Security

- Update rustls to 0.23.46 for RUSTSEC-2026-0346, RUSTSEC-2026-0347,
  RUSTSEC-2026-0348 and RUSTSEC-2026-0349.

## v0.1.12 — 2026-10-12

### Changed

- Prefetch now refreshes earlier and more often by default: `min_hits = 1`,
  `remaining_percent = 25`, `max_inflight = 8` and `rate_per_sec = 20`.
  Prefetch stays off by default. Installations that set these fields keep their
  values; only fields left unset pick up the new defaults.
- The overview shows, next to the cache hit rate, the share of queries that
  joined an identical query already in progress instead of asking an upstream
  again. The hit rate itself is unchanged.
- DoH over HTTP/2 keeps one reusable connection per upstream instead of opening
  a new TLS connection for every query, so most queries skip the connection
  setup round trips. A connection closes after 10 seconds without queries, is
  replaced after the server sends GOAWAY or fails, and is retired when an
  HTTP/2 PING goes unanswered. A query that was already sent is never resent.
- Upstream attempt details in query logs and transport diagnostics show whether
  an HTTP/2 or DoT query used a new or reused connection.
- Software updates has its own page in the console navigation instead of
  sitting under Performance and limits.
- Console text is shorter: the login and first-run pages show only what is
  needed to sign in or set up, page eyebrow labels and explanations under the
  overview numbers are gone, and the remaining help uses plain short sentences.

### Fixed

- DoH over HTTP/3 now sends queries as RFC 8484 GET requests. Some servers
  answered the previous POST requests with 405, which made every query wait for
  the HTTP/2 fallback. When a server still refuses HTTP/3 with a definite
  client error, PariNS stops trying HTTP/3 for that upstream until the upstream
  configuration is next applied, instead of retrying every 30 seconds.
- A failed update check's diagnostic code stays inside the version group.

## v0.1.11 — 2026-10-12

### Upgrade notice

- The cache snapshot format changed. The first start after upgrading skips the
  snapshot written by the previous version and begins with an empty cache;
  later restarts restore normally. DNS answers are not affected.

### Changed

- Simplify console text: each idea is explained once per page, field
  explanations sit behind an ⓘ button, byte counts use KB/MB/GB, internal
  version numbers and codes move into collapsed details, and buttons appear only
  when they can be used. Raw diagnostics remain available.
- Give the console an iOS-style layout and motion: large titles that collapse
  into the top bar, grouped settings lists, switches, segmented controls,
  popup menus, steppers, sheets, action sheets and banners. Previewing changes
  moves to the save bar, and the settings toolbar becomes a More menu.

### Fixed

- A cached query name whose labels contain characters such as `:` or `/`
  (for example a client asking for `https://example.com`) made the whole clean
  cache snapshot fail to restore, so the next start began with an empty cache.
  Snapshots now store names in DNS wire format, so every cached name restores.

## v0.1.10 — 2026-10-12

### Changed

- Redesign the web console with a glass look, light and dark themes, and
  restrained motion that respects reduced-motion and reduced-transparency
  settings. The overview now leads with a one-line status, a status strip and
  four key numbers, and moves storage, timing, cache and transport diagnostics
  into a collapsed "Advanced diagnostics" section.
- Show byte, millisecond and second settings with a unit selector. The
  configuration still stores the original units, and input that cannot be
  converted exactly is passed to validation unchanged.
- Summarize the network settings in the last first-run setup step and keep the
  full TOML in a collapsed editor.
- Rewrite console text in both languages to describe results instead of
  internal mechanisms. Raw diagnostic values remain visible.

### Fixed

- Cache inspection showed untranslated labels for a miss and for an empty ECS
  scope.
- Address-in-use and invalid-query errors from the management API showed only
  the raw English message.
- Stop the updater's request path while the installer runs and restore it
  afterwards. A request written during installation, such as the Abort that a
  recovering service sends at startup, previously hit systemd's trigger limit,
  left the path unit failed and made a successful installation exit with an
  error.
- If the request path cannot be enabled after a committed installation, the
  installer now completes its usual summary, then reports that the request path
  is not enabled with the commands to enable it, and does not roll back.

## v0.1.9 — 2026-10-11

### Upgrade notice

- Install this version with the official installer. The readiness fix lives in
  the root updater helper, and an online update replaces only the application,
  so updating online from v0.1.7 or v0.1.8 would still run the old helper and
  can fail readiness the same way. Online updates benefit from the fix only
  after v0.1.9 or later has been installed with the installer.
- An installation frozen with "Update recovery required" recovers by installing
  this version with the installer: after the new installation passes readiness,
  the unfinished update is closed and configuration changes unfreeze. Keep a
  separate backup first.

### Fixed

- Fix online updates that failed readiness and rolled back although both the
  candidate and the restored program were healthy. The updater could hash the
  service process before systemd had executed the program; it now checks the
  executable only after that process reports its own health.
- A verified reinstallation now completes recovery after an update that needed
  manual recovery: the unfinished update is closed as superseded and
  configuration changes unfreeze. Previously the console stayed frozen even
  after a successful reinstall. Registration keeps earlier terminal outcomes.
- Show "Update recovery required" with the reinstall instruction instead of
  "update components are unavailable", and report an installation in progress
  separately.

### Operational notes

- Installations that are already frozen need this fixed version installed with
  the official installer; the fix cannot be delivered by an online update while
  the updater reports manual recovery.

## v0.1.8 — 2026-10-11

### Upgrade notice

- v0.1.7 managed installations can update from the console: the updater
  contract, helper protocol and data formats are unchanged. This is the first
  official version pair for an in-place update; treat it as such and keep a
  separate backup. v0.1.5/v0.1.6 installations still require the installer,
  as described for v0.1.7.
- The cache snapshot format and semantics are unchanged, so a snapshot that
  v0.1.7 publishes remains restorable. v0.1.7 itself may skip publishing it when
  idle encrypted-DNS clients are connected; v0.1.8 fixes that for later stops.

### Fixed

- Bound how long a stopping DoH HTTP/2 connection waits for an idle client to
  acknowledge shutdown to half of `shutdown_grace_ms`. A silent connection (for
  example a sleeping device behind NAT) no longer forces a normal stop, so the
  clean cache snapshot is published again; responsive clients still get a
  graceful close. Requests still resolving past the grace keep forcing the stop.
- Log the cause when a stop is not quiescent and no cache snapshot is saved, for
  example `cache snapshot not saved: shutdown not quiescent (listener_drain)`;
  the cause names the first deadline or failure that forced the stop.
- Stop DoQ/DoH3 connections that have not finished their QUIC handshake or HTTP/3
  setup immediately, and bound QUIC close notification to half of
  `shutdown_grace_ms`. An unfinished or unresponsive QUIC peer no longer forces a
  normal stop and loses the clean cache snapshot.
- Let admitted DoH requests finish during a stop even when the client closes its
  connection, instead of forcing the stop.
- Omit the empty `additional_terminal` field from the updater status file, so an
  older application restored by an installer rollback can still read the status
  written by the new helper.

### Changed

- Correct the HTTPS upstream documentation: HTTP/3 connections are reused, while
  HTTP/2 currently opens a new connection for each query.
- Show the subscription, storage and security console pages with fewer status
  requests, keep Logs pages and messages when switching language, and align the
  subscription disk-budget minimum with the server (256 KiB).

## v0.1.7 — 2026-10-10

### Upgrade notice

- Existing v0.1.5/v0.1.6 managed installations report this version as a manual
  upgrade: it requires updater helper protocol 2, which an online binary-only
  update cannot install. Reinstall the current managed layout from a verified
  v0.1.7 package or bootstrap to update the application and helper together.
  Account, configuration and runtime data are kept; keep a separate backup first.
  There is no automatic migration of older layouts.
- After upgrading, sign in again. The first start begins with a cold DNS cache.

### Added

- Add an authenticated administrator account endpoint and bilingual console form
  for online username/password rotation with current-password verification.
  Successful saves revoke all existing sessions without changing DNS configuration,
  runtime data, certificates, subscriptions or software-update state.

### Changed

- Increase the default cache capacity from 4096 answers / 8 MiB to 32768 answers /
  32 MiB, retaining the existing negative-cache partition, TTLs and ECS isolation.
  The setup template mirrors the Rust defaults; omitted capacity fields use the
  new defaults on next parse, while existing explicit settings are unchanged.
  Memory grows on demand rather than being reserved up front.

### Fixed

- Cache proven CNAME-chain terminal NXDOMAIN/NODATA responses with one shared
  classification for admission and restoration. Preserve negative TTL/budget,
  query-type and ECS isolation, and exclude negative entries from stale/prefetch.
- Align the setup template's query-log limit with the existing Rust default of
  100000 entries instead of 1000. Logging remains disabled by default, with the
  same 64 MiB log and 128 MiB database budgets; actual retention may be shorter.
- Persist explicit updater Stage admission refusals so a rejected operation can
  reconcile without restarting the service. Preserve the existing operation,
  installation identity, cooldown and late-Commit fences.
- Use the same updates-only classification for configuration validation, restart
  prediction and apply. Unchanged DNS materials are not reloaded when changing
  update settings, and a stopped DNS service remains stopped.
- Recheck credential snapshots before issuing login sessions and recheck old
  sessions at protected mutation admission. Preserve already accepted background
  work and the updater's consumed-plan/freeze semantics.
- Distinguish uninitialized setup from an initialized managed reinstall in success
  guidance, using the existing verified preflight state. Initialized installations
  keep their original address/account and no longer receive first-setup token or
  guessed HTTP address instructions.

### Operational notes

- The cache semantic fingerprint advances to 3 (snapshot wire format stays 2), so
  older snapshots are skipped. No business-data format changes.
- After a credential change, sign in again on every tab; resolve an unknown result
  by explicit login, not by repeating the change. Offline recovery, additional users
  and password-algorithm changes are not included.

## v0.1.6 — 2026-10-08

### Fixed

- Select the current release by default in the Linux bootstrap. The v0.1.5
  bootstrap incorrectly selected v0.1.4 and then rejected that older package
  because it does not contain the updater components.
- Bind bootstrap regression fixtures to the Cargo version and stage each actual
  native release package through the default bootstrap before publication.
- Require the public quick-start check to install the exact published version.

### Operational notes

- DNS, filtering, updater protocols and durable-data formats are unchanged from
  v0.1.5.
- Existing v0.1.4 installations still require explicit updater enrollment from
  a verified newer package. Follow the
  [v0.1.5 enrollment guidance](https://github.com/paricafe/PariNS/blob/v0.1.5/CHANGELOG.md#v015--2026-10-08)
  and keep a separate backup; there is no automatic migration.

## v0.1.5 — 2026-10-08

### Upgrade notice

- This is the first release with the managed Linux updater. The official v0.1.4
  release has no updater; enroll an existing exact v0.1.4 managed installation
  manually from a verified v0.1.5 package with `install.sh --enable-updater`.
  Custom layouts are not adopted, and no data or configuration migration is
  performed. Keep a separate backup before enrollment. If enrollment fails after
  the new binary has run, v0.1.4 is not restarted automatically against data it
  may no longer understand.

### Added and changed

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
- Reclaim unreferenced prepared subscriptions under disk pressure before admitting
  downloads, while preserving configured sources and work-pinned material.
- Apply the same disk-pressure admission to first-time subscription downloads
  in file mode, including sources with automatic updates disabled.
- Show subscription update schedules from saved settings, with explicit manual-only
  and inactive states that remain unchanged while editing drafts.
- Recover interrupted update intents through root Abort arbitration after a
  management restart; retain the freeze until the matching durable outcome is known.
- Reply to UDP queries from their original local destination address on Linux and
  macOS, including wildcard dual-stack listeners and IPv6 interface scopes.
- Reject inbound certificates whose leaf EKU does not permit server authentication
  before import, configuration save or reload, keeping the active identity unchanged.
- Avoid hashing the executable at startup for independent management instances
  that cannot participate in the Linux updater installation protocol.
- Fix successful managed reinstalls reporting a cleanup error after releasing
  their installation lock.
- Keep the service mount namespace pinned during both low-privilege reinstall
  preflights when the installer runs under dash.
- Duplicate the updater's inherited installation lock with close-on-exec set
  atomically, preventing concurrent child execution from retaining the lock.

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
- Query logs use bounded asynchronous storage and can drop records under overload;
  retention time is a maximum, not guaranteed coverage.

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

Byte accounting now includes conservative per-entry metadata, so the same byte
budget may hold fewer responses; positive and negative budgets are partitioned.

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

- Linux installation requires systemd and root privileges.
- Management listens on `0.0.0.0:3000` over self-signed HTTPS; verify its
  certificate fingerprint and restrict access to administrators. Use a systemd
  override for loopback-only management.
- DNS starts after setup; the wizard starts with loopback DNS on port 5353.
- Configuration application restarts DNS and clears in-memory caches/metrics.

### Known limits

- Infrastructure abuse protection remains the operator's responsibility.
- No local DNSSEC validation, automatic certificate renewal, external blocklist
  downloads, DoH/DoQ upstream clients, password changes or reverse-proxy
  management configuration.
- Aggregate history is memory-only.
