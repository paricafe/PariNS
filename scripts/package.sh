#!/bin/sh
# Host-native development packages, or checked Linux release packages.
set -eu
cd "$(dirname "$0")/.."
fail() { printf 'PariNS package: %s\n' "$*" >&2; exit 1; }
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
    printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || fail 'version must be vX.Y.Z'
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
# A standalone package always embeds assets built from this checkout. Do not
# infer freshness from an existing, ignored web/dist directory.
(
    cd web
    npm ci --ignore-scripts
    npm run typecheck
    npm test
    npm run build
)
if [ "$release_mode" = true ]; then
    cargo build --locked --release --target "$target"
    binary="target/$target/release/parins"
    [ "$(od -An -tx1 -N6 "$binary" | tr -d ' \n')" = 7f454c460201 ] || fail 'not an ELF64 little-endian binary'
    [ "$(od -An -tx1 -j18 -N2 "$binary" | tr -d ' \n')" = "$machine" ] || fail 'wrong ELF architecture'
    program_headers=$(readelf --program-headers "$binary")
    dynamic_section=$(readelf --dynamic "$binary")
    if printf '%s\n' "$program_headers" | grep -q INTERP || printf '%s\n' "$dynamic_section" | grep -q NEEDED; then
        fail 'release binary must be fully static (no interpreter or shared-library dependencies)'
    fi
    archive="parins-$version-linux-$arch"
else
    cargo build --locked --release
    binary=target/release/parins
    revision=$(git rev-parse --short=12 HEAD)
    case "$(git status --porcelain --untracked-files=no)" in
        '') ;; *) revision="$revision-dirty" ;;
    esac
    archive="parins-$revision-$(uname -s)-$(uname -m)"
fi
"./$binary" --config parins.example.toml --check
staging=$(mktemp -d "${TMPDIR:-/tmp}/parins-package.XXXXXX")
mkdir -p "$staging/$archive" target/packages
cp "$binary" parins.example.toml LICENSE README.md CHANGELOG.md "$staging/$archive/"
cp web/src/components/beui/LICENSE.beui "$staging/$archive/"
cp scripts/install.sh "$staging/$archive/"
cp -R deploy "$staging/$archive/"
(cd "$staging/$archive" && shasum -a 256 parins install.sh parins.example.toml LICENSE LICENSE.beui README.md CHANGELOG.md deploy/*.service) > "$staging/$archive/SHA256SUMS"
tar -czf "target/packages/$archive.tar.gz" -C "$staging" "$archive"
(cd target/packages && shasum -a 256 "$archive.tar.gz") > "target/packages/$archive.tar.gz.sha256"
printf 'Package: target/packages/%s.tar.gz\nChecksum: target/packages/%s.tar.gz.sha256\n' "$archive" "$archive"
# Leave the small staging directory for inspection; no broad cleanup command.
printf 'Staging: %s\n' "$staging"
