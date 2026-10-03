//! Socket channels: the per-socket conversation with a client (see the
//! `vproto::net` module docs for the protocol and its credit-based flow
//! control).

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::net::SocketAddr;

use vabi::Error;
use vipc::{Bytes, Encode, Encoder};
use vnetstack::{PingEvent, SockId, Stack, TcpState};
use vproto::net::{MAX_SEND, NetError, RECEIVE_WINDOW, SocketEvent, SocketRequest};
use vrt::object::Channel;

/// Bytes a client may send ahead of what the stack has taken.
pub const SEND_WINDOW: u32 = 64 * 1024;
/// Largest received chunk per message.
const CHUNK: usize = 16 * 1024;
/// Return send credit once this much accumulated (or the queue drained).
const CREDIT_BATCH: u32 = 16 * 1024;
/// Most requests handled per socket per loop iteration.
const REQUEST_BUDGET: usize = 64;

pub enum Kind {
    Tcp {
        /// `Connected` (or `Error` while connecting) was sent.
        announced: bool,
        eof_sent: bool,
        /// Data from the client not yet taken by the stack.
        pending: VecDeque<u8>,
        shutdown_requested: bool,
    },
    Listener,
    Udp {
        pending: VecDeque<(SocketAddr, Vec<u8>)>,
    },
    Ping,
}

/// One socket channel.
pub struct Conn {
    pub channel: Channel,
    pub id: SockId,
    pub kind: Kind,
    /// Send credit granted but not yet used by the client.
    pub credit_out: u32,
    /// Credit earned back (bytes the stack took), not yet returned.
    pub credit_due: u32,
    /// Received bytes sent to the client and not yet acknowledged.
    pub rx_outstanding: u32,
    /// The client's queue was full; wait for it to drain.
    pub blocked: bool,
    /// The socket is finished (error reported or closed): drop the
    /// connection once the client closes it.
    pub done: bool,
    /// Which client created it (for listings).
    pub owner: u64,
}

/// Writes one event to a socket channel. Returns `false` if the client's
/// queue is full (the caller retries later) or the client is gone.
fn send(conn: &mut Conn, ev: SocketEvent) -> bool {
    let mut e = Encoder::with_header(0, 0, vipc::FLAG_EVENT);
    ev.encode(&mut e);
    match conn.channel.write(&e.bytes, e.handles) {
        Ok(()) => true,
        Err(Error::ShouldWait) => {
            conn.blocked = true;
            false
        }
        Err(_) => {
            conn.done = true;
            false
        }
    }
}

impl Conn {
    pub fn new(channel: Channel, id: SockId, kind: Kind, owner: u64) -> Conn {
        Conn { channel, id, kind, credit_out: 0, credit_due: 0, rx_outstanding: 0, blocked: false, done: false, owner }
    }

    fn fail(&mut self, error: NetError) {
        let _ = send(self, SocketEvent::Error { error });
        self.done = true;
    }

    /// Handles the client's requests. Returns `false` when the client closed
    /// the channel.
    pub fn handle_requests(&mut self, stack: &mut Stack) -> bool {
        for _ in 0..REQUEST_BUDGET {
            // Do not take more data than the credit allows us to buffer.
            let msg = match self.channel.read() {
                Ok(m) => m,
                Err(Error::ShouldWait) => return true,
                Err(_) => return false,
            };
            let req = match vipc::decode_event::<SocketRequest>(msg) {
                Ok((_, r)) => r,
                Err(_) => {
                    self.fail(NetError::ProtocolViolation);
                    return true;
                }
            };
            if self.done {
                continue;
            }
            self.handle(stack, req);
        }
        true
    }

    fn handle(&mut self, stack: &mut Stack, req: SocketRequest) {
        match (&mut self.kind, req) {
            (Kind::Tcp { pending, shutdown_requested, .. }, SocketRequest::Send { data }) => {
                let n = data.0.len();
                if n > MAX_SEND || n as u32 > self.credit_out || *shutdown_requested {
                    return self.fail(NetError::ProtocolViolation);
                }
                self.credit_out -= n as u32;
                pending.extend(data.0);
            }
            (Kind::Udp { pending }, SocketRequest::SendTo { dest, data }) => {
                let n = data.0.len();
                if n > vproto::net::MAX_DATAGRAM || n as u32 > self.credit_out {
                    return self.fail(NetError::ProtocolViolation);
                }
                self.credit_out -= n as u32;
                pending.push_back((dest, data.0));
            }
            (Kind::Tcp { .. } | Kind::Udp { .. }, SocketRequest::Consumed { bytes }) => {
                self.rx_outstanding = self.rx_outstanding.saturating_sub(bytes);
            }
            (Kind::Tcp { shutdown_requested, .. }, SocketRequest::Shutdown {}) => {
                *shutdown_requested = true;
            }
            (Kind::Tcp { .. }, SocketRequest::Abort {}) => {
                stack.tcp_abort(self.id);
            }
            (Kind::Tcp { .. }, SocketRequest::SetNoDelay { nodelay }) => {
                stack.tcp_set_nodelay(self.id, nodelay);
            }
            (Kind::Ping, SocketRequest::Echo { dest, seq, ttl, size }) => {
                if let Err(error) = stack.ping_send(self.id, dest, seq, ttl, size) {
                    let _ = send(self, SocketEvent::EchoFailed { seq, from: None, error });
                }
            }
            _ => self.fail(NetError::ProtocolViolation),
        }
    }

    /// Moves data between the client and the stack and reports state
    /// changes. Returns `true` if it put data into the stack (so the stack
    /// should be polled again).
    pub fn pump(&mut self, stack: &mut Stack, accepted: &mut Vec<Conn>) -> bool {
        if self.done {
            return false;
        }
        self.blocked = false;
        match self.kind {
            Kind::Tcp { .. } => self.pump_tcp(stack),
            Kind::Listener => {
                self.pump_listener(stack, accepted);
                false
            }
            Kind::Udp { .. } => self.pump_udp(stack),
            Kind::Ping => {
                for ev in stack.ping_events(self.id) {
                    let ev = match ev {
                        PingEvent::Reply { from, seq, size, rtt_us } => {
                            SocketEvent::EchoReply { from, seq, ttl: 0, size, rtt_us }
                        }
                        PingEvent::Failed { seq, error } => SocketEvent::EchoFailed { seq, from: None, error },
                    };
                    if !send(self, ev) {
                        break;
                    }
                }
                false
            }
        }
    }

    fn pump_tcp(&mut self, stack: &mut Stack) -> bool {
        let Some(view) = stack.tcp_view(self.id) else {
            self.fail(NetError::Closed);
            return false;
        };
        let Kind::Tcp { announced, .. } = &self.kind else { return false };
        let announced = *announced;
        match view.state {
            TcpState::Connecting => return false,
            TcpState::Closed { error: Some(error) } => {
                self.fail(error);
                return false;
            }
            TcpState::Open { local, remote } if !announced => {
                self.credit_out = SEND_WINDOW;
                if !send(self, SocketEvent::Connected { local, remote, credit: SEND_WINDOW }) {
                    self.credit_out = 0;
                    return false;
                }
                if let Kind::Tcp { announced, .. } = &mut self.kind {
                    *announced = true;
                }
            }
            _ => {}
        }
        // Received data, within the client's window.
        let mut buf = alloc::vec![0u8; CHUNK];
        while self.rx_outstanding < RECEIVE_WINDOW {
            let room = ((RECEIVE_WINDOW - self.rx_outstanding) as usize).min(CHUNK);
            let n = match stack.tcp_recv(self.id, &mut buf[..room]) {
                Ok(n) => n,
                Err(_) => 0,
            };
            if n == 0 {
                break;
            }
            self.rx_outstanding += n as u32;
            if !send(self, SocketEvent::Data { data: Bytes(buf[..n].to_vec()) }) {
                // The client's queue is full (it ignores the window): the
                // bytes are lost and the socket fails.
                self.fail(NetError::ProtocolViolation);
                return false;
            }
        }
        // End of stream, once everything before it was delivered.
        let eof = stack.tcp_view(self.id).is_some_and(|v| v.eof);
        let announce_eof = match &mut self.kind {
            Kind::Tcp { eof_sent, .. } if eof && !*eof_sent => {
                *eof_sent = true;
                true
            }
            _ => false,
        };
        if announce_eof {
            let _ = send(self, SocketEvent::Eof {});
        }
        self.pump_tcp_send(stack)
    }

    fn pump_tcp_send(&mut self, stack: &mut Stack) -> bool {
        let Kind::Tcp { pending, shutdown_requested, .. } = &mut self.kind else { return false };
        let mut fed = false;
        while !pending.is_empty() {
            let (a, _) = pending.as_slices();
            let n = match stack.tcp_send(self.id, a) {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            pending.drain(..n);
            self.credit_due += n as u32;
            fed = true;
        }
        if pending.is_empty() && *shutdown_requested {
            *shutdown_requested = false;
            stack.tcp_shutdown(self.id);
            fed = true;
        }
        let drained = pending.is_empty();
        if self.credit_due >= CREDIT_BATCH || (drained && self.credit_due > 0) {
            let bytes = self.credit_due;
            self.credit_due = 0;
            self.credit_out += bytes;
            if !send(self, SocketEvent::SendCredit { bytes }) {
                self.credit_out -= bytes;
                self.credit_due = bytes;
            }
        }
        fed
    }

    fn pump_listener(&mut self, stack: &mut Stack, accepted: &mut Vec<Conn>) {
        while let Some(new) = stack.tcp_accept(self.id) {
            let Some(view) = stack.tcp_view(new) else { continue };
            let TcpState::Open { local, remote } = view.state else {
                stack.close(new);
                continue;
            };
            let Ok((ours, theirs)) = Channel::create() else {
                stack.close(new);
                continue;
            };
            let ev = SocketEvent::Accepted { socket: theirs, local, remote, credit: SEND_WINDOW };
            if send(self, ev) {
                stack.set_owner(new, self.owner);
                let mut conn = Conn::new(
                    ours,
                    new,
                    Kind::Tcp { announced: true, eof_sent: false, pending: VecDeque::new(), shutdown_requested: false },
                    self.owner,
                );
                conn.credit_out = SEND_WINDOW;
                accepted.push(conn);
            } else {
                stack.close(new);
            }
        }
    }

    fn pump_udp(&mut self, stack: &mut Stack) -> bool {
        let mut buf = alloc::vec![0u8; vproto::net::MAX_DATAGRAM];
        while self.rx_outstanding < RECEIVE_WINDOW {
            let Some(d) = stack.udp_recv(self.id, &mut buf) else { break };
            self.rx_outstanding += d.len as u32;
            if !send(self, SocketEvent::Datagram { from: d.from, data: Bytes(buf[..d.len].to_vec()) }) {
                break;
            }
        }
        let Kind::Udp { pending } = &mut self.kind else { return false };
        let mut fed = false;
        while let Some((dest, data)) = pending.front() {
            match stack.udp_send(self.id, *dest, data) {
                Ok(()) => {}
                // The stack's buffer is full: try after the next poll.
                Err(NetError::LimitReached) => break,
                // Undeliverable datagrams are dropped, as on any system.
                Err(_) => {}
            }
            let (_, data) = pending.pop_front().unwrap();
            self.credit_due += data.len() as u32;
            fed = true;
        }
        if self.credit_due > 0 {
            let bytes = self.credit_due;
            self.credit_due = 0;
            self.credit_out += bytes;
            if !send(self, SocketEvent::SendCredit { bytes }) {
                self.credit_out -= bytes;
                self.credit_due = bytes;
            }
        }
        fed
    }

    /// Grants the initial send credit of a UDP socket.
    pub fn grant_initial(&mut self, bytes: u32) {
        self.credit_out = bytes;
        if !send(self, SocketEvent::SendCredit { bytes }) {
            self.credit_out = 0;
        }
    }
}
