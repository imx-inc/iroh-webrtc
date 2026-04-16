//! WebRTC custom transport for iroh.
//!
//! Provides WebRTC data channels as an iroh custom transport path, enabling
//! direct peer-to-peer connections — particularly browser-to-native where
//! iroh's default UDP/QUIC path isn't available.
//!
//! # Architecture
//!
//! This crate has three pieces:
//!
//! 1. **Transport** (`WebRtcTransport`) — implements iroh's `CustomTransport` /
//!    `CustomEndpoint` / `CustomSender` traits. Platform-specific: `str0m` on
//!    native, `web-sys RTCPeerConnection` in browser WASM.
//!
//! 2. **Address Lookup** (`WebRtcAddressLookup`) — implements iroh's `AddressLookup`
//!    trait. Tells iroh which peers are reachable via WebRTC by returning
//!    `TransportAddr::Custom` addresses for known peers. Without this, iroh
//!    never tries the WebRTC path.
//!
//! 3. **Signaling** (`SignalingMessage`) — wire format for SDP offer/answer and
//!    ICE candidate exchange. Signaling transport is the consumer's responsibility
//!    (HTTP endpoint, WebSocket, iroh relay side-channel, etc.) — this crate just
//!    takes `tokio::sync::mpsc` channels.
//!
//! # Usage
//!
//! ```ignore
//! use iroh_webrtc::{WebRtcTransport, WebRtcAddressLookup, SignalingMessage};
//!
//! // 1. Set up signaling channels (you bridge these to your signaling server)
//! let (sig_out_tx, sig_out_rx) = tokio::sync::mpsc::unbounded_channel::<SignalingMessage>();
//! let (sig_in_tx, sig_in_rx) = tokio::sync::mpsc::unbounded_channel::<SignalingMessage>();
//!
//! // 2. Create the transport and address lookup
//! let stun = vec!["stun:stun.l.google.com:19302".into()];
//! let (transport, lookup) = WebRtcTransport::new(my_endpoint_id, stun, sig_out_tx, sig_in_rx);
//!
//! // 3. Wire into iroh endpoint builder
//! let endpoint = iroh::Endpoint::builder(your_preset)
//!     .add_custom_transport(transport)
//!     .address_lookup(lookup.clone())
//!     .bind()
//!     .await?;
//!
//! // 4. Tell the lookup which peers support WebRTC
//! lookup.add_peer(remote_endpoint_id);
//!
//! // 5. Connect as usual — iroh will try WebRTC alongside relay
//! let conn = endpoint.connect(remote_endpoint_id, ALPN).await?;
//! ```

pub mod lookup;
pub mod signaling;

#[cfg(not(target_arch = "wasm32"))]
pub mod native;

#[cfg(target_arch = "wasm32")]
pub mod browser;

#[cfg(not(target_arch = "wasm32"))]
pub use native::WebRtcTransport;

#[cfg(target_arch = "wasm32")]
pub use browser::WebRtcTransport;

pub use lookup::WebRtcAddressLookup;
pub use signaling::SignalingMessage;

/// Transport ID for WebRTC within iroh's custom address system.
///
/// Both native and browser backends use this so they recognize each other's
/// `CustomAddr`. Registered in TRANSPORTS.md per iroh convention.
pub const TRANSPORT_ID: u64 = 0x6972_6f68_7772_7463; // "irohwrtc"

/// Encode an EndpointId as a CustomAddr for this transport.
pub fn to_custom_addr(endpoint_id: iroh_base::EndpointId) -> iroh_base::CustomAddr {
    iroh_base::CustomAddr::from((TRANSPORT_ID, &endpoint_id.as_bytes()[..]))
}

/// Decode an EndpointId from a CustomAddr. Returns `None` if the transport ID
/// doesn't match or the data isn't a valid EndpointId.
pub fn from_custom_addr(addr: &iroh_base::CustomAddr) -> Option<iroh_base::EndpointId> {
    if addr.id() != TRANSPORT_ID {
        return None;
    }
    let key_bytes: &[u8; 32] = addr.data().try_into().ok()?;
    iroh_base::EndpointId::from_bytes(key_bytes).ok()
}
