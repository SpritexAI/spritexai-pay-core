//! HTTP surface. Routes are mounted here and delegate to per-domain modules as the
//! engine grows (charges, webhooks, gateways, devices).

use crate::charge::{self, ChargeError, CreateCharge};
use crate::config::Config;
use crate::crypto;
use crate::db::Db;
use crate::device::{self, PairDevice, RegisterGateway};
use crate::reconcile;
use crate::sms::{self, IngestError};
use axum::extract::Request;
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
}

pub async fn serve(cfg: Config, db: Db) -> anyhow::Result<()> {
    // Durable outbound webhook delivery runs in the background off the same DB.
    crate::webhook::spawn_worker(db.clone(), cfg.webhook_hmac_secret.clone());

    let state = AppState {
        db,
        sms_hmac_secret: Arc::new(cfg.sms_hmac_secret.clone()),
        admin_password: cfg.admin_password.clone().map(Arc::new),
        auth_secret: Arc::new(cfg.auth_secret.clone()),
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
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/status", get(auth_status))
        .route("/v1/devices/exchange", post(exchange_token))
        .route("/v1/webhooks/sms", post(sms_webhook))
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

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}
