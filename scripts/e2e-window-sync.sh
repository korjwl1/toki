#!/usr/bin/env bash
# End-to-end window sync against a REAL toki-sync server in Docker.
#
# Covers the gap unit tests structurally cannot: TCP, the auth handshake, wire
# framing, server-side storage, and read-back. The client-side unit tests stop
# at the sender trait; the server-side ones start at an already-decoded frame.
#
# Build the image first (the server's Cargo.toml uses a local [patch] for
# toki-sync-protocol, so the build context must include that crate):
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
if cargo test --test window_sync_e2e -- --nocapture 2>&1 | tee "$R/e2e.log" | grep -q "test result: ok"; then
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
print(sum(len(v) for v in (d.get('windows') or {}).values()))
")
[ "$N" = "3" ] && ok "server returns all 3 uploaded windows" || bad "server returned $N windows, expected 3"

say "6. idempotent resend merges in place"
echo "  (waiting out the 60s server-side per-user throttle)"
sleep 62
# Second upload contributes a HIGHER peak for the same windows, so the
# field-wise merge is observable rather than a no-op.
export TOKI_E2E_PEAK=8800
OUT6=$(cargo test --test window_sync_e2e -- --nocapture 2>&1 | grep -o "OUTCOME=[A-Za-z]*" | head -1)
[ "$OUT6" = "OUTCOME=Sent" ] && ok "resend accepted after the throttle window" || bad "resend not accepted: $OUT6"
N2=$(curl -s "http://127.0.0.1:19091/api/v1/toki/query?query=windows&start=$(( $(date +%s) - 86400 ))&end=$(( $(date +%s) + 86400 ))" \
      -H "authorization: Bearer $ACCESS" | python3 -c "
import json,sys
d=json.load(sys.stdin); print(sum(len(v) for v in (d.get('windows') or {}).values()))
")
[ "$N2" = "3" ] && ok "still 3 rows — no duplicates" || bad "resend duplicated rows: $N2"
PEAK=$(curl -s "http://127.0.0.1:19091/api/v1/toki/query?query=windows&start=$(( $(date +%s) - 86400 ))&end=$(( $(date +%s) + 86400 ))" \
      -H "authorization: Bearer $ACCESS" | python3 -c "
import json,sys
d=json.load(sys.stdin)
rows=[w for v in (d.get('windows') or {}).values() for w in v]
print(max(w['peak_pct'] for w in rows) if rows else -1)
")
python3 -c "import sys; sys.exit(0 if abs(float('$PEAK') - 88.02) < 0.05 else 1)" \
  && ok "higher peak from the resend won the merge (peak_pct=$PEAK)" \
  || bad "field-wise merge did not take the max peak: $PEAK"

say "7. per-user throttle survives a reconnect"
# Each cargo test run is a FRESH TCP connection: under the old per-connection
# throttle this would be accepted, which is the bug the per-user limiter fixes.
OUT7=$(cargo test --test window_sync_e2e -- --nocapture 2>&1 | grep -o "OUTCOME=[A-Za-z]*" | head -1)
[ "$OUT7" = "OUTCOME=NotSent" ] && ok "immediate resend on a new connection was throttled" || bad "throttle bypassed by reconnect: $OUT7"

say "8. server logs clean"
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
