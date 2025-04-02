// Copyright (C) 2025, Vivoh, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, is permitted only by Vivoh, Inc by License.
//

// ─── Standard Library ───────────────────────────────────────────────────────────
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::Duration;
use std::collections::{VecDeque, HashSet};
use std::fs;
use std::path::PathBuf;

// ─── External Crates ────────────────────────────────────────────────────────────
use quinn::{ClientConfig as QuinnClientConfig, Endpoint};
use rustls::{ClientConfig as RustlsClientConfig, RootCertStore};
use rustls_native_certs::load_native_certs;
use tokio::sync::broadcast;
use tokio::time::sleep;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tracing::{debug, error, info, warn};
use url::Url;
use web_transport_quinn::{Client, Session};
use clap::Parser;
use serde::Deserialize;
use quick_xml::de::from_str;
use bytes::Bytes;

// ─── Internal Crate ─────────────────────────────────────────────────────────────
use vivoh_quic_dash::{
    VqdError,
    WebTransportMediaPacket,
    serialize_media_packet,
};

const CONNECTION_RETRY_MAX: u32 = 5;
const CONNECTION_RETRY_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Parser, Clone)]
pub struct Args {
    /// Input folder containing DASH segments and MPD
    #[arg(short, long, conflicts_with = "pipe")]
    pub input: Option<PathBuf>,

    /// Read DASH segments from stdin pipe
    #[arg(long, conflicts_with = "input")]
    pub pipe: bool,

    /// Server URL to connect via WebTransport (e.g. https://va01.wtmpeg.com)
    #[arg(short, long)]
    pub server: String,
}

#[derive(Debug, Deserialize)]
struct Mpd {
    #[serde(rename = "Period")]
    period: Period,
}

#[derive(Debug, Deserialize)]
struct Period {
    #[serde(rename = "AdaptationSet")]
    adaptation_sets: Vec<AdaptationSet>,
}

#[derive(Debug, Deserialize)]
struct AdaptationSet {
    #[serde(rename = "Representation")]
    representations: Vec<Representation>,
}

#[derive(Debug, Deserialize)]
struct Representation {
    #[serde(rename = "SegmentTemplate")]
    segment_template: SegmentTemplate,
    #[serde(rename = "@id")]
    id: u32,
}

#[derive(Debug, Deserialize)]
struct SegmentTemplate {
    #[serde(rename = "@startNumber")]
    start_number: Option<u32>,
    #[serde(rename = "@timescale")]
    _timescale: u32,
    #[serde(rename = "SegmentTimeline")]
    timeline: SegmentTimeline,
}

#[derive(Debug, Deserialize)]
struct SegmentTimeline {
    #[serde(rename = "S")]
    segments: Vec<SegmentEntry>,
}

#[derive(Debug, Deserialize)]
struct SegmentEntry {
    #[serde(rename = "@t")]
    start_time: Option<u64>,
    #[serde(rename = "@d")]
    duration: u32,
    #[serde(rename = "@r")]
    repeat: Option<i32>,
}

#[tokio::main]
async fn main() -> Result<(), VqdError> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Validate input options
    if !args.pipe && args.input.is_none() {
        return Err(VqdError::Other(
            "Either --input or --pipe must be specified".to_string(),
        ));
    }

    if args.pipe {
        info!("Reading DASH data from stdin pipe");
    } else if let Some(input_dir) = &args.input {
        // Make sure the input directory exists
        if !input_dir.exists() || !input_dir.is_dir() {
            return Err(VqdError::Other(format!(
                "Input directory {} does not exist or is not a directory",
                input_dir.display()
            )));
        }
        info!("Starting publisher with input directory: {}", input_dir.display());
    }
    
    info!("Server URL: {}", args.server);

    // Initialize the crypto provider
    if let Err(e) = rustls::crypto::ring::default_provider().install_default() {
        warn!("Crypto provider already installed or failed to install: {:?}", e);
    }

    // Create a channel for media packets with increased capacity for better buffering
    let (tx, _) = broadcast::channel(64);
    let connection_ready = Arc::new(AtomicBool::new(false));

    // Initialize the client with a clone of the sender
    let client = WebTransportClient::new(
        args.server.clone(),
        tx.clone(),
        connection_ready.clone(),
    )?;

    // Initialize the appropriate reader based on input type
    let reader_handle = if args.pipe {
        // Initialize the pipe reader
        let pipe_reader = PipeReader::new().await?;
        
        // Create a reader task that waits for the connection to be ready
        let connection_ready_for_reader = connection_ready.clone();
        let tx_for_reader = tx.clone();
        
        tokio::spawn(async move {
            // Wait for connection to be ready before starting to read
            info!("Pipe reader waiting for WebTransport connection to be established...");
            while !connection_ready_for_reader.load(Ordering::Relaxed) {
                sleep(Duration::from_millis(100)).await;
            }
            info!("Connection ready, starting to read piped DASH data");
            
            if let Err(e) = pipe_reader.start_reading(tx_for_reader).await {
                error!("Pipe reader error: {}", e);
            }
        })
    } else {
        // Initialize the dash reader with the directory input
        let dash_reader = DashReader::new(args.input.unwrap().clone()).await?;
        
        // Create a reader task that waits for the connection to be ready
        let connection_ready_for_reader = connection_ready.clone();
        let tx_for_reader = tx.clone();
        
        tokio::spawn(async move {
            // Wait for connection to be ready before starting to read
            info!("DASH reader waiting for WebTransport connection to be established...");
            while !connection_ready_for_reader.load(Ordering::Relaxed) {
                sleep(Duration::from_millis(100)).await;
            }
            info!("Connection ready, starting to read DASH data");
            
            if let Err(e) = dash_reader.start_reading(tx_for_reader).await {
                error!("DASH reader error: {}", e);
            }
        })
    };

    // Initialize the client with a clone of the sender
    let client_handle = tokio::spawn(async move {
        if let Err(e) = client.run().await {
            error!("WebTransport client error: {}", e);
        }
    });

    // Wait for both tasks to complete
    tokio::select! {
        _ = client_handle => info!("WebTransport client task ended"),
        _ = reader_handle => info!("Reader task ended"),
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down");
        }
    }

    Ok(())
}

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

        if let Err(e) = rustls::crypto::ring::default_provider().install_default() {
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

        let session: Session = match wt_client.connect(&url).await {
            Ok(s) => {
                info!("✅ WebTransport session established.");
                s
            }
            Err(e) => {
                error!("❌ Failed to connect: {}", e);
                return Err(e.into());
            }
        };
        
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

fn read_file(path: PathBuf) -> Result<Bytes, VqdError> {
    match fs::read(path.clone()) {
        Ok(data) => Ok(Bytes::from(data)),
        Err(e) => {
            error!("Failed to read file {}: {}", path.display(), e);
            Err(VqdError::Io(e))
        }
    }
}

fn find_common_segments(dir: &PathBuf) -> Result<Vec<u32>, VqdError> {
    let mut audio_segments = vec![];
    let mut video_segments = vec![];

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if let Some(num) = name.strip_prefix("chunk-1-").and_then(|s| s.strip_suffix(".m4s")) {
            if let Ok(n) = num.parse() {
                audio_segments.push(n);
            }
        }

        if let Some(num) = name.strip_prefix("chunk-0-").and_then(|s| s.strip_suffix(".m4s")) {
            if let Ok(n) = num.parse() {
                video_segments.push(n);
            }
        }
    }

    audio_segments.sort_unstable();
    video_segments.sort_unstable();

    // Only return segment numbers that exist for both audio and video
    let common: Vec<u32> = audio_segments
        .into_iter()
        .filter(|n| video_segments.contains(n))
        .collect();

    Ok(common)
}

pub fn extract_timings_from_mpd(mpd_str: &str, segment_number: u32) -> Option<((u64, u32), (u64, u32))> {
    let parsed: Mpd = from_str(mpd_str).ok()?;

    let mut result_video = None;
    let mut result_audio = None;

    for set in &parsed.period.adaptation_sets {
        for rep in &set.representations {
            let start_number = rep.segment_template.start_number.unwrap_or(1);
            let timeline = &rep.segment_template.timeline.segments;

            let mut current_number = start_number;
            let mut current_time = 0;

            for seg in timeline {
                let t = seg.start_time.unwrap_or(current_time);
                let d = seg.duration;
                let r = seg.repeat.unwrap_or(0);

                for _ in 0..=r {
                    if current_number == segment_number {
                        let result = (t, d);
                        if rep.id == 0 {
                            result_video = Some(result);
                        } else if rep.id == 1 {
                            result_audio = Some(result);
                        }
                        break;
                    }
                    current_number += 1;
                    current_time = t + d as u64;
                }
            }
        }
    }

    match (result_video, result_audio) {
        (Some(v), Some(a)) => Some((v, a)),
        _ => None,
    }
}

pub fn format_timestamp(ticks: u64, timescale: u32) -> String {
    let seconds = ticks as f64 / timescale as f64;
    let millis = (seconds * 1000.0).round() as u64;
    let h = millis / 3_600_000;
    let m = (millis % 3_600_000) / 60_000;
    let s = (millis % 60_000) / 1000;
    let ms = millis % 1000;
    format!("{:02}:{:02}:{:02}.{:03}", h, m, s, ms)
}

struct DashReader {
    input_dir: PathBuf,
    audio_init: Option<Bytes>,
    video_init: Option<Bytes>,
    packet_id: u32,
    seen_segments: HashSet<u32>,
    seen_queue: VecDeque<u32>,
}

impl DashReader {
    async fn new(input_dir: PathBuf) -> Result<Self, VqdError> {

        Ok(Self {
            input_dir,
            audio_init: None,
            video_init: None,
            packet_id: 0,
            seen_segments: HashSet::new(),
            seen_queue: VecDeque::new(),
        })
    }

    async fn ensure_init_files_loaded(&mut self) -> Result<bool, VqdError> {
        let audio_init_path = self.input_dir.join("init-1.mp4");
        let video_init_path = self.input_dir.join("init-0.mp4");
        
        // Check if files exist
        if !audio_init_path.exists() || !video_init_path.exists() {
            warn!("Initialization files missing. Audio: {}, Video: {}", 
                  audio_init_path.exists(), video_init_path.exists());
            return Ok(false);
        }
        
        // If we already have the init segments, no need to read again
        if self.audio_init.is_some() && self.video_init.is_some() {
            return Ok(true);
        }
        
        // Read initialization files
        match read_file(audio_init_path) {
            Ok(data) => self.audio_init = Some(data),
            Err(e) => {
                error!("Failed to read audio init file: {}", e);
                return Ok(false);
            }
        }
        
        match read_file(video_init_path) {
            Ok(data) => self.video_init = Some(data),
            Err(e) => {
                error!("Failed to read video init file: {}", e);
                return Ok(false);
            }
        }
        
        info!("Successfully loaded initialization files");
        Ok(true)
    }

    async fn start_reading(mut self, tx: broadcast::Sender<WebTransportMediaPacket>) -> Result<(), VqdError> {
        const MAX_SEEN: usize = 10;
        let mut mpd_logged = false; 
        
        loop {

            if tx.receiver_count() == 0 {
                warn!("No receivers listening, waiting...");
                sleep(Duration::from_secs(1)).await;
                continue;
            }
            
            // Ensure init files are loaded
            if !self.ensure_init_files_loaded().await? {
                warn!("Waiting for initialization files...");
                sleep(Duration::from_secs(1)).await;
                continue;
            }

            // Required files: MPD, plus audio and video init segments
            let mpd_path = self.input_dir.join("stream.mpd");
            
            // Make sure MPD file exists before proceeding
            if !mpd_path.exists() {
                warn!("Required MPD file missing: {}", mpd_path.display());
                sleep(Duration::from_secs(1)).await;
                continue;
            }

            // Find available segment numbers
            let segment_numbers = match find_common_segments(&self.input_dir) {
                Ok(segments) => {
                    if segments.is_empty() {
                        warn!("No common audio/video segments found, waiting...");
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    segments
                },
                Err(e) => {
                    error!("Failed to find common segments: {}", e);
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
                
            // Validate that at least one pair of segment files exists
            let next_segment = match segment_numbers.iter().find(|&n| !self.seen_segments.contains(n)) {
                Some(&n) => n,
                None => {
                    debug!("No new segments found, waiting...");
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            
            // Check if both audio and video files exist for this segment
            let audio_path = self.input_dir.join(format!("chunk-1-{:05}.m4s", next_segment));
            let video_path = self.input_dir.join(format!("chunk-0-{:05}.m4s", next_segment));
            
            if !audio_path.exists() || !video_path.exists() {
                warn!("Incomplete segment pair for segment {}: audio exists: {}, video exists: {}", 
                    next_segment, audio_path.exists(), video_path.exists());
                sleep(Duration::from_secs(1)).await;
                continue;
            }

            let mpd = match read_file(self.input_dir.join("stream.mpd")) {
                Ok(data) => {
                    // Add this code to log the MPD content once
                    if !mpd_logged {
                        match std::str::from_utf8(&data) {
                            Ok(mpd_str) => {
                                debug!("MPD Content (first 500 chars):\n{}", 
                                       &mpd_str[..std::cmp::min(mpd_str.len(), 500)]);
                                
                                // Check if it's a valid MPD
                                if !mpd_str.trim_start().starts_with("<?xml") && 
                                   !mpd_str.trim_start().starts_with("<MPD") {
                                    warn!("MPD content doesn't appear to be valid XML/MPD");
                                }
                                
                                debug!("Full MPD size: {} bytes", data.len());
                            },
                            Err(e) => {
                                warn!("MPD content is not valid UTF-8: {}", e);
                                debug!("MPD binary content (first 32 bytes): {:?}", 
                                      &data[..std::cmp::min(data.len(), 32)]);
                            }
                        }
                        mpd_logged = true;
                    }
                    data
                },
                Err(e) => {
                    error!("Failed to read MPD file: {}", e);
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            // Find available segment numbers
            let segment_numbers = match find_common_segments(&self.input_dir) {
                Ok(segments) => segments,
                Err(e) => {
                    error!("Failed to find common segments: {}", e);
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            for number in segment_numbers {
                if self.seen_segments.contains(&number) {
                    continue;
                }
            
                self.seen_queue.push_back(number);
                self.seen_segments.insert(number);
            
                if self.seen_queue.len() > MAX_SEEN {
                    if let Some(old) = self.seen_queue.pop_front() {
                        self.seen_segments.remove(&old);
                    }
                }
            
                let audio_data_path = self.input_dir.join(format!("chunk-1-{:05}.m4s", number));
                let video_data_path = self.input_dir.join(format!("chunk-0-{:05}.m4s", number));
            
                let audio_data = match read_file(audio_data_path) {
                    Ok(data) => data,
                    Err(e) => {
                        error!("Failed to read audio segment: {}", e);
                        continue;
                    }
                };
                
                let video_data = match read_file(video_data_path) {
                    Ok(data) => data,
                    Err(e) => {
                        error!("Failed to read video segment: {}", e);
                        continue;
                    }
                };
            
            let mut wmp = WebTransportMediaPacket {
                packet_id: self.packet_id,
                timestamp: 0,
                duration: 0,
                mpd: mpd.clone(),
                audio_init: self.audio_init.clone().unwrap(),
                video_init: self.video_init.clone().unwrap(),
                audio_data,
                video_data,
            };
            
                let mpd_str = match std::str::from_utf8(&mpd) {
                    Ok(s) => s,
                    Err(e) => {
                        error!("Failed to decode MPD as UTF-8: {}", e);
                        continue;
                    }
                };
            
                if let Some(((video_ts, video_dur), (audio_ts, _))) =
                    extract_timings_from_mpd(mpd_str, number)
                {
                    wmp.timestamp = video_ts;
                    wmp.duration = video_dur;
            
                    info!("WMP Packet Summary:");
                    info!("  packet_id     --> {}", wmp.packet_id);
                    info!("  timestamp     --> {} (raw: {})", format_timestamp(video_ts, 30_000), video_ts);
                    info!("  duration      --> {} ticks", video_dur);
                    info!("  mpd           --> {} bytes", wmp.mpd.len());
                    info!("  audio_init    --> {} bytes", wmp.audio_init.len());
                    info!("  video_init    --> {} bytes", wmp.video_init.len());
                    info!("  audio_data    --> {} bytes", wmp.audio_data.len());
                    info!("  video_data    --> {} bytes", wmp.video_data.len());
                    info!("  audio_ts      --> {} (raw: {})", format_timestamp(audio_ts, 44100), audio_ts);
                }
                
                // Send the packet to the channel
                if let Err(e) = tx.send(wmp) {
                    error!("Failed to send WMP to channel: {}", e);
                }
            
                self.packet_id += 1;
            }              

            sleep(Duration::from_secs(1)).await;
        }
    }
}

struct PipeReader {
    audio_init: Option<Bytes>,
    video_init: Option<Bytes>,
    packet_id: u32,
}

impl PipeReader {
    async fn new() -> Result<Self, VqdError> {
        Ok(Self {
            audio_init: None,
            video_init: None,
            packet_id: 0,
        })
    }
    
    async fn start_reading(mut self, tx: broadcast::Sender<WebTransportMediaPacket>) -> Result<(), VqdError> {
        // Set stdin to raw mode to efficiently read binary data
        let stdin = tokio::io::stdin();
        let mut stdin_reader = tokio::io::BufReader::new(stdin);
        
        // MPD data placeholder - we'll need to extract this from the input stream
        let mut mpd_data = Bytes::new();
        
        info!("Starting to read from stdin pipe");
        
        // First, read initialization segments and MPD
        // The exact format will depend on how the encoder formats the output
        // WIP: Need to adapt it based on the actual format
        
        // Read the MPD first (assuming it comes with a header or marker)
        match self.read_mpd_from_stdin(&mut stdin_reader).await {
            Ok(mpd) => {
                info!("Successfully read MPD from stdin: {} bytes", mpd.len());
                mpd_data = mpd;
            },
            Err(e) => {
                error!("Failed to read MPD from stdin: {}", e);
                return Err(e);
            }
        }
        
        // Read initialization segments
        match self.read_init_segments_from_stdin(&mut stdin_reader).await {
            Ok((audio, video)) => {
                info!("Successfully read initialization segments from stdin");
                self.audio_init = Some(audio);
                self.video_init = Some(video);
            },
            Err(e) => {
                error!("Failed to read initialization segments from stdin: {}", e);
                return Err(e);
            }
        }
        
        // Now continuously read media segments
        loop {
            if tx.receiver_count() == 0 {
                warn!("No receivers listening, waiting...");
                sleep(Duration::from_secs(1)).await;
                continue;
            }
            
            // Read the next segment pair
            match self.read_segment_pair_from_stdin(&mut stdin_reader).await {
                Ok((audio_data, video_data)) => {
                    // Create a WebTransportMediaPacket with the segment data
                    let wmp = WebTransportMediaPacket {
                        packet_id: self.packet_id,
                        timestamp: self.calculate_timestamp_from_segments(&audio_data, &video_data),
                        duration: self.calculate_duration_from_segments(&audio_data, &video_data),
                        mpd: mpd_data.clone(),
                        audio_init: self.audio_init.clone().unwrap(),
                        video_init: self.video_init.clone().unwrap(),
                        audio_data,
                        video_data,
                    };
                    
                    info!("WMP Packet Summary:");
                    info!("  packet_id     --> {}", wmp.packet_id);
                    info!("  timestamp     --> {} (raw: {})", 
                          format_timestamp(wmp.timestamp, 30_000), wmp.timestamp);
                    info!("  duration      --> {} ticks", wmp.duration);
                    info!("  mpd           --> {} bytes", wmp.mpd.len());
                    info!("  audio_init    --> {} bytes", wmp.audio_init.len());
                    info!("  video_init    --> {} bytes", wmp.video_init.len());
                    info!("  audio_data    --> {} bytes", wmp.audio_data.len());
                    info!("  video_data    --> {} bytes", wmp.video_data.len());
                    
                    // Send the packet to the channel
                    if let Err(e) = tx.send(wmp) {
                        error!("Failed to send WMP to channel: {}", e);
                    }
                    
                    self.packet_id += 1;
                },
                Err(e) => {
                    error!("Failed to read segment pair from stdin: {}", e);
                    // If we encounter an EOF, break the loop
                    if let VqdError::Io(io_err) = &e {
                        if io_err.kind() == std::io::ErrorKind::UnexpectedEof {
                            info!("End of input stream reached");
                            break;
                        }
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
        
        info!("Pipe reader finished");
        Ok(())
    }
    
    // Helper method to read the MPD from stdin
    async fn read_mpd_from_stdin(&self, reader: &mut tokio::io::BufReader<tokio::io::Stdin>) 
        -> Result<Bytes, VqdError> {
        // WIP: Need to adapt it based on how the MPD is formatted in the pipe
        let mut mpd_buffer = Vec::new();
        let mut reading_mpd = false;
        let mpd_start_marker = b"<MPD";
        let mpd_end_marker = b"</MPD>";
        
        // Read until we find the MPD start marker
        loop {
            let mut line = Vec::new();
            let n = reader.read_until(b'\n', &mut line).await?;
            if n == 0 {
                return Err(VqdError::Other("End of input before finding MPD".to_string()));
            }
            
            if !reading_mpd && line.windows(mpd_start_marker.len()).any(|window| window == mpd_start_marker) {
                reading_mpd = true;
                mpd_buffer.extend_from_slice(&line);
            } else if reading_mpd {
                mpd_buffer.extend_from_slice(&line);
                if line.windows(mpd_end_marker.len()).any(|window| window == mpd_end_marker) {
                    break;
                }
            }
        }
        
        Ok(Bytes::from(mpd_buffer))
    }
    
    // Helper method to read initialization segments from stdin
    async fn read_init_segments_from_stdin(&self, reader: &mut tokio::io::BufReader<tokio::io::Stdin>) 
        -> Result<(Bytes, Bytes), VqdError> {
        // WIP: Need to adapt it based on how the initialization segments are formatted in the pipe
        
        // For simplicity, let's assume we can recognize init segments by some marker or pattern
        
        // Read audio init segment
        let audio_init = self.read_mp4_box(reader, b"moov").await?;
        
        // Read video init segment
        let video_init = self.read_mp4_box(reader, b"moov").await?;
        
        Ok((audio_init, video_init))
    }
    
    // Helper method to read a segment pair from stdin
    async fn read_segment_pair_from_stdin(&self, reader: &mut tokio::io::BufReader<tokio::io::Stdin>) 
        -> Result<(Bytes, Bytes), VqdError> {
        // WIP: Need to adapt it based on how the media segments are formatted in the pipe
        
        // For simplicity, assume we can recognize segments by some marker or pattern
        
        // Read audio segment
        let audio_data = self.read_mp4_box(reader, b"moof").await?;
        
        // Read video segment
        let video_data = self.read_mp4_box(reader, b"moof").await?;
        
        Ok((audio_data, video_data))
    }
    
    // Helper method to read an MP4 box from stdin
    async fn read_mp4_box(&self, reader: &mut tokio::io::BufReader<tokio::io::Stdin>, box_type: &[u8]) 
        -> Result<Bytes, VqdError> {
        // WIP: Need to:
        // 1. Read the box size (4 bytes)
        // 2. Read the box type (4 bytes)
        // 3. Read the remaining data based on the box size
        
        // WIP: Need to implement proper MP4 box parsing
        let mut buffer = Vec::new();
        let mut header = [0u8; 8]; // 4 bytes for size, 4 bytes for type
        
        reader.read_exact(&mut header).await?;
        
        let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let typ = &header[4..8];
        
        if typ != box_type {
            return Err(VqdError::Other(format!(
                "Expected box type {:?}, got {:?}", box_type, typ
            )));
        }
        
        buffer.extend_from_slice(&header);
        
        let mut remaining_data = vec![0u8; size - 8]; // size includes the 8-byte header
        reader.read_exact(&mut remaining_data).await?;
        buffer.extend_from_slice(&remaining_data);
        
        Ok(Bytes::from(buffer))
    }
    
    // Helper method to calculate timestamp from segment data
    fn calculate_timestamp_from_segments(&self, audio_data: &Bytes, video_data: &Bytes) -> u64 {
        // WIP: Need to parse the MP4 segments to extract the timestamp
        // For now, we'll just return a placeholder value based on the packet ID
        self.packet_id as u64 * 30_000 // Assuming 30,000 ticks per second
    }
    
    // Helper method to calculate duration from segment data
    fn calculate_duration_from_segments(&self, audio_data: &Bytes, video_data: &Bytes) -> u32 {
        // WIP: Need to parse the MP4 segments to extract the duration
        // For now, we'll just return a placeholder value
        30_000 // Assuming 1 second duration at 30,000 ticks per second
    }
}