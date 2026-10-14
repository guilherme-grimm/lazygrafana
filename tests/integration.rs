//! Integration tests against the dev stack (`make dev-up dev-bootstrap`).
//! Opt-in: `LAZYGRAFANA_IT=1 cargo test --test integration`.
//! Runs against Grafana 13 (:3000) and, when reachable, Grafana 11 (:3011).

use lazygrafana::app::query::build_panel_queries;
use lazygrafana::config::Context;
use lazygrafana::grafana::dashboard::Dashboard;
use lazygrafana::grafana::dataframe::{logs_from_frames, series_from_frames};
use lazygrafana::grafana::models::{Matcher, NewSilence};
use lazygrafana::grafana::variables::{VarSet, fetch_options, post_process};
use lazygrafana::grafana::{Client, QueryRequest};
use lazygrafana::timerange::now_ms;

fn enabled() -> bool {
    std::env::var("LAZYGRAFANA_IT").is_ok_and(|v| v == "1")
}

fn ctx(url: &str, token_file: &str) -> Option<Context> {
    let path = format!("{}/dev/.secrets/{token_file}", env!("CARGO_MANIFEST_DIR"));
    let token = std::fs::read_to_string(path).ok()?.trim().to_string();
    Some(Context {
        name: "it".into(),
        url: url.into(),
        org: None,
        token: Some(token),
        token_error: None,
        readonly: false,
        cloud: false,
        ca_file: None,
        insecure_skip_verify: false,
    })
}

/// (label, admin client, viewer client) for every reachable instance.
async fn instances() -> Vec<(&'static str, Client, Client)> {
    let mut out = Vec::new();
    for (label, url, prefix) in [("grafana-13", "http://localhost:3000", ""), ("grafana-11", "http://localhost:3011", "compat-")] {
        let (Some(admin), Some(viewer)) = (ctx(url, &format!("{prefix}admin.token")), ctx(url, &format!("{prefix}viewer.token"))) else {
            continue;
        };
        let admin = Client::new(&admin).unwrap();
        if admin.health().await.is_err() {
            eprintln!("skipping {label}: not reachable");
            continue;
        }
        out.push((label, admin, Client::new(&viewer).unwrap()));
    }
    assert!(!out.is_empty(), "no dev Grafana reachable; run `make dev-up dev-bootstrap`");
    out
}

#[tokio::test]
async fn dashboards_variables_and_queries() {
    if !enabled() {
        return;
    }
    for (label, c, _) in instances().await {
        let hits = c.search("", false).await.unwrap();
        for uid in ["lg-demo-signals", "lg-node-overview", "lg-self-monitoring", "lg-service-logs", "lg-scratch"] {
            assert!(hits.iter().any(|h| h.uid == uid), "{label}: {uid} missing from search");
        }
        assert!(
            hits.iter().any(|h| h.is_folder() && h.uid == "lg-team-checkout-oncall" && h.folder_uid.as_deref() == Some("lg-team-checkout"))
        );

        let ds = c.datasources().await.unwrap();
        let resp = c.dashboard("lg-demo-signals").await.unwrap();
        let dash = Dashboard::from_json(&resp.dashboard);
        let mut vars = VarSet::new(&dash.variables);
        let (from, to) = (now_ms() - 3_600_000, now_ms());

        // Query variable against Prometheus.
        let region = vars.get("region").unwrap().def.clone();
        let prom = ds.iter().find(|d| d.uid == "prometheus").unwrap();
        let raw = fetch_options(&c, &region, &region.query, Some(prom), &ds, from, to).await.unwrap();
        let opts = post_process(&region, raw);
        assert_eq!(opts.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(), vec!["eu-west-1", "us-east-1"], "{label}");
        vars.get_mut("region").unwrap().set_options(opts);

        // Every query panel returns data without errors.
        for panel in dash.panels.iter().filter(|p| p.needs_query()) {
            let built = build_panel_queries(panel, &vars, &ds, (from, to), 300, false).unwrap();
            let r = c.query(&QueryRequest { from, to, queries: built.queries }).await.unwrap();
            assert!(r.errors().is_empty(), "{label}/{}: {:?}", panel.title, r.errors());
            assert!(!r.frames().is_empty(), "{label}/{}: no frames", panel.title);
        }

        // Loki variable + logs.
        let logs_dash = Dashboard::from_json(&c.dashboard("lg-service-logs").await.unwrap().dashboard);
        let service = logs_dash.variables.iter().find(|v| v.name == "service").unwrap();
        let loki = ds.iter().find(|d| d.uid == "loki").unwrap();
        let services = fetch_options(&c, service, &service.query, Some(loki), &ds, from, to).await.unwrap();
        assert!(services.iter().any(|s| s == "logs-api"), "{label}: {services:?}");
        let (q, _) = lazygrafana::app::query::adhoc_query(loki, r#"{service="logs-api"}"#, (now_ms() - 300_000, now_ms()), 100, 20);
        let r = c.query(&QueryRequest { from: now_ms() - 300_000, to: now_ms(), queries: vec![q] }).await.unwrap();
        let lines = logs_from_frames(&r.frames());
        assert!(!lines.is_empty() && lines.len() <= 20, "{label}: {} lines", lines.len());

        // Backfilled history is there; a 6h window gives a 15s step, fine enough for 45s spikes.
        let old = (now_ms() - 5 * 86_400_000 - 6 * 3_600_000, now_ms() - 5 * 86_400_000);
        let (q, _) = lazygrafana::app::query::adhoc_query(prom, r#"demo_signal{shape="spike"}"#, old, 2000, 0);
        let r = c.query(&QueryRequest { from: old.0, to: old.1, queries: vec![q] }).await.unwrap();
        let s = series_from_frames(&r.frames());
        assert!(s.first().and_then(|s| s.reduce("max")).unwrap_or(0.0) > 90.0, "{label}: spikes visible at a fine step");

        // Versions + health.
        assert!(c.versions("lg-scratch").await.unwrap().len() >= 3);
        assert_eq!(c.datasource_health("prometheus").await.unwrap().status, "OK");
        assert_eq!(c.datasource_health("broken").await.unwrap().status, "ERROR");
    }
}

#[tokio::test]
async fn alerting_and_permissions() {
    if !enabled() {
        return;
    }
    for (label, c, viewer) in instances().await {
        let groups = c.rules().await.unwrap();
        let always = groups.iter().flat_map(|g| &g.rules).find(|r| r.uid == "always-firing").expect("AlwaysFiring rule");
        assert_eq!(always.state, "firing", "{label}");

        // Silence round trip.
        let now = chrono::Utc::now();
        let id = c
            .create_silence(&NewSilence {
                matchers: vec![Matcher::eq("__alert_rule_uid__", "always-firing")],
                starts_at: now.to_rfc3339(),
                ends_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
                created_by: "lazygrafana-it".into(),
                comment: "integration test".into(),
            })
            .await
            .unwrap();
        assert!(!id.is_empty());
        let silences = c.silences().await.unwrap();
        assert!(silences.iter().any(|s| s.id == id && s.is_active()), "{label}");
        c.expire_silence(&id).await.unwrap();
        let silences = c.silences().await.unwrap();
        assert!(silences.iter().any(|s| s.id == id && s.status.state == "expired"), "{label}");

        // Read-only token cannot write.
        let denied = NewSilence {
            matchers: vec![Matcher::eq("alertname", "x")],
            starts_at: now.to_rfc3339(),
            ends_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
            created_by: "it".into(),
            comment: "should fail".into(),
        };
        assert!(viewer.create_silence(&denied).await.is_err(), "{label}: viewer created a silence");
        let dash = c.dashboard("lg-scratch").await.unwrap().dashboard;
        assert_eq!(viewer.save_dashboard(&dash, None, "nope").await.unwrap_err().status, Some(403), "{label}");

        // State history (annotation backend) is available.
        assert!(c.rule_history("spike-detected", now_ms() - 86_400_000, now_ms()).await.is_ok(), "{label}");
    }
}
