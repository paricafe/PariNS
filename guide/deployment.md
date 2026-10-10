# Deployment

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md)

For the shortest installation path, start with the [Quick start](../README.md#quick-start).
This guide covers installation choices, network access and managed/file modes.

## Deployment choices

- **Local machine:** keep DNS and management bound to loopback.
- **Home or private network:** bind DNS to a LAN address and allow only intended
  clients through the firewall; configure those clients or your router to use it.
- **VPS or remote clients:** configure encrypted DNS listeners and their
  certificates, restrict DNS and management access at the network boundary, and
  measure capacity on the target host. Installing PariNS does not automatically
  make an unrestricted public resolver safe to operate.

## Install a Linux release

Requires a Linux x86_64 or ARM64 host running systemd, `curl`, `tar`, a SHA256
utility (`sha256sum` or `shasum`), and root/sudo access. **No Rust, Git, Node.js,
or container runtime is needed.** Static Linux binaries are provided in
[Releases](https://github.com/paricafe/PariNS/releases).

```sh
curl --proto '=https' --proto-redir '=https' -fL \
  https://github.com/paricafe/PariNS/releases/latest/download/bootstrap.sh \
  -o parins-install.sh && sudo sh parins-install.sh
```

The script detects the CPU architecture, downloads the release over HTTPS,
checks the archive and its contents against SHA256 manifests, installs the
binary, and starts `parins-managed.service`. You may inspect `parins-install.sh`
before running it. SHA256 detects corruption; it is not an independent signature
or a substitute for trusting the release publisher.

## First-run setup

This step is for an uninitialized installation, including a reinstall that only
has a setup token. An initialized reinstall keeps its existing management address
and administrator account; do not run setup again or infer a new HTTP/HTTPS URL
from the server IP. The installer prints this distinction only after
the saved configuration passes its existing preflight and installation readiness
checks. It does not inspect the setup token to decide whether setup is complete.

Open `http://SERVER_IP:3000` (or <http://127.0.0.1:3000> on the server itself).
The console switches to HTTPS after inbound DoH (with optional HTTP/3) is configured with a
valid certificate and `[web].public_host`.
Allow inbound TCP 3000 only from your administrator IPs. The installer prints
local interface URLs; a VPS behind NAT may need its provider-assigned public IP.
Read the one-time setup token locally:

```sh
sudo cat /var/lib/parins-managed/setup-token
```

Enter the token, create an administrator, and enter your upstream DNS servers,
one per line. IP addresses and encrypted DNS URLs are supported; hostname URLs
also need bootstrap DNS addresses. The wizard starts with `127.0.0.1:5353` for local testing. For LAN clients,
choose the server's LAN IP on port 53 and allow those clients through your firewall.
Port 53 must be free; conflicting DNS services are not stopped automatically.
DNS starts after setup succeeds. HTTP carries passwords, configuration and
private-key paste in plaintext; prefer local access or an SSH tunnel during
initial setup. See [Management HTTPS](#management-https)
for the DoH certificate and HTTPS transition.

## Connect clients

Point your devices or router to the DNS listen IP selected in the wizard.
Most device DNS settings require port 53. To check the initial loopback setup:

```sh
dig @127.0.0.1 -p 5353 example.com A
sudo systemctl status parins-managed.service
```

The installer does not change your host/router DNS or firewall. Use the dashboard
to inspect traffic, then configure filtering, caching, ECS, and encrypted DNS as
needed. Reinstalling the current managed layout preserves the account and
configuration. Upgrading an older layout requires explicit preparation as
described in the upgrade notice; the installer will reject it without migration.
Pin a version with `sudo sh parins-install.sh --version v0.1.8`.
Use `--dry-run` to download/verify and inspect targets without installing a service.

For offline installation, download the matching `.tar.gz` and `.tar.gz.sha256`
from the release page, verify `sha256sum --check FILE.tar.gz.sha256`, extract it,
then run `sha256sum --check SHA256SUMS` and `sudo sh install.sh` inside the package.
For a headless deployment without the console, use [File-configured mode](#file-configured-mode).

## Managed mode

### Build from source

The Quick Start installs a prebuilt release. If you prefer to build from source,
install Rust with [rustup](https://rust-lang.org/tools/install/) and run:

```sh
git clone https://github.com/paricafe/PariNS.git
cd PariNS
cd web && npm ci --ignore-scripts && npm run build && cd ..
cargo build --locked --release
sudo sh scripts/install.sh
```

Alternatively, build a host-native package with `sh scripts/package.sh`, verify
its adjacent `.sha256` and extracted `SHA256SUMS`, and run `sudo sh install.sh`
inside the extracted Linux package. macOS packages cannot be installed as Linux
services. Linux x86_64 and
aarch64 ELF binaries are accepted, with architecture checked before installation.
Use `--dry-run` to inspect planned targets without writes.

The installer enables and starts `parins-managed.service`, using
`/opt/parins-managed/parins` and private state under `/var/lib/parins-managed`. It does not
stop `systemd-resolved`, change host DNS, open firewall ports, or modify the legacy
`parins.service`. Re-running upgrades the managed binary/unit while retaining
state and keeping a private prior binary/unit backup. An installation/startup
failure restores the prior service only when its data contract permits that
recovery; first updater enrollment has the limits described in
[Software updates](operations.md#software-updates). Inspect the
reported backup and `journalctl -u parins-managed.service` if recovery fails.

### Network access and first setup

The first start serves only the console; **DNS starts after successful setup**.
The CLI and installed service default to **`0.0.0.0:3000`, HTTP** until inbound
DoH is enabled with a valid certificate and `[web].public_host`.
Open `http://SERVER_PUBLIC_IP:3000` after allowing inbound TCP 3000 in the host
firewall and cloud security group for your intended administrator IPs. The
installer does not change those rules, configure NAT, or prove Internet routing.
Because HTTP is unencrypted, restrict access and prefer local access or an SSH
tunnel for initial credentials and private-key entry. Existing installations
retain their saved configuration; this change does not erase old self-signed files.

For IPv6 use `--web-listen '[::]:3000'` and access `http://[SERVER_IPV6]:3000`.
IPv4 acceptance on an IPv6 socket depends on the operating system; the default
IPv4 socket does not claim IPv6 coverage. `--web-listen 127.0.0.1:3000` retains
local-only access. An SSH tunnel protects a remote HTTP setup connection:

```sh
ssh -N -L 3000:127.0.0.1:3000 USER@HOST
```

For a source installation, read `sudo cat /var/lib/parins-managed/setup-token` on the server. If using the tunnel,
open <http://127.0.0.1:3000>. Enter the one-time token, choose an
administrator name and a password of at least 12 bytes, and set the DNS listen
address and upstream list. The wizard defaults to loopback DNS;
set port 53 explicitly if wanted and free. The service has only the capability
needed to bind low ports. Port conflicts reject setup/application; no conflicting
service is automatically stopped. Never share the token or put it in a URL.

### Management HTTPS

The console does not create a certificate. When inbound `[doh]` is enabled,
PariNS verifies its certificate/key and the `[web].public_host` name, then serves
HTTPS on the same management port with that identity. `[doh].http3` adds HTTP/3
to that same DNS identity and port; standalone `[doh3]` is rejected. DoT, DoQ and encrypted
upstreams do not change the console protocol. The DoH DNS listener and management
listener keep separate ports, routes and ALPN. For example, DoH on 443 and the
console on 3000 can use the same certificate for `dns.example.com`; the console
URL becomes `https://dns.example.com:3000/`. The certificate must cover the
chosen name; PariNS does not infer a public IP from `0.0.0.0`, configure DNS,
or make a private CA trusted by clients.

Saving a configuration that enables inbound DoH switches the existing management
port to HTTPS and requires a new login. If setup itself enables DoH, open the
returned HTTPS address and log in with the newly created administrator account.
Disabling all inbound DoH switches to HTTP only after explicit downgrade
confirmation. A failed candidate or TLS material fault never silently exposes
the management API over HTTP. Renew external certificate files through your
issuer, then use **Reload certificates** or the [managed SIGHUP hook](operations.md#certificate-renewal). There is no built-in ACME or certificate watcher. Reload preserves the
management session and does not save configuration drafts. Old generated
`https-identity.pem`/`https-cert.pem` files are unused and not automatically
removed; check their purpose before manually cleaning them up.

For authentication, configuration transactions and state ownership, see
[Operations](operations.md#management-console).

## File-configured mode

Build `web/` first as shown in [Build from source](#build-from-source), even for file-configured mode: the Rust
binary embeds the management assets at compile time but does not start the
console unless `--manage` is used.

```sh
cargo build --locked --release
cp parins.example.toml parins.toml
# Edit parins.toml and set upstreams.servers to your chosen resolver(s).
./target/release/parins --check
./target/release/parins
```

In another terminal:

```sh
dig @127.0.0.1 -p 5353 example.com A
dig @127.0.0.1 -p 5353 example.com A +tcp
```

Use `--config PATH` to select a different configuration file and `--data-dir PATH`
for private runtime storage (default `parins-data`, not shared with managed state).
`--check` creates no runtime directory, database or cache snapshot. `listen` selects
the same address and port for UDP and TCP; port zero selects a shared ephemeral
port, printed on startup. Set `[upstreams].servers` to one or more endpoints
(see [Upstream DNS settings](configuration.md#upstream-dns-settings)). UDP endpoints
must also support TCP for truncated answers. Do not point them back at PariNS, including through a local
interface alias that configuration validation cannot identify.

Ctrl-C or SIGTERM (Unix) stops accepting traffic, closes idle TCP clients, and
allows active queries up to `shutdown_grace_ms` to finish before cancellation.

## Existing installations

The installer uses the exclusive `/var/lib/parins-managed` state directory.
It rejects unsupported older layouts and unknown directories before making changes;
it does not translate configuration or migrate data. Read the
[upgrade notices](../CHANGELOG.md#v015--2026-10-08) and
[updater enrollment requirements](operations.md#software-updates) before upgrading.
