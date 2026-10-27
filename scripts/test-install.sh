#!/bin/sh
# Portable, unprivileged fixture tests. No real service manager is invoked.
set -eu
umask 077
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
fixture=$(mktemp -d "${TMPDIR:-/tmp}/parins-installer-test.XXXXXX")
fixture=$(CDPATH= cd -- "$fixture" && pwd -P)
binary="$fixture/candidate"
helper="$fixture/parins-updater"
build_info="$fixture/install-build-info.json"
printf '#!/bin/sh\nexit 99\n' > "$binary"
cp "$binary" "$helper"
printf '{"official_release":false}\n' > "$build_info"
mkdir "$fixture/tools"
printf '#!/bin/sh\nprintf called > "%s"\nexit 99\n' "$fixture/systemctl-called" > "$fixture/tools/systemctl"
chmod 0755 "$fixture/tools/systemctl"
PATH="$fixture/tools:$PATH"
export PATH
expect_failure() {
    if sh "$repo/scripts/install.sh" --helper "$helper" --build-info "$build_info" "$@" > "$fixture/failure.log" 2>&1; then
        printf 'Expected refusal: %s\n' "$*" >&2
        exit 1
    fi
}
mode() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"; }
# Exercise the installer's real EXIT handler without invoking the live install
# path. A successful reinstallation has stopped the old service and already
# closed its lock descriptor; cleanup must not unlock or restart it again.
sed -n '/^rollback() {/,/^}$/p' "$repo/scripts/install.sh" > "$fixture/rollback-function.sh"
sh -eu -c '
    . "$1"
    root= changed=true committed=true stopped=true was_active=true request_path_stopped=true
    flock() { printf "unexpected lock cleanup\n" >&2; return 65; }
    systemctl() { printf "unexpected service cleanup\n" >&2; return 99; }
    exec 9>&-
    trap rollback EXIT
' sh "$fixture/rollback-function.sh"
# The namespace is pinned in the installer across both preflight calls. Inspect
# the actual parent's FD while the child runs, without nsenter or privilege.
sed -n '/^        low_preflight() /,/^        [})]$/p' "$repo/scripts/install.sh" > "$fixture/preflight-function.sh"
cat > "$fixture/tools/env" <<'MOCK_ENV'
#!/bin/sh
set -eu
namespace=
for argument do case "$argument" in --mount=*) namespace=${argument#--mount=} ;; esac; done
[ -n "$namespace" ]
if [ -d /proc/self/fd ]; then
    [ -e "$namespace" ] || { printf 'parent namespace FD missing\n' >&2; exit 71; }
else
    parent=${namespace#/proc/}; parent=${parent%/fd/8}
    lsof -a -p "$parent" -d 8 -Ff 2>/dev/null | grep -Fxq f8 || {
        printf 'parent namespace FD missing\n' >&2; exit 71;
    }
fi
for descriptor in 8 9; do
    if (eval ": <&$descriptor") 2>/dev/null; then
        printf 'preflight inherited private FD %s\n' "$descriptor" >&2
        exit 72
    fi
done
printf '{}\n'
MOCK_ENV
chmod 0755 "$fixture/tools/env"
preflight_shell=sh
if command -v dash >/dev/null 2>&1; then preflight_shell=dash; fi
"$preflight_shell" -eu -c '
    . "$1/preflight-function.sh"
    updater_private=$1 groups= uid=1234 gid=1234 preflight=/unused-candidate
    printf namespace > "$1/namespace-pin"
    exec 8<"$1/namespace-pin" 9>"$1/installer-lock"
    mount_namespace="/proc/$$/fd/8"
    low_preflight || { cat "$1/install-preflight-error.txt" >&2; exit 1; }
    rm "$1/namespace-pin"
    low_preflight || { cat "$1/install-preflight-error.txt" >&2; exit 1; }
    : <&8
    : >&9
    exec 8<&- 9>&-
' sh "$fixture"
rm "$fixture/tools/env"
# First-setup guidance prints the exact token, a filled-in tunnel and the local
# URL, and never a symlinked, hard-linked or malformed token file.
sed -n '/^fresh_guidance() {/,/^}$/p' "$repo/scripts/install.sh" > "$fixture/guidance-function.sh"
mkdir "$fixture/guidance-tools" "$fixture/guidance-state"
printf '#!/bin/sh\nprintf "192.0.2.1 via 198.51.100.1 dev eth0 src 198.51.100.7 uid 0\\n"\n' > "$fixture/guidance-tools/ip"
printf '#!/bin/sh\nshift\nexec "$@"\n' > "$fixture/guidance-tools/timeout"
chmod 0755 "$fixture/guidance-tools/ip" "$fixture/guidance-tools/timeout"
guidance() {
    PATH="$fixture/guidance-tools:$PATH" SUDO_USER=$1 sh -eu -c '
        links() { stat -c %h "$1" 2>/dev/null || stat -f %l "$1"; }
        . "$1/guidance-function.sh"
        state_dir=$1/guidance-state
        fresh_guidance
    ' sh "$fixture" > "$fixture/guidance.log"
}
token=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
printf '%s' "$token" > "$fixture/guidance-state/setup-token"
guidance alice
grep -Fxq '       ssh -N -L 3000:127.0.0.1:3000 alice@198.51.100.7' "$fixture/guidance.log"
grep -Fxq '  2. Open http://127.0.0.1:3000 in your browser.' "$fixture/guidance.log"
grep -Fxq "       $token" "$fixture/guidance.log"
! grep -Fq 'sudo cat' "$fixture/guidance.log" || exit 1
! grep -q "http.*$token" "$fixture/guidance.log" || exit 1
guidance ''
grep -Fq ' root@198.51.100.7' "$fixture/guidance.log"
for bad in short not-hex link hardlink; do
    rm -f "$fixture/guidance-state/setup-token" "$fixture/guidance-state/other"
    case "$bad" in
        short) printf '0123' > "$fixture/guidance-state/setup-token" ;;
        not-hex) printf '%064d' 0 | tr 0 g > "$fixture/guidance-state/setup-token" ;;
        link) printf '%s' "$token" > "$fixture/guidance-state/other"; ln -s other "$fixture/guidance-state/setup-token" ;;
        hardlink) printf '%s' "$token" > "$fixture/guidance-state/other"; ln "$fixture/guidance-state/other" "$fixture/guidance-state/setup-token" ;;
    esac
    guidance alice
    grep -Fq 'setup token from: sudo cat /var/lib/parins-managed/setup-token' "$fixture/guidance.log"
    ! grep -Eq '^ +[0-9a-g]{64}$' "$fixture/guidance.log" || exit 1
done
stage="$fixture/stage"
mkdir -m 0700 "$stage"
sh "$repo/scripts/install.sh" --helper "$helper" --build-info "$build_info" --root "$stage" --binary "$binary" --dry-run > "$fixture/dry-run.log"
grep -Fxq 'Dry run: no files or services changed.' "$fixture/dry-run.log"
! grep -Eq 'Fresh setup|setup-token|ssh -N|Existing configuration preserved|Check service:' "$fixture/dry-run.log" || exit 1
[ ! -e "$stage/opt" ]
mkdir -p "$stage/var/lib/parins/tls" "$stage/etc/systemd/system"
printf 'external private HTTPS identity\n' > "$stage/var/lib/parins/tls/identity.pem"
printf 'external public HTTPS certificate\n' > "$stage/var/lib/parins/tls/cert.pem"
printf 'legacy service unchanged\n' > "$stage/etc/systemd/system/parins.service"
chmod 0750 "$stage/etc"
sh "$repo/scripts/install.sh" --helper "$helper" --build-info "$build_info" --root "$stage" --binary "$binary" > "$fixture/staging.log"
grep -Fxq "Staged for inspection under $(CDPATH= cd -- "$stage" && pwd -P)." "$fixture/staging.log"
! grep -Eq 'Fresh setup|setup-token|ssh -N|Existing configuration preserved|Check service:' "$fixture/staging.log" || exit 1
cmp "$binary" "$stage/opt/parins-managed/parins"
cmp "$repo/deploy/parins-managed.service" "$stage/etc/systemd/system/parins-managed.service"
cmp "$helper" "$stage/usr/libexec/parins-updater"
cmp "$build_info" "$stage/var/lib/parins-updater/private/install-build-info.json"
for unit in parins-updater.service parins-updater.path parins-update-recovery.service; do
    cmp "$repo/deploy/$unit" "$stage/etc/systemd/system/$unit"
done
[ "$(mode "$stage/var/lib/parins-updater/private")" = 700 ]
[ "$(mode "$stage/var/lib/parins-updater/private/install-build-info.json")" = 600 ]
[ "$(mode "$stage/usr/libexec/parins-updater")" = 755 ]
grep -Fq -- '--web-listen 0.0.0.0:3000' "$stage/etc/systemd/system/parins-managed.service"
grep -Fqx 'ExecReload=/bin/kill -HUP $MAINPID' "$stage/etc/systemd/system/parins-managed.service"
[ "$(mode "$stage/opt/parins-managed/parins")" = 755 ]
[ "$(mode "$stage/etc/systemd/system/parins-managed.service")" = 644 ]
[ "$(mode "$stage/etc")" = 750 ]
[ ! -e "$stage/var/lib/parins-managed" ]
mkdir -p "$stage/var/lib/parins-managed/certificates" "$stage/var/lib/parins-managed/runtime"
printf 'private existing state\n' > "$stage/var/lib/parins-managed/state.json"
printf 'private imported certificate\n' > "$stage/var/lib/parins-managed/certificates/import.pem"
printf 'private running history\n' > "$stage/var/lib/parins-managed/runtime/observability.sqlite3"
mkdir "$stage/etc/systemd/system/parins-managed.service.d"
printf '[Service]\nSupplementaryGroups=certificate-readers\n' > "$stage/etc/systemd/system/parins-managed.service.d/certificates.conf"
printf 'second candidate, must never execute\n' > "$binary"
sh "$repo/scripts/install.sh" --helper "$helper" --build-info "$build_info" --root "$stage" --binary "$binary"
cmp "$binary" "$stage/opt/parins-managed/parins"
[ "$(sed -n 1p "$stage/var/lib/parins-managed/state.json")" = 'private existing state' ]
[ "$(sed -n 1p "$stage/var/lib/parins-managed/certificates/import.pem")" = 'private imported certificate' ]
[ "$(sed -n 1p "$stage/var/lib/parins-managed/runtime/observability.sqlite3")" = 'private running history' ]
[ "$(sed -n 1p "$stage/var/lib/parins/tls/identity.pem")" = 'external private HTTPS identity' ]
[ "$(sed -n 1p "$stage/var/lib/parins/tls/cert.pem")" = 'external public HTTPS certificate' ]
[ "$(mode "$stage/var/lib/parins/tls/identity.pem")" = 600 ]
[ "$(sed -n 1p "$stage/etc/systemd/system/parins.service")" = 'legacy service unchanged' ]
# Fail the second atomic rename after the binary changed; both old files return.
cp "$stage/opt/parins-managed/parins" "$fixture/previous-binary"
printf 'third candidate\n' > "$binary"
real_mv=$(command -v mv)
printf '#!/bin/sh\ncase "$2" in */.parins-managed.*) exit 73 ;; esac\nexec "%s" "$@"\n' "$real_mv" > "$fixture/tools/mv"
chmod 0755 "$fixture/tools/mv"
expect_failure --root "$stage" --binary "$binary"
cmp "$fixture/previous-binary" "$stage/opt/parins-managed/parins"
cmp "$repo/deploy/parins-managed.service" "$stage/etc/systemd/system/parins-managed.service"
rm "$fixture/tools/mv"
# DynamicUser logical links are accepted only with the known private backing.
mkdir "$stage/var/lib/private"
mv "$stage/var/lib/parins-managed" "$stage/var/lib/private/parins-managed"
ln -s private/parins-managed "$stage/var/lib/parins-managed"
sh "$repo/scripts/install.sh" --helper "$helper" --build-info "$build_info" --root "$stage" --binary "$binary"
[ "$(sed -n 1p "$stage/var/lib/parins-managed/state.json")" = 'private existing state' ]
# Leftovers of earlier runs: the preflight copy and backups beyond the newest
# two. Staging and dry runs only print them; look-alike names, links and online
# update copies are never listed.
managed="$(CDPATH= cd -- "$stage/opt/parins-managed" && pwd -P)"
rm -rf "$managed"/.install-backup.*
for backup in OLD001:202601010000 OLD002:202601020000 OLD003:202601030000 OLD004:202601040000; do
    mkdir "$managed/.install-backup.${backup%%:*}"
    touch -t "${backup#*:}" "$managed/.install-backup.${backup%%:*}"
done
mkdir "$managed/.install-backup.keep-me" "$fixture/linked-backup"
ln -s "$fixture/linked-backup" "$managed/.install-backup.LINK01"
mkdir -p "$managed/.update/operation-1"
printf 'candidate\n' > "$managed/.update/install-preflight"
printf 'candidate\n' > "$managed/.update/.install-preflight.TMP001"
leftover_lines() { grep '^Would remove old installer data: ' "$1" | sed 's/^Would remove old installer data: //' | sort; }
printf '%s\n' "$managed/.install-backup.OLD001" "$managed/.install-backup.OLD002" "$managed/.install-backup.OLD003" \
    "$managed/.update/.install-preflight.TMP001" "$managed/.update/install-preflight" | sort > "$fixture/expected-leftovers"
sh "$repo/scripts/install.sh" --helper "$helper" --build-info "$build_info" --root "$stage" --binary "$binary" --dry-run > "$fixture/leftovers-dry-run.log"
leftover_lines "$fixture/leftovers-dry-run.log" | cmp - "$fixture/expected-leftovers"
sh "$repo/scripts/install.sh" --helper "$helper" --build-info "$build_info" --root "$stage" --binary "$binary" > "$fixture/leftovers-staging.log"
leftover_lines "$fixture/leftovers-staging.log" | cmp - "$fixture/expected-leftovers"
for kept in .install-backup.OLD001 .install-backup.OLD004 .install-backup.keep-me .install-backup.LINK01 .update/operation-1 .update/install-preflight; do
    [ -e "$managed/$kept" ]
done
[ "$(find "$managed" -maxdepth 1 -name '.install-backup.??????' -type d | wc -l)" -eq 5 ]
# Every rejected preflight must precede replacement, backups and service calls.
cp "$stage/opt/parins-managed/parins" "$fixture/preflight-binary"
cp "$stage/etc/systemd/system/parins-managed.service" "$fixture/preflight-unit"
backups=$(find "$stage/opt/parins-managed" -name '.install-backup.*' | wc -l)
for directive in 'User=someone' 'Group=someone' 'DynamicUser=no' 'StateDirectory=elsewhere' 'WorkingDirectory=/tmp' 'ExecStart=/bin/true' 'ExecReload=/bin/kill -HUP $MAINPID' 'Environment=HOME=/tmp' 'BindPaths=/tmp' 'SupplementaryGroups='; do
    printf '[Service]\n%s\n' "$directive" > "$stage/etc/systemd/system/parins-managed.service.d/override.conf"
    expect_failure --root "$stage" --binary "$binary"
    cmp "$fixture/preflight-binary" "$stage/opt/parins-managed/parins"
    cmp "$fixture/preflight-unit" "$stage/etc/systemd/system/parins-managed.service"
done
rm "$stage/etc/systemd/system/parins-managed.service.d/override.conf"
[ "$(find "$stage/opt/parins-managed" -name '.install-backup.*' | wc -l)" = "$backups" ]
# Changing the base unit itself also invalidates its ownership contract.
sed '/DynamicUser=yes/a\
User=custom
' "$fixture/preflight-unit" > "$stage/etc/systemd/system/parins-managed.service"
expect_failure --root "$stage" --binary "$binary"
cp "$fixture/preflight-unit" "$stage/etc/systemd/system/parins-managed.service"
for reload in '' 'ExecReload=/bin/true' 'ExecReload=-/bin/kill -HUP $MAINPID' 'ExecReload=/bin/kill -HUP $MAINPID; /bin/true'; do
    awk -v replacement="$reload" '/^ExecReload=/ { if (replacement != "") print replacement; next } { print }' "$fixture/preflight-unit" > "$stage/etc/systemd/system/parins-managed.service"
    expect_failure --root "$stage" --binary "$binary"
    cmp "$fixture/preflight-binary" "$stage/opt/parins-managed/parins"
done
awk '/^ExecReload=/ { print } { print }' "$fixture/preflight-unit" > "$stage/etc/systemd/system/parins-managed.service"
expect_failure --root "$stage" --binary "$binary"
cp "$fixture/preflight-unit" "$stage/etc/systemd/system/parins-managed.service"
printf 'parins-managed:x:1234:1234::/var/lib/parins-managed:/bin/false\n' > "$stage/etc/passwd"
expect_failure --root "$stage" --binary "$binary"
rm "$stage/etc/passwd"
printf 'parins-managed:x:1234:\n' > "$stage/etc/group"
expect_failure --root "$stage" --binary "$binary"
rm "$stage/etc/group"
for hierarchy in service.d parins-.service.d; do
    mkdir "$stage/etc/systemd/system/$hierarchy"
    printf '[Service]\nStateDirectory=outside\n' > "$stage/etc/systemd/system/$hierarchy/override.conf"
    expect_failure --root "$stage" --binary "$binary"
    rm "$stage/etc/systemd/system/$hierarchy/override.conf"
    rmdir "$stage/etc/systemd/system/$hierarchy"
done
printf 'unowned service\n' > "$stage/etc/systemd/system/parins-managed.service"
expect_failure --root "$stage" --binary "$binary"
[ "$(sed -n 1p "$stage/etc/systemd/system/parins-managed.service")" = 'unowned service' ]
mkdir -m 0700 "$fixture/symlink-stage" "$fixture/outside"
ln -s "$fixture/outside" "$fixture/symlink-stage/opt"
expect_failure --root "$fixture/symlink-stage" --binary "$binary"
[ ! -e "$fixture/outside/parins-managed" ]
mkdir -m 0700 "$fixture/writable-stage"
mkdir -m 0777 "$fixture/writable-stage/opt"
expect_failure --root "$fixture/writable-stage" --binary "$binary"
mkdir -m 0700 "$fixture/binary-stage"
mkdir -p "$fixture/binary-stage/opt/parins-managed"
ln -s "$binary" "$fixture/binary-stage/opt/parins-managed/parins"
expect_failure --root "$fixture/binary-stage" --binary "$binary"
ln -s "$binary" "$fixture/source-link"
expect_failure --root "$fixture/binary-stage" --binary "$fixture/source-link"
expect_failure --root '' --binary "$binary"
expect_failure --root relative --binary "$binary"
expect_failure --root / --binary "$binary"
expect_failure --root /. --binary "$binary"
expect_failure --root "$fixture/missing" --binary "$binary"
for scenario in unknown-directory dangling-link orphaned-private legacy-state legacy-token legacy-private legacy-unit; do
    target="$fixture/$scenario"
    mkdir -p "$target/var/lib" "$target/etc/systemd/system"
    case "$scenario" in
        unknown-directory) mkdir "$target/var/lib/parins-managed" ;;
        dangling-link) ln -s private/parins-managed "$target/var/lib/parins-managed" ;;
        orphaned-private) mkdir -p "$target/var/lib/private/parins-managed" ;;
        legacy-state|legacy-token)
            mkdir "$target/var/lib/parins"
            case "$scenario" in legacy-state) name=state.json ;; *) name=setup-token ;; esac
            printf 'legacy sentinel\n' > "$target/var/lib/parins/$name" ;;
        legacy-private)
            mkdir -p "$target/var/lib/private/parins"
            printf 'legacy sentinel\n' > "$target/var/lib/private/parins/state.json" ;;
        legacy-unit)
            printf '# PariNS managed installer unit v1\n[Service]\nStateDirectory=parins\n' > "$target/etc/systemd/system/parins-managed.service" ;;
    esac
    expect_failure --root "$target" --binary "$binary"
    expect_failure --root "$target" --binary "$binary" --dry-run
    [ ! -e "$target/opt" ]
done
# Older managed layouts are rejected before any write, with a manual-handling
# hint; the removed enrollment option is not accepted either.
old_layout="$fixture/old-layout"
mkdir -p "$old_layout/etc/systemd/system" "$old_layout/opt/parins-managed" "$old_layout/var/lib/parins-managed"
printf '# PariNS managed installer unit v2\n[Service]\nStateDirectory=parins-managed\n' > "$old_layout/etc/systemd/system/parins-managed.service"
cp "$old_layout/etc/systemd/system/parins-managed.service" "$fixture/old-unit"
cp "$binary" "$old_layout/opt/parins-managed/parins"
printf 'existing state stays byte-identical\n' > "$old_layout/var/lib/parins-managed/state.json"
expect_failure --root "$old_layout" --binary "$binary"
grep -Fq 'is not a current PariNS managed unit; older layouts are not adopted. Back up and remove the old service and its data by hand' "$fixture/failure.log"
expect_failure --root "$old_layout" --binary "$binary" --enable-updater
grep -Fq 'unknown option: --enable-updater' "$fixture/failure.log"
cmp "$fixture/old-unit" "$old_layout/etc/systemd/system/parins-managed.service"
grep -Fxq 'existing state stays byte-identical' "$old_layout/var/lib/parins-managed/state.json"
[ ! -e "$old_layout/usr" ]
# Even an owned unit cannot legitimize an unexpected logical or backing link.
cp "$fixture/preflight-unit" "$stage/etc/systemd/system/parins-managed.service"
rm "$stage/var/lib/parins-managed"
ln -s "$fixture/outside" "$stage/var/lib/parins-managed"
expect_failure --root "$stage" --binary "$binary"
rm "$stage/var/lib/parins-managed"
# The same private backing without its logical link is not silently adopted.
expect_failure --root "$stage" --binary "$binary"
[ ! -e "$fixture/systemctl-called" ]
printf 'Installer fixture checks passed. Retained fixtures: %s\n' "$fixture"
