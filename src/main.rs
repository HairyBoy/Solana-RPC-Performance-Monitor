#![allow(dead_code)]
// Axum (routing and extractors)
use axum::{
    extract::{Query, State},
    routing::{get, get_service},
    Json, Router,
};
// RocksDB
use rocksdb::{DB, IteratorMode, Options};
// Serde (serialization)
use serde::{Deserialize, Serialize};
use serde_json::json;
// Standard library
use std::{
    collections::{HashMap, HashSet},
    fs,
    net::SocketAddr,
    path::Path,
    str,
    sync::Arc,
    time::Instant,
};
// Chrono (timestamps)
use chrono::{Duration, Utc};
// Tower HTTP (static file serving)
use tower_http::services::ServeDir;
// Tokio
use tokio::task;
use tokio_tungstenite::{
    connect_async,
    tungstenite::protocol::Message,
};
use futures::{future::join_all, SinkExt, StreamExt};
// Solana
use solana_client::rpc_client::RpcClient;
// URL parsing
use url::Url;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RpcEndpoint {
    url: String,
    nickname: String,
}

/// Represents the slot information contained within the notification.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SlotNotificationResult {
    /// The current slot number.
    slot: u64,
    /// The parent slot number.
    parent: u64,
    /// The root slot number.
    root: u64,
}

/// Holds the parameters section of the notification.
#[derive(Debug, Serialize, Deserialize)]
pub struct NotificationParams {
    /// The result contains the slot information.
    result: SlotNotificationResult,
    /// The subscription id for the notification.
    subscription: u64,
}

/// Represents an entire slot notification received via WebSocket,
/// including the JSON-RPC envelope and custom fields such as timestamp and nickname.
#[derive(Debug, Serialize, Deserialize)]
pub struct WebSocketMessage {
    jsonrpc: String,
    method: String,
    params: NotificationParams
}

/// Represents an entire slot notification received via WebSocket,
/// including the JSON-RPC envelope and custom fields such as timestamp,
/// nickname, and a delay value in milliseconds.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SlotUpdate {
    /// The time (in seconds, with fractional part) when the message was received or stored.
    pub timestamp: f64,
    /// The nickname of the endpoint that sent the notification.
    pub nickname: String,
    /// The slot information.
    pub slot_info: SlotNotificationResult,
    /// The delay in milliseconds, optional until calculated.
    pub delay: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebSocketStats {
    nickname: String,
    win_rate: f64,
    avg_delay: f64,
    slot_details: SlotUpdate,
}

#[derive(Debug, Serialize)]
pub struct WebSocketMetricsResponse {
    start_slot: u64,
    latest_slot: u64,
    stats: Vec<WebSocketStats>,
}

#[derive(Debug, Deserialize, Clone)]
struct Config {
    rpc: RpcConfig,
    ws: RpcConfig,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RpcConfig {
    endpoints: Vec<RpcEndpoint>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RPCResponse {
    timestamp: f64,
    slot: u64,
    blockhash: String,
    latency_ms: u128,
    rpc_url: String,
    nickname: String,
}

#[derive(Clone)]
struct AppState {
    db: Arc<DB>,
    ws_db: Arc<DB>,
    config: Config,
}

#[derive(Debug, Serialize)]
struct LeaderboardEntry {
    nickname: String,
    value: u64,
    latency_ms: u128,
    timestamp: f64,
}

#[derive(Debug, Serialize)]
struct ConsensusStats {
    fastest_rpc: String,
    slowest_rpc: String,
    fastest_latency: u128,
    slowest_latency: u128,
    consensus_blockhash: String,
    consensus_slot: u64,
    consensus_percentage: f64,
    total_rpcs: usize,
    average_latency: f64,
    slot_difference: i64,
    slot_skew: String,
    latency_leaderboard: Vec<LeaderboardEntry>,
    slot_leaderboard: Vec<LeaderboardEntry>,
}



fn setup_db() -> Arc<DB> {
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.set_write_buffer_size(64 * 1024 * 1024);
    opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
    
    Arc::new(DB::open(&opts, "rpc_metrics.db").expect("Failed to open database"))
}

/// Set up a second RocksDB database for WebSocket slot notifications.
fn setup_websocket_db() -> Arc<DB> {
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.set_write_buffer_size(64 * 1024 * 1024);
    opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
    Arc::new(DB::open(&opts, "websocket_metrics.db").expect("Failed to open websocket database"))
}

fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    let config_str = fs::read_to_string("config.toml")?;
    let config: Config = toml::from_str(&config_str)?;
    Ok(config)
}

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

async fn cleanup_websocket_db(db: Arc<DB>) -> Result<(), Box<dyn std::error::Error>> {
    cleanup_websocket_db_with_threshold(db, 500).await
}


/// Connects to the websocket server at the given URL, sends a subscription message,
/// and returns the websocket stream.
///
/// # Arguments
///
/// * `url` - The parsed Url of the websocket endpoint.
/// * `endpoint` - A reference to the RpcEndpoint struct containing metadata like nickname.
///
/// # Returns
///
/// A `tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>` that can be used to further process messages.
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

/// Processes a slot notification message. It calculates the current high-precision timestamp,
/// checks whether a SlotUpdate for the same slot with a delay of zero exists in the database,
/// computes the delay accordingly, creates a new SlotUpdate struct, serializes it, and saves it.
///
/// # Arguments
/// * `ws_message` - The parsed WebSocketMessage.
/// * `endpoint` - A reference to the RpcEndpoint metadata.
/// * `websocket_db` - An Arc pointer to the RocksDB instance for WebSocket notifications.
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

/// Returns the maximum slot number (latest slot) reported by any websocket,
/// based on the key format "nickname:slot".
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

/// Returns the earliest common slot number among all unique endpoints found in the RocksDB.
/// 
/// This function uses the fact that each key is in the format "nickname:slot"
///
/// # Arguments
///
/// * `db` - An `Arc<DB>` instance representing the RocksDB where the slot updates are stored.
///
/// # Returns
///
/// * `Option<u64>` - The lowest slot number that appears for every endpoint, or None if no common slot exists.
fn get_earliest_common_websocket_slot(db: Arc<rocksdb::DB>) -> Option<u64> {
    
    return get_earliest_common_websocket_slot_with_min(db, 0);
}

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

/// Uses the `get_latest_websocket_slot` function to check the database and prints
/// the latest (highest) slot value found.
fn print_latest_websocket_slot(db: Arc<DB>) {
    match get_latest_websocket_slot(db.clone()) {
        Some(slot) => println!("The latest slot reported by any websocket is: {}", slot),
        None => println!("No slot notifications found in the websocket_metrics database."),
    }
    match get_earliest_common_websocket_slot(db.clone()) {
        Some(slot) => println!("The earliest slot reported by all websockets is: {}", slot),
        None => println!("No slot notifications found in the websocket_metrics database."),
    }
}



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

/// Prints every key–value pair in the given RocksDB database.
///
/// This function iterates over the entire database starting from the beginning.
/// It attempts to convert each key and value into a UTF-8 string and then prints it.
/// If the conversion fails, it prints the raw bytes (in debug format).
/*fn print_database(db: &DB) {
    // Create an iterator starting from the beginning.
    let iter = db.iterator(IteratorMode::Start);
    for item in iter {
        match item {
            Ok((key, value)) => {
                // Try to convert the key and value to strings.
                let key_str = str::from_utf8(&key).unwrap_or("<non-utf8 key>");
                let value_str = str::from_utf8(&value).unwrap_or("<non-utf8 value>");
                println!("Key: {}, Value: {}", key_str, value_str);
            }
            Err(e) => {
                eprintln!("Error reading database entry: {:?}", e);
            }
        }
    }
}*/


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
