//! REST API for gateway status and management.
//!
//! Provides endpoints:
//! - GET /gateways - List all known gateways with status
//! - GET /gateways/:mac - Get status for a specific gateway
//! - GET /gateways/:mac/packets - Get recent packet metadata
//! - POST /gateways/:mac/sign - Sign data with gateway's keypair
//! - GET /events - SSE stream of real-time gateway events
//! - GET /metrics - Prometheus metrics

use crate::gateway_table::GatewayTable;
use axum::{
    extract::{Path, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Json, Response,
    },
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

/// Maximum concurrent SSE connections
const MAX_SSE_CONNECTIONS: u32 = 20;

/// Shared API state
#[derive(Clone)]
pub struct ApiState {
    pub table: Arc<GatewayTable>,
    pub read_api_key: Option<String>,
    pub write_api_key: Option<String>,
    pub sse_connections: Arc<AtomicU32>,
}

/// API response for listing gateways
#[derive(Serialize)]
pub struct GatewaysResponse {
    pub gateways: Vec<crate::gateway_table::GatewayInfo>,
    pub total: usize,
    pub connected: usize,
}

/// API response for errors
#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

/// Request body for add gateway transaction
#[derive(Deserialize)]
pub struct AddGatewayRequest {
    /// Solana address of the owner (base58)
    pub owner: String,
    /// Solana address of the payer (base58)
    pub payer: String,
}

/// Response body for add gateway transaction
#[derive(Serialize)]
pub struct AddGatewayResponse {
    /// Base64-encoded signed transaction
    pub txn: String,
    /// Gateway public key
    pub gateway: String,
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
    read_api_key: Option<String>,
    write_api_key: Option<String>,
) -> Router {
    let state = ApiState {
        table,
        read_api_key,
        write_api_key,
        sse_connections: Arc::new(AtomicU32::new(0)),
    };

    let read_routes = Router::new()
        .route("/gateways", get(list_gateways))
        .route("/gateways/{mac}", get(get_gateway))
        .route("/gateways/{mac}/packets", get(get_gateway_packets))
        .route("/metrics", get(metrics_handler))
        .layer(middleware::from_fn_with_state(state.clone(), read_auth));

    let write_routes = Router::new()
        .route("/gateways/{mac}/sign", post(sign_data))
        .route("/gateways/{mac}/add", post(add_gateway))
        .layer(middleware::from_fn_with_state(state.clone(), write_auth));

    // SSE events endpoint — public, no auth, connection-limited, CORS open
    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any);
    let public_routes = Router::new()
        .route("/events", get(events_handler))
        .layer(cors);

    read_routes
        .merge(write_routes)
        .merge(public_routes)
        .with_state(state)
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

    Json(GatewaysResponse {
        gateways,
        total,
        connected,
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

/// Get recent packets for a gateway
async fn get_gateway_packets(
    State(state): State<ApiState>,
    Path(mac): Path<String>,
) -> Result<Json<Vec<crate::gateway_table::PacketMetadata>>, (StatusCode, Json<ErrorResponse>)> {
    let mac_addr = parse_mac(&mac)?;
    match state.table.get_recent_packets(&mac_addr).await {
        Some(packets) => Ok(Json(packets)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Gateway {} not found", mac),
            }),
        )),
    }
}

/// RAII guard that decrements SSE connection count on drop
struct SseConnectionGuard(Arc<AtomicU32>);

impl Drop for SseConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// SSE events endpoint — streams real-time gateway events
async fn events_handler(
    State(state): State<ApiState>,
) -> Result<
    Sse<impl Stream<Item = Result<SseEvent, std::convert::Infallible>>>,
    (StatusCode, Json<ErrorResponse>),
> {
    let current = state.sse_connections.fetch_add(1, Ordering::Relaxed);
    if current >= MAX_SSE_CONNECTIONS {
        state.sse_connections.fetch_sub(1, Ordering::Relaxed);
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Too many SSE connections".to_string(),
            }),
        ));
    }

    let guard = SseConnectionGuard(state.sse_connections.clone());
    let rx = state.table.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        let _guard = &guard;
        match result {
            Ok(event) => serde_json::to_string(&event)
                .ok()
                .map(|json| Ok(SseEvent::default().data(json))),
            Err(_) => None, // skip lagged messages
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Create an add-gateway transaction signed by the gateway's keypair
async fn add_gateway(
    State(state): State<ApiState>,
    Path(mac): Path<String>,
    Json(request): Json<AddGatewayRequest>,
) -> Result<Json<AddGatewayResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mac_addr = parse_mac(&mac)?;

    // Decode owner and payer from base58
    let owner = bs58::decode(&request.owner).into_vec().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid owner address (expected base58)".to_string(),
            }),
        )
    })?;
    let payer = bs58::decode(&request.payer).into_vec().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid payer address (expected base58)".to_string(),
            }),
        )
    })?;

    match state
        .table
        .create_add_gateway_txn(&mac_addr, owner, payer)
        .await
    {
        Ok(Some(txn)) => {
            let gateway_info = state.table.get_gateway(&mac_addr).await;
            Ok(Json(AddGatewayResponse {
                txn: BASE64.encode(&txn),
                gateway: gateway_info.map(|g| g.public_key).unwrap_or_default(),
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
                error: format!("Failed to create add gateway transaction: {}", e),
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
    read_api_key: Option<String>,
    write_api_key: Option<String>,
    shutdown: triggered::Listener,
) -> anyhow::Result<()> {
    let app = create_router(table, read_api_key, write_api_key);

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
