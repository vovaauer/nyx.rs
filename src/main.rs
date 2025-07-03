use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use dotenvy::dotenv;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

// --- Data Structures ---

// Represents the data we expect from the user to create an order
#[derive(Deserialize)]
struct CreateOrderPayload {
    service_id: u32,
    link: String,
    quantity: u32,
}

// Represents a service from Spotboost
#[derive(Serialize, Clone)]
struct Service {
    service: u32,
    name: String,
    rate: String,
    min: String,
    max: String,
}

// Represents the response we give to the user after creating an order
#[derive(Serialize)]
struct OrderResponse {
    order_id: Uuid,
}

// In-memory "database" to store our internal order ID -> Spotboost's order ID.
// For a production system, you'd replace this with a simple database like SQLite.
// Arc<Mutex<T>> makes it safe to share across multiple threads.
type Db = Arc<Mutex<HashMap<Uuid, u64>>>;

// --- Application State ---
// This struct holds shared state, like the HTTP client and our database.
#[derive(Clone)]
struct AppState {
    http_client: Client,
    api_key: String,
    db: Db,
}

// --- Main Function ---
#[tokio::main]
async fn main() {
    // Load .env file variables
    dotenv().expect("Failed to read .env file");

    // Initialize logging (without printing sensitive info)
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Create the shared state
    let shared_state = AppState {
        http_client: Client::new(),
        api_key: env::var("SPOTBOOST_API_KEY").expect("SPOTBOOST_API_KEY must be set"),
        db: Arc::new(Mutex::new(HashMap::new())),
    };

    // Build our application with the routes
    let app = Router::new()
        .route("/api/services", get(get_services))
        .route("/api/order", post(create_order))
        .route("/api/status/:id", get(get_order_status))
        .with_state(shared_state);

    // Run it
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::info!("listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// --- API Handlers ---

// Handler to get the list of services from Spotboost
async fn get_services(
    State(state): State<AppState>,
) -> Result<Json<Vec<Service>>, StatusCode> {
    let params = [
        ("key", state.api_key.as_str()),
        ("action", "services"),
    ];

    let response = state
        .http_client
        .post("https://spotboost.top/api/v2")
        .form(&params)
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if response.status().is_success() {
        // We need to define a temporary struct to deserialize the full response
        #[derive(Deserialize)]
        struct SpotboostService {
            service: u32,
            name: String,
            rate: String,
            min: String,
            max: String,
        }
        
        let services: Vec<SpotboostService> = response
            .json()
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        // Map to our public Service struct
        let public_services = services
            .into_iter()
            .map(|s| Service {
                service: s.service,
                name: s.name,
                rate: s.rate,
                min: s.min,
                max: s.max,
            })
            .collect();
        
        Ok(Json(public_services))
    } else {
        Err(StatusCode::BAD_GATEWAY)
    }
}

// Handler to create a new order
async fn create_order(
    State(state): State<AppState>,
    Json(payload): Json<CreateOrderPayload>,
) -> Result<Json<OrderResponse>, StatusCode> {
    // Here is where you would integrate your payment processor (e.g., BTCPay Server).
    // 1. Calculate the price based on payload.quantity and the service rate.
    // 2. Create an invoice with the payment processor.
    // 3. Return the invoice details to the user.
    // 4. The user pays.
    // 5. A webhook from the payment processor hits another endpoint on your server.
    // 6. That webhook handler then calls the code below to place the order with Spotboost.
    // For now, we will skip payment and place the order directly for simplicity.

    let params = [
        ("key", state.api_key.clone()),
        ("action", "add".to_string()),
        ("service", payload.service_id.to_string()),
        ("link", payload.link.clone()),
        ("quantity", payload.quantity.to_string()),
    ];

    let response = state
        .http_client
        .post("https://spotboost.top/api/v2")
        .form(&params)
        .send()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if response.status().is_success() {
        #[derive(Deserialize)]
        struct SpotboostOrderResponse {
            order: u64,
        }
        let spotboost_order: SpotboostOrderResponse = response
            .json()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        
        // Generate our own private, unique order ID
        let our_order_id = Uuid::new_v4();
        
        // Store the mapping
        state
            .db
            .lock()
            .unwrap()
            .insert(our_order_id, spotboost_order.order);

        // Return OUR order ID to the user, not Spotboost's
        Ok(Json(OrderResponse {
            order_id: our_order_id,
        }))
    } else {
        Err(StatusCode::BAD_REQUEST)
    }
}

// Handler to get an order's status
async fn get_order_status(
    State(state): State<AppState>,
    Path(order_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Find the corresponding Spotboost order ID from our DB
    let spotboost_order_id = {
        let db = state.db.lock().unwrap();
        db.get(&order_id).cloned()
    };

    if let Some(id) = spotboost_order_id {
        let params = [
            ("key", state.api_key.as_str()),
            ("action", "status"),
            ("order", &id.to_string()),
        ];
        
        let response = state
            .http_client
            .post("https://spotboost.top/api/v2")
            .form(&params)
            .send()
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        
        let status_json: serde_json::Value = response
            .json()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;

        Ok(Json(status_json))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}
