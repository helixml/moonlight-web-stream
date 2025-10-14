# Multi-WebRTC Implementation Status

## Executive Summary

**Completion: ~60% (Foundation Complete, Integration Remaining)**

The architectural foundation for multi-WebRTC peer support is **fully implemented and compiling**. The remaining work is primarily integration - wiring together the existing components and implementing the full lifecycle management in the API layer.

## ✅ Completed Work (Phases 1-4)

### Phase 1: Core Infrastructure ✅
**Files Modified:**
- `moonlight-web/streamer/src/peer.rs` (NEW) - WebRtcPeer struct
- `moonlight-web/streamer/src/main.rs` - Added peers HashMap, rtc_config storage

**What Works:**
- `WebRtcPeer` struct created for representing individual peer connections
- `peers: HashMap<String, Arc<WebRtcPeer>>` added to StreamConnection
- `rtc_config: RTCConfiguration` stored for creating additional peers
- Backward compatible - existing single-peer flow unchanged

### Phase 2: Broadcasters ✅
**Files Modified:**
- `moonlight-web/streamer/src/broadcaster.rs` (NEW)
- `moonlight-web/streamer/src/main.rs` - Integrated broadcasters

**What Works:**
- `VideoBroadcaster` with non-blocking frame distribution
- `AudioBroadcaster` with non-blocking sample distribution
- Uses `UnboundedSender` (non-blocking, unlimited buffer)
- `subscribe(peer_id)` / `unsubscribe(peer_id)` for dynamic peer management
- `broadcast(frame/sample)` sends to all subscribed peers without blocking

### Phase 3: Input Aggregator ✅
**Files Modified:**
- `moonlight-web/streamer/src/input_aggregator.rs` (NEW)
- `moonlight-web/streamer/src/main.rs` - Integrated aggregator with shared stream

**What Works:**
- `HashMap<key, HashSet<peer_id>>` tracks which peers hold each key
- Union semantics: key down when FIRST peer presses, up when LAST releases
- Mouse: last writer wins, immediate forwarding
- `remove_peer()` automatically releases all inputs on disconnect
- Shares `Arc<RwLock<MoonlightStream>>` for immediate input forwarding

### Phase 4: IPC Messages & API Types ✅
**Files Modified:**
- `moonlight-web/common/src/ipc.rs` - New IPC variants
- `moonlight-web/common/src/api_bindings.rs` - API types
- `moonlight-web/web-server/src/api/streamers.rs` (NEW) - REST endpoints
- `moonlight-web/web-server/src/api/mod.rs` - Route registration
- `moonlight-web/streamer/src/main.rs` - Stub IPC handlers
- `moonlight-web/web-server/src/api/stream.rs` - Stub IPC handlers

**What Works:**
- New `ServerIpcMessage` variants: `StartMoonlight`, `AddPeer`, `FromPeer`, `RemovePeer`
- New `StreamerIpcMessage` variants: `ToPeer`, `Broadcast`, `StreamerReady`, `MoonlightConnected`
- API types: `CreateStreamerRequest`, `StreamerInfo`, `PeerInfo`, `ListStreamersResponse`
- REST endpoints (skeleton): POST/GET/DELETE `/api/streamers`, WS `/api/streamers/{id}/peer`
- `StreamerRegistry` for state management
- All pattern matches handle new variants (with TODOs for implementation)

**Status:** Compiles successfully, endpoints return NotImplemented

## ⏳ Remaining Work (Phases 5-6)

### Phase 5: Full Multi-Peer Implementation (Estimated: 8-10 hours)

#### 5.1: Streamer-Side Peer Lifecycle (3-4 hours)
**File:** `moonlight-web/streamer/src/main.rs`

**TODO:** Implement IPC message handlers:

```rust
ServerIpcMessage::StartMoonlight => {
    // Start Moonlight stream without creating WebRTC peer
    // 1. Call self.start_stream().await
    // 2. Send StreamerIpcMessage::MoonlightConnected
}

ServerIpcMessage::AddPeer { peer_id } => {
    // Create new RTCPeerConnection for this peer
    // 1. Reconstruct API from stored rtc_config (or pass API reference)
    // 2. Create new RTCPeerConnection
    // 3. Subscribe peer to video_broadcaster
    // 4. Subscribe peer to audio_broadcaster
    // 5. Setup data channels
    // 6. Store in self.peers HashMap
    // 7. Setup WebRTC event handlers (onicecandidate → ToPeer IPC)
}

ServerIpcMessage::FromPeer { peer_id, message } => {
    // Route signaling message to correct peer
    // 1. Look up peer in self.peers
    // 2. Handle Signaling(Description) → set_remote_description on peer
    // 3. Handle Signaling(IceCandidate) → add_ice_candidate on peer
    // 4. Send responses via ToPeer IPC
}

ServerIpcMessage::RemovePeer { peer_id } => {
    // Clean up peer connection
    // 1. video_broadcaster.unsubscribe(&peer_id)
    // 2. audio_broadcaster.unsubscribe(&peer_id)
    // 3. input_aggregator.remove_peer(&peer_id)
    // 4. Close RTCPeerConnection
    // 5. Remove from self.peers HashMap
}
```

#### 5.2: Wire Broadcasters to WebRTC (2-3 hours)
**Files:** `moonlight-web/streamer/src/video.rs`, `moonlight-web/streamer/src/audio.rs`

**TODO:** Modify decoders to broadcast frames:

```rust
// In TrackSampleVideoDecoder::on_video_sample:
async fn on_video_sample(&mut self, sample: VideoSample) {
    // Existing code to decode sample...

    // NEW: Broadcast to all peers
    self.connection.video_broadcaster.broadcast(VideoFrame {
        data: Arc::new(encoded_data),
        codec: codec_capability,
        timestamp: sample.timestamp,
    }).await;

    // Legacy: Send to single peer track (for backward compat)
    // ... existing track write code
}

// Similar for OpusTrackSampleAudioDecoder
```

**TODO:** Subscribe peer tracks to broadcaster channels:

```rust
// In AddPeer handler, after creating RTCPeerConnection:
let mut video_rx = video_broadcaster.subscribe(peer_id.clone()).await;
let video_track = create_video_track(&peer);

spawn(async move {
    while let Some(frame) = video_rx.recv().await {
        video_track.write_sample(&frame.data, frame.timestamp).await;
    }
});
// Similar for audio
```

#### 5.3: Full Streamers API Implementation (3-4 hours)
**File:** `moonlight-web/web-server/src/api/streamers.rs`

**TODO:** Implement endpoint logic:

```rust
#[post("/api/streamers")]
pub async fn create_streamer(...) -> HttpResponse {
    // 1. Generate unique streamer_id
    // 2. Spawn streamer process (Command::new("streamer"))
    // 3. Create IPC channels (stdin/stdout)
    // 4. Send ServerIpcMessage::Init with settings
    // 5. Send ServerIpcMessage::StartMoonlight
    // 6. Wait for StreamerIpcMessage::MoonlightConnected (with timeout)
    // 7. Store StreamerState in registry:
    //    - process handle
    //    - IPC sender
    //    - peer_websockets: Arc<RwLock<HashMap<peer_id, WebSocket>>>
    // 8. Spawn IPC receiver task for routing ToPeer/Broadcast messages
    // 9. Return StreamerInfo
}

#[get("/api/streamers/{id}/peer")]
pub async fn connect_peer(...) -> HttpResponse {
    // 1. Look up streamer in registry
    // 2. Upgrade to WebSocket connection
    // 3. Generate unique peer_id
    // 4. Send ServerIpcMessage::AddPeer to streamer
    // 5. Store WebSocket in streamer.peer_websockets
    // 6. Spawn bidirectional routing:
    //    - WebSocket recv → ServerIpcMessage::FromPeer → streamer IPC
    //    - Streamer IPC StreamerIpcMessage::ToPeer → WebSocket send
    // 7. On WebSocket close: ServerIpcMessage::RemovePeer
}
```

**TODO:** IPC Routing Task (in create_streamer):

```rust
spawn(async move {
    while let Some(msg) = streamer_ipc_rx.recv().await {
        match msg {
            StreamerIpcMessage::ToPeer { peer_id, message } => {
                if let Some(ws) = peer_websockets.read().await.get(&peer_id) {
                    ws.send(serialize_json(&message)).await;
                }
            }
            StreamerIpcMessage::Broadcast(message) => {
                let json = serialize_json(&message);
                for ws in peer_websockets.read().await.values() {
                    ws.send(json.clone()).await;
                }
            }
            StreamerIpcMessage::MoonlightConnected => {
                // Update streamer status in registry
            }
            _ => {}
        }
    }
});
```

### Phase 6: Helix Integration (2-3 hours)

#### 6.1: Backend Integration (1-2 hours)
**File:** `helix/api/pkg/external-agent/wolf_executor_apps.go`

**TODO:** Replace WebSocket with HTTP API:

```go
// Instead of connecting to /host/stream WebSocket:

// 1. Create streamer via REST API
resp, err := http.Post(
    fmt.Sprintf("%s/api/streamers", moonlightWebURL),
    "application/json",
    bytes.NewBuffer(createStreamerJSON),
)

// 2. Parse response to get streamer_id
var streamerInfo StreamerInfo
json.NewDecoder(resp.Body).Decode(&streamerInfo)

// 3. Connect to peer WebSocket (browser can also connect here)
// No changes needed - frontend connects to /api/streamers/{id}/peer

// 4. Remove all keepalive logic (streamer stays alive automatically)
```

#### 6.2: Frontend Integration (1 hour)
**File:** `helix/frontend/src/lib/moonlight-web-ts/stream/index.ts`

**TODO:** Update WebSocket URL:

```typescript
// Change from:
const ws = new WebSocket(`/host/stream`)

// To:
const ws = new WebSocket(`/api/streamers/${streamerId}/peer`)

// Keep all existing Stream class logic - protocol unchanged!
// Just change the WebSocket endpoint
```

**File:** `helix/frontend/src/components/admin/AgentSandboxes.tsx` (if needed)

**TODO:** Display streamer info:

```typescript
// Call GET /api/streamers to list active streamers
// Show streamer status, connected peer count, etc.
```

## Testing Plan

### Unit Testing
- [ ] Broadcaster subscribe/unsubscribe/broadcast
- [ ] Input aggregator union semantics (multi-peer key tracking)
- [ ] IPC message serialization/deserialization

### Integration Testing
1. **Legacy Flow (Baseline)**
   - [ ] Existing `/host/stream` WebSocket works unchanged
   - [ ] Single peer connection, Moonlight stream, input/video/audio

2. **Streamer API**
   - [ ] POST /api/streamers creates persistent stream
   - [ ] Moonlight starts without WebRTC peer
   - [ ] GET /api/streamers returns correct info
   - [ ] DELETE /api/streamers stops stream

3. **Multi-Peer WebSocket**
   - [ ] First peer connects, WebRTC negotiates
   - [ ] Second peer connects to same streamer
   - [ ] Both peers receive same video/audio frames
   - [ ] Both peers can send input (union semantics work)
   - [ ] One peer disconnects, other continues
   - [ ] All peers disconnect, streamer persists

4. **External Agents (Helix)**
   - [ ] Agent starts, streamer created automatically
   - [ ] Agent works before browser connects
   - [ ] Browser connects to existing agent streamer
   - [ ] Browser disconnects, agent continues
   - [ ] Agent completes task, streamer stops

## Known Issues / Limitations

1. **API Reconstruction**: webrtc::API is not Clone, need to reconstruct for each new peer or refactor to share
2. **Process Management**: Full lifecycle (respawn on crash, cleanup orphans) not implemented
3. **Error Handling**: Minimal error handling in stub implementations
4. **Metrics**: No metrics/monitoring for streamer health, peer counts, etc.
5. **Security**: No authentication on streamer endpoints beyond existing auth middleware
6. **Concurrency**: Peer HashMap access patterns may need refinement under heavy load

## File Summary

### Created Files
- `moonlight-web/streamer/src/peer.rs` - WebRtcPeer struct
- `moonlight-web/streamer/src/broadcaster.rs` - Video/Audio broadcasters
- `moonlight-web/streamer/src/input_aggregator.rs` - Multi-peer input aggregation
- `moonlight-web/web-server/src/api/streamers.rs` - Streamer REST API

### Modified Files
- `moonlight-web/common/src/ipc.rs` - New IPC message types
- `moonlight-web/common/src/api_bindings.rs` - API request/response types
- `moonlight-web/streamer/src/main.rs` - Integrated all new components
- `moonlight-web/web-server/src/api/mod.rs` - Registered new endpoints
- `moonlight-web/web-server/src/api/stream.rs` - Handle new IPC variants

### Compilation Status
✅ **All code compiles successfully** (verified with `docker compose -f docker-compose.dev.yaml build moonlight-web`)

## Next Steps to Complete

**Immediate Priority (to get working end-to-end):**

1. **Implement AddPeer/RemovePeer handlers in streamer** (2-3 hours)
   - Create RTCPeerConnection for new peers
   - Subscribe to broadcasters
   - Store peer in HashMap

2. **Wire broadcasters to decoders** (1-2 hours)
   - Modify video/audio decoders to broadcast frames
   - Subscribe peer tracks to broadcaster channels

3. **Implement create_streamer and connect_peer APIs** (2-3 hours)
   - Spawn streamer process with IPC
   - WebSocket peer management
   - Bidirectional IPC routing

4. **Test multi-peer flow** (1 hour)
   - Two browsers connecting to same streamer
   - Verify video/audio broadcast and input union

5. **Helix integration** (2 hours)
   - Update wolf_executor to use streamer API
   - Test external agent persistence

**Total Remaining: ~8-10 hours of focused implementation work**

## Alternative: Quick Win Approach

If immediate functionality is needed with less implementation time (~3-4 hours):

**Modify existing `/host/stream` to persist streamers:**
1. Add streamer registry to web-server
2. Check for existing streamer before spawning new one
3. Reuse existing Moonlight stream, create new WebRTC peer
4. Don't kill streamer on WebSocket close
5. Optional cleanup timer (kill after N minutes of no connections)

This gets:
- ✅ External agents working before browser connects
- ✅ Browser reconnection without stream restart
- ✅ Foundation for full multi-peer (broadcasters/aggregators already exist)

But doesn't get:
- ❌ Multiple simultaneous viewers (only one WebRTC peer)
- ❌ Clean REST API
- ❌ Explicit streamer lifecycle management

The quick win can be upgraded to full multi-peer later by completing the remaining work.
