//! `params`: the query string merged with the body. Inertia (axios) posts
//! JSON objects; a plain HTML form posts urlencoded, whose `a[b]` / `a[]`
//! names are nested the way Rack does. Values keep their JSON types, and
//! `s()` is Ruby's `params[:x].to_s`.

use axum::extract::{FromRequest, Request};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};

/// 1 MiB is far beyond any form here.
const MAX_BODY: usize = 1 << 20;

#[derive(Debug, Clone, Default)]
pub struct Params(pub Map<String, Value>);

impl Params {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// `params[:x].to_s`
    pub fn s(&self, key: &str) -> String {
        match self.0.get(key) {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(other) => other.to_string(),
        }
    }

    /// `params[:x].presence`
    pub fn presence(&self, key: &str) -> Option<String> {
        Some(self.s(key)).filter(|s| !s.trim().is_empty())
    }

    pub fn has(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    pub fn i64(&self, key: &str) -> Option<i64> {
        match self.0.get(key)? {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// ActiveModel::Type::Boolean.cast: false for "", "0", "f", "false",
    /// "off" (any case) and false; nil for nil; true otherwise.
    pub fn bool(&self, key: &str) -> Option<bool> {
        match self.0.get(key)? {
            Value::Null => None,
            Value::Bool(b) => Some(*b),
            Value::String(s) if s.is_empty() => None,
            Value::String(s) => Some(!matches!(s.to_ascii_lowercase().as_str(), "0" | "f" | "false" | "off")),
            Value::Number(n) => Some(n.as_f64() != Some(0.0)),
            _ => Some(true),
        }
    }

    /// A string array (`scopes[]`): an array, or a single value.
    pub fn strings(&self, key: &str) -> Vec<String> {
        match self.0.get(key) {
            Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string)).collect(),
            Some(Value::String(s)) => vec![s.clone()],
            _ => vec![],
        }
    }

    pub fn from_query(q: Option<&str>) -> Params {
        let mut p = Params::default();
        if let Some(q) = q {
            for (k, v) in form_urlencoded::parse(q.as_bytes()) {
                p.insert_rack(&k, Value::String(v.into_owned()));
            }
        }
        p
    }

    /// Rack's nested parameter names: `a[b][c]`, `a[]`.
    fn insert_rack(&mut self, name: &str, value: Value) {
        let (head, rest) = match name.find('[') {
            Some(i) => (&name[..i], &name[i..]),
            None => (name, ""),
        };
        if head.is_empty() {
            return;
        }
        let mut keys: Vec<&str> = Vec::new();
        let mut r = rest;
        while let Some(stripped) = r.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            keys.push(&stripped[..end]);
            r = &stripped[end + 1..];
        }
        insert_path(&mut self.0, head, &keys, value);
    }

    fn merge(&mut self, other: Map<String, Value>) {
        for (k, v) in other {
            self.0.insert(k, v);
        }
    }
}

fn insert_path(map: &mut Map<String, Value>, head: &str, keys: &[&str], value: Value) {
    match keys.split_first() {
        None => {
            map.insert(head.to_string(), value);
        }
        Some((&"", _)) => {
            let entry = map.entry(head.to_string()).or_insert_with(|| Value::Array(vec![]));
            if let Value::Array(a) = entry {
                a.push(value);
            }
        }
        Some((k, rest)) => {
            let entry = map.entry(head.to_string()).or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            if let Value::Object(m) = entry {
                insert_path(m, k, rest, value);
            }
        }
    }
}

pub struct Rejection(StatusCode, &'static str);

impl IntoResponse for Rejection {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

impl<S: Send + Sync> FromRequest<S> for Params {
    type Rejection = Rejection;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        let mut p = Params::from_query(req.uri().query());
        let ctype = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        let body = axum::body::to_bytes(req.into_body(), MAX_BODY).await.map_err(|_| Rejection(StatusCode::PAYLOAD_TOO_LARGE, "body too large"))?;
        if body.is_empty() {
            return Ok(p);
        }
        if ctype.starts_with("application/json") {
            match serde_json::from_slice::<Value>(&body) {
                Ok(Value::Object(m)) => p.merge(m),
                Ok(_) => {}
                Err(_) => return Err(Rejection(StatusCode::BAD_REQUEST, "malformed JSON")),
            }
        } else if ctype.starts_with("application/x-www-form-urlencoded") {
            for (k, v) in form_urlencoded::parse(&body) {
                p.insert_rack(&k, Value::String(v.into_owned()));
            }
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rack_names_nest_and_casts_follow_rails() {
        let p = Params::from_query(Some("a=1&params[sort]=p95&params[dir]=desc&scopes[]=read&scopes[]=api&terms=on&wd=0&e="));
        assert_eq!(p.s("a"), "1");
        assert_eq!(p.get("params").unwrap()["sort"], "p95");
        assert_eq!(p.strings("scopes"), ["read", "api"]);
        assert_eq!(p.bool("terms"), Some(true));
        assert_eq!(p.bool("wd"), Some(false));
        assert_eq!(p.bool("e"), None);
        assert_eq!(p.presence("e"), None);
        assert_eq!(p.s("missing"), "");
    }
}
