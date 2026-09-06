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
| `UNVELT_DID_SUFFIX` | `-rs` | see below |
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

**`UNVELT_DID_SUFFIX` exists for the parity window.** Both collectors run on
the same machine for a week, and the `eid` formulas are identical by design —
so with the same `did` the server would dedupe one against the other and the
diff would come back empty for entirely the wrong reason. Set it to the empty
string when this agent takes over for real.

## What is deliberately the same, and the one thing that is not

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

## Platform status

| | Windows | macOS | Linux |
|---|---|---|---|
| build + tests | native, verified | CI only | CI only |
| probes | native Win32 | subprocess | subprocess, X11 |
| lock/unlock | yes | — | — |
| fullscreen | yes | — | — |

macOS and Linux keep the subprocess probes the Python collector uses. Replacing
those with native frameworks is a change in behaviour as well as in language,
and doing both at once would spend the parity test. Native Core Audio, the
`usernoted` notification read and the rest arrive with the signals that need
them, in step 4.

Linux is X11 only. Wayland offers no cross-desktop way to ask what has focus,
and guessing per-compositor belongs in its own change rather than smuggled into
a port.
