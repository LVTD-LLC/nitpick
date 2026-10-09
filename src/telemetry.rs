//! Deliberately allowlisted telemetry. Never accepts code, paths, prompts, responses or error text.
//! Events + OTLP/HTTP JSON share a bounded, best-effort flush; telemetry cannot fail a review.
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

static STATE: OnceLock<Option<State>> = OnceLock::new();
const HOST: &str = "https://us.i.posthog.com";
const LIMIT: usize = 256;
struct State {
    token: String,
    id: String,
    command: &'static str,
    queue: Mutex<Queue>,
}
#[derive(Default)]
struct Queue {
    events: Vec<Value>,
    spans: Vec<Value>,
    logs: Vec<Value>,
}

pub fn disabled(value: Option<&str>, do_not_track: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"))
        || do_not_track == Some("1")
}

fn installation_id(dir: &Path) -> Option<String> {
    let path = dir.join("installation-id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        return Uuid::parse_str(id.trim()).ok().map(|id| id.to_string());
    }
    std::fs::create_dir_all(dir).ok()?;
    let id = Uuid::new_v4().to_string();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut f) => {
            f.write_all(id.as_bytes()).ok()?;
            Some(id)
        }
        // Another invocation won the race. Never overwrite its identity.
        Err(_) => None,
    }
}

pub fn init(command: &'static str) {
    STATE.get_or_init(|| {
        if cfg!(test)
            || disabled(
                std::env::var("NITPICK_TELEMETRY").ok().as_deref(),
                std::env::var("DO_NOT_TRACK").ok().as_deref(),
            )
        {
            return None;
        }
        // Official builds supply the public, write-only project token at compile time.
        // Source builds without it are telemetry-free; never use the caller's generic POSTHOG_API_KEY.
        let token = option_env!("NITPICK_POSTHOG_PROJECT_TOKEN")?.to_owned();
        if !token.starts_with("phc_") {
            return None;
        }
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state")))?;
        let id = installation_id(&base.join("nitpick/telemetry"))?;
        Some(State { token, id, command, queue: Mutex::new(Queue::default()) })
    });
}

fn state() -> Option<&'static State> {
    STATE.get().and_then(Option::as_ref)
}
fn nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}
fn attributes(value: &Value) -> Vec<Value> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, v)| {
            let v = match v {
                Value::String(s) => json!({"stringValue": s}),
                Value::Bool(b) => json!({"boolValue": b}),
                Value::Number(n) => json!({"doubleValue": n.as_f64()}),
                _ => return None,
            };
            Some(json!({"key": key, "value": v}))
        })
        .collect()
}
fn iso_timestamp(ns: u128) -> String {
    chrono::DateTime::from_timestamp((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as u32)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn record(name: &'static str, mut properties: Value, span: Option<Value>) {
    let Some(s) = state() else { return };
    properties["distinct_id"] = json!(format!("cli:{}", s.id));
    properties["source"] = json!("cli");
    properties["command"] = json!(s.command);
    properties["cli_version"] = json!(env!("CARGO_PKG_VERSION"));
    properties["os"] = json!(std::env::consts::OS);
    properties["arch"] = json!(std::env::consts::ARCH);
    properties["$lib"] = json!("nitpick-rust");
    properties["$process_person_profile"] = json!(false);
    properties["$geoip_disable"] = json!(true);
    let Ok(mut q) = s.queue.lock() else { return };
    if q.events.len() >= LIMIT {
        return;
    }
    let timestamp = span
        .as_ref()
        .and_then(|s| s["startTimeUnixNano"].as_str())
        .and_then(|t| t.parse::<u128>().ok())
        .unwrap_or_else(nanos);
    q.events.push(json!({"event":name,"uuid":Uuid::new_v4().to_string(),"timestamp":iso_timestamp(timestamp),"properties":properties}));
    let error = properties["$ai_is_error"] == true || name == "$exception";
    let mut log = json!({"timeUnixNano":nanos().to_string(),"severityNumber":if error {17} else {9},
        "severityText":if error {"ERROR"} else {"INFO"},"body":{"stringValue":name},"attributes":attributes(&properties)});
    if let Some(attrs) = log["attributes"].as_array_mut() {
        attrs.push(json!({"key":"posthogDistinctId","value":{"stringValue":format!("cli:{}",s.id)}}));
    }
    if let Some(mut span) = span {
        if let Some(attrs) = span["attributes"].as_array_mut() {
            attrs.push(json!({"key":"distinct_id","value":{"stringValue":format!("cli:{}",s.id)}}));
        }
        log["traceId"] = span["traceId"].clone();
        log["spanId"] = span["spanId"].clone();
        q.spans.push(span);
    }
    q.logs.push(log);
}

/// Finite public model labels: custom/local names can contain private paths or project names.
pub fn model_label(model: &str) -> &'static str {
    match model {
        "stealth/space-bunny-alpha" => "stealth/space-bunny-alpha",
        "nvidia/nemotron-3-ultra-550b-a55b:free" => "nvidia/nemotron-3-ultra-550b-a55b:free",
        "nvidia/nemotron-3-super-120b-a12b:free" => "nvidia/nemotron-3-super-120b-a12b:free",
        "anthropic/claude-sonnet-4.6" => "anthropic/claude-sonnet-4.6",
        "anthropic/claude-opus-4.6" => "anthropic/claude-opus-4.6",
        "anthropic/claude-sonnet-4.5" => "anthropic/claude-sonnet-4.5",
        "openai/gpt-5.4" => "openai/gpt-5.4",
        "openai/gpt-5-mini" => "openai/gpt-5-mini",
        "google/gemini-3.1-pro-preview" => "google/gemini-3.1-pro-preview",
        "google/gemini-3-flash-preview" => "google/gemini-3-flash-preview",
        "gpt-5.4" => "gpt-5.4",
        "gpt-5-mini" => "gpt-5-mini",
        _ => "other",
    }
}

pub struct Span {
    trace: String,
    id: String,
    parent: Option<String>,
    name: &'static str,
    time: u128,
    started: Instant,
}
impl Span {
    pub fn root(name: &'static str) -> Self {
        Self {
            trace: Uuid::new_v4().simple().to_string(),
            id: Uuid::new_v4().simple().to_string()[..16].to_owned(),
            parent: None,
            name,
            time: nanos(),
            started: Instant::now(),
        }
    }
    pub fn child(&self, name: &'static str) -> Self {
        let mut child = Self::root(name);
        child.trace = self.trace.clone();
        child.parent = Some(self.id.clone());
        child
    }
    pub fn finish(self, event: &'static str, mut props: Value, error: bool) {
        props["$ai_trace_id"] = json!(self.trace);
        props["$ai_session_id"] = Value::Null;
        props["$ai_span_id"] = json!(self.id);
        props["$ai_span_name"] = json!(self.name);
        if let Some(parent) = &self.parent {
            props["$ai_parent_id"] = json!(parent);
        }
        props["$ai_latency"] = json!(self.started.elapsed().as_secs_f64());
        props["$ai_is_error"] = json!(error);
        let mut span = json!({"traceId":self.trace,"spanId":self.id,"name":self.name,"kind":1,
            "startTimeUnixNano":self.time.to_string(),"endTimeUnixNano":nanos().to_string(),
            "attributes":attributes(&props),"status":{"code":if error {2} else {1}}});
        if let Some(parent) = self.parent {
            span["parentSpanId"] = json!(parent);
        }
        record(event, props, Some(span));
    }
}

pub fn exception(category: &'static str) {
    record(
        "$exception",
        json!({"$exception_list":[{"type":"NitpickError","value":category,
        "mechanism":{"type":"generic","handled":true,"synthetic":true}}],
        "$exception_fingerprint":format!("nitpick:{category}"),"$exception_level":"error"}),
        None,
    );
}
pub fn finish_command(started: Instant, exit: i32) {
    record("cli_command_completed", json!({"exit_code":exit,"duration_ms":started.elapsed().as_millis() as u64}), None);
    if exit == 2 {
        exception("command_failed");
    }
    flush();
}

pub fn flush_worker() {
    if state().is_some_and(|s| s.command == "watch_worker") {
        flush();
    }
}

pub fn flush() {
    // Never nest a runtime, even if a future caller exports from async code.
    if tokio::runtime::Handle::try_current().is_ok() {
        return;
    }
    let Some(s) = state() else { return };
    let q = {
        let Ok(mut q) = s.queue.lock() else { return };
        std::mem::take(&mut *q)
    };
    if q.events.is_empty() {
        return;
    }
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
    rt.block_on(async {
        let Ok(client) = reqwest::Client::builder().connect_timeout(Duration::from_millis(1000))
            .timeout(Duration::from_millis(1400)).redirect(reqwest::redirect::Policy::none()).build() else { return };
        let resource = json!({"attributes":attributes(&json!({"service.name":"nitpick-cli","service.version":env!("CARGO_PKG_VERSION")}))});
        let scope = json!({"name":"nitpick","version":env!("CARGO_PKG_VERSION")});
        let requests = [
            ("/batch/", json!({"api_key":s.token,"batch":q.events})),
            ("/i/v1/logs",json!({"resourceLogs":[{"resource":resource,"scopeLogs":[{"scope":scope,"logRecords":q.logs}]}]})),
            ("/i/v1/traces",json!({"resourceSpans":[{"resource":resource,"scopeSpans":[{"scope":scope,"spans":q.spans}]}]})),
        ];
        let send = futures::future::join_all(requests.into_iter().map(|(path,body)| {
            let client = &client;
            async move { let result = client.post(format!("{HOST}{path}")).bearer_auth(&s.token).json(&body).send().await;
                if std::env::var("NITPICK_TELEMETRY_DEBUG").as_deref() == Ok("1") {
                    match result {
                        Ok(response) => eprintln!("nitpick telemetry: {path} HTTP {}", response.status().as_u16()),
                        Err(error) => eprintln!("nitpick telemetry: {path} {}", if error.is_timeout() { "timeout" } else if error.is_connect() { "connection_failed" } else { "export_failed" }),
                    }
                } }
        }));
        let _ = tokio::time::timeout(Duration::from_millis(1500),send).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opt_out_is_explicit_and_respects_do_not_track() {
        for v in ["0", "off", "OFF", "false", "no"] {
            assert!(disabled(Some(v), None));
        }
        assert!(disabled(None, Some("1")));
        assert!(!disabled(None, None));
        assert!(!disabled(Some("1"), None));
    }
    #[test]
    fn identity_is_random_stable_and_does_not_overwrite_corrupt_data() {
        let d = tempfile::tempdir().unwrap();
        let first = installation_id(d.path()).unwrap();
        assert_eq!(Some(first.clone()), installation_id(d.path()));
        assert!(Uuid::parse_str(&first).is_ok());
        std::fs::write(d.path().join("installation-id"), "private-invalid-value").unwrap();
        assert!(installation_id(d.path()).is_none());
    }
    #[test]
    fn untrusted_model_identifiers_never_leave_the_machine() {
        for name in ["/home/alice/private/model", "sk-secret", "anthropic/private-customer-model", "user@example.com"] {
            assert_eq!(model_label(name), "other");
        }
        assert_eq!(model_label("openai/gpt-5.4"), "openai/gpt-5.4");
    }
    #[test]
    fn event_time_is_utc_and_preserves_milliseconds() {
        assert_eq!(iso_timestamp(1_700_000_000_123_000_000), "2023-11-14T22:13:20.123Z");
    }
    #[test]
    fn spans_share_trace_and_have_unique_ids() {
        let root = Span::root("review");
        let child = root.child("generation");
        assert_eq!(root.trace, child.trace);
        assert_eq!(child.parent.as_ref(), Some(&root.id));
        assert_ne!(root.id, child.id);
        assert_eq!(root.trace.len(), 32);
        assert_eq!(root.id.len(), 16);
    }
}
