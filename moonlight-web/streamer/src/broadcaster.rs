use std::collections::HashMap;
use std::sync::Arc;
use log::warn;
use tokio::sync::{RwLock, mpsc};
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;

/// Video frame wrapper for broadcasting
#[derive(Clone)]
pub struct VideoFrame {
    pub data: Arc<Vec<u8>>,
    pub codec: RTCRtpCodecCapability,
    pub timestamp: u32,
}

/// Audio sample wrapper for broadcasting
#[derive(Clone)]
pub struct AudioSample {
    pub data: Arc<Vec<u8>>,
    pub timestamp: u32,
}

/// Broadcasts video frames to multiple WebRTC peers without blocking
pub struct VideoBroadcaster {
    subscribers: RwLock<HashMap<String, mpsc::UnboundedSender<VideoFrame>>>,
}

impl VideoBroadcaster {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            subscribers: RwLock::new(HashMap::new()),
        })
    }

    /// Subscribe a peer to receive video frames
    pub async fn subscribe(&self, peer_id: String) -> mpsc::UnboundedReceiver<VideoFrame> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.write().await.insert(peer_id, tx);
        rx
    }

    /// Unsubscribe a peer (called on disconnect)
    pub async fn unsubscribe(&self, peer_id: &str) {
        self.subscribers.write().await.remove(peer_id);
    }

    /// Broadcast a frame to all subscribers (non-blocking)
    pub async fn broadcast(&self, frame: VideoFrame) {
        let subs = self.subscribers.read().await;
        for (peer_id, tx) in subs.iter() {
            // UnboundedSender::send is always non-blocking
            // If channel is closed (peer disconnected), ignore error
            if let Err(_) = tx.send(frame.clone()) {
                warn!("[Broadcaster] Peer {} video channel closed", peer_id);
            }
        }
    }

    /// Get current subscriber count
    pub async fn subscriber_count(&self) -> usize {
        self.subscribers.read().await.len()
    }
}

/// Broadcasts audio samples to multiple WebRTC peers without blocking
pub struct AudioBroadcaster {
    subscribers: RwLock<HashMap<String, mpsc::UnboundedSender<AudioSample>>>,
}

impl AudioBroadcaster {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            subscribers: RwLock::new(HashMap::new()),
        })
    }

    /// Subscribe a peer to receive audio samples
    pub async fn subscribe(&self, peer_id: String) -> mpsc::UnboundedReceiver<AudioSample> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.write().await.insert(peer_id, tx);
        rx
    }

    /// Unsubscribe a peer (called on disconnect)
    pub async fn unsubscribe(&self, peer_id: &str) {
        self.subscribers.write().await.remove(peer_id);
    }

    /// Broadcast a sample to all subscribers (non-blocking)
    pub async fn broadcast(&self, sample: AudioSample) {
        let subs = self.subscribers.read().await;
        for (peer_id, tx) in subs.iter() {
            // UnboundedSender::send is always non-blocking
            // If channel is closed (peer disconnected), ignore error
            if let Err(_) = tx.send(sample.clone()) {
                warn!("[Broadcaster] Peer {} audio channel closed", peer_id);
            }
        }
    }

    /// Get current subscriber count
    pub async fn subscriber_count(&self) -> usize {
        self.subscribers.read().await.len()
    }
}
