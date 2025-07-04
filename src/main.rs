// src/main.rs - COMPLETE AND CORRECTED

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};

use tower_http::services::ServeDir;
use chrono::Utc;
use dotenvy::dotenv;
use regex::Regex;
use reqwest::Client;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time;
use tokio_rusqlite::params;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

mod db;

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
struct AppState {
    http_client: Client,
    spotboost_api_key: String,
    monero_rpc_url: String,
    db: tokio_rusqlite::Connection,
}

// --- Main Function ---
#[tokio::main]
async fn main() {
    dotenv().expect("Failed to read .env file");
    tracing_subscriber::registry().with(tracing_subscriber::fmt::layer()).init();

    let db_path = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let db_conn = db::init_db(&db_path)
        .await
        .expect("Failed to initialize database");

    let custom_client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/108.0.0.0 Safari/537.36")
        .build()
        .expect("Failed to build reqwest client");

    let shared_state = AppState {
        http_client: custom_client,
        spotboost_api_key: env::var("SPOTBOOST_API_KEY").expect("SPOTBOOST_API_KEY must be set"),
        monero_rpc_url: env::var("MONERO_RPC_URL").expect("MONERO_RPC_URL must be set"),
        db: db_conn,
    };

    // --- Scanner for new payments ---
    let payment_scanner_state = shared_state.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            tracing::info!("Scanning for new payments...");
            check_for_payments(payment_scanner_state.clone()).await;
        }
    });

    // --- Processor for paid orders ---
    let fulfillment_processor_state = shared_state.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            tracing::info!("Processing paid orders...");
            fulfill_paid_orders(fulfillment_processor_state.clone()).await;
        }
    });

    let app = Router::new()
        // API routes
        .route("/api/structured-services", get(get_structured_services))
        .route("/api/create-order", post(create_order_and_get_address))
        .route("/api/status/:id", get(get_order_status))
        // Static file serving for the frontend
        .nest_service("/", ServeDir::new("static"))
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
    let now_iso = Utc::now().to_rfc3339();
    let usd_price_for_db = (usd_price * 100.0).round() / 100.0;
    
    // Clone the address for the response *before* it's moved into the closure
    let response_address = payment_address.clone();

    let db_op_result = state.db.call(move |conn| {
        conn.execute(
            "
            INSERT INTO orders (order_id, service_id, link, quantity, usd_price, xmr_amount, payment_address, status, created_at, last_updated)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ",
            params![
                order_id.to_string(),
                payload.service_id,
                payload.link,
                payload.quantity,
                usd_price_for_db,
                xmr_amount,
                payment_address, // The original is moved here
                "AWAITING_PAYMENT",
                &now_iso,
                &now_iso,
            ],
        )?;
        Ok(())
    }).await;

    if let Err(e) = db_op_result {
        tracing::error!("Failed to insert new order into database: {:?}", e);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    
    Ok(Json(PaymentDetailsResponse {
        payment_address: response_address, // We use the clone here
        xmr_amount: (xmr_amount * 1_000_000_000_000.0).round() / 1_000_000_000_000.0,
        order_id,
    }))
}

async fn get_order_status(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
) -> impl IntoResponse {
    #[derive(Debug)]
    struct OrderStatus {
        status: String,
        spotboost_order_id: Option<u64>,
    }

    let result = state.db.call(move |conn| {
        let mut stmt = conn.prepare("SELECT status, spotboost_order_id FROM orders WHERE order_id = ?1")?;
        let order_status = stmt.query_row(params![order_id.to_string()], |row| {
            Ok(OrderStatus {
                status: row.get(0)?,
                spotboost_order_id: row.get(1)?,
            })
        }).optional()?; // .optional() comes from the OptionalExtension trait
        Ok(order_status)
    }).await;

    match result {
        Ok(Some(order)) => {
            let response_body = match order.status.as_str() {
                "AWAITING_PAYMENT" => "Awaiting payment. Please send Monero to the address provided.".to_string(),
                "PAID" => "Payment received. Your order is in the queue for processing.".to_string(),
                "FULFILLED" => format!("Order is complete. Provider Order ID: {}", order.spotboost_order_id.unwrap_or(0)),
                _ => "Order status is unknown. Please contact support.".to_string(),
            };
            (StatusCode::OK, response_body).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "Order ID not found.".to_string()).into_response(),
        Err(e) => {
            tracing::error!("Error checking order status: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error checking order status.".to_string()).into_response()
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
    #[derive(Debug)]
    struct AwaitingOrder {
        order_id: String,
        payment_address: String,
        xmr_amount: f64,
    }

    let awaiting_orders = match state.db.call(|conn| {
        let mut stmt = conn.prepare("SELECT order_id, payment_address, xmr_amount FROM orders WHERE status = 'AWAITING_PAYMENT'")?;
        let rows = stmt.query_map([], |row| {
            Ok(AwaitingOrder {
                order_id: row.get(0)?,
                payment_address: row.get(1)?,
                xmr_amount: row.get(2)?,
            })
        })?;
        let orders = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(orders)
    }).await {
        Ok(orders) => orders,
        Err(e) => {
            tracing::error!("Failed to query awaiting orders from DB: {:?}", e);
            return;
        }
    };

    if awaiting_orders.is_empty() { return; }
    
    let rpc_payload = json!({ "jsonrpc": "2.0", "id": "0", "method": "get_transfers", "params": { "in": true, "pool": true } });
    let rpc_res = match state.http_client.post(&state.monero_rpc_url).json(&rpc_payload).send().await {
        Ok(res) => res,
        Err(e) => {
            tracing::error!("Scanner failed to check for transfers: {:?}", e);
            return;
        }
    };
    let transfers: GetTransfersResponse = match rpc_res.json().await {
        Ok(data) => data,
        Err(e) => {
            tracing::error!("Scanner failed to parse transfers response: {:?}", e);
            return;
        }
    };

    if let Some(incoming_txs) = transfers.result.incoming {
        for tx in incoming_txs {
            for order in &awaiting_orders {
                if tx.address == order.payment_address {
                    let required_atomic_amount = (order.xmr_amount * 1_000_000_000_000.0) as u64;
                    if tx.amount >= required_atomic_amount {
                        tracing::info!("Payment detected for order {}. Marking as PAID.", order.order_id);
                        
                        let order_id_clone = order.order_id.clone();
                        let db_result = state.db.call(move |conn| {
                            conn.execute("UPDATE orders SET status = 'PAID' WHERE order_id = ?1", params![order_id_clone])?;
                            Ok(())
                        }).await;

                        if let Err(e) = db_result {
                            tracing::error!("Failed to update order {} to PAID: {:?}", order.order_id, e);
                        }
                    }
                }
            }
        }
    }
}

async fn fulfill_paid_orders(state: AppState) {
    #[derive(Debug, Clone)]
    struct PaidOrder {
        order_id: String,
        service_id: String,
        link: String,
        quantity: u32,
    }

    let paid_orders = match state.db.call(|conn| {
        let mut stmt = conn.prepare("SELECT order_id, service_id, link, quantity FROM orders WHERE status = 'PAID'")?;
        let rows = stmt.query_map([], |row| {
            Ok(PaidOrder {
                order_id: row.get(0)?,
                service_id: row.get(1)?,
                link: row.get(2)?,
                quantity: row.get(3)?,
            })
        })?;
        let orders = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(orders)
    }).await {
        Ok(orders) => orders,
        Err(e) => {
            tracing::error!("Failed to query paid orders from DB: {:?}", e);
            return;
        }
    };

    for order in paid_orders {
        tracing::info!("Attempting to fulfill order {}", order.order_id);
        
        let params = [
            ("key", state.spotboost_api_key.clone()),
            ("action", "add".to_string()),
            ("service", order.service_id.clone()),
            ("link", order.link.clone()),
            ("quantity", order.quantity.to_string()),
        ];
        
        match state.http_client.post("https://spotboost.top/api/v2").form(&params).send().await {
            Ok(res) => {
                match res.json::<HashMap<String, serde_json::Value>>().await {
                    Ok(spotboost_res) => {
                        if let Some(spotboost_order_id) = spotboost_res.get("order").and_then(|v| v.as_u64()) {
                            tracing::info!("Successfully placed Spotboost order {} for our order {}", spotboost_order_id, order.order_id);
                            
                            let order_id_clone = order.order_id.clone();
                            let db_result = state.db.call(move |conn| {
                                conn.execute("UPDATE orders SET status = 'FULFILLED', spotboost_order_id = ?1 WHERE order_id = ?2", params![spotboost_order_id, order_id_clone])?;
                                Ok(())
                            }).await;

                            if let Err(e) = db_result {
                                tracing::error!("DB error after fulfillment for order {}: {:?}", order.order_id, e);
                            }
                        } else {
                            tracing::error!("Spotboost API error for order {}: response did not contain a valid order ID. Response: {:?}", order.order_id, spotboost_res);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to parse Spotboost JSON response for order {}: {:?}", order.order_id, e);
                    }
                }
            },
            Err(e) => {
                tracing::error!("Failed to send fulfillment request to Spotboost for order {}: {:?}", order.order_id, e);
            }
        }
    }
}
