# Improvements (1.3, phase 3.5): desk audit

A per-module reliability and performance pass over the desk after the
module split (693cf8f). Each module group was audited read-only by an
independent reviewer; every finding below was checked against its callers.
The phone app has its own list in `desklink-mobile/docs/IMPROVEMENTS.md`.

Severity: **S1** crash, hang, data loss or security; **S2** a feature breaks
or stays broken until a restart under realistic conditions; **S3** degraded,
rare, or a cheap performance win; **S4** cosmetic or code quality.

Status: **fixed** (with the test that pins it), or **listed** (with the
reason it was not changed now). Out of scope here: the phone-screen pipeline
(phase 3.4) and keyboard / control re-ask / accessibility detection (3.6).

## Transport, coordinator, peer, daemon (non-screen)

No S1 or S2. Production code has no reachable panic on peer data, poisoned
locks are recovered, queues are bounded and blocking reads have deadlines.

| ID | S | Where | Finding | Status |
|---|---|---|---|---|
| W1 | S3 | `coordinator/peer_worker.rs` `PeerWorkerGuard` | "superseded" is judged with the worker's own (stale) binding: after a generation change the old worker's exit runs the device-keyed reset and can tear down the new generation's session-password proof, sync and remote session. | listed: needs a two-generation harness test; the window is a worker busy in a sync handler while the phone restarts |
| W2 | S3 | `core/device_manager/mod.rs` | `sessions` never removes an unpaired id: a host cycling self-signed ids grows the map walked once a second. | listed: bounded by real LAN peers; remove-on-disconnect needs care with Requested pairing states |
| W3 | S3 | `peer_connection/callbacks.rs`, `coordinator/peer_worker.rs` | `thread::spawn` in a GStreamer callback (a spawn failure would abort across FFI); a failed event-worker spawn leaves the peer installed. | listed: only at the thread limit |
| W4 | S3 | `daemon/mod.rs`, `daemon/state.rs` | The reconciler takes the connection snapshot before the devices lock; a stale "not ready" applied after a fresh "ready" logs a spurious Disconnected for a second. | listed: cosmetic edge, needs the lock order changed |
| W5 | S3 | `transport.rs` | A direct send at the moment feature-ready flips can overtake the flush of queued features (Android has `sendOrderLock`). | listed: last-writer-wins packets only, rare |
| W6 | S3 perf | `coordinator/peer_worker.rs` | Per-event housekeeping (5-7 global locks, an allocation, the remote-session lock) runs for every file frame. | listed: perf.md measured DeskLink's own file thread at 3 %; transfers are SCTP-bound |
| W7 | S3 perf | `daemon/packet_handler.rs` | Every inbound feature packet takes the config lock and clones the capability lists. | listed: same reason |
| W8 | S4 | `coordinator/envelopes.rs` | A `Degraded` handover after feature-ready leaves a wedged peer. | listed: latent, Android never sends it |
| W9 | S4 | `core/device_manager/webrtc_state.rs` | A refused install drops the peer under the sessions lock. | listed |
| W10 | S4 | `daemon/mod.rs` | The 1 Hz reconciler waits for `get-stats` (up to 2 s) and LAN probes inline. | listed |
| W11 | S4 | `daemon/packet_handler.rs` | The LAN reader reads one byte per lock round. | listed: LAN carries only signalling |
| W12 | S4 | `peer_connection/mod.rs` | A pipeline that fails to start is dropped without going to Null (ICE sockets and threads linger). | **fixed**: set to Null on failure |

## Transfers and files

| ID | S | Where | Finding | Status |
|---|---|---|---|---|
| X1 | S2 | `transfer_manager/send.rs` `pump_sends` | Sends that met trouble are found over several rounds and were removed by stale indexes: a read error could end a healthy send, or panic past the end. | **fixed** (`highest_index_first`; `troubles_found_in_later_rounds_end_their_own_sends`) |
| X2 | S2 | `daemon/file_transfer.rs` cancel | Cancelling a transfer while the phone is away only cleared the row: the stored send was offered again at the next connection, and the phone's re-offer resumed a part. | **fixed** (`forget_transfer` removes the records, a re-offer is answered `cancel`; a cancel on a detached engine is remembered too; `a_transfer_cancelled_while_the_phone_is_away_is_not_offered_again`, `a_cancel_while_detached_is_remembered_for_the_re_offer`). Ceiling: the "cancelled" set is in memory |
| X3 | S3 | `folders/engine.rs` `place` | A stale arrival removes the newer `WANTED` entry before comparing hashes: the Bucket shows Syncing until the next manifest. | **fixed** (`take_wanted_matching`; `a_stale_arrival_keeps_the_newer_request`) |
| X4 | S3 | `folders/store.rs` | A transient read error at start quarantines the Bucket state and starts from defaults. | listed: needs `vault::open_or_quarantine` wiring and its own test |
| X5 | S3 | `image_limits.rs` | The pixel bound is checked after the first 64 KiB: a JPEG with a long EXIF header is decoded fully first. | listed: needs a header parser |
| X6 | S3 | `transfer_manager.rs` `cancel_transfer` | Blocks its caller up to 10 s; the Bucket worker calls it per row. | listed |
| X7 | S3 | `transfer_manager/store.rs` `place_file` | A cross-filesystem publish copies on the per-peer file thread. | listed |
| X8 | S3 | `transfer_manager/recv.rs` `restart_at` | A restart below the checkpoint leaves the on-disk checkpoint claiming blocks until the next one (2 s). | listed: needs damage plus a crash in that window |
| X9 | S3 | `transfer_manager/recv.rs` | A finished transfer offered again (its `complete` lost) is received twice. | listed: needs an on-disk done record |
| X10 | S3 | `transfer_manager/recv.rs` | A part that could not be placed counts toward the 16 GiB per phone for 7 days. | listed |
| X11 | S3 perf | `recv.rs` / `store.rs` | About 8 fsyncs and two directory scans per new incoming file. | listed: not measured |
| X12 | S4 | `store.rs` `enforce_partial_cap` | The global partial cap could remove another phone's live part. | listed |
| X13 | S4 | `store.rs` `sweep_stages` | A received file named `.desklink-*.stage` is swept after 7 days. | **fixed**: such a name is received as `_desklink-*` (`a_received_file_never_takes_a_staging_name`) |
| X14 | S4 | `vault.rs` `seal_later`, `folders/import.rs` | A direct seal under the pending lock; an import reports added when the queue dropped it. | listed |

## Terminals

| ID | S | Where | Finding | Status |
|---|---|---|---|---|
| T1 | S1 | `pty/launchers.rs` `run_with_timeout` | The login-PATH probe waited for end of file with no deadline: a profile that backgrounds a job keeping stdout (`nohup x &`) wedged the registry and froze the desk UI. | **fixed** (output read as it comes, deadline kept; `a_probe_whose_background_job_keeps_stdout_returns_by_its_deadline`) |
| T2 | S2 | `pty/session.rs`, `pty/desk.rs` (F40) | One shared session in two desk windows: when the one that set the size closed, the other kept showing the program at the closed window's size. | **fixed** (each window keeps its size; `closing_one_of_two_desk_windows_gives_the_size_to_the_other`) |
| T3 | S2 | `pty/registry.rs` `reap_idle` | The idle reaper closed a session shown in a desk window (an editor left open over a weekend). | **fixed** (`a_session_shown_at_the_desk_is_not_closed_as_idle`) |
| T4 | S3 | `pty/desk.rs` | A full input queue (a large paste into a busy program) closed the desk window's connection. | **fixed**: waited out for as long as the session lives, never dropped (a dropped middle could run a truncated command; review) (`a_full_input_queue_is_waited_out_never_dropped`) |
| T5 | S3 | `terminal_bridge/bridge.rs` | The pump serves attached terminals in a fixed order: a flooding one can starve another. | **fixed**: each pump starts one further on (`a_flooding_terminal_does_not_starve_another`) |
| T6 | S3 | `pty/desk.rs` `pump_output` | `finished()` is read after the output, so the last line can be lost at exit. | **fixed**: read before the output, as the phone bridge does |
| T7 | S3 perf | `pty/session.rs` | One global condvar wakes every pump on every 4 KiB read. | listed: not measured |
| T8 | S3 | `pty/session.rs` | A reaped session whose background job keeps the slave leaks its reader and writer threads. | listed |
| T9 | S3 | `pty/desk.rs` | `resolve_cwd` runs on the GTK thread (a hung mount freezes the UI). | listed |
| T10 | S4 | `terminal_previews.rs` | Each tick snapshots and re-parses every preview on the GTK thread. | listed |
| T11 | S4 | `pty/jobs.rs` | The job watch builds full descriptions once a second just for the sessions. | listed |

## Glide and remote input (non-keyboard)

| ID | S | Where | Finding | Status |
|---|---|---|---|---|
| G1 | S2 | `coordinator/share_worker.rs`, `desktop_input.rs` | A failed portal call dropped the rest of its batch, including a button release, and so did the input rate limit and a full portal queue: the button stayed held and every later move dragged on the desk. | **fixed**: after a failure the releases of held inputs still go; the rate limit and a full queue keep a batch's releases (a full queue: up to 250 ms) (`a_refused_move_still_lets_the_button_go`, `after_a_failure_only_held_inputs_are_released`, `only_releases_survive_a_rate_limited_or_full_input_batch`) |
| G2 | S3 perf | `glide/worker.rs`, `eis.rs` | 20 wakeups/s while a portal session exists (even with glide idle), plus a 100 ms EIS poll. | listed: needs a measured before/after with a portal session |
| G3 | S4 | `glide/worker.rs` | The settings are cloned 4 times a second to read one field. | listed |
| G4 | S4 | `text_focus.rs` | A 40 ms loop while a desktop share is active. | listed |

## Notifications, calls, clipboard, media, contacts, notes, sync

| ID | S | Where | Finding | Status |
|---|---|---|---|---|
| F1 | S1 | `desk_notifications.rs` `close_from_phone` | An async gio D-Bus call on a peer's event worker: gio panics on a thread that does not own the main context, so dismissing a desk notification on the phone killed the worker and dropped the peer. | **fixed** (blocking call on the desk thread; `the_daemon_never_makes_an_async_bus_call` pins all of `device_links/`) |
| F2 | S2 | `mpris.rs`, `daemon/packet_handler.rs` | A seek-bar drag queued one job per step on the one desk thread (300 ms sleep each): stale seeks replayed for seconds and the full queue dropped the final one. | **fixed** (newest absolute request per phone, player and kind; one settle thread per phone; `a_burst_of_seeks_runs_only_the_newest`, `a_burst_of_changes_waits_on_one_settle_thread_per_phone`) |
| F3 | S3 | `calls.rs` `MediaHold` | A call that ends before its pause job ran leaves the players paused. | **fixed**: the resume job takes the list when it runs (`a_call_that_ends_before_its_pause_ran_still_resumes_what_it_paused`) |
| F4 | S3 | `session_lock.rs` | One phone reconnecting voids another phone's correct password being hashed (it asks again). | **fixed**: the epoch is per device (`another_phones_reconnect_does_not_void_a_right_password`; the straddle test still holds) |
| F5 | S3 | `desk_features.rs` unlock | An approval inside the 500 ms settle window is dropped without an `unlockRefused` answer. | **fixed**: answered `not-ready`, nonce not spent (`an_approval_inside_the_settle_window_is_answered_not_dropped`). A flood that fills the desk queue still drops silently (16 waiting) |
| F6 | S3 | `phone_mpris.rs`, `mpris.rs` | `Position` is announced in `PropertiesChanged`, waking the desk's own reader. | listed |
| F7 | S3 | `media_art.rs` | A failed art fetch is cached until evicted. | **fixed**: tried again after 60 s (`a_failed_fetch_is_tried_again_after_a_while`) |
| F8 | S3 | `window/calls.rs`, `walk_away.rs` | Sync logind calls on the GTK thread per call event. | listed |
| F9 | S3 | `desk_notifications.rs` `start` | A transient monitor failure at login is permanent until restart. | **fixed**: up to 4 tries; only AccessDenied is final (`a_timeout_is_not_a_refusal`); the replaced monitor is closed blocking (an async close panicked on that thread and poisoned its lock), and the async-call pin now catches any gio call with a callback |
| F10 | S4 | `notes/wire.rs` | A full shelf refused edits of notes it already holds. | **fixed**: a full shelf takes what adds no entry (a straight edit, the same version, an older one); a sibling edit, which makes a conflict copy, is still refused (review: it grew the shelf past its cap) (`a_full_shelf_takes_straight_edits_but_no_conflict_copies`) |
| F11 | S4 | `pairing_qr.rs` | A code burned by a flood was not retired, so the real phone saw "no code" instead of "expired". | **fixed** |
| F12 | S4 | `app_icons.rs` `store` | `known()` (a disk read and decode) ran twice. | **fixed** |
| F13 | S4 | `dial_recents.rs` | Written with default modes, not atomic. | **fixed** (`private_fs`) |
| F14 | S4 | `unlock.rs` | Nonce compared with `==`. | listed: reachable only after the pref check, negligible |
| F15 | S4 | `media_art.rs`, `mpris.rs` | Two stale doc texts. | listed |
| - | note | `pairing.rs` | Prompt pairing compares 6 digits; QR pairing is the strong path. | design, wire-level: no change |

## Unlock, session lock, logind

Checked and sound: nonces single-use and spent on refusal, the per-device
cap, `apply_lock_request` reading only `setLocked`, `lock_report_notice`
never acting, QR proofs in constant time. Findings F4, F5, F14 above.

## Platform, config, autostart, UI, shell_dbus

No S1. Config writes are atomic and private, a damaged config is set aside,
the Shell1 sender gate is right and PropertiesChanged is sent only on change.

| ID | S | Where | Finding | Status |
|---|---|---|---|---|
| U1 | S2 | `application/`, `tray.rs` | KDE Wayland: a notification or tray click did not raise the window. GLib's notification backend drops the `ActivationToken`, and the tray had no `ProvideXdgActivationToken`. | **fixed** (token kept and passed with `set_startup_id` by every window raise; `a_fresh_activation_token_is_used_once_and_a_stale_one_never`). Needs the Plasma VM to confirm. Ceiling: a token from another app's click in the 3 s before is taken too (the signal does not say whose notification it was without tracking GLib's ids) |
| U2 | S2 | `autostart.rs` | An AppImage registered its mount's binary (`/tmp/.mount_*`) to start at login. | **fixed** (`$APPIMAGE`; pinned in the autostart test) |
| U3 | S3 | `window/refresh.rs` | While a mouse button is held, events (call alerts, pings) wait for the release. | listed: the event arm also rebuilds lists, so the gate cannot simply move |
| U4 | S3 perf | `shell_dbus/mod.rs` | The top-bar properties are rebuilt each tick on desktops that are not GNOME. | listed |
| U5 | S3 | `device_links/asks.rs` | `probe_buttons` makes sync D-Bus calls on the GTK thread once a minute (500 ms each). | **fixed**: on its own thread, blocking calls only (no unit test: the answer logic keeps its tests) |
| U6 | S4 | `search_provider.rs`, `kde.rs` | `ActivateResult` / `Run` have no caller gate. | listed: any process that can call them already runs as the user and can read DeskLink's key |
| U7 | S3 perf | `platform/mod.rs` | `/etc/os-release` was read on the GTK thread every Home tick. | **fixed** (read once) |
| U8 | S4 | `window/overview/mod.rs` | Home's rebuild key holds all 25 notifications. | listed |
| U9 | S4 | `window/sidebar.rs` | Accessible labels set every tick. | listed |
| U10 | S4 | `application/mod.rs` | `connect_map` added on every activation. | listed |
| U11 | S4 | `shell_dbus/mod.rs` | One retry timer per throttled Hello. | listed |
| U12 | S4 | `autostart.rs` | The entry is written non-atomically; `%` not escaped in `Exec`. | listed |

### The open minor issues from phase 2.6 (row 3.5 item 2)

| Issue | Status |
|---|---|
| sway tray symbolic icon invisible on a dark bar | **fixed**: the glyph is `currentColor` with Plasma's `current-color-scheme` sheet and GTK's symbolic grey `#bebebe`; GTK/St hosts still recolour, Plasma follows its scheme (`tray::` test) |
| Xfce empty default-action button | **fixed**: on xfce4-notifyd (`GetServerInformation`) a click is an "Open" button instead of GLib's unlabelled default action (`only_xfce_notifyd_gets_an_open_button_for_the_click`) |
| KDE Wayland notification click does not raise | **fixed** as U1 |
| Phone wording on view-only desktops | **fixed** on the phone (see its list) |
| Sidebar Search placeholder ellipsizes at 150 % | **fixed**: the word is never ellipsized (`the_sidebar_search_word_is_never_cut`) |
| "File moved or deleted" card shows an image icon for non-images | **fixed** (`a_missing_file_shows_its_own_kind_and_only_an_image_a_broken_picture`) |
| F40 desk terminal size after one of two windows closes | **fixed** as T2 |

## For other phases

- Screen (3.4): nothing reliability-relevant on the desk side.
