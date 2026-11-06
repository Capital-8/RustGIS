//! `host.fetch`: a plugin with the `network` permission reads a small text or JSON document
//! from a web address (an API's answer, a service's index).
//!
//! The address must be `http(s)://` and on the internet: never this computer, the local network
//! or a forbidden address (see `rustgis_tiles::address`), and no redirect may lead there. It is
//! read anonymously (never signed with an S3 connection's credentials), at most
//! [`MAX_BYTES`], and must be UTF-8. Answers are kept for a while ([`TTL`]) in a small cache,
//! so a plugin walking the same chain of documents again doesn't fetch them again.
//!
//! Natively the request waits for the answer, at most [`TIMEOUT`] each and [`CALL_BUDGET`] for
//! all of one plugin call's fetches. In the browser nothing may wait: the fetch starts, the
//! request answers `{"pending": true, "url"}`, and once every pending answer has landed the
//! plugin command runs again from the start ([`Session::poll_plugin_retries`]), when the cache
//! answers at once.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::{Value, json};
use web_time::{Duration, Instant};

use crate::render::rustgis_tiles::{address, s3};
use crate::{CmdResult, Session};

/// The most bytes a fetched document may have.
pub const MAX_BYTES: usize = 2 << 20;
/// How long one fetch may take (natively).
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// How long all the fetches of one plugin call may take together (natively): the app waits.
pub const CALL_BUDGET: Duration = Duration::from_secs(60);
/// The most fetches one plugin call may make (cached answers included).
pub const MAX_PER_CALL: usize = 64;
/// How long a fetched answer is kept.
pub const TTL: Duration = Duration::from_secs(10 * 60);
/// How long a failed fetch's error is kept: long enough for every command waiting for it to
/// see it (the browser), short enough that a later try fetches again.
pub const ERROR_TTL: Duration = Duration::from_secs(30);
/// The most answers kept, and their bytes.
const CACHE_ENTRIES: usize = 256;
const CACHE_BYTES: usize = 16 << 20;
/// The browser: the most fetches one call may leave pending, and how many times a command is
/// run again for its fetches (a chain of documents, one a run) before it is given up.
pub const MAX_PENDING: usize = 8;
pub const MAX_RETRIES: u32 = 64;
/// The browser: how long a fetch is waited for before it is taken as lost (a request that
/// never answers), so the commands waiting for it run again and it can be fetched again.
const IN_FLIGHT_FOR: Duration = Duration::from_secs(2 * 60);
/// Plugin commands waiting for their fetches.
const MAX_WAITING: usize = 8;
/// The longest address accepted.
const MAX_URL: usize = 8 * 1024;

struct Entry {
    url: String,
    at: Instant,
    size: usize,
    answer: Result<Value, String>,
}

#[derive(Default)]
struct Cache {
    entries: VecDeque<Entry>,
    /// Addresses being fetched off the frame (the browser), and since when.
    in_flight: Vec<(String, Instant)>,
}

impl Cache {
    /// The kept answer for `url`. A failed fetch's error is kept for [`ERROR_TTL`], so every
    /// command waiting for it sees it, and fetched again after that.
    fn get(&mut self, url: &str) -> Option<Result<Value, String>> {
        self.entries.retain(|e| e.at.elapsed() < if e.answer.is_ok() { TTL } else { ERROR_TTL });
        let i = self.entries.iter().position(|e| e.url == url)?;
        let e = self.entries.remove(i)?;
        let answer = e.answer.clone();
        // Kept answers move to the back: the least recently used go first.
        self.entries.push_back(e);
        Some(answer)
    }

    fn keep(&mut self, url: &str, answer: Result<Value, String>) {
        // About the bytes kept: an answer holds its text twice, as `text` and parsed as `json`.
        let size = url.len() + answer.as_ref().map_or_else(String::len, |v| v.get("text").and_then(Value::as_str).map_or(0, str::len) * 2);
        self.in_flight.retain(|(u, _)| u != url);
        self.entries.retain(|e| e.url != url);
        self.entries.push_back(Entry { url: url.to_string(), at: Instant::now(), size, answer });
        let mut bytes: usize = self.entries.iter().map(|e| e.size).sum();
        while self.entries.len() > CACHE_ENTRIES || bytes > CACHE_BYTES {
            let Some(e) = self.entries.pop_front() else { break };
            bytes = bytes.saturating_sub(e.size);
        }
    }

    /// Whether `url` is no longer being fetched, so a command waiting for it can run again. Its
    /// answer is usually kept by then; one already dropped (evicted, or an error expired while
    /// frames were throttled) is fetched again by the run, so a waiting command never sticks.
    /// Nor does one whose fetch never answers: after [`IN_FLIGHT_FOR`] it counts as landed.
    fn landed(&self, url: &str) -> bool {
        !self.in_flight.iter().any(|(u, at)| u == url && at.elapsed() < IN_FLIGHT_FOR)
    }
}

fn cache() -> std::sync::MutexGuard<'static, Cache> {
    static CACHE: Mutex<Cache> = Mutex::new(Cache { entries: VecDeque::new(), in_flight: Vec::new() });
    // A poisoned lock still holds a usable cache: at worst an entry is missing.
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
thread_local! {
    /// Tests: let the plugin fetch from this computer (a test server) and, when `wait` is off,
    /// fetch as the browser does (pending, then run again).
    pub(crate) static TEST: std::cell::Cell<(bool, bool)> = const { std::cell::Cell::new((false, true)) };
}

fn allow_local() -> bool {
    #[cfg(test)]
    {
        TEST.with(|t| t.get().0)
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// Whether a fetch waits for its answer (natively) or answers pending (the browser).
fn waits() -> bool {
    #[cfg(test)]
    {
        TEST.with(|t| t.get().1)
    }
    #[cfg(not(test))]
    {
        cfg!(not(target_arch = "wasm32"))
    }
}

/// The address a plugin may fetch, as fetched: `http(s)://`, on the internet, no fragment.
pub fn checked(url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("url is required".into());
    }
    if url.len() > MAX_URL {
        return Err(format!("the address is longer than {MAX_URL} characters"));
    }
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        let shown: String = url.chars().take(80).collect();
        return Err(format!("{shown}: plugins fetch only http:// and https:// addresses"));
    }
    // A fragment never reaches the server (and RustGIS marks byte ranges with one).
    let url = url.split('#').next().unwrap_or(url);
    address::check(url, None)?;
    if address::reach(url) != address::Reach::Public && !allow_local() {
        return Err(format!("{url}: plugins fetch only from the internet, not this computer or the local network"));
    }
    Ok(url.to_string())
}

/// The answer for a fetched document: `{url, text}`, and `json` when the text is a JSON object
/// or array.
pub fn answer(url: &str, bytes: &[u8]) -> Result<Value, String> {
    if bytes.len() > MAX_BYTES {
        return Err(format!("{url}: larger than {MAX_BYTES} bytes"));
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let text = std::str::from_utf8(bytes).map_err(|_| format!("{url}: the answer isn't UTF-8 text"))?;
    let trimmed = text.trim_start();
    let parsed = if trimmed.starts_with('{') || trimmed.starts_with('[') { serde_json::from_str::<Value>(text).ok() } else { None };
    Ok(match parsed {
        Some(v) => json!({"url": url, "text": text, "json": v}),
        None => json!({"url": url, "text": text}),
    })
}

/// The cache's key for `plugin`'s fetch of `url`: each plugin has answers of its own, so one
/// can't read what another fetched (an address may carry a token).
fn key(plugin: &str, url: &str) -> String {
    format!("{plugin} {url}")
}

/// Starts fetching `url` off the frame under `key`, unless it is already under way.
fn start(key: &str, url: &str) {
    {
        let mut c = cache();
        if !c.landed(key) {
            return;
        }
        // A fetch taken as lost is started again.
        c.in_flight.retain(|(u, _)| u != key);
        c.in_flight.push((key.to_string(), Instant::now()));
    }
    let (key, url) = (key.to_string(), url.to_string());
    crate::render::rustgis_tiles::cache::download_limited(
        &s3::anonymous(&url),
        MAX_BYTES,
        Box::new(move |r| {
            let answer = r.map_err(|e| format!("{url}: {e}")).and_then(|bytes| answer(&url, &bytes));
            cache().keep(&key, answer);
        }),
    );
}

/// What one plugin call has fetched.
#[derive(Default)]
pub(super) struct Calls {
    /// The plugin fetching: the cache keeps each plugin's answers apart.
    plugin: String,
    count: usize,
    started: Option<Instant>,
    /// Cache keys of the answers pending (the browser).
    pub(super) pending: Vec<String>,
}

impl Calls {
    pub(super) fn for_plugin(plugin: &str) -> Calls {
        Calls { plugin: plugin.to_string(), ..Calls::default() }
    }

    /// Runs `host.fetch {url}` for a plugin that may.
    pub(super) fn fetch(&mut self, p: &Value) -> CmdResult {
        let url = checked(p.get("url").and_then(Value::as_str).ok_or("url is required")?)?;
        if self.count >= MAX_PER_CALL {
            return Err(format!("a plugin call may fetch at most {MAX_PER_CALL} documents"));
        }
        self.count += 1;
        let key = key(&self.plugin, &url);
        let kept = cache().get(&key);
        if let Some(answer) = kept {
            return answer;
        }
        if !waits() {
            if !self.pending.contains(&key) {
                if self.pending.len() >= MAX_PENDING {
                    return Err(format!("a plugin call may wait for at most {MAX_PENDING} documents at once"));
                }
                self.pending.push(key.clone());
                start(&key, &url);
            }
            return Ok(json!({"pending": true, "url": url}));
        }
        self.blocking(&key, &url)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn blocking(&mut self, key: &str, url: &str) -> CmdResult {
        let started = *self.started.get_or_insert_with(Instant::now);
        let left = CALL_BUDGET.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Err(format!("{url}: this plugin call's fetches took longer than {} s in all", CALL_BUDGET.as_secs()));
        }
        // Names are checked again where they resolve: a public-looking name for a local address
        // (or a redirect to one) is refused there. Tests fetch from a server on this computer.
        let (fetched, within) = (&s3::anonymous(url), TIMEOUT.min(left));
        let bytes = if allow_local() {
            crate::render::rustgis_tiles::cache::download_blocking_within(fetched, MAX_BYTES, within)
        } else {
            crate::render::rustgis_tiles::cache::download_public_within(fetched, MAX_BYTES, within)
        };
        // Errors are kept too (for `ERROR_TTL`), so a plugin run again and again on an address
        // that doesn't answer doesn't wait for it each time.
        let answer = bytes.map_err(|e| format!("{url}: {e}")).and_then(|bytes| answer(url, &bytes));
        cache().keep(key, answer.clone());
        answer
    }

    #[cfg(target_arch = "wasm32")]
    fn blocking(&mut self, _key: &str, url: &str) -> CmdResult {
        let _ = self.started;
        Err(format!("{url}: the web app can't wait for a fetch"))
    }
}

/// A plugin command waiting for its fetches before it runs again (the browser).
pub(crate) struct Retry {
    continuation: crate::policy::Continuation,
    id: String,
    params: Value,
    urls: Vec<String>,
    tries: u32,
}

impl Session {
    /// Keeps plugin command `id` to run again once `urls` (cache keys) have landed: its answer,
    /// which says so.
    pub(super) fn retry_plugin(&mut self, id: &str, params: &Value, urls: Vec<String>) -> CmdResult {
        let tries = self.retry_tries;
        if tries >= MAX_RETRIES {
            return Err(format!("{id}: gave up after fetching {MAX_RETRIES} times"));
        }
        let continuation = self.continuation()?;
        // One waiting run per command: the latest call's, which replaces an earlier one.
        if let Some(i) = self.plugin_retries.iter().position(|r| r.id == id) {
            self.plugin_retries.remove(i);
            self.notify(format!("{id}: an earlier run still waiting for the web was replaced by this one"), false);
        }
        if self.plugin_retries.len() >= MAX_WAITING {
            let dropped = self.plugin_retries.remove(0);
            self.notify(format!("{}: dropped while waiting for the web (too many plugin commands waiting)", dropped.id), true);
        }
        // `urls` are cache keys (`<plugin> <url>`; plugin ids have no spaces): the answer names
        // the addresses.
        let fetching: Vec<&str> = urls.iter().map(|k| k.split_once(' ').map_or(k.as_str(), |(_, u)| u)).collect();
        let answer = json!({"pending": true, "fetching": fetching});
        self.plugin_retries.push(Retry { id: id.to_string(), params: params.clone(), urls, tries: tries + 1, continuation });
        Ok(answer)
    }

    /// Whether plugin commands are waiting for their fetches, so the front end keeps polling.
    pub fn plugin_retries_waiting(&self) -> bool {
        !self.plugin_retries.is_empty()
    }

    /// Runs again the plugin commands whose fetches have all landed (the browser: see
    /// `host.fetch` in `docs/plugins.md`): each command and what it answered this time. Errors
    /// are also given as notices, as for any command.
    pub fn poll_plugin_retries(&mut self) -> Vec<(String, CmdResult)> {
        if self.plugin_retries.is_empty() {
            return Vec::new();
        }
        let (ready, waiting): (Vec<Retry>, Vec<Retry>) = std::mem::take(&mut self.plugin_retries).into_iter().partition(|r| {
            let c = cache();
            r.urls.iter().all(|u| c.landed(u))
        });
        self.plugin_retries = waiting;
        ready
            .into_iter()
            .map(|r| {
                self.retry_tries = r.tries;
                let res = self.retry_continuation(&r.continuation, &r.params);
                self.retry_tries = 0;
                (r.id, res)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_addresses() {
        assert_eq!(checked(" https://example.com/a?b=1#frag ").unwrap(), "https://example.com/a?b=1");
        for bad in [
            "",
            "file:///etc/passwd",
            "s3://bucket/key",
            "ftp://example.com/x",
            "rustgis-anonymous:https://example.com/",
            "http://localhost:8080/",
            "http://127.0.0.1/",
            "http://10.0.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://example.com/a b",
        ] {
            assert!(checked(bad).is_err(), "{bad}");
        }
        assert!(checked(&format!("https://example.com/{}", "a".repeat(MAX_URL))).is_err());
    }

    #[test]
    fn answers_text_and_json() {
        let a = answer("https://x.org/t", br#"{"select": [49059], "valid": true}"#).unwrap();
        assert_eq!(a["json"]["select"][0], 49059);
        assert_eq!(a["url"], "https://x.org/t");
        let a = answer("https://x.org/t", b"\xEF\xBB\xBFplain").unwrap();
        assert_eq!(a["text"], "plain");
        assert!(a.get("json").is_none());
        assert!(answer("https://x.org/t", b"{broken").unwrap().get("json").is_none());
        assert!(answer("https://x.org/t", b"\xff\xfe").unwrap_err().contains("UTF-8"));
        assert!(answer("https://x.org/t", &vec![b' '; MAX_BYTES + 1]).unwrap_err().contains("larger"));
    }

    #[test]
    fn each_plugin_has_answers_of_its_own() {
        let mut c = Cache::default();
        let url = "https://cache.test/t?token=1";
        c.keep(&key("one", url), Ok(json!({"text": "secret"})));
        assert!(c.get(&key("one", url)).is_some());
        assert!(c.get(&key("two", url)).is_none(), "another plugin's answer");
    }

    #[test]
    fn the_cache_is_bounded_and_keeps_errors_briefly() {
        let mut c = Cache::default();
        for i in 0..CACHE_ENTRIES + 10 {
            c.keep(&format!("https://cache.test/{i}"), Ok(json!({"text": ""})));
        }
        assert!(c.get("https://cache.test/0").is_none(), "the oldest went first");
        assert!(c.get(&format!("https://cache.test/{}", CACHE_ENTRIES + 9)).is_some());
        assert!(c.entries.len() <= CACHE_ENTRIES);
        c.in_flight.push(("https://cache.test/err".into(), Instant::now()));
        assert!(!c.landed("https://cache.test/err"));
        // A fetch that never answers is taken as lost after a while.
        if let Some(lost) = Instant::now().checked_sub(IN_FLIGHT_FOR) {
            c.in_flight.push(("https://cache.test/lost".into(), lost));
            assert!(c.landed("https://cache.test/lost"));
        }
        c.keep("https://cache.test/err", Err("HTTP 500".into()));
        assert!(c.landed("https://cache.test/err"));
        // An address no longer fetched counts as landed even once its answer is gone: the
        // command waiting for it runs again (and fetches it again) instead of waiting forever.
        assert!(c.landed("https://cache.test/never-kept"));
        // Every command waiting for it sees the error, until it expires.
        assert_eq!(c.get("https://cache.test/err"), Some(Err("HTTP 500".into())));
        assert_eq!(c.get("https://cache.test/err"), Some(Err("HTTP 500".into())));
        if let Some(e) = c.entries.iter_mut().find(|e| e.url == "https://cache.test/err") {
            e.at = e.at.checked_sub(ERROR_TTL).unwrap_or(e.at);
        }
        assert_eq!(c.get("https://cache.test/err"), None);
        // Large answers count by their bytes.
        let big = "x".repeat(MAX_BYTES / 2);
        for i in 0..40 {
            c.keep(&format!("https://cache.test/big{i}"), Ok(json!({"text": big})));
        }
        assert!(c.entries.iter().map(|e| e.size).sum::<usize>() <= CACHE_BYTES);
        assert!(c.get("https://cache.test/big39").is_some());
    }
}
