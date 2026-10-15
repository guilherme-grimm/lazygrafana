#!/usr/bin/env python3
"""Seeds a Grafana with load-test content for lazygrafana (stdlib only).

  - ~300 light dashboards in a 3-level folder tree (Load/Team NN/Area)
  - heavy dashboards: 48 panels × 300 series, a 3000-option variable,
    heatmaps, log floods, a stat/bargauge/table wall
  - ~100 alert rules (some multi-dimensional, ~300 instances each)

Requires the `load` compose profile (avalanche + log flooders).

  GRAFANA_URL=http://localhost:3000 ./dev/load/seed.py [--clean]
"""

import base64
import json
import os
import sys
import urllib.error
import urllib.request

URL = os.environ.get("GRAFANA_URL", "http://localhost:3000").rstrip("/")
AUTH = "Basic " + base64.b64encode(os.environ.get("GRAFANA_AUTH", "admin:admin").encode()).decode()
PROM = {"type": "prometheus", "uid": "prometheus"}
LOKI = {"type": "loki", "uid": "loki"}
TEAMS, AREAS, PER_AREA = 10, 3, 10  # 300 light dashboards


def api(method, path, body=None, headers=None, ok404=False):
    req = urllib.request.Request(URL + path, method=method, data=json.dumps(body).encode() if body is not None else None)
    req.add_header("Authorization", AUTH)
    req.add_header("Content-Type", "application/json")
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            data = r.read()
            return json.loads(data) if data else None
    except urllib.error.HTTPError as e:
        if ok404 and e.code == 404:
            return None
        raise SystemExit(f"{method} {path}: HTTP {e.code}: {e.read()[:300]!r}")


def folder(uid, title, parent=None):
    if api("GET", f"/api/folders/{uid}", ok404=True) is None:
        body = {"uid": uid, "title": title}
        if parent:
            body["parentUid"] = parent
        api("POST", "/api/folders", body)
    return uid


def save(dash, folder_uid):
    api("POST", "/api/dashboards/db", {"dashboard": dash, "folderUid": folder_uid, "overwrite": True, "message": "load seed"})


def panel(pid, ptype, title, x, y, w, h, targets, ds=PROM, **extra):
    p = {
        "id": pid, "type": ptype, "title": title, "datasource": ds,
        "gridPos": {"x": x, "y": y, "w": w, "h": h},
        "targets": [dict(t, refId=chr(65 + i), datasource=ds) for i, t in enumerate(targets)],
        "fieldConfig": {"defaults": extra.pop("defaults", {}), "overrides": []},
        "options": extra.pop("options", {}),
    }
    p.update(extra)
    return p


def dashboard(uid, title, panels, variables=(), time_from="now-1h", tags=("load",)):
    return {
        "uid": uid, "title": title, "tags": list(tags), "schemaVersion": 39, "editable": True,
        "time": {"from": time_from, "to": "now"}, "refresh": "",
        "templating": {"list": list(variables)}, "panels": panels,
    }


def gauge_metric(i):
    return f"avalanche_gauge_metric_mmmmm_0_{i}"


def light(uid, title, n):
    # Six small panels; aggregated queries so they are cheap.
    m = gauge_metric(n % 40)
    ps = [
        panel(1, "timeseries", "Average", 0, 0, 12, 8, [{"expr": f"avg({m})"}]),
        panel(2, "timeseries", "Max by label", 12, 0, 12, 8, [{"expr": f"max by (label_key_kkkkk_0) ({m})"}]),
        panel(3, "stat", "Series", 0, 8, 6, 4, [{"expr": f"count({m})"}]),
        panel(4, "gauge", "p50", 6, 8, 6, 4, [{"expr": f"quantile(0.5, {m})"}], defaults={"min": 0, "max": 200}),
        panel(5, "bargauge", "Top 5", 12, 8, 12, 4, [{"expr": f"topk(5, {m})", "instant": True, "legendFormat": "{{series_id}}"}]),
        panel(6, "timeseries", "Demo signal", 0, 12, 24, 6, [{"expr": "demo_signal"}]),
    ]
    return dashboard(uid, title, ps)


def heavy_48():
    ps = []
    for i in range(48):
        x, y = (i % 3) * 8, (i // 3) * 7
        ps.append(panel(i + 1, "timeseries", f"metric {i} (300 series)", x, y, 8, 7, [{"expr": gauge_metric(i % 40)}],
                        options={"legend": {"showLegend": False}}))
    return dashboard("load-heavy-48", "Heavy: 48 panels × 300 series", ps)


def heavy_vars():
    variables = [
        {"name": "series", "label": "Series", "type": "query", "datasource": PROM,
         "query": 'label_values(avalanche_gauge_metric_mmmmm_0_0{wide="true"}, series_id)', "refresh": 2, "sort": 3,
         "multi": True, "includeAll": True, "current": {}, "options": [], "hide": 0},
        {"name": "metric", "label": "Metric", "type": "query", "datasource": PROM,
         "query": r'metrics(avalanche_gauge_metric_mmmmm_0_\d+)', "refresh": 1, "sort": 1,
         "multi": False, "includeAll": False, "current": {}, "options": [], "hide": 0},
        {"name": "sid", "label": "Series of $metric", "type": "query", "datasource": PROM,
         "query": "label_values($metric, series_id)", "refresh": 2, "sort": 3,
         "multi": True, "includeAll": True, "current": {}, "options": [], "hide": 0},
    ]
    ps = [
        panel(1, "timeseries", "Wide metric, selected series", 0, 0, 24, 10, [{"expr": 'avalanche_gauge_metric_mmmmm_0_0{wide="true", series_id=~"$series"}'}],
              options={"legend": {"showLegend": False}}),
        panel(2, "timeseries", "$metric, selected series", 0, 10, 24, 10, [{"expr": '$metric{series_id=~"$sid", wide=""}'}],
              options={"legend": {"showLegend": False}}),
    ]
    return dashboard("load-heavy-vars", "Heavy: 3000-option variables", ps, variables)


def heavy_heatmaps():
    ps = []
    for i in range(8):
        ps.append(panel(i + 1, "heatmap", f"histogram {i % 4} by le", (i % 2) * 12, (i // 2) * 8, 12, 8,
                        [{"expr": f"sum by (le) (rate(avalanche_histogram_metric_mmmmm_0_{i % 4}_bucket[$__rate_interval]))",
                          "format": "heatmap", "legendFormat": "{{le}}"}],
                        options={"calculate": False, "yAxis": {"unit": "s"}}))
    return dashboard("load-heavy-heatmaps", "Heavy: heatmaps", ps)


def heavy_logs():
    ps = [
        panel(1, "timeseries", "Log volume by container", 0, 0, 24, 7,
              [{"expr": 'sum by (container) (count_over_time({service=~"load-logs.*"} [$__interval]))', "legendFormat": "{{container}}"}],
              ds=LOKI, defaults={"custom": {"drawStyle": "bars", "fillOpacity": 80, "stacking": {"mode": "normal"}}}),
        panel(2, "logs", "Flood (1000 lines)", 0, 7, 24, 20, [{"expr": '{service="load-logs"}', "maxLines": 1000}], ds=LOKI),
        panel(3, "logs", "Apache errors", 0, 27, 24, 10, [{"expr": '{service="load-logs-apache"} |= "error"'}], ds=LOKI),
    ]
    return dashboard("load-heavy-logs", "Heavy: log flood", ps, time_from="now-15m")


def heavy_wall():
    ps, pid = [], 1
    for i in range(24):
        ps.append(panel(pid, "stat", f"stat {i}", (i % 6) * 4, (i // 6) * 4, 4, 4, [{"expr": f"avg({gauge_metric(i)})"}]))
        pid += 1
    for i in range(6):
        ps.append(panel(pid, "bargauge", f"bars {i} (300 series)", (i % 2) * 12, 16 + (i // 2) * 8, 12, 8,
                        [{"expr": gauge_metric(i), "instant": True, "legendFormat": "{{series_id}}"}],
                        options={"displayMode": "gradient", "orientation": "horizontal", "reduceOptions": {"calcs": ["lastNotNull"]}}))
        pid += 1
    ps.append(panel(pid, "table", "Table (300 rows)", 0, 40, 24, 12, [{"expr": gauge_metric(0), "instant": True, "format": "table"}]))
    return dashboard("load-heavy-wall", "Heavy: stat/bargauge/table wall", ps)


def alert_rules(folder_uid):
    rules = []
    for i in range(90):
        rules.append(("agg", i, f"max({gauge_metric(i % 40)})", 50 if i % 2 else 1000))
    for i in range(10):
        rules.append(("multi", i, f"{gauge_metric(i)}", 60))  # ~300 instances each
    for kind, i, expr, threshold in rules:
        uid = f"load-{kind}-{i}"
        body = {
            "uid": uid, "title": f"Load {kind} {i}", "folderUID": folder_uid, "ruleGroup": f"load-{kind}-{i // 20}",
            "condition": "C", "for": "0s", "noDataState": "OK", "execErrState": "Error",
            "labels": {"team": f"team-{i % 10:02d}", "severity": ["info", "warning", "critical"][i % 3]},
            "annotations": {"summary": f"{expr} > {threshold}"},
            "data": [
                {"refId": "A", "datasourceUid": "prometheus", "relativeTimeRange": {"from": 300, "to": 0},
                 "model": {"refId": "A", "expr": expr, "instant": True}},
                {"refId": "C", "datasourceUid": "__expr__",
                 "model": {"refId": "C", "type": "threshold", "expression": "A",
                           "conditions": [{"evaluator": {"type": "gt", "params": [threshold]}}]}},
            ],
        }
        if api("GET", f"/api/v1/provisioning/alert-rules/{uid}", ok404=True) is None:
            api("POST", "/api/v1/provisioning/alert-rules", body, headers={"X-Disable-Provenance": "true"})
    # Evaluate every 30s rather than the default 1m.
    for group in {f"load-{k}-{i // 20}" for k, i, _, _ in rules}:
        g = api("GET", f"/api/v1/provisioning/folder/{folder_uid}/rule-groups/{group}")
        g["interval"] = 30
        api("PUT", f"/api/v1/provisioning/folder/{folder_uid}/rule-groups/{group}", g, headers={"X-Disable-Provenance": "true"})
    return len(rules)


def clean():
    for uid in ["load-root", "load-alerts"]:
        if api("GET", f"/api/folders/{uid}", ok404=True) is not None:
            api("DELETE", f"/api/folders/{uid}?forceDeleteRules=true")
    print("removed load folders, dashboards and rules")


def main():
    if "--clean" in sys.argv:
        clean()
        return
    root = folder("load-root", "Load")
    n = 0
    for t in range(TEAMS):
        team = folder(f"load-team-{t:02d}", f"Team {t:02d}", root)
        for a in range(AREAS):
            area = folder(f"load-team-{t:02d}-{a}", ["Backend", "Data", "Edge"][a], team)
            for d in range(PER_AREA):
                uid = f"load-{t:02d}-{a}-{d}"
                save(light(uid, f"Service {t:02d}-{a}-{d}", n), area)
                n += 1
        print(f"  team {t:02d}: {AREAS * PER_AREA} dashboards", flush=True)
    heavy = folder("load-heavy", "Heavy", root)
    for d in [heavy_48(), heavy_vars(), heavy_heatmaps(), heavy_logs(), heavy_wall()]:
        save(d, heavy)
    alerts = folder("load-alerts", "Load Alerts")
    r = alert_rules(alerts)
    print(f"seeded {n} light dashboards, 5 heavy dashboards, {r} alert rules")


if __name__ == "__main__":
    main()
