//! REST API for gateway status and management.
//!
//! Provides endpoints:
//! - GET /gateways - List all known gateways with status
//! - GET /gateways/:mac - Get status for a specific gateway
//! - POST /gateways/:mac/sign - Sign data with gateway's keypair
//! - GET /metrics - Prometheus metrics

use crate::gateway_table::GatewayTable;
use crate::udp_dispatcher::DispatcherHealth;
use axum::{
    extract::{Path, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;

/// Shared API state
#[derive(Clone)]
pub struct ApiState {
    pub table: Arc<GatewayTable>,
    pub dispatcher_health: Arc<DispatcherHealth>,
    pub read_api_key: Option<String>,
    pub write_api_key: Option<String>,
}

/// Dispatcher health status for API responses
#[derive(Serialize)]
pub struct DispatcherStatus {
    pub last_event_seconds_ago: Option<u64>,
    pub uptime_seconds: u64,
    pub events_processed: u64,
}

/// API response for listing gateways
#[derive(Serialize)]
pub struct GatewaysResponse {
    pub gateways: Vec<crate::gateway_table::GatewayInfo>,
    pub total: usize,
    pub connected: usize,
    pub dispatcher: DispatcherStatus,
}

/// API response for errors
#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

/// Request body for signing
#[derive(Deserialize)]
pub struct SignRequest {
    /// Base64-encoded data to sign
    pub data: String,
}

/// Response body for signing
#[derive(Serialize)]
pub struct SignResponse {
    /// Base64-encoded signature
    pub signature: String,
}

#[allow(clippy::result_large_err)]
fn check_api_key(expected: &Option<String>, request: &Request) -> Result<(), Response> {
    if let Some(expected_key) = expected {
        match request.headers().get("x-api-key") {
            Some(provided) if provided.as_bytes() == expected_key.as_bytes() => Ok(()),
            Some(_) => Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Invalid API key".to_string(),
                }),
            )
                .into_response()),
            None => Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "Missing X-API-Key header".to_string(),
                }),
            )
                .into_response()),
        }
    } else {
        Ok(())
    }
}

/// Middleware for read-only endpoints.
async fn read_auth(State(state): State<ApiState>, request: Request, next: Next) -> Response {
    if let Err(resp) = check_api_key(&state.read_api_key, &request) {
        return resp;
    }
    next.run(request).await
}

/// Middleware for write endpoints (signing).
async fn write_auth(State(state): State<ApiState>, request: Request, next: Next) -> Response {
    if let Err(resp) = check_api_key(&state.write_api_key, &request) {
        return resp;
    }
    next.run(request).await
}

/// Create the API router
pub fn create_router(
    table: Arc<GatewayTable>,
    dispatcher_health: Arc<DispatcherHealth>,
    read_api_key: Option<String>,
    write_api_key: Option<String>,
) -> Router {
    let state = ApiState {
        table,
        dispatcher_health,
        read_api_key,
        write_api_key,
    };

    let read_routes = Router::new()
        .route("/gateways", get(list_gateways))
        .route("/gateways/{mac}", get(get_gateway))
        .route("/metrics", get(metrics_handler))
        .layer(middleware::from_fn_with_state(state.clone(), read_auth));

    let write_routes = Router::new()
        .route("/gateways/{mac}/sign", post(sign_data))
        .layer(middleware::from_fn_with_state(state.clone(), write_auth));

    read_routes.merge(write_routes).with_state(state)
}

/// Prometheus metrics endpoint
async fn metrics_handler() -> String {
    crate::metrics::gather()
}

/// List all gateways
async fn list_gateways(State(state): State<ApiState>) -> Json<GatewaysResponse> {
    let gateways = state.table.list_gateways().await;
    let total = gateways.len();
    let connected = gateways.iter().filter(|g| g.connected).count();
    let health = &state.dispatcher_health;

    Json(GatewaysResponse {
        gateways,
        total,
        connected,
        dispatcher: DispatcherStatus {
            last_event_seconds_ago: health.seconds_since_last_event(),
            uptime_seconds: health.uptime_secs(),
            events_processed: health.total_events(),
        },
    })
}

/// Get a specific gateway by MAC address
async fn get_gateway(
    State(state): State<ApiState>,
    Path(mac): Path<String>,
) -> Result<Json<crate::gateway_table::GatewayInfo>, (StatusCode, Json<ErrorResponse>)> {
    // Parse MAC address from string (16 hex chars)
    if mac.len() != 16 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid MAC address format. Expected 16 hex characters.".to_string(),
            }),
        ));
    }

    let mac_bytes: Result<Vec<u8>, _> = (0..8)
        .map(|i| u8::from_str_radix(&mac[i * 2..i * 2 + 2], 16))
        .collect();

    let mac_bytes = mac_bytes.map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid MAC address format. Expected 16 hex characters.".to_string(),
            }),
        )
    })?;

    let mut arr = [0u8; 8];
    arr.copy_from_slice(&mac_bytes);
    let mac_addr = gateway_rs::semtech_udp::MacAddress::from(arr);

    match state.table.get_gateway(&mac_addr).await {
        Some(info) => Ok(Json(info)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Gateway {} not found", mac),
            }),
        )),
    }
}

/// Sign data with a gateway's keypair
async fn sign_data(
    State(state): State<ApiState>,
    Path(mac): Path<String>,
    Json(request): Json<SignRequest>,
) -> Result<Json<SignResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Parse MAC address
    let mac_addr = parse_mac(&mac)?;

    // Decode base64 data
    let data = BASE64.decode(&request.data).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid base64 data".to_string(),
            }),
        )
    })?;

    // Sign the data
    match state.table.sign(&mac_addr, &data).await {
        Ok(Some(signature)) => {
            let signature_b64 = BASE64.encode(&signature);
            Ok(Json(SignResponse {
                signature: signature_b64,
            }))
        }
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Gateway {} not found", mac),
            }),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Signing failed: {}", e),
            }),
        )),
    }
}

/// Parse MAC address from hex string
fn parse_mac(
    mac: &str,
) -> Result<gateway_rs::semtech_udp::MacAddress, (StatusCode, Json<ErrorResponse>)> {
    if mac.len() != 16 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid MAC address format. Expected 16 hex characters.".to_string(),
            }),
        ));
    }

    let mac_bytes: Result<Vec<u8>, _> = (0..8)
        .map(|i| u8::from_str_radix(&mac[i * 2..i * 2 + 2], 16))
        .collect();

    let mac_bytes = mac_bytes.map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid MAC address format. Expected 16 hex characters.".to_string(),
            }),
        )
    })?;

    let mut arr = [0u8; 8];
    arr.copy_from_slice(&mac_bytes);
    Ok(gateway_rs::semtech_udp::MacAddress::from(arr))
}

/// Run the API server
pub async fn run_api_server(
    listen_addr: String,
    table: Arc<GatewayTable>,
    dispatcher_health: Arc<DispatcherHealth>,
    read_api_key: Option<String>,
    write_api_key: Option<String>,
    shutdown: triggered::Listener,
) -> anyhow::Result<()> {
    let app = create_router(table, dispatcher_health, read_api_key, write_api_key);

    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    info!(addr = %listen_addr, "API server starting");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            info!("API server shutting down");
        })
        .await?;

    Ok(())
}
