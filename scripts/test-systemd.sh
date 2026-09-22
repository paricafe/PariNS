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
for command in sudo systemctl systemd-analyze curl jq openssl python3; do
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
    for name in token-copy state-copy credentials.json candidate.toml setup.json setup-header response.json auth-header status.json; do
        rm -f "$fixture/$name"
    done
    rmdir "$fixture" || status=1
    exit "$status"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
sudo chmod go-w /opt
install_service() {
    sudo sh "$repo/scripts/install.sh" --binary "$repo/target/release/parins"
    sudo systemctl is-active --quiet parins-managed.service
}
session() { curl --fail --silent --show-error --max-time 5 http://127.0.0.1:3000/api/session; }
installed=true
install_service
sudo systemd-analyze verify /etc/systemd/system/parins-managed.service
session | jq -e '.setup_required == true' >/dev/null
sudo test -s /var/lib/parins/setup-token
[ "$(sudo stat -c %a /var/lib/parins/setup-token)" = 600 ]
[ "$(sudo stat -Lc %a /var/lib/parins)" = 700 ]
sudo cat /var/lib/parins/setup-token > "$fixture/token-copy"
install_service
sudo cmp "$fixture/token-copy" /var/lib/parins/setup-token
session | jq -e '.setup_required == true' >/dev/null
! sudo test -e /var/lib/parins/state.json

# No public DNS, certificates or external APIs: blocked .invalid queries are
# answered locally on a kernel-assigned high port, with a loopback-only upstream.
printf '%s\n' 'listen = "127.0.0.1:0"' 'upstream = "127.0.0.1:9"' \
    'query_timeout_ms = 100' 'tcp_io_timeout_ms = 1000' 'shutdown_grace_ms = 1000' \
    'max_inflight = 16' 'max_tcp_connections = 8' \
    '[filter]' 'enabled = true' 'block_exact = ["ci.invalid"]' > "$fixture/candidate.toml"
openssl rand -hex 24 | jq -Rs '{username:"ci-admin",password:rtrimstr("\n")}' > "$fixture/credentials.json"
jq --rawfile toml "$fixture/candidate.toml" '. + {toml:$toml}' "$fixture/credentials.json" > "$fixture/setup.json"
sed 's/^/X-PariNS-Setup: /' "$fixture/token-copy" > "$fixture/setup-header"
curl --fail --silent --show-error --max-time 15 -H 'Content-Type: application/json' \
    -H "@$fixture/setup-header" --data-binary "@$fixture/setup.json" \
    http://127.0.0.1:3000/api/setup > "$fixture/response.json"
jq -er '"Authorization: Bearer " + .token' "$fixture/response.json" > "$fixture/auth-header"
session | jq -e '.setup_required == false' >/dev/null
sudo test -s /var/lib/parins/state.json
[ "$(sudo stat -c %a /var/lib/parins/state.json)" = 600 ]
sudo cat /var/lib/parins/state.json > "$fixture/state-copy"

check_dns() {
    curl --fail --silent --show-error --max-time 5 -H "@$fixture/auth-header" \
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
sudo cmp "$fixture/state-copy" /var/lib/parins/state.json
session | jq -e '.setup_required == false' >/dev/null
curl --fail --silent --show-error --max-time 15 -H 'Content-Type: application/json' \
    --data-binary "@$fixture/credentials.json" http://127.0.0.1:3000/api/login > "$fixture/response.json"
jq -er '"Authorization: Bearer " + .token' "$fixture/response.json" > "$fixture/auth-header"
check_dns
printf '%s\n' 'Linux systemd install, setup, UDP/TCP DNS and state-preserving upgrade passed.'
