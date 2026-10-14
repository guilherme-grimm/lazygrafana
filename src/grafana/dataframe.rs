//! Parser for the data-frame JSON returned by `/api/ds/query` (and a few other
//! endpoints, e.g. alert state history). Everything downstream — charts,
//! tables, logs — consumes these types.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::timerange::Millis;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Time,
    Number,
    String,
    Bool,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Values {
    Time(Vec<Option<Millis>>),
    Number(Vec<Option<f64>>),
    String(Vec<Option<String>>),
    Bool(Vec<Option<bool>>),
    Other(Vec<Value>),
}

impl Values {
    pub fn len(&self) -> usize {
        match self {
            Self::Time(v) => v.len(),
            Self::Number(v) => v.len(),
            Self::String(v) => v.len(),
            Self::Bool(v) => v.len(),
            Self::Other(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A display string for row `i`.
    pub fn display(&self, i: usize) -> String {
        match self {
            Self::Time(v) => v.get(i).copied().flatten().map(crate::timerange::format_absolute).unwrap_or_default(),
            Self::Number(v) => v.get(i).copied().flatten().map(|n| crate::units::format_short(n, None)).unwrap_or_default(),
            Self::String(v) => v.get(i).cloned().flatten().unwrap_or_default(),
            Self::Bool(v) => v.get(i).copied().flatten().map(|b| b.to_string()).unwrap_or_default(),
            Self::Other(v) => v.get(i).map(|x| if x.is_null() { String::new() } else { x.to_string() }).unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub kind: FieldType,
    pub labels: BTreeMap<String, String>,
    pub config: Value,
    pub values: Values,
}

impl Field {
    /// Grafana's display name: explicit config first, then labels, then the field name.
    pub fn display_name(&self) -> String {
        for key in ["displayNameFromDS", "displayName"] {
            if let Some(s) = self.config.get(key).and_then(Value::as_str)
                && !s.is_empty()
            {
                return s.to_string();
            }
        }
        if self.labels.is_empty() {
            return self.name.clone();
        }
        let shown: Vec<String> =
            self.labels.iter().filter(|(k, _)| k.as_str() != "__name__").map(|(k, v)| format!("{k}=\"{v}\"")).collect();
        let metric = self.labels.get("__name__").cloned().unwrap_or_default();
        if shown.is_empty() { metric } else { format!("{metric}{{{}}}", shown.join(", ")) }
    }

    pub fn numbers(&self) -> Option<&[Option<f64>]> {
        match &self.values {
            Values::Number(v) => Some(v),
            _ => None,
        }
    }

    pub fn times(&self) -> Option<&[Option<Millis>]> {
        match &self.values {
            Values::Time(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.config.pointer("/custom/hidden").and_then(Value::as_bool).unwrap_or(false)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Frame {
    pub name: Option<String>,
    pub ref_id: Option<String>,
    pub meta: Value,
    pub fields: Vec<Field>,
}

impl Frame {
    pub fn len(&self) -> usize {
        self.fields.first().map(|f| f.values.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn time_field(&self) -> Option<&Field> {
        self.fields.iter().find(|f| f.kind == FieldType::Time)
    }

    pub fn meta_type(&self) -> Option<&str> {
        self.meta.get("type").and_then(Value::as_str)
    }

    pub fn executed_query(&self) -> Option<&str> {
        self.meta.get("executedQueryString").and_then(Value::as_str)
    }

    /// Loki log frames carry a `Line` string field next to `Time`.
    pub fn is_logs(&self) -> bool {
        self.meta_type() == Some("log-lines")
            || (self.field("Line").is_some() && self.time_field().is_some())
            || self.meta.pointer("/preferredVisualisationType").and_then(Value::as_str) == Some("logs")
    }
}

/// One time series extracted from frames.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub ref_id: Option<String>,
    pub points: Vec<(Millis, Option<f64>)>,
}

impl Series {
    pub fn last_value(&self) -> Option<f64> {
        self.points.iter().rev().find_map(|(_, v)| *v)
    }

    pub fn value_at(&self, t: Millis) -> Option<f64> {
        // Points are sorted by time; take the closest point at or before t.
        let idx = self.points.partition_point(|(pt, _)| *pt <= t);
        if idx == 0 {
            return None;
        }
        self.points[idx - 1].1
    }

    pub fn reduce(&self, calc: &str) -> Option<f64> {
        let vals: Vec<f64> = self.points.iter().filter_map(|(_, v)| *v).filter(|v| v.is_finite()).collect();
        if vals.is_empty() {
            return None;
        }
        Some(match calc {
            "min" => vals.iter().copied().fold(f64::INFINITY, f64::min),
            "max" => vals.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            "mean" | "avg" => vals.iter().sum::<f64>() / vals.len() as f64,
            "sum" | "total" => vals.iter().sum(),
            "first" | "firstNotNull" => vals[0],
            "count" => vals.len() as f64,
            "range" => {
                let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let min = vals.iter().copied().fold(f64::INFINITY, f64::min);
                max - min
            }
            "delta" | "diff" => vals[vals.len() - 1] - vals[0],
            _ => vals[vals.len() - 1], // last / lastNotNull
        })
    }
}

/// Extracts every numeric field paired with its frame's time field.
pub fn series_from_frames(frames: &[Frame]) -> Vec<Series> {
    let mut out = Vec::new();
    for frame in frames {
        let Some(times) = frame.time_field().and_then(Field::times) else { continue };
        for field in frame.fields.iter().filter(|f| f.kind == FieldType::Number && !f.is_hidden()) {
            let Some(values) = field.numbers() else { continue };
            let mut points: Vec<(Millis, Option<f64>)> =
                times.iter().zip(values).filter_map(|(t, v)| t.map(|t| (t, v.filter(|x| x.is_finite())))).collect();
            points.sort_by_key(|(t, _)| *t);
            out.push(Series { name: field.display_name(), labels: field.labels.clone(), ref_id: frame.ref_id.clone(), points });
        }
    }
    out
}

/// A log line extracted from Loki frames.
#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    pub time: Millis,
    /// Nanosecond timestamp when available (for dedup while tailing).
    pub ts_ns: Option<String>,
    pub line: String,
    pub labels: BTreeMap<String, String>,
    pub id: Option<String>,
}

impl LogLine {
    pub fn level(&self) -> LogLevel {
        if let Some(l) = self.labels.get("level").or_else(|| self.labels.get("detected_level"))
            && let Some(level) = LogLevel::parse(l)
        {
            return level;
        }
        LogLevel::sniff(&self.line)
    }

    pub fn key(&self) -> String {
        self.id.clone().unwrap_or_else(|| format!("{}:{}", self.ts_ns.as_deref().unwrap_or(""), self.line))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Critical,
    Unknown,
}

impl LogLevel {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "trace" => Self::Trace,
            "debug" | "dbug" => Self::Debug,
            "info" | "information" | "notice" => Self::Info,
            "warn" | "warning" => Self::Warn,
            "error" | "err" | "eror" => Self::Error,
            "critical" | "crit" | "fatal" | "emerg" | "alert" | "panic" => Self::Critical,
            _ => return None,
        })
    }

    /// Guess the level from explicit markers in the line: `level=error`, `"level":"warn"`,
    /// `[core:error]` / `[warn]`, or an uppercase level word (`ERROR`, `WARN`).
    pub fn sniff(line: &str) -> Self {
        static KV: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r#"(?i)\b(?:level|lvl|severity|loglevel)["']?\s*[=:]\s*["']?([a-z]+)"#).expect("valid regex")
        });
        static BRACKET: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"(?i)\[(?:[\w-]+:)?(trace|debug|info|notice|warn|warning|err|error|crit|critical|alert|emerg|fatal)\]")
                .expect("valid regex")
        });
        static UPPER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"\b(TRACE|DEBUG|INFO|NOTICE|WARN|WARNING|ERROR|FATAL|CRITICAL|PANIC)\b").expect("valid regex")
        });
        for re in [&*KV, &*BRACKET, &*UPPER] {
            if let Some(level) = re.captures(line).and_then(|c| Self::parse(&c[1])) {
                return level;
            }
        }
        Self::Unknown
    }
}

pub fn logs_from_frames(frames: &[Frame]) -> Vec<LogLine> {
    let mut out = Vec::new();
    for frame in frames.iter().filter(|f| f.is_logs()) {
        let Some(times) = frame.time_field().and_then(Field::times) else { continue };
        let line_field = frame.field("Line").or_else(|| frame.field("line")).or_else(|| frame.field("body"));
        let Some(Values::String(lines)) = line_field.map(|f| &f.values) else { continue };
        let labels_field = frame.field("labels");
        let ts_ns = frame.field("tsNs");
        let ids = frame.field("id");
        for (i, time) in times.iter().enumerate() {
            let labels = match labels_field.map(|f| &f.values) {
                Some(Values::Other(v)) => v
                    .get(i)
                    .and_then(Value::as_object)
                    .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect())
                    .unwrap_or_default(),
                _ => line_field.map(|f| f.labels.clone()).unwrap_or_default(),
            };
            let get_str = |f: Option<&Field>| match f.map(|f| &f.values) {
                Some(Values::String(v)) => v.get(i).cloned().flatten(),
                _ => None,
            };
            out.push(LogLine {
                time: time.unwrap_or_default(),
                ts_ns: get_str(ts_ns),
                line: lines.get(i).cloned().flatten().unwrap_or_default(),
                labels,
                id: get_str(ids),
            });
        }
    }
    out
}

// ---- JSON wire format ------------------------------------------------------

#[derive(Deserialize)]
struct WireFrame {
    #[serde(default)]
    schema: WireSchema,
    #[serde(default)]
    data: WireData,
}

#[derive(Deserialize, Default)]
struct WireSchema {
    name: Option<String>,
    #[serde(rename = "refId")]
    ref_id: Option<String>,
    #[serde(default)]
    meta: Value,
    #[serde(default)]
    fields: Vec<WireField>,
}

#[derive(Deserialize)]
struct WireField {
    #[serde(default)]
    name: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    config: Value,
}

#[derive(Deserialize, Default)]
struct WireData {
    #[serde(default)]
    values: Vec<Vec<Value>>,
    #[serde(default)]
    nanos: Vec<Option<Vec<i64>>>,
    #[serde(default)]
    entities: Vec<Option<WireEntities>>,
}

#[derive(Deserialize, Default)]
struct WireEntities {
    #[serde(rename = "NaN", default)]
    nan: Vec<usize>,
    #[serde(rename = "Inf", default)]
    inf: Vec<usize>,
    #[serde(rename = "NegInf", default)]
    neg_inf: Vec<usize>,
}

impl Frame {
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let w: WireFrame = serde_json::from_value(v.clone()).map_err(|e| format!("invalid data frame: {e}"))?;
        let mut fields = Vec::with_capacity(w.schema.fields.len());
        for (i, wf) in w.schema.fields.into_iter().enumerate() {
            let raw = w.data.values.get(i).cloned().unwrap_or_default();
            let kind = match wf.kind.as_str() {
                "time" => FieldType::Time,
                "number" => FieldType::Number,
                "string" => FieldType::String,
                "boolean" => FieldType::Bool,
                _ => FieldType::Other,
            };
            let values = match kind {
                FieldType::Time => Values::Time(raw.iter().map(value_to_millis).collect()),
                FieldType::Number => {
                    let mut nums: Vec<Option<f64>> = raw.iter().map(Value::as_f64).collect();
                    if let Some(Some(ent)) = w.data.entities.get(i) {
                        for &j in &ent.nan {
                            if let Some(slot) = nums.get_mut(j) {
                                *slot = Some(f64::NAN);
                            }
                        }
                        for &j in &ent.inf {
                            if let Some(slot) = nums.get_mut(j) {
                                *slot = Some(f64::INFINITY);
                            }
                        }
                        for &j in &ent.neg_inf {
                            if let Some(slot) = nums.get_mut(j) {
                                *slot = Some(f64::NEG_INFINITY);
                            }
                        }
                    }
                    Values::Number(nums)
                }
                FieldType::String => Values::String(raw.iter().map(|v| v.as_str().map(str::to_string)).collect()),
                FieldType::Bool => Values::Bool(raw.iter().map(Value::as_bool).collect()),
                FieldType::Other => Values::Other(raw),
            };
            // `nanos` carries sub-millisecond precision; milliseconds are enough for us.
            let _ = w.data.nanos.get(i);
            fields.push(Field {
                name: wf.name,
                kind,
                labels: wf.labels.unwrap_or_default(),
                config: if wf.config.is_null() { Value::Object(Default::default()) } else { wf.config },
                values,
            });
        }
        Ok(Frame { name: w.schema.name, ref_id: w.schema.ref_id, meta: w.schema.meta, fields })
    }
}

fn value_to_millis(v: &Value) -> Option<Millis> {
    match v {
        // Some endpoints (alert state history) send µs or ns; no real ms timestamp exceeds 1e14.
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).map(|t| match t {
            t if t > 100_000_000_000_000_000 => t / 1_000_000,
            t if t > 100_000_000_000_000 => t / 1_000,
            t => t,
        }),
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn frames(name: &str) -> Vec<Frame> {
        fixture(name)["results"]["A"]["frames"].as_array().unwrap().iter().map(|f| Frame::from_json(f).unwrap()).collect()
    }

    #[test]
    fn range_query_series() {
        let fs = frames("query_range.json");
        let series = series_from_frames(&fs);
        assert_eq!(series.len(), 4);
        let sine = series.iter().find(|s| s.name == "sine eu-west-1").unwrap();
        assert!(sine.points.len() > 30);
        assert!(sine.points.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(sine.labels["region"], "eu-west-1");
        assert!(sine.reduce("max").unwrap() <= 90.0);
    }

    #[test]
    fn table_query() {
        let fs = frames("query_table.json");
        assert!(!fs.is_empty());
        assert_eq!(fs[0].time_field().unwrap().name, "Time");
        assert!(fs[0].fields.iter().any(|f| f.kind == FieldType::Number));
    }

    #[test]
    fn heatmap_buckets() {
        let fs = frames("query_heatmap.json");
        let series = series_from_frames(&fs);
        assert!(series.iter().any(|s| s.name == "+Inf"));
    }

    #[test]
    fn loki_logs() {
        let fs = frames("query_logs.json");
        assert!(fs[0].is_logs());
        let logs = logs_from_frames(&fs);
        assert_eq!(logs.len(), 5);
        assert_eq!(logs[0].labels["service"], "logs-errors");
        assert!(logs[0].ts_ns.is_some() && logs[0].id.is_some());
        assert!(series_from_frames(&fs).is_empty());
    }

    #[test]
    fn display_names() {
        let f = Field {
            name: "Value".into(),
            kind: FieldType::Number,
            labels: [("__name__".to_string(), "up".to_string()), ("job".to_string(), "node".to_string())].into(),
            config: Value::Null,
            values: Values::Number(vec![]),
        };
        assert_eq!(f.display_name(), "up{job=\"node\"}");
    }

    #[test]
    fn entities_and_nulls() {
        let v = serde_json::json!({
            "schema": {"fields": [{"name": "t", "type": "time"}, {"name": "v", "type": "number"}]},
            "data": {"values": [[1000, 2000, 3000], [1.0, null, null]], "entities": [null, {"NaN": [2]}]}
        });
        let f = Frame::from_json(&v).unwrap();
        let nums = f.fields[1].numbers().unwrap();
        assert_eq!(nums[0], Some(1.0));
        assert_eq!(nums[1], None);
        assert!(nums[2].unwrap().is_nan());
        let s = series_from_frames(&[f]);
        assert_eq!(s[0].points[2].1, None, "NaN is dropped from series");
        assert_eq!(s[0].value_at(2500), None);
        assert_eq!(s[0].value_at(1500), Some(1.0));
    }

    #[test]
    fn log_levels() {
        assert_eq!(LogLevel::sniff("[core:error] boom"), LogLevel::Error);
        assert_eq!(LogLevel::sniff("level=warn msg=x"), LogLevel::Warn);
        assert_eq!(LogLevel::sniff("hello"), LogLevel::Unknown);
        assert_eq!(LogLevel::sniff(r#"{"level":"error","msg":"x"}"#), LogLevel::Error);
        assert_eq!(LogLevel::sniff("[Wed Oct 14] [temporibus:warn] [pid 1]"), LogLevel::Warn);
        assert_eq!(LogLevel::sniff("2026 ERROR something"), LogLevel::Error);
        assert_eq!(LogLevel::sniff(r#"{"referer": "https://x.com/alert/critical-information"}"#), LogLevel::Unknown);
        assert_eq!(LogLevel::parse("WARNING"), Some(LogLevel::Warn));
    }
}
