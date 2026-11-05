#!/bin/sh
# Read-only REST fixture: no invocation can fall through to real curl/gcloud.
printf 'curl\n' >> "$FAKE_CLI_LOG"
root=${FAKE_CLI_LOG%/*}
reply() {
    status=200
    case "$1" in
      https://tpu.googleapis.com/v1/*/locations\?*) body='{"locations":[{"locationId":"us-east5-b"},{"locationId":"us-central1-a"}]}' ;;
      https://tpu.googleapis.com/v2/*/acceleratorTypes*) body='{"acceleratorTypes":[{"type":"v5litepod-1"},{"type":"v6e-1"}]}' ;;
      https://tpu.googleapis.com/v2/*/nodes\?*) body=$(cat "$root/gcp-nodes.json") ;;
      https://cloudtasks.googleapis.com/v2/*/tasks\?*) body='{"tasks":[]}' ;;
      https://cloudbilling.googleapis.com/v1beta/skus/*/price\?*)
        if [ -f "$root/gcp-prices" ]; then
          units=2
          case "$1" in *@SPOT_ID@*) units=0 ;; esac
          body='{"currencyCode":"USD","rate":{"unitInfo":{"unit":"h","unitDescription":"hour","unitQuantity":{"value":"1"}},"tiers":[{"startAmount":{"value":"0"},"listPrice":{"currencyCode":"USD","units":"'"$units"'","nanos":750000000}}]}}'
        else status=403; body='{"error":{"status":"PERMISSION_DENIED"}}'; fi ;;
      *) status=400; body='{}' ;;
    esac
}
url=
for arg in "$@"; do case "$arg" in https://*) url=$arg ;; esac; done
if [ -n "$url" ]; then
    reply "$url"; printf '%s\n%s' "$body" "$status"; exit 0
fi
while IFS= read -r line; do
    case "$line" in
      'url = "'*) url=${line#'url = "'}; url=${url%\"} ;;
      'output = "'*)
        file=${line#'output = "'}; file=${file%\"}
        reply "$url"; printf '%s' "$body" > "$file"
        printf '%s\t%s\t0.001\n' "$file" "$status" ;;
    esac
done
