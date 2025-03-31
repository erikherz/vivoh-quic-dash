use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use tokio::sync::broadcast;
use url::Url;
use web_transport_quinn::{Client, Session};
use quinn::{ClientConfig as QuinnClientConfig, Endpoint};
use rustls::{ClientConfig as RustlsClientConfig, RootCertStore};
use rustls_native_certs::load_native_certs;
use rustls::crypto::ring;
use vivoh_quic_dash::{VqdError, WebTransportMediaPacket};
use std::time::Duration;
use tracing::{info, debug, error, warn};
use bytes::BytesMut;
use bytes::BufMut;

const CONNECTION_RETRY_MAX: u32 = 5;
const CONNECTION_RETRY_INTERVAL: Duration = Duration::from_secs(5);

pub struct WebTransportClient {
    server_url: String,
    media_channel: broadcast::Sender<WebTransportMediaPacket>,
    connection_ready: Arc<AtomicBool>,
}

impl WebTransportClient {
    pub fn new(
        server_url: String,
        media_channel: broadcast::Sender<WebTransportMediaPacket>,
        connection_ready: Arc<AtomicBool>,
    ) -> Result<Self, VqdError> {
        Ok(Self {
            server_url,
            media_channel,
            connection_ready,
        })
    }

    pub async fn run(&self) -> Result<(), VqdError> {
        info!("Starting WebTransport client to publish to {}", self.server_url);

        if let Err(e) = ring::default_provider().install_default() {
            warn!("Crypto provider already installed or failed to install: {:?}", e);
        }

        let mut retry_count = 0;

        loop {
            info!("Connecting to WebTransport server (attempt {})", retry_count + 1);

            match self.connect_and_publish().await {
                Ok(_) => {
                    info!("WebTransport client completed successfully");
                    break;
                }
                Err(error) => {
                    error!("WebTransport client error: {}", error);
                    self.connection_ready.store(false, Ordering::Relaxed);
                    retry_count += 1;

                    if retry_count >= CONNECTION_RETRY_MAX {
                        return Err(VqdError::Other(format!("Failed to connect after {} attempts", retry_count)));
                    }

                    info!("Retrying in {} seconds...", CONNECTION_RETRY_INTERVAL.as_secs());
                    tokio::time::sleep(CONNECTION_RETRY_INTERVAL).await;
                }
            }
        }

        Ok(())
    }

    fn build_client_config(&self) -> Result<QuinnClientConfig, VqdError> {
        let mut roots = RootCertStore::empty();
        for cert in load_native_certs().unwrap_or_default() {
            let _ = roots.add(cert);
        }

        let mut client_crypto = RustlsClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        client_crypto.alpn_protocols = vec![b"h3".to_vec()];

        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)
            .map_err(|e| VqdError::Other(format!("Failed to create crypto config: {}", e)))?;

        Ok(QuinnClientConfig::new(Arc::new(crypto)))
    }

    async fn connect_and_publish(&self) -> Result<(), VqdError> {
        self.connection_ready.store(false, Ordering::SeqCst);

        let mut url = Url::parse(&self.server_url)?;
        if !url.path().ends_with("/pub") {
            let mut path = url.path().to_string();
            if path.ends_with('/') {
                path.push_str("pub");
            } else {
                path.push_str("/pub");
            }
            url.set_path(&path);
        }

        info!("Final publisher URL: {}", url);

        let client_config = self.build_client_config()?;
        let mut endpoint = Endpoint::client("[::]:0".parse()?)?;
        endpoint.set_default_client_config(client_config.clone());

        let wt_client = Client::new(endpoint, client_config);
        info!("Connecting to WebTransport server at {}", url);

        let session: Session = wt_client.connect(&url).await?;
        info!("WebTransport session established");

        self.connection_ready.store(true, Ordering::SeqCst);
        info!("Publisher connection ready");

        let mut receiver = self.media_channel.subscribe();

        while let Ok(media_packet) = receiver.recv().await {
            if !self.connection_ready.load(Ordering::SeqCst) {
                break;
            }

            let data = serialize_media_packet(&media_packet);


            match session.open_uni().await {
                Ok(mut stream) => {
                    if let Err(e) = stream.write_all(data.as_ref()).await {
                        error!("❌ Failed to send media packet: {}", e);
                    } else {
                        debug!("📤 Sent media packet #{} ({} bytes)", media_packet.packet_id, data.len());
                    }
                }
                Err(e) => {
                    error!("❌ Failed to open unidirectional stream: {}", e);
                    break;
                }
            }
        }

        session.close(0u32.into(), b"done");
        Ok(())
    }
}

pub fn serialize_media_packet(packet: &WebTransportMediaPacket) -> bytes::Bytes {
    let mut buf = BytesMut::new();

    // Fixed header: 4 (packet_id) + 8 (timestamp) + 4 (duration)
    buf.put_u32(packet.packet_id);
    buf.put_u64(packet.timestamp);
    buf.put_u32(packet.duration);

    // Write 5 length-prefixed fields
    let fields = [
        &packet.mpd,
        &packet.audio_init,
        &packet.video_init,
        &packet.audio_data,
        &packet.video_data,
    ];

    for field in &fields {
        buf.put_u32(field.len() as u32);
    }

    for field in &fields {
        buf.put_slice(field);
    }

    buf.freeze()
}

#[tokio::main]
async fn main() -> Result<(), VqdError> {
    use clap::Parser;
    use vivoh_quic_dash::Args;

    let args = Args::parse();
    info!("Input path: {:?}", args.input);
    info!("Server URL: {:?}", args.server);


    let (tx, _) = broadcast::channel(8);
    let connection_ready = Arc::new(AtomicBool::new(false));

    let server_url = args.server.expect("Server URL is required");
    let client = WebTransportClient::new(server_url, tx.clone(), connection_ready.clone())?;

    client.run().await
}
