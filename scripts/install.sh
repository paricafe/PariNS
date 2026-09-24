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
for command in install mktemp mv cp od stat find awk readlink; do
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
marker='# PariNS managed installer unit v2 (exclusive state directory)'
IFS= read -r line < "$unit_source" || fail 'empty service template'
[ "$line" = "$marker" ] || fail 'unrecognized managed service template'
if [ -f "$unit_target" ]; then
    IFS= read -r line < "$unit_target" || fail 'empty existing service unit'
    [ "$line" = "$marker" ] || fail 'existing managed unit uses an unknown or old state-directory contract; arrange an explicit manual handoff before installing'
elif [ -e "$binary_target" ]; then
    fail 'existing binary without a managed service marker; migrate explicitly'
fi
# Ownership must be established before replacing files or asking systemd to
# prepare StateDirectory. DynamicUser may chown a directory before ExecStart.
state_dir="$root/var/lib/parins-managed"
private_dir="$root/var/lib/private/parins-managed"
for path in "$root/var" "$root/var/lib" "$root/var/lib/private"; do
    check_path "$path"
    [ ! -e "$path" ] || [ -d "$path" ] || fail "not a directory: $path"
done
for legacy in "$root/var/lib/parins" "$root/var/lib/private/parins"; do
    for name in state.json setup-token; do
        [ ! -e "$legacy/$name" ] && [ ! -L "$legacy/$name" ] ||
            fail "old managed state exists at $legacy/$name; arrange an explicit manual handoff (no automatic migration)"
    done
done
if [ ! -f "$unit_target" ]; then
    for path in "$state_dir" "$private_dir"; do
        [ ! -e "$path" ] && [ ! -L "$path" ] || fail "state directory is not installer-owned: $path"
    done
else
    # The base unit owns the identity/directory contract. Other service keys
    # remain installer-owned; a drop-in may only add read-only certificate groups.
    awk '
        /^[[:space:]]*([#;]|$)/ { next }
        /^\[/ { section=$0; next }
        section == "[Service]" {
            n=split($0, parts, "="); key=parts[1]
            if (key == "DynamicUser") expected="yes"
            else if (key == "StateDirectory") expected="parins-managed"
            else if (key == "StateDirectoryMode") expected="0700"
            else if (key == "WorkingDirectory") expected="/var/lib/parins-managed"
            else if (key == "ExecStart") expected="/opt/parins-managed/parins --manage --state-dir /var/lib/parins-managed --web-listen 0.0.0.0:3000"
            else if (key == "ExecReload") expected="/bin/kill -HUP $MAINPID"
            else if (key ~ /^(Type|Restart|RestartSec|TimeoutStopSec|CapabilityBoundingSet|AmbientCapabilities|NoNewPrivileges|ProtectSystem|ProtectHome|PrivateTmp|PrivateDevices|ProtectKernelTunables|ProtectKernelModules|ProtectControlGroups|RestrictAddressFamilies|RestrictSUIDSGID|LockPersonality|UMask)$/) next
            else { bad=1; next }
            if (++seen[key] != 1 || substr($0, length(key)+2) != expected) bad=1
        }
        END { exit (bad || seen["DynamicUser"] != 1 || seen["StateDirectory"] != 1 || seen["StateDirectoryMode"] != 1 || seen["WorkingDirectory"] != 1 || seen["ExecStart"] != 1 || seen["ExecReload"] != 1) }
    ' "$unit_target" || fail 'existing unit changes the managed identity/directory/executable contract; use an explicit manual handoff'
    if [ -L "$state_dir" ]; then
        link=$(readlink "$state_dir")
        [ "$link" = /var/lib/private/parins-managed ] || [ "$link" = private/parins-managed ] ||
            fail "unexpected managed state link: $state_dir -> $link"
        [ -d "$private_dir" ] && [ ! -L "$private_dir" ] || fail "managed state link has no regular private backing: $private_dir"
    else
        [ ! -e "$state_dir" ] || [ -d "$state_dir" ] || fail "managed state is not a directory: $state_dir"
        [ ! -e "$private_dir" ] && [ ! -L "$private_dir" ] || fail "orphaned managed private backing: $private_dir"
    fi
fi
check_dropin() {
    check_path "$1"
    [ -f "$1" ] || fail "not a regular drop-in: $1"
    awk '
        /^[[:space:]]*([#;]|$)/ { next }
        /^\[Service\]$/ { service=1; next }
        service && /^SupplementaryGroups=[A-Za-z0-9_. -]+$/ { next }
        { bad=1 }
        END { exit bad }
    ' "$1" || fail "drop-in changes the managed contract (only SupplementaryGroups is supported): $1"
}
# Include the service-wide and dash-prefix drop-in hierarchies, including when
# a fresh service is not yet loaded and systemctl cannot report DropInPaths.
for base in "$root/etc/systemd/system" "$root/run/systemd/system" "$root/usr/local/lib/systemd/system" "$root/usr/lib/systemd/system" "$root/lib/systemd/system"; do
    for suffix in service.d parins-.service.d parins-managed.service.d; do
        directory="$base/$suffix"
        [ ! -e "$directory" ] && [ ! -L "$directory" ] && continue
        check_path "$directory"
        [ -d "$directory" ] || fail "not a drop-in directory: $directory"
        for dropin in "$directory"/*.conf; do
            [ ! -e "$dropin" ] && [ ! -L "$dropin" ] && continue
            check_dropin "$dropin"
        done
    done
done
if [ -n "$root" ]; then
    for account_file in "$root/etc/passwd" "$root/etc/group"; do
        if [ -f "$account_file" ] && awk -F: '$1 == "parins-managed" { found=1 } END { exit !found }' "$account_file"; then
            fail "static parins-managed identity conflicts with DynamicUser: $account_file"
        fi
    done
else
    command -v getent >/dev/null 2>&1 || fail 'getent is required to inspect service identity'
    # Exclude nss-systemd, which legitimately reports the running dynamic user;
    # check every other configured NSS source, including directory accounts.
    for database in passwd group; do
        sources=$(awk -F: -v database="$database" '
            $1 == database { sub(/#.*/, "", $2); gsub(/\[[^]]*\]/, "", $2); print $2 }
        ' /etc/nsswitch.conf) || fail 'cannot inspect service identity sources'
        [ -n "$sources" ] || sources=files
        for source in $sources; do
            [ "$source" != systemd ] || continue
            if getent -s "$source" "$database" parins-managed >/dev/null; then
                fail "static parins-managed $database identity from $source conflicts with DynamicUser"
            else
                result=$?
                [ "$result" = 2 ] || fail "cannot inspect $source $database identities (getent: $result)"
            fi
        done
    done
    loaded_dropins=$(systemctl show --property=DropInPaths --value parins-managed.service) || fail 'cannot inspect effective drop-ins'
    for dropin in $loaded_dropins; do
        check_dropin "$dropin"
    done
    if [ -f "$unit_target" ]; then
        effective() {
            actual=$(systemctl show --property="$1" --value parins-managed.service) || fail "cannot query systemd property $1"
            [ "$actual" = "$2" ] || fail "effective $1 changes the managed contract: $actual"
        }
        effective DynamicUser yes
        effective User parins-managed
        effective Group parins-managed
        effective StateDirectory parins-managed
        effective StateDirectoryMode 0700
        effective WorkingDirectory /var/lib/parins-managed
        command_line=$(systemctl show --property=ExecStart --value parins-managed.service) || fail 'cannot inspect effective ExecStart'
        [ "$(printf '%s\n' "$command_line" | awk '{ print gsub(/\{/, "") }')" = 1 ] || fail 'effective ExecStart must contain exactly one command'
        case "$command_line" in
            '{ path=/opt/parins-managed/parins ; argv[]=/opt/parins-managed/parins --manage --state-dir /var/lib/parins-managed --web-listen 0.0.0.0:3000 ; ignore_errors=no ; '*'}') ;;
            *) fail 'effective ExecStart changes the managed executable contract' ;;
        esac
        reload_line=$(systemctl show --property=ExecReload --value parins-managed.service) || fail 'cannot inspect effective ExecReload'
        [ "$(printf '%s\n' "$reload_line" | awk '{ print gsub(/\{/, "") }')" = 1 ] || fail 'effective ExecReload must contain exactly one command'
        case "$reload_line" in
            '{ path=/bin/kill ; argv[]=/bin/kill -HUP $MAINPID ; ignore_errors=no ; '*'}') ;;
            *) fail 'effective ExecReload changes the managed reload contract' ;;
        esac
        for property in RootDirectory RootImage Environment EnvironmentFiles ExecStartPre ExecStartPost ExecStop ExecStopPost; do
            effective "$property" ''
        done
    fi
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
    printf '%s\n' 'Fresh setup console: http://SERVER_PUBLIC_IP:3000 (listens on 0.0.0.0:3000 by default)' \
        'Allow TCP 3000 in the host firewall/cloud security group for your admin IP; no firewall rules are changed here.' \
        'HTTP is unencrypted: prefer local access or an SSH tunnel for initial setup, especially when entering passwords or private keys.' \
        'If an existing or new configuration enables inbound DoH, use the HTTPS management address shown by PariNS instead.' \
        'Read the one-time setup token locally: sudo cat /var/lib/parins-managed/setup-token' \
        'Open the console to initialize. DNS starts only after setup; no host DNS or firewall was changed.'
    # Local interface addresses are useful hints, not a claim about public NAT.
    if command -v ip >/dev/null 2>&1; then
        ip -4 -o address show scope global | awk '{split($4, address, "/"); printf "Local interface setup address (before DoH): http://%s:3000\n", address[1]}'
    fi
    printf '%s\n' 'Check service: sudo systemctl status parins-managed.service' \
        'View logs: sudo journalctl -u parins-managed.service -n 50 --no-pager'
fi
