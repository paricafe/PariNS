#!/bin/sh
# Offline fixtures: mock only the network/platform; use real archives/install.sh.
set -eu
umask 077
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
fixture=$(mktemp -d "${TMPDIR:-/tmp}/parins-bootstrap-test.XXXXXX")
fixture=$(CDPATH= cd -- "$fixture" && pwd -P)
mkdir "$fixture/tools" "$fixture/downloads" "$fixture/build" "$fixture/stage" "$fixture/refused"
export BOOTSTRAP_FIXTURE="$fixture"
export BOOTSTRAP_ARCH=x86_64 BOOTSTRAP_OS=Linux
printf '%s\n' '#!/bin/sh
case "$1" in -s) printf "%s\n" "$BOOTSTRAP_OS" ;; -m) printf "%s\n" "$BOOTSTRAP_ARCH" ;; *) exit 95 ;; esac' > "$fixture/tools/uname"
printf '%s\n' '#!/bin/sh
set -eu
[ ! -f "$BOOTSTRAP_FIXTURE/download-fails" ] || exit 22
output= url= protocol= redirect=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output) output=$2; shift 2 ;;
        --proto) protocol=$2; shift 2 ;;
        --proto-redir) redirect=$2; shift 2 ;;
        --connect-timeout|--max-time) shift 2 ;;
        --fail|--silent|--show-error|--location) shift ;;
        https://github.com/paricafe/PariNS/releases/download/v0.1.4/*) url=$1; shift ;;
        *) exit 94 ;;
    esac
done
[ "$protocol" = =https ] && [ "$redirect" = =https ] || exit 93
[ -n "$output" ] && [ -n "$url" ] || exit 92
cp "$BOOTSTRAP_FIXTURE/downloads/${url##*/}" "$output"' > "$fixture/tools/curl"
printf '%s\n' '#!/bin/sh
printf called > "$BOOTSTRAP_FIXTURE/systemctl-called"
exit 99' > "$fixture/tools/systemctl"
chmod 0755 "$fixture/tools/uname" "$fixture/tools/curl" "$fixture/tools/systemctl"
PATH="$fixture/tools:$PATH"
export PATH
digest() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
    else shasum -a 256 "$1" | awk '{print $1}'; fi
}
archive=parins-v0.1.4-linux-x86_64
asset="$archive.tar.gz"
package="$fixture/build/$archive"
mkdir "$package" "$package/deploy"
printf '%s\n' '#!/bin/sh' 'printf called > "$BOOTSTRAP_FIXTURE/binary-called"' 'exit 99' > "$package/parins"
cp "$repo/scripts/install.sh" "$package/install.sh"
cp "$repo/deploy/parins-managed.service" "$repo/deploy/parins.service" "$package/deploy/"
cp "$repo/parins.example.toml" "$repo/LICENSE" "$repo/README.md" "$package/"
cp "$repo/web/src/components/beui/LICENSE.beui" "$package/"
printf 'fixture release notes\n' > "$package/CHANGELOG.md"
manifest() {
    for name in parins install.sh parins.example.toml LICENSE README.md CHANGELOG.md deploy/parins-managed.service deploy/parins.service; do
        printf '%s  %s\n' "$(digest "$package/$name")" "$name"
    done > "$package/SHA256SUMS"
    if [ -f "$package/LICENSE.beui" ]; then
        printf '%s  LICENSE.beui\n' "$(digest "$package/LICENSE.beui")" >> "$package/SHA256SUMS"
    fi
}
checksum() { printf '%s  %s\n' "$(digest "$fixture/downloads/$asset")" "$asset" > "$fixture/downloads/$asset.sha256"; }
pack() { tar -czf "$fixture/downloads/$asset" -C "$fixture/build" "$archive"; checksum; }
expect_failure() {
    if sh "$repo/scripts/bootstrap.sh" --root "$fixture/refused" "$@" > "$fixture/failure.log" 2>&1; then
        printf 'Expected bootstrap refusal: %s\n' "$*" >&2
        exit 1
    fi
    [ ! -e "$fixture/refused/opt" ]
}
manifest
pack
# A caller can start from any directory; dry-run stages nothing.
(cd / && sh "$repo/scripts/bootstrap.sh" --root "$fixture/stage" --dry-run)
[ ! -e "$fixture/stage/opt" ]
# Piped execution is independent of $0 and the working directory.
(cd / && sh -s -- --root "$fixture/stage" < "$repo/scripts/bootstrap.sh")
cmp "$package/parins" "$fixture/stage/opt/parins-managed/parins"
mkdir -p "$fixture/stage/var/lib/parins-managed/certificates" "$fixture/stage/var/lib/parins/tls"
printf 'keep private state\n' > "$fixture/stage/var/lib/parins-managed/state.json"
printf 'keep certificate identity\n' > "$fixture/stage/var/lib/parins-managed/certificates/identity.pem"
printf 'keep external certificate\n' > "$fixture/stage/var/lib/parins/tls/external.pem"
sh "$repo/scripts/bootstrap.sh" --root "$fixture/stage" --version v0.1.4
grep -Fxq 'keep private state' "$fixture/stage/var/lib/parins-managed/state.json"
grep -Fxq 'keep certificate identity' "$fixture/stage/var/lib/parins-managed/certificates/identity.pem"
grep -Fxq 'keep external certificate' "$fixture/stage/var/lib/parins/tls/external.pem"
# Archives without a beUI notice remain installable; when present it is verified.
mv "$package/LICENSE.beui" "$fixture/notice-copy"
manifest
pack
sh "$repo/scripts/bootstrap.sh" --root "$fixture/stage" --version v0.1.4 --dry-run
printf '%064d  LICENSE.beui\n' 0 >> "$package/SHA256SUMS"
pack
expect_failure --version v0.1.4
mv "$fixture/notice-copy" "$package/LICENSE.beui"
manifest
pack
# Both architecture mappings select their exact release asset.
cp -R "$package" "$fixture/build/parins-v0.1.4-linux-aarch64"
tar -czf "$fixture/downloads/parins-v0.1.4-linux-aarch64.tar.gz" -C "$fixture/build" parins-v0.1.4-linux-aarch64
printf '%s  parins-v0.1.4-linux-aarch64.tar.gz\n' "$(digest "$fixture/downloads/parins-v0.1.4-linux-aarch64.tar.gz")" > "$fixture/downloads/parins-v0.1.4-linux-aarch64.tar.gz.sha256"
BOOTSTRAP_ARCH=aarch64 sh "$repo/scripts/bootstrap.sh" --root "$fixture/stage" --dry-run
BOOTSTRAP_ARCH=arm64 sh "$repo/scripts/bootstrap.sh" --root "$fixture/stage" --dry-run
BOOTSTRAP_ARCH=riscv64 expect_failure
BOOTSTRAP_OS=Darwin expect_failure
expect_failure --version ../../escape
expect_failure --version 'v0.1.4
v0.1.4'
expect_failure --version
expect_failure --root ''
expect_failure --root /
expect_failure --root "$fixture/missing"
mkdir -m 0755 "$fixture/public-stage"
for mode in 0755 0740 0720 0710 0704 0702 0701; do
    chmod "$mode" "$fixture/public-stage"
    BOOTSTRAP_OS=Linux BOOTSTRAP_ARCH=x86_64 expect_failure --root "$fixture/public-stage"
    grep -Fq 'staging root must be private' "$fixture/failure.log"
done
ln -s "$fixture/stage" "$fixture/linked-stage"
expect_failure --root "$fixture/linked-stage"
touch "$fixture/download-fails"
expect_failure
rm "$fixture/download-fails"
printf '%064d  %s\n' 0 "$asset" > "$fixture/downloads/$asset.sha256"
expect_failure
checksum
printf 'unexpected checksum entry\n' >> "$fixture/downloads/$asset.sha256"
expect_failure
checksum
# Correct outer checksum must not conceal corrupt internal content.
printf 'tampered\n' >> "$package/README.md"
pack
expect_failure
cp "$repo/README.md" "$package/README.md"
manifest
# Reject symlinks and hardlinks even when their names are allowlisted.
mv "$package/parins" "$fixture/original-parins"
ln -s "$fixture/original-parins" "$package/parins"
pack
expect_failure
rm "$package/parins"
mv "$fixture/original-parins" "$package/parins"
mv "$package/README.md" "$fixture/original-readme"
ln "$package/parins" "$package/README.md"
manifest
pack
expect_failure
rm "$package/README.md"
mv "$fixture/original-readme" "$package/README.md"
manifest
mv "$package/README.md" "$fixture/original-readme"
mkfifo "$package/README.md"
pack
expect_failure
rm "$package/README.md"
mv "$fixture/original-readme" "$package/README.md"
# Never extract ../ or absolute entries, even if the checksum agrees.
tar -P -czf "$fixture/downloads/$asset" -C "$package" "../$archive/install.sh"
checksum
expect_failure
tar -P -czf "$fixture/downloads/$asset" "$package/install.sh"
checksum
expect_failure
# Duplicate archive entries cannot silently replace an earlier file.
tar -czf "$fixture/downloads/$asset" -C "$fixture/build" "$archive" "$archive/install.sh"
checksum
expect_failure
pack
[ ! -e "$fixture/systemctl-called" ]
[ ! -e "$fixture/binary-called" ]
printf 'Bootstrap fixture checks passed. Retained fixtures: %s\n' "$fixture"
