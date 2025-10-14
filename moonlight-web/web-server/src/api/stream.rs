use std::{process::Stdio, sync::Arc};

use actix_web::{
    Either, Error, HttpRequest, HttpResponse, get, post, rt as actix_rt,
    web::{Data, Json, Payload},
};
use actix_ws::{Closed, Message, Session};
use common::{
    StreamSettings,
    api_bindings::{
        PostCancelRequest, PostCancelResponse, StreamClientMessage, StreamServerMessage, SessionMode,
    },
    config::Config,
    ipc::{ServerIpcMessage, StreamerIpcMessage, create_child_ipc},
    serialize_json,
};
use log::{debug, info, warn};
use moonlight_common::{PairStatus, stream::bindings::SupportedVideoFormats};
use tokio::{process::Command, spawn, sync::Mutex};

use crate::data::RuntimeApiData;

/// The stream handler WILL authenticate the client because it is a websocket
/// The Authenticator will let this route through
#[get("/host/stream")]
pub async fn start_host(
    data: Data<RuntimeApiData>,
    config: Data<Config>,
    request: HttpRequest,
    payload: Payload,
) -> Result<HttpResponse, Error> {
    let (response, mut session, mut stream) = actix_ws::handle(&request, payload)?;

    actix_rt::spawn(async move {
        let message;
        loop {
            message = match stream.recv().await {
                Some(Ok(Message::Text(text))) => text,
                Some(Ok(Message::Binary(_))) => {
                    return;
                }
                Some(Ok(_)) => continue,
                Some(Err(_)) => {
                    return;
                }
                None => {
                    return;
                }
            };
            break;
        }

        let message = match serde_json::from_str::<StreamClientMessage>(&message) {
            Ok(value) => value,
            Err(_) => {
                return;
            }
        };

        let StreamClientMessage::AuthenticateAndInit {
            credentials,
            session_id,
            mode,
            client_unique_id,
            host_id,
            app_id,
            bitrate,
            packet_size,
            fps,
            width,
            height,
            video_sample_queue_size,
            play_audio_local,
            audio_sample_queue_size,
            video_supported_formats,
            video_colorspace,
            video_color_range_full,
        } = message
        else {
            let _ = session.close(None).await;
            return;
        };

        if credentials != config.credentials {
            return;
        }

        // Check if session already exists
        let sessions = data.sessions.read().await;
        let existing_session = sessions.get(&session_id).cloned();
        drop(sessions);

        // Handle existing sessions based on mode
        match (&existing_session, mode) {
            // Case 1: Session exists, mode=Join → Kick old client, attach new WebSocket
            (Some(stream_session), SessionMode::Join) => {
                info!("[Stream]: Joining existing session {}, kicking old client", session_id);

                // Kick old WebSocket client
                let mut ws_lock = stream_session.websocket.lock().await;
                if let Some(mut old_ws) = ws_lock.take() {
                    let _ = send_ws_message(&mut old_ws, StreamServerMessage::ClientKicked).await;
                    let _ = old_ws.close(None).await;
                }
                *ws_lock = Some(session);
                drop(ws_lock);

                // Notify streamer that a client joined (important for keepalive→WebRTC transition)
                let mut ipc_sender = stream_session.ipc_sender.lock().await;
                ipc_sender.send(ServerIpcMessage::ClientJoined).await;
                drop(ipc_sender);

                info!("[Stream]: Sent ClientJoined signal to streamer for session {}", session_id);

                // Forward WebSocket messages to existing IPC
                while let Some(Ok(Message::Text(text))) = stream.recv().await {
                    let Ok(message) = serde_json::from_str::<StreamClientMessage>(&text) else {
                        warn!("[Stream]: failed to deserialize WebSocket message");
                        continue;
                    };

                    let mut ipc_sender = stream_session.ipc_sender.lock().await;
                    ipc_sender.send(ServerIpcMessage::WebSocket(message)).await;
                }

                // When WebSocket disconnects, set to None (back to keepalive mode)
                let mut ws_lock = stream_session.websocket.lock().await;
                *ws_lock = None;

                return;
            }

            // Case 2: Session exists, mode=Keepalive → Close WebSocket (no WebRTC needed)
            (Some(_), SessionMode::Keepalive) => {
                info!("[Stream]: Keepalive mode for existing session {} - no WebRTC needed", session_id);
                let _ = session.close(None).await;
                return;
            }

            // Case 3: Session exists, mode=Create → Error (already exists)
            (Some(_), SessionMode::Create) => {
                let _ = send_ws_message(&mut session, StreamServerMessage::AlreadyStreaming).await;
                let _ = session.close(None).await;
                return;
            }

            // Case 4: No session, mode=Join → Error (can't join non-existent)
            (None, SessionMode::Join) => {
                let _ = send_ws_message(&mut session, StreamServerMessage::SessionNotFound).await;
                let _ = session.close(None).await;
                return;
            }

            // Case 5: No session, mode=Create or Keepalive → Create new session (below)
            (None, SessionMode::Create | SessionMode::Keepalive) => {
                info!("[Stream]: Creating new session {} with mode {:?}", session_id, mode);
            }
        }

        let stream_settings = StreamSettings {
            bitrate,
            packet_size,
            fps,
            width,
            height,
            video_sample_queue_size,
            audio_sample_queue_size,
            play_audio_local,
            video_supported_formats: SupportedVideoFormats::from_bits(video_supported_formats)
                .unwrap_or_else(|| {
                    warn!("[Stream]: Received invalid supported video formats");
                    SupportedVideoFormats::H264
                }),
            video_colorspace: video_colorspace.into(),
            video_color_range_full,
        };

        // Collect host data
        let (
            host_address,
            host_http_port,
            client_private_key_pem,
            client_certificate_pem,
            server_certificate_pem,
            app,
        ) = {
            let hosts = data.hosts.read().await;
            let Some(host) = hosts.get(host_id as usize) else {
                let _ = send_ws_message(&mut session, StreamServerMessage::HostNotFound).await;
                let _ = session.close(None).await;
                return;
            };
            let mut host = host.lock().await;
            let host = &mut host.moonlight;

            if host.is_paired() == PairStatus::NotPaired {
                warn!("[Stream]: tried to connect to a not paired host");

                let _ = send_ws_message(&mut session, StreamServerMessage::HostNotPaired).await;
                let _ = session.close(None).await;
                return;
            }

            // Clear cache to get fresh app list from Wolf (apps may have been added since last query)
            host.clear_cache();

            let apps = match host.app_list().await {
                Ok(value) => value,
                Err(err) => {
                    warn!("[Stream]: failed to get app list from host: {err:?}");

                    let _ = send_ws_message(&mut session, StreamServerMessage::InternalServerError)
                        .await;
                    let _ = session.close(None).await;
                    return;
                }
            };

            // Debug: Log available apps for troubleshooting
            info!("[Stream]: Looking for app_id {} in {} available apps", app_id, apps.len());
            for (idx, app) in apps.iter().enumerate() {
                info!("[Stream]:   App {}: id={}, title={:?}", idx, app.id, app.title);
            }

            let Some(app) = apps.iter().find(|app| app.id == app_id).cloned() else {
                warn!("[Stream]: AppNotFound - requested app_id {} not in list of {} apps", app_id, apps.len());
                let _ = send_ws_message(&mut session, StreamServerMessage::AppNotFound).await;
                let _ = session.close(None).await;
                return;
            };

            if let Some(client_private_key) = host.client_private_key()
                && let Some(client_certificate) = host.client_certificate()
                && let Some(server_certificate) = host.server_certificate()
            {
                (
                    host.address().to_string(),
                    host.http_port(),
                    client_private_key.to_string(),
                    client_certificate.to_string(),
                    server_certificate.to_string(),
                    app,
                )
            } else {
                warn!("[Stream]: Missing certificates - private_key={}, client_cert={}, server_cert={}",
                    host.client_private_key().is_some(),
                    host.client_certificate().is_some(),
                    host.server_certificate().is_some());
                let _ = send_ws_message(&mut session, StreamServerMessage::InternalServerError).await;
                let _ = session.close(None).await;
                return;
            }
        };

        // Send App info
        let _ = send_ws_message(
            &mut session,
            StreamServerMessage::UpdateApp { app: app.into() },
        )
        .await;

        // Starting stage: launch streamer
        let _ = send_ws_message(
            &mut session,
            StreamServerMessage::StageComplete {
                stage: "Launch Streamer".to_string(),
            },
        )
        .await;

        // Spawn child
        let (mut child, stdin, stdout) = match Command::new(&config.streamer_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(mut child) => {
                if let Some(stdin) = child.stdin.take()
                    && let Some(stdout) = child.stdout.take()
                {
                    (child, stdin, stdout)
                } else {
                    warn!("[Stream]: streamer process didn't include a stdin or stdout");

                    let _ = send_ws_message(&mut session, StreamServerMessage::InternalServerError)
                        .await;
                    let _ = session.close(None).await;

                    if let Err(err) = child.kill().await {
                        warn!("[Stream]: failed to kill child: {err:?}");
                    }

                    return;
                }
            }
            Err(err) => {
                warn!("[Stream]: failed to spawn streamer process: {err:?}");

                let _ =
                    send_ws_message(&mut session, StreamServerMessage::InternalServerError).await;
                let _ = session.close(None).await;
                return;
            }
        };

        // Create ipc
        let (mut ipc_sender, mut ipc_receiver) =
            create_child_ipc::<ServerIpcMessage, StreamerIpcMessage>(
                "Streamer".to_string(),
                stdin,
                stdout,
                child.stderr.take(),
            )
            .await;

        // Create and store the session
        use crate::data::StreamSession;
        let stream_session = Arc::new(StreamSession {
            session_id: session_id.clone(),
            client_unique_id: client_unique_id.clone(),  // Store unique client ID for dashboard display
            streamer: Mutex::new(child),
            ipc_sender: Mutex::new(ipc_sender.clone()),
            websocket: Mutex::new(if mode == SessionMode::Keepalive {
                None  // Keepalive mode: no WebSocket attached
            } else {
                Some(session.clone())
            }),
            mode,
        });

        // Store session in global map
        {
            let mut sessions = data.sessions.write().await;
            sessions.insert(session_id.clone(), stream_session.clone());
        }

        // Redirect ipc message into ws
        let session_clone = stream_session.clone();
        let data_clone = data.clone();
        spawn(async move {
            while let Some(message) = ipc_receiver.recv().await {
                match message {
                    StreamerIpcMessage::WebSocket(message) => {
                        let mut ws_lock = session_clone.websocket.lock().await;
                        if let Some(ws) = &mut *ws_lock {
                            if let Err(Closed) = send_ws_message(ws, message).await {
                                warn!(
                                    "[Ipc]: Tried to send a ws message but the socket is already closed"
                                );
                            }
                        }
                        // If websocket is None (keepalive mode), just drop the message
                    }
                    StreamerIpcMessage::Stop => {
                        debug!("[Ipc]: ipc receiver stopped by streamer");
                        break;
                    }
                }
            }
            info!("[Ipc]: ipc receiver is closed");

            // Cleanup session based on mode
            if session_clone.mode != SessionMode::Keepalive {
                info!("[Stream]: Cleaning up non-keepalive session {}", session_clone.session_id);
                let mut sessions = data_clone.sessions.write().await;
                sessions.remove(&session_clone.session_id);

                // Kill streamer for non-keepalive sessions
                use tokio::process::Child;
                let mut streamer: tokio::sync::MutexGuard<Child> = session_clone.streamer.lock().await;
                if let Err(err) = streamer.kill().await {
                    warn!("[Stream]: failed to kill streamer: {err:?}");
                }
            } else {
                info!("[Stream]: Keepalive session {} persisting after IPC closed", session_clone.session_id);
            }

            // Close the websocket if it's still attached
            use actix_ws::Session;
            let mut ws_lock: tokio::sync::MutexGuard<Option<Session>> = session_clone.websocket.lock().await;
            if let Some(ws) = ws_lock.take() {
                let _ = ws.close(None).await;
            }
        });

        // Send init into ipc
        ipc_sender
            .send(ServerIpcMessage::Init {
                server_config: Config::clone(&config),
                stream_settings,
                host_address,
                host_http_port,
                host_unique_id: client_unique_id,
                client_private_key_pem,
                client_certificate_pem,
                server_certificate_pem,
                app_id,
                keepalive_mode: mode == SessionMode::Keepalive,
            })
            .await;

        // Redirect ws message into ipc (only if WebSocket is attached)
        if mode != SessionMode::Keepalive {
            while let Some(Ok(Message::Text(text))) = stream.recv().await {
                let Ok(message) = serde_json::from_str::<StreamClientMessage>(&text) else {
                    warn!("[Stream]: failed to deserialize from json");
                    return;
                };

                use common::ipc::IpcSender;
                let mut ipc: tokio::sync::MutexGuard<IpcSender<ServerIpcMessage>> = stream_session.ipc_sender.lock().await;
                ipc.send(ServerIpcMessage::WebSocket(message)).await;
            }

            // WebSocket disconnected - set to None (session goes to keepalive mode)
            info!("[Stream]: WebSocket disconnected for session {}, entering keepalive mode", session_id);
            let mut ws_lock = stream_session.websocket.lock().await;
            *ws_lock = None;
        } else {
            // Keepalive mode: close WebSocket immediately (no WebRTC needed)
            info!("[Stream]: Keepalive mode - closing WebSocket, streamer will run headless");
            let _ = session.close(None).await;
        }
    });

    Ok(response)
}

async fn send_ws_message(sender: &mut Session, message: StreamServerMessage) -> Result<(), Closed> {
    let Some(json) = serialize_json(&message) else {
        return Ok(());
    };

    sender.text(json).await
}

#[post("/host/cancel")]
pub async fn cancel_host(
    data: Data<RuntimeApiData>,
    request: Json<PostCancelRequest>,
) -> Either<Json<PostCancelResponse>, HttpResponse> {
    let hosts = data.hosts.read().await;

    let host_id = request.host_id;
    let Some(host) = hosts.get(host_id as usize) else {
        return Either::Right(HttpResponse::NotFound().finish());
    };

    let mut host = host.lock().await;

    let success = match host.moonlight.cancel().await {
        Ok(value) => value,
        Err(err) => {
            warn!("[Api]: failed to cancel stream for {host_id}:{err:?}");

            return Either::Right(HttpResponse::InternalServerError().finish());
        }
    };

    Either::Left(Json(PostCancelResponse { success }))
}
