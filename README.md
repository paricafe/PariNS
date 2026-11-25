# PariNS

PariNS is a self-hosted DNS server written in Rust and shipped as a static Linux binary. It forwards queries to upstreams you choose or resolves from the root servers with DNSSEC validation. It caches answers per client subnet, blocks domains with local rules and online lists, serves plain and encrypted DNS, and includes a built-in web console.

## Features

Each PariNS binary includes six components:

- **Forwarding and recursion.** Forward over UDP, TCP, DoT, DoH over HTTP/2 or HTTP/3, and DoQ with weighted or parallel scheduling, or set `[resolution] mode = "recursive"` for DNSSEC-validating recursion from the root servers.
- **Per-network ECS cache.** Cache answers per client subnet when ECS is on, with optional learned sharing across disjoint networks, prefetch, serve stale, and query coalescing.
- **Filtering and subscriptions.** Apply exact and subdomain allow and block rules, filter CNAME chains, and sync HTTPS blocklists in domain-list or hosts format.
- **Encrypted DNS.** Serve DoT, DoH over HTTP/2 and HTTP/3, and DoQ with live certificate reload. Upstream TLS always verifies certificates and never falls back to plaintext.
- **Web console.** Run first-run setup, edit forms or TOML with preview and rollback, view statistics and an opt-in query log, switch Chinese and English or light and dark themes, and guard sign-in with optional Cloudflare Turnstile.
- **Online updates.** Check official GitHub releases and install them through a restricted root updater with automatic rollback.

PariNS is not an authoritative server. Recursive mode serves only PariNS clients, with no RPZ, DNS64, or local zones, and one instance either forwards or resolves recursively, never both.

## Quick start

You need a Linux `x86_64` or `aarch64` host running systemd with `sudo` access.

1. Run the installer on the server:

   ```sh
   curl -fsSL https://github.com/paricafe/PariNS/releases/latest/download/bootstrap.sh | sudo sh
   ```

   The script verifies checksums, starts `parins-managed.service`, and prints the SSH tunnel command and one-time setup token.

2. Open the console. Run the printed tunnel command on your computer, or skip it if your browser runs on the server:

   ```sh
   ssh -N -L 3000:127.0.0.1:3000 USER@SERVER
   ```

   Open `http://127.0.0.1:3000`, enter the token, create an admin account, and choose an upstream and listener mode. DNS starts when setup finishes.

3. Verify DNS and connect clients:

   - **Local testing only.** Run `dig @127.0.0.1 -p 5353 example.com A` on the server.
   - **LAN & internet (:53).** Point your router or devices at the server IP, and allow UDP and TCP port 53 only from your own clients.

See [Install](https://github.com/paricafe/PariNS/blob/main/guide/deployment.md#install) to review the script before running it, or [Port 53 conflicts](https://github.com/paricafe/PariNS/blob/main/guide/deployment.md#port-53-conflicts) if port 53 is already in use.

## Documentation

The guides cover the `main` branch. For a published release, also read its entry in [CHANGELOG.md](CHANGELOG.md).

- [Deployment](https://github.com/paricafe/PariNS/blob/main/guide/deployment.md). Install, first-run setup, clients, console HTTPS, file mode, source builds, and uninstall.
- [Configuration](https://github.com/paricafe/PariNS/blob/main/guide/configuration.md). Reference for every TOML section, default, range, and rule.
- [Operations](https://github.com/paricafe/PariNS/blob/main/guide/operations.md). Accounts, Turnstile, updates, certificates, backups, query logs, metrics, and troubleshooting.
- [Development](https://github.com/paricafe/PariNS/blob/main/guide/development.md). Build and test gates, benchmarks, packaging, and acceptance workflows.
- [Architecture](https://github.com/paricafe/PariNS/blob/main/guide/architecture.md). Request path, state ownership, configuration transactions, shutdown, and security boundaries.
- [Releases](https://github.com/paricafe/PariNS/releases) and [CHANGELOG.md](CHANGELOG.md). Release archives and version notes.

## License

Copyright 2026 Natsuki-Kaede and PariNS contributors.

Licensed under the [Apache License, Version 2.0](LICENSE).
