# iroh-webrtc Implementation Plan

Generic WebRTC custom transport for iroh. No IMX dependencies.

## Current State

Scaffold complete with correct architecture:
- `src/lib.rs` — TRANSPORT_ID, CustomAddr helpers, re-exports
- `src/signaling.rs` — SignalingMessage wire format
- `src/lookup.rs` — WebRtcAddressLookup (AddressLookup impl)
- `src/native.rs` — str0m backend stubs (CustomTransport/Endpoint/Sender)
- `src/browser.rs` — web-sys backend stubs (same traits)

Follows iroh's own TestTransport pattern (iroh 0.97 src/test_utils/test_transport.rs).

## Phase 1: Native Backend (str0m)

### 1.1 — UDP socket + str0m setup
- [ ] In `WebRtcTransport::new()`, bind a UDP socket (port 0)
- [ ] Store local socket addr; publish it via local_addrs watcher
- [ ] Create helper: `new_rtc(stun_servers) -> str0m::Rtc` with ICE config

### 1.2 — Signaling processing in poll_recv
- [ ] In `poll_recv`, drain `signaling_in` channel
- [ ] On `SignalingMessage::Offer`: create Rtc as answerer, set remote SDP, generate answer, send via `signaling_out`, store PeerState
- [ ] On `SignalingMessage::Answer`: look up PeerState, set remote SDP on Rtc
- [ ] On `SignalingMessage::IceCandidate`: add candidate to Rtc

### 1.3 — str0m I/O loop in poll_recv
- [ ] Poll UDP socket for incoming packets
- [ ] Route packets to correct Rtc by source addr (need addr→EndpointId mapping)
- [ ] Call `rtc.handle_input(data, source_addr)` to feed str0m
- [ ] Call `rtc.poll_output()` to get events:
  - `Event::DataChannelData` → push to recv_queue as IncomingPacket
  - `Event::DataChannelOpen` → mark peer as connected, wake pending senders
  - `Event::IceCandidate` → send via signaling_out
- [ ] Drive str0m timers via `rtc.poll_timeout()`

### 1.4 — poll_send with str0m
- [ ] On `poll_send(dst, transmit)`: extract EndpointId from CustomAddr
- [ ] If peer has open data channel: write transmit.contents (split by segment_size), drive str0m output → send UDP packets, return Ready
- [ ] If peer exists but connecting: queue packet, register waker, return Pending
- [ ] If no peer: create Rtc as offerer, create data channel, generate SDP offer, send via signaling_out, store PeerState(connecting), register waker, return Pending

### 1.5 — Connection lifecycle
- [ ] Handle data channel close / ICE failure → remove PeerState, clean up
- [ ] Handle Rtc timeout → close stale connections
- [ ] Expose connection state for debugging (tracing events)

## Phase 2: Browser Backend (web-sys)

### 2.1 — RTCPeerConnection creation
- [ ] Helper: `create_peer_connection(stun_servers) -> RtcPeerConnection` via web_sys
- [ ] SendWrapper for !Send web-sys types (same pattern as iroh relay transport)
- [ ] Wire `onicecandidate` callback → signaling_out
- [ ] Wire `ondatachannel` callback (for answerer side)

### 2.2 — Signaling processing
- [ ] Process signaling_in in poll_recv
- [ ] On Offer: create RTCPeerConnection, setRemoteDescription, createAnswer, setLocalDescription, send answer via signaling_out
- [ ] On Answer: setRemoteDescription on existing connection
- [ ] On IceCandidate: addIceCandidate

### 2.3 — Data channel send/recv
- [ ] On `poll_send` for new peer: createDataChannel, createOffer, setLocalDescription, send offer via signaling_out
- [ ] Wire `datachannel.onmessage` → push to recv_queue_tx
- [ ] On `poll_send` for open channel: `datachannel.send(data)` → Ready
- [ ] On `poll_recv`: drain recv_queue (same pattern as native, already written)

### 2.4 — WASM build verification
- [ ] Verify crate compiles to wasm32-unknown-unknown
- [ ] Verify web-sys features are sufficient
- [ ] Test with wasm-pack or nix WASM build

## Phase 3: Testing

### 3.1 — Native-to-native in-process test
- [ ] Two WebRtcTransport instances with mpsc signaling (bridge sig_out_rx → sig_in_tx)
- [ ] Both add each other via lookup.add_peer()
- [ ] Build iroh Endpoints with custom transport + lookup
- [ ] Echo test: connect, open bi-stream, send data, verify response
- [ ] Follow TestTransport test pattern exactly

### 3.2 — Path selection test
- [ ] Endpoint with both relay and WebRTC custom transport
- [ ] Verify WebRTC path is selected (primary) over relay (backup)
- [ ] Verify relay fallback when WebRTC signaling fails

### 3.3 — Signaling edge cases
- [ ] Test: signaling arrives out of order (answer before offer completes)
- [ ] Test: ICE candidate trickle timing
- [ ] Test: peer disconnects mid-signaling
- [ ] Test: multiple concurrent connections

## Phase 4: Polish

- [ ] Cargo.toml version pins verified against iroh 0.97
- [ ] `cargo check` passes (native)
- [ ] `cargo check --target wasm32-unknown-unknown` passes (browser)
- [ ] Minimal example in examples/echo.rs
- [ ] README with usage docs
