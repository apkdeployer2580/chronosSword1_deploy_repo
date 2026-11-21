//! Ask the private server to reserve an ID, without ever blocking the frame.
//!
//! The guest protocol has no concept of choosing a name, and we do not patch
//! the guest's networking. So this is a **separate conversation** on the same
//! socket the game already dials: one line of JSON behind the magic `CSID1 `,
//! which `cs_server`'s `ctrl_reply` picks off before anything else looks at the
//! buffer. The full protocol note lives in `cs_server/server.py`.
//!
//! # Why a worker thread
//!
//! The frame thread must never wait on a socket. Android's input-dispatch ANR
//! fires at five seconds, and this repository has already spent a session on
//! one stall that came close (the boss-kill radial blur, `CLAUDE.md` 성능 6);
//! a DNS lookup and a TCP connect to a machine that has gone away is far worse
//! than that, and it would happen on the one screen where the player is
//! definitely touching the display.
//!
//! So the shape is `audio.rs`'s: a named thread, an `mpsc` in, an `mpsc` out,
//! and a frame-side [`Claimer::poll`] that never blocks. The only state the
//! frame thread keeps is "a request is outstanding", which is what makes the
//! 30-second give-up a comparison against `Vm::wall_now_ms` rather than a
//! timer.
//!
//! # Why the answer is read as hex
//!
//! `cs_server` writes its JSON with `ensure_ascii=False`, so the `id` field of
//! a Hangul name arrives as **UTF-8**. The ID itself is EUC-KR — the bytes the
//! guest's own IME produced. Round-tripping through the `id` field would mean
//! decoding UTF-8 and re-encoding EUC-KR with a table we do not have, and
//! getting it subtly wrong is exactly the 「썍몌예ㅃ 쏙옙」 failure `CLAUDE.md`
//! documents for `.rodata`. The server also sends `raw`, the hex of the bytes
//! it was given. We read that and ignore `id` entirely.

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

/// The line prefix `cs_server` splits on. Chosen because a guest frame always
/// starts with `0x00` (its length word is bounded) and a cash-shop blob starts
/// with `'G'`, so `'C'` cannot collide with either.
const MAGIC: &[u8] = b"CSID1 ";

/// How long the worker waits for the server, per attempt. Deliberately shorter
/// than the frame side's give-up so that "could not connect" and "connected but
/// silent" land in the log as different lines.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the frame side offers to wait before telling the player to try
/// later. The settled design says 30 s; the worker's own timeouts add to at
/// most 13, so this is a ceiling and not the usual path.
pub const GIVE_UP_MS: i64 = 30_000;

/// What the server said. `raw` is EUC-KR bytes, never a `String`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Reserved for this install. The bytes are what the server recorded, which
    /// is what must be stored — asking about one form and keeping another is
    /// how two installs end up holding names the server thinks are different.
    Ours(Vec<u8>),
    /// Someone else holds it.
    Taken,
    /// Nobody holds it, but an install has been playing under that address.
    InUse,
    /// This install already renamed away from the name it asked about.
    Moved,
    /// The server refused the name itself. Should not happen — the host checks
    /// first — so it is worth a log line rather than a player-facing message.
    Rejected(String),
    /// No answer, or an answer we could not read.
    Unreachable(String),
}

/// A request in flight. One at a time: the player can only be looking at one
/// name-entry screen, and serialising means the worker needs no bookkeeping.
struct Request {
    install: String,
    raw: Vec<u8>,
    was: Vec<u8>,
}

pub struct Claimer {
    tx: Sender<Request>,
    rx: Receiver<Answer>,
}

impl Claimer {
    pub fn start() -> Self {
        let (tx, req_rx) = std::sync::mpsc::channel::<Request>();
        let (ans_tx, rx) = std::sync::mpsc::channel::<Answer>();
        std::thread::Builder::new()
            .name("cs-id-claim".into())
            .spawn(move || {
                for req in req_rx {
                    let answer = ask(&req);
                    log::info!("id claim: server said {answer:?}");
                    if ans_tx.send(answer).is_err() {
                        break; // the VM is gone
                    }
                }
            })
            .ok();
        Self { tx, rx }
    }

    /// Queue a claim. `was` is the address the server currently keys this
    /// install by — see `identity::Identity::wire_address`, which is not always
    /// the stored number.
    ///
    /// Returns false if the worker is gone, so the caller can report
    /// unreachable rather than wait forever for an answer that cannot come.
    pub fn submit(&self, install: &str, raw: &[u8], was: &[u8]) -> bool {
        self.tx
            .send(Request { install: install.to_owned(), raw: raw.to_vec(), was: was.to_vec() })
            .is_ok()
    }

    /// The answer, if one has arrived. **Never blocks.**
    pub fn poll(&self) -> Option<Answer> {
        match self.rx.try_recv() {
            Ok(a) => Some(a),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Answer::Unreachable("the claim worker stopped".into()))
            }
        }
    }
}

/// One round trip, on the worker thread.
fn ask(req: &Request) -> Answer {
    let Some((host, port)) = crate::net::redirect_target() else {
        // A build with no server baked in. `redirect_target` is also what
        // decides where the *guest* dials, so this is exactly the state
        // versionCode 34 shipped in — worth naming rather than timing out.
        return Answer::Unreachable("no server address is configured".into());
    };
    let line = request_line(req);
    match round_trip(&host, port, &line) {
        Ok(reply) => parse(&reply),
        Err(e) => Answer::Unreachable(format!("{host}:{port}: {e}")),
    }
}

/// The request line, built by hand.
///
/// No JSON library: every value we emit is ASCII by construction — a hex token,
/// two hex strings — so there is nothing to escape and nothing to get wrong.
/// Adding a crate here would also be a *host* build-dependency in the android
/// cross-build, which is what drags in the `dlltool` shim (`build_android.ps1`).
fn request_line(req: &Request) -> Vec<u8> {
    let hex = |b: &[u8]| -> String { b.iter().map(|x| format!("{x:02x}")).collect() };
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(
        format!(
            "{{\"op\":\"claim\",\"install\":\"{}\",\"raw\":\"{}\",\"was\":\"{}\"}}\n",
            req.install.chars().filter(char::is_ascii_alphanumeric).collect::<String>(),
            hex(&req.raw),
            hex(&req.was),
        )
        .as_bytes(),
    );
    out
}

fn round_trip(host: &str, port: u16, line: &[u8]) -> std::io::Result<String> {
    use std::net::ToSocketAddrs;
    // Resolve and connect with a bound — a dead address must not hold the
    // worker for the OS default, because the player is watching a spinner.
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("no address"))?;
    let sock = std::net::TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    sock.set_read_timeout(Some(READ_TIMEOUT))?;
    sock.set_write_timeout(Some(READ_TIMEOUT))?;
    let mut sock = sock;
    sock.write_all(line)?;
    sock.flush()?;
    let mut reader = BufReader::new(sock);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if reply.is_empty() {
        return Err(std::io::Error::other("the server closed without answering"));
    }
    Ok(reply)
}

/// Read `status` and `raw` out of the reply.
///
/// A scanner rather than a parser, for the same reason `identity.rs` has one:
/// the only writer of this line is `cs_server`, and the two fields we need are
/// flat strings. Anything unexpected becomes `Unreachable`, which the caller
/// already has to handle.
fn parse(reply: &str) -> Answer {
    let body = reply.trim_start_matches(std::str::from_utf8(MAGIC).unwrap_or("")).trim();
    let Some(status) = field(body, "status") else {
        return Answer::Unreachable(format!("no status in {body:?}"));
    };
    match status.as_str() {
        "claimed" | "yours" => match field(body, "raw").as_deref().and_then(unhex) {
            // **The bytes, not the `id` field.** See the module note.
            Some(raw) if !raw.is_empty() => Answer::Ours(raw),
            _ => Answer::Unreachable(format!("no usable raw in {body:?}")),
        },
        "taken" => Answer::Taken,
        "inuse" => Answer::InUse,
        "moved" => Answer::Moved,
        other => Answer::Rejected(other.to_owned()),
    }
}

/// The value of `"key"`, if it is a plain string.
fn field(text: &str, key: &str) -> Option<String> {
    let at = text.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = text[at..].trim_start().strip_prefix(':')?.trim_start();
    let body = rest.strip_prefix('"')?;
    Some(body[..body.find('"')?].to_owned())
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let raw = text.as_bytes();
    if raw.is_empty() || raw.len() % 2 != 0 {
        return None;
    }
    fn nibble(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    raw.chunks(2).map(|p| Some(nibble(p[0])? << 4 | nibble(p[1])?)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line has to be exactly what `cs_server`'s `ctrl_reply` splits on:
    /// the magic, one line of JSON, one `\n`.
    #[test]
    fn the_request_line_is_the_shape_the_server_splits_on() {
        let line = request_line(&Request {
            install: "0123456789abcdef".into(),
            raw: vec![0xB0, 0xA1, 0xB8, 0xA7], // 가름
            was: b"01077778888".to_vec(),
        });
        assert!(line.starts_with(MAGIC), "the magic is what tells it from a guest frame");
        assert_eq!(line.last(), Some(&b'\n'), "one line, newline terminated");
        let text = String::from_utf8(line).expect("the request is ASCII by construction");
        assert!(text.is_ascii(), "a non-ASCII byte would need escaping we do not do");
        assert!(text.contains("\"raw\":\"b0a1b8a7\""), "{text}");
        assert!(text.contains("\"was\":\"3031303737373738383838\""), "{text}");
        assert!(text.contains("\"op\":\"claim\""), "{text}");
        // Exactly one newline: a second would be a second control line.
        assert_eq!(text.matches('\n').count(), 1);
    }

    /// A hostile or broken install token must not be able to inject a field.
    #[test]
    fn a_rubbish_install_token_cannot_break_the_line() {
        let line = request_line(&Request {
            install: "\",\"op\":\"status\",\"x\":\"".into(),
            raw: vec![b'A'],
            was: vec![],
        });
        let text = String::from_utf8(line).unwrap();
        // The filter keeps alphanumerics, so `status` survives *as letters* —
        // what must not survive is the punctuation that would make it a field.
        let install = text.split("\"install\":\"").nth(1).unwrap().split('"').next().unwrap();
        assert_eq!(install, "opstatusx", "only alphanumerics survive: {install:?}");
        assert!(!install.contains(['"', ':', ',', '{', '}', '\\', '\n']), "{install:?}");
        assert_eq!(text.matches("\"op\":").count(), 1, "one op field: {text}");
        assert_eq!(text.matches('\n').count(), 1, "still one line: {text}");
        // And the server would refuse that token anyway — `_token` wants hex.
        assert!(!install.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Every status the server can answer with maps to something the caller can
    /// act on, and an unparseable line never looks like success.
    #[test]
    fn every_server_status_is_understood() {
        let ok = r#"CSID1 {"ok": true, "op": "claim", "status": "claimed", "id": "가름", "raw": "b0a1b8a7"}"#;
        assert_eq!(parse(ok), Answer::Ours(vec![0xB0, 0xA1, 0xB8, 0xA7]));
        // `yours` is the idempotent re-ask, and must be as good as `claimed` —
        // that is the whole reason the server has no intermediate state.
        let yours = r#"CSID1 {"status": "yours", "raw": "4741", "id": "GA"}"#;
        assert_eq!(parse(yours), Answer::Ours(vec![b'G', b'A']));
        assert_eq!(parse(r#"CSID1 {"status": "taken", "id": "x"}"#), Answer::Taken);
        assert_eq!(parse(r#"CSID1 {"status": "inuse", "id": "x"}"#), Answer::InUse);
        assert_eq!(parse(r#"CSID1 {"status": "moved"}"#), Answer::Moved);
        assert_eq!(parse(r#"CSID1 {"status": "badid"}"#), Answer::Rejected("badid".into()));
        // **The `id` field is never trusted.** It is UTF-8 in the reply while
        // the ID is EUC-KR, so a claim that answered without `raw` must fail
        // rather than store a re-encoded name.
        assert!(matches!(
            parse(r#"CSID1 {"status": "claimed", "id": "가름"}"#),
            Answer::Unreachable(_)
        ));
        for junk in ["", "not json", "CSID1 ", r#"CSID1 {"ok": true}"#, r#"{"status":}"#] {
            assert!(matches!(parse(junk), Answer::Unreachable(_)), "{junk:?} looked like success");
        }
        // Odd-length and non-hex raw are not bytes.
        for bad in [r#"CSID1 {"status":"claimed","raw":"abc"}"#, r#"CSID1 {"status":"claimed","raw":"zz"}"#] {
            assert!(matches!(parse(bad), Answer::Unreachable(_)), "{bad}");
        }
    }

    /// Polling an idle claimer must never block and never invent an answer.
    #[test]
    fn polling_is_free_and_says_nothing_until_there_is_something_to_say() {
        let c = Claimer::start();
        for _ in 0..100 {
            assert_eq!(c.poll(), None);
        }
    }
}
