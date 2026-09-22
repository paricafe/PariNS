# Changelog

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
