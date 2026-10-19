# desklink-relay

A small WebSocket relay that lets a paired DeskLink phone and desktop exchange
WebRTC signaling when they are not on the same LAN, see whether the other is
online, and obtain short-lived TURN credentials. Media never passes through
it. Deployment: [`deploy/DEPLOY.md`](deploy/DEPLOY.md).

```sh
cargo test
```

## Privacy properties

- **No payload access.** `data` is opaque to the relay; clients encrypt it end
  to end. The relay only stamps the authenticated sender.
- **No accounts.** A device's address is `base64url(SHA-256(certificate DER))`
  of the certificate it already uses for pairing.
- **Strangers learn nothing.** Presence and delivery exist only between two
  devices that have each allowed the other. A device that is not mutual gets
  the same `offline` answer whether the other is online or not.
- **IPs and fingerprints stay in memory.** They are used for limits only. The
  quota file stores salted hashes, and the salt is replaced every UTC day.
- **Aggregate logs.** stderr carries startup/config errors and periodic counts;
  `/metrics` carries counters. Neither contains payloads, IPs or fingerprints.

## Protocol v1

WebSocket on `wss://<host>/v1` (the relay itself speaks plain WS behind
Caddy). Text frames, one JSON object each, tagged by `"t"`. Types live in
`src/protocol.rs`.

| Direction | Frame |
|---|---|
| S→C on connect | `{"t":"challenge","v":1,"minV":1,"nonce":"<b64url, 32 bytes>"}` |
| C→S | `{"t":"auth","v":1,"cert":"<b64 DER cert>","sig":"<b64 signature>"}` |
| S→C | `{"t":"welcome","fp":"<your fp>"}` |
| C→S | `{"t":"allow","fps":["<fp>",...]}` (≤ 32, replaces the set) |
| S→C | `{"t":"presence","fp":"<fp>","online":true\|false}` |
| C→S | `{"t":"send","to":"<fp>","data":<any JSON>}` |
| S→C | `{"t":"msg","from":"<fp>","data":<as sent>}` |
| C→S | `{"t":"turn","peer":"<fp>"}` |
| S→C | `{"t":"turnCreds","peer":"<fp>","iceServers":[{"urls":[...],"username":"...","credential":"..."}],"ttl":<secs>}` |
| S→C | `{"t":"error","code":"offline"\|"not-allowed"\|"turn-denied"\|"turn-quota","to":"<fp>"}` |

**Signature** — SHA-256 with the certificate's key (ECDSA P-256 DER signature,
or RSA PKCS#1 v1.5) over the UTF-8 bytes of

```
desklink-relay-auth-v1\n<nonce exactly as received>\n<relay public host>
```

with no trailing newline. `cert` and `sig` use the standard base64 alphabet
with padding (line breaks are tolerated). `fp = base64url_nopad(SHA-256(cert DER))`.

**Mutual** — A and B are mutual while both are online and each lists the other
in its latest `allow`. Each side gets `presence online:true` when that starts
and `online:false` when it ends (disconnect, revoked allow, or replacement).
A fresh connection has an empty allow set, so send `allow` after every
`welcome`.

**Errors** — `not-allowed`: you have not allowed `to` yourself. `offline`:
anything else that prevented delivery (peer offline, peer has not allowed you,
or peer too far behind). `turn-denied`: not mutual/online, no backend, or
backend failure. `turn-quota`: 20 grants per pair, or 48 per IPv4 address /
IPv6 /64, or 192 per IPv6 /48, per UTC day used up. Credentials last one hour
(`ttl` 3600); request new ones on each rebuild.

**Close codes**

| Code | Meaning |
|---|---|
| 4400 | malformed frame, binary frame, second `auth`, oversized `allow` |
| 4401 | bad certificate/signature, frame before `auth`, or no `auth` within 10 s |
| 4409 | a newer connection authenticated with the same certificate |
| 4426 | client `v` below `minV` |
| 4429 | rate, auth-attempt or connection limit exceeded, or too far behind on reading |
| 1009 | frame larger than 16 KiB before `welcome`, 512 KiB after |

The relay pings every 20 s and drops a socket silent for 60 s.

## HTTP

`GET /healthz` → `200 ok`. `GET /metrics` → Prometheus counters (blocked at
Caddy; scrape it on the host). Other paths → 404.
