#!/usr/bin/env bash
# Boot-time auto-destroy guard. The generated onstart script starts it first,
# as `guard.sh boot DEADLINE` (epoch seconds), so a deadline holds from boot
# even if the CLI never reaches SSH. It uses the state file, lock and worker
# identity of expiry_remote.sh, so `gpu expire ID show|cancel|WHEN` see,
# cancel or replace it. On a later boot it resumes the deadline in the state
# file; after a cancel it does nothing. Linux and bash; needs flock and curl.
# Uses only Vast's instance-scoped CONTAINER_ID/CONTAINER_API_KEY, and the
# key reaches curl on stdin. Kept small to fit Vast's onstart length limit.

set -u
ROOT=${HOME:-/root}/.local/state/gpu-expire
DESTROY_URL=https://console.vast.ai/api/v0/instances
PROC1_ENVIRON=/proc/1/environ
ETC_ENVIRONMENT=/etc/environment

digits() {
    case $1 in '' | *[!0-9]*) return 1 ;; esac
}

field() {
    sed -n "s/^$1=//p" "$ROOT/state" 2>/dev/null
}

# save DEADLINE TOKEN PID STATUS, atomically and mode 0600.
save() {
    printf 'deadline=%s\ntoken=%s\npid=%s\nstatus=%s\n' "$@" >"$ROOT/state.tmp" &&
        chmod 600 "$ROOT/state.tmp" && mv -f "$ROOT/state.tmp" "$ROOT/state"
}

# The last NAME=value in /etc/environment, without comment or quotes.
etc_value() {
    sed -n "s/^[[:space:]]*\(export[[:space:]]\{1,\}\)\{0,1\}$1=//p" "$ETC_ENVIRONMENT" 2>/dev/null |
        tail -n 1 | sed -e 's/[[:space:]]*#.*//' -e 's/^"\(.*\)"$/\1/' -e "s/^'\(.*\)'$/\1/"
}

proc1_value() {
    tr '\0' '\n' <"$PROC1_ENVIRON" 2>/dev/null | sed -n "s/^$1=//p" | tail -n 1
}

# Whether $1 is a flat JSON object whose "success" is true. Nested values,
# escapes or a non-true "success" count as not accepted: a missed acceptance
# only retries, a false one would stop the guard.
accepted() {
    local body=${1//[[:space:]]/}
    case $body in '{'*'}') ;; *) return 1 ;; esac
    body=,${body:1:${#body}-2},
    case $body in *[][{}\\]*) return 1 ;; esac
    case $body in *',"success":true,'*) ;; *) return 1 ;; esac
    case $body in *',"success":'[!t]*) return 1 ;; esac
}

destroy() {
    local key=${CONTAINER_API_KEY//\\/\\\\} body
    key=${key//\"/\\\"}
    body=$(printf 'header = "Authorization: Bearer %s"\n' "$key" |
        curl -q --config - --silent --fail --noproxy '*' --max-time 15 \
            --request DELETE --header 'Content-Type: application/json' --data '{}' \
            "$DESTROY_URL/$CONTAINER_ID/" 2>/dev/null | head -c 65536) || return 1
    accepted "$body"
}

boot() {
    local deadline=$1 id=${CONTAINER_ID:-} key=${CONTAINER_API_KEY:-} token name
    [ -n "$id" ] || id=$(etc_value CONTAINER_ID)
    [ -n "$key" ] || key=$(etc_value CONTAINER_API_KEY)
    [ -n "$id" ] || id=$(proc1_value CONTAINER_ID)
    [ -n "$key" ] || key=$(proc1_value CONTAINER_API_KEY)
    digits "$id" && digits "$deadline" || exit 1
    case $key in '' | *[[:space:]]*) exit 1 ;; esac
    command -v flock >/dev/null && command -v curl >/dev/null || exit 1
    umask 077
    mkdir -p "$ROOT" && chmod 700 "$ROOT" || exit 1
    exec 9>>"$ROOT/lock" && flock 9 || exit 1
    if [ -e "$ROOT/booted" ]; then
        # A restart: resume the latest deadline, unless it was cancelled.
        [ -e "$ROOT/state" ] || exit 0
        deadline=$(field deadline)
        digits "$deadline" || exit 1
    else
        : >"$ROOT/booted" || exit 1
        # `gpu expire` armed before this first boot reached here.
        [ -e "$ROOT/state" ] && exit 0
    fi
    token=$(tr -d '-' </proc/sys/kernel/random/uuid) && [ -n "$token" ] || exit 1
    save "$deadline" "$token" $$ armed || exit 1
    # Keep only the scoped credentials; the lock on fd 9 passes to the worker.
    for name in $(compgen -e); do
        case $name in HOME | PATH) ;; *) unset "$name" 2>/dev/null ;; esac
    done
    export CONTAINER_ID=$id CONTAINER_API_KEY=$key
    exec bash "$0" worker "$token"
}

# Same PID, now with `worker TOKEN` in its argv as expiry_remote.sh's
# identity check expects. Stops once the state names another token.
worker() {
    local token=$1 deadline
    exec 9>&-
    while :; do
        (
            exec 9>>"$ROOT/lock" && flock 9 || exit 3
            [ "$(field token)" = "$token" ] || exit 0
            deadline=$(field deadline)
            digits "$deadline" || exit 0
            [ "$deadline" -gt "$(date +%s)" ] && exit 1
            if destroy; then
                save "$deadline" "$token" $$ destroy-request-accepted
                exit 0
            fi
            save "$deadline" "$token" $$ retrying
            exit 3
        )
        case $? in
            1) sleep 1 ;;
            3) sleep 30 ;;
            *) exit 0 ;;
        esac
    done
}

# Tests source this file with GPU_EXPIRE_LIBRARY=1 and call functions directly.
if [ "${GPU_EXPIRE_LIBRARY:-}" != 1 ]; then
    case ${1:-} in
        boot) [ $# -eq 2 ] && boot "$2" ;;
        worker) [ $# -eq 2 ] && worker "$2" ;;
    esac
    exit 1
fi
