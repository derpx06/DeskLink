# Deploying the DeskLink relay

Target: one DigitalOcean droplet in BLR1 (Basic, $6/month, 1 vCPU / 1 GB /
1 TB transfer), Ubuntu 24.04 LTS or Debian 12. Caddy terminates TLS; the relay
listens on `127.0.0.1:8080`. TURN comes from Cloudflare Realtime (default) or
from an optional coturn on the same droplet.

Throughout, `relay.example.com` stands for your domain.

## 1. Create the droplet

1. Create → Droplets → region **Bangalore (BLR1)** → Ubuntu 24.04 → Basic,
   Regular, $6/month. Enable **IPv6** and **Monitoring**. Add your SSH key.
2. Note the droplet's public IPv4 and IPv6 addresses.

## 2. DNS

At your DNS provider create:

| Type | Name | Value |
|---|---|---|
| A | relay.example.com | droplet IPv4 |
| AAAA | relay.example.com | droplet IPv6 |

If the zone is on Cloudflare, set these records to **DNS only** (grey cloud).
Cloudflare's proxy would hide client addresses behind its own and break the
per-network limits.

## 3. Firewall

```sh
sudo ufw default deny incoming
sudo ufw default allow outgoing
sudo ufw allow 22/tcp
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
# Only with the coturn profile:
sudo ufw allow 3478/udp
sudo ufw allow 3478/tcp
sudo ufw allow 5349/tcp
sudo ufw allow 49160:49260/udp
sudo ufw enable
```

Port 8080 is never opened: the relay binds to loopback and is reached only
through Caddy.

## 4. Install Docker (official repository)

```sh
sudo apt-get update
sudo apt-get install -y ca-certificates curl git
sudo install -m 0755 -d /etc/apt/keyrings
. /etc/os-release   # ID is ubuntu or debian
sudo curl -fsSL https://download.docker.com/linux/$ID/gpg -o /etc/apt/keyrings/docker.asc
sudo chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] \
  https://download.docker.com/linux/$ID $VERSION_CODENAME stable" \
  | sudo tee /etc/apt/sources.list.d/docker.list > /dev/null
sudo apt-get update
sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
```

Docker publishes no ports here (every service uses host networking), so its
iptables rules do not bypass ufw.

## 5. Cloudflare Realtime TURN key

1. Cloudflare dashboard → **Realtime** → **TURN Server** → **Create**.
2. Name it (for example `desklink-relay`) and create it.
3. Copy the **Turn Token ID** (this is `key_id`) and the **API Token**. The API
   token is shown once.

## 6. Configure

```sh
git clone <your DeskLink repository> desklink
cd desklink/relay/deploy
cp config.example.toml config.toml
```

Edit `config.toml`:

- `public_host = "relay.example.com"` (must match the domain exactly; it is
  part of what every client signs).
- `turn_backend = "cloudflare"` and `cloudflare.key_id` from step 5, or
  `turn_backend = "coturn"` (step 8), or `"none"`.
- Keep `listen = "127.0.0.1:8080"` and the default `trusted_proxies`.

Create `.env` beside `docker-compose.yml` (never commit it):

```sh
cat > .env <<'EOF'
RELAY_DOMAIN=relay.example.com
DESKLINK_RELAY_CF_TOKEN=<Cloudflare API token from step 5>
DESKLINK_RELAY_TURN_SECRET=
EOF
chmod 600 .env
```

## 7. Start

```sh
docker compose up -d --build
docker compose logs -f
```

Caddy fetches the certificate on first start (ports 80 and 443 must be
reachable). Then verify:

```sh
curl -fsS https://relay.example.com/healthz        # prints: ok
curl -fsS http://127.0.0.1:8080/metrics            # on the droplet only
```

`/metrics` answers 404 through Caddy by design.

## 8. Optional: self-hosted TURN (coturn profile)

Use this instead of, or as a fallback to, Cloudflare.

1. Generate the shared secret. It lives only in the untracked `.env`; both
   the relay and coturn (via `--static-auth-secret` in `docker-compose.yml`)
   read it from there, so it never enters the tracked `turnserver.conf`:

   ```sh
   SECRET=$(openssl rand -hex 32)
   sed -i "s/^DESKLINK_RELAY_TURN_SECRET=.*/DESKLINK_RELAY_TURN_SECRET=$SECRET/" .env
   ```

   Then edit `turnserver.conf` (it holds no secret; coturn runs as `nobody`,
   so leave it world-readable, mode 0644):

   - `realm=relay.example.com`
   - Uncomment the two placeholder `denied-peer-ip` lines at the end of the
     deny list and put the droplet's own public IPv4 and IPv6 there
     (`curl -4 ifconfig.me`, `curl -6 ifconfig.me`). Otherwise a TURN client
     can reach services listening on the droplet itself.
   - Set `bps-capacity` to what your droplet's transfer allowance can carry.

2. In `config.toml` set `turn_backend = "coturn"` and fix the three
   `coturn.urls` host names.

3. Give coturn a copy of Caddy's certificate. The coturn image runs as
   `nobody` (uid 65534; confirm with `docker compose run --rm coturn id`) and
   cannot read Caddy's root-only key, so copy it. Save as
   `/usr/local/sbin/desklink-turn-certs`:

   ```sh
   #!/bin/sh
   set -e
   DOMAIN=relay.example.com
   DIR=/root/desklink/relay/deploy
   SRC=$(docker volume inspect -f '{{.Mountpoint}}' deploy_caddy_data)/caddy/certificates/acme-v02.api.letsencrypt.org-directory/$DOMAIN
   install -d -m 0755 "$DIR/coturn-certs"
   install -m 0644 -o 65534 "$SRC/$DOMAIN.crt" "$DIR/coturn-certs/fullchain.pem"
   install -m 0600 -o 65534 "$SRC/$DOMAIN.key" "$DIR/coturn-certs/privkey.pem"
   cd "$DIR" && docker compose --profile coturn kill -s USR2 coturn 2>/dev/null || true
   ```

   Run it once now, then weekly from root's crontab:
   `17 4 * * 1 /usr/local/sbin/desklink-turn-certs`.
   (coturn reloads certificates on SIGUSR2.)

4. Open the TURN ports (step 3) and start:

   ```sh
   docker compose --profile coturn up -d --build
   ```

## 9. Point the apps at the relay

Both apps carry a build constant:

```
DEFAULT_RELAY_URL = "wss://relay.example.com/v1"
```

The relay upgrades only on path `/v1`; any other path answers 404. Rebuild the
desktop and Android apps after changing it.

## 10. Monitoring

- **Uptime:** an external HTTP check (UptimeRobot, Better Stack, Healthchecks,
  DigitalOcean Uptime) on `https://relay.example.com/healthz`, expecting `200`
  and body `ok`, every 1–5 minutes, alerting by email/push.
- **Synthetic TURN probe (coturn profile):** from root's crontab every 15
  minutes. It allocates a relay with REST credentials exactly as the apps do and
  pings a dead-man's-switch URL only on success:

  ```sh
  */15 * * * * . /root/desklink/relay/deploy/.env && docker run --rm --network host coturn/coturn:4 turnutils_uclient -y -n 3 -m 1 -W "$DESKLINK_RELAY_TURN_SECRET" -u probe relay.example.com >/dev/null 2>&1 && curl -fsS -m 10 https://hc-ping.com/<your-check-uuid> >/dev/null
  ```

- **Egress at 70 %:** the $6 droplet includes 1 TB/month of outbound
  transfer (≈3 Mbit/s averaged over a month). In DigitalOcean → Monitoring →
  Create alert policy → **Bandwidth, public outbound**, alert when above
  **2.2 Mbit/s for 1 hour** (70 % of the monthly pace). Add a billing alert
  (Billing → Billing alerts) at your expected total so overage is visible.
  Signaling alone is tiny; egress only grows with the coturn profile, where
  relayed media flows through the droplet.
- **Logs:** `docker compose logs relay` shows startup errors and a line of
  aggregate counts every five minutes. It never logs payloads, IP addresses or
  device fingerprints.

## 11. Updating

```sh
cd ~/desklink && git pull
cd relay/deploy
docker compose pull            # caddy (and coturn) images
docker compose up -d --build   # rebuilds the relay image
```

Restarting the relay drops every WebSocket; the apps reconnect on their own.
The relay saves its quota file on SIGTERM, so `docker compose restart` and
`up -d` lose nothing.

## 12. Backups

Only `state_file` matters (today's TURN quota counters, in the `relay_state`
volume). Everything else is in git, `.env` or Caddy's volume (which re-issues
certificates by itself). Losing the state file at worst resets one day of TURN
quotas.

```sh
docker run --rm -v deploy_relay_state:/s -v "$PWD":/b debian:bookworm-slim cp /s/state.json /b/state.json.bak
```

## 13. Expected cost

| Item | Cost |
|---|---|
| Droplet (BLR1, Basic $6) | ~$6/month, includes 1 TB outbound |
| Cloudflare Realtime TURN | first 1 TB/month free, then $0.05/GB |
| coturn profile | droplet transfer beyond 1 TB is billed by DigitalOcean |

Most sessions connect directly or on the LAN and never touch TURN, so a
small deployment stays at about $6/month.
