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
    local forwarded=${5:-'export TEST_SECRET="secret value"'} linger=${6:-0}
    local dir="$TMP/work/$token" deadline=$(( $(date +%s) + 120 ))
    mkdir -p "$dir"
    GPU_RUN_CONFIG=$(printf 'JOBDIR=%q\nOUTPUTS=outputs\nSETUP=%q\nCOMMAND=%q\nDEADLINE=%s\nJOB_DEADLINE=%s\nLINGER=%s\nFORWARDED=%q\n' "$dir" "$setup" "$command" "$deadline" "$(( $(date +%s) + limit ))" "$linger" "$forwarded")
    if [ -n "${7:-}" ]; then
        GPU_RUN_CONFIG+=$'\n'"WAIT_FOR_STARTUP=$7"
    fi
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

status_of() { env GPU_RUN_LIBRARY=0 bash "$SOURCE" status "$TMP/state/$1" "$1"; }

wait_startup_log() {
    local token=$1 count=0
    while [ "$count" -lt 300 ]; do
        check test "$(status_of "$token" | cut -f2)" = -
        if grep -Fxq 'Waiting for instance startup to finish.' "$TMP/state/$token/log"; then
            check test "$(status_of "$token" | cut -f2)" = -
            return
        fi
        sleep 0.1
        count=$((count+1))
    done
    echo "startup wait was not logged within 30 seconds: $token" >&2; exit 1
}

wait_hook() {
    local token=$1 count=0
    while [ "$count" -lt 300 ]; do
        [ ! -s "$TMP/hook-$token" ] || return 0
        sleep 0.1
        count=$((count+1))
    done
    echo "expiry hook was not written within 30 seconds: $token" >&2; exit 1
}

# A backend without onstart can run setup and the job without the Vast marker.
launch nogate 'cat setup-seen; echo ran' 'echo setup >setup-seen' 60 '' 0 0
status=$(wait_done nogate)
check test ! -e "$HOME/.works"
check test "$(printf '%s' "$status" | cut -f2)" = 0
check test "$(cat "$TMP/state/nogate/log")" = $'setup\nran'
passed=$((passed+1))

# Setup waits for onstart (uv install) to finish, bounded by the job deadline.
launch notready 'echo ran'
wait_startup_log notready
# A one-second wall-clock deadline can kill the task before it even logs the
# startup wait. Freeze this run's clock until that observable handshake, then
# expire it explicitly. The clock is inherited only by this launch's processes.
mkdir -p "$TMP/clock-bin"
cat >"$TMP/clock-bin/date" <<'CLOCK'
#!/usr/bin/env bash
[ "$#" = 1 ] && [ "$1" = +%s ] || exit 1
cat "$GPU_RUN_TEST_CLOCK"
CLOCK
chmod +x "$TMP/clock-bin/date"
printf '2000000000\n' >"$TMP/clock"
PATH="$TMP/clock-bin:$PATH" GPU_RUN_TEST_CLOCK="$TMP/clock" launch neverready 'echo ran'
wait_startup_log neverready
printf '2000000061\n' >"$TMP/clock.next"
mv "$TMP/clock.next" "$TMP/clock"
status=$(wait_done neverready)
check test "$(printf '%s' "$status" | cut -f1)" = setup
check test "$(printf '%s' "$status" | cut -f2)" = 17
check test "$(printf '%s' "$status" | cut -f3)" = 1
check test "$(cat "$TMP/state/neverready/log")" = 'Waiting for instance startup to finish.'
touch "$HOME/.works"
status=$(wait_done notready)
check test "$(printf '%s' "$status" | cut -f2)" = 0
check test "$(tail -n 1 "$TMP/state/notready/log")" = ran
passed=$((passed+2))

launch success 'printf "%s\n" "$TEST_SECRET"; printf result >"$GPU_RUN_OUTPUTS/result"'
status=$(wait_done success)
check test "$(printf '%s' "$status" | cut -f2)" = 0
check test "$(cat "$TMP/state/success/log")" = 'secret value'
check test "$(cat "$TMP/work/success/outputs/result")" = result
# A live attachment retains the original download ceiling. Once its heartbeat
# goes stale, default zero linger requests immediate expiry (a few seconds for
# the expiry worker's readiness handshake), not an implicit 30-minute hold.
check test ! -f "$TMP/hook-success"
printf '0\n' >"$TMP/state/success/attached"
wait_hook success
hook=$(cat "$TMP/hook-success")
check test "$hook" -le "$(( $(date +%s) + 5 ))"
passed=$((passed+1))

# Explicit retention is still available; it never extends the original ceiling.
launch retained 'echo retained' '' 60 'export TEST_SECRET="secret value"' 30
status=$(wait_done retained)
printf '0\n' >"$TMP/state/retained/attached"
wait_hook retained
hook=$(cat "$TMP/hook-retained")
check test "$hook" -gt "$(( $(date +%s) + 10 ))"
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

# Forwarded values reach setup and the job via the environment, never argv.
scan='found=0; for f in /proc/[0-9]*/cmdline; do while IFS= read -r -d "" a; do case $a in *"$TEST_SECRET"*) found=1;; esac; done 2>/dev/null <"$f"; done; printf "%s:%s" "$found" "$TEST_SECRET"'
launch argv "$scan >\"\$GPU_RUN_OUTPUTS/job\"" "$scan >\"\$GPU_RUN_OUTPUTS/setup\""
status=$(wait_done argv)
check test "$(printf '%s' "$status" | cut -f2)" = 0
check test "$(cat "$TMP/work/argv/outputs/setup")" = '0:secret value'
check test "$(cat "$TMP/work/argv/outputs/job")" = '0:secret value'
passed=$((passed+1))

# Forwarded names that match runner internals do not change the runner.
launch overlap 'printf "%s" "$COMMAND" >"$GPU_RUN_OUTPUTS/seen"' 'printf "%s" "$SETUP" >setup-seen' 60 \
    "export COMMAND=clobbered SETUP=clobbered JOBDIR=/nonexistent ROOT=/nonexistent TOKEN=bad FORWARDED=bad"
status=$(wait_done overlap)
check test "$(printf '%s' "$status" | cut -f1)" = job
check test "$(printf '%s' "$status" | cut -f2)" = 0
check test "$(cat "$TMP/work/overlap/outputs/seen")" = clobbered
check test "$(cat "$TMP/work/overlap/setup-seen")" = clobbered
passed=$((passed+1))

# A second start with the same run ID must not launch duplicate work.
if { printf 'GPU_RUN_SOURCE=%q\nGPU_RUN_CONFIG=%q\nGPU_RUN_HOOK=%q\n' "$GPU_RUN_SOURCE" "$GPU_RUN_CONFIG" "$GPU_RUN_HOOK"; cat "$SOURCE"; } |
    env GPU_RUN_LIBRARY=0 bash -s -- start "$TMP/state/success" success >/dev/null 2>&1; then
    echo 'duplicate start accepted' >&2; exit 1
fi
passed=$((passed+1))
printf '%s passed, 0 failed\n' "$passed"
