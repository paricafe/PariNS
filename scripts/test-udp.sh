#!/bin/sh
# Source-address acceptance only inside a newly created, disposable Linux netns.
set -eu

if [ "$#" -ne 1 ] || [ "$1" != "--ephemeral-ci" ]; then
    echo "usage: $0 --ephemeral-ci (disposable Linux CI only)" >&2
    exit 2
fi
[ "$(uname -s)" = Linux ] || { echo "Linux required" >&2; exit 2; }
[ "${GITHUB_ACTIONS:-}" = true ] && \
    [ "${RUNNER_ENVIRONMENT:-}" = github-hosted ] && \
    [ "${RUNNER_OS:-}" = Linux ] || {
    echo "Requires a disposable GitHub-hosted Linux runner" >&2
    exit 2
}
for tool in cargo python3 ip unshare sudo; do
    command -v "$tool" >/dev/null || { echo "missing $tool" >&2; exit 2; }
done

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"
# Build as the invoking user, outside sudo and before disconnecting networking.
build_output=$(mktemp "${TMPDIR:-/tmp}/parins-udp-build.XXXXXX")
trap 'rm -f "$build_output"' EXIT
trap 'exit 1' HUP INT TERM
cargo test --locked --test server --no-run --message-format=json > "$build_output"
binary=$(python3 -c '
import json, sys
paths = [message["executable"] for line in sys.stdin
         if (message := json.loads(line)).get("reason") == "compiler-artifact"
         and message.get("target", {}).get("name") == "server"
         and message.get("executable")]
if len(paths) != 1:
    raise SystemExit("expected exactly one server test executable")
print(paths[0])
' < "$build_output")
parent_namespace=$(readlink /proc/self/ns/net)

sudo unshare --net -- sh -eu -c '
    # Verify isolation before the first networking mutation. No host address,
    # firewall, DNS, trust or service is changed; namespace exit removes aliases.
    [ "$(readlink /proc/self/ns/net)" != "$1" ] || exit 2
    ip link set lo up
    ip address add 192.0.2.1/24 dev lo
    ip address add 192.0.2.2/24 dev lo
    ip -6 address add fd00:7061::1/64 dev lo nodad
    ip -6 address add fd00:7061::2/64 dev lo nodad
    ip -6 address add fe80::1/64 dev lo nodad
    ip -6 address add fe80::2/64 dev lo nodad
    index=$(cat /sys/class/net/lo/ifindex)
    export PARINS_UDP_TEST_IPV4=127.0.0.1,127.0.0.2,192.0.2.1,192.0.2.2
    export PARINS_UDP_TEST_IPV6=::1,fd00:7061::1,fd00:7061::2,fe80::2%$index
    exec "$2" wildcard --test-threads=1 --nocapture
' parins-udp-netns "$parent_namespace" "$binary"
