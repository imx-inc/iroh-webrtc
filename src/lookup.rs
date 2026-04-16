//! Address lookup for WebRTC-capable peers.
//!
//! Implements iroh's `AddressLookup` trait to tell the iroh endpoint which
//! remote peers are reachable via WebRTC. Without this, iroh never tries
//! the WebRTC custom transport path — it wouldn't know the peer supports it.
//!
//! # How it works
//!
//! 1. Consumer registers WebRTC-capable peers via [`WebRtcAddressLookup::add_peer`]
//! 2. When iroh resolves a peer's EndpointId, this lookup returns a
//!    `TransportAddr::Custom(CustomAddr)` with our transport ID
//! 3. iroh sees the custom addr and routes packets through `WebRtcSender::poll_send`
//! 4. If signaling fails or the peer doesn't actually support WebRTC, the path
//!    stays pending and iroh falls back to relay — no harm done
//!
//! # Example
//!
//! ```ignore
//! let lookup = WebRtcAddressLookup::new();
//! lookup.add_peer(device_endpoint_id);
//!
//! let endpoint = iroh::Endpoint::builder(preset)
//!     .address_lookup(lookup.clone())
//!     .bind()
//!     .await?;
//! ```

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use iroh::address_lookup::{self, AddressLookup, EndpointData, EndpointInfo, Item};
use iroh_base::{EndpointId, TransportAddr};
use n0_future::boxed::BoxStream;

use crate::to_custom_addr;

/// Address lookup that resolves WebRTC-capable peers.
///
/// Maintains a set of EndpointIds known to support WebRTC. When iroh asks
/// "how do I reach peer X?", this returns a `TransportAddr::Custom` for any
/// peer in the set.
///
/// Clone is cheap (shared `Arc` interior).
#[derive(Debug, Clone)]
pub struct WebRtcAddressLookup {
    peers: Arc<RwLock<HashSet<EndpointId>>>,
}

impl Default for WebRtcAddressLookup {
    fn default() -> Self {
        Self::new()
    }
}

impl WebRtcAddressLookup {
    /// Provenance string for address lookup results from this service.
    pub const PROVENANCE: &'static str = "webrtc";

    /// Create a new empty address lookup.
    pub fn new() -> Self {
        Self {
            peers: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Register a peer as WebRTC-capable.
    ///
    /// Once added, iroh will try to reach this peer via the WebRTC custom
    /// transport in addition to relay/UDP.
    pub fn add_peer(&self, endpoint_id: EndpointId) {
        self.peers.write().expect("poisoned").insert(endpoint_id);
    }

    /// Remove a peer from the WebRTC-capable set.
    pub fn remove_peer(&self, endpoint_id: &EndpointId) {
        self.peers.write().expect("poisoned").remove(endpoint_id);
    }

    /// Check if a peer is registered as WebRTC-capable.
    pub fn has_peer(&self, endpoint_id: &EndpointId) -> bool {
        self.peers.read().expect("poisoned").contains(endpoint_id)
    }
}

impl AddressLookup for WebRtcAddressLookup {
    fn publish(&self, _data: &EndpointData) {
        // We don't publish — the consumer manages which peers are WebRTC-capable.
    }

    fn resolve(
        &self,
        endpoint_id: EndpointId,
    ) -> Option<BoxStream<Result<Item, address_lookup::Error>>> {
        let guard = self.peers.read().expect("poisoned");
        if !guard.contains(&endpoint_id) {
            return None;
        }

        let custom_addr = to_custom_addr(endpoint_id);
        let item = Item::new(
            EndpointInfo::from_parts(
                endpoint_id,
                EndpointData::new([TransportAddr::Custom(custom_addr)]),
            ),
            Self::PROVENANCE,
            None,
        );
        Some(Box::pin(n0_future::stream::once(Ok(item))))
    }
}
