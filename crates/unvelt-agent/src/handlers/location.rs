//! Where-am-I context, with no GPS: Wi-Fi network plus attached-monitor count.
//!
//! The office network and the docked two-monitor setup are strong home/office
//! proxies, and neither needs a location permission. Emits only when the
//! (ssid, monitors, net) triple changes.
//!
//! `net` is the gateway MAC and it is here because the SSID is not reliable any
//! more: modern macOS returns "<redacted>" without Location permission. The
//! gateway MAC is just as stable a per-network id and nothing gates it.

use super::{Handler, Tick};
use crate::config::Config;
use crate::envelope::Event;

type Key = (Option<String>, Option<i32>, Option<String>);

pub struct LocationHandler {
    interval: f64,
    last: Option<Key>,
}

impl LocationHandler {
    pub fn new(cfg: &Config) -> Self {
        LocationHandler {
            interval: cfg.context_sec as f64,
            last: None,
        }
    }
}

impl Handler for LocationHandler {
    fn name(&self) -> &'static str {
        "location"
    }

    fn interval(&self) -> f64 {
        self.interval
    }

    fn poll(&mut self, tick: &mut Tick) -> Vec<Event> {
        let ssid = tick.backend.ssid();
        let mons = tick.backend.monitors();
        let net = tick.backend.net();
        let key: Key = (ssid.clone(), mons, net.clone());
        if self.last.as_ref() == Some(&key) {
            return Vec::new();
        }
        self.last = Some(key);

        let mut p = serde_json::Map::new();
        if let Some(v) = ssid {
            p.insert("ssid".into(), serde_json::json!(v));
        }
        if let Some(v) = mons {
            p.insert("monitors".into(), serde_json::json!(v));
        }
        if let Some(v) = net {
            p.insert("net".into(), serde_json::json!(v));
        }
        if p.is_empty() {
            // Every probe came back None. Emitting an empty payload would say
            // "the context changed to nothing", which is not what happened.
            return Vec::new();
        }
        vec![tick.event(
            "desktop",
            "context",
            tick.now,
            format!("dt:{}:ctx:{}", tick.cfg.did, tick.now),
            Some(serde_json::Value::Object(p)),
        )]
    }
}
