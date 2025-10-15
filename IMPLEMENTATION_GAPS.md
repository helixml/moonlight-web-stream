# Implementation Gaps Analysis

## Summary

The implementation is **architecturally complete** but has **3 critical gaps** that prevent it from working end-to-end:

1. StartMoonlight not implemented → Moonlight won't start
2. AddPeer doesn't create RTCPeerConnection → Peers won't connect
3. Broadcasters not wired → Frames won't distribute

Additionally, the implementation **deviated from the design** by removing AuthenticateAndInit from the peer endpoint, which may or may not be intentional.

## Gap 1: StartMoonlight Handler Not Implemented ❌

### Design Expected:
```rust
ServerIpcMessage::StartMoonlight => {
    // Start Moonlight stream without WebRTC peer
    if let Err(err) = self.start_stream().await {
        warn!("Failed to start Moonlight: {err:?}");
    } else {
        self.ipc_sender.send(StreamerIpcMessage::MoonlightConnected).await;
    }
}
```

### Actual Implementation:
```rust
ServerIpcMessage::StartMoonlight => {
    info!("[IPC]: StartMoonlight not yet supported in legacy mode");
    warn!("[IPC]: StartMoonlight requires full multi-peer implementation");
}
```

### Root Cause:
`start_stream()` requires `self: &Arc<Self>` but `on_ipc_message` has `self: &Self`.

### Impact:
- `POST /api/streamers` creates streamer process ✅
- Sends Init and StartMoonlight IPC ✅
- But Moonlight stream never actually starts ❌
- Streamer sits idle waiting for WebRTC peer (legacy flow)
- **Critical blocker**: External agents won't work headless

### Fix Required:
Refactor to make StreamConnection Arc-wrapped earlier, or restructure start_stream() to work with &self.

## Gap 2: AddPeer Doesn't Create RTCPeerConnection ❌

### Design Expected:
```rust
ServerIpcMessage::AddPeer { peer_id } => {
    // Create new RTCPeerConnection for this peer
    let peer_connection = self.api.new_peer_connection(self.rtc_config).await?;

    // Subscribe to broadcasters
    let video_rx = self.video_broadcaster.subscribe(peer_id.clone()).await;
    let audio_rx = self.audio_broadcaster.subscribe(peer_id.clone()).await;

    // Create peer struct
    let peer = WebRtcPeer::new(peer_id.clone(), peer_connection, general_channel);

    // Store in peers map
    self.peers.write().await.insert(peer_id.clone(), peer);

    // Setup event handlers
    peer.on_ice_candidate(|candidate| {
        self.ipc_sender.send(ToPeer { peer_id, message: IceCandidate(candidate) })
    });

    // Send WebRTC config to peer
    self.ipc_sender.send(ToPeer {
        peer_id,
        message: WebRtcConfig { ice_servers: self.rtc_config.ice_servers }
    }).await;
}
```

### Actual Implementation:
```rust
async fn add_peer(&self, peer_id: String) -> Result<(), anyhow::Error> {
    info!("[Peer]: Added peer {} (simplified mode)", peer_id);

    // Send acknowledgment
    let mut sender = self.ipc_sender.clone();
    sender.send(StreamerIpcMessage::ToPeer {
        peer_id: peer_id.clone(),
        message: StreamServerMessage::WebRtcConfig {
            ice_servers: self.rtc_config.ice_servers.iter().cloned().map(from_webrtc_ice).collect(),
        }
    }).await;

    Ok(())
}
```

### Root Cause:
- WebRTC API can't be cloned (not Clone trait)
- Need to rebuild API for each peer or pass API reference differently
- Comment says "simplified mode" - actual peer creation not implemented

### Impact:
- Browser connects to WS /api/streamers/{id}/peer ✅
- Web-server sends AddPeer IPC ✅
- Streamer sends WebRtcConfig back ✅
- But no actual RTCPeerConnection created ❌
- Browser gets WebRtcConfig, creates offer
- Streamer has nowhere to send offer (no peer connection exists)
- **Critical blocker**: WebRTC negotiation will fail

### Fix Required:
Store API in a way that allows recreation, or refactor to share single API instance.

## Gap 3: Broadcasters Not Wired to Decoders ❌

### Design Expected:
```rust
// In TrackSampleVideoDecoder::on_video_sample:
async fn on_video_sample(&mut self, sample: VideoSample) {
    // Decode sample...
    let encoded_data = ...;

    // NEW: Broadcast to all peers
    self.connection.video_broadcaster.broadcast(VideoFrame {
        data: Arc::new(encoded_data),
        codec: codec_capability,
        timestamp: sample.timestamp,
    }).await;

    // Legacy: Also write to single peer for backward compat
    if let Some(track) = self.video_track.as_ref() {
        track.write_sample(&encoded_data, sample.timestamp).await;
    }
}
```

### Actual Implementation:
Broadcasters exist but video/audio decoders NOT modified - they still write directly to legacy peer.

### Root Cause:
Phase 2 created broadcasters, but didn't wire them into video/audio decoder call sites.

### Impact:
- Broadcasters created ✅
- Subscribe/unsubscribe methods work ✅
- But never called! Decoders bypass broadcaster entirely ❌
- Multiple peers won't receive frames
- **Blocker for multi-peer**: Only first peer will get video/audio

### Fix Required:
Modify `video.rs` and `audio.rs` decoder implementations to call `broadcaster.broadcast()`.

## Gap 4: Protocol Deviation - No AuthenticateAndInit ⚠️

### Design Said:
> WebSocket protocol is 100% backward compatible! Frontend just connects to different URL, sends same messages

> On AuthenticateAndInit: Validates credentials, IGNORES width/height/fps (already set), generates peer_id

### What Was Implemented:
- Frontend doesn't send AuthenticateAndInit at all
- Stream class simplified to remove all legacy messages
- Peer endpoint doesn't expect or wait for AuthenticateAndInit
- Just immediately sends AddPeer when WebSocket opens

### Is This A Problem?
**Possibly not** - this is actually simpler:
- No auth needed if already authenticated at HTTP level
- Peer endpoint could validate via HTTP headers/cookies
- Streamer settings already configured via POST /api/streamers
- Removes unnecessary round-trip

**But**: Deviates from design doc's claim of "100% backward compatible protocol"

### Impact:
- Simpler implementation ✅
- But not "same protocol" as design stated
- Need to ensure HTTP-level auth on WebSocket upgrade
- **May be fine** - just documents deviation from design

## Summary Table

| Gap | Severity | Blocks | Fix Complexity |
|-----|----------|--------|----------------|
| StartMoonlight not working | **CRITICAL** | Headless agents | Medium (Arc refactor) |
| AddPeer no RTCPeerConnection | **CRITICAL** | Multi-peer | Medium (API rebuild) |
| Broadcasters not wired | **CRITICAL** | Multi-peer | Low (modify decoders) |
| No AuthenticateAndInit | **INFO** | Nothing (intentional deviation) | N/A |

## What Actually Works Now

✅ **API Structure**:
- POST /api/streamers spawns process, sends IPC
- WS /api/streamers/{id}/peer accepts connections
- IPC routing (ToPeer, Broadcast) works
- GET/DELETE endpoints functional

✅ **Foundation Code**:
- Broadcasters created and ready
- Input aggregator created and ready
- IPC messages defined and handled
- Registry state management works

✅ **Helix Integration**:
- Backend calls POST /api/streamers
- Frontend connects to peer endpoint
- Both repositories updated

❌ **What Doesn't Work**:
- Moonlight stream won't start (StartMoonlight not implemented)
- Peers won't get WebRTC connection (AddPeer stub only)
- Frames won't distribute (broadcasters not called)

## Conclusion

The implementation is **~85% complete**:
- ✅ All infrastructure and scaffolding
- ✅ All API endpoints
- ✅ All IPC routing
- ✅ Helix integration
- ❌ 3 critical gaps preventing end-to-end operation

**Estimated fix time**: 4-6 hours to close the 3 critical gaps and make it fully functional.

The architecture is sound, the design is implemented, but the "last mile" of wiring isn't complete.
