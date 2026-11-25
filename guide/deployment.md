# Deployment

[Home](../README.md) · [Deployment](deployment.md) · [Configuration](configuration.md) · [Operations](operations.md) · [Development](development.md) · [Architecture](architecture.md)

PariNS runs in one of two modes:

- **Managed mode.** Recommended for most hosts. The installer sets up `parins-managed.service`, and you manage settings and online updates in the web console.
- **File mode.** PariNS reads a static TOML file with no console or online updates. See [File mode](#file-mode).

## Requirements

Live managed installation requires:

- Linux on `x86_64` or `aarch64` with systemd as the service manager.
- Root access through `sudo`.
- `curl`, `tar`, `sha256sum` or `shasum`, `flock`, `timeout`, `prlimit`, and `getent`. The installer stops and names any missing command.

The target host needs no Rust toolchain, Node.js, Git, or container runtime.

## Install

Run the bootstrap installer as root:

```sh
curl -fsSL https://github.com/paricafe/PariNS/releases/latest/download/bootstrap.sh | sudo sh
```

To review the script before running it, download it first:

```sh
curl -fsSL -o bootstrap.sh https://github.com/paricafe/PariNS/releases/latest/download/bootstrap.sh
less bootstrap.sh
sudo sh bootstrap.sh
```

The bootstrap script starts only after the full script finishes downloading. It fetches the archive for your CPU architecture, verifies the archive against its `.sha256` file and every file against `SHA256SUMS`, and runs the bundled `install.sh`. SHA256 detects corruption rather than proving authorship, so you still trust the GitHub release publisher.

These flags control `bootstrap.sh`:

| Option | Effect |
| --- | --- |
| `--version vX.Y.Z` | Install a specific release instead of the version bundled with the script. |
| `--dry-run` | Download, verify, and print install targets without writing files. |
| `--root DIR` | Stage files into an existing private mode-0700 directory without starting services. |

When piping to `sh`, pass options after `-s --`:

```sh
curl -fsSL https://github.com/paricafe/PariNS/releases/latest/download/bootstrap.sh | sudo sh -s -- --dry-run
```

### Offline install

Download `parins-VERSION-linux-ARCH.tar.gz` and its `.sha256` file from the [releases page](https://github.com/paricafe/PariNS/releases), then verify and install them:

```sh
sha256sum --check parins-VERSION-linux-ARCH.tar.gz.sha256
tar -xzf parins-VERSION-linux-ARCH.tar.gz
cd parins-VERSION-linux-ARCH
sha256sum --check SHA256SUMS
sudo sh install.sh
```

### Installed files

The installer writes these paths:

| Path | Content |
| --- | --- |
| `/opt/parins-managed/parins` | Main binary. Replaced files are backed up to `/opt/parins-managed/.install-backup.*`, keeping the two newest backups and removing the temporary candidate copy. |
| `/usr/libexec/parins-updater` | Restricted root update helper. |
| `/etc/systemd/system/` | `parins-managed.service`, `parins-updater.service`, `parins-updater.path`, and `parins-update-recovery.service`. |
| `/var/lib/parins-managed` | Private state directory created by systemd for its dynamic user, backed by `/var/lib/private/parins-managed`. |
| `/var/lib/parins-updater` | Updater journal and public status file. |

The installer enables and starts `parins-managed.service` and `parins-updater.path`. It does not change firewall rules, client DNS, `systemd-resolved`, other DNS services, or a file-mode `parins.service`.

### Fresh install output

Once the service passes readiness checks, the installer prints three items:

1. An SSH tunnel command filled in with your login and detected host address, such as `ssh -N -L 3000:127.0.0.1:3000 admin@192.0.2.10`. Behind NAT, replace the address with the server's public address.
2. The local console URL, `http://127.0.0.1:3000`.
3. The one-time setup token. If the installer cannot read the token file, it prints `sudo cat /var/lib/parins-managed/setup-token` instead.

Never put the setup token in a URL or share it.

### Reinstall and upgrade

Run the install command again to upgrade an existing managed installation. An initialized instance keeps its configuration, admin account, and management address, and the installer prints no setup steps.

Before replacing any file, the installer runs the new binary read-only as the service user against the saved configuration and its rollback copy. Because of this check, `parins-managed.service` must be running during a reinstall. If replacement or startup fails, the installer restores the previous binary and unit files. For routine upgrades, you can also use [online updates](operations.md#online-updates-and-recovery).

The installer accepts only the current managed unit contract. It stops before writing anything if it finds an older or modified `parins-managed.service`, an installed binary without that unit, or old state in `/var/lib/parins`. It does not migrate legacy state. Back up and remove the old service and its data manually, then run the installer again.

## First-run setup

Before setup completes, PariNS serves only the console on `0.0.0.0:3000` over plain HTTP and does not start DNS. Plain HTTP carries the setup token, password, and configuration without encryption. Use the SSH tunnel, or allow TCP port 3000 in your firewall and cloud security group only from your own address.

The **First-run setup** wizard has three steps:

1. **Admin account.** Enter the setup token, an admin username of 1 to 64 ASCII letters, digits, `-`, or `_`, and a password of 12 to 256 bytes.
2. **Listener & upstream.** Choose a DNS listener and an upstream source:
   - Choose **Local testing only** to listen on `127.0.0.1:5353`, or **LAN & internet (:53)** to listen on `0.0.0.0:53`. The managed service grants only `CAP_NET_BIND_SERVICE` so it can bind ports below 1024.
   - Pick a DoH preset with built-in bootstrap addresses, including Cloudflare, Google Public DNS, Quad9, or AliDNS, or select **Custom** to set endpoints, scheduling, bootstrap DNS, and HTTP/3 preference. See [Upstreams](configuration.md#upstreams).
3. **Review.** Inspect the summary and finish setup. Open **Full configuration** if you need to edit the full TOML before saving.

Setup binds every configured listener before writing state. If a listener address is already bound, setup fails with `CONFIG_CONFLICT`, saves nothing, and names the conflict. See [Port 53 conflicts](#port-53-conflicts).

Setup requests share the sign-in limit of 5 attempts per minute per client address. Each submission uses two attempts because the console validates through `/api/setup/preview` before posting to `/api/setup`.

## Connect clients

If you chose **Local testing only**, test both UDP and TCP on the server:

```sh
dig @127.0.0.1 -p 5353 example.com A
dig @127.0.0.1 -p 5353 example.com A +tcp
```

To serve other devices on your network:

1. Listen on port 53, because most routers and client systems cannot use a custom port.
2. Allow UDP and TCP port 53 in the host firewall only from your own clients. An open resolver can be abused for amplification attacks.
3. Configure the server's IP address as DNS on your devices or in your router's DHCP settings.

To change the listener address after setup, edit **Listen address** under **Upstreams & listeners**.

### Port 53 conflicts

PariNS never stops other services or edits host DNS settings, so port 53 must be free on the address you choose. Check which process holds port 53:

```sh
sudo ss -lntup 'sport = :53'
```

On many Linux distributions, the `systemd-resolved` stub holds `127.0.0.53` and `127.0.0.54`. You have two options:

- Bind PariNS to a specific LAN IP instead of `0.0.0.0`, such as `listen = "192.168.1.10:53"`. That address does not collide with the stub's loopback addresses.
- Disable the stub by setting `DNSStubListener=no` in `/etc/systemd/resolved.conf`, running `sudo systemctl restart systemd-resolved`, and confirming that `/etc/resolv.conf` still points to a working resolver.

After freeing the address, submit setup or apply your configuration again in the console.

## Console HTTPS

The console has no separate certificate setting. It switches to HTTPS on port 3000 when you enable inbound DoH with a valid certificate and set `[web].public_host` to the hostname the browser uses:

```toml
[doh]
listen = "0.0.0.0:443"
cert_file = "/path/to/fullchain.pem"
key_file = "/path/to/privkey.pem"

[web]
public_host = "dns.example.com"
```

With that configuration, the console URL becomes `https://dns.example.com:3000/`. The DoH certificate must cover `public_host`. DoH on port 443 and the console on port 3000 keep separate ports and routes. Enabling DoT, DoQ, or encrypted upstreams does not switch the console to HTTPS.

Applying this change moves the management listener to HTTPS and requires a fresh sign-in. Disabling inbound DoH returns the console to HTTP only after you confirm the downgrade. A rejected configuration or certificate error never downgrades HTTPS to HTTP. You can paste certificates in the console or reference files on disk. See [Certificates](operations.md#certificates).

## File mode

File mode runs directly from a TOML file without the web console, accounts, or online updates. Build the binary as described in [Build from source](#build-from-source), then configure and run it:

```sh
cp parins.example.toml parins.toml
# Edit upstreams.servers in parins.toml before real use.
./target/release/parins --config parins.toml --check
./target/release/parins --config parins.toml
```

The web console assets are still embedded at build time, though file mode never serves them.

File mode accepts these command-line flags:

| Option | Default | Meaning |
| --- | --- | --- |
| `--config PATH` | `parins.toml` | TOML configuration file. Relative paths inside it resolve against its directory. |
| `--data-dir DIR` | `parins-data` | Private runtime directory for the database, cache snapshot, and subscription store. |
| `--check` | Off | Validate the configuration and referenced files, then exit without creating state. |

`parins.example.toml` listens on `127.0.0.1:5353` and forwards to a placeholder upstream on `127.0.0.1:5354`. Replace that upstream before real use. Send `SIGHUP` to reload `filter_file` and listener certificates. Other changes require a restart. Send `Ctrl-C` or `SIGTERM` to stop accepting traffic and drain active queries within `shutdown_grace_ms`.

To run file mode under systemd, review `deploy/parins.service` first. It expects `/opt/parins/parins`, `/etc/parins/parins.toml`, and `/var/lib/parins-file-data`. It does not grant `CAP_NET_BIND_SERVICE`, so add that capability if you bind ports below 1024:

```sh
sudo install -D -m 0755 target/release/parins /opt/parins/parins
sudo install -D -m 0644 parins.toml /etc/parins/parins.toml
sudo install -m 0644 deploy/parins.service /etc/systemd/system/parins.service
sudo systemctl daemon-reload
sudo systemctl enable --now parins.service
```

## Build from source

Install Rust with [rustup](https://rust-lang.org/tools/install/) and install Node.js, then build the console and Rust binary:

```sh
git clone https://github.com/paricafe/PariNS.git
cd PariNS
cd web && npm ci --ignore-scripts && npm run build && cd ..
cargo build --locked --release
```

The first Rust build fetches the pinned Hickory fork through Git. To package a source build as a managed Linux installation:

```sh
sh scripts/package.sh
```

The archive and its `.sha256` file are written to `target/packages/`. Extract the archive, run `sha256sum --check SHA256SUMS` inside it, and run `sudo sh install.sh`. Online updates apply only to official release installations.

## Uninstall

PariNS provides no uninstall script. Back up `/var/lib/parins-managed` first if you want to keep your configuration, admin account, or history. See [Backups and the state directory](operations.md#backups-and-the-state-directory).

Run these commands to remove a managed installation:

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

Remove any systemd drop-in under `/etc/systemd/system/parins-managed.service.d/` as well. Revert any firewall, router, or host DNS changes you made manually, because the installer made none.
