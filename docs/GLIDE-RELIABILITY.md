# Glide reliability audit (row 3.9)

Owner: "it works sometimes and sometimes doesn't". This is the audit of the whole path (desk `device_links/glide/*`,
phone `plugins/inputdevicesreceiver/*` and `plugins/mousereceiver/*`), what was fixed, and the check for real hardware.

Status words: **fixed** (code + test), **by design** (read end to end, the guard already exists, test cited),
**not reproduced** (read, no defect found; no live run, see "What was not run").

## Why "sometimes"

Glide has five gates in series, and each one used to fail silently or with the wrong words:

1. the phone's link is feature-ready and it has reported `allowed: true` (the desk arms the barrier only then);
2. the phone's per-desk switch "Let <desk> control this phone" is on;
3. the phone's accessibility service (`MouseReceiverService`) is bound (it draws and moves the pointer);
4. the phone is awake and unlocked;
5. the desktop's InputCapture portal session is alive and armed.

Gate 3 is the one that flips on its own: ColorOS/Android switch the service off after every app update and force-stop
(STATE.md 16:44), and `uiautomator dump` restarts it. Gate 1 is reset whenever the link drops.

## Findings

| # | Sev | Cause | Result |
|---|---|---|---|
| 1 | S2 | Accessibility service off or not bound, switch on. The phone reported a plain `allowed: false`, so the desk said "Glide is off on X" with **Ask on phone**; pressing Allow set a switch that was already on and changed nothing (a loop). The desk never said what to do. | **fixed**: the phone reports `reason: "accessibility"` (new value of the existing optional `reason`, pinned in `glide_bodies.json` on both sides and the workspace copy). The desk says "Turn on DeskLink in X's Accessibility settings to use Glide" in Settings, Find anything (with the control ask where the phone takes it) and the tray, and does not auto-ask. The phone still hands the pointer back on a refused enter, and mid-glide when the service dies (`MouseReceiverService.onUnbind/onDestroy -> endActive(true)`, `onMotion`). Tests: `a_phone_with_its_accessibility_service_off_is_not_asked_to_allow_glide`, `glide_bodies_are_classified_as_written`, `theStateIsReportedOnFeatureReady...`, `DeskSharedFixturesTest`. |
| 2 | S2 | Glide setting lost when the same phone is paired again under a new id (`[Glide] setting now None`): `untrust_device(old)` clears it and nothing moved it. | **fixed**: `Config::move_glide` runs before the old record is dropped in `consider_replacement`; same edge, new id. Test `glide_follows_a_replaced_pairing`. A plain Unpair still clears it (the person asked). It survives a desk restart (stored in config) and a phone reconnect (the phone reports on every feature-ready). The phone's `glide_allowed_<desk id>` is keyed by the desk, so a phone-side re-pair of the same desk keeps it. |
| 3 | S3 | Settings row said "X needs to allow ..." while the link was simply down (no report yet). | **fixed**: "Waiting for X to connect. Glide is ready as soon as it does." |
| 4 | S3 | A refused crossing (screen off, locked) said only "X did not accept the pointer". | **fixed**: adds "Check that its screen is on and unlocked." |
| 5 | - | Edge crossing bounce (F35). | **by design**: `Glide.mayHandBack` (ARM_PX 48) and `Barrier::inside` (2 px). `GlideTest.theEntryEdgeGivesThePointerBackOnlyAfter...` covers Left/Top; Right/Bottom use the same code with `depth` per edge and the release test covers all four. Pointer bounds are `[0,w]x[0,h]` (`PointerFrames`), so the release condition (`y >= height`) is reachable. |
| 6 | - | Other edges, several monitors, fractional scaling. | **by design**: `Barrier::on` takes the outermost zone (longest edge wins, order-independent), `enter_packet` scales along the edge by `edgeLength`, so the entry point is proportional, not pixel-exact. Only the chosen zone's edge is a barrier (a second monitor reaching equally far is not). Fractional scaling uses the portal's logical zones, as the barrier requires; the pointer-position quirk in CLAUDE.md concerns `NotifyPointerMotionAbsolute` (remote desktop), not capture. Not measured live. |
| 7 | - | Link dozing / reconnecting / silent. | **by design**: barrier armed only while ready and not silent; a phone silent for 8 s mid-glide (`GLIDE_SILENT_AFTER`) gets the pointer taken back; a dozing phone is not judged by silence (LAN heartbeat thaws it). Enter has 1 s to be accepted (`ENTER_WAIT`) and nothing streams before, then a short back-off. |
| 8 | - | Phone screen off / locked / mid-call. | **by design**: refused at enter (`screenIsOn`, `locked()`), `allowed: false, reason: locked` reported, a lock mid-glide hands back within 500 ms. A call does not block Glide (the pointer is an overlay); not tested. |
| 9 | - | Another remote session (Phone Screen control) holds the lease. | **not reproduced**: glide mousepad packets are accepted while `Glide.isGliding` without a lease, and a lease ending does not release held input during a glide (`WebRtcSessionCoordinator` check). Two controllers at once share one pointer; not a supported combination. |
| 10 | - | Lost, reordered, duplicated press/release/move. | **by design**: only pure motion rides the unordered `InputRealtime` (a lost delta is a slightly short move). Press, release, scroll and keys ride the ordered reliable `InputReliable` (`channel_for`). Every way out sends `singlerelease` first (`Glide::let_go`) and the phone stops the swipe (`letGoOfThePointer`, `onFeatureLost`). Residual S4: a click on the reliable channel can overtake the last motion on the unordered one by a few ms. |
| 11 | - | Portal: refusals, dismissed dialog, dead session. | **by design**: `Consent` (once per run, remembers Cancel, "Turn on again"), Release failure falls back to Disable, `ZonesChanged`/`Disabled` release then re-arm (max 6 arms/min), a closed session or EIS reader releases and asks again when the person is back, a panic releases (`catch_unwind`), quitting releases within 2.5 s (`shutdown`). Status text is shown in Settings ("Glide is paused" + Turn on again). |
| 12 | - | Hand back failing. | **by design**, bounded: phone killed/link dead 8 s (above); user key: Esc+Super at any time, also while still entering; lid closed: the session locks, `Deactivated` releases; DeskLink restarting: `shutdown`; DeskLink killed hard: the compositor drops the capture with the process. Visible cue: the pointer reappears at the crossing point; the phone's notification and Home row end the glide. |
| 13 | - | Keyboard. | **by design**: keys go through `AccessibilityTyping.handle` only while gliding (or controlling); it refuses password fields and the lock screen; the phone refuses the whole glide while locked. The keyboard agent (keyboard-2) owns `AccessibilityTyping.kt`, untouched here. |
| 14 | S3 | Phone indicator and stop. | **by design**: the cursor overlay, an ongoing notification ("<desk> is using this phone", Stop action) and Home's row; idle end after 10 minutes without a packet (`IDLE_END_MS`). |

## What was not run

- The live emulator and a private desk instance with a fresh identity (the INCIDENT rule makes that a heavy harness
  and the emulator was in use by other rows); a real mouse crossing cannot be automated on GNOME. The desk side is
  covered by the pure state machine's tests (`glide/tests.rs`, 1000+ lines, including refusal, bounce, drag release,
  held input, back-off, link silence), the phone side by the Robolectric tests under
  `plugins/inputdevicesreceiver/`. The new wire value is pinned in the shared fixture on both sides.
- Fractional scaling and multi-monitor crossing are in the owner's checklist below.

## Install note

Both apps change (the phone reports the new reason; an old desk ignores an unknown `reason` and reads a plain
`allowed: false`, an old phone never sends it): install both, no ordering needed.

## Owner's real-hardware check (realme + a physical mouse, 5-10 minutes)

1. Settings > Glide onto realme: switch on, pick the edge where the phone sits. Row says "Ready: move the pointer off the
   ... edge" within a few seconds. Allow the GNOME "Capture Input" dialog once.
2. Cross the edge: the pointer appears on the phone within 1 s at the matching height. Wobble at the crossing: it must
   not bounce back. Move across the phone to the same edge: it returns to the laptop at the same height.
3. Repeat for each edge (Left, Right, Top, Bottom) and, if you have a second monitor, with it connected and unplugged.
4. On the phone: tap, drag (hold and move; releasing the button ends the drag), scroll wheel, right click (Back), middle
   (Home), type in a text field. Press Esc+Super: the pointer returns at once.
5. Drag, then pull the edge mid-drag or press Esc+Super: nothing stays held on the phone.
6. Lock the phone and cross: refused, the laptop says to unlock it; the pointer comes back. Turn the screen off and
   cross: "did not accept ... screen is on and unlocked".
7. Switch DeskLink's accessibility off in Android Settings: within seconds the laptop says "Turn on DeskLink in
   realme's Accessibility settings to use Glide". Cross: nothing is trapped. Switch it on again: Ready returns.
8. Force-stop DeskLink on the phone (or install over it): ColorOS usually switches accessibility off. Expect the same
   text on the laptop; this is the usual cause of "it stopped working".
9. Disconnect the phone's Wi-Fi mid-glide: the pointer is back on the laptop within about 8 s.
10. Re-pair the realme (Unpair there and here is NOT this test; use pairing a second time without unpairing): Glide
    stays on, same edge.
