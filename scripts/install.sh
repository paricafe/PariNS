#!/bin/sh
# Install only the managed deployment. Never touches DNS, firewall or state files.
set -eu
umask 077
fail() { printf 'PariNS installer: %s\n' "$*" >&2; exit 1; }
usage() {
    printf '%s\n' 'Usage: sudo sh install.sh [--binary PATH] [--enable-updater] [--dry-run]' \
        '       sh install.sh --root EXISTING_PRIVATE_DIRECTORY [--binary PATH] [--dry-run]' \
        'The default binary is parins beside install.sh (or ../target/release/parins in source).' \
        '--root is staging only: no systemctl, privileged changes or binary execution.' \
        '--enable-updater explicitly enrolls only the exact official v0.1.4 managed layout; no data migration.'
}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
binary="$script_dir/parins"
helper="$script_dir/parins-updater"
build_info="$script_dir/install-build-info.json"
unit_source="$script_dir/deploy/parins-managed.service"
if [ ! -f "$binary" ]; then binary="$script_dir/../target/release/parins"; fi
if [ ! -f "$helper" ]; then helper="$script_dir/../target/release/parins-updater"; fi
if [ ! -f "$unit_source" ]; then unit_source="$script_dir/../deploy/parins-managed.service"; fi
root= dry_run=false enable_updater=false adopting_v2=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --binary|--root|--helper|--build-info)
            [ "$#" -ge 2 ] || fail "missing value for $1"
            case "$1" in --binary) binary=$2 ;; --helper) helper=$2 ;; --build-info) build_info=$2 ;; --root) root=$2; [ -n "$root" ] || fail 'empty staging root' ;; esac
            shift 2 ;;
        --dry-run) dry_run=true; shift ;;
        --enable-updater) enable_updater=true; shift ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; fail "unknown option: $1" ;;
    esac
done
for command in install mktemp mv cp od stat find awk readlink; do
    command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
done
owner() { stat -c %u "$1" 2>/dev/null || stat -f %u "$1"; }
links() { stat -c %h "$1" 2>/dev/null || stat -f %l "$1"; }
digest() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }
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
[ -f "$helper" ] && [ ! -L "$helper" ] || fail 'helper must be a regular non-symlink file'
[ -f "$build_info" ] && [ ! -L "$build_info" ] || fail 'verified install-build-info.json is required'
[ -f "$unit_source" ] && [ ! -L "$unit_source" ] || fail 'managed service template missing or symlinked'
if [ -z "$root" ]; then
    # Inspect, never execute, a candidate as root before installation. ELF64 LE only.
    for executable in "$binary" "$helper"; do
    elf=$(od -An -tx1 -N6 "$executable" | tr -d ' \n')
    [ "$elf" = 7f454c460201 ] || fail 'binary must be a Linux ELF64 little-endian executable'
    machine=$(od -An -tx1 -j18 -N2 "$executable" | tr -d ' \n')
    case "$(uname -m):$machine" in x86_64:3e00|aarch64:b700|arm64:b700) ;; *) fail 'binary architecture does not match this host' ;; esac
    done
fi
bin_dir="$root/opt/parins-managed"
unit_dir="$root/etc/systemd/system"
binary_target="$bin_dir/parins"
unit_target="$unit_dir/parins-managed.service"
helper_target="$root/usr/libexec/parins-updater"
updater_dir="$root/var/lib/parins-updater"
updater_private="$updater_dir/private"
deploy_source=$(dirname "$unit_source")
for name in parins-updater.service parins-updater.path parins-update-recovery.service; do
    [ -f "$deploy_source/$name" ] && [ ! -L "$deploy_source/$name" ] || fail "missing updater unit: $name"
done
for path in "$root" "$root/opt" "$bin_dir" "$root/etc" "$root/etc/systemd" "$unit_dir"; do
    [ -n "$path" ] || continue
    check_path "$path"
    [ ! -e "$path" ] || [ -d "$path" ] || fail "not a directory: $path"
done
for path in "$binary_target" "$unit_target" "$helper_target" "$unit_dir/parins-updater.service" "$unit_dir/parins-updater.path" "$unit_dir/parins-update-recovery.service" "$updater_private/journal.json" "$updater_private/install-build-info.json" "$updater_private/lock"; do
    check_path "$path"
    [ ! -e "$path" ] || [ -f "$path" ] || fail "not a regular file: $path"
    [ ! -e "$path" ] || [ "$(links "$path")" = 1 ] || fail "hard-linked installation file: $path"
done
marker='# PariNS managed installer unit v3 (restricted updater contract)'
IFS= read -r line < "$unit_source" || fail 'empty service template'
[ "$line" = "$marker" ] || fail 'unrecognized managed service template'
if [ -f "$unit_target" ]; then
    IFS= read -r line < "$unit_target" || fail 'empty existing service unit'
    if [ "$line" != "$marker" ]; then
        "$enable_updater" || fail 'existing managed unit requires explicit --enable-updater enrollment; unknown layouts are not adopted'
        # Exact bytes from official tag v0.1.4, commit d33ee4612f10719e036d885edf2851e68bdd7ca9.
        [ "$(digest "$unit_target")" = 7ee1b07a86ef834ee1abae5d9383f7b94e8a46945b5f49e38ea3c78d9d00293d ] || fail '--enable-updater accepts only the exact official v0.1.4 managed unit'
        adopting_v2=true
    else
        cmp -s "$unit_target" "$unit_source" || fail 'existing v3 base unit differs from the supported installation contract'
    fi
elif [ -e "$binary_target" ]; then
    fail 'existing binary without a managed service marker; migrate explicitly'
fi
before_binary= before_unit=
[ ! -f "$binary_target" ] || before_binary=$(digest "$binary_target")
[ ! -f "$unit_target" ] || before_unit=$(digest "$unit_target")
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
            else if (key == "RuntimeDirectory") expected="parins-managed"
            else if (key == "RuntimeDirectoryMode") expected="0700"
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
for path in "$root/usr" "$root/usr/libexec" "$updater_dir" "$updater_private"; do
    check_path "$path"
    [ ! -e "$path" ] || [ -d "$path" ] || fail "not a directory: $path"
done
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
for path in "$root/opt" "$bin_dir" "$root/etc" "$root/etc/systemd" "$unit_dir" "$root/usr" "$root/usr/libexec" "$root/var" "$root/var/lib" "$updater_dir"; do
    if [ ! -d "$path" ]; then install -d -m 0755 "$path"; fi
done
[ -d "$updater_private" ] || install -d -m 0700 "$updater_private"
if [ -z "$root" ]; then
    for command in flock nsenter setpriv timeout prlimit; do command -v "$command" >/dev/null 2>&1 || fail "missing installer dependency: $command"; done
    # The helper duplicates this held descriptor during register. Closing it
    # before registration would open an installer/updater race.
    exec 9>"$updater_private/lock"
    chmod 0600 "$updater_private/lock"
    flock -n 9 || fail 'another installer or update is active'
    [ -z "$before_binary" ] || [ "$(digest "$binary_target")" = "$before_binary" ] || fail 'installed program changed before installer acquired lock'
    [ -z "$before_unit" ] || [ "$(digest "$unit_target")" = "$before_unit" ] || fail 'unit changed before installer acquired lock'
    "$helper" install-check <&9 9<&-
fi
backup=$(mktemp -d "$bin_dir/.install-backup.XXXXXX")
had_binary=false had_unit=false was_active=false was_enabled=false changed=false committed=false stopped=false new_started=false lock_held=true installation_nonce=
if [ -f "$binary_target" ]; then cp -p "$binary_target" "$backup/parins"; had_binary=true; fi
if [ -f "$unit_target" ]; then cp -p "$unit_target" "$backup/parins-managed.service"; had_unit=true; fi
for item in parins-updater.service parins-updater.path parins-update-recovery.service; do
    [ ! -f "$unit_dir/$item" ] || cp -p "$unit_dir/$item" "$backup/$item"
done
[ ! -f "$helper_target" ] || cp -p "$helper_target" "$backup/parins-updater"
[ ! -f "$updater_private/install-build-info.json" ] || cp -p "$updater_private/install-build-info.json" "$backup/install-build-info.json"
[ ! -f "$updater_private/journal.json" ] || cp -p "$updater_private/journal.json" "$backup/journal.json"
if [ -z "$root" ]; then
    if systemctl is-active --quiet parins-managed.service; then was_active=true; fi
    if systemctl is-enabled --quiet parins-managed.service; then was_enabled=true; fi
fi
rollback() {
    status=$?
    trap - EXIT HUP INT TERM
    if "$changed" && ! "$committed"; then
        if [ -z "$root" ]; then
            if ! "$lock_held"; then
                exec 9<>"$updater_private/lock"
                flock -n 9 || { printf '%s\n' 'Manual recovery required: installer lock is busy; no rollback files were changed.' >&2; exit "$status"; }
                lock_held=true
                "$helper" install-rollback-check "$installation_nonce" <&9 9<&- || {
                    printf '%s\n' 'Manual recovery required: installation nonce or identity changed; rollback refused.' >&2
                    exit "$status"
                }
            fi
            if "$had_unit" || "$new_started"; then
                systemctl stop parins-managed.service 9>&- || { printf '%s\n' 'Manual recovery required: service did not stop; rollback refused.' >&2; exit "$status"; }
                [ "$(systemctl show --property=MainPID --value parins-managed.service 9>&-)" = 0 ] || exit "$status"
            fi
        fi
        printf 'Installation failed; restoring prior binary/unit from %s\n' "$backup" >&2
        if "$had_binary"; then
            restored=$(mktemp "$bin_dir/.rollback.XXXXXX")
            cp -p "$backup/parins" "$restored" && mv -f "$restored" "$binary_target"
        else rm -f "$binary_target"; fi
        if "$had_unit"; then
            restored=$(mktemp "$unit_dir/.rollback.XXXXXX")
            cp -p "$backup/parins-managed.service" "$restored" && mv -f "$restored" "$unit_target"
        else rm -f "$unit_target"; fi
        for item in parins-updater.service parins-updater.path parins-update-recovery.service; do
            if [ -f "$backup/$item" ]; then
                restored=$(mktemp "$unit_dir/.rollback.XXXXXX")
                cp -p "$backup/$item" "$restored" && mv -f "$restored" "$unit_dir/$item"
            else rm -f "$unit_dir/$item"; fi
        done
        if [ -f "$backup/parins-updater" ]; then
            restored=$(mktemp "$root/usr/libexec/.rollback.XXXXXX")
            cp -p "$backup/parins-updater" "$restored" && mv -f "$restored" "$helper_target"
        else rm -f "$helper_target"; fi
        for item in install-build-info.json journal.json; do
            if [ -f "$backup/$item" ]; then
                restored=$(mktemp "$updater_private/.rollback.XXXXXX")
                cp -p "$backup/$item" "$restored" && mv -f "$restored" "$updater_private/$item"
            else rm -f "$updater_private/$item"; fi
        done
        if [ -z "$root" ]; then
            if [ -f "$updater_private/journal.json" ]; then
                "$helper" rebuild-status <&9 9<&- || { printf '%s\n' 'Manual recovery required: restored journal could not publish status.' >&2; exit "$status"; }
            else
                rm -f "$updater_dir/status.json"
            fi
            if ! "$was_enabled"; then systemctl disable parins-managed.service 9>&- || true; fi
            systemctl daemon-reload 9>&- || exit "$status"
            if "$was_active" && "$new_started" && ! "$adopting_v2"; then
                restore_nonce=$("$helper" install-restore <&9 9<&-) || { printf '%s\n' 'Manual recovery required: prior version was not started.' >&2; exit "$status"; }
                flock -u 9; exec 9>&-; lock_held=false
                systemctl start parins-managed.service && "$helper" install-verify "$restore_nonce" || printf '%s\n' 'Manual recovery required: prior version readiness not confirmed.' >&2
            else
                flock -u 9; exec 9>&-; lock_held=false
                if "$was_active" && ! "$new_started"; then systemctl start parins-managed.service || true; fi
                if "$adopting_v2" && "$new_started"; then printf '%s\n' 'Installation files restored; service remains stopped. v0.1.4 has no verified rollback epoch; inspect retained business data before manually starting it.' >&2; fi
            fi
        fi
    elif ! "$committed" && [ -z "$root" ] && "$stopped" && "$was_active"; then
        # A read-only preflight failure before replacement has never run the
        # candidate and cannot have introduced candidate business writes.
        flock -u 9; exec 9>&-; lock_held=false
        systemctl start parins-managed.service || true
    fi
    exit "$status"
}
trap rollback EXIT
trap 'exit 1' HUP INT TERM
if [ -z "$root" ]; then
    install -d -m 0755 "$bin_dir/.update"
    check_path "$bin_dir/.update"
    preflight="$bin_dir/.update/install-preflight"
    check_path "$preflight"
    temporary=$(mktemp "$bin_dir/.update/.install-preflight.XXXXXX")
    install -m 0755 "$binary" "$temporary"
    mv -f "$temporary" "$preflight"
    if [ -f "$state_dir/state.json" ]; then
        "$was_active" || fail 'existing initialized service must be running for an identity-preserving read-only preflight'
        original_pid=$(systemctl show --property=MainPID --value parins-managed.service 9>&-)
        original_invocation=$(systemctl show --property=InvocationID --value parins-managed.service 9>&-)
        case "$original_pid" in ''|*[!0-9]*|0) fail 'invalid running MainPID' ;; esac
        uid=$(awk '/^Uid:/ {print $3}' "/proc/$original_pid/status")
        gid=$(awk '/^Gid:/ {print $3}' "/proc/$original_pid/status")
        groups=$(awk '/^Groups:/ {for(i=2;i<=NF;i++)printf "%s%s",i==2?"":",",$i}' "/proc/$original_pid/status")
        case "$uid:$gid:$groups" in *[!0-9:,]*|0:*) fail 'invalid or privileged service identity' ;; esac
        [ -n "$uid" ] && [ -n "$gid" ] || fail 'missing service identity'
        exec 8<"/proc/$original_pid/ns/mnt"
        mount_namespace="/proc/$$/fd/8"
        low_preflight() {
            if [ -n "$groups" ]; then set -- --groups "$groups"; else set -- --clear-groups; fi
            # Two output files at 32 KiB each bound the combined output to
            # 64 KiB without losing the candidate's exit status in a pipeline.
            env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin LANG=C timeout --signal=KILL 30 \
                prlimit --fsize=32768 -- nsenter --mount="$mount_namespace" -- \
                setpriv --reuid "$uid" --regid "$gid" "$@" --bounding-set=-all --inh-caps=-all --ambient-caps=-all --no-new-privs -- \
                "$preflight" --manage --check --state-dir /var/lib/parins-managed --web-listen 0.0.0.0:3000 \
                </dev/null 8<&- 9<&- >"$updater_private/install-preflight.json" 2>"$updater_private/install-preflight-error.txt"
        }
        low_preflight || fail 'read-only candidate preflight failed; no installation files were replaced'
        [ "$(systemctl show --property=MainPID --value parins-managed.service 8<&- 9>&-)" = "$original_pid" ] && \
            [ "$(systemctl show --property=InvocationID --value parins-managed.service 8<&- 9>&-)" = "$original_invocation" ] || fail 'running service changed during preflight'
        systemctl stop parins-managed.service 8<&- 9>&-
        stopped=true
        [ "$(systemctl show --property=MainPID --value parins-managed.service 8<&- 9>&-)" = 0 ] || fail 'service has not exited'
        [ "$(systemctl show --property=Result --value parins-managed.service 8<&- 9>&-)" = success ] || fail 'service did not stop cleanly'
        low_preflight || fail 'saved configuration/materials failed final read-only validation'
        exec 8<&-
    else
        printf 'null\n' > "$updater_private/install-preflight.json"
        if "$was_active"; then systemctl stop parins-managed.service 9>&-; stopped=true; fi
    fi
fi
binary_tmp=$(mktemp "$bin_dir/.parins.XXXXXX")
unit_tmp=$(mktemp "$unit_dir/.parins-managed.XXXXXX")
install -m 0755 "$binary" "$binary_tmp"
install -m 0644 "$unit_source" "$unit_tmp"
changed=true
mv -f "$binary_tmp" "$binary_target"
mv -f "$unit_tmp" "$unit_target"
for item in parins-updater.service parins-updater.path parins-update-recovery.service; do
    temporary=$(mktemp "$unit_dir/.updater-unit.XXXXXX")
    install -m 0644 "$deploy_source/$item" "$temporary"
    mv -f "$temporary" "$unit_dir/$item"
done
temporary=$(mktemp "$root/usr/libexec/.parins-updater.XXXXXX")
install -m 0755 "$helper" "$temporary"
mv -f "$temporary" "$helper_target"
temporary=$(mktemp "$updater_private/.install-build-info.XXXXXX")
install -m 0600 "$build_info" "$temporary"
mv -f "$temporary" "$updater_private/install-build-info.json"
if [ -z "$root" ]; then
    systemctl daemon-reload 9>&-
    installation_nonce=$("$helper_target" register <&9 9<&-)
    # No lock is inherited by systemctl or the service processes it launches.
    flock -u 9
    exec 9>&-
    lock_held=false
    systemctl enable parins-managed.service
    new_started=true
    systemctl start parins-managed.service
    "$helper_target" install-verify "$installation_nonce" || fail 'installation readiness failed; check journalctl -u parins-managed.service'
    committed=true
    systemctl enable --now parins-updater.path
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
