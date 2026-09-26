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
build_info=
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
    build_info="$package/install-build-info.json"
    (cd "$package" && sha256sum --check SHA256SUMS)
fi
helper="$(dirname "$binary")/parins-updater"
[ -f "$binary" ] && [ -f "$helper" ] && [ -f "$installer" ]
for command in sudo systemctl systemd-analyze curl jq openssl python3 ip ss sha256sum; do
    command -v "$command" >/dev/null 2>&1 || { printf 'Missing: %s\n' "$command" >&2; exit 1; }
done
sudo -n true
[ -z "$(systemctl show --property=FragmentPath --value parins-managed.service)" ]
! sudo test -e /var/lib/parins
! sudo test -e /var/lib/parins-managed
! sudo test -L /var/lib/parins-managed
! sudo test -e /var/lib/private/parins-managed
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
        sudo grep -Fqx '# PariNS managed installer unit v3 (restricted updater contract)' /etc/systemd/system/parins-managed.service; then
        sudo systemctl disable --now parins-updater.path || status=1
        sudo systemctl disable --now parins-managed.service || status=1
    fi
    sudo chmod "$opt_mode" /opt || status=1
    # Only our known private fixture files; installed state remains on the
    # disposable runner until it is destroyed, with the service disabled.
    for name in token-copy state-copy credentials.json candidate.toml setup.json setup-header response.json cookies.txt status.json refusal.log home-error.log tls-before tls-after install-build-info.json; do
        rm -f "$fixture/$name"
    done
    rmdir "$fixture" || status=1
    exit "$status"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
if [ -z "$build_info" ]; then
    # Source CI has no package manifest. Query the selected build as the runner,
    # before sudo; the root installer must never execute a candidate for metadata.
    build_info="$fixture/install-build-info.json"
    "$binary" --build-info=json > "$build_info"
fi
sudo chmod go-w /opt
install_service() {
    if sudo sh "$installer" --binary "$binary" --helper "$helper" --build-info "$build_info"; then
        :
    else
        install_status=$?
        # This disposable fixture contains only generated test configuration.
        # Preserve the bounded preflight error before runner cleanup; do not
        # print state.json, credentials, setup responses or session cookies.
        if sudo test -f /var/lib/parins-updater/private/install-preflight-error.txt; then
            printf '%s\n' 'Candidate preflight stderr:' >&2
            sudo head -c 32768 /var/lib/parins-updater/private/install-preflight-error.txt >&2 || true
        fi
        return "$install_status"
    fi
    sudo systemctl is-active --quiet parins-managed.service
    sudo cmp "$binary" /opt/parins-managed/parins
}
http() {
    curl --fail --silent --show-error --noproxy '*' \
        --cookie "$fixture/cookies.txt" --cookie-jar "$fixture/cookies.txt" "$@"
}
session() { http --max-time 5 http://127.0.0.1:3000/api/session; }
refuse_install() {
    if sudo sh "$installer" --binary "$binary" --helper "$helper" --build-info "$build_info" > "$fixture/refusal.log" 2>&1; then
        printf '%s\n' 'Expected installation refusal before changing files or service.' >&2
        exit 1
    fi
    ! sudo test -e /opt/parins-managed/parins
}
# Real systemd must never get the chance to claim unknown or legacy state.
sudo mkdir -m 0700 /var/lib/parins-managed
refuse_install
sudo rmdir /var/lib/parins-managed
sudo ln -s /var/lib/private/parins-managed /var/lib/parins-managed
refuse_install
sudo rm /var/lib/parins-managed
sudo install -d -m 0700 /var/lib/private/parins-managed
refuse_install
sudo rmdir /var/lib/private/parins-managed
sudo install -d -m 0700 /var/lib/parins
for name in state.json setup-token; do
    printf 'legacy managed sentinel\n' | sudo tee "/var/lib/parins/$name" >/dev/null
    refuse_install
    sudo grep -Fxq 'legacy managed sentinel' "/var/lib/parins/$name"
    sudo rm "/var/lib/parins/$name"
done
printf '# PariNS managed installer unit v1\n[Service]\nDynamicUser=yes\nStateDirectory=parins\n' |
    sudo tee /etc/systemd/system/parins-managed.service >/dev/null
refuse_install
sudo rm /etc/systemd/system/parins-managed.service
# External TLS files under the old shared path can coexist unchanged.
sudo install -d -m 0750 /var/lib/parins/tls
printf 'external certificate sentinel\n' | sudo tee /var/lib/parins/tls/external.pem >/dev/null
sudo chmod 0640 /var/lib/parins/tls/external.pem
sudo stat -c '%n %u %g %a %i %s' /var/lib/parins /var/lib/parins/tls /var/lib/parins/tls/external.pem > "$fixture/tls-before"
mkdir -m 0700 "$fixture/account-home"
if env HOME="$fixture/account-home" "$binary" --manage --state-dir "$fixture/account-home" --web-listen 127.0.0.1:0 > "$fixture/home-error.log" 2>&1; then
    printf '%s\n' 'HOME state directory was unexpectedly accepted.' >&2
    exit 1
fi
grep -Fq 'dedicated private subdirectory' "$fixture/home-error.log"
grep -Fq "$fixture/account-home" "$fixture/home-error.log"
rmdir "$fixture/account-home"
installed=true
install_service
sudo systemd-analyze verify /etc/systemd/system/parins-managed.service
sudo systemd-analyze verify /etc/systemd/system/parins-updater.service /etc/systemd/system/parins-updater.path /etc/systemd/system/parins-update-recovery.service
systemctl is-active --quiet parins-updater.path
systemctl is-active --quiet parins-update-recovery.service
test "$(systemctl show --property=RemainAfterExit --value parins-update-recovery.service)" = yes
test "$(systemctl show --property=TimeoutStopUSec --value parins-managed.service)" = '2min 25s'
systemctl show --property=After,Before,Requires parins-managed.service | grep -Fq parins-update-recovery.service
! systemctl show --property=After,Before,Requires parins-managed.service | grep -E '(^| )parins-updater.service( |$)'
sudo jq -e '.schema == 1 and .operation == null and .installed.build.install_contract == "linux-managed-updater-v1"' /var/lib/parins-updater/private/journal.json >/dev/null
test "$(sudo stat -c %a /var/lib/parins-updater/private)" = 700
test "$(sudo stat -c %a /var/lib/parins-updater/status.json)" = 644
# Invalid untrusted inbox is consumed without repeating activations or touching
# business files. This runs only in this disposable, real-systemd environment.
printf '{"schema":1,"command":"/bin/false"}\n' | sudo tee /var/lib/parins-managed/update-request.json >/dev/null
sleep 2
! sudo test -e /var/lib/parins-managed/update-request.json
systemctl is-active --quiet parins-managed.service
sudo systemctl reset-failed parins-updater.service
sudo grep -Fqx 'ExecReload=/bin/kill -HUP $MAINPID' /etc/systemd/system/parins-managed.service
systemctl show --property=ExecReload --value parins-managed.service | grep -F 'argv[]=/bin/kill -HUP $MAINPID ; ignore_errors=no' >/dev/null
: > "$fixture/cookies.txt"
! sudo test -e /var/lib/parins-managed/https-identity.pem
! sudo test -e /var/lib/parins-managed/https-cert.pem
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
sudo test -s /var/lib/parins-managed/setup-token
[ "$(sudo stat -c %a /var/lib/parins-managed/setup-token)" = 600 ]
[ "$(sudo stat -Lc %a /var/lib/parins-managed)" = 700 ]
sudo cat /var/lib/parins-managed/setup-token > "$fixture/token-copy"
install_service
sudo cmp "$fixture/token-copy" /var/lib/parins-managed/setup-token
session | jq -e '.setup_required == true and .transport.scheme == "http"' >/dev/null
! sudo test -e /var/lib/parins-managed/state.json

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
setup_service() {
    # Root installation readiness precedes the managed process's next frozen
    # state reconciliation. Only this explicit rejection proves no setup was
    # accepted; unknown outcomes and every other failure must not be replayed.
    setup_attempt=1
    while :; do
        if setup_status=$(curl --silent --show-error --noproxy '*' --max-time 15 \
            --cookie "$fixture/cookies.txt" --cookie-jar "$fixture/cookies.txt" \
            -H 'Origin: http://127.0.0.1:3000' -H 'Content-Type: application/json' \
            -H "@$fixture/setup-header" --data-binary "@$fixture/setup.json" \
            --output "$fixture/response.json" --write-out '%{http_code}' \
            http://127.0.0.1:3000/api/setup); then
            [ "$setup_status" != 200 ] || return 0
        else
            setup_curl_status=$?
            printf 'Setup transport failed (curl %s); outcome unknown, not retried.\n' "$setup_curl_status" >&2
            return "$setup_curl_status"
        fi
        # Never print response bodies: a successful body carries a session binding.
        setup_code=$(jq -er '.error.code | select(type == "string" and test("^[A-Za-z0-9_]{1,64}$"))' "$fixture/response.json" 2>/dev/null) || setup_code=invalid_response
        if [ "$setup_status" = 409 ] && [ "$setup_code" = update_in_progress ] && [ "$setup_attempt" -lt 3 ]; then
            setup_attempt=$((setup_attempt + 1))
            sleep 1
        else
            printf 'Setup rejected: HTTP %s, code %s (attempt %s).\n' "$setup_status" "$setup_code" "$setup_attempt" >&2
            return 1
        fi
    done
}
setup_service
binding=$(jq -er '.session.binding | select(type == "string" and length > 0)' "$fixture/response.json")
session | jq -e '.setup_required == false and .authenticated == true' >/dev/null
sudo test -s /var/lib/parins-managed/state.json
[ "$(sudo stat -c %a /var/lib/parins-managed/state.json)" = 600 ]
sudo cat /var/lib/parins-managed/state.json > "$fixture/state-copy"

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
! sudo test -e /var/lib/parins-managed/https-identity.pem
! sudo test -e /var/lib/parins-managed/https-cert.pem
sudo cmp "$fixture/state-copy" /var/lib/parins-managed/state.json
session | jq -e '.setup_required == false and .authenticated == false' >/dev/null
http --max-time 15 -H 'Origin: http://127.0.0.1:3000' \
    -H 'Content-Type: application/json' \
    --data-binary "@$fixture/credentials.json" http://127.0.0.1:3000/api/login > "$fixture/response.json"
binding=$(jq -er '.session.binding' "$fixture/response.json")
session | jq -e '.setup_required == false and .authenticated == true' >/dev/null
check_dns
# A valid native executable that exits immediately fails read-only preflight
# before replacement or stopping the previously active service.
if sudo sh "$installer" --binary /bin/false --helper "$helper" --build-info "$build_info" > "$fixture/refusal.log" 2>&1; then
    printf '%s\n' 'Expected read-only preflight failure without replacing the running service.' >&2
    exit 1
fi
grep -Fq 'read-only candidate preflight failed' "$fixture/refusal.log"
sudo systemctl is-active --quiet parins-managed.service
sudo cmp "$binary" /opt/parins-managed/parins
sudo cmp "$fixture/state-copy" /var/lib/parins-managed/state.json
sudo stat -c '%n %u %g %a %i %s' /var/lib/parins /var/lib/parins/tls /var/lib/parins/tls/external.pem > "$fixture/tls-after"
cmp "$fixture/tls-before" "$fixture/tls-after"
sudo grep -Fxq 'external certificate sentinel' /var/lib/parins/tls/external.pem
printf '%s\n' 'Linux systemd directory ownership, external TLS, HOME errors, setup, UDP/TCP DNS, state-preserving upgrade and read-only preflight rejection passed.'
