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

    // Get host info
    info!("🚀 [Streamers API] Looking up host_id={}", req.host_id);
    let (host_address, host_http_port, client_private_key_pem, client_certificate_pem, server_certificate_pem) = {
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
        if let Some(client_private_key) = host.client_private_key()
            && let Some(client_certificate) = host.client_certificate()
            && let Some(server_certificate) = host.server_certificate()
        {
            info!("🚀 [Streamers API] Host is paired, got certificates");
            (
                host.address().to_string(),
                host.http_port(),
                client_private_key.to_string(),
                client_certificate.to_string(),
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

    // Spawn streamer process
    let (mut child, stdin, stdout) = match Command::new(&config.streamer_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(mut child) => {
            info!("🚀 [Streamers API] Streamer process spawned successfully, PID will be assigned");
            if let Some(stdin) = child.stdin.take()
                && let Some(stdout) = child.stdout.take()
            {
                info!("🚀 [Streamers API] Got stdin/stdout from streamer process");
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

    // Send Init message
    info!("🚀 [Streamers API] Sending Init IPC message to streamer...");
    ipc_sender.send(ServerIpcMessage::Init {
        server_config: Config::clone(&config),
        stream_settings,
        host_address,
        host_http_port,
        host_unique_id: Some(req.client_unique_id),
        client_private_key_pem,
        client_certificate_pem,
        server_certificate_pem,
        app_id: req.app_id,
    }).await;
    info!("🚀 [Streamers API] Init IPC sent");

    // Send StartMoonlight message
    info!("🚀 [Streamers API] Sending StartMoonlight IPC message (headless mode)...");
    ipc_sender.send(ServerIpcMessage::StartMoonlight).await;
    info!("🚀 [Streamers API] StartMoonlight IPC sent - streamer should start Moonlight now!");

    // Create streamer state
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

    // Spawn IPC receiver task
    let streamer_state_clone = streamer_state.clone();
    let registry_clone = registry.clone();
    let streamer_id = req.streamer_id.clone();

    info!("🚀 [Streamers API] Spawning IPC receiver task for streamer {}", streamer_id);
    spawn(async move {
        info!("🔄 [Streamer {}] IPC receiver task started, waiting for messages...", streamer_id);
        while let Some(message) = ipc_receiver.recv().await {
            info!("🔄 [Streamer {}] Received IPC message: {:?}", streamer_id, message);
            match message {
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
                StreamerIpcMessage::Stop => {
                    info!("🛑 [Streamer {}] Received Stop IPC, exiting receiver task", streamer_id);
                    break;
                }
                _ => {
                    info!("🔄 [Streamer {}] Received other IPC message (WebSocket/etc)", streamer_id);
                }
            }
        }

        info!("🛑 [Streamer {}] IPC receiver task ended, cleaning up", streamer_id);
        // Clean up when streamer stops
        registry_clone.streamers.write().await.remove(&streamer_id);
        info!("🛑 [Streamer {}] Removed from registry", streamer_id);
    });

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
