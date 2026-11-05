//! Public dispatch, frozen consent and owned late work share the engine's permission outcome.
use super::*;
use crate::policy::{Call, Effects, Outcome, Pending, Revocation, Scope};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Live(AtomicBool);
impl Revocation for Live {
    fn allows(&self, _: Effects) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
fn assistant(id: u64) -> (Scope, Arc<Live>) {
    let live = Arc::new(Live(AtomicBool::new(true)));
    (Scope::assistant(id, live.clone()), live)
}
fn pending(s: &Session, scope: &Scope, call: &Call) -> Pending {
    match s.review(scope, call) {
        Outcome::NeedsApproval(p) => p,
        Outcome::Refused(e) => panic!("refused instead of a review: {e}"),
        Outcome::Allowed => panic!("the external operation needs approval"),
    }
}

/// Equal display redactions do not confer consent for different raw addresses or defaults.
#[test]
fn permission_policy_consent_binds_raw_params_and_default_targets() {
    let mut s = world();
    #[cfg(not(target_arch = "wasm32"))]
    {
        s.leave_downloads = true;
    }
    let (scope, _) = assistant(1);
    let a = Call::command("layer.addUrl", json!({"url":"https://example.invalid/data.geojson?token=raw-one"})).with_request("r");
    let b = Call::command("layer.addUrl", json!({"url":"https://example.invalid/data.geojson?token=raw-two"})).with_request("r");
    let p = pending(&s, &scope, &a);
    assert!(!p.review().what.contains("raw-one"));
    assert!(s.run_permitted(scope.clone(), p.allow(), &b).unwrap_err().contains("changed"));
    assert!(s.take_url_requests().is_empty());
    let p = pending(&s, &scope, &a);
    s.active_layer = s.map().layer_by_name("Cities").map(|l| l.id);
    assert!(s.run_permitted(scope.clone(), p.allow(), &a).unwrap_err().contains("changed"));
    let p = pending(&s, &scope, &a);
    s.run_permitted(scope, p.allow(), &a).unwrap();
    assert_eq!(s.take_url_requests().len(), 1);
    let (scope, _) = assistant(127);
    let nested = Call::command(
        "layer.addUrl",
        json!({"url":"https://example.invalid/clean.csv", "token":"RGSYNTH_direct",
        "note":format!("message {}", json!({"script":format!("layer.addUrl {}", json!({"password":"RGSYNTH_nested","url":"https://example.invalid/x?token=RGSYNTH_url"}))}))}),
    );
    let shown = pending(&s, &scope, &nested);
    assert!(shown.review().what.contains("clean.csv"));
    assert!(!shown.review().what.contains("RGSYNTH_"));
    let long = Call::command("layer.addUrl", json!({"url":"https://example.invalid/clean.csv","note":"safe".repeat(1500)}));
    assert!(matches!(s.review(&scope, &long), Outcome::Refused(e) if e.contains("too long")));

    // A single-layer owner falls back to the active layer for a non-scalar selector.
    // Review diagram and manual-label operations against their actual effect in an asking script.
    let mut s = world();
    for name in ["Diagram A", "Diagram B"] {
        s.run(
            "layer.addDataText",
            &json!({"name":name,"fileName":"diagram.geojson","text":r#"{"type":"Feature","properties":{"n":1},"geometry":{"type":"Point","coordinates":[0,0]}}"#}),
        )
        .unwrap();
    }
    let (scope, _) = assistant(130);
    for selector in [json!({"layer":["Diagram A"]}), json!({"layer":"Diagram A"}), json!({})] {
        for name in ["Diagram A", "Diagram B"] {
            s.run("layer.diagram", &json!({"layer":name,"kind":"pie","fields":["n"],"size":8})).unwrap();
            let feature = s.map().layer_by_name(name).unwrap().dataset.features.first().unwrap().id;
            s.run("labels.move", &json!({"layer":name,"feature":feature,"to":[1,2]})).unwrap();
        }
        s.run("layer.select", &json!({"layer":"Diagram B"})).unwrap();
        let expected = if selector["layer"].is_string() { "Diagram A" } else { "Diagram B" };
        let other = if expected == "Diagram A" { "Diagram B" } else { "Diagram A" };
        let mut set = selector.clone();
        set["kind"] = json!("bar");
        set["fields"] = json!(["n"]);
        set["size"] = json!(12);
        let mut moved = selector.clone();
        moved["feature"] = json!(s.map().layer_by_name(expected).unwrap().dataset.features.first().unwrap().id);
        moved["to"] = json!([3, 4]);
        let call = Call::script(&format!(
            "layer.diagram {set}\nlayer.diagramInfo {selector}\nlayer.clearDiagram {selector}\nlabels.move {moved}\nlabels.resetMoved {selector}\nmap.addTiles {{\"url\":\"https://diagram.invalid/{{z}}/{{x}}/{{y}}.png\"}}"
        ));
        let p = pending(&s, &scope, &call);
        let details = p.review().details.clone();
        let defaults = p.review().defaults.clone();
        s.run_permitted(scope.clone(), p.allow(), &call).unwrap();
        assert!(s.map().layer_by_name(expected).unwrap().diagram.is_none(), "the executor clears its actual target");
        assert!(s.map().layer_by_name(other).unwrap().diagram.is_some(), "the other layer retains its diagram");
        assert!(s.map().layer_by_name(expected).unwrap().labels.default.moved.is_empty(), "the label owner clears its actual target");
        assert_eq!(s.map().layer_by_name(other).unwrap().labels.default.moved.len(), 1, "the other layer keeps its saved label move");
        let commands = ["layer.diagram", "layer.diagramInfo", "layer.clearDiagram", "labels.move", "labels.resetMoved"];
        if selector["layer"].is_string() {
            for id in commands {
                let detail = details.iter().find(|d| d.operation == id).unwrap();
                assert_eq!(
                    detail.targets,
                    [format!("{} / {expected}", s.map().name)],
                    "single-layer review target differs from execution: {id}"
                );
            }
        } else {
            // Defaults are named once in Starting selection, not repeated as explicit targets.
            assert_eq!(defaults, Some((Some(expected.to_string()), vec![expected.to_string()])));
            for detail in details.iter().filter(|d| commands.contains(&d.operation.as_str())) {
                assert_eq!(detail.targets, [format!("{} / {expected}", s.map().name)], "no contradictory target may appear for a default");
            }
        }
    }
    // A scalar name created or renamed by an earlier step remains a bound in-run target.
    for (first, params, name, named) in [
        (
            "layer.addDataText",
            json!({"name":"Fresh created","fileName":"fresh.geojson","text":r#"{"type":"Feature","properties":{"n":1},"geometry":{"type":"Point","coordinates":[0,0]}}"#}),
            "Fresh created",
            true,
        ),
        ("layer.rename", json!({"layer":"Diagram A","name":"Fresh renamed"}), "Fresh renamed", true),
        (
            "layer.addDataText",
            json!({"name":"Fresh default","fileName":"fresh.geojson","text":r#"{"type":"Feature","properties":{"n":1},"geometry":{"type":"Point","coordinates":[0,0]}}"#}),
            "Fresh default",
            false,
        ),
    ] {
        let selector = if named { json!({"layer":name}) } else { json!({}) };
        let mut diagram = selector.clone();
        diagram["kind"] = json!("pie");
        diagram["fields"] = json!(["n"]);
        diagram["size"] = json!(8);
        let mut moved = selector.clone();
        moved["feature"] = json!(0);
        moved["to"] = Value::Null;
        let call = Call::script(&format!(
            "{first} {params}\nlayer.diagram {diagram}\nlayer.diagramInfo {selector}\nlayer.clearDiagram {selector}\nlabels.move {moved}\nlabels.resetMoved {selector}\nmap.addTiles {{\"url\":\"https://diagram.invalid/{{z}}/{{x}}/{{y}}.png\"}}"
        ));
        let p = pending(&s, &scope, &call);
        s.run_permitted(scope.clone(), p.allow(), &call).unwrap();
        assert!(s.map().layer_by_name(name).unwrap().diagram.is_none(), "the in-run target is created, styled and cleared");
    }
    let unknown =
        Call::script("layer.diagramInfo {\"layer\":\"Never made\"}\nmap.addTiles {\"url\":\"https://diagram.invalid/{z}/{x}/{y}.png\"}");
    assert!(matches!(s.review(&scope, &unknown), Outcome::Refused(why) if why.contains("no earlier output")));
}

/// A displayed task is frozen by content, including its project-only edits; undo restores trust.
#[test]
fn permission_policy_task_content_and_trust_follow_undo() {
    let mut s = world();
    #[cfg(not(target_arch = "wasm32"))]
    {
        s.leave_downloads = true;
    }
    s.run("task.new", &json!({"name":"Reviewed", "steps":[{"title":"data", "command":"layer.addUrl", "params":{"url":"https://example.invalid/data.geojson"}}]})).unwrap();
    assert!(s.item_trusted("task", "Reviewed"));
    let (scope, _) = assistant(2);
    let call = Call::command("task.run", json!({"task":"Reviewed"}));
    let p = pending(&s, &scope, &call);
    s.run_scoped(scope.clone(), &Call::command("task.editStep", json!({"task":"Reviewed", "step":0, "title":"agent changed"}))).unwrap();
    assert!(!s.item_trusted("task", "Reviewed"));
    assert!(s.run_permitted(scope.clone(), p.allow(), &call).is_err());
    s.run("edit.undo", &Value::Null).unwrap();
    assert!(s.item_trusted("task", "Reviewed"));
    let p = pending(&s, &scope, &call);
    s.run_permitted(scope, p.allow(), &call).unwrap();
    assert_eq!(s.take_url_requests().len(), 1);
    s.run("edit.undo", &Value::Null).unwrap();
    assert!(!s.item_trusted("task", "Reviewed"), "undo of creation removes its content record");
}

/// A request has one opaque claim, refuses a foreign completion, and revokes before sending.
#[test]
fn permission_policy_deferred_url_claim_keeps_owner_and_generation() {
    let mut s = world();
    #[cfg(not(target_arch = "wasm32"))]
    {
        s.leave_downloads = true;
    }
    let (scope, live) = assistant(3);
    let (foreign, _) = assistant(4);
    let call = Call::command("layer.addUrl", json!({"url":"https://example.invalid/data.geojson"}));
    let p = pending(&s, &scope, &call);
    s.run_permitted(scope.clone(), p.allow(), &call).unwrap();
    let q = s.take_url_requests().pop().unwrap();
    let id = s.download_started_for(&q).unwrap();
    assert!(s.download_started_for(&q).is_err());
    let still = s.download_still(id).unwrap();
    assert!(still());
    let error = s.run_scoped(foreign, &Call::command("layer.addDownload", json!({"id":id}))).unwrap_err();
    assert!(error.contains("another caller"), "{error}");
    assert!(s.download_continues(id).is_ok(), "foreign call does not consume the owner's result");
    live.0.store(false, Ordering::SeqCst);
    assert!(!still());
    assert!(s.download_continues(id).is_err());
    assert!(!still());
    let (scope, _) = assistant(5);
    let p = pending(&s, &scope, &call);
    s.run_permitted(scope, p.allow(), &call).unwrap();
    let q = s.take_url_requests().pop().unwrap();
    let id = s.download_started_for(&q).unwrap();
    let still = s.download_still(id).unwrap();
    s.run("project.new", &json!({"name":"Replacement"})).unwrap();
    assert!(!still());
    assert!(s.download_continues(id).is_err());
}

/// Native tools check revocation on actual worker entry and again before applying a result.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn permission_policy_native_tool_revocation_and_foreign_finish() {
    let mut s = world();
    #[cfg(not(target_arch = "wasm32"))]
    {
        s.leave_downloads = true;
    }
    let (scope, live) = assistant(6);
    let run = s
        .with_scope(scope.clone(), |s| crate::cmd::gp::tool_start(s, "buffer", &json!({"input":"Cities", "distance":10})))
        .unwrap()
        .unwrap();
    let id = run.id;
    live.0.store(false, Ordering::SeqCst);
    let before = s.map().layers.len();
    let done = run.run(&rustgis_analysis::control::Control::new());
    assert!(crate::cmd::gp::tool_complete(&mut s, id, done).is_err());
    assert_eq!(s.map().layers.len(), before);
    let (scope, _) = assistant(7);
    let run = s
        .with_scope(scope.clone(), |s| crate::cmd::gp::tool_start(s, "buffer", &json!({"input":"Cities", "distance":10})))
        .unwrap()
        .unwrap();
    let id = run.id;
    let (foreign, _) = assistant(8);
    assert!(
        s.with_scope(foreign.clone(), |s| crate::cmd::gp::plugin_complete(s, id, Err("foreign result".into())))
            .unwrap()
            .unwrap_err()
            .contains("another caller")
    );
    crate::cmd::gp::tool_complete(&mut s, id, run.run(&rustgis_analysis::control::Control::new())).unwrap();
    assert!(s.run_scoped(foreign, &Call::command("gp.finish", json!({"id":id}))).unwrap_err().contains("another caller"));
    s.with_scope(scope, |s| s.run("gp.finish", &json!({"id":id}))).unwrap().unwrap();
    assert_eq!(s.map().layers.len(), before + 1);
    assert!(s.finish_gp(id).is_err());
}

/// Presence is a registry invariant, not evidence that each classification is correct.
#[test]
fn permission_policy_metadata_presence_and_unreviewed_default() {
    for spec in crate::cmd::all() {
        assert!(!spec.effects.contains(crate::policy::Effect::Unreviewed), "{}", spec.id);
        assert!(spec.roles.is_some(), "{}", spec.id);
    }
    let mut s = world();
    #[cfg(not(target_arch = "wasm32"))]
    {
        s.leave_downloads = true;
    }
    s.registry.commands.push(crate::CommandSpec::new("test.unreviewed", "Unreviewed", "Test", |s, _| {
        s.map_mut().name = "Changed".into();
        Ok(Value::Null)
    }));
    let (scope, _) = assistant(9);
    let call = Call::command("test.unreviewed", Value::Null);
    assert!(matches!(s.review(&scope, &call), Outcome::Refused(_)));
    assert!(s.run_scoped(scope, &call).is_err());
    assert_ne!(s.map().name, "Changed");
    s.run("test.unreviewed", &Value::Null).unwrap();
    let (scope, _) = assistant(13);
    let before = s.map().name.clone();
    let huge = Call::script(&"layer.addData {\"path\":\"never read.gpkg\"}\n".repeat(10_000));
    assert!(matches!(s.review(&scope, &huge), Outcome::Refused(e) if e.contains("too large")));
    assert_eq!(s.map().name, before);
}

/// Actual execution families retain additive reach and every plugin call asks separately.
#[test]
fn permission_policy_effects_and_real_plugin_approval() {
    use crate::policy::{Effect, command_effects};
    let mut s = world();
    for id in ["layer.addSql", "cube.reduce", "cube.trend"] {
        let effects = command_effects(s.registry.get(id).unwrap(), &Value::Null);
        assert!(effects.contains(Effect::Files) && effects.contains(Effect::Network), "{id}: {effects:?}");
    }
    assert!(command_effects(s.registry.get("project.open").unwrap(), &Value::Null).contains(Effect::Replaces));
    assert!(command_effects(s.registry.get("notebook.run").unwrap(), &Value::Null).contains(Effect::Files));
    assert!(command_effects(s.registry.get("gp.run").unwrap(), &json!({"tool":"buffer"})).contains(Effect::Edits));
    let mut pkg = rustgis_plugins::Package::from_zip(&rustgis_plugins::testing::package("eng-policy-approval", &["edit"])).unwrap();
    pkg.manifest.tools.iter_mut().find(|t| t.id == "x").unwrap().name = "Permission selection".into();
    rustgis_plugins::load(pkg, "test").unwrap();
    let tool = "plugin:eng-policy-approval/x";
    let effects = command_effects(s.registry.get("gp.run").unwrap(), &json!({"tool":tool,"params":{"input":"Cities"}}));
    assert!(effects.contains(Effect::Edits) && effects.contains(Effect::Plugin));
    let (scope, _) = assistant(14);
    for name in [tool, "Permission selection"] {
        let call = Call::tool(name, json!({"input":"Cities"}));
        let p = pending(&s, &scope, &call);
        assert!(!p.review().remember);
        s.run_permitted(scope.clone(), p.allow(), &call).unwrap();
        assert_eq!(s.map().layer_by_name("Cities").unwrap().selection.len(), 2);
        assert!(!pending(&s, &scope, &call).review().remember);
    }
    rustgis_plugins::unload("eng-policy-approval");
}

/// The newly upstreamed native classification route carries its owner through work and `ran`.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn permission_policy_native_classification_keeps_scope_through_completion() {
    use crate::cmd::workflows::{raster_land, raster_start};
    let mut s = world();
    s.run("layer.addDataBytes", &json!({"fileName":"scene.tif", "data":two_class_b64()})).unwrap();
    let params = json!({"layer":"scene", "classes":3});
    let (scope, live) = assistant(10);
    let run = s.with_scope(scope, |s| raster_start(s, "raster.classify", &params)).unwrap().unwrap();
    live.0.store(false, Ordering::SeqCst);
    assert!(run.run(&rustgis_analysis::control::Control::new()).err().unwrap().contains("revoked"));
    let (scope, _) = assistant(11);
    let run = s.with_scope(scope.clone(), |s| raster_start(s, "raster.classify", &params)).unwrap().unwrap();
    let id = run.id;
    let made = run.run(&rustgis_analysis::control::Control::new()).unwrap();
    raster_land(&mut s, id, made).unwrap();
    let (foreign, _) = assistant(12);
    let finish = Call::command("raster.classify", json!({"ran":id}));
    assert!(s.run_scoped(foreign, &finish).unwrap_err().contains("another caller"));
    s.run_scoped(scope.clone(), &finish).unwrap();
    assert!(s.run_scoped(scope, &finish).is_err());
}

/// The new export worker cannot start after revocation or land in a replacement project;
/// native snapshots still report after the person switches maps.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn permission_policy_export_worker_keeps_its_initiating_owner() {
    use crate::cmd::exports::{export_land, export_start};
    use crate::policy::AgentAccess;
    use rustgis_analysis::control::Control;
    let mut s = world();
    let dir = std::env::temp_dir().join(format!("rustgis-export-owner-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = |name: &str| dir.join(name);
    let params = |name: &str| json!({"layer":"Cities", "path":path(name)});
    let (assistant, _) = assistant(129);
    assert!(s.with_scope(assistant, |s| export_start(s, "share.exportLayer", &params("unapproved.geojson"))).unwrap().is_err());
    let live = Arc::new(Live(AtomicBool::new(true)));
    let scope = Scope::agent(AgentAccess::Allow, false, "export-owner", None, live.clone());
    let run = s.with_scope(scope.clone(), |s| export_start(s, "share.exportLayer", &params("revoked.geojson"))).unwrap().unwrap();
    live.0.store(false, Ordering::SeqCst);
    assert!(std::thread::spawn(move || run.run(&Control::new())).join().unwrap().is_err());
    assert!(!path("revoked.geojson").exists());
    live.0.store(true, Ordering::SeqCst);
    let run = s.with_scope(scope, |s| export_start(s, "share.exportLayer", &params("completed.geojson"))).unwrap().unwrap();
    let wrote = run.run(&Control::new()).unwrap();
    s.run("project.new", &Value::Null).unwrap();
    assert!(export_land(&mut s, wrote).unwrap_err().contains("replaced project"));
    assert!(path("completed.geojson").exists(), "a completed write is not undone");
    s.run("project.sample", &Value::Null).unwrap();
    let run = export_start(&mut s, "share.exportLayer", &params("native.geojson")).unwrap();
    s.run("map.new", &json!({"name":"Other"})).unwrap();
    let wrote = run.run(&Control::new()).unwrap();
    let count = s.map().layers.len();
    assert_eq!(export_land(&mut s, wrote).unwrap()["path"], json!(path("native.geojson")));
    assert_eq!(s.map().layers.len(), count);
    std::fs::remove_dir_all(dir).unwrap();
}

/// The delayed UI token inherits a scoped caller and cannot escape a handler's transaction.
#[test]
fn permission_policy_prepared_plugin_keeps_scope_and_pre_handler_boundary() {
    use crate::policy::AgentAccess;
    let mut s = world();
    plugin("eng-prepared-owner", &["edit"]);
    let id = "plugin:eng-prepared-owner/f";
    let params = json!({"op":"map.rename", "params":{"name":"Queued"}});
    let before = s.map().name.clone();
    let live = Arc::new(Live(AtomicBool::new(true)));
    let scope = Scope::agent(AgentAccess::Allow, false, "prepared-owner", None, live.clone());
    let prepared = s.with_scope(scope, |s| s.prepare_command(id, &params)).unwrap().unwrap();
    live.0.store(false, Ordering::SeqCst);
    assert!(s.run_prepared(prepared).unwrap_err().contains("revoked"));
    assert!(!s.leave_downloads, "a refused prepared command restores the caller's download mode");
    assert_eq!(s.map().name, before);
    assert!(
        s.guarded("Transaction", true, |s| {
            assert!(s.prepare_command(id, &params).err().unwrap().contains("before handler"));
            Err("a later transaction failure".into())
        })
        .is_err()
    );
    assert_eq!(s.map().name, before);
    rustgis_plugins::unload("eng-prepared-owner");
}

/// Public creation returns a numeric notebook handle; trust follows its exact content and undo.
#[test]
fn permission_policy_notebook_creator_edit_and_import_provenance() {
    let mut s = world();
    let made = s.run("notebook.new", &json!({"name":"Local"})).unwrap();
    assert!(made["notebook"].is_number());
    assert!(s.item_trusted("notebook", "Local"));
    s.run("notebook.editCell", &json!({"notebook":"Local","cell":1,"source":"map.rename {\"name\":\"Notebook map\"}"})).unwrap();
    assert!(s.item_trusted("notebook", "Local"));
    let (scope, _) = assistant(101);
    let call = Call::command("notebook.run", json!({"notebook":"Local","cell":1}));
    let p = pending(&s, &scope, &call);
    s.run_permitted(scope.clone(), p.allow(), &call).unwrap();
    assert_eq!(s.map().name, "Notebook map");
    plugin("policy-notebook", &["edit", "files"]);
    forward(
        &mut s,
        "policy-notebook",
        "notebook.editCell",
        json!({"notebook":"Local","cell":1,"source":"map.rename {\"name\":\"plugin edit\"}"}),
    )
    .unwrap();
    assert!(!s.item_trusted("notebook", "Local"));
    s.run("edit.undo", &Value::Null).unwrap();
    assert!(s.item_trusted("notebook", "Local"));
    let text = json!({"name":"Imported","cells":[{"kind":"code","source":"map.zoomFull {}"}]}).to_string();
    s.run("notebook.open", &json!({"text":text})).unwrap();
    s.run("notebook.editCell", &json!({"notebook":"Imported","cell":0,"source":"map.zoomIn {}"})).unwrap();
    assert!(!s.item_trusted("notebook", "Imported"));
    rustgis_plugins::unload("policy-notebook");
}

/// Plans accept their own new outputs/renames while refusing unknown producers and stale cards.
#[test]
fn permission_policy_ordered_outputs_rename_and_default_cursor() {
    let mut s = world();
    let (scope, _) = assistant(102);
    s.run(
        "task.new",
        &json!({"name":"Make then use","steps":[
            {"title":"Step","command":"gp.run","params":{"tool":"buffer","params":{"input":"Cities","distance":10,"output":"Fresh"}}},
            {"title":"Step","command":"layer.rename","params":{"layer":"Fresh","name":"Renamed"}},
            {"title":"Step","command":"layer.visibility","params":{"visible":false}}
        ]}),
    )
    .unwrap();
    let call = Call::command("task.run", json!({"task":"Make then use"}));
    let p = pending(&s, &scope, &call);
    s.run_permitted(scope.clone(), p.allow(), &call).unwrap();
    assert!(!s.map().layer_by_name("Renamed").unwrap().visible);
    s.run("edit.undo", &Value::Null).unwrap();
    assert!(s.map().layer_by_name("Renamed").is_none());
    s.run(
        "layer.addDataText",
        &json!({"name":"Numeric","fileName":"Numeric.geojson","text":json!({"type":"FeatureCollection","features":[
        {"type":"Feature","properties":{"v":2},"geometry":{"type":"Point","coordinates":[0,0]}},
        {"type":"Feature","properties":{"v":4},"geometry":{"type":"Point","coordinates":[1,1]}}
    ]}).to_string()}),
    )
    .unwrap();
    let origin = s.map().name.clone();
    // gp.run retains its existing nonempty-map enabled check; the input below is still
    // the produced project-global table, with no same-named layer in this destination.
    s.run("map.duplicate", &json!({"name":"Table destination"})).unwrap();
    s.run("map.activate", &json!({"name":origin})).unwrap();
    s.run("task.new", &json!({"name":"Produce table then use","steps":[
        {"title":"Statistics","command":"gp.run","params":{"tool":"summaryStatistics","params":{"input":"Numeric","field":"v","output":"Numeric_Statistics"}}},
        {"title":"Switch","command":"map.activate","params":{"name":"Table destination"}},
        {"title":"Use global table","command":"gp.run","params":{"tool":"summaryStatistics","params":{"input":"Numeric_Statistics","field":"FREQUENCY","output":"Numeric_Statistics_Statistics"}}}
    ]})).unwrap();
    let table_call = Call::command("task.run", json!({"task":"Produce table then use"}));
    let approved = pending(&s, &scope, &table_call);
    s.run_permitted(scope.clone(), approved.allow(), &table_call).unwrap();
    assert!(s.project.tables.iter().any(|t| t.dataset.name == "Numeric_Statistics_Statistics"));
    assert!(s.map().layer_by_name("Numeric_Statistics").is_none());
    s.run("edit.undo", &Value::Null).unwrap();
    assert_eq!(s.map().name, origin);
    assert!(!s.project.tables.iter().any(|t| t.dataset.name == "Numeric_Statistics"));
    let missing = Call::script("layer.rename {\"layer\":\"Absent\",\"name\":\"No\"}\nlayer.addData {\"path\":\"never.gpkg\"}");
    assert!(matches!(s.review(&scope, &missing), Outcome::Refused(_)));
    s.run("layer.select", &json!({"layer":"Cities"})).unwrap();
    s.run(
        "task.new",
        &json!({"name":"Rename existing","steps":[
            {"title":"Step","command":"layer.rename","params":{"layer":"Cities","name":"Same id"}},
            {"title":"Step","command":"layer.visibility","params":{"visible":false}}
        ]}),
    )
    .unwrap();
    let call = Call::command("task.run", json!({"task":"Rename existing"}));
    let p = pending(&s, &scope, &call);
    s.run_permitted(scope.clone(), p.allow(), &call).unwrap();
    assert!(!s.map().layer_by_name("Same id").unwrap().visible);
    s.run("edit.undo", &Value::Null).unwrap();
    s.run(
        "task.new",
        &json!({"name":"Remove default","steps":[
            {"title":"Step","command":"layer.remove","params":{"layer":"Cities"}},
            {"title":"Step","command":"layer.visibility","params":{"visible":false}}
        ]}),
    )
    .unwrap();
    let call = Call::command("task.run", json!({"task":"Remove default"}));
    let p = pending(&s, &scope, &call);
    assert!(s.run_permitted(scope, p.allow(), &call).is_err());
    assert!(s.map().layer_by_name("Cities").is_some(), "transaction rolls back the removal");
    // Layer ids are per-map: activation resolves later defaults on the destination, while a
    // changed destination before approval use never inherits a same-numbered source layer.
    let original = s.map().name.clone();
    let original_map = s.map().instance_id();
    let shared_id = s.map().layers.first().unwrap().id;
    s.run("map.duplicate", &json!({"name":"Destination"})).unwrap();
    let destination = s.map().instance_id();
    assert_ne!(original_map, destination);
    assert_eq!(s.map().layers.first().unwrap().id, shared_id);
    s.run("map.activate", &json!({"name":original})).unwrap();
    s.run(
        "task.new",
        &json!({"name":"Activate then edit","steps":[
            {"title":"Step","command":"map.activate","params":{"name":"Destination"}},
            {"title":"Step","command":"layer.visibility","params":{"visible":false}}
        ]}),
    )
    .unwrap();
    let call = Call::command("task.run", json!({"task":"Activate then edit"}));
    let (scope, _) = assistant(122);
    let p = pending(&s, &scope, &call);
    s.run_permitted(scope.clone(), p.allow(), &call).unwrap();
    assert_eq!(s.map().instance_id(), destination);
    assert!(!s.map().layer(shared_id).unwrap().visible);
    assert!(s.project.maps.iter().find(|m| m.instance_id() == original_map).unwrap().layer(shared_id).unwrap().visible);
    s.run("edit.undo", &Value::Null).unwrap();
    assert_eq!(s.map().instance_id(), original_map);
    let p = pending(&s, &scope, &call);
    let dest = s.project.maps.iter_mut().find(|m| m.instance_id() == destination).unwrap();
    dest.layer_mut(shared_id).unwrap().renew_instance_id();
    assert!(s.run_permitted(scope, p.allow(), &call).unwrap_err().contains("changed"));
    assert_eq!(s.map().instance_id(), original_map);
    // Renaming keeps default identities, but later named-map choices cannot use the old namespace.
    // The executor makes an existing destination name unique; planning must not bind that old map.
    for renamed in ["Approved renamed", "Destination"] {
        let names = s.project.maps.iter().map(|m| m.name.clone()).collect::<Vec<_>>();
        let call = Call::script(&format!(
            "map.rename {}\nmap.activate {}\nshare.exportMap {{}}",
            json!({"name":renamed}),
            json!({"name":original})
        ));
        let (scope, _) = assistant(128);
        assert!(matches!(s.review(&scope, &call), Outcome::Refused(why) if why.contains("namespace")));
        assert_eq!(s.project.maps.iter().map(|m| m.name.clone()).collect::<Vec<_>>(), names);
    }
    // Planning streamed reach must use the borrowed destination, not the initial in-memory map.
    let mut source = rustgis_map::source::SourceLayer::default();
    source.path = Some("https://example.invalid/destination.fgb".into());
    let dest = s.project.maps.iter_mut().find(|m| m.instance_id() == destination).unwrap();
    let city = dest.layer_by_name("Cities").unwrap().id;
    dest.layer_mut(city).unwrap().source = Some(source);
    s.run(
        "task.new",
        &json!({"name":"Destination stream","steps":[
            {"title":"Activate","command":"map.activate","params":{"name":"Destination"}},
            {"title":"Buffer","command":"gp.run","params":{"tool":"buffer","params":{"input":"Cities","distance":10}}}
        ]}),
    )
    .unwrap();
    let (scope, live) = assistant(126);
    let streamed = Call::command("task.run", json!({"task":"Destination stream"}));
    let approved = pending(&s, &scope, &streamed);
    assert!(approved.review().steps.iter().any(|line| line.starts_with("buffer ")), "the destination's streamed tool is an external step");
    live.0.store(false, Ordering::SeqCst);
    assert!(s.run_permitted(scope, approved.allow(), &streamed).is_err());
    assert_eq!(s.map().instance_id(), original_map);
}

/// External dataset/sheet names are frozen with their origin without opening it for review.
#[test]
fn permission_policy_external_selectors_and_dataset_precedence() {
    let mut s = world();
    let (scope, _) = assistant(103);
    for (id, origin) in [("layer.addData", "path"), ("layer.addDatabase", "database"), ("info.fileColumns", "path")] {
        for name in ["absent dataset", ""] {
            let call = Call::command(id, json!({origin:"never-opened.gpkg","layer":name}));
            assert!(pending(&s, &scope, &call).review().why.contains("files"));
        }
    }
    let ds = s.map().layer_by_name("Cities").unwrap().dataset.clone();
    let table = s.project.add_table(ds);
    s.run("task.new", &json!({"name":"Colliding table","steps":[{"title":"Step","command":"gp.run","params":{"tool":"buffer","params":{"input":"Cities","distance":10}}}]})).unwrap();
    let call = Call::command("task.run", json!({"task":"Colliding table"}));
    let p = pending(&s, &scope, &call);
    s.project.tables.iter_mut().find(|t| t.id == table).unwrap().dataset.source = Some("changed unrelated table".into());
    // Replanning conservatively notices project changes, but the tool's actual target is the layer.
    let p2 = pending(&s, &scope, &call);
    assert_eq!(p.review().what, p2.review().what);
    s.run_permitted(scope, p2.allow(), &call).unwrap();
    assert!(s.map().layers.len() > 3);
}

/// A same-public-URL account replacement invalidates a held card and its owned send predicate.
#[test]
fn permission_policy_exact_runtime_account_rotation() {
    // Account stores are process-wide: isolate mutation from every parallel plan reader.
    #[cfg(not(target_arch = "wasm32"))]
    if std::env::var_os("RUSTGIS_POLICY_ACCOUNT_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::permission_policy::permission_policy_exact_runtime_account_rotation", "--nocapture"])
            .env("RUSTGIS_POLICY_ACCOUNT_CHILD", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }

    use crate::render::rustgis_tiles::earthdata;
    let mut s = world();
    #[cfg(not(target_arch = "wasm32"))]
    {
        s.leave_downloads = true;
    }
    let (scope, _) = assistant(104);
    earthdata::set_token(Some("synthetic-policy-first")).unwrap();
    let call = Call::command("layer.addUrl", json!({"url":"https://example.invalid/unchanged.geojson"}));
    let p = pending(&s, &scope, &call);
    earthdata::set_token(Some("synthetic-policy-second")).unwrap();
    assert!(s.run_permitted(scope.clone(), p.allow(), &call).unwrap_err().contains("changed"));
    let p = pending(&s, &scope, &call);
    s.run_permitted(scope, p.allow(), &call).unwrap();
    let request = s.take_url_requests().pop().unwrap();
    let id = s.download_started_for(&request).unwrap();
    let still = s.download_still(id).unwrap();
    assert!(still());
    earthdata::set_token(Some("synthetic-policy-third")).unwrap();
    assert!(!still());
    assert!(s.download_continues(id).is_err());
    earthdata::set_token(None).unwrap();
    use crate::render::rustgis_tiles::arcgis_auth;
    let url = "https://policy-identity.invalid/server/rest/services/Data/FeatureServer/0";
    let root = arcgis_auth::add_token(url, "synthetic-policy-server-first").unwrap();
    let (scope, _) = assistant(125);
    let call = Call::command("layer.addUrl", json!({"url":url}));
    let p = pending(&s, &scope, &call);
    arcgis_auth::add_token(url, "synthetic-policy-server-second").unwrap();
    assert!(s.run_permitted(scope, p.allow(), &call).unwrap_err().contains("changed"));
    arcgis_auth::sign_out(Some(root.trim_end_matches('/')));
}

/// Foreign in-place completions cannot borrow/consume ownership; omitted options retain defaults.
#[test]
fn permission_policy_download_refinements_rollback_and_abandonment() {
    let mut s = world();
    #[cfg(not(target_arch = "wasm32"))]
    {
        s.leave_downloads = true;
    }
    let (scope, _) = assistant(105);
    let (foreign, _) = assistant(106);
    let call = Call::command(
        "layer.addUrl",
        json!({"url":"https://example.invalid/data.csv","name":"Frozen","x":"east","y":"north","zoom":false}),
    );
    let p = pending(&s, &scope, &call);
    s.run_permitted(scope.clone(), p.allow(), &call).unwrap();
    let q = s.take_url_requests().pop().unwrap();
    let id = s.download_started_for(&q).unwrap();
    let parameters = q.params().clone();
    assert!(s.with_scope(foreign, |s| s.add_in_place_for(id, &parameters)).unwrap().unwrap_err().contains("another caller"));
    assert!(s.download_still(id).unwrap()());
    assert!(s.download_landed(id, b"east,north\n1,2\n".to_vec()));
    s.complete_download(id, &json!({"id":id})).unwrap();
    assert_eq!(s.map().layer_by_name("Frozen").unwrap().dataset.features.len(), 1);
    assert!(s.complete_download(id, &json!({"id":id})).is_err());
    assert!(s.run("layer.addUrl", &json!({"url":"https://example.invalid/project.json","geolibreProject":true})).is_err());
    for i in 0..32 {
        let failed = format!("layer.addUrl {{\"url\":\"https://example.invalid/rollback-{i}.csv\"}}\nmissing.command {{}}");
        assert!(s.run("script.run", &json!({"script":failed})).is_err());
        assert!(s.take_url_requests().is_empty());
    }
    s.request_url(&json!({"url":"https://example.invalid/earlier.csv"})).unwrap();
    let earlier = s.take_url_requests().pop().unwrap();
    let mut taken = None;
    assert!(
        s.guarded("Retained failed request", false, |s| {
            s.request_url(&json!({"url":"https://example.invalid/failed-retained.csv"}))?;
            taken = s.take_url_requests().pop();
            Err("fail after the owner took the request".into())
        })
        .is_err()
    );
    assert!(s.download_started_for(&taken.unwrap()).is_err());
    let id = s.download_started_for(&earlier).unwrap();
    assert!(s.download_continues(id).is_ok());
    s.download_failed(id);
    let mut held = Vec::new();
    for i in 0..crate::MAX_URL_REQUESTS {
        s.request_url(&json!({"url":format!("https://example.invalid/held-{i}.csv")})).unwrap();
        held.extend(s.take_url_requests());
    }
    assert!(s.request_url(&json!({"url":"https://example.invalid/over.csv"})).is_err());
    drop(held);
    s.request_url(&json!({"url":"https://example.invalid/fresh.csv"})).unwrap();
    assert_eq!(s.take_url_requests().len(), 1);
}

/// Async apply retains its initiating actor so only that agent can undo the new step.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn permission_policy_async_actor_and_streamed_preparation() {
    use crate::policy::AgentAccess;
    let mut s = world();
    let live = Arc::new(Live(AtomicBool::new(true)));
    let scope = Scope::agent(AgentAccess::Allow, true, "actor-run", Some("rightful".into()), live);
    let run = s
        .with_scope(scope.clone(), |s| crate::cmd::gp::tool_start(s, "buffer", &json!({"input":"Cities","distance":10})))
        .unwrap()
        .unwrap();
    let id = run.id;
    crate::cmd::gp::tool_complete(&mut s, id, run.run(&rustgis_analysis::control::Control::new())).unwrap();
    s.finish_gp(id).unwrap();
    assert_eq!(s.undo_actor(), Some(Some("rightful")));
    assert_eq!(s.actor, None);
    let foreign = Scope::agent(AgentAccess::Allow, true, "foreign", Some("foreign".into()), Arc::new(Live(AtomicBool::new(true))));
    assert!(s.run_scoped(foreign, &Call::command("edit.undo", Value::Null)).is_err());
    s.run_scoped(scope, &Call::command("edit.undo", Value::Null)).unwrap();
    assert_eq!(s.actor, None);
}

/// Hold the real streamed-input worker before its range send, then revoke its initiating scope.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn permission_policy_streamed_tool_worker_refuses_before_remote_input_send() {
    use std::sync::{Mutex, mpsc};
    struct Held {
        armed: AtomicBool,
        live: AtomicBool,
        polls: std::sync::atomic::AtomicUsize,
        entered: mpsc::SyncSender<()>,
        released: Mutex<mpsc::Receiver<()>>,
    }
    impl Revocation for Held {
        fn allows(&self, _: Effects) -> bool {
            if self.armed.load(Ordering::SeqCst) && self.polls.fetch_add(1, Ordering::SeqCst) == 1 {
                self.entered.send(()).unwrap();
                self.released.lock().unwrap().recv().unwrap();
            }
            self.live.load(Ordering::SeqCst)
        }
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/input.fgb", listener.local_addr().unwrap());
    let mut s = world();
    let cities = s.map().layer_by_name("Cities").unwrap().id;
    let mut source = rustgis_map::source::SourceLayer::default();
    source.path = Some(url);
    s.map_mut().layer_mut(cities).unwrap().source = Some(source);
    let (entered, waiting) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let live = Arc::new(Held {
        armed: AtomicBool::new(false),
        live: AtomicBool::new(true),
        polls: 0.into(),
        entered,
        released: Mutex::new(released),
    });
    let scope = Scope::agent(crate::policy::AgentAccess::Allow, false, "streamed-worker", None, live.clone());
    let run = s.with_scope(scope, |s| crate::cmd::gp::tool_start(s, "buffer", &json!({"input":"Cities","distance":10}))).unwrap().unwrap();
    let id = run.id;
    live.armed.store(true, Ordering::SeqCst);
    let worker = std::thread::spawn(move || run.run(&rustgis_analysis::control::Control::new()));
    let entered = waiting.recv_timeout(std::time::Duration::from_secs(3));
    live.live.store(false, Ordering::SeqCst);
    let _ = release.send(());
    assert!(entered.is_ok(), "the actual input preparation must inherit its worker's owned predicate");
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    assert!(crate::cmd::gp::tool_complete(&mut s, id, worker.join().unwrap()).is_err());
}

/// The real rustgis-source thread must carry the initiating context through a held source read.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn permission_policy_source_worker_checks_follow_on_send_and_landing() {
    use rustgis_data::source::{Batch, FeatureSource, Request, SourceInfo};
    use std::{
        io::Write,
        sync::{Mutex, mpsc},
        time::Duration,
    };
    struct HeldSource {
        inner: Box<dyn FeatureSource>,
        first: AtomicBool,
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        url: String,
    }
    impl FeatureSource for HeldSource {
        fn info(&self) -> &SourceInfo {
            self.inner.info()
        }
        fn read(&self, req: &Request) -> rustgis_data::Result<Batch> {
            if self.first.swap(false, Ordering::SeqCst) {
                assert_eq!(std::thread::current().name(), Some("rustgis-source"));
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                crate::render::rustgis_tiles::cache::download_blocking_within(&self.url, 64, Duration::from_secs(2))
                    .map_err(rustgis_data::DataError::Format)?;
            }
            self.inner.read(req)
        }
    }
    for revoke in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/follow-on", listener.local_addr().unwrap());
        let mut s = world();
        let bytes = Arc::new(rustgis_data::source::fgb::write(&s.map().layer_by_name("Cities").unwrap().dataset).unwrap());
        let inner = rustgis_data::source::open_shared("held.fgb", bytes.clone(), None).unwrap();
        let (entered, waiting) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let mut source = rustgis_map::source::SourceLayer::from_bytes(
            "held.fgb",
            bytes,
            None,
            Box::new(HeldSource { inner, first: AtomicBool::new(true), entered, release: Mutex::new(released), url }),
        );
        let (scope, live) = assistant(120 + u64::from(revoke));
        let call = Call::adapter("source export", Effects::NONE.with(crate::policy::Effect::Files), Value::Null);
        let p = pending(&s, &scope, &call);
        let completion = s.defer(scope, &call, Some(p.allow())).unwrap();
        s.finish_deferred(completion, |_| source.start(rustgis_geom::Extent::new(-180.0, -85.0, 180.0, 85.0), false)).unwrap();
        waiting.recv_timeout(Duration::from_secs(3)).unwrap();
        live.0.store(!revoke, Ordering::SeqCst);
        release.send(()).unwrap();
        if !revoke {
            let started = std::time::Instant::now();
            let mut peer = loop {
                match listener.accept() {
                    Ok((peer, _)) => break peer,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && started.elapsed() < Duration::from_secs(3) => {
                        std::thread::yield_now()
                    }
                    other => panic!("the allowed actual worker must send: {other:?}"),
                }
            };
            peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
            assert!(matches!(source.poll(true), rustgis_map::source::Polled::Done(_, _)));
        } else {
            assert!(matches!(source.poll(true), rustgis_map::source::Polled::Failed(_)));
            assert!(matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
            assert_eq!(source.window().unwrap().kept, 0);
        }
        assert!(rustgis_data::source::remote::current_still().is_none(), "synchronous reentry restores the caller");
    }
}

/// A notebook cannot change its frozen code while running; model wires and cycle refusal use real commands.
#[test]
fn permission_policy_notebook_self_edit_model_outputs_and_intersections() {
    let mut s = world();
    s.run("notebook.new", &json!({"name":"Self"})).unwrap();
    let script = "layer.visibility {\"layer\":\"Cities\",\"visible\":false}\nnotebook.editCell {\"notebook\":\"Self\",\"cell\":1,\"source\":\"map.zoomIn {}\"}\nmap.zoomOut {}";
    s.run("notebook.editCell", &json!({"notebook":"Self","cell":1,"source":script})).unwrap();
    let old = s.map().layer_by_name("Cities").unwrap().visible;
    let (scope, _) = assistant(122);
    let call = Call::command("notebook.run", json!({"notebook":"Self","cell":1}));
    let p = pending(&s, &scope, &call);
    assert!(s.run_permitted(scope.clone(), p.allow(), &call).is_err());
    assert_eq!(s.map().layer_by_name("Cities").unwrap().visible, old);
    assert!(s.project.notebooks.iter().find(|n| n.name == "Self").unwrap().cells[1].source.contains("notebook.editCell"));
    let graph = json!({"nodes":[
        {"id":"i","kind":"input","layer":"Cities"},
        {"id":"b","kind":"tool","tool":"buffer","params":{"distance":10}},
        {"id":"d","kind":"tool","tool":"dissolve"},
        {"id":"o","kind":"output","name":"Policy zones"}],
        "edges":[{"from":"i","fromPort":"out","to":"b","toPort":"input"},{"from":"b","fromPort":"out","to":"d","toPort":"input"},{"from":"d","fromPort":"out","to":"o","toPort":"in"}]});
    s.run("model.new", &json!({"name":"Wired","graph":graph})).unwrap();
    let call = Call::command("model.run", json!({"name":"Wired"}));
    let p = pending(&s, &scope, &call);
    s.run_permitted(scope.clone(), p.allow(), &call).unwrap();
    assert!(s.map().layer_by_name("Policy zones").is_some());
    let mut cycle = graph.clone();
    cycle["edges"].as_array_mut().unwrap().push(json!({"from":"d","fromPort":"out","to":"b","toPort":"input"}));
    assert!(
        s.run("model.new", &json!({"name":"Cycle","graph":cycle})).is_err()
            || matches!(s.review(&scope, &Call::command("model.run", json!({"name":"Cycle"}))), Outcome::Refused(_))
    );
    s.run(
        "task.new",
        &json!({"name":"Imported external","imported":true,"steps":[{"title":"Step","command":"layer.addData","params":{"path":"never-read.gpkg"}}]}),
    )
    .unwrap();
    let full = Scope::agent(crate::policy::AgentAccess::Full, false, "full", None, Arc::new(Live(AtomicBool::new(true))));
    assert!(s.run_scoped(full, &Call::command("task.run", json!({"task":"Imported external"}))).is_err());
    let nested = s.with_scope(scope.clone(), |s| s.run_scoped(Scope::peer(), &Call::command("map.zoomIn", Value::Null))).unwrap();
    assert!(nested.is_err(), "nested entry cannot widen or switch owner");
    let external = Call::command("layer.addData", json!({"path":"never-read.gpkg"})).with_request("first");
    let p = pending(&s, &scope, &external);
    assert!(s.run_permitted(scope, p.allow(), &external.with_request("second")).is_err());
}

/// State-dependent declarations follow the source actually used, including saved/default locator paths.
#[test]
fn permission_policy_effects_follow_saved_locator_streamed_and_time_sources() {
    let mut s = world();
    let (scope, _) = assistant(123);
    for p in [json!({"locator":"bare.rgisloc","text":"Austin"}), json!({"text":"Austin"})] {
        let outcome = s.review(&scope, &Call::command("locator.find", p));
        assert!(!matches!(outcome, Outcome::Allowed));
    }
    let id = s.map().layer_by_name("Cities").unwrap().id;
    let mut source = rustgis_map::source::SourceLayer::default();
    source.path = Some("https://example.invalid/data.fgb".into());
    s.map_mut().layer_mut(id).unwrap().source = Some(source);
    assert!(matches!(s.review(&scope, &Call::command("layer.loadAll", json!({"layer":"Cities"}))), Outcome::NeedsApproval(_)));
    s.run("layer.addDataBytes", &json!({"fileName":"scene.tif","data":two_class_b64()})).unwrap();
    s.map_mut().layers.iter_mut().find(|l| l.name == "scene").unwrap().raster.as_mut().unwrap().time_zarr =
        Some(rustgis_map::raster::ZarrTime {
            store: "https://example.invalid/cube.zarr".into(),
            variable: "v".into(),
            dim: "time".into(),
            dates: vec![0., 1.],
            ..Default::default()
        });
    assert!(matches!(s.review(&scope, &Call::command("time.step", json!({"direction":"next"}))), Outcome::NeedsApproval(_)));
}

/// Cross-map frames and detached clipboards bind their actual owners; managed shares bind saved paths.
#[test]
fn permission_policy_stored_cross_map_clipboard_and_managed_share_targets() {
    let mut s = world();
    s.leave_downloads = true;
    let original = s.map().name.clone();
    s.run("map.duplicate", &json!({"name":"Other"})).unwrap();
    s.run("map.activate", &json!({"name":original})).unwrap();
    s.run("layout.new", &json!({"name":"Framed","template":"blank"})).unwrap();
    let frame = s.run("layout.addFrame", &json!({"map":"Other"})).unwrap()["element"].clone();
    s.run(
        "task.new",
        &json!({"name":"Frame export","steps":[
            {"title":"Step","command":"layout.frameZoom","params":{"element":frame,"to":"layer","layer":"Cities"}},
            {"title":"Step","command":"layer.addUrl","params":{"url":"https://example.invalid/no-send.csv"}}
        ]}),
    )
    .unwrap();
    let (scope, _) = assistant(124);
    let call = Call::command("task.run", json!({"task":"Frame export"}));
    let p = pending(&s, &scope, &call);
    s.run("map.activate", &json!({"name":"Other"})).unwrap();
    s.run("layer.remove", &json!({"layer":"Cities"})).unwrap();
    s.run("layer.addDataText", &json!({"name":"Cities","fileName":"Cities.geojson","text":"{\"type\":\"Point\",\"coordinates\":[0,0]}"}))
        .unwrap();
    s.run("map.activate", &json!({"name":original})).unwrap();
    assert!(s.run_permitted(scope.clone(), p.allow(), &call).unwrap_err().contains("changed"));
    assert!(s.take_url_requests().is_empty());
    s.run("layer.copy", &json!({"layer":"Cities"})).unwrap();
    s.run("task.new",&json!({"name":"Paste","steps":[{"title":"Step","command":"layer.paste","params":{"map":"Other"}},{"title":"Step","command":"layer.addUrl","params":{"url":"https://example.invalid/no-send.csv"}}]})).unwrap();
    let call = Call::command("task.run", json!({"task":"Paste"}));
    let p = pending(&s, &scope, &call);
    s.run("layer.copy", &json!({"layer":"Countries"})).unwrap();
    assert!(s.run_permitted(scope.clone(), p.allow(), &call).unwrap_err().contains("changed"));
    let dir = std::env::temp_dir().join(format!("rustgis-policy-stored-{}-{}", std::process::id(), crate::project_log::now_ms()));
    std::fs::create_dir(&dir).unwrap();
    let first = dir.join("first.zip");
    let second = dir.join("second.zip");
    s.run("share.webLayer", &json!({"layer":"Cities","path":first})).unwrap();
    let call = Call::command("share.replaceWebLayer", json!({}));
    let p = pending(&s, &scope, &call);
    assert!(
        p.review()
            .details
            .iter()
            .any(|d| d.targets.iter().any(|n| n.ends_with(" / Cities")) && d.places.iter().any(|n| n == &first.display().to_string()))
    );
    assert!(p.review().defaults.is_none(), "a managed saved source does not act on the unrelated active layer");
    s.run("share.webLayer", &json!({"layer":"Countries","path":second})).unwrap();
    let before = std::fs::read(&second).unwrap();
    assert!(s.run_permitted(scope, p.allow(), &call).unwrap_err().contains("changed"));
    assert_eq!(std::fs::read(&second).unwrap(), before);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Plugin preparation reads a held streamed source before returning its pinned job.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn permission_policy_plugin_preparation_keeps_owner_before_remote_send() {
    use rustgis_data::source::{Batch, FeatureSource, Request, SourceInfo};
    use std::{
        io::Write,
        sync::{Mutex, mpsc},
        time::Duration,
    };
    struct Held {
        inner: Box<dyn FeatureSource>,
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        url: String,
        first: AtomicBool,
    }
    impl FeatureSource for Held {
        fn info(&self) -> &SourceInfo {
            self.inner.info()
        }
        fn read(&self, request: &Request) -> rustgis_data::Result<Batch> {
            if self.first.swap(false, Ordering::SeqCst) {
                assert!(rustgis_data::source::remote::current_still().is_some(), "actual plugin input preparation owns its carrier");
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                crate::render::rustgis_tiles::cache::download_blocking_within(&self.url, 64, Duration::from_secs(2))
                    .map_err(rustgis_data::DataError::Format)?;
            }
            self.inner.read(request)
        }
    }
    let plugin = "policy-preparing-owner";
    rustgis_plugins::load(rustgis_plugins::Package::from_zip(&rustgis_plugins::testing::package(plugin, &["edit"])).unwrap(), "test")
        .unwrap();
    for revoke in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/input-prepare", listener.local_addr().unwrap());
        let mut s = world();
        let id = s.map().layer_by_name("Cities").unwrap().id;
        let bytes = Arc::new(rustgis_data::source::fgb::write(&s.map().layer(id).unwrap().dataset).unwrap());
        let inner = rustgis_data::source::open_shared("input.fgb", bytes.clone(), None).unwrap();
        let (entered, waiting) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        s.map_mut().layer_mut(id).unwrap().source = Some(rustgis_map::source::SourceLayer::from_bytes(
            "input.fgb",
            bytes,
            None,
            Box::new(Held { inner, entered, release: Mutex::new(released), url, first: AtomicBool::new(true) }),
        ));
        let live = Arc::new(Live(AtomicBool::new(true)));
        let scope = Scope::agent(crate::policy::AgentAccess::Allow, false, "plugin-preparation", None, live.clone());
        let worker = std::thread::spawn(move || {
            let result = s
                .with_scope(scope, |s| crate::cmd::gp::plugin_start(s, "plugin:policy-preparing-owner/x", &json!({"input":"Cities"})))
                .unwrap();
            assert!(rustgis_data::source::remote::current_still().is_none());
            result
        });
        waiting.recv_timeout(Duration::from_secs(3)).unwrap();
        live.0.store(!revoke, Ordering::SeqCst);
        release.send(()).unwrap();
        if !revoke {
            let start = std::time::Instant::now();
            let mut peer = loop {
                match listener.accept() {
                    Ok((peer, _)) => break peer,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && start.elapsed() < Duration::from_secs(3) => {
                        std::thread::yield_now()
                    }
                    other => panic!("live preparation must actually send: {other:?}"),
                }
            };
            peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
        }
        let result = worker.join().unwrap();
        assert_eq!(result.is_err(), revoke);
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }
    rustgis_plugins::unload(plugin);
}

/// Choosing another already-active login changes consent without rotating either bearer.
#[test]
fn permission_policy_web_login_reselection_refuses_old_consent_and_landing() {
    use crate::{render::rustgis_tiles::logins, service_logins};
    let mut s = Session::new(Project::default());
    let origin = "https://policy-choice-r15.example.invalid";
    let mut ids = Vec::new();
    for name in ["A", "B"] {
        let id =
            s.run("connection.new", &json!({"kind":"wms","name":name,"address":origin,"auth":"header","keyName":"X-Key"})).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string();
        s.run("connection.login", &json!({"id":id,"apiKey":format!("RGSYNTH_choice_r15_{name}")})).unwrap();
        ids.push(id);
    }
    s.run("connection.login", &json!({"id":ids[0]})).unwrap();
    let registry = logins::account_identity(s.web_logins.session_id()).unwrap();
    let url = format!("{origin}/roads.geojson");
    let selected_a = service_logins::request(&mut s, None, &url).unwrap();
    let call = Call::command("layer.addUrl", json!({"url":url,"zoom":true})).with_request("choice-r15");
    let (scope, _) = assistant(1501);
    let old = pending(&s, &scope, &call);
    let run = pending(&s, &scope, &call);
    #[cfg(not(target_arch = "wasm32"))]
    {
        s.leave_downloads = true;
    }
    s.run_permitted(scope.clone(), run.allow(), &call).unwrap();
    let request = s.take_url_requests().pop().unwrap();
    assert_eq!(request.params()["url"], selected_a);
    assert_eq!(request.params()["connection"], ids[0]);
    let claim = s.download_started_for(&request).unwrap();
    s.run("connection.login", &json!({"id":ids[1]})).unwrap();
    assert!(registry.is_current(), "reselection keeps both active generations");
    let selected_b = service_logins::request(&mut s, None, &url).unwrap();
    assert_ne!(selected_a, selected_b);
    assert_eq!(logins::apply(&selected_a).unwrap().unwrap().header.unwrap().1, "RGSYNTH_choice_r15_A");
    assert!(s.run_permitted(scope, old.allow(), &call).unwrap_err().contains("changed"));
    assert!(s.take_url_requests().is_empty(), "the old approval started no fresh request");
    assert!(s.download_continues(claim).unwrap_err().contains("account changed"));
    assert!(s.map().layers.is_empty());
}

/// Runtime snapshots include absence and this owner's logout, while another Session is separate.
#[test]
fn permission_policy_web_accounts_ignore_other_sessions_and_revoke_owned_sends() {
    use crate::render::rustgis_tiles::logins;
    let mut s = Session::new(Project::default());
    let origin = "https://policy-owner-r15.example.invalid";
    let definition = json!({"kind":"wms","address":origin,"auth":"header","keyName":"X-Key"});
    let id = s.run("connection.new", &definition).unwrap()["id"].as_str().unwrap().to_string();
    let absent = logins::account_identity(s.web_logins.session_id()).unwrap();
    s.run("connection.login", &json!({"id":id,"apiKey":"fake002"})).unwrap();
    assert!(!absent.is_current(), "a newly covering login is a different runtime identity");
    let registry = logins::account_identity(s.web_logins.session_id()).unwrap();
    let (scope, _) = assistant(1502);
    let call = Call::command("layer.addUrl", json!({"url":format!("{origin}/roads.geojson")})).with_request("owner-r15");
    let approved = pending(&s, &scope, &call);
    let mut other = Session::new(Project::default());
    let other_id = other.run("connection.new", &definition).unwrap()["id"].as_str().unwrap().to_string();
    other.run("connection.login", &json!({"id":other_id,"apiKey":"fake003"})).unwrap();
    assert!(registry.is_current());
    #[cfg(not(target_arch = "wasm32"))]
    {
        s.leave_downloads = true;
    }
    s.run_permitted(scope, approved.allow(), &call).unwrap();
    let request = s.take_url_requests().pop().unwrap();
    let claim = s.download_started_for(&request).unwrap();
    let still = s.download_still(claim).unwrap();
    assert!(still());
    s.run("connection.logout", &json!({"id":id})).unwrap();
    assert!(!registry.is_current());
    assert!(!still(), "the actual send predicate sees this owner's logout");
    assert!(!s.download_landed(claim, b"late".to_vec()));
    assert!(s.map().layers.is_empty());
}
