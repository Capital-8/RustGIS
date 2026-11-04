use super::*;
use crate::cmd::notebook::{Cell, CellKind, Notebook};
use crate::{
    GpRecord, Peer, durable,
    project::{Model, ModelGraph, ModelNode, ModelStep, Task, TaskStep},
};
use rustgis_map::presentation::{Page, PageKind, Presentation};

const SECRET: &str = "RGSYNTH_output_20261103";
const DATA: &str = "RGSYNTH_data_20261103";

/// Inline synthetic state exercises owning fields without contacting any service.
fn marked() -> Session {
    let mut s = world();
    let url = format!("https://h.invalid/a?token={SECRET}&layer=a%20b");
    s.project.maps[0].layers[0].dataset.source = Some(url.clone());
    s.project.tasks.push(Task { name: "Credentials".into(), description: DATA.into(), steps: vec![
        TaskStep { title: DATA.into(), instructions: DATA.into(), command: "layer.addUrl".into(), params: json!({"url": url}) },
        TaskStep { title: "Malformed wrapper".into(), instructions: String::new(), command: "layer.addUrl".into(), params: json!({"url": format!("rustgis-s3:broken:{SECRET}")}) },
        TaskStep { title: "Data".into(), instructions: String::new(), command: "layer.addDataText".into(), params: json!({"text": format!(r#"{{"type":"Feature","geometry":null,"properties":{{"token":"{DATA}","key":"{DATA}","password":"{DATA}","url":"https://h.invalid/?token={DATA}"}}}}"#)}) },
    ] });
    let params = json!({"input": "Cities", "token": SECRET, "values": {"token": DATA, "key": DATA, "password": DATA}});
    s.project.models.push(Model {
        name: "Credentials".into(),
        steps: vec![ModelStep { tool: "buffer".into(), params: params.clone() }],
        graph: Some(ModelGraph {
            nodes: vec![ModelNode { id: "tool1".into(), tool: Some("buffer".into()), params: params.clone(), ..Default::default() }],
            edges: Vec::new(),
        }),
    });
    s.project.gp_history.push(GpRecord {
        tool: "Buffer".into(),
        id: "buffer".into(),
        params,
        command: Some(("gp.bufferPoint".into(), json!({"token": SECRET}))),
        at: 0,
        ok: true,
        messages: vec![format!("source {url}")],
        millis: 0,
        outputs: vec!["Cities".into()],
    });
    s.project.notebooks.push(Notebook {
        name: "Credentials".into(),
        jupyter: None,
        cells: vec![Cell {
            kind: CellKind::Code,
            source: format!("# {DATA}\nlayer.addUrl {{\"url\":\"{url}\"}}\nlayer.addUrl {{\"url\":\"{url}\"\n"),
            output: Some(format!("source {url}")),
            error: true,
            jupyter: None,
        }],
    });
    s.project.presentations.push(Presentation {
        name: "Credentials".into(),
        pages: vec![Page {
            title: DATA.into(),
            kind: PageKind::Video { source: url, poster: None, caption: DATA.into() },
            notes: DATA.into(),
            seconds: 5.0,
        }],
        ..Default::default()
    });
    s
}

#[test]
fn durable_writers_clean_all_owned_roles_and_preserve_the_live_project() {
    let mut s = marked();
    let before = serde_json::to_value(&s.project).unwrap();
    let (text, report) = s.project.saved_text(None).unwrap();
    assert!(!text.contains(SECRET));
    assert!(report.layer_addresses() > 0 && report.steps > 0 && report.lines > 0 && report.videos > 0 && report.withheld > 0);
    assert!(text.contains(DATA));
    assert_eq!(before, serde_json::to_value(&s.project).unwrap());
    let opened = Project::from_json(&text).unwrap();
    assert_eq!(opened.tasks[0].steps, s.project.tasks[0].steps);
    assert_eq!(opened.models[0], s.project.models[0]);
    assert_eq!(opened.notebooks[0].cells[0].source, s.project.notebooks[0].cells[0].source);
    assert!(!opened.notebooks[0].cells[0].output.as_ref().unwrap().contains(SECRET), "generated text is never restored");
    assert!(!s.project.clone().recovery_text().unwrap().contains(SECRET));
    for kind in [
        "map",
        "layer",
        "template",
        "projectPackage",
        "mapPackage",
        "mobilePackage",
        "gpPackage",
        "task",
        "notebook",
        "jupyter",
        "presentation",
        "webTool",
    ] {
        let (_, bytes) = crate::cmd::items::item_bytes(&mut s, kind, &Value::Null).unwrap();
        let text = if bytes.starts_with(b"PK") {
            rustgis_data::zip::read_matching(&bytes, |_| true)
                .unwrap()
                .into_iter()
                .map(|(_, b)| String::from_utf8_lossy(&b).into_owned())
                .collect::<String>()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        assert!(!text.contains(SECRET), "{kind}");
        assert_eq!(before, serde_json::to_value(&s.project).unwrap(), "{kind} mutated live fields");
    }
    let code = s.run("gp.historyCode", &json!({"run": 0, "as": "command"})).unwrap();
    assert!(!code.to_string().contains(SECRET));
    s.project.gp_history[0].command = None;
    let code = s.run("gp.historyScript", &Value::Null).unwrap();
    assert!(!code.to_string().contains(SECRET));
    assert!(code.to_string().contains(DATA), "replayable data properties must remain");
}

#[test]
fn durable_clean_and_opaque_content_keep_their_meaning() {
    let mut s = marked();
    let original = s.project.tasks[0].steps[2].params.clone();
    let (clean, report) = crate::roles::command("layer.addDataText", &original, 0);
    assert!(clean.is_none() && report.notices().is_empty());
    let source = format!("print('https://h.invalid/?token={SECRET}')");
    let notebook = Notebook {
        name: "Python".into(),
        jupyter: Some(std::sync::Arc::new(json!({"metadata":{"kernelspec":{"language":"python"}}}))),
        cells: vec![Cell { kind: CellKind::Code, source: source.clone(), output: None, error: false, jupyter: None }],
    };
    let (text, report) = durable::output_owned(notebook, |n| serde_json::to_string(n).map_err(|e| e.to_string())).unwrap();
    assert!(text.contains(SECRET) && report.notices().is_empty());
    let params = json!({"nested": {"token": SECRET}});
    let (clean, report) = crate::roles::command("plugin.unknown", &params, 0);
    assert!(clean.is_none() && report.unchecked == 1);
    let clean = Project::default();
    let expected = serde_json::to_string(&clean).unwrap();
    let (text, report) = durable::output_owned(clean, |p| serde_json::to_string(p).map_err(|e| e.to_string())).unwrap();
    assert_eq!(text, expected);
    assert!(report.notices().is_empty());
    s.project.tasks[0].steps[0].params = json!({"url": "rustgis-s3:broken:RGSYNTH_output_20261103"});
    let (text, report) = s.project.saved_text(None).unwrap();
    assert!(!text.contains(SECRET) && report.withheld > 0);
}

#[test]
fn durable_formatter_errors_unwinds_and_apply_failures_restore_whole_fields() {
    use durable::{Durable, FieldMut, Leaf, Owner, Prepare};
    let mut s = marked();
    s.project.maps[0].layers[0].dataset.source = Some("/tmp/rustgis-output/data.geojson".into());
    let mut document = Project::default();
    document.tasks = s.project.tasks.clone();
    let text = serde_json::to_string_pretty(&document).unwrap();
    let mut params = json!({"text": text});
    let mut id = "project.openText";
    for _ in 0..6 {
        params = json!({"steps": [{"title":"nested", "command":id, "params":params}]});
        id = "task.new";
    }
    s.project.tasks[0].steps[0].command = id.into();
    s.project.tasks[0].steps[0].params = params;
    // A separate document is actually parsed and rewritten before the deeper envelope withholds.
    s.project.tasks[0].steps.push(TaskStep {
        title: "document".into(),
        instructions: String::new(),
        command: "project.openText".into(),
        params: json!({"text":text}),
    });
    let before = serde_json::to_value(&s.project).unwrap();
    let result = durable::output(&mut s.project, Some("/tmp/rustgis-output"), |p| {
        assert_eq!(p.maps[0].layers[0].dataset.source.as_deref(), Some("./data.geojson"));
        assert!(!serde_json::to_string(p).unwrap().contains(SECRET));
        Err::<(), _>("formatter failed".into())
    });
    assert!(result.is_err());
    assert_eq!(before, serde_json::to_value(&s.project).unwrap());
    #[cfg(not(target_arch = "wasm32"))]
    {
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = durable::output(&mut s.project, Some("/tmp/rustgis-output"), |_| -> Result<(), String> {
                std::panic::resume_unwind(Box::new("formatter failed"))
            });
        }));
        assert!(unwound.is_err());
        assert_eq!(before, serde_json::to_value(&s.project).unwrap());
    }
    struct Bad(Project);
    impl Durable for Bad {
        /// Offers classified whole fields once, before any live mutation.
        fn prepare(&self, p: &mut Prepare<'_>) {
            self.0.prepare(p);
            p.string(Leaf::Video(usize::MAX), "https://h.invalid/?token=RGSYNTH_output_20261103", true, false);
        }

        /// Resolves a prepared location through owned fields, without allocation or policy.
        fn field_mut(&mut self, owners: &[Owner], leaf: &Leaf) -> Option<FieldMut<'_>> {
            self.0.field_mut(owners, leaf)
        }
    }
    let mut bad = Bad(s.project);
    assert!(durable::output(&mut bad, Some("/tmp/rustgis-output"), |_| Ok(())).is_err());
    assert_eq!(before, serde_json::to_value(&bad.0).unwrap());
}

#[test]
fn model_and_presentation_non_item_writers_use_public_forms() {
    let mut s = marked();
    let graph = s.project.models[0].graph.clone().unwrap();
    let out = s.run("model.export", &json!({"model":"Credentials"})).unwrap();
    assert!(!out["text"].as_str().unwrap().contains(SECRET));
    let file = std::env::temp_dir().join(format!("rustgis-output-model-{}.json", std::process::id()));
    s.run("model.export", &json!({"model":"Credentials", "path":file})).unwrap();
    assert!(!std::fs::read_to_string(&file).unwrap().contains(SECRET));
    std::fs::remove_file(file).unwrap();
    assert_eq!(s.project.models[0].graph.as_ref(), Some(&graph));
    let userinfo = format!("https://u:{SECRET}@h.invalid");
    assert_eq!(crate::cmd::presentation::video_label(&userinfo), "h.invalid");
    assert_eq!(crate::cmd::presentation::video_label("/tmp/movie?cut#1.mp4"), "movie?cut#1.mp4");
    s.run("presentation.videoPage", &json!({"source":userinfo})).unwrap();
    assert_eq!(s.project.presentations[0].pages.last().unwrap().title, "h.invalid");
    let (bytes, _) = crate::cmd::presentation::export_bytes(&mut s, 0, "html", 48.0).unwrap();
    let html = String::from_utf8(bytes).unwrap();
    assert!(!html.contains(SECRET));
    assert!(html.contains(DATA));
    assert!(s.notices.iter().any(|n| n.text.contains("video address(es)")));
}

#[test]
fn generated_notebook_errors_are_cleaned_before_the_output_is_retained() {
    let mut s = marked();
    let source = format!("layer.addUrl {{\"url\":\"https://h.invalid/?token={SECRET}\",\"cellSize\":1}}");
    s.project.notebooks[0].cells[0].source = source.clone();
    s.trusted_notebooks.insert("credentials".into(), std::sync::Arc::new(s.item_content("notebook", "credentials").unwrap()));
    let _ = s.run("notebook.run", &json!({"notebook":0}));
    assert_eq!(s.project.notebooks[0].cells[0].source, source);
    assert!(!s.project.notebooks[0].cells[0].output.as_ref().unwrap().contains(SECRET));
    for kind in ["notebook", "jupyter"] {
        assert!(!String::from_utf8_lossy(&crate::cmd::items::item_bytes(&mut s, kind, &Value::Null).unwrap().1).contains(SECRET));
    }
    // Failed file tools retain their failure history through the command rollback. The missing
    // input is a layer name, so this reaches the production error path without any network I/O.
    let history = s.project.gp_history.len();
    assert!(
        s.run(
            "gp.run",
            &json!({"tool":"vectorToPmtiles", "params":{"input":format!("https://h.invalid/?token={SECRET}"), "minZoom":0, "maxZoom":0}})
        )
        .is_err()
    );
    assert_eq!(s.project.gp_history.len(), history + 1);
    let record = s.project.gp_history.last().unwrap();
    assert_eq!(record.id, "vectorToPmtiles");
    assert!(!record.ok && !record.messages.join("\n").contains(SECRET));
}

#[test]
fn peers_bypass_saved_restore_and_retention_requires_the_current_exact_slot() {
    let mut s = marked();
    let saved = s.project.saved_text(None).unwrap().0;
    s.trusted_tasks.insert("credentials".into(), std::sync::Arc::new(s.item_content("task", "credentials").unwrap()));
    let generation = s.opened;
    s.open_peer_text(Peer::Embedder, &saved).unwrap();
    assert!(s.opened != generation && s.trusted_tasks.is_empty());
    assert!(!serde_json::to_string(&s.project).unwrap().contains(SECRET));
    for scenario in ["unchanged", "rotation", "removed", "inserted", "moved", "accounts", "generation", "duplicate_table"] {
        let mut s = marked();
        s.project.tasks.clear();
        s.project.models.clear();
        s.project.gp_history.clear();
        s.project.notebooks.clear();
        s.project.presentations.clear();
        if scenario == "accounts" {
            let mut other = s.project.maps[0].layers[0].clone();
            other.id = 9000;
            other.dataset.source = Some("https://h.invalid/a?token=RGSYNTH_other_20261103&layer=a%20b".to_string());
            s.project.maps[0].layers.push(other);
        }
        if scenario == "duplicate_table" {
            let mut table = rustgis_data::Dataset::new("Table", rustgis_data::GeometryType::None, vec![]);
            table.source = Some(format!("https://h.invalid/table?token={SECRET}"));
            s.project.add_table(table);
        }
        let (public, _) = s.peer_text(Peer::Bridge).unwrap();
        assert!(!public.contains(SECRET));
        let mut incoming: Value = serde_json::from_str(&public).unwrap();
        match scenario {
            "rotation" => {
                s.project.maps[0].layers[0].dataset.source = Some("https://h.invalid/a?token=RGSYNTH_rotated_20261103&layer=a%20b".into())
            }
            "removed" => {
                s.project.maps[0].layers.remove(0);
            }
            "inserted" => incoming["maps"][0]["layers"][0]["id"] = json!(9001),
            "moved" => {
                incoming["maps"][0]["name"] = json!("Another map");
            }
            "generation" => {
                s.run("project.new", &Value::Null).unwrap();
            }
            "duplicate_table" => {
                let table = incoming["tables"][0].clone();
                incoming["tables"].as_array_mut().unwrap().push(table);
            }
            _ => {}
        }
        s.open_peer_text(Peer::Bridge, &incoming.to_string()).unwrap();
        let full = serde_json::to_string(&s.project).unwrap();
        if scenario == "unchanged" || scenario == "duplicate_table" {
            assert!(full.contains(SECRET), "{scenario}: ordinary layer stays");
        } else {
            assert!(!full.contains(SECRET), "{scenario}");
        }
        if scenario == "duplicate_table" {
            assert!(s.project.tables.iter().all(|t| !t.source.as_deref().unwrap_or("").contains(SECRET)));
        }
        assert!(s.trusted_tasks.is_empty() && s.trusted_models.is_empty() && s.trusted_notebooks.is_empty());
    }
}
