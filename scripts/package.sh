#!/bin/sh
# Local, host-native artifact. Does not publish or deploy anything.
set -eu
cd "$(dirname "$0")/.."
cargo build --locked --release
./target/release/parins --config parins.example.toml --check
revision=$(git rev-parse --short=12 HEAD)
case "$(git status --porcelain --untracked-files=no)" in
  '') ;;
  *) revision="$revision-dirty" ;;
esac
platform="$(uname -s)-$(uname -m)"
archive="parins-$revision-$platform"
staging=$(mktemp -d "${TMPDIR:-/tmp}/parins-package.XXXXXX")
mkdir -p "$staging/$archive" target/packages
cp target/release/parins parins.example.toml LICENSE README.md "$staging/$archive/"
cp scripts/install.sh "$staging/$archive/"
cp -R deploy "$staging/$archive/"
(cd "$staging/$archive" && shasum -a 256 parins install.sh parins.example.toml LICENSE README.md deploy/*.service) > "$staging/$archive/SHA256SUMS"
tar -czf "target/packages/$archive.tar.gz" -C "$staging" "$archive"
(cd target/packages && shasum -a 256 "$archive.tar.gz") > "target/packages/$archive.tar.gz.sha256"
printf 'Package: target/packages/%s.tar.gz\nChecksum: target/packages/%s.tar.gz.sha256\n' "$archive" "$archive"
# Leave the small staging directory for inspection; no broad cleanup command.
printf 'Staging: %s\n' "$staging"
