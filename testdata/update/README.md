# Update-system shared fixtures v3 (TEST ONLY)

Master copy: `desklink-updates/fixtures/` = `phone_connect/docs/update/fixtures/`. Byte-identical copies live in
`DeskLink/testdata/update/` and `desklink-mobile/src/test/resources/shared/update/`; `protocol/check-fixtures.sh`
fails when one differs. Never edit a copy: change `desklink-updates/scripts/make-fixtures.py`, run it, then
`desklink-updates/scripts/sync-fixtures.sh`. The generator is deterministic (`git diff` is empty on a re-run).
`scripts/dl_update.py` is the reference implementation of everything below; where this text and the code differ, fix the text.

**Everything here is a throwaway test artefact. `test1.private.pem` / `test2.private.pem` are published on purpose and
are NOT the project's signing keys.** Their kids are `test1`/`test2`, never `k1`/`k2`, so a real key set cannot collide.
**Rule for the clients:** a RELEASE build embeds ONLY the real keys; a unit test must fail if any kid starting with
`test` (or any key of `keys.json` here) is present in a release key configuration. Debug builds may compile the test keys in.

| File | What |
|---|---|
| `keys.json` | `{"keys":[{"kid","alg":"ed25519","public":<base64url, 32 bytes>}]}`: `test1` current, `test2` next |
| `vector.json` | one Ed25519 vector: public key, payload bytes, signature (hex and base64url) |
| `manifests/<id>.json` | the bytes a server would send (envelope, section 2.1 of docs/UPDATES.md) |
| `payloads/<id>.payload.json` | the unsigned payload of that manifest, pretty printed (humans only; apps never read it) |
| `cases.json` | the table: per manifest a list of runs, each with the expected verdict |
| `test*.private.pem` | the test private keys (so a debug build/test can sign its own manifests) |

## Two opposite rules, stated first

- The **ENVELOPE** has EXACTLY the keys `v`, `kid`, `payload`, `sig`: an unknown, missing or duplicated key is `parse`.
- The **PAYLOAD** IGNORES unknown fields at every level (top level, channels, components, notes, downloads). Additive
  fields never bump `schemaVersion`; a Rust client must NOT use `deny_unknown_fields` on the payload (fixture
  `unknown-fields-everywhere`).

## JSON: RFC 8259 grammar only, plus a mandatory custom validator

The envelope and the payload are parsed with **RFC 8259 grammar and nothing else**. Common library parsers are NOT enough:
`org.json`, Gson and lenient `serde_json` settings accept comments, trailing commas, single quotes, unquoted keys, `01`, `+1`,
`.5` and silently keep the LAST duplicate key. Every client therefore needs its own pre-validator or strict parser that enforces
all of the following (reason `parse` for each), BEFORE any value is used:

1. Bytes are strict UTF-8 (invalid sequences, UTF-16, a leading BOM are errors). No leading or trailing garbage; only JSON
   whitespace (space, tab, CR, LF) around and between tokens.
2. **Depth:** the top-level object is depth 1; arrays and objects both count; depth 32 is accepted, 33 is rejected. Checked while
   scanning (before building values), so a 32000-deep body cannot overflow the stack (Kotlin: scan first or catch `Throwable`).
   Brackets inside strings do not count. Fixtures at exactly 32/33 (arrays, and mixed objects/arrays) and 20000/32000 deep.
3. **No duplicate keys**, compared AFTER unescaping (`"kid"` and `"kid"` are duplicates), at any depth.
4. **No `NaN`/`Infinity`, no lone surrogates** (`"\ud800"`) in any key or string.
5. **Numbers: the integer token grammar only, `0|[1-9][0-9]*`.** A sign (so `-0` and `-1`), a fraction (`100.0`, `.5`), an exponent
   (`5e1`, `1e999`) or a leading zero is a parse error of the WHOLE payload, not a field-level problem. Integers above the i64
   range are out of range for the field that reads them (timestamps: 0..2^63-1, see below). Where an integer is required, a
   string, bool or `null` is invalid for that field.
6. **null is not absent.** `"rollout": null` or `"minSupported": null` makes the component invalid (dropped); only a MISSING key
   means the default.

## Envelope and verification order (identical in both apps)

`{"v":1,"kid":"test1","payload":"<base64url>","sig":"<base64url>"}`. The signature covers the **decoded payload bytes as served**;
nothing is re-serialised, and the payload bytes are NOT parsed or inspected before the signature check passes.

1. Body size <= 65536 bytes, else `oversized` (checked before parsing; exactly 65536 is accepted).
2. Parse the envelope (rules above), else `parse`. Exactly four keys; `v` is the integer token `1` (`1.0`, `"1"`, `1e0`, `true`: `parse`).
3. The envelope shape is checked first: exactly the four keys, `v` the integer 1, `kid` a STRING (else `parse`). Then `kid` is looked up
   exactly (case-sensitive) in the client's key set; unknown = `signature`, decided BEFORE `payload`/`sig` are looked at (so an unknown
   kid with a non-string payload is `signature`). A known kid with a `payload` or `sig` that is not a string is also `signature`
   (fixtures `envelope-unknown-kid-nonstring-payload`, `envelope-known-kid-nonstring-payload`, `envelope-known-kid-nonstring-sig`,
   `envelope-nonstring-kid` = `parse`).
4. `payload` and `sig` are CANONICAL base64url: alphabet `A-Za-z0-9-_` only (`+`, `/` are not accepted), no `=` padding, no length = 1
   (mod 4), and re-encoding the decoded bytes must reproduce the text exactly (rejects non-zero trailing bits). The empty string
   is a valid encoding of zero bytes (and fails verification). Else `signature`.
5. `sig` is 64 bytes and Ed25519 strict: `S < L` (L = 2^252 + 27742317777372353535851937790883648493; a malleable S + L is rejected). Else `signature`.
6. The payload bytes are parsed under the same profile (else `parse`).
7. `schemaVersion` must be an integer token: missing, `< 1` (including 0) is `parse`; `> 1` is `schema` and nothing else is looked at.
8. `issuedAt` and `expires` must be integer tokens in 0..2^63-1 (else `parse`).
9. Time, in this ORDER when several apply: `issuedAt <= now + 3600` (else `expired`: the code set has no clock code, so a manifest from
   the future, or ANY manifest when the device clock is far in the past, is `expired`); `0 < expires - issuedAt <= 22 days` (else
   `expired`); `now <= expires` (else `expired`); `issuedAt >= lastSeenIssuedAt` (else `replay`; equal is fine).
   The complete failure order is therefore: size, parse (envelope), signature, parse (payload), schema, parse (types), expired, replay.
10. **Ratchet:** after a manifest is fully accepted the client stores `lastSeenIssuedAt = min(issuedAt, now)`, never the raw
   `issuedAt` (`ratchetVectors`, case `legit-after-future-ish`).

## Components (a bad component is DROPPED; the other component and channel are unaffected)

- `version`, `minSupported`: `MAJOR.MINOR.PATCH`, ASCII digits only, no leading zeros, no build metadata or pre-release, exactly
  three parts, each part <= 2147483647 (2147483647 accepted, 2147483648 rejected). Parts are compared as numbers (0.10.0 > 0.9.0),
  never as text. `minSupported` must be <= `version` (otherwise the component is invalid: there is no "Required" for a version that
  does not satisfy it). `minSupported` absent = 0.0.0.
- `rollout`: absent = 100; present = an integer token 0..100, else the component is invalid.
- Android `versionCode`: integer token 1..2100000000 (missing, string, null, overflow = invalid).
- `size` (optional, display only, up to 2^40): read it as a 64-bit integer (Kotlin `Long`, Rust `u64`) or ignore it; never let it
  throw or overflow.
- **The client's OWN installed version:** if it does not parse as strict semver (a build bug), that component is `UpToDate`
  (nothing is offered, nothing is Required). Fixtures `installed-*-version-malformed`.
- Beta falls back to the stable entry only when that is newer. Decision order: component absent or invalid -> UpToDate;
  **`rollout == 0` -> UpToDate (a PAUSE: nothing is shown, not even for a manual check or Required)**; installed < minSupported
  -> `Required`; not newer (Android: `versionCode <= installed`) -> UpToDate; `bucket >= rollout` (not manual) -> UpToDate;
  skipped / snoozed (not manual); else `Offer`. Rollouts 1..99 are ignored by `Required` and by a manual "Check now".
- `installId`: exactly 32 LOWERCASE hex characters (16 bytes). Anything else (uppercase, wrong length, non-hex) is invalid: regenerate
  it (`installIdChecks`). `bucket = u32_be(sha256(installId_bytes || "|" || component || "|" || version)[0..4]) % 100`.

## Cosmetic fields (invalid parts are dropped, never fatal)

- `notes`: not a list = none; each entry that is not an object, has a title that is not a string of 1..40 Unicode SCALARS, or a detail
  that is present, non-empty and longer than 80 scalars or not a string, is skipped; then the first 8 kept. Scalars are code points
  (not UTF-16 units, bytes or graphemes: `e` + U+0301 is 2). `glyph` that is missing, unknown or not a string = `star`; empty `detail` = none.
- **Allowed text** (notes, `requiredReason` 1..160 scalars): none of the code points below and none of `<` `>` `&`. The set is FROZEN
  here and independent of any Unicode-version table (Python, Rust and Java disagree about U+180E, U+13430-13440 and unassigned
  code points; unassigned code points, private use, noncharacters and other space separators such as U+00A0 and U+3000 are ALLOWED):
  - Cc: U+0000-001F, U+007F-009F (so NUL, tab, newline, CR, U+0085)
  - Zl, Zp: U+2028-2029
  - Cs: U+D800-DFFF
  - Cf: U+00AD, U+0600-0605, U+061C, U+06DD, U+070F, U+0890-0891, U+08E2, U+180E, U+200B-200F, U+202A-202E, U+2060-2064,
    U+2066-206F, U+FEFF, U+FFF9-FFFB, U+110BD, U+110CD, U+13430-13440, U+1BCA0-1BCA3, U+1D173-1D17A, U+E0001, U+E0020-E007F
  A note failing this is skipped (so a ZWJ emoji sequence is skipped); a bad `requiredReason` is ignored (the Required state stays).
  **Client rule: all manifest text is rendered as PLAIN text (GTK `use_markup(false)`, Compose `Text`), never parsed as markup.**
- `released`: a real `YYYY-MM-DD` date: ASCII digits only, year 0001-9999, leap days honoured (2024-02-29 and 2000-02-29 are real,
  2026-02-29 and 2100-02-29 are not), else absent. Display only.
- URLs: exact pinned shapes (`url_ok` in `scripts/dl_update.py`; `urlChecks` and the context table `urlContextChecks`
  `[url, component, field, version, allowed]`; a `null` component, field or version means "not applicable: any"). Match the WHOLE string (full match; never a regex `$` that tolerates a final newline).
  Scheme `https` and the host are compared after lower-casing (`GITHUB.COM` is fine); host `github.com` or `play.google.com`; no
  userinfo, no port except an explicit 443, no fragment, no query (Play: exactly `?id=org.desklink.mobile`), no `%`, no backslash,
  whitespace, control or non-ASCII character, no empty or `.`/`..` segment, no trailing-dot host, length <= 300. GitHub paths:
  `/derpx06/DeskLink/releases/tag/v<semver>` and `/derpx06/desklink-mobile/releases/tag/v<semver>` (`notesUrl`/`releaseUrl` of that
  component's repo only), `/derpx06/DeskLink/releases/download/v<semver>/desklink-<semver>-x86_64.tar.gz` (desktop `tarball` only),
  `/derpx06/desklink-mobile/releases/download/v<semver>/desklink-<semver>.apk` (android `apk` only); the version in the URL (both
  places) must equal the component's `version`. Anything else is dropped (the field is absent); `downloads.play.id` is ignored unless it
  is `org.desklink.mobile`. Other hosts are NOT manifest URLs; the app builds those links locally.

## cases.json (schema 3)

`defaults` (merged under every run): `installed {desktop, android, androidCode}` (0.3.0 / 0.3.0 / 20300), `channel` (`beta`),
`now` (1795003600), `lastSeenIssuedAt` (1795000000), `installId`, `manual`, `prefs` (`skipped`, `snoozedVersion`, `snoozedUntil`),
`keys` (the client's key set: kids of `keys.json`).

A run's `expect` is either a verdict string for the whole manifest (`Ignore:<reason>`) or `{desktop, android}`:

| Verdict | Meaning |
|---|---|
| `UpToDate` | nothing to offer (absent/dropped component or channel, not newer, outside the rollout, paused) |
| `Offer:<v>` | `Available`: show the popup for `<v>` (Android compares `versionCode`, `<v>` is only the name) |
| `Required:<v>` | installed < `minSupported` |
| `Skipped:<v>` / `Snoozed:<v>` | would be `Offer` but for the stored preference |
| `Ignore:<reason>` | whole manifest rejected; reason in `oversized parse signature schema expired replay` |

Per-run checks: `expectNotes` applies to the DESKTOP component's notes and `expectNotesAndroid` to the ANDROID component's (the exact
notes after the rules above, hand-written in the generator, never computed by it; `detail` null when absent; when a run has only
`expectNotes`, the android component's notes are not asserted). The remaining checks apply to the desktop component: `expectRequiredReason` (string or null), `expectReleased` (string or null), `expectUrls`
(`"<component>.<field>": "kept" | "dropped"`, field `notesUrl releaseUrl tarball apk`, or `playId` = `"ignored"`).
Other tables: `ratchetVectors` (`stored = min(issuedAt, now)`), `bucketVectors`, `urlChecks` (`[url, allowed]`), `urlContextChecks`, `installIdChecks`.

`oversized.json` is a real 95 KB file (valid signature, body > 65536). `body-exactly-65536.json` / `body-65537.json` pad a valid envelope
with trailing whitespace. The 20000/32000-deep bodies are the deepest that fit under 64 KiB. `payload-empty` cannot carry a valid
signature (openssl will not sign zero bytes): it only pins that an empty `payload` string fails as `signature`.
