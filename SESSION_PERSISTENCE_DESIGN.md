# Moonlight-Web Session Persistence Design

## Problem Statement

Current moonlight-web architecture has a 1:1:1 coupling:
- 1 WebSocket connection → 1 streamer process → 1 Moonlight session

**Critical Issues:**
1. **Zero-client support required**: External AI agents need to run autonomously before any user connects
2. **Wolf lobby unreliability**: Multi-client Wolf lobbies are unstable and being reverted to single-client apps
3. **No reconnection**: When WebSocket disconnects, streamer dies and Moonlight session is lost

**Required Behavior:**
- Autonomous agents run with zero WebRTC clients connected
- Users can connect/disconnect/reconnect without killing the Moonlight session
- Last client wins: new client kicks out previous client if one exists
- Support exactly 0 or 1 WebRTC clients per Moonlight session

## Architecture Overview

### Current Flow (1:1:1)
```
WebSocket connects → Spawn streamer → Connect to Wolf → Create WebRTC peer
WebSocket disconnects → Kill streamer → Moonlight session dies
```

### New Flow (Persistent Sessions)
```
First WebSocket (keepalive):
  → Create session + spawn streamer → Connect to Wolf → No WebRTC peer (discard mode)
  → WebSocket disconnects → Streamer STAYS ALIVE → Session persists

User connects (join mode):
  → Lookup existing session → Create WebRTC peer → Attach to streamer
  → Frames from Wolf → WebRTC → Browser

User disconnects:
  → Close WebRTC peer → Streamer back to discard mode → Session persists

Second user connects:
  → Close old WebRTC peer (kick first user) → Create new WebRTC peer → Attach
```

## Key Design Decisions

### 1. Session Identification
**Session ID maps to Wolf app/lobby identifier:**
- External agents: `"agent-{helix_session_id}"` → Wolf app "Agent xyz"
- PDEs: `"pde-{pde_id}"` → Wolf app "PDE:name"
- Spec tasks: `"spec-{spec_task_id}"` → Wolf app "Spec xyz"

**1:1 mapping:** Helix session ↔ Wolf app ↔ Moonlight-web session

### 2. Session Modes

```rust
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum SessionMode {
    Create,    // Create new session (fail if exists)
    Keepalive, // Create if missing, or join existing without WebRTC peer
    Join,      // Join existing session, kick any connected client
}
```

### 3. WebRTC Peer Lifecycle Separation

**Key insight:** Moonlight connection and WebRTC peer are already independent!

```rust
struct StreamConnection {
    moonlight: MoonlightInstance,           // Persistent to Wolf
    stream: RwLock<Option<MoonlightStream>>, // Lives across WebRTC changes
    peer: Arc<RTCPeerConnection>,           // Ephemeral, swappable
}
```

**Change:** Make `peer` optional and rebuildable:
```rust
struct StreamConnection {
    moonlight: MoonlightInstance,
    stream: RwLock<Option<MoonlightStream>>,
    peer: RwLock<Option<Arc<RTCPeerConnection>>>,  // None = discard mode
}
```

### 4. Discard Mode (Zero Clients)

When no WebRTC peer exists:
- Moonlight still receives frames from Wolf
- `submit_decode_unit()` is called by Moonlight
- Video decoder just returns `DecodeResult::Ok` without sending anywhere
- **No CPU/GPU waste** - frames are already compressed, just drop them

## Implementation Plan

### Phase 1: API Message Changes (30 lines)

**File: `moonlight-web/common/src/api_bindings.rs`**

```rust
// Add SessionMode enum (lines 204-210)
#[derive(Serialize, Deserialize, Debug, TS, Clone, Copy, PartialEq)]
#[ts(export, export_to = EXPORT_PATH)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    Create,
    Keepalive,
    Join,
}

// Modify AuthenticateAndInit (lines 240-257)
pub enum StreamClientMessage {
    AuthenticateAndInit {
        credentials: String,
        session_id: String,    // NEW: session identifier
        mode: SessionMode,     // NEW: how to handle existing sessions
        host_id: u32,
        app_id: u32,
        // ... rest unchanged
    },
    Signaling(StreamSignalingMessage),
}

// Add ClientKicked message (line 311)
pub enum StreamServerMessage {
    // ... existing variants
    ClientKicked,  // NEW: notify client they were kicked
}
```

### Phase 2: Session Storage (40 lines)

**File: `moonlight-web/web-server/src/data.rs`**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub struct StreamSession {
    pub session_id: String,
    pub streamer: Mutex<Child>,
    pub ipc_sender: Mutex<IpcSender<ServerIpcMessage>>,
    pub websocket: Mutex<Option<Session>>,  // None in keepalive mode
    pub mode: SessionMode,
}

pub struct RuntimeApiData {
    pub hosts: Arc<RwLock<Vec<Mutex<HostData>>>>,
    pub sessions: Arc<RwLock<HashMap<String, Arc<StreamSession>>>>,  // NEW
}

impl RuntimeApiData {
    pub fn new() -> Self {
        Self {
            hosts: Arc::new(RwLock::new(Vec::new())),
            sessions: Arc::new(RwLock::new(HashMap::new())),  // NEW
        }
    }
}
```

### Phase 3: Stream Handler Refactor (150 lines)

**File: `moonlight-web/web-server/src/api/stream.rs`**

Main changes:
1. **Session lookup/create logic** (~40 lines)
2. **WebSocket takeover** (~20 lines)
3. **IPC forwarding helper** (~30 lines)
4. **Cleanup logic** (~20 lines)
5. **Error handling for all modes** (~40 lines)

See detailed pseudocode in previous message.

### Phase 4: Streamer Peer Refactor (OPTIONAL - 50 lines)

**File: `moonlight-web/streamer/src/main.rs`**

Make WebRTC peer optional for true discard mode:

```rust
struct StreamConnection {
    peer: RwLock<Option<Arc<RTCPeerConnection>>>,  // None = discard
    // ... rest
}

impl VideoDecoder for TrackSampleVideoDecoder {
    fn submit_decode_unit(&mut self, unit: VideoDecodeUnit<'_>) -> DecodeResult {
        // If no WebRTC peer attached, just discard
        if self.sender.stream.peer.blocking_read().is_none() {
            return DecodeResult::Ok;  // Discard frame
        }

        // Otherwise process and send as normal
        // ... existing code
    }
}
```

**Note:** This is optional - even without this change, the WebRTC peer will just not have anyone listening, which is fine. The main benefit is cleaner architecture.

## Implementation Order

1. ✅ API bindings (30 lines) - Add SessionMode, session_id, ClientKicked
2. ✅ Data storage (40 lines) - Add sessions HashMap to RuntimeApiData
3. ✅ Stream handler (150 lines) - Implement session lookup/create/join/kick logic
4. ⏸️ Streamer refactor (50 lines) - OPTIONAL: Make peer truly optional for discard mode
5. ✅ Helix integration (50 lines) - Update keepalive code to use new session API

**Total: 220-270 lines depending on optional streamer changes**

## Testing Strategy

### Phase 1: Basic Session Creation
```bash
# Start keepalive session
wscat -c ws://localhost:8080/api/host/stream
> {"type":"AuthenticateAndInit","session_id":"test-1","mode":"keepalive",...}

# Verify streamer spawned and Wolf session created
curl http://wolf:47989/serverinfo

# Disconnect wscat
# Verify streamer STAYS ALIVE (ps aux | grep streamer)
# Verify Wolf session PERSISTS
```

### Phase 2: User Join/Kick
```bash
# Join with browser WebRTC client
# Send: {"session_id":"test-1","mode":"join",...}
# Verify video streams to browser

# Join with second browser tab
# Send same session_id with mode=join
# Verify first tab kicked (receives ClientKicked message)
# Verify second tab receives video
```

### Phase 3: Helix Integration
```bash
# Create external agent via Helix
# Verify keepalive establishes with mode=keepalive
# User clicks "Stream" button
# Verify user connects with mode=join
# Verify user receives video
# Disconnect user
# Verify keepalive persists
```

## Security Considerations

1. **Session ID Authorization**: Helix must validate user can access session_id before generating streaming tokens
2. **Credential Validation**: Existing credentials check still applies to all connections
3. **Kick Protection**: Only connections with valid credentials can kick existing clients
4. **No new attack surface**: Session IDs are Helix-generated UUIDs, not user-controlled

## Backward Compatibility

**Breaking change:** Requires updating `AuthenticateAndInit` message schema.

**Migration path:**
1. Update moonlight-web first
2. Update Helix keepalive code to use new schema
3. Update browser clients to use new schema
4. Old clients will fail with deserialization error (graceful failure)

## Success Criteria

✅ External agent runs with zero WebRTC clients
✅ User can connect to existing session
✅ Second user kicks first user
✅ User disconnect doesn't kill agent session
✅ Moonlight session to Wolf persists across all WebSocket/WebRTC changes
✅ No Wolf lobbies required

## Open Questions

1. **Session cleanup**: When should sessions be garbage collected?
   - Option A: Explicit DELETE API call from Helix
   - Option B: Timeout after N hours of inactivity
   - Option C: Never (until moonlight-web restart)

2. **Partial failures**: What if streamer crashes but session object remains?
   - Add health check in session object
   - Reconciliation loop to clean dead sessions

3. **Multiple apps per session**: Should one session support switching apps?
   - No - keep it simple, 1 session = 1 Wolf app
   - To switch apps, create new session
