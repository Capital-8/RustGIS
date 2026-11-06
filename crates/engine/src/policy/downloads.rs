//! Engine-held claims for deferred URL stages. Payload JSON never creates authority.
use super::Continuation;
use crate::{CmdResult, Session, UrlRequest};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(super) struct Claim {
    pub c: Continuation,
    pub params: Value,
    pub live: Arc<AtomicBool>,
    import: bool,
}
impl Drop for Claim {
    /// Revokes queued browser/native predicates when an engine claim is retired.
    fn drop(&mut self) {
        self.live.store(false, Ordering::SeqCst);
    }
}
pub(super) struct Queued {
    c: std::sync::Weak<Continuation>,
    import: bool,
}
#[derive(Default)]
pub(crate) struct Work {
    pub(super) queued: BTreeMap<u64, Queued>,
    pub(super) claims: BTreeMap<u64, Claim>,
}
impl Session {
    /// The current project generation, used to prune adapter-owned remembered operations.
    pub fn project_generation(&self) -> u64 {
        self.opened
    }
    /// Marks queued authority present before a guarded operation, including taken requests.
    pub(crate) fn queued_authority(&self) -> Vec<u64> {
        self.policy.work.queued.keys().copied().collect()
    }
    /// Retires only newly issued queued keys after failure, even if a caller retained its request.
    pub(crate) fn rollback_queued_authority(&mut self, before: &[u64]) {
        self.policy.work.queued.retain(|key, _| before.contains(key));
        self.retire_abandoned_requests();
    }
    /// Captures a handler's authority, or checks the equivalent direct native ingress.
    pub fn request_url(&mut self, p: &Value) -> CmdResult {
        if ["geolibreProject", "geolibreLayer", "geolibreZarr"].iter().any(|k| p.get(k).is_some()) {
            return Err("internal import targets require an engine import request".into());
        }
        if self.policy_has_no_operation() {
            return self.request_url_authorized(p);
        }
        self.enqueue_url(p)
    }
    /// Only the engine import executor supplies reservation flags; JSON never chooses purpose.
    pub(crate) fn request_import_url(&mut self, p: &Value) -> CmdResult {
        if self.policy_has_no_operation() {
            return self.request_import_url_authorized(p);
        }
        self.enqueue_url_for(p, true)
    }
    /// Drops only expired weak queue entries; taken requests own their authority until claimed/dropped.
    pub(crate) fn retire_abandoned_requests(&mut self) {
        self.policy.work.queued.retain(|_, q| q.c.strong_count() > 0);
    }
    /// Deduplicates exact requests within one owner and retains their initiating node.
    pub(super) fn enqueue_url(&mut self, p: &Value) -> CmdResult {
        self.enqueue_url_for(p, false)
    }
    /// Owns the originating continuation in the opaque request and bounds all retained unclaimed requests.
    fn enqueue_url_for(&mut self, p: &Value, import: bool) -> CmdResult {
        let mut c = self.continuation()?;
        self.check_continuation(&c)?;
        // Only the built-in data reader proves this bounded set. Services, imports and
        // plugin-initiated discovery retain the full active storage identity.
        if !import
            && matches!(&c.node.op, super::plan::Op::Command(spec) if spec.id == "layer.addUrl")
            && let Some(urls) = crate::cmd::layer::fixed_download_urls(p)
        {
            let selected = urls
                .iter()
                .map(|url| crate::render::rustgis_tiles::s3::account_identity_for(crate::render::rustgis_tiles::logins::inner(url)))
                .collect::<Result<Vec<_>, _>>()?;
            // A concurrent account change cannot become fresh authority during narrowing.
            self.check_continuation(&c)?;
            c.s3 = Some(selected);
        }
        let c = Arc::new(c);
        self.retire_abandoned_requests();
        let mut params = std::borrow::Cow::Borrowed(p);
        if self.web_logins.is_anonymous() {
            params.to_mut().as_object_mut().ok_or("download parameters must be an object")?.insert("anonymous".into(), json!(true));
        }
        let p = params.as_ref();
        let duplicate = self.url_requests.iter().any(|q| q.params == *p && q.authority.scope.same_caller(&c.scope));
        if !duplicate {
            if self.policy.work.queued.len() >= crate::MAX_URL_REQUESTS {
                return Err(format!("{} downloads are already waiting; try again when they have landed", crate::MAX_URL_REQUESTS));
            }
            self.next_pending = self.next_pending.wrapping_add(1);
            let key = self.next_pending;
            self.policy.work.queued.insert(key, Queued { c: Arc::downgrade(&c), import });
            self.url_requests.push(UrlRequest { key, params: p.clone(), authority: c });
        }
        self.request_only = true;
        Ok(json!({"pending":true,"url":p.get("url")}))
    }
    /// Taking an opaque request sends nothing. Authority is checked again when it is claimed.
    pub fn take_url_requests(&mut self) -> Vec<UrlRequest> {
        let mut live = Vec::new();
        let mut revoked = 0;
        for request in std::mem::take(&mut self.url_requests) {
            let ok = self
                .policy
                .work
                .queued
                .get(&request.key)
                .and_then(|q| q.c.upgrade())
                .ok_or("the request was already claimed")
                .and_then(|c| self.check_continuation_origin(&c).map_err(|_| "this URL request was revoked"));
            if ok.is_ok() {
                live.push(request);
            } else {
                self.policy.work.queued.remove(&request.key);
                revoked += 1;
            }
        }
        match revoked {
            0 => {}
            1 => self.notify("1 deferred URL request was revoked before it started", true),
            n => self.notify(format!("{n} deferred URL requests were revoked before they started"), true),
        }
        live
    }
    /// Claims one engine-issued request immediately before its first I/O.
    pub fn download_started_for(&mut self, request: &UrlRequest) -> Result<u64, String> {
        if self.policy.work.claims.len() >= 64 {
            return Err("64 downloads are already awaiting completion".into());
        }
        let q = self.policy.work.queued.get(&request.key).ok_or("this URL request was already claimed or dropped")?;
        let c = q.c.upgrade().ok_or("this URL request was dropped")?;
        if !Arc::ptr_eq(&request.authority, &c) {
            return Err("this URL request belongs to another session".into());
        }
        self.check_delivery(&c)?;
        let import = q.import;
        self.policy.work.queued.remove(&request.key);
        let c = (*c).clone();
        let id = self.download_started();
        self.policy.work.claims.insert(id, Claim { c, params: request.params.clone(), live: Arc::new(AtomicBool::new(true)), import });
        Ok(id)
    }
    /// Rechecks every new stage, including a probe's fallback and later pages or keys.
    pub fn download_continues(&mut self, id: u64) -> Result<(), String> {
        if !self.download_is_pending(id)
            && !self.downloaded.iter().any(|(n, _)| *n == id)
            && !self.landed_reads.iter().any(|(n, _)| *n == id)
        {
            return Err("this download belongs to a replaced project or was dropped".into());
        }
        if let Some(claim) = self.policy.work.claims.get(&id)
            && let Err(e) = self.check_continuation(&claim.c)
        {
            self.policy.work.claims.remove(&id);
            self.downloading.retain(|n| *n != id);
            self.downloaded.retain(|(n, _)| *n != id);
            self.landed_reads.retain(|(n, _)| *n != id);
            return Err(e);
        }
        Ok(())
    }
    /// Owns a principal-free predicate that survives queues and browser awaits.
    pub fn download_still(&self, id: u64) -> Option<crate::render::rustgis_tiles::cache::Still> {
        let claim = self.policy.work.claims.get(&id)?;
        let (c, live) = (claim.c.clone(), claim.live.clone());
        Some(Arc::new(move || live.load(Ordering::SeqCst) && c.check().is_ok()))
    }
    /// Completes a read-in-place probe under its engine-held owner and exact original address.
    pub fn add_in_place_for(&mut self, id: u64, p: &Value) -> CmdResult {
        let c = self.policy.work.claims.get(&id).ok_or("no claimed URL request with that id")?.c.clone();
        self.check_delivery(&c)?;
        self.download_continues(id)?;
        self.download_params(id, p, true)?;
        let r = self.resume(&c, |s| s.run_url_completion(id, "layer.addUrl", p));
        self.policy.work.claims.remove(&id);
        self.downloading.retain(|n| *n != id);
        r?
    }
    /// Native delivery resumes the claim. Ordinary user-owned downloads retain their ingress.
    pub fn complete_download(&mut self, id: u64, p: &Value) -> CmdResult {
        if let Some(claim) = self.policy.work.claims.get(&id) {
            let c = claim.c.clone();
            if !self.policy_ingress_idle() {
                self.continuation_owner(&c)?;
            }
            let result = self.resume(&c, |s| s.run("layer.addDownload", p));
            // Consume only the owner's claim. A rejected foreign call never reaches this path.
            self.policy.work.claims.remove(&id);
            result?
        } else {
            self.run("layer.addDownload", p)
        }
    }
    /// Reconstructs the frozen effective request, allowing only documented completion refinements.
    pub(super) fn download_params(&self, id: u64, p: &Value, in_place: bool) -> Result<Value, String> {
        let claim = self.policy.work.claims.get(&id).ok_or("no claimed download with that id")?;
        self.check_continuation(&claim.c)?;
        let given = p.as_object().ok_or("download parameters must be an object")?;
        let mut params = claim.params.as_object().cloned().ok_or("the stored download parameters are not an object")?;
        for (key, value) in given {
            if ["geolibreProject", "geolibreLayer", "geolibreZarr"].contains(&key.as_str()) && !claim.import {
                return Err("a simple URL completion cannot import a project's internal targets".into());
            }
            let refinement = (key == "id" && !in_place && value.as_u64() == Some(id))
                || (key == "cog" && in_place && value.as_bool() == Some(true))
                || (key == "whole" && value.as_bool() == Some(true))
                || (key == "zoom" && value.as_bool() == Some(false) && claim.params.get(key).and_then(Value::as_bool) == Some(true));
            if !refinement && claim.params.get(key) != Some(value) {
                return Err("the download's address or parameters changed since it was requested".into());
            }
            params.insert(key.clone(), value.clone());
        }
        params.insert("id".into(), json!(id));
        Ok(Value::Object(params))
    }
    /// Internal import flags are accepted only while applying that engine-issued claim.
    pub(crate) fn import_completion(&self, p: &Value) -> bool {
        p.get("id")
            .and_then(Value::as_u64)
            .and_then(|id| self.policy.work.claims.get(&id))
            .is_some_and(|claim| claim.import && self.check_delivery(&claim.c).is_ok())
    }
    /// Borrows stored completion authority; payload fields cannot manufacture one.
    pub(super) fn claim_for(&self, id: u64) -> Option<Continuation> {
        self.policy.work.claims.get(&id).map(|c| c.c.clone())
    }
    /// Moves claims with the project state so transactional replacement can roll back.
    pub(crate) fn take_policy_work(&mut self) -> Work {
        std::mem::take(&mut self.policy.work)
    }
    /// Restores the original project's still-owned claims after rollback.
    pub(crate) fn restore_policy_work(&mut self, work: Work) {
        self.policy.work = work;
    }
    /// Retires a failed or consumed claim and invalidates every captured predicate.
    pub(crate) fn forget_download_claim(&mut self, id: u64) {
        self.policy.work.claims.remove(&id);
    }
}
