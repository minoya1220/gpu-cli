#!/bin/bash
# gpu directory-sync worker. Sent over SSH stdin and run as
#   bash -s -- ACTION DIR [ARGS...]
# It only reads the directory it is given (never writes inside it, except
# `prepare`, which creates it). Output is line based: `key=value` lines, then
# for manifests one `SIZE<TAB>MTIME<TAB>PATH` line per regular file.
#
#   prepare DIR           create DIR, make sure rsync exists, report the watcher
#   manifest DIR          signature and file list
#   wait DIR SECS SIG     block until the tree changes (then print the manifest)
#                         or SECS pass (status=idle)
#
# Everything lives in main so bash has read the whole script before it runs
# anything: stdin is the script, and no command may consume it.

main() {
    local action="${1:-}" dir="${2:-}"
    case "$dir" in
    /*) ;;
    *)
        echo "error=bad-directory"
        return 2
        ;;
    esac
    case "$action" in
    prepare) prepare "$dir" ;;
    manifest)
        [ -d "$dir" ] || {
            echo "error=missing-directory"
            return 3
        }
        manifest "$dir"
        ;;
    wait)
        [ -d "$dir" ] || {
            echo "error=missing-directory"
            return 3
        }
        wait_for_change "$dir" "${3:-30}" "${4:-}"
        ;;
    *)
        echo "error=bad-action"
        return 2
        ;;
    esac
}

# Directories that never travel in either direction.
PRUNE=('(' -name .git -o -name .venv -o -name venv -o -name __pycache__ -o -name node_modules -o -name gpu-runs ')')
INOTIFY_EXCLUDE='(^|/)(\.git|\.venv|venv|__pycache__|node_modules|gpu-runs)(/|$)'

list_files() {
    cd "$1" || return 2
    # Paths with control characters would break the line format: skip them.
    if find /dev/null -maxdepth 0 -printf '' 2>/dev/null; then
        find . "${PRUNE[@]}" -prune -o -type f ! -name .DS_Store ! -path '*[[:cntrl:]]*' \
            -printf '%s\t%T@\t%P\n' </dev/null 2>/dev/null
    else
        find . "${PRUNE[@]}" -prune -o -type f ! -name .DS_Store ! -path '*[[:cntrl:]]*' \
            -exec stat -f '%z%t%m%t%N' {} + </dev/null 2>/dev/null |
            awk -F'\t' 'BEGIN { OFS = "\t" } { sub(/^\.\//, "", $3); print }'
    fi
}

signature() {
    list_files "$1" | LC_ALL=C sort | cksum | tr ' ' '-'
}

manifest() {
    local listing
    listing=$(list_files "$1" | LC_ALL=C sort)
    echo "sig=$(printf '%s\n' "$listing" | cksum | tr ' ' '-')"
    [ -n "$listing" ] && printf '%s\n' "$listing"
    return 0
}

have() { command -v "$1" >/dev/null 2>&1; }

install_packages() {
    local sudo=()
    if [ "$(id -u)" != 0 ]; then
        if have sudo && sudo -n true 2>/dev/null; then
            sudo=(sudo -n)
        else
            return 1
        fi
    fi
    if have apt-get; then
        "${sudo[@]}" env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" >/dev/null 2>&1 ||
            { "${sudo[@]}" apt-get update -qq >/dev/null 2>&1 &&
                "${sudo[@]}" env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@" >/dev/null 2>&1; }
    elif have dnf; then
        "${sudo[@]}" dnf install -y -q "$@" >/dev/null 2>&1
    elif have yum; then
        "${sudo[@]}" yum install -y -q "$@" >/dev/null 2>&1
    elif have apk; then
        "${sudo[@]}" apk add -q "$@" >/dev/null 2>&1
    else
        return 1
    fi
}

prepare() {
    local dir="$1"
    if ! mkdir -p -- "$dir" 2>/dev/null; then
        # A non-root login (TPU VMs): create it as root and hand it over.
        if have sudo && sudo -n true 2>/dev/null; then
            sudo -n mkdir -p -- "$dir" && sudo -n chown "$(id -u):$(id -g)" -- "$dir"
        fi
        [ -d "$dir" ] && [ -w "$dir" ] || {
            echo "error=cannot-create-directory"
            return 3
        }
    fi
    have rsync || install_packages rsync inotify-tools || true
    have rsync || install_packages rsync || true
    if have rsync; then echo "rsync=ok"; else echo "rsync=missing"; fi
    if have inotifywait; then echo "watcher=inotify"; else echo "watcher=poll"; fi
    echo "existing=$(list_files "$dir" | wc -l | tr -d ' ')"
}

emit_changed() {
    echo "status=changed"
    manifest "$1"
}

# Quiet period: keep waiting while saves keep arriving, so a burst becomes one
# change. Capped so a constantly written file cannot postpone it forever.
settle_inotify() {
    local n=0
    while [ "$n" -lt 15 ]; do
        inotifywait -r -q -t 1 -e close_write,create,delete,moved_to,moved_from \
            --exclude "$INOTIFY_EXCLUDE" "$1" </dev/null >/dev/null 2>&1 || return 0
        n=$((n + 1))
    done
}

poll_for_change() {
    local dir="$1" secs="$2" sig="$3" waited=0 now
    while [ "$waited" -lt "$secs" ]; do
        sleep 2
        waited=$((waited + 2))
        now=$(signature "$dir")
        if [ "$now" != "$sig" ]; then
            # Settle: wait until two consecutive reads agree.
            local n=0 again
            while [ "$n" -lt 10 ]; do
                sleep 1
                again=$(signature "$dir")
                [ "$again" = "$now" ] && break
                now="$again"
                n=$((n + 1))
            done
            emit_changed "$dir"
            return 0
        fi
    done
    echo "status=idle"
}

wait_for_change() {
    local dir="$1" secs="$2" sig="$3" rc
    case "$secs" in '' | *[!0-9]*) secs=30 ;; esac
    # Anything that changed since the caller's last listing is reported at once.
    if [ -n "$sig" ] && [ "$(signature "$dir")" != "$sig" ]; then
        emit_changed "$dir"
        return 0
    fi
    if have inotifywait; then
        inotifywait -r -q -t "$secs" -e close_write,create,delete,moved_to,moved_from \
            --exclude "$INOTIFY_EXCLUDE" "$dir" </dev/null >/dev/null 2>&1
        rc=$?
        case "$rc" in
        0)
            settle_inotify "$dir"
            emit_changed "$dir"
            return 0
            ;;
        2)
            echo "status=idle"
            return 0
            ;;
        esac
        # Could not watch (watch limit, unsupported file system): poll instead.
    fi
    poll_for_change "$dir" "$secs" "${sig:-$(signature "$dir")}"
}

main "$@"
