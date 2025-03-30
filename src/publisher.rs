// Copyright (C) 2025, Vivoh, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, is permitted only by Vivoh, Inc by License.
//

use std::{collections::VecDeque, fs, path::PathBuf, time::Duration};
use tokio::time::sleep;
use clap::Parser;
use tracing::{info, error};
use vivoh_quic_dash::{WebTransportMediaPacket, Error};
use bytes::Bytes;

#[derive(Parser, Debug)]
#[command(name = "vqd-publisher")]
#[command(about = "Reads DASH output and builds WebTransport Media Packets", long_about = None)]
struct Args {
    /// Path to the directory containing DASH output
    #[arg(short, long)]
    input: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let mut buffer: VecDeque<WebTransportMediaPacket> = VecDeque::new();

    let audio_init = read_file(args.input.join("init-1.mp4"))?;
    let video_init = read_file(args.input.join("init-0.mp4"))?;

    let mut packet_id: u32 = 0;

    loop {
        let mpd = read_file(args.input.join("stream.mpd"))?;

        // Find available segment numbers by looking for matching audio/video m4s files
        let segment_numbers = find_common_segments(&args.input)?;

        for number in segment_numbers {
            let audio_data_path = args.input.join(format!("chunk-1-{:05}.m4s", number));
            let video_data_path = args.input.join(format!("chunk-0-{:05}.m4s", number));

            let audio_data = read_file(audio_data_path)?;
            let video_data = read_file(video_data_path)?;

            let wmp = WebTransportMediaPacket {
                packet_id,
                timestamp: 0, // Will be populated later from MPD parsing
                duration: 0,  // Same
                mpd: mpd.clone(),
                audio_init: audio_init.clone(),
                video_init: video_init.clone(),
                audio_data,
                video_data,
            };

            info!("Successfully created WMP packet with {} bytes (mpd), {} (audio_init), {} (video_init), {} (audio_data), {} (video_data)",
                wmp.mpd.len(), wmp.audio_init.len(), wmp.video_init.len(), wmp.audio_data.len(), wmp.video_data.len());

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

