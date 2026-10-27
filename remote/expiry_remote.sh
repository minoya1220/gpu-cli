#!/usr/bin/env bash
# Instance-side auto-destroy controller for `gpu expire`. Linux and bash only;
# needs flock, setsid and curl, no python3. The local CLI sends this file over
# SSH stdin (`bash -s -- ACTION ID [DEADLINE]`), preceded by a
# GPU_EXPIRE_SOURCE assignment holding this same text, so the script can
# install itself as the detached worker.
#
# No account key, CLI configuration, or API response body is read or logged.
# State lives in ~/.local/state/gpu-expire/state. Detached workers survive SSH
# disconnects, not instance shutdown or reboot. Every mutation and the destroy
# request hold the same lock, so replacing or cancelling cannot race an old
# worker.
#
# Output is one JSON line: {"status": ..., "deadline": N} or {"error": CODE}.

set -u

PROC1_ENVIRON=/proc/1/environ
ETC_ENVIRONMENT=/etc/environment
DESTROY_URL=https://console.vast.ai/api/v0/instances
WORKER_PATH=/usr/local/bin:/usr/bin:/bin
READY_TRIES=50

fail() {
    printf '{"error": "%s"}\n' "$1"
    exit 1
}

now() {
    date +%s
}

is_digits() {
    case $1 in
        '' | *[!0-9]*) return 1 ;;
        *) return 0 ;;
    esac
}

# Sets KEY to the instance-scoped key, or fails with "credentials". SSH non-login
# shells do not always inherit Vast's injected environment, so PID 1's
# environment and /etc/environment are read too; the process environment wins.
credentials() {
    local instance_id=$1 id='' key='' entry name value line char quote i
    if [ -r "$PROC1_ENVIRON" ]; then
        {
            while IFS= read -r -d '' entry; do
                name=${entry%%=*}
                value=${entry#*=}
                case $name in
                    CONTAINER_ID) id=$value ;;
                    CONTAINER_API_KEY) key=$value ;;
                esac
            done <"$PROC1_ENVIRON"
        } 2>/dev/null
    fi
    if [ -r "$ETC_ENVIRONMENT" ]; then
        while IFS= read -r line || [ -n "$line" ]; do
            quote=''
            for ((i = 0; i < ${#line}; i++)); do
                char=${line:i:1}
                if [ -n "$quote" ]; then
                    [ "$char" = "$quote" ] && quote=''
                elif [ "$char" = '"' ] || [ "$char" = "'" ]; then
                    quote=$char
                elif [ "$char" = '#' ]; then
                    line=${line:0:i}
                    break
                fi
            done
            line=${line#"${line%%[![:space:]]*}"}
            line=${line#export }
            line=${line#"${line%%[![:space:]]*}"}
            line=${line%"${line##*[![:space:]]}"}
            name=${line%%=*}
            value=${line#*=}
            [ "$name" = "$line" ] && continue
            case $value in
                \"*\") value=${value#\"}; value=${value%\"} ;;
                \'*\') value=${value#\'}; value=${value%\'} ;;
            esac
            case $name in
                CONTAINER_ID) id=$value ;;
                CONTAINER_API_KEY) key=$value ;;
            esac
        done <"$ETC_ENVIRONMENT"
    fi
    [ -n "${CONTAINER_ID:-}" ] && id=$CONTAINER_ID
    [ -n "${CONTAINER_API_KEY:-}" ] && key=$CONTAINER_API_KEY
    case $key in
        '' | *[[:space:]]*) fail credentials ;;
    esac
    [ "$id" = "$instance_id" ] || fail credentials
    KEY=$key
}

# Holds the lock on fd 9 until the calling subshell exits.
lock() {
    exec 9>>"$ROOT/lock" || fail remote
    flock 9 || fail remote
}

# Reads the state file into STATE_* variables; returns 1 when there is none.
load_state() {
    STATE_DEADLINE='' STATE_TOKEN='' STATE_PID='' STATE_STATUS=''
    [ -e "$ROOT/state" ] || return 1
    local name value
    while IFS='=' read -r name value; do
        case $name in
            deadline) STATE_DEADLINE=$value ;;
            token) STATE_TOKEN=$value ;;
            pid) STATE_PID=$value ;;
            status) STATE_STATUS=$value ;;
        esac
    done <"$ROOT/state" || fail remote
    is_digits "$STATE_DEADLINE" && is_digits "$STATE_PID" && [ -n "$STATE_TOKEN" ] || fail remote
    case $STATE_STATUS in
        armed | retrying | destroy-request-accepted) ;;
        *) fail remote ;;
    esac
    return 0
}

# Atomically writes the state (deadline token pid status), mode 0600.
save_state() {
    printf 'deadline=%s\ntoken=%s\npid=%s\nstatus=%s\n' "$1" "$2" "$3" "$4" >"$ROOT/state.tmp" || fail remote
    chmod 600 "$ROOT/state.tmp" || fail remote
    mv -f "$ROOT/state.tmp" "$ROOT/state" || fail remote
}

# Verifies worker identity, not just `kill -0`: PIDs can be recycled.
running() {
    local pid=$1 token=$2 arg found_worker='' found_token=''
    is_digits "$pid" || return 1
    [ -r "/proc/$pid/cmdline" ] || return 1
    while IFS= read -r -d '' arg; do
        [ "$arg" = worker ] && found_worker=1
        [ "$arg" = "$token" ] && found_token=1
    done <"/proc/$pid/cmdline"
    [ -n "$found_worker" ] && [ -n "$found_token" ]
}

public_state() {
    if ! load_state; then
        printf '{"status": "none"}\n'
        return
    fi
    local status=$STATE_STATUS
    if [ "$status" != destroy-request-accepted ] && ! running "$STATE_PID" "$STATE_TOKEN"; then
        status=inactive
    fi
    printf '{"status": "%s", "deadline": %s}\n' "$status" "$STATE_DEADLINE"
}

# Sends the self-destroy request with the instance-scoped key. The key goes
# to curl on stdin, never on a command line. No proxy, no redirects (so the
# Authorization header never reaches another host), no logged body.
destroy() {
    local instance_id=$1 key=$2 body
    key=${key//\\/\\\\}
    key=${key//\"/\\\"}
    body=$(printf 'header = "Authorization: Bearer %s"\n' "$key" |
        curl -q --config - --silent --fail --noproxy '*' --max-time 15 \
            --request DELETE --header 'Content-Type: application/json' --data '{}' \
            "$DESTROY_URL/$instance_id/" 2>/dev/null | head -c 65536) || return 1
    accepted "$body"
}

# Whether a reply is a JSON object whose top-level "success" is true, like
# Python's `isinstance(result, dict) and result.get("success") is True`.
# Nested values and string contents are skipped; a reply that is not a
# complete object counts as not accepted.
accepted() {
    local LC_ALL=C body=$1 i=0 n=${#1} c depth=0 key='' success=0 rest
    while [ "$i" -lt "$n" ]; do
        c=${body:i:1}
        case $c in
            '{' | [[:space:]]) ;;
            *) [ "$depth" -gt 0 ] || return 1 ;;
        esac
        case $c in
            '"')
                key=''
                i=$((i + 1))
                while [ "$i" -lt "$n" ]; do
                    c=${body:i:1}
                    case $c in
                        '"') break ;;
                        '\')
                            key+=${body:i:2}
                            i=$((i + 2))
                            ;;
                        *)
                            key+=$c
                            i=$((i + 1))
                            ;;
                    esac
                done
                [ "$i" -lt "$n" ] || return 1
                ;;
            :)
                if [ "$depth" -eq 1 ] && [ "$key" = success ]; then
                    rest=${body:i+1}
                    rest=${rest#"${rest%%[![:space:]]*}"}
                    case $rest in
                        true | true[[:space:],}]*) success=1 ;;
                        *) success=0 ;;
                    esac
                fi
                key=''
                ;;
            '{' | '[')
                depth=$((depth + 1))
                ;;
            '}' | ']')
                depth=$((depth - 1))
                if [ "$depth" -eq 0 ]; then
                    rest=${body:i+1}
                    [ -z "${rest//[[:space:]]/}" ] && [ "$success" = 1 ]
                    return
                fi
                [ "$depth" -gt 0 ] || return 1
                ;;
        esac
        i=$((i + 1))
    done
    return 1
}

# One locked worker iteration. Prints "exit" or the seconds until the next.
tick() {
    local token=$1 instance_id=$2 key=$3 at=${4:-}
    (
        lock
        if ! load_state || [ "$STATE_TOKEN" != "$token" ]; then
            echo exit
            exit 0
        fi
        [ -n "$at" ] || at=$(now)
        if [ "$STATE_DEADLINE" -gt "$at" ]; then
            echo 1
            exit 0
        fi
        if destroy "$instance_id" "$key"; then
            save_state "$STATE_DEADLINE" "$STATE_TOKEN" "$STATE_PID" destroy-request-accepted
            echo exit
        else
            save_state "$STATE_DEADLINE" "$STATE_TOKEN" "$STATE_PID" retrying
            echo 30
        fi
    )
}

worker() {
    local token=$1 instance_id=$2 delay
    credentials "$instance_id"
    printf 'ready %s\n' "$$" >"$ROOT/ready.$token"
    while :; do
        delay=$(tick "$token" "$instance_id" "$KEY")
        case $delay in
            1 | 30) sleep "$delay" ;;
            *) return 0 ;;
        esac
    done
}

# Starts the detached worker and prints its PID once it reports ready.
start_worker() {
    local token=$1 instance_id=$2 key=$3 ready="$ROOT/ready.$1" line='' tries=0 pid
    rm -f "$ready"
    (
        # Give the detached worker only the scoped credentials, never ambient keys.
        for name in $(compgen -e); do
            unset "$name" 2>/dev/null
        done
        export HOME=$WORKER_HOME PATH=$WORKER_PATH
        export CONTAINER_ID=$instance_id CONTAINER_API_KEY=$key
        exec setsid bash "$ROOT/runner.sh" worker "$ROOT" "$token" "$instance_id" \
            </dev/null >/dev/null 2>&1 9>&-
    ) &
    while [ "$tries" -lt "$READY_TRIES" ]; do
        if [ -s "$ready" ]; then
            read -r line <"$ready"
            break
        fi
        sleep 0.1
        tries=$((tries + 1))
    done
    rm -f "$ready"
    pid=${line#ready }
    case $line in
        "ready "*) ;;
        *) return 1 ;;
    esac
    running "$pid" "$token" || return 1
    printf '%s' "$pid"
}

new_token() {
    local token
    token=$(tr -d '-' </proc/sys/kernel/random/uuid 2>/dev/null)
    [ -n "$token" ] || token=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
    [ -n "$token" ] || return 1
    printf '%s' "$token"
}

control() {
    local action=$1 instance_id=$2 deadline=${3:-}
    umask 077
    mkdir -p "$ROOT" || fail remote
    chmod 700 "$ROOT" || fail remote
    (
        lock
        case $action in
            show)
                public_state
                exit 0
                ;;
            cancel)
                if load_state; then
                    rm -f "$ROOT/state" || fail remote
                    printf '{"status": "cancelled"}\n'
                else
                    printf '{"status": "none"}\n'
                fi
                exit 0
                ;;
        esac
        local token pid had_previous=''
        local prev_deadline='' prev_token='' prev_pid='' prev_status=''
        credentials "$instance_id"
        [ "$deadline" -gt "$(now)" ] || fail past
        [ -n "${GPU_EXPIRE_SOURCE:-}" ] || fail remote
        # Atomic replacement means an old worker never reads a half-written script.
        printf '%s' "$GPU_EXPIRE_SOURCE" >"$ROOT/runner.tmp" || fail remote
        mv -f "$ROOT/runner.tmp" "$ROOT/runner.sh" || fail remote
        if load_state; then
            had_previous=1
            prev_deadline=$STATE_DEADLINE prev_token=$STATE_TOKEN
            prev_pid=$STATE_PID prev_status=$STATE_STATUS
        fi
        token=$(new_token) || fail remote
        pid=$(start_worker "$token" "$instance_id" "$KEY") || fail verification
        if [ "$deadline" -le "$(now)" ]; then
            kill "$pid" 2>/dev/null
            fail past
        fi
        save_state "$deadline" "$token" "$pid" armed
        if ! running "$pid" "$token"; then
            if [ -n "$had_previous" ]; then
                save_state "$prev_deadline" "$prev_token" "$prev_pid" "$prev_status"
            else
                rm -f "$ROOT/state"
            fi
            fail verification
        fi
        printf '{"status": "armed", "deadline": %s}\n' "$deadline"
    )
}

main() {
    exec </dev/null
    WORKER_HOME=${HOME:-/root}
    if [ "${1:-}" = worker ]; then
        [ $# -eq 4 ] || exit 1
        ROOT=$2
        worker "$3" "$4"
        exit 0
    fi
    ROOT=$WORKER_HOME/.local/state/gpu-expire
    local action=${1:-} instance_id=${2:-}
    case $action in
        arm)
            [ $# -eq 3 ] && is_digits "${3:-}" || fail remote
            ;;
        show | cancel)
            [ $# -eq 2 ] || fail remote
            ;;
        *) fail remote ;;
    esac
    is_digits "$instance_id" || fail remote
    command -v flock >/dev/null 2>&1 || fail remote
    if [ "$action" = arm ]; then
        command -v setsid >/dev/null 2>&1 && command -v curl >/dev/null 2>&1 || fail remote
    fi
    control "$action" "$instance_id" "${3:-}"
}

# Tests source this file with GPU_EXPIRE_LIBRARY=1 and call functions directly.
if [ "${GPU_EXPIRE_LIBRARY:-}" != 1 ]; then
    main "$@"
    exit $?
fi
