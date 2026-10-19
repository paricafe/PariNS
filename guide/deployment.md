# Deployment

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md)

PariNS runs in one of two modes:

- **Managed mode** (recommended): the installer sets up `parins-managed.service`, and
  you configure everything in the web console. Online updates are available.
- **File mode**: PariNS reads a TOML file and has no console or online updates. See
  [File mode](#file-mode).

## Requirements

- Linux on x86_64 or aarch64 (ARM64), with systemd as the service manager.
- Root access (`sudo`).
- `curl`, `tar` and `sha256sum` or `shasum`, plus common base tools the installer
  uses (`flock`, `nsenter`, `setpriv`, `timeout`, `prlimit`, `getent`). The
  installer stops and names any missing command.

No Rust, Node.js, Git or container runtime is needed on the host.

## Install

```sh
curl -fsSL https://github.com/paricafe/PariNS/releases/latest/download/bootstrap.sh | sudo sh
```

To read the script before running it:

```sh
curl -fsSL -o bootstrap.sh https://github.com/paricafe/PariNS/releases/latest/download/bootstrap.sh
less bootstrap.sh
sudo sh bootstrap.sh
```

The script starts installing only after it has been downloaded completely. It
downloads the release package for your CPU, checks the archive against its
`.sha256` file and every packaged file against `SHA256SUMS`, then runs the bundled
`install.sh`. SHA256 detects corruption; it is not a signature, so you still trust
the GitHub release publisher.

| Option | Effect |
| --- | --- |
| `--version vX.Y.Z` | Install that release instead of the one the script was published with. |
| `--dry-run` | Download, verify and print the install targets without changing anything. |
| `--root DIR` | Stage files into an existing private directory (mode 0700) for inspection; nothing is started. |

When piping, pass options after `-s --`:

```sh
curl -fsSL https://github.com/paricafe/PariNS/releases/latest/download/bootstrap.sh | sudo sh -s -- --dry-run
```

**Offline install.** From the [releases page](https://github.com/paricafe/PariNS/releases),
download `parins-VERSION-linux-ARCH.tar.gz` and its `.sha256` file, then:

```sh
sha256sum --check parins-VERSION-linux-ARCH.tar.gz.sha256
tar -xzf parins-VERSION-linux-ARCH.tar.gz
cd parins-VERSION-linux-ARCH
sha256sum --check SHA256SUMS
sudo sh install.sh
```

**What the installer changes.** It installs:

| Path | Content |
| --- | --- |
| `/opt/parins-managed/parins` | The program. Backups of replaced files go to `/opt/parins-managed/.install-backup.*`. |
| `/usr/libexec/parins-updater` | The root update helper. |
| `/etc/systemd/system/` | `parins-managed.service`, `parins-updater.service`, `parins-updater.path`, `parins-update-recovery.service` |
| `/var/lib/parins-managed` | Private state, created by systemd for the service's dynamic user (stored under `/var/lib/private/parins-managed`). |
| `/var/lib/parins-updater` | The updater's journal and public status. |

It then enables and starts `parins-managed.service` and `parins-updater.path`.
It never changes host or router DNS, firewall rules or NAT, never stops
systemd-resolved or another DNS service, and never touches a file-mode
`parins.service`.

**Fresh install output.** After the service passes its readiness check, the
installer prints three steps:

1. An SSH tunnel command filled in with your login and the host's detected address,
   for example `ssh -N -L 3000:127.0.0.1:3000 admin@192.0.2.10`. Behind NAT, use
   the server's public address instead.
2. The local URL <http://127.0.0.1:3000>.
3. The one-time setup token itself. If the installer cannot read it, it prints
   `sudo cat /var/lib/parins-managed/setup-token` instead.

Never put the token in a URL or share it.

**Reinstall and upgrade.** Running the install command again installs the latest
release over the current one. An initialized instance keeps its configuration,
administrator account and management address; the installer says so and prints no
setup steps. Before replacing anything it runs the new program, as the service
user and read-only, against the saved configuration and its rollback copy, so the
service must be running. If installation or startup fails, the previous program
and units are restored. Routine upgrades can also use
[online updates](operations.md#online-updates-and-recovery).

**Older layouts are refused.** The installer accepts only the current managed unit.
It stops before writing anything if it finds an older or modified
`parins-managed.service`, a program without that unit, or old state in
`/var/lib/parins`. It does not migrate data: back up and remove the old service and
its data by hand, then install again.

## First-run setup

Until setup is done, the service serves only the console on `0.0.0.0:3000` over
plain HTTP; DNS is not running. HTTP carries the token, passwords and configuration
unencrypted, so use the SSH tunnel, or allow TCP 3000 in the host firewall and cloud
security group only from your own address.

The **First-run setup** wizard has three steps:

1. **Admin account:** the setup token, an administrator name (1–64 ASCII letters,
   digits, `-` or `_`) and a password of 12–256 bytes.
2. **Listener & upstream:**
   - **DNS listener:** **Local testing only** listens on `127.0.0.1:5353`;
     **LAN & internet (:53)** listens on `0.0.0.0:53`. The service unit grants only
     the capability to bind ports below 1024 (`CAP_NET_BIND_SERVICE`).
   - **Upstream DNS:** a preset (Cloudflare, Google Public DNS, Quad9 or AliDNS, each
     over DoH with its bootstrap addresses filled in) or **Custom**, where you list
     endpoints, scheduling, bootstrap DNS and HTTP/3 preference. See
     [Upstreams](configuration.md#upstreams).
3. **Review:** a summary of the choices. **Full configuration** shows the complete
   TOML for edits beyond the wizard; usually nothing needs to change.

Setup binds every listener before it saves anything. If a listener address is
taken, setup fails with `CONFIG_CONFLICT`, nothing is saved, and the console explains
the conflict; see [Port 53 conflicts](#port-53-conflicts).

Setup tries count against the sign-in attempt budget of 5 attempts per minute per
client address. Each submit uses two attempts, because the console first validates
the configuration (`/api/setup/preview`) and then sends the setup (`/api/setup`).

## Connect clients

With **Local testing only**, test on the server:

```sh
dig @127.0.0.1 -p 5353 example.com A
dig @127.0.0.1 -p 5353 example.com A +tcp
```

To serve other devices, listen on port 53 (most devices and routers cannot use
another port), allow UDP and TCP 53 in the host firewall only from your own
clients, and set the server's address as DNS on the devices or the router's DHCP
settings. A DNS server open to everyone is abused for amplification attacks. To
change the listener after setup, edit **Listen address** under
**Upstreams & listeners**.

### Port 53 conflicts

PariNS never stops other services or changes system DNS, so port 53 must be free
for the address you choose. Find the current owner:

```sh
sudo ss -lntup 'sport = :53'
```

On many distributions this is the systemd-resolved stub, which listens on
`127.0.0.53` and `127.0.0.54`. Two common choices, both your decision as the host
administrator:

- Listen on a specific address instead of all addresses, for example
  `listen = "192.168.1.10:53"` (your LAN address). It does not collide with the
  stub's loopback addresses.
- Turn off the stub: set `DNSStubListener=no` in `/etc/systemd/resolved.conf`, run
  `sudo systemctl restart systemd-resolved`, and make sure the host's own
  `/etc/resolv.conf` still points to a working resolver.

Then submit setup again, or apply the configuration again in the console.

## Console HTTPS

The console has no certificate of its own. It switches to HTTPS on the same port
when inbound DoH is enabled with a valid certificate and `[web].public_host` names
the host the browser uses, for example:

```toml
[doh]
listen = "0.0.0.0:443"
cert_file = "/path/to/fullchain.pem"
key_file = "/path/to/privkey.pem"

[web]
public_host = "dns.example.com"
```

The console URL then becomes `https://dns.example.com:3000/`. The certificate must
cover that name. DoH on 443 and the console on 3000 keep separate ports and routes.
DoT, DoQ and encrypted upstreams do not change the console protocol.

Saving such a configuration switches the management port to HTTPS and requires a
new sign-in. Turning off inbound DoH returns to HTTP only after you confirm the
downgrade. A rejected candidate or a certificate fault never falls back to HTTP.
Certificates can be pasted in the console or referenced as files; see
[Certificates](operations.md#certificates).

## File mode

File mode runs from a TOML file, without the console, online updates or accounts.
Build the program as in [Build from source](#build-from-source) (the web console is
embedded at build time even though file mode does not serve it), then:

```sh
cp parins.example.toml parins.toml
# Set upstreams.servers to your resolvers.
./target/release/parins --config parins.toml --check
./target/release/parins --config parins.toml
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--config PATH` | `parins.toml` | Configuration file. Relative paths inside it resolve against its directory. |
| `--data-dir DIR` | `parins-data` | Private runtime data: database, cache snapshot, subscription store. |
| `--check` | | Validate the configuration and the files it references, then exit. Creates nothing. |

The example configuration listens on `127.0.0.1:5353` and forwards to a test
upstream on `127.0.0.1:5354`; replace that before real use. SIGHUP reloads
`filter_file` and listener certificates; other changes need a restart. Ctrl-C or
SIGTERM stops accepting traffic and lets active queries finish within
`shutdown_grace_ms`.

`deploy/parins.service` is a systemd template for file mode. It expects the program
at `/opt/parins/parins` and the configuration at `/etc/parins/parins.toml`, keeps
data in `/var/lib/parins-file-data`, and does not grant `CAP_NET_BIND_SERVICE`, so it
suits ports above 1023 unless you add that capability. Review it before use:

```sh
sudo install -D -m 0755 target/release/parins /opt/parins/parins
sudo install -D -m 0644 parins.toml /etc/parins/parins.toml
sudo install -m 0644 deploy/parins.service /etc/systemd/system/parins.service
sudo systemctl daemon-reload
sudo systemctl enable --now parins.service
```

## Build from source

Install Rust with [rustup](https://rust-lang.org/tools/install/) and Node.js, then:

```sh
git clone https://github.com/paricafe/PariNS.git
cd PariNS
cd web && npm ci --ignore-scripts && npm run build && cd ..
cargo build --locked --release
```

The first build fetches the pinned Hickory fork with Git. To install a source build
as a managed service on Linux, build a package and install it like a release:

```sh
sh scripts/package.sh
```

The archive and its `.sha256` file land in `target/packages/`. Extract it, run
`sha256sum --check SHA256SUMS` inside, then `sudo sh install.sh`. Online updates are
offered only to official release installations.

## Uninstall

There is no uninstall script. These steps remove the managed installation. Back up
`/var/lib/parins-managed` first if you may need the configuration, account or
history again (see [Backups](operations.md#backups-and-the-state-directory)).

```sh
sudo systemctl disable --now parins-updater.path parins-managed.service
sudo systemctl stop parins-update-recovery.service
sudo rm -f /etc/systemd/system/parins-managed.service \
  /etc/systemd/system/parins-updater.service \
  /etc/systemd/system/parins-updater.path \
  /etc/systemd/system/parins-update-recovery.service
sudo systemctl daemon-reload
sudo rm -rf /opt/parins-managed /usr/libexec/parins-updater /var/lib/parins-updater
sudo rm -rf /var/lib/parins-managed /var/lib/private/parins-managed
```

If you added a drop-in under `/etc/systemd/system/parins-managed.service.d/`, remove
it too. Undo any firewall, router or system DNS changes you made for PariNS
yourself; the installer made none.
