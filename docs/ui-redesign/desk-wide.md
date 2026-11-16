# Desk wide layouts and the 3.7 redesign (design note)

Phase 3.7, agent "ui", 2026-11-17. Builds on `docs/UI-REDESIGN.md` (the spec) and its tokens; nothing here
changes a token or a word of the glossary. Owner requests: "use a better layout for larger displays",
"improve the UI of the desktop much more", "the complete ecosystem: extension, phone, desktop as one".

## 1. Shared decisions (desk, top bar, phone: one product)

The top-bar redesign (row 3.8, topbar-2) and the phone follow these. Where UI-REDESIGN.md already says it, this
section only points there.

| Decision | Rule | Source |
|---|---|---|
| Colour roles | accent = actions and selection only; mint = connected/done; amber = needs you; red = failed/unpair; coral = the other device is using this one (live, with End). Colour never alone: a word and a glyph shape too. | UI-REDESIGN 2.1 |
| Status words | Connected · Connecting… · Reconnecting… · Can't reach · Needs your OK · Live. Detail line: "over Wi-Fi · 12 ms", "Check that <name> is on and on your network". Said **once** per screen (the hero, the pill); a control that cannot work is hidden or folded, not labelled "Can't reach" again. | UI-REDESIGN 3.0, this note 3 |
| Type scale | Hero 28/34 700; title 22/28 600; card title 16/22 600; body 15/22; secondary 13/18 dim; label 14/20 600; caption 12/16. Desk in `em` of the UI font; phone in `sp`; numbers in progress, sizes and times use tabular figures. Nothing under 12. | UI-REDESIGN 2.5 |
| Icons | One family: Material Symbols Rounded 400, outlined; filled for the selected nav item and an "on" toggle. 16 inline, 20 in buttons and chips, 24 in rows, bars and tiles. | UI-REDESIGN 2.6 |
| Cards | `surface` + 1 px hairline, radius 16, padding 16-20, header = card title + at most one trailing text button; one primary (filled) action per card at most, the rest tonal or text. | UI-REDESIGN 2.8.8 |
| Banners | Radius 16, one action at the end (under the words when they would break). Live (coral) always has End. | UI-REDESIGN 2.8.11 |
| Layout rule | Extra width is used for **structure** (columns, a list beside its detail, more tiles), never for longer lines: text measures stay at most ~75 characters (cards clamp at ~800 px). | this note |
| Large text | Words are never broken inside: rows put their trailing control under the words when the longest word would not fit (phone `TextAndTrailing`, shot check `MidWord`). | 3.7 batch 1 |
| Glossary | computer / phone / device name; Bucket ("a folder that is the same on both devices"); send, receive, open, pair, unpair, end, allow, turn on/off, try again; sentence case; no relay/peer/WebRTC/handover in user text. | UI-REDESIGN 3.0, 5.1 |

## 2. Desk: breakpoints

The window already has `narrow` (window ≤ 480 sp) and `compact` (page ≤ 760 sp). New: `wide`, the page area
≥ 1200 sp (a window of about 1500 px with its 300 px sidebar; at 1100 the two 560 px columns wrapped the hero's status line), on `page_bin`. Pages read `DesklinkWindow::apply_layout`.
The sidebar may grow to 300 px (20 % of the window).

## 3. Home

Before (owner's 1920x1080 screenshot, images/14.png): one 944 px column centred, 400 px empty each side, five
tiles then "More" alone, everything stacked, scrolling on a 1080p screen.

```
wide (page ≥ 1200 sp, clamp 1640)                     one column (below), same reading order
+----------------------------+----------------------+   hero
| hero (phone, status, Send) | Finish setting up    |   activity / Now playing / Connecting
| activity / Now playing     | (steps)              |   tiles (6, 3x2 or 2x3: never an orphan)
| tiles 3 x 2                | Terminals            |   Notifications | Files
| Notifications | Files      |                      |   Finish setting up
+----------------------------+----------------------+   Terminals
| Everything DeskLink does (both columns)            |   Everything DeskLink does
+----------------------------------------------------+
```

Windows of about 1100-1500 px keep the single column on purpose: two columns there are 560 px or less, which
wraps the hero's status line and squeezes the tiles. Notifications and Files sit under the tiles, so the two
columns end near each other instead of the left one stopping after the tiles.

Tile columns come from the width the row really has and the width a tile needs with its words on one line:
6 if they fit, else 3, else 2 (6 divides by both).

Away (Can't reach, or the reconnect ladder gave up while the phone still reads as paired: `phone_away`): the hero
says it once with Try again and what to check; the tiles, Send files / Send clipboard, Reply, the setup steps and
the feature list are left out until the phone is back (each said "Can't reach" again, or was greyed: seven times
on the owner's screen). The setup steps are still listed in Settings (Setup), including the ones done at this
computer. What came in (notifications, files, terminals on this computer) stays, dimmed as last known.

## 4. Next pages (later batches, in this order)

| Page | Wide layout | Also |
|---|---|---|
| Sidebar | up to 300 px; counts on Notifications / Terminals / Files (transfers); clearer selected row; status dot in the phone switcher | compact icon rail below ~900 px is a later step |
| Notifications | **done (batch 3)**: grouped by app, the app with the newest first; a heading ("Signal · 3", counting what the filter shows) only over an app with more than one card, since a lone card already names its app; filter All / Can reply (has Reply: chats, often email) / Other; Clear all clears what the filter shows ("Clear these"); filtering to nothing shows the empty state saying so. One readable column (900 px) at every width. Day headers and Mark all read were left out: the phone sends no read state to act on. | the new card from 3.3 stays |
| Files | **done (batch 4)**: 1900 px, up to eight cards a line (seven Bucket tiles at 2560) | grid/list toggle not done |
| Notes & clipboard | **done (batch 4)**: the list up to 420 px beside the note; the note keeps its 760 px measure | |
| Settings | **done (batch 5)**: the ten pages as a side list beside the open page (720 px), the pair centred; Back still leads to the full list (Setup, quick switches); drill-down below the breakpoint | every row stays |
| Terminals | **done (batch 4)**: four previews a line, centred in 1600 px | |
| Contacts | **done (batch 5)**: the chosen contact beside the list (Call, Message, Copy there); a dialog below the breakpoint | |

Phone (after the desk Home and sidebar): tablets and foldables get a two-pane Home (the hero and actions beside
what is going on), Files, Notes and Settings as list beside detail; batches by screen with Robolectric shots at
font 1.0/1.3/2.0, light and dark.

## 5. How each batch is checked

Widget tests that the layout switches at its breakpoint and that every page fits every width (360, 560, 800,
1100, 1440, 1920, 2560 px); harness shots light/dark/HC/RTL at those widths, before and after, under
`.tools/screenshots/p37-ui/`; gate; an independent review before merge.

Screenshots (before/after per batch, with an INDEX.md) live in the workspace at `.tools/screenshots/p37-ui/`, not in this repo.
