//! Windows notifications, read from the shell's own store.
//!
//! `%LOCALAPPDATA%\Microsoft\Windows\Notifications\wpndatabase.db` is the
//! SQLite database the Action Centre keeps. Reading it needs no package
//! identity, no signature and no consent prompt — which is the whole reason
//! this ships at all. `UserNotificationListener`, the documented API, requires
//! package identity, which requires a signed sparse package, which requires a
//! $120/yr certificate. This route was verified on real hardware first: twelve
//! distinct apps resolved, arrival times correct to the microsecond, `Id`
//! monotonic. It stays the upgrade path if signing is ever bought.
//!
//! CONTENT-FREE BY CONSTRUCTION, NOT BY POLICY
//!
//! `Notification.Payload` holds the toast XML — the title and body of every
//! notification you have received. **The query below never names that column.**
//! That is deliberate and it is the load-bearing line in this file: a policy
//! that says "we don't look at content" is one careless SELECT away from being
//! false, whereas a query that cannot return it is safe by shape. Per-app
//! content capture is a separate feature with its own allowlist that starts
//! empty (`docs/desktop-plan.md` §2b), and when it arrives it will be a
//! different query, not a wider one.
//!
//! WHY ONLY TOASTS
//!
//! `Type` is `toast`, `badge` or `tile`. Badges and tiles are an app changing
//! its own icon — nobody was interrupted. Filtering to toasts is the desktop
//! equivalent of Android's `ongoing = 0` rule, which exists because counting
//! transport bars and download progress measures the notification system
//! rather than the person: 74% of one POC participant's volume was that churn.

use std::path::PathBuf;

use super::{Handler, Tick};
use crate::config::Config;
use crate::envelope::Event;

/// Windows FILETIME epoch (1601-01-01) to Unix epoch, in 100-nanosecond ticks.
const FILETIME_TO_UNIX: i64 = 116_444_736_000_000_000;

pub struct NotifHandler {
    interval: f64,
    db: PathBuf,
    cursor_path: PathBuf,
    /// Highest `Notification.Id` already emitted. The column is monotonic, so
    /// this is both the cursor and what keeps a restart from re-sending.
    cursor: i64,
    /// Set once the store has been read successfully, so a machine that simply
    /// has no such database (a Windows build without Action Centre, or a
    /// locked-down profile) is not retried noisily forever.
    unavailable: bool,
}

impl NotifHandler {
    pub fn new(cfg: &Config) -> Self {
        let base = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let cursor_path = crate::config::state_dir().join("notif_cursor");
        let cursor = std::fs::read_to_string(&cursor_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        NotifHandler {
            // Notifications are sparse and the read is a file copy, so this
            // runs on the slow clock rather than the five-second tick.
            interval: cfg.context_sec as f64,
            db: base
                .join("Microsoft")
                .join("Windows")
                .join("Notifications")
                .join("wpndatabase.db"),
            cursor_path,
            cursor,
            unavailable: false,
        }
    }

    fn save_cursor(&self) {
        if let Some(dir) = self.cursor_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&self.cursor_path, self.cursor.to_string());
    }
}

/// One row, already stripped to what we are willing to hold.
pub struct Posted {
    pub id: i64,
    pub app: String,
    pub arrival_ms: i64,
}

impl Handler for NotifHandler {
    fn name(&self) -> &'static str {
        "notif"
    }

    fn interval(&self) -> f64 {
        self.interval
    }

    fn poll(&mut self, tick: &mut Tick) -> Vec<Event> {
        if self.unavailable {
            return Vec::new();
        }
        let rows = match read_since(&self.db, self.cursor) {
            Ok(r) => r,
            Err(e) => {
                // A missing database is a fact about this machine, said once.
                // A locked one is transient and worth retrying.
                if !self.db.exists() {
                    self.unavailable = true;
                    if tick.cfg.debug {
                        eprintln!("unvelt: no notification store at {}", self.db.display());
                    }
                } else if tick.cfg.debug {
                    eprintln!("unvelt: notification read failed: {e}");
                }
                return Vec::new();
            }
        };

        let mut out = Vec::new();
        for r in rows {
            self.cursor = self.cursor.max(r.id);
            out.push(tick.event(
                "notif",
                "posted",
                r.arrival_ms,
                // Keyed on the store's own row id, so a re-read after a crash
                // collides with the original instead of duplicating it.
                format!("nt:{}:{}", tick.cfg.did, r.id),
                Some(serde_json::json!({
                    "pkg": r.app,
                    // Windows toasts have no equivalent of FLAG_ONGOING_EVENT,
                    // and filtering to `toast` has already excluded the badge
                    // and tile updates that would map to it. Sent explicitly
                    // as 0 rather than omitted, because the field is required
                    // and the digest counts on `ongoing = 0`.
                    "ongoing": 0,
                })),
            ));
        }
        if !out.is_empty() {
            self.save_cursor();
        }
        out
    }
}

/// Copy the store aside and read rows newer than `cursor`.
///
/// The copy is not paranoia. The database is in WAL mode and open for writing
/// by the shell, so a reader must take `-wal` and `-shm` with it or it sees a
/// stale snapshot missing the most recent notifications — which is precisely
/// the set we want. Copying also means we never hold a lock on a file the
/// Action Centre needs.
#[cfg(windows)]
fn read_since(db: &std::path::Path, cursor: i64) -> Result<Vec<Posted>, String> {
    use rusqlite::Connection;

    if !db.exists() {
        return Err("no notification store".into());
    }
    let tmp = std::env::temp_dir().join(format!("unvelt-wpn-{}.db", std::process::id()));
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{}", db.display(), suffix));
        let to = PathBuf::from(format!("{}{}", tmp.display(), suffix));
        if from.exists() {
            std::fs::copy(&from, &to).map_err(|e| format!("copy {suffix}: {e}"))?;
        }
    }

    let out = (|| -> Result<Vec<Posted>, String> {
        let conn = Connection::open(&tmp).map_err(|e| e.to_string())?;
        // Payload is NOT selected. See the module docstring; this is the line
        // that makes content-free structural rather than a promise.
        let mut stmt = conn
            .prepare(
                "SELECT n.Id, h.PrimaryId, n.ArrivalTime \
                 FROM Notification n \
                 JOIN NotificationHandler h ON h.RecordId = n.HandlerId \
                 WHERE n.Id > ?1 AND n.Type = 'toast' \
                 ORDER BY n.Id",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([cursor], |row| {
                let id: i64 = row.get(0)?;
                let app: String = row.get(1)?;
                let filetime: i64 = row.get(2)?;
                Ok(Posted {
                    id,
                    app,
                    arrival_ms: filetime_to_unix_ms(filetime),
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

#[cfg(not(windows))]
fn read_since(_db: &std::path::Path, _cursor: i64) -> Result<Vec<Posted>, String> {
    Err("the notification store is Windows-only".into())
}

/// Windows FILETIME (100ns ticks since 1601) to Unix milliseconds.
pub fn filetime_to_unix_ms(ft: i64) -> i64 {
    (ft - FILETIME_TO_UNIX) / 10_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_converts_to_the_unix_epoch() {
        // 1601-01-01, the FILETIME epoch itself, is Unix -11644473600s.
        assert_eq!(filetime_to_unix_ms(0), -11_644_473_600_000);
        // The Unix epoch expressed as a FILETIME must come back as zero.
        assert_eq!(filetime_to_unix_ms(FILETIME_TO_UNIX), 0);
        // A real arrival time from this machine's store, sanity-bounded to
        // "some time this decade" rather than pinned to a magic number.
        let ms = filetime_to_unix_ms(133_700_000_000_000_000);
        assert!(
            (1_600_000_000_000..2_000_000_000_000).contains(&ms),
            "implausible conversion: {ms}"
        );
    }

    #[test]
    fn the_query_can_never_return_notification_content() {
        // The guarantee this whole module rests on, asserted rather than
        // trusted to review: if someone widens the SELECT to include the toast
        // payload, this fails before it ships.
        let src = include_str!("notif.rs");
        let sql_start = src
            .find("\"SELECT n.Id")
            .expect("the query moved; update this test");
        let sql = &src[sql_start..sql_start + 400];
        for forbidden in ["Payload", "payload"] {
            assert!(
                !sql.contains(forbidden),
                "the notification query names {forbidden}"
            );
        }
    }
}
