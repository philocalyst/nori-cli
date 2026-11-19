//! Typed Conduit runtime values with lossless JSON and Pkl expression codecs.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "tag", content = "val", rename_all = "snake_case")]
pub enum ConduitValue {
    Unit,
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<ConduitValue>),
    Tuple(Vec<ConduitValue>),
    Some(Box<ConduitValue>),
    None,
    Map(BTreeMap<String, ConduitValue>),
}

impl<'de> Deserialize<'de> for ConduitValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = serde_json::Value::deserialize(deserializer)?;
        Ok(Self::from_wire_or_plain_json(raw))
    }
}

impl ConduitValue {
    pub fn from_wire_or_plain_json(value: serde_json::Value) -> Self {
        if let serde_json::Value::Object(ref map) = value
            && let Some(serde_json::Value::String(tag)) = map.get("tag")
            && map.keys().all(|k| k == "tag" || k == "val")
        {
            let val = map.get("val").cloned().unwrap_or(serde_json::Value::Null);
            match tag.as_str() {
                "unit" if map.len() == 1 => return Self::Unit,
                "null" if map.len() == 1 => return Self::Null,
                "none" if map.len() == 1 => return Self::None,
                "bool" => {
                    if let serde_json::Value::Bool(b) = val {
                        return Self::Bool(b);
                    }
                }
                "int" => {
                    if let Some(i) = val.as_i64() {
                        return Self::Int(i);
                    }
                }
                "float" => {
                    if let Some(f) = val.as_f64() {
                        return Self::Float(f);
                    }
                }
                "string" => {
                    if let serde_json::Value::String(s) = val {
                        return Self::String(s);
                    }
                }
                "list" => {
                    if let serde_json::Value::Array(items) = val {
                        return Self::List(
                            items
                                .into_iter()
                                .map(Self::from_wire_or_plain_json)
                                .collect(),
                        );
                    }
                }
                "tuple" => {
                    if let serde_json::Value::Array(items) = val {
                        return Self::Tuple(
                            items
                                .into_iter()
                                .map(Self::from_wire_or_plain_json)
                                .collect(),
                        );
                    }
                }
                "some" => {
                    return Self::Some(Box::new(Self::from_wire_or_plain_json(val)));
                }
                "map" => {
                    if let serde_json::Value::Object(entries) = val {
                        let mut converted = BTreeMap::new();
                        for (k, v) in entries {
                            converted.insert(k, Self::from_wire_or_plain_json(v));
                        }
                        return Self::Map(converted);
                    }
                }
                _ => {}
            }
        }
        Self::from_json(value)
    }
    pub fn from_json(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(b) => Self::Bool(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Self::Int(i)
                } else {
                    Self::Float(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => Self::String(s),
            serde_json::Value::Array(items) => {
                Self::List(items.into_iter().map(Self::from_json).collect())
            }
            serde_json::Value::Object(map) => {
                let mut converted = BTreeMap::new();
                for (k, v) in map {
                    converted.insert(k, Self::from_json(v));
                }
                Self::Map(converted)
            }
        }
    }

    pub fn to_plain_json(&self) -> serde_json::Value {
        match self {
            Self::Unit | Self::Null | Self::None => serde_json::Value::Null,
            Self::Bool(b) => serde_json::Value::Bool(*b),
            Self::Int(i) => serde_json::Value::Number((*i).into()),
            Self::Float(f) => serde_json::Number::from_f64(*f)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            Self::String(s) => serde_json::Value::String(s.clone()),
            Self::List(items) | Self::Tuple(items) => {
                serde_json::Value::Array(items.iter().map(Self::to_plain_json).collect())
            }
            Self::Some(inner) => inner.to_plain_json(),
            Self::Map(entries) => {
                let mut map = serde_json::Map::new();
                for (k, v) in entries {
                    map.insert(k.clone(), v.to_plain_json());
                }
                serde_json::Value::Object(map)
            }
        }
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Unit | Self::Null | Self::None | Self::Bool(false) => false,
            Self::Bool(true)
            | Self::Int(_)
            | Self::Float(_)
            | Self::String(_)
            | Self::List(_)
            | Self::Tuple(_)
            | Self::Some(_)
            | Self::Map(_) => true,
        }
    }

    pub fn display_compact(&self) -> String {
        match self {
            Self::Unit => "unit".to_owned(),
            Self::Null => "null".to_owned(),
            Self::None => "none".to_owned(),
            Self::Some(inner) => format!("some({})", inner.display_compact()),
            Self::Bool(b) => b.to_string(),
            Self::Int(i) => i.to_string(),
            Self::Float(f) => f.to_string(),
            Self::String(s) => s.clone(),
            Self::List(_) | Self::Tuple(_) | Self::Map(_) => self.to_plain_json().to_string(),
        }
    }

    pub fn to_pkl_expr(&self) -> String {
        match self {
            Self::Unit | Self::Null | Self::None => "null".to_owned(),
            Self::Some(inner) => inner.to_pkl_expr(),
            Self::Bool(b) => b.to_string(),
            Self::Int(i) => i.to_string(),
            Self::Float(f) => {
                if !f.is_finite() {
                    "0.0".to_owned()
                } else {
                    let s = f.to_string();
                    if s.contains('.') || s.contains('e') || s.contains('E') {
                        s
                    } else {
                        format!("{s}.0")
                    }
                }
            }
            Self::String(s) => format!("\"{}\"", escape_pkl_str(s)),
            Self::List(items) | Self::Tuple(items) => {
                if items.is_empty() {
                    "new Listing {}".to_owned()
                } else {
                    let elems: Vec<String> = items.iter().map(Self::to_pkl_expr).collect();
                    format!("new Listing {{ {} }}", elems.join("; "))
                }
            }
            Self::Map(entries) => {
                if entries.is_empty() {
                    "new Mapping {}".to_owned()
                } else {
                    let pairs: Vec<String> = entries
                        .iter()
                        .map(|(k, v)| format!("[\"{}\"] = {}", escape_pkl_str(k), v.to_pkl_expr()))
                        .collect();
                    format!("new Mapping {{ {} }}", pairs.join("; "))
                }
            }
        }
    }
}

pub(crate) fn escape_pkl_str(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}
