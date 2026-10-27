#!/usr/bin/env bash
# Tests for expiry_remote.sh. Linux only (flock, setsid, /proc); run by
# `cargo test` through tests/remote_worker.rs, or directly with bash. No test
# reaches the network: `curl` is always a local stub.

set -u
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPT=$HERE/expiry_remote.sh
SCRATCH=$(mktemp -d)
trap 'rm -rf "$SCRATCH"' EXIT
passed=0
failed=0

check() {
    # check DESCRIPTION COMMAND...: fail the current test unless COMMAND succeeds.
    local what=$1
    shift
    if ! "$@"; then
        echo "    check failed: $what" >&2
        exit 1
    fi
}

refute() {
    # refute DESCRIPTION COMMAND...: fail the current test if COMMAND succeeds.
    local what=$1
    shift
    if "$@"; then
        echo "    check failed: $what" >&2
        exit 1
    fi
}

equals() {
    if [ "$1" != "$2" ]; then
        printf '    expected: %s\n    actual:   %s\n' "$2" "$1" >&2
        exit 1
    fi
}

# Each test runs in a subshell with the script loaded as a library, a fresh
# state directory and HOME, and its own stub directory first on PATH.
run_test() {
    local name=$1 dir
    dir=$(mktemp -d "$SCRATCH/$name.XXXX")
    if (
        export HOME=$dir/home
        mkdir -p "$HOME" "$dir/bin"
        export PATH=$dir/bin:$PATH
        unset CONTAINER_ID CONTAINER_API_KEY
        GPU_EXPIRE_LIBRARY=1
        # shellcheck source=expiry_remote.sh
        . "$SCRIPT"
        ROOT=$dir/state
        WORKER_HOME=$HOME
        PROC1_ENVIRON=$dir/proc1-environ
        ETC_ENVIRONMENT=$dir/etc-environment
        mkdir -p "$ROOT"
        chmod 700 "$ROOT"
        TEST_DIR=$dir
        "$name"
    ); then
        echo "ok   $name"
        passed=$((passed + 1))
    else
        echo "FAIL $name"
        failed=$((failed + 1))
    fi
}

# The Python tests' `timer` fixture: scoped credentials, a live worker.
fake_timer() {
    credentials() { KEY=scoped-key; }
    running() { return 0; }
    start_worker() { printf 456; }
    GPU_EXPIRE_SOURCE='# fake runner'
}

arm() {
    control arm 123 "${1:-$(($(date +%s) + 7200))}"
}

state_field() {
    sed -n "s/^$1=//p" "$ROOT/state"
}

test_arm_show_replace_cancel() {
    fake_timer
    equals "$(control show 123)" '{"status": "none"}'
    local deadline=$(($(date +%s) + 7200)) old new
    equals "$(arm "$deadline")" "{\"status\": \"armed\", \"deadline\": $deadline}"
    old=$(state_field token)
    refute "no key in state" grep -q scoped-key "$ROOT/state"
    equals "$(stat -c %a "$ROOT/state")" 600
    equals "$(control show 123)" "{\"status\": \"armed\", \"deadline\": $deadline}"
    equals "$(arm $((deadline + 3600)))" "{\"status\": \"armed\", \"deadline\": $((deadline + 3600))}"
    new=$(state_field token)
    check "token replaced" [ "$new" != "$old" ]
    destroy() { touch "$ROOT/destroyed"; return 1; }
    equals "$(tick "$old" 123 scoped-key $((deadline + 9999)))" exit
    equals "$(control cancel 123)" '{"status": "cancelled"}'
    equals "$(tick "$new" 123 scoped-key $((deadline + 9999)))" exit
    equals "$(control cancel 123)" '{"status": "none"}'
    check "superseded timers never destroy" [ ! -e "$ROOT/destroyed" ]
}

test_tick_only_destroys_at_deadline_and_retries() {
    fake_timer
    arm >/dev/null
    local token deadline
    token=$(state_field token)
    deadline=$(state_field deadline)
    # A failed request, a refused one, then an accepted one.
    destroy() {
        echo "$*" >>"$ROOT/destroy-calls"
        case $(wc -l <"$ROOT/destroy-calls") in
            1 | 2) return 1 ;;
            *) return 0 ;;
        esac
    }
    equals "$(tick "$token" 123 scoped-key $((deadline - 1)))" 1
    check "no destroy before the deadline" [ ! -e "$ROOT/destroy-calls" ]
    for _ in 1 2; do
        equals "$(tick "$token" 123 scoped-key "$deadline")" 30
        equals "$(state_field status)" retrying
    done
    equals "$(tick "$token" 123 scoped-key "$deadline")" exit
    equals "$(state_field status)" destroy-request-accepted
    equals "$(tail -n 1 "$ROOT/destroy-calls")" "123 scoped-key"
    refute "no key in state" grep -q scoped-key "$ROOT/state"
}

test_failed_rearm_preserves_existing_deadline() {
    fake_timer
    arm >/dev/null
    local original
    original=$(cat "$ROOT/state")
    start_worker() { return 1; }
    equals "$(arm)" '{"error": "verification"}'
    equals "$(cat "$ROOT/state")" "$original"
    equals "$(arm $(($(date +%s) - 10)))" '{"error": "past"}'
    equals "$(cat "$ROOT/state")" "$original"
}

test_post_arm_verification_failure_rolls_back() {
    for existing in false true; do
        rm -f "$ROOT/state"
        (
            fake_timer
            if [ "$existing" = true ]; then
                arm >/dev/null
            fi
            local original=''
            [ -e "$ROOT/state" ] && original=$(cat "$ROOT/state")
            running() { return 1; }
            equals "$(arm)" '{"error": "verification"}'
            if [ "$existing" = true ]; then
                equals "$(cat "$ROOT/state")" "$original"
            else
                check "no state left behind" [ ! -e "$ROOT/state" ]
            fi
        ) || exit 1
    done
}

test_cancel_serializes_with_inflight_destroy() {
    fake_timer
    arm >/dev/null
    local token deadline cancel_pid
    token=$(state_field token)
    deadline=$(state_field deadline)
    destroy() {
        touch "$ROOT/started"
        while [ ! -e "$ROOT/release" ]; do sleep 0.01; done
        return 1
    }
    tick "$token" 123 scoped "$deadline" >"$TEST_DIR/tick.out" &
    local tick_pid=$!
    for _ in $(seq 500); do [ -e "$ROOT/started" ] && break; sleep 0.01; done
    check "destroy started" [ -e "$ROOT/started" ]
    control cancel 123 >"$TEST_DIR/cancel.out" &
    cancel_pid=$!
    sleep 0.05
    check "cancel waits for the in-flight destroy" [ ! -s "$TEST_DIR/cancel.out" ]
    touch "$ROOT/release"
    wait "$tick_pid"
    wait "$cancel_pid"
    equals "$(cat "$TEST_DIR/tick.out")" 30
    equals "$(cat "$TEST_DIR/cancel.out")" '{"status": "cancelled"}'
    check "state removed" [ ! -e "$ROOT/state" ]
}

test_show_detects_dead_worker() {
    fake_timer
    arm >/dev/null
    running() { return 1; }
    case $(control show 123) in
        '{"status": "inactive", '*) ;;
        *) exit 1 ;;
    esac
}

test_worker_is_detached_and_startup_verified() {
    patched_script >"$ROOT/runner.sh"
    export CONTAINER_ID=123 CONTAINER_API_KEY=scoped-key VAST_API_KEY=personal-key-must-never-be-used
    (
        # Hold the lock as `control` does, so the worker waits on its first tick.
        lock
        pid=$(start_worker generation 123 scoped-key) || exit 1
        tr '\0' ' ' <"/proc/$pid/cmdline" >"$TEST_DIR/cmdline"
        check "worker argv" grep -q "runner.sh worker $ROOT generation 123" "$TEST_DIR/cmdline"
        refute "no key in argv" grep -q scoped-key "$TEST_DIR/cmdline"
        own_session=$(awk '{print $6}' /proc/$$/stat)
        worker_session=$(awk '{print $6}' "/proc/$pid/stat")
        check "session leader" [ "$worker_session" = "$pid" ]
        check "own session" [ "$worker_session" != "$own_session" ]
        for fd in 0 1 2; do
            equals "$(readlink "/proc/$pid/fd/$fd")" /dev/null
        done
        tr '\0' '\n' <"/proc/$pid/environ" >"$TEST_DIR/environ"
        check "scoped key passed" grep -qx CONTAINER_API_KEY=scoped-key "$TEST_DIR/environ"
        refute "no account key" grep -q VAST_API_KEY "$TEST_DIR/environ"
        echo "$pid" >"$TEST_DIR/worker.pid"
    ) || exit 1
    # Without state the released worker exits on its first tick.
    pid=$(cat "$TEST_DIR/worker.pid")
    for _ in $(seq 200); do [ -e "/proc/$pid" ] || break; sleep 0.01; done
    check "worker exited" [ ! -e "/proc/$pid" ]
    # A worker that never reports ready fails verification.
    READY_TRIES=3
    printf 'exit 0\n' >"$ROOT/runner.sh"
    refute "startup verification" start_worker generation 123 scoped-key
}

test_process_identity_check() {
    bash -c 'while :; do sleep 0.1; done' bash runner.sh worker state token >/dev/null 2>&1 &
    local pid=$!
    sleep 0.1
    check "matching token" running "$pid" token
    refute "other token" running "$pid" another-token
    kill "$pid"
}

test_scoped_credentials_sources() {
    export VAST_API_KEY=personal-key-must-never-be-used
    for environment in container vm pid1; do
        (
            case $environment in
                container) export CONTAINER_ID=123 CONTAINER_API_KEY=scoped ;;
                vm) printf 'CONTAINER_ID="123"\nCONTAINER_API_KEY="scoped"\n' >"$ETC_ENVIRONMENT" ;;
                pid1) printf 'CONTAINER_ID=123\0CONTAINER_API_KEY=scoped\0' >"$PROC1_ENVIRON" ;;
            esac
            credentials 123
            equals "$KEY" scoped
            equals "$( (credentials 456) )" '{"error": "credentials"}'
        ) || exit 1
        rm -f "$ETC_ENVIRONMENT" "$PROC1_ENVIRON"
    done
}

test_missing_scoped_credentials_never_falls_back_to_account() {
    export VAST_API_KEY=personal-key
    : >"$ETC_ENVIRONMENT"
    : >"$PROC1_ENVIRON"
    equals "$( (credentials 123) )" '{"error": "credentials"}'
}

# A curl stub that logs its arguments and stdin and answers $1 (default: accepted).
fake_curl() {
    local response=${1:-}
    [ -n "$response" ] || response='{"success": true}'
    printf '%s' "$response" >"$TEST_DIR/curl-response"
    cat >"$TEST_DIR/bin/curl" <<EOF
#!/bin/sh
printf '%s\n' "\$@" >>"$TEST_DIR/curl-args"
cat >>"$TEST_DIR/curl-config"
cat "$TEST_DIR/curl-response"
EOF
    chmod 755 "$TEST_DIR/bin/curl"
}

# The script with its worker PATH pointed at the stubs and its endpoint at a
# closed local port, so a detached worker can never reach the real API.
patched_script() {
    sed -e "s|^WORKER_PATH=.*|WORKER_PATH=$TEST_DIR/bin:/usr/bin:/bin|" \
        -e "s|^DESTROY_URL=.*|DESTROY_URL=http://127.0.0.1:9/instances|" "$SCRIPT"
}

test_exact_destroy_request_mocked() {
    fake_curl
    check "accepted" destroy 123 scoped-key
    check "exact URL" grep -qx 'https://console.vast.ai/api/v0/instances/123/' "$TEST_DIR/curl-args"
    check "DELETE" grep -qx DELETE "$TEST_DIR/curl-args"
    check "15 s timeout" grep -qx 15 "$TEST_DIR/curl-args"
    check "no proxy" grep -qx -- --noproxy "$TEST_DIR/curl-args"
    refute "no redirects" grep -qx -e -L -e --location "$TEST_DIR/curl-args"
    refute "key not on the command line" grep -q scoped-key "$TEST_DIR/curl-args"
    equals "$(cat "$TEST_DIR/curl-config")" 'header = "Authorization: Bearer scoped-key"'
    fake_curl '{"success": false}'
    refute "refused" destroy 123 scoped-key
}

# The full transport path: payload on stdin, a real detached worker, a fake curl.
test_arm_and_destroy_end_to_end() {
    fake_curl
    local payload="$TEST_DIR/payload" deadline state source
    source=$(patched_script)
    printf 'GPU_EXPIRE_SOURCE=%q\n%s\n' "$source" "$source" >"$payload"
    export CONTAINER_ID=123 CONTAINER_API_KEY=scoped-key
    run() { env -u GPU_EXPIRE_LIBRARY bash -s -- "$@" <"$payload"; }
    equals "$(run show 123)" '{"status": "none"}'
    equals "$(run bogus 123)" '{"error": "remote"}'
    equals "$(run arm 123 1)" '{"error": "past"}'
    equals "$(CONTAINER_ID=999 run arm 123 $(($(date +%s) + 60)))" '{"error": "credentials"}'
    deadline=$(($(date +%s) + 2))
    equals "$(run arm 123 "$deadline")" "{\"status\": \"armed\", \"deadline\": $deadline}"
    state=$HOME/.local/state/gpu-expire
    equals "$(stat -c %a "$state")" 700
    equals "$(run show 123)" "{\"status\": \"armed\", \"deadline\": $deadline}"
    for _ in $(seq 100); do
        grep -q destroy-request-accepted "$state/state" 2>/dev/null && break
        sleep 0.1
    done
    equals "$(run show 123)" "{\"status\": \"destroy-request-accepted\", \"deadline\": $deadline}"
    check "one destroy request" [ "$(grep -c '^DELETE$' "$TEST_DIR/curl-args")" = 1 ]
    check "stub endpoint" grep -qx 'http://127.0.0.1:9/instances/123/' "$TEST_DIR/curl-args"
    refute "no key logged" grep -rq scoped-key "$state/state"
    equals "$(run cancel 123)" '{"status": "cancelled"}'
    equals "$(run cancel 123)" '{"status": "none"}'
}

for test in $(compgen -A function test_); do
    run_test "$test"
done
echo "$passed passed, $failed failed"
[ "$failed" -eq 0 ]
