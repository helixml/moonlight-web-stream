use std::{collections::HashMap, path::Path, sync::Arc};

use actix_web::web::{Bytes, Data};
use actix_ws::Session;
use common::{
    api_bindings::SessionMode,
    ipc::{ServerIpcMessage, IpcSender},
};
use futures::future::join_all;
use log::{info, warn};
use moonlight_common::{
    PairStatus, mac::MacAddress, network::reqwest::ReqwestMoonlightHost, pair::ClientAuth,
};
use serde::{Deserialize, Serialize};
use slab::Slab;
use tokio::{
    fs, process::Child, spawn,
    sync::{
        Mutex, RwLock,
        mpsc::{Receiver, Sender, channel},
    },
};

use crate::Config;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ApiData {
    hosts: Vec<Host>,
    #[serde(default)]
    client_certificates: HashMap<String, SerializableClientAuth>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Host {
    address: String,
    http_port: u16,
    #[serde(default)]
    unique_id: Option<String>,
    #[serde(default)]
    cache: HostCache,
    paired: Option<PairedHost>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedHost {
    pub client_private_key: String,
    pub client_certificate: String,
    pub server_certificate: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableClientAuth {
    pub private_key: String,
    pub certificate: String,
}

pub struct RuntimeApiHost {
    pub cache: HostCache,
    pub moonlight: ReqwestMoonlightHost,
    pub app_images_cache: HashMap<u32, Bytes>,
    pub configured_unique_id: Option<String>,
}

impl RuntimeApiHost {
    pub async fn try_recache(&mut self, data: &RuntimeApiData) {
        let mut changed = false;

        let name = self.moonlight.host_name().await.ok();
        if let Some(name) = name {
            self.cache.name = Some(name.to_string());
            changed = true;
        }

        let mac = self.moonlight.mac().await.ok();
        if let Some(mac) = mac.flatten() {
            self.cache.mac = Some(mac);
            changed = true;
        }

        if changed {
            let _ = data.file_writer.try_send(());
        }
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct HostCache {
    pub name: Option<String>,
    pub mac: Option<MacAddress>,
}

/// Persistent streaming session that survives WebSocket disconnects
pub struct StreamSession {
    pub session_id: String,
    pub client_unique_id: Option<String>,  // Unique Moonlight client ID (for multi-app streaming)
    pub streamer: Mutex<Child>,
    pub ipc_sender: Mutex<IpcSender<ServerIpcMessage>>,
    pub websocket: Mutex<Option<Session>>,  // None when in keepalive mode
    pub mode: SessionMode,
}

pub struct RuntimeApiData {
    // TODO: make this private, make the save fn internal, only expose fn which uses this filer_writer sender to try_send on it
    pub(crate) file_writer: Sender<()>,
    pub(crate) hosts: RwLock<Slab<Mutex<RuntimeApiHost>>>,
    pub(crate) sessions: RwLock<HashMap<String, Arc<StreamSession>>>,  // NEW: persistent sessions
    pub(crate) client_certificates: RwLock<HashMap<String, ClientAuth>>,  // KICKOFF: Cache certificates by client_unique_id
}

impl RuntimeApiData {
    pub async fn load(config: &Config, data: ApiData) -> Data<Self> {
        info!("Loading hosts");

        let mut hosts = Slab::new();
        let loaded_hosts = join_all(data.hosts.into_iter().map(|host_data| async move {
            let unique_id_for_init = host_data.unique_id.clone();
            let mut host =
                match ReqwestMoonlightHost::new(host_data.address, host_data.http_port, unique_id_for_init) {
                    Ok(value) => value,
                    Err(err) => {
                        warn!("[Load]: failed to load host: {err:?}");
                        return None;
                    }
                };
            set_pair_state(&mut host, host_data.paired.as_ref()).await;

            // Try to cache data
            let _ = host.host_name().await;

            Some(RuntimeApiHost {
                cache: host_data.cache,
                moonlight: host,
                app_images_cache: Default::default(),
                configured_unique_id: host_data.unique_id,
            })
        }))
        .await;

        for loaded_host in loaded_hosts.into_iter().flatten() {
            hosts.insert(Mutex::new(loaded_host));
        }

        // This channel only requires a capacity of 1:
        // 1. All sender will use try_send after finishing their write operations
        // 2. If the buffer would larger than 1 we would do multiple writes after each other without data changes
        // -> no extra write operations
        let (file_writer, file_writer_receiver) = channel(1);

        // Load persisted client certificates
        let mut client_certs = HashMap::new();
        for (client_unique_id, serializable_auth) in data.client_certificates.into_iter() {
            match (
                pem::parse(&serializable_auth.private_key),
                pem::parse(&serializable_auth.certificate),
            ) {
                (Ok(private_key), Ok(certificate)) => {
                    client_certs.insert(
                        client_unique_id.clone(),
                        ClientAuth {
                            private_key,
                            certificate,
                        },
                    );
                    info!("Loaded persisted certificate for client_unique_id: {}", client_unique_id);
                }
                _ => {
                    warn!("Failed to parse persisted certificate for client_unique_id: {}", client_unique_id);
                }
            }
        }

        let this = Data::new(Self {
            file_writer,
            hosts: RwLock::new(hosts),
            sessions: RwLock::new(HashMap::new()),  // NEW: initialize empty sessions map
            client_certificates: RwLock::new(client_certs),  // KICKOFF: load persisted certificate cache
        });

        spawn({
            let path = config.data_path.clone();
            let this = this.clone();

            async move { self::file_writer(file_writer_receiver, path, this).await }
        });

        info!("Finished loading hosts");
        this
    }

    pub async fn save(&self) -> ApiData {
        let hosts = self.hosts.read().await;

        let mut output = ApiData {
            hosts: Vec::with_capacity(hosts.len()),
            client_certificates: HashMap::new(),
        };

        for (_, host) in &*hosts {
            let host = host.lock().await;

            let paired = Self::extract_paired(&host.moonlight);

            output.hosts.push(Host {
                address: host.moonlight.address().to_string(),
                http_port: host.moonlight.http_port(),
                unique_id: host.configured_unique_id.clone(),
                cache: host.cache.clone(),
                paired,
            });
        }

        // Save client certificates for persistence across restarts
        let certs = self.client_certificates.read().await;
        for (client_unique_id, client_auth) in certs.iter() {
            output.client_certificates.insert(
                client_unique_id.clone(),
                SerializableClientAuth {
                    private_key: pem::encode(&client_auth.private_key),
                    certificate: pem::encode(&client_auth.certificate),
                },
            );
        }

        output
    }
    fn extract_paired(host: &ReqwestMoonlightHost) -> Option<PairedHost> {
        let client_private_key = host.client_private_key()?;
        let client_certificate = host.client_certificate()?;
        let server_certificate = host.server_certificate()?;

        Some(PairedHost {
            client_private_key: client_private_key.to_string(),
            client_certificate: client_certificate.to_string(),
            server_certificate: server_certificate.to_string(),
        })
    }
}

async fn set_pair_state(host: &mut ReqwestMoonlightHost, paired: Option<&PairedHost>) {
    let Some(paired) = paired else {
        return;
    };

    let Ok(client_private_key) = pem::parse(&paired.client_private_key) else {
        warn!(
            "failed to parse client private key as pem. Client: {}",
            host.address()
        );
        return;
    };
    let Ok(client_certificate) = pem::parse(&paired.client_certificate) else {
        warn!(
            "failed to parse client certificate as pem. Client: {}",
            host.address()
        );
        return;
    };
    let Ok(server_certificate) = pem::parse(&paired.server_certificate) else {
        warn!(
            "failed to parse server certificate as pem. Client: {}",
            host.address()
        );
        return;
    };

    match host.set_pairing_info(
        &ClientAuth {
            private_key: client_private_key,
            certificate: client_certificate,
        },
        &server_certificate,
    ) {
        Ok(value) => value,
        Err(err) => {
            warn!(
                "failed to pair client even though it has pair data: Client: {}, Error: {:?}",
                host.address(),
                err
            );
            return;
        }
    };

    let Ok(status) = host.verify_paired().await else {
        // Fails if the host is offline
        return;
    };

    if status != PairStatus::Paired {
        warn!(
            "failed to pair client even though it has pair data: Client: {}",
            host.address(),
        );
    }
}

async fn file_writer(
    mut receiver: Receiver<()>,
    path: impl AsRef<Path>,
    data: Data<RuntimeApiData>,
) {
    loop {
        if receiver.recv().await.is_none() {
            return;
        }

        let data = data.save().await;

        let text = match serde_json::to_string_pretty(&data) {
            Err(err) => {
                warn!("failed to save data: {err:?}");

                continue;
            }
            Ok(value) => value,
        };

        if let Err(err) = fs::write(&path, text).await {
            warn!("failed to save data: {err:?}");
        }
    }
}
