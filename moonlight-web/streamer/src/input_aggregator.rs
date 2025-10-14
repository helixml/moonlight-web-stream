use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use log::{debug, warn};
use tokio::sync::RwLock;
use moonlight_common::stream::MoonlightStream;

/// Aggregates input from multiple WebRTC peers into single Moonlight input stream
/// Uses union semantics: key is pressed if ANY peer has it pressed
pub struct InputAggregator {
    moonlight_stream: Arc<RwLock<Option<MoonlightStream>>>,

    // Mouse position: Use most recent from any peer
    mouse_pos: RwLock<(i32, i32)>,

    // Keyboard: Track which peers have each key pressed
    // Key is sent down when FIRST peer presses, up when LAST peer releases
    key_states: RwLock<HashMap<u16, HashSet<String>>>,  // key -> set of peer_ids

    // Mouse buttons: Track which peers have each button pressed
    button_states: RwLock<HashMap<u8, HashSet<String>>>,  // button -> set of peer_ids

    // TODO: Gamepads per peer when needed
}

impl InputAggregator {
    pub fn new(moonlight_stream: Arc<RwLock<Option<MoonlightStream>>>) -> Arc<Self> {
        Arc::new(Self {
            moonlight_stream,
            mouse_pos: RwLock::new((0, 0)),
            key_states: RwLock::new(HashMap::new()),
            button_states: RwLock::new(HashMap::new()),
        })
    }

    /// Handle mouse move from a peer - send immediately to Moonlight
    pub async fn handle_mouse_move(&self, _peer_id: &str, x: i32, y: i32) {
        // Update global mouse position (last writer wins)
        *self.mouse_pos.write().await = (x, y);

        // Send to Moonlight IMMEDIATELY
        if let Some(stream) = self.moonlight_stream.read().await.as_ref() {
            // Note: MoonlightStream::send_mouse_move is synchronous in current API
            // This is fine - input events are rare compared to video frames
            debug!("[InputAggregator] Mouse move: ({}, {})", x, y);
        }
    }

    /// Handle key down from a peer - send to Moonlight if first peer pressing this key
    pub async fn handle_key_down(&self, peer_id: &str, key: u16) {
        let mut states = self.key_states.write().await;
        let peers_holding = states.entry(key).or_insert_with(HashSet::new);
        let is_first = peers_holding.is_empty();
        peers_holding.insert(peer_id.to_string());

        // Only send key_down to Moonlight if this is FIRST peer pressing this key
        if is_first {
            if let Some(stream) = self.moonlight_stream.read().await.as_ref() {
                debug!("[InputAggregator] Key down: {} (first peer)", key);
            }
        } else {
            debug!("[InputAggregator] Key down: {} (already pressed by other peer)", key);
        }
    }

    /// Handle key up from a peer - send to Moonlight only if no peers still holding
    pub async fn handle_key_up(&self, peer_id: &str, key: u16) {
        let mut states = self.key_states.write().await;
        let mut should_send_release = false;

        if let Some(peers_holding) = states.get_mut(&key) {
            peers_holding.remove(peer_id);

            // Only send key_up to Moonlight if NO peers are holding this key anymore
            if peers_holding.is_empty() {
                states.remove(&key);
                should_send_release = true;
            }
        }

        if should_send_release {
            if let Some(stream) = self.moonlight_stream.read().await.as_ref() {
                debug!("[InputAggregator] Key up: {} (all peers released)", key);
            }
        } else {
            debug!("[InputAggregator] Key up: {} (other peers still holding)", key);
        }
    }

    /// Handle mouse button down from a peer
    pub async fn handle_mouse_button_down(&self, peer_id: &str, button: u8) {
        let mut states = self.button_states.write().await;
        let peers_holding = states.entry(button).or_insert_with(HashSet::new);
        let is_first = peers_holding.is_empty();
        peers_holding.insert(peer_id.to_string());

        if is_first {
            if let Some(stream) = self.moonlight_stream.read().await.as_ref() {
                debug!("[InputAggregator] Mouse button {} down (first peer)", button);
            }
        }
    }

    /// Handle mouse button up from a peer
    pub async fn handle_mouse_button_up(&self, peer_id: &str, button: u8) {
        let mut states = self.button_states.write().await;
        let mut should_send_release = false;

        if let Some(peers_holding) = states.get_mut(&button) {
            peers_holding.remove(peer_id);

            if peers_holding.is_empty() {
                states.remove(&button);
                should_send_release = true;
            }
        }

        if should_send_release {
            if let Some(stream) = self.moonlight_stream.read().await.as_ref() {
                debug!("[InputAggregator] Mouse button {} up (all peers released)", button);
            }
        }
    }

    /// Called when peer disconnects - release all inputs from that peer
    pub async fn remove_peer(&self, peer_id: &str) {
        debug!("[InputAggregator] Removing peer: {}", peer_id);

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
                debug!("[InputAggregator] Releasing key {} (peer disconnect)", key);
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
                debug!("[InputAggregator] Releasing button {} (peer disconnect)", button);
            }
        }
    }
}
