#!/bin/sh
# Portable, unprivileged fixture tests. No real service manager is invoked.
set -eu
umask 077
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
fixture=$(mktemp -d "${TMPDIR:-/tmp}/parins-installer-test.XXXXXX")
fixture=$(CDPATH= cd -- "$fixture" && pwd -P)
binary="$fixture/candidate"
printf '#!/bin/sh\nexit 99\n' > "$binary"
mkdir "$fixture/tools"
printf '#!/bin/sh\nprintf called > "%s"\nexit 99\n' "$fixture/systemctl-called" > "$fixture/tools/systemctl"
chmod 0755 "$fixture/tools/systemctl"
PATH="$fixture/tools:$PATH"
export PATH
expect_failure() {
    if sh "$repo/scripts/install.sh" "$@" > "$fixture/failure.log" 2>&1; then
        printf 'Expected refusal: %s\n' "$*" >&2
        exit 1
    fi
}
mode() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"; }
stage="$fixture/stage"
mkdir -m 0700 "$stage"
sh "$repo/scripts/install.sh" --root "$stage" --binary "$binary" --dry-run
[ ! -e "$stage/opt" ]
mkdir -p "$stage/var/lib/parins" "$stage/etc/systemd/system"
printf 'private existing state\n' > "$stage/var/lib/parins/state.json"
printf 'existing private HTTPS identity\n' > "$stage/var/lib/parins/https-identity.pem"
printf 'existing public HTTPS certificate\n' > "$stage/var/lib/parins/https-cert.pem"
printf 'legacy service unchanged\n' > "$stage/etc/systemd/system/parins.service"
chmod 0750 "$stage/etc"
sh "$repo/scripts/install.sh" --root "$stage" --binary "$binary"
cmp "$binary" "$stage/opt/parins-managed/parins"
cmp "$repo/deploy/parins-managed.service" "$stage/etc/systemd/system/parins-managed.service"
grep -Fq -- '--web-listen 0.0.0.0:3000' "$stage/etc/systemd/system/parins-managed.service"
[ "$(mode "$stage/opt/parins-managed/parins")" = 755 ]
[ "$(mode "$stage/etc/systemd/system/parins-managed.service")" = 644 ]
[ "$(mode "$stage/etc")" = 750 ]
[ "$(mode "$stage/var/lib/parins/state.json")" = 600 ]
printf 'second candidate, must never execute\n' > "$binary"
sh "$repo/scripts/install.sh" --root "$stage" --binary "$binary"
cmp "$binary" "$stage/opt/parins-managed/parins"
[ "$(sed -n 1p "$stage/var/lib/parins/state.json")" = 'private existing state' ]
[ "$(sed -n 1p "$stage/var/lib/parins/https-identity.pem")" = 'existing private HTTPS identity' ]
[ "$(sed -n 1p "$stage/var/lib/parins/https-cert.pem")" = 'existing public HTTPS certificate' ]
[ "$(mode "$stage/var/lib/parins/https-identity.pem")" = 600 ]
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
[ ! -e "$fixture/systemctl-called" ]
printf 'Installer fixture checks passed. Retained fixtures: %s\n' "$fixture"
