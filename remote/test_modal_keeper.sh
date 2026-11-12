#!/usr/bin/env bash
# Virtual clock; each keeper poll advances exactly one second. No timing races.
set -eu
keeper=$(cd "$(dirname "$0")" && pwd)/modal_keeper.sh
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
mkdir "$root/bin"
cat >"$root/bin/date" <<'STUB'
#!/bin/bash
[ "$1" = +%s ] || exit 98
cat "$CLOCK"
STUB
cat >"$root/bin/sleep" <<'STUB'
#!/bin/bash
set -eu
now=$(cat "$CLOCK")
now=$((now + 1))
[ "$now" -lt 1050 ] || exit 99 # bound even a broken keeper loop
printf '%s\n' "$now" >"$CLOCK"
case "$SCENARIO:$now" in
    hook:1001|late-hook:1002) touch "$STATE/started";;
    hook:1002) echo 1005 >"$STATE/end";;
    late-hook:1005) echo 1007 >"$STATE/end";;
esac
STUB
chmod +x "$root/bin/date" "$root/bin/sleep"
export PATH="$root/bin:$PATH" CLOCK="$root/clock" STATE="$root/state" SCENARIO
run() {
    SCENARIO=$1
    rm -rf "$STATE"; mkdir "$STATE"
    echo 1000 >"$CLOCK"
    case $SCENARIO in
        deadline) touch "$STATE/started";;
        cap) echo 2000 >"$STATE/end";;
        invalid) echo invalid >"$STATE/end";;
    esac
    rc=0
    bash "$keeper" "$STATE" "$2" 3 || rc=$?
    [ "$rc" = "$4" ] && [ "$(cat "$CLOCK")" = "$3" ] || {
        echo "FAIL $SCENARIO: rc=$rc now=$(cat "$CLOCK")" >&2; exit 1;
    }
}
run no-start 2000 1003 0
run deadline 1004 1004 0
run hook 2000 1005 0
run late-hook 2000 1007 0
run cap 1004 1004 0
run invalid 2000 1000 1
echo '6 passed, 0 failed'
