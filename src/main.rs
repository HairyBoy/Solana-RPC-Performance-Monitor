//! This crate implements a monitoring tool for Solana RPC endpoints.
//! It collects latency and slot notification metrics, persists them using RocksDB,
//! and serves data via an Axum-based API.

// --- Imports ---
use axum::{extract::{Query, State}, routing::{get, get_service}, Json, Router};
use rocksdb::{DB, IteratorMode, Options};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::{HashMap, HashSet}, fs, net::SocketAddr, path::Path, str, sync::Arc, time::Instant};
use chrono::{Duration, Utc};
use tower_http::services::ServeDir;
use tokio::task;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures::{future::join_all, SinkExt, StreamExt};
use solana_client::rpc_client::RpcClient;
use url::Url;

/// Represents a Solana RPC or WebSocket endpoint.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RpcEndpoint {
    pub url: String,
    pub nickname: String,
}

/// Slot information structure from Solana WebSocket.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SlotNotificationResult {
    pub slot: u64,
    pub parent: u64,
    pub root: u64,
}

/// Inner `params` field in WebSocket slot notifications.
#[derive(Debug, Serialize, Deserialize)]
pub struct NotificationParams {
    pub result: SlotNotificationResult,
    pub subscription: u64,
}

/// Top-level WebSocket message structure.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebSocketMessage {
    pub jsonrpc: String,
    pub method: String,
    pub params: NotificationParams,
}

/// Represents a processed WebSocket slot notification.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SlotUpdate {
    pub timestamp: f64,
    pub nickname: String,
    pub slot_info: SlotNotificationResult,
    pub delay: Option<f64>,
}

/// Aggregated WebSocket performance statistics per endpoint.
#[derive(Clone, Debug, Serialize)]
pub struct WebSocketStats {
    pub nickname: String,
    pub win_rate: f64,
    pub avg_delay: f64,
    pub slot_details: SlotUpdate,
}

/// Full response returned by the /api/ws_metrics endpoint.
#[derive(Debug, Serialize)]
pub struct WebSocketMetricsResponse {
    pub start_slot: u64,
    pub latest_slot: u64,
    pub stats: Vec<WebSocketStats>,
}

/// Configuration loaded from `config.toml`, includes RPC and WS sections.
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub rpc: RpcConfig,
    pub ws: RpcConfig,
}

/// Subsection of configuration listing endpoints.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RpcConfig {
    pub endpoints: Vec<RpcEndpoint>,
}

/// RPC polling response payload.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RPCResponse {
    pub timestamp: f64,
    pub slot: u64,
    pub blockhash: String,
    pub latency_ms: u128,
    pub rpc_url: String,
    pub nickname: String,
}

/// Application state shared across Axum routes.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<DB>,
    pub ws_db: Arc<DB>,
    pub config: Config,
}

/// Used for ranking latency and slot positions.
#[derive(Debug, Serialize)]
pub struct LeaderboardEntry {
    pub nickname: String,
    pub value: u64,
    pub latency_ms: u128,
    pub timestamp: f64,
}

/// Summarizes consensus and performance stats across endpoints.
#[derive(Debug, Serialize)]
pub struct ConsensusStats {
    pub fastest_rpc: String,
    pub slowest_rpc: String,
    pub fastest_latency: u128,
    pub slowest_latency: u128,
    pub consensus_blockhash: String,
    pub consensus_slot: u64,
    pub consensus_percentage: f64,
    pub total_rpcs: usize,
    pub average_latency: f64,
    pub slot_difference: i64,
    pub slot_skew: String,
    pub latency_leaderboard: Vec<LeaderboardEntry>,
    pub slot_leaderboard: Vec<LeaderboardEntry>,
}

/// Initializes and returns a RocksDB instance for storing RPC metrics.
/// The database is configured with LZ4 compression and a 64MB write buffer.
///
/// # Returns
/// A shared `Arc<DB>` handle pointing to the opened RocksDB instance.
fn setup_db() -> Arc<DB> {
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.set_write_buffer_size(64 * 1024 * 1024);
    opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
    
    Arc::new(DB::open(&opts, "rpc_metrics.db").expect("Failed to open database"))
}

/// Initializes and returns a separate RocksDB instance for storing WebSocket slot updates.
/// Uses the same configuration as `setup_db`, but stores data in `websocket_metrics.db`.
///
/// # Returns
/// A shared `Arc<DB>` handle pointing to the opened WebSocket RocksDB instance.
fn setup_websocket_db() -> Arc<DB> {
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.set_write_buffer_size(64 * 1024 * 1024);
    opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
    Arc::new(DB::open(&opts, "websocket_metrics.db").expect("Failed to open websocket database"))
}

/// Loads the configuration file (`config.toml`) and deserializes it into a `Config` struct.
///
/// # Returns
/// `Ok(Config)` if successful, otherwise an error.
fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    let config_str = fs::read_to_string("config.toml")?;
    let config: Config = toml::from_str(&config_str)?;
    Ok(config)
}

/// Queries a Solana RPC endpoint for its current blockhash and slot.
/// Measures request latency and stores the response in RocksDB with a timestamp.
///
/// # Arguments
/// * `endpoint` - The Solana RPC endpoint to query.
/// * `db` - Shared RocksDB database to persist the response.
///
/// # Returns
/// `Ok(())` on success, or an error if any RPC or RocksDB operation fails.
async fn fetch_blockhash_and_slot(endpoint: RpcEndpoint, db: Arc<DB>) -> Result<(), Box<dyn std::error::Error>> {
    println!("Querying {}: {}", endpoint.nickname, endpoint.url);
    let client = RpcClient::new(endpoint.url.clone());
    let start_time = Instant::now();
    
    let blockhash = match client.get_latest_blockhash() {
        Ok(hash) => hash.to_string(),
        Err(err) => format!("Error: {}", err),
    };
    
    let slot = match client.get_slot() {
        Ok(slot) => slot,
        Err(err) => {
            println!("Error fetching slot from {}: {}", endpoint.url, err);
            0
        }
    };
    
    let latency = start_time.elapsed().as_millis();
    
    let response = RPCResponse {
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64(),
        slot,
        blockhash: blockhash.clone(),
        latency_ms: latency,
        rpc_url: endpoint.url.clone(),
        nickname: endpoint.nickname.clone(),
    };
    
    let key = format!("{}:{}", endpoint.url, Utc::now().timestamp());
    let value = serde_json::to_string(&response)?;
    db.put(key.as_bytes(), value.as_bytes())?;
    
    println!("[{}] Slot: {}, Blockhash: {} ({}ms)", 
        endpoint.nickname, slot, blockhash, latency);
    
    Ok(())
}

/// Aggregates RPC responses to determine consensus metrics, such as:
/// - Most common blockhash and slot
/// - Fastest/slowest RPC endpoints by latency
/// - Slot skew, leaderboards, and average latency
///
/// # Arguments
/// * `responses` - A slice of `RPCResponse` items to analyze.
///
/// # Returns
/// A `ConsensusStats` struct containing the aggregated metrics.
fn calculate_consensus(responses: &[RPCResponse]) -> ConsensusStats {
    if responses.is_empty() {
        return ConsensusStats {
            fastest_rpc: String::from("No data"),
            slowest_rpc: String::from("No data"),
            fastest_latency: 0,
            slowest_latency: 0,
            consensus_blockhash: String::from("No data"),
            consensus_slot: 0,
            consensus_percentage: 0.0,
            total_rpcs: 0,
            average_latency: 0.0,
            slot_difference: 0,
            slot_skew: String::from("No data"),
            latency_leaderboard: Vec::new(),
            slot_leaderboard: Vec::new(),
        };
    }
    let mut blockhash_counts: HashMap<String, usize> = HashMap::new();
    let mut slot_counts: HashMap<u64, usize> = HashMap::new();
    let total_rpcs = responses.len();

    // Count occurrences of each blockhash and slot
    for response in responses {
        *blockhash_counts.entry(response.blockhash.clone()).or_insert(0) += 1;
        *slot_counts.entry(response.slot).or_insert(0) += 1;
    }

    // Find consensus values
    let consensus_blockhash = blockhash_counts
        .iter()
        .max_by_key(|&(_, count)| count)
        .map(|(hash, count)| (hash.clone(), *count))
        .unwrap_or((String::from("No consensus"), 0));

    let consensus_slot = slot_counts
        .iter()
        .max_by_key(|&(_, count)| count)
        .map(|(&slot, _)| slot)
        .unwrap_or(0);

    // Calculate consensus percentage
    let consensus_percentage = (consensus_blockhash.1 as f64 / total_rpcs as f64) * 100.0;

    // Find fastest and slowest RPCs with their latencies
    let fastest = responses
        .iter()
        .min_by_key(|r| r.latency_ms)
        .unwrap();

    let slowest = responses
        .iter()
        .max_by_key(|r| r.latency_ms)
        .unwrap();

    // Calculate slot differences and skew
    let slot_difference = fastest.slot as i64 - slowest.slot as i64;
    let slot_skew = if slot_difference == 0 {
        "No skew".to_string()
    } else if slot_difference > 0 {
        format!("Fastest ahead by {} slots", slot_difference.abs())
    } else {
        format!("Slowest ahead by {} slots", slot_difference.abs())
    };

    // Calculate average latency
    let average_latency = responses
        .iter()
        .map(|r| r.latency_ms as f64)
        .sum::<f64>() / total_rpcs as f64;

    // Create leaderboards
    let mut latency_leaderboard: Vec<LeaderboardEntry> = responses.iter()
        .map(|r| LeaderboardEntry {
            nickname: r.nickname.clone(),
            value: r.latency_ms as u64,
            latency_ms: r.latency_ms,
            timestamp: r.timestamp,
        })
        .collect();
    latency_leaderboard.sort_by_key(|entry| entry.value);
    latency_leaderboard.truncate(4); // Keep top 4

    let mut slot_leaderboard: Vec<LeaderboardEntry> = responses.iter()
        .map(|r| LeaderboardEntry {
            nickname: r.nickname.clone(),
            value: r.slot,
            latency_ms: r.latency_ms,
            timestamp: r.timestamp,
        })
        .collect();
    slot_leaderboard.sort_by(|a, b| b.value.cmp(&a.value));
    slot_leaderboard.truncate(4); // Keep top 4

    ConsensusStats {
        fastest_rpc: fastest.nickname.clone(),
        slowest_rpc: slowest.nickname.clone(),
        fastest_latency: fastest.latency_ms,
        slowest_latency: slowest.latency_ms,
        consensus_blockhash: consensus_blockhash.0,
        consensus_slot,
        consensus_percentage,
        total_rpcs,
        average_latency,
        slot_difference,
        slot_skew,
        latency_leaderboard,
        slot_leaderboard,
    }
}

/// Axum handler for `/api/metrics`. Retrieves filtered RPC responses from the database,
/// applies time and RPC filters, computes consensus stats, and returns the result.
///
/// # Arguments
/// * `state` - Shared application state containing the database and config.
/// * `params` - Query parameters for filtering by `rpc`, `from`, and `to`.
///
/// # Returns
/// A JSON response containing RPC response list and consensus statistics.
async fn get_metrics(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>
) -> Json<(Vec<RPCResponse>, ConsensusStats)> {
    let db = state.db;
    let mut responses = Vec::new();
    
    let rpc_filter = params.get("rpc");
    let from_ts = params.get("from")
        .and_then(|ts| ts.parse::<i64>().ok());
    let to_ts = params.get("to")
        .and_then(|ts| ts.parse::<i64>().ok());
    
    // Get most recent response for each RPC
    let mut latest_by_rpc: HashMap<String, RPCResponse> = HashMap::new();
    
    let iter = db.iterator(rocksdb::IteratorMode::End);
    for item in iter {
        if let Ok((key, value)) = item {
            let key_str = String::from_utf8_lossy(&key);
            if let Ok(response) = serde_json::from_slice::<RPCResponse>(&value) {
                if !latest_by_rpc.contains_key(&response.rpc_url) {
                    latest_by_rpc.insert(response.rpc_url.clone(), response.clone());
                }
                
                if let Some((url, _)) = key_str.split_once(':') {
                    let matches_rpc = rpc_filter
                        .as_ref()
                        .map_or(true, |filter| url.contains(filter.as_str()));
                    let matches_time = match (from_ts, to_ts) {
                        (Some(from), Some(to)) => response.timestamp >= from as f64 && response.timestamp <= to as f64,
                        (Some(from), None) => response.timestamp >= from as f64,
                        (None, Some(to)) => response.timestamp <= to as f64,
                        (None, None) => true,
                    };
                    
                    if matches_rpc && matches_time {
                        responses.push(response);
                    }
                }
            }
        }
    }
    
    responses.sort_by(|a, b| b.timestamp.partial_cmp(&a.timestamp).unwrap_or(std::cmp::Ordering::Equal));
    
    // Calculate consensus based on latest response from each RPC
    let consensus_stats = calculate_consensus(&latest_by_rpc.values().cloned().collect::<Vec<_>>());
    
    // Remove sensitive rpc_url before returning the data
    let public_responses: Vec<RPCResponse> = responses
        .into_iter()
        .map(|mut r| {
            r.rpc_url = String::new();
            r
        })
        .collect();
    
    Json((public_responses, consensus_stats))
}

/// Axum handler for `/api/ws_metrics`. Computes win rates, delays, and recent slot info
/// for each WebSocket endpoint over a defined evaluation slot range.
///
/// # Arguments
/// * `state` - Shared application state with RocksDB and config.
///
/// # Returns
/// A JSON response with WebSocket metrics and recent slot statistics.
async fn get_ws_metrics(State(state): State<AppState>) -> Json<WebSocketMetricsResponse> {
    let db = Arc::clone(&state.ws_db);
    let eval_range = 200;

    let latest_slot = match get_latest_websocket_slot(Arc::clone(&db)) {
        Some(slot) => slot,
        None => {
            return Json(WebSocketMetricsResponse {
                start_slot: 0,
                latest_slot: 0,
                stats: vec![],
            })
        }
    };

    let candidate1 = latest_slot.saturating_sub(eval_range);
    let candidate2 = get_earliest_common_websocket_slot_with_min(Arc::clone(&db), candidate1).unwrap_or(0);
    let start_slot = candidate1.max(candidate2);

    let endpoint_names: Vec<String> = state
        .config
        .ws
        .endpoints
        .iter()
        .map(|e| e.nickname.clone())
        .collect();

    let win_rates = match compute_win_rates_with_range(
        Arc::clone(&db),
        &endpoint_names,
        start_slot,
        latest_slot,
    )
    .await
    {
        Some(rates) => rates,
        None => {
            return Json(WebSocketMetricsResponse {
                start_slot: 0,
                latest_slot: 0,
                stats: vec![],
            })
        }
    };

    let avg_list = match compute_avg_delays_with_range(
        Arc::clone(&db),
        &endpoint_names,
        start_slot,
        latest_slot,
    )
    .await
    {
        Some(list) => list,
        None => {
            return Json(WebSocketMetricsResponse {
                start_slot: 0,
                latest_slot: 0,
                stats: vec![],
            })
        }
    };

    let slot_details =
        get_slot_updates_for_target_slot(Arc::clone(&db), &endpoint_names, latest_slot);

    let mut stats = Vec::new();
    for nickname in &endpoint_names {
        let win_rate = *win_rates.get(nickname).unwrap_or(&0.0);
        let avg_delay = avg_list
            .iter()
            .find(|(nick, _)| nick == nickname)
            .and_then(|(_, delay_opt)| *delay_opt)
            .unwrap_or(-1.0);
        let slot_details = slot_details
            .get(nickname)
            .cloned()
            .unwrap_or(SlotUpdate {
                timestamp: 0.0,
                nickname: nickname.clone(),
                slot_info: SlotNotificationResult {
                    slot: 0,
                    parent: 0,
                    root: 0,
                },
                delay: Some(-1.0),
            });

        stats.push(WebSocketStats {
            nickname: nickname.clone(),
            win_rate,
            avg_delay,
            slot_details,
        });
    }

    let response = WebSocketMetricsResponse {
        start_slot,
        latest_slot,
        stats,
    };

    if false {
        match serde_json::to_string_pretty(&response) {
            Ok(json_str) => println!("\n🛰️ WebSocketMetricsResponse JSON:\n{}", json_str),
            Err(err) => eprintln!("Failed to serialize WebSocketMetricsResponse: {}", err),
        }
    }

    Json(response)
}

/// Deletes RPC entries from RocksDB that are older than one hour.
///
/// # Arguments
/// * `db` - RocksDB instance to clean up.
///
/// # Returns
/// `Ok(())` if cleanup succeeds; otherwise returns an error.
async fn cleanup_old_entries(db: Arc<DB>) -> Result<(), Box<dyn std::error::Error>> {
    let one_hour_ago = Utc::now() - Duration::hours(1);
    let one_hour_ago_ts = one_hour_ago.timestamp();

    let mut batch = rocksdb::WriteBatch::default();
    let iter = db.iterator(rocksdb::IteratorMode::Start);
    for item in iter {
        if let Ok((key, value)) = item {
            if let Ok(response) = serde_json::from_slice::<RPCResponse>(&value) {
                if response.timestamp < one_hour_ago_ts as f64 {
                    batch.delete(key);
                }
            }
        }
    }
    db.write(batch)?;

    Ok(())
}

/// Deletes WebSocket slot entries whose slot number is older than the threshold from the latest.
///
/// # Arguments
/// * `db` - RocksDB database for WebSocket data.
/// * `threshold` - Number of slots to retain.
///
/// # Returns
/// `Ok(())` on success, or an error on failure.
async fn cleanup_websocket_db_with_threshold(
    db: Arc<DB>,
    threshold: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(latest_slot) = get_latest_websocket_slot(db.clone()) else {
        return Ok(());
    };
    let min_slot = latest_slot.saturating_sub(threshold);

    let mut batch = rocksdb::WriteBatch::default();
    let iter = db.iterator(rocksdb::IteratorMode::Start);
    for item in iter {
        if let Ok((key, _value)) = item {
            if let Ok(key_str) = std::str::from_utf8(&key) {
                if let Some((_nickname, slot_str)) = key_str.split_once(':') {
                    if let Ok(slot) = slot_str.parse::<u64>() {
                        if slot < min_slot {
                            batch.delete(key);
                        }
                    }
                }
            }
        }
    }

    db.write(batch)?;
    Ok(())
}

/// Shortcut for cleaning up WebSocket database with a default threshold of 500 slots.
///
/// # Arguments
/// * `db` - RocksDB instance for WebSocket data.
///
/// # Returns
/// `Ok(())` or an error on failure.
async fn cleanup_websocket_db(db: Arc<DB>) -> Result<(), Box<dyn std::error::Error>> {
    cleanup_websocket_db_with_threshold(db, 500).await
}


/// Connects to a Solana WebSocket endpoint and sends a `slotSubscribe` request.
///
/// # Arguments
/// * `url` - Parsed WebSocket URL to connect to.
/// * `endpoint` - Endpoint metadata (nickname and URL).
///
/// # Returns
/// A WebSocket stream used to receive slot notifications.

async fn connect_and_subscribe(url: Url, endpoint: &RpcEndpoint) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    // Connect to the websocket server.
    let (mut ws_stream, response) = connect_async(url)
        .await
        .expect("Failed to connect to websocket");

    println!(
        "Connected to websocket server {} with HTTP status: {}",
        endpoint.nickname,
        response.status()
    );

    // Create the JSON subscription request.
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "slotSubscribe"
    });

    // Send the subscription message.
    ws_stream
        .send(Message::Text(request.to_string()))
        .await
        .expect("Failed to send subscribe message");

    // Return the websocket stream for further processing.
    ws_stream
}

/// Processes messages received from a WebSocket stream.
/// Parses slot notifications and stores them in the database.
///
/// # Arguments
/// * `ws_stream` - Active WebSocket connection.
/// * `websocket_db` - RocksDB database for WebSocket slot updates.
/// * `endpoint` - Metadata for the current endpoint.
async fn handle_websocket_messages(
    mut ws_stream: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    websocket_db: Arc<DB>,
    endpoint: RpcEndpoint
) {
    // Continuously receive messages from the WebSocket and process them.
    while let Some(message) = ws_stream.next().await {
        match message {
            Ok(msg) => {
                match msg {
                    Message::Text(text) => {
                        // Try to parse the incoming text as a WebSocketMessage.
                        match serde_json::from_str::<WebSocketMessage>(&text) {
                            Ok(ws_message) => {
                                // Check the method field.
                                if ws_message.method == "slotNotification" {
                                    // Call a separate function to process slot notifications.
                                    //println!("slotNotification message with params: {}", *&text);
                                    process_slot_notification(ws_message, &endpoint, Arc::clone(&websocket_db)).await;
                                } else {
                                    println!("Received a non-slotNotification message with method: {}", ws_message.method);
                                }
                            },
                            Err(e) => {
                                println!("Received text message: {}", text);
                                eprintln!("Failed to parse incoming message as WebSocketMessage: {}", e);
                            }
                        }
                    },
                    Message::Binary(bin) => {
                        println!("Received binary message: {:?}", bin);
                    },
                    Message::Ping(data) => {
                        println!("Received ping: {:?}", data);
                    },
                    Message::Pong(data) => {
                        println!("Received pong: {:?}", data);
                    },
                    Message::Close(frame) => {
                        println!("Received close frame: {:?}", frame);
                        break; // Exit the loop on close.
                    },
                    _ => {
                        println!("Received an unexpected type of message.");
                    }
                }
            },
            Err(e) => {
                eprintln!("Error receiving message: {}", e);
                break;
            }
        }
    }
}

/// Processes a single slot notification, generating a `SlotUpdate` and saving it to RocksDB.
///
/// # Arguments
/// * `ws_message` - Parsed `WebSocketMessage` containing slot info.
/// * `endpoint` - RPC endpoint metadata.
/// * `websocket_db` - RocksDB for storing slot updates.
async fn process_slot_notification(
    ws_message: WebSocketMessage,
    endpoint: &RpcEndpoint,
    websocket_db: Arc<DB>,
) {
    let timestamp = match Utc::now().timestamp_nanos_opt() {
        Some(n) => n as f64 / 1e9,
        None => {
            eprintln!("Failed to obtain high-precision timestamp.");
            return;
        }
    };

    let nickname = endpoint.nickname.clone();
    let slot_info = ws_message.params.result.clone();

    let slot_update = SlotUpdate {
        timestamp,
        nickname: nickname.clone(),
        slot_info: slot_info.clone(),
        delay: None,
    };

    let serialized = serde_json::to_string(&slot_update)
        .expect("Failed to serialize SlotUpdate");

    let key = format!("{}:{}", nickname, slot_info.slot);

    websocket_db
        .put(key.as_bytes(), serialized.as_bytes())
        .expect("Failed to write slot update to websocket_db");
}

/// Finds and returns the highest slot number stored in the WebSocket RocksDB.
///
/// # Arguments
/// * `db` - WebSocket RocksDB instance.
///
/// # Returns
/// The highest slot number, or `None` if no data exists.
fn get_latest_websocket_slot(db: Arc<DB>) -> Option<u64> {
    let iter = db.iterator(IteratorMode::Start);
    let mut max_slot: Option<u64> = None;

    for item in iter {
        if let Ok((key, _)) = item {
            if let Ok(key_str) = str::from_utf8(&key) {
                // Split the key at the colon.
                let parts: Vec<&str> = key_str.split(':').collect();
                if parts.len() == 2 {
                    // Parse the slot number, which should be the second part.
                    if let Ok(slot) = parts[1].parse::<u64>() {
                        max_slot = Some(max_slot.map_or(slot, |max| max.max(slot)));
                    }
                }
            }
        }
    }
    max_slot
}

/// Retrieves `SlotUpdate` entries for a specific slot across all endpoints.
/// Fills missing data with placeholder entries.
///
/// # Arguments
/// * `db` - WebSocket RocksDB instance.
/// * `endpoint_names` - List of endpoint nicknames.
/// * `slot` - Target slot number.
///
/// # Returns
/// A mapping of endpoint nicknames to their respective `SlotUpdate` structs.
pub fn get_slot_updates_for_target_slot(
    db: Arc<DB>,
    endpoint_names: &[String],
    slot: u64,
) -> HashMap<String, SlotUpdate> {
    let mut updates: HashMap<String, SlotUpdate> = HashMap::new();

    // Determine the winner's timestamp (earliest valid timestamp for the slot)
    let winner_ts = endpoint_names
        .iter()
        .filter_map(|nickname| {
            let key = format!("{}:{}", nickname, slot);
            db.get(key.as_bytes()).ok().flatten().and_then(|value| {
                serde_json::from_slice::<SlotUpdate>(&value).ok()
            }).map(|su| su.timestamp)
        })
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    // Precompute fallback delay if winner_ts exists
    let fallback_delay = winner_ts.map(|winner| {
        let now = Utc::now().timestamp_nanos_opt().unwrap_or(0) as f64 / 1e9;
        (now - winner) * 1000.0
    });

    for nickname in endpoint_names {
        let key = format!("{}:{}", nickname, slot);
        if let Ok(Some(value)) = db.get(key.as_bytes()) {
            if let Ok(slot_update) = serde_json::from_slice::<SlotUpdate>(&value) {
                updates.insert(nickname.clone(), slot_update);
                continue;
            }
        }

        // If no update found, insert fallback SlotUpdate
        updates.insert(
            nickname.clone(),
            SlotUpdate {
                timestamp: -1.0,
                nickname: nickname.clone(),
                slot_info: SlotNotificationResult {
                    slot,
                    parent: u64::MAX,
                    root: u64::MAX,
                },
                delay: fallback_delay,
            },
        );
    }

    updates
}

/// Determines the earliest slot number shared across all endpoints above a given minimum.
///
/// # Arguments
/// * `db` - WebSocket RocksDB instance.
/// * `min_slot` - Minimum slot to consider.
///
/// # Returns
/// The earliest shared slot, or `None` if no overlap is found.
fn get_earliest_common_websocket_slot_with_min(
    db: Arc<rocksdb::DB>,
    min_slot: u64,
) -> Option<u64> {
    let mut endpoint_slots: HashMap<String, HashSet<u64>> = HashMap::new();

    let iter = db.iterator(IteratorMode::Start);
    for item in iter {
        if let Ok((key, _)) = item {
            if let Ok(key_str) = str::from_utf8(&key) {
                if let Some((nickname, slot_str)) = key_str.split_once(':') {
                    if let Ok(slot) = slot_str.parse::<u64>() {
                        if slot >= min_slot {
                            endpoint_slots
                                .entry(nickname.to_string())
                                .or_insert_with(HashSet::new)
                                .insert(slot);
                        }
                    }
                }
            }
        }
    }

    if endpoint_slots.is_empty() {
        return None;
    }

    let mut common_slots: Option<HashSet<u64>> = None;
    for (_nickname, slots) in endpoint_slots {
        common_slots = match common_slots {
            None => Some(slots),
            Some(current_common) => {
                let intersection: HashSet<u64> = current_common
                    .intersection(&slots)
                    .copied()
                    .collect();
                Some(intersection)
            }
        };
    }

    common_slots?.into_iter().min()
}

/// Determines the "winner" of a slot (the endpoint that reported it first),
/// then updates delay values for all participants.
///
/// # Arguments
/// * `db` - WebSocket RocksDB instance.
/// * `endpoint_names` - List of endpoint nicknames.
/// * `target_slot` - Slot number to evaluate.
///
/// # Returns
/// The nickname and timestamp of the winner, or `None` if no entries exist.
pub async fn get_slot_winner(
    db: Arc<DB>,
    endpoint_names: &[String],
    target_slot: u64,
) -> Option<(String, f64)> {
    let mut slot_updates: Vec<SlotUpdate> = Vec::new();

    for nickname in endpoint_names {
        let key = format!("{}:{}", nickname, target_slot);
        if let Ok(Some(value)) = db.get(key.as_bytes()) {
            if let Ok(slot_update) = serde_json::from_slice::<SlotUpdate>(&value) {
                if slot_update.slot_info.slot == target_slot {
                    slot_updates.push(slot_update);
                }
            }
        }
    }

    if slot_updates.is_empty() {
        return None;
    }

    let winner = slot_updates
        .iter()
        .min_by(|a, b| a.timestamp.partial_cmp(&b.timestamp).unwrap_or(std::cmp::Ordering::Equal))?;

    // Update delays for all slot_updates relative to the winner's timestamp
    update_delays_for_timestamp(Arc::clone(&db), target_slot, slot_updates.clone(), winner.timestamp).await;

    Some((winner.nickname.clone(), winner.timestamp))
}

/// Updates the delay field in all `SlotUpdate`s for a given slot based on the winner's timestamp.
///
/// # Arguments
/// * `db` - RocksDB instance.
/// * `slot` - Slot number to update.
/// * `updates` - Vector of `SlotUpdate`s to modify.
/// * `winner_ts` - Timestamp of the winning endpoint.
pub async fn update_delays_for_timestamp(
    db: Arc<rocksdb::DB>,
    slot: u64,
    updates: Vec<SlotUpdate>,
    winner_ts: f64,
) {
    let db_clone = Arc::clone(&db);

    task::spawn_blocking(move || {
        let mut batch = rocksdb::WriteBatch::default();

        for mut update in updates {
            update.delay = Some((update.timestamp - winner_ts) * 1000.0);
            let key = format!("{}:{}", update.nickname, slot);
            if let Ok(serialized) = serde_json::to_vec(&update) {
                batch.put(key.as_bytes(), &serialized);
            }
        }

        if let Err(e) = db_clone.write(batch) {
            eprintln!("❌ Failed to write batch delay updates: {}", e);
        }
    })
    .await
    .expect("Failed to spawn blocking batch write task");
}

/// Calculates win rates (percent of wins) for each endpoint over a slot range.
///
/// # Arguments
/// * `db` - WebSocket RocksDB instance.
/// * `endpoint_names` - List of endpoint nicknames.
/// * `start_slot` - Start of evaluation range.
/// * `latest_slot` - End of evaluation range.
///
/// # Returns
/// Map of nicknames to their win percentages, or `None` if no data found.
pub async fn compute_win_rates_with_range(
    db: Arc<DB>,
    endpoint_names: &[String],
    start_slot: u64,
    latest_slot: u64,
) -> Option<HashMap<String, f64>> {
    let mut win_counts: HashMap<String, u64> = HashMap::new();
    let mut total_evaluated = 0;

    for slot in start_slot..=latest_slot {
        if let Some((winner, _)) = get_slot_winner(Arc::clone(&db), endpoint_names, slot).await {
            total_evaluated += 1;
            *win_counts.entry(winner).or_insert(0) += 1;
        }
    }

    let win_sum: u64 = win_counts.values().sum();
    if win_sum != total_evaluated {
        println!(
            "⚠️ Win count total ({}) does not match total evaluated slots ({}).",
            win_sum, total_evaluated
        );
    }

    if total_evaluated == 0 {
        return None;
    }

    // Convert counts to percentages
    let win_rates = win_counts
        .into_iter()
        .map(|(nickname, count)| (nickname, (count as f64 / total_evaluated as f64) * 100.0))
        .collect();

    Some(win_rates)
}

/// Computes the average delay for each endpoint across a given slot range.
///
/// # Arguments
/// * `db` - RocksDB instance.
/// * `endpoint_names` - List of endpoint nicknames.
/// * `start_slot` - Starting slot number.
/// * `latest_slot` - Ending slot number.
///
/// # Returns
/// Vector of tuples (nickname, average delay), or `None` if no data.
pub async fn compute_avg_delays_with_range(
    db: Arc<DB>,
    endpoint_names: &[String],
    start_slot: u64,
    latest_slot: u64,
) -> Option<Vec<(String, Option<f64>)>> {
    if start_slot > latest_slot {
        return None;
    }

    let mut results = Vec::new();
    for endpoint in endpoint_names {
        let delay = avg_delay_for_endpoint(Arc::clone(&db), endpoint_names, endpoint, start_slot, latest_slot).await;
        results.push((endpoint.clone(), delay));
    }

    Some(results)
}

/// Computes the average delay in milliseconds for a single endpoint over a slot range.
///
/// # Arguments
/// * `db` - RocksDB database instance.
/// * `endpoint_names` - All endpoint names (needed for fallback).
/// * `nickname` - Endpoint to compute delay for.
/// * `start_slot` - Beginning of slot range.
/// * `end_slot` - End of slot range.
///
/// # Returns
/// `Some(average_delay)` or `None` if no valid delay values found.
pub async fn avg_delay_for_endpoint(
    db: Arc<DB>,
    endpoint_names: &[String],
    nickname: &str,
    start_slot: u64,
    end_slot: u64,
) -> Option<f64> {
    let mut total_delay = 0.0;
    let mut count = 0;

    for slot in start_slot..=end_slot {
        let key = format!("{}:{}", nickname, slot);
        if let Ok(Some(value)) = db.get(key.as_bytes()) {
            if let Ok(slot_update) = serde_json::from_slice::<SlotUpdate>(&value) {
                if let Some(delay) = slot_update.delay {
                    total_delay += delay;
                    count += 1;
                    continue;
                }
            }
        }

        if let Some((_, winner_ts)) = get_slot_winner(Arc::clone(&db), endpoint_names, slot).await {
            let now = match Utc::now().timestamp_nanos_opt() {
                Some(n) => n as f64 / 1e9,
                None => {
                    eprintln!("Failed to obtain high-precision timestamp.");
                    continue;
                }
            };
            total_delay += (now - winner_ts) * 1000.0;
            count += 1;
        }
    }

    if count > 0 {
        Some(total_delay / count as f64)
    } else {
        None
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {

    let db = setup_db();
    let websocket_db = setup_websocket_db();
    
    let config = match load_config() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to load config: {}", e);
            return Ok(());
        }
    };

    let app_state = AppState {
        db: Arc::clone(&db),
        ws_db: Arc::clone(&websocket_db),
        config: config.clone(), // the loaded config including websocket_endpoints
    };

    // Spawn websocket tasks for every endpoint in the [ws] section of the config file
    let ws_endpoints = config.ws.endpoints.clone();
    for endpoint in ws_endpoints {

        // Parse the URL
        let url = Url::parse(&endpoint.url).expect("Invalid URL");
        let ws_db_clone = Arc::clone(&websocket_db);
        let endpoint_clone = endpoint.clone();

        tokio::spawn(async move {
            let ws_stream = connect_and_subscribe(url, &endpoint).await;
            // Process messages for this ws_stream in your handler.
            handle_websocket_messages(ws_stream, ws_db_clone, endpoint_clone).await;
        });
    }


    std::fs::create_dir_all("static")?;
    std::fs::write(
        "static/index.html",
        include_str!("static/index.html")
    )?;

    /*
        Include the web socket monitoring file in the compiled project
    */
    let js_files = vec![
        "web_socket_monitoring_new.js",
    ];
    for filename in js_files {
        let source_str = format!("src/static/js/{}", filename); // <-- create String first
        let source = Path::new(&source_str);
        let destination_dir = Path::new("static/js");
        fs::create_dir_all(destination_dir).expect("Failed to create static/js directory");

        let destination = destination_dir.join(filename);
        fs::copy(source, &destination).expect(&format!("Failed to copy {}", filename));
    }

    let db_clone = Arc::clone(&db);
    let endpoints = app_state.config.rpc.endpoints.clone();

    tokio::spawn(async move {
        loop {
            let tasks: Vec<_> = endpoints
                .clone()
                .into_iter()
                .map(|endpoint| {
                    let db = Arc::clone(&db_clone);
                    task::spawn(async move {
                        if let Err(e) = fetch_blockhash_and_slot(endpoint, db).await {
                            eprintln!("Error: {}", e);
                        }
                    })
                })
                .collect();

            join_all(tasks).await;
            tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
        }
    });

    let db_clone = Arc::clone(&db);
    let ws_db_clone = Arc::clone(&websocket_db);
    tokio::spawn(async move {
        loop {
            if let Err(e) = cleanup_old_entries(db_clone.clone()).await {
                eprintln!("Error cleaning up old entries: {}", e);
            }
            if let Err(e) = cleanup_websocket_db(ws_db_clone.clone()).await {
                eprintln!("Error cleaning up old websocket entries: {}", e);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await; // Run cleanup every minute
        }
    });

    let app = Router::new()
        .route("/", get(|| async { 
            axum::response::Redirect::to("/static/index.html")
        }))
        .route("/api/metrics", get(get_metrics))
        .route("/api/ws_metrics", get(get_ws_metrics))
        .nest_service("/static", get_service(ServeDir::new("static")))
        .with_state(app_state);
    
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Server running on http://localhost:3000");
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}
