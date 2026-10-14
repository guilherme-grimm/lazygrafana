//! Dashboard template variables: parsing, option resolution (incl. query
//! variables against Prometheus / Loki), and client-side interpolation with
//! Grafana's formatting rules.

use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::client::{ApiError, ApiResult, Client};
use super::dashboard::DsRef;
use super::models::Datasource;
use crate::timerange::{Millis, format_duration, nice_step, parse_duration};

pub const ALL_VALUE: &str = "$__all";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VarKind {
    Query,
    Custom,
    Constant,
    Textbox,
    Interval,
    Datasource,
    AdHoc,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarOption {
    pub text: String,
    pub value: String,
}

impl VarOption {
    fn same(v: &str) -> Self {
        Self { text: v.to_string(), value: v.to_string() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VariableDef {
    pub name: String,
    pub label: Option<String>,
    pub kind: VarKind,
    /// The query as a string (object queries are converted to their classic form).
    pub query: String,
    pub datasource: Option<DsRef>,
    pub multi: bool,
    pub include_all: bool,
    pub all_value: Option<String>,
    pub regex: Option<String>,
    pub sort: i64,
    /// 0 = shown, 1 = label hidden, 2 = variable hidden.
    pub hide: i64,
    /// 1 = on dashboard load, 2 = on time range change.
    pub refresh: i64,
    pub current: Vec<String>,
    pub auto: bool,
    pub auto_count: u32,
    pub auto_min: String,
}

impl VariableDef {
    pub fn from_json(v: &Value) -> Option<Self> {
        let name = v.get("name")?.as_str()?.to_string();
        let kind = match v.get("type").and_then(Value::as_str).unwrap_or("query") {
            "query" => VarKind::Query,
            "custom" => VarKind::Custom,
            "constant" => VarKind::Constant,
            "textbox" => VarKind::Textbox,
            "interval" => VarKind::Interval,
            "datasource" => VarKind::Datasource,
            "adhoc" => VarKind::AdHoc,
            other => VarKind::Other(other.to_string()),
        };
        let query = match v.get("query") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Object(o)) => object_query_to_string(o),
            _ => v.get("definition").and_then(Value::as_str).unwrap_or_default().to_string(),
        };
        let current = match v.pointer("/current/value") {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
            _ => Vec::new(),
        };
        let regex = v.get("regex").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        Some(Self {
            name,
            label: v.get("label").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
            kind,
            query,
            datasource: v.get("datasource").and_then(DsRef::from_json),
            multi: v.get("multi").and_then(Value::as_bool).unwrap_or(false),
            include_all: v.get("includeAll").and_then(Value::as_bool).unwrap_or(false),
            all_value: v.get("allValue").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
            regex,
            sort: v.get("sort").and_then(Value::as_i64).unwrap_or(0),
            hide: v.get("hide").and_then(Value::as_i64).unwrap_or(0),
            refresh: v.get("refresh").and_then(Value::as_i64).unwrap_or(1),
            current,
            auto: v.get("auto").and_then(Value::as_bool).unwrap_or(false),
            auto_count: v.get("auto_count").and_then(Value::as_u64).unwrap_or(30) as u32,
            auto_min: v.get("auto_min").and_then(Value::as_str).unwrap_or("10s").to_string(),
        })
    }

    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }

    /// Options that don't need a datasource round-trip.
    pub fn static_options(&self) -> Vec<VarOption> {
        match self.kind {
            VarKind::Custom => split_custom(&self.query),
            VarKind::Constant | VarKind::Textbox => vec![VarOption::same(&self.query)],
            VarKind::Interval => {
                let mut opts: Vec<VarOption> =
                    self.query.split(',').map(str::trim).filter(|s| !s.is_empty()).map(VarOption::same).collect();
                if self.auto {
                    opts.insert(0, VarOption { text: "auto".into(), value: "$__auto_interval".into() });
                }
                opts
            }
            _ => Vec::new(),
        }
    }

    pub fn needs_fetch(&self) -> bool {
        matches!(self.kind, VarKind::Query | VarKind::Datasource)
    }
}

/// Converts the editor's object form of a query into the classic string form.
fn object_query_to_string(o: &serde_json::Map<String, Value>) -> String {
    if let Some(q) = o.get("query").and_then(Value::as_str)
        && !q.is_empty()
    {
        return q.to_string();
    }
    let s = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    // Prometheus: qryType 0 label names, 1 label values, 2 metric names, 3 query result.
    if let Some(t) = o.get("qryType").and_then(Value::as_i64) {
        return match t {
            0 => "label_names()".into(),
            1 if s("metric").is_empty() => format!("label_values({})", s("label")),
            1 => format!("label_values({}, {})", s("metric"), s("label")),
            2 => format!("metrics({})", s("metric")),
            3 => format!("query_result({})", s("varQuery")),
            _ => String::new(),
        };
    }
    // Loki: type 0 label names, 1 label values.
    if let Some(t) = o.get("type").and_then(Value::as_i64) {
        return match t {
            1 if s("stream").is_empty() => format!("label_values({})", s("label")),
            1 => format!("label_values({}, {})", s("stream"), s("label")),
            _ => "label_names()".into(),
        };
    }
    String::new()
}

fn split_custom(q: &str) -> Vec<VarOption> {
    static SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:\\,|[^,])+").expect("valid regex"));
    SPLIT
        .find_iter(q)
        .map(|m| m.as_str().trim().replace("\\,", ","))
        .filter(|s| !s.is_empty())
        .map(|s| match s.split_once(" : ") {
            Some((t, v)) => VarOption { text: t.trim().into(), value: v.trim().into() },
            None => VarOption::same(&s),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct VarState {
    pub def: VariableDef,
    pub options: Vec<VarOption>,
    pub selected: Vec<String>,
    pub error: Option<String>,
    pub loaded: bool,
}

impl VarState {
    pub fn is_all(&self) -> bool {
        self.selected.iter().any(|s| s == ALL_VALUE || s == "All")
    }

    /// Selected values with "All" expanded (empty when All has a custom allValue).
    pub fn values(&self) -> Vec<String> {
        if self.is_all() {
            return self.options.iter().map(|o| o.value.clone()).collect();
        }
        self.selected.clone()
    }

    pub fn texts(&self) -> Vec<String> {
        if self.is_all() {
            return vec!["All".into()];
        }
        self.selected
            .iter()
            .map(|v| self.options.iter().find(|o| &o.value == v).map(|o| o.text.clone()).unwrap_or_else(|| v.clone()))
            .collect()
    }

    pub fn display(&self) -> String {
        let t = self.texts();
        if t.is_empty() { "—".into() } else { t.join(" + ") }
    }

    /// Choices shown in the picker, including the All option.
    pub fn choices(&self) -> Vec<VarOption> {
        let mut c = Vec::new();
        if self.def.include_all {
            c.push(VarOption { text: "All".into(), value: ALL_VALUE.into() });
        }
        c.extend(self.options.iter().cloned());
        c
    }

    /// Replace options, keeping the selection where it still makes sense.
    pub fn set_options(&mut self, options: Vec<VarOption>) {
        self.options = options;
        self.loaded = true;
        self.error = None;
        let valid = |v: &String| v == ALL_VALUE || self.options.iter().any(|o| &o.value == v || &o.text == v);
        let kept: Vec<String> = self.selected.iter().filter(|v| valid(v)).cloned().collect();
        self.selected = if self.def.kind == VarKind::Constant {
            self.options.iter().map(|o| o.value.clone()).take(1).collect()
        } else if !kept.is_empty() {
            kept
        } else if self.def.kind == VarKind::Textbox && !self.selected.is_empty() {
            self.selected.clone()
        } else if let Some(first) = self.options.first() {
            vec![first.value.clone()]
        } else if self.def.include_all {
            vec![ALL_VALUE.into()]
        } else {
            Vec::new()
        };
    }
}

/// Context for interpolating built-ins that the client must resolve.
#[derive(Debug, Clone, Copy)]
pub struct Scope {
    pub from: Millis,
    pub to: Millis,
}

#[derive(Debug, Clone, Default)]
pub struct VarSet {
    pub vars: Vec<VarState>,
}

impl VarSet {
    pub fn new(defs: &[VariableDef]) -> Self {
        let vars = defs
            .iter()
            .map(|d| {
                let mut st = VarState { def: d.clone(), options: Vec::new(), selected: d.current.clone(), error: None, loaded: false };
                if !d.needs_fetch() {
                    let opts = d.static_options();
                    if d.kind == VarKind::Textbox && !d.current.is_empty() {
                        st.options = opts;
                        st.loaded = true;
                    } else {
                        st.set_options(opts);
                    }
                }
                st
            })
            .collect();
        Self { vars }
    }

    pub fn get(&self, name: &str) -> Option<&VarState> {
        self.vars.iter().find(|v| v.def.name == name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut VarState> {
        self.vars.iter_mut().find(|v| v.def.name == name)
    }

    /// Names of variables referenced by `text`.
    pub fn references(&self, text: &str) -> BTreeSet<String> {
        VAR_RE
            .captures_iter(text)
            .filter_map(|c| c.get(1).or(c.get(2)).or(c.get(4)).map(|m| m.as_str().to_string()))
            .filter(|n| self.get(n).is_some())
            .collect()
    }

    /// Variables that need fetching, in dependency order (a variable after those it references).
    pub fn fetch_order(&self) -> Vec<String> {
        let mut order = Vec::new();
        let mut done = BTreeSet::new();
        // Bounded passes resolve chains; cycles are appended at the end.
        for _ in 0..self.vars.len() + 1 {
            for v in self.vars.iter().filter(|v| v.def.needs_fetch()) {
                if done.contains(&v.def.name) {
                    continue;
                }
                let mut deps = self.references(&v.def.query);
                if let Some(uid) = v.def.datasource.as_ref().and_then(|d| d.uid.as_ref()) {
                    deps.extend(self.references(uid));
                }
                deps.remove(&v.def.name);
                if deps.iter().all(|d| done.contains(d) || !self.get(d).is_some_and(|x| x.def.needs_fetch())) {
                    done.insert(v.def.name.clone());
                    order.push(v.def.name.clone());
                }
            }
        }
        for v in self.vars.iter().filter(|v| v.def.needs_fetch() && !done.contains(&v.def.name)) {
            order.push(v.def.name.clone());
        }
        order
    }

    /// Variables (transitively) depending on `name`.
    pub fn dependents(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut frontier = vec![name.to_string()];
        while let Some(n) = frontier.pop() {
            for v in &self.vars {
                let refs = self.references(&v.def.query);
                let ds_refs = v.def.datasource.as_ref().and_then(|d| d.uid.as_ref()).map(|u| self.references(u)).unwrap_or_default();
                if (refs.contains(&n) || ds_refs.contains(&n)) && v.def.name != name && !out.contains(&v.def.name) {
                    out.push(v.def.name.clone());
                    frontier.push(v.def.name.clone());
                }
            }
        }
        out
    }

    /// Interpolates `$var`, `${var}`, `${var:format}` and `[[var]]`. `ds_kind` selects
    /// the default multi-value format (Prometheus/Loki use regex alternation).
    pub fn interpolate(&self, text: &str, ds_kind: Option<&str>, scope: Scope) -> String {
        VAR_RE
            .replace_all(text, |c: &regex::Captures| {
                let whole = c.get(0).expect("match").as_str();
                let name = c.get(1).or(c.get(2)).or(c.get(4)).map(|m| m.as_str()).unwrap_or_default();
                let fmt = c.get(3).or(c.get(5)).map(|m| m.as_str());
                match name {
                    "__from" => return format_time_builtin(scope.from, fmt),
                    "__to" => return format_time_builtin(scope.to, fmt),
                    _ => {}
                }
                let Some(var) = self.get(name) else { return whole.to_string() };
                self.format_var(var, fmt, ds_kind, scope)
            })
            .into_owned()
    }

    fn format_var(&self, var: &VarState, fmt: Option<&str>, ds_kind: Option<&str>, scope: Scope) -> String {
        if var.def.kind == VarKind::Interval {
            let v = var.selected.first().cloned().unwrap_or_default();
            if v.starts_with("$__auto_interval") || v == "auto" {
                return auto_interval(&var.def, scope);
            }
            return v;
        }
        if var.is_all()
            && let Some(all) = &var.def.all_value
        {
            return all.clone();
        }
        let values = var.values();
        let multi = var.def.multi || var.def.include_all;
        match fmt {
            Some(f) => format_values(&values, &var.texts(), f, &var.def.name),
            None => match ds_kind {
                Some("prometheus" | "loki") => {
                    if !multi {
                        return values.first().map(|v| prom_regular_escape(v)).unwrap_or_default();
                    }
                    let esc: Vec<String> = values.iter().map(|v| prom_regex_escape(v)).collect();
                    match esc.len() {
                        0 => String::new(),
                        1 => esc[0].clone(),
                        _ => format!("({})", esc.join("|")),
                    }
                }
                _ if values.len() > 1 => format!("{{{}}}", values.join(",")),
                _ => values.first().cloned().unwrap_or_default(),
            },
        }
    }

    /// `var-name=value` query parameters for Grafana URLs.
    pub fn url_params(&self) -> String {
        let mut parts = Vec::new();
        for v in &self.vars {
            if matches!(v.def.kind, VarKind::Constant | VarKind::AdHoc) {
                continue;
            }
            for s in &v.selected {
                let s = if s == ALL_VALUE { "$__all" } else { s.as_str() };
                parts.push(format!("var-{}={}", urlencoding::encode(&v.def.name), urlencoding::encode(s)));
            }
        }
        parts.join("&")
    }
}

static VAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$(\w+)|\$\{(\w+)(?::([\w:-]+))?\}|\[\[(\w+)(?::([\w:-]+))?\]\]").expect("valid regex"));

fn format_time_builtin(ms: Millis, fmt: Option<&str>) -> String {
    match fmt {
        Some("date") | Some("date:iso") => chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339()).unwrap_or_default(),
        Some("date:seconds") => (ms / 1000).to_string(),
        _ => ms.to_string(),
    }
}

fn auto_interval(def: &VariableDef, scope: Scope) -> String {
    let min = parse_duration(&def.auto_min).unwrap_or(10_000);
    format_duration(nice_step(scope.to - scope.from, def.auto_count.max(1), min))
}

/// Prometheus string escaping for single values (`\` and `'`).
fn prom_regular_escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Prometheus regex escaping for values used in `=~` (double-escaped for the PromQL string).
fn prom_regex_escape(v: &str) -> String {
    let mut out = String::new();
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\\\\\"),
            '$' | '^' | '*' | '{' | '}' | '[' | ']' | '\'' | '+' | '?' | '.' | '(' | ')' | '|' => {
                out.push_str("\\\\");
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn regex_escape(v: &str) -> String {
    regex::escape(v)
}

pub fn format_values(values: &[String], texts: &[String], fmt: &str, name: &str) -> String {
    let single = values.len() == 1;
    match fmt {
        "csv" | "raw" => values.join(","),
        "pipe" => values.join("|"),
        "regex" => {
            let esc: Vec<String> = values.iter().map(|v| regex_escape(v)).collect();
            if single { esc[0].clone() } else { format!("({})", esc.join("|")) }
        }
        "json" => serde_json::to_string(values).unwrap_or_default(),
        "glob" => {
            if single {
                values[0].clone()
            } else {
                format!("{{{}}}", values.join(","))
            }
        }
        "doublequote" => values.iter().map(|v| format!("\"{}\"", v.replace('"', "\\\""))).collect::<Vec<_>>().join(","),
        "singlequote" => values.iter().map(|v| format!("'{}'", v.replace('\'', "\\'"))).collect::<Vec<_>>().join(","),
        "sqlstring" => values.iter().map(|v| format!("'{}'", v.replace('\'', "''"))).collect::<Vec<_>>().join(","),
        "text" => texts.join(" + "),
        "queryparam" => values.iter().map(|v| format!("var-{name}={}", urlencoding::encode(v))).collect::<Vec<_>>().join("&"),
        "percentencode" => {
            let joined = if single { values[0].clone() } else { format!("{{{}}}", values.join(",")) };
            urlencoding::encode(&joined).into_owned()
        }
        "lucene" => {
            let q: Vec<String> = values.iter().map(|v| format!("\"{}\"", v.replace('"', "\\\""))).collect();
            if single { q[0].clone() } else { format!("({})", q.join(" OR ")) }
        }
        "distributed" => {
            let mut it = values.iter();
            let first = it.next().cloned().unwrap_or_default();
            std::iter::once(first).chain(it.map(|v| format!("{name}={v}"))).collect::<Vec<_>>().join(",")
        }
        _ => values.join(","),
    }
}

// ---- option resolution ------------------------------------------------------

/// Applies the variable's regex (`/pattern/flags`) and sort to raw option values.
pub fn post_process(def: &VariableDef, raw: Vec<String>) -> Vec<VarOption> {
    let re = def.regex.as_deref().and_then(compile_js_regex);
    let mut seen = BTreeSet::new();
    let mut opts: Vec<VarOption> = Vec::new();
    for v in raw {
        let opt = match &re {
            None => VarOption::same(&v),
            Some(re) => {
                let Some(c) = re.captures(&v) else { continue };
                let text = c.name("text").map(|m| m.as_str().to_string());
                let value = c.name("value").map(|m| m.as_str().to_string());
                match (text, value) {
                    (Some(t), Some(v)) => VarOption { text: t, value: v },
                    (Some(t), None) => VarOption::same(&t),
                    (None, Some(v)) => VarOption::same(&v),
                    _ => VarOption::same(c.get(1).map(|m| m.as_str()).unwrap_or_else(|| c.get(0).map(|m| m.as_str()).unwrap_or_default())),
                }
            }
        };
        if seen.insert(opt.value.clone()) {
            opts.push(opt);
        }
    }
    sort_options(&mut opts, def.sort);
    opts
}

fn compile_js_regex(s: &str) -> Option<Regex> {
    let (pattern, flags) = match s.strip_prefix('/').and_then(|r| r.rfind('/').map(|i| (&r[..i], &r[i + 1..]))) {
        Some((p, f)) => (p.to_string(), f.to_string()),
        None => (s.to_string(), String::new()),
    };
    let pattern = pattern.replace("(?<", "(?P<");
    let prefix = if flags.contains('i') { "(?i)" } else { "" };
    Regex::new(&format!("{prefix}{pattern}")).ok()
}

fn sort_options(opts: &mut [VarOption], sort: i64) {
    let num = |s: &str| s.parse::<f64>().unwrap_or(f64::NAN);
    match sort {
        1 => opts.sort_by(|a, b| a.text.cmp(&b.text)),
        2 => opts.sort_by(|a, b| b.text.cmp(&a.text)),
        3 => opts.sort_by(|a, b| num(&a.text).total_cmp(&num(&b.text))),
        4 => opts.sort_by(|a, b| num(&b.text).total_cmp(&num(&a.text))),
        5 | 7 => opts.sort_by_key(|o| o.text.to_lowercase()),
        6 | 8 => opts.sort_by_key(|o| std::cmp::Reverse(o.text.to_lowercase())),
        _ => {}
    }
}

static LABEL_VALUES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*label_values\(\s*(?:(.+?)\s*,\s*)?([a-zA-Z_][a-zA-Z0-9_.]*)\s*\)\s*$").expect("valid regex"));
static LABEL_NAMES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*label_names\(\s*(.*?)\s*\)\s*$").expect("valid regex"));
static METRICS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*metrics\(\s*(.*?)\s*\)\s*$").expect("valid regex"));
static QUERY_RESULT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*query_result\(\s*(.+)\s*\)\s*$").expect("valid regex"));

/// Fetches raw option values for a query or datasource variable.
/// `query` must already be interpolated; `ds` is the resolved datasource.
pub async fn fetch_options(
    client: &Client,
    def: &VariableDef,
    query: &str,
    ds: Option<&Datasource>,
    datasources: &[Datasource],
    from: Millis,
    to: Millis,
) -> ApiResult<Vec<String>> {
    if def.kind == VarKind::Datasource {
        return Ok(datasources.iter().filter(|d| d.kind == def.query).map(|d| d.name.clone()).collect());
    }
    let ds = ds.ok_or_else(|| ApiError { status: None, message: format!("variable ${}: datasource not found", def.name) })?;
    let enc = |s: &str| urlencoding::encode(s).into_owned();
    let unsupported = || ApiError { status: None, message: format!("variable ${}: unsupported query '{query}' for {}", def.name, ds.kind) };
    let strings = |v: &Value| -> Vec<String> {
        v.get("data")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    match ds.kind.as_str() {
        "prometheus" => {
            let (start, end) = (from / 1000, to / 1000);
            if let Some(c) = LABEL_VALUES.captures(query) {
                let label = &c[2];
                let mut path = format!("api/v1/label/{}/values?start={start}&end={end}", enc(label));
                if let Some(sel) = c.get(1) {
                    path.push_str(&format!("&match[]={}", enc(sel.as_str())));
                }
                return Ok(strings(&client.ds_resource(&ds.uid, &path).await?));
            }
            if let Some(c) = LABEL_NAMES.captures(query) {
                let mut path = format!("api/v1/labels?start={start}&end={end}");
                if !c[1].is_empty() {
                    path.push_str(&format!("&match[]={}", enc(&c[1])));
                }
                return Ok(strings(&client.ds_resource(&ds.uid, &path).await?));
            }
            if let Some(c) = METRICS.captures(query) {
                let names = strings(&client.ds_resource(&ds.uid, &format!("api/v1/label/__name__/values?start={start}&end={end}")).await?);
                let re = Regex::new(&c[1]).map_err(|e| ApiError { status: None, message: e.to_string() })?;
                return Ok(names.into_iter().filter(|n| re.is_match(n)).collect());
            }
            if let Some(c) = QUERY_RESULT.captures(query) {
                let v = client.ds_resource(&ds.uid, &format!("api/v1/query?query={}&time={end}", enc(&c[1]))).await?;
                let results = v.pointer("/data/result").and_then(Value::as_array).cloned().unwrap_or_default();
                return Ok(results
                    .iter()
                    .map(|r| {
                        let metric = r.get("metric").and_then(Value::as_object).cloned().unwrap_or_default();
                        let name = metric.get("__name__").and_then(Value::as_str).unwrap_or_default().to_string();
                        let labels: Vec<String> = metric
                            .iter()
                            .filter(|(k, _)| k.as_str() != "__name__")
                            .map(|(k, v)| format!("{k}=\"{}\"", v.as_str().unwrap_or_default()))
                            .collect();
                        let value = r.pointer("/value/1").and_then(Value::as_str).unwrap_or_default();
                        let ts = r.pointer("/value/0").map(|t| t.to_string()).unwrap_or_default();
                        format!("{name}{{{}}} {value} {ts}", labels.join(","))
                    })
                    .collect());
            }
            if query.trim().is_empty() {
                return Ok(Vec::new());
            }
            // Bare selector/metric: Grafana treats it as a series query; list its label sets.
            let v = client.ds_resource(&ds.uid, &format!("api/v1/series?match[]={}&start={start}&end={end}", enc(query))).await?;
            Ok(v.get("data").and_then(Value::as_array).map(|a| a.iter().map(|s| s.to_string()).collect()).unwrap_or_default())
        }
        "loki" => {
            let (start, end) = (from as i128 * 1_000_000, to as i128 * 1_000_000);
            if let Some(c) = LABEL_VALUES.captures(query) {
                let mut path = format!("label/{}/values?start={start}&end={end}", enc(&c[2]));
                if let Some(sel) = c.get(1) {
                    path.push_str(&format!("&query={}", enc(sel.as_str())));
                }
                return Ok(strings(&client.ds_resource(&ds.uid, &path).await?));
            }
            if LABEL_NAMES.is_match(query) {
                return Ok(strings(&client.ds_resource(&ds.uid, &format!("labels?start={start}&end={end}")).await?));
            }
            Err(unsupported())
        }
        _ => Err(unsupported()),
    }
}

/// Resolves a variable's datasource reference (which may itself be a `$variable`).
pub fn resolve_datasource<'a>(r: Option<&DsRef>, vars: &VarSet, datasources: &'a [Datasource], scope: Scope) -> Option<&'a Datasource> {
    resolve_ds(r, vars, datasources, scope)
}

pub fn resolve_ds<'a>(r: Option<&DsRef>, vars: &VarSet, datasources: &'a [Datasource], scope: Scope) -> Option<&'a Datasource> {
    let default = || datasources.iter().find(|d| d.is_default);
    let Some(r) = r else { return default() };
    if let Some(uid) = &r.uid {
        let uid = vars.interpolate(uid, None, scope);
        if uid == "default" {
            return default();
        }
        if let Some(d) = datasources.iter().find(|d| d.uid == uid || d.name == uid) {
            return Some(d);
        }
    }
    match &r.kind {
        Some(kind) => datasources.iter().find(|d| &d.kind == kind && d.is_default).or_else(|| datasources.iter().find(|d| &d.kind == kind)),
        None => default(),
    }
}

/// Values for a datasource variable are names; dashboards often store uids. Map both.
pub fn datasource_options(datasources: &[Datasource], names: Vec<String>) -> Vec<VarOption> {
    let by_name: HashMap<&str, &Datasource> = datasources.iter().map(|d| (d.name.as_str(), d)).collect();
    names
        .into_iter()
        .map(|n| VarOption { value: by_name.get(n.as_str()).map(|d| d.uid.clone()).unwrap_or_else(|| n.clone()), text: n })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SCOPE: Scope = Scope { from: 0, to: 6 * 3_600_000 };

    fn def(v: Value) -> VariableDef {
        VariableDef::from_json(&v).unwrap()
    }

    fn set(defs: Vec<Value>) -> VarSet {
        VarSet::new(&defs.into_iter().map(def).collect::<Vec<_>>())
    }

    #[test]
    fn custom_and_constant() {
        let s = set(vec![
            json!({"name": "shape", "type": "custom", "query": "sine, spike ,sawtooth", "multi": true, "includeAll": true, "current": {"value": ["sine", "spike"]}}),
            json!({"name": "env", "type": "constant", "query": "dev"}),
            json!({"name": "kv", "type": "custom", "query": "Prod : prod-1,Dev : dev-1"}),
        ]);
        assert_eq!(s.interpolate("x{shape=~\"$shape\"}", Some("prometheus"), SCOPE), "x{shape=~\"(sine|spike)\"}");
        assert_eq!(s.interpolate("${env}-[[env]]", None, SCOPE), "dev-dev");
        assert_eq!(s.get("kv").unwrap().options[0], VarOption { text: "Prod".into(), value: "prod-1".into() });
        assert_eq!(s.interpolate("$kv", None, SCOPE), "prod-1");
        assert_eq!(s.interpolate("$unknown $__interval", None, SCOPE), "$unknown $__interval");
    }

    #[test]
    fn formats() {
        let mut s = set(vec![json!({"name": "v", "type": "custom", "query": "a.b,c", "multi": true, "current": {"value": ["a.b", "c"]}})]);
        let i = |s: &VarSet, t: &str| s.interpolate(t, None, SCOPE);
        assert_eq!(i(&s, "${v:csv}"), "a.b,c");
        assert_eq!(i(&s, "${v:pipe}"), "a.b|c");
        assert_eq!(i(&s, "${v:regex}"), "(a\\.b|c)");
        assert_eq!(i(&s, "${v:json}"), "[\"a.b\",\"c\"]");
        assert_eq!(i(&s, "${v:doublequote}"), "\"a.b\",\"c\"");
        assert_eq!(i(&s, "${v:sqlstring}"), "'a.b','c'");
        assert_eq!(i(&s, "${v:queryparam}"), "var-v=a.b&var-v=c");
        assert_eq!(i(&s, "${v:lucene}"), "(\"a.b\" OR \"c\")");
        assert_eq!(i(&s, "$v"), "{a.b,c}");
        assert_eq!(s.interpolate("$v", Some("prometheus"), SCOPE), "(a\\\\.b|c)");
        s.get_mut("v").unwrap().selected = vec!["c".into()];
        assert_eq!(i(&s, "${v:regex}"), "c");
    }

    #[test]
    fn all_values() {
        let mut s = set(vec![
            json!({"name": "r", "type": "custom", "query": "eu,us", "includeAll": true, "multi": true, "current": {"value": "$__all"}}),
            json!({"name": "a", "type": "custom", "query": "x,y", "includeAll": true, "allValue": ".*", "current": {"value": ["$__all"]}}),
        ]);
        assert_eq!(s.interpolate("$r", Some("prometheus"), SCOPE), "(eu|us)");
        assert_eq!(s.interpolate("$a", Some("prometheus"), SCOPE), ".*");
        assert_eq!(s.get("r").unwrap().display(), "All");
        s.get_mut("r").unwrap().set_options(vec![VarOption::same("ap")]);
        assert!(s.get("r").unwrap().is_all(), "All survives option refresh");
    }

    #[test]
    fn interval_auto() {
        let s = set(vec![
            json!({"name": "step", "type": "interval", "query": "1m,5m", "auto": true, "auto_count": 30, "auto_min": "1m", "current": {"value": "$__auto_interval_step"}}),
        ]);
        // 6h / 30 = 12m -> nice step 15m
        assert_eq!(s.get("step").unwrap().options.len(), 3);
        let mut s2 = s.clone();
        s2.get_mut("step").unwrap().selected = vec!["$__auto_interval".into()];
        assert_eq!(s2.interpolate("[$step]", None, SCOPE), "[15m]");
        s2.get_mut("step").unwrap().selected = vec!["5m".into()];
        assert_eq!(s2.interpolate("[$step]", None, SCOPE), "[5m]");
    }

    #[test]
    fn single_value_prometheus_escaping() {
        let s = set(vec![json!({"name": "q", "type": "textbox", "query": "", "current": {"value": "it's"}})]);
        assert_eq!(s.interpolate("$q", Some("prometheus"), SCOPE), "it\\'s");
    }

    #[test]
    fn builtins() {
        let s = VarSet::default();
        assert_eq!(s.interpolate("$__from-${__to}", None, SCOPE), "0-21600000");
        assert_eq!(s.interpolate("${__to:date:seconds}", None, SCOPE), "21600");
    }

    #[test]
    fn dependency_order() {
        let s = set(vec![
            json!({"name": "instance", "type": "query", "query": "label_values(up{job=\"$job\"}, instance)"}),
            json!({"name": "job", "type": "query", "query": "label_values(up, job)"}),
            json!({"name": "c", "type": "custom", "query": "a"}),
        ]);
        assert_eq!(s.fetch_order(), vec!["job", "instance"]);
        assert_eq!(s.dependents("job"), vec!["instance"]);
    }

    #[test]
    fn object_queries() {
        let d =
            def(json!({"name": "x", "type": "query", "query": {"qryType": 1, "label": "region", "metric": "demo_signal", "refId": "X"}}));
        assert_eq!(d.query, "label_values(demo_signal, region)");
        let d = def(json!({"name": "x", "type": "query", "query": {"type": 1, "label": "service", "stream": "", "refId": "X"}}));
        assert_eq!(d.query, "label_values(service)");
        let d = def(json!({"name": "x", "type": "query", "query": {"query": "label_values(job)", "refId": "X"}}));
        assert_eq!(d.query, "label_values(job)");
    }

    #[test]
    fn regex_and_sort() {
        let d = def(json!({"name": "x", "type": "query", "regex": "/node-(\\d+)/", "sort": 3}));
        let opts = post_process(&d, vec!["node-10".into(), "node-2".into(), "other".into(), "node-2".into()]);
        assert_eq!(opts.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(), vec!["2", "10"]);
        let d = def(json!({"name": "x", "type": "query", "regex": "/(?<text>[a-z]+)=(?<value>\\d+)/"}));
        assert_eq!(post_process(&d, vec!["a=1".into()])[0], VarOption { text: "a".into(), value: "1".into() });
    }

    #[test]
    fn label_values_parsing() {
        let c = LABEL_VALUES.captures("label_values(demo_signal{shape=\"a,b\"}, region)").unwrap();
        assert_eq!(&c[2], "region");
        assert_eq!(c.get(1).unwrap().as_str(), "demo_signal{shape=\"a,b\"}");
        let c = LABEL_VALUES.captures("label_values(job)").unwrap();
        assert!(c.get(1).is_none());
    }

    #[test]
    fn url_params() {
        let s = set(vec![json!({"name": "r", "type": "custom", "query": "a b,c", "multi": true, "current": {"value": ["a b", "c"]}})]);
        assert_eq!(s.url_params(), "var-r=a%20b&var-r=c");
    }
}
