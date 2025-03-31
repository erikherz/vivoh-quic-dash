// Copyright (C) 2025, Vivoh, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, is permitted only by Vivoh, Inc by License.
//

// WebTransport Media Packet structure
use bytes::Bytes;
use quick_xml::de;
use std::path::PathBuf;
use clap::Parser;

#[derive(Debug, Clone)]
pub struct WebTransportMediaPacket {
    pub packet_id: u32,
    pub timestamp: u64,
    pub duration: u32,
    pub mpd: Bytes,
    pub audio_init: Bytes,
    pub video_init: Bytes,
    pub audio_data: Bytes,
    pub video_data: Bytes,
}

#[derive(Debug, Parser, Clone)]
pub struct Args {
    /// Input folder containing DASH segments and MPD
    #[arg(short, long)]
    pub input: PathBuf,

    /// Server URL to connect via WebTransport (e.g. https://va01.wtmpeg.com)
    #[arg(short, long)]
    pub server: Option<String>,
}

// Shared error type for both publisher and server
use thiserror::Error;

#[derive(Error, Debug)]
pub enum VqdError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("XML parsing error: {0}")]
    Xml(#[from] de::DeError),

    #[error("Missing file: {0}")]
    MissingFile(String),

    #[error("Other error: {0}")]
    Other(String),
}

impl From<url::ParseError> for VqdError {
    fn from(err: url::ParseError) -> Self {
        VqdError::Other(err.to_string())
    }
}

impl From<std::net::AddrParseError> for VqdError {
    fn from(err: std::net::AddrParseError) -> Self {
        VqdError::Other(err.to_string())
    }
}

impl From<web_transport_quinn::ClientError> for VqdError {
    fn from(err: web_transport_quinn::ClientError) -> Self {
        VqdError::Other(err.to_string())
    }
}

// Re-export for shared use
pub use bytes;
pub use anyhow;
pub use VqdError as Error;

