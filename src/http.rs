//! HTTP surface. Routes are mounted here and delegate to per-domain modules as the
//! engine grows (charges, webhooks, gateways, devices).

use crate::charge::{self, ChargeError, CreateCharge};
use crate::config::Config;
use crate::crypto;
use crate::db::Db;
use crate::device::{self, PairDevice, RegisterGateway};
use crate::reconcile;
use crate::sms::{self, IngestError};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
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
}

pub async fn serve(cfg: Config, db: Db) -> anyhow::Result<()> {
    // Durable outbound webhook delivery runs in the background off the same DB.
    crate::webhook::spawn_worker(db.clone(), cfg.webhook_hmac_secret.clone());

    let state = AppState {
        db,
        sms_hmac_secret: Arc::new(cfg.sms_hmac_secret.clone()),
    };

    // Per-client-IP rate limit: sustained ~10 req/s with a burst of 20. Uses the
    // smart extractor so it honors X-Forwarded-For behind a reverse proxy and falls
    // back to the socket peer otherwise.
    let governor = GovernorConfigBuilder::default()
        .per_millisecond(100)
        .burst_size(20)
        .key_extractor(SmartIpKeyExtractor)
        .finish()
        .expect("valid governor config");

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/charges", post(create_charge))
        .route("/v1/charges/:id", get(get_charge))
        .route("/v1/webhooks/sms", post(sms_webhook))
        .route("/v1/gateways", post(register_gateway))
        .route("/v1/ledger/query", get(ledger_query))
        .route("/v1/devices/pair", post(pair_device))
        .route("/v1/devices", get(list_devices))
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
