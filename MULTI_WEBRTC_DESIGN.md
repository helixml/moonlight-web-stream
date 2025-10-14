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
    streamer_id: String,

    // Moonlight connection (1:1 with streamer)
    moonlight_host: Mutex<ReqwestMoonlightHost>,
    moonlight_stream: Arc<RwLock<Option<MoonlightStream>>>,
    moonlight: MoonlightInstance,
    app_id: u32,
    settings: StreamSettings,

    // WebRTC multiplexing (1:N)
    peers: RwLock<HashMap<String, Arc<WebRtcPeer>>>,  // peer_id -> peer
    api: Arc<API>,  // To create new peers
    rtc_config: RTCConfiguration,

    // Media forwarding
    video_broadcaster: Arc<VideoBroadcaster>,  // Tees video to all peers
    audio_broadcaster: Arc<AudioBroadcaster>,  // Tees audio to all peers

    // Input aggregation
    input_aggregator: Arc<InputAggregator>,  // Combines inputs from all peers

    // IPC communication
    ipc_sender: IpcSender<StreamerIpcMessage>,

    // Control
    terminate: Notify,
}

impl Streamer {
    async fn start_moonlight_stream(&self) -> Result<()> {
        // Called during streamer initialization
        // Starts Moonlight RTSP/RTP connection to Wolf
        // Does NOT create any WebRTC peers
    }

    async fn add_peer(&self, peer_id: String) -> Result<()> {
        // Create new RTCPeerConnection
        // Subscribe to video/audio broadcasters
        // Add to peers map
        // Send WebRtcConfig to peer
    }

    async fn remove_peer(&self, peer_id: &str) {
        // Remove from peers map
        // Unsubscribe from broadcasters
        // Clean up input aggregator state
        // Close peer connection
    }

    async fn handle_peer_message(&self, peer_id: &str, message: StreamClientMessage) {
        // Route signaling messages to correct peer
        // Handle input events via input aggregator
    }
}
```

**Responsibilities**:
- Start/stop Moonlight stream (independent of peers)
- Add/remove WebRTC peers dynamically
- Broadcast video/audio frames to all connected peers
- Aggregate input from all peers → send to Moonlight immediately
- Handle Moonlight stream lifecycle independently
- Route IPC messages to/from correct peers

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
    moonlight_stream: Arc<RwLock<Option<MoonlightStream>>>,

    // Mouse position: Use most recent from any peer
    mouse_pos: RwLock<(i32, i32)>,

    // Keyboard: Track which peers have each key pressed
    key_states: RwLock<HashMap<u16, HashSet<String>>>,  // key -> set of peer_ids

    // Mouse buttons: Track which peers have each button pressed
    button_states: RwLock<HashMap<MouseButton, HashSet<String>>>,  // button -> set of peer_ids

    // Gamepads: Each peer can have separate gamepad
    gamepads: RwLock<HashMap<String, GamepadState>>,  // peer_id -> gamepad
}

impl InputAggregator {
    async fn handle_mouse_move(&self, peer_id: &str, x: i32, y: i32) {
        // Update global mouse position (last writer wins)
        *self.mouse_pos.write().await = (x, y);
        // Send to Moonlight IMMEDIATELY
        if let Some(stream) = self.moonlight_stream.read().await.as_ref() {
            stream.send_mouse_move(x, y).await;
        }
    }

    async fn handle_key_down(&self, peer_id: &str, key: u16) {
        let mut states = self.key_states.write().await;
        let peers_holding = states.entry(key).or_insert_with(HashSet::new);
        let is_first = peers_holding.is_empty();
        peers_holding.insert(peer_id.to_string());

        // Only send key_down to Moonlight if this is FIRST peer pressing this key
        if is_first {
            if let Some(stream) = self.moonlight_stream.read().await.as_ref() {
                stream.send_key_event(key, true).await;
            }
        }
    }

    async fn handle_key_up(&self, peer_id: &str, key: u16) {
        let mut states = self.key_states.write().await;
        if let Some(peers_holding) = states.get_mut(&key) {
            peers_holding.remove(peer_id);

            // Only send key_up to Moonlight if NO peers are holding this key anymore
            if peers_holding.is_empty() {
                states.remove(&key);
                if let Some(stream) = self.moonlight_stream.read().await.as_ref() {
                    stream.send_key_event(key, false).await;
                }
            }
        }
    }

    // Called when peer disconnects - release all inputs from that peer
    async fn remove_peer(&self, peer_id: &str) {
        // Release all keys this peer was holding
        let mut key_states = self.key_states.write().await;
        let mut keys_to_release = Vec::new();

        for (key, peers) in key_states.iter_mut() {
            if peers.remove(peer_id) && peers.is_empty() {
                keys_to_release.push(*key);
            }
        }

        // Send key_up for all keys that are now fully released
        if let Some(stream) = self.moonlight_stream.read().await.as_ref() {
            for key in keys_to_release {
                key_states.remove(&key);
                stream.send_key_event(key, false).await;
            }
        }

        // Same for mouse buttons
        let mut button_states = self.button_states.write().await;
        let mut buttons_to_release = Vec::new();

        for (button, peers) in button_states.iter_mut() {
            if peers.remove(peer_id) && peers.is_empty() {
                buttons_to_release.push(*button);
            }
        }

        if let Some(stream) = self.moonlight_stream.read().await.as_ref() {
            for button in buttons_to_release {
                button_states.remove(&button);
                stream.send_mouse_button(button, false).await;
            }
        }

        // Remove gamepad for this peer
        self.gamepads.write().await.remove(peer_id);
    }
}
```

**Key Properties**:
- Union semantics: Key sent down when FIRST peer presses, up when LAST peer releases
- Mouse: Most recent movement wins
- Gamepads: Independent per peer (don't conflict)
- Cleanup: Peer disconnect releases all its inputs

## API Changes

### New Endpoint: Create Streamer (Moonlight Only)

**Request**: `POST /api/streamers`
```json
{
  "streamer_id": "agent-ses_01k7...",  // Unique streamer ID (client chooses)
  "client_unique_id": "helix-agent-ses_01k7...",  // Moonlight client ID
  "host_id": 0,
  "app_id": 304391717,
  "settings": {
    "width": 2560,
    "height": 1600,
    "fps": 60,
    "bitrate": 5000,
    "packet_size": 1024,
    "video_sample_queue_size": 10,
    "audio_sample_queue_size": 10,
    "play_audio_local": false,
    "video_supported_formats": 1,  // H264
    "video_colorspace": "Rec709",
    "video_color_range_full": false
  }
}
```

**Response**:
```json
{
  "streamer_id": "agent-ses_01k7...",
  "status": "active",
  "moonlight_connected": true,
  "connected_peers": 0,
  "width": 2560,
  "height": 1600,
  "fps": 60
}
```

**Behavior**:
- Spawns streamer process
- Starts Moonlight stream immediately with specified settings
- Does NOT create any WebRTC peers
- Returns success when Moonlight stream active

### New Endpoint: List Streamers

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
      "width": 2560,
      "height": 1600,
      "fps": 60,
      "peers": [
        {"peer_id": "peer-1234", "connected_at": "2025-10-14T19:00:00Z"},
        {"peer_id": "peer-5678", "connected_at": "2025-10-14T19:05:00Z"}
      ]
    }
  ]
}
```

### New Endpoint: Stop Streamer

**Request**: `DELETE /api/streamers/{streamer_id}`

**Response**: `204 No Content`

**Behavior**:
- Disconnects all WebRTC peers gracefully (sends PeerDisconnect to each)
- Stops Moonlight stream
- Terminates streamer process
- Returns 404 if streamer_id not found

### New Endpoint: Get Streamer Details

**Request**: `GET /api/streamers/{streamer_id}`

**Response**:
```json
{
  "streamer_id": "agent-ses_01k7...",
  "status": "active",
  "moonlight_connected": true,
  "connected_peers": 2,
  "width": 2560,
  "height": 1600,
  "fps": 60,
  "created_at": "2025-10-14T18:00:00Z",
  "peers": [
    {"peer_id": "peer-1234", "connected_at": "2025-10-14T19:00:00Z"},
    {"peer_id": "peer-5678", "connected_at": "2025-10-14T19:05:00Z"}
  ]
}
```

### New WebSocket: Connect WebRTC Peer to Existing Streamer

**URL**: `ws://moonlight-web:8080/api/streamers/{streamer_id}/peer`

**Messages**: **IDENTICAL to current /host/stream WebSocket!**

```typescript
// Client → Server (SAME as current protocol)
type StreamClientMessage =
  | { AuthenticateAndInit: { credentials, client_unique_id, host_id, app_id, ... } }
  | { Signaling: SignalingMessage }

// Server → Client (SAME as current protocol)
type StreamServerMessage =
  | { WebRtcConfig: { ice_servers } }
  | { Signaling: SignalingMessage }
  | { ConnectionComplete: { capabilities, width, height } }
  | { PeerDisconnect }
  | ...
```

**Behavior**:
- Uses EXISTING StreamClientMessage/StreamServerMessage types (no protocol changes!)
- On `AuthenticateAndInit`:
  - Validates credentials
  - **IGNORES**: width, height, fps, bitrate, video_sample_queue_size, etc. (streamer already configured)
  - **USES**: credentials for auth
  - Generates unique peer_id
  - Creates new RTCPeerConnection
  - Adds peer to streamer's peer list
- Then handles WebRTC signaling normally
- On disconnect: Removes peer, keeps streamer running

**Key Insight**: WebSocket protocol is **100% backward compatible**! Frontend just connects to different URL, sends same messages, streamer ignores irrelevant settings.

## Startup Flow Sequences

### Create Streamer (Headless)

```
1. Helix calls: POST /api/streamers { streamer_id, client_unique_id, app_id, settings }
2. Web-server spawns streamer process (stdin/stdout IPC)
3. Web-server sends: Init { host_config, app_id, client_unique_id, settings }
4. Streamer receives Init → creates MoonlightHost, configures WebRTC API
5. Web-server sends: StartMoonlight
6. Streamer starts Moonlight stream (RTSP handshake, starts Wolf container)
7. Streamer sends: MoonlightConnected
8. Web-server returns 200 OK to Helix
9. Streamer runs in background, no WebRTC peers yet (headless)
```

### Browser Joins Existing Streamer

```
1. Browser opens: WS /api/streamers/{streamer_id}/peer
2. Web-server generates unique peer_id: "peer-{timestamp}-{random}"
3. Web-server stores: peer_websockets.insert(peer_id, websocket_session)
4. Browser sends: AuthenticateAndInit { credentials, ... }
5. Web-server validates credentials
6. Web-server sends to streamer via IPC: AddPeer { peer_id: "peer-abc123" }
7. Streamer creates RTCPeerConnection for this peer
8. Streamer subscribes peer to video/audio broadcasters
9. Streamer sends via IPC: ToPeer { peer_id, WebRtcConfig { ice_servers } }
10. Web-server routes to peer's WebSocket → Browser
11. Browser receives WebRtcConfig, creates RTCPeerConnection
12. Streamer sends via IPC: ToPeer { peer_id, Signaling(Offer) }
13. Web-server routes to WebSocket → Browser receives offer
14. Browser creates answer, sends: Signaling(Answer)
15. Web-server forwards via IPC: FromPeer { peer_id, Signaling(Answer) }
16. Streamer sets remote description on that peer's RTCPeerConnection
17. ICE candidates exchanged (same FromPeer/ToPeer routing via WebSocket)
18. ICE connects → **DIRECT WebRTC connection established** (browser ↔ streamer process)
19. Media flows: Streamer → Browser (P2P or via TURN, NOT through web-server!)
20. Streamer sends via IPC: ToPeer { peer_id, ConnectionComplete }
21. Web-server routes to WebSocket → Browser shows stream, enables controls
```

**Critical Architecture Point**:
- **Signaling** (offer/answer/ICE): Goes through WebSocket → web-server → IPC → streamer
- **Media** (video/audio RTP): Direct P2P connection between browser and streamer process
- Web-server is ONLY a signaling router, not a media proxy
- This is standard WebRTC architecture - signaling separate from media transport

### Browser Disconnects

```
1. Browser closes WebSocket (or network failure)
2. Web-server detects close
3. Web-server sends to streamer: RemovePeer { peer_id }
4. Streamer calls peer.remove_peer(peer_id):
   - Unsubscribes from video/audio broadcasters
   - Calls input_aggregator.remove_peer(peer_id) (releases keys/buttons)
   - Removes from peers map
   - Closes RTCPeerConnection
5. Other peers completely unaffected
6. Moonlight stream continues (headless mode)
```

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
      ├─▶ Update global mouse_pos = (100, 200)
      └─▶ IMMEDIATELY: MoonlightStream::send_mouse_move(100, 200)


Peer 2 Data Channel: Key down 'A'
      │
      ▼
InputAggregator::handle_key_down(peer_id, VK_KEY_A)
      │
      ├─▶ pressed_keys.insert(VK_KEY_A)
      └─▶ IMMEDIATELY: MoonlightStream::send_key_event(VK_KEY_A, down)

Peer 1 Data Channel: Key up 'A' (but Peer 2 still holding 'A')
      │
      ▼
InputAggregator::handle_key_up(peer_id, VK_KEY_A)
      │
      ├─▶ Check: Are any OTHER peers still holding 'A'?
      └─▶ If all released: MoonlightStream::send_key_event(VK_KEY_A, up)
          If others holding: DO NOTHING (key stays pressed)
```

**Key Decisions**:
- **Mouse position**: Send immediately, last writer wins (most recent peer)
- **Keyboard down**: Send immediately when ANY peer presses
- **Keyboard up**: Only send when ALL peers have released that key
  - Track which peers have each key pressed: `HashMap<key, HashSet<peer_id>>`
  - On key_up: Remove peer from set, if set empty → send key_up to Moonlight
- **Mouse buttons**: Same as keyboard (union with immediate send)
- **No batching**: Every input event triggers immediate Moonlight send

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

### IPC Communication Architecture

**Problem**: Streamer uses stdin/stdout IPC but needs to communicate with N WebSockets

**Solution**: Web-server acts as message router

```rust
// In web-server (RuntimeApiData)
struct StreamerState {
    process: Child,
    ipc_sender: IpcSender<ServerIpcMessage>,
    ipc_receiver: IpcReceiver<StreamerIpcMessage>,
    peer_websockets: Arc<RwLock<HashMap<String, Session>>>,  // peer_id -> WebSocket
}

// Message routing
streamer_ipc_receiver loop:
    message = recv()
    match message {
        StreamerIpcMessage::ToPeer { peer_id, message } => {
            // Route to specific peer's WebSocket
            websockets.get(peer_id).send(message)
        }
        StreamerIpcMessage::Broadcast(message) => {
            // Send to ALL peer WebSockets
            for ws in websockets.values() {
                ws.send(message)
            }
        }
    }

// From WebSocket to streamer
peer_websocket loop:
    message = recv()
    streamer.ipc_sender.send(ServerIpcMessage::FromPeer { peer_id, message })
```

**IPC Message Updates**:
```rust
enum ServerIpcMessage {
    Init { ... },  // Initial streamer setup
    StartMoonlight,  // Start Moonlight stream (no WebRTC)
    AddPeer { peer_id: String },  // New WebRTC peer joined
    FromPeer { peer_id: String, message: StreamClientMessage },  // Peer message
    RemovePeer { peer_id: String },  // Peer disconnected
    Stop,
}

enum StreamerIpcMessage {
    ToPeer { peer_id: String, message: StreamServerMessage },  // Message for specific peer
    Broadcast(StreamServerMessage),  // Message for all peers
    StreamerReady { streamer_id: String },  // Streamer initialized
    MoonlightConnected,  // Moonlight stream active
    Stop,
}
```

### Broadcaster Cleanup

```rust
struct VideoBroadcaster {
    subscribers: RwLock<HashMap<String, mpsc::UnboundedSender<VideoFrame>>>,  // peer_id -> channel
}

impl VideoBroadcaster {
    async fn subscribe(&self, peer_id: String) -> mpsc::UnboundedReceiver<VideoFrame> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.write().await.insert(peer_id, tx);
        rx
    }

    async fn unsubscribe(&self, peer_id: &str) {
        self.subscribers.write().await.remove(peer_id);
    }

    async fn broadcast(&self, frame: VideoFrame) {
        let subs = self.subscribers.read().await;
        for (peer_id, tx) in subs.iter() {
            if let Err(_) = tx.try_send(frame.clone()) {
                warn!("Peer {peer_id} channel full, dropping frame");
            }
        }
    }
}
```

### Error Handling

**Moonlight stream fails**:
- Streamer detects stream termination
- Sends `Broadcast(PeerDisconnect)` to all WebRTC peers
- Web-server closes all peer WebSockets
- Terminates streamer process
- Removes from streamers map
- Clients get error, can create new streamer

**WebRTC peer fails** (connection timeout, network error):
- Web-server detects WebSocket close
- Sends `RemovePeer { peer_id }` to streamer
- Streamer removes from peers map
- Unsubscribes from video/audio broadcasters
- Calls `input_aggregator.remove_peer(peer_id)` to release inputs
- Other peers completely unaffected

**Last peer disconnects**:
- Moonlight stream continues (headless mode)
- Streamer process continues running
- Ready for new peers to join
- This is DESIRED behavior for external agents

**Streamer process crashes**:
- Web-server detects process exit
- Closes all peer WebSockets
- Removes from streamers map
- Clients get disconnected
- Can create new streamer to recover

**Web-server restarts**:
- All streamer processes are child processes
- Child processes automatically terminated on parent exit
- Streamers map cleared (in-memory only)
- Clients must recreate streamers
- Clean slate on restart

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
