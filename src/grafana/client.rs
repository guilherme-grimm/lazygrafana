//! Hand-written Grafana HTTP API client.

use std::fmt;
use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::dataframe::Frame;
use super::models::*;
use crate::config::Context;
use crate::timerange::Millis;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: Option<u16>,
    pub message: String,
}

impl ApiError {
    fn new(status: Option<u16>, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }

    pub fn is_not_found(&self) -> bool {
        self.status == Some(404)
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(401) => write!(f, "unauthorized (401): check the context's token — {}", self.message),
            Some(403) => write!(f, "forbidden (403): the token lacks permission — {}", self.message),
            Some(s) => write!(f, "HTTP {s}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for ApiError {}

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    org: Option<i64>,
    token: Option<String>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the token.
        f.debug_struct("Client").field("base", &self.base).field("org", &self.org).finish_non_exhaustive()
    }
}

/// One query in a `/api/ds/query` request: the raw target plus what we add.
#[derive(Debug, Clone)]
pub struct QueryRequest {
    pub from: Millis,
    pub to: Millis,
    pub queries: Vec<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub frames: Vec<Frame>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct QueryResponse {
    /// refId -> result, in request order.
    pub results: Vec<(String, QueryResult)>,
}

impl QueryResponse {
    pub fn frames(&self) -> Vec<Frame> {
        self.results.iter().flat_map(|(_, r)| r.frames.iter().cloned()).collect()
    }

    pub fn errors(&self) -> Vec<String> {
        self.results.iter().filter_map(|(id, r)| r.error.as_ref().map(|e| format!("{id}: {e}"))).collect()
    }

    pub fn parse(v: &Value, order: &[String]) -> Self {
        let mut results = Vec::new();
        let Some(map) = v.get("results").and_then(Value::as_object) else { return Self { results } };
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort_by_key(|k| order.iter().position(|o| o == *k).unwrap_or(usize::MAX));
        for k in keys {
            let r = &map[k];
            let frames = r
                .get("frames")
                .and_then(Value::as_array)
                .map(|fs| fs.iter().filter_map(|f| Frame::from_json(f).ok()).filter(|f| !f.fields.is_empty()).collect())
                .unwrap_or_default();
            let error = r.get("error").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
            results.push((k.clone(), QueryResult { frames, error }));
        }
        Self { results }
    }
}

impl Client {
    pub fn new(ctx: &Context) -> Result<Self, ApiError> {
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("lazygrafana/", env!("CARGO_PKG_VERSION")))
            .danger_accept_invalid_certs(ctx.insecure_skip_verify);
        if let Some(ca) = &ctx.ca_file {
            let pem = std::fs::read(ca).map_err(|e| ApiError::new(None, format!("reading ca_file {}: {e}", ca.display())))?;
            let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| ApiError::new(None, format!("invalid ca_file: {e}")))?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder.build().map_err(|e| ApiError::new(None, format!("building HTTP client: {e}")))?;
        Ok(Self { http, base: ctx.url.clone(), org: ctx.org, token: ctx.token.clone() })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(org) = self.org {
            req = req.header("X-Grafana-Org-Id", org.to_string());
        }
        req
    }

    async fn send(&self, method: Method, path: &str, body: Option<&Value>) -> ApiResult<reqwest::Response> {
        for attempt in 0..3 {
            let mut req = self.request(method.clone(), path);
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(|e| ApiError::new(None, describe_reqwest(&e)))?;
            if resp.status() == StatusCode::TOO_MANY_REQUESTS && attempt < 2 {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(1)
                    .min(10);
                tracing::warn!(path, wait, "rate limited, retrying");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            return Ok(resp);
        }
        unreachable!("loop always returns")
    }

    async fn call<T: DeserializeOwned>(&self, method: Method, path: &str, body: Option<&Value>) -> ApiResult<T> {
        let resp = self.send(method, path, body).await?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| ApiError::new(Some(status.as_u16()), e.to_string()))?;
        // ds/query returns 207/400 with per-query errors in the body; let the caller see them.
        let partial_ok = path.starts_with("/api/ds/query") && (status.as_u16() == 207 || status.is_client_error());
        if !status.is_success() && !partial_ok {
            return Err(ApiError::new(Some(status.as_u16()), error_message(&bytes, status)));
        }
        let parse_target: &[u8] = if bytes.is_empty() { b"null" } else { &bytes };
        serde_json::from_slice(parse_target).map_err(|e| {
            if !status.is_success() {
                ApiError::new(Some(status.as_u16()), error_message(&bytes, status))
            } else {
                ApiError::new(Some(status.as_u16()), format!("unexpected response from {path}: {e}"))
            }
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> ApiResult<T> {
        self.call(Method::GET, path, None).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> ApiResult<T> {
        self.call(Method::POST, path, Some(body)).await
    }

    // ---- instance ---------------------------------------------------------------

    pub async fn health(&self) -> ApiResult<Health> {
        self.get("/api/health").await
    }

    pub async fn frontend_settings(&self) -> ApiResult<FrontendSettings> {
        self.get("/api/frontend/settings").await
    }

    pub async fn user(&self) -> ApiResult<User> {
        self.get("/api/user").await
    }

    // ---- dashboards ---------------------------------------------------------------

    pub async fn search(&self, query: &str, starred: bool) -> ApiResult<Vec<SearchHit>> {
        let mut path = format!("/api/search?limit=5000&query={}", urlencoding::encode(query));
        if starred {
            path.push_str("&starred=true");
        }
        self.get(&path).await
    }

    pub async fn folders(&self, parent: Option<&str>) -> ApiResult<Vec<Folder>> {
        let path = match parent {
            Some(p) => format!("/api/folders?limit=1000&parentUid={}", urlencoding::encode(p)),
            None => "/api/folders?limit=1000".to_string(),
        };
        self.get(&path).await
    }

    pub async fn folder(&self, uid: &str) -> ApiResult<Folder> {
        self.get(&format!("/api/folders/{}", urlencoding::encode(uid))).await
    }

    pub async fn dashboard(&self, uid: &str) -> ApiResult<DashboardResponse> {
        self.get(&format!("/api/dashboards/uid/{}", urlencoding::encode(uid))).await
    }

    pub async fn save_dashboard(&self, dashboard: &Value, folder_uid: Option<&str>, message: &str) -> ApiResult<SaveResponse> {
        let mut body = json!({ "dashboard": dashboard, "overwrite": true, "message": message });
        if let Some(f) = folder_uid {
            body["folderUid"] = json!(f);
        }
        self.post("/api/dashboards/db", &body).await
    }

    pub async fn delete_dashboard(&self, uid: &str) -> ApiResult<Value> {
        self.call(Method::DELETE, &format!("/api/dashboards/uid/{}", urlencoding::encode(uid)), None).await
    }

    pub async fn versions(&self, uid: &str) -> ApiResult<Vec<VersionInfo>> {
        let v: VersionList = self.get(&format!("/api/dashboards/uid/{}/versions?limit=100", urlencoding::encode(uid))).await?;
        Ok(v.versions)
    }

    pub async fn version(&self, uid: &str, id: i64) -> ApiResult<VersionFull> {
        self.get(&format!("/api/dashboards/uid/{}/versions/{id}", urlencoding::encode(uid))).await
    }

    pub async fn restore_version(&self, uid: &str, version: i64) -> ApiResult<Value> {
        self.post(&format!("/api/dashboards/uid/{}/restore", urlencoding::encode(uid)), &json!({ "version": version })).await
    }

    pub async fn set_star(&self, uid: &str, starred: bool) -> ApiResult<Value> {
        let method = if starred { Method::POST } else { Method::DELETE };
        self.call(method, &format!("/api/user/stars/dashboard/uid/{}", urlencoding::encode(uid)), None).await
    }

    /// Renders a panel to PNG with the Grafana Image Renderer. `extra` is appended to the
    /// query string (e.g. `var-…` parameters); the local timezone is passed along.
    pub async fn render_panel(
        &self,
        uid: &str,
        slug: &str,
        panel_id: i64,
        range: (&str, &str),
        size: (u32, u32),
        extra: &str,
    ) -> ApiResult<Vec<u8>> {
        let tz = iana_time_zone::get_timezone().unwrap_or_else(|_| "browser".into());
        let mut path = format!(
            "/render/d-solo/{uid}/{slug}?panelId={panel_id}&from={}&to={}&width={}&height={}&theme=dark&tz={}",
            urlencoding::encode(range.0),
            urlencoding::encode(range.1),
            size.0,
            size.1,
            urlencoding::encode(&tz)
        );
        if !extra.is_empty() {
            path.push('&');
            path.push_str(extra);
        }
        let resp = self.send(Method::GET, &path, None).await?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| ApiError::new(Some(status.as_u16()), e.to_string()))?;
        if !status.is_success() || bytes.is_empty() {
            return Err(ApiError::new(
                Some(status.as_u16()),
                if bytes.is_empty() {
                    "renderer returned no image (is the Grafana Image Renderer installed?)".into()
                } else {
                    error_message(&bytes, status)
                },
            ));
        }
        Ok(bytes.to_vec())
    }

    // ---- queries ----------------------------------------------------------------

    pub async fn query(&self, req: &QueryRequest) -> ApiResult<QueryResponse> {
        let order: Vec<String> = req.queries.iter().filter_map(|q| q.get("refId").and_then(Value::as_str).map(str::to_string)).collect();
        let body = json!({ "from": req.from.to_string(), "to": req.to.to_string(), "queries": req.queries });
        let v: Value = self.post("/api/ds/query", &body).await?;
        if v.get("results").is_none() {
            let msg = v.get("message").and_then(Value::as_str).unwrap_or("query failed").to_string();
            return Err(ApiError::new(None, msg));
        }
        Ok(QueryResponse::parse(&v, &order))
    }

    pub async fn datasources(&self) -> ApiResult<Vec<Datasource>> {
        self.get("/api/datasources").await
    }

    pub async fn datasource_health(&self, uid: &str) -> ApiResult<DatasourceHealth> {
        let resp = self.send(Method::GET, &format!("/api/datasources/uid/{}/health", urlencoding::encode(uid)), None).await?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| ApiError::new(Some(status.as_u16()), e.to_string()))?;
        // A failing health check is a 400 with a normal body.
        serde_json::from_slice(&bytes).map_err(|_| ApiError::new(Some(status.as_u16()), error_message(&bytes, status)))
    }

    /// `GET /api/datasources/uid/:uid/resources/<path>` (label values etc.).
    pub async fn ds_resource(&self, uid: &str, path: &str) -> ApiResult<Value> {
        self.get(&format!("/api/datasources/uid/{}/resources/{path}", urlencoding::encode(uid))).await
    }

    // ---- alerting -----------------------------------------------------------------

    pub async fn rules(&self) -> ApiResult<Vec<RuleGroup>> {
        let r: RulesResponse = self.get("/api/prometheus/grafana/api/v1/rules").await?;
        Ok(r.data.groups)
    }

    pub async fn am_alerts(&self) -> ApiResult<Vec<AmAlert>> {
        self.get("/api/alertmanager/grafana/api/v2/alerts").await
    }

    pub async fn silences(&self) -> ApiResult<Vec<Silence>> {
        self.get("/api/alertmanager/grafana/api/v2/silences").await
    }

    pub async fn create_silence(&self, s: &NewSilence) -> ApiResult<String> {
        let v: Value = self.post("/api/alertmanager/grafana/api/v2/silences", &serde_json::to_value(s).expect("serializable")).await?;
        Ok(v.get("silenceID").and_then(Value::as_str).unwrap_or_default().to_string())
    }

    pub async fn expire_silence(&self, id: &str) -> ApiResult<Value> {
        self.call(Method::DELETE, &format!("/api/alertmanager/grafana/api/v2/silence/{}", urlencoding::encode(id)), None).await
    }

    pub async fn provisioned_rule(&self, uid: &str) -> ApiResult<Value> {
        self.get(&format!("/api/v1/provisioning/alert-rules/{}", urlencoding::encode(uid))).await
    }

    /// Updates a rule. `keep_ui_editable` sends `X-Disable-Provenance` so rules created in the UI
    /// stay editable there; it must be false for rules provisioned through the API.
    pub async fn update_provisioned_rule(&self, uid: &str, rule: &Value, keep_ui_editable: bool) -> ApiResult<Value> {
        let mut req = self.request(Method::PUT, &format!("/api/v1/provisioning/alert-rules/{}", urlencoding::encode(uid)));
        if keep_ui_editable {
            req = req.header("X-Disable-Provenance", "true");
        }
        let resp = req.json(rule).send().await.map_err(|e| ApiError::new(None, describe_reqwest(&e)))?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| ApiError::new(Some(status.as_u16()), e.to_string()))?;
        if !status.is_success() {
            return Err(ApiError::new(Some(status.as_u16()), error_message(&bytes, status)));
        }
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Alert state history as a data frame (needs state history enabled; not on every instance).
    pub async fn rule_history(&self, uid: &str, from: Millis, to: Millis) -> ApiResult<Frame> {
        let v: Value = self
            .get(&format!("/api/v1/rules/history?ruleUID={}&from={}&to={}&limit=200", urlencoding::encode(uid), from / 1000, to / 1000))
            .await?;
        Frame::from_json(&v).map_err(|e| ApiError::new(None, e))
    }

    // ---- annotations ------------------------------------------------------------------

    pub async fn annotations(&self, dashboard_uid: Option<&str>, from: Millis, to: Millis) -> ApiResult<Vec<Annotation>> {
        let mut path = format!("/api/annotations?from={from}&to={to}&limit=500");
        if let Some(uid) = dashboard_uid {
            path.push_str(&format!("&dashboardUID={}", urlencoding::encode(uid)));
        }
        self.get(&path).await
    }

    pub async fn create_annotation(&self, a: &NewAnnotation) -> ApiResult<Value> {
        self.post("/api/annotations", &serde_json::to_value(a).expect("serializable")).await
    }
}

fn error_message(bytes: &[u8], status: StatusCode) -> String {
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        for key in ["message", "error", "msg"] {
            if let Some(m) = v.get(key).and_then(Value::as_str)
                && !m.is_empty()
            {
                return m.to_string();
            }
        }
    }
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.is_empty() || text.starts_with('<') {
        status.canonical_reason().unwrap_or("request failed").to_string()
    } else {
        text.chars().take(300).collect()
    }
}

fn describe_reqwest(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "request timed out".into();
    }
    if e.is_connect() {
        let url = e.url().map(|u| format!("{}://{}", u.scheme(), u.authority())).unwrap_or_default();
        return format!("cannot connect to {url}");
    }
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        msg = format!("{msg}: {s}");
        src = s.source();
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ctx(url: &str) -> Context {
        Context {
            name: "t".into(),
            url: url.into(),
            org: Some(2),
            token: Some("secret".into()),
            token_error: None,
            readonly: false,
            cloud: false,
            ca_file: None,
            insecure_skip_verify: false,
        }
    }

    #[tokio::test]
    async fn sends_auth_and_org_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/health"))
            .and(header("authorization", "Bearer secret"))
            .and(header("x-grafana-org-id", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"version": "13.2.4", "database": "ok"})))
            .mount(&server)
            .await;
        let c = Client::new(&ctx(&server.uri())).unwrap();
        assert_eq!(c.health().await.unwrap().version, "13.2.4");
        assert!(!format!("{c:?}").contains("secret"));
    }

    #[tokio::test]
    async fn maps_errors() {
        let server = MockServer::start().await;
        Mock::given(path("/api/dashboards/uid/x"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Dashboard not found"})))
            .mount(&server)
            .await;
        Mock::given(path("/api/user"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({"message": "invalid API key"})))
            .mount(&server)
            .await;
        let c = Client::new(&ctx(&server.uri())).unwrap();
        let e = c.dashboard("x").await.unwrap_err();
        assert!(e.is_not_found() && e.message == "Dashboard not found");
        let e = c.user().await.unwrap_err();
        assert!(e.to_string().contains("unauthorized"));
    }

    #[tokio::test]
    async fn retries_rate_limits() {
        let server = MockServer::start().await;
        Mock::given(path("/api/health"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/api/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"version": "11.6.0"})))
            .mount(&server)
            .await;
        let c = Client::new(&ctx(&server.uri())).unwrap();
        assert_eq!(c.health().await.unwrap().version, "11.6.0");
    }

    #[tokio::test]
    async fn query_partial_errors() {
        let server = MockServer::start().await;
        let body: Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/query_bad_expr.json")).unwrap(),
        )
        .unwrap();
        Mock::given(path("/api/ds/query")).respond_with(ResponseTemplate::new(400).set_body_json(body)).mount(&server).await;
        let c = Client::new(&ctx(&server.uri())).unwrap();
        let r = c.query(&QueryRequest { from: 0, to: 1, queries: vec![json!({"refId": "A"})] }).await.unwrap();
        assert_eq!(r.errors().len(), 1);
        assert!(r.errors()[0].contains("parse error"));
    }

    #[tokio::test]
    async fn connection_errors_are_readable() {
        let c = Client::new(&ctx("http://127.0.0.1:1")).unwrap();
        let e = c.health().await.unwrap_err();
        assert!(e.message.contains("cannot connect"), "{e}");
    }
}
