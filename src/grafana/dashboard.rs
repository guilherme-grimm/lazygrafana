//! Typed view over dashboard JSON (classic schema): panels, targets, field
//! config and annotation definitions. The raw JSON stays the source of truth.

use serde_json::Value;

use super::variables::VariableDef;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DsRef {
    pub uid: Option<String>,
    pub kind: Option<String>,
}

impl DsRef {
    /// Accepts `{"type":…, "uid":…}`, a bare uid/name string, or null.
    pub fn from_json(v: &Value) -> Option<Self> {
        match v {
            Value::Object(o) => {
                let uid = o.get("uid").and_then(Value::as_str).map(str::to_string);
                let kind = o.get("type").and_then(Value::as_str).map(str::to_string);
                (uid.is_some() || kind.is_some()).then_some(Self { uid, kind })
            }
            Value::String(s) if !s.is_empty() => Some(Self { uid: Some(s.clone()), kind: None }),
            _ => None,
        }
    }

    pub fn is_mixed(&self) -> bool {
        self.uid.as_deref() == Some("-- Mixed --") || self.kind.as_deref() == Some("mixed")
    }

    pub fn is_grafana_builtin(&self) -> bool {
        matches!(self.uid.as_deref(), Some("-- Grafana --" | "grafana" | "-- Dashboard --"))
            || matches!(self.kind.as_deref(), Some("grafana" | "datasource"))
    }

    pub fn to_json(&self) -> Value {
        let mut o = serde_json::Map::new();
        if let Some(u) = &self.uid {
            o.insert("uid".into(), Value::String(u.clone()));
        }
        if let Some(k) = &self.kind {
            o.insert("type".into(), Value::String(k.clone()));
        }
        Value::Object(o)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GridPos {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub ref_id: String,
    pub raw: Value,
    pub hide: bool,
    pub datasource: Option<DsRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DrawStyle {
    #[default]
    Line,
    Bars,
    Points,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FieldDefaults {
    pub unit: Option<String>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub decimals: Option<u32>,
    /// `(threshold, colour)`; the first step has no threshold (base colour).
    pub thresholds: Vec<(Option<f64>, String)>,
    pub thresholds_percentage: bool,
    pub thresholds_style: Option<String>,
    pub draw_style: DrawStyle,
    pub fill_opacity: f64,
    pub line_width: f64,
    pub stacking: bool,
    pub color: Option<String>,
    pub mappings: Value,
}

impl FieldDefaults {
    fn from_json(v: &Value) -> Self {
        let custom = v.get("custom").cloned().unwrap_or(Value::Null);
        let thresholds = v
            .pointer("/thresholds/steps")
            .and_then(Value::as_array)
            .map(|steps| {
                steps
                    .iter()
                    .map(|s| {
                        (s.get("value").and_then(Value::as_f64), s.get("color").and_then(Value::as_str).unwrap_or("green").to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            unit: v.get("unit").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
            min: v.get("min").and_then(Value::as_f64),
            max: v.get("max").and_then(Value::as_f64),
            decimals: v.get("decimals").and_then(Value::as_u64).map(|d| d as u32),
            thresholds,
            thresholds_percentage: v.pointer("/thresholds/mode").and_then(Value::as_str) == Some("percentage"),
            thresholds_style: custom.pointer("/thresholdsStyle/mode").and_then(Value::as_str).filter(|m| *m != "off").map(str::to_string),
            draw_style: match custom.get("drawStyle").and_then(Value::as_str) {
                Some("bars") => DrawStyle::Bars,
                Some("points") => DrawStyle::Points,
                _ => DrawStyle::Line,
            },
            fill_opacity: custom.get("fillOpacity").and_then(Value::as_f64).unwrap_or(0.0),
            line_width: custom.get("lineWidth").and_then(Value::as_f64).unwrap_or(1.0),
            stacking: custom.pointer("/stacking/mode").and_then(Value::as_str).is_some_and(|m| m == "normal" || m == "percent"),
            color: v.pointer("/color/fixedColor").and_then(Value::as_str).map(str::to_string),
            mappings: v.get("mappings").cloned().unwrap_or(Value::Null),
        }
    }

    /// Text for a value via value mappings (`{"type":"value","options":{"0":{"text":"down"}}}`), if any.
    pub fn mapped(&self, v: f64) -> Option<(String, Option<String>)> {
        for m in self.mappings.as_array()? {
            let opts = m.get("options")?;
            match m.get("type").and_then(Value::as_str) {
                Some("value") => {
                    let key = if v.fract() == 0.0 { format!("{}", v as i64) } else { v.to_string() };
                    if let Some(o) = opts.get(&key) {
                        let text = o.get("text").and_then(Value::as_str).unwrap_or(&key).to_string();
                        return Some((text, o.get("color").and_then(Value::as_str).map(str::to_string)));
                    }
                }
                Some("range") => {
                    let from = opts.get("from").and_then(Value::as_f64).unwrap_or(f64::NEG_INFINITY);
                    let to = opts.get("to").and_then(Value::as_f64).unwrap_or(f64::INFINITY);
                    if v >= from && v <= to {
                        let r = opts.get("result")?;
                        return Some((
                            r.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
                            r.get("color").and_then(Value::as_str).map(str::to_string),
                        ));
                    }
                }
                _ => {}
            }
        }
        None
    }
}

/// A field override we understand: series matching `matcher` get `properties`.
#[derive(Debug, Clone, PartialEq)]
pub struct Override {
    pub matcher_id: String,
    pub matcher_options: String,
    pub properties: Vec<(String, Value)>,
}

impl Override {
    pub fn matches(&self, series_name: &str) -> bool {
        match self.matcher_id.as_str() {
            "byName" | "byFrameRefID" => series_name == self.matcher_options,
            "byRegexp" => regex::Regex::new(&self.matcher_options).map(|r| r.is_match(series_name)).unwrap_or(false),
            _ => false,
        }
    }

    pub fn property(&self, id: &str) -> Option<&Value> {
        self.properties.iter().find(|(k, _)| k == id).map(|(_, v)| v)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Panel {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub description: String,
    pub grid: GridPos,
    pub datasource: Option<DsRef>,
    pub targets: Vec<Target>,
    pub defaults: FieldDefaults,
    pub overrides: Vec<Override>,
    pub options: Value,
    pub interval: Option<String>,
    pub max_data_points: Option<u32>,
    /// Title of the row this panel belongs to, if any.
    pub row: Option<String>,
    pub raw: Value,
}

impl Panel {
    fn from_json(v: &Value, row: Option<&str>) -> Self {
        let grid = v.get("gridPos").cloned().unwrap_or(Value::Null);
        let g = |k: &str| grid.get(k).and_then(Value::as_u64).unwrap_or(0) as u16;
        let datasource = v.get("datasource").and_then(DsRef::from_json);
        let targets = v
            .get("targets")
            .and_then(Value::as_array)
            .map(|ts| {
                ts.iter()
                    .enumerate()
                    .map(|(i, t)| Target {
                        ref_id: t
                            .get("refId")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| ((b'A' + i as u8) as char).to_string()),
                        raw: t.clone(),
                        hide: t.get("hide").and_then(Value::as_bool).unwrap_or(false),
                        datasource: t.get("datasource").and_then(DsRef::from_json),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let overrides = v
            .pointer("/fieldConfig/overrides")
            .and_then(Value::as_array)
            .map(|os| {
                os.iter()
                    .map(|o| Override {
                        matcher_id: o.pointer("/matcher/id").and_then(Value::as_str).unwrap_or_default().to_string(),
                        matcher_options: o
                            .pointer("/matcher/options")
                            .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
                            .unwrap_or_default(),
                        properties: o
                            .get("properties")
                            .and_then(Value::as_array)
                            .map(|ps| {
                                ps.iter()
                                    .filter_map(|p| {
                                        Some((p.get("id")?.as_str()?.to_string(), p.get("value").cloned().unwrap_or(Value::Null)))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            id: v.get("id").and_then(Value::as_i64).unwrap_or(0),
            kind: v.get("type").and_then(Value::as_str).unwrap_or("graph").to_string(),
            title: v.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
            description: v.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
            grid: GridPos { x: g("x"), y: g("y"), w: g("w").max(1), h: g("h").max(1) },
            datasource,
            targets,
            defaults: FieldDefaults::from_json(v.pointer("/fieldConfig/defaults").unwrap_or(&Value::Null)),
            overrides,
            options: v.get("options").cloned().unwrap_or(Value::Null),
            interval: v.get("interval").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
            max_data_points: v.get("maxDataPoints").and_then(Value::as_u64).map(|n| n as u32),
            row: row.map(str::to_string),
            raw: v.clone(),
        }
    }

    /// A bare panel of the given type (Explore, alert charts).
    pub fn synthetic(kind: &str) -> Self {
        Self::from_json(&serde_json::json!({ "type": kind, "options": { "legend": { "showLegend": true } } }), None)
    }

    pub fn is_row(&self) -> bool {
        self.kind == "row"
    }

    /// Panel types the TUI can draw itself.
    pub fn is_supported(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "timeseries"
                | "graph"
                | "stat"
                | "singlestat"
                | "gauge"
                | "bargauge"
                | "table"
                | "table-old"
                | "heatmap"
                | "logs"
                | "text"
                | "barchart"
        )
    }

    pub fn needs_query(&self) -> bool {
        !matches!(self.kind.as_str(), "text" | "row" | "news" | "dashlist" | "alertlist" | "welcome")
    }

    /// Calculation for stat-like panels (`reduceOptions.calcs[0]`).
    pub fn reduce_calc(&self) -> &str {
        self.options.pointer("/reduceOptions/calcs/0").and_then(Value::as_str).unwrap_or("lastNotNull")
    }

    pub fn override_for(&self, series: &str, prop: &str) -> Option<&Value> {
        self.overrides.iter().filter(|o| o.matches(series)).find_map(|o| o.property(prop))
    }

    pub fn is_instant(&self) -> bool {
        let visible: Vec<&Target> = self.targets.iter().filter(|t| !t.hide).collect();
        !visible.is_empty()
            && visible.iter().all(|t| {
                t.raw.get("instant").and_then(Value::as_bool).unwrap_or(false)
                    && !t.raw.get("range").and_then(Value::as_bool).unwrap_or(false)
                    || t.raw.get("queryType").and_then(Value::as_str) == Some("instant")
            })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationDef {
    pub name: String,
    pub enabled: bool,
    pub builtin: bool,
    pub datasource: Option<DsRef>,
    pub expr: Option<String>,
    pub color: String,
    pub title_format: Option<String>,
    pub step: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Dashboard {
    pub uid: String,
    pub title: String,
    pub tags: Vec<String>,
    pub panels: Vec<Panel>,
    pub variables: Vec<VariableDef>,
    pub annotations: Vec<AnnotationDef>,
    pub time_from: String,
    pub time_to: String,
    pub refresh: Option<String>,
    pub version: i64,
    pub raw: Value,
}

impl Dashboard {
    pub fn from_json(v: &Value) -> Self {
        let mut panels = Vec::new();
        let mut current_row: Option<String> = None;
        for p in v.get("panels").and_then(Value::as_array).into_iter().flatten() {
            if p.get("type").and_then(Value::as_str) == Some("row") {
                let title = p.get("title").and_then(Value::as_str).unwrap_or_default().to_string();
                current_row = Some(title.clone());
                // Collapsed rows keep their panels nested inside the row.
                for inner in p.get("panels").and_then(Value::as_array).into_iter().flatten() {
                    panels.push(Panel::from_json(inner, Some(&title)));
                }
                continue;
            }
            panels.push(Panel::from_json(p, current_row.as_deref()));
        }
        // Old schema: rows[].panels[]
        for row in v.get("rows").and_then(Value::as_array).into_iter().flatten() {
            let title = row.get("title").and_then(Value::as_str).map(str::to_string);
            for p in row.get("panels").and_then(Value::as_array).into_iter().flatten() {
                panels.push(Panel::from_json(p, title.as_deref()));
            }
        }
        let variables = v
            .pointer("/templating/list")
            .and_then(Value::as_array)
            .map(|vs| vs.iter().filter_map(VariableDef::from_json).collect())
            .unwrap_or_default();
        let annotations = v
            .pointer("/annotations/list")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(|a| AnnotationDef {
                        name: a.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                        enabled: a.get("enable").and_then(Value::as_bool).unwrap_or(true),
                        builtin: a.get("builtIn").and_then(Value::as_i64).unwrap_or(0) == 1,
                        datasource: a.get("datasource").and_then(DsRef::from_json),
                        expr: a.get("expr").and_then(Value::as_str).map(str::to_string),
                        color: a.get("iconColor").and_then(Value::as_str).unwrap_or("blue").to_string(),
                        title_format: a.get("titleFormat").and_then(Value::as_str).map(str::to_string),
                        step: a.get("step").and_then(Value::as_str).map(str::to_string),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            uid: v.get("uid").and_then(Value::as_str).unwrap_or_default().to_string(),
            title: v.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
            tags: v
                .get("tags")
                .and_then(Value::as_array)
                .map(|t| t.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            panels,
            variables,
            annotations,
            time_from: v.pointer("/time/from").and_then(Value::as_str).unwrap_or("now-6h").to_string(),
            time_to: v.pointer("/time/to").and_then(Value::as_str).unwrap_or("now").to_string(),
            refresh: v.get("refresh").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
            version: v.get("version").and_then(Value::as_i64).unwrap_or(0),
            raw: v.clone(),
        }
    }

    /// Panels in reading order (top-to-bottom, left-to-right).
    pub fn ordered_panels(&self) -> Vec<&Panel> {
        let mut ps: Vec<&Panel> = self.panels.iter().filter(|p| !p.is_row()).collect();
        ps.sort_by_key(|p| (p.grid.y, p.grid.x));
        ps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> Dashboard {
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/dashboard_demo.json")).unwrap(),
        )
        .unwrap();
        Dashboard::from_json(&v["dashboard"])
    }

    #[test]
    fn parses_demo_dashboard() {
        let d = demo();
        assert_eq!(d.uid, "lg-demo-signals");
        // 7 visible panels + 1 inside the collapsed row; row panels themselves are skipped.
        assert_eq!(d.panels.len(), 7);
        let collapsed = d.panels.iter().find(|p| p.title.starts_with("Step-interval")).unwrap();
        assert_eq!(collapsed.row.as_deref(), Some("Collapsed extras"));
        let ts = d.panels.iter().find(|p| p.title == "Demo signals").unwrap();
        assert_eq!(ts.kind, "timeseries");
        assert_eq!(ts.defaults.max, Some(100.0));
        assert_eq!(ts.defaults.thresholds.len(), 2);
        assert_eq!(ts.defaults.thresholds_style.as_deref(), Some("dashed"));
        assert_eq!(ts.targets[0].ref_id, "A");
        let bars = d.panels.iter().find(|p| p.kind == "bargauge").unwrap();
        assert!(bars.is_instant());
        assert_eq!(d.variables.len(), 4);
        assert_eq!(d.annotations.len(), 2);
        assert!(d.annotations[0].builtin);
        let table = d.panels.iter().find(|p| p.kind == "table").unwrap();
        assert_eq!(table.override_for("Time", "custom.hidden"), Some(&Value::Bool(true)));
    }

    #[test]
    fn value_mappings() {
        let defaults = FieldDefaults::from_json(&serde_json::json!({
            "mappings": [{"type": "value", "options": {"0": {"text": "down", "color": "red"}}},
                         {"type": "range", "options": {"from": 10, "to": 20, "result": {"text": "mid"}}}]
        }));
        assert_eq!(defaults.mapped(0.0), Some(("down".into(), Some("red".into()))));
        assert_eq!(defaults.mapped(15.0).unwrap().0, "mid");
        assert_eq!(defaults.mapped(3.0), None);
    }

    #[test]
    fn ds_refs() {
        assert_eq!(DsRef::from_json(&serde_json::json!("Prometheus")).unwrap().uid.as_deref(), Some("Prometheus"));
        assert!(DsRef::from_json(&Value::Null).is_none());
        assert!(DsRef::from_json(&serde_json::json!({"uid": "-- Mixed --"})).unwrap().is_mixed());
    }
}
