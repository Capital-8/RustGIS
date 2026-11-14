//! Photos attached to features: the device's camera takes one (`Services::take_photo`), and the
//! feature names it in its layer's photos field, a text field listing each photo's path,
//! separated by `;` (as a report's attachments field does). A layer without one gets a `photos`
//! field when its first photo is attached, and the sheet says so. The field stays in the form, to
//! be typed into as any other: the photos taken go after what is typed there.

use rustgis_ui_egui::rustgis_engine::data::FieldType;
use rustgis_ui_egui::rustgis_engine::map::Layer;
use rustgis_ui_egui::{RustGisApp, i18n};
use serde_json::json;
use std::path::PathBuf;

/// The name of the field a layer is given for its photos.
pub(crate) const FIELD: &str = "photos";

/// The layer's photos field: its text field named `photos` or `photo`, whatever the case.
pub(crate) fn field(layer: &Layer) -> Option<String> {
    layer
        .dataset
        .fields
        .iter()
        .find(|f| f.ty == FieldType::Text && (f.name.eq_ignore_ascii_case(FIELD) || f.name.eq_ignore_ascii_case("photo")))
        .map(|f| f.name.clone())
}

/// Layer `id`'s photos field, added (and said) when it has none. `None` when it couldn't be
/// added (the front end says why), and when the layer has a field of that name that isn't text,
/// which is said by name.
pub(crate) fn ensure_field(app: &mut RustGisApp, id: u32) -> Option<String> {
    let layer = app.session.map().layer(id)?;
    if let Some(found) = field(layer) {
        return Some(found);
    }
    let name = layer.name.clone();
    let other = layer.dataset.fields.iter().find(|f| f.name.eq_ignore_ascii_case(FIELD) || f.name.eq_ignore_ascii_case("photo"));
    if let Some(other) = other {
        let text = i18n::fmt(
            "The photos weren't kept with the feature: {layer}'s field {field} isn't a text field to list them in.",
            &[("layer", &name), ("field", &other.name)],
        );
        app.session.notify(text, true);
        return None;
    }
    app.run_by_user("edit.addField", json!({"layer": id, "name": FIELD, "type": "text"})).ok()?;
    app.session.notify(i18n::fmt("Added a photos field to {layer}: it lists each feature's photos.", &[("layer", &name)]), false);
    app.session.map().layer(id).and_then(field)
}

/// A photos field's text with `more` photos after what it held: paths separated by `;`, none
/// listed twice.
pub(crate) fn joined(held: &str, more: &[PathBuf]) -> String {
    let mut all: Vec<String> = held.split(';').map(str::trim).filter(|p| !p.is_empty()).map(String::from).collect();
    for path in more {
        let path = path.display().to_string();
        if !all.contains(&path) {
            all.push(path);
        }
    }
    all.join(";")
}

/// Attaches `more` photos to the feature in row `row` of layer `id`, after the ones it has: one
/// edit, which undoes (the first photo of a layer without a photos field also adds the field, an
/// edit of its own). Says whether they were attached.
pub(crate) fn attach(app: &mut RustGisApp, id: u32, row: usize, more: &[PathBuf]) -> bool {
    if more.is_empty() {
        return true;
    }
    if !super::edit::ensure_editing(app) {
        return false;
    }
    let Some(name) = ensure_field(app, id) else { return false };
    let held = app
        .session
        .map()
        .layer(id)
        .and_then(|l| Some(l.dataset.value(row, l.dataset.field_index(&name)?).to_string()))
        .filter(|v| v != "<Null>")
        .unwrap_or_default();
    let value = joined(&held, more);
    if app.run_by_user("edit.setValue", json!({"layer": id, "row": row, "field": name, "value": value})).is_err() {
        return false;
    }
    let n = more.len().to_string();
    app.session.notify(i18n::fmt("Attached {n} photo(s) to the feature.", &[("n", &n)]), false);
    true
}

/// A photo's file name, for its chip.
pub(crate) fn name(path: &std::path::Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photos_are_listed_by_path_once_each() {
        let a = PathBuf::from("/data/photos/a.jpg");
        let b = PathBuf::from("/data/photos/b.jpg");
        assert_eq!(joined("", std::slice::from_ref(&a)), "/data/photos/a.jpg");
        assert_eq!(joined("/data/photos/a.jpg", &[a.clone(), b.clone()]), "/data/photos/a.jpg;/data/photos/b.jpg");
        assert_eq!(joined(" ; /x.jpg ;", &[]), "/x.jpg");
        assert_eq!(name(&b), "b.jpg");
    }
}
