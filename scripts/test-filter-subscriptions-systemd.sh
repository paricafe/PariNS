#!/bin/sh
# Opt-in external HTTPS subscription acceptance on a fresh disposable runner.
# The installation/Abort fixture owns cleanup and all system mutations.
set -eu
[ "${1:-}" = --ephemeral-ci ] && [ "${GITHUB_ACTIONS:-}" = true ] && \
    [ "${RUNNER_ENVIRONMENT:-}" = github-hosted ] && \
    [ "${RUNNER_OS:-}" = Linux ] && [ "$(uname -s)" = Linux ] || {
    printf '%s\n' 'Refusing: requires --ephemeral-ci on a disposable GitHub Linux runner.' >&2
    exit 1
}
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
for command in node iptables ip6tables; do
    command -v "$command" >/dev/null 2>&1 || { printf 'Missing: %s\n' "$command" >&2; exit 1; }
done
exec sh "$repo/scripts/test-systemd.sh" "$@" --filter-subscriptions
