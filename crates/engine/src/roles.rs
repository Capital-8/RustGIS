//! Replayable parameters are cleaned by role, never by recursively deleting secret-named data
//! properties. Unknown nested content and foreign notebook cells remain opaque, with honest
//! notices. Logs deliberately have a separate, stricter policy.

use std::collections::HashSet;
use std::sync::OnceLock;

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::durable::{self, Durable, Kept, Report};

const WITHHELD: &str = "[parameters withheld]";
const NESTED: &str = "[withheld: nested too deep]";
const DATA: &[&str] = &[
    "text",
    "data",
    "values",
    "value",
    "attributes",
    "properties",
    "features",
    "feature",
    "geometry",
    "rows",
    "expression",
    "where",
    "html",
    "markdown",
    "caption",
    "title",
    "notes",
    "description",
    "label",
    "name",
    "field",
    "fields",
];
const ADDRESS: &[&str] =
    &["url", "urls", "href", "source", "template", "tiles", "store", "endpoint", "address", "uri", "connection", "database"];
const CONFIG: &[&str] = &["headers", "options", "auth", "connection", "source", "sources"];

/// Registered commands are classified once; plugin commands keep their nested content opaque.
fn known(id: &str) -> bool {
    static IDS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    IDS.get_or_init(|| crate::cmd::all().into_iter().map(|c| c.id).collect()).contains(id)
}

/// A conservative bounded read-only check, used solely to announce unchecked/withheld content.
fn suspicious(v: &Value) -> bool {
    crate::sources::holds_secret(v)
        || v.as_str().is_some_and(|s| {
            s.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '%')))
                .take(65_536)
                .any(|word| !word.is_empty() && rustgis_data::address::secret_key(word))
        })
}

/// Keeps invalid executable content invalid while withholding credential-bearing text.
fn invalid(value: &Value, report: &mut Report) -> Option<Value> {
    if suspicious(value) {
        report.withheld = report.withheld.saturating_add(1);
        Some(Value::String(WITHHELD.into()))
    } else {
        None
    }
}

/// Cleans one command without treating parameter JSON strings as executor envelopes.
pub(crate) fn command(id: &str, params: &Value, depth: usize) -> (Option<Value>, Report) {
    let mut report = Report::default();
    let changed = object(id, params, depth, 0, known(id), &mut report);
    (changed, report)
}

/// Tools use command roles too, including the data-valued `input`/`values` parameters.
pub(crate) fn tool(id: &str, params: &Value, depth: usize) -> (Option<Value>, Report) {
    let mut report = Report::default();
    let builtin = rustgis_analysis::TOOLS.iter().any(|t| t.id == id);
    let changed = object("", params, depth, 0, builtin, &mut report);
    (changed, report)
}

/// Lazily copies only changed owning parameter objects. Configuration recursion is bounded.
fn object(id: &str, value: &Value, depth: usize, config_depth: usize, registered: bool, report: &mut Report) -> Option<Value> {
    if value.is_null() {
        return None;
    }
    let Some(obj) = value.as_object() else { return invalid(value, report) };
    if config_depth >= 8 {
        return invalid(value, report);
    }
    let mut replacement = None;
    let mut unchecked = false;
    for (key, v) in obj {
        unchecked |= !matches!(durable::public_address(key), Kept::Unchanged);
        let k = rustgis_data::address::normalized_name(key).unwrap_or_else(|| key.to_ascii_lowercase());
        let executable = registered
            && matches!(
                (id, k.as_str()),
                ("gp.run", "params")
                    | ("script.run", "script")
                    | ("task.new", "steps")
                    | ("model.new" | "model.save" | "model.run" | "model.validate" | "model.export", "graph")
                    | ("model.new", "steps")
            );
        let document = registered
            && k == "text"
            && matches!(id, "project.openText" | "map.import" | "task.import" | "notebook.open" | "presentation.import" | "model.import");
        let clean = if executable || document {
            if depth >= 4 {
                report.withheld = report.withheld.saturating_add(1);
                Some(Value::String(NESTED.into()))
            } else {
                envelope(id, &k, v, obj, depth + 1, report)
            }
        } else if rustgis_data::address::secret_key(key)
            && (k != "key"
                || crate::sources::looks_like_a_key(v.as_str().unwrap_or(""))
                || obj.iter().any(|(other, value)| other != key && !DATA.contains(&other.as_str()) && address_like(value)))
        {
            match v {
                Value::String(s) if !s.is_empty() => Some(Value::String(String::new())),
                Value::Number(_) => Some(Value::String(String::new())),
                _ => None,
            }
        } else if DATA.contains(&k.as_str()) {
            None
        } else if registered && id.starts_with("sql.") && matches!(k.as_str(), "sql" | "query") {
            v.as_str().and_then(|s| {
                let clean = crate::project_log::sql_secrets_removed(s);
                (clean != s).then_some(Value::String(clean))
            })
        } else if ADDRESS.contains(&k.as_str()) || address_like(v) {
            if v.is_object() && registered && CONFIG.contains(&k.as_str()) {
                object("", v, depth, config_depth + 1, true, report)
            } else {
                address(v, report)
            }
        } else if registered && CONFIG.contains(&k.as_str()) && v.is_object() {
            object("", v, depth, config_depth + 1, true, report)
        } else {
            unchecked |= suspicious(v);
            None
        };
        if !registered && !DATA.contains(&k.as_str()) && (v.is_object() || v.is_array()) {
            unchecked |= suspicious(v);
        }
        if let Some(clean) = clean.filter(|clean| clean != v) {
            replacement.get_or_insert_with(|| obj.clone()).insert(key.clone(), clean);
        }
    }
    if unchecked {
        report.unchecked = report.unchecked.saturating_add(1);
    }
    replacement.map(Value::Object)
}

/// Recognition precedes parsing, including malformed wrappers and differently cased prefixes.
fn address_like(v: &Value) -> bool {
    match v {
        Value::String(s) => {
            let s = s.trim_start();
            ["http:", "https:", "s3:", "rustgis-anonymous:", "rustgis-s3:", "pmtiles:", "cog:", "postgres:", "postgresql:", "pg:"]
                .iter()
                .any(|p| s.get(..p.len()).is_some_and(|s| s.eq_ignore_ascii_case(p)))
        }
        Value::Array(a) => a.iter().any(|v| v.is_string() && address_like(v)),
        _ => false,
    }
}

/// Array addresses are string leaves only; data and nested opaque objects are never walked.
fn address(v: &Value, report: &mut Report) -> Option<Value> {
    match v {
        Value::String(s) => match durable::public_address(s) {
            Kept::Unchanged => None,
            Kept::Changed(s) => Some(Value::String(s)),
            Kept::Withheld(prefix) if prefix != s => {
                report.withheld = report.withheld.saturating_add(1);
                Some(Value::String(prefix.into()))
            }
            Kept::Withheld(_) => None,
        },
        Value::Array(a) => {
            let mut changed = None;
            for (i, v) in a.iter().enumerate() {
                if v.is_string()
                    && let Some(clean) = address(v, report)
                    && let Some(slot) = changed.get_or_insert_with(|| a.clone()).get_mut(i)
                {
                    *slot = clean;
                }
            }
            changed.map(Value::Array)
        }
        _ => None,
    }
}

/// Explicit executable and document roles are the only route into nested executable content.
fn envelope(id: &str, key: &str, v: &Value, siblings: &serde_json::Map<String, Value>, depth: usize, report: &mut Report) -> Option<Value> {
    match (id, key) {
        ("gp.run", "params") => {
            let (clean, r) = tool(siblings.get("tool").and_then(Value::as_str).unwrap_or(""), v, depth);
            report.merge(&r);
            clean
        }
        ("script.run", "script") => match v.as_str() {
            Some(s) => {
                let (clean, r) = script(s, depth);
                report.merge(&r);
                clean.map(Value::String)
            }
            None => invalid(v, report),
        },
        ("task.new", "steps") => typed_value::<VecTask>(v, depth, report),
        ("model.new", "steps") => typed_value::<VecSteps>(v, depth, report),
        (_, "graph") => typed_value::<crate::project::ModelGraph>(v, depth, report),
        (_, "text") => document(id, v, depth, report),
        _ => None,
    }
}

/// A typed envelope is prepared on its own copy, preserving the live outer params as a whole.
fn typed_value<T: DeserializeOwned + Serialize + Durable>(v: &Value, depth: usize, report: &mut Report) -> Option<Value> {
    let Ok(parsed) = serde_json::from_value::<T>(v.clone()) else { return invalid(v, report) };
    let Ok((clean, r)) = durable::output_owned_changed(parsed, depth, |v, changed| {
        if changed { serde_json::to_value(v).map(Some).map_err(|e| e.to_string()) } else { Ok(None) }
    }) else {
        return invalid(v, report);
    };
    report.merge(&r);
    clean
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
struct VecTask(Vec<crate::project::TaskStep>);

impl Durable for VecTask {
    /// Offers classified whole fields once, before any live mutation.
    fn prepare(&self, p: &mut durable::Prepare<'_>) {
        for (i, st) in self.0.iter().enumerate() {
            p.command(durable::Leaf::Task(i), &st.command, &st.params);
        }
    }

    /// Resolves a prepared location through owned fields, without allocation or policy.
    fn field_mut(&mut self, owners: &[durable::Owner], leaf: &durable::Leaf) -> Option<durable::FieldMut<'_>> {
        if !owners.is_empty() {
            return None;
        }
        let durable::Leaf::Task(i) = leaf else { return None };
        Some(durable::FieldMut::Json(&mut self.0.get_mut(*i)?.params))
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
struct VecSteps(Vec<crate::project::ModelStep>);

impl Durable for VecSteps {
    /// Offers classified whole fields once, before any live mutation.
    fn prepare(&self, p: &mut durable::Prepare<'_>) {
        for (i, st) in self.0.iter().enumerate() {
            p.tool(durable::Leaf::ModelStep(i), &st.tool, &st.params);
        }
    }

    /// Resolves a prepared location through owned fields, without allocation or policy.
    fn field_mut(&mut self, owners: &[durable::Owner], leaf: &durable::Leaf) -> Option<durable::FieldMut<'_>> {
        if !owners.is_empty() {
            return None;
        }
        let durable::Leaf::ModelStep(i) = leaf else { return None };
        Some(durable::FieldMut::Json(&mut self.0.get_mut(*i)?.params))
    }
}

/// Reads documents at the existing reader bound, without normalization, trust or restore-cache
/// access. Unchanged documents keep their exact text; only changed typed copies are serialized.
fn document(id: &str, v: &Value, depth: usize, report: &mut Report) -> Option<Value> {
    let Some(text) = v.as_str().filter(|s| s.len() as u64 <= crate::project::MAX_BYTES) else { return invalid(v, report) };
    let result = match id {
        "project.openText" => text_document::<crate::Project>(text, depth),
        "map.import" => {
            if serde_json::from_str::<crate::Project>(text).is_ok() {
                text_document::<crate::Project>(text, depth)
            } else {
                text_document::<rustgis_map::Map>(text, depth)
            }
        }
        "task.import" => text_document::<crate::project::Task>(text, depth),
        "presentation.import" => text_document::<rustgis_map::presentation::Presentation>(text, depth),
        "notebook.open" => crate::cmd::notebook::durable_document(text, depth),
        "model.import" => model_document(text, depth),
        _ => return None,
    };
    match result {
        Ok((clean, r)) => {
            let changed = clean != text;
            report.merge(&r);
            changed.then_some(Value::String(clean))
        }
        Err(_) => invalid(v, report),
    }
}

/// Uses the owning type's ordinary deserializer, without losing unchanged text representation.
fn text_document<T: DeserializeOwned + Serialize + Durable>(text: &str, depth: usize) -> Result<(String, Report), String> {
    let parsed = serde_json::from_str::<T>(text).map_err(|e| e.to_string())?;
    durable::output_owned_changed(parsed, depth, |value, changed| {
        if changed { serde_json::to_string(value).map_err(|e| e.to_string()) } else { Ok(text.into()) }
    })
}

/// Model import has two envelopes; keeps the envelope's other fields and cleans only its model.
fn model_document(text: &str, depth: usize) -> Result<(String, Report), String> {
    let mut v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut report = Report::default();
    let Some(obj) = v.as_object_mut() else { return Err("not a model".into()) };
    let (key, graph) = if obj.contains_key("graph") { ("graph", true) } else { ("steps", false) };
    let Some(field) = obj.get(key) else { return Err("not a model".into()) };
    let clean = if graph {
        typed_value::<crate::project::ModelGraph>(field, depth, &mut report)
    } else {
        typed_value::<VecSteps>(field, depth, &mut report)
    };
    if let Some(clean) = clean {
        obj.insert(key.into(), clean);
        Ok((v.to_string(), report))
    } else {
        Ok((text.into(), report))
    }
}

/// Uses the executor's line parser, preserving comments, blank lines and unchanged formatting.
pub(crate) fn script(source: &str, depth: usize) -> (Option<String>, Report) {
    let mut report = Report::default();
    let mut out = None;
    let mut offset = 0usize;
    for line in source.split_inclusive('\n') {
        let ending = if line.ends_with("\r\n") {
            "\r\n"
        } else if line.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        let replacement = match crate::script_line(line) {
            None => None,
            Some(Ok((id, params))) => {
                let (clean, r) = command(&id, &params, depth);
                report.merge(&r);
                clean.map(|params| format!("{id} {params}{ending}"))
            }
            Some(Err(_)) if suspicious(&Value::String(line.into())) => {
                report.withheld = report.withheld.saturating_add(1);
                let id = line.trim_start().split(char::is_whitespace).next().unwrap_or("");
                Some(format!("{id} {WITHHELD}{ending}"))
            }
            Some(Err(_)) => None,
        };
        if let Some(clean) = replacement {
            report.lines = report.lines.saturating_add(1);
            out.get_or_insert_with(|| source.get(..offset).unwrap_or("").to_string()).push_str(&clean);
        } else if let Some(out) = &mut out {
            out.push_str(line);
        }
        offset += line.len();
    }
    (out, report)
}
