//! unvelt-agent — the headless desktop collector.
//!
//!     unvelt-agent              run the loop until interrupted
//!     unvelt-agent --once       one cycle, spool it, exit (parity harness)
//!     unvelt-agent --probe      print every backend probe once and exit
//!     unvelt-agent --version
//!
//! This is step 2 of the desktop build order: parity with the Python collector,
//! nothing new. The five handlers, the envelope, the spool format, the eids and
//! the HTTP contract are all ports rather than redesigns, and the acceptance
//! test is a week of side-by-side capture producing the same events. The tray,
//! the consent UI and the auth flow arrive in step 3; notifications and media
//! in step 4.
//!
//! The one intentional behaviour change is the app identifier on macOS, which
//! migration 0015 requires and which is documented in `backend/unix.rs`.

mod backend;
mod client;
mod config;
mod controller;
mod envelope;
mod handlers;
mod spool;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a == f);

    if has("--version") {
        println!("unvelt-agent {VERSION}");
        return;
    }

    let cfg = config::Config::from_env();

    if has("--probe") {
        probe(&cfg);
        return;
    }

    if cfg.uid.is_empty() {
        eprintln!(
            "unvelt: UNVELT_UID is not set. Nothing would be attributable, so \
             refusing to collect rather than spooling events nobody can claim."
        );
        std::process::exit(2);
    }

    let backend = backend::make_backend(&cfg);
    let client = client::ApiClient::new(&cfg);
    let spool = spool::Spool::new(&cfg);
    let hs = handlers::build_default(&cfg);

    if cfg.debug {
        eprintln!(
            "unvelt-agent {VERSION} | did={} | spool={}",
            cfg.did,
            spool.describe()
        );
    }

    let mut ctl = controller::Controller::new(cfg, client, spool, hs, backend);

    if has("--once") {
        ctl.tick_once();
        ctl.flush();
        return;
    }

    let stop = ctl.stop_flag();
    // Ctrl-C flushes rather than dropping whatever is in `current.jsonl`.
    // Failing to install the handler is not fatal: the spool is still durable,
    // the process just exits less politely.
    let _ = ctrlc_handler(stop);
    ctl.run();
}

/// Minimal Ctrl-C handling without a dependency. On Windows this is a console
/// control handler; on Unix, a signal handler for INT and TERM.
fn ctrlc_handler(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Result<(), ()> {
    use std::sync::atomic::Ordering;
    use std::sync::OnceLock;
    static STOP: OnceLock<std::sync::Arc<std::sync::atomic::AtomicBool>> = OnceLock::new();
    STOP.set(stop).map_err(|_| ())?;

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::BOOL;
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        unsafe extern "system" fn handler(_kind: u32) -> BOOL {
            if let Some(s) = STOP.get() {
                s.store(true, Ordering::Relaxed);
            }
            1 // handled: do not let the default handler kill us mid-flush
        }
        unsafe {
            if SetConsoleCtrlHandler(Some(handler), 1) == 0 {
                return Err(());
            }
        }
    }
    #[cfg(unix)]
    {
        extern "C" fn handler(_sig: i32) {
            if let Some(s) = STOP.get() {
                s.store(true, Ordering::Relaxed);
            }
        }
        extern "C" {
            fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
        }
        unsafe {
            signal(2, handler); // SIGINT
            signal(15, handler); // SIGTERM
        }
    }
    Ok(())
}

/// Print every probe once. This is the first thing to run on a new machine: it
/// answers "which of these does this OS actually let us see" in one line each,
/// which is the question the whole design turns on.
fn probe(cfg: &config::Config) {
    let mut b = backend::make_backend(cfg);
    println!("unvelt-agent {VERSION} on {} ({})", cfg.osname, cfg.host);
    println!("  did          {}", cfg.did);
    println!("  spool        {}", cfg.spool_dir.display());
    match b.frontmost() {
        Some(f) => println!(
            "  frontmost    pkg={:?} label={:?} title={:?}",
            f.pkg, f.label, f.title
        ),
        None => println!("  frontmost    (none)"),
    }
    println!("  idle         {:.1}s", b.idle());
    println!("  fullscreen   {:?}", b.fullscreen());
    println!("  locked       {:?}", b.locked());
    println!("  monitors     {:?}", b.monitors());
    println!("  ssid         {:?}", b.ssid());
    println!("  net          {:?}", b.net());
    match b.power() {
        Some(p) => println!("  power        ac={} pct={:?}", p.ac, p.pct),
        None => println!("  power        (none)"),
    }
    println!("  tz offset    {} min", envelope::tz_min(now_ms()));
}
