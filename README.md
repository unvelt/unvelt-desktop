# unvelt-desktop

The desktop collector. One repository, three platforms, and installers for all
of them built from a single source tree — all unsigned, all at zero cost. The
plan this implements lives in `poc-compound-tracker/docs/desktop-plan.md`.

Right now this is **step 2 of that plan: the headless agent, at parity with the
Python collector it replaces.** The tray, the consent cards and the auth flow
are step 3; notifications and media are step 4. Nothing here ships a signal the
Python collector did not already send.

```
crates/unvelt-agent/     the agent — the whole of step 2
  src/backend/           OS probes, one module per platform
  src/handlers/          one kind of signal each; the controller drives them
  src/controller.rs      the loop clock, and the only thing that sees sleep
  src/spool.rs           the durable buffer, and the source of truth
  src/envelope.rs        the wire shape, shared with the Android collector
```

## Running it

```sh
cargo run -- --probe        # every OS probe once: what does this machine allow?
cargo run -- --once         # one full cycle, spooled, then exit
cargo test                  # 20 tests, no network, no database
UNVELT_UID=<subject> cargo run
```

`--probe` is the first thing to run on a new machine. It answers "which of
these can this OS actually see" one line at a time, which is the question the
whole design turns on — a probe that returns `None` is a signal that platform
does not give us, and every handler is written to stay quiet rather than
inventing a zero.

## Configuration

Environment only, and the defaults are the Python collector's.

| variable | default | |
|---|---|---|
| `UNVELT_UID` | — | **required**; the agent refuses to collect without it |
| `UNVELT_URL` | `https://compound-kx.duckdns.org` | |
| `UNVELT_INGEST_KEY` | — | sent as `X-Compound-Key` |
| `UNVELT_DID` | `<os>-<hostname>` | |
| `UNVELT_DID_SUFFIX` | *(empty)* | see below |
| `UNVELT_SAMPLE_SEC` | `5` | base tick and focus poll |
| `UNVELT_IDLE_SEC` | `180` | idle → away |
| `UNVELT_RESAMPLE_SEC` | `120` | re-emit an unchanged foreground |
| `UNVELT_INPUT_WINDOW_SEC` | `60` | one `desktop.input` per window |
| `UNVELT_CONTEXT_SEC` | `60` | wifi, power, monitors |
| `UNVELT_SSID_SEC` | `900` | macOS SSID can cost ~4s; its own clock |
| `UNVELT_FLUSH_SEC` | `60` | |
| `UNVELT_HB_SEC` | `300` | |
| `UNVELT_SPOOL_DIR` | per-OS state dir | |
| `UNVELT_SPOOL_MAX_BYTES` | 64 MiB | oldest batches dropped, with a `meta.gap` |
| `UNVELT_DEBUG` | off | print every event as it is spooled |

**`UNVELT_DID_SUFFIX` was for a parity run that is no longer happening.** The
plan had been to run this agent beside the Python collector for a week and
compare; the suffix kept the two apart, because identical `eid` formulas under
one device id would have made the server dedupe them against each other and
return a flawless comparison of nothing.

That run was dropped, and for a good reason: the Python collector was itself
never validated beyond a couple of manual runs, so agreeing with it would have
proved very little and disagreeing with it would have proved nothing at all. A
baseline has to be trusted before it is worth measuring against. What replaced
it is this crate's own tests, plus a direct probe-for-probe comparison of the
two backends on real hardware.

So the suffix defaults to empty and the agent simply takes over the device id
the history is already under.

## What is deliberately the same, and the one thing that is not

(The parity framing below is kept because it explains why the port is shaped
the way it is, not because a side-by-side run is still planned.)

Same: the envelope and its field order, the `eid` formulas, the spool file
naming and rotation, the JSONL-over-gzip contract, the five handlers and every
threshold in them, and the category list. (One cosmetic exception: keys *inside*
`p` come out alphabetically rather than in insertion order. `jsonb` normalises
key order on write, so neither ordering survives to the database.) All of it is a port, because the
acceptance test for this step is that a week of side-by-side capture produces
the same events, and a rewrite dressed as a port makes every difference
ambiguous.

Not the same: **the app identifier on macOS.** The Python collector sends the
display name — "Google Chrome" — as the app key, so the same browser is
`Google Chrome` from a Mac and `com.android.chrome` from the phone and the two
never fold into one app. Migration 0015 keys `derived.app_labels` on
(platform, pkg) to fix that, and it requires the collector to send the
platform-native identifier. So macOS now sends the **bundle id** as the key and
the display name as `inventory.app`'s label, in the same release, because a
release that changed one without the other would leave every macOS breakdown
reading `com.google.Chrome`.

Windows was never affected: it has always sent the executable base name, which
is already platform-native, so five thousand existing events keep their
`dim_key` and nothing in the history forks.

## Installing

macOS, through our own tap:

```sh
brew tap unvelt/tap
brew install --cask unvelt
```

The tap exists because Homebrew's official cask repo stopped accepting
unsigned casks on 1 September 2026, and unvelt is unsigned. Third-party taps
are unaffected. The cask clears the `com.apple.quarantine` flag in postflight
and says so in its caveats — Gatekeeper refuses an unsigned app that carries
it, and Sequoia removed the Control-click bypass, so without that the install
ends at a dialog with no way past it.

Windows and Linux: the artifacts on the release page. Windows shows a
SmartScreen warning on first run, which is what an unsigned installer costs.

`.github/workflows/release.yml` builds all four targets on a tag. macOS is
built there and nowhere else — the development machine is Windows, and a
`.app` needs a Mac.

## Platform status

| | Windows | macOS | Linux |
|---|---|---|---|
| build + tests | native, verified | CI only | CI only |
| probes | native Win32 | subprocess | subprocess, X11 |
| lock/unlock | yes | — | — |
| fullscreen | yes | — | — |
| notifications | yes (`wpndatabase.db`) | planned (`usernoted`) | — |
| media | yes (SMTC) | planned (3 layers) | planned (MPRIS2) |

Notifications and media are **off until you turn them on**. On Android both sit
behind an OS permission granted by hand; on Windows they sit behind nothing —
the notification store is a readable file and SMTC answers any process that
asks. When the platform declines to put the question, the app's own switch is
the only place it gets put.

macOS and Linux keep the subprocess probes the Python collector uses. Replacing
those with native frameworks is a change in behaviour as well as in language,
and doing both at once would spend the parity test. Native Core Audio, the
`usernoted` notification read and the rest arrive with the signals that need
them, in step 4.

Linux is X11 only. Wayland offers no cross-desktop way to ask what has focus,
and guessing per-compositor belongs in its own change rather than smuggled into
a port.

## The app

`crates/unvelt-app` is the tray application: the same collector, embedded, with
a status window in front of it. Step 3 of the plan.

```sh
cargo run -p unvelt-app          # tray icon; the window opens from it
cargo build --release -p unvelt-app
```

It is a library the app embeds, not a sidecar it supervises. A sidecar would
mean two binaries to sign, two to update, an IPC channel to define, and a
supervision problem in both directions — for a component that is a five-second
polling loop. One process instead: one updater, one signature, and a window
that can be destroyed and rebuilt without touching collection, which is the
reason Tauri was chosen over Electron in the first place.

Three rules the app holds to, all of them about the window mattering less than
the collection:

- **Closing the window never stops collecting.** Close hides; it does not
  destroy and does not exit. The collector thread does not know a window exists.
- **The UI reads a snapshot, never the loop.** `Status` is published once per
  cycle behind a mutex. A window that could reach into the running controller
  would eventually be given a button that changed it mid-cycle.
- **Pause is not stop.** Pausing keeps the process, the spool and the session,
  and only stops asking the OS anything — so the gap is one the person chose,
  which is the distinction coverage exists to preserve. Quit asks the loop to
  stop first, so `current.jsonl` is flushed rather than dropped.

The window shows what the machine is actually answering right now, and a probe
the OS refuses renders as *not available* rather than as `0` or `false` — the
same rule the handlers follow when they stay quiet instead of inventing a
reading.

Measured at rest on Windows: **3.9 MB binary, 28.5 MB resident.** The Electron
equivalent of this window would be a ~120 MB installer idling at 150–300 MB.
