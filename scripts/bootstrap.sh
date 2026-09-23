#!/bin/sh
# Download a verified release and delegate installation to its bundled installer.
# Keep the entry point last so a truncated piped download cannot start installing.
set -eu
umask 077

fail() { printf 'PariNS bootstrap: %s\n' "$*" >&2; exit 1; }
usage() {
    printf '%s\n' 'Usage: sudo sh bootstrap.sh [--version v0.1.2] [--dry-run]' \
        '       sh bootstrap.sh --root EXISTING_PRIVATE_DIRECTORY [--version v0.1.2] [--dry-run]' \
        'Linux x86_64/aarch64 and systemd are required for live installation.' \
        '--root stages files only; no service or candidate binary is executed.'
}
owner() { stat -c %u "$1" 2>/dev/null || stat -f %u "$1"; }
private_directory() {
    mode=$(stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1") || return 1
    case "$mode" in *00) return 0 ;; *) return 1 ;; esac
}
digest() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
    else shasum -a 256 "$1" | awk '{print $1}'; fi
}
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    # The only recursive deletion is our mktemp-created, privately owned leaf.
    if [ -n "${download_dir:-}" ] && [ -d "$download_dir" ] && [ ! -L "$download_dir" ]; then
        case "$download_dir" in "$temporary_parent"/parins-bootstrap.??????)
            if [ "$(owner "$download_dir")" = "$(id -u)" ] &&
                private_directory "$download_dir"; then
                rm -rf -- "$download_dir"
            fi ;;
        esac
    fi
    exit "$status"
}
main() {
    version=v0.1.2 root= dry_run=false
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version|--root)
                [ "$#" -ge 2 ] || fail "missing value for $1"
                case "$1" in --version) version=$2 ;; --root) root=$2; [ -n "$root" ] || fail 'empty staging root' ;; esac
                shift 2 ;;
            --dry-run) dry_run=true; shift ;;
            --help|-h) usage; exit 0 ;;
            *) usage >&2; fail "unknown option: $1" ;;
        esac
    done
    printf '%s\n' "$version" | LC_ALL=C grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || fail 'version must be vMAJOR.MINOR.PATCH'
    [ "$(printf '%s' "$version" | wc -l | tr -d ' ')" = 0 ] || fail 'invalid version'
    for command in curl tar awk grep mktemp stat find id uname mkdir rm sh; do
        command -v "$command" >/dev/null 2>&1 || fail "missing command: $command"
    done
    command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1 || fail 'sha256sum or shasum is required'
    [ "$(uname -s)" = Linux ] || fail 'supported platforms: Linux x86_64 and aarch64'
    case "$(uname -m)" in
        x86_64) architecture=x86_64 ;;
        aarch64|arm64) architecture=aarch64 ;;
        *) fail 'supported platforms: Linux x86_64 and aarch64' ;;
    esac
    if [ -n "$root" ]; then
        case "$root" in /*) ;; *) fail 'staging root must be absolute' ;; esac
        [ -d "$root" ] && [ ! -L "$root" ] || fail 'staging root must be an existing non-symlink directory'
        root=$(CDPATH= cd -- "$root" && pwd -P)
        [ "$root" != / ] || fail 'staging root must not be /'
        [ "$(owner "$root")" = "$(id -u)" ] || fail 'staging root must be owned by the current user'
        private_directory "$root" || fail 'staging root must be private (mode 0700)'
    else
        [ "$(id -u)" = 0 ] || fail 'live installation requires root; run sudo sh bootstrap.sh'
        command -v systemctl >/dev/null 2>&1 || fail 'systemctl is required'
        [ -d /run/systemd/system ] || fail 'systemd must be running as the service manager'
    fi
    archive="parins-$version-linux-$architecture"
    asset="$archive.tar.gz"
    base_url="https://github.com/paricafe/PariNS/releases/download/$version"
    temporary_parent=$(CDPATH= cd /tmp && pwd -P)
    download_dir=$(mktemp -d "$temporary_parent/parins-bootstrap.XXXXXX")
    trap cleanup EXIT
    trap 'exit 1' HUP INT TERM
    printf 'Downloading PariNS %s for Linux %s\n' "$version" "$architecture"
    for name in "$asset" "$asset.sha256"; do
        curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
            --connect-timeout 15 --max-time 300 --output "$download_dir/$name" "$base_url/$name" || fail "download failed: $name"
    done
    expected=$(awk -v asset="$asset" '
        NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-fA-F]/ || $2 != asset { exit 1 }
        { hash = tolower($1) }
        END { if (NR != 1) exit 1; print hash }
    ' "$download_dir/$asset.sha256") || fail 'invalid archive checksum file'
    [ "$(digest "$download_dir/$asset")" = "$expected" ] || fail 'archive checksum mismatch'
    # Reject links, special files, extra paths, duplicate entries and traversal.
    tar -tzf "$download_dir/$asset" > "$download_dir/members" || fail 'cannot list archive'
    awk -v prefix="$archive/" '
        { if (++seen[$0] != 1) exit 1 }
        $0 == prefix || $0 == prefix "deploy/" { next }
        index($0, prefix) != 1 { exit 1 }
        { name = substr($0, length(prefix) + 1) }
        name !~ /^(SHA256SUMS|parins|install\.sh|parins\.example\.toml|LICENSE|README\.md|CHANGELOG\.md|deploy\/parins(-managed)?\.service)$/ { exit 1 }
        END { if (NR == 0) exit 1 }
    ' "$download_dir/members" || fail 'unsafe or unexpected archive path'
    tar -tvzf "$download_dir/$asset" > "$download_dir/types" || fail 'cannot inspect archive types'
    awk -v prefix="$archive/" '
        substr($0, 1, 1) == "d" && ($NF == prefix || $NF == prefix "deploy/") { next }
        substr($0, 1, 1) != "-" { exit 1 }
    ' "$download_dir/types" || fail 'archive links and special files are forbidden'
    package="$download_dir/$archive"
    mkdir -m 0700 "$package" "$package/deploy"
    files='parins install.sh parins.example.toml LICENSE README.md CHANGELOG.md deploy/parins-managed.service deploy/parins.service'
    # Extract exact regular-file contents, never archive paths or permissions.
    for name in SHA256SUMS $files; do
        grep -Fxq "$archive/$name" "$download_dir/members" || fail "missing package member: $name"
        tar -xOzf "$download_dir/$asset" "$archive/$name" > "$package/$name" || fail "cannot read package member: $name"
    done
    awk '
        NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-fA-F]/ { exit 1 }
        $2 !~ /^(parins|install\.sh|parins\.example\.toml|LICENSE|README\.md|CHANGELOG\.md|deploy\/parins(-managed)?\.service)$/ { exit 1 }
        ++seen[$2] != 1 { exit 1 }
        END { if (NR == 0) exit 1 }
    ' "$package/SHA256SUMS" || fail 'invalid package checksum manifest'
    for name in $files; do
        expected=$(awk -v name="$name" '$2 == name {print tolower($1)}' "$package/SHA256SUMS")
        [ -n "$expected" ] && [ "$(digest "$package/$name")" = "$expected" ] || fail "package checksum mismatch: $name"
    done
    set --
    if [ -n "$root" ]; then set -- "$@" --root "$root"; fi
    if "$dry_run"; then set -- "$@" --dry-run; fi
    # The installer owns platform validation, atomic replacement and rollback.
    sh "$package/install.sh" "$@"
}

main "$@"
