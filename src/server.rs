// Copyright (C) 2025, Vivoh, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, is permitted only by Vivoh, Inc by License.
//

// ─── Standard Library ───────────────────────────────────────────────────────────
use std::io::ErrorKind;
use std::sync::Arc;
use std::path::PathBuf;
use std::net::SocketAddr;

// ─── External Crates ────────────────────────────────────────────────────────────
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use web_transport_quinn::Session;
use clap::Parser;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use rustls_pki_types::pem::PemObject;

// ─── Internal Crate ─────────────────────────────────────────────────────────────
use vivoh_quic_dash::{
    VqdError,
    WebTransportMediaPacket,
    ConnectionRole,
    serialize_media_packet,
    ChannelManager,
};

const CHANNEL_NAME: &str = "/live";

#[derive(Parser, Debug, Clone)]
#[command(name = "vivoh-quic-dash-server", about = "WebTransport DASH server")]
pub struct Args {
    #[arg(short, long, default_value = "[::]:443")]
    pub addr: SocketAddr,

    #[arg(long)]
    pub cert: PathBuf,

    #[arg(long)]
    pub key: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Initialize crypto provider
    if let Err(e) = rustls::crypto::ring::default_provider().install_default() {
        warn!("Crypto provider initialization warning (may already be installed): {:?}", e);
    }

    // Parse CLI args
    let args = Args::parse();

    // Load certificate and key
    let certs = CertificateDer::pem_file_iter(&args.cert)?
        .collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_file(&args.key)?;

    // Create channel manager with a live channel
    let manager = Arc::new(Mutex::new(ChannelManager::new()));
    {
        let mut mgr = manager.lock().await;
        mgr.channels.insert("/live".to_string(), vivoh_quic_dash::ChannelInfo::new("/live".to_string()));
    }

    // Construct rustls config manually (same as your tls::build_server_config but inline)
    let mut rustls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    rustls_config.alpn_protocols = vec![b"h3".to_vec()];
    debug!("Server ALPN: {:?}", rustls_config.alpn_protocols);

    // Start WebTransport server
    info!("Starting WebTransport server on {}", args.addr);
    let quic_config = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_config)
        .map_err(|e| anyhow::anyhow!("QUIC config error: {e}"))?;
    let server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_config));
    let endpoint = quinn::Endpoint::server(server_config, args.addr)?;
    let server = web_transport_quinn::Server::new(endpoint);

    tokio::select! {
        res = run_accept_loop(server, manager) => res.map_err(|e| anyhow::anyhow!("Server error: {e}")),
        _ = tokio::signal::ctrl_c() => {
            info!("Shutdown requested via Ctrl+C");
            Ok(())
        }
    }
}

async fn handle_publisher(
    session: Session,
    conn_id: String,
    manager: Arc<Mutex<ChannelManager>>,
) {
    info!("Handling publisher stream for {}", conn_id);

    loop {
        match session.accept_uni().await {
            Ok(mut stream) => {
                let manager = manager.clone();
                let _conn_id = conn_id.clone();

                tokio::spawn(async move {
                    // Limit to 2MB per WMP to avoid memory abuse
                    let read_result = stream.read_to_end(2_000_000).await;

                    match read_result {
                        Ok(buf) => {
                            match WebTransportMediaPacket::parse(&buf) {
                                Ok(packet) => {
                                    debug!(
                                        "Parsed WMP #{} ({}ms, video: {} bytes)",
                                        packet.packet_id,
                                        packet.duration,
                                        packet.video_data.len()
                                    );

                                    let mut mgr = manager.lock().await;
                                    if let Some(channel) = mgr.channels.get_mut(CHANNEL_NAME) {
                                        // Add to buffer
                                        channel.buffer.push_back(packet.clone());

                                        // Trim oldest if over capacity
                                        while channel.buffer.len() > 32 {
                                            channel.buffer.pop_front();
                                        }

                                        // Broadcast serialized version to players
                                        let data = crate::serialize_media_packet(&packet);
                                        let _ = channel.broadcast.send(data);
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to parse WMP from publisher: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            error!("Publisher stream read error: {e}");
                        }
                    }
                });
            }
            Err(e) => {
                error!("Publisher session error: {e}");
                break;
            }
        }
    }
}


async fn handle_player(
    session: Session,
    conn_id: String,
    manager: Arc<Mutex<ChannelManager>>,
) {
    info!("Handling player stream for {}", conn_id);

    let (initial_packets, mut rx) = {
        let mgr = manager.lock().await;
        if let Some(channel) = mgr.channels.get(CHANNEL_NAME) {
            let buffered = channel
                .buffer
                .iter()
                .cloned()
                .map(|wmp| crate::serialize_media_packet(&wmp))
                .collect::<Vec<_>>();

            (buffered, channel.broadcast.subscribe())
        } else {
            error!("Channel {} not found", CHANNEL_NAME);
            return;
        }
    };

    // Send initial buffered packets
    for data in initial_packets {
        if let Err(e) = send_stream(&session, data).await {
            error!("Failed to send buffered WMP to player: {e}");
            return;
        }
    }

    // Listen for new packets
    while let Ok(data) = rx.recv().await {
        if let Err(e) = send_stream(&session, data).await {
            error!("Failed to send live WMP to player: {e}");
            break;
        }
    }
}

async fn send_stream(session: &Session, data: bytes::Bytes) -> Result<(), std::io::Error> {

    let mut stream = session.open_uni().await.map_err(|e| {
        error!("Failed to open uni stream: {e}");
        std::io::Error::new(std::io::ErrorKind::Other, "stream open failed")
    })?;

    stream.write_all(&data).await.map_err(|e| {
        std::io::Error::new(ErrorKind::Other, format!("WebTransport write failed: {e}"))
    })?;    

    Ok(())
}

async fn run_accept_loop(
    mut server: web_transport_quinn::Server,
    manager: Arc<Mutex<ChannelManager>>,
) -> Result<(), VqdError> {
    let mut next_id = 0;
    info!("WebTransport accept loop started");

    while let Some(req) = server.accept().await {
        let path = req.url().path().to_string();
        let conn_id = format!("conn-{}", next_id);
        next_id += 1;

        let is_publisher = path.ends_with("/pub");
        let role = if is_publisher {
            ConnectionRole::ActivePublisher
        } else {
            ConnectionRole::Player
        };

        info!(
            "Accepted session on path: {}, role: {:?}, conn_id: {}",
            path, role, conn_id
        );

        let session = match req.ok().await {
            Ok(sess) => sess,
            Err(e) => {
                error!("Failed to establish session for {}: {}", conn_id, e);
                continue;
            }
        };

        let manager = manager.clone();
        tokio::spawn(async move {
            {
                let mut mgr = manager.lock().await;

                mgr.channels
                    .entry("/live".to_string())
                    .or_insert_with(|| vivoh_quic_dash::ChannelInfo::new("/live".to_string()));

                mgr.set_connection_role(&conn_id, role.clone());

                if role == ConnectionRole::ActivePublisher {
                    if let Some(channel) = mgr.channels.get_mut("/live") {
                        channel.set_active_publisher(conn_id.clone());
                    }
                }
            }

            info!("WebTransport session established for {}", conn_id);

            if role == ConnectionRole::ActivePublisher {
                handle_publisher(session, conn_id.clone(), manager.clone()).await;
            } else {
                handle_player(session, conn_id.clone(), manager.clone()).await;
            }

            info!("Connection {} ended, cleaning up", conn_id);
            manager.lock().await.remove_connection(&conn_id);
        });
    }

    Ok(())
}