use std::{panic, process::exit, str::FromStr, sync::Arc};

use common::{
    StreamSettings,
    api_bindings::StreamServerGeneralMessage,
    config::PortRange,
    ipc::{IpcReceiver, IpcSender, ServerIpcMessage, StreamerIpcMessage, create_process_ipc},
    serialize_json,
};
use log::{LevelFilter, debug, info, warn};
use moonlight_common::{
    MoonlightError,
    high::HostError,
    network::reqwest::ReqwestMoonlightHost,
    pair::ClientAuth,
    stream::{
        MoonlightInstance, MoonlightStream,
        bindings::{ColorRange, HostFeatures, SupportedVideoFormats},
    },
};
use pem::Pem;
use simplelog::{ColorChoice, TermLogger, TerminalMode};
use std::collections::HashMap;
use tokio::{
    io::{stdin, stdout},
    runtime::Handle,
    spawn,
    sync::{Mutex, Notify, RwLock},
    task::spawn_blocking,
};
use webrtc::{
    api::{
        API, APIBuilder, interceptor_registry::register_default_interceptors,
        media_engine::MediaEngine, setting_engine::SettingEngine,
    },
    data_channel::RTCDataChannel,
    ice::udp_network::{EphemeralUDP, UDPNetwork},
    ice_transport::{
        ice_candidate::{RTCIceCandidate, RTCIceCandidateInit},
        ice_connection_state::RTCIceConnectionState,
    },
    interceptor::registry::Registry,
    peer_connection::{
        RTCPeerConnection,
        configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::{sdp_type::RTCSdpType, session_description::RTCSessionDescription},
    },
    track::track_local::{TrackLocal, TrackLocalWriter},
};

use common::api_bindings::{
    RtcIceCandidate, RtcSdpType, RtcSessionDescription, StreamCapabilities, StreamClientMessage,
    StreamServerMessage, StreamSignalingMessage,
};

use crate::{
    audio::{OpusTrackSampleAudioDecoder, register_audio_codecs},
    connection::StreamConnectionListener,
    convert::{
        from_webrtc_ice, from_webrtc_sdp, into_webrtc_ice, into_webrtc_ice_candidate,
        into_webrtc_network_type,
    },
    input::StreamInput,
    video::{TrackSampleVideoDecoder, register_video_codecs},
};

mod audio;
mod broadcaster;
mod buffer;
mod connection;
mod convert;
mod input;
mod input_aggregator;
mod peer;
mod sender;
mod video;

#[tokio::main]
async fn main() {
    eprintln!("🎬 [Streamer] STARTING UP - main() entry point");

    #[cfg(debug_assertions)]
    let log_level = LevelFilter::Debug;
    #[cfg(not(debug_assertions))]
    let log_level = LevelFilter::Info;

    eprintln!("🎬 [Streamer] Initializing logger...");
    TermLogger::init(
        log_level,
        simplelog::Config::default(),
        TerminalMode::Stderr,
        ColorChoice::Auto,
    )
    .expect("failed to init logger");
    eprintln!("🎬 [Streamer] Logger initialized");

    eprintln!("🎬 [Streamer] Setting up panic hook...");
    let default_panic = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        eprintln!("💥 [Streamer] PANIC: {:?}", info);
        default_panic(info);
        exit(0);
    }));
    eprintln!("🎬 [Streamer] Panic hook set");

    // At this point we're authenticated
    eprintln!("🎬 [Streamer] Creating IPC channels with stdin/stdout...");
    let (mut ipc_sender, mut ipc_receiver) =
        create_process_ipc::<ServerIpcMessage, StreamerIpcMessage>(stdin(), stdout()).await;
    eprintln!("🎬 [Streamer] IPC channels created successfully");

    // Send stage
    eprintln!("🎬 [Streamer] Sending StageComplete message...");
    ipc_sender
        .send(StreamerIpcMessage::WebSocket(
            StreamServerMessage::StageComplete {
                stage: "Launch Streamer".to_string(),
            },
        ))
        .await;
    eprintln!("🎬 [Streamer] StageComplete sent, now waiting for Init message...");

    let (
        server_config,
        stream_settings,
        host_address,
        host_http_port,
        host_unique_id,
        client_private_key_pem,
        client_certificate_pem,
        server_certificate_pem,
        app_id,
    ) = loop {
        eprintln!("🎬 [Streamer] Calling ipc_receiver.recv()...");
        match ipc_receiver.recv().await {
            Some(ServerIpcMessage::Init {
                server_config,
                stream_settings,
                host_address,
                host_http_port,
                host_unique_id,
                client_private_key_pem,
                client_certificate_pem,
                server_certificate_pem,
                app_id,
            }) => {
                eprintln!("🎬 [Streamer] Received Init message!");
                debug!(
                    "Client supported codecs: {:?}",
                    stream_settings
                        .video_supported_formats
                        .iter_names()
                        .collect::<Vec<_>>()
                );
                eprintln!("🎬 [Streamer] Breaking out of Init wait loop...");

                break (
                    server_config,
                    stream_settings,
                    host_address,
                    host_http_port,
                    host_unique_id,
                    client_private_key_pem,
                    client_certificate_pem,
                    server_certificate_pem,
                    app_id,
                );
            }
            None => {
                eprintln!("🎬 [Streamer] ERROR: recv() returned None - parent closed stdin!");
                continue;
            }
            _ => {
                eprintln!("🎬 [Streamer] Received non-Init message, continuing loop...");
                continue;
            }
        }
    };
    eprintln!("🎬 [Streamer] Init received and processed, continuing to WebRTC setup...");

    // Send stage
    eprintln!("🎬 [Streamer] Sending StageStarting message...");
    ipc_sender
        .send(StreamerIpcMessage::WebSocket(
            StreamServerMessage::StageStarting {
                stage: "Setup WebRTC Peer".to_string(),
            },
        ))
        .await;
    eprintln!("🎬 [Streamer] StageStarting sent");

    // -- Create the host and pair it
    eprintln!("🎬 [Streamer] Creating ReqwestMoonlightHost with address={}, port={}", host_address, host_http_port);
    let mut host = ReqwestMoonlightHost::new(host_address.clone(), host_http_port, host_unique_id)
        .expect(&format!("failed to create host at {}:{}", host_address, host_http_port));
    eprintln!("🎬 [Streamer] ReqwestMoonlightHost created");

    eprintln!("🎬 [Streamer] Setting pairing info...");
    host.set_pairing_info(
        &ClientAuth {
            private_key: Pem::from_str(&client_private_key_pem)
                .expect("failed to parse client private key"),
            certificate: Pem::from_str(&client_certificate_pem)
                .expect("failed to parse client certificate"),
        },
        &Pem::from_str(&server_certificate_pem).expect("failed to parse server certificate"),
    )
    .expect("failed to set pairing info");
    eprintln!("🎬 [Streamer] Pairing info set");

    // -- Configure moonlight
    eprintln!("🎬 [Streamer] Getting MoonlightInstance...");
    let moonlight = MoonlightInstance::global().expect("failed to find moonlight");
    eprintln!("🎬 [Streamer] MoonlightInstance obtained");

    // -- Configure WebRTC
    let rtc_config = RTCConfiguration {
        ice_servers: server_config
            .webrtc_ice_servers
            .clone()
            .into_iter()
            .map(into_webrtc_ice)
            .collect(),
        ..Default::default()
    };
    let mut api_settings = SettingEngine::default();

    if let Some(PortRange { min, max }) = server_config.webrtc_port_range {
        match EphemeralUDP::new(min, max) {
            Ok(udp) => {
                api_settings.set_udp_network(UDPNetwork::Ephemeral(udp));
            }
            Err(err) => {
                warn!("[Stream]: Invalid port range in config: {err:?}");
            }
        }
    }
    if let Some(mapping) = server_config.webrtc_nat_1to1 {
        api_settings.set_nat_1to1_ips(
            mapping.ips.clone(),
            into_webrtc_ice_candidate(mapping.ice_candidate_type),
        );
    }
    api_settings.set_network_types(
        server_config
            .webrtc_network_types
            .iter()
            .copied()
            .map(into_webrtc_network_type)
            .collect(),
    );

    // -- Register media codecs
    let mut api_media = MediaEngine::default();
    register_audio_codecs(&mut api_media).expect("failed to register audio codecs");
    register_video_codecs(&mut api_media, stream_settings.video_supported_formats)
        .expect("failed to register video codecs");

    // -- Build Api
    let mut api_registry = Registry::new();

    // Use the default set of Interceptors
    api_registry = register_default_interceptors(api_registry, &mut api_media)
        .expect("failed to register webrtc default interceptors");

    let api_settings_arc = Arc::new(api_settings.clone());

    eprintln!("🎬 [Streamer] Building WebRTC API...");
    let api = APIBuilder::new()
        .with_setting_engine(api_settings)
        .with_media_engine(api_media)
        .with_interceptor_registry(api_registry)
        .build();
    eprintln!("🎬 [Streamer] WebRTC API built");

    // -- Create and Configure Peer
    eprintln!("🎬 [Streamer] Creating StreamConnection...");
    let connection = StreamConnection::new(
        moonlight,
        StreamInfo {
            host: Mutex::new(host),
            app_id,
        },
        stream_settings,
        ipc_sender.clone(),
        ipc_receiver,
        &api,
        rtc_config,
        api_settings_arc,
    )
    .await
    .expect("failed to create connection");
    eprintln!("🎬 [Streamer] StreamConnection created successfully!");

    // Send stage
    eprintln!("🎬 [Streamer] Sending StageComplete (Setup WebRTC Peer)...");
    ipc_sender
        .send(StreamerIpcMessage::WebSocket(
            StreamServerMessage::StageComplete {
                stage: "Setup WebRTC Peer".to_string(),
            },
        ))
        .await;

    // Send stage
    eprintln!("🎬 [Streamer] Sending StageStarting (WebRTC Peer Negotiation)...");
    ipc_sender
        .send(StreamerIpcMessage::WebSocket(
            StreamServerMessage::StageStarting {
                stage: "WebRTC Peer Negotiation".to_string(),
            },
        ))
        .await;
    eprintln!("🎬 [Streamer] All initialization complete, waiting for termination signal...");

    // Wait for termination
    connection.terminate.notified().await;

    eprintln!("🎬 [Streamer] Termination signal received, exiting...");
    // Exit streamer
    exit(0);
}

struct StreamInfo {
    host: Mutex<ReqwestMoonlightHost>,
    app_id: u32,
}

struct StreamConnection {
    pub runtime: Handle,
    pub moonlight: MoonlightInstance,
    pub info: StreamInfo,
    pub settings: StreamSettings,

    // Legacy: Single peer for backward compatibility
    pub peer: Arc<RTCPeerConnection>,
    pub general_channel: Arc<RTCDataChannel>,

    // New: Multi-peer support (Phase 1 preparation)
    pub peers: RwLock<HashMap<String, Arc<peer::WebRtcPeer>>>,
    pub rtc_config: RTCConfiguration,

    // For recreating API for new peers (API is not Clone)
    pub api_settings: Arc<SettingEngine>,
    pub supported_video_formats: SupportedVideoFormats,

    // Track which peer we're currently handling (for routing responses)
    pub current_peer_id: RwLock<Option<String>>,

    // Media broadcasting (Phase 2)
    pub video_broadcaster: Arc<broadcaster::VideoBroadcaster>,
    pub audio_broadcaster: Arc<broadcaster::AudioBroadcaster>,

    // Input aggregation (Phase 3)
    pub input_aggregator: Arc<input_aggregator::InputAggregator>,

    // IPC
    pub ipc_sender: IpcSender<StreamerIpcMessage>,

    // Legacy: Input handler for backward compatibility
    pub input: StreamInput,
    // Video
    pub video_size: Mutex<(u32, u32)>,
    // Stream (Arc wrapper for sharing with input_aggregator)
    pub stream: Arc<RwLock<Option<MoonlightStream>>>,
    pub terminate: Notify,
}

impl StreamConnection {
    pub async fn new(
        moonlight: MoonlightInstance,
        info: StreamInfo,
        settings: StreamSettings,
        mut ipc_sender: IpcSender<StreamerIpcMessage>,
        mut ipc_receiver: IpcReceiver<ServerIpcMessage>,
        api: &API,
        config: RTCConfiguration,
        api_settings: Arc<SettingEngine>,
    ) -> Result<Arc<Self>, anyhow::Error> {
        // Send WebRTC Info
        ipc_sender
            .send(StreamerIpcMessage::WebSocket(
                StreamServerMessage::WebRtcConfig {
                    ice_servers: config
                        .ice_servers
                        .iter()
                        .cloned()
                        .map(from_webrtc_ice)
                        .collect(),
                },
            ))
            .await;

        let peer = Arc::new(api.new_peer_connection(config.clone()).await?);

        // -- Input
        let input = StreamInput::new();

        let general_channel = peer.create_data_channel("general", None).await?;

        // Create shared stream reference for input aggregator
        let stream = Arc::new(RwLock::new(None));

        let supported_formats = settings.video_supported_formats;

        let this = Arc::new(Self {
            runtime: Handle::current(),
            moonlight,
            info,
            settings,
            peer: peer.clone(),
            general_channel,
            peers: RwLock::new(HashMap::new()),
            rtc_config: config.clone(),
            api_settings,
            supported_video_formats: supported_formats,
            current_peer_id: RwLock::new(None),
            video_broadcaster: broadcaster::VideoBroadcaster::new(),
            audio_broadcaster: broadcaster::AudioBroadcaster::new(),
            input_aggregator: input_aggregator::InputAggregator::new(stream.clone()),
            ipc_sender,
            video_size: Mutex::new((0, 0)),
            input,
            stream,
            terminate: Notify::new(),
        });

        // -- Connection state
        peer.on_ice_connection_state_change({
            let this = this.clone();
            Box::new(move |state| {
                let this = this.clone();
                Box::pin(async move {
                    this.on_ice_connection_state_change(state).await;
                })
            })
        });
        peer.on_peer_connection_state_change({
            let this = this.clone();
            Box::new(move |state| {
                let this = this.clone();
                Box::pin(async move {
                    this.on_peer_connection_state_change(state).await;
                })
            })
        });

        // -- Signaling
        peer.on_negotiation_needed({
            let this = this.clone();
            Box::new(move || {
                let this = this.clone();
                Box::pin(async move {
                    this.on_negotiation_needed().await;
                })
            })
        });
        peer.on_ice_candidate({
            let this = this.clone();
            Box::new(move |candidate| {
                let this = this.clone();
                Box::pin(async move {
                    this.on_ice_candidate(candidate).await;
                })
            })
        });

        spawn({
            let this = this.clone();

            async move {
                while let Some(message) = ipc_receiver.recv().await {
                    match message {
                        ServerIpcMessage::Stop => {
                            this.on_ipc_message(ServerIpcMessage::Stop).await;
                            return;
                        }
                        ServerIpcMessage::StartMoonlight => {
                            // Handle StartMoonlight here where we have Arc<Self>
                            info!("[IPC]: Starting Moonlight stream (headless mode)");
                            if let Err(err) = this.start_stream().await {
                                warn!("[IPC]: Failed to start Moonlight: {err:?}");
                            } else {
                                let mut sender = this.ipc_sender.clone();
                                sender.send(StreamerIpcMessage::MoonlightConnected).await;
                            }
                        }
                        ServerIpcMessage::AddPeer { peer_id } => {
                            // Handle AddPeer here where we can access Arc fields
                            if let Err(err) = this.add_peer_with_arc(peer_id).await {
                                warn!("[IPC]: Failed to add peer: {err:?}");
                            }
                        }
                        _ => {
                            this.on_ipc_message(message).await;
                        }
                    }
                }
            }
        });

        // -- Data Channels
        peer.on_data_channel({
            let this = this.clone();
            Box::new(move |channel| {
                let this = this.clone();
                Box::pin(async move {
                    this.on_data_channel(channel).await;
                })
            })
        });

        Ok(this)
    }

    // -- Handle Connection State
    async fn on_ice_connection_state_change(self: &Arc<Self>, state: RTCIceConnectionState) {
        #[allow(clippy::collapsible_if)]
        if matches!(state, RTCIceConnectionState::Connected) {
            if let Err(err) = self.start_stream().await {
                warn!("[Stream]: failed to start stream: {err:?}");

                self.stop().await;
            }
        }
    }
    async fn on_peer_connection_state_change(&self, state: RTCPeerConnectionState) {
        if matches!(
            state,
            RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Disconnected
                | RTCPeerConnectionState::Closed
        ) {
            self.stop().await;
        }
    }

    // -- Handle Signaling
    async fn on_negotiation_needed(&self) {
        // Do nothing
    }

    async fn send_answer(&self) -> bool {
        let peer_id = self.current_peer_id.read().await.clone();
        self.send_answer_to_peer(peer_id).await
    }

    async fn send_answer_to_peer(&self, peer_id: Option<String>) -> bool {
        let local_description = match self.peer.create_answer(None).await {
            Err(err) => {
                warn!("[Signaling]: failed to create answer: {err:?}");
                return false;
            }
            Ok(value) => value,
        };

        if let Err(err) = self
            .peer
            .set_local_description(local_description.clone())
            .await
        {
            warn!("[Signaling]: failed to set local description: {err:?}");
            return false;
        }

        debug!(
            "[Signaling] Sending Local Description as Answer: {:?}",
            local_description.sdp
        );

        let message = StreamServerMessage::Signaling(StreamSignalingMessage::Description(
            RtcSessionDescription {
                ty: from_webrtc_sdp(local_description.sdp_type),
                sdp: local_description.sdp,
            },
        ));

        // Send via ToPeer if peer_id provided, otherwise Broadcast (legacy)
        self.ipc_sender
            .clone()
            .send(if let Some(peer_id) = peer_id {
                StreamerIpcMessage::ToPeer { peer_id, message }
            } else {
                StreamerIpcMessage::Broadcast(message)
            })
            .await;

        true
    }
    async fn send_offer(&self) -> bool {
        let local_description = match self.peer.create_offer(None).await {
            Err(err) => {
                warn!("[Signaling]: failed to create offer: {err:?}");
                return false;
            }
            Ok(value) => value,
        };

        if let Err(err) = self
            .peer
            .set_local_description(local_description.clone())
            .await
        {
            warn!("[Signaling]: failed to set local description: {err:?}");
            return false;
        }

        debug!(
            "[Signaling] Sending Local Description as Offer: {:?}",
            local_description.sdp
        );

        let message = StreamServerMessage::Signaling(StreamSignalingMessage::Description(
            RtcSessionDescription {
                ty: from_webrtc_sdp(local_description.sdp_type),
                sdp: local_description.sdp,
            },
        ));

        let peer_id = self.current_peer_id.read().await.clone();
        self.ipc_sender
            .clone()
            .send(if let Some(peer_id) = peer_id {
                StreamerIpcMessage::ToPeer { peer_id, message }
            } else {
                StreamerIpcMessage::Broadcast(message)
            })
            .await;

        true
    }

    async fn on_ipc_message(&self, message: ServerIpcMessage) {
        match message {
            ServerIpcMessage::Init { .. } => {}
            ServerIpcMessage::WebSocket(message) => {
                self.on_ws_message(message).await;
            }
            ServerIpcMessage::StartMoonlight => {
                // Handled in IPC loop where Arc<Self> is available
            }
            ServerIpcMessage::AddPeer { .. } => {
                // Handled in IPC loop where Arc<Self> is available
            }
            ServerIpcMessage::FromPeer { peer_id, message } => {
                self.on_peer_message(peer_id, message).await;
            }
            ServerIpcMessage::RemovePeer { peer_id } => {
                info!("[IPC]: Removing peer {}", peer_id);
                self.remove_peer(&peer_id).await;
            }
            ServerIpcMessage::Stop => {
                self.stop().await;
            }
        }
    }
    async fn on_ws_message(&self, message: StreamClientMessage) {
        match message {
            StreamClientMessage::Signaling(StreamSignalingMessage::Description(description)) => {
                debug!("[Signaling] Received Remote Description: {:?}", description);

                let description = match &description.ty {
                    RtcSdpType::Offer => RTCSessionDescription::offer(description.sdp),
                    RtcSdpType::Answer => RTCSessionDescription::answer(description.sdp),
                    RtcSdpType::Pranswer => RTCSessionDescription::pranswer(description.sdp),
                    _ => {
                        warn!(
                            "[Signaling]: failed to handle RTCSdpType {:?}",
                            description.ty
                        );
                        return;
                    }
                };

                let Ok(description) = description else {
                    warn!("[Signaling]: Received invalid RTCSessionDescription");
                    return;
                };

                let remote_ty = description.sdp_type;
                if let Err(err) = self.peer.set_remote_description(description).await {
                    warn!("[Signaling]: failed to set remote description: {err:?}");
                    return;
                }

                // Send an answer (local description) if we got an offer
                if remote_ty == RTCSdpType::Offer {
                    self.send_answer().await;
                }
            }
            StreamClientMessage::Signaling(StreamSignalingMessage::AddIceCandidate(
                description,
            )) => {
                debug!("[Signaling] Received Ice Candidate");

                if let Err(err) = self
                    .peer
                    .add_ice_candidate(RTCIceCandidateInit {
                        candidate: description.candidate,
                        sdp_mid: description.sdp_mid,
                        sdp_mline_index: description.sdp_mline_index,
                        username_fragment: description.username_fragment,
                    })
                    .await
                {
                    warn!("[Signaling]: failed to add ice candidate: {err:?}");
                }
            }
            // This should already be done
            StreamClientMessage::AuthenticateAndInit { .. } => {}
        }
    }

    async fn on_ice_candidate(&self, candidate: Option<RTCIceCandidate>) {
        let Some(candidate) = candidate else {
            return;
        };

        let Ok(candidate_json) = candidate.to_json() else {
            return;
        };

        debug!(
            "[Signaling] Sending Ice Candidate: {}",
            candidate_json.candidate
        );

        let message = StreamServerMessage::Signaling(StreamSignalingMessage::AddIceCandidate(
            RtcIceCandidate {
                candidate: candidate_json.candidate,
                sdp_mid: candidate_json.sdp_mid,
                sdp_mline_index: candidate_json.sdp_mline_index,
                username_fragment: candidate_json.username_fragment,
            },
        ));

        let peer_id = self.current_peer_id.read().await.clone();
        self.ipc_sender
            .clone()
            .send(if let Some(peer_id) = peer_id {
                StreamerIpcMessage::ToPeer { peer_id, message }
            } else {
                StreamerIpcMessage::Broadcast(message)
            })
            .await;
    }

    // -- Data Channels
    async fn on_data_channel(self: &Arc<Self>, channel: Arc<RTCDataChannel>) {
        self.input.on_data_channel(self, channel).await;
    }

    // Start Moonlight Stream
    async fn start_stream(self: &Arc<Self>) -> Result<(), anyhow::Error> {
        // Send stage
        let mut ipc_sender = self.ipc_sender.clone();
        ipc_sender
            .send(StreamerIpcMessage::WebSocket(
                StreamServerMessage::StageStarting {
                    stage: "Moonlight Stream".to_string(),
                },
            ))
            .await;

        let mut host = self.info.host.lock().await;

        let gamepads = self.input.active_gamepads.read().await;

        let video_decoder = TrackSampleVideoDecoder::new(
            self.clone(),
            self.settings.video_supported_formats,
            self.settings.video_sample_queue_size as usize,
        );

        let audio_decoder = OpusTrackSampleAudioDecoder::new(
            self.clone(),
            self.settings.audio_sample_queue_size as usize,
        );

        let connection_listener = StreamConnectionListener::new(self.clone());

        let stream = match host
            .start_stream(
                &self.moonlight,
                self.info.app_id,
                self.settings.width,
                self.settings.height,
                self.settings.fps,
                false,
                true,
                self.settings.play_audio_local,
                *gamepads,
                false,
                self.settings.video_colorspace,
                if self.settings.video_color_range_full {
                    ColorRange::Full
                } else {
                    ColorRange::Limited
                },
                self.settings.bitrate,
                self.settings.packet_size,
                connection_listener,
                video_decoder,
                audio_decoder,
            )
            .await
        {
            Ok(value) => value,
            Err(err) => {
                warn!("[Stream]: failed to start moonlight stream: {err:?}");

                #[allow(clippy::single_match)]
                match err {
                    HostError::Moonlight(MoonlightError::ConnectionAlreadyExists) => {
                        ipc_sender
                            .send(StreamerIpcMessage::WebSocket(
                                StreamServerMessage::AlreadyStreaming,
                            ))
                            .await;
                    }
                    _ => {}
                }

                return Err(err.into());
            }
        };

        let host_features = stream.host_features().unwrap_or_else(|err| {
            warn!("[Stream]: failed to get host features: {err:?}");
            HostFeatures::empty()
        });

        let capabilities = StreamCapabilities {
            touch: host_features.contains(HostFeatures::PEN_TOUCH_EVENTS),
        };

        let (width, height) = {
            let video_size = self.video_size.lock().await;
            if *video_size == (0, 0) {
                (self.settings.width, self.settings.height)
            } else {
                *video_size
            }
        };

        spawn(async move {
            ipc_sender
                .send(StreamerIpcMessage::WebSocket(
                    StreamServerMessage::ConnectionComplete {
                        capabilities,
                        width,
                        height,
                    },
                ))
                .await;
        });

        drop(gamepads);

        let mut stream_guard = self.stream.write().await;
        stream_guard.replace(stream);

        Ok(())
    }

    // Multi-peer management methods (called from IPC loop with Arc<Self>)
    async fn add_peer_with_arc(self: &Arc<Self>, peer_id: String) -> Result<(), anyhow::Error> {
        info!("[Peer]: Creating new RTCPeerConnection for peer {}", peer_id);

        // Rebuild API for this peer (API is not Clone)
        let mut api_media = MediaEngine::default();
        register_audio_codecs(&mut api_media)?;
        register_video_codecs(&mut api_media, self.supported_video_formats)?;

        let mut api_registry = Registry::new();
        api_registry = register_default_interceptors(api_registry, &mut api_media)?;

        let api = APIBuilder::new()
            .with_setting_engine((*self.api_settings).clone())
            .with_media_engine(api_media)
            .with_interceptor_registry(api_registry)
            .build();

        // Create new RTCPeerConnection for this peer
        let peer_connection = Arc::new(api.new_peer_connection(self.rtc_config.clone()).await?);

        // Create data channel
        let general_channel = peer_connection.create_data_channel("general", None).await?;

        // Create WebRtcPeer struct
        let webrtc_peer = peer::WebRtcPeer::new(
            peer_id.clone(),
            peer_connection.clone(),
            general_channel.clone(),
        );

        // Store in peers map
        self.peers.write().await.insert(peer_id.clone(), webrtc_peer.clone());

        // Setup peer event handlers
        let this_clone = self.clone();
        let peer_id_clone = peer_id.clone();
        peer_connection.on_ice_candidate(Box::new(move |candidate| {
            let peer_id = peer_id_clone.clone();
            let ipc_sender = this_clone.ipc_sender.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    if let Ok(candidate_json) = candidate.to_json() {
                        let message = StreamServerMessage::Signaling(StreamSignalingMessage::AddIceCandidate(
                            RtcIceCandidate {
                                candidate: candidate_json.candidate,
                                sdp_mid: candidate_json.sdp_mid,
                                sdp_mline_index: candidate_json.sdp_mline_index,
                                username_fragment: candidate_json.username_fragment,
                            },
                        ));
                        let mut sender = ipc_sender.clone();
                        sender.send(StreamerIpcMessage::ToPeer { peer_id, message }).await;
                    }
                }
            })
        }));

        // Subscribe peer to broadcasters
        let mut video_rx = self.video_broadcaster.subscribe(peer_id.clone()).await;
        let mut audio_rx = self.audio_broadcaster.subscribe(peer_id.clone()).await;

        // Create video track (using H264 for now - most compatible)
        use webrtc::{
            api::media_engine::MIME_TYPE_H264,
            track::track_local::track_local_static_rtp::TrackLocalStaticRTP,
            rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
        };

        let video_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
                rtcp_feedback: vec![],
            },
            format!("video-{}", peer_id),
            "moonlight-peer".to_string(),
        ));

        // Create audio track (Opus)
        use webrtc::api::media_engine::MIME_TYPE_OPUS;
        let audio_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            format!("audio-{}", peer_id),
            "moonlight-peer".to_string(),
        ));

        // Add tracks to peer connection
        peer_connection.add_track(Arc::clone(&video_track) as Arc<dyn webrtc::track::track_local::TrackLocal + Send + Sync>).await?;
        peer_connection.add_track(Arc::clone(&audio_track) as Arc<dyn webrtc::track::track_local::TrackLocal + Send + Sync>).await?;

        // Spawn task to forward broadcast video frames to this peer
        let peer_id_video = peer_id.clone();
        spawn(async move {
            while let Some(frame) = video_rx.recv().await {
                // Write raw data to track (frames come from decoder already encoded)
                if let Err(err) = video_track.write_rtp(&webrtc::rtp::packet::Packet {
                    header: webrtc::rtp::header::Header {
                        version: 2,
                        padding: false,
                        extension: false,
                        marker: true,
                        payload_type: 96, // H264
                        sequence_number: 0, // Track handles this
                        timestamp: frame.timestamp,
                        ssrc: 0, // Track handles this
                        ..Default::default()
                    },
                    payload: frame.data.to_vec().into(),
                }).await {
                    warn!("[Peer {}] Failed to write video: {err:?}", peer_id_video);
                    break;
                }
            }
            info!("[Peer {}] Video forwarding task ended", peer_id_video);
        });

        // Spawn task to forward broadcast audio samples to this peer
        let peer_id_audio = peer_id.clone();
        spawn(async move {
            while let Some(sample) = audio_rx.recv().await {
                if let Err(err) = audio_track.write_rtp(&webrtc::rtp::packet::Packet {
                    header: webrtc::rtp::header::Header {
                        version: 2,
                        padding: false,
                        extension: false,
                        marker: true,
                        payload_type: 111, // Opus
                        sequence_number: 0,
                        timestamp: sample.timestamp,
                        ssrc: 0,
                        ..Default::default()
                    },
                    payload: sample.data.to_vec().into(),
                }).await {
                    warn!("[Peer {}] Failed to write audio: {err:?}", peer_id_audio);
                    break;
                }
            }
            info!("[Peer {}] Audio forwarding task ended", peer_id_audio);
        });

        // Send WebRTC config to peer
        let mut sender = self.ipc_sender.clone();
        sender.send(StreamerIpcMessage::ToPeer {
            peer_id: peer_id.clone(),
            message: StreamServerMessage::WebRtcConfig {
                ice_servers: self.rtc_config.ice_servers.iter().cloned().map(from_webrtc_ice).collect(),
            }
        }).await;

        info!("[Peer]: Peer {} added successfully with dedicated RTCPeerConnection", peer_id);
        Ok(())
    }

    async fn on_peer_message(&self, peer_id: String, message: StreamClientMessage) {
        debug!("[Peer {}]: Received message", peer_id);

        // Look up actual peer
        let peers = self.peers.read().await;
        let peer = peers.get(&peer_id);

        if let Some(peer) = peer {
            // Route to correct peer's RTCPeerConnection
            match message {
                StreamClientMessage::Signaling(StreamSignalingMessage::Description(description)) => {
                    let description = match &description.ty {
                        RtcSdpType::Offer => RTCSessionDescription::offer(description.sdp),
                        RtcSdpType::Answer => RTCSessionDescription::answer(description.sdp),
                        RtcSdpType::Pranswer => RTCSessionDescription::pranswer(description.sdp),
                        _ => {
                            warn!("[Peer {}]: Invalid SDP type {:?}", peer_id, description.ty);
                            return;
                        }
                    };

                    let Ok(description) = description else {
                        warn!("[Peer {}]: Invalid RTCSessionDescription", peer_id);
                        return;
                    };

                    let remote_ty = description.sdp_type;
                    if let Err(err) = peer.connection.set_remote_description(description).await {
                        warn!("[Peer {}]: Failed to set remote description: {err:?}", peer_id);
                        return;
                    }

                    // If we received an offer, send answer
                    if remote_ty == RTCSdpType::Offer {
                        if let Ok(answer) = peer.connection.create_answer(None).await {
                            if peer.connection.set_local_description(answer.clone()).await.is_ok() {
                                let message = StreamServerMessage::Signaling(StreamSignalingMessage::Description(
                                    RtcSessionDescription {
                                        ty: from_webrtc_sdp(answer.sdp_type),
                                        sdp: answer.sdp,
                                    },
                                ));
                                let mut sender = self.ipc_sender.clone();
                                sender.send(StreamerIpcMessage::ToPeer {
                                    peer_id: peer_id.clone(),
                                    message,
                                }).await;
                            }
                        }
                    }
                }
                StreamClientMessage::Signaling(StreamSignalingMessage::AddIceCandidate(candidate)) => {
                    let _ = peer.connection.add_ice_candidate(RTCIceCandidateInit {
                        candidate: candidate.candidate,
                        sdp_mid: candidate.sdp_mid,
                        sdp_mline_index: candidate.sdp_mline_index,
                        username_fragment: candidate.username_fragment,
                    }).await;
                }
                StreamClientMessage::AuthenticateAndInit { .. } => {
                    // Ignore - already authenticated
                }
            }
        } else {
            warn!("[Peer {}]: Peer not found in peers map", peer_id);
        }
    }

    async fn remove_peer(&self, peer_id: &str) {
        info!("[Peer]: Removing peer {}", peer_id);

        // Cleanup from aggregators
        self.video_broadcaster.unsubscribe(peer_id).await;
        self.audio_broadcaster.unsubscribe(peer_id).await;
        self.input_aggregator.remove_peer(peer_id).await;

        // Remove from peers map
        self.peers.write().await.remove(peer_id);
    }

    async fn stop(&self) {
        debug!("[Stream]: Stopping...");

        let mut ipc_sender = self.ipc_sender.clone();
        spawn(async move {
            ipc_sender
                .send(StreamerIpcMessage::WebSocket(
                    StreamServerMessage::PeerDisconnect,
                ))
                .await;
        });

        let general_channel = self.general_channel.clone();
        spawn(async move {
            if let Some(message) = serialize_json(&StreamServerGeneralMessage::ConnectionTerminated)
            {
                let _ = general_channel.send_text(message).await;
            }
        });

        let stream = {
            let mut stream = self.stream.write().await;
            stream.take()
        };
        if let Err(err) = spawn_blocking(move || {
            drop(stream);
        })
        .await
        {
            warn!("[Stream]: failed to stop stream: {err}");
        };

        let mut ipc_sender = self.ipc_sender.clone();
        ipc_sender.send(StreamerIpcMessage::Stop).await;

        info!("Terminating Self");
        self.terminate.notify_waiters();
    }
}
