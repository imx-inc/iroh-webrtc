//! Echo test: two iroh endpoints connected via WebRTC custom transport.
//!
//! Uses in-process mpsc channels for signaling (no real network signaling server).
//! Both endpoints use WebRTC as the only transport (no IP, no relay).

use std::sync::Arc;

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, RelayMode, SecretKey, TransportAddr};
use iroh_webrtc::{SignalingMessage, WebRtcAddressLookup, WebRtcTransport, to_custom_addr};
use tokio::sync::mpsc;

const ECHO_ALPN: &[u8] = b"test/echo";

/// Simple echo protocol handler.
#[derive(Debug, Clone)]
struct Echo;

impl ProtocolHandler for Echo {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = connection.accept_bi().await?;
        tokio::io::copy(&mut recv, &mut send).await?;
        send.finish()?;
        connection.closed().await;
        Ok(())
    }
}

/// Bridge signaling between two transports via mpsc channels.
///
/// sig1_out → sig2_in (messages from peer1 delivered to peer2)
/// sig2_out → sig1_in (messages from peer2 delivered to peer1)
fn signaling_bridge() -> (
    (
        mpsc::UnboundedSender<SignalingMessage>,
        mpsc::UnboundedReceiver<SignalingMessage>,
    ),
    (
        mpsc::UnboundedSender<SignalingMessage>,
        mpsc::UnboundedReceiver<SignalingMessage>,
    ),
) {
    let (tx1, rx1) = mpsc::unbounded_channel();
    let (tx2, rx2) = mpsc::unbounded_channel();
    // peer1 sends on tx1, peer2 receives on rx1
    // peer2 sends on tx2, peer1 receives on rx2
    ((tx1, rx2), (tx2, rx1))
}

/// Spawn a background task that forwards signaling messages between peers.
fn spawn_signaling_relay(
    mut rx_from_1: mpsc::UnboundedReceiver<SignalingMessage>,
    tx_to_2: mpsc::UnboundedSender<SignalingMessage>,
    mut rx_from_2: mpsc::UnboundedReceiver<SignalingMessage>,
    tx_to_1: mpsc::UnboundedSender<SignalingMessage>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(msg) = rx_from_1.recv() => {
                    tracing::info!(msg_type = ?msg_type(&msg), from = ?msg.from_endpoint(), to = ?msg.to_endpoint(), "relay: 1 → 2");
                    let _ = tx_to_2.send(msg);
                }
                Some(msg) = rx_from_2.recv() => {
                    tracing::info!(msg_type = ?msg_type(&msg), from = ?msg.from_endpoint(), to = ?msg.to_endpoint(), "relay: 2 → 1");
                    let _ = tx_to_1.send(msg);
                }
                else => break,
            }
        }
    });
}

fn msg_type(msg: &SignalingMessage) -> &'static str {
    match msg {
        SignalingMessage::Offer { .. } => "offer",
        SignalingMessage::Answer { .. } => "answer",
        SignalingMessage::IceCandidate { .. } => "ice",
    }
}

/// Create an endpoint with only WebRTC transport (no IP, no relay).
async fn make_endpoint(
    secret_key: SecretKey,
    transport: WebRtcTransport,
    lookup: WebRtcAddressLookup,
) -> anyhow::Result<Endpoint> {
    let ep = Endpoint::empty_builder()
        .secret_key(secret_key)
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .add_custom_transport(Arc::new(transport))
        .address_lookup(lookup)
        .bind()
        .await?;
    Ok(ep)
}

/// Build an EndpointAddr with only the WebRTC custom addr.
fn webrtc_addr(id: iroh::EndpointId) -> EndpointAddr {
    EndpointAddr::from_parts(
        id,
        std::iter::once(TransportAddr::Custom(to_custom_addr(id))),
    )
}

#[tokio::test]
async fn test_echo_over_webrtc() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("iroh_webrtc=trace,iroh=warn,str0m=warn")
        .with_test_writer()
        .try_init()
        .ok();

    let s1 = SecretKey::generate(&mut rand::rng());
    let s2 = SecretKey::generate(&mut rand::rng());

    // Set up signaling relay.
    let (sig1_out_tx, sig1_out_rx) = mpsc::unbounded_channel();
    let (sig1_in_tx, sig1_in_rx) = mpsc::unbounded_channel();
    let (sig2_out_tx, sig2_out_rx) = mpsc::unbounded_channel();
    let (sig2_in_tx, sig2_in_rx) = mpsc::unbounded_channel();

    // Bridge: peer1's outgoing → peer2's incoming, and vice versa.
    spawn_signaling_relay(sig1_out_rx, sig2_in_tx, sig2_out_rx, sig1_in_tx);

    // Create transports.
    let (transport1, lookup1) = WebRtcTransport::new(s1.public(), vec![], sig1_out_tx, sig1_in_rx);
    let (transport2, lookup2) = WebRtcTransport::new(s2.public(), vec![], sig2_out_tx, sig2_in_rx);

    // Register each peer as WebRTC-capable with the other.
    lookup1.add_peer(s2.public());
    lookup2.add_peer(s1.public());

    // Build endpoints.
    let ep1: Endpoint = make_endpoint(s1.clone(), transport1, lookup1).await?;
    let ep2: Endpoint = make_endpoint(s2.clone(), transport2, lookup2).await?;

    // Start echo server on ep2.
    let router = Router::builder(ep2).accept(ECHO_ALPN, Echo).spawn();

    // Connect from ep1 to ep2.
    let addr = webrtc_addr(s2.public());
    tracing::info!("connecting to {:?}", addr);

    let conn: Connection = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ep1.connect(addr, ECHO_ALPN),
    )
    .await??;

    tracing::info!("connected, sending echo");

    // Send data and verify echo.
    let (mut send, mut recv) = conn.open_bi().await?;
    let msg = b"hello from iroh-webrtc!";
    send.write_all(msg).await?;
    send.finish()?;

    let response = recv.read_to_end(1024).await?;
    assert_eq!(response, msg, "echo mismatch");

    tracing::info!("echo verified!");

    conn.close(0u32.into(), b"done");
    router.shutdown().await?;

    Ok(())
}
