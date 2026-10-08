# PariNS

PariNS is a self-hosted DNS forwarder written in Rust, with subnet-aware caching,
filtering, encrypted DNS and a built-in web console. Run it on a local machine,
home network or VPS, using your own upstream resolvers and policies.

## Features

- **DNS transports:** UDP, TCP, DoT, DoH over HTTP/2 and HTTP/3, and DoQ.
- **Upstream pools:** weighted selection or parallel queries across plaintext
  and authenticated encrypted resolvers.
- **Subnet-aware cache:** ECS isolation, positive and negative caching, optional
  prefetch, stale answers and request coalescing. ECS is opt-in.
- **Filtering:** local rules and HTTPS subscriptions, allow exceptions and CNAME
  checks.
- **Web console:** guided setup, Chinese/English UI, light/dark themes, statistics,
  optional query logs and configuration editing.
- **Deployment:** prebuilt Linux binaries, systemd installation, managed software
  update checks, or standalone TOML configuration.

PariNS forwards queries to existing resolvers; it is not an authoritative DNS
server or standalone iterative resolver and does not perform DNSSEC validation.

## Quick start

### 1. Install on Linux

Requires Linux x86_64 or ARM64, systemd, `curl`, `tar`, a SHA256 utility and
root/sudo access. No Rust, Node.js or container runtime is needed.

```sh
curl --proto '=https' --proto-redir '=https' -fL \
  https://github.com/paricafe/PariNS/releases/latest/download/bootstrap.sh \
  -o parins-install.sh && sudo sh parins-install.sh
```

The script downloads and verifies the release package, then starts the management
service. You can inspect the script before running it. For an existing
installation, read the [upgrade guide](https://github.com/paricafe/PariNS/blob/main/guide/operations.md#software-updates)
first; there is no automatic migration of older layouts.

### 2. Complete setup

The new console listens on `0.0.0.0:3000` over **unencrypted HTTP**. Restrict TCP
3000 to administrators and use local access or an SSH tunnel for initial setup.
Read the one-time token on the server:

```sh
sudo cat /var/lib/parins-managed/setup-token
```

For a remote server, run this on your computer and keep it open:

```sh
ssh -N -L 3000:127.0.0.1:3000 USER@SERVER
```

Open <http://127.0.0.1:3000>, enter the token, create an administrator and choose
your upstream DNS servers. DNS starts only after setup succeeds. An initialized
reinstall keeps its original management address and account; do not set it up again.

### 3. Connect clients

The wizard defaults to `127.0.0.1:5353` for local testing. Check it on the server:

```sh
dig @127.0.0.1 -p 5353 example.com A
```

For LAN use, select the server's LAN IP and port 53 in the console, allow your
intended clients through the firewall, and point their DNS settings to that IP.
Port 53 must be free. The installer does not change host DNS or firewall rules,
or stop conflicting DNS services.

## Documentation

- [Deployment](https://github.com/paricafe/PariNS/blob/main/guide/deployment.md) — offline/source installation, file mode and HTTPS setup.
- [Configuration](https://github.com/paricafe/PariNS/blob/main/guide/configuration.md) — upstreams, ECS, cache, filters and resource limits.
- [Operations](https://github.com/paricafe/PariNS/blob/main/guide/operations.md) — accounts, updates, backups, certificates and diagnostics.
- [Development](https://github.com/paricafe/PariNS/blob/main/guide/development.md) — build, test, benchmark and package.
- [Releases](https://github.com/paricafe/PariNS/releases) · [Changelog](CHANGELOG.md)

Guides describe the current main branch; unreleased features are marked.
For an installed release, also check its version notes.

## License

Copyright 2026 Natsuki-Kaede and PariNS contributors.

Licensed under the [Apache License, Version 2.0](LICENSE).
