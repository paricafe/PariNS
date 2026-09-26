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
package= binary_override=false filter_subscriptions=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --filter-subscriptions) filter_subscriptions=true; shift ;;
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
fs_uid= fs_chain=
fs_trace_job= fs_tracer_pid= fs_fault_pid= fs_fault_ns= fs_staging=
fs_tmpfs_active=false
fs_trace_stop() {
    [ -n "$fs_trace_job" ] || return 0
    if [ -n "$fs_tracer_pid" ] && sudo test -e "/proc/$fs_tracer_pid/exe"; then
        [ "$(sudo readlink "/proc/$fs_tracer_pid/exe")" = "$fs_strace" ] || {
            printf '%s\n' 'FS7 trace cleanup refused: tracer executable identity changed.' >&2; return 1;
        }
        sudo kill -INT "$fs_tracer_pid" || return 1
    elif [ -z "$fs_tracer_pid" ]; then
        # sudo/timeout forwards termination to its child if attachment failed.
        sudo kill -TERM "$fs_trace_job" 2>/dev/null || true
    fi
    fs_trace_status=0
    wait "$fs_trace_job" || fs_trace_status=$?
    fs_trace_job= fs_tracer_pid=
    [ "$fs_trace_status" -eq 0 ] || {
        printf 'FS7 tracer exited unexpectedly: status=%s\n' "$fs_trace_status" >&2; return 1;
    }
}
fs_tmpfs_remove() {
    "$fs_tmpfs_active" || return 0
    if ! sudo test -e "/proc/$fs_fault_pid/ns/mnt"; then
        # No other fixture process stays in this namespace. Its disappearance
        # destroys the private mount; never unmount through a different PID.
        printf '%s\n' 'FS7 tmpfs cleanup: original service namespace has exited.'
        fs_tmpfs_active=false
        return 0
    fi
    [ "$(sudo readlink "/proc/$fs_fault_pid/ns/mnt")" = "$fs_fault_ns" ] || {
        printf '%s\n' 'FS7 tmpfs cleanup refused: mount namespace identity changed.' >&2; return 1;
    }
    fs_top=$(sudo "$fs_nsenter" --target "$fs_fault_pid" --mount -- "$fs_findmnt" -rn -T "$fs_staging" -o TARGET,FSTYPE,SOURCE) || return 1
    if [ "$fs_top" = "$fs_staging tmpfs parins-fs7-enospc" ]; then
        sudo "$fs_nsenter" --target "$fs_fault_pid" --mount -- "$fs_umount" "$fs_staging" || return 1
    else
        case "$fs_top" in "$fs_staging "*)
            printf '%s\n' 'FS7 tmpfs cleanup refused: unexpected mount at fixture target.' >&2; return 1 ;;
        esac
    fi
    fs_tmpfs_active=false
}
fs_offline_remove() {
    [ -n "$fs_chain" ] || return 0
    for table in iptables ip6tables; do
        if sudo "$table" -w -C OUTPUT -m owner --uid-owner "$fs_uid" -j "$fs_chain" 2>/dev/null; then
            sudo "$table" -w -D OUTPUT -m owner --uid-owner "$fs_uid" -j "$fs_chain"
        fi
        if sudo "$table" -w -S "$fs_chain" >/dev/null 2>&1; then
            sudo "$table" -w -F "$fs_chain"
            sudo "$table" -w -X "$fs_chain"
        fi
    done
    fs_chain=
}
cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    fs_trace_stop || status=1
    fs_tmpfs_remove || status=1
    fs_offline_remove || status=1
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
    for name in token-copy state-copy credentials.json candidate.toml setup.json setup-header response.json cookies.txt status.json refusal.log home-error.log tls-before tls-after install-build-info.json filter-evidence.json enospc.trace enospc-tracer.log; do
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
revision=1
binding=$(jq -er '.session.binding | select(type == "string" and length > 0)' "$fixture/response.json")
session | jq -e '.setup_required == false and .authenticated == true' >/dev/null
sudo test -s /var/lib/parins-managed/state.json
[ "$(sudo stat -c %a /var/lib/parins-managed/state.json)" = 600 ]
sudo cat /var/lib/parins-managed/state.json > "$fixture/state-copy"

check_dns() {
    http --max-time 5 -H "X-PariNS-Session: $binding" \
        http://127.0.0.1:3000/api/status > "$fixture/status.json"
    jq -e --argjson revision "$revision" '.running == true and .revision == $revision and .last_error == null' "$fixture/status.json" >/dev/null
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

if "$filter_subscriptions"; then
    for command in node iptables ip6tables nsenter mount umount findmnt strace timeout; do
        command -v "$command" >/dev/null 2>&1 || { printf 'Missing: %s\n' "$command" >&2; exit 1; }
    done
    fs_node=$(command -v node)
    fs_nsenter=$(command -v nsenter)
    fs_stat=$(command -v stat)
    "$fs_node" "$repo/scripts/test-filter-subscriptions-systemd.mjs" activate "$fixture"
    revision=$(jq -er '.config_revision' "$fixture/filter-evidence.json")
    sudo cat /var/lib/parins-managed/state.json > "$fixture/state-copy"
    fs_dynamic=$(systemctl show --property=DynamicUser --value parins-managed.service)
    fs_pid=$(systemctl show --property=MainPID --value parins-managed.service)
    printf 'FS7 permissions: DynamicUser=%s MainPID=%s\n' "$fs_dynamic" "$fs_pid"
    [ "$fs_dynamic" = yes ] && [ "$fs_pid" -gt 0 ] || {
        printf '%s\n' 'FS7 permissions: expected DynamicUser=yes and live MainPID.' >&2
        exit 1
    }
    sudo awk '/^(Uid|Gid):/ {print "FS7 process credentials: " $0}' "/proc/$fs_pid/status"
    fs_uid=$(sudo awk '/^Uid:/ {print $2}' "/proc/$fs_pid/status")
    [ "$fs_uid" -gt 0 ] || { printf 'FS7 permissions: expected non-root UID, actual=%s\n' "$fs_uid" >&2; exit 1; }
    fs_hash=$(jq -er '.sha256' "$fixture/filter-evidence.json")
    fs_permission_errors=0
    for relative in filter-subscriptions filter-subscriptions/objects filter-subscriptions/indexes \
        filter-subscriptions/catalog.json "filter-subscriptions/objects/$fs_hash.txt"; do
        case "$relative" in *.json|*.txt) fs_mode=600 ;; *) fs_mode=700 ;; esac
        fs_path="/var/lib/parins-managed/$relative"
        fs_mnt=$(sudo "$fs_nsenter" --target "$fs_pid" --mount -- "$fs_stat" -c '%u:%g:%a:%d:%i' "$fs_path") || fs_mnt=stat_failed
        fs_actual=$(printf '%s\n' "$fs_mnt" | awk -F: '{print $1 ":" $3}')
        printf 'FS7 permissions: %s expected uid:mode=%s:%s actual=%s\n' \
            "$relative" "$fs_uid" "$fs_mode" "$fs_actual"
        if [ "$fs_actual" != "$fs_uid:$fs_mode" ]; then
            printf 'FS7 permissions FAILED: %s expected uid:mode=%s:%s actual=%s\n' \
                "$relative" "$fs_uid" "$fs_mode" "$fs_actual" >&2
            # Failure-only numeric metadata. Distinguish id-mapped backing from
            # the process root without following leaf symlinks or dumping env.
            fs_host=$(sudo "$fs_stat" -c '%u:%g:%a:%d:%i' "$fs_path") || fs_host=stat_failed
            fs_proc=$(sudo "$fs_stat" -c '%u:%g:%a:%d:%i' "/proc/$fs_pid/root$fs_path") || fs_proc=stat_failed
            fs_root=$(sudo "$fs_nsenter" --target "$fs_pid" --mount --root --wd=/ -- "$fs_stat" -c '%u:%g:%a:%d:%i' "$fs_path") || fs_root=stat_failed
            printf 'FS7 stat %s (uid:gid:mode:dev:ino): host=%s mount=%s process_root=%s entered_root=%s\n' \
                "$relative" "$fs_host" "$fs_mnt" "$fs_proc" "$fs_root" >&2
            fs_permission_errors=$((fs_permission_errors + 1))
        fi
    done
    [ "$fs_permission_errors" -eq 0 ] || exit 1
    # Reject only this DynamicUser's external HTTPS. Both families are covered;
    # the already-present rules precede restart. A changed UID fails this fixture
    # instead of incorrectly claiming that the new process started offline.
    fs_chain="PARINS_FS_$$"
    for table in iptables ip6tables; do
        sudo "$table" -w -N "$fs_chain"
        case "$table" in iptables) loopback=127.0.0.0/8 ;; ip6tables) loopback=::1/128 ;; esac
        sudo "$table" -w -A "$fs_chain" ! -d "$loopback" -p tcp --dport 443 -j REJECT
        sudo "$table" -w -I OUTPUT 1 -m owner --uid-owner "$fs_uid" -j "$fs_chain"
        printf 'FS7 offline rule installed: table=%s uid=%s chain=%s external_tcp_port=443\n' "$table" "$fs_uid" "$fs_chain"
    done
    fs_invocation=$(systemctl show --property=InvocationID --value parins-managed.service)
    sudo systemctl restart parins-managed.service
    fs_pid=$(systemctl show --property=MainPID --value parins-managed.service)
    fs_early_uid=$(sudo awk '/^Uid:/ {print $2}' "/proc/$fs_pid/status" 2>/dev/null) || fs_early_uid=unavailable
    printf 'FS7 restart returned: early MainPID=%s UID=%s (diagnostic only; waiting for API readiness)\n' "$fs_pid" "$fs_early_uid"
    # Type=simple reports start before child credentials/exec are complete.
    # Only the actual application's read-only response establishes readiness
    # for the strict identity checks below; the firewall is already in place.
    attempt=0
    until session >/dev/null 2>&1; do
        attempt=$((attempt + 1))
        [ "$attempt" -lt 30 ] || { printf '%s\n' 'FS7 restart FAILED: expected ready session API, actual=timeout.' >&2; exit 1; }
        sleep 1
    done
    fs_new_invocation=$(systemctl show --property=InvocationID --value parins-managed.service)
    fs_pid=$(systemctl show --property=MainPID --value parins-managed.service)
    fs_new_uid=$(sudo awk '/^Uid:/ {print $2}' "/proc/$fs_pid/status") || fs_new_uid=unavailable
    fs_exe=$(sudo readlink "/proc/$fs_pid/exe") || fs_exe=unavailable
    printf 'FS7 ready identity: MainPID=%s old_invocation=%s new_invocation=%s expected_uid=%s actual_uid=%s expected_exe=/opt/parins-managed/parins actual_exe=%s\n' \
        "$fs_pid" "$fs_invocation" "$fs_new_invocation" "$fs_uid" "$fs_new_uid" "$fs_exe"
    [ -n "$fs_new_invocation" ] && [ "$fs_new_invocation" != "$fs_invocation" ] || {
        printf '%s\n' 'FS7 restart FAILED: expected a new nonempty InvocationID.' >&2; exit 1;
    }
    [ "$fs_new_uid" = "$fs_uid" ] || {
        printf 'FS7 offline identity FAILED: expected UID=%s actual=%s; cannot claim offline restart.\n' "$fs_uid" "$fs_new_uid" >&2; exit 1;
    }
    [ "$fs_exe" = /opt/parins-managed/parins ] || {
        printf 'FS7 restart FAILED: expected exe=/opt/parins-managed/parins actual=%s\n' "$fs_exe" >&2; exit 1;
    }
    "$fs_node" "$repo/scripts/test-filter-subscriptions-systemd.mjs" restart "$fixture"
    "$fs_node" "$repo/scripts/test-filter-subscriptions-systemd.mjs" offline "$fixture"
    fs_packets4=$(sudo iptables -w -L "$fs_chain" -nvx | awk '$3 == "REJECT" {sum += $1} END {print sum + 0}')
    fs_packets6=$(sudo ip6tables -w -L "$fs_chain" -nvx | awk '$3 == "REJECT" {sum += $1} END {print sum + 0}')
    printf 'Subscription offline fault: rejected IPv4=%s IPv6=%s HTTPS packets.\n' "$fs_packets4" "$fs_packets6"
    [ "$((fs_packets4 + fs_packets6))" -gt 0 ] || {
        printf '%s\n' 'FS7 offline fault FAILED: expected rejected HTTPS packets >0, actual=0.' >&2; exit 1;
    }
    fs_offline_remove
fi

# Reproduce a crash after durable app intent, before Commit reaches the inbox.
# Only this disposable fixture injects journal state; no candidate is executed
# or downloaded, and the root installed identity remains the actual binary.
old_invocation=$(systemctl show --property=InvocationID --value parins-managed.service)
recovery_invocation=$(systemctl show --property=InvocationID --value parins-update-recovery.service)
sudo systemctl stop parins-updater.path parins-managed.service parins-updater.service
sudo python3 - "$old_invocation" "$revision" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import sys
import time

app = Path('/var/lib/parins-managed')
private = Path('/var/lib/parins-updater/private')
journal_path = private / 'journal.json'
journal = json.loads(journal_path.read_text())
assert journal['operation'] is None and journal['installation_pending'] is None
assert not (app / 'update-request.json').exists()
installed = journal['installed']
build = installed['build']
operation_id, nonce = 'b' * 32, 'c' * 32
stamp = int(time.time() * 1000)
size = Path('/opt/parins-managed/parins').stat().st_size
tag = 'v' + build['version']
# Export the installed schema/contract values rather than guessing a release
# identity. Both artifact descriptors are inert: this fixture only sends Abort.
manifest = {key: build[key] for key in (
    'version', 'source_commit', 'update_protocol', 'install_contract',
    'durable_contract_epoch', 'runtime_database_format',
    'cache_snapshot_format', 'cache_semantics')}
manifest.update(schema=1, repository='paricafe/PariNS', tag=tag,
                min_helper_protocol=build['helper_protocol'], upgrade_mode='in_place',
                artifacts=[dict(target=arch + '-unknown-linux-musl',
                                name=f'parins-{tag}-linux-{arch}.bin',
                                size=size, sha256=installed['sha256'])
                           for arch in ('x86_64', 'aarch64')])
asset = next(a for a in manifest['artifacts'] if a['target'] == build['target'])
download = dict(release_id=1, tag=tag, asset_id=1, asset_name=asset['name'],
                size=size, sha256=installed['sha256'],
                manifest_sha256=hashlib.sha256(json.dumps(manifest).encode()).hexdigest())
status = dict(operation_id=operation_id, phase_nonce=nonce, phase='staged',
              version=build['version'], reason=None, updated_at_ms=stamp,
              downloaded_bytes=size, total_bytes=size)
journal['operation'] = dict(status=status, old=installed, candidate=installed,
                            invocation_id=sys.argv[1], config_revision=int(sys.argv[2]),
                            started_at_ms=stamp, rollback_attempted=False,
                            rollback_started=False, pending_launch=None)
state_path = app / 'update-state.json'
# These are AppState/CheckState's current serialized defaults. A prior checker
# envelope is retained if present; no release lookup is needed for this fixture.
state = json.loads(state_path.read_text()) if state_path.exists() else dict(
    schema=1, check=dict(validated=False, etag=None, last_check_at_ms=None,
                        last_success_at_ms=None, next_check_at_ms=0, retry_at_ms=0,
                        failures=0, error=None, manual=False),
    candidate=None, plan=None, operation=None, commit_intent=None)
state['check']['next_check_at_ms'] = stamp + 3600000
state['plan'] = None
state['commit_intent'] = operation_id
state['operation'] = dict(plan_id='a' * 32, expected_version=build['version'],
                          config_revision=int(sys.argv[2]), operation_id=operation_id, phase_nonce=nonce,
                          invocation_id=sys.argv[1], download=download, manifest=manifest,
                          phase='committing', reason=None, finished=False)

def replace(path, value, owner):
    temporary = path.with_name('.recovery-fixture-' + path.name)
    with temporary.open('x') as output:
        os.fchmod(output.fileno(), 0o600)
        os.fchown(output.fileno(), owner.st_uid, owner.st_gid)
        json.dump(value, output)
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, path)
    fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)

replace(journal_path, journal, journal_path.stat())
replace(state_path, state, (app / 'state.json').stat())
PY
test "$(sudo stat -c '%u:%g:%a' /var/lib/parins-managed/update-state.json)" = "$(sudo stat -c '%u:%g:%a' /var/lib/parins-managed/state.json)"
test "$(sudo stat -c '%u:%a' /var/lib/parins-updater/private/journal.json)" = '0:600'
test "$(sudo stat -c '%u:%a' /var/lib/parins-updater/private)" = '0:700'
sudo /usr/libexec/parins-updater recover
sudo systemctl start parins-managed.service
test "$(systemctl show --property=InvocationID --value parins-managed.service)" != "$old_invocation"
test "$(systemctl show --property=InvocationID --value parins-update-recovery.service)" = "$recovery_invocation"
attempt=0
until sudo test -f /var/lib/parins-managed/update-request.json; do
    attempt=$((attempt + 1))
    [ "$attempt" -lt 15 ] || { printf '%s\n' 'Restart did not request root abort arbitration.' >&2; exit 1; }
    sleep 1
done
sudo jq -e '.operation_id == "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" and .phase_nonce == "cccccccccccccccccccccccccccccccc" and .request.kind == "abort"' /var/lib/parins-managed/update-request.json >/dev/null
sudo jq -e '.commit_intent != null and .operation.finished == false' /var/lib/parins-managed/update-state.json >/dev/null
http --max-time 15 -H 'Origin: http://127.0.0.1:3000' -H 'Content-Type: application/json' \
    --data-binary "@$fixture/credentials.json" http://127.0.0.1:3000/api/login > "$fixture/response.json"
binding=$(jq -er '.session.binding' "$fixture/response.json")
reload_status=$(curl --silent --show-error --noproxy '*' --max-time 5 \
    --cookie "$fixture/cookies.txt" -H 'Origin: http://127.0.0.1:3000' \
    -H "X-PariNS-Session: $binding" -H 'Content-Type: application/json' \
    --data "{\"revision\":$revision}" --output "$fixture/response.json" --write-out '%{http_code}' \
    http://127.0.0.1:3000/api/certificates/reload)
[ "$reload_status" = 409 ]
jq -e '.error.code == "update_in_progress"' "$fixture/response.json" >/dev/null
if "$filter_subscriptions"; then
    "$fs_node" "$repo/scripts/test-filter-subscriptions-systemd.mjs" frozen "$fixture"
fi
sudo systemctl start parins-updater.path
attempt=0
until sudo jq -e '.commit_intent == null and .operation.finished == true and .operation.phase == "aborted"' /var/lib/parins-managed/update-state.json >/dev/null; do
    attempt=$((attempt + 1))
    [ "$attempt" -lt 15 ] || { printf '%s\n' 'Root abort fence did not reconcile interrupted intent.' >&2; exit 1; }
    sleep 1
done
sudo jq -e '.operation.status.operation_id == "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" and .operation.status.phase_nonce == "cccccccccccccccccccccccccccccccc" and .operation.status.phase == "aborted" and .last_operation == .operation.status and .installed == .operation.old' /var/lib/parins-updater/private/journal.json >/dev/null
! sudo test -e /var/lib/parins-managed/update-request.json
http --max-time 5 -H 'Origin: http://127.0.0.1:3000' -H "X-PariNS-Session: $binding" \
    -H 'Content-Type: application/json' --data "{\"revision\":$revision}" \
    http://127.0.0.1:3000/api/certificates/reload | jq -e '.outcome == "unchanged"' >/dev/null
sudo cmp "$binary" /opt/parins-managed/parins
sudo cmp "$fixture/state-copy" /var/lib/parins-managed/state.json
check_dns
if "$filter_subscriptions"; then
    "$fs_node" "$repo/scripts/test-filter-subscriptions-systemd.mjs" unfrozen "$fixture"
    # Mount only an idle, empty staging child inside this running service's
    # namespace. Isolate its exact state mount first when it has shared peers.
    # Catalog, objects and lock stay in place.
    fs_fault_pid=$(systemctl show --property=MainPID --value parins-managed.service)
    fs_fault_invocation=$(systemctl show --property=InvocationID --value parins-managed.service)
    fs_fault_ns=$(sudo readlink "/proc/$fs_fault_pid/ns/mnt")
    [ "$fs_fault_ns" != "$(readlink /proc/self/ns/mnt)" ] || {
        printf '%s\n' 'FS7 ENOSPC refused: service shares the host mount namespace.' >&2; exit 1;
    }
    fs_uid=$(sudo awk '/^Uid:/ {print $2}' "/proc/$fs_fault_pid/status")
    fs_gid=$(sudo awk '/^Gid:/ {print $2}' "/proc/$fs_fault_pid/status")
    [ "$fs_uid" -gt 0 ] && [ "$(sudo readlink "/proc/$fs_fault_pid/exe")" = /opt/parins-managed/parins ] || {
        printf '%s\n' 'FS7 ENOSPC refused: expected live non-root PariNS executable.' >&2; exit 1;
    }
    fs_mount=$(command -v mount)
    fs_umount=$(command -v umount)
    fs_findmnt=$(command -v findmnt)
    fs_strace=$(readlink -f "$(command -v strace)")
    fs_python=$(command -v python3)
    fs_timeout=$(command -v timeout)
    fs_staging=$(sudo "$fs_nsenter" --target "$fs_fault_pid" --mount -- readlink -f /var/lib/parins-managed/filter-subscriptions/staging)
    case "$fs_staging" in /var/lib/parins-managed/filter-subscriptions/staging|/var/lib/private/parins-managed/filter-subscriptions/staging) ;; *) printf '%s\n' 'FS7 ENOSPC refused: unexpected canonical staging path.' >&2; exit 1 ;; esac
    fs_host_staging=$(sudo "$fs_stat" -c '%d:%i' "$fs_staging")
    fs_host_mount=$(sudo "$fs_findmnt" -rn -T "$fs_staging" -o ID,TARGET,PROPAGATION)
    fs_propagation=$(sudo "$fs_nsenter" --target "$fs_fault_pid" --mount -- "$fs_findmnt" -rn -T "$fs_staging" -o PROPAGATION)
    case "$fs_propagation" in shared|shared,slave)
        # shared+slave still forwards mounts to its own peers. --make-private
        # changes this one existing mount, not descendants or the host's mount.
        # Its namespace is destroyed by fixture cleanup; do not rejoin peers.
        fs_state_mount=$(sudo "$fs_nsenter" --target "$fs_fault_pid" --mount -- "$fs_findmnt" -rn -T "$fs_staging" -o TARGET)
        case "$fs_state_mount" in /var/lib/parins-managed|/var/lib/private/parins-managed) ;; *)
            printf 'FS7 ENOSPC refused: containing mount=%s is not the exact service state mount.\n' "$fs_state_mount" >&2; exit 1 ;;
        esac
        [ "$(sudo readlink "/proc/$fs_fault_pid/ns/mnt")" = "$fs_fault_ns" ] || {
            printf '%s\n' 'FS7 ENOSPC refused: service mount namespace changed.' >&2; exit 1;
        }
        sudo "$fs_nsenter" --target "$fs_fault_pid" --mount -- "$fs_mount" --make-private "$fs_state_mount"
        [ "$(sudo "$fs_nsenter" --target "$fs_fault_pid" --mount -- "$fs_findmnt" -rn -T "$fs_staging" -o TARGET,PROPAGATION)" = "$fs_state_mount private" ] && \
            [ "$(sudo "$fs_findmnt" -rn -T "$fs_staging" -o ID,TARGET,PROPAGATION)" = "$fs_host_mount" ] && \
            [ "$(sudo "$fs_stat" -c '%d:%i' "$fs_staging")" = "$fs_host_staging" ] || {
            printf '%s\n' 'FS7 ENOSPC isolation failed: expected private service state mount and unchanged host mount/inode.' >&2; exit 1;
        }
        printf 'FS7 ENOSPC state mount isolated: target=%s, before=%s, after=private, host unchanged.\n' "$fs_state_mount" "$fs_propagation"
        fs_propagation=private
    esac
    case "$fs_propagation" in private|slave) ;; *) printf 'FS7 ENOSPC refused: propagation=%s, expected private or slave.\n' "$fs_propagation" >&2; exit 1 ;; esac
    if sudo "$fs_nsenter" --target "$fs_fault_pid" --mount -- "$fs_findmnt" -rn -M "$fs_staging" >/dev/null; then
        printf '%s\n' 'FS7 ENOSPC refused: staging is already a mount point.' >&2; exit 1
    fi
    sudo "$fs_nsenter" --target "$fs_fault_pid" --mount -- "$fs_python" - "$fs_staging" "$fs_uid" "$fs_gid" <<'PY'
import os, stat, sys
p, uid, gid = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
s = os.lstat(p)
assert stat.S_ISDIR(s.st_mode) and not stat.S_ISLNK(s.st_mode)
assert (s.st_uid, s.st_gid, stat.S_IMODE(s.st_mode)) == (uid, gid, 0o700)
assert os.listdir(p) == [], 'staging must be empty before covering it'
PY
    fs_tmpfs_active=true
    sudo "$fs_nsenter" --target "$fs_fault_pid" --mount -- "$fs_mount" -t tmpfs \
        -o "size=4096,nr_inodes=64,mode=0700,uid=$fs_uid,gid=$fs_gid,nodev,nosuid,noexec" parins-fs7-enospc "$fs_staging"
    [ "$(sudo "$fs_stat" -c '%d:%i' "$fs_staging")" = "$fs_host_staging" ] && \
        [ "$(sudo "$fs_findmnt" -rn -T "$fs_staging" -o ID,TARGET,PROPAGATION)" = "$fs_host_mount" ] || {
        printf '%s\n' 'FS7 ENOSPC failed isolation: host staging mount changed.' >&2; exit 1;
    }
    sudo "$fs_nsenter" --target "$fs_fault_pid" --mount -- "$fs_python" - "$fs_staging" "$fs_uid" "$fs_gid" <<'PY'
import errno, json, os, stat, sys
p, uid, gid = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
s = os.lstat(p)
assert (s.st_uid, s.st_gid, stat.S_IMODE(s.st_mode)) == (uid, gid, 0o700)
os.setgroups([])
os.setgid(gid)
os.setuid(uid)
with open(p + '/fs7-fill', 'xb') as fill:
    os.chmod(fill.fileno(), 0o600)
    fill.write(b'0' * 4096)
    fill.flush()
assert os.statvfs(p).f_bavail == 0
# Plenty of inodes: file creation works; actual data allocation must fail.
fd = os.open(p + '/fs7-probe', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
try:
    try:
        os.write(fd, b'x')
    except OSError as error:
        assert error.errno == errno.ENOSPC
    else:
        raise AssertionError('expected real ENOSPC on data write')
finally:
    os.close(fd)
    os.unlink(p + '/fs7-probe')
print(json.dumps(dict(stage='enospc-mount', tmpfs_bytes=4096, free_blocks=0,
                     uid=uid, gid=gid, empty_file_created=True, probe_errno='ENOSPC')))
PY
    [ "$(sudo awk '/^TracerPid:/ {print $2}' "/proc/$fs_fault_pid/status")" = 0 ] || {
        printf '%s\n' 'FS7 ENOSPC refused: service already has a tracer.' >&2; exit 1;
    }
    # Failed write calls only; zero string bytes. Raw trace stays private and is
    # removed by cleanup, never placed in the uploaded artifact directory.
    sudo -n "$fs_timeout" -s INT -k 5 330 "$fs_strace" -f -yy -s 0 -e trace=write -e status=failed \
        -o "$fixture/enospc.trace" -p "$fs_fault_pid" 2>"$fixture/enospc-tracer.log" &
    fs_trace_job=$!
    attempt=0
    while :; do
        # -f attaches existing Tokio threads as well as future children. Do not
        # start the download while only the main thread is attached.
        fs_tracer_pid=$(sudo "$fs_python" - "$fs_fault_pid" <<'PY'
import pathlib, sys
tracers = set()
for task in pathlib.Path('/proc/' + sys.argv[1] + '/task').iterdir():
    try:
        lines = (task / 'status').read_text().splitlines()
    except FileNotFoundError:
        continue
    tracers.add(int(next(line.split()[1] for line in lines if line.startswith('TracerPid:'))))
print(next(iter(tracers)) if len(tracers) == 1 else 0)
PY
        )
        [ "$fs_tracer_pid" = 0 ] || break
        fs_tracer_pid=
        attempt=$((attempt + 1))
        [ "$attempt" -lt 10 ] || { printf '%s\n' 'FS7 ENOSPC tracer did not attach.' >&2; exit 1; }
        sleep 1
    done
    [ "$(sudo readlink "/proc/$fs_tracer_pid/exe")" = "$fs_strace" ] || {
        printf '%s\n' 'FS7 ENOSPC tracer identity mismatch.' >&2; exit 1;
    }
    "$fs_node" "$repo/scripts/test-filter-subscriptions-systemd.mjs" enospc "$fixture"
    fs_trace_stop
    # The operation is terminal and its writer drained. No lazy unmount; remove
    # the cross-filesystem staging overlay before the single recovery prepare.
    fs_tmpfs_remove
    fs_fault_identity() {
        [ "$(systemctl show --property=MainPID --value parins-managed.service)" = "$fs_fault_pid" ] && \
            [ "$(systemctl show --property=InvocationID --value parins-managed.service)" = "$fs_fault_invocation" ] && \
            [ "$(sudo awk '/^Uid:/ {print $2}' "/proc/$fs_fault_pid/status")" = "$fs_uid" ] && \
            [ "$(sudo readlink "/proc/$fs_fault_pid/exe")" = /opt/parins-managed/parins ] || {
            printf '%s\n' 'FS7 ENOSPC recovery refused: service identity changed.' >&2; return 1;
        }
    }
    fs_fault_identity
    sudo "$fs_nsenter" --target "$fs_fault_pid" --mount -- "$fs_python" - "$fs_staging" "$fs_host_staging" <<'PY'
import os, sys
p = sys.argv[1]
s = os.lstat(p)
assert f'{s.st_dev}:{s.st_ino}' == sys.argv[2], 'original staging must be restored'
assert os.listdir(p) == [], 'original staging remains empty'
PY
    "$fs_node" "$repo/scripts/test-filter-subscriptions-systemd.mjs" recovered "$fixture"
    fs_fault_identity
    printf '%s\n' 'FS7 download, activation, DNS, DynamicUser, offline LKG, UP freeze and actual staging-write ENOSPC/recovery passed. Performance remains a separate gate.'
fi
printf '%s\n' 'Linux systemd ownership, setup, UDP/TCP DNS, state-preserving install, preflight rejection and interrupted-intent Abort/fence recovery passed.'
