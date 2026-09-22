#!/bin/sh
# Install only the managed deployment. Never touches DNS, firewall or state files.
set -eu
umask 077
fail() { printf 'PariNS installer: %s\n' "$*" >&2; exit 1; }
usage() {
    printf '%s\n' 'Usage: sudo sh install.sh [--binary PATH] [--dry-run]' \
        '       sh install.sh --root EXISTING_PRIVATE_DIRECTORY [--binary PATH] [--dry-run]' \
        'The default binary is parins beside install.sh (or ../target/release/parins in source).' \
        '--root is staging only: no systemctl, privileged changes or binary execution.'
}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
binary="$script_dir/parins"
unit_source="$script_dir/deploy/parins-managed.service"
if [ ! -f "$binary" ]; then binary="$script_dir/../target/release/parins"; fi
if [ ! -f "$unit_source" ]; then unit_source="$script_dir/../deploy/parins-managed.service"; fi
root= dry_run=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --binary|--root)
            [ "$#" -ge 2 ] || fail "missing value for $1"
            case "$1" in --binary) binary=$2 ;; --root) root=$2; [ -n "$root" ] || fail 'empty staging root' ;; esac
            shift 2 ;;
        --dry-run) dry_run=true; shift ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; fail "unknown option: $1" ;;
    esac
done
for command in install mktemp mv cp od stat find; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done
owner() { stat -c %u "$1" 2>/dev/null || stat -f %u "$1"; }
expected_owner=0
if [ -n "$root" ]; then
    case "$root" in /*) ;; *) fail 'staging root must be absolute' ;; esac
    [ "$root" != / ] || fail 'staging root must not be /'
    [ -d "$root" ] && [ ! -L "$root" ] || fail 'staging root must be an existing non-symlink directory'
    root=$(CDPATH= cd -- "$root" && pwd -P)
    [ "$root" != / ] || fail 'staging root must not resolve to /'
    expected_owner=$(id -u)
else
    [ "$(uname -s)" = Linux ] || fail 'live installation requires Linux'
    [ "$(id -u)" = 0 ] || fail 'live installation requires root (sudo)'
    command -v systemctl >/dev/null 2>&1 || fail 'systemctl is required'
    [ -d /run/systemd/system ] || fail 'systemd must be running as the service manager'
    case "$(uname -m)" in x86_64|aarch64|arm64) ;; *) fail 'supported platforms: Linux x86_64 and aarch64' ;; esac
fi
check_path() {
    [ ! -L "$1" ] || fail "refusing symlink: $1"
    if [ -e "$1" ]; then
        [ "$(owner "$1")" = "$expected_owner" ] || fail "unexpected owner: $1"
        [ -z "$(find "$1" -prune \( -perm -002 -o -perm -020 \) -print)" ] || fail "group/world writable target: $1"
    fi
}
[ -f "$binary" ] && [ ! -L "$binary" ] || fail 'binary must be a regular non-symlink file'
[ -f "$unit_source" ] && [ ! -L "$unit_source" ] || fail 'managed service template missing or symlinked'
if [ -z "$root" ]; then
    # Inspect, never execute, a candidate as root before installation. ELF64 LE only.
    elf=$(od -An -tx1 -N6 "$binary" | tr -d ' \n')
    [ "$elf" = 7f454c460201 ] || fail 'binary must be a Linux ELF64 little-endian executable'
    machine=$(od -An -tx1 -j18 -N2 "$binary" | tr -d ' \n')
    case "$(uname -m):$machine" in x86_64:3e00|aarch64:b700|arm64:b700) ;; *) fail 'binary architecture does not match this host' ;; esac
fi
bin_dir="$root/opt/parins-managed"
unit_dir="$root/etc/systemd/system"
binary_target="$bin_dir/parins"
unit_target="$unit_dir/parins-managed.service"
for path in "$root" "$root/opt" "$bin_dir" "$root/etc" "$root/etc/systemd" "$unit_dir"; do
    [ -n "$path" ] || continue
    check_path "$path"
    [ ! -e "$path" ] || [ -d "$path" ] || fail "not a directory: $path"
done
for path in "$binary_target" "$unit_target"; do
    check_path "$path"
    [ ! -e "$path" ] || [ -f "$path" ] || fail "not a regular file: $path"
done
marker='# PariNS managed installer unit v1'
IFS= read -r line < "$unit_source" || fail 'empty service template'
[ "$line" = "$marker" ] || fail 'unrecognized managed service template'
if [ -f "$unit_target" ]; then
    IFS= read -r line < "$unit_target" || fail 'empty existing service unit'
    [ "$line" = "$marker" ] || fail 'existing managed service name is not installer-owned'
elif [ -e "$binary_target" ]; then
    fail 'existing binary without a managed service marker; migrate explicitly'
fi
if [ -z "$root" ]; then
    registered=$(systemctl show --property=FragmentPath --value parins-managed.service) || fail 'cannot query systemd'
    [ -z "$registered" ] || [ "$registered" = "$unit_target" ] || fail "managed service name already belongs to $registered"
fi
printf 'Install %s -> %s\nRegister %s\n' "$binary" "$binary_target" "$unit_target"
if "$dry_run"; then
    printf '%s\n' 'Dry run: no files or services changed.'
    exit 0
fi
for path in "$root/opt" "$bin_dir" "$root/etc" "$root/etc/systemd" "$unit_dir"; do
    if [ ! -d "$path" ]; then install -d -m 0755 "$path"; fi
done
backup=$(mktemp -d "$bin_dir/.install-backup.XXXXXX")
had_binary=false had_unit=false was_active=false was_enabled=false changed=false committed=false
if [ -f "$binary_target" ]; then cp -p "$binary_target" "$backup/parins"; had_binary=true; fi
if [ -f "$unit_target" ]; then cp -p "$unit_target" "$backup/parins-managed.service"; had_unit=true; fi
if [ -z "$root" ]; then
    if systemctl is-active --quiet parins-managed.service; then was_active=true; fi
    if systemctl is-enabled --quiet parins-managed.service; then was_enabled=true; fi
fi
rollback() {
    status=$?
    trap - EXIT HUP INT TERM
    if "$changed" && ! "$committed"; then
        printf 'Installation failed; restoring prior binary/unit from %s\n' "$backup" >&2
        if "$had_binary"; then
            restored=$(mktemp "$bin_dir/.rollback.XXXXXX")
            cp -p "$backup/parins" "$restored" && mv -f "$restored" "$binary_target"
        else rm -f "$binary_target"; fi
        if "$had_unit"; then
            restored=$(mktemp "$unit_dir/.rollback.XXXXXX")
            cp -p "$backup/parins-managed.service" "$restored" && mv -f "$restored" "$unit_target"
        else rm -f "$unit_target"; fi
        if [ -z "$root" ]; then
            systemctl stop parins-managed.service || true
            if ! "$was_enabled"; then systemctl disable parins-managed.service || true; fi
            systemctl daemon-reload || true
            if "$was_active"; then systemctl start parins-managed.service || true; fi
        fi
    fi
    exit "$status"
}
trap rollback EXIT
trap 'exit 1' HUP INT TERM
binary_tmp=$(mktemp "$bin_dir/.parins.XXXXXX")
unit_tmp=$(mktemp "$unit_dir/.parins-managed.XXXXXX")
install -m 0755 "$binary" "$binary_tmp"
install -m 0644 "$unit_source" "$unit_tmp"
changed=true
mv -f "$binary_tmp" "$binary_target"
mv -f "$unit_tmp" "$unit_target"
if [ -z "$root" ]; then
    systemctl daemon-reload
    systemctl enable parins-managed.service
    systemctl restart parins-managed.service
    sleep 2
    systemctl is-active --quiet parins-managed.service || fail 'service did not stay active; check journalctl -u parins-managed.service'
fi
committed=true
printf 'Installed. Backup: %s\n' "$backup"
if [ -n "$root" ]; then
    printf '%s\n' 'Staging only: state is untouched and no service or executable was started.'
else
    printf '%s\n' 'Console: https://SERVER_PUBLIC_IP:3000 (listens on 0.0.0.0:3000; HTTPS only)' \
        'Allow TCP 3000 in the host firewall/cloud security group for your admin IP; no firewall rules are changed here.' \
        'The initial certificate is self-signed. Verify its fingerprint over SSH before trusting it in your browser.' \
        'Public certificate: /var/lib/parins/https-cert.pem (safe to export); fingerprint: sudo openssl x509 -in /var/lib/parins/https-cert.pem -noout -sha256 -fingerprint' \
        'NEVER export /var/lib/parins/https-identity.pem: it contains the private key.' \
        'Read the one-time setup token locally: sudo cat /var/lib/parins/setup-token' \
        'Open the console to initialize. DNS starts only after setup; no host DNS or firewall was changed.'
    if command -v openssl >/dev/null 2>&1 && [ -f /var/lib/parins/https-cert.pem ]; then
        openssl x509 -in /var/lib/parins/https-cert.pem -noout -sha256 -fingerprint || true
    fi
    # Local interface addresses are useful hints, not a claim about public NAT.
    if command -v ip >/dev/null 2>&1; then
        ip -4 -o address show scope global | awk '{split($4, address, "/"); printf "Local interface console: https://%s:3000\n", address[1]}'
    fi
    printf '%s\n' 'Check service: sudo systemctl status parins-managed.service' \
        'View logs: sudo journalctl -u parins-managed.service -n 50 --no-pager'
fi
