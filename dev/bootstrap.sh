#!/usr/bin/env bash
# Seeds the dev Grafana with everything provisioning files can't express:
#   - service account tokens (admin + viewer, plus one in a second org) -> dev/.secrets/
#   - nested folders and a scratch dashboard with version history
#   - a second organization with its own dashboard
#   - a few annotations
#   - 7 days of backfilled history for the synthetic demo_* series
#
# Safe to re-run: existing objects are reused; only tokens are re-issued.
#
# Env:
#   GRAFANA_URL     (default http://localhost:3000)
#   SECRETS_PREFIX  prefix for token files, e.g. "compat-" (default "")
#   SKIP_BACKFILL   set to 1 to skip the Prometheus backfill
set -euo pipefail

GRAFANA_URL=${GRAFANA_URL:-http://localhost:3000}
SECRETS_PREFIX=${SECRETS_PREFIX:-}
SKIP_BACKFILL=${SKIP_BACKFILL:-0}
AUTH=admin:admin

DEV_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SECRETS_DIR="$DEV_DIR/.secrets"
COMPOSE=(docker compose -f "$DEV_DIR/docker-compose.yml")

for bin in curl jq docker; do
  command -v "$bin" >/dev/null || { echo "missing dependency: $bin" >&2; exit 1; }
done

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

# api METHOD PATH [JSON] [ORG_ID] -> prints the response body, fails on HTTP errors.
api() {
  local method=$1 path=$2 body=${3:-} org=${4:-1}
  curl -fsS -u "$AUTH" -X "$method" -H 'Content-Type: application/json' \
    -H "X-Grafana-Org-Id: $org" ${body:+--data "$body"} "$GRAFANA_URL$path"
}

# exists PATH [ORG_ID] -> exit status only.
exists() {
  curl -fs -o /dev/null -u "$AUTH" -H "X-Grafana-Org-Id: ${2:-1}" "$GRAFANA_URL$1"
}

wait_for_grafana() {
  log "waiting for Grafana at $GRAFANA_URL"
  for _ in $(seq 60); do
    curl -fs -o /dev/null "$GRAFANA_URL/api/health" && return
    sleep 2
  done
  echo "Grafana did not become healthy" >&2
  exit 1
}

# issue_token NAME ROLE FILE [ORG_ID]
issue_token() {
  local name=$1 role=$2 file=$3 org=${4:-1} id
  id=$(api GET "/api/serviceaccounts/search?query=$name" "" "$org" \
    | jq -r --arg n "$name" '.serviceAccounts[] | select(.name == $n) | .id' | head -n1)
  if [[ -z $id ]]; then
    id=$(api POST /api/serviceaccounts "$(jq -nc --arg n "$name" --arg r "$role" '{name: $n, role: $r}')" "$org" | jq -r .id)
  fi
  api POST "/api/serviceaccounts/$id/tokens" "$(jq -nc --arg n "dev-$(date +%s%N)" '{name: $n}')" "$org" \
    | jq -r .key >"$SECRETS_DIR/$SECRETS_PREFIX$file"
  chmod 600 "$SECRETS_DIR/$SECRETS_PREFIX$file"
  log "token for $name ($role, org $org) -> dev/.secrets/$SECRETS_PREFIX$file"
}

# ensure_folder UID TITLE [PARENT_UID] [ORG_ID]
ensure_folder() {
  local uid=$1 title=$2 parent=${3:-} org=${4:-1}
  exists "/api/folders/$uid" "$org" && return
  api POST /api/folders "$(jq -nc --arg u "$uid" --arg t "$title" --arg p "$parent" \
    '{uid: $u, title: $t} + (if $p == "" then {} else {parentUid: $p} end)')" "$org" >/dev/null
  log "folder $title"
}

# save_dashboard FOLDER_UID MESSAGE DASHBOARD_JSON [ORG_ID]
save_dashboard() {
  api POST /api/dashboards/db "$(jq -nc --arg f "$1" --arg m "$2" --argjson d "$3" \
    '{dashboard: $d, folderUid: $f, message: $m, overwrite: true}')" "${4:-1}" >/dev/null
}

scratch_dashboard() {
  local title=$1 threshold=$2
  jq -nc --arg t "$title" --argjson th "$threshold" '{
    uid: "lg-scratch", title: $t, tags: ["dev", "scratch"], schemaVersion: 39,
    time: {from: "now-6h", to: "now"},
    panels: [{
      id: 1, type: "timeseries", title: "Checkout traffic",
      gridPos: {x: 0, y: 0, w: 24, h: 9},
      datasource: {type: "prometheus", uid: "prometheus"},
      targets: [{refId: "A", expr: "demo_requests_per_second{service=\"checkout\"}"}],
      fieldConfig: {defaults: {unit: "reqps", thresholds: {mode: "absolute",
        steps: [{color: "green", value: null}, {color: "red", value: $th}]}}, overrides: []},
      options: {}
    }]
  }'
}

seed_folders_and_dashboards() {
  ensure_folder lg-teams Teams
  ensure_folder lg-team-checkout Checkout lg-teams
  ensure_folder lg-team-checkout-oncall "On-call" lg-team-checkout

  if ! exists /api/dashboards/uid/lg-scratch; then
    # Three saves -> three versions to diff and restore.
    save_dashboard lg-team-checkout-oncall "initial version" "$(scratch_dashboard 'Checkout Scratch' 1200)"
    save_dashboard lg-team-checkout-oncall "lower threshold" "$(scratch_dashboard 'Checkout Scratch' 1000)"
    save_dashboard lg-team-checkout-oncall "rename" "$(scratch_dashboard 'Checkout Scratchpad' 1000)"
    log "dashboard Teams/Checkout/On-call/Checkout Scratchpad (3 versions)"
  fi
}

seed_second_org() {
  local org_id
  org_id=$(curl -fsS -u "$AUTH" "$GRAFANA_URL/api/orgs/name/Team%20B" 2>/dev/null | jq -r .id || true)
  if [[ -z $org_id ]]; then
    org_id=$(api POST /api/orgs '{"name": "Team B"}' | jq -r .orgId)
    log "org Team B (id $org_id)"
  fi

  if ! exists "/api/datasources/uid/prometheus" "$org_id"; then
    api POST /api/datasources '{"name": "Prometheus", "uid": "prometheus", "type": "prometheus",
      "access": "proxy", "url": "http://prometheus:9090", "isDefault": true}' "$org_id" >/dev/null
  fi
  ensure_folder lg-team-b-services Services "" "$org_id"
  if ! exists /api/dashboards/uid/lg-team-b-search "$org_id"; then
    save_dashboard lg-team-b-services "initial version" "$(jq -nc '{
      uid: "lg-team-b-search", title: "Search Service", tags: ["team-b"], schemaVersion: 39,
      time: {from: "now-6h", to: "now"},
      panels: [{id: 1, type: "timeseries", title: "Search traffic",
        gridPos: {x: 0, y: 0, w: 24, h: 9},
        datasource: {type: "prometheus", uid: "prometheus"},
        targets: [{refId: "A", expr: "demo_requests_per_second{service=\"search\"}"}],
        fieldConfig: {defaults: {unit: "reqps"}, overrides: []}, options: {}}]
    }')" "$org_id"
    log "dashboard Team B/Services/Search Service"
  fi
  issue_token lazygrafana-team-b Editor team-b.token "$org_id"
}

seed_annotations() {
  local count now_ms
  count=$(api GET "/api/annotations?tags=lazygrafana-dev&limit=1" | jq length)
  [[ $count -gt 0 ]] && return
  now_ms=$(( $(date +%s) * 1000 ))
  api POST /api/annotations "$(jq -nc --argjson t $(( now_ms - 2 * 3600 * 1000 )) '{
    dashboardUID: "lg-demo-signals", time: $t,
    tags: ["lazygrafana-dev", "deploy"], text: "Deploy checkout v1.4.2"}')" >/dev/null
  api POST /api/annotations "$(jq -nc --argjson t $(( now_ms - 26 * 3600 * 1000 )) '{
    time: $t, timeEnd: ($t + 1800000),
    tags: ["lazygrafana-dev", "maintenance"], text: "Maintenance window (org-wide)"}')" >/dev/null
  log "annotations (deploy marker + maintenance window)"
}

# A rule without provenance, as if created in the UI: lazygrafana can pause/resume it.
seed_manual_rule() {
  exists /api/v1/provisioning/alert-rules/manual-rule && return
  local folder
  folder=$(api GET "/api/search?type=dash-folder&query=Dev%20Alerts" | jq -r '.[] | select(.title == "Dev Alerts") | .uid' | head -n1)
  [[ -z $folder ]] && { log "Dev Alerts folder not provisioned yet, skipping manual rule"; return; }
  curl -fsS -o /dev/null -u "$AUTH" -X POST -H 'Content-Type: application/json' -H 'X-Disable-Provenance: true' \
    "$GRAFANA_URL/api/v1/provisioning/alert-rules" --data "$(jq -nc --arg f "$folder" '{
      uid: "manual-rule", title: "ManualRule", folderUID: $f, ruleGroup: "manual", condition: "C",
      for: "1m", noDataState: "NoData", execErrState: "Error", labels: {severity: "info", team: "platform"},
      annotations: {summary: "Created like a UI rule (no provenance): try pausing it with p."},
      data: [
        {refId: "A", datasourceUid: "prometheus", relativeTimeRange: {from: 300, to: 0},
         model: {refId: "A", expr: "demo_signal{shape=\"sawtooth\"}", instant: true}},
        {refId: "C", datasourceUid: "__expr__", model: {refId: "C", type: "threshold", expression: "A",
         conditions: [{evaluator: {type: "gt", params: [90]}}]}}
      ]}')"
  log "alert rule ManualRule (UI-style, pausable)"
}

backfill_prometheus() {
  [[ $SKIP_BACKFILL == 1 ]] && return
  local prom=http://localhost:${PROMETHEUS_PORT:-9090} lowest start end
  # Backfill only the gap between 7 days ago and the oldest sample Prometheus has.
  lowest=$(curl -fsS "$prom/api/v1/query" --data-urlencode 'query=prometheus_tsdb_lowest_timestamp_seconds' \
    | jq -r '.data.result[0].value[1] // empty' | cut -d. -f1)
  start=$(( $(date +%s) - 7 * 86400 ))
  end=${lowest:-$(date +%s)}
  if (( end - start < 3600 )); then
    log "prometheus already has history, skipping backfill"
    return
  fi
  log "backfilling demo_* series from $(date -d "@$start" '+%F %R') to $(date -d "@$end" '+%F %R')"
  "${COMPOSE[@]}" exec -T prometheus promtool tsdb create-blocks-from rules \
    --start="$start" --end="$end" --eval-interval=15s \
    --url=http://localhost:9090 --output-dir=/prometheus \
    /etc/prometheus/demo-rules.yml >/dev/null
  "${COMPOSE[@]}" restart prometheus >/dev/null
  log "prometheus restarted with backfilled blocks"
}

mkdir -p "$SECRETS_DIR"
wait_for_grafana
issue_token lazygrafana-admin Admin admin.token
issue_token lazygrafana-viewer Viewer viewer.token
seed_folders_and_dashboards
seed_second_org
seed_annotations
seed_manual_rule
backfill_prometheus
log "done - try: lazygrafana --config dev/lazygrafana.yml"
