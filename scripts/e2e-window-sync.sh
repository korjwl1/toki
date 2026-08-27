#!/usr/bin/env bash
# End-to-end window sync against a REAL toki-sync server in Docker.
#
# Covers the gap unit tests structurally cannot: TCP, the auth handshake, wire
# framing, server-side storage, and read-back. The client-side unit tests stop
# at the sender trait; the server-side ones start at an already-decoded frame.
#
# Build the image first from the workspace root with the release-pinned
# toki-sync-protocol dependency:
#   cd ~/Documents/toki_projects
#   docker build -f toki/scripts/Dockerfile.e2e -t toki-sync:e2e .
#
# Then:  bash toki/scripts/e2e-window-sync.sh
# Docker socket: colima puts it under the user home, and HOME is redirected
# below for daemon isolation, so resolve it before that happens.
export DOCKER_HOST="${DOCKER_HOST:-$(docker context inspect --format '{{.Endpoints.docker.Host}}' 2>/dev/null)}"
# HOME is redirected below for daemon isolation; rustup/cargo and the docker
# context both live under the real home, so pin them explicitly.
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
FAIL=0
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
say() { echo; echo "── $* ────────────────────────────────"; }
ok()  { echo "  ✅ $*"; }
bad() { echo "  ❌ $*"; FAIL=1; }

# Must live under the real $HOME: colima only shares the home directory into
# the VM, so a /tmp bind mount silently becomes an empty directory.
R="$HOME/.toki-e2e"; rm -rf "$R"; mkdir -p "$R/home/.codex/sessions/2026/08/05" "$R/home/.config/toki"
export HOME="$R/home"

say "1. server container"
docker rm -f toki-e2e >/dev/null 2>&1
mkdir -p "$R/cfg"
cat > "$R/cfg/toki-sync.toml" <<'CFG'
[server]
http_port = 9091
tcp_port = 9090
[auth]
jwt_secret = "e2e-secret-not-for-production"
[storage]
sqlite_path = "/data/toki-sync.db"
[events]
backend = "fjall"
fjall_path = "/data/events.fjall"
CFG
docker run -d --name toki-e2e -p 19090:9090 -p 19091:9091 \
  -e TOKI_ADMIN_PASSWORD=adminpass1234 \
  -e TOKI_SYNC_CONFIG=/etc/toki-sync/config.toml \
  -v "$R/cfg/toki-sync.toml:/etc/toki-sync/config.toml:ro" \
  toki-sync:e2e >/dev/null
for i in $(seq 1 40); do
  curl -sf http://127.0.0.1:19091/health >/dev/null 2>&1 && break
  sleep 1
done
curl -sf http://127.0.0.1:19091/health >/dev/null && ok "server healthy" || { bad "server never became healthy"; docker logs toki-e2e | tail -20; exit 1; }

say "2. capability advertised"
CAP=$(curl -s http://127.0.0.1:19091/api/v1/capabilities)
echo "  $CAP"
echo "$CAP" | grep -q '"sync_windows_v1":true' && ok "sync_windows_v1 advertised" || bad "capability missing — client would never send windows"

say "3. device auth"
TOK=$(curl -s -X POST http://127.0.0.1:19091/login -H 'content-type: application/json' \
      -d '{"username":"admin","password":"adminpass1234"}' | python3 -c "import json,sys;print(json.load(sys.stdin).get('access_token',''))")
[ -n "$TOK" ] && ok "admin login" || { bad "login failed"; exit 1; }
DC=$(curl -s -X POST http://127.0.0.1:19091/device/code -H 'content-type: application/json' -d '{}')
DEVCODE=$(echo "$DC" | python3 -c "import json,sys;print(json.load(sys.stdin)['device_code'])")
USERCODE=$(echo "$DC" | python3 -c "import json,sys;print(json.load(sys.stdin)['user_code'])")
curl -s -X POST http://127.0.0.1:19091/device/approve -H "authorization: Bearer $TOK" \
  -H 'content-type: application/json' -d "{\"user_code\":\"$USERCODE\"}" >/dev/null
TR=$(curl -s -X POST http://127.0.0.1:19091/device/token -H 'content-type: application/json' \
     -d "{\"device_code\":\"$DEVCODE\"}")
ACCESS=$(echo "$TR" | python3 -c "import json,sys;d=json.load(sys.stdin);print(d.get('access_token',''))")
REFRESH=$(echo "$TR" | python3 -c "import json,sys;d=json.load(sys.stdin);print(d.get('refresh_token',''))")
[ -n "$ACCESS" ] && ok "device approved and tokenized" || { bad "device token failed: $TR"; exit 1; }
echo "$TR" > "$R/token.json"

say "4. real client -> real server over TCP"
export TOKI_E2E_ADDR=127.0.0.1:19090
export TOKI_E2E_JWT="$ACCESS"
# Fixed anchor so every run uploads the SAME logical windows.
export TOKI_E2E_ANCHOR=$(( $(date +%s) * 1000 ))
cd "$REPO"
if cargo test --test window_sync_e2e windows_reach -- --ignored --nocapture 2>&1 | tee "$R/e2e.log" | grep -q "test result: ok"; then
  grep -q "skipping" "$R/e2e.log" && bad "test skipped (env not seen)" || ok "client delivered the batch and the server acked"
else
  bad "client->server sync failed"; tail -20 "$R/e2e.log"
fi

say "5. rows readable back from the server"
WIN=$(curl -s "http://127.0.0.1:19091/api/v1/toki/query?query=windows&start=$(( $(date +%s) - 86400 ))&end=$(( $(date +%s) + 86400 ))" \
      -H "authorization: Bearer $ACCESS")
echo "$WIN" | head -c 400; echo
N=$(echo "$WIN" | python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: print(-1); raise SystemExit
print(sum(1 for v in (d.get('windows') or {}).values() for w in v if w['limit_id'].startswith('e2e_limit_')))
")
[ "$N" = "3" ] && ok "server returns all 3 uploaded windows" || bad "server returned $N windows, expected 3"

say "6. idempotent resend merges in place"
echo "  (waiting out the 60s server-side per-user throttle)"
sleep 62
# Second upload contributes a HIGHER peak for the same windows, so the
# field-wise merge is observable rather than a no-op.
export TOKI_E2E_PEAK=8800
OUT6=$(cargo test --test window_sync_e2e windows_reach -- --ignored --nocapture 2>&1 | grep -o "OUTCOME=[A-Za-z]*" | head -1)
[ "$OUT6" = "OUTCOME=Sent" ] && ok "resend accepted after the throttle window" || bad "resend not accepted: $OUT6"
N2=$(curl -s "http://127.0.0.1:19091/api/v1/toki/query?query=windows&start=$(( $(date +%s) - 86400 ))&end=$(( $(date +%s) + 86400 ))" \
      -H "authorization: Bearer $ACCESS" | python3 -c "
import json,sys
d=json.load(sys.stdin)
print(sum(1 for v in (d.get('windows') or {}).values() for w in v if w['limit_id'].startswith('e2e_limit_')))
")
[ "$N2" = "3" ] && ok "still 3 rows — no duplicates" || bad "resend duplicated rows: $N2"
PEAK=$(curl -s "http://127.0.0.1:19091/api/v1/toki/query?query=windows&start=$(( $(date +%s) - 86400 ))&end=$(( $(date +%s) + 86400 ))" \
      -H "authorization: Bearer $ACCESS" | python3 -c "
import json,sys
d=json.load(sys.stdin)
rows=[w for v in (d.get('windows') or {}).values() for w in v if w['limit_id'].startswith('e2e_limit_')]
print(max(w['peak_pct'] for w in rows) if rows else -1)
")
python3 -c "import sys; sys.exit(0 if abs(float('$PEAK') - 88.02) < 0.05 else 1)" \
  && ok "higher peak from the resend won the merge (peak_pct=$PEAK)" \
  || bad "field-wise merge did not take the max peak: $PEAK"

say "7. per-user throttle survives a reconnect"
# Each cargo test run is a FRESH TCP connection: under the old per-connection
# throttle this would be accepted, which is the bug the per-user limiter fixes.
OUT7=$(cargo test --test window_sync_e2e windows_reach -- --ignored --nocapture 2>&1 | grep -o "OUTCOME=[A-Za-z]*" | head -1)
[ "$OUT7" = "OUTCOME=NotSent" ] && ok "immediate resend on a new connection was throttled" || bad "throttle bypassed by reconnect: $OUT7"

say "8. event sync and window sync share one connection"
# Window frames travel on the SAME TCP connection as event sync. A framing or
# state bug in the new path would break a feature that already shipped.
sleep 62
MIX=$(cargo test --test window_sync_e2e event_sync_and_window_sync -- --ignored --nocapture 2>&1)
echo "$MIX" | grep -q "OUTCOME_AFTER_EVENTS=Sent" \
  && ok "windows accepted on a connection that just carried events" \
  || bad "window sync broke after an event batch: $(echo "$MIX" | grep -o 'OUTCOME_AFTER_EVENTS=[A-Za-z]*')"
echo "$MIX" | grep -q "EVENTS_AFTER_WINDOWS_ACK=" \
  && ok "events still accepted after a window frame ($(echo "$MIX" | grep -o 'EVENTS_AFTER_WINDOWS_ACK=[-0-9]*'))" \
  || bad "event sync broke after a window frame"

say "9. multi-device field-wise merge"
# The reason this feature has a server at all. Two devices on ONE account
# upload the SAME window with different values; the stored row must be the
# field-wise merge, not last-writer-wins.
export TOKI_E2E_ANCHOR=$(( $(date +%s) * 1000 ))
MD=$(cargo test --test window_sync_e2e two_devices -- --ignored --nocapture 2>&1)
echo "$MD" | grep -q "DEVICE_dev-b_SENT=true" \
  && ok "a second device on the same account was accepted" \
  || bad "second device refused — a limiter is blocking the merge path"
MERGED=$(curl -s "http://127.0.0.1:19091/api/v1/toki/query?query=windows&start=$(( $(date +%s) - 86400 ))&end=$(( $(date +%s) + 86400 ))" \
  -H "authorization: Bearer $ACCESS" | python3 -c "
import json,sys
d=json.load(sys.stdin)
rows=[w for v in (d.get('windows') or {}).values() for w in v if w['limit_id']=='shared_limit']
if len(rows)!=1: print('BADROWS', len(rows)); raise SystemExit
w=rows[0]
ok = (w['peak_pct']==77.0 and w['finalized'] and w['maxed_out']
      and w['time_to_100_ms']==4200000 and w['active_ms']==90000)
print('MERGED_OK' if ok else 'MERGED_WRONG %s' % w)
")
[ "$MERGED" = "MERGED_OK" ] && ok "one row, max peak kept, flags OR-ed, later observation won" || bad "merge wrong: $MERGED"

say "10. server logs clean"
ERRS=$(docker logs toki-e2e 2>&1 | grep -ciE "\berror\b|panic" || true)
[ "$ERRS" = "0" ] && ok "no errors/panics in server log" || { bad "$ERRS error lines in server log"; docker logs toki-e2e 2>&1 | grep -iE "\berror\b|panic" | tail -5; }

echo
if [ "$FAIL" = "0" ]; then echo "════ ALL E2E CHECKS PASSED ════"; else echo "════ E2E FAILURES PRESENT ════"; fi

# ── ClickHouse backend ────────────────────────────────────────────────────────
# The fjall path above does not exercise the ClickHouse code at all, and the
# `updated_at` migration is data-destroying if wrong. To run it:
#
#   docker run -d --name toki-ch -p 18123:8123 clickhouse/clickhouse-server:latest
#   # create the PRE-updated_at table (21 cols, ReplacingMergeTree(observed_ts_ms)),
#   # insert legacy rows, then start toki-sync with backend = "clickhouse".
#
# Verified 2026-08-05 against clickhouse-server:latest:
#   - migration ran once, preserved all rows, engine became
#     ReplacingMergeTree(updated_at), updated_at derived from observed_ts_ms
#   - restarts did NOT re-run it (idempotent)
#   - with the live table renamed away (crash between the two RENAMEs),
#     recover_interrupted_migration adopted toki_windows_old with zero loss
#   - uploads merged field-wise: resend kept the row count and took the max peak
#   - 2500 windows with every string at the 64-byte maximum: the client capped
#     at 2000 and the payload stayed under the server's 1 MiB limit

# ── Full live path: real daemon with sync ENABLED ─────────────────────────────
# The checks above drive SyncClient directly against the server. This one runs
# the real thing end to end and is worth repeating by hand before a release:
#
#   1. start the server container (as in step 1)
#   2. toki settings sync enable --server 127.0.0.1 --sync-port <p> \
#        --http-port <p> --no-tls --device-name e2e-live
#      then approve the printed code via POST /device/approve with an admin
#      token, so no browser click is needed
#   3. start the daemon with window_tracking = true and wait out one
#      WINDOWS_SYNC_INTERVAL (300s)
#   4. GET /api/v1/toki/query?query=windows and count the rows
#   5. toki settings sync disable  — the enable step writes real credentials
#      into the login keychain (service "toki-sync"), so this cleanup is not
#      optional
#
# Verified 2026-08-05: the daemon uploaded 21 rows on its own (3 Claude —
# five_hour, seven_day, weekly_fable — and 18 Codex), event sync and window
# sync ran together on one connection, and the capability probe logged
# "server supports windows sync".

# ── Monitor's server-read path ────────────────────────────────────────────────
# Plan Fit reads merged multi-device rows from the server, not just from the
# local daemon. Verified 2026-08-05 by decoding a REAL server response
# (/api/v1/toki/query?query=windows) against ServerQueryClient's WindowRow
# CodingKeys: envelope {schema, windows}, all 19 row fields present, nothing
# undecoded, nothing required missing.

# ── Backward compatibility with a server that predates window sync ────────────
# Build an image from toki_sync's pre-feature commit and point the client at it.
#
# Verified 2026-08-05 against toki_sync main (85177eb):
#   - GET /api/v1/capabilities returns 404, which probe_windows_capability maps
#     to Some(false) -> WindowsCapability::Unsupported, so the client never
#     sends the frame
#   - forcing the send anyway (calling windows_sync_step directly) confirms why
#     that gate exists: the old server logs
#     "dropping TCP connection: unknown msg_type: 36" (0x24) and closes the
#     socket WITHOUT a SyncErr. Since event sync shares that connection, an
#     ungated send would tear down event sync on every window cycle.
#
# ── Provider data reality (same date, this machine) ───────────────────────────
# Codex stopped issuing the 5-hour window for plan_type=prolite between
# 2026-07-02 and 2026-07-14 ("secondary": null); the payload shape did not
# change. Free-tier history never had it either. A monitor showing only the
# weekly window for Codex is therefore correct, not a bug — replaying June
# rollout files (which do carry both) produces both session and weekly rows.
