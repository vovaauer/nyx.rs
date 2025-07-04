use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use dotenvy::dotenv;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

// --- Data Structures ---

#[derive(Deserialize, Clone)]
struct CreateOrderPayload {
    service_id: String,
    link: String,
    quantity: u32,
}

#[derive(Serialize)]
struct PaymentDetailsResponse {
    payment_address: String,
    xmr_amount: f64,
    order_id: Uuid,
}

#[derive(Deserialize, Serialize, Clone)]
struct StructuredService {
    id: String,
    platform: String,
    service_type: String,
    variant: String,
    rate: String,
    min: String,
    max: String,
}

#[derive(Deserialize, Clone)]
struct TempSpotboostService {
    service: String,
    rate: String,
}

#[derive(Deserialize, Clone)]
struct FullSpotboostService {
    service: String,
    name: String,
    rate: String,
    min: String,
    max: String,
}

#[derive(Clone)]
struct PendingOrder {
    service_id: String,
    link: String,
    quantity: u32,
    payment_address: String,
    xmr_amount: f64,
}

type PendingDb = Arc<Mutex<HashMap<Uuid, PendingOrder>>>;
type FinalDb = Arc<Mutex<HashMap<Uuid, u64>>>;

#[derive(Clone)]
struct AppState {
    http_client: Client,
    spotboost_api_key: String,
    monero_rpc_url: String,
    pending_db: PendingDb,
    final_db: FinalDb,
}

// --- Main Function ---
#[tokio::main]
async fn main() {
    dotenv().expect("Failed to read .env file");
    tracing_subscriber::registry().with(tracing_subscriber::fmt::layer()).init();

    let custom_client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/108.0.0.0 Safari/537.36")
        .build()
        .expect("Failed to build reqwest client");

    let shared_state = AppState {
        http_client: custom_client,
        spotboost_api_key: env::var("SPOTBOOST_API_KEY").expect("SPOTBOOST_API_KEY must be set"),
        monero_rpc_url: env::var("MONERO_RPC_URL").expect("MONERO_RPC_URL must be set"),
        pending_db: Arc::new(Mutex::new(HashMap::new())),
        final_db: Arc::new(Mutex::new(HashMap::new())),
    };

    let scanner_state = shared_state.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            tracing::info!("Scanning for new payments...");
            check_for_payments(scanner_state.clone()).await;
        }
    });

    let app = Router::new()
        .route("/api/structured-services", get(get_structured_services))
        .route("/api/create-order", post(create_order_and_get_address))
        .route("/api/status/:id", get(get_order_status))
        .with_state(shared_state);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::info!("listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// --- API Handlers ---

async fn get_structured_services(
    State(state): State<AppState>,
) -> Result<Json<HashMap<String, Vec<StructuredService>>>, StatusCode> {
    tracing::debug!("Fetching services from Spotboost for structuring...");
    let params = [("key", state.spotboost_api_key.as_str()), ("action", "services")];
    
    let response = state.http_client.post("https://spotboost.top/api/v2").form(&params).send().await
        .map_err(|e| {
            tracing::error!("Failed to connect to Spotboost: {:?}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let response_text = response.text().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    let services: Vec<FullSpotboostService> = serde_json::from_str(&response_text).map_err(|e| {
        tracing::error!("Failed to parse JSON from Spotboost: {:?}\nResponse Text: {}", e, response_text);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut structured_map: HashMap<String, Vec<StructuredService>> = HashMap::new();
    let re_variants = Regex::new(r"\[.*?\]").unwrap();

    for service in services {
        let name = service.name.trim();
        let platform = name.split_whitespace().next().unwrap_or("Other").to_string();
        let variants: Vec<&str> = re_variants.find_iter(name).map(|m| m.as_str()).collect();
        let variant_str = variants.join(" ").trim().to_string();
        let mut service_type = name.replacen(&platform, "", 1);
        for var in &variants {
            service_type = service_type.replace(var, "");
        }
        
        let s = StructuredService {
            id: service.service,
            platform: platform.clone(),
            service_type: service_type.trim().to_string(),
            variant: variant_str,
            rate: service.rate,
            min: service.min,
            max: service.max,
        };
        structured_map.entry(platform).or_default().push(s);
    }
    Ok(Json(structured_map))
}

async fn create_order_and_get_address(
    State(state): State<AppState>,
    Json(payload): Json<CreateOrderPayload>,
) -> Result<Json<PaymentDetailsResponse>, StatusCode> {
    let services_response = state.http_client.post("https://spotboost.top/api/v2")
        .form(&[("key", state.spotboost_api_key.as_str()), ("action", "services")])
        .send().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let services: Vec<TempSpotboostService> = services_response.json().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let service_details = services.into_iter().find(|s| s.service == payload.service_id).ok_or(StatusCode::BAD_REQUEST)?;
    let rate = service_details.rate.parse::<f64>().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let usd_price = (rate / 1000.0) * payload.quantity as f64;

    #[derive(Deserialize)] struct CoinGeckoResponse { monero: HashMap<String, f64> }
    let coingecko_res: CoinGeckoResponse = state.http_client.get("https://api.coingecko.com/api/v3/simple/price?ids=monero&vs_currencies=usd").send().await.map_err(|_| StatusCode::BAD_GATEWAY)?.json().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    let xmr_usd_price = coingecko_res.monero.get("usd").ok_or(StatusCode::BAD_GATEWAY)?;
    let xmr_amount = usd_price / xmr_usd_price;

    let rpc_payload = json!({ "jsonrpc": "2.0", "id": "0", "method": "create_address", "params": { "account_index": 0, "label": "nyx.rs order" } });
    let rpc_response = state.http_client.post(&state.monero_rpc_url).json(&rpc_payload).send().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    #[derive(Deserialize)] struct RpcResult { address: String }
    #[derive(Deserialize)] struct RpcResponse { result: RpcResult }
    let rpc_data: RpcResponse = rpc_response.json().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let payment_address = rpc_data.result.address;
    
    let order_id = Uuid::new_v4();
    let pending_order = PendingOrder { service_id: payload.service_id, link: payload.link, quantity: payload.quantity, payment_address: payment_address.clone(), xmr_amount };
    state.pending_db.lock().unwrap().insert(order_id, pending_order);

    Ok(Json(PaymentDetailsResponse { payment_address, xmr_amount: (xmr_amount * 1_000_000_000_000.0).round() / 1_000_000_000_000.0, order_id }))
}

async fn get_order_status(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
) -> impl IntoResponse {
    let final_db = state.final_db.lock().unwrap();
    if let Some(spotboost_id) = final_db.get(&order_id) {
        let response_body = format!("Order is complete. Provider Order ID: {}", spotboost_id);
        (StatusCode::OK, response_body).into_response()
    } else {
        let pending_db = state.pending_db.lock().unwrap();
        if pending_db.contains_key(&order_id) {
            (StatusCode::OK, "Awaiting payment. Please send Monero to the address provided.").into_response()
        } else {
            (StatusCode::NOT_FOUND, "Order ID not found.").into_response()
        }
    }
}

#[derive(Deserialize, Debug)]
struct Transfer {
    amount: u64,
    address: String,
}

#[derive(Deserialize, Debug)]
struct GetTransfersResult {
    #[serde(rename = "in")]
    incoming: Option<Vec<Transfer>>,
}

#[derive(Deserialize, Debug)]
struct GetTransfersResponse {
    result: GetTransfersResult,
}

async fn check_for_payments(state: AppState) {
    let pending_orders = state.pending_db.lock().unwrap().clone();
    if pending_orders.is_empty() {
        return;
    }

    let rpc_payload = json!({
        "jsonrpc": "2.0",
        "id": "0",
        "method": "get_transfers",
        "params": {
            "in": true,
            "pool": true
        }
    });

    let rpc_res = match state.http_client.post(&state.monero_rpc_url).json(&rpc_payload).send().await {
        Ok(res) => res,
        Err(e) => {
            tracing::error!("Scanner failed to check for transfers: {:?}", e);
            return;
        }
    };

    let response_text = match rpc_res.text().await {
        Ok(text) => text,
        Err(e) => {
            tracing::error!("Scanner failed to read transfers response text: {:?}", e);
            return;
        }
    };

    let transfers: GetTransfersResponse = match serde_json::from_str(&response_text) {
        Ok(data) => data,
        Err(e) => {
            tracing::error!("Scanner failed to parse transfers response: {:?}\nResponse: {}", e, response_text);
            return;
        }
    };

    if let Some(incoming_txs) = transfers.result.incoming {
        for tx in incoming_txs {
            for (order_id, order_details) in &pending_orders {
                if tx.address == order_details.payment_address {
                    let required_atomic_amount = (order_details.xmr_amount * 1_000_000_000_000.0) as u64;
                    if tx.amount >= required_atomic_amount {
                        fulfill_order(state.clone(), *order_id, order_details.clone()).await;
                        break; 
                    }
                }
            }
        }
    }
}

async fn fulfill_order(state: AppState, order_id: Uuid, details: PendingOrder) {
    tracing::info!("Payment detected for order {}. Fulfilling now...", order_id);

    if state.pending_db.lock().unwrap().remove(&order_id).is_none() {
         tracing::warn!("Order {} was already being fulfilled. Ignoring duplicate.", order_id);
         return; 
    }

    let params = [
        ("key", state.spotboost_api_key.clone()),
        ("action", "add".to_string()),
        ("service", details.service_id.clone()),
        ("link", details.link.clone()),
        ("quantity", details.quantity.to_string()),
    ];

    match state.http_client.post("https://spotboost.top/api/v2").form(&params).send().await {
        Ok(res) => {
            if let Ok(spotboost_res) = res.json::<HashMap<String, serde_json::Value>>().await {
                if let Some(spotboost_order_id) = spotboost_res.get("order").and_then(|v| v.as_u64()) {
                    state.final_db.lock().unwrap().insert(order_id, spotboost_order_id);
                    tracing::info!("Successfully placed Spotboost order {} for our order {}", spotboost_order_id, order_id);
                } else {
                    tracing::error!("Failed to get order ID from Spotboost response for order {}", order_id);
                }
            } else {
                tracing::error!("Failed to parse Spotboost response for order {}", order_id);
            }
        },
        Err(e) => {
            tracing::error!("Failed to send fulfillment request to Spotboost for order {}: {:?}", order_id, e);
        }
    }
}
