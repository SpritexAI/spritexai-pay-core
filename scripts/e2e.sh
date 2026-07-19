#!/usr/bin/env bash
# SpritEXAI Pay — end-to-end feature check.
# Author: Mohammad Sijan (SpritexAI).
#
# Boots the real release-ish binary and drives the whole money path against a live
# HTTP server: create charge -> HMAC-signed SMS webhook -> settlement -> merchant
# webhook delivery -> reconciliation. Asserts real status codes and a real signed
# delivery. This is the feature gate CI runs after the unit/integration suite.

set -euo pipefail

PORT=8188
SMS_SECRET="e2e-sms-secret"
WH_SECRET="e2e-wh-secret"
DB="e2e.db"
BASE="http://127.0.0.1:${PORT}"

cd "$(dirname "$0")/.."
rm -f "$DB" "$DB"-shm "$DB"-wal

echo "==> building"
cargo build --locked --quiet

fail() { echo "E2E FAIL: $*" >&2; exit 1; }

# Local receiver that verifies the outbound HMAC and records the delivery.
python3 - "$WH_SECRET" >/tmp/e2e_recv.log 2>&1 <<'PY' &
import http.server, hmac, hashlib, sys
SECRET=sys.argv[1].encode()
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n=int(self.headers.get('content-length',0)); body=self.rfile.read(n)
        sig=self.headers.get('X-SpritexAI-Signature','')
        ok=hmac.compare_digest(sig, hmac.new(SECRET, body, hashlib.sha256).hexdigest())
        print("DELIVERED" if ok else "BADSIG", flush=True)
        self.send_response(200); self.end_headers()
    def log_message(self,*a): pass
http.server.HTTPServer(('127.0.0.1',9188),H).serve_forever()
PY
RECV=$!

DATABASE_URL="sqlite://${DB}?mode=rwc" SMS_HMAC_SECRET="$SMS_SECRET" \
  WEBHOOK_HMAC_SECRET="$WH_SECRET" PORT="$PORT" \
  ./target/debug/spritexai-pay >/tmp/e2e_server.log 2>&1 &
SRV=$!

cleanup() { kill "$SRV" "$RECV" 2>/dev/null || true; rm -f "$DB" "$DB"-shm "$DB"-wal; }
trap cleanup EXIT

# Wait for readiness.
for _ in $(seq 1 30); do
  curl -sf "${BASE}/health" >/dev/null 2>&1 && break || sleep 0.3
done
curl -sf "${BASE}/health" >/dev/null || fail "server did not become healthy"

echo "==> create charge"
code=$(curl -s -o /tmp/e2e_chg.json -w '%{http_code}' -X POST "${BASE}/v1/charges" \
  -H 'content-type: application/json' \
  -d '{"order_id":"E2E-1","amount_minor":50000,"callback_url":"http://127.0.0.1:9188/hook"}')
[ "$code" = "201" ] || fail "charge create expected 201, got $code"

echo "==> deliver signed SMS"
BODY='{"gateway":"bkash","body":"You have received Tk 500.00 from 01710000000. TrxID E2ETX1"}'
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$SMS_SECRET" -hex | sed 's/.*= //')
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "${BASE}/v1/webhooks/sms" \
  -H "x-signature: ${SIG}" -d "$BODY")
[ "$code" = "202" ] || fail "sms webhook expected 202, got $code"

echo "==> reject replay"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "${BASE}/v1/webhooks/sms" \
  -H "x-signature: ${SIG}" -d "$BODY")
[ "$code" = "409" ] || fail "sms replay expected 409, got $code"

echo "==> reconciliation reflects settlement"
curl -s "${BASE}/v1/ledger/query" | grep -q '"total_settled_minor":50000' \
  || fail "reconciliation did not show 50000 settled"

echo "==> merchant webhook delivered with valid signature"
for _ in $(seq 1 15); do grep -q DELIVERED /tmp/e2e_recv.log && break || sleep 0.5; done
grep -q DELIVERED /tmp/e2e_recv.log || fail "merchant webhook was not delivered with a valid signature"
grep -q BADSIG /tmp/e2e_recv.log && fail "merchant webhook had an invalid signature"

echo "E2E PASS"
