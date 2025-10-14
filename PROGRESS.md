# Multi-WebRTC Implementation Progress

## Completed (Phases 1-3)

### ✅ Phase 1: Core Infrastructure
- Created `WebRtcPeer` struct in `peer.rs`
- Added `peers: HashMap` to `StreamConnection` for multi-peer support
- Stored `rtc_config` for creating additional peers
- Backward compatible - existing single-peer flow works unchanged

### ✅ Phase 2: Broadcasters
- Implemented `VideoBroadcaster` with non-blocking distribution
- Implemented `AudioBroadcaster` with same pattern
- Uses `UnboundedSender` (non-blocking, unlimited buffer)
- Subscribe/unsubscribe methods for dynamic peer management

### ✅ Phase 3: Input Aggregator
- Tracks `HashMap<key, HashSet<peer_id>>` for union semantics
- Key down: Sends immediately when first peer presses
- Key up: Sends only when last peer releases
- Mouse: Last writer wins, immediate forwarding
- `remove_peer()`: Automatically releases all inputs on disconnect

## Remaining Work (Phases 4-6)

### Phase 4: Streamer API Endpoints (Estimated: 3-4 hours)

**What's needed:**

1. **New IPC Messages** (`common/src/ipc.rs`):
```rust
pub enum ServerIpcMessage {
    Init { ... },  // Exists
    StartMoonlight,  // NEW: Start Moonlight without WebRTC
    AddPeer { peer_id: String },  // NEW
    FromPeer { peer_id: String, message: StreamClientMessage },  // NEW
    RemovePeer { peer_id: String },  // NEW
    Stop,
}

pub enum StreamerIpcMessage {
    WebSocket(StreamServerMessage),  // Exists
    ToPeer { peer_id: String, message: StreamServerMessage },  // NEW
    Broadcast(StreamServerMessage),  // NEW
    MoonlightConnected,  // NEW
    Stop,
}
```

2. **API Types** (`common/src/api_bindings.rs`):
```rust
pub struct CreateStreamerRequest {
    pub streamer_id: String,
    pub client_unique_id: String,
    pub host_id: u32,
    pub app_id: u32,
    pub settings: StreamSettings,
}

pub struct StreamerInfo {
    pub streamer_id: String,
    pub status: String,
    pub moonlight_connected: bool,
    pub connected_peers: usize,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}
```

3. **Web-server State** (`web-server/src/data.rs`):
```rust
pub struct StreamerState {
    pub streamer_id: String,
    pub process: Child,
    pub ipc_sender: IpcSender<ServerIpcMessage>,
    pub peer_websockets: Arc<RwLock<HashMap<String, Session>>>,
    pub created_at: Instant,
}

pub struct RuntimeApiData {
    pub streamers: RwLock<HashMap<String, Arc<StreamerState>>>,  // NEW
    // ... existing fields
}
```

4. **API Endpoints** (`web-server/src/api/streamers.rs` - NEW FILE):
- `POST /api/streamers` - Create streamer, start Moonlight
- `GET /api/streamers` - List all streamers
- `GET /api/streamers/{id}` - Get streamer details
- `DELETE /api/streamers/{id}` - Stop streamer

### Phase 5: WebRTC Peer WebSocket (Estimated: 4-5 hours)

**What's needed:**

1. **WebSocket Handler** (`web-server/src/api/streamers.rs`):
- `WS /api/streamers/{streamer_id}/peer` endpoint
- Generate unique peer_id on connection
- Maintain `websocket_to_peer_id: HashMap` for routing
- Forward messages via IPC with peer_id

2. **Streamer-side Peer Management** (`streamer/src/main.rs`):
- Handle `AddPeer` IPC: Create RTCPeerConnection, subscribe to broadcasters
- Handle `FromPeer` IPC: Route signaling to correct peer
- Handle `RemovePeer` IPC: Clean up peer, unsubscribe
- Send messages via `ToPeer` IPC

3. **IPC Router Loop** (in web-server):
```rust
// Listen to streamer IPC, route to correct WebSocket
spawn(async move {
    while let Some(msg) = streamer_ipc_rx.recv().await {
        match msg {
            ToPeer { peer_id, message } => {
                // Route to specific peer's WebSocket
                if let Some(ws) = peer_websockets.get(&peer_id) {
                    ws.send(serialize_json(&message))
                }
            }
            Broadcast(message) => {
                // Send to all peers
                for ws in peer_websockets.values() {
                    ws.send(serialize_json(&message))
                }
            }
            ...
        }
    }
})
```

### Phase 6: Helix Integration (Estimated: 2-3 hours)

**What's needed:**

1. **Helix API** (`helix/api/pkg/external-agent/wolf_executor_apps.go`):
- Replace WebSocket connection with HTTP `POST /api/streamers`
- Include `streamer_id`, `client_unique_id`, `app_id`, settings
- Wait for response confirming Moonlight connected
- Remove all keepalive WebSocket logic

2. **Helix Frontend** (`helix/frontend/src/components/external-agent/MoonlightStreamViewer.tsx`):
- Change WebSocket URL to `/api/streamers/{streamer_id}/peer`
- Keep existing Stream class usage (protocol unchanged!)
- No mode="join" needed (always creates fresh peer)

## Why Phases 4-6 Weren't Completed Tonight

**Time Complexity**:
- Each phase requires careful IPC message routing implementation
- WebSocket → IPC → Streamer → IPC → WebSocket bidirectional flow
- Error handling for each step
- Testing at each integration point
- Estimated 9-12 hours of focused work

**Current State**:
- ✅ All foundation code complete (Phases 1-3)
- ✅ Compiles successfully
- ✅ Architecture fully designed and documented
- ⏳ API implementation and integration requires extended focused time

## Next Steps to Complete

The implementation is **50% complete** with all the hard architectural foundation done. To finish:

1. Implement IPC message types (30 min)
2. Create streamers API endpoints (2 hours)
3. Implement IPC routing in web-server (2 hours)
4. Update streamer to handle multi-peer IPC (2-3 hours)
5. Integrate with Helix backend and frontend (2 hours)
6. Testing and debugging (2-3 hours)

**Total remaining**: ~10-12 hours of implementation time

## Alternative: Quick Win Approach

If immediate functionality is needed, could implement a simplified version:
- Single API endpoint that creates streamer+peer together (like current /host/stream)
- But streamer stays alive when WebSocket disconnects
- Browser reconnect joins existing streamer
- Gets 80% of benefit with 20% of work

Would take ~3-4 hours to implement this simpler approach.
