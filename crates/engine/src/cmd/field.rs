//! Field packages (#1902): a map taken to a phone and brought back.
//!
//! A field package (`.rgisfield`) is a zip holding a project of one map, `<area> (offline)`, and
//! the files it reads beside it: the MBTiles and PMTiles archives its tile layers draw from, under
//! `basemaps/`, so the map has its basemap with no network. Packaging the active map makes an
//! offline area of it in this project first (as Download Map does), so the edits made in the
//! field can be brought back to it by feature, with the offline area's conflict handling.
//!
//! Opening a field package unpacks it into the app's own folder (`field/<name>`) and opens the
//! project there, unsaved, as a project package opens.
//!
//! Coming back, the field's map is packaged as it is, with the photos its features name (in a
//! layer's `photos` field, paths separated by `;`) under `photos/`. Bring Back Edits
//! (`offline.bringBack`) reads such a package in the project it was made from, puts its photos in
//! a folder beside that project, and copies the edits back to the area's map by feature, as Sync
//! does, saying which features were changed on both sides.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::Session;
use crate::project::Project;

/// A field package's extension.
pub const EXTENSION: &str = "rgisfield";
/// The folder of a field package that holds its tile archives.
const BASEMAPS: &str = "basemaps";
/// The folder of a field package that holds its features' photos.
const PHOTOS: &str = "photos";
/// What an offline area's map is called after: `<area> (offline)`.
pub(crate) const OFFLINE: &str = " (offline)";
/// The most bytes read from one field package's entries when it is opened.
const MOST_UNPACKED: usize = rustgis_data::zip::MAX_TOTAL;

/// Whether a file name or path is a field package's, ignoring case.
pub fn is_field_package(name: &str) -> bool {
    name.to_ascii_lowercase().strip_suffix(EXTENSION).is_some_and(|s| s.ends_with('.'))
}

/// A field package of the active map, as (name, bytes).
///
/// An offline area's map (`<area> (offline)`, as a field package opens) is packaged as it is, to
/// bring its edits back. Any other map is first made an offline area of this project (`extent`
/// and `name` as for `offline.download`, the view by default), whose map is packaged.
///
/// Never silent: the raster layers an offline area leaves behind, the tile layers that stay
/// online (a web address, not an archive), and an archive that couldn't be read are said.
///
/// It runs where it is called, on the window's thread from Save (`ui.saveItem`), as Package
/// Project does: each archive is one read and the zip one write, with no Cancel between them.
pub(crate) fn package(s: &mut Session, p: &Value) -> Result<(String, Vec<u8>), String> {
    let returning = s.map().name.strip_suffix(OFFLINE).map(str::to_string);
    // Only a map coming back carries its photos: going out, the photos named are at home.
    let coming_back = returning.is_some();
    let (area, mut map) = match returning {
        Some(area) => (area, s.map().clone()),
        None => {
            let rasters = s.map().layers.iter().filter(|l| l.is_raster()).count();
            let made = super::workspace::offline_download(s, p)?;
            let name = made.get("name").and_then(Value::as_str).ok_or("the offline area wasn't made")?.to_string();
            let data =
                s.project.offline_areas.iter().find(|a| a.name == name).map(|a| a.data.clone()).ok_or("the offline area wasn't made")?;
            if rasters > 0 {
                s.notify(
                    format!("{rasters} raster layer(s) were left out of the field package: an offline area holds feature layers"),
                    false,
                );
            }
            s.notify(
                format!("Made the offline area '{name}' of this map for the field package: save this project to bring the field's edits back to it later"),
                false,
            );
            let mut m = data;
            m.name = format!("{name}{OFFLINE}");
            (name, m)
        }
    };
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut budget = MOST_UNPACKED as u64 / 2;
    // Each local archive goes in beside the project, its layer reading it from there.
    let (mut online, mut unread) = (Vec::new(), Vec::new());
    let mut taken = std::collections::HashSet::new();
    for t in &mut map.tiles {
        let Some(file) = t.archive_file().map(str::to_string) else {
            online.push(t.name.clone());
            continue;
        };
        match crate::project::read_file(Path::new(&file), budget) {
            Ok(bytes) => {
                budget = budget.saturating_sub(bytes.len() as u64);
                let stem = Path::new(&file).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "basemap".into());
                let mut name = stem.clone();
                for n in 2.. {
                    if taken.insert(name.to_ascii_lowercase()) {
                        break;
                    }
                    name = format!("{n}-{stem}");
                }
                t.url = format!("{BASEMAPS}/{name}");
                entries.push((t.url.clone(), bytes));
            }
            Err(e) => unread.push(format!("{} ({e})", t.name)),
        }
    }
    if !online.is_empty() {
        s.notify(
            format!(
                "{} tile layer(s) in the field package draw from the web and show only online: {}. Add an MBTiles or PMTiles archive for a basemap with no network",
                online.len(),
                online.join(", ")
            ),
            false,
        );
    }
    if !unread.is_empty() {
        s.notify(format!("{} tile archive(s) couldn't be put in the field package: {}", unread.len(), unread.join(", ")), true);
    }
    // Coming back, the photos taken in the field go with their features.
    if coming_back {
        let (photos, unread) = take_photos(&mut map, &mut budget);
        if !unread.is_empty() {
            s.notify(format!("{} photo(s) couldn't be put in the field package: {}", unread.len(), unread.join(", ")), true);
        }
        entries.extend(photos);
    }
    // A project of the one map: what else the project holds stays at home.
    let mut project = s.project.clone();
    project.name = area.clone();
    project.maps = vec![super::items::embedded_map(&map, &mut budget)];
    project.active_map = 0;
    project.offline_areas.clear();
    let readme = format!(
        "{area}: a RustGIS field package.\nOpen it with RustGIS on a phone or a computer (Open Project). Its basemaps are in {BASEMAPS}/.\n"
    );
    let room = crate::project::max_bytes().min(MOST_UNPACKED as u64).saturating_sub(entries.iter().map(|(_, b)| b.len() as u64).sum());
    let too_large = || "the field package would be too large: leave a tile archive or some data out".to_string();
    let (text, report) = crate::durable::output_owned(project, |p| crate::project::json_within(p, room)?.ok_or_else(too_large))?;
    crate::durable::notify(s, &report);
    entries.insert(0, (format!("{}.{}", super::items::safe(&area), crate::project::EXTENSION), text.into_bytes()));
    entries.push(("README.txt".into(), readme.into_bytes()));
    Ok((area, rustgis_data::zip::write(&entries)?))
}

/// Whether `map` is an offline area's map (`<area> (offline)`), packaged to come back.
fn returning_map(map: &rustgis_map::Map) -> bool {
    map.name.ends_with(OFFLINE)
}

/// A layer's photos field, by its place: its text field named `photos` or `photo`, whatever the
/// case (as the touch interface names it).
fn photos_field(layer: &rustgis_map::Layer) -> Option<usize> {
    layer.dataset.fields.iter().position(|f| {
        f.ty == rustgis_data::FieldType::Text && (f.name.eq_ignore_ascii_case("photos") || f.name.eq_ignore_ascii_case("photo"))
    })
}

/// Rewrites each photos field of `map` with `rewrite` (one path in, the path to name instead out).
fn rewrite_photos(map: &mut rustgis_map::Map, mut rewrite: impl FnMut(&str) -> String) {
    for l in map.layers.iter_mut().filter(|l| !l.is_raster()) {
        let Some(fi) = photos_field(l) else { continue };
        for f in &mut l.dataset.features {
            let Some(rustgis_data::Value::Text(held)) = f.attrs.get(fi) else { continue };
            let paths: Vec<String> = held.split(';').map(str::trim).filter(|p| !p.is_empty()).map(&mut rewrite).collect();
            if let Some(slot) = f.attrs.get_mut(fi) {
                *slot = rustgis_data::Value::Text(paths.join(";"));
            }
        }
    }
}

/// The photo files `map`'s features name that are on this device, as package entries under
/// `photos/` (within `budget`), each feature then naming its photos there; and the photos named
/// that couldn't be read. Only picture files ([`PICTURES`]) named by an absolute path go in, each
/// under a plain name ([`plain_name`]): a photos field is free text, and could name any file. A
/// photo named that isn't on this device (one named at home) is left as it is named.
fn take_photos(map: &mut rustgis_map::Map, budget: &mut u64) -> (Vec<(String, Vec<u8>)>, Vec<String>) {
    let (mut entries, mut unread) = (Vec::new(), Vec::new());
    let mut placed: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut taken = std::collections::HashSet::new();
    rewrite_photos(map, |path| {
        if let Some(entry) = placed.get(path) {
            return entry.clone();
        }
        let file = Path::new(path);
        // A picture only: a photos field is free text, and could name any file on the device.
        let picture = file.extension().is_some_and(|e| PICTURES.iter().any(|p| e.eq_ignore_ascii_case(p)));
        if !file.is_absolute() || !picture || !file.is_file() {
            return path.to_string();
        }
        match crate::project::read_file(file, *budget) {
            Ok(bytes) => {
                *budget = budget.saturating_sub(bytes.len() as u64);
                let stem = file.file_name().map(|n| plain_name(&n.to_string_lossy())).unwrap_or_else(|| "photo.jpg".into());
                let mut name = stem.clone();
                for n in 2.. {
                    if taken.insert(name.to_ascii_lowercase()) {
                        break;
                    }
                    name = format!("{n}-{stem}");
                }
                let entry = format!("{PHOTOS}/{name}");
                entries.push((entry.clone(), bytes));
                placed.insert(path.to_string(), entry.clone());
                entry
            }
            Err(e) => {
                unread.push(format!("{path} ({e})"));
                path.to_string()
            }
        }
    });
    (entries, unread)
}

/// The extensions of the picture files a returning field package carries.
const PICTURES: &[&str] = &["jpg", "jpeg", "png", "heic", "heif", "webp"];

/// A file name with nothing a package's photo entry may not hold: no folder separator or drive
/// colon, and not hidden (a leading dot).
fn plain_name(name: &str) -> String {
    let name: String = name.chars().map(|c| if matches!(c, '/' | '\\' | ':') { '_' } else { c }).collect();
    match name.strip_prefix('.') {
        Some(rest) => format!("_{rest}"),
        None => name,
    }
}

/// Brings a field package's edits back (`offline.bringBack`): the package (`path`, or `data` in
/// base64) holds an offline area's map, which must be an area of this project. Its photos go in a
/// folder beside this project (`<area> photos`), or the app's own `field photos/<area>` for a
/// project not saved yet, and its features name them there; one whose name is taken there by
/// other bytes gets a name of its own (`2-<name>`), and an entry that isn't a plain file name is
/// left out and counted. Then its edits are copied back to the area's map by feature, as Sync does
/// (`sync_area`), which says the conflicts.
///
/// It runs where it is called, on the window's thread, as Sync does: the package is one read, each
/// photo one write, and the sync one pass over the area's features, with no Cancel between them.
pub(crate) fn bring_back(s: &mut Session, p: &Value) -> Result<Value, String> {
    let bytes = match super::ps(p, "path") {
        Some(path) => crate::project::read_file(Path::new(path), MOST_UNPACKED as u64)?,
        None => {
            use base64::Engine;
            let data = super::ps(p, "data").ok_or("path or data is required")?;
            base64::engine::general_purpose::STANDARD.decode(data.trim()).map_err(|_| "data must be base64".to_string())?
        }
    };
    let entries = rustgis_data::zip::read(&bytes)?;
    let mut text = None;
    let mut photos = Vec::new();
    // Photo entries that aren't a plain file name, left out.
    let mut strange = 0usize;
    for (name, data) in entries {
        if !name.contains('/') && crate::project::is_project_file(&name) {
            text = Some(String::from_utf8(data).map_err(|_| "the field package's project is damaged".to_string())?);
        } else if let Some(file) = name.strip_prefix(&format!("{PHOTOS}/")) {
            // A photo's own name only: nothing that would put it outside its folder.
            if !file.is_empty() && !file.contains(['/', '\\', ':']) && file != ".." && !file.starts_with('.') {
                photos.push((file.to_string(), data));
            } else {
                strange += 1;
            }
        }
    }
    let text = text.ok_or("the file holds no field package's project")?;
    let project = Project::from_import_json(&text, None)?;
    let mut edited = project
        .maps
        .into_iter()
        .find(returning_map)
        .ok_or("the field package holds no offline area's map: package the map in the field (Send Field Package) to bring it back")?;
    let area = edited.name.strip_suffix(OFFLINE).unwrap_or_default().to_string();
    let ai = s.project.offline_areas.iter().position(|a| a.name.eq_ignore_ascii_case(&area)).ok_or_else(|| {
        format!(
            "this project has no offline area '{area}': open the project the field package was made from, as it was saved after packaging"
        )
    })?;
    // The photos, beside the project, and the features naming them there.
    let mut put = 0usize;
    let mut renamed = 0usize;
    let mut folder = None;
    if !photos.is_empty() {
        let dir = match s.path.as_deref().and_then(Path::parent) {
            Some(home) => home.join(format!("{} photos", super::items::safe(&area))),
            None => rustgis_data::dirs::app_data_dir()
                .ok_or("this computer gives RustGIS no folder for the field package's photos: save the project first")?
                .join("field photos")
                .join(super::items::safe(&area)),
        };
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut written = std::collections::HashMap::new();
        for (name, data) in photos {
            // A file of that name already there (an earlier package's) is kept: the same bytes
            // are that photo, other bytes get a name of their own.
            let mut path = dir.join(&name);
            for n in 2.. {
                match std::fs::read(&path) {
                    Ok(there) if there == data => break,
                    Ok(_) => path = dir.join(format!("{n}-{name}")),
                    Err(_) => break,
                }
            }
            if path.file_name().is_some_and(|f| f.to_string_lossy() != name) {
                renamed += 1;
            }
            std::fs::write(&path, data).map_err(|e| format!("{}: {e}", path.display()))?;
            written.insert(format!("{PHOTOS}/{name}"), path.display().to_string());
            put += 1;
        }
        rewrite_photos(&mut edited, |path| written.get(path).cloned().unwrap_or_else(|| path.to_string()));
        folder = Some(dir.display().to_string());
    }
    // Fields the field added (a layer's photos field, made with its first photo) come too.
    let added = take_new_fields(s, ai, &edited);
    if !added.is_empty() {
        s.notify(format!("Added the field(s) the field package's layers added: {}", added.join(", ")), false);
    }
    let mut done = super::workspace::sync_area(s, ai, &edited)?;
    let said = |k: &str| done.get(k).and_then(Value::as_u64).unwrap_or(0);
    let mut summary = format!(
        "Brought the field's edits back to offline area '{area}': {} feature(s) changed, {} added, {} deleted",
        said("updated"),
        said("added"),
        said("deleted")
    );
    if let Some(dir) = &folder {
        summary.push_str(&format!("; {put} photo(s) put in {dir}"));
        if renamed > 0 {
            summary.push_str(&format!(" ({renamed} under a new name: one of its name was there already)"));
        }
    }
    if strange > 0 {
        s.notify(format!("{strange} photo(s) in the field package weren't brought back: their names aren't plain file names"), true);
    }
    s.notify(summary, false);
    if let Some(o) = done.as_object_mut() {
        o.insert("area".into(), json!(area));
        o.insert("photos".into(), json!(put));
        o.insert("photoFolder".into(), json!(folder));
    }
    Ok(done)
}

/// Adds to offline area `ai`'s source layers, and to the area's own copy of them (so features
/// aren't all taken as edited for lacking it), the fields `edited`'s layers have after theirs: a
/// field the field added. Only fields added after all of the layer's own, of names it hasn't;
/// fields that differ otherwise are left for the sync to refuse. Returns them as `layer: field`.
fn take_new_fields(s: &mut Session, ai: usize, edited: &rustgis_map::Map) -> Vec<String> {
    let Some(area) = s.project.offline_areas.get(ai) else { return Vec::new() };
    let source = area.map.clone();
    let same = |a: &[rustgis_data::Field], b: &[rustgis_data::Field]| {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.name.eq_ignore_ascii_case(&y.name) && x.ty == y.ty)
    };
    let mut added = Vec::new();
    for ol in edited.layers.iter().filter(|l| !l.is_raster()) {
        let fields = &ol.dataset.fields;
        let Some(si) = s.project.maps.iter().position(|m| m.name.eq_ignore_ascii_case(&source)) else { break };
        let Some(sl) = s.project.maps.get(si).and_then(|m| m.layers.iter().find(|l| l.id == ol.id && !l.is_raster())) else { continue };
        let had = sl.dataset.fields.len();
        let base_same = s
            .project
            .offline_areas
            .get(ai)
            .and_then(|a| a.data.layers.iter().find(|l| l.id == ol.id))
            .is_none_or(|b| same(&b.dataset.fields, &sl.dataset.fields));
        let Some(extra) = fields.get(had..).filter(|e| !e.is_empty()) else { continue };
        if !base_same || !fields.get(..had).is_some_and(|own| same(own, &sl.dataset.fields)) {
            continue;
        }
        if extra.iter().any(|f| sl.dataset.field_index(&f.name).is_some()) {
            continue;
        }
        let name = sl.name.clone();
        if let Some(l) = s.project.maps.get_mut(si).and_then(|m| m.layers.iter_mut().find(|l| l.id == ol.id)) {
            for f in extra {
                l.dataset.ensure_field(&f.name, f.ty);
            }
        }
        if let Some(b) = s.project.offline_areas.get_mut(ai).and_then(|a| a.data.layers.iter_mut().find(|l| l.id == ol.id)) {
            for f in extra {
                b.dataset.ensure_field(&f.name, f.ty);
            }
        }
        added.extend(extra.iter().map(|f| format!("{name}: {}", f.name)));
    }
    added
}

/// The folder a field package named `stem` is unpacked into: the app's own `field/<stem>`.
fn unpack_dir(stem: &str) -> Result<PathBuf, String> {
    let base =
        rustgis_data::dirs::app_data_dir().ok_or("this computer gives RustGIS no folder of its own to unpack the field package into")?;
    Ok(fresh_dir(&base.join("field"), &super::items::safe(stem)))
}

/// A folder in `base` named `name`, or `name 2`, `name 3`… when one is there: a copy unpacked
/// before (another package of the same name, perhaps still open) is never written over.
fn fresh_dir(base: &Path, name: &str) -> PathBuf {
    let mut dir = base.join(name);
    for n in 2.. {
        if !dir.exists() {
            break;
        }
        dir = base.join(format!("{name} {n}"));
    }
    dir
}

/// Unpacks a field package's `bytes` into a folder of its own in the app's folder (`field/<name>`,
/// numbered when that is taken) and returns the folder and the project's text. An entry that
/// would land outside the folder is refused, and the package with it.
///
/// The project's own paths are trusted as a project file's are: a package from someone else can
/// name files outside the folder, as a `.rgis` can.
pub(crate) fn unpack(bytes: &[u8], stem: &str) -> Result<(PathBuf, String), String> {
    let dir = unpack_dir(stem)?;
    let text = unpack_into(bytes, &dir)?;
    Ok((dir, text))
}

/// Unpacks a field package's `bytes` into `dir`, a folder not there yet (see [`unpack`]), and
/// returns the project's text. A write that fails removes what was written.
fn unpack_into(bytes: &[u8], dir: &Path) -> Result<String, String> {
    let entries = rustgis_data::zip::read(bytes)?;
    let mut project = None;
    let mut files = Vec::new();
    for (name, data) in entries {
        let parts: Vec<&str> = name.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
        if parts.is_empty() || parts.iter().any(|p| *p == ".." || *p == "." || p.contains(':')) || name.starts_with(['/', '\\']) {
            return Err(format!("the field package holds an entry outside its folder: {name}"));
        }
        if parts.len() == 1 && crate::project::is_project_file(&name) {
            project = Some(String::from_utf8(data).map_err(|_| "the field package's project is damaged".to_string())?);
        } else {
            files.push((parts.iter().collect::<PathBuf>(), data));
        }
    }
    let text = project.ok_or("the field package holds no project")?;
    if dir.exists() {
        return Err(format!("{}: the folder is already there", dir.display()));
    }
    let written = (|| {
        for (rel, data) in files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            std::fs::write(&path, data).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok::<(), String>(())
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_dir_all(dir);
        return Err(e);
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(text)
}

/// Reads a field package's project, its relative paths resolved against `dir`.
pub(crate) fn project_in(text: &str, dir: &Path) -> Result<Project, String> {
    Project::from_import_json(text, Some(dir))
}

/// Removes the offline areas past the first `had` of this project: those a field package just made,
/// when it wasn't saved after all (the save was cancelled or failed). Says so.
pub fn forget_new_areas(s: &mut Session, had: usize) {
    let made: Vec<String> = s.project.offline_areas.iter().skip(had).map(|a| a.name.clone()).collect();
    for name in made {
        s.project.offline_areas.retain(|a| a.name != name);
        s.notify(format!("The field package wasn't saved, so its offline area '{name}' was removed too"), false);
    }
}

/// What `offline.package` returns.
pub(crate) fn packaged(path: &str, area: &str, bytes: usize) -> Value {
    json!({"path": path, "area": area, "bytes": bytes})
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustgis_map::tiles::{TileKind, TileLayer};

    /// A folder of the test's own, removed when it ends.
    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Temp {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let dir = std::env::temp_dir().join(format!("rustgis-field-{name}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Temp(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample() -> Session {
        Session::new(crate::sample::world_project())
    }

    #[test]
    fn a_map_packaged_for_the_field_opens_with_its_basemap_and_comes_from_an_offline_area() {
        let temp = Temp::new("package");
        let archive = temp.path().join("site.mbtiles");
        std::fs::write(&archive, b"stand-in archive bytes").unwrap();
        let mut s = sample();
        s.map_mut().tiles.push(TileLayer::new("Site", TileKind::MbTiles, archive.display().to_string()));
        s.map_mut().tiles.push(TileLayer::new("Streets", TileKind::Raster, "https://tiles.example.com/{z}/{x}/{y}.png"));
        let mark = s.notice_mark();
        let (area, bytes) = package(&mut s, &json!({"name": "Survey", "extent": [-20.0, -40.0, 60.0, 40.0]})).unwrap();
        assert_eq!(area, "Survey");
        // The offline area is in this project, to bring the edits back to.
        assert!(s.project.offline_areas.iter().any(|a| a.name == "Survey"));
        // The online tile layer is said; so is the area made.
        let said: Vec<String> = s.notices_since(mark).iter().map(|n| n.text.clone()).collect();
        assert!(said.iter().any(|t| t.contains("Streets") && t.contains("online")), "{said:?}");
        assert!(said.iter().any(|t| t.contains("offline area 'Survey'")), "{said:?}");
        // The package: the project, the archive under basemaps/, a README.
        let entries = rustgis_data::zip::read(&bytes).unwrap();
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Survey.rgis", "basemaps/site.mbtiles", "README.txt"]);
        assert_eq!(entries[1].1, b"stand-in archive bytes");
        // Unpacked and read: one map, the area's, its features in it, its basemap read from beside it.
        let into = temp.path().join("unpacked");
        let text = unpack_into(&bytes, &into).unwrap();
        let project = project_in(&text, &into).unwrap();
        assert_eq!(project.maps.len(), 1);
        let map = &project.maps[0];
        assert_eq!(map.name, "Survey (offline)");
        assert!(map.layers.iter().any(|l| !l.dataset.features.is_empty()));
        let base = map.tiles.iter().find(|t| t.name == "Site").unwrap();
        assert_eq!(Path::new(&base.url), into.join("basemaps/site.mbtiles"));
        assert!(into.join("basemaps/site.mbtiles").is_file());
        // Unpacked again: into a folder of its own, the first left as it is.
        assert!(unpack_into(&bytes, &into).unwrap_err().contains("already there"));
        let again = fresh_dir(temp.path(), "unpacked");
        assert_eq!(again, temp.path().join("unpacked 2"));
        unpack_into(&bytes, &again).unwrap();
        assert!(into.join("basemaps/site.mbtiles").is_file() && again.join("basemaps/site.mbtiles").is_file());
    }

    #[test]
    fn a_field_package_not_saved_leaves_no_offline_area() {
        let mut s = sample();
        let had = s.project.offline_areas.len();
        // A place that can't be written: the command is refused, and the area made for it goes.
        let e = s.run("offline.package", &json!({"path": "/no/such/folder/site.rgisfield", "name": "Lost"})).unwrap_err();
        assert!(e.contains("site.rgisfield"), "{e}");
        assert_eq!(s.project.offline_areas.len(), had);
        // As the front end does when its Save is cancelled: the area goes, and that is said.
        package(&mut s, &json!({"name": "Cancelled"})).unwrap();
        assert_eq!(s.project.offline_areas.len(), had + 1);
        let mark = s.notice_mark();
        forget_new_areas(&mut s, had);
        assert_eq!(s.project.offline_areas.len(), had);
        assert!(s.notices_since(mark).iter().any(|n| n.text.contains("'Cancelled' was removed")));
    }

    #[test]
    fn an_offline_map_is_packaged_as_it_is_to_go_back() {
        let mut s = sample();
        s.run("offline.download", &json!({"name": "Back", "extent": [-20.0, -40.0, 60.0, 40.0]})).unwrap();
        s.run("offline.open", &json!({"name": "Back"})).unwrap();
        assert_eq!(s.map().name, "Back (offline)");
        let areas = s.project.offline_areas.len();
        let (area, bytes) = package(&mut s, &Value::Null).unwrap();
        assert_eq!(area, "Back");
        // No new area: the map already is one.
        assert_eq!(s.project.offline_areas.len(), areas);
        let entries = rustgis_data::zip::read(&bytes).unwrap();
        assert_eq!(entries[0].0, "Back.rgis");
    }

    /// Layer `name`'s id in the active map.
    fn layer(s: &Session, name: &str) -> u32 {
        s.map().layers.iter().find(|l| l.name == name).map(|l| l.id).unwrap()
    }

    /// The text of field `field` in row `row` of layer `name`.
    fn value(s: &Session, name: &str, row: usize, field: &str) -> String {
        let l = s.map().layers.iter().find(|l| l.name == name).unwrap();
        l.dataset.value(row, l.dataset.field_index(field).unwrap()).to_string()
    }

    #[test]
    fn the_fields_edits_and_photos_come_back_to_the_map_they_were_packaged_from() {
        let temp = Temp::new("round-trip");
        let mut home = sample();
        home.path = Some(temp.path().join("home.rgis"));
        let (_, out) = package(&mut home, &json!({"name": "Trip", "extent": [-20.0, -40.0, 60.0, 40.0]})).unwrap();
        // In the field: the package opened, a city renamed, one added, one deleted, a photo taken.
        let unpacked = temp.path().join("field");
        let text = unpack_into(&out, &unpacked).unwrap();
        let mut field = Session::new(project_in(&text, &unpacked).unwrap());
        assert_eq!(field.map().name, "Trip (offline)");
        let cities = layer(&field, "Cities");
        let name =
            field.map().layer(cities).unwrap().dataset.fields.iter().find(|f| f.ty == rustgis_data::FieldType::Text).unwrap().name.clone();
        let renamed = field.map().layer(cities).unwrap().dataset.features[0].id;
        field.run("edit.setValue", &json!({"layer": cities, "row": 0, "field": name, "value": "Field Camp"})).unwrap();
        field.run("edit.addFeature", &json!({"layer": cities, "geometry": {"type": "Point", "coordinates": [10.0, 5.0]}, "attributes": {name.clone(): "New Site"}})).unwrap();
        let gone = field.map().layer(cities).unwrap().dataset.features[1].id;
        field.run("select.rows", &json!({"layer": cities, "rows": [1]})).unwrap();
        field.run("edit.delete", &Value::Null).unwrap();
        let photo = temp.path().join("photo-1.jpg");
        std::fs::write(&photo, b"jpeg bytes").unwrap();
        field.run("edit.addField", &json!({"layer": cities, "name": "photos", "type": "text"})).unwrap();
        field.run("edit.setValue", &json!({"layer": cities, "row": 0, "field": "photos", "value": photo.display().to_string()})).unwrap();
        // Sent back: the photo goes with it, its feature naming it in the package.
        let (area, back) = package(&mut field, &Value::Null).unwrap();
        assert_eq!(area, "Trip");
        let entries = rustgis_data::zip::read(&back).unwrap();
        assert!(entries.iter().any(|(n, b)| n == "photos/photo-1.jpg" && b == b"jpeg bytes"));
        // Meanwhile at home, the renamed city was changed too: a conflict, said.
        let row = home.map().layer(layer(&home, "Cities")).unwrap().dataset.features.iter().position(|f| f.id == renamed).unwrap();
        let home_cities = layer(&home, "Cities");
        home.run("edit.setValue", &json!({"layer": home_cities, "row": row, "field": name, "value": "Home Edit"})).unwrap();
        let before = home.map().layer(home_cities).unwrap().dataset.features.len();
        let sent = temp.path().join("back.rgisfield");
        std::fs::write(&sent, &back).unwrap();
        let mark = home.notice_mark();
        let done = home.run("offline.bringBack", &json!({"path": sent.display().to_string()})).unwrap();
        assert_eq!((done["updated"].as_u64(), done["added"].as_u64(), done["deleted"].as_u64()), (Some(1), Some(1), Some(1)), "{done}");
        let home_layer = home.map().layer(home_cities).unwrap();
        assert_eq!(home_layer.dataset.features.len(), before);
        assert!(!home_layer.dataset.features.iter().any(|f| f.id == gone));
        let row = home_layer.dataset.features.iter().position(|f| f.id == renamed).unwrap();
        assert_eq!(value(&home, "Cities", row, &name), "Field Camp");
        // The photo is beside the project, and the feature names it there.
        let kept = temp.path().join("Trip photos").join("photo-1.jpg");
        assert_eq!(std::fs::read(&kept).unwrap(), b"jpeg bytes");
        assert_eq!(value(&home, "Cities", row, "photos"), kept.display().to_string());
        let said: Vec<String> = home.notices_since(mark).iter().map(|n| n.text.clone()).collect();
        assert!(said.iter().any(|t| t.contains("changed in the map") && t.contains("Cities")), "{said:?}");
        assert!(said.iter().any(|t| t.contains("Brought the field's edits back") && t.contains("1 photo(s)")), "{said:?}");
        // A package for an area this project doesn't have is refused, by name.
        let mut other = sample();
        let e = other.run("offline.bringBack", &json!({"path": sent.display().to_string()})).unwrap_err();
        assert!(e.contains("no offline area 'Trip'"), "{e}");
    }

    #[test]
    fn photos_go_back_only_as_pictures_and_never_replace_one_already_home() {
        let temp = Temp::new("photos");
        let mut home = sample();
        home.path = Some(temp.path().join("home.rgis"));
        let cities = layer(&home, "Cities");
        home.run("edit.addField", &json!({"layer": cities, "name": "photos", "type": "text"})).unwrap();
        let secret = temp.path().join("notes.txt");
        let picture = temp.path().join(".pending.jpg");
        std::fs::write(&secret, b"not a picture").unwrap();
        std::fs::write(&picture, b"first").unwrap();
        let named = format!("{};{}", secret.display(), picture.display());
        home.run("edit.setValue", &json!({"layer": cities, "row": 0, "field": "photos", "value": named})).unwrap();
        // Going out: no photos, whatever the features name.
        let (_, out) = package(&mut home, &json!({"name": "Pics", "extent": [-180.0, -85.0, 180.0, 85.0]})).unwrap();
        assert!(rustgis_data::zip::read(&out).unwrap().iter().all(|(n, _)| !n.starts_with("photos/")));
        // Coming back: the picture only, under a plain name; the text file stays named as it was.
        let unpacked = temp.path().join("field");
        let text = unpack_into(&out, &unpacked).unwrap();
        let mut field = Session::new(project_in(&text, &unpacked).unwrap());
        let back = |field: &mut Session| package(field, &Value::Null).unwrap().1;
        let entries = rustgis_data::zip::read(&back(&mut field)).unwrap();
        let photos: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).filter(|n| n.starts_with("photos/")).collect();
        assert_eq!(photos, ["photos/_pending.jpg"]);
        let first = back(&mut field);
        // A second package's photo of that name, other bytes: kept beside the first, renamed.
        std::fs::write(&picture, b"second").unwrap();
        let second = back(&mut field);
        let path = temp.path().join("back.rgisfield");
        std::fs::write(&path, &first).unwrap();
        home.run("offline.bringBack", &json!({"path": path.display().to_string()})).unwrap();
        std::fs::write(&path, &second).unwrap();
        let mark = home.notice_mark();
        home.run("offline.bringBack", &json!({"path": path.display().to_string()})).unwrap();
        let dir = temp.path().join("Pics photos");
        assert_eq!(std::fs::read(dir.join("_pending.jpg")).unwrap(), b"first");
        assert_eq!(std::fs::read(dir.join("2-_pending.jpg")).unwrap(), b"second");
        assert!(home.notices_since(mark).iter().any(|n| n.text.contains("under a new name")));
    }

    #[test]
    fn a_package_entry_outside_its_folder_is_refused() {
        let temp = Temp::new("refused");
        for bad in ["../escape.txt", "/abs.txt", "a/../../b.txt", "C:/x.txt"] {
            let bytes = rustgis_data::zip::write(&[("p.rgis".into(), b"{}".to_vec()), (bad.into(), b"x".to_vec())]).unwrap();
            let e = unpack_into(&bytes, &temp.path().join("in")).unwrap_err();
            assert!(e.contains("outside"), "{bad}: {e}");
        }
        let none = rustgis_data::zip::write(&[("a.txt".into(), b"x".to_vec())]).unwrap();
        assert!(unpack_into(&none, &temp.path().join("in")).unwrap_err().contains("no project"));
        assert!(is_field_package("Site.RGISFIELD") && !is_field_package("site.rgis"));
        assert!(crate::project::opens_as_project("site.rgisfield"));
    }
}
