use std::sync::Arc;
use tokio::sync::Mutex;
use webrtc::{
    data_channel::RTCDataChannel,
    peer_connection::RTCPeerConnection,
};

/// Represents a single WebRTC peer connection
/// Each browser connection gets its own WebRtcPeer
pub struct WebRtcPeer {
    pub peer_id: String,
    pub connection: Arc<RTCPeerConnection>,
    pub general_channel: Arc<RTCDataChannel>,

    // Statistics and lifecycle
    pub created_at: std::time::Instant,
}

impl WebRtcPeer {
    pub fn new(
        peer_id: String,
        connection: Arc<RTCPeerConnection>,
        general_channel: Arc<RTCDataChannel>,
    ) -> Arc<Self> {
        Arc::new(Self {
            peer_id,
            connection,
            general_channel,
            created_at: std::time::Instant::now(),
        })
    }
}
