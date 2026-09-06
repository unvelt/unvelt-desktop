// No console window on Windows. The app is a tray icon; a flashing black
// rectangle at every login would be the first thing anyone noticed about it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! The tray app.
//!
//! Three rules shape this file, and all three are about the window mattering
//! less than the collection:
//!
//!   1. **Closing the window must never stop collecting.** The window is a
//!      view; it is hidden rather than destroyed on close, and the collector
//!      lives on its own thread that knows nothing about it.
//!   2. **The UI reads a snapshot, never the loop.** `Status` is published
//!      once per cycle behind a mutex. A status window that could reach into
//!      the running controller would eventually be given a button that
//!      changes it mid-cycle.
//!   3. **Pause is not stop.** Pausing keeps the process, the spool and the
//!      session; it only stops asking the OS anything. That makes the gap a
//!      thing the person chose, which is exactly the distinction coverage
//!      spends its whole design preserving.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager};

use unvelt_agent::{
    allowlist, auth, backend, client, config, controller, handlers, instance, sources, spool,
    Probe, Status,
};

// macOS keeps two of unvelt's signals behind grants no Settings pane can
// hand out: the microphone pane has no "+" button, so an app that never asks
// is never granted, and the front window's title stays empty until the app
// is trusted for accessibility. `src/mac_perms.m`, compiled into the link by
// build.rs, asks for both. Asked before the collector thread spawns, so its
// first poll already sees what it may.
#[cfg(target_os = "macos")]
extern "C" {
    fn unvelt_request_mic();
    fn unvelt_prompt_accessibility() -> bool;
}

/// What the commands are allowed to reach.
///
/// Deliberately without the stop flag: quitting belongs to the tray menu,
/// which owns its own clone, and a `stop` reachable from the webview would be
/// one `invoke` away from a page ending collection for everyone.
struct App {
    status: Arc<Mutex<Status>>,
    paused: Arc<AtomicBool>,
    toggles: sources::Toggles,
    allow: allowlist::Allowlist,
    cfg: config::Config,
    /// False when another unvelt already holds the collector lock. This
    /// instance then shows the window and reads nothing, rather than
    /// double-counting the day.
    collecting: bool,
}

#[tauri::command]
fn status(app: tauri::State<'_, App>) -> Status {
    let mut st = current(&app);
    if !app.collecting {
        st.collecting = false;
        st.duplicate = true;
    }
    // Read here rather than cached at startup: signing in from this window
    // should change the answer without a restart.
    if let Some(mut s) = auth::Session::load(&app.cfg) {
        st.signed_in = true;
        st.email = s.email();
        st.uid = s.uid();
    } else {
        st.signed_in = false;
    }
    st
}

/// Whether this machine launches unvelt at login.
///
/// A switch rather than a silent registration. The app's whole argument is
/// that nothing happens to your machine without you deciding it, and quietly
/// adding yourself to someone's startup items is exactly the move that
/// argument is against -- even though continuous collection is the entire
/// point of the thing.
#[tauri::command]
fn autostart(app: tauri::AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: tauri::AppHandle, on: bool) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    let mgr = app.autolaunch();
    let _ = if on { mgr.enable() } else { mgr.disable() };
    mgr.is_enabled().unwrap_or(false)
}

/// Apps whose notification CONTENT this machine may keep, and every app that
/// has notified it, so the choice is made from a list rather than typed.
#[tauri::command]
fn content_apps(app: tauri::State<'_, App>) -> (Vec<String>, Vec<String>) {
    (app.allow.list(), app.allow.seen())
}

#[tauri::command]
fn set_content_app(app: tauri::State<'_, App>, id: String, on: bool) -> Vec<String> {
    app.allow.set(&id, on);
    app.allow.list()
}

#[tauri::command]
fn sources(app: tauri::State<'_, App>) -> Vec<sources::Source> {
    app.toggles.list()
}

/// Signals unvelt will collect but cannot yet. Shown without switches.
#[tauri::command]
fn planned() -> Vec<(&'static str, &'static str, &'static str)> {
    sources::PLANNED.to_vec()
}

#[tauri::command]
fn set_source(app: tauri::State<'_, App>, id: String, on: bool) -> Vec<sources::Source> {
    app.toggles.set(&id, on);
    app.toggles.list()
}

/// Forget the session on this machine.
///
/// Deletes the stored refresh token and nothing else. Events already collected
/// stay in the spool and still belong to the person -- signing out is not a
/// deletion request, and quietly treating it as one would lose data nobody
/// asked to lose. Deleting an account's data is `DELETE /v1/me`, elsewhere.
#[tauri::command]
fn sign_out(app: tauri::State<'_, App>) -> bool {
    std::fs::remove_file(auth::token_path(&app.cfg)).is_ok()
}

fn current(app: &tauri::State<'_, App>) -> Status {
    app.status.lock().map(|s| s.clone()).unwrap_or_else(|e| {
        // A poisoned lock means a collector thread panicked mid-publish. The
        // snapshot is still readable and still true as of that panic, which is
        // more useful to show than an error page.
        e.into_inner().clone()
    })
}

#[tauri::command]
fn probe(app: tauri::State<'_, App>) -> Probe {
    unvelt_agent::probe_once(&app.cfg)
}

#[tauri::command]
fn set_paused(app: tauri::State<'_, App>, paused: bool) -> bool {
    app.paused.store(paused, Ordering::Relaxed);
    paused
}

/// Sign in, on a thread of its own.
///
/// The OAuth flow blocks on a loopback listener until the browser comes back,
/// which on the UI thread would freeze the window for as long as the person
/// takes to choose an account — and look exactly like a crash.
#[tauri::command]
async fn login(window: tauri::Window) -> Result<String, String> {
    let cfg = config::Config::from_env();
    let handle = std::thread::spawn(move || auth::login(&cfg));
    let out = handle
        .join()
        .map_err(|_| "the sign-in thread panicked".to_string())?;
    match out {
        Ok(uid) => {
            // The collector re-reads the session on its next flush, so there
            // is nothing to restart -- but the window should stop saying
            // "not signed in" immediately rather than at the next poll.
            let _ = window.emit("signed-in", &uid);
            Ok(uid)
        }
        Err(e) => Err(e),
    }
}

/// Check for a new version, download it, and let it apply on next start.
///
/// Deliberately not a prompt. This is a tray app somebody opens once a month,
/// so an update waiting behind a dialog is an update that never installs --
/// and an out-of-date collector is one quietly missing signals the current one
/// reports. It swaps in when the app next starts, which for a login-launched
/// agent is the next reboot.
///
/// Every failure is silent by design. A machine that is offline, or behind a
/// proxy, or on a network that blocks GitHub, must keep collecting; an update
/// check is the least important thing this process does.
async fn check_for_update(app: tauri::AppHandle) {
    use tauri_plugin_updater::UpdaterExt;
    let Ok(updater) = app.updater() else { return };
    let Ok(Some(update)) = updater.check().await else {
        return;
    };
    let version = update.version.clone();
    if update.download_and_install(|_, _| {}, || {}).await.is_ok() {
        // Recorded rather than announced: the window can say "0.3.1 installs
        // when unvelt next starts" the next time somebody looks, and nothing
        // interrupts them before then.
        let _ = std::fs::write(config::state_dir().join("update_pending"), &version);
    }
}

fn show_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("status") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn main() {
    tauri::Builder::default()
        // Runs in the FIRST instance when a second is launched; the second
        // then exits on its own. Someone double-clicking the icon wants the
        // window, not a second copy of the tray icon and a puzzle about which
        // one is collecting.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_window(app);
        }))
        // `--hidden` so a login launch goes straight to the tray. Without it
        // the window would appear on every boot, which is how a background
        // collector becomes something people uninstall.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            status,
            probe,
            set_paused,
            login,
            sources,
            planned,
            set_source,
            sign_out,
            autostart,
            set_autostart,
            content_apps,
            set_content_app
        ])
        .setup(|app| {
            // Ask macOS for what its Settings cannot grant by hand (see the
            // extern block above). Both calls are no-ops once granted: the
            // mic request only fires while the answer is "not determined",
            // and the accessibility prompt only shows while untrusted. The
            // trusted bool is advisory for now; the prompt itself is the
            // delivery mechanism.
            #[cfg(target_os = "macos")]
            unsafe {
                unvelt_request_mic();
                unvelt_prompt_accessibility();
            }
            // The controller owns a `Box<dyn Backend>`, which is not Send, so
            // it is built inside its own thread and only its handles come
            // back out. That is also the honest arrangement: nothing outside
            // that thread can touch the loop.
            // One collector per user. The single-instance plugin above
            // already stops a second copy of the APP; this mutex is what
            // stops the CLI collector running alongside it, which the plugin
            // knows nothing about. Two loops post every signal twice under
            // one device id, and eids carry a millisecond, so the duplicates
            // do not dedupe -- the day simply reads as twice as busy.
            //
            // If the lock is somehow held anyway, the app still opens: it
            // becomes a window onto the collector that IS running rather than
            // refusing to start, and says so.
            let lock = instance::acquire();
            let collecting = lock.is_some();
            if collecting {
                // Held for the life of the process; the OS releases it if we
                // die, which is what makes it safe after a crash.
                std::mem::forget(lock);
            }

            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("unvelt-collector".into())
                .spawn(move || {
                    let mut cfg = config::Config::from_env();
                    // The app has no environment to read a uid from, and does
                    // not need one: identity is the token subject, and the
                    // envelope's uid is advisory. Take it from the session so
                    // an installed app is self-contained.
                    if cfg.uid.is_empty() {
                        if let Some(uid) = auth::Session::load(&cfg).and_then(|mut s| s.uid()) {
                            cfg.uid = uid;
                        }
                    }
                    let backend = backend::make_backend(&cfg);
                    let cl = client::ApiClient::new(&cfg);
                    let sp = spool::Spool::new(&cfg);
                    let hs = handlers::build_default(&cfg);
                    let mut ctl = controller::Controller::new(cfg.clone(), cl, sp, hs, backend);
                    let _ = tx.send((
                        ctl.status_handle(),
                        ctl.pause_flag(),
                        ctl.stop_flag(),
                        ctl.toggles(),
                        cfg,
                    ));
                    if collecting {
                        ctl.run();
                    }
                })?;
            let (status, paused, stop, toggles, cfg) = rx
                .recv()
                .map_err(|_| "the collector thread died before it started")?;

            app.manage(App {
                status,
                paused: paused.clone(),
                toggles,
                allow: allowlist::Allowlist::load(),
                cfg,
                collecting,
            });

            let open = MenuItem::with_id(app, "open", "Status…", true, None::<&str>)?;
            let pause = MenuItem::with_id(app, "pause", "Pause collection", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit unvelt", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &open,
                    &PredefinedMenuItem::separator(app)?,
                    &pause,
                    &PredefinedMenuItem::separator(app)?,
                    &quit,
                ],
            )?;

            // On by default, but registered once and never re-asserted: if
            // someone turns it off, it stays off. A daily "helpfully" re-added
            // startup entry is malware behaviour.
            {
                use tauri_plugin_autostart::ManagerExt;
                let marker = config::state_dir().join("autostart_asked");
                if !marker.exists() {
                    let _ = app.autolaunch().enable();
                    let _ = std::fs::create_dir_all(config::state_dir());
                    let _ = std::fs::write(&marker, "1");
                }
            }

            // Tray-only on every later launch, but not the first. Someone who
            // has never signed in would otherwise get a new icon in a tray of
            // twenty and no reason to think it wanted anything from them.
            //
            // `--hidden` is what the login launch passes, and it wins over
            // everything: a boot should never put a window in your face.
            // Once at startup, then every six hours. A collector that runs
            // for weeks would otherwise only ever update when someone thought
            // to restart it.
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    loop {
                        check_for_update(handle.clone()).await;
                        tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
                    }
                });
            }

            let hidden = std::env::args().any(|a| a == "--hidden");
            if !hidden && (auth::Session::load(&app.state::<App>().cfg).is_none() || !collecting) {
                show_window(app.handle());
            }

            let paused_for_menu = paused.clone();
            let stop_for_menu = stop.clone();
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("unvelt")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "open" => show_window(app),
                    "pause" => {
                        let now = !paused_for_menu.load(Ordering::Relaxed);
                        paused_for_menu.store(now, Ordering::Relaxed);
                        let _ = pause.set_text(if now {
                            "Resume collection"
                        } else {
                            "Pause collection"
                        });
                    }
                    "quit" => {
                        // Ask the loop to stop so it flushes what is in
                        // `current.jsonl`, then leave. Exiting first would
                        // drop up to a minute of events that were already
                        // observed, which is the one loss the spool exists to
                        // make impossible.
                        stop_for_menu.store(true, Ordering::Relaxed);
                        std::thread::sleep(std::time::Duration::from_millis(600));
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_window(tray.app_handle());
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Hide, do not destroy, and above all do not exit. Closing the
                // window is how someone dismisses a status panel; it is not
                // how they say "stop watching", which is what Pause is for.
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("unvelt: failed to start");
}
