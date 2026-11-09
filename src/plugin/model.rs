//! Minimal mirror of the Hyprdeck API-1 state schema (docs/PLUGINS.md) plus `json!` control
//! builders. Controls stay untyped `serde_json::Value`, like hyprdeck-razer.

use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Serialize)]
pub struct State {
    pub title: String,
    pub description: String,
    pub groups: Vec<Group>,
    pub products: Vec<Product>,
    pub refresh_interval_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct Product {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub description: String,
    pub status: String,
    pub connection: String,
    pub battery: Option<Battery>,
    pub sections: Vec<Section>,
}

#[derive(Debug, Serialize)]
pub struct Battery {
    pub percentage: Option<f64>,
    pub charging: Option<bool>,
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct Section {
    pub id: &'static str,
    pub title: &'static str,
    pub groups: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Group {
    pub id: String,
    pub title: String,
    pub description: String,
    pub rows: Vec<Row>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visualization: Option<Value>,
    pub collapsed: bool,
}

#[derive(Debug, Serialize)]
pub struct Row {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub control: Option<Value>,
}

impl Row {
    pub fn info(
        id: impl Into<String>,
        title: impl Into<String>,
        subtitle: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            subtitle: subtitle.into(),
            control: None,
        }
    }
    pub fn controlled(mut self, control: Value) -> Self {
        self.control = Some(control);
        self
    }
}

impl Group {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: description.into(),
            rows: Vec::new(),
            visualization: None,
            collapsed: false,
        }
    }
    pub fn collapsed(mut self) -> Self {
        self.collapsed = true;
        self
    }
    pub fn row(&mut self, row: Row) -> &mut Self {
        self.rows.push(row);
        self
    }
}

/// One `{"value","label"}` option.
pub fn option(value: impl Into<String>, label: impl Into<String>) -> Value {
    json!({"value": value.into(), "label": label.into()})
}

fn device_args(device: &str, extra: Value) -> Value {
    let mut args = json!({"device": device});
    if let (Some(map), Value::Object(extra)) = (args.as_object_mut(), extra) {
        map.extend(extra);
    }
    args
}

pub fn button(label: &str, action: &str, device: &str, extra: Value) -> Value {
    json!({"kind": "button", "label": label, "action": action,
        "args": device_args(device, extra), "destructive": false})
}

pub fn switch(value: bool, action: &str, device: &str, extra: Value) -> Value {
    json!({"kind": "switch", "value": value, "action": action, "args": device_args(device, extra)})
}

pub fn number(value: f64, min: f64, max: f64, step: f64, action: &str, device: &str) -> Value {
    json!({"kind": "number", "value": value, "min": min, "max": max, "step": step,
        "action": action, "args": device_args(device, json!({}))})
}

pub fn choice(value: &str, options: Vec<Value>, action: &str, device: &str, extra: Value) -> Value {
    json!({"kind": "choice", "value": value, "options": options, "action": action,
        "args": device_args(device, extra)})
}

pub fn color(value: &str, action: &str, device: &str, extra: Value) -> Value {
    json!({"kind": "color", "value": value, "action": action, "args": device_args(device, extra)})
}

pub fn form(label: &str, action: &str, device: &str, extra: Value, fields: Vec<Value>) -> Value {
    json!({"kind": "form", "label": label, "action": action, "args": device_args(device, extra),
        "fields": fields})
}

pub fn text_field(id: &str, label: &str, value: &str) -> Value {
    json!({"id": id, "label": label, "kind": "text", "value": value})
}

pub fn choice_field(id: &str, label: &str, value: &str, options: Vec<Value>) -> Value {
    json!({"id": id, "label": label, "kind": "choice", "value": value, "options": options})
}
