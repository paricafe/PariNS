# PariNS

PariNS is a self-hosted DNS server for a computer, a home or office network, or a
VPS. It forwards queries to upstream resolvers you choose, or optionally resolves
from the root servers with DNSSEC validation. It caches answers per client network,
blocks domains with local rules and online lists, and serves plain and encrypted
DNS. A built-in web console covers setup, settings, statistics and software
updates. PariNS is written in Rust and ships as a static Linux binary.

## Features

- **Forwarding and optional validating recursion:** forward to UDP, TCP, DoT, DoH
  (HTTP/2 or HTTP/3) and DoQ upstreams, with weighted or parallel scheduling. Or set
  `[resolution] mode = "recursive"` to resolve from the root servers with DNSSEC
  validation.
- **Per-network ECS cache with learned sharing:** with ECS on, answers are cached per
  client subnet, so each network keeps the answers meant for it. Learned ECS sharing
  stores one copy per address family only after several networks got the same
  answer, and stops at the first difference. Prefetch, serve stale and query
  coalescing are built in.
- **Filtering and online subscriptions:** exact and subdomain block and allow rules,
  CNAME checks, and HTTPS subscriptions in domain-list or hosts format.
- **Encrypted DNS inbound and upstream:** DoT, DoH over HTTP/2 and HTTP/3, and DoQ
  listeners with certificate reload without restart. Upstream TLS is always verified
  and never falls back to plaintext.
- **Console:** guided first-run setup, Chinese and English, light and dark themes,
  statistics, an opt-in query log, and a TOML editor with preview and rollback.
  Optional Cloudflare Turnstile protects sign-in.
- **Online updates:** an installed instance checks for official releases and
  installs them through a restricted updater with automatic rollback.

PariNS is not an authoritative DNS server. Recursive mode serves PariNS's own
clients: it has no RPZ, DNS64 or local zones, and one instance either forwards or
resolves recursively, never both.

## Quick start

You need a Linux x86_64 or ARM64 host running systemd, and root access.

1. **Install.**

   ```sh
   curl -fsSL https://github.com/paricafe/PariNS/releases/latest/download/bootstrap.sh | sudo sh
   ```

   The script verifies the release checksums, installs and starts the managed
   service, and prints the next steps with the one-time setup token. To read the
   script before running it, see
   [Install](https://github.com/paricafe/PariNS/blob/main/guide/deployment.md#install).

2. **Open the console and enter the token.** The console uses plain HTTP on port
   3000. On your computer, run the SSH tunnel command the installer printed and keep
   it open:

   ```sh
   ssh -N -L 3000:127.0.0.1:3000 USER@SERVER
   ```

   Open <http://127.0.0.1:3000>, enter the setup token, create the administrator,
   and choose an upstream and a listening mode. DNS starts when setup finishes.

3. **Connect clients.** With **Local testing only**, test on the server:

   ```sh
   dig @127.0.0.1 -p 5353 example.com A
   ```

   With **LAN & internet (:53)**, point your devices or router at the server's
   address, and allow port 53 only from your own clients. If port 53 is already in
   use (often by systemd-resolved), see
   [Port 53 conflicts](https://github.com/paricafe/PariNS/blob/main/guide/deployment.md#port-53-conflicts).

## Documentation

- [Deployment](https://github.com/paricafe/PariNS/blob/main/guide/deployment.md):
  installation, first-run setup, clients, console HTTPS, file mode, uninstall.
- [Configuration](https://github.com/paricafe/PariNS/blob/main/guide/configuration.md):
  every configuration section and field.
- [Operations](https://github.com/paricafe/PariNS/blob/main/guide/operations.md):
  accounts, updates, certificates, backups, query log, metrics, troubleshooting.
- [Development](https://github.com/paricafe/PariNS/blob/main/guide/development.md):
  build and test gates, packaging, acceptance workflows.
- [Releases](https://github.com/paricafe/PariNS/releases) and the [changelog](CHANGELOG.md).

The guides describe the current main branch. For an installed release, also read
its changelog entry.

## License

Copyright 2026 Natsuki-Kaede and PariNS contributors.

Licensed under the [Apache License, Version 2.0](LICENSE).
