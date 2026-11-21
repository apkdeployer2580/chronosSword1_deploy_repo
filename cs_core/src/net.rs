//! TCP for the guest's WIPI network API.
//!
//! The game speaks plain BSD sockets: `socket`, three `setsockopt`s, `fcntl`
//! to go non-blocking, `connect`, then a WIPI timer that polls with `select`
//! and drains with `recv`. `MC_netSocketWrite` uses `write`. That is the whole
//! surface, and it is implemented here on `std::net` so that pointing the game
//! at a private server is a matter of running one — nothing in the guest needs
//! to change.
//!
//! Two deliberate simplifications, both chosen because the guest's own code
//! handles the resulting states and the alternatives do not buy anything:
//!
//! * `connect` completes synchronously with a short timeout instead of
//!   returning `EINPROGRESS`. The guest then finds the socket writable on its
//!   first `select` and proceeds. A real asynchronous connect would need a
//!   reactor for no behavioural gain at one connection.
//! * `select` reports readiness by peeking, and treats any connected socket as
//!   writable — which it is, for the small request packets this game sends.

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::time::Duration;

/// errno values the guest checks for (bionic/Linux ARM).
pub const EAGAIN: i32 = 11;
pub const EBADF: i32 = 9;
pub const ECONNREFUSED: i32 = 111;
pub const EINVAL: i32 = 22;

/// How long a blocking connect is allowed to take. The guest runs its own
/// connect-timeout timer on top of this; this only stops a dead address from
/// stalling the frame loop for the OS default (~2 minutes).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Where a private server listens. The guest's addresses are baked into
/// `.rodata` — `210.222.18.28` for the MIDAS auth host (the same host
/// 영웅서기3 dialled on :8091) and `210.222.18.32` for the charge/cash host —
/// and both died with the service. Redirecting here means the *unpatched*
/// game binary talks to us; nothing in the guest changes.
///
/// Resolution order, matching the sister project:
/// 1. `CS_SERVER_HOST` / `CS_SERVER_PORT` in the environment — the desktop
///    probe and `adb shell setprop`-free local runs.
/// 2. `CS_SERVER_HOST` baked in at build time (`option_env!`), which is how a
///    release APK carries the public address. **`build_android.ps1 -Server`
///    is what sets it** — an APK has no environment, so step 1 cannot fire on
///    a phone and this is the only step that can. `cs_core/build.rs` is what
///    makes cargo rebuild when it changes; without that the value silently
///    survives from an earlier build.
/// 3. Debug builds fall back to loopback, for `adb reverse tcp:8094 tcp:8094`.
///    Release builds with nothing configured do **not** redirect — a shipped
///    build must never silently dial someone else's machine. versionCode 34
///    shipped in exactly that state (nothing baked), so the guest dialled its
///    own dead hardcoded host and every network feature failed to connect;
///    the `-Server` default now covers it, and `-Server off` restores it.
const DEFAULT_PORT: u16 = 8094;

fn net_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("CS_NET_DEBUG").is_ok())
}

fn hex(b: &[u8]) -> String {
    hex_n(b, 48)
}

/// Requests are what a new server op has to be written against, so the send
/// side is dumped whole (up to `max`) rather than clipped to a preview the way
/// replies are.
fn hex_n(b: &[u8], max: usize) -> String {
    let n = b.len().min(max);
    let mut s: String =
        b[..n].iter().map(|v| format!("{v:02x} ")).collect::<String>().trim_end().to_owned();
    if b.len() > n {
        s.push_str(&format!(" ...(+{} more)", b.len() - n));
    }
    s
}

pub(crate) fn redirect_target() -> Option<(String, u16)> {
    let host = std::env::var("CS_SERVER_HOST")
        .ok()
        .or_else(|| option_env!("CS_SERVER_HOST").map(str::to_owned))
        .or_else(|| cfg!(debug_assertions).then(|| "127.0.0.1".to_owned()))?;
    if host.is_empty() || host == "off" {
        return None;
    }
    let port = std::env::var("CS_SERVER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .or_else(|| option_env!("CS_SERVER_PORT").and_then(|p| p.parse().ok()))
        .unwrap_or(DEFAULT_PORT);
    Some((host, port))
}

/// Is the cash shop free, answered here instead of on the wire?
///
/// On by default. `CS_FREE_CASH=0` puts the charge round-trips back on the
/// socket, which is what you want when testing `cs_server`'s own handler.
pub fn free_cash() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        // Build time too, since the device has no environment to set — same
        // reason `CS_SERVER_HOST` is read both ways. `build.rs` tells cargo to
        // watch it, so changing it no longer needs `cargo clean -p cs_core`.
        let off = |v: &str| v == "0";
        match std::env::var("CS_FREE_CASH") {
            Ok(v) => !off(&v),
            Err(_) => !option_env!("CS_FREE_CASH").is_some_and(off),
        }
    })
}

/// The two 스마트요청 charge requests `Charge_makeData` builds, recognised by
/// body marker rather than by address.
///
/// Keying on the payload is deliberate. The charge round-trips share
/// `netSocket` with the game protocol and with 데이터 관리, and which host that
/// socket dialled is decided inside `_midas_funcLibConnect` from a config blob;
/// the markers, by contrast, appear in nothing else the guest sends. It also
/// means a socket that connected to a real `cs_server` still gets its charge
/// answered locally — the request never leaves the process.
const CHARGE_MARKERS: [(&[u8], u8); 2] = [(b"CHARGE_BUYABLE", 0x11), (b"CHARGE_ALL", 0x00)];

/// If `buf` is a charge request, the reply frame the guest expects.
///
/// Framing differs from the main protocol — no leading byte:
/// `[u32 BE total length, counting itself][body]`. `netLibCB` only looks at
/// `body[0]`: `0x11` moves `isGameServer` 3 -> 4 and sends `CHARGE_ALL`, then
/// `0x00` is success and `NpcUI_setBoughtItemOnCASH` grants the item the
/// client's own MERCHANT selection already picked. The body is padded to six
/// bytes because the read callback discards anything with `len <= 5`.
fn charge_reply(buf: &[u8]) -> Option<Vec<u8>> {
    let (marker, status) =
        CHARGE_MARKERS.iter().find(|(m, _)| buf.windows(m.len()).any(|w| w == *m))?;
    let body = [*status, 0, 0, 0, 0, 0];
    let mut frame = ((body.len() + 4) as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&body);
    log::info!(
        "free cash: answering {} locally with status {status:#02x}",
        String::from_utf8_lossy(marker)
    );
    Some(frame)
}

#[derive(Default)]
pub struct Sock {
    pub stream: Option<TcpStream>,
    pub nonblocking: bool,
    /// Sticky peer-closed flag, so `select` keeps reporting the socket readable
    /// after EOF (which is what lets the guest notice the disconnect) instead of
    /// spinning.
    pub eof: bool,
    /// Reply bytes synthesised by [`charge_reply`], waiting to be read. A byte
    /// queue rather than a frame queue because the guest reads the four-byte
    /// header and the body in two separate `recv`s.
    charge_rx: std::collections::VecDeque<u8>,
}

impl Sock {
    pub fn connect(&mut self, ip: u32, port: u16) -> Result<(), i32> {
        let want = SocketAddrV4::new(Ipv4Addr::from(ip), port);
        let addr = match redirect_target() {
            Some((host, rport)) => {
                use std::net::ToSocketAddrs;
                match (host.as_str(), rport).to_socket_addrs().ok().and_then(|mut a| a.next()) {
                    Some(a) => {
                        log::info!("redirecting guest connect {want} -> {a}");
                        a
                    }
                    None => {
                        log::warn!("CS_SERVER_HOST {host:?} did not resolve; using {want}");
                        want.into()
                    }
                }
            }
            None => want.into(),
        };
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                let _ = s.set_nonblocking(true);
                self.stream = Some(s);
                self.eof = false;
                Ok(())
            }
            Err(e) => {
                log::warn!("connect to {addr} failed: {e}");
                Err(errno_of(&e))
            }
        }
    }

    pub fn recv(&mut self, buf: &mut [u8]) -> Result<usize, i32> {
        // A locally answered charge takes priority over the wire, and is served
        // even on a socket that never connected.
        if !self.charge_rx.is_empty() {
            let n = buf.len().min(self.charge_rx.len());
            for b in buf[..n].iter_mut() {
                *b = self.charge_rx.pop_front().unwrap_or(0);
            }
            if net_debug() {
                log::info!("recv want={} got={n} (free cash): {}", buf.len(), hex(&buf[..n]));
            }
            return Ok(n);
        }
        let Some(s) = self.stream.as_mut() else { return Err(EBADF) };
        // `CS_NET_DEBUG=1`. The guest reads a 4-byte length and then the body,
        // so "how many did it ask for / how many did it get" is the difference
        // between the host not delivering and the guest not accepting.
        let want = buf.len();
        match s.read(buf) {
            Ok(0) => {
                self.eof = true;
                if net_debug() {
                    log::info!("recv want={want} -> EOF");
                }
                Ok(0)
            }
            Ok(n) => {
                if net_debug() {
                    log::info!("recv want={want} got={n}: {}", hex(&buf[..n]));
                }
                Ok(n)
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => Err(EAGAIN),
            Err(e) => {
                self.eof = true;
                Err(errno_of(&e))
            }
        }
    }

    pub fn send(&mut self, buf: &[u8]) -> Result<usize, i32> {
        // Swallow the charge request and queue its answer. Reporting the whole
        // write as accepted is what `MX_smartSocketWrite` expects; nothing goes
        // out, so no server has to exist and no carrier is ever billed.
        if free_cash() {
            if let Some(frame) = charge_reply(buf) {
                self.charge_rx.extend(frame);
                return Ok(buf.len());
            }
        }
        let Some(s) = self.stream.as_mut() else { return Err(EBADF) };
        match s.write(buf) {
            Ok(n) => {
                // `CS_NET_DEBUG=1`. Without this an op the server does not
                // implement looks identical to a dead socket: the guest just
                // stops. Dumping the request whole is what makes a new
                // `cs_server` handler writable — it is how the 데이터 관리
                // (:11536) op was approached. `want` vs `wrote` also catches a
                // short write, which the guest is free to make no attempt to
                // retry.
                if net_debug() {
                    log::info!("send want={} wrote={n}: {}", buf.len(), hex_n(buf, 4096));
                }
                Ok(n)
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => Err(EAGAIN),
            Err(e) => Err(errno_of(&e)),
        }
    }

    /// Is there data (or an EOF) waiting? Peeking is the only readiness test
    /// `std::net` offers, and it is enough: it does not consume.
    pub fn readable(&mut self) -> bool {
        if !self.charge_rx.is_empty() {
            return true;
        }
        if self.eof {
            return true;
        }
        let Some(s) = self.stream.as_ref() else { return false };
        let mut probe = [0u8; 1];
        match s.peek(&mut probe) {
            Ok(0) => {
                self.eof = true;
                true
            }
            Ok(_) => true,
            Err(e) if e.kind() == ErrorKind::WouldBlock => false,
            Err(_) => true, // an error is a readable event: the guest reads and sees it
        }
    }

    pub fn writable(&self) -> bool {
        self.stream.is_some()
    }

    pub fn shutdown(&mut self) {
        if let Some(s) = self.stream.as_ref() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

pub fn errno_of(e: &std::io::Error) -> i32 {
    match e.kind() {
        ErrorKind::WouldBlock => EAGAIN,
        ErrorKind::ConnectionRefused => ECONNREFUSED,
        ErrorKind::InvalidInput => EINVAL,
        _ => 111,
    }
}

/// A bionic 32-bit `fd_set`: 1024 bits in 32-bit words.
pub const FD_SETSIZE: u32 = 1024;

pub fn fd_isset(words: &[u32], fd: i32) -> bool {
    if fd < 0 || fd as u32 >= FD_SETSIZE {
        return false;
    }
    let (w, b) = (fd as usize / 32, fd as usize % 32);
    words.get(w).is_some_and(|v| v & (1 << b) != 0)
}

pub fn fd_set(words: &mut [u32], fd: i32) {
    if fd < 0 || fd as u32 >= FD_SETSIZE {
        return;
    }
    let (w, b) = (fd as usize / 32, fd as usize % 32);
    if let Some(v) = words.get_mut(w) {
        *v |= 1 << b;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Charge_makeData` builds `"G1000148 C <phone> CHARGE_BUYABLE<n> <model>"`
    /// into a 100-byte NUL-padded buffer, and `MX_smartSocketWrite` queues a
    /// 108-byte header in front of it, so the marker is found by scanning, not at
    /// a fixed offset.
    fn request(marker: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 108];
        b.extend_from_slice(b"G1000148 C 01012345678 ");
        b.extend_from_slice(marker);
        b.resize(208, 0);
        b
    }

    #[test]
    fn charge_frames_carry_the_status_the_guest_branches_on() {
        for (marker, status) in [(&b"CHARGE_BUYABLE3"[..], 0x11u8), (b"CHARGE_ALL", 0x00)] {
            let f = charge_reply(&request(marker)).expect("marker recognised");
            // [u32 BE total, counting itself][body]; body > 5 bytes or the
            // guest's read callback discards the frame.
            assert_eq!(&f[..4], &10u32.to_be_bytes());
            assert_eq!(f.len(), 10);
            assert_eq!(f[4], status);
        }
    }

    #[test]
    fn other_traffic_is_left_on_the_wire() {
        assert!(charge_reply(&request(b"LOGIN")).is_none());
        assert!(charge_reply(&[]).is_none());
    }

    /// The guest reads the four-byte header and the body in two `recv`s, so the
    /// queue has to split at whatever boundary it asks for.
    #[test]
    fn a_queued_reply_is_read_back_in_two_parts() {
        let mut s = Sock::default();
        s.charge_rx.extend(charge_reply(&request(b"CHARGE_ALL")).unwrap());
        assert!(s.readable());
        let mut hdr = [0u8; 4];
        assert_eq!(s.recv(&mut hdr), Ok(4));
        assert_eq!(u32::from_be_bytes(hdr), 10);
        let mut body = [0u8; 6];
        assert_eq!(s.recv(&mut body), Ok(6));
        assert_eq!(body[0], 0);
        // Drained: no stream behind it, so the guest now sees a plain bad fd
        // rather than a phantom readable socket.
        assert!(!s.readable());
        assert_eq!(s.recv(&mut body), Err(EBADF));
    }
}

/// Parse a `struct sockaddr_in`: `{ u16 family; u16 port_be; u32 addr_be; ... }`.
pub fn parse_sockaddr_in(b: &[u8]) -> Option<(u32, u16)> {
    if b.len() < 8 {
        return None;
    }
    let port = u16::from_be_bytes([b[2], b[3]]);
    let addr = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    Some((addr, port))
}
