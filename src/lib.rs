// Copyright (C) 2025, Vivoh, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, is permitted only by Vivoh, Inc by License.
//

// WebTransport Media Packet structure
use bytes::Bytes;
use quick_xml::de;

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

// Re-export for shared use
pub use bytes;
pub use anyhow;
pub use VqdError as Error;

