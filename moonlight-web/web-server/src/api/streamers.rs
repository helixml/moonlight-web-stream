// Phase 5: Full Streamer API implementation with process management and IPC routing

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::SystemTime;

use actix_web::{
    HttpRequest, HttpResponse, delete, get, post, rt as actix_rt,
    web::{Data, Json, Path, Payload},
};
use actix_ws::{Closed, Message, Session};
use common::{
    StreamSettings,
    api_bindings::{
        CreateStreamerRequest, ListStreamersResponse, PeerInfo, StreamerInfo,
        StreamClientMessage, StreamServerMessage,
    },
    config::Config,
    ipc::{IpcSender, ServerIpcMessage, StreamerIpcMessage, create_child_ipc},
    serialize_json,
};
use log::{debug, info, warn};
use moonlight_common::stream::bindings::SupportedVideoFormats;
use tokio::{process::Command, spawn, sync::{Mutex, RwLock}};

use crate::data::RuntimeApiData;

// Streamer state management
pub struct StreamerRegistry {
    pub streamers: RwLock<HashMap<String, Arc<StreamerState>>>,
}

pub struct StreamerState {
    pub streamer_id: String,
    pub created_at: SystemTime,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub ipc_sender: Mutex<IpcSender<ServerIpcMessage>>,
    pub peer_websockets: RwLock<HashMap<String, Mutex<Session>>>,
    pub moonlight_connected: RwLock<bool>,
}

impl StreamerRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            streamers: RwLock::new(HashMap::new()),
        })
    }
}

/// Create a new streamer with persistent Moonlight stream
/// POST /api/streamers
#[post("/streamers")]
pub async fn create_streamer(
    data: Data<RuntimeApiData>,
    registry: Data<Arc<StreamerRegistry>>,
    config: Data<Config>,
    request: Json<CreateStreamerRequest>,
) -> HttpResponse {
    let req = request.into_inner();

    info!("🚀 [Streamers API] POST /api/streamers called!");
    info!("🚀 [Streamers API] Request: streamer_id={}, client_unique_id={}, app_id={}, {}x{}@{}fps",
        req.streamer_id, req.client_unique_id, req.app_id, req.width, req.height, req.fps);

    // Check if streamer already exists
    {
        let streamers = registry.streamers.read().await;
        if streamers.contains_key(&req.streamer_id) {
            warn!("🚀 [Streamers API] Streamer {} already exists", req.streamer_id);
            return HttpResponse::Conflict().json(serde_json::json!({
                "error": "Streamer already exists"
            }));
        }
        info!("🚀 [Streamers API] Streamer {} not in registry, proceeding to create", req.streamer_id);
    }

    // Get host info and generate UNIQUE client credentials per streamer
    info!("🚀 [Streamers API] Looking up host_id={}", req.host_id);
    let (host_address, host_http_port, server_certificate_pem) = {
        let hosts = data.hosts.read().await;
        info!("🚀 [Streamers API] Total hosts available: {}", hosts.len());
        let Some(host) = hosts.get(req.host_id as usize) else {
            warn!("🚀 [Streamers API] Host {} not found", req.host_id);
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "Host not found"
            }));
        };
        info!("🚀 [Streamers API] Found host {}, locking...", req.host_id);
        let mut host = host.lock().await;
        let host = &mut host.moonlight;

        info!("🚀 [Streamers API] Host address: {}, checking pairing...", host.address());
        if let Some(server_certificate) = host.server_certificate() {
            info!("🚀 [Streamers API] Host is paired, got server certificate");
            (
                host.address().to_string(),
                host.http_port(),
                server_certificate.to_string(),
            )
        } else {
            warn!("🚀 [Streamers API] Host {} not paired!", req.host_id);
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Host not paired"
            }));
        }
    };
    info!("🚀 [Streamers API] Host info retrieved successfully");

    // CRITICAL: Generate UNIQUE pairing credentials for this streamer
    // Wolf identifies clients by certificate hash - shared certs = same client_id
    // Each streamer needs unique cert to get unique client_id and avoid session conflicts
    info!("🚀 [Streamers API] Generating unique pairing credentials for streamer {}", req.streamer_id);
    use moonlight_common::pair::generate_new_client;
    use moonlight_common::PairPin;

    let client_auth = match generate_new_client() {
        Ok(auth) => auth,
        Err(err) => {
            warn!("🚀 [Streamers API] Failed to generate client credentials: {:?}", err);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Failed to generate client credentials: {:?}", err)
            }));
        }
    };

    let client_private_key_pem = client_auth.private_key.to_string();
    let client_certificate_pem = client_auth.certificate.to_string();

    info!("🚀 [Streamers API] Generated unique client credentials, now auto-pairing...");

    // Auto-pair using internal PIN
    let pin = std::env::var("MOONLIGHT_INTERNAL_PAIRING_PIN")
        .ok()
        .and_then(|pin_str| {
            if pin_str.len() == 4 && pin_str.chars().all(|c| c.is_ascii_digit()) {
                let digits: Vec<u8> = pin_str.chars().map(|c| c.to_digit(10).unwrap() as u8).collect();
                PairPin::from_array([digits[0], digits[1], digits[2], digits[3]])
            } else {
                None
            }
        });

    let Some(pin) = pin else {
        warn!("🚀 [Streamers API] MOONLIGHT_INTERNAL_PAIRING_PIN not configured or invalid");
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "Auto-pairing PIN not configured"
        }));
    };

    // Create temporary host to pair
    use moonlight_common::network::reqwest::ReqwestMoonlightHost;
    let mut temp_host = match ReqwestMoonlightHost::new(host_address.clone(), host_http_port, None) {
        Ok(h) => h,
        Err(err) => {
            warn!("🚀 [Streamers API] Failed to create temp host: {:?}", err);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Failed to create host: {:?}", err)
            }));
        }
    };

    // Pair with unique credentials
    if let Err(err) = temp_host.pair(&client_auth, format!("streamer-{}", req.streamer_id), pin).await {
        warn!("🚀 [Streamers API] Auto-pairing failed: {:?}", err);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("Auto-pairing failed: {:?}", err)
        }));
    }

    info!("🚀 [Streamers API] Auto-pairing successful with unique credentials!");

    // CRITICAL: Clear host cache to force fresh app list query
    // When Wolf creates a new app, moonlight-web doesn't know about it until cache is cleared
    // This is what the "reload" button in the UI does
    info!("🚀 [Streamers API] Clearing host cache to refresh app list");
    {
        let hosts = data.hosts.read().await;
        if let Some(host) = hosts.get(req.host_id as usize) {
            let mut host = host.lock().await;
            host.moonlight.clear_cache();
            info!("🚀 [Streamers API] Host cache cleared successfully");
        }
    }

    let stream_settings = StreamSettings {
        bitrate: req.bitrate,
        packet_size: req.packet_size,
        fps: req.fps,
        width: req.width,
        height: req.height,
        video_sample_queue_size: req.video_sample_queue_size,
        audio_sample_queue_size: req.audio_sample_queue_size,
        play_audio_local: req.play_audio_local,
        video_supported_formats: SupportedVideoFormats::from_bits(req.video_supported_formats)
            .unwrap_or(SupportedVideoFormats::H264),
        video_colorspace: req.video_colorspace.into(),
        video_color_range_full: req.video_color_range_full,
    };

    info!("🚀 [Streamers API] Spawning streamer process: {}", config.streamer_path);
    info!("🚀 [Streamers API] Current working directory: {:?}", std::env::current_dir());

    // Verify streamer binary exists
    let streamer_path = std::path::Path::new(&config.streamer_path);
    if !streamer_path.exists() {
        warn!("🚀 [Streamers API] ERROR: Streamer binary not found at: {}", config.streamer_path);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": format!("Streamer binary not found at: {}", config.streamer_path)
        }));
    }
    info!("🚀 [Streamers API] Streamer binary exists at: {}", config.streamer_path);

    // Spawn streamer process
    let (mut child, stdin, stdout) = match Command::new(&config.streamer_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(mut child) => {
            let pid = child.id();
            info!("🚀 [Streamers API] Streamer process spawned successfully, PID: {}", pid.unwrap_or(0));

            // Check if process is still alive immediately after spawn
            match child.try_wait() {
                Ok(Some(status)) => {
                    warn!("🚀 [Streamers API] ERROR: Streamer process ALREADY EXITED with status: {:?}", status);
                    return HttpResponse::InternalServerError().json(serde_json::json!({
                        "error": format!("Streamer exited immediately with status: {:?}", status)
                    }));
                }
                Ok(None) => {
                    info!("🚀 [Streamers API] Streamer process is running (PID: {})", pid.unwrap_or(0));
                }
                Err(e) => {
                    warn!("🚀 [Streamers API] Failed to check streamer status: {:?}", e);
                }
            }

            if let Some(stdin) = child.stdin.take()
                && let Some(stdout) = child.stdout.take()
            {
                info!("🚀 [Streamers API] Got stdin/stdout from streamer process (PID: {})", pid.unwrap_or(0));
                (child, stdin, stdout)
            } else {
                warn!("🚀 [Streamers API] Failed to get stdin/stdout from streamer");
                return HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Failed to get streamer stdin/stdout"
                }));
            }
        }
        Err(err) => {
            warn!("🚀 [Streamers API] Failed to spawn streamer: {err:?}");
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Failed to spawn streamer: {}", err)
            }));
        }
    };
    info!("🚀 [Streamers API] Setting up IPC channels...");

    // Create IPC
    let (mut ipc_sender, mut ipc_receiver) = create_child_ipc::<ServerIpcMessage, StreamerIpcMessage>(
        format!("Streamer-{}", req.streamer_id),
        stdin,
        stdout,
        child.stderr.take(),
    ).await;
    info!("🚀 [Streamers API] IPC channels created");

    // Check if streamer is still alive after IPC setup
    match child.try_wait() {
        Ok(Some(status)) => {
            warn!("🚀 [Streamers API] ERROR: Streamer exited DURING IPC setup with status: {:?}", status);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": format!("Streamer exited during IPC setup: {:?}", status)
            }));
        }
        Ok(None) => {
            info!("🚀 [Streamers API] Streamer still running after IPC setup");
        }
        Err(e) => {
            warn!("🚀 [Streamers API] Failed to check streamer status after IPC: {:?}", e);
        }
    }

    // Create streamer state (with IPC sender for receiver task to use)
    let streamer_state = Arc::new(StreamerState {
        streamer_id: req.streamer_id.clone(),
        created_at: SystemTime::now(),
        width: req.width,
        height: req.height,
        fps: req.fps,
        ipc_sender: Mutex::new(ipc_sender),
        peer_websockets: RwLock::new(HashMap::new()),
        moonlight_connected: RwLock::new(false),
    });

    // Store in registry
    info!("🚀 [Streamers API] Storing streamer {} in registry", req.streamer_id);
    registry.streamers.write().await.insert(req.streamer_id.clone(), streamer_state.clone());
    info!("🚀 [Streamers API] Streamer stored, registry now has {} streamers", registry.streamers.read().await.len());

    // CRITICAL: Spawn IPC receiver task BEFORE sending any messages
    // The streamer sends messages immediately on startup, so we need to be listening
    let streamer_state_clone = streamer_state.clone();
    let registry_clone = registry.clone();
    let streamer_id = req.streamer_id.clone();

    info!("🔧 [FIXED CODE v4] Spawning IPC receiver task BEFORE sending Init/StartMoonlight for streamer {}", streamer_id);
    spawn(async move {
        info!("🔄 [Streamer {}] IPC receiver task started, waiting for messages...", streamer_id);

        // CRITICAL: Keep child alive by moving it into this task!
        // When handler returns, child would be dropped and process killed (kill_on_drop=true)
        let _child = child;

        while let Some(message) = ipc_receiver.recv().await {
            info!("🔄 [Streamer {}] Received IPC message: {:?}", streamer_id, message);
            match message {
                // Legacy WebSocket messages (backward compatibility)
                StreamerIpcMessage::WebSocket(message) => {
                    info!("📨 [Streamer {}] WebSocket message: {:?}", streamer_id, message);
                    // In headless mode, broadcast to all connected peers
                    // If no peers connected yet, these are just logged (no UI to show progress to)
                    let peers = streamer_state_clone.peer_websockets.read().await;
                    if !peers.is_empty() {
                        if let Some(json) = serialize_json(&message) {
                            for session in peers.values() {
                                let mut session = session.lock().await;
                                let _ = session.text(json.clone()).await;
                            }
                        }
                    }
                }
                StreamerIpcMessage::MoonlightConnected => {
                    info!("✅ [Streamer {}] MOONLIGHT CONNECTED! Stream is live headless!", streamer_id);
                    *streamer_state_clone.moonlight_connected.write().await = true;
                }
                StreamerIpcMessage::ToPeer { peer_id, message } => {
                    // Route to specific peer WebSocket
                    let peers = streamer_state_clone.peer_websockets.read().await;
                    if let Some(session) = peers.get(&peer_id) {
                        let mut session = session.lock().await;
                        if let Some(json) = serialize_json(&message) {
                            let _ = session.text(json).await;
                        }
                    }
                }
                StreamerIpcMessage::Broadcast(message) => {
                    // Broadcast to all peers
                    let peers = streamer_state_clone.peer_websockets.read().await;
                    if let Some(json) = serialize_json(&message) {
                        for session in peers.values() {
                            let mut session = session.lock().await;
                            let _ = session.text(json.clone()).await;
                        }
                    }
                }
                StreamerIpcMessage::StreamerReady { streamer_id: ready_id } => {
                    info!("🎬 [Streamer {}] Streamer ready: {}", streamer_id, ready_id);
                }
                StreamerIpcMessage::Stop => {
                    info!("🛑 [Streamer {}] Received Stop IPC, exiting receiver task", streamer_id);
                    break;
                }
            }
        }

        info!("🛑 [Streamer {}] IPC receiver task ended, cleaning up", streamer_id);
        // Clean up when streamer stops
        registry_clone.streamers.write().await.remove(&streamer_id);
        info!("🛑 [Streamer {}] Removed from registry", streamer_id);
    });

    // NOW send Init and StartMoonlight messages (after receiver is listening)
    info!("🚀 [Streamers API] Sending Init IPC message to streamer...");
    info!("🚀 [Streamers API] client_unique_id={}, app_id={}", req.client_unique_id, req.app_id);
    {
        let mut sender = streamer_state.ipc_sender.lock().await;
        sender.send(ServerIpcMessage::Init {
            server_config: Config::clone(&config),
            stream_settings,
            host_address,
            host_http_port,
            host_unique_id: Some(req.client_unique_id.clone()),
            client_private_key_pem,
            client_certificate_pem,
            server_certificate_pem,
            app_id: req.app_id,
        }).await;
    }
    info!("🚀 [Streamers API] Init IPC sent");

    // Send StartMoonlight message
    info!("🚀 [Streamers API] Sending StartMoonlight IPC message (headless mode)...");
    {
        let mut sender = streamer_state.ipc_sender.lock().await;
        sender.send(ServerIpcMessage::StartMoonlight).await;
    }
    info!("🚀 [Streamers API] StartMoonlight IPC sent - streamer should start Moonlight now!");

    info!("🚀 [Streamers API] Returning 200 OK to client");
    HttpResponse::Ok().json(StreamerInfo {
        streamer_id: req.streamer_id,
        status: "active".to_string(),
        moonlight_connected: false,
        connected_peers: 0,
        width: req.width,
        height: req.height,
        fps: req.fps,
        created_at: chrono::Utc::now().to_rfc3339(),
        peers: vec![],
    })
}

/// List all active streamers
#[get("/streamers")]
pub async fn list_streamers(
    registry: Data<Arc<StreamerRegistry>>,
) -> Json<ListStreamersResponse> {
    let streamers = registry.streamers.read().await;
    let mut list = vec![];

    for (_, state) in streamers.iter() {
        let peers = state.peer_websockets.read().await;
        let peer_list: Vec<PeerInfo> = peers.keys().map(|peer_id| PeerInfo {
            peer_id: peer_id.clone(),
            connected_at: chrono::Utc::now().to_rfc3339(), // TODO: track actual connection time
        }).collect();

        list.push(StreamerInfo {
            streamer_id: state.streamer_id.clone(),
            status: "active".to_string(),
            moonlight_connected: *state.moonlight_connected.read().await,
            connected_peers: peers.len() as u32,
            width: state.width,
            height: state.height,
            fps: state.fps,
            created_at: chrono::Utc::now().to_rfc3339(), // TODO: use actual created_at
            peers: peer_list,
        });
    }

    Json(ListStreamersResponse { streamers: list })
}

/// Get details about a specific streamer
#[get("/streamers/{streamer_id}")]
pub async fn get_streamer(
    registry: Data<Arc<StreamerRegistry>>,
    path: Path<String>,
) -> HttpResponse {
    let streamer_id = path.into_inner();
    let streamers = registry.streamers.read().await;

    if let Some(state) = streamers.get(&streamer_id) {
        let peers = state.peer_websockets.read().await;
        let peer_list: Vec<PeerInfo> = peers.keys().map(|peer_id| PeerInfo {
            peer_id: peer_id.clone(),
            connected_at: chrono::Utc::now().to_rfc3339(),
        }).collect();

        HttpResponse::Ok().json(StreamerInfo {
            streamer_id: state.streamer_id.clone(),
            status: "active".to_string(),
            moonlight_connected: *state.moonlight_connected.read().await,
            connected_peers: peers.len() as u32,
            width: state.width,
            height: state.height,
            fps: state.fps,
            created_at: chrono::Utc::now().to_rfc3339(),
            peers: peer_list,
        })
    } else {
        HttpResponse::NotFound().json(serde_json::json!({
            "error": "Streamer not found"
        }))
    }
}

/// Stop a streamer and terminate its Moonlight stream
#[delete("/streamers/{streamer_id}")]
pub async fn delete_streamer(
    registry: Data<Arc<StreamerRegistry>>,
    path: Path<String>,
) -> HttpResponse {
    let streamer_id = path.into_inner();

    let streamer = {
        let streamers = registry.streamers.read().await;
        streamers.get(&streamer_id).cloned()
    };

    if let Some(streamer) = streamer {
        // Send Stop message
        let mut sender = streamer.ipc_sender.lock().await;
        sender.send(ServerIpcMessage::Stop).await;

        // Remove from registry (IPC task will also remove it)
        registry.streamers.write().await.remove(&streamer_id);

        HttpResponse::NoContent().finish()
    } else {
        HttpResponse::NotFound().json(serde_json::json!({
            "error": "Streamer not found"
        }))
    }
}

/// WebSocket endpoint for WebRTC peer to join existing streamer
#[get("/streamers/{streamer_id}/peer")]
pub async fn connect_peer(
    registry: Data<Arc<StreamerRegistry>>,
    path: Path<String>,
    request: HttpRequest,
    payload: Payload,
) -> Result<HttpResponse, actix_web::Error> {
    let streamer_id = path.into_inner();

    // Get streamer from registry
    let streamer = {
        let streamers = registry.streamers.read().await;
        streamers.get(&streamer_id).cloned()
    };

    let Some(streamer) = streamer else {
        return Ok(HttpResponse::NotFound().json(serde_json::json!({
            "error": "Streamer not found"
        })));
    };

    // Establish WebSocket
    let (response, mut session, mut stream) = actix_ws::handle(&request, payload)?;

    actix_rt::spawn(async move {
        // Generate unique peer ID
        let peer_id = uuid::Uuid::new_v4().to_string();
        info!("[Streamer {}]: Peer {} connecting", streamer_id, peer_id);

        // Store peer WebSocket
        streamer.peer_websockets.write().await.insert(peer_id.clone(), Mutex::new(session.clone()));

        // Send AddPeer to streamer
        {
            let mut sender = streamer.ipc_sender.lock().await;
            sender.send(ServerIpcMessage::AddPeer { peer_id: peer_id.clone() }).await;
        }

        // Handle WebSocket messages
        while let Some(Ok(Message::Text(text))) = stream.recv().await {
            let Ok(message) = serde_json::from_str::<StreamClientMessage>(&text) else {
                warn!("[Peer {}]: Failed to deserialize message", peer_id);
                continue;
            };

            // Send FromPeer to streamer
            let mut sender = streamer.ipc_sender.lock().await;
            sender.send(ServerIpcMessage::FromPeer {
                peer_id: peer_id.clone(),
                message,
            }).await;
        }

        // Peer disconnected
        info!("[Streamer {}]: Peer {} disconnected", streamer_id, peer_id);

        // Remove peer from registry
        streamer.peer_websockets.write().await.remove(&peer_id);

        // Send RemovePeer to streamer
        let mut sender = streamer.ipc_sender.lock().await;
        sender.send(ServerIpcMessage::RemovePeer { peer_id }).await;
    });

    Ok(response)
}
