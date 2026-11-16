//! The capture tool's event log: timestamped JSON lines, kept in memory and echoed to stderr.

use std::sync::Mutex;
use std::time::Instant;

use crate::sanitize::Sanitizer;
use crate::text::json_str;

/// A field value of an event line.
#[derive(Clone)]
pub enum Val {
    /// A string (sanitised when the log sanitises).
    Str(String),
    /// An integer.
    Num(i64),
    /// A boolean.
    Bool(bool),
    /// JSON `null`.
    Null,
}

struct Event {
    t_ms: u128,
    kind: String,
    fields: Vec<(String, Val)>,
}

impl Event {
    /// `{"t_ms":…,"kind":…,<fields>}`; `offsets` rewrites the `offset` of `screen_reset`.
    fn line(&self, offsets: Option<&dyn Fn(usize) -> usize>) -> String {
        let mut line = format!("{{\"t_ms\":{},\"kind\":{}", self.t_ms, json_str(&self.kind));
        for (key, val) in &self.fields {
            line.push(',');
            line.push_str(&json_str(key));
            line.push(':');
            match (val, offsets) {
                (Val::Num(n), Some(map)) if self.kind == "screen_reset" && key == "offset" => {
                    let mapped = usize::try_from(*n)
                        .map_or(*n, |o| i64::try_from(map(o)).unwrap_or(i64::MAX));
                    line.push_str(&mapped.to_string());
                }
                (Val::Str(s), _) => line.push_str(&json_str(s)),
                (Val::Num(n), _) => line.push_str(&n.to_string()),
                (Val::Bool(b), _) => line.push_str(if *b { "true" } else { "false" }),
                (Val::Null, _) => line.push_str("null"),
            }
        }
        line.push('}');
        line
    }
}

/// Timestamped JSON events, kept in memory and echoed to stderr unless quiet.
pub struct EventLog {
    start: Instant,
    quiet: bool,
    sanitizer: Option<Sanitizer>,
    events: Mutex<Vec<Event>>,
}

impl EventLog {
    /// A log whose `t_ms` counts from now.
    pub fn new(quiet: bool, sanitizer: Option<Sanitizer>) -> Self {
        Self {
            start: Instant::now(),
            quiet,
            sanitizer,
            events: Mutex::new(Vec::new()),
        }
    }

    /// The instant `t_ms` counts from.
    pub fn start(&self) -> Instant {
        self.start
    }

    /// `s` sanitised the way `emit` sanitises string values (unchanged without `--sanitize`);
    /// for callers that cut a value, so the cut cannot split a name.
    pub fn clean(&self, s: &str) -> String {
        self.sanitizer
            .as_ref()
            .map_or_else(|| s.to_string(), |san| san.apply_str(s))
    }

    /// Records one event: `{"t_ms":…,"kind":…,<fields>}`; string values are sanitised here.
    pub fn emit(&self, kind: &str, fields: &[(&str, Val)]) {
        let fields = fields
            .iter()
            .map(|(key, val)| {
                let val = match val {
                    Val::Str(s) => Val::Str(self.clean(s)),
                    val => val.clone(),
                };
                ((*key).to_string(), val)
            })
            .collect();
        let event = Event {
            t_ms: self.start.elapsed().as_millis(),
            kind: kind.to_string(),
            fields,
        };
        if !self.quiet {
            eprintln!("{}", event.line(None));
        }
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }

    /// Every line so far. `offsets` maps `screen_reset` offsets (raw stream bytes) into the
    /// stored stream when that differs from the raw one.
    pub fn lines(&self, offsets: Option<&dyn Fn(usize) -> usize>) -> Vec<String> {
        let events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        events.iter().map(|e| e.line(offsets)).collect()
    }
}
