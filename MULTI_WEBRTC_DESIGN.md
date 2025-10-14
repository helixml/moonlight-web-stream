# Multi-WebRTC Architecture Design

## Current Architecture (1:1 Coupling)

```
┌─────────────┐      ┌──────────────┐      ┌─────────────────┐
│   Browser   │─────▶│  Streamer    │─────▶│  Moonlight      │
│  WebSocket  │◀─────│  Process     │◀─────│  Stream (Wolf)  │
└─────────────┘      └──────────────┘      └─────────────────┘
     1 : 1                1 : 1                   1 : 1

Lifecycle: WebSocket dies → Streamer exits → Moonlight stream terminates
```

**Problems:**
- Can't have multiple browsers viewing same stream
- Can't have "headless" Moonlight stream (agent working before user connects)
- Can't reconnect without recreating entire Moonlight connection
- Each browser requires full Moonlight protocol overhead

## New Architecture (1:N Multiplexing)

```
┌─────────────┐
│   Browser   │─┐
│ WebSocket 1 │ │
└─────────────┘ │
                │    ┌──────────────┐      ┌─────────────────┐
┌─────────────┐ │    │  Streamer    │      │  Moonlight      │
│   Browser   │─┼───▶│  Process     │─────▶│  Stream (Wolf)  │
│ WebSocket 2 │ │    │              │◀─────│  (persistent)   │
└─────────────┘ │    │  - peers: [] │      └─────────────────┘
                │    │  - stream    │
┌─────────────┐ │    └──────────────┘
│   Browser   │─┘           ▲
│ WebSocket N │              │
└─────────────┘              │
      N : 1         WebRTC Peers List

Lifecycle:
- Streamer created → Moonlight stream started (no WebRTC yet)
- Browser connects → New WebRTC peer added to list
- Browser disconnects → Peer removed from list
- Last browser disconnects → Moonlight stream continues (headless)
- Streamer explicitly stopped → Moonlight stream terminates
```

## Component Breakdown

### 1. Streamer Process (Long-lived)

**Purpose**: Manages ONE Moonlight stream, multiplexes to N WebRTC peers

**Lifecycle**:
- Created by: `POST /api/streamers` (new endpoint)
- Lives: Until explicitly stopped or Moonlight stream fails
- Independent of: WebRTC peer connections

**State**:
```rust
struct Streamer {
    // Moonlight connection (1:1 with streamer)
    moonlight_host: Mutex<ReqwestMoonlightHost>,
    moonlight_stream: RwLock<Option<MoonlightStream>>,
    app_id: u32,

    // WebRTC multiplexing (1:N)
    peers: RwLock<HashMap<String, Arc<WebRtcPeer>>>,  // peer_id -> peer
    api: Arc<API>,  // To create new peers
    rtc_config: RTCConfiguration,

    // Media forwarding
    video_broadcaster: Arc<VideoBroadcaster>,  // Tees video to all peers
    audio_broadcaster: Arc<AudioBroadcaster>,  // Tees audio to all peers

    // Input aggregation
    input_aggregator: Arc<InputAggregator>,  // Combines inputs from all peers

    // Control
    terminate: Notify,
}
```

**Responsibilities**:
- Start/stop Moonlight stream
- Add/remove WebRTC peers dynamically
- Broadcast video/audio frames to all connected peers
- Aggregate input from all peers → send to Moonlight
- Handle Moonlight stream lifecycle independently

### 2. WebRTC Peer (Short-lived, Multiple)

**Purpose**: Represents ONE browser connection

**Lifecycle**:
- Created by: Browser WebSocket connecting to existing streamer
- Lives: Until browser disconnects or peer fails
- Independent of: Other peers and Moonlight stream

**State**:
```rust
struct WebRtcPeer {
    peer_id: String,  // Unique identifier
    connection: Arc<RTCPeerConnection>,
    ws_sender: Mutex<Option<WebSocketSender>>,  // IPC back to browser

    // Media tracks
    video_track: Arc<TrackLocalStaticSample>,
    audio_track: Arc<TrackLocalStaticSample>,

    // Input from this peer
    input_handler: Arc<PeerInputHandler>,

    // Statistics
    last_active: Mutex<Instant>,
}
```

**Responsibilities**:
- WebRTC signaling (offer/answer/ICE)
- Receive frames from broadcaster, send to peer
- Capture input from browser, send to aggregator
- Independent lifecycle management

### 3. Video/Audio Broadcasters

**Purpose**: Non-blocking frame distribution to multiple peers

**Architecture**:
```rust
struct VideoBroadcaster {
    subscribers: RwLock<Vec<mpsc::UnboundedSender<VideoFrame>>>,
}

impl VideoBroadcaster {
    async fn broadcast(&self, frame: VideoFrame) {
        let subs = self.subscribers.read().await;
        for sender in subs.iter() {
            // Try send, don't block if channel full (slow client)
            let _ = sender.try_send(frame.clone());
        }
    }

    async fn subscribe(&self) -> mpsc::UnboundedReceiver<VideoFrame> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.write().await.push(tx);
        rx
    }
}
```

**Key Properties**:
- Non-blocking: Slow clients don't slow down others
- Fan-out: Single source → multiple destinations
- Dynamic: Subscribers added/removed without affecting stream

### 4. Input Aggregator

**Purpose**: Combine inputs from all peers into single Moonlight input stream

**Architecture**:
```rust
struct InputAggregator {
    // Mouse position: Use most recent from any peer
    mouse_pos: RwLock<(i32, i32)>,

    // Keyboard: Union of all pressed keys
    pressed_keys: RwLock<HashSet<u16>>,

    // Mouse buttons: Union of all pressed buttons
    pressed_buttons: RwLock<HashSet<MouseButton>>,

    // Gamepads: Each peer can have separate gamepad
    gamepads: RwLock<HashMap<String, GamepadState>>,  // peer_id -> gamepad
}

impl InputAggregator {
    async fn handle_mouse_move(&self, peer_id: &str, x: i32, y: i32) {
        // Update global mouse position (last writer wins)
        *self.mouse_pos.write().await = (x, y);
        // Send to Moonlight
    }

    async fn handle_key_down(&self, peer_id: &str, key: u16) {
        self.pressed_keys.write().await.insert(key);
        // Send to Moonlight
    }

    async fn handle_key_up(&self, peer_id: &str, key: u16) {
        self.pressed_keys.write().await.remove(&key);
        // Send to Moonlight
    }
}
```

**Key Properties**:
- Union semantics: If ANY peer has key pressed, send to Moonlight
- Mouse: Most recent movement wins
- Gamepads: Independent per peer (don't conflict)

## API Changes

### New Endpoint: Create Streamer (Moonlight Only)

**Request**: `POST /api/streamers`
```json
{
  "streamer_id": "agent-ses_01k7...",  // Unique streamer ID
  "client_unique_id": "helix-agent-ses_01k7...",  // Moonlight client ID
  "host_id": 0,
  "app_id": 304391717,
  "settings": {
    "width": 2560,
    "height": 1600,
    "fps": 60,
    "bitrate": 5000,
    ...
  }
}
```

**Response**:
```json
{
  "streamer_id": "agent-ses_01k7...",
  "status": "active",
  "moonlight_connected": true,
  "connected_peers": 0
}
```

**Behavior**:
- Spawns streamer process
- Starts Moonlight stream immediately
- Does NOT create any WebRTC peers
- Returns success when Moonlight stream active

### New WebSocket: Connect WebRTC Peer to Existing Streamer

**URL**: `ws://moonlight-web:8080/api/streamers/{streamer_id}/peer`

**Messages**:
```typescript
// Client → Server
type WebRtcClientMessage =
  | { type: "authenticate", credentials: string }
  | { type: "signaling", data: SignalingMessage }

// Server → Client
type WebRtcServerMessage =
  | { type: "authenticated", peer_id: string }
  | { type: "signaling", data: SignalingMessage }
  | { type: "connectionComplete", capabilities, width, height }
  | { type: "error", message: string }
```

**Behavior**:
- Authenticates client
- Generates unique peer_id
- Creates new RTCPeerConnection
- Adds peer to streamer's peer list
- Handles WebRTC signaling independently
- On disconnect: Removes peer, keeps streamer running

### Modified Endpoint: List Streamers

**Request**: `GET /api/streamers`

**Response**:
```json
{
  "streamers": [
    {
      "streamer_id": "agent-ses_01k7...",
      "status": "active",
      "moonlight_connected": true,
      "connected_peers": 2,
      "peers": [
        {"peer_id": "browser-1234", "connected_at": "..."},
        {"peer_id": "browser-5678", "connected_at": "..."}
      ]
    }
  ]
}
```

### Modified Endpoint: Stop Streamer

**Request**: `DELETE /api/streamers/{streamer_id}`

**Behavior**:
- Disconnects all WebRTC peers gracefully
- Stops Moonlight stream
- Terminates streamer process

## Data Flow

### Video Frame Path (Moonlight → WebRTC)

```
Moonlight Stream
      │
      ▼
TrackSampleVideoDecoder::on_video_frame(frame)
      │
      ▼
VideoBroadcaster::broadcast(frame)
      │
      ├──▶ Peer 1: try_send(frame) [non-blocking]
      ├──▶ Peer 2: try_send(frame) [non-blocking]
      └──▶ Peer N: try_send(frame) [non-blocking]

Each peer has own receiver channel:
  loop {
    frame = rx.recv().await
    video_track.write_sample(frame).await
  }
```

**Key Decisions**:
- Use `try_send` not `send` - don't block on slow peers
- Each peer gets own unbounded channel (memory trade-off for no blocking)
- Frames cloned per peer (cheap - Arc<Vec<u8>>)

### Audio Frame Path (Moonlight → WebRTC)

Same pattern as video - identical broadcaster architecture.

### Input Path (WebRTC → Moonlight)

```
Peer 1 Data Channel: Mouse move (100, 200)
      │
      ▼
InputAggregator::handle_mouse_move(peer_id, 100, 200)
      │
      ▼
Update global mouse_pos = (100, 200)
      │
      ▼
MoonlightStream::send_mouse_move(100, 200)


Peer 2 Data Channel: Key down 'A'
      │
      ▼
InputAggregator::handle_key_down(peer_id, VK_KEY_A)
      │
      ▼
pressed_keys.insert(VK_KEY_A)
      │
      ▼
MoonlightStream::send_key_event(VK_KEY_A, down)
```

**Key Decisions**:
- Mouse position: Last writer wins (most recent peer)
- Keyboard: Union of all pressed keys
- If Peer 1 presses 'A', Peer 2 presses 'B' → Moonlight sees both pressed
- Key release: Only release when ALL peers released that key

## Implementation Plan

### Phase 1: Core Streamer Refactoring (Foundation)

**Goal**: Separate Moonlight stream management from WebRTC peer management

**Steps**:
1. Create new `Streamer` struct that owns `MoonlightStream` only
2. Move WebRTC peer logic into separate `WebRtcPeer` struct
3. Add `peers: RwLock<HashMap<String, Arc<WebRtcPeer>>>` to Streamer
4. Keep existing single-peer flow working (compatibility)

**Files to modify**:
- `moonlight-web/streamer/src/main.rs` - Restructure
- `moonlight-web/streamer/src/peer.rs` - NEW: WebRtcPeer implementation

**Validation**: Existing single browser connection still works

### Phase 2: Video/Audio Broadcasters

**Goal**: Enable frame distribution to multiple peers

**Steps**:
1. Create `moonlight-web/streamer/src/broadcaster.rs`
2. Implement `VideoBroadcaster` with non-blocking channel distribution
3. Implement `AudioBroadcaster` with same pattern
4. Modify video/audio decoders to broadcast instead of direct peer write
5. Each peer subscribes to broadcasters on creation

**Files to modify**:
- `moonlight-web/streamer/src/broadcaster.rs` - NEW
- `moonlight-web/streamer/src/video.rs` - Use broadcaster
- `moonlight-web/streamer/src/audio.rs` - Use broadcaster

**Validation**: Single peer still receives frames correctly

### Phase 3: Input Aggregator

**Goal**: Combine input from multiple peers

**Steps**:
1. Create `moonlight-web/streamer/src/input_aggregator.rs`
2. Implement mouse position tracking (last writer wins)
3. Implement keyboard state tracking (union of pressed keys)
4. Implement mouse button tracking (union)
5. Modify StreamInput to send to aggregator instead of direct Moonlight

**Files to modify**:
- `moonlight-web/streamer/src/input_aggregator.rs` - NEW
- `moonlight-web/streamer/src/input.rs` - Use aggregator

**Validation**: Single peer input still works correctly

### Phase 4: Streamer Creation API

**Goal**: Create streamers without WebRTC

**Steps**:
1. Add `POST /api/streamers` endpoint in web-server
2. Spawn streamer process with Init message (no WebSocket)
3. Store streamer process handle in RuntimeApiData
4. Streamer starts Moonlight stream immediately
5. Returns success when stream active

**Files to modify**:
- `moonlight-web/web-server/src/api/streamers.rs` - NEW
- `moonlight-web/web-server/src/data.rs` - Add streamers map
- `moonlight-web/common/src/ipc.rs` - Add StartMoonlight IPC message

**Validation**: Streamer creates, Moonlight stream starts, no WebRTC

### Phase 5: WebRTC Peer Addition

**Goal**: Dynamically add WebRTC peers to existing streamers

**Steps**:
1. Add WebSocket endpoint `/api/streamers/{streamer_id}/peer`
2. Authenticate WebSocket connection
3. Send AddPeer IPC message to streamer process
4. Streamer creates new WebRtcPeer, adds to peers map
5. Handle WebRTC signaling over WebSocket
6. Subscribe peer to video/audio broadcasters

**Files to modify**:
- `moonlight-web/web-server/src/api/streamers.rs` - Add WebSocket handler
- `moonlight-web/common/src/ipc.rs` - Add AddPeer/RemovePeer messages
- `moonlight-web/streamer/src/main.rs` - Handle AddPeer IPC

**Validation**: Can add multiple browsers to same streamer

### Phase 6: Helix Integration

**Goal**: Use new streamer API from Helix

**Steps**:
1. Update `wolf_executor_apps.go` to call `POST /api/streamers` instead of WebSocket
2. Remove WebSocket keepalive connection logic
3. Update frontend to connect to `/api/streamers/{streamer_id}/peer`
4. Remove mode="join" logic (not needed - always fresh peer)

**Files to modify**:
- `helix/api/pkg/external-agent/wolf_executor_apps.go` - Use streamers API
- `helix/frontend/src/lib/moonlight-web-ts/stream/index.ts` - Connect to streamer peer endpoint

**Validation**: External agents work with new architecture

## Key Technical Decisions

### Non-Blocking Frame Distribution

**Problem**: One slow peer shouldn't slow down others

**Solution**: Use `try_send` instead of `send`
```rust
for (peer_id, tx) in broadcasters {
    if let Err(_) = tx.try_send(frame.clone()) {
        warn!("Peer {peer_id} channel full, dropping frame");
        // Don't block - just drop frame for this peer
    }
}
```

**Trade-off**: Slow peers get frame drops vs blocking all peers

### Input Aggregation Semantics

**Mouse Position**:
- Last writer wins (most recent peer movement)
- Instant updates, no buffering

**Keyboard**:
- Union of all pressed keys
- Key down: Any peer presses → send to Moonlight
- Key up: Only when ALL peers released → send to Moonlight
- Prevents: Peer A holds 'A', Peer B disconnects → 'A' still pressed

**Mouse Buttons**:
- Same as keyboard - union semantics
- Any peer clicks → button pressed

### Peer ID Generation

Use random UUID or timestamp-based:
```rust
let peer_id = format!("peer-{}-{}", timestamp, random);
```

Must be unique across all peers in streamer.

### Error Handling

**Moonlight stream fails**:
- Stop all WebRTC peers gracefully
- Terminate streamer process
- Client reconnect creates new streamer

**WebRTC peer fails**:
- Remove from peers list
- Unsubscribe from broadcasters
- Clean up input state for that peer
- Other peers unaffected

**Last peer disconnects**:
- Moonlight stream continues (headless mode)
- Streamer process continues running
- Ready for new peers to join

## Migration Strategy

### Backward Compatibility

During migration, support BOTH APIs:
- Old: `POST /host/stream` (WebSocket, creates streamer+peer in one)
- New: `POST /api/streamers` + `WS /api/streamers/{id}/peer`

Helix can migrate to new API while keeping old API functional.

### Rollout Steps

1. Implement new architecture on `feat/multi-webrtc` branch
2. Deploy both APIs side-by-side
3. Migrate Helix external agents to new API
4. Test thoroughly
5. Migrate other Helix components
6. Deprecate old API

## File Structure

```
moonlight-web/
├── streamer/
│   └── src/
│       ├── main.rs           # Main loop, Streamer struct
│       ├── peer.rs            # NEW: WebRtcPeer implementation
│       ├── broadcaster.rs     # NEW: Video/Audio broadcasting
│       ├── input_aggregator.rs # NEW: Input combination
│       ├── video.rs           # Modified: Use broadcaster
│       ├── audio.rs           # Modified: Use broadcaster
│       ├── input.rs           # Modified: Send to aggregator
│       └── ...
├── web-server/
│   └── src/
│       └── api/
│           ├── streamers.rs   # NEW: Streamer CRUD + WebSocket
│           ├── stream.rs      # Keep for backward compat
│           └── mod.rs          # Route both endpoints
└── common/
    └── src/
        ├── ipc.rs             # Add: StartMoonlight, AddPeer, RemovePeer
        └── api_bindings.rs    # Add: Streamer types
```

## Testing Strategy

### Unit Tests
- VideoBroadcaster: Single frame → N subscribers
- AudioBroadcaster: Verify non-blocking on slow subscriber
- InputAggregator: Union semantics for keyboard
- InputAggregator: Mouse position updates

### Integration Tests
1. Create streamer → verify Moonlight connects
2. Add peer 1 → verify video/audio received
3. Add peer 2 → verify both receive frames
4. Peer 1 sends input → verify Moonlight receives
5. Peer 2 sends input → verify both inputs combined
6. Remove peer 1 → verify peer 2 unaffected, Moonlight continues
7. Remove peer 2 → verify streamer stays alive (headless)

### Performance Tests
- 5 concurrent peers → measure frame latency
- Slow peer (throttle bandwidth) → verify others unaffected
- Input from 3 peers simultaneously → verify union correct

## Success Criteria

✅ External agent starts → Moonlight stream active (no browser)
✅ Browser 1 connects → sees video/audio
✅ Browser 2 connects → both see same stream
✅ Browser 1 types 'A' → appears in stream
✅ Browser 2 types 'B' → appears in stream
✅ Browser 1 disconnects → Browser 2 unaffected
✅ Browser 2 disconnects → Moonlight stream continues
✅ No log spam from cleanup
✅ Clean architecture, maintainable code

## Timeline Estimate

- Phase 1 (Foundation): 2-3 hours
- Phase 2 (Broadcasters): 1-2 hours
- Phase 3 (Input Aggregator): 1-2 hours
- Phase 4 (Streamer API): 1 hour
- Phase 5 (Peer WebSocket): 2 hours
- Phase 6 (Helix Integration): 1 hour
- Testing & Debugging: 2-3 hours

**Total**: 10-15 hours of focused development

This is a significant architectural change but provides the clean separation you want between Moonlight and WebRTC lifecycles.
