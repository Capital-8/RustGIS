//! Review regressions exercise public admission and opaque download ownership without I/O.
use super::*;
use crate::policy::{AgentAccess, Call, Effect, Effects, Outcome, Revocation, Scope};
use std::sync::Arc;

struct Live;
impl Revocation for Live {
    fn allows(&self, _: Effects) -> bool {
        true
    }
}

/// A live agent whose external access setting is enforced by the normal decision table.
fn agent(access: AgentAccess) -> Scope {
    Scope::agent(access, true, "review-boundaries", None, Arc::new(Live))
}

#[test]
fn permission_review_authentication_changes_are_reserved() {
    let s = world();
    for (id, params) in [
        ("arcgis.signIn", json!({})),
        ("arcgis.token", json!({"url":"https://auth-review.example.invalid/arcgis/rest/services","token":"RGSYNTH_review_auth"})),
        ("arcgis.signOut", json!({})),
        ("arcgis.cancelSignIn", json!({})),
    ] {
        let call = Call::command(id, params);
        assert!(matches!(s.review(&agent(AgentAccess::Allow), &call), Outcome::Refused(_)), "{id} is reserved even with external access");
        assert!(
            matches!(s.review(&Scope::assistant(1461, Arc::new(Live)), &call), Outcome::NeedsApproval(_)),
            "{id} needs explicit approval"
        );
        assert!(matches!(s.review(&agent(AgentAccess::Full), &call), Outcome::Allowed), "full native-client access remains explicit");
    }
}

#[test]
fn permission_review_memory_only_commands_need_no_external_access() {
    let mut s = world();
    let mut errors = Vec::new();
    for (id, params) in [
        ("graphics.add", json!({"shape":"text","points":[[0,0]],"text":"Review note"})),
        ("cache.empty", json!({})),
        ("cache.cancel", json!({})),
        ("map.body", json!({"name":"moon","basemap":false})),
    ] {
        if let Err(error) = s.with_scope(agent(AgentAccess::Refuse), |s| s.run(id, &params)).unwrap() {
            errors.push((id, error));
        }
    }
    assert!(errors.is_empty(), "in-memory work was refused: {errors:?}");
    assert_eq!(s.map().body().id, "moon");
    for call in [
        Call::command("graphics.add", json!({"shape":"picture","points":[[0,0],[1,1]],"path":"missing-review.png"})),
        Call::command("cache.emptyIndexes", json!({})),
        Call::command("map.body", json!({"name":"earth"})),
    ] {
        assert!(matches!(s.review(&agent(AgentAccess::Refuse), &call), Outcome::Refused(_)), "real external reach stays gated");
    }
}

#[test]
fn permission_review_postgis_write_declares_network_reach() {
    struct NoNetwork;
    impl Revocation for NoNetwork {
        fn allows(&self, effects: Effects) -> bool {
            !effects.contains(Effect::Network)
        }
    }
    let s = world();
    let scope = Scope::assistant(1462, Arc::new(NoNetwork));
    let call = Call::command("pg.saveEdits", json!({"layer":"Cities"}));
    assert!(matches!(s.review(&scope, &call), Outcome::Refused(_)), "a database write must check live network authority before any I/O");
    assert!(matches!(s.review(&scope, &Call::command("pg.pendingEdits", json!({"layer":"Cities"}))), Outcome::Allowed));
}

#[test]
fn permission_review_url_claim_rejects_another_sessions_request() {
    let mut a = world();
    let mut b = world();
    for (s, url) in [(&mut a, "https://request-a.invalid/a.geojson"), (&mut b, "https://request-b.invalid/b.geojson")] {
        s.request_url(&json!({"url":url})).unwrap();
    }
    let qa = a.take_url_requests().pop().unwrap();
    let qb = b.take_url_requests().pop().unwrap();
    assert_eq!(qa.key, qb.key, "independent sessions reuse their own queue sequence");
    assert!(b.download_started_for(&qa).is_err(), "a foreign opaque request must not consume the matching local queue key");
    let id = b.download_started_for(&qb).unwrap();
    assert!(b.download_continues(id).is_ok(), "the rejected claim preserves the rightful request");
    assert!(a.download_started_for(&qa).is_ok());
    assert!(b.download_started_for(&qb).is_err(), "a rightful claim still has one use");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn permission_review_non_s3_download_survives_an_unrelated_storage_login() {
    if isolate_account_test("tests::review_boundaries::permission_review_non_s3_download_survives_an_unrelated_storage_login") {
        return;
    }
    use crate::render::rustgis_tiles::s3;
    let connection = |name: &str, bucket: &str, endpoint: &str| s3::Connection {
        name: name.into(),
        endpoint: Some(endpoint.into()),
        region: s3::DEFAULT_REGION.into(),
        buckets: vec![bucket.into()],
        credentials: None,
    };
    s3::connect(connection("review-owned", "review-owned", "https://owned-storage.invalid")).unwrap();
    let mut s = world();
    let mut ids = Vec::new();
    for url in ["https://plain-request.invalid/data.geojson", "s3://review-owned/data.geojson"] {
        s.request_url(&json!({"url":url})).unwrap();
        let request = s.take_url_requests().pop().unwrap();
        ids.push(s.download_started_for(&request).unwrap());
    }
    let plain_still = s.download_still(ids[0]).unwrap();
    let storage_still = s.download_still(ids[1]).unwrap();
    s3::connect(connection("review-unrelated", "review-unrelated", "https://other-storage.invalid")).unwrap();
    assert!(s.download_continues(ids[0]).is_ok(), "an unrelated S3 login cannot change this HTTPS request's account choice");
    assert!(plain_still(), "the actual sender keeps the same account choice");

    // A service can follow discovered links even when its first URL looks like a data file.
    s.request_url(&json!({"url":"https://plain-request.invalid/data.geojson","service":true})).unwrap();
    let discovery = s.take_url_requests().pop().unwrap();
    let discovery = s.download_started_for(&discovery).unwrap();
    s3::connect(connection("review-owned", "review-owned", "https://changed-storage.invalid")).unwrap();
    assert!(!storage_still(), "a changed account refuses the actual sender before another request");
    assert!(s.download_continues(ids[1]).is_err(), "a changed endpoint for the actual S3 source still revokes its continuation");
    assert!(s.download_continues(discovery).is_err(), "unresolved service discovery keeps conservative account checks");
    assert!(plain_still(), "a fixed HTTPS address still has no S3 signer");

    let mut newly_covering = connection("review-covering", "review-covering", "https://plain-request.invalid");
    newly_covering.credentials = Some(s3::Credentials {
        access_key_id: "RGSYNTH_review_covering".into(),
        secret_access_key: "RGSYNTH_review_covering_secret".into(),
        session_token: None,
    });
    s3::connect(newly_covering).unwrap();
    assert!(!plain_still(), "absence is authority too: a newly covering signer requires a fresh request");
    assert!(s.download_continues(ids[0]).is_err());
    for name in ["review-owned", "review-unrelated", "review-covering"] {
        s3::disconnect(name);
    }
}
