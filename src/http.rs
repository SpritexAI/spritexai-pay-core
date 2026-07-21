//! HTTP surface. Routes are mounted here and delegate to per-domain modules as the
//! engine grows (charges, webhooks, gateways, devices).

use crate::charge::{self, ChargeError, CreateCharge};
use crate::checkout::{self, CheckoutError, CreateCheckout};
use crate::config::Config;
use crate::crypto;
use crate::customer::{self, CreateCustomer};
use crate::db::Db;
use crate::device::{self, PairDevice, RegisterGateway};
use crate::domain::{self, CreateDomain};
use crate::invoice::{self, CreateInvoice};
use crate::merchant::{self, CreateApiKey};
use crate::payment_link::{self, CreatePaymentLink};
use crate::reconcile;
use crate::sms::{self, IngestError};
use axum::extract::Request;
use axum::response::Redirect;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_governor::{
    governor::GovernorConfigBuilder, key_extractor::SmartIpKeyExtractor, GovernorLayer,
};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub sms_hmac_secret: Arc<String>,
    // None → auth disabled (API open). Some → login required on protected routes.
    pub admin_password: Option<Arc<String>>,
    pub auth_secret: Arc<String>,
    // Public base URL for building hosted checkout links; None → derive from Host.
    pub base_url: Option<Arc<String>>,
}

pub async fn serve(cfg: Config, db: Db) -> anyhow::Result<()> {
    // Durable outbound webhook delivery runs in the background off the same DB.
    crate::webhook::spawn_worker(db.clone(), cfg.webhook_hmac_secret.clone());

    let state = AppState {
        db,
        sms_hmac_secret: Arc::new(cfg.sms_hmac_secret.clone()),
        admin_password: cfg.admin_password.clone().map(Arc::new),
        auth_secret: Arc::new(cfg.auth_secret.clone()),
        base_url: cfg.base_url.clone().map(Arc::new),
    };
    if state.admin_password.is_some() {
        tracing::info!("admin auth enabled — console requires login");
    } else {
        tracing::warn!("ADMIN_PASSWORD unset — API is open (no login required)");
    }

    // Per-client-IP rate limit: sustained ~10 req/s with a burst of 20. Uses the
    // smart extractor so it honors X-Forwarded-For behind a reverse proxy and falls
    // back to the socket peer otherwise.
    let governor = GovernorConfigBuilder::default()
        .per_millisecond(100)
        .burst_size(20)
        .key_extractor(SmartIpKeyExtractor)
        .finish()
        .expect("valid governor config");

    // Admin-console routes: guarded by the bearer-token middleware when auth is on.
    // The SMS webhook is deliberately NOT here — it authenticates with its own
    // inbound HMAC (the Android forwarder has no admin token).
    let protected = Router::new()
        .route("/v1/charges", post(create_charge).get(list_charges))
        .route("/v1/charges/:id", get(get_charge))
        .route("/v1/gateways", post(register_gateway).get(list_gateways))
        .route(
            "/v1/gateways/:gateway/regex-suggestion",
            get(regex_suggestion),
        )
        .route("/v1/ledger/query", get(ledger_query))
        .route("/v1/fraud/scan", get(fraud_scan))
        .route("/v1/recon/chat", post(recon_chat))
        .route("/v1/devices/pair", post(pair_device))
        .route("/v1/devices", get(list_devices))
        .route(
            "/v1/merchant-keys",
            post(create_merchant_key).get(list_merchant_keys),
        )
        .route("/v1/merchant-keys/:id/revoke", post(revoke_merchant_key))
        .route("/v1/customers", post(create_customer).get(list_customers))
        .route("/v1/customers/:id", get(get_customer))
        .route("/v1/invoices", post(create_invoice).get(list_invoices))
        .route("/v1/invoices/:id", get(get_invoice))
        .route("/v1/invoices/:id/issue", post(issue_invoice))
        .route("/v1/payment-links", post(create_link).get(list_links))
        .route("/v1/domains", post(create_domain).get(list_domains))
        .route("/v1/domains/:id", axum::routing::delete(delete_domain))
        .route("/v1/settings", get(get_settings))
        .route("/v1/settings/:key", axum::routing::put(put_setting))
        .route("/v1/activities", get(list_activities))
        .route("/v1/sms-events", get(list_sms_events))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    // Merchant checkout API — compatible with the common PHP SMS-gateway
    // integration shape, guarded by an API key (a separate
    // trust boundary from admin auth). The key's scope is checked per-route.
    let merchant_api = Router::new()
        .route("/api/checkout/redirect", post(api_create_checkout))
        .route("/api/verify-payment", post(api_verify_payment))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ));

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/status", get(auth_status))
        .route("/v1/devices/exchange", post(exchange_token))
        .route("/v1/webhooks/sms", post(sms_webhook))
        // Customer-facing checkout — public (no auth): the hosted page and its poll.
        .route("/pay/:pay_ref", get(checkout_page))
        .route("/v1/checkout/:pay_ref", get(checkout_status))
        .route("/v1/checkout/:pay_ref/select", post(checkout_select))
        .route("/v1/checkout/:pay_ref/claim", post(checkout_claim))
        // Public payment link: open it → spawn a checkout and redirect to the pay page.
        .route("/link/:ref", get(open_link))
        .route("/v1/payment-links/:ref/open", post(open_link_amount))
        .merge(merchant_api)
        .merge(protected)
        .with_state(state)
        .layer(GovernorLayer {
            config: Arc::new(governor),
        });

    // Serve the built dashboard when its assets are present. Unknown paths fall
    // back to index.html so client-side routing works. API-only self-hosted
    // deployments simply don't ship a static dir. Static assets bypass the API
    // rate limiter (applied above) so the SPA loads freely.
    let app = match std::fs::metadata(&cfg.static_dir) {
        Ok(m) if m.is_dir() => {
            let index = format!("{}/index.html", cfg.static_dir);
            tracing::info!(dir = %cfg.static_dir, "serving dashboard assets");
            app.fallback_service(ServeDir::new(&cfg.static_dir).fallback(ServeFile::new(index)))
        }
        _ => app,
    };

    let app = app.layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(cfg.bind_addr).await?;
    tracing::info!(addr = %cfg.bind_addr, "listening");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let db_ok = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&state.db)
        .await
        .is_ok();

    let code = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(json!({ "status": if db_ok { "ok" } else { "degraded" }, "db": db_ok })),
    )
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}

#[derive(Deserialize)]
struct LoginReq {
    password: String,
}

/// Exchange the admin password for a signed bearer token. When auth is disabled
/// (no ADMIN_PASSWORD) this returns 404 — there's nothing to log into.
async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginReq>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(admin) = state.admin_password.as_deref() else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "auth is not enabled".into(),
        ));
    };
    if !crate::auth::password_matches(&state.auth_secret, admin, &req.password) {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid password".into(),
        ));
    }
    let token = crate::auth::issue_token(&state.auth_secret);
    Ok((StatusCode::OK, Json(json!({ "token": token }))))
}

/// Whether login is required. Lets the SPA decide to show a login screen without
/// first bouncing a protected request off a 401.
async fn auth_status(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({ "auth_required": state.admin_password.is_some() }))
}

#[derive(Deserialize)]
struct ExchangeReq {
    token: String,
}

/// The Android forwarder trades its pairing token for the shared SMS HMAC secret.
/// Public (the app has only the pairing token, no admin session) but rate-limited.
/// The secret is returned once per call over TLS; the token stays server-verified.
async fn exchange_token(
    State(state): State<AppState>,
    Json(req): Json<ExchangeReq>,
) -> Result<impl IntoResponse, ApiError> {
    match device::exchange_token(&state.db, &req.token).await {
        Ok(Some(device_id)) => Ok((
            StatusCode::OK,
            Json(json!({
                "device_id": device_id,
                "gateway_secret": state.sms_hmac_secret.as_str(),
            })),
        )),
        Ok(None) => Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "unknown or revoked pairing token".into(),
        )),
        Err(e) => Err(db_error(e)),
    }
}

// ---- Merchant API keys (admin-guarded) ----

async fn create_merchant_key(
    State(state): State<AppState>,
    Json(req): Json<CreateApiKey>,
) -> Result<impl IntoResponse, ApiError> {
    let key = merchant::create_api_key(&state.db, req)
        .await
        .map_err(db_error)?;
    Ok((StatusCode::CREATED, Json(key)))
}

async fn list_merchant_keys(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let keys = merchant::list_api_keys(&state.db).await.map_err(db_error)?;
    Ok((StatusCode::OK, Json(keys)))
}

async fn revoke_merchant_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let ok = merchant::revoke_api_key(&state.db, &id)
        .await
        .map_err(db_error)?;
    if ok {
        Ok((StatusCode::OK, Json(json!({ "revoked": true }))))
    } else {
        Err(ApiError(StatusCode::NOT_FOUND, "key not found".into()))
    }
}

// ---- Customers ----

async fn create_customer(
    State(state): State<AppState>,
    Json(req): Json<CreateCustomer>,
) -> Result<impl IntoResponse, ApiError> {
    let c = customer::create(&state.db, req).await.map_err(db_error)?;
    Ok((StatusCode::CREATED, Json(c)))
}

async fn list_customers(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let list = customer::list(&state.db, 100).await.map_err(db_error)?;
    Ok((StatusCode::OK, Json(list)))
}

async fn get_customer(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    match customer::get(&state.db, &id).await.map_err(db_error)? {
        Some(c) => Ok((StatusCode::OK, Json(c))),
        None => Err(ApiError(StatusCode::NOT_FOUND, "customer not found".into())),
    }
}

// ---- Invoices ----

async fn create_invoice(
    State(state): State<AppState>,
    Json(req): Json<CreateInvoice>,
) -> Result<impl IntoResponse, ApiError> {
    let inv = invoice::create(&state.db, req).await?;
    Ok((StatusCode::CREATED, Json(inv)))
}

async fn list_invoices(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let list = invoice::list(&state.db, 100).await?;
    Ok((StatusCode::OK, Json(list)))
}

async fn get_invoice(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    match invoice::get(&state.db, &id).await? {
        Some(inv) => Ok((StatusCode::OK, Json(inv))),
        None => Err(ApiError(StatusCode::NOT_FOUND, "invoice not found".into())),
    }
}

async fn issue_invoice(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let base = resolve_base_url(&state, &headers);
    let created = invoice::issue_payment(&state.db, &base, &id).await?;
    Ok((StatusCode::OK, Json(json!({ "sap_url": created.sap_url }))))
}

// ---- Payment links ----

async fn create_link(
    State(state): State<AppState>,
    Json(req): Json<CreatePaymentLink>,
) -> Result<impl IntoResponse, ApiError> {
    let link = payment_link::create(&state.db, req).await?;
    Ok((StatusCode::CREATED, Json(link)))
}

async fn list_links(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let list = payment_link::list(&state.db, 100).await?;
    Ok((StatusCode::OK, Json(list)))
}

/// Public: open a fixed-amount link → spawn a checkout and 302 to the pay page.
async fn open_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(reference): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let base = resolve_base_url(&state, &headers);
    let created = payment_link::open(&state.db, &base, &reference, None).await?;
    Ok(Redirect::to(&created.sap_url))
}

#[derive(Deserialize)]
struct OpenLinkBody {
    amount: f64,
}

/// Public: open an open-amount link with a customer-entered amount.
async fn open_link_amount(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(reference): Path<String>,
    Json(body): Json<OpenLinkBody>,
) -> Result<impl IntoResponse, ApiError> {
    let base = resolve_base_url(&state, &headers);
    let minor = (body.amount * 100.0).round() as i64;
    let created = payment_link::open(&state.db, &base, &reference, Some(minor)).await?;
    Ok((StatusCode::OK, Json(json!({ "sap_url": created.sap_url }))))
}

// ---- Domains ----

async fn create_domain(
    State(state): State<AppState>,
    Json(req): Json<CreateDomain>,
) -> Result<impl IntoResponse, ApiError> {
    let d = domain::create(&state.db, req).await.map_err(db_error)?;
    Ok((StatusCode::CREATED, Json(d)))
}

async fn list_domains(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let list = domain::list(&state.db).await.map_err(db_error)?;
    Ok((StatusCode::OK, Json(list)))
}

async fn delete_domain(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let ok = domain::delete(&state.db, &id).await.map_err(db_error)?;
    if ok {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(StatusCode::NOT_FOUND, "domain not found".into()))
    }
}

// ---- Settings ----

#[derive(Deserialize)]
struct SettingsQuery {
    group: String,
}

async fn get_settings(
    State(state): State<AppState>,
    Query(q): Query<SettingsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let rows = crate::settings::get_group(&state.db, &q.group)
        .await
        .map_err(db_error)?;
    Ok((StatusCode::OK, Json(rows)))
}

#[derive(Deserialize)]
struct SetSetting {
    value: String,
}

async fn put_setting(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(body): Json<SetSetting>,
) -> Result<impl IntoResponse, ApiError> {
    crate::settings::set(&state.db, &key, &body.value)
        .await
        .map_err(db_error)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Activities + SMS data (read-only over existing tables) ----

async fn list_activities(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let list = crate::activity::list(&state.db, 100)
        .await
        .map_err(db_error)?;
    Ok((StatusCode::OK, Json(list)))
}

async fn list_sms_events(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let list = sms::list_events(&state.db, 100).await.map_err(db_error)?;
    Ok((StatusCode::OK, Json(list)))
}

// ---- Merchant checkout API (API-key-guarded; common PHP SMS-gateway shape) ----

/// The scope the current request's API key satisfied — stashed by `require_api_key`.
#[derive(Clone)]
#[allow(dead_code)] // carried for future per-key auditing/rate-limiting
struct ApiScope(String);

/// API-key middleware. Reads the key from `x-sap-api-key` or `Authorization: Bearer`,
/// checks the scope the route needs, and passes the key context down via a request
/// extension. A separate boundary from admin auth.
async fn require_api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Result<impl IntoResponse, ApiError> {
    let raw = headers
        .get("x-sap-api-key")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        })
        .unwrap_or("");

    // Route → required scope: verify-payment needs verify_payment, else create_payment.
    let required = if request.uri().path().contains("verify-payment") {
        "verify_payment"
    } else {
        "create_payment"
    };

    match merchant::verify_api_key(&state.db, raw, required).await {
        Ok(Some(ctx)) => {
            request.extensions_mut().insert(ApiScope(ctx.id));
            Ok(next.run(request).await)
        }
        Ok(None) => Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid API key or insufficient scope".into(),
        )),
        Err(e) => Err(db_error(e)),
    }
}

/// Resolve the public base URL: configured value, else the request's Host header.
fn resolve_base_url(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(b) = &state.base_url {
        return b.as_str().to_string();
    }
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    // Behind Cloudflare/any TLS proxy the original scheme is in this header.
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("https");
    format!("{scheme}://{host}")
}

async fn api_create_checkout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateCheckout>,
) -> Result<impl IntoResponse, ApiError> {
    let base = resolve_base_url(&state, &headers);
    let created = checkout::create_checkout(&state.db, &base, req).await?;
    Ok((StatusCode::OK, Json(created)))
}

#[derive(Deserialize)]
struct VerifyReq {
    sap_id: String,
}

async fn api_verify_payment(
    State(state): State<AppState>,
    Json(req): Json<VerifyReq>,
) -> Result<impl IntoResponse, ApiError> {
    let result = checkout::verify(&state.db, &req.sap_id).await?;
    Ok((StatusCode::OK, Json(result)))
}

// ---- Customer-facing checkout (public) ----

async fn checkout_status(
    State(state): State<AppState>,
    Path(pay_ref): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let view = checkout::get_public_charge(&state.db, &pay_ref).await?;
    Ok((StatusCode::OK, Json(view)))
}

#[derive(Deserialize)]
struct SelectReq {
    gateway: String,
}

async fn checkout_select(
    State(state): State<AppState>,
    Path(pay_ref): Path<String>,
    Json(req): Json<SelectReq>,
) -> Result<impl IntoResponse, ApiError> {
    checkout::select_gateway(&state.db, &pay_ref, &req.gateway).await?;
    Ok((StatusCode::OK, Json(json!({ "ok": true }))))
}

#[derive(Deserialize)]
struct ClaimReq {
    trx_id: String,
    sender: Option<String>,
}

async fn checkout_claim(
    State(state): State<AppState>,
    Path(pay_ref): Path<String>,
    Json(req): Json<ClaimReq>,
) -> Result<impl IntoResponse, ApiError> {
    checkout::submit_manual(&state.db, &pay_ref, &req.trx_id, req.sender.as_deref()).await?;
    Ok((StatusCode::OK, Json(json!({ "ok": true }))))
}

/// Serve the self-contained hosted checkout page.
async fn checkout_page(
    State(state): State<AppState>,
    Path(pay_ref): Path<String>,
) -> impl IntoResponse {
    match checkout::get_public_charge(&state.db, &pay_ref).await {
        Ok(view) => axum::response::Html(crate::checkout_page::render(&view)).into_response(),
        Err(CheckoutError::NotFound) => (
            StatusCode::NOT_FOUND,
            axum::response::Html(crate::checkout_page::not_found()),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "checkout page load failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// Bearer-token guard for the admin console routes. No-op when auth is disabled.
async fn require_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<impl IntoResponse, ApiError> {
    if state.admin_password.is_none() {
        return Ok(next.run(request).await);
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if !crate::auth::verify_token(&state.auth_secret, token) {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "authentication required".into(),
        ));
    }
    Ok(next.run(request).await)
}

async fn create_charge(
    State(state): State<AppState>,
    Json(req): Json<CreateCharge>,
) -> Result<impl IntoResponse, ApiError> {
    let charge = charge::create(&state.db, req).await?;
    Ok((StatusCode::CREATED, Json(charge)))
}

async fn get_charge(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let charge = charge::get(&state.db, &id).await?;
    Ok((StatusCode::OK, Json(charge)))
}

/// Recent charges, newest first. Capped so a busy merchant can't pull the whole
/// history in one request; the dashboard shows the latest page.
async fn list_charges(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let charges = charge::list(&state.db, 100).await?;
    Ok((StatusCode::OK, Json(charges)))
}

#[derive(Deserialize)]
struct SmsPayload {
    gateway: String,
    body: String,
}

/// Inbound endpoint for the paired Android SMS forwarder. The `X-Signature` header
/// carries the HMAC-SHA256 of the raw request body; we verify it over the exact
/// bytes received before trusting anything inside.
async fn sms_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let sig = headers
        .get("x-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "missing signature".into()))?;

    if !crypto::verify(state.sms_hmac_secret.as_bytes(), &raw, sig) {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "invalid signature".into(),
        ));
    }

    let payload: SmsPayload = serde_json::from_slice(&raw)
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "invalid JSON body".into()))?;

    let result = sms::ingest(&state.db, &payload.gateway, &payload.body).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "sms_event_id": result.sms_event_id,
            "txn_id": result.txn_id,
            "matched_charge": result.matched_charge,
        })),
    ))
}

/// Thin HTTP shell over domain errors — keeps status-code policy in one place.
struct ApiError(StatusCode, String);

async fn register_gateway(
    State(state): State<AppState>,
    Json(req): Json<RegisterGateway>,
) -> Result<impl IntoResponse, ApiError> {
    let gw = device::register_gateway(&state.db, req)
        .await
        .map_err(db_error)?;
    Ok((StatusCode::CREATED, Json(gw)))
}

async fn list_gateways(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let gateways = device::list_gateways(&state.db).await.map_err(db_error)?;
    Ok((StatusCode::OK, Json(gateways)))
}

/// Ask the AI layer to propose updated regex from drifted SMS samples it recovered.
/// Advisory only — a maintainer reviews and edits `gateway.rs` by hand; regex is
/// never hot-swapped from model output. 404 when there's no drift data yet (or no
/// AI keys configured).
async fn regex_suggestion(
    State(state): State<AppState>,
    Path(gateway): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    match crate::ai::suggest_regex(&state.db, &gateway).await {
        Some(s) => Ok((StatusCode::OK, Json(s))),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            "no recovered drift samples for this gateway (or AI disabled)".into(),
        )),
    }
}

async fn ledger_query(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, ApiError> {
    let gateway = params.get("gateway").map(String::as_str);
    let summary = reconcile::reconcile(&state.db, gateway)
        .await
        .map_err(db_error)?;
    Ok((StatusCode::OK, Json(summary)))
}

/// Deterministic anomaly scan over recorded events — duplicate TXIDs across
/// gateways, sender mismatches, amount outliers. Rules, not a model verdict, so
/// results are auditable.
async fn fraud_scan(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let report = crate::fraud::scan(&state.db).await.map_err(db_error)?;
    Ok((StatusCode::OK, Json(report)))
}

#[derive(Deserialize)]
struct ChatQuestion {
    question: String,
}

/// Reconciliation chat: a plain-language question is routed by the AI layer to a
/// structured intent, then answered with a REAL deterministic query. The model
/// only classifies — it never invents the numbers, so answers are ground truth.
/// 503 when no AI keys are configured (routing needs a provider).
async fn recon_chat(
    State(state): State<AppState>,
    Json(req): Json<ChatQuestion>,
) -> Result<impl IntoResponse, ApiError> {
    let intent = crate::ai::classify_question(&req.question)
        .await
        .ok_or_else(|| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "reconciliation chat needs an AI provider (set OPENROUTER_API_KEY)".into(),
            )
        })?;

    let answer = match intent.intent.as_str() {
        "reconcile" => {
            let r = reconcile::reconcile(&state.db, intent.gateway.as_deref())
                .await
                .map_err(db_error)?;
            json!({ "intent": "reconcile", "data": r })
        }
        "fraud" => {
            let r = crate::fraud::scan(&state.db).await.map_err(db_error)?;
            json!({ "intent": "fraud", "data": r })
        }
        _ => json!({ "intent": "unknown", "data": null }),
    };
    Ok((StatusCode::OK, Json(answer)))
}

async fn pair_device(
    State(state): State<AppState>,
    Json(req): Json<PairDevice>,
) -> Result<impl IntoResponse, ApiError> {
    let paired = device::pair_device(&state.db, req)
        .await
        .map_err(db_error)?;
    Ok((StatusCode::CREATED, Json(paired)))
}

async fn list_devices(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let devices = device::list_devices(&state.db).await.map_err(db_error)?;
    Ok((StatusCode::OK, Json(devices)))
}

fn db_error(e: sqlx::Error) -> ApiError {
    tracing::error!(error = %e, "database operation failed");
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
}

impl From<ChargeError> for ApiError {
    fn from(e: ChargeError) -> Self {
        let code = match e {
            ChargeError::InvalidAmount => StatusCode::UNPROCESSABLE_ENTITY,
            ChargeError::DuplicateOrder(_) => StatusCode::CONFLICT,
            ChargeError::NotFound => StatusCode::NOT_FOUND,
            ChargeError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if code == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %e, "charge operation failed");
            return ApiError(code, "internal error".into());
        }
        ApiError(code, e.to_string())
    }
}

impl From<IngestError> for ApiError {
    fn from(e: IngestError) -> Self {
        let code = match e {
            IngestError::UnknownGateway => StatusCode::UNPROCESSABLE_ENTITY,
            IngestError::Parse(_) => StatusCode::UNPROCESSABLE_ENTITY,
            IngestError::Duplicate => StatusCode::CONFLICT,
            IngestError::Charge(_) | IngestError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if code == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %e, "sms ingest failed");
            return ApiError(code, "internal error".into());
        }
        ApiError(code, e.to_string())
    }
}

impl From<CheckoutError> for ApiError {
    fn from(e: CheckoutError) -> Self {
        let code = match e {
            CheckoutError::InvalidAmount => StatusCode::UNPROCESSABLE_ENTITY,
            CheckoutError::NotFound => StatusCode::NOT_FOUND,
            CheckoutError::DomainNotAllowed => StatusCode::UNPROCESSABLE_ENTITY,
            CheckoutError::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if code == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %e, "checkout operation failed");
            return ApiError(code, "internal error".into());
        }
        ApiError(code, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<invoice::InvoiceError> for ApiError {
    fn from(e: invoice::InvoiceError) -> Self {
        use invoice::InvoiceError as E;
        let code = match e {
            E::InvalidAmount => StatusCode::UNPROCESSABLE_ENTITY,
            E::NotFound => StatusCode::NOT_FOUND,
            E::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if code == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %e, "invoice operation failed");
            return ApiError(code, "internal error".into());
        }
        ApiError(code, e.to_string())
    }
}

impl From<payment_link::LinkError> for ApiError {
    fn from(e: payment_link::LinkError) -> Self {
        use payment_link::LinkError as E;
        let code = match e {
            E::NotFound => StatusCode::NOT_FOUND,
            E::Inactive => StatusCode::CONFLICT,
            E::AmountRequired => StatusCode::UNPROCESSABLE_ENTITY,
            E::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if code == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %e, "payment link operation failed");
            return ApiError(code, "internal error".into());
        }
        ApiError(code, e.to_string())
    }
}
