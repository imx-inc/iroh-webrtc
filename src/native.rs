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

/// Perform a blocking STUN binding request to discover our public IP.
/// Returns a server-reflexive candidate if successful.
fn discover_srflx(stun_servers: &[String], local_port: u16) -> Option<str0m::Candidate> {
    for server_uri in stun_servers {
        // Parse "stun:host:port" format.
        let Some(addr_str) = server_uri.strip_prefix("stun:") else {
            continue;
        };

        // Resolve DNS — may return multiple addresses (IPv4 + IPv6).
        // Try each in turn; our response parser only handles IPv4 so we
        // prefer IPv4 addresses first.
        let addrs: Vec<SocketAddr> = match addr_str.to_socket_addrs() {
            Ok(addrs) => {
                let mut v: Vec<_> = addrs.collect();
                // IPv4 first.
                v.sort_by_key(|a| if a.is_ipv4() { 0 } else { 1 });
                v
            }
            Err(e) => {
                tracing::debug!(%server_uri, %e, "STUN DNS resolution failed");
                continue;
            }
        };

        if addrs.is_empty() {
            tracing::debug!(%server_uri, "STUN resolved to no addresses");
            continue;
        }

        tracing::debug!(%server_uri, resolved = ?addrs, "STUN resolving");

        for stun_addr in addrs {
            // Bind socket matching the address family.
            let bind_addr = if stun_addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
            let sock = match std::net::UdpSocket::bind(bind_addr) {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(%e, %bind_addr, "STUN socket bind failed");
                    continue;
                }
            };
            let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));

            let mut req = [0u8; 20];
            req[0] = 0x00; req[1] = 0x01;
            req[2] = 0x00; req[3] = 0x00;
            req[4] = 0x21; req[5] = 0x12; req[6] = 0xA4; req[7] = 0x42;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            req[8..20].copy_from_slice(&now.to_le_bytes()[..12]);

            if let Err(e) = sock.send_to(&req, stun_addr) {
                tracing::debug!(%stun_addr, %e, "STUN send failed");
                continue;
            }
            tracing::debug!(%stun_addr, "STUN request sent");

            let mut buf = [0u8; 256];
            let n = match sock.recv(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    tracing::debug!(%stun_addr, %e, "STUN recv timeout/error");
                    continue;
                }
            };

            if let Some(c) = parse_stun_response(&buf[..n], stun_addr, local_port) {
                return Some(c);
            }
        }
    }
    None
}

/// Parse a STUN Binding Success Response and extract XOR-MAPPED-ADDRESS (IPv4).
fn parse_stun_response(buf: &[u8], stun_addr: SocketAddr, local_port: u16) -> Option<str0m::Candidate> {
    if buf.len() < 20 { return None; }
    // Verify Binding Success Response (0x0101).
    if buf[0] != 0x01 || buf[1] != 0x01 { return None; }

    let magic = [0x21, 0x12, 0xA4, 0x42];
    let mut offset = 20;
    while offset + 4 <= buf.len() {
        let attr_type = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
        let attr_len = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
        let attr_start = offset + 4;

        if attr_type == 0x0020 && attr_len >= 8 {
            let family = buf[attr_start + 1];
            if family == 0x01 {
                let xor_port = u16::from_be_bytes([buf[attr_start + 2], buf[attr_start + 3]]) ^ 0x2112;
                let xor_ip = [
                    buf[attr_start + 4] ^ magic[0],
                    buf[attr_start + 5] ^ magic[1],
                    buf[attr_start + 6] ^ magic[2],
                    buf[attr_start + 7] ^ magic[3],
                ];
                let ip = std::net::Ipv4Addr::new(xor_ip[0], xor_ip[1], xor_ip[2], xor_ip[3]);
                let public_addr = SocketAddr::new(ip.into(), xor_port);
                let local_addr = SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), local_port);
                tracing::info!(%public_addr, %stun_addr, "STUN: got public address");
                return str0m::Candidate::server_reflexive(public_addr, local_addr, "udp").ok();
            }
        }
        offset = attr_start + ((attr_len + 3) & !3);
    }
    None
}

use std::net::ToSocketAddrs;

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
    /// The real bound socket addr (0.0.0.0:port).
    local_addr: SocketAddr,
    /// The address to use as `destination` when feeding packets to str0m.
    /// Must match a local candidate's addr so str0m can pair it.
    local_recv_addr: SocketAddr,
    /// All local host candidates (real interface IPs + bound port).
    local_candidates: Vec<str0m::Candidate>,
    /// Server-reflexive candidate (public IP from STUN), if discovered.
    srflx_candidate: Option<str0m::Candidate>,
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
        // Build Rtc with tuned ICE timeouts for residential NAT traversal.
        // Defaults: initial_rto=250ms, max_rto=3s, max_retransmits=9 (~30s total).
        // We give it more headroom: more retransmits and longer max RTO,
        // since home NATs can have intermittent packet loss.
        let mut cfg = Rtc::builder();
        cfg.set_max_stun_retransmits(20);
        cfg.set_max_stun_rto(Duration::from_secs(5));
        cfg.build(Instant::now())
    }

    /// Add all known local + srflx candidates to an Rtc instance,
    /// and send them as ICE signaling messages.
    fn add_candidates_and_signal(
        &self,
        rtc: &mut Rtc,
        local_id_hex: &str,
        remote_id_hex: &str,
    ) {
        for c in &self.local_candidates {
            rtc.add_local_candidate(c.clone());
            let _ = self.signaling_out.send(SignalingMessage::IceCandidate {
                from: local_id_hex.to_string(),
                to: remote_id_hex.to_string(),
                candidate: c.to_sdp_string(),
                sdp_m_line_index: Some(0),
            });
        }
        if let Some(ref c) = self.srflx_candidate {
            rtc.add_local_candidate(c.clone());
            let _ = self.signaling_out.send(SignalingMessage::IceCandidate {
                from: local_id_hex.to_string(),
                to: remote_id_hex.to_string(),
                candidate: c.to_sdp_string(),
                sdp_m_line_index: Some(0),
            });
        }
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

        let mut transmit_count = 0u32;
        loop {
            match peer.rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    transmit_count += 1;
                    match udp_socket.try_send_to(&t.contents, t.destination) {
                        Ok(n) => tracing::trace!(remote = %remote_id.fmt_short(), dest = %t.destination, bytes = n, "UDP sent"),
                        Err(e) => tracing::warn!(remote = %remote_id.fmt_short(), dest = %t.destination, %e, "UDP send failed"),
                    }
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
                    tracing::info!(remote = %remote_id.fmt_short(), ?s, "ICE state change");
                }
                Ok(Output::Event(Event::Connected)) => {
                    tracing::info!(remote = %remote_id.fmt_short(), "ICE + DTLS connected");
                }
                Ok(Output::Event(ev)) => {
                    tracing::info!(remote = %remote_id.fmt_short(), ?ev, "str0m event");
                }
                Ok(Output::Timeout(_)) => break,
                Err(e) => {
                    tracing::warn!(remote = %remote_id.fmt_short(), %e, "str0m error");
                    break;
                }
            }
        }
        if transmit_count > 0 {
            tracing::debug!(remote = %remote_id.fmt_short(), count = transmit_count, "drive_peer flushed UDP transmits");
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
                    tracing::warn!(from, "bad remote endpoint id in offer");
                    return;
                };
                tracing::info!(remote = %remote_id.fmt_short(), sdp_len = sdp.len(), "received SDP offer");

                match self.create_answerer(&sdp) {
                    Ok((mut rtc, answer_sdp)) => {
                        tracing::info!(remote = %remote_id.fmt_short(), answer_len = answer_sdp.len(), "created answer");
                        let local_id_hex = self.local_id_hex();
                        let local_count = self.local_candidates.len();
                        let has_srflx = self.srflx_candidate.is_some();
                        self.add_candidates_and_signal(&mut rtc, &local_id_hex, &from);
                        tracing::info!(remote = %remote_id.fmt_short(), local_count, has_srflx, "added local candidates to peer + sent via signaling");
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
                    Err(e) => tracing::warn!(remote = %remote_id.fmt_short(), %e, "create_answerer failed"),
                }
            }

            SignalingMessage::Answer { from, sdp, .. } => {
                let Ok(remote_id) = from.parse::<EndpointId>() else {
                    tracing::warn!(from, "bad remote endpoint id in answer");
                    return;
                };
                if let Some(peer) = self.peers.get_mut(&remote_id) {
                    if let Some(pending) = peer.pending_offer.take() {
                        match str0m::change::SdpAnswer::from_sdp_string(&sdp) {
                            Ok(answer) => {
                                if let Err(e) = peer.rtc.sdp_api().accept_answer(pending, answer) {
                                    tracing::warn!(remote = %remote_id.fmt_short(), %e, "accept_answer failed");
                                } else {
                                    tracing::info!(remote = %remote_id.fmt_short(), "SDP answer accepted");
                                }
                            }
                            Err(e) => tracing::warn!(remote = %remote_id.fmt_short(), %e, "invalid SDP answer"),
                        }
                    } else {
                        tracing::warn!(remote = %remote_id.fmt_short(), "received answer but no pending offer");
                    }
                } else {
                    tracing::warn!(remote = %remote_id.fmt_short(), "received answer for unknown peer");
                }
                self.drive_peer(&remote_id, udp_socket);
            }

            SignalingMessage::IceCandidate { from, candidate, .. } => {
                let Ok(remote_id) = from.parse::<EndpointId>() else {
                    tracing::warn!(from, "bad remote endpoint id in ice");
                    return;
                };
                if let Some(peer) = self.peers.get_mut(&remote_id) {
                    match str0m::Candidate::from_sdp_string(&candidate) {
                        Ok(c) => {
                            let addr = c.addr();
                            tracing::info!(remote = %remote_id.fmt_short(), %addr, kind = ?c.kind(), "added remote ICE candidate");
                            peer.rtc.add_remote_candidate(c);
                            self.addr_to_peer.insert(addr, remote_id);
                            peer.remote_addr = Some(addr);
                        }
                        Err(e) => tracing::warn!(remote = %remote_id.fmt_short(), candidate, %e, "failed to parse remote ICE candidate"),
                    }
                } else {
                    tracing::warn!(remote = %remote_id.fmt_short(), candidate, "received ICE for unknown peer");
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
                        let local_addr = s.local_recv_addr;

                        // Route by known source addr.
                        let remote_id = s.addr_to_peer.get(&source).copied();
                        if let Some(id) = remote_id {
                            tracing::trace!(%source, len, remote = %id.fmt_short(), "UDP recv (known peer)");
                            if let Some(peer) = s.peers.get_mut(&id) {
                                if let Ok(recv) = Receive::new(Protocol::Udp, source, local_addr, &data) {
                                    let _ = peer.rtc.handle_input(Input::Receive(Instant::now(), recv));
                                }
                            }
                            s.drive_peer(&id, &udp_socket);
                        } else {
                            tracing::info!(%source, len, peers = s.peers.len(), "UDP recv (unknown source, trying all peers)");
                            // Unknown source — try all peers.
                            let ids: Vec<_> = s.peers.keys().copied().collect();
                            let mut matched = false;
                            for id in ids {
                                if let Some(peer) = s.peers.get_mut(&id) {
                                    if let Ok(recv) = Receive::new(Protocol::Udp, source, local_addr, &data) {
                                        if peer.rtc.handle_input(Input::Receive(Instant::now(), recv)).is_ok() {
                                            tracing::info!(%source, remote = %id.fmt_short(), "UDP source matched to peer");
                                            s.addr_to_peer.insert(source, id);
                                            s.drive_peer(&id, &udp_socket);
                                            matched = true;
                                            break;
                                        }
                                    }
                                }
                            }
                            if !matched {
                                tracing::warn!(%source, "UDP packet didn't match any peer");
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
        // Bind UDP socket on all interfaces for str0m I/O.
        let std_socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        std_socket.set_nonblocking(true)?;
        let local_addr = std_socket.local_addr()?;
        let port = local_addr.port();
        let udp_socket = Arc::new(tokio::net::UdpSocket::from_std(std_socket)?);

        // Gather host candidates from local network interfaces.
        let mut local_candidates = Vec::new();

        // Always add localhost candidate (needed for same-machine connections).
        let localhost_addr = SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port);
        if let Ok(c) = str0m::Candidate::host(localhost_addr, "udp") {
            local_candidates.push(c);
        }

        // Discover the default LAN IP by connecting to a public address.
        if let Ok(ifaces) = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| {
                s.connect("8.8.8.8:80")?;
                s.local_addr()
            })
        {
            let addr = SocketAddr::new(ifaces.ip(), port);
            if !addr.ip().is_loopback() {
                if let Ok(c) = str0m::Candidate::host(addr, "udp") {
                    tracing::info!(%addr, "WebRTC LAN host candidate");
                    local_candidates.push(c);
                }
            }
        }

        // STUN: discover server-reflexive (public) candidate.
        let srflx_candidate = discover_srflx(&self.stun_servers, port);
        if let Some(ref c) = srflx_candidate {
            tracing::info!(candidate = %c.to_sdp_string(), "WebRTC srflx candidate");
        }

        tracing::info!(%local_addr, candidates = local_candidates.len(), has_srflx = srflx_candidate.is_some(), "WebRTC transport bound");

        // Use the first candidate's address as the recv destination for str0m.
        let local_recv_addr = local_candidates
            .first()
            .map(|c| c.addr())
            .unwrap_or(local_addr);

        let state = Arc::new(Mutex::new(SharedState {
            local_id: Some(self.local_id),
            peers: HashMap::new(),
            addr_to_peer: HashMap::new(),
            signaling_out: self.signaling_out.clone(),
            local_addr,
            local_recv_addr,
            local_candidates,
            srflx_candidate,
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

                    // Send offer first, then ICE candidates.
                    let _ = state.signaling_out.send(SignalingMessage::Offer {
                        from: local_id_hex.clone(),
                        to: remote_id_hex.clone(),
                        sdp: offer_sdp,
                    });
                    state.add_candidates_and_signal(&mut rtc, &local_id_hex, &remote_id_hex);

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
