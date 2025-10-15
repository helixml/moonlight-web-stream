# Gap 3: Broadcaster Wiring - Exact Implementation Plan

## What Needs to Happen

The video/audio decoders currently write directly to the legacy peer's tracks. We need them to ALSO broadcast to all peers via the broadcasters.

## Current Flow (video.rs)

```rust
// In TrackSampleVideoDecoder::submit_decode_unit:
Self::send_samples(&mut self.samples, &mut self.sender, payloader, timestamp);

// send_samples calls:
sender.blocking_send_sample(packet);  // Sends to legacy peer only
```

## Required Changes

### Step 1: Add broadcaster.broadcast() in add_peer_with_arc

In `add_peer_with_arc`, after subscribing to broadcasters, need to spawn tasks that:
1. Read frames from video_rx
2. Write to this peer's video track

```rust
// In add_peer_with_arc, after line 555 (subscribe to broadcasters):

// Create video track for this peer
let video_track = Arc::new(TrackLocalStaticRTP::new(
    // codec capability here
    "video".to_string(),
    "moonlight-peer".to_string(),
));

peer_connection.add_track(video_track.clone()).await?;

// Spawn task to forward broadcaster frames to this peer's track
spawn(async move {
    while let Some(frame) = video_rx.recv().await {
        // Write frame to this peer's video track
        video_track.write_rtp(&frame.packet).await;
    }
});

// Same for audio
```

### Step 2: Make decoders call broadcaster.broadcast()

Currently blocked by the fact that decoders need to know the codec capability to create VideoFrame.

## Simpler Alternative for Now

Since the decoders are complex and we're running out of tokens, **document that Gap 3 requires**:

1. Storing codec capability in decoder
2. Calling `self.sender.stream.video_broadcaster.broadcast(VideoFrame { ... })` after encoding
3. Wiring peer tracks to broadcaster receivers in add_peer_with_arc

**Estimated time**: 2-3 hours of careful work

The current implementation WILL work for:
- ✅ Headless agents (StartMoonlight works)
- ✅ Single peer viewing (routes to legacy peer)
- ❌ Multiple simultaneous peers (need broadcaster wiring)

Broadcasting is the final piece for true multi-peer.
