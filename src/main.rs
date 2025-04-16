use axum::{
    routing::{get, get_service},
    Router, Json, extract::Query, extract::State,
};
use rocksdb::{DB, IteratorMode, Options};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, str};
use std::collections::{HashMap, HashSet};
use tower_http::services::ServeDir;
use chrono::{Utc, Duration};
use std::net::SocketAddr;
use tokio::task;
use futures::future::join_all;
use std::time::Instant;
use solana_client::rpc_client::RpcClient;
use std::{fs, path::Path};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures::{StreamExt, SinkExt};
use url::Url;
use serde_json::{json};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RPCResponse {
    timestamp: f64,
    slot: u64,
    blockhash: String,
    latency_ms: u128,
    rpc_url: String,
    nickname: String,
}

/// Represents an entire slot notification received via WebSocket,
/// including the JSON-RPC envelope and custom fields such as timestamp,
/// nickname, and a delay value in milliseconds.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct SlotUpdate {
    /// The time (in seconds, with fractional part) when the message was received or stored.
    timestamp: f64,
    /// The nickname of the endpoint that sent the notification.
    nickname: String,
    /// The slot information.
    slot_info: SlotNotificationResult,
    /// The delay in milliseconds (calculated as described below).
    delay: f64,
}

/// Represents an entire slot notification received via WebSocket,
/// including the JSON-RPC envelope and custom fields such as timestamp and nickname.
#[derive(Debug, Serialize, Deserialize)]
struct WebSocketMessage {
    jsonrpc: String,
    method: String,
    params: NotificationParams
}

/// Holds the parameters section of the notification.
#[derive(Debug, Serialize, Deserialize)]
struct NotificationParams {
    /// The result contains the slot information.
    result: SlotNotificationResult,
    /// The subscription id for the notification.
    subscription: u64,
}

/// Represents the slot information contained within the notification.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct SlotNotificationResult {
    /// The current slot number.
    slot: u64,
    /// The parent slot number.
    parent: u64,
    /// The root slot number.
    root: u64,
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
struct RpcEndpoint {
    url: String,
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

#[derive(Clone, Debug, Serialize)]
struct WebSocketStats {
    nickname: String,
    win_rate: f64,
    avg_delay: f64,
    last_slot_update: SlotUpdate,
}

#[derive(Debug, Serialize)]
struct WebSocketMetricsResponse {
    start_slot: u64,
    latest_slot: u64,
    stats: Vec<WebSocketStats>,
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
    let max_eval_range = 200;

    let (win_counts, start_slot1, latest_slot1) =
        match compute_win_rates_with_range(Arc::clone(&db), max_eval_range) {
            Some(t) => t,
            None => return Json(WebSocketMetricsResponse {
                start_slot: 0,
                latest_slot: 0,
                stats: vec![],
            }),
        };

    let (avg_list, start_slot2, latest_slot2) =
        match compute_avg_delays_with_range(Arc::clone(&db), max_eval_range) {
            Some(t) => t,
            None => return Json(WebSocketMetricsResponse {
                start_slot: 0,
                latest_slot: 0,
                stats: vec![],
            }),
        };
    
    let latest_updates = get_latest_slot_updates_by_nickname(Arc::clone(&db));

    let start_slot = start_slot1.max(start_slot2);
    let latest_slot = latest_slot1.min(latest_slot2);
    let total_slots = (latest_slot - start_slot + 1) as f64;

    let mut all_nicknames: HashSet<String> = win_counts.keys().cloned().collect();
    all_nicknames.extend(avg_list.iter().map(|(nick, _)| nick.clone()));
    all_nicknames.extend(latest_updates.keys().cloned());

    let mut stats = Vec::new();
    for nickname in all_nicknames {
        let win_rate = win_counts
            .get(&nickname)
            .map(|count| (*count as f64 / total_slots) * 100.0)
            .unwrap_or(0.0);
        let avg_delay = avg_list
            .iter()
            .find(|(nick, _)| nick == &nickname)
            .and_then(|(_, delay_opt)| *delay_opt)
            .unwrap_or(-1.0);
        let last_slot_update = match latest_updates.get(&nickname) {
            Some(update) => update.clone(),
            None => SlotUpdate {
                timestamp: 0.0,
                nickname: nickname.clone(),
                slot_info: SlotNotificationResult {
                    slot: 0,
                    parent: 0,
                    root: 0,
                },
                delay: -1.0,
            },
        };

        stats.push(WebSocketStats {
            nickname,
            win_rate,
            avg_delay,
            last_slot_update
        });
    }

    let response = WebSocketMetricsResponse {
        start_slot,
        latest_slot,
        stats,
    };

    //println!("WebSocket metrics: {:?}", response);
    Json(response)
}
/*
    Return json information with the websocket endpoints from the config file
*/
async fn get_node_websockets(State(state): State<AppState>) -> Json<RpcConfig> {
    // Return a clone of the websocket endpoints from the config.
    Json(state.config.ws.clone())
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
    // Get a high-precision timestamp (in seconds with fractional precision).
    let timestamp = match Utc::now().timestamp_nanos_opt() {
        Some(n) => n as f64 / 1e9,
        None => {
            eprintln!("Failed to obtain high-precision timestamp.");
            return;
        }
    };

    // Get the nickname and slot info.
    let nickname = endpoint.nickname.clone();
    let slot_info = ws_message.params.result.clone();
    let current_slot = slot_info.slot;

    // Search the entire database for any existing SlotUpdate for the same slot (from any endpoint)
    // with a delay of 0 (within epsilon tolerance)
    let mut existing_timestamp: Option<f64> = None;
    let iter = websocket_db.iterator(IteratorMode::Start);
    for item in iter {
        if let Ok((key, value)) = item {
            if let Ok(key_str) = str::from_utf8(&key) {
                // Our key is formatted as "nickname:slot" -- split it.
                let parts: Vec<&str> = key_str.split(':').collect();
                if parts.len() == 2 {
                    // Try parsing the second part as the slot.
                    if let Ok(slot_num) = parts[1].parse::<u64>() {
                        if slot_num == current_slot {
                            // If the slot matches, deserialize the value.
                            if let Ok(existing_update) = serde_json::from_slice::<SlotUpdate>(&value) {
                                // Check if the stored delay is zero (within floating‐point epsilon).
                                if (existing_update.delay - 0.0).abs() < std::f64::EPSILON {
                                    existing_timestamp = Some(existing_update.timestamp);
                                    break; // Found an entry; no need to continue.
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Calculate the delay in milliseconds.
    // If an existing record for the same slot with delay == 0 is found, compute:
    // delay = (current_timestamp - stored_timestamp) * 1000.
    // Otherwise, if none exists, the delay is 0.
    let delay = if let Some(old_ts) = existing_timestamp {
        (timestamp - old_ts) * 1000.0
    } else {
        0.0
    };

    // Create a new SlotUpdate struct with the new values.
    let slot_update = SlotUpdate {
        timestamp,
        nickname: nickname.clone(),
        slot_info: slot_info.clone(),
        delay,
    };

    // Serialize the SlotUpdate struct to a JSON string.
    let serialized = serde_json::to_string(&slot_update)
        .expect("Failed to serialize SlotUpdate");

    // Construct the key in the format "nickname:slot" (using the new key structure).
    let key = format!("{}:{}", nickname, slot_info.slot);

    // Save the serialized SlotUpdate to the websocket_db.
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

fn get_latest_slot_updates_by_nickname(db: Arc<DB>) -> HashMap<String, SlotUpdate> {
    let mut latest_updates: HashMap<String, SlotUpdate> = HashMap::new();

    for item in db.iterator(IteratorMode::Start) {
        if let Ok((key, value)) = item {
            if let Ok(key_str) = str::from_utf8(&key) {
                if let Some((nickname, slot_str)) = key_str.split_once(':') {
                    if let Ok(slot) = slot_str.parse::<u64>() {
                        if let Ok(update) = serde_json::from_slice::<SlotUpdate>(&value) {
                            match latest_updates.get(nickname) {
                                Some(existing) if existing.slot_info.slot >= slot => {}
                                _ => {
                                    latest_updates.insert(nickname.to_string(), update);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    latest_updates
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
    // Map from endpoint nickname to the set of slot numbers received.
    let mut endpoint_slots: HashMap<String, HashSet<u64>> = HashMap::new();

    // Iterate over all entries in the RocksDB.
    let iter = db.iterator(IteratorMode::Start);
    for item in iter {
        if let Ok((key, _)) = item {
            // Convert the key bytes to a string.
            if let Ok(key_str) = str::from_utf8(&key) {
                // The key is expected to be "nickname:slot".
                let parts: Vec<&str> = key_str.split(':').collect();
                if parts.len() == 2 {
                    let nickname = parts[0].to_string();
                    if let Ok(slot) = parts[1].parse::<u64>() {
                        // Insert the slot number for this nickname.
                        endpoint_slots
                            .entry(nickname)
                            .or_insert_with(HashSet::new)
                            .insert(slot);
                    }
                }
            }
        }
    }

    // Return None if no endpoints were found.
    if endpoint_slots.is_empty() {
        return None;
    }

    // Compute the intersection of all slot sets.
    let mut common_slots: Option<HashSet<u64>> = None;
    for (_nickname, slots) in endpoint_slots {
        common_slots = match common_slots {
            None => Some(slots),
            Some(current_common) => {
                // Compute the intersection between the current common set and the new set.
                let new_common: HashSet<u64> = current_common.intersection(&slots).copied().collect();
                Some(new_common)
            }
        }
    }

    // If the intersection is non-empty, return the minimum slot; otherwise, return None.
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



/// Returns the nickname of the endpoint that has a SlotUpdate for the target slot
/// with a delay value of 0. This indicates that this update was recorded earliest.
///
/// It iterates over all entries in the RocksDB and, for each entry:
///  - It parses the value into a `SlotUpdate` struct.
///  - It checks if the slot in `slot_info.slot` matches the target slot.
///  - It then checks whether the `delay` field is exactly 0 (within an epsilon).
///
/// # Arguments
///
/// * `db` - A reference to the RocksDB instance storing websocket notifications.
/// * `target_slot` - The slot number for which to find the notification with a 0 delay.
///
/// # Returns
///
/// * `Option<String>` - The nickname of the endpoint that first reported the target slot
///   (i.e. with a delay of 0), or `None` if no such notification is found.
fn get_slot_winner(db: &rocksdb::DB, target_slot: u64) -> Option<(String, f64)> {
    let epsilon = std::f64::EPSILON;
    let iter = db.iterator(IteratorMode::Start);
    for item in iter {
        if let Ok((_, value)) = item {
            // Attempt to parse the value into a SlotUpdate struct.
            if let Ok(slot_update) = serde_json::from_slice::<SlotUpdate>(&value) {
                // Check if this notification is for the target slot.
                if slot_update.slot_info.slot == target_slot {
                    // Check if the delay is 0 (within epsilon tolerance).
                    if (slot_update.delay - 0.0).abs() < epsilon {
                        return Some((slot_update.nickname, slot_update.timestamp));
                    }
                }
            }
        }
    }
    None
}

fn print_slot_winner(db: &DB, target_slot: u64) {

    if let Some((nickname, _)) = get_slot_winner(db, target_slot) {
        println!(
            "For slot {} the earliest notification was from endpoint '{}'",
            target_slot, nickname
        );
    } else {
        println!("No winner found for slot {}", target_slot);
    }
}

fn compute_win_rates_with_range(
    db: Arc<DB>,
    max_eval_range: u64,
) -> Option<(HashMap<String, u64>, u64, u64)> {
    let latest_slot = get_latest_websocket_slot(Arc::clone(&db))?;
    let candidate1 = latest_slot.saturating_sub(max_eval_range);
    let candidate2 = get_earliest_common_websocket_slot(Arc::clone(&db)).unwrap_or(0);
    let start_slot = candidate1.max(candidate2);

    let mut total_evaluated = 0;
    let mut win_counts: HashMap<String, u64> = HashMap::new();

    for slot in start_slot..=latest_slot {
        if let Some((winner, _)) = get_slot_winner(&*db, slot) {
            total_evaluated += 1;
            *win_counts.entry(winner).or_insert(0) += 1;
        }
    }

    Some((win_counts, start_slot, latest_slot))
}

fn print_win_rates_with_range(db: Arc<DB>, max_eval_range: u64) {
    match compute_win_rates_with_range(db, max_eval_range) {
        Some((win_counts, start_slot, latest_slot)) => {
            println!("Win rates for slots {} to {}:", start_slot, latest_slot);
            for (nickname, count) in win_counts {
                let percentage = (count as f64 / (latest_slot - start_slot + 1) as f64) * 100.0;
                println!("Endpoint '{}' won {:.2}% of the time.", nickname, percentage);
            }
        }
        None => {
            println!("No slot notifications found.");
        }
    }
}

fn print_win_rates(db: Arc<DB>) {
    print_win_rates_with_range(db, 200);
}

/// Calculates the average delay (in milliseconds) for a given endpoint over the slot range [start_slot, end_slot].
///
/// For each slot in the range:
/// - If there is a SlotUpdate entry in the DB for the given endpoint, use its stored delay.
/// - Otherwise, if a winner is found via `get_slot_winner2`, compute the delay as:
///      (current_time (from timestamp_nanos_opt) - winner_timestamp) * 1000.
///   (If no winner is found, that slot is skipped.)
///
/// Returns the average delay as an Option<f64>.
///
/// # Arguments
///
/// * `db` - A reference to the RocksDB database.
/// * `endpoint` - The endpoint nickname (which is now the prefix of the key).
/// * `start_slot` - The beginning of the evaluation range (inclusive).
/// * `end_slot` - The end of the evaluation range (inclusive).
fn avg_delay_for_endpoint(db: &DB, nickname: &str, start_slot: u64, end_slot: u64) -> Option<f64> {

    // We'll count how many entries match the prefix.
    let mut total_entries = 0;

    // Create a HashMap to store the delay for each slot for this endpoint.
    let mut endpoint_delays: HashMap<u64, f64> = HashMap::new();

    // Iterate over all DB entries.
    for item in db.iterator(IteratorMode::Start) {
        if let Ok((key, value)) = item {
            // Convert the key from bytes to a string.
            if let Ok(key_str) = str::from_utf8(&key) {
                // Manually check whether the key starts with the prefix.
                if key_str.starts_with(&nickname) {
                    total_entries += 1;
                    // The expected key format is "nickname:slot"
                    let parts: Vec<&str> = key_str.split(':').collect();
                    if parts.len() == 2 {
                        // Try to parse the slot number.
                        if let Ok(slot) = parts[1].parse::<u64>() {
                            // Only process slots within our evaluation range.
                            if slot >= start_slot && slot <= end_slot {
                                // Deserialize the stored value into a SlotUpdate struct.
                                if let Ok(slot_update) = serde_json::from_slice::<SlotUpdate>(&value) {
                                    //println!("Found entry for slot {}: delay={}", slot, slot_update.delay);
                                    // Record the delay for this slot.
                                    endpoint_delays.insert(slot, slot_update.delay);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    //println!("Total entries matching '{}' : {}", &nickname, total_entries);

    let mut total_delay = 0.0;
    let mut count = 0;
    // For each slot in the evaluation range, determine the delay.
    for slot in start_slot..=end_slot {
        if let Some(&stored_delay) = endpoint_delays.get(&slot) {
            //println!("stored delay: {}", &stored_delay);
            total_delay += stored_delay;
            count += 1;
        } else {
            // Fallback: if no delay is stored for this endpoint for the slot,
            // try to get the winner’s timestamp for that slot.
            if let Some((_, winner_ts)) = get_slot_winner(db, slot) {
                // Get a high-precision current time (in seconds with fractional part).
                let now = match Utc::now().timestamp_nanos_opt() {
                    Some(n) => n as f64 / 1e9,
                    None => {
                        eprintln!("Failed to obtain high-precision timestamp.");
                        continue;
                    }
                };
                // Compute the delay in milliseconds.
                let computed_delay = (now - winner_ts) * 1000.0;
                //println!("computed delay: {}", &computed_delay);
                total_delay += computed_delay;
                count += 1;
            }
        }
    }

    if count > 0 {
        Some(total_delay / (count as f64))
    } else {
        None
    }
}

fn compute_avg_delays_with_range(
    db: Arc<DB>,
    max_eval_range: u64,
) -> Option<(Vec<(String, Option<f64>)>, u64, u64)> {
    let latest_slot = get_latest_websocket_slot(Arc::clone(&db))?;
    let candidate1 = latest_slot.saturating_sub(max_eval_range);
    let candidate2 = get_earliest_common_websocket_slot(Arc::clone(&db)).unwrap_or(0);
    let start_slot = candidate1.max(candidate2);

    let mut endpoints: HashSet<String> = HashSet::new();
    let iter = db.iterator(IteratorMode::Start);
    for item in iter {
        if let Ok((_, value)) = item {
            if let Ok(slot_update) = serde_json::from_slice::<SlotUpdate>(&value) {
                endpoints.insert(slot_update.nickname);
            }
        }
    }

    let mut results = Vec::new();
    for endpoint in endpoints {
        let avg = avg_delay_for_endpoint(&db, &endpoint, start_slot, latest_slot);
        results.push((endpoint, avg));
    }

    Some((results, start_slot, latest_slot))
}

fn print_avg_delays_with_range(db: Arc<DB>, max_eval_range: u64) {
    match compute_avg_delays_with_range(db, max_eval_range) {
        Some((results, _start_slot, _latest_slot)) => {
            for (endpoint, avg_delay_opt) in results {
                match avg_delay_opt {
                    Some(avg_delay) => {
                        println!(
                            "Endpoint '{}' has an average delay of {:.2} ms",
                            endpoint, avg_delay
                        );
                    }
                    None => {
                        println!("Endpoint '{}' has no delay data", endpoint);
                    }
                }
            }
        }
        None => println!("No slot notifications found."),
    }
}

/// Prints every key–value pair in the given RocksDB database.
///
/// This function iterates over the entire database starting from the beginning.
/// It attempts to convert each key and value into a UTF-8 string and then prints it.
/// If the conversion fails, it prints the raw bytes (in debug format).
fn print_database(db: &DB) {
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
}

/// Wrapper function that uses a default max evaluation range of 200 slots.
fn print_avg_delays(db: Arc<DB>) {
    print_avg_delays_with_range(db, 200);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {

    /*
    // Parse the WebSocket URL.
    let my_url = "ws://elite.swqos.solanavibestation.com/?api_key=f17b7753247a92d255f0fd2d1c44b4fb";
    //let ws_url = "wss://echo.websocket.events"; // Example echo server URL.
    let url = Url::parse(my_url).expect("Invalid URL");

    // Connect to the WebSocket server.
    let (mut ws_stream, response) = connect_async(url)
        .await
        .expect("Failed to connect to WebSocket");

    println!("Connected to the websocket server");
    println!("HTTP status: {}", response.status());

    // Create the JSON subscription request.
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "slotSubscribe"
    });

    // Send the JSON request as a text message.
    ws_stream
       .send(Message::Text(request.to_string()))
       .await
       .expect("Failed to send subscribe message");
    
    // Spawn the WebSocket message handler on a separate task.
    tokio::spawn(handle_websocket_messages(ws_stream));
    */



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

    // Spawn a task that calls print_latest_websocket_slot every 2 seconds.
    let ws_db_clone = Arc::clone(&websocket_db);
    tokio::spawn(async move {
        loop {
            print_latest_websocket_slot(Arc::clone(&ws_db_clone));
            if let Some(latest_slot) = get_latest_websocket_slot(Arc::clone(&ws_db_clone)) {
                print_slot_winner(&*ws_db_clone, latest_slot);
                print_win_rates(Arc::clone(&ws_db_clone));
            } else {
                println!("No latest websocket slot available.");
            }
            print_avg_delays(Arc::clone(&ws_db_clone));
            tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
        }
    });












    std::fs::create_dir_all("static")?;
    std::fs::write(
        "static/index.html",
        include_str!("static/index.html")
    )?;

    /*
        Include the web socket monitoring file in the compiled project
    */
    let wsm_filename = "web_socket_monitoring.js";
    // Define the source and destination paths.
    let source_str = format!("src/static/js/{}", wsm_filename);
    let source = Path::new(&source_str); 
    let destination_dir = Path::new("static/js");
    // Create the destination directory if it doesn't exist.
    fs::create_dir_all(&destination_dir).expect("Failed to create static/js directory");
    // Copy the file.
    let destination = destination_dir.join(wsm_filename);
    fs::copy(&source, &destination).expect(&format!("Failed to copy {}", wsm_filename));

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
        .route("/api/node_websockets", get(get_node_websockets))
        .nest_service("/static", get_service(ServeDir::new("static")))
        .with_state(app_state);
    
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Server running on http://localhost:3000");
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}
