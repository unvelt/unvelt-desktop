//! The only place that talks HTTP to the server.
//!
//! Fire-and-forget from the caller's view: every method returns a bool and
//! never fails loudly. A `false` just means the spool file stays on disk for
//! the next flush, which is the whole point of the spool being the source of
//! truth rather than the network.
//!
//! Matches `compound/client.py`: gzipped NDJSON to `/e`, JSON to `/hb`, and the
//! `X-Compound-Key` header. The header name keeps its old spelling on purpose —
//! renaming it is an ingest change, and this port is not the place for one.

use std::io::Write;
use std::time::Duration;

use crate::auth::Session;
use crate::config::Config;

pub struct ApiClient {
    url: String,
    key: String,
    /// `None` until somebody signs in, and re-checked on every request until
    /// they do.
    ///
    /// It used to be loaded once, at construction, and that was wrong in the
    /// one case that matters: a fresh install has no session yet, so the
    /// client cached `None` and kept it after the person signed in. The window
    /// re-read the session on every poll and said "signed in" while every
    /// upload went out unauthenticated and came back 401 -- the UI right, the
    /// uploader wrong, which is worse than both being wrong.
    session: std::cell::RefCell<Option<Session>>,
    /// Kept so the session can be re-read later. Cheap: a handful of strings.
    cfg: Config,
    agent: ureq::Agent,
    debug: bool,
}

impl ApiClient {
    pub fn signed_in(&self) -> bool {
        self.bearer().is_some()
    }

    /// A bearer token, loading the session if one has appeared since startup.
    ///
    /// The reload only happens while signed out, so the steady state is one
    /// `Option` check per request and the file is not re-read on a machine
    /// that already has a session.
    fn bearer(&self) -> Option<String> {
        let mut slot = self.session.borrow_mut();
        if slot.is_none() {
            *slot = Session::load(&self.cfg);
        }
        slot.as_mut().and_then(|s| s.id_token())
    }

    pub fn new(cfg: &Config) -> Self {
        ApiClient {
            url: cfg.url.clone(),
            key: cfg.key.clone(),
            session: std::cell::RefCell::new(Session::load(cfg)),
            cfg: cfg.clone(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .build(),
            debug: cfg.debug,
        }
    }

    fn post(&self, path: &str, body: Vec<u8>, content_type: &str, gzipped: bool) -> bool {
        let mut req = self
            .agent
            .post(&format!("{}{}", self.url, path))
            .set("Content-Type", content_type);
        if gzipped {
            req = req.set("Content-Encoding", "gzip");
        }
        if !self.key.is_empty() {
            req = req.set("X-Compound-Key", &self.key);
        }
        if let Some(tok) = self.bearer() {
            req = req.set("Authorization", &format!("Bearer {tok}"));
        } else if self.debug {
            eprintln!("unvelt: no session yet; sending unauthenticated");
        }
        match req.send_bytes(&body) {
            Ok(r) => (200..300).contains(&r.status()),
            Err(ureq::Error::Status(401, _)) => {
                // Worth saying out loud even when not debugging: an agent that
                // spools for a week into a 401 is the failure mode this whole
                // step exists to avoid.
                eprintln!("unvelt: {path} rejected the request (401). Run `unvelt-agent --login`.");
                false
            }
            Err(err) => {
                if self.debug {
                    eprintln!("unvelt: POST {path} failed: {err}");
                }
                false
            }
        }
    }

    /// Gzip a JSONL batch and POST it to `/e`.
    pub fn post_events(&self, jsonl: &[u8]) -> bool {
        let Some(body) = gzip(jsonl) else {
            return false;
        };
        self.post("/e", body, "application/x-ndjson", true)
    }

    pub fn post_heartbeat(&self, hb: &serde_json::Value) -> bool {
        let body = hb.to_string().into_bytes();
        self.post("/hb", body, "application/json", false)
    }
}

/// A minimal gzip container around a stored (uncompressed) deflate stream.
///
/// Deliberately not a compression library. The server only needs a valid gzip
/// member, the payload is already small and the link is not the bottleneck —
/// and a dependency whose only job is to shrink a few kilobytes of JSONL is a
/// dependency to audit forever. If batches ever grow enough for this to matter,
/// swap the block writer below for a real deflate; the framing does not change.
fn gzip(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() + 64);
    // Header: magic, deflate, no flags, no mtime, no extra flags, unknown OS.
    out.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff]);
    // Stored deflate blocks: each carries at most 65535 bytes as LEN/~LEN.
    let mut chunks = data.chunks(65535).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    while let Some(chunk) = chunks.next() {
        let last = chunks.peek().is_none();
        out.push(if last { 0x01 } else { 0x00 });
        let n = chunk.len() as u16;
        out.write_all(&n.to_le_bytes()).ok()?;
        out.write_all(&(!n).to_le_bytes()).ok()?;
        out.extend_from_slice(chunk);
    }
    out.write_all(&crc32(data).to_le_bytes()).ok()?;
    out.write_all(&(data.len() as u32).to_le_bytes()).ok()?;
    Some(out)
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *entry = c;
    }
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_created_after_startup_is_picked_up() {
        // The Mac bug, pinned. The client is built before anyone has signed
        // in -- which is what happens on every fresh install -- and must
        // notice the session that appears when they do, without a restart.
        // Caching the absence made the window say "signed in" while every
        // upload went out unauthenticated.
        let mut cfg = Config::for_test();
        cfg.spool_dir = std::env::temp_dir()
            .join(format!("unvelt-sess-{}", crate::now_ms()))
            .join("spool");
        std::fs::create_dir_all(&cfg.spool_dir).unwrap();

        let client = ApiClient::new(&cfg);
        assert!(!client.signed_in(), "no session should exist yet");

        // Sign in happens elsewhere, in another thread, after this client was
        // constructed. All it leaves behind is the stored refresh token.
        crate::auth::write_secret_for_test(&crate::auth::token_path(&cfg), "not-a-real-token");

        // `signed_in` must now find it. It cannot mint a real ID token from a
        // fake refresh token, so this asserts the RELOAD happened rather than
        // the network call succeeding.
        assert!(
            client.session.borrow().is_none(),
            "precondition: still cached as absent"
        );
        let _ = client.signed_in();
        assert!(
            client.session.borrow().is_some(),
            "the session written after startup was never re-read"
        );

        let _ = std::fs::remove_dir_all(cfg.spool_dir.parent().unwrap());
    }

    #[test]
    fn crc32_matches_the_known_check_value() {
        // The standard CRC-32 check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn gzip_frames_a_payload_the_way_a_decoder_expects() {
        let body = b"{\"a\":1}\n{\"a\":2}\n";
        let g = gzip(body).unwrap();
        assert_eq!(&g[..3], &[0x1f, 0x8b, 0x08], "gzip magic + deflate method");
        let n = g.len();
        let size = u32::from_le_bytes(g[n - 4..].try_into().unwrap());
        let crc = u32::from_le_bytes(g[n - 8..n - 4].try_into().unwrap());
        assert_eq!(size as usize, body.len(), "ISIZE trailer");
        assert_eq!(crc, crc32(body), "CRC32 trailer");
        // One stored block, final bit set, LEN and ~LEN agreeing.
        assert_eq!(g[10], 0x01);
        let len = u16::from_le_bytes(g[11..13].try_into().unwrap());
        let nlen = u16::from_le_bytes(g[13..15].try_into().unwrap());
        assert_eq!(len, body.len() as u16);
        assert_eq!(nlen, !len);
    }
}
