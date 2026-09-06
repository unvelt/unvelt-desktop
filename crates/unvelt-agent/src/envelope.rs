//! The event envelope — the shape the phone app and the Python collector
//! already send (`docs/event-envelope.md`).
//!
//! Two properties matter more than anything else here and both are load-bearing:
//!
//! 1. **Envelope field order and compactness.** The struct field order below IS
//!    the wire order; do not sort it. Note the limit of that claim: the
//!    envelope's own keys are in declaration order, but `p` is a
//!    `serde_json::Value`, whose object is a BTreeMap, so payload keys come out
//!    alphabetically where Python emits them in insertion order — `{"app",
//!    "cat", "title"}` against `{"app", "title", "cat"}`. Nothing depends on
//!    that: ingest stores payloads as `jsonb`, which normalises key order on
//!    write and discards both orderings anyway, and the parity harness compares
//!    distributions rather than bytes. Left alone rather than pinned with
//!    serde_json's `preserve_order` feature, which would add indexmap to buy
//!    a property no reader has.
//! 2. **`eid` is deterministic.** A retried batch must collide with the
//!    original so the server's idempotent insert drops it. Every formula here is
//!    copied verbatim from the Python handlers; changing one silently turns
//!    retries into duplicates.

use serde::Serialize;

use crate::config::Config;

/// A ready-to-spool event. Serialized compactly, in declaration order.
#[derive(Serialize, Debug, Clone)]
pub struct Event {
    pub uid: String,
    pub did: String,
    pub src: &'static str,
    pub et: &'static str,
    pub ts: i64,
    pub tz: i32,
    pub eid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p: Option<serde_json::Value>,
}

/// Local UTC offset in minutes at that instant, DST-correct.
///
/// Done without a date library on purpose. The whole question is "what does
/// this machine think the offset is right now", and both platforms answer it
/// directly: Windows through `GetTimeZoneInformation`, Unix through `localtime_r`
/// on a `time_t`. Pulling in chrono-tz to re-derive an answer the OS already has
/// would add a bundled tzdata that goes stale.
pub fn tz_min(ts_ms: i64) -> i32 {
    platform_tz_min(ts_ms / 1000)
}

#[cfg(windows)]
fn platform_tz_min(_secs: i64) -> i32 {
    use windows_sys::Win32::System::Time::{
        GetTimeZoneInformation, TIME_ZONE_ID_INVALID, TIME_ZONE_INFORMATION,
    };
    // windows-sys exports TIME_ZONE_ID_INVALID but not the other three return
    // values, so DAYLIGHT is spelled out. 0 = unknown, 1 = standard,
    // 2 = daylight.
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    unsafe {
        let mut tzi: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let id = GetTimeZoneInformation(&mut tzi);
        if id == TIME_ZONE_ID_INVALID {
            return 0;
        }
        // Windows reports Bias as UTC = local + bias, i.e. the negation of the
        // offset every other system means by "UTC offset". Negate it once here
        // rather than at each call site.
        let bias = if id == TIME_ZONE_ID_DAYLIGHT {
            tzi.Bias + tzi.DaylightBias
        } else {
            tzi.Bias + tzi.StandardBias
        };
        -bias
    }
}

#[cfg(not(windows))]
fn platform_tz_min(secs: i64) -> i32 {
    extern "C" {
        fn localtime_r(t: *const i64, tm: *mut Tm) -> *mut Tm;
    }
    #[repr(C)]
    #[derive(Default)]
    struct Tm {
        sec: i32,
        min: i32,
        hour: i32,
        mday: i32,
        mon: i32,
        year: i32,
        wday: i32,
        yday: i32,
        isdst: i32,
        gmtoff: i64,
        zone: *const i8,
    }
    unsafe {
        let mut tm = Tm {
            zone: std::ptr::null(),
            ..Default::default()
        };
        if localtime_r(&secs, &mut tm).is_null() {
            return 0;
        }
        (tm.gmtoff / 60) as i32
    }
}

pub fn build(
    cfg: &Config,
    src: &'static str,
    et: &'static str,
    ts_ms: i64,
    eid: String,
    p: Option<serde_json::Value>,
) -> Event {
    Event {
        uid: cfg.uid.clone(),
        did: cfg.did.clone(),
        src,
        et,
        ts: ts_ms,
        tz: tz_min(ts_ms),
        eid,
        p,
    }
}

pub fn dumps(e: &Event) -> String {
    // serde_json's compact form is exactly Python's separators=(",", ":").
    serde_json::to_string(e).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let mut c = Config::for_test();
        c.uid = "u1".into();
        c.did = "win-testhost".into();
        c
    }

    #[test]
    fn wire_shape_matches_the_python_collector() {
        let e = build(
            &cfg(),
            "desktop",
            "focus",
            1_757_000_000_000,
            "dt:win-testhost:focus:1757000000000".into(),
            Some(serde_json::json!({"app": "chrome"})),
        );
        let s = dumps(&e);
        // Envelope field order is the wire order. Only `tz` is machine
        // dependent, so it is matched loosely rather than pinned to this
        // machine's zone. Payload key order is deliberately not asserted --
        // see the module docstring.
        assert!(s.starts_with(
            r#"{"uid":"u1","did":"win-testhost","src":"desktop","et":"focus","ts":1757000000000,"tz":"#
        ));
        assert!(
            s.ends_with(r#","eid":"dt:win-testhost:focus:1757000000000","p":{"app":"chrome"}}"#)
        );
        assert!(!s.contains(", "), "must be compact, not pretty");
    }

    #[test]
    fn payloadless_events_omit_p_entirely() {
        // The Python side passes p=None and the key is absent, not null. An
        // `"p":null` would be a different line for the parity diff.
        let e = build(&cfg(), "desktop", "active", 1, "x".into(), None);
        assert!(
            !dumps(&e).contains("\"p\""),
            "absent payload must omit the key"
        );
    }

    #[test]
    fn tz_offset_is_a_whole_number_of_minutes_in_range() {
        let tz = tz_min(1_757_000_000_000);
        assert!((-720..=840).contains(&tz), "implausible offset {tz}");
    }
}
