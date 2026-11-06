//! Project exchange with a peer is an import, never an explicit saved-file reopen. A framing
//! page receives no credentials and cannot borrow the process's identical-text restore memory.

use crate::{
    Project, Session,
    durable::{self, Report},
};
use durable::{At, Durable, FieldMut, Kept, Leaf, Owner};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SlotKey {
    Map { index: usize, name: String, at: rustgis_map::SlotAt },
    Area { index: usize, at: rustgis_map::SlotAt },
    Table(u32),
    Video { presentation: usize, page: usize },
}

struct Slot<'a> {
    key: SlotKey,
    at: At,
    value: &'a str,
}
struct Entry {
    key: SlotKey,
    public: String,
    full: String,
    ambiguous: bool,
}
pub(crate) struct Exchange {
    generation: u64,
    entries: Vec<Entry>,
}
const MAX_ENTRIES: usize = 4096;
const MAX_BYTES: usize = 1 << 20;

/// Enumerates slot identities before any normalization. Duplicate identities are retained in the
/// census: they must never be made unique by dropping or renumbering a hostile duplicate.
fn slots(p: &Project) -> Vec<Slot<'_>> {
    let mut out = Vec::new();
    for (i, m) in p.maps.iter().enumerate() {
        for slot in m.slots().into_iter().filter(|s| s.address) {
            if let Some(at) = m.slot_identity(&slot.at) {
                out.push(Slot {
                    key: SlotKey::Map { index: i, name: m.name.clone(), at },
                    at: At { owners: vec![Owner::Map(i)], leaf: Leaf::Map(slot.at) },
                    value: slot.value,
                });
            }
        }
    }
    for (i, area) in p.offline_areas.iter().enumerate() {
        for slot in area.data.slots().into_iter().filter(|s| s.address) {
            if let Some(at) = area.data.slot_identity(&slot.at) {
                out.push(Slot {
                    key: SlotKey::Area { index: i, at },
                    at: At { owners: vec![Owner::Area(i)], leaf: Leaf::Map(slot.at) },
                    value: slot.value,
                });
            }
        }
    }
    for (i, table) in p.tables.iter().enumerate() {
        if let Some(value) = &table.source {
            out.push(Slot { key: SlotKey::Table(table.id), at: At { owners: vec![Owner::Table(i)], leaf: Leaf::Table }, value });
        }
    }
    for (i, pres) in p.presentations.iter().enumerate() {
        for (page, p) in pres.pages.iter().enumerate() {
            if let rustgis_map::presentation::PageKind::Video { source, .. } = &p.kind {
                out.push(Slot {
                    key: SlotKey::Video { presentation: i, page },
                    at: At { owners: vec![Owner::Presentation(i)], leaf: Leaf::Video(page) },
                    value: source,
                });
            }
        }
    }
    out
}

/// A wrapper's binding belongs to S3 connection resolution, never exchange retention.
fn wrapped(value: &str) -> bool {
    let value = crate::sources::template_archive(value).map_or(value, |(_, v)| v);
    value.trim_start().get(..11).is_some_and(|v| v.eq_ignore_ascii_case("rustgis-s3:"))
}

/// Charges both forms and the complete identity payload, bounded before copying either form.
fn key_bytes(key: &SlotKey) -> usize {
    let extra = match key {
        SlotKey::Map { name, at, .. } => name.len().saturating_add(match at {
            rustgis_map::SlotAt::Tile { id, .. } => id.len(),
            _ => 0,
        }),
        SlotKey::Area { at: rustgis_map::SlotAt::Tile { id, .. }, .. } => id.len(),
        _ => 0,
    };
    std::mem::size_of::<SlotKey>().saturating_add(extra)
}

impl Exchange {
    /// Recorded only after output rollback. Every entry is session-local and replaced per push.
    fn record(p: &Project, generation: u64) -> (Self, u32) {
        let mut entries: Vec<Entry> = Vec::new();
        let mut forms: HashMap<String, (usize, bool)> = HashMap::new();
        let mut recording = true;
        let mut held = 0usize;
        let mut omitted = 0u32;
        for slot in slots(p) {
            if wrapped(slot.value) {
                continue;
            }
            let public = match durable::public_address(slot.value) {
                Kept::Unchanged => continue,
                Kept::Changed(s) => s,
                Kept::Withheld(prefix) => prefix.into(),
            };
            // Even an account omitted by the cap cannot make an earlier recorded public form
            // unambiguous. After the first overflow, no later slot is recorded.
            if let Some((first, ambiguous)) = forms.get_mut(&public) {
                *ambiguous |= entries.get(*first).is_none_or(|e| e.full != slot.value);
            }
            let bytes = slot.value.len().saturating_add(public.len()).saturating_add(key_bytes(&slot.key));
            if !recording || entries.len() >= MAX_ENTRIES || held.saturating_add(bytes) > MAX_BYTES {
                recording = false;
                omitted = omitted.saturating_add(1);
                continue;
            }
            held += bytes;
            forms.entry(public.clone()).or_insert((entries.len(), false));
            entries.push(Entry { key: slot.key, public, full: slot.value.into(), ambiguous: false });
        }
        for entry in &mut entries {
            entry.ambiguous = forms.get(&entry.public).is_none_or(|(_, ambiguous)| *ambiguous);
        }
        (Self { generation, entries }, omitted)
    }

    /// The pre-normalization census must contain exactly one byte-equal public slot.
    fn incoming(&self, p: &Project) -> HashSet<SlotKey> {
        let census = census(p);
        self.entries
            .iter()
            .filter(|e| census.get(&e.key).is_some_and(|(n, value)| *n == 1 && *value == e.public))
            .map(|e| e.key.clone())
            .collect()
    }

    /// Checks last exchange, both incoming censuses, unique account and the exact current live
    /// value in one synchronous replacement. Public URL or incoming id alone grants nothing.
    fn retain(&self, live: &Project, incoming: &mut Project, raw: &HashSet<SlotKey>) {
        let current = census(live);
        let normalized = census(incoming);
        let mut records: HashMap<&SlotKey, (usize, &Entry)> = HashMap::new();
        let mut accounts: HashMap<&str, Option<&str>> = HashMap::new();
        for e in &self.entries {
            records.entry(&e.key).or_insert((0, e)).0 += 1;
            let account = accounts.entry(&e.public).or_insert(Some(&e.full));
            if *account != Some(e.full.as_str()) {
                *account = None;
            }
        }
        let edits: Vec<_> = slots(incoming)
            .into_iter()
            .filter_map(|slot| {
                let (count, e) = records.get(&slot.key)?;
                let unique = |census: &HashMap<SlotKey, (usize, &str)>, expected: &str| {
                    census.get(&slot.key).is_some_and(|(n, value)| *n == 1 && *value == expected)
                };
                (!e.ambiguous
                    && raw.contains(&slot.key)
                    && *count == 1
                    && accounts.get(e.public.as_str()) == Some(&Some(e.full.as_str()))
                    && unique(&current, &e.full)
                    && unique(&normalized, &e.public))
                .then(|| (slot.at, e.full.clone()))
            })
            .collect();
        drop(normalized);
        for (at, full) in edits {
            if let Some(FieldMut::Text(slot)) = incoming.field_mut(&at.owners, &at.leaf) {
                *slot = full;
            }
            reopen(incoming, &at);
        }
    }
}

/// A retained address must reopen its own reader, which normalization previously opened with
/// the public form. Only the exact retained owner/role is touched; S3 wrappers are excluded.
fn reopen(p: &mut Project, at: &At) {
    let Some(owner) = at.owners.first() else { return };
    let map = match owner {
        Owner::Map(i) => p.maps.get_mut(*i),
        Owner::Area(i) => p.offline_areas.get_mut(*i).map(|a| &mut a.data),
        _ => None,
    };
    let Some(map) = map else { return };
    let Leaf::Map(rustgis_map::SlotPos::Layer { layer, role }) = &at.leaf else { return };
    let Some(layer) = map.layers.get_mut(*layer) else { return };
    match role {
        rustgis_map::LayerRole::Streamed
        | rustgis_map::LayerRole::PostgisDatabase
        | rustgis_map::LayerRole::MysqlDatabase
        | rustgis_map::LayerRole::SqlServerDatabase => {
            if let Some(source) = &mut layer.source
                && let Err(e) = source.reopen()
            {
                source.error = Some(rustgis_data::diagnostics::Redacted::new(&e).into_string());
            }
        }
        rustgis_map::LayerRole::Raster => {
            if let Some(raster) = &mut layer.raster
                && let Err(e) = raster.reload()
            {
                raster.error = Some(rustgis_data::diagnostics::Redacted::new(&e).into_string());
            }
        }
        _ => {}
    }
}

/// Values borrow the project, but identities include all duplicates; no normalization runs here.
fn census(p: &Project) -> HashMap<SlotKey, (usize, &str)> {
    let mut out = HashMap::new();
    for slot in slots(p) {
        let entry = out.entry(slot.key).or_insert((0usize, slot.value));
        entry.0 = entry.0.saturating_add(1);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Peer {
    Bridge,
    Embedder,
}

impl Session {
    /// Writes a credential-free project for a peer, without evicting explicit-save restoration.
    /// Pending downloads require a structural draft; settled projects use the live journal.
    pub fn peer_text(&mut self, peer: Peer) -> Result<(String, Report), String> {
        let (text, mut report) = if self.project.ensure_imports_complete().is_err() {
            let mut draft = self.project.clone();
            draft.omit_pending_imports();
            durable::output_owned(draft, text)
        } else {
            durable::output(&mut self.project, None, text)
        }?;
        self.exchange = if peer == Peer::Bridge {
            let (record, omitted) = Exchange::record(&self.project, self.opened);
            report.not_retained = omitted;
            Some(record)
        } else {
            None
        };
        Ok((text, report))
    }

    /// Replaces a project synchronously with peer/import semantics. No command runs with User
    /// authority. Reset advances generation, clears item trust, history, pending work and active
    /// connections; old approvals and continuations must bind to that generation.
    pub fn open_peer_text(&mut self, peer: Peer, text: &str) -> Result<Report, String> {
        self.with_scope(crate::policy::Scope::peer(), |s| s.open_peer_text_scoped(peer, text))?
    }
    /// The existing raw census/reset path runs synchronously under peer provenance.
    fn open_peer_text_scoped(&mut self, peer: Peer, text: &str) -> Result<Report, String> {
        let mut p = Project::from_peer_raw(text)?;
        let exchange = self.exchange.as_ref().filter(|e| peer == Peer::Bridge && e.generation == self.opened);
        let raw = exchange.map(|e| e.incoming(&p));
        crate::cmd::connections::clear_active();
        p.normalize();
        let mut report = Report::default();

        if let (Some(exchange), Some(raw)) = (exchange, raw) {
            exchange.retain(&self.project, &mut p, &raw);
        }
        let incoming = census(&p);
        report.lost = slots(&self.project)
            .into_iter()
            .filter(|slot| {
                !matches!(durable::public_address(slot.value), Kept::Unchanged)
                    && !incoming.get(&slot.key).is_some_and(|(count, value)| *count == 1 && *value == slot.value)
            })
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        drop(incoming);
        crate::cmd::project::reset_to(self, p);
        self.path = None;
        crate::cmd::connections::resolve_project(self);
        crate::cmd::connections::note_waiting(self);
        self.revision = self.revision.wrapping_add(1);
        if peer == Peer::Bridge {
            let (exchange, omitted) = Exchange::record(&self.project, self.opened);
            self.exchange = Some(exchange);
            report.not_retained = omitted;
        }
        Ok(report)
    }
}

/// The same opening bound applies to peers; no destination is written on a formatting failure.
fn text(p: &Project) -> Result<String, String> {
    let text = serde_json::to_string(p).map_err(|e| e.to_string())?;
    if text.len() as u64 > crate::project::MAX_BYTES {
        return Err("the project is too large to exchange".into());
    }
    Ok(text)
}
