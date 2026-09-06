//! Signing in, and staying signed in.
//!
//! There is no ingest key. `X-Compound-Key` was the POC's spam gate and its own
//! docstring admitted it was not auth — `uid` in the envelope was the identity,
//! so any key holder could append to anyone's data. The deployed ingest
//! verifies a **Firebase ID token** and nothing the client says about itself.
//!
//! Which makes an hour the wrong unit. ID tokens expire in one; the agent runs
//! for weeks. So the thing worth keeping is the **refresh token**, and the ID
//! token is derived from it on demand and never stored.
//!
//! The flow, once per machine:
//!
//!   1. `--login` opens the browser at Google's consent screen and listens on
//!      `127.0.0.1:<port>` for the redirect. Loopback rather than a pasted
//!      code, because the code never leaves the machine that asked for it.
//!   2. PKCE (S256). The loopback port is public to every process on the box,
//!      so the authorization code alone must not be enough — the verifier
//!      never goes over the wire until the exchange.
//!   3. Google returns an ID token; Firebase's `signInWithIdp` trades it for a
//!      Firebase refresh token for the same account the phone signs into.
//!   4. That refresh token is stored, and only that.
//!
//! WHAT PROTECTS THE STORED TOKEN, AND WHAT DOES NOT
//!
//! A Firebase refresh token is a long-lived credential: whoever holds it can
//! act as this account until it is revoked. On Windows it is sealed with DPAPI,
//! so the blob is bound to this user account and useless if copied off the
//! machine. On macOS and Linux it is a 0600 file, which stops another user
//! reading it and does not stop anything running AS this user. The Keychain and
//! the Secret Service are step 3 proper; this is written down here rather than
//! implied, because "encrypted at rest" and "0600" are very different claims
//! and only one of them is true on two of the three platforms.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;

use crate::config::Config;

const IDP: &str = "https://identitytoolkit.googleapis.com/v1/accounts";
const SECURETOKEN: &str = "https://securetoken.googleapis.com/v1/token";
const GOOGLE_AUTH: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN: &str = "https://oauth2.googleapis.com/token";

/// Refreshed slightly before it actually expires, so a request in flight when
/// the clock ticks over does not fail on a token that was valid when it left.
const REFRESH_SKEW_S: i64 = 120;

pub struct Session {
    api_key: String,
    refresh_token: String,
    id_token: Option<String>,
    expires_at: i64,
    agent: ureq::Agent,
}

impl Session {
    /// Load the stored refresh token. `None` when nobody has signed in yet.
    pub fn load(cfg: &Config) -> Option<Session> {
        let refresh = read_secret(&token_path(cfg))?;
        Some(Session {
            api_key: cfg.api_key.clone(),
            refresh_token: refresh,
            id_token: None,
            expires_at: 0,
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(20))
                .build(),
        })
    }

    /// A valid ID token, refreshing only when the one in hand is close to
    /// expiry. Returns `None` if the refresh itself failed — the caller keeps
    /// its batch on disk and tries again, which is the same thing it does for
    /// any other network failure.
    pub fn id_token(&mut self) -> Option<String> {
        let now = crate::now_ms() / 1000;
        if let Some(t) = &self.id_token {
            if now < self.expires_at - REFRESH_SKEW_S {
                return Some(t.clone());
            }
        }
        let body = format!(
            "grant_type=refresh_token&refresh_token={}",
            urlencode(&self.refresh_token)
        );
        let resp = self
            .agent
            .post(&format!("{SECURETOKEN}?key={}", self.api_key))
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string(&body)
            .ok()?;
        let v: serde_json::Value = resp.into_json().ok()?;
        let id = v.get("id_token")?.as_str()?.to_string();
        let ttl: i64 = v
            .get("expires_in")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600);
        // Firebase may hand back a new refresh token; keeping the old one
        // would work until it silently stopped.
        if let Some(r) = v.get("refresh_token").and_then(|x| x.as_str()) {
            if r != self.refresh_token {
                self.refresh_token = r.to_string();
            }
        }
        self.expires_at = now + ttl;
        self.id_token = Some(id.clone());
        Some(id)
    }
}

pub fn token_path(cfg: &Config) -> PathBuf {
    cfg.spool_dir
        .parent()
        .unwrap_or(&cfg.spool_dir)
        .join("session.bin")
}

/// Interactive sign-in. Returns the Firebase uid on success.
pub fn login(cfg: &Config) -> Result<String, String> {
    if cfg.oauth_client_id.is_empty() {
        return Err("UNVELT_OAUTH_CLIENT_ID is not set.\n\n\
             Create one once, in the Google Cloud console for project \
             compound-506422:\n\
             \x20 APIs & Services -> Credentials -> Create credentials\n\
             \x20 -> OAuth client ID -> Application type: Desktop app\n\n\
             Then set UNVELT_OAUTH_CLIENT_ID (and UNVELT_OAUTH_CLIENT_SECRET, \
             which Google issues alongside it and which is not a secret for a \
             desktop client -- it ships in every copy of the app)."
            .into());
    }

    // Port 0: the OS picks a free one. A fixed port would collide with whatever
    // else is on the machine and fail at the worst moment.
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("cannot listen: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("no local addr: {e}"))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}");

    let verifier = random_token();
    let challenge = b64url(&sha256(verifier.as_bytes()));
    let state = random_token();

    let url = format!(
        "{GOOGLE_AUTH}?client_id={}&redirect_uri={}&response_type=code\
         &scope={}&code_challenge={}&code_challenge_method=S256&state={}\
         &access_type=offline&prompt=consent",
        urlencode(&cfg.oauth_client_id),
        urlencode(&redirect),
        urlencode("openid email profile"),
        challenge,
        state,
    );

    println!("Opening your browser to sign in.");
    println!("If it does not open, paste this:\n\n{url}\n");
    open_browser(&url);

    let code = wait_for_code(&listener, &state)?;

    // Exchange the code for a Google ID token.
    let mut body = format!(
        "code={}&client_id={}&redirect_uri={}&grant_type=authorization_code&code_verifier={}",
        urlencode(&code),
        urlencode(&cfg.oauth_client_id),
        urlencode(&redirect),
        urlencode(&verifier),
    );
    if !cfg.oauth_client_secret.is_empty() {
        body.push_str(&format!(
            "&client_secret={}",
            urlencode(&cfg.oauth_client_secret)
        ));
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let resp = agent
        .post(GOOGLE_TOKEN)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
        .map_err(|e| format!("google token exchange failed: {}", describe(e)))?;
    let v: serde_json::Value = resp
        .into_json()
        .map_err(|e| format!("google token response unreadable: {e}"))?;
    let google_id_token = v
        .get("id_token")
        .and_then(|x| x.as_str())
        .ok_or("google returned no id_token")?
        .to_string();

    // Trade it for a Firebase session on the same account the phone uses.
    let resp = agent
        .post(&format!("{IDP}:signInWithIdp?key={}", cfg.api_key))
        .send_json(serde_json::json!({
            "postBody": format!("id_token={google_id_token}&providerId=google.com"),
            "requestUri": redirect,
            "returnSecureToken": true,
        }))
        .map_err(|e| format!("firebase sign-in failed: {}", describe(e)))?;
    let v: serde_json::Value = resp
        .into_json()
        .map_err(|e| format!("firebase response unreadable: {e}"))?;
    let refresh = v
        .get("refreshToken")
        .and_then(|x| x.as_str())
        .ok_or("firebase returned no refreshToken")?;
    let uid = v
        .get("localId")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let email = v.get("email").and_then(|x| x.as_str()).unwrap_or("");

    write_secret(&token_path(cfg), refresh)?;
    println!("\nSigned in as {email}");
    println!("firebase uid: {uid}");
    println!("stored: {}", token_path(cfg).display());
    Ok(uid)
}

/// Serve exactly one redirect, check `state`, and hand the browser a page that
/// says what happened. Anything else on the port gets a 404 and is ignored.
fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String, String> {
    for stream in listener.incoming() {
        let mut stream = stream.map_err(|e| format!("accept failed: {e}"))?;
        let mut line = String::new();
        BufReader::new(&stream)
            .read_line(&mut line)
            .map_err(|e| format!("read failed: {e}"))?;
        let target = line.split_whitespace().nth(1).unwrap_or("");
        let params = query_params(target);

        if let Some(err) = params.get("error") {
            respond(&mut stream, "Sign-in was refused. You can close this tab.");
            return Err(format!("google returned error={err}"));
        }
        let (Some(code), Some(got_state)) = (params.get("code"), params.get("state")) else {
            respond(&mut stream, "Waiting for the sign-in redirect...");
            continue;
        };
        // The loopback port is reachable by anything on this machine, so a
        // redirect arriving with the wrong state is not ours.
        if got_state != state {
            respond(
                &mut stream,
                "That request did not match. Nothing was changed.",
            );
            return Err("state mismatch on the loopback redirect".into());
        }
        respond(&mut stream, "Signed in. You can close this tab.");
        return Ok(code.clone());
    }
    Err("the browser never came back".into())
}

fn respond(stream: &mut std::net::TcpStream, msg: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8>\
         <title>unvelt</title>\
         <body style=\"font:16px/1.5 system-ui;margin:4rem auto;max-width:32rem;color:#1b1b1b\">\
         <p>{msg}</p>"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.flush();
}

fn query_params(target: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(q) = target.split_once('?').map(|x| x.1) else {
        return out;
    };
    // The caller may hand us a bare target or a whole request line; a real URL
    // never contains a raw space, so everything from the first one is framing.
    let q = q.split_whitespace().next().unwrap_or("");
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            out.insert(k.to_string(), urldecode(v));
        }
    }
    out
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

// ------------------------------------------------------------------ storage

#[cfg(windows)]
fn write_secret(path: &std::path::Path, value: &str) -> Result<(), String> {
    let blob = dpapi(value.as_bytes(), true).ok_or("DPAPI encrypt failed")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{e}"))?;
    }
    std::fs::write(path, blob).map_err(|e| format!("{e}"))
}

#[cfg(windows)]
fn read_secret(path: &std::path::Path) -> Option<String> {
    let blob = std::fs::read(path).ok()?;
    let plain = dpapi(&blob, false)?;
    String::from_utf8(plain).ok()
}

/// CryptProtectData / CryptUnprotectData. Binds the blob to this Windows user,
/// so a copy taken off the machine is inert.
#[cfg(windows)]
fn dpapi(input: &[u8], protect: bool) -> Option<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };
    unsafe {
        let inb = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = if protect {
            CryptProtectData(
                &inb,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &inb,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                &mut out,
            )
        };
        if ok == 0 || out.pbData.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        windows_sys::Win32::Foundation::LocalFree(out.pbData as *mut std::ffi::c_void);
        Some(v)
    }
}

#[cfg(not(windows))]
fn write_secret(path: &std::path::Path, value: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{e}"))?;
    }
    // 0600 from the moment it exists. Writing it and then chmod-ing leaves a
    // window where the file is world-readable, which is the whole risk.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{e}"))?;
    f.write_all(value.as_bytes()).map_err(|e| format!("{e}"))
}

#[cfg(not(windows))]
fn read_secret(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

// ------------------------------------------------------------------ crypto

fn random_token() -> String {
    // 32 bytes of OS randomness, base64url. Not `rand`: one call to the
    // platform's own generator is the whole requirement.
    let mut buf = [0u8; 32];
    fill_random(&mut buf);
    b64url(&buf)
}

#[cfg(windows)]
fn fill_random(buf: &mut [u8]) {
    use windows_sys::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        );
    }
}

#[cfg(not(windows))]
fn fill_random(buf: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(buf);
    }
}

fn b64url(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        if c.len() > 1 {
            out.push(A[(n >> 6) as usize & 63] as char);
        }
        if c.len() > 2 {
            out.push(A[n as usize & 63] as char);
        }
    }
    out // unpadded, which is what RFC 7636 asks for
}

/// SHA-256, for the PKCE challenge.
fn sha256(msg: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut data = msg.to_vec();
    let bits = (msg.len() as u64) * 8;
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bits.to_be_bytes());

    for block in data.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (i, v) in [a, b, c, d, e, f, g, hh].into_iter().enumerate() {
            h[i] = h[i].wrapping_add(v);
        }
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn describe(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            format!(
                "HTTP {code}: {}",
                body.chars().take(300).collect::<String>()
            )
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_published_vectors() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // Crosses the 56-byte padding boundary, which is where a hand-written
        // SHA-256 goes wrong if it goes wrong at all.
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn pkce_challenge_matches_rfc7636() {
        // The worked example from RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            b64url(&sha256(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn base64url_is_unpadded_and_url_safe() {
        assert_eq!(b64url(b"a"), "YQ");
        assert_eq!(b64url(b"ab"), "YWI");
        assert_eq!(b64url(b"abc"), "YWJj");
        assert!(!b64url(&[251, 255, 190]).contains(['+', '/', '=']));
    }

    #[test]
    fn redirect_query_is_parsed_and_decoded() {
        let p = query_params("/?code=4%2F0Ab_c-d&state=xyz&scope=email+profile HTTP/1.1");
        assert_eq!(p.get("code").unwrap(), "4/0Ab_c-d");
        assert_eq!(p.get("state").unwrap(), "xyz");
        assert_eq!(p.get("scope").unwrap(), "email profile");
        assert!(query_params("/favicon.ico").is_empty());
    }

    #[test]
    fn urlencode_escapes_everything_outside_the_unreserved_set() {
        assert_eq!(urlencode("a b/c?d=e&f"), "a%20b%2Fc%3Fd%3De%26f");
        assert_eq!(urlencode("A-Z_a.z~0"), "A-Z_a.z~0");
    }

    #[test]
    fn a_secret_survives_a_round_trip_through_storage() {
        let dir = std::env::temp_dir().join(format!("unvelt-auth-{}", crate::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("session.bin");
        let secret = "AMf-vBx_not_a_real_refresh_token_0123456789";
        write_secret(&p, secret).unwrap();
        assert_eq!(read_secret(&p).as_deref(), Some(secret));
        // On Windows the stored bytes are DPAPI-sealed, so the plaintext must
        // not be sitting in the file.
        let raw = std::fs::read(&p).unwrap();
        if cfg!(windows) {
            assert!(
                !raw.windows(secret.len()).any(|w| w == secret.as_bytes()),
                "refresh token was written in the clear"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
