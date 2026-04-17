//! Standalone probe that connects to a live iroh device and attempts the
//! WebRTC custom transport path — the same thing a browser would do.
//!
//! Usage:
//!   cargo run --example webrtc_probe -- <device_endpoint_id> [relay_url]
//!
//! Outputs verbose tracing so you can see exactly where ICE fails.

use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{Connection, presets::Preset};
use iroh::{Endpoint, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey};
use iroh_webrtc::{SignalingMessage, native::WebRtcTransport};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

const ALPN_TUNNEL: &[u8] = b"/imx/tunnel/1";
const ALPN_WEBRTC_SIGNAL: &[u8] = b"/imx/webrtc-signal/1";

/// Minimal preset — just configures a custom relay URL.
struct ProbePreset {
    relay_url: RelayUrl,
}

impl Preset for ProbePreset {
    fn apply(self, builder: iroh::endpoint::Builder) -> iroh::endpoint::Builder {
        let relay_map = RelayMap::from_iter(vec![self.relay_url]);
        builder.relay_mode(RelayMode::Custom(relay_map))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "iroh_webrtc=debug,str0m=debug,webrtc_probe=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: {} <device_endpoint_id> [relay_url]", args[0]);
        std::process::exit(1);
    }

    let device_id: EndpointId = args[1]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid EndpointId"))?;
    let relay_url: RelayUrl = args
        .get(2)
        .map(|s| s.as_str())
        .unwrap_or("https://relay.imx.inc:7842/")
        .parse()?;

    info!(
        device = %device_id.fmt_short(),
        %relay_url,
        "starting WebRTC probe"
    );

    // Generate an ephemeral identity (like a browser would).
    let secret_key = SecretKey::generate(&mut rand::rng());
    let local_id = secret_key.public();
    info!(local = %local_id.fmt_short(), "local identity");

    // Set up signaling channels.
    let (sig_out_tx, mut sig_out_rx) = mpsc::unbounded_channel::<SignalingMessage>();
    let (sig_in_tx, sig_in_rx) = mpsc::unbounded_channel::<SignalingMessage>();

    // Create WebRTC transport + lookup.
    let (webrtc_transport, webrtc_lookup) = WebRtcTransport::new(
        local_id,
        vec!["stun:stun.l.google.com:19302".into()],
        sig_out_tx,
        sig_in_rx,
    );
    webrtc_lookup.add_peer(device_id);

    // Build endpoint with WebRTC transport + relay preset.
    let endpoint = Endpoint::builder(ProbePreset { relay_url })
        .secret_key(secret_key)
        .add_custom_transport(Arc::new(webrtc_transport))
        .address_lookup(webrtc_lookup)
        .bind()
        .await?;

    info!("endpoint bound");

    // Open a signaling connection to the device.
    info!("connecting for signaling...");
    let sig_endpoint = endpoint.clone();
    tokio::spawn(async move {
        let sig_conn = match sig_endpoint.connect(device_id, ALPN_WEBRTC_SIGNAL).await {
            Ok(c) => {
                info!("signaling connection established");
                c
            }
            Err(e) => {
                error!("signaling connect failed: {e}");
                return;
            }
        };

        let (mut send, mut recv) = match sig_conn.open_bi().await {
            Ok(s) => {
                info!("signaling bi-stream opened");
                s
            }
            Err(e) => {
                error!("signaling open_bi failed: {e}");
                return;
            }
        };

        // Read incoming signaling messages from device.
        let sig_in_tx_clone = sig_in_tx.clone();
        tokio::spawn(async move {
            loop {
                match imx_protocol_read_message::<SignalingMessage>(&mut recv).await {
                    Ok(msg) => {
                        info!(
                            msg_type = %msg_type(&msg),
                            from = msg.from_endpoint(),
                            "signal recv"
                        );
                        if sig_in_tx_clone.send(msg).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("signal read ended: {e}");
                        break;
                    }
                }
            }
        });

        // Write outgoing signaling messages to device.
        while let Some(msg) = sig_out_rx.recv().await {
            info!(
                msg_type = %msg_type(&msg),
                to = msg.to_endpoint(),
                "signal send"
            );
            if let Err(e) = imx_protocol_write_message(&mut send, &msg).await {
                error!("signal write failed: {e}");
                break;
            }
        }
    });

    // Try to open the tunnel — this will trigger poll_send on the WebRTC transport.
    info!("attempting tunnel connection (will try WebRTC + relay)...");

    let conn: Connection = tokio::time::timeout(
        Duration::from_secs(120),
        endpoint.connect(device_id, ALPN_TUNNEL),
    )
    .await??;

    info!("tunnel connected!");
    info!("probe complete, keeping connection alive 30s to observe...");
    tokio::time::sleep(Duration::from_secs(30)).await;

    conn.close(0u32.into(), b"probe done");
    endpoint.close().await;

    Ok(())
}

fn msg_type(msg: &SignalingMessage) -> &'static str {
    match msg {
        SignalingMessage::Offer { .. } => "offer",
        SignalingMessage::Answer { .. } => "answer",
        SignalingMessage::IceCandidate { .. } => "ice",
    }
}

// ---------------------------------------------------------------------------
// Wire protocol helpers (copy of imx_protocol::read_message / write_message)
// ---------------------------------------------------------------------------

async fn imx_protocol_read_message<T: for<'de> serde::Deserialize<'de>>(
    reader: &mut iroh::endpoint::RecvStream,
) -> anyhow::Result<T> {
    use tokio::io::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 1024 * 1024 {
        anyhow::bail!("message too large: {len}");
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

async fn imx_protocol_write_message<T: serde::Serialize>(
    writer: &mut iroh::endpoint::SendStream,
    msg: &T,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let json = serde_json::to_vec(msg)?;
    let len = (json.len() as u32).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(&json).await?;
    Ok(())
}
