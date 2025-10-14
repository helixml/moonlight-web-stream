use std::{process::Stdio, time::Duration};

use actix_web::{
    Either, Error, HttpRequest, HttpResponse, get, post, rt as actix_rt,
    web::{Data, Json, Payload},
};
use actix_ws::{Closed, Message, Session};
use common::{
    StreamSettings,
    api_bindings::{
        PostCancelRequest, PostCancelResponse, StreamClientMessage, StreamServerMessage,
    },
    config::Config,
    ipc::{ServerIpcMessage, StreamerIpcMessage, create_child_ipc},
    serialize_json,
};
use log::{debug, info, warn};
use moonlight_common::{PairStatus, stream::bindings::SupportedVideoFormats};
use tokio::{process::Command, spawn, time::sleep};

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
            let Some(app) = apps.iter().find(|app| app.id == app_id).cloned() else {
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

        // Redirect ipc message into ws
        spawn(async move {
            while let Some(message) = ipc_receiver.recv().await {
                match message {
                    StreamerIpcMessage::WebSocket(message) => {
                        if let Err(Closed) = send_ws_message(&mut session, message).await {
                            warn!(
                                "[Ipc]: Tried to send a ws message but the socket is already closed"
                            );
                        }
                    }
                    StreamerIpcMessage::Stop => {
                        debug!("[Ipc]: ipc receiver stopped by streamer");
                        break;
                    }
                }
            }
            info!("[Ipc]: ipc receiver is closed");

            // close the websocket when the streamer crashed / disconnected / whatever
            let _ = session.close(None).await;
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
            })
            .await;

        // Redirect ws message into ipc
        while let Some(Ok(Message::Text(text))) = stream.recv().await {
            let Ok(message) = serde_json::from_str::<StreamClientMessage>(&text) else {
                warn!("[Stream]: failed to deserialize from json");
                return;
            };

            ipc_sender.send(ServerIpcMessage::WebSocket(message)).await;
        }

        // -- After the websocket disconnects we kill the stream:
        ipc_sender.send(ServerIpcMessage::Stop).await;
        drop(ipc_sender);

        sleep(Duration::from_secs(4)).await;

        info!("[Stream]: killing streamer");
        match child.kill().await {
            Ok(_) => {
                info!("[Stream]: killed streamer");
            }
            Err(err) => {
                warn!("[Stream]: failed to kill child: {err:?}");
            }
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
