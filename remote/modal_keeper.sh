#!/usr/bin/env bash
# Sandbox entrypoint: no credentials, network calls, or persistent objects.
# State is shared with the provider's start prelude and end-of-job hook.
set -eu
umask 077
root=$1 deadline=$2 grace=$3
mkdir -p "$root"
start_until=$(( $(date +%s) + grace ))
while :; do
    now=$(date +%s)
    [ "$now" -lt "$deadline" ] || exit 0
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
