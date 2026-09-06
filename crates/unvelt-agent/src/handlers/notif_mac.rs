//! macOS notifications, read from Notification Centre's own store.
//!
//! `~/Library/Group Containers/group.com.apple.usernoted/db2/db` is the SQLite
//! database Notification Centre keeps. It moved there and went behind TCC in
//! Sequoia, so reading it needs **Full Disk Access** — a large ask, and the
//! reason this source is off until someone turns it on. The app says what it
//! needs rather than smuggling the grant into onboarding.
//!
//! CONTENT-FREE BY CONSTRUCTION, THE SAME WAY WINDOWS IS
//!
//! The `record` column holds a binary plist containing the title and body of
//! every notification. **The query below never names that column**, for the
//! same reason as `notif.rs`: a policy is one careless SELECT from being
//! false; a query that cannot return content is safe by shape. A test greps
//! the SQL, so widening it fails before it ships.
//!
//! WHAT macOS GIVES THAT NOTHING ELSE DOES
//!
//! `presented` records whether the notification was actually shown to the
//! person rather than delivered while Do Not Disturb was on or the app was
//! frontmost. Android can only infer that from an unlock landing inside a
//! window after the post (`derivations.notif.acted`), and the inference is
//! noisy: act-rate sits between 15% and 21% for every app with volume. This
//! is the platform telling us directly, and it is the one macOS signal with
//! no equivalent anywhere else.
//!
//! It is NOT mapped onto `ongoing`. Those mean different things -- `ongoing`
//! is Android's transport-bar flag, "this app is redrawing its own status" --
//! and folding one into the other would put macOS notifications that were
//! merely suppressed into the same bucket as download-progress churn.

use std::path::PathBuf;

use super::{Handler, Tick};
use crate::config::Config;
use crate::envelope::Event;

/// Apple's epoch is 2001-01-01; Unix's is 1970-01-01.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const APPLE_EPOCH_OFFSET: i64 = 978_307_200;

pub struct MacNotifHandler {
    interval: f64,
    db: PathBuf,
    cursor_path: PathBuf,
    cursor: i64,
    /// Set after a read fails in a way that will not fix itself, so a machine
    /// without Full Disk Access is not retried noisily every minute forever.
    blocked: bool,
    warned: bool,
}

impl MacNotifHandler {
    pub fn new(cfg: &Config) -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        MacNotifHandler {
            interval: cfg.context_sec as f64,
            db: PathBuf::from(home)
                .join("Library")
                .join("Group Containers")
                .join("group.com.apple.usernoted")
                .join("db2")
                .join("db"),
            cursor_path: crate::config::state_dir().join("notif_mac_cursor"),
            cursor: std::fs::read_to_string(crate::config::state_dir().join("notif_mac_cursor"))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0),
            blocked: false,
            warned: false,
        }
    }
}

impl Handler for MacNotifHandler {
    fn name(&self) -> &'static str {
        // The same switch as the Windows reader. One person's answer to "may
        // this machine see my notifications" should not depend on which OS
        // they are sitting at.
        "notif"
    }

    fn interval(&self) -> f64 {
        self.interval
    }

    fn poll(&mut self, tick: &mut Tick) -> Vec<Event> {
        if self.blocked {
            return Vec::new();
        }
        let rows = match read_since(&self.db, self.cursor) {
            Ok(r) => r,
            Err(e) => {
                // "Operation not permitted" is TCC refusing, and it will keep
                // refusing until someone grants Full Disk Access. Said once,
                // with the fix named, then dropped.
                if !self.warned {
                    self.warned = true;
                    if e.contains("not permitted") || e.contains("unable to open") {
                        self.blocked = true;
                        eprintln!(
                            "unvelt: cannot read Notification Centre. macOS needs Full Disk \
                             Access for unvelt in System Settings > Privacy & Security."
                        );
                    } else if tick.cfg.debug {
                        eprintln!("unvelt: notification read failed: {e}");
                    }
                }
                return Vec::new();
            }
        };

        let mut out = Vec::new();
        for r in rows {
            self.cursor = self.cursor.max(r.id);
            let mut p = serde_json::Map::new();
            p.insert("pkg".into(), serde_json::json!(r.app));
            // Windows has no equivalent of FLAG_ONGOING_EVENT and neither does
            // this; both filter the equivalent noise before it gets here.
            p.insert("ongoing".into(), serde_json::json!(0));
            out.push(tick.event(
                "notif",
                "posted",
                r.delivered_ms,
                format!("nt:{}:{}", tick.cfg.did, r.id),
                Some(serde_json::Value::Object(p)),
            ));
        }
        if !out.is_empty() {
            if let Some(dir) = self.cursor_path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&self.cursor_path, self.cursor.to_string());
        }
        out
    }
}

pub struct Delivered {
    pub id: i64,
    pub app: String,
    pub delivered_ms: i64,
}

/// Copy the store aside and read records newer than `cursor`.
///
/// Copied for the same reason as the Windows one: the database is open for
/// writing by `usernoted`, and a reader must take `-wal` and `-shm` with it or
/// it sees a snapshot missing exactly the newest notifications. Copying also
/// means never holding a lock on a file Notification Centre needs.
#[cfg(target_os = "macos")]
fn read_since(db: &std::path::Path, cursor: i64) -> Result<Vec<Delivered>, String> {
    use rusqlite::Connection;

    if !db.exists() {
        return Err("no notification store".into());
    }
    let tmp = std::env::temp_dir().join(format!("unvelt-un-{}.db", std::process::id()));
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{}", db.display(), suffix));
        let to = PathBuf::from(format!("{}{}", tmp.display(), suffix));
        if from.exists() {
            std::fs::copy(&from, &to).map_err(|e| format!("copy {suffix}: {e}"))?;
        }
    }

    let out = (|| -> Result<Vec<Delivered>, String> {
        let conn = Connection::open(&tmp).map_err(|e| e.to_string())?;
        // `record` is NOT selected. It is a binary plist holding the title and
        // body of the notification; see the module docstring.
        let mut stmt = conn
            .prepare(
                "SELECT r.rec_id, a.identifier, r.delivered_date \
                 FROM record r JOIN app a ON a.app_id = r.app_id \
                 WHERE r.rec_id > ?1 AND r.delivered_date IS NOT NULL \
                 ORDER BY r.rec_id",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([cursor], |row| {
                let id: i64 = row.get(0)?;
                let app: String = row.get(1)?;
                // Stored as seconds since the Apple epoch, as a float.
                let delivered: f64 = row.get(2)?;
                Ok(Delivered {
                    id,
                    app,
                    delivered_ms: apple_seconds_to_unix_ms(delivered),
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    })();

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{}", tmp.display(), suffix)));
    }
    out
}

#[cfg(not(target_os = "macos"))]
fn read_since(_db: &std::path::Path, _cursor: i64) -> Result<Vec<Delivered>, String> {
    Err("Notification Centre is macOS-only".into())
}

/// Seconds since 2001-01-01 to Unix milliseconds.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn apple_seconds_to_unix_ms(secs: f64) -> i64 {
    ((secs + APPLE_EPOCH_OFFSET as f64) * 1000.0) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_apple_epoch_converts_to_unix() {
        // 0 in Apple time is 2001-01-01T00:00:00Z, which is 978307200 in Unix
        // time. Getting this wrong by 31 years is the classic Core Foundation
        // mistake and it fails silently -- every notification simply lands in
        // 1970 or 2032.
        assert_eq!(apple_seconds_to_unix_ms(0.0), 978_307_200_000);
        // A plausible recent timestamp must land in this decade.
        let ms = apple_seconds_to_unix_ms(780_000_000.0);
        assert!(
            (1_700_000_000_000..2_100_000_000_000).contains(&ms),
            "implausible conversion: {ms}"
        );
    }

    #[test]
    fn the_query_can_never_return_notification_content() {
        // The same guarantee as the Windows reader, asserted rather than
        // trusted to review. `record` is the binary plist with the title and
        // body in it.
        let src = include_str!("notif_mac.rs");
        let start = src
            .find("\"SELECT r.rec_id")
            .expect("the query moved; update this test");
        let sql = &src[start..start + 400];
        for forbidden in ["record,", "r.record", "encodedRecord"] {
            assert!(!sql.contains(forbidden), "the query names {forbidden}");
        }
    }
}
