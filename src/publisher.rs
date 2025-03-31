// Copyright (C) 2025, Vivoh, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, is permitted only by Vivoh, Inc by License.
//

use std::{collections::{VecDeque, HashSet}, fs, path::PathBuf, time::Duration};
use tokio::time::sleep;
use clap::Parser;
use tracing::{info, error};
use vivoh_quic_dash::{WebTransportMediaPacket, Error, VqdError};
use bytes::Bytes;
use quick_xml::de::from_str;
use serde::Deserialize;
use url::Url;
use web_transport_quinn::{Client, Session};
use quinn::{ClientConfig as QuinnClientConfig, Endpoint};
use std::sync::Arc;
use rustls::{ClientConfig as RustlsClientConfig};
use rustls_native_certs::load_native_certs;
use rustls::RootCertStore;
use quinn::crypto::rustls::QuicClientConfig;



#[derive(Parser, Debug)]
#[command(name = "vqd-publisher")]
#[command(about = "Reads DASH files and sends WebTransport Media Packets")]
pub struct Args {
    /// Path to the input directory (containing DASH files)
    #[arg(short, long)]
    pub input: PathBuf,

    /// WebTransport server URL (e.g. https://127.0.0.1:4433)
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
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    info!("Input path: {:?}", args.input);
    info!("Server URL: {}", args.server);  

    let session = connect_to_server(&args.server).await?;
    info!("Connected to WebTransport server successfully.");     

    let mut buffer: VecDeque<WebTransportMediaPacket> = VecDeque::new();

    const MAX_SEEN: usize = 10;
    let mut seen_queue: VecDeque<u32> = VecDeque::new();
    let mut seen_set: HashSet<u32> = HashSet::new();    

    let audio_init = read_file(args.input.join("init-1.mp4"))?;
    let video_init = read_file(args.input.join("init-0.mp4"))?;

    let mut packet_id: u32 = 0;

    loop {
        let mpd = read_file(args.input.join("stream.mpd"))?;

        // Find available segment numbers by looking for matching audio/video m4s files
        let segment_numbers = find_common_segments(&args.input)?;

        for number in segment_numbers {
            if seen_set.contains(&number) {
                continue;
            }
        
            seen_queue.push_back(number);
            seen_set.insert(number);
        
            if seen_queue.len() > MAX_SEEN {
                if let Some(old) = seen_queue.pop_front() {
                    seen_set.remove(&old);
                }
            }
        
            let audio_data_path = args.input.join(format!("chunk-1-{:05}.m4s", number));
            let video_data_path = args.input.join(format!("chunk-0-{:05}.m4s", number));
        
            let audio_data = read_file(audio_data_path)?;
            let video_data = read_file(video_data_path)?;
        
            let mut wmp = WebTransportMediaPacket {
                packet_id,
                timestamp: 0,
                duration: 0,
                mpd: mpd.clone(),
                audio_init: audio_init.clone(),
                video_init: video_init.clone(),
                audio_data,
                video_data,
            };
        
            let mpd_str = std::str::from_utf8(&mpd).map_err(|e| VqdError::Other(e.to_string()))?;
        
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
        
            if buffer.len() >= 5 {
                buffer.pop_front();
            }
        
            buffer.push_back(wmp);
            packet_id += 1;
        }              

        sleep(Duration::from_secs(1)).await;
    }
}

fn read_file(path: PathBuf) -> Result<Bytes, Error> {
    Ok(Bytes::from(fs::read(path)?))
}

fn find_common_segments(dir: &PathBuf) -> Result<Vec<u32>, Error> {
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

fn build_client_config() -> Result<QuinnClientConfig, VqdError> {
    let mut roots = RootCertStore::empty();

    for cert in load_native_certs().map_err(|e| VqdError::Other(e.to_string()))? {
        roots.add(cert).map_err(|e| VqdError::Other(format!("Failed to add cert: {:?}", e)))?;
    }

    let mut client_crypto = RustlsClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    client_crypto.alpn_protocols = vec![b"h3".to_vec()];

    let crypto = QuicClientConfig::try_from(client_crypto)
        .map_err(|e| VqdError::Other(format!("Crypto config error: {}", e)))?;

    Ok(QuinnClientConfig::new(Arc::new(crypto)))
}

async fn connect_to_server(server_url: &str) -> Result<Session, VqdError> {
    let url = Url::parse(server_url).map_err(|e| VqdError::Other(e.to_string()))?;
    let endpoint = Endpoint::client("[::]:0".parse().unwrap())?;

    let quinn_config = build_client_config()?;

    let client = Client::new(endpoint, quinn_config);

    info!("Connecting to WebTransport server at {}", server_url);

    let session = client.connect(&url).await.map_err(|e| {
        VqdError::Other(format!("Failed to connect: {e}"))
    })?;

    info!("WebTransport handshake started...");
    Ok(session)
}




