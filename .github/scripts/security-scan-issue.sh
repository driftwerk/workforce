#!/usr/bin/env bash
# Keeps one open issue carrying the latest security scan report; closes it once a scan comes back clean.

set -euo pipefail

report=${1:?report path required}
title="Security scan findings"
label="security-scan"

gh label create "$label" --color B60205 --description "Daily vulnerability and end-of-life scan" --force >/dev/null

existing=$(gh issue list --label "$label" --state open --json number --jq '.[0].number // empty')

if grep -q '^Nothing found\.$' "$report"; then
    if [ -n "$existing" ]; then
        gh issue close "$existing" --comment "The latest scan found nothing: ${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID}"
    fi
    exit 0
fi

body=$(mktemp)
{
    cat "$report"
    echo
    echo "Run: ${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID}"
} > "$body"

if [ -z "$existing" ]; then
    gh issue create --title "$title" --label "$label" --body-file "$body"
elif [ "${ACTION:-false}" = "true" ]; then
    gh issue edit "$existing" --body-file "$body"
    gh issue comment "$existing" --body "Still needs action: ${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID}"
else
    gh issue edit "$existing" --body-file "$body"
fi
