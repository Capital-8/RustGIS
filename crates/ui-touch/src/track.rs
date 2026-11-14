//! Field positions: the average of the device's positions for one place, and a track of where it
//! went. Both take each position the shell reported ([`RustGisApp::fixes`]), not only the last,
//! so none is lost between frames or while the app was out of sight.
//!
//! An average is taken in the Capture sheet: the positions that come while it runs are averaged,
//! and **Use Average** puts the capture cross there. A track is recorded from the More sheet: its
//! positions are drawn over the map as they come, and stopping it adds them as one line to the
//! map's **Tracks** layer (made when the map has none), with when it started and how many
//! positions it holds.

use rustgis_ui_egui::rustgis_engine::platform::Fix;
use rustgis_ui_egui::{RustGisApp, i18n, tl};
use serde_json::{Value, json};
use std::sync::Arc;

/// A position less exact than this, in metres, is left out of a track (and the track says how
/// many were): one off by more would draw a spike.
pub(crate) const TRACK_ACCURACY: f64 = 50.0;
/// The name of the layer a track is added to.
pub(crate) const TRACKS: &str = "Tracks";

/// A track being recorded.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Track {
    /// The positions kept, oldest first.
    /// Shared, so reading the field positions each frame doesn't copy them: a track runs for
    /// hours, a position a second.
    pub points: Arc<Vec<Fix>>,
    /// The positions left out, less exact than [`TRACK_ACCURACY`].
    pub poor: usize,
    /// When it was started, in milliseconds since 1970, when the clock could say.
    pub started: Option<i64>,
}

impl Track {
    /// A track started now.
    pub fn new() -> Track {
        Track { started: now_ms(), ..Track::default() }
    }

    /// Keeps a position, unless it is less exact than [`TRACK_ACCURACY`] (counted instead).
    pub fn push(&mut self, fix: Fix) {
        if fix.accuracy.is_some_and(|m| m > TRACK_ACCURACY) {
            self.poor += 1;
        } else {
            Arc::make_mut(&mut self.points).push(fix);
        }
    }

    /// How long it ran, in whole minutes, from its first position's time to its last.
    pub fn minutes(&self) -> Option<i64> {
        let first = self.points.first()?.time?;
        let last = self.points.last()?.time?;
        Some(last.saturating_sub(first).max(0) / 60_000)
    }
}

/// What the field positions hold between frames: an average being taken, and a track being
/// recorded. Kept in egui's memory, as the interface keeps its other state.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Field {
    /// The positions averaged so far, while an average is being taken.
    pub averaging: Option<Arc<Vec<Fix>>>,
    pub track: Option<Track>,
}

pub(crate) fn field(ctx: &egui::Context) -> Field {
    ctx.data(|d| d.get_temp(egui::Id::unique("touch_field"))).unwrap_or_default()
}

pub(crate) fn set_field(ctx: &egui::Context, field: Field) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::unique("touch_field"), field));
}

/// Takes the positions the shell reported since the last frame into the average and the track
/// being taken, if any. Taken each frame either way, so positions don't wait for one to start.
pub(crate) fn take(app: &mut RustGisApp, ctx: &egui::Context) {
    if app.fixes.is_empty() {
        return;
    }
    // Taken out of egui's memory while positions go in, so the points' `Arc` has one owner and
    // `make_mut` adds to it in place instead of copying the track.
    let mut state: Field = ctx.data_mut(|d| d.remove_temp(egui::Id::unique("touch_field"))).unwrap_or_default();
    for fix in app.fixes.drain(..) {
        if let Some(fixes) = &mut state.averaging {
            Arc::make_mut(fixes).push(fix);
        }
        if let Some(track) = &mut state.track {
            track.push(fix);
        }
    }
    set_field(ctx, state);
}

/// The average of some positions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Average {
    pub lon: f64,
    pub lat: f64,
    /// How far the positions lie from it, in metres (their root mean square distance).
    pub spread: f64,
    /// The mean of the accuracies they gave, in metres, when every one gave one.
    pub accuracy: Option<f64>,
    pub count: usize,
}

/// The average of `fixes`: each weighted by how exact it says it is (one over its accuracy
/// squared, the accuracy taken as at least a centimetre and at most a thousand kilometres, so
/// no weight is infinite or nothing) when every one says, else equally. Longitudes are taken
/// about the first one's, so positions on both sides of the antimeridian average to a place
/// between them. `None` for none.
pub(crate) fn average(fixes: &[Fix]) -> Option<Average> {
    let first = fixes.first()?;
    let weighed = fixes.iter().all(|f| f.accuracy.is_some());
    let weight = |f: &Fix| if weighed { f.accuracy.map_or(1.0, |m| 1.0 / m.clamp(0.01, 1e6).powi(2)) } else { 1.0 };
    let near = |lon: f64| first.lon + (lon - first.lon + 540.0).rem_euclid(360.0) - 180.0;
    let total: f64 = fixes.iter().map(weight).sum();
    if !(total.is_finite() && total > 0.0) {
        return None;
    }
    let lon = fixes.iter().map(|f| weight(f) * near(f.lon)).sum::<f64>() / total;
    let lat = fixes.iter().map(|f| weight(f) * f.lat).sum::<f64>() / total;
    let metres = |f: &Fix| {
        let dx = (near(f.lon) - lon) * lat.to_radians().cos() * 111_320.0;
        let dy = (f.lat - lat) * 110_540.0;
        dx * dx + dy * dy
    };
    let spread = (fixes.iter().map(metres).sum::<f64>() / fixes.len() as f64).sqrt();
    let accuracy = fixes.iter().map(|f| f.accuracy).sum::<Option<f64>>().map(|sum| sum / fixes.len() as f64);
    Some(Average { lon: (lon + 540.0).rem_euclid(360.0) - 180.0, lat, spread, accuracy, count: fixes.len() })
}

/// What the Capture sheet says of an average being taken.
pub(crate) fn averaging_text(fixes: &[Fix]) -> String {
    match average(fixes) {
        None => tl!("Averaging: waiting for the first position…").to_string(),
        Some(a) => i18n::fmt(
            "Averaging {n} positions: about {spread} m from their average (root mean square).",
            &[("n", &a.count.to_string()), ("spread", &format!("{:.1}", a.spread))],
        ),
    }
}

/// The line the track bar shows while a track is being recorded.
pub(crate) fn track_text(track: &Track, locating: bool) -> String {
    if !locating {
        return tl!("Track paused: this device's position is off.").to_string();
    }
    let n = track.points.len().to_string();
    match track.minutes() {
        Some(m) => i18n::fmt("Recording a track: {n} positions, {m} min", &[("n", &n), ("m", &m.to_string())]),
        None => i18n::fmt("Recording a track: {n} positions", &[("n", &n)]),
    }
}

/// Adds a finished track to the map's Tracks layer, made when the map has none, and says what was
/// added and what was left out. A track of fewer than two positions adds nothing, and says so.
/// Says whether a line was added.
pub(crate) fn finish(app: &mut RustGisApp, track: Track) -> bool {
    let left_out = |app: &mut RustGisApp| {
        if track.poor > 0 {
            let text = i18n::fmt(
                "{n} positions were left out of the track: each was less exact than {m} m.",
                &[("n", &track.poor.to_string()), ("m", &TRACK_ACCURACY.to_string())],
            );
            app.session.notify(text, false);
        }
    };
    if track.points.len() < 2 {
        app.session.notify(tl!("The track wasn't added: it has fewer than two positions."), true);
        left_out(app);
        return false;
    }
    // Editing first: a Tracks layer isn't made for a track that then can't be added. Every way
    // out still says what was left out (the refusal itself the front end says).
    if !super::edit::ensure_editing(app) {
        left_out(app);
        return false;
    }
    let Some(layer) = tracks_layer(app) else {
        left_out(app);
        return false;
    };
    let started = track.started.or(track.points.first().and_then(|f| f.time)).and_then(stamp).unwrap_or_default();
    let name = if started.is_empty() { tl!("Track").to_string() } else { i18n::fmt("Track {start}", &[("start", &started)]) };
    let notes = match track.minutes() {
        Some(m) => i18n::fmt("{n} positions over {m} min", &[("n", &track.points.len().to_string()), ("m", &m.to_string())]),
        None => i18n::fmt("{n} positions", &[("n", &track.points.len().to_string())]),
    };
    let line: Vec<Value> = track.points.iter().map(|f| json!([f.lon, f.lat])).collect();
    let feature = json!({
        "layer": layer,
        "geometry": {"type": "LineString", "coordinates": line},
        "attributes": {"name": name, "notes": notes},
    });
    if app.run_by_user("edit.addFeature", feature).is_err() {
        left_out(app);
        return false;
    }
    let named = app.session.map().layer(layer).map_or_else(|| TRACKS.to_string(), |l| l.name.clone());
    app.session.notify(i18n::fmt("Added the track to {layer}: {notes}.", &[("layer", &named), ("notes", &notes)]), false);
    left_out(app);
    true
}

/// The map's Tracks layer: its line layer of that name with a text `name` and `notes` field, or
/// one made for it with them. A Tracks layer without them is passed over: the track's name and
/// notes would be dropped from it.
fn tracks_layer(app: &mut RustGisApp) -> Option<u32> {
    use rustgis_ui_egui::rustgis_engine::data::{FieldType, GeometryType};
    let has = |l: &rustgis_ui_egui::rustgis_engine::map::Layer, name: &str| {
        l.dataset.fields.iter().any(|f| f.name.eq_ignore_ascii_case(name) && f.ty == FieldType::Text)
    };
    let found = app
        .session
        .map()
        .layers
        .iter()
        .find(|l| tracks_name(&l.name) && l.geometry_type() == GeometryType::Polyline && has(l, "name") && has(l, "notes"))
        .map(|l| l.id);
    if found.is_some() {
        return found;
    }
    let made = app.run_by_user("layer.template", json!({"template": "lines", "name": TRACKS})).ok()?;
    made.get("layer").and_then(Value::as_u64).and_then(|id| u32::try_from(id).ok())
}

/// Whether a layer's name is a Tracks layer's: `Tracks`, or `Tracks (2)`… as one made beside
/// another is named.
fn tracks_name(name: &str) -> bool {
    let numbered = name.strip_prefix(TRACKS).and_then(|n| n.strip_prefix(" (")).and_then(|n| n.strip_suffix(')'));
    name == TRACKS || numbered.is_some_and(|n| n.parse::<u32>().is_ok())
}

/// Milliseconds since 1970 as a date and time in UTC, to the minute (`2026-11-14 10:05Z`).
fn stamp(ms: i64) -> Option<String> {
    use rustgis_ui_egui::rustgis_engine::data::DateTime;
    let text = DateTime::with_offset(ms.checked_div(60_000)?.checked_mul(60_000_000)?, 0)?.to_string();
    // `2026-11-14T10:05:00Z`: the seconds are always nought here.
    Some(format!("{} {}Z", text.get(..10)?, text.get(11..16)?))
}

/// Now, in milliseconds since 1970.
fn now_ms() -> Option<i64> {
    let since = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(lon: f64, lat: f64, accuracy: Option<f64>, time: Option<i64>) -> Fix {
        Fix { lon, lat, accuracy, time }
    }

    #[test]
    fn an_average_weighs_each_position_by_how_exact_it_says_it_is() {
        assert_eq!(average(&[]), None);
        // Equal accuracies: the middle.
        let a = average(&[fix(10.0, 20.0, Some(5.0), None), fix(10.0002, 20.0, Some(5.0), None)]).unwrap();
        assert!((a.lon - 10.0001).abs() < 1e-9 && (a.lat - 20.0).abs() < 1e-9, "{a:?}");
        assert_eq!(a.count, 2);
        assert_eq!(a.accuracy, Some(5.0));
        // About 21 m apart: each about 10.5 m from the middle.
        assert!((a.spread - 10.46).abs() < 0.2, "{a:?}");
        // One three times as exact counts nine times as much.
        let b = average(&[fix(10.0, 20.0, Some(1.0), None), fix(10.001, 20.0, Some(3.0), None)]).unwrap();
        assert!((b.lon - 10.0001).abs() < 1e-9, "{b:?}");
        // One without an accuracy: all count the same, and no accuracy is given.
        let c = average(&[fix(10.0, 20.0, Some(1.0), None), fix(10.001, 20.0, None, None)]).unwrap();
        assert!((c.lon - 10.0005).abs() < 1e-9 && c.accuracy.is_none(), "{c:?}");
        // Accuracies out of all measure, or of nothing, still weigh: none is infinite or nothing.
        let e = average(&[fix(10.0, 20.0, Some(1e300), None), fix(10.001, 20.0, Some(1e300), None)]).unwrap();
        assert!((e.lon - 10.0005).abs() < 1e-9, "{e:?}");
        let z = average(&[fix(10.0, 20.0, Some(0.0), None), fix(10.001, 20.0, Some(5.0), None)]).unwrap();
        assert!((z.lon - 10.0).abs() < 1e-6, "{z:?}");
        // Either side of the antimeridian: a place between them, not the other side of the Earth.
        let d = average(&[fix(179.9999, 0.0, None, None), fix(-179.9999, 0.0, None, None)]).unwrap();
        assert!(d.lon.abs() > 179.99, "{d:?}");
    }

    #[test]
    fn a_track_leaves_out_poor_positions_and_counts_them() {
        let mut track = Track::default();
        track.push(fix(1.0, 1.0, Some(3.0), Some(0)));
        track.push(fix(1.1, 1.0, Some(500.0), Some(60_000)));
        track.push(fix(1.2, 1.0, None, Some(600_000)));
        assert_eq!((track.points.len(), track.poor), (2, 1));
        assert_eq!(track.minutes(), Some(10));
        // Times out of all measure don't overflow.
        let wide = Track { points: Arc::new(vec![fix(0.0, 0.0, None, Some(i64::MAX)), fix(1.0, 0.0, None, Some(0))]), ..Track::default() };
        assert_eq!(wide.minutes(), Some(0));
        let wide = Track { points: Arc::new(vec![fix(0.0, 0.0, None, Some(0)), fix(1.0, 0.0, None, Some(i64::MAX))]), ..Track::default() };
        assert_eq!(wide.minutes(), Some(i64::MAX / 60_000));
        assert_eq!(stamp(1_794_614_460_123).as_deref(), Some("2026-11-14 00:01Z"));
    }

    #[test]
    fn a_finished_track_is_added_to_the_tracks_layer_and_what_was_left_out_is_said() {
        let mut app = RustGisApp::with_sample();
        let ctx = egui::Context::default();
        // Positions reported before a track starts aren't in it.
        app.report_position(fix(30.0, -2.0, Some(4.0), Some(0)));
        take(&mut app, &ctx);
        set_field(&ctx, Field { track: Some(Track::default()), averaging: Some(Arc::default()) });
        for (i, accuracy) in [4.0, 4.0, 80.0, 4.0].into_iter().enumerate() {
            app.report_position(fix(30.0 + i as f64 * 0.001, -2.0, Some(accuracy), Some(i as i64 * 60_000)));
        }
        take(&mut app, &ctx);
        assert!(app.fixes.is_empty());
        let state = field(&ctx);
        let track = state.track.clone().unwrap();
        assert_eq!((track.points.len(), track.poor), (3, 1));
        assert_eq!(state.averaging.as_ref().map(|a| a.len()), Some(4));
        // Added as one line to a new Tracks layer, and the position left out said.
        let mark = app.session.notice_mark();
        assert!(finish(&mut app, track.clone()));
        let tracks = app.session.map().layers.iter().find(|l| l.name == TRACKS).unwrap();
        assert_eq!(tracks.dataset.features.len(), 1);
        let said: Vec<String> = app.session.notices_since(mark).iter().map(|n| n.text.clone()).collect();
        assert!(said.iter().any(|t| t.contains("left out")), "{said:?}");
        // A second goes to the same layer.
        assert!(finish(&mut app, track.clone()));
        assert_eq!(app.session.map().layers.iter().filter(|l| l.name == TRACKS).count(), 1);
        // A Tracks layer without the fields is passed over: one with them is made.
        let tracks = app.session.map().layers.iter().find(|l| l.name == TRACKS).map(|l| l.id).unwrap();
        app.run_by_user("edit.deleteField", json!({"layer": tracks, "field": "notes"})).unwrap();
        assert!(finish(&mut app, track.clone()));
        assert_eq!(app.session.map().layers.iter().filter(|l| tracks_name(&l.name)).count(), 2);
        // And it is the one the next track goes to.
        assert!(finish(&mut app, track));
        assert_eq!(app.session.map().layers.iter().filter(|l| tracks_name(&l.name)).count(), 2);
        assert!(tracks_name("Tracks (2)") && !tracks_name("Tracks of deer") && !tracks_name("My Tracks"));
        // Too short: nothing, said as an error.
        let mark = app.session.notice_mark();
        assert!(!finish(&mut app, Track { points: Arc::new(vec![fix(1.0, 1.0, None, None)]), ..Track::default() }));
        assert!(app.session.notices_since(mark).iter().any(|n| n.error));
    }
}
