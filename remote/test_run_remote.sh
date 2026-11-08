#!/usr/bin/env bash
# Fake-only Linux worker tests. No cloud command or endpoint is involved.
set -eu
SOURCE=$(cd "$(dirname "$0")" && pwd)/run_remote.sh
TMP=$(mktemp -d)
export HOME="$TMP/home"
mkdir -p "$HOME"
cleanup() {
    for state in "$TMP"/state/*; do
        [ -f "$state/pid" ] || continue
        read -r pid <"$state/pid"
        [ -r "/proc/$pid/cmdline" ] || continue
        if tr '\0' '\n' <"/proc/$pid/cmdline" | grep -Fxq "$state"; then
            kill -TERM "$pid" 2>/dev/null || true
        fi
    done
    rm -rf "$TMP"
}
trap cleanup EXIT
export GPU_RUN_LIBRARY=1
GPU_RUN_SOURCE=$(<"$SOURCE")
source "$SOURCE"
passed=0
check() { if ! "$@"; then echo "FAIL: $*" >&2; exit 1; fi; }

launch() {
    local token=$1 command=$2 setup=${3:-} limit=${4:-60}
    local dir="$TMP/work/$token" deadline=$(( $(date +%s) + 120 ))
    mkdir -p "$dir"
    GPU_RUN_CONFIG=$(printf 'JOBDIR=%q\nOUTPUTS=outputs\nSETUP=%q\nCOMMAND=%q\nDEADLINE=%s\nJOB_DEADLINE=%s\nLINGER=30\nFORWARDED=%q\n' "$dir" "$setup" "$command" "$deadline" "$(( $(date +%s) + limit ))" 'export TEST_SECRET="secret value"')
    GPU_RUN_HOOK=$(printf 'printf "%%s" "$1" >%q\n' "$TMP/hook-$token")
    # Feed a separate bash, just like SSH stdin; it exits immediately after start.
    { printf 'GPU_RUN_SOURCE=%q\nGPU_RUN_CONFIG=%q\nGPU_RUN_HOOK=%q\n' "$GPU_RUN_SOURCE" "$GPU_RUN_CONFIG" "$GPU_RUN_HOOK"; cat "$SOURCE"; } |
        env GPU_RUN_LIBRARY=0 bash -s -- start "$TMP/state/$token" "$token" >/dev/null
}
wait_done() {
    local token=$1 count=0 text=''
    while [ "$count" -lt 150 ]; do
        text=$(env GPU_RUN_LIBRARY=0 bash "$SOURCE" status "$TMP/state/$token" "$token")
        if [ "$(printf '%s' "$text" | cut -f2)" != - ]; then printf '%s' "$text"; return; fi
        sleep 0.1
        count=$((count+1))
    done
    echo "did not finish: $token" >&2; exit 1
}

launch success 'printf "%s\n" "$TEST_SECRET"; printf result >"$GPU_RUN_OUTPUTS/result"'
status=$(wait_done success)
check test "$(printf '%s' "$status" | cut -f2)" = 0
check test "$(cat "$TMP/state/success/log")" = 'secret value'
check test "$(cat "$TMP/work/success/outputs/result")" = result
# The local SSH-like process is already gone; linger still runs remotely.
for i in $(seq 1 50); do [ -f "$TMP/hook-success" ] && break; sleep 0.1; done
check test -f "$TMP/hook-success"
hook=$(cat "$TMP/hook-success")
check test "$hook" -le "$(( $(date +%s) + 30 ))"
passed=$((passed+1))

launch setupfailure 'echo must-not-run' 'echo setupfailed; exit 7'
status=$(wait_done setupfailure)
check test "$(printf '%s' "$status" | cut -f1)" = setup
check test "$(printf '%s' "$status" | cut -f2)" = 7
check test "$(cat "$TMP/state/setupfailure/log")" = setupfailed
passed=$((passed+1))

launch jobfailure 'echo partial >"$GPU_RUN_OUTPUTS/checkpoint"; exit 9'
status=$(wait_done jobfailure)
check test "$(printf '%s' "$status" | cut -f2)" = 9
check test -f "$TMP/work/jobfailure/outputs/checkpoint"
passed=$((passed+1))

launch timeout 'echo started; sleep 120' '' 1
status=$(wait_done timeout)
check test "$(printf '%s' "$status" | cut -f2)" = 17
check test "$(printf '%s' "$status" | cut -f3)" = 1
passed=$((passed+1))

launch cancelled 'echo before; sleep 120'
env GPU_RUN_LIBRARY=0 bash "$SOURCE" stop "$TMP/state/cancelled" cancelled
status=$(wait_done cancelled)
check test "$(printf '%s' "$status" | cut -f2)" != 0
passed=$((passed+1))

# A second start with the same run ID must not launch duplicate work.
if { printf 'GPU_RUN_SOURCE=%q\nGPU_RUN_CONFIG=%q\nGPU_RUN_HOOK=%q\n' "$GPU_RUN_SOURCE" "$GPU_RUN_CONFIG" "$GPU_RUN_HOOK"; cat "$SOURCE"; } |
    env GPU_RUN_LIBRARY=0 bash -s -- start "$TMP/state/success" success >/dev/null 2>&1; then
    echo 'duplicate start accepted' >&2; exit 1
fi
passed=$((passed+1))
printf '%s passed, 0 failed\n' "$passed"
