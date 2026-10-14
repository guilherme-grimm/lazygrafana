//! Results of background tasks, delivered to the UI loop.

use crate::grafana::dataframe::Frame;
use crate::grafana::models::*;
use crate::grafana::variables::VarOption;
use crate::grafana::{ApiError, QueryResponse};
use crate::render::chart::ChartAnnotation;
use crate::timerange::Millis;

#[derive(Debug, Clone)]
pub struct InstanceInfo {
    pub version: String,
    pub edition: String,
    pub user: Option<User>,
    pub app_url: String,
}

/// What to do after a mutating API call succeeds.
#[derive(Debug, Clone)]
pub enum After {
    Nothing,
    ReloadDashboards,
    ReloadDashboard(String),
    CloseDashboard,
    ReloadAlerts,
    ReloadAnnotations,
}

#[derive(Debug)]
pub enum Msg {
    Instance {
        ctx: String,
        result: Result<InstanceInfo, ApiError>,
    },
    ContextHealth {
        ctx: String,
        result: Result<String, String>,
    },
    Search {
        ctx_gen: u64,
        result: Result<Vec<SearchHit>, ApiError>,
    },
    Datasources {
        ctx_gen: u64,
        result: Result<Vec<Datasource>, ApiError>,
    },
    Dashboard {
        generation: u64,
        result: Result<DashboardResponse, ApiError>,
    },
    VarOptions {
        generation: u64,
        name: String,
        rest: Vec<String>,
        then_query: bool,
        result: Result<Vec<VarOption>, ApiError>,
    },
    PanelData {
        generation: u64,
        query_gen: u64,
        panel_id: i64,
        range: (Millis, Millis),
        step: Millis,
        result: Result<QueryResponse, ApiError>,
    },
    Annotations {
        generation: u64,
        items: Vec<ChartAnnotation>,
    },
    Versions {
        generation: u64,
        result: Result<Vec<VersionInfo>, ApiError>,
    },
    Diff {
        title: String,
        result: Result<(String, String), ApiError>,
    },
    ExactRender {
        generation: u64,
        panel_id: i64,
        result: Result<Vec<u8>, ApiError>,
    },
    EditFetched {
        uid: String,
        result: Result<DashboardResponse, ApiError>,
        purpose: FetchPurpose,
    },
    Rules {
        ctx_gen: u64,
        result: Result<Vec<RuleGroup>, ApiError>,
    },
    Silences {
        ctx_gen: u64,
        result: Result<Vec<Silence>, ApiError>,
    },
    AmAlerts {
        ctx_gen: u64,
        result: Result<Vec<AmAlert>, ApiError>,
    },
    RuleHistory {
        uid: String,
        result: Result<Frame, ApiError>,
    },
    RuleChart {
        uid: String,
        range: (Millis, Millis),
        result: Result<QueryResponse, ApiError>,
    },
    Explore {
        generation: u64,
        range: (Millis, Millis),
        step: Millis,
        append: bool,
        result: Result<QueryResponse, ApiError>,
    },
    LokiLabels {
        result: Result<Vec<String>, ApiError>,
        then: LogsJump,
    },
    DsHealth {
        uid: String,
        result: Result<DatasourceHealth, ApiError>,
    },
    Done {
        ok: String,
        after: After,
        result: Result<(), ApiError>,
    },
}

#[derive(Debug, Clone)]
pub enum FetchPurpose {
    Edit,
    Move { folder_uid: Option<String>, folder_title: String },
    YankJson,
    Export,
}

/// Pending "logs for this selection" jump, waiting for Loki's label names.
#[derive(Debug, Clone)]
pub struct LogsJump {
    pub ds_uid: String,
    pub labels: Vec<(String, String)>,
    pub range: (Millis, Millis),
}
