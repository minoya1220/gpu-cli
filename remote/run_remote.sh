#!/usr/bin/env bash
# Detached-first job supervisor. Linux bash, setsid and /proc; provider-neutral.
# Installed via SSH stdin; configuration and forwarded env never enter argv.
set -u
umask 077

identity() {
    local pid=$1 token=$2 arg seen=''
    case $pid in ''|*[!0-9]*) return 1;; esac
    [ -r "/proc/$pid/cmdline" ] || return 1
    while IFS= read -r -d '' arg; do
        [ "$arg" = "$token" ] && seen=1
    done <"/proc/$pid/cmdline"
    [ -n "$seen" ]
}

publish() {
    printf '%s\t%s\t%s\n' "$1" "$2" "$3" >"$ROOT/status.tmp"
    mv -f "$ROOT/status.tmp" "$ROOT/status"
}

supervise() {
    source "$ROOT/config"
    export PATH="$HOME/.local/bin:$PATH"
    local stopped=0 timed=0 child='' phase=setup rc=0 end now last_commit=$SECONDS
    trap 'stopped=1' TERM INT HUP
    printf '%s\n' "$$" >"$ROOT/pid"
    publish setup - 0
    # The task has its own process group, independent of SSH and supervisor.
    setsid bash "$ROOT/runner.sh" task "$ROOT" "$TOKEN" >>"$ROOT/log" 2>&1 </dev/null &
    child=$!
    while kill -0 "$child" 2>/dev/null; do
        now=$(date +%s)
        if [ "$now" -ge "$JOB_DEADLINE" ] || [ "$stopped" = 1 ]; then
            [ "$now" -ge "$JOB_DEADLINE" ] && timed=1
            if identity "$child" "$TOKEN"; then
                kill -TERM -- "-$child" 2>/dev/null || true
                end=$((now + 30))
                while kill -0 "$child" 2>/dev/null && [ "$(date +%s)" -lt "$end" ]; do sleep 1; done
                # Kill the group even if its leader has exited (grandchildren).
                kill -KILL -- "-$child" 2>/dev/null || true
            fi
            break
        fi
        if [ -f "$ROOT/outputs-hook.sh" ] && [ $((SECONDS - last_commit)) -ge 30 ]; then
            bash "$ROOT/outputs-hook.sh" >>"$ROOT/log" 2>&1 || true
            last_commit=$SECONDS
        fi
        sleep 1 & wait $! || true
    done
    wait "$child" || rc=$?
    # Best effort flush at job end, before publishing completion. A preemption
    # can bypass this; only the last durable checkpoint is recoverable.
    if [ -f "$ROOT/outputs-hook.sh" ]; then
        bash "$ROOT/outputs-hook.sh" >>"$ROOT/log" 2>&1 || true
    fi
    [ -f "$ROOT/phase" ] && read -r phase <"$ROOT/phase"
    [ "$timed" = 1 ] && rc=17
    end=$(( $(date +%s) + LINGER ))
    [ "$end" -gt "$DEADLINE" ] && end=$DEADLINE
    rm -f "$ROOT/config"  # discard forwarded secrets after the task ends
    # Publish only after the log is complete. An attached caller renews a
    # short heartbeat in status/download; keep its original download ceiling.
    publish "$phase" "$rc" "$timed"
    trap 'exit' TERM INT HUP
    local attached=0
    while [ "$(date +%s)" -lt "$DEADLINE" ]; do
        attached=0
        read -r attached <"$ROOT/attached" 2>/dev/null || true
        case $attached in ''|*[!0-9]*) attached=0;; esac
        [ $(( $(date +%s) - attached )) -lt 30 ] || break
        sleep 1
    done
    # No implicit retention: zero linger destroys at unattended finish (or
    # after detecting a lost attachment). Never extend the original ceiling.
    now=$(date +%s)
    [ "$end" -le "$now" ] && end=$((now + 5))
    [ "$end" -gt "$DEADLINE" ] && end=$DEADLINE
    bash "$ROOT/expiry-hook.sh" "$end" >/dev/null 2>&1 || true
}

# Forwarded exports enter this shell's environment, never a child's argv.
# Positional parameters cannot be clobbered by forwarded variable names.
forwarded() {
    eval "$1"
    exec bash -c "$2"
}

task() {
    source "$ROOT/config"
    export PATH="$HOME/.local/bin:$PATH"
    cd "$JOBDIR" || exit 1
    export GPU_RUN_OUTPUTS="$JOBDIR/$OUTPUTS"
    mkdir -p "$GPU_RUN_OUTPUTS" || exit 1
    printf 'setup\n' >"$ROOT/phase"
    # Vast onstart installs uv; other backends can disable this readiness gate
    # in config. Omitted config retains the original Vast behavior.
    if [ "${WAIT_FOR_STARTUP:-1}" = 1 ] && [ ! -e "$HOME/.works" ]; then
        printf 'Waiting for instance startup to finish.\n'
        while [ ! -e "$HOME/.works" ]; do
            if [ "$(cat /workspace/.gpu-setup 2>/dev/null)" = failed ]; then
                echo 'Instance startup failed.' >&2; exit 1
            fi
            sleep 1
        done
    fi
    if [ -n "$SETUP" ]; then
        ( forwarded "$FORWARDED" "$SETUP" ) || exit $?
    fi
    if [ -d .venv ]; then
        export VIRTUAL_ENV="$PWD/.venv" PATH="$PWD/.venv/bin:$PATH"
    fi
    printf 'job\n' >"$ROOT/phase"
    # exec preserves the job PID; the supervisor's token remains in the
    # enclosing task's argv until the command starts. Keep this shell as leader.
    ( forwarded "$FORWARDED" 'if [ -d .venv ]; then export VIRTUAL_ENV="$PWD/.venv" PATH="$PWD/.venv/bin:$PATH"; fi'$'\n'"$COMMAND" ) &
    wait $!
}

main() {
    local action=${1:-} pid='' phase=launch rc=- timed=0 count=0
    ROOT=${2:-} TOKEN=${3:-}
    case $TOKEN in ''|*[!a-zA-Z0-9-]*) exit 1;; esac
    case $action in
        start)
            # A retried start must not execute a job twice.
            mkdir -p "$(dirname "$ROOT")" || exit 1
            mkdir "$ROOT" || exit 1
            printf '%s' "$GPU_RUN_SOURCE" >"$ROOT/runner.sh"
            printf '%s' "$GPU_RUN_CONFIG" >"$ROOT/config"
            printf '%s' "$GPU_RUN_HOOK" >"$ROOT/expiry-hook.sh"
            if [ -n "${GPU_RUN_OUTPUTS_HOOK:-}" ]; then
                printf '%s' "$GPU_RUN_OUTPUTS_HOOK" >"$ROOT/outputs-hook.sh"
            fi
            : >"$ROOT/log"
            date +%s >"$ROOT/attached"
            setsid bash "$ROOT/runner.sh" supervise "$ROOT" "$TOKEN" </dev/null >/dev/null 2>&1 &
            while [ ! -f "$ROOT/status" ] && [ "$count" -lt 50 ]; do sleep 0.1; count=$((count+1)); done
            [ -f "$ROOT/status" ] || exit 1
            printf 'started\n'
            ;;
        supervise) supervise ;;
        task) task ;;
        status)
            date +%s >"$ROOT/attached"
            [ -f "$ROOT/status" ] || exit 1
            IFS=$'\t' read -r phase rc timed <"$ROOT/status"
            read -r pid <"$ROOT/pid"
            count=$(wc -c <"$ROOT/log")
            if [ "$rc" = - ] && ! identity "$pid" "$TOKEN"; then
                printf 'lost\n'
                return 0
            fi
            printf '%s\t%s\t%s\t%s\n' "$phase" "$rc" "$timed" "$count"
            ;;
        stop)
            IFS=$'\t' read -r phase rc timed <"$ROOT/status" || exit 1
            [ "$rc" = - ] || return 0
            read -r pid <"$ROOT/pid" || exit 1
            if identity "$pid" "$TOKEN"; then kill -TERM "$pid"; fi
            ;;
        *) exit 1 ;;
    esac
}

if [ "${GPU_RUN_LIBRARY:-}" != 1 ]; then main "$@"; fi
