#!/bin/sh
# Real service acceptance ONLY on a disposable GitHub-hosted Linux runner.
# Never run this on a development machine or an existing PariNS installation.
set -eu
umask 077
[ "${1:-}" = --ephemeral-ci ] && [ "${GITHUB_ACTIONS:-}" = true ] && \
    [ "${RUNNER_ENVIRONMENT:-}" = github-hosted ] && \
    [ "${RUNNER_OS:-}" = Linux ] && [ "$(uname -s)" = Linux ] || {
    printf '%s\n' 'Refusing: requires --ephemeral-ci on a disposable GitHub Linux runner.' >&2
    exit 1
}
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
shift
binary="$repo/target/release/parins"
installer="$repo/scripts/install.sh"
package= binary_override=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --binary-path|--package)
            [ "$#" -ge 2 ] || { printf 'Missing value for %s\n' "$1" >&2; exit 1; }
            case "$1" in
                --binary-path) binary=$2; binary_override=true ;;
                --package) package=$2 ;;
            esac
            shift 2 ;;
        *) printf 'Unknown option: %s\n' "$1" >&2; exit 1 ;;
    esac
done
if [ -n "$package" ]; then
    ! "$binary_override" || { printf '%s\n' '--package and --binary-path are mutually exclusive' >&2; exit 1; }
    package=$(CDPATH= cd -- "$package" && pwd -P)
    binary="$package/parins"
    installer="$package/install.sh"
    (cd "$package" && sha256sum --check SHA256SUMS)
fi
[ -f "$binary" ] && [ -f "$installer" ]
for command in sudo systemctl systemd-analyze curl jq openssl python3 ip ss sha256sum; do
    command -v "$command" >/dev/null 2>&1 || { printf 'Missing: %s\n' "$command" >&2; exit 1; }
done
sudo -n true
[ -z "$(systemctl show --property=FragmentPath --value parins-managed.service)" ]
! sudo test -e /var/lib/parins
! sudo test -e /opt/parins-managed/parins
# Hosted runners make /opt group-writable for their tool cache. Only this
# disposable-runner fixture tightens it temporarily; the installer stays strict.
[ -d /opt ] && [ ! -L /opt ] && [ "$(sudo stat -c %u /opt)" = 0 ] || {
    printf '%s\n' 'Refusing: runner /opt must be a root-owned, non-symlink directory.' >&2
    exit 1
}
opt_mode=$(sudo stat -c %a /opt)
fixture=$(mktemp -d "${TMPDIR:-/tmp}/parins-systemd-test.XXXXXX")
installed=false
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    if "$installed" && sudo test -f /etc/systemd/system/parins-managed.service && \
        ! sudo test -L /etc/systemd/system/parins-managed.service && \
        [ "$(sudo stat -c %u /etc/systemd/system/parins-managed.service)" = 0 ] && \
        sudo grep -Fqx '# PariNS managed installer unit v1' /etc/systemd/system/parins-managed.service; then
        sudo systemctl disable --now parins-managed.service || status=1
    fi
    sudo chmod "$opt_mode" /opt || status=1
    # Only our known private fixture files; installed state remains on the
    # disposable runner until it is destroyed, with the service disabled.
    for name in token-copy state-copy credentials.json candidate.toml setup.json setup-header response.json cookies.txt status.json; do
        rm -f "$fixture/$name"
    done
    rmdir "$fixture" || status=1
    exit "$status"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
sudo chmod go-w /opt
install_service() {
    sudo sh "$installer" --binary "$binary"
    sudo systemctl is-active --quiet parins-managed.service
    sudo cmp "$binary" /opt/parins-managed/parins
}
http() {
    curl --fail --silent --show-error --noproxy '*' \
        --cookie "$fixture/cookies.txt" --cookie-jar "$fixture/cookies.txt" "$@"
}
session() { http --max-time 5 http://127.0.0.1:3000/api/session; }
installed=true
install_service
sudo systemd-analyze verify /etc/systemd/system/parins-managed.service
: > "$fixture/cookies.txt"
! sudo test -e /var/lib/parins/https-identity.pem
! sudo test -e /var/lib/parins/https-cert.pem
session | jq -e '.setup_required == true and .transport.scheme == "http"' >/dev/null
for asset in / /theme-init.js /assets/app.js /assets/app.css; do
    http --max-time 5 "http://127.0.0.1:3000$asset" >/dev/null
done
# Exercise the real non-loopback interface without requiring public Internet routing.
ss -H -ltn4 'sport = :3000' | awk '{print $4}' | grep -Fx '0.0.0.0:3000' >/dev/null
interface_ip=$(ip -4 -o address show scope global | awk 'NR == 1 {split($4, address, "/"); print address[1]}')
[ -n "$interface_ip" ]
http --max-time 5 --connect-to "127.0.0.1:3000:$interface_ip:3000" \
    -H 'Host: 198.51.100.42:3000' -H 'Origin: http://198.51.100.42:3000' \
    http://127.0.0.1:3000/api/session | jq -e '.setup_required == true' >/dev/null
# Negative protocol probe only: ignore trust so a stray self-signed HTTPS
# listener cannot make this check pass merely because its certificate is unknown.
if curl --fail --silent --insecure --noproxy '*' --max-time 5 https://127.0.0.1:3000/api/session >/dev/null 2>&1; then
    printf '%s\n' 'Unexpected HTTPS response without inbound DoH.' >&2
    exit 1
fi
sudo test -s /var/lib/parins/setup-token
[ "$(sudo stat -c %a /var/lib/parins/setup-token)" = 600 ]
[ "$(sudo stat -Lc %a /var/lib/parins)" = 700 ]
sudo cat /var/lib/parins/setup-token > "$fixture/token-copy"
install_service
sudo cmp "$fixture/token-copy" /var/lib/parins/setup-token
session | jq -e '.setup_required == true and .transport.scheme == "http"' >/dev/null
! sudo test -e /var/lib/parins/state.json

# No public DNS, certificate authorities or external APIs: blocked .invalid queries are
# answered locally on a kernel-assigned high port, with a loopback-only upstream.
printf '%s\n' 'listen = "127.0.0.1:0"' \
    'query_timeout_ms = 100' 'tcp_io_timeout_ms = 1000' 'shutdown_grace_ms = 1000' \
    'max_inflight = 16' 'max_tcp_connections = 8' \
    '[upstreams]' 'servers = ["udp://127.0.0.1:9"]' \
    '[filter]' 'enabled = true' 'block_exact = ["ci.invalid"]' > "$fixture/candidate.toml"
openssl rand -hex 24 | jq -Rs '{username:"ci-admin",password:rtrimstr("\n")}' > "$fixture/credentials.json"
jq --rawfile toml "$fixture/candidate.toml" '. + {toml:$toml}' "$fixture/credentials.json" > "$fixture/setup.json"
sed 's/^/X-PariNS-Setup: /' "$fixture/token-copy" > "$fixture/setup-header"
http --max-time 15 -H 'Origin: http://127.0.0.1:3000' \
    -H 'Content-Type: application/json' \
    -H "@$fixture/setup-header" --data-binary "@$fixture/setup.json" \
    http://127.0.0.1:3000/api/setup > "$fixture/response.json"
binding=$(jq -er '.session.binding | select(type == "string" and length > 0)' "$fixture/response.json")
session | jq -e '.setup_required == false and .authenticated == true' >/dev/null
sudo test -s /var/lib/parins/state.json
[ "$(sudo stat -c %a /var/lib/parins/state.json)" = 600 ]
sudo cat /var/lib/parins/state.json > "$fixture/state-copy"

check_dns() {
    http --max-time 5 -H "X-PariNS-Session: $binding" \
        http://127.0.0.1:3000/api/status > "$fixture/status.json"
    jq -e '.running == true and .revision == 1 and .last_error == null' "$fixture/status.json" >/dev/null
    address=$(jq -er '.listen' "$fixture/status.json")
    python3 - "$address" <<'PY'
import socket
import struct
import sys

host, port = sys.argv[1].rsplit(":", 1)
assert host == "127.0.0.1" and int(port) > 1024
query = struct.pack("!6H", 0x504e, 0x100, 1, 0, 0, 0) + b"\x02ci\x07invalid\x00\x00\x01\x00\x01"
for kind in (socket.SOCK_DGRAM, socket.SOCK_STREAM):
    with socket.socket(socket.AF_INET, kind) as client:
        client.settimeout(3)
        client.connect((host, int(port)))
        if kind == socket.SOCK_DGRAM:
            client.send(query)
            response = client.recv(4096)
        else:
            client.sendall(struct.pack("!H", len(query)) + query)
            def read_exact(size):
                result = b""
                while len(result) < size:
                    block = client.recv(size - len(result))
                    assert block, "unexpected DNS EOF"
                    result += block
                return result
            response = read_exact(struct.unpack("!H", read_exact(2))[0])
        ident, flags, questions, answers, _, _ = struct.unpack("!6H", response[:12])
        assert ident == 0x504e and flags & 0x8000 and flags & 0xf == 0
        assert questions == 1 and answers == 0
PY
}
check_dns
install_service
! sudo test -e /var/lib/parins/https-identity.pem
! sudo test -e /var/lib/parins/https-cert.pem
sudo cmp "$fixture/state-copy" /var/lib/parins/state.json
session | jq -e '.setup_required == false and .authenticated == false' >/dev/null
http --max-time 15 -H 'Origin: http://127.0.0.1:3000' \
    -H 'Content-Type: application/json' \
    --data-binary "@$fixture/credentials.json" http://127.0.0.1:3000/api/login > "$fixture/response.json"
binding=$(jq -er '.session.binding' "$fixture/response.json")
session | jq -e '.setup_required == false and .authenticated == true' >/dev/null
check_dns
printf '%s\n' 'Linux systemd install, HTTP on wildcard/non-loopback, setup, UDP/TCP DNS and state-preserving upgrade passed.'
