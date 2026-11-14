#!/usr/bin/env bash
# Fetch exact CI tools; verify bytes before extracting or executing them.
set -euo pipefail
case "${1:-}" in
    shellcheck)
        repo=koalaman/shellcheck version=v0.11.0
        asset=shellcheck-v0.11.0.linux.x86_64.tar.xz
        sha=8c3be12b05d5c177a04c29e3c78ce89ac86f1595681cab149b65b97c4e227198
        member=shellcheck-v0.11.0/shellcheck ;;
    actionlint)
        repo=rhysd/actionlint version=v1.7.7
        asset=actionlint_1.7.7_linux_amd64.tar.gz
        sha=023070a287cd8cccd71515fedc843f1985bf96c436b7effaecce67290e7e0757
        member=actionlint ;;
    gitleaks)
        repo=gitleaks/gitleaks version=v8.24.3
        asset=gitleaks_8.24.3_linux_x64.tar.gz
        sha=9991e0b2903da4c8f6122b5c3186448b927a5da4deef1fe45271c3793f4ee29c
        member=gitleaks ;;
    *) echo 'usage: ci-tool.sh {shellcheck|actionlint|gitleaks}' >&2; exit 2 ;;
esac
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
curl --fail --silent --show-error --location --retry 3 \
    "https://github.com/$repo/releases/download/$version/$asset" -o "$scratch/$asset"
printf '%s  %s\n' "$sha" "$scratch/$asset" | sha256sum --check --strict
mkdir -p "$RUNNER_TEMP/ci-bin"
tar -xOf "$scratch/$asset" "$member" >"$RUNNER_TEMP/ci-bin/$1"
chmod 755 "$RUNNER_TEMP/ci-bin/$1"
printf '%s\n' "$RUNNER_TEMP/ci-bin" >>"$GITHUB_PATH"
