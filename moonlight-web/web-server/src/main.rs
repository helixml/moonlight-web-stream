use common::config::Config;
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};
use std::{io::ErrorKind, path::Path};
use tokio::{
    fs,
    io::{AsyncBufReadExt, BufReader, stdin},
};

use actix_web::{App, HttpServer, web::Data};
use log::{LevelFilter, info};
use serde::{Serialize, de::DeserializeOwned};
use simplelog::{ColorChoice, TermLogger, TerminalMode};

use crate::{
    api::api_service,
    data::{ApiData, RuntimeApiData},
    web::{web_config_js_service, web_service},
};

mod api;
mod data;
mod web;

#[actix_web::main]
async fn main() {
    #[cfg(debug_assertions)]
    let log_level = LevelFilter::Debug;
    #[cfg(not(debug_assertions))]
    let log_level = LevelFilter::Info;

    TermLogger::init(
        log_level,
        simplelog::Config::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )
    .expect("failed to init logger");

    if let Err(err) = main2().await {
        info!("Error: {err:?}");
    }

    exit().await.expect("exit failed")
}

async fn exit() -> Result<(), anyhow::Error> {
    info!("Press Enter to close this window");

    let mut line = String::new();
    let mut reader = BufReader::new(stdin());

    reader.read_line(&mut line).await?;

    Ok(())
}

async fn main2() -> Result<(), anyhow::Error> {
    // Load Config
    let config = read_or_default::<Config>("./server/config.json").await;
    if config.credentials == "default" {
        info!("Enter your credentials in the config (server/config.json)");

        return Ok(());
    }
    let config = Data::new(config);

    // Load Data
    let data = read_or_default::<ApiData>(&config.data_path).await;
    let data = RuntimeApiData::load(&config, data).await;

    let bind_address = config.bind_address;
    let server = HttpServer::new({
        let config = config.clone();

        move || {
            App::new()
                .app_data(config.clone())
                .service(api_service(data.clone(), config.credentials.to_string(), config.clone()))
                .service(web_config_js_service())
                .service(web_service())
        }
    });

    if let Some(certificate) = config.certificate.as_ref() {
        info!("[Server]: Running Https Server with ssl tls");

        let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls())
            .expect("failed to create ssl tls acceptor");
        builder
            .set_private_key_file(&certificate.private_key_pem, SslFiletype::PEM)
            .expect("failed to set private key");
        builder
            .set_certificate_chain_file(&certificate.certificate_pem)
            .expect("failed to set certificate");

        server.bind_openssl(bind_address, builder)?.run().await?;
    } else {
        server.bind(bind_address)?.run().await?;
    }

    Ok(())
}

async fn read_or_default<T>(path: impl AsRef<Path>) -> T
where
    T: DeserializeOwned + Serialize + Default,
{
    match fs::read_to_string(path.as_ref()).await {
        Ok(value) => serde_json::from_str(&value).expect("invalid file"),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let value = T::default();

            let value_str = serde_json::to_string_pretty(&value).expect("failed to serialize file");

            if let Some(parent) = path.as_ref().parent() {
                fs::create_dir_all(parent)
                    .await
                    .expect("failed to create directories to file");
            }
            fs::write(path.as_ref(), value_str)
                .await
                .expect("failed to write default file");

            value
        }
        Err(err) => panic!("failed to read file: {err}"),
    }
}
