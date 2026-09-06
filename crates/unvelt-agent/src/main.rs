//! unvelt-agent — the headless desktop collector, as a command line.
//!
//!     unvelt-agent --login      sign in once on this machine
//!     unvelt-agent              run the loop until interrupted
//!     unvelt-agent --once       one cycle, spool it, exit (parity harness)
//!     unvelt-agent --probe      print every backend probe once and exit
//!     unvelt-agent --version
//!
//! Everything here is argument parsing and printing. The collector itself is
//! the library beside this file, so the tray app runs exactly the same code
//! rather than a second implementation of it — and so this CLI keeps working
//! as the thing to reach for when the window is the part that is broken.

use unvelt_agent::{
    auth, backend, client, config, controller, envelope, handlers, instance, spool, VERSION,
};

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

    if has("--login") {
        match auth::login(&cfg) {
            Ok(_) => {}
            Err(msg) => {
                eprintln!("\nunvelt: sign-in failed.\n{msg}");
                std::process::exit(1);
            }
        }
        return;
    }

    if cfg.uid.is_empty() {
        eprintln!(
            "unvelt: UNVELT_UID is not set. Nothing would be attributable, so \
             refusing to collect rather than spooling events nobody can claim."
        );
        std::process::exit(2);
    }

    // Taken before anything is built, and only for the run that collects:
    // --probe, --login and --once have all returned by now. Two loops on one
    // machine post every signal twice under the same device id, and because
    // eids carry a millisecond they do not dedupe -- the day just reads as
    // twice as busy as it was.
    let _lock = if has("--once") {
        None
    } else {
        match instance::acquire() {
            Some(l) => Some(l),
            None => {
                eprintln!(
                    "unvelt: another unvelt collector is already running for this user.\n\
                     Quit it from the tray first -- two would double-count everything."
                );
                std::process::exit(3);
            }
        }
    };

    let backend = backend::make_backend(&cfg);
    let client = client::ApiClient::new(&cfg);
    let spool = spool::Spool::new(&cfg);
    let hs = handlers::build_default(&cfg);

    if !client.signed_in() {
        eprintln!(
            "unvelt: not signed in on this machine -- run `unvelt-agent --login` first.\n\
             Collecting anyway; the spool keeps everything until a sign-in lets it drain."
        );
    }
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

/// Enough of an identifier to recognise, not enough to copy out of a screen
/// shot. A client id is not a secret, but a probe's output gets pasted around
/// and there is no reason for it to carry the whole string.
fn short(id: &str) -> String {
    let head: String = id.chars().take(12).collect();
    format!("{head}...")
}

/// Print every probe once. This is the first thing to run on a new machine: it
/// answers "which of these does this OS actually let us see" in one line each,
/// which is the question the whole design turns on.
fn probe(cfg: &config::Config) {
    let p = unvelt_agent::probe_once(cfg);
    println!("unvelt-agent {VERSION} on {} ({})", cfg.osname, cfg.host);
    println!("  did          {}", cfg.did);
    println!("  spool        {}", cfg.spool_dir.display());
    match (&p.app, &p.title) {
        (None, _) => println!("  frontmost    (none)"),
        (Some(a), t) => println!(
            "  frontmost    pkg={a:?} title={:?}",
            t.as_deref().unwrap_or("")
        ),
    }
    println!("  idle         {:.1}s", p.idle_s);
    println!("  fullscreen   {:?}", p.fullscreen);
    println!("  locked       {:?}", p.locked);
    println!("  monitors     {:?}", p.monitors);
    println!("  ssid         {:?}", p.ssid);
    println!("  net          {:?}", p.net);
    match (p.ac, p.battery_pct) {
        (None, None) => println!("  power        (none)"),
        (ac, pct) => println!("  power        ac={ac:?} pct={pct:?}"),
    }
    println!(
        "  tz offset    {} min",
        envelope::tz_min(unvelt_agent::now_ms())
    );
    println!(
        "  signed in    {}",
        if auth::Session::load(cfg).is_some() {
            "yes"
        } else {
            "no -- run --login"
        }
    );
    // Shown before the sign-in line is acted on, because "--login did nothing
    // useful" and "the client id never reached the process" look identical
    // from the outside and have completely different fixes.
    println!(
        "  oauth client {}",
        match (
            cfg.oauth_client_id.is_empty(),
            cfg.oauth_client_secret.is_empty()
        ) {
            (true, _) => "not set -- see SETUP.md".to_string(),
            (false, true) => format!("{} (no secret set)", short(&cfg.oauth_client_id)),
            (false, false) => format!("{} + secret", short(&cfg.oauth_client_id)),
        }
    );
    println!("  posting to   {}", cfg.url);
}
