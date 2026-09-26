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
    root= changed=true committed=true stopped=true was_active=true
    flock() { printf "unexpected lock cleanup\n" >&2; return 65; }
    systemctl() { printf "unexpected service cleanup\n" >&2; return 99; }
    exec 9>&-
    trap rollback EXIT
' sh "$fixture/rollback-function.sh"
stage="$fixture/stage"
mkdir -m 0700 "$stage"
sh "$repo/scripts/install.sh" --helper "$helper" --build-info "$build_info" --root "$stage" --binary "$binary" --dry-run
[ ! -e "$stage/opt" ]
mkdir -p "$stage/var/lib/parins/tls" "$stage/etc/systemd/system"
printf 'external private HTTPS identity\n' > "$stage/var/lib/parins/tls/identity.pem"
printf 'external public HTTPS certificate\n' > "$stage/var/lib/parins/tls/cert.pem"
printf 'legacy service unchanged\n' > "$stage/etc/systemd/system/parins.service"
chmod 0750 "$stage/etc"
sh "$repo/scripts/install.sh" --helper "$helper" --build-info "$build_info" --root "$stage" --binary "$binary"
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
# First onboarding is an explicit, exact-layout operation, never a marker-only
# legacy fallback. Staging proves no candidate/systemctl runs or business writes.
enrollment="$fixture/enrollment"
mkdir -p "$enrollment/etc/systemd/system/parins-managed.service.d" "$enrollment/opt/parins-managed" "$enrollment/var/lib/parins-managed/runtime"
cp "$repo/scripts/fixtures/parins-managed-v0.1.4.service" "$enrollment/etc/systemd/system/parins-managed.service"
cp "$binary" "$enrollment/opt/parins-managed/parins"
printf 'existing state stays byte-identical\n' > "$enrollment/var/lib/parins-managed/state.json"
printf 'existing runtime stays byte-identical\n' > "$enrollment/var/lib/parins-managed/runtime/observability.sqlite3"
printf '[Service]\nSupplementaryGroups=certificate-readers\n' > "$enrollment/etc/systemd/system/parins-managed.service.d/certificates.conf"
expect_failure --root "$enrollment" --binary "$binary"
sh "$repo/scripts/install.sh" --helper "$helper" --build-info "$build_info" --root "$enrollment" --binary "$binary" --enable-updater --dry-run
cmp "$repo/scripts/fixtures/parins-managed-v0.1.4.service" "$enrollment/etc/systemd/system/parins-managed.service"
printf '# marker retained but base unit changed\n' >> "$enrollment/etc/systemd/system/parins-managed.service"
expect_failure --root "$enrollment" --binary "$binary" --enable-updater
cp "$repo/scripts/fixtures/parins-managed-v0.1.4.service" "$enrollment/etc/systemd/system/parins-managed.service"
printf '[Service]\nExecStart=/bin/true\n' > "$enrollment/etc/systemd/system/parins-managed.service.d/evil.conf"
expect_failure --root "$enrollment" --binary "$binary" --enable-updater
rm "$enrollment/etc/systemd/system/parins-managed.service.d/evil.conf"
sh "$repo/scripts/install.sh" --helper "$helper" --build-info "$build_info" --root "$enrollment" --binary "$binary" --enable-updater
cmp "$repo/deploy/parins-managed.service" "$enrollment/etc/systemd/system/parins-managed.service"
grep -Fxq 'existing state stays byte-identical' "$enrollment/var/lib/parins-managed/state.json"
grep -Fxq 'existing runtime stays byte-identical' "$enrollment/var/lib/parins-managed/runtime/observability.sqlite3"
[ ! -f "$enrollment/var/lib/parins-updater/private/journal.json" ]
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
