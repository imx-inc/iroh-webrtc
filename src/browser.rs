//! Browser (WASM) WebRTC transport using web-sys RTCPeerConnection.
//!
//! Uses the browser's built-in WebRTC stack. A `wasm_bindgen_futures::spawn_local`
//! task drives signaling; JS callbacks on data channels push received packets
//! into the recv queue for `poll_recv`.
//!
//! # Send/Sync in WASM
//!
//! iroh's custom transport traits require `Send + Sync + 'static`.
//! `web_sys::RtcPeerConnection` is `!Send`.
//!
//! This is safe: WASM is single-threaded. We use `send_wrapper::SendWrapper`
//! to satisfy the trait bounds — the same pattern iroh's own relay transport
//! uses for `web_sys::WebSocket`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::num::NonZeroUsize;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use iroh::endpoint::transports::{Addr, CustomEndpoint, CustomSender, CustomTransport, Transmit};
use iroh_base::{CustomAddr, EndpointId};
use send_wrapper::SendWrapper;
use tokio::sync::mpsc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

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

/// State for a single browser-side WebRTC peer connection.
/// All web_sys types live here, wrapped in SendWrapper at the container level.
struct BrowserPeerState {
    pc: web_sys::RtcPeerConnection,
    dc: Option<web_sys::RtcDataChannel>,
    conn_state: ConnState,
    waker: Option<Waker>,
    // Hold closures alive so they don't get GC'd.
    _closures: Vec<Closure<dyn FnMut(JsValue)>>,
}

impl std::fmt::Debug for BrowserPeerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserPeerState")
            .field("conn_state", &self.conn_state)
            .field("has_dc", &self.dc.is_some())
            .finish()
    }
}

/// Interior state shared between endpoint, sender, and JS callbacks.
/// Lives inside `SendWrapper<Rc<RefCell<_>>>` — single-threaded WASM only.
struct BrowserState {
    local_id: EndpointId,
    peers: HashMap<EndpointId, BrowserPeerState>,
    stun_servers: Vec<String>,
    signaling_out: mpsc::UnboundedSender<SignalingMessage>,
    recv_queue_tx: mpsc::UnboundedSender<IncomingPacket>,
}

impl BrowserState {
    fn local_id_hex(&self) -> String {
        format!("{}", self.local_id)
    }

    /// Create an RTCPeerConnection with our STUN config.
    fn create_pc(&self) -> Result<web_sys::RtcPeerConnection, JsValue> {
        let config = web_sys::RtcConfiguration::new();

        if !self.stun_servers.is_empty() {
            let ice_servers = js_sys::Array::new();
            for url in &self.stun_servers {
                let server = js_sys::Object::new();
                let urls = js_sys::Array::new();
                urls.push(&JsValue::from_str(url));
                js_sys::Reflect::set(&server, &"urls".into(), &urls)?;
                ice_servers.push(&server);
            }
            config.set_ice_servers(&ice_servers);
        }

        web_sys::RtcPeerConnection::new_with_configuration(&config)
    }
}

/// Wrapper that makes the browser state Send+Sync.
/// Safe because wasm32 is single-threaded.
#[derive(Clone)]
struct SharedBrowserState(SendWrapper<Rc<RefCell<BrowserState>>>);

// SendWrapper already provides Send+Sync, but we need the outer struct
// to be Debug for the trait bounds.
impl std::fmt::Debug for SharedBrowserState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedBrowserState").finish()
    }
}

impl SharedBrowserState {
    fn new(state: BrowserState) -> Self {
        Self(SendWrapper::new(Rc::new(RefCell::new(state))))
    }

    fn borrow(&self) -> std::cell::Ref<'_, BrowserState> {
        self.0.borrow()
    }

    fn borrow_mut(&self) -> std::cell::RefMut<'_, BrowserState> {
        self.0.borrow_mut()
    }
}

// ---------------------------------------------------------------------------
// WebRtcTransport
// ---------------------------------------------------------------------------

/// WebRTC custom transport for iroh (browser backend).
pub struct WebRtcTransport {
    state: SharedBrowserState,
    signaling_in: Arc<SendWrapper<RefCell<Option<mpsc::UnboundedReceiver<SignalingMessage>>>>>,
    recv_queue_rx: Arc<SendWrapper<RefCell<mpsc::UnboundedReceiver<IncomingPacket>>>>,
}

impl WebRtcTransport {
    /// Create a new browser WebRTC transport and its companion address lookup.
    pub fn new(
        local_id: EndpointId,
        stun_servers: Vec<String>,
        signaling_out: mpsc::UnboundedSender<SignalingMessage>,
        signaling_in: mpsc::UnboundedReceiver<SignalingMessage>,
    ) -> (Self, WebRtcAddressLookup) {
        let (recv_tx, recv_rx) = mpsc::unbounded_channel();

        let state = SharedBrowserState::new(BrowserState {
            local_id,
            peers: HashMap::new(),
            stun_servers,
            signaling_out,
            recv_queue_tx: recv_tx,
        });

        let transport = Self {
            state,
            signaling_in: Arc::new(SendWrapper::new(RefCell::new(Some(signaling_in)))),
            recv_queue_rx: Arc::new(SendWrapper::new(RefCell::new(recv_rx))),
        };

        (transport, WebRtcAddressLookup::new())
    }
}

impl std::fmt::Debug for WebRtcTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebRtcTransport").finish()
    }
}

impl CustomTransport for WebRtcTransport {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        // Take the signaling receiver (bind once).
        let signaling_in = self
            .signaling_in
            .borrow_mut()
            .take()
            .ok_or_else(|| io::Error::other("WebRtcTransport::bind() called more than once"))?;

        // Spawn signaling processing loop.
        let state_clone = self.state.clone();
        wasm_bindgen_futures::spawn_local(async move {
            signaling_loop(state_clone, signaling_in).await;
        });

        tracing::info!("WebRTC browser transport bound");

        Ok(Box::new(WebRtcEndpoint {
            state: self.state.clone(),
            recv_queue_rx: Arc::clone(&self.recv_queue_rx),
            local_addrs: n0_watcher::Watchable::new(Vec::new()),
        }))
    }
}

// ---------------------------------------------------------------------------
// Signaling loop (runs via spawn_local)
// ---------------------------------------------------------------------------

async fn signaling_loop(
    state: SharedBrowserState,
    mut signaling_in: mpsc::UnboundedReceiver<SignalingMessage>,
) {
    while let Some(msg) = signaling_in.recv().await {
        handle_signaling(&state, msg);
    }
}

fn handle_signaling(state: &SharedBrowserState, msg: SignalingMessage) {
    match msg {
        SignalingMessage::Offer { from, sdp, .. } => {
            let Ok(remote_id) = from.parse::<EndpointId>() else {
                return;
            };
            tracing::debug!(remote = %remote_id.fmt_short(), "browser: received SDP offer");

            let state_clone = state.clone();
            let from_clone = from.clone();

            // Create peer connection and set up answer flow.
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = handle_offer(&state_clone, remote_id, &from_clone, &sdp).await {
                    tracing::warn!(remote = %remote_id.fmt_short(), "handle_offer failed: {:?}", e);
                }
            });
        }

        SignalingMessage::Answer { from, sdp, .. } => {
            let Ok(remote_id) = from.parse::<EndpointId>() else {
                return;
            };
            tracing::debug!(remote = %remote_id.fmt_short(), "browser: received SDP answer");

            let state_clone = state.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = handle_answer(&state_clone, remote_id, &sdp).await {
                    tracing::warn!(remote = %remote_id.fmt_short(), "handle_answer failed: {:?}", e);
                }
            });
        }

        SignalingMessage::IceCandidate {
            from,
            candidate,
            sdp_m_line_index,
            ..
        } => {
            let Ok(remote_id) = from.parse::<EndpointId>() else {
                return;
            };

            let state_clone = state.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) =
                    handle_ice_candidate(&state_clone, remote_id, &candidate, sdp_m_line_index)
                        .await
                {
                    tracing::warn!(remote = %remote_id.fmt_short(), "handle_ice failed: {:?}", e);
                }
            });
        }
    }
}

async fn handle_offer(
    state: &SharedBrowserState,
    remote_id: EndpointId,
    from_hex: &str,
    sdp: &str,
) -> Result<(), JsValue> {
    let (pc, closures) = {
        let s = state.borrow();
        let pc = s.create_pc()?;
        let closures = setup_pc_callbacks(state, &pc, remote_id);
        (pc, closures)
    };

    // Set remote description (the offer).
    let offer_init = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Offer);
    offer_init.set_sdp(sdp);
    let promise = pc.set_remote_description(&offer_init);
    wasm_bindgen_futures::JsFuture::from(promise).await?;

    // Create and set local description (the answer).
    let answer = wasm_bindgen_futures::JsFuture::from(pc.create_answer()).await?;
    let answer_sdp = js_sys::Reflect::get(&answer, &"sdp".into())?
        .as_string()
        .unwrap_or_default();

    let answer_init = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Answer);
    answer_init.set_sdp(&answer_sdp);
    let promise = pc.set_local_description(&answer_init);
    wasm_bindgen_futures::JsFuture::from(promise).await?;

    // Store peer state and send answer.
    {
        let mut s = state.borrow_mut();
        s.peers.insert(remote_id, BrowserPeerState {
            pc,
            dc: None,
            conn_state: ConnState::Connecting,
            waker: None,
            _closures: closures,
        });

        let _ = s.signaling_out.send(SignalingMessage::Answer {
            from: s.local_id_hex(),
            to: from_hex.to_string(),
            sdp: answer_sdp,
        });
    }

    Ok(())
}

async fn handle_answer(
    state: &SharedBrowserState,
    remote_id: EndpointId,
    sdp: &str,
) -> Result<(), JsValue> {
    let pc = {
        let s = state.borrow();
        let peer = s.peers.get(&remote_id).ok_or("no peer for answer")?;
        peer.pc.clone()
    };

    let answer_init = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Answer);
    answer_init.set_sdp(sdp);
    let promise = pc.set_remote_description(&answer_init);
    wasm_bindgen_futures::JsFuture::from(promise).await?;

    tracing::debug!(remote = %remote_id.fmt_short(), "browser: SDP answer applied");
    Ok(())
}

async fn handle_ice_candidate(
    state: &SharedBrowserState,
    remote_id: EndpointId,
    candidate: &str,
    sdp_m_line_index: Option<u16>,
) -> Result<(), JsValue> {
    let pc = {
        let s = state.borrow();
        let peer = s.peers.get(&remote_id).ok_or("no peer for ICE")?;
        peer.pc.clone()
    };

    let init = web_sys::RtcIceCandidateInit::new(candidate);
    if let Some(idx) = sdp_m_line_index {
        init.set_sdp_m_line_index(Some(idx));
    }
    let candidate = web_sys::RtcIceCandidate::new(&init)?;
    let promise = pc.add_ice_candidate_with_opt_rtc_ice_candidate(Some(&candidate));
    wasm_bindgen_futures::JsFuture::from(promise).await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// RTCPeerConnection callback setup
// ---------------------------------------------------------------------------

/// Wire up onicecandidate, ondatachannel, and onmessage callbacks.
/// Returns closures that must be kept alive for the lifetime of the connection.
fn setup_pc_callbacks(
    state: &SharedBrowserState,
    pc: &web_sys::RtcPeerConnection,
    remote_id: EndpointId,
) -> Vec<Closure<dyn FnMut(JsValue)>> {
    let mut closures = Vec::new();

    // onicecandidate → send via signaling
    {
        let state = state.clone();
        let cb = Closure::<dyn FnMut(JsValue)>::new(move |ev: JsValue| {
            let ev: web_sys::RtcPeerConnectionIceEvent = ev.unchecked_into();
            if let Some(candidate) = ev.candidate() {
                let s = state.borrow();
                let _ = s.signaling_out.send(SignalingMessage::IceCandidate {
                    from: s.local_id_hex(),
                    to: format!("{remote_id}"),
                    candidate: candidate.candidate(),
                    sdp_m_line_index: candidate.sdp_m_line_index(),
                });
            }
        });
        pc.set_onicecandidate(Some(cb.as_ref().unchecked_ref()));
        closures.push(cb);
    }

    // ondatachannel → store channel, wire onmessage
    {
        let state = state.clone();
        let cb = Closure::<dyn FnMut(JsValue)>::new(move |ev: JsValue| {
            let ev: web_sys::RtcDataChannelEvent = ev.unchecked_into();
            let dc = ev.channel();
            tracing::info!(remote = %remote_id.fmt_short(), "browser: data channel open (answerer)");
            setup_dc_onmessage(&state, &dc, remote_id);

            let mut s = state.borrow_mut();
            if let Some(peer) = s.peers.get_mut(&remote_id) {
                peer.dc = Some(dc);
                peer.conn_state = ConnState::Connected;
                if let Some(w) = peer.waker.take() {
                    w.wake();
                }
            }
        });
        pc.set_ondatachannel(Some(cb.as_ref().unchecked_ref()));
        closures.push(cb);
    }

    closures
}

/// Wire a data channel's onmessage to push into recv_queue.
fn setup_dc_onmessage(
    state: &SharedBrowserState,
    dc: &web_sys::RtcDataChannel,
    remote_id: EndpointId,
) {
    let state = state.clone();
    let from_addr = to_custom_addr(remote_id);
    let cb = Closure::<dyn FnMut(JsValue)>::new(move |ev: JsValue| {
        let ev: web_sys::MessageEvent = ev.unchecked_into();
        // Data can be ArrayBuffer or String.
        let data = ev.data();
        let bytes = if let Some(ab) = data.dyn_ref::<js_sys::ArrayBuffer>() {
            let arr = js_sys::Uint8Array::new(ab);
            let mut buf = vec![0u8; arr.length() as usize];
            arr.copy_to(&mut buf);
            buf
        } else if let Some(s) = data.as_string() {
            s.into_bytes()
        } else {
            return;
        };

        let s = state.borrow();
        let _ = s.recv_queue_tx.send(IncomingPacket {
            data: Bytes::from(bytes),
            from: from_addr.clone(),
        });
    });
    dc.set_onmessage(Some(cb.as_ref().unchecked_ref()));
    // Leak the closure — it needs to live as long as the data channel.
    // In a production version, store it in PeerState._closures.
    cb.forget();
}

// ---------------------------------------------------------------------------
// Offerer-side: create PC + data channel (called from poll_send)
// ---------------------------------------------------------------------------

fn initiate_connection(state: &SharedBrowserState, remote_id: EndpointId) {
    let state_clone = state.clone();
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = do_initiate(&state_clone, remote_id).await {
            tracing::warn!(remote = %remote_id.fmt_short(), "initiate failed: {:?}", e);
        }
    });
}

async fn do_initiate(state: &SharedBrowserState, remote_id: EndpointId) -> Result<(), JsValue> {
    let (pc, dc, closures) = {
        let s = state.borrow();
        let pc = s.create_pc()?;
        let closures = setup_pc_callbacks(state, &pc, remote_id);

        // Create data channel (offerer side).
        let dc = pc.create_data_channel(DATA_CHANNEL_LABEL);
        setup_dc_onmessage(state, &dc, remote_id);

        // Wire onopen to mark connected.
        {
            let state = state.clone();
            let onopen = Closure::<dyn FnMut(JsValue)>::new(move |_: JsValue| {
                tracing::info!(remote = %remote_id.fmt_short(), "browser: data channel open (offerer)");
                let mut s = state.borrow_mut();
                if let Some(peer) = s.peers.get_mut(&remote_id) {
                    peer.conn_state = ConnState::Connected;
                    if let Some(w) = peer.waker.take() {
                        w.wake();
                    }
                }
            });
            dc.set_onopen(Some(onopen.as_ref().unchecked_ref()));
            onopen.forget();
        }

        (pc, dc, closures)
    };

    // Create offer.
    let offer = wasm_bindgen_futures::JsFuture::from(pc.create_offer()).await?;
    let offer_sdp = js_sys::Reflect::get(&offer, &"sdp".into())?
        .as_string()
        .unwrap_or_default();

    // Set local description.
    let offer_init = web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Offer);
    offer_init.set_sdp(&offer_sdp);
    let promise = pc.set_local_description(&offer_init);
    wasm_bindgen_futures::JsFuture::from(promise).await?;

    // Store peer state and send offer.
    {
        let mut s = state.borrow_mut();

        let _ = s.signaling_out.send(SignalingMessage::Offer {
            from: s.local_id_hex(),
            to: format!("{remote_id}"),
            sdp: offer_sdp,
        });

        s.peers.insert(remote_id, BrowserPeerState {
            pc,
            dc: Some(dc),
            conn_state: ConnState::Connecting,
            waker: None,
            _closures: closures,
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// WebRtcEndpoint (poll_recv)
// ---------------------------------------------------------------------------

struct WebRtcEndpoint {
    state: SharedBrowserState,
    recv_queue_rx: Arc<SendWrapper<RefCell<mpsc::UnboundedReceiver<IncomingPacket>>>>,
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
            state: self.state.clone(),
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

        // Drain recv_queue (fed by data channel onmessage callbacks).
        let mut rx = self.recv_queue_rx.borrow_mut();
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
// WebRtcSender (poll_send)
// ---------------------------------------------------------------------------

struct WebRtcSender {
    state: SharedBrowserState,
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

        let mut s = self.state.borrow_mut();

        if let Some(peer) = s.peers.get_mut(&remote_id) {
            match peer.conn_state {
                ConnState::Connected => {
                    if let Some(ref dc) = peer.dc {
                        // Send data via the data channel.
                        let seg = transmit.segment_size.unwrap_or(transmit.contents.len());
                        for chunk in transmit.contents.chunks(seg) {
                            let arr = js_sys::Uint8Array::from(chunk);
                            if let Err(e) = dc.send_with_array_buffer_view(&arr) {
                                return Poll::Ready(Err(io::Error::other(format!(
                                    "dataChannel.send failed: {:?}",
                                    e
                                ))));
                            }
                        }
                        Poll::Ready(Ok(()))
                    } else {
                        peer.waker = Some(cx.waker().clone());
                        Poll::Pending
                    }
                }
                ConnState::Connecting => {
                    peer.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        } else {
            // New peer — initiate WebRTC connection.
            tracing::debug!(remote = %remote_id.fmt_short(), "browser: initiating WebRTC signaling");

            // Insert placeholder so we don't initiate twice.
            let pc = s.create_pc().map_err(|e| {
                io::Error::other(format!("create_pc failed: {:?}", e))
            })?;
            s.peers.insert(remote_id, BrowserPeerState {
                pc,
                dc: None,
                conn_state: ConnState::Connecting,
                waker: Some(cx.waker().clone()),
                _closures: vec![],
            });
            drop(s); // Release borrow before spawn_local.

            initiate_connection(&self.state, remote_id);

            Poll::Pending
        }
    }
}
