//! Native (non-WASM) WebRTC transport using str0m (sans-IO).
//!
//! A background tokio task drives str0m I/O and signaling. `poll_recv` and
//! `poll_send` communicate with it via mpsc channels.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use iroh::endpoint::transports::{Addr, CustomEndpoint, CustomSender, CustomTransport, Transmit};
use iroh_base::{CustomAddr, EndpointId};
use parking_lot::Mutex;
use str0m::channel::ChannelId;
use str0m::net::{Protocol, Receive};
use str0m::{Event, Input, Output, Rtc};
use tokio::sync::mpsc;

use crate::lookup::WebRtcAddressLookup;
use crate::signaling::SignalingMessage;
use crate::{from_custom_addr, to_custom_addr, TRANSPORT_ID};

const DATA_CHANNEL_LABEL: &str = "iroh-quic";

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct IncomingPacket {
    data: Bytes,
    from: CustomAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    Connecting,
    Connected,
}

struct PeerState {
    rtc: Rtc,
    channel_id: Option<ChannelId>,
    conn_state: ConnState,
    remote_addr: Option<SocketAddr>,
    waker: Option<Waker>,
    pending_offer: Option<str0m::change::SdpPendingOffer>,
}

impl std::fmt::Debug for PeerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerState")
            .field("channel_id", &self.channel_id)
            .field("conn_state", &self.conn_state)
            .field("remote_addr", &self.remote_addr)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// SharedState — accessed by background task, endpoint, and sender
// ---------------------------------------------------------------------------

struct SharedState {
    local_id: Option<EndpointId>,
    peers: HashMap<EndpointId, PeerState>,
    addr_to_peer: HashMap<SocketAddr, EndpointId>,
    signaling_out: mpsc::UnboundedSender<SignalingMessage>,
    local_addr: SocketAddr,
    recv_queue_tx: mpsc::UnboundedSender<IncomingPacket>,
}

impl std::fmt::Debug for SharedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedState")
            .field("local_id", &self.local_id)
            .field("local_addr", &self.local_addr)
            .field("peer_count", &self.peers.len())
            .finish()
    }
}

impl SharedState {
    fn new_rtc(&self) -> Rtc {
        Rtc::new(Instant::now())
    }

    fn local_id_hex(&self) -> String {
        self.local_id.map(|id| format!("{id}")).unwrap_or_default()
    }

    fn create_offerer(&self) -> io::Result<(Rtc, String, str0m::change::SdpPendingOffer)> {
        let mut rtc = self.new_rtc();
        let mut api = rtc.sdp_api();
        let _ch = api.add_channel(DATA_CHANNEL_LABEL.to_string());
        let (offer, pending) = api
            .apply()
            .ok_or_else(|| io::Error::other("str0m: no SDP offer generated"))?;
        Ok((rtc, offer.to_sdp_string(), pending))
    }

    fn create_answerer(&self, offer_sdp: &str) -> io::Result<(Rtc, String)> {
        let mut rtc = self.new_rtc();
        let offer = str0m::change::SdpOffer::from_sdp_string(offer_sdp)
            .map_err(|e| io::Error::other(format!("invalid SDP offer: {e}")))?;
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| io::Error::other(format!("accept_offer failed: {e}")))?;
        Ok((rtc, answer.to_sdp_string()))
    }

    /// Drive a peer's str0m: flush outputs via the provided socket.
    fn drive_peer(&mut self, remote_id: &EndpointId, udp_socket: &tokio::net::UdpSocket) {
        let Some(peer) = self.peers.get_mut(remote_id) else {
            return;
        };
        let from_addr = to_custom_addr(*remote_id);

        loop {
            match peer.rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    let _ = udp_socket.try_send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(Event::ChannelOpen(ch_id, label))) => {
                    tracing::info!(remote = %remote_id.fmt_short(), %label, "data channel open");
                    peer.channel_id = Some(ch_id);
                    peer.conn_state = ConnState::Connected;
                    if let Some(w) = peer.waker.take() {
                        w.wake();
                    }
                }
                Ok(Output::Event(Event::ChannelData(data))) => {
                    let _ = self.recv_queue_tx.send(IncomingPacket {
                        data: Bytes::copy_from_slice(&data.data),
                        from: from_addr.clone(),
                    });
                }
                Ok(Output::Event(Event::ChannelClose(_))) => {
                    tracing::info!(remote = %remote_id.fmt_short(), "data channel closed");
                    peer.channel_id = None;
                    peer.conn_state = ConnState::Connecting;
                }
                Ok(Output::Event(Event::IceConnectionStateChange(s))) => {
                    tracing::debug!(remote = %remote_id.fmt_short(), ?s, "ICE state");
                }
                Ok(Output::Event(_)) => {}
                Ok(Output::Timeout(_)) => break,
                Err(e) => {
                    tracing::warn!(remote = %remote_id.fmt_short(), %e, "str0m error");
                    break;
                }
            }
        }
    }

    fn handle_signaling(
        &mut self,
        msg: SignalingMessage,
        udp_socket: &tokio::net::UdpSocket,
    ) {
        match msg {
            SignalingMessage::Offer { from, sdp, .. } => {
                let Ok(remote_id) = from.parse::<EndpointId>() else {
                    return;
                };
                tracing::debug!(remote = %remote_id.fmt_short(), "received SDP offer");

                match self.create_answerer(&sdp) {
                    Ok((mut rtc, answer_sdp)) => {
                        if let Ok(c) = str0m::Candidate::host(self.local_addr, "udp") {
                            rtc.add_local_candidate(c.clone());
                            // Send our ICE candidate to the offerer.
                            let _ = self.signaling_out.send(SignalingMessage::IceCandidate {
                                from: self.local_id_hex(),
                                to: from.clone(),
                                candidate: c.to_sdp_string(),
                                sdp_m_line_index: Some(0),
                            });
                        }
                        self.peers.insert(remote_id, PeerState {
                            rtc,
                            channel_id: None,
                            conn_state: ConnState::Connecting,
                            remote_addr: None,
                            waker: None,
                            pending_offer: None,
                        });
                        let _ = self.signaling_out.send(SignalingMessage::Answer {
                            from: self.local_id_hex(),
                            to: from,
                            sdp: answer_sdp,
                        });
                        self.drive_peer(&remote_id, udp_socket);
                    }
                    Err(e) => tracing::warn!(remote = %remote_id.fmt_short(), %e, "answer failed"),
                }
            }

            SignalingMessage::Answer { from, sdp, .. } => {
                let Ok(remote_id) = from.parse::<EndpointId>() else {
                    return;
                };
                if let Some(peer) = self.peers.get_mut(&remote_id) {
                    if let Some(pending) = peer.pending_offer.take() {
                        match str0m::change::SdpAnswer::from_sdp_string(&sdp) {
                            Ok(answer) => {
                                if let Err(e) = peer.rtc.sdp_api().accept_answer(pending, answer) {
                                    tracing::warn!(remote = %remote_id.fmt_short(), %e, "accept_answer failed");
                                } else {
                                    tracing::debug!(remote = %remote_id.fmt_short(), "SDP answer accepted");
                                }
                            }
                            Err(e) => tracing::warn!(remote = %remote_id.fmt_short(), %e, "invalid SDP answer"),
                        }
                    }
                }
                self.drive_peer(&remote_id, udp_socket);
            }

            SignalingMessage::IceCandidate { from, candidate, .. } => {
                let Ok(remote_id) = from.parse::<EndpointId>() else {
                    return;
                };
                if let Some(peer) = self.peers.get_mut(&remote_id) {
                    match str0m::Candidate::from_sdp_string(&candidate) {
                        Ok(c) => {
                            let addr = c.addr();
                            peer.rtc.add_remote_candidate(c);
                            self.addr_to_peer.insert(addr, remote_id);
                            peer.remote_addr = Some(addr);
                        }
                        Err(e) => tracing::warn!(remote = %remote_id.fmt_short(), %e, "bad ICE candidate"),
                    }
                }
                self.drive_peer(&remote_id, udp_socket);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Background I/O task
// ---------------------------------------------------------------------------

/// Runs in a tokio task: processes signaling, receives UDP packets, drives str0m.
async fn io_loop(
    state: Arc<Mutex<SharedState>>,
    udp_socket: Arc<tokio::net::UdpSocket>,
    mut signaling_in: mpsc::UnboundedReceiver<SignalingMessage>,
) {
    let mut udp_buf = vec![0u8; 65536];

    loop {
        // Wait for either signaling, UDP data, or a timeout.
        tokio::select! {
            Some(msg) = signaling_in.recv() => {
                tracing::debug!(from = msg.from_endpoint(), to = msg.to_endpoint(), "io_loop: received signaling");
                let mut s = state.lock();
                s.handle_signaling(msg, &udp_socket);
            }

            result = udp_socket.recv_from(&mut udp_buf) => {
                match result {
                    Ok((len, source)) => {
                        if len == 0 { continue; }
                        let data = udp_buf[..len].to_vec();
                        let mut s = state.lock();
                        let local_addr = s.local_addr;

                        // Route by known source addr.
                        let remote_id = s.addr_to_peer.get(&source).copied();
                        if let Some(id) = remote_id {
                            if let Some(peer) = s.peers.get_mut(&id) {
                                if let Ok(recv) = Receive::new(Protocol::Udp, source, local_addr, &data) {
                                    let _ = peer.rtc.handle_input(Input::Receive(Instant::now(), recv));
                                }
                            }
                            s.drive_peer(&id, &udp_socket);
                        } else {
                            // Unknown source — try all peers.
                            let ids: Vec<_> = s.peers.keys().copied().collect();
                            for id in ids {
                                if let Some(peer) = s.peers.get_mut(&id) {
                                    if let Ok(recv) = Receive::new(Protocol::Udp, source, local_addr, &data) {
                                        if peer.rtc.handle_input(Input::Receive(Instant::now(), recv)).is_ok() {
                                            s.addr_to_peer.insert(source, id);
                                            s.drive_peer(&id, &udp_socket);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::trace!(%e, "UDP recv error");
                    }
                }
            }

            // Drive str0m timeouts every 50ms.
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                let mut s = state.lock();
                let now = Instant::now();
                let ids: Vec<_> = s.peers.keys().copied().collect();
                for id in ids {
                    if let Some(peer) = s.peers.get_mut(&id) {
                        let _ = peer.rtc.handle_input(Input::Timeout(now));
                    }
                    s.drive_peer(&id, &udp_socket);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WebRtcTransport
// ---------------------------------------------------------------------------

/// WebRTC custom transport for iroh (native backend).
pub struct WebRtcTransport {
    local_id: EndpointId,
    stun_servers: Vec<String>,
    signaling_out: mpsc::UnboundedSender<SignalingMessage>,
    signaling_in: Arc<Mutex<Option<mpsc::UnboundedReceiver<SignalingMessage>>>>,
    recv_queue_tx: mpsc::UnboundedSender<IncomingPacket>,
    recv_queue_rx: Arc<Mutex<mpsc::UnboundedReceiver<IncomingPacket>>>,
}

impl WebRtcTransport {
    /// Create a new WebRTC transport and its companion address lookup.
    ///
    /// `local_id` is the EndpointId of the local iroh endpoint (derived from
    /// the SecretKey). It's included in signaling messages so the remote peer
    /// knows who is sending the offer/answer.
    pub fn new(
        local_id: EndpointId,
        stun_servers: Vec<String>,
        signaling_out: mpsc::UnboundedSender<SignalingMessage>,
        signaling_in: mpsc::UnboundedReceiver<SignalingMessage>,
    ) -> (Self, WebRtcAddressLookup) {
        let (recv_tx, recv_rx) = mpsc::unbounded_channel();
        let transport = Self {
            local_id,
            stun_servers,
            signaling_out,
            signaling_in: Arc::new(Mutex::new(Some(signaling_in))),
            recv_queue_tx: recv_tx,
            recv_queue_rx: Arc::new(Mutex::new(recv_rx)),
        };
        (transport, WebRtcAddressLookup::new())
    }
}

impl std::fmt::Debug for WebRtcTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebRtcTransport")
            .field("stun_servers", &self.stun_servers)
            .finish()
    }
}

impl CustomTransport for WebRtcTransport {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        // Bind UDP socket for str0m I/O.
        // Bind to localhost for now. A real deployment would bind 0.0.0.0
        // and gather ICE candidates for all interfaces.
        let std_socket = std::net::UdpSocket::bind("127.0.0.1:0")?;
        std_socket.set_nonblocking(true)?;
        let local_addr = std_socket.local_addr()?;
        let udp_socket = Arc::new(tokio::net::UdpSocket::from_std(std_socket)?);

        tracing::info!(%local_addr, "WebRTC transport bound");

        let state = Arc::new(Mutex::new(SharedState {
            local_id: Some(self.local_id),
            peers: HashMap::new(),
            addr_to_peer: HashMap::new(),
            signaling_out: self.signaling_out.clone(),
            local_addr,
            recv_queue_tx: self.recv_queue_tx.clone(),
        }));

        // Take signaling receiver (can only bind once).
        let signaling_in = self
            .signaling_in
            .lock()
            .take()
            .ok_or_else(|| io::Error::other("WebRtcTransport::bind() called more than once"))?;

        // Spawn background I/O task.
        tokio::spawn(io_loop(
            Arc::clone(&state),
            Arc::clone(&udp_socket),
            signaling_in,
        ));

        Ok(Box::new(WebRtcEndpoint {
            state,
            recv_queue_rx: Arc::clone(&self.recv_queue_rx),
            local_addrs: n0_watcher::Watchable::new(Vec::new()),
        }))
    }
}

// ---------------------------------------------------------------------------
// WebRtcEndpoint (poll_recv side)
// ---------------------------------------------------------------------------

struct WebRtcEndpoint {
    state: Arc<Mutex<SharedState>>,
    recv_queue_rx: Arc<Mutex<mpsc::UnboundedReceiver<IncomingPacket>>>,
    local_addrs: n0_watcher::Watchable<Vec<CustomAddr>>,
}

impl std::fmt::Debug for WebRtcEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebRtcEndpoint").finish()
    }
}

impl CustomEndpoint for WebRtcEndpoint {
    fn watch_local_addrs(&self) -> n0_watcher::Direct<Vec<CustomAddr>> {
        self.local_addrs.watch()
    }

    fn create_sender(&self) -> Arc<dyn CustomSender> {
        Arc::new(WebRtcSender {
            state: Arc::clone(&self.state),
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context,
        bufs: &mut [io::IoSliceMut<'_>],
        metas: &mut [noq_udp::RecvMeta],
        source_addrs: &mut [Addr],
    ) -> Poll<io::Result<usize>> {
        let n = bufs.len();
        debug_assert_eq!(n, metas.len());
        debug_assert_eq!(n, source_addrs.len());
        if n == 0 {
            return Poll::Ready(Ok(0));
        }

        // Drain recv_queue (fed by the background I/O task).
        let mut rx = self.recv_queue_rx.lock();
        let mut packets = Vec::new();
        match rx.poll_recv_many(cx, &mut packets, n) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(0) => return Poll::Ready(Err(io::Error::other("recv channel closed"))),
            Poll::Ready(_) => {}
        };

        let mut count = 0;
        for (((pkt, meta), buf), src) in packets
            .into_iter()
            .zip(metas.iter_mut())
            .zip(bufs.iter_mut())
            .zip(source_addrs.iter_mut())
        {
            if buf.len() < pkt.data.len() {
                break;
            }
            buf[..pkt.data.len()].copy_from_slice(&pkt.data);
            *src = pkt.from.into();
            meta.len = pkt.data.len();
            meta.stride = pkt.data.len();
            count += 1;
        }

        if count > 0 {
            Poll::Ready(Ok(count))
        } else {
            Poll::Pending
        }
    }

    fn max_transmit_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }
}

// ---------------------------------------------------------------------------
// WebRtcSender (poll_send side)
// ---------------------------------------------------------------------------

struct WebRtcSender {
    state: Arc<Mutex<SharedState>>,
}

impl std::fmt::Debug for WebRtcSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebRtcSender").finish()
    }
}

impl CustomSender for WebRtcSender {
    fn is_valid_send_addr(&self, addr: &CustomAddr) -> bool {
        addr.id() == TRANSPORT_ID
    }

    fn poll_send(
        &self,
        cx: &mut Context,
        dst: &CustomAddr,
        transmit: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        let remote_id = match from_custom_addr(dst) {
            Some(id) => id,
            None => return Poll::Ready(Err(io::Error::other("invalid custom addr"))),
        };

        let mut state = self.state.lock();

        if let Some(peer) = state.peers.get_mut(&remote_id) {
            match (peer.conn_state, peer.channel_id) {
                (ConnState::Connected, Some(ch_id)) => {
                    let seg = transmit.segment_size.unwrap_or(transmit.contents.len());
                    for chunk in transmit.contents.chunks(seg) {
                        if let Some(mut ch) = peer.rtc.channel(ch_id) {
                            if let Err(e) = ch.write(true, chunk) {
                                return Poll::Ready(Err(io::Error::other(format!(
                                    "data channel write: {e}"
                                ))));
                            }
                        }
                    }
                    // str0m produced output will be flushed by the background task
                    // on the next timeout cycle. For lower latency we could signal
                    // the task here, but 50ms is acceptable for now.
                    Poll::Ready(Ok(()))
                }
                _ => {
                    peer.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        } else {
            tracing::debug!(remote = %remote_id.fmt_short(), "initiating WebRTC signaling");

            match state.create_offerer() {
                Ok((mut rtc, offer_sdp, pending)) => {
                    let local_id_hex = state.local_id_hex();
                    let remote_id_hex = format!("{remote_id}");

                    if let Ok(c) = str0m::Candidate::host(state.local_addr, "udp") {
                        rtc.add_local_candidate(c.clone());

                        // Send offer first, then ICE candidate.
                        let _ = state.signaling_out.send(SignalingMessage::Offer {
                            from: local_id_hex.clone(),
                            to: remote_id_hex.clone(),
                            sdp: offer_sdp,
                        });
                        let _ = state.signaling_out.send(SignalingMessage::IceCandidate {
                            from: local_id_hex,
                            to: remote_id_hex,
                            candidate: c.to_sdp_string(),
                            sdp_m_line_index: Some(0),
                        });
                    } else {
                        let _ = state.signaling_out.send(SignalingMessage::Offer {
                            from: local_id_hex,
                            to: remote_id_hex,
                            sdp: offer_sdp,
                        });
                    }

                    state.peers.insert(remote_id, PeerState {
                        rtc,
                        channel_id: None,
                        conn_state: ConnState::Connecting,
                        remote_addr: None,
                        waker: Some(cx.waker().clone()),
                        pending_offer: Some(pending),
                    });

                    Poll::Pending
                }
                Err(e) => Poll::Ready(Err(e)),
            }
        }
    }
}
