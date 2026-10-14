//! Building `/api/ds/query` requests from panel targets.

use serde_json::{Value, json};

use crate::grafana::dashboard::{DsRef, Panel};
use crate::grafana::models::Datasource;
use crate::grafana::variables::{Scope, VarSet, resolve_ds};
use crate::timerange::{Millis, nice_step, parse_duration};

/// Recursively interpolates every string in a target (except identifiers).
fn interpolate_value(v: &mut Value, vars: &VarSet, kind: Option<&str>, scope: Scope) {
    match v {
        Value::String(s) if s.contains('$') || s.contains("[[") => *s = vars.interpolate(s, kind, scope),
        Value::Array(a) => a.iter_mut().for_each(|x| interpolate_value(x, vars, kind, scope)),
        Value::Object(o) => {
            for (k, x) in o.iter_mut() {
                if k != "refId" && k != "datasource" {
                    interpolate_value(x, vars, kind, scope);
                }
            }
        }
        _ => {}
    }
}

/// Minimum step: the panel's interval, else the datasource scrape interval, else 15s.
pub fn min_step(panel_interval: Option<&str>, ds: Option<&Datasource>) -> Millis {
    let ds_min = ds
        .and_then(|d| d.json_data.get("timeInterval").and_then(Value::as_str))
        .and_then(|s| parse_duration(s.trim_start_matches('>')).ok());
    panel_interval.and_then(|s| parse_duration(s.trim_start_matches('>')).ok()).or(ds_min).unwrap_or(15_000)
}

pub struct BuiltQuery {
    pub queries: Vec<Value>,
    pub step: Millis,
}

/// Wraps a PromQL expression so short spikes survive coarse steps.
pub fn max_over_time(expr: &str) -> String {
    format!("max_over_time(({expr})[$__interval:])")
}

/// Builds the request body queries for a panel, or `Err` with a reason.
pub fn build_panel_queries(
    panel: &Panel,
    vars: &VarSet,
    datasources: &[Datasource],
    range: (Millis, Millis),
    max_points: u32,
    max_ot: bool,
) -> Result<BuiltQuery, String> {
    let scope = Scope { from: range.0, to: range.1 };
    let panel_ds = panel.datasource.as_ref().filter(|d| !d.is_mixed());
    let mut out = Vec::new();
    let mut step = 0;
    for t in panel.targets.iter().filter(|t| !t.hide) {
        let ds_ref = t.datasource.as_ref().filter(|d| !d.is_mixed()).or(panel_ds);
        if ds_ref.is_some_and(DsRef::is_grafana_builtin) {
            continue;
        }
        let is_expr = ds_ref.and_then(|d| d.uid.as_deref()) == Some("__expr__")
            || t.raw.get("type").is_some() && ds_ref.and_then(|d| d.kind.as_deref()) == Some("__expr__");
        let mut q = t.raw.clone();
        if is_expr {
            q["datasource"] = json!({"uid": "__expr__", "type": "__expr__"});
        } else {
            let ds = resolve_ds(ds_ref, vars, datasources, scope)
                .ok_or_else(|| format!("datasource {:?} not found", ds_ref.and_then(|d| d.uid.clone())))?;
            interpolate_value(&mut q, vars, Some(&ds.kind), scope);
            q["datasource"] = json!({"uid": ds.uid, "type": ds.kind});
            let s = nice_step(range.1 - range.0, max_points, min_step(panel.interval.as_deref(), Some(ds)));
            step = step.max(s);
            q["intervalMs"] = json!(s);
            if max_ot
                && ds.kind == "prometheus"
                && let Some(expr) = q.get("expr").and_then(Value::as_str)
                && !expr.trim().is_empty()
            {
                q["expr"] = json!(max_over_time(expr));
            }
        }
        q["maxDataPoints"] = json!(panel.max_data_points.unwrap_or(max_points).min(max_points.max(1)));
        if let Some(o) = q.as_object_mut() {
            o.remove("hide");
        }
        out.push(q);
    }
    if out.is_empty() {
        return Err("no queries".into());
    }
    Ok(BuiltQuery { queries: out, step: step.max(1) })
}

/// A single ad-hoc query (Explore, alert rule charts).
pub fn adhoc_query(ds: &Datasource, expr: &str, range: (Millis, Millis), max_points: u32, logs_limit: u32) -> (Value, Millis) {
    let step = nice_step(range.1 - range.0, max_points, min_step(None, Some(ds)));
    let mut q = json!({
        "refId": "A",
        "datasource": {"uid": ds.uid, "type": ds.kind},
        "expr": expr,
        "intervalMs": step,
        "maxDataPoints": max_points,
    });
    if ds.kind == "loki" {
        q["queryType"] = json!("range");
        q["maxLines"] = json!(logs_limit);
    } else if ds.kind != "prometheus" {
        // Other datasources commonly use `query`/`rawSql`; send both shapes.
        q["query"] = json!(expr);
        q["rawSql"] = json!(expr);
    }
    (q, step)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grafana::dashboard::Dashboard;

    fn ds() -> Vec<Datasource> {
        serde_json::from_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/datasources.json")).unwrap())
            .unwrap()
    }

    #[test]
    fn builds_interpolated_queries() {
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/dashboard_demo.json")).unwrap(),
        )
        .unwrap();
        let d = Dashboard::from_json(&v["dashboard"]);
        let mut vars = VarSet::new(&d.variables);
        vars.get_mut("region")
            .unwrap()
            .set_options(vec![crate::grafana::variables::VarOption { text: "eu-west-1".into(), value: "eu-west-1".into() }]);
        let panel = d.panels.iter().find(|p| p.title == "Demo signals").unwrap();
        let q = build_panel_queries(panel, &vars, &ds(), (0, 6 * 3_600_000), 1000, false).unwrap();
        let expr = q.queries[0]["expr"].as_str().unwrap();
        assert!(!expr.contains('$'), "{expr}");
        assert!(expr.contains("eu-west-1"));
        assert_eq!(q.queries[0]["datasource"]["type"], "prometheus");
        assert_eq!(q.step, 30_000);
        let q = build_panel_queries(panel, &vars, &ds(), (0, 6 * 3_600_000), 1000, true).unwrap();
        assert!(q.queries[0]["expr"].as_str().unwrap().starts_with("max_over_time(("));
    }
}
