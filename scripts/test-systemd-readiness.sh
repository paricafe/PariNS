#!/bin/sh
# Unprivileged checks of the real service fixture's read-only readiness gate.
# No systemd, service, network or ephemeral-runner guard is invoked here.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
command -v jq >/dev/null 2>&1 || { printf '%s\n' 'Missing: jq' >&2; exit 1; }
readiness_fixture=$(mktemp -d "${TMPDIR:-/tmp}/parins-readiness-test.XXXXXX")
trap 'rm -rf "$readiness_fixture"' EXIT HUP INT TERM
sed -n '/^wait_dns_ready() {/,/^}$/p' "$repo/scripts/test-systemd.sh" > "$readiness_fixture/function.sh"
test -s "$readiness_fixture/function.sh"
for readiness_case in transition wrong-revision transport invalid-json starting degraded generation failed deadline missing-fields official-a; do
    mkdir "$readiness_fixture/$readiness_case"
    readiness_status=0
    sh -eu -c '
        . "$1/function.sh"
        fixture=$1/$2
        scenario=$2
        binding=private-binding-sentinel
        revision=1
        official_updater_check=false
        [ "$scenario" != official-a ] || official_updater_check=true
        http() {
            calls=0
            [ ! -f "$fixture/calls" ] || calls=$(cat "$fixture/calls")
            calls=$((calls + 1))
            printf "%s\n" "$calls" > "$fixture/calls"
            case "$scenario" in
                transport) return 77 ;;
                invalid-json) printf "invalid\n"; return 0 ;;
                missing-fields|official-a) printf "{\"running\":true,\"revision\":1,\"last_error\":null}\n"; return 0 ;;
            esac
            phase=stable ready=true health=running generation=2 health_generation=2 saved=1 error=null
            case "$scenario:$calls" in
                transition:1|starting:*) phase=starting ;;
                transition:2|degraded:*) ready=false health=degraded ;;
                transition:3|generation:*) health_generation=1 ;;
                wrong-revision:*) saved=2 ;;
                failed:*|deadline:*) ready=false health=failed error="\"private-error-sentinel\"" ;;
            esac
            printf "{\"running\":true,\"lifecycle_phase\":\"%s\",\"revision\":%s,\"generation\":%s,\"last_error\":%s,\"dns_health\":{\"ready\":%s,\"state\":\"%s\",\"generation\":%s},\"session\":\"private-binding-sentinel\"}\n" \
                "$phase" "$saved" "$generation" "$error" "$ready" "$health" "$health_generation"
        }
        sleep() { :; }
        date() {
            if [ "$scenario" = deadline ]; then
                if [ -f "$fixture/clock" ]; then printf "31\n"; else : > "$fixture/clock"; printf "0\n"; fi
            else command date "$@"; fi
        }
        wait_dns_ready
    ' sh "$readiness_fixture" "$readiness_case" > "$readiness_fixture/$readiness_case/output" 2>&1 || readiness_status=$?
    case "$readiness_case" in
        transition) expected_status=0 expected_calls=4 ;;
        official-a) expected_status=0 expected_calls=1 ;;
        transport) expected_status=77 expected_calls=1 ;;
        wrong-revision|invalid-json|deadline) expected_status=1 expected_calls=1 ;;
        *) expected_status=1 expected_calls=30 ;;
    esac
    [ "$readiness_status" -eq "$expected_status" ] && \
        [ "$(cat "$readiness_fixture/$readiness_case/calls")" -eq "$expected_calls" ] || {
        printf 'Readiness case %s failed: status %s, calls %s.\n' "$readiness_case" "$readiness_status" \
            "$(cat "$readiness_fixture/$readiness_case/calls")" >&2
        exit 1
    }
    ! grep -q 'private-.*-sentinel' "$readiness_fixture/$readiness_case/output"
done
# The real cleanup removes the private wait snapshots, but an unexpected file
# must still fail the fixture instead of broadening removal to the directory.
sed -n '/^cleanup() {/,/^}$/p' "$repo/scripts/test-systemd.sh" > "$readiness_fixture/cleanup.sh"
for cleanup_case in known unknown; do
    cleanup_dir="$readiness_fixture/cleanup-$cleanup_case"
    mkdir "$cleanup_dir"
    : > "$cleanup_dir/wait-journal.json"
    : > "$cleanup_dir/wait-status.json"
    [ "$cleanup_case" != unknown ] || : > "$cleanup_dir/unexpected"
    cleanup_status=0
    sh -eu -c '
        . "$1/cleanup.sh"
        fixture=$2 installed=false opt_mode=755 acceptance_stage=fixture-cleanup
        fs_trace_stop() { :; }
        fs_tmpfs_remove() { :; }
        fs_offline_remove() { :; }
        sudo() { [ "$*" = "chmod 755 /opt" ]; }
        cleanup
    ' sh "$readiness_fixture" "$cleanup_dir" > "$readiness_fixture/cleanup-$cleanup_case.log" 2>&1 || cleanup_status=$?
    if [ "$cleanup_case" = known ]; then
        [ "$cleanup_status" -eq 0 ] && [ ! -e "$cleanup_dir" ]
    else
        [ "$cleanup_status" -eq 1 ] && [ -f "$cleanup_dir/unexpected" ]
        [ ! -e "$cleanup_dir/wait-journal.json" ] && [ ! -e "$cleanup_dir/wait-status.json" ]
    fi
done
printf '%s\n' 'Service readiness fixture checks passed (11 readiness, 2 cleanup cases).'
