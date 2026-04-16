//! Signaling protocol for WebRTC connection setup.
//!
//! WebRTC peers must exchange SDP offers/answers and ICE candidates before a
//! direct data channel can open. This module defines the wire format.
//!
//! # Signaling transport is your responsibility
//!
//! This crate does **not** ship a signaling server or transport. You provide
//! signaling by bridging `tokio::sync::mpsc` channels to your infrastructure:
//!
//! - HTTP endpoint on your server that relays messages between peers
//! - WebSocket connection to a signaling server
//! - iroh relay side-channel
//! - For testing: wire the `sig_out_rx` directly to the other side's `sig_in_tx`
//!
//! The `from` / `to` fields in each message are hex-encoded iroh EndpointIds,
//! so your signaling server can route messages to the right peer.

use serde::{Deserialize, Serialize};

/// A signaling message exchanged during WebRTC connection setup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    /// SDP offer from the initiating peer.
    #[serde(rename = "offer")]
    Offer {
        /// Hex-encoded iroh EndpointId of the sender.
        from: String,
        /// Hex-encoded iroh EndpointId of the recipient.
        to: String,
        /// SDP offer string.
        sdp: String,
    },

    /// SDP answer from the receiving peer.
    #[serde(rename = "answer")]
    Answer {
        from: String,
        to: String,
        /// SDP answer string.
        sdp: String,
    },

    /// ICE candidate discovered during gathering.
    #[serde(rename = "ice")]
    IceCandidate {
        from: String,
        to: String,
        /// ICE candidate string (SDP format).
        candidate: String,
        /// SDP media description index.
        sdp_m_line_index: Option<u16>,
    },
}

impl SignalingMessage {
    /// The recipient's EndpointId (hex).
    pub fn to_endpoint(&self) -> &str {
        match self {
            Self::Offer { to, .. } | Self::Answer { to, .. } | Self::IceCandidate { to, .. } => to,
        }
    }

    /// The sender's EndpointId (hex).
    pub fn from_endpoint(&self) -> &str {
        match self {
            Self::Offer { from, .. }
            | Self::Answer { from, .. }
            | Self::IceCandidate { from, .. } => from,
        }
    }
}
