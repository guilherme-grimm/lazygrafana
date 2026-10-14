//! Typed models for the Grafana HTTP API responses lazygrafana uses.
//! Unknown fields are ignored; dashboard JSON is kept as raw `Value` so it
//! round-trips losslessly.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Health {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub database: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub edition: String,
    #[serde(default)]
    pub env: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FrontendSettings {
    #[serde(default)]
    pub build_info: BuildInfo,
    #[serde(default)]
    pub app_url: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct User {
    #[serde(default)]
    pub login: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub org_id: i64,
    #[serde(default)]
    pub is_grafana_admin: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub uid: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub is_starred: bool,
    #[serde(default)]
    pub folder_uid: Option<String>,
    #[serde(default)]
    pub folder_title: Option<String>,
}

impl SearchHit {
    pub fn is_folder(&self) -> bool {
        self.kind == "dash-folder"
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub uid: String,
    pub title: String,
    #[serde(default)]
    pub parent_uid: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DashboardMeta {
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub folder_uid: Option<String>,
    #[serde(default)]
    pub folder_title: Option<String>,
    #[serde(default)]
    pub can_save: bool,
    #[serde(default)]
    pub can_edit: bool,
    #[serde(default)]
    pub can_delete: bool,
    #[serde(default)]
    pub can_star: bool,
    #[serde(default)]
    pub is_starred: bool,
    #[serde(default)]
    pub provisioned: bool,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub updated_by: String,
    #[serde(default)]
    pub version: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DashboardResponse {
    pub dashboard: Value,
    #[serde(default)]
    pub meta: DashboardMeta,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveResponse {
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    pub id: i64,
    pub version: i64,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub restored_from: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionList {
    #[serde(default)]
    pub versions: Vec<VersionInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionFull {
    pub version: i64,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Datasource {
    pub uid: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub type_name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub access: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub json_data: Value,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DatasourceHealth {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub message: String,
}

// ---- alerting ---------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RulesResponse {
    #[serde(default)]
    pub data: RulesData,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RulesData {
    #[serde(default)]
    pub groups: Vec<RuleGroup>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuleGroup {
    pub name: String,
    /// Folder title.
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub folder_uid: String,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub interval: f64,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub name: String,
    #[serde(default)]
    pub uid: String,
    /// `firing`, `pending`, `inactive`, `recovering`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub health: String,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[serde(default)]
    pub alerts: Vec<RuleAlert>,
    #[serde(default)]
    pub active_at: Option<String>,
    #[serde(default)]
    pub last_evaluation: Option<String>,
    #[serde(default)]
    pub is_paused: bool,
    #[serde(default)]
    pub provenance: Option<String>,
    #[serde(rename = "queriedDatasourceUIDs", default)]
    pub queried_datasource_uids: Vec<String>,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuleAlert {
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    /// `Alerting`, `Pending`, `Normal`, `NoData`, `Error`, `Recovering`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub active_at: Option<String>,
    #[serde(default)]
    pub value: String,
}

/// An alert instance from the Grafana Alertmanager (`/api/alertmanager/grafana/api/v2/alerts`).
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AmAlert {
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub starts_at: String,
    #[serde(default)]
    pub status: AmAlertStatus,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AmAlertStatus {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub silenced_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Matcher {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub is_regex: bool,
    #[serde(default = "default_true")]
    pub is_equal: bool,
}

fn default_true() -> bool {
    true
}

impl Matcher {
    pub fn eq(name: &str, value: &str) -> Self {
        Self { name: name.into(), value: value.into(), is_regex: false, is_equal: true }
    }

    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        let v = labels.get(&self.name).map(String::as_str).unwrap_or("");
        let hit = if self.is_regex {
            regex::Regex::new(&format!("^(?:{})$", self.value)).map(|r| r.is_match(v)).unwrap_or(false)
        } else {
            v == self.value
        };
        hit == self.is_equal
    }
}

impl std::fmt::Display for Matcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let op = match (self.is_equal, self.is_regex) {
            (true, false) => "=",
            (false, false) => "!=",
            (true, true) => "=~",
            (false, true) => "!~",
        };
        write!(f, "{}{op}\"{}\"", self.name, self.value)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Silence {
    pub id: String,
    #[serde(default)]
    pub matchers: Vec<Matcher>,
    #[serde(default)]
    pub starts_at: String,
    #[serde(default)]
    pub ends_at: String,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub status: SilenceStatus,
}

impl Silence {
    pub fn is_active(&self) -> bool {
        self.status.state == "active"
    }

    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        !self.matchers.is_empty() && self.matchers.iter().all(|m| m.matches(labels))
    }
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct SilenceStatus {
    #[serde(default)]
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSilence {
    pub matchers: Vec<Matcher>,
    pub starts_at: String,
    pub ends_at: String,
    pub created_by: String,
    pub comment: String,
}

// ---- annotations ------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Annotation {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub dashboard_uid: Option<String>,
    #[serde(default)]
    pub panel_id: Option<i64>,
    #[serde(default)]
    pub time: i64,
    #[serde(default)]
    pub time_end: i64,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub alert_name: Option<String>,
    #[serde(default)]
    pub new_state: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewAnnotation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dashboard_uid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panel_id: Option<i64>,
    pub time: i64,
    pub time_end: i64,
    pub tags: Vec<String>,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn parses_fixtures() {
        let hits: Vec<SearchHit> = serde_json::from_str(&fixture("search.json")).unwrap();
        assert!(hits.iter().any(|h| h.is_folder() && h.title == "Platform"));
        let rules: RulesResponse = serde_json::from_str(&fixture("rules.json")).unwrap();
        let all: Vec<&Rule> = rules.data.groups.iter().flat_map(|g| &g.rules).collect();
        assert!(all.iter().any(|r| r.name == "AlwaysFiring" && r.state == "firing" && !r.alerts.is_empty()));
        assert!(all.iter().all(|r| r.queried_datasource_uids == vec!["prometheus".to_string()]));
        let am: Vec<AmAlert> = serde_json::from_str(&fixture("am_alerts.json")).unwrap();
        assert!(!am.is_empty());
        let v: VersionList = serde_json::from_str(&fixture("versions.json")).unwrap();
        assert_eq!(v.versions.len(), 3);
        let ds: Vec<Datasource> = serde_json::from_str(&fixture("datasources.json")).unwrap();
        assert!(ds.iter().any(|d| d.is_default && d.kind == "prometheus"));
        let a: Vec<Annotation> = serde_json::from_str(&fixture("annotations.json")).unwrap();
        assert!(!a.is_empty());
        let d: DashboardResponse = serde_json::from_str(&fixture("dashboard_demo.json")).unwrap();
        assert_eq!(d.dashboard["uid"], "lg-demo-signals");
    }

    #[test]
    fn matchers() {
        let labels: BTreeMap<String, String> = [("alertname".to_string(), "X".to_string())].into();
        assert!(Matcher::eq("alertname", "X").matches(&labels));
        let re = Matcher { name: "alertname".into(), value: "X|Y".into(), is_regex: true, is_equal: true };
        assert!(re.matches(&labels));
        let ne = Matcher { name: "team".into(), value: "a".into(), is_regex: false, is_equal: false };
        assert!(ne.matches(&labels));
        assert_eq!(re.to_string(), "alertname=~\"X|Y\"");
    }
}
