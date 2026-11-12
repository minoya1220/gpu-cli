#!/usr/bin/env bash
# Sandbox entrypoint: no credentials, network calls, or persistent objects.
# State is shared with the provider's upload heartbeat, start prelude and
# end-of-job hook. The start grace counts from the latest upload heartbeat.
set -eu
umask 077
root=$1 deadline=$2 grace=$3
mkdir -p "$root"
start_until=$(( $(date +%s) + grace ))
while :; do
    now=$(date +%s)
    [ "$now" -lt "$deadline" ] || exit 0
    if [ -f "$root/upload" ]; then
        beat=''
        read -r beat <"$root/upload" || true
        case $beat in
            ''|*[!0-9]*) ;;
            *) [ "$((beat + grace))" -le "$start_until" ] || start_until=$((beat + grace));;
        esac
    fi
    if [ -f "$root/end" ]; then
        end=''
        read -r end <"$root/end" || true
        case $end in ''|*[!0-9]*) exit 1;; esac
        [ "$now" -lt "$end" ] || exit 0
    elif [ ! -f "$root/started" ] && [ "$now" -ge "$start_until" ]; then
        exit 0
    fi
    sleep 1
done
