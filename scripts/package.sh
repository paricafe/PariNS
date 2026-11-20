#!/bin/sh
# Host-native development packages, or checked Linux release packages.
set -eu
export COPYFILE_DISABLE=1 LC_ALL=C
cd "$(dirname "$0")/.."
fail() { printf 'PariNS package: %s\n' "$*" >&2; exit 1; }
sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"
    else shasum -a 256 "$@"; fi
}
version= target=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --version|--target)
            [ "$#" -ge 2 ] || fail "missing value for $1"
            case "$1" in --version) version=$2 ;; --target) target=$2 ;; esac
            shift 2 ;;
        --help|-h)
            printf '%s\n' 'Usage: sh scripts/package.sh [--version vX.Y.Z --target {x86_64,aarch64}-unknown-linux-musl]'
            exit 0 ;;
        *) fail "unknown option: $1" ;;
    esac
done
if [ -n "$version" ] || [ -n "$target" ]; then
    [ -n "$version" ] && [ -n "$target" ] || fail '--version and --target must be provided together'
    printf '%s\n' "$version" | grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' || fail 'version must be vX.Y.Z'
    cargo_version=$(cargo pkgid --locked | sed 's/.*[@#]//')
    [ "$version" = "v$cargo_version" ] || fail 'release version does not match Cargo.toml'
    [ -z "$(git status --porcelain --untracked-files=no)" ] || fail 'release requires clean tracked files'
    case "$target:$(uname -s):$(uname -m)" in
        x86_64-unknown-linux-musl:Linux:x86_64) arch=x86_64; machine=3e00 ;;
        aarch64-unknown-linux-musl:Linux:aarch64|aarch64-unknown-linux-musl:Linux:arm64) arch=aarch64; machine=b700 ;;
        *) fail 'release packages must be built and tested on their native Linux architecture' ;;
    esac
    command -v readelf >/dev/null 2>&1 || fail 'readelf is required for static ELF verification'
    release_mode=true
else
    release_mode=false
fi
(
    cd web
    npm ci --ignore-scripts
    npm run typecheck
    npm test
    npm run build
)
if [ "$release_mode" = true ]; then
    cargo build --locked --release --bins --target "$target"
    binary="target/$target/release/parins"
    helper="target/$target/release/parins-updater"
    for executable in "$binary" "$helper"; do
        [ "$(od -An -tx1 -N6 "$executable" | tr -d ' \n')" = 7f454c460201 ] || fail 'not an ELF64 little-endian binary'
        [ "$(od -An -tx1 -j18 -N2 "$executable" | tr -d ' \n')" = "$machine" ] || fail 'wrong ELF architecture'
        program_headers=$(readelf --program-headers "$executable")
        dynamic_section=$(readelf --dynamic "$executable")
        if printf '%s\n' "$program_headers" | grep -q INTERP || printf '%s\n' "$dynamic_section" | grep -q NEEDED; then
            fail 'release binary must be fully static (no interpreter or shared-library dependencies)'
        fi
    done
    archive="parins-$version-linux-$arch"
else
    cargo build --locked --release --bins
    binary=target/release/parins
    helper=target/release/parins-updater
    revision=$(git rev-parse --short=12 HEAD)
    case "$(git status --porcelain --untracked-files=no)" in
        '') ;; *) revision="$revision-dirty" ;;
    esac
    archive="parins-$revision-$(uname -s)-$(uname -m)"
fi
"./$binary" --config parins.example.toml --check
staging=$(mktemp -d "${TMPDIR:-/tmp}/parins-package.XXXXXX")
trap 'rm -rf "$staging"' EXIT
mkdir -p "$staging/$archive" target/packages
cp "$binary" "$helper" parins.example.toml LICENSE README.md CHANGELOG.md "$staging/$archive/"
"./$binary" --build-info=json > "$staging/$archive/install-build-info.json"
cp web/src/components/beui/LICENSE.beui "$staging/$archive/"
cp scripts/install.sh "$staging/$archive/"
mkdir "$staging/$archive/deploy"
units='parins.service parins-managed.service parins-updater.service parins-updater.path parins-update-recovery.service'
for unit in $units; do cp "deploy/$unit" "$staging/$archive/deploy/$unit"; done
(
    cd "$staging/$archive"
    chmod 0755 . deploy parins parins-updater install.sh
    chmod 0644 parins.example.toml LICENSE LICENSE.beui README.md CHANGELOG.md install-build-info.json deploy/*
    sha256_file parins parins-updater install-build-info.json install.sh parins.example.toml LICENSE LICENSE.beui README.md CHANGELOG.md deploy/* > SHA256SUMS
    chmod 0644 SHA256SUMS
)
if [ "$release_mode" = true ]; then
    cp "$binary" "target/packages/$archive.bin"
    cp "$staging/$archive/install-build-info.json" "target/packages/$archive.build-info.json"
    cmp "$staging/$archive/parins" "target/packages/$archive.bin" || fail 'raw and package binary mismatch'
fi
(
    cd "$staging"
    find "$archive" -exec env TZ=UTC touch -t 197001010000.00 {} +
    find "$archive" -print | sort > members
    if tar --version | grep -q 'GNU tar'; then
        set -- --owner=0 --group=0 --numeric-owner
    else
        set -- --uid 0 --gid 0 --uname '' --gname ''
        if tar --no-mac-metadata -cf /dev/null -T /dev/null 2>/dev/null; then
            set -- "$@" --no-mac-metadata
        fi
    fi
    tar "$@" --format ustar --no-recursion -cf package.tar -T members
)
gzip -n -c "$staging/package.tar" > "target/packages/$archive.tar.gz"
(cd target/packages && sha256_file "$archive.tar.gz") > "target/packages/$archive.tar.gz.sha256"
printf 'Package: target/packages/%s.tar.gz\nChecksum: target/packages/%s.tar.gz.sha256\n' "$archive" "$archive"
