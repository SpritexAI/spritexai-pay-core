# SpritEXAI Pay — Core Engine

AI-native, open-core payment orchestration for mobile financial services (bKash,
Nagad). Runs comfortably on a modest VPS: a single Rust binary, SQLite by default,
Redis only when you scale horizontally.

Built by **Mohammad Sijan** ([SpritexAI](https://github.com/SpritexAI)) — the team
behind RexiO.

## What it does

Merchants collect payments over the de-facto BD integration path: an Android phone
receives the MFS confirmation SMS, forwards it here, and the engine verifies it,
settles a double-entry ledger, and fires a signed webhook back to the merchant.

```
POST /v1/charges          → create a payment intent
POST /v1/webhooks/sms     → SMS in → parse → idempotency → ledger → merchant webhook
GET  /v1/charges/:id      → charge status
GET  /v1/ledger/query     → reconciliation summary
POST /v1/gateways         → register a gateway (bKash / Nagad)
POST /v1/devices/pair     → pair an Android forwarder (one-time token)
GET  /v1/devices          → list paired devices
```

## Run it

```sh
cp .env.example .env        # set your HMAC secrets
cargo run                   # or: docker run ghcr.io/<owner>/spritexai-pay-core
```

Every merchant webhook is signed `X-SpritexAI-Signature: HMAC-SHA256(payload)`.
Inbound SMS payloads must carry a matching `X-Signature` header.

## Configuration

| Env | Default | Purpose |
|-----|---------|---------|
| `PORT` | `8080` | HTTP listen port |
| `DATABASE_URL` | `sqlite://spritexai_pay.db?mode=rwc` | SQLite (WAL) or Postgres URL |
| `SMS_HMAC_SECRET` | dev default | verifies inbound SMS forwarder payloads |
| `WEBHOOK_HMAC_SECRET` | dev default | signs outbound merchant webhooks |
| `REDIS_URL` | unset | optional; only needed for multi-instance (Cloud) |

## Develop

```sh
cargo test              # unit + integration
cargo clippy -- -D warnings
./scripts/e2e.sh        # full live money-path feature check
```

CI runs all of the above and only builds/pushes the Docker image when they pass.
The VPS never compiles — it pulls the finished image over SSH.

## License

Apache-2.0. Core engine is open; SpritEXAI Pay Cloud (multi-tenant billing, hosted
AI) is proprietary.
