//! A number that says which install this is — and, later, a name the player chose.
//!
//! The game was written for phones, so its idea of "who are you" is a phone
//! number: `getSKTPhoneNumber` asks `MC_knlGetSystemProperty("PHONENUMBER")`,
//! normalises the result to start with `'0'`, and hands it to
//! `setServerConnected` (the handshake) and to `Charge_makeData` (billing).
//! `_midas_funcDspCrossRoadMakePacket` reaches for the same well when it builds
//! the 데이터 관리 packets, which is why the save-slot key was empty too.
//!
//! Our HLE never answered, so **every install reported the same nothing** and
//! the private server could not tell two players apart. With one shared server
//! that is not a cosmetic gap: one mailbox for everyone, one save slot for
//! everyone, and a letter's gold going to whoever opens 편지함 first.
//!
//! So the host answers instead. One APK for everybody — the number is made on
//! first launch and kept in the app's own storage, next to `cs_potions.json`,
//! exactly the way that file already works. Nothing about it is secret; it is
//! an address a player reads off the log and gives to a friend.
//!
//! **The probe gets a fixed number.** `Vm::now_ms` is virtual there so replays
//! stay deterministic, and an identity that changed per run would move the
//! bytes the guest sends and split every dump comparison.
//!
//! # The gap this leaves, and what is being built on top
//!
//! A minted number is **not something a player can read off their own screen**.
//! Measured: the boss sent two letters to their real number `<real number>` while
//! the phone's game address was `01077778888`. The letters arrived — at a slot
//! nobody uses, and had to be moved by hand on the server. Handing the APK to a
//! friend makes 편지 unusable in practice.
//!
//! The fix is a name the player types once and can tell people. That lives in a
//! **second** file, `cs_identity.json`, and `cs_id.txt` is left exactly as it
//! was: one line, plain digits, same meaning. Putting the new state in the old
//! file would have been tidier and is the wrong trade — a build that predates
//! the change reads `cs_id.txt` with a digit filter, so it would have scraped
//! the digits out of the JSON and reported a **different identity**, silently
//! orphaning that install's mailbox on the server. Two files cannot do that.

use std::path::{Path, PathBuf};

/// Where the minted number lives, under the storage root. Plain digits, one
/// line — **do not change this format**, see the module note.
const FILE: &str = "cs_id.txt";

/// Where the chosen ID and its bookkeeping live.
const SIDECAR: &str = "cs_identity.json";

/// What the probe always gets. Deliberately not a plausible real number.
const FIXED: &str = "01000000000";

/// The digits after the `010` prefix. `getSKTPhoneNumber` copies through a
/// 0x20-byte buffer and the packet field is 16 bytes, so 11 digits is
/// comfortable; the guest also special-cases a leading `'0'` and would
/// otherwise prepend one.
const DIGITS: usize = 8;

/// How long an install token is, in hex characters. Only has to be unguessable
/// enough that two friends do not collide; it is not a credential — see
/// `cs_server`'s 제어 채널 note on why the guest protocol has no auth at all.
const TOKEN_HEX: usize = 32;

/// The hard ceiling on a chosen ID, in **bytes** (not characters — Hangul is
/// two bytes each in EUC-KR).
///
/// Two guest buffers set it, and neither can be widened:
///
/// * **Handshake.** `setServerConnected+0x10` is `add r8, sp, #0x8c` and the
///   length out-parameter is `sp+0x9c` — the field is exactly 16 bytes. With
///   `getSKTPhoneNumber`'s leading `'0'` and the NUL, 14 fills it exactly.
/// * **Recipient.** `onCloseIME_MainFrame+0x98` does
///   `memcpy(_mailBox+0x42, buf, strlen(buf))` with **no clamp and no NUL**,
///   and that slot is 16 bytes before the letter body at `+0x52`.
///
/// 15 overruns both. The shipped IME cap is 11 (`MX_imeSetProperty(3, 0xb)` at
/// `InputInit+0x74` and `+0xb4`, in *both* modes) — note that the `0xe` next to
/// it is the input-mode mask from property 0, not a length.
pub const ID_MAX_BYTES: usize = 14;

/// The handshake's identity field, in bytes, NUL included.
///
/// `setServerConnected+0x0c` is `sub sp, sp, #0xa0` and `+0x10` is
/// `add r8, sp, #0x8c`, with the length out-parameter at `sp+0x9c` — sixteen
/// bytes between them. `MC_knlSprintk` has no bound, so a seventeenth byte
/// lands on that word and is then overwritten by `str r4, [sb]` at 0xd1cac:
/// the overrun is **silent**, and what reaches the server is a truncated
/// address, i.e. a different player.
pub const HANDSHAKE_FIELD: usize = 16;

/// The longest string we may report as `PHONENUMBER`.
///
/// `getSKTPhoneNumber` never lengthens its input except by prepending a `'0'`
/// when it does not already start with one, so this is the bound on **what
/// comes out** of it — [`wire_number`] is the transcription to measure with.
pub const WIRE_MAX_BYTES: usize = HANDSHAKE_FIELD - 1;

/// Bytes the shipped text renderer **eats**: `HF_IsEscape` (0x8803c) answers
/// yes for `';'` and for the twelve-byte table at 0x10595c, which reads
/// `& | $ { } o p q r s t \0`.
///
/// These are zero-width colour/였 escapes, so an ID containing one would be
/// partly invisible everywhere it is drawn — the 「내 ID」 tile, the letter
/// screen, the sender line in someone else's 편지함. Note the asymmetry the
/// table actually has: **lowercase** `o`–`t` are eaten, uppercase are not,
/// which is why the game's own English strings are uppercase.
///
/// `%` is not an escape; it is here because `setPopUp` hands its argument to
/// `MC_knlSprintk` as a **format string** (the same reason
/// `DATA_MANAGE_MSG` is asserted `%`-free), and the ID goes into that popup.
const REJECTED_ASCII: &[u8] = b"&|${}opqrst;%";

/// Why an ID was refused. The host refuses before the server ever sees it, so
/// the player gets the keyboard back instead of a round trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdProblem {
    Empty,
    TooLong,
    /// A byte the renderer would swallow, or `%`.
    Escape(u8),
    /// Not ASCII alphanumeric and not a KS X 1001 Hangul syllable.
    NotAllowed(u8),
    /// A lead byte with no trail byte after it.
    Truncated,
    /// Digits and nothing else — the namespace [`mint`] draws from.
    AllDigits,
}

/// Fold a freshly typed name into the form the game can actually draw, then
/// validate it.
///
/// **ASCII letters are upper-cased.** That is not a style choice — six of the
/// twelve bytes `HF_IsEscape` swallows are the *lowercase* letters `o`–`t`, and
/// the shipped data proves the consequence: of 775 strings in the
/// `getInCodeString` table, every single ASCII one that contains a lowercase
/// `o`–`t` uses it as a leading colour marker (`rNEXT`, `tCOMPARE`,
/// `oTAP THE SCREEN`, `rEXP &%dr/%d`) and the readable text is **always**
/// uppercase. The game has no lowercase English anywhere.
///
/// So refusing those six letters would mean `Garum` is not a legal ID while
/// `GARUM` is — a rule no player could guess. Upper-casing removes the entire
/// class instead, and lands on exactly the character set the game already
/// draws.
///
/// The walk is two-byte aware: a Hangul trail byte must never be "upper-cased".
/// Accepted trails are `0xA1..=0xFE` so none of them are ASCII letters today,
/// but a blind `to_ascii_uppercase()` over the slice would be a landmine for
/// whoever widens the accepted range later.
pub fn normalise_id(bytes: &[u8]) -> Result<Vec<u8>, IdProblem> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if (0xB0..=0xC8).contains(&b) {
            match bytes.get(i + 1) {
                Some(&t) if (0xA1..=0xFE).contains(&t) => {
                    out.extend_from_slice(&[b, t]);
                    i += 2;
                }
                Some(&t) => return Err(IdProblem::NotAllowed(t)),
                None => return Err(IdProblem::Truncated),
            }
            continue;
        }
        out.push(b.to_ascii_uppercase());
        i += 1;
    }
    check_id(&out)?;
    Ok(out)
}

/// Is this a name we are willing to store and draw?
///
/// The bytes arrive **already EUC-KR** — they come out of the guest's own IME
/// (`imeCore_*` composes the syllables), so nothing here encodes or decodes,
/// and no conversion table is needed. That is the whole reason the ID is kept
/// as bytes rather than a `String`: `String::from_utf8_lossy` would turn every
/// Hangul byte into U+FFFD and a round trip would produce completely different
/// bytes — the trap `CLAUDE.md` records for `.rodata` strings.
///
/// Hangul is accepted as the KS X 1001 syllable rectangle: lead `0xB0..=0xC8`,
/// trail `0xA1..=0xFE`. That is 25 x 94 = **exactly the 2350 assigned
/// syllables**, and exactly what `font.rs` can draw — its index is
/// `(lead - 0xB0) * 0x5e + (trail - 0xA1)` with the cut at `0x92d` = 2349.
///
/// A syllable outside it (`힣` is `C6 52` in cp949's *extended* range) is not
/// drawn as a missing glyph; `font.rs` indexes with it anyway and paints a
/// **different syllable**, or reads before the table. So refusing is right, not
/// merely cautious — but it does mean a player who composes one is bounced with
/// no message. See LETTER_ID.md §9.
pub fn check_id(bytes: &[u8]) -> Result<(), IdProblem> {
    if bytes.is_empty() {
        return Err(IdProblem::Empty);
    }
    if bytes.len() > ID_MAX_BYTES {
        return Err(IdProblem::TooLong);
    }
    // **An all-digit name is not available.** That is exactly the shape [`mint`]
    // produces — `010` plus eight digits — and every mailbox and save slot on
    // the server today is keyed by one of those. A player typing another
    // install's number as their ID would be asking the server for that person's
    // address; the server refuses it now (`ctrl_claim`'s `inuse`), but that
    // refusal arrives after a round trip and reads as "taken" for a name the
    // player thinks is free. Refusing here keeps the two namespaces apart,
    // which is the actual invariant.
    //
    // Digits *inside* a name are fine — the design says 한글·영문·숫자. It is
    // digits and nothing else that collides.
    if bytes.iter().all(u8::is_ascii_digit) {
        return Err(IdProblem::AllDigits);
    }
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if REJECTED_ASCII.contains(&b) {
            return Err(IdProblem::Escape(b));
        }
        if b.is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        if (0xB0..=0xC8).contains(&b) {
            match bytes.get(i + 1) {
                Some(&t) if (0xA1..=0xFE).contains(&t) => i += 2,
                Some(&t) => return Err(IdProblem::NotAllowed(t)),
                None => return Err(IdProblem::Truncated),
            }
            continue;
        }
        return Err(IdProblem::NotAllowed(b));
    }
    Ok(())
}

/// Everything this install knows about who it is.
///
/// `number` always exists. **The game must work fully with no ID at all** —
/// that is a settled requirement, not a fallback, so every consumer here has to
/// be happy with no name.
///
/// # Why a proposal and a held name are separate fields
///
/// They were one field plus a `claimed` flag, and that lost data twice over:
///
/// * `propose` overwrote the field, so **merely typing a second name destroyed
///   the one the server had already agreed to.** If that second claim then
///   failed — or timed out, which the settled design says happens after 30 s —
///   `withdraw` dropped the field and the install was left with no name at all,
///   while the server still had the first one reserved to this install token.
///   The player could not get it back: re-claiming it answers `yours`, but
///   nothing local remembers what it was.
/// * `should_prompt` keyed on "is there a name", so an unconfirmed proposal
///   that survived a restart silenced the first-run prompt **forever** while
///   `wire()` still reported the minted number. A name that is not on the wire
///   and cannot be re-offered is the worst of both.
///
/// Keeping them apart makes both cases fall out: a proposal never touches the
/// held name, and only a held name counts as having one.
#[derive(Clone, Debug)]
pub struct Identity {
    storage: PathBuf,
    /// Virtual clock (the probe): answer from memory, never touch the disk.
    ephemeral: bool,
    number: String,
    install: String,
    /// The name the server agreed to. This is the only thing that goes on the
    /// wire and the only thing that persists.
    held: Option<Vec<u8>>,
    /// A name the player typed, waiting on the server. **Not persisted** — an
    /// unanswered proposal has no meaning next launch, and persisting it is
    /// what silenced the prompt.
    pending: Option<Vec<u8>>,
    /// How many times the first-run prompt has been opened and cancelled.
    /// Bounded so a player who does not want an ID stops being nagged.
    asked: u32,
    dirty: bool,
}

/// How many launches will offer the prompt before it only lives on the tile.
pub const MAX_PROMPTS: u32 = 3;

impl Identity {
    pub fn load(storage: &Path, virtual_clock: bool) -> Self {
        let number = number(storage, virtual_clock);
        if virtual_clock {
            return Self {
                storage: storage.to_path_buf(),
                ephemeral: true,
                install: "0".repeat(TOKEN_HEX),
                number,
                held: None,
                pending: None,
                asked: 0,
                dirty: false,
            };
        }
        let text = std::fs::read_to_string(storage.join(SIDECAR)).unwrap_or_default();
        let stored_token = field(&text, "install").filter(|t| is_token(t));
        let install = match stored_token {
            Some(ref t) => t.clone(),
            // A different domain constant from `mint`, so the token and the
            // number cannot come out of the generator in lockstep.
            None => mint_hex(TOKEN_HEX, 0x1157_a11_0_c0de_u64),
        };
        // Only a **claimed** name is restored. An `id_hex` without
        // `"claimed": true` is a proposal we never got an answer for, and the
        // next launch should offer the prompt again rather than sit on it.
        let held = field(&text, "id_hex")
            .as_deref()
            .and_then(unhex)
            .filter(|b| check_id(b).is_ok())
            .filter(|_| field(&text, "claimed").as_deref() == Some("true"));
        let mut me = Self {
            storage: storage.to_path_buf(),
            ephemeral: false,
            number,
            install,
            held,
            pending: None,
            asked: field(&text, "asked").and_then(|t| t.parse().ok()).unwrap_or(0),
            dirty: false,
        };
        // **Persist whenever the token did not come out of the file**, not just
        // when the file was missing. A sidecar that exists but does not parse —
        // truncated by a power cut, zero-filled by the filesystem, hand-edited —
        // mints a fresh token; if that is never written, the token is *different
        // on every launch*, and the install token is the only evidence the
        // control channel has that this is the same install. The player would
        // get `taken` for their own ID, forever, with no protocol recovery.
        if stored_token.is_none() {
            me.dirty = true;
        }
        me
    }

    /// The minted number. Always present, never changes for an install.
    pub fn number(&self) -> &str {
        &self.number
    }

    pub fn install(&self) -> &str {
        &self.install
    }

    /// The name the server agreed to, in EUC-KR bytes. A proposal is not one.
    pub fn id(&self) -> Option<&[u8]> {
        self.held.as_deref()
    }

    /// The name typed but not yet agreed to — what a claim must ask about.
    pub fn pending(&self) -> Option<&[u8]> {
        self.pending.as_deref()
    }

    pub fn is_claimed(&self) -> bool {
        self.held.is_some()
    }

    /// What to report as `PHONENUMBER`: the chosen name once the server has
    /// agreed to it, otherwise the minted number.
    ///
    /// Only a **claimed** ID goes on the wire. An unconfirmed one would key a
    /// mailbox that might belong to someone else, and letters are not
    /// recoverable once delivered.
    pub fn wire(&self) -> &[u8] {
        match &self.held {
            Some(id) => id,
            None => self.number.as_bytes(),
        }
    }

    /// Should the first-run prompt open this launch?
    ///
    /// Keyed on the **held** name, not on whether anything was typed: a
    /// proposal the server never answered leaves the install reporting its
    /// number, so it still has no ID and the offer should stand.
    pub fn should_prompt(&self) -> bool {
        !self.ephemeral && self.held.is_none() && self.asked < MAX_PROMPTS
    }

    /// The player opened the prompt and did not finish it.
    pub fn note_prompt(&mut self) {
        self.asked = self.asked.saturating_add(1);
        self.dirty = true;
    }

    /// The player typed a name. Normalised on the way in (see
    /// [`normalise_id`]) and **not on the wire** until [`Self::confirm`].
    ///
    /// Returns what was actually stored, which is what the server must be asked
    /// about — asking about the typed form and storing the folded one would let
    /// two players hold names the server thinks are different.
    ///
    /// **A held name is untouched.** Typing a second name while holding a first
    /// is a rename attempt, and it must not cost the first one if it fails.
    pub fn propose(&mut self, bytes: &[u8]) -> Result<&[u8], IdProblem> {
        self.pending = Some(normalise_id(bytes)?);
        Ok(self.pending.as_deref().unwrap_or_default())
    }

    /// The server said the name is ours. The proposal becomes the held name.
    pub fn confirm(&mut self) {
        if let Some(name) = self.pending.take() {
            self.held = Some(name);
            self.dirty = true;
        }
    }

    /// The server said no, or never answered.
    ///
    /// Drops **the proposal only**. Any name already agreed to survives — the
    /// 30-second timeout in the settled design lands here, and losing a held
    /// name to a network hiccup is not recoverable from the client side.
    pub fn withdraw(&mut self) {
        self.pending = None;
    }

    /// Write the sidecar if anything changed.
    ///
    /// **Temp file then rename**, unlike `potions.rs`. The difference is what a
    /// half-written file costs: a truncated `cs_potions.json` loses a buff, a
    /// truncated `cs_identity.json` loses the install token, and losing that
    /// means the server answers `taken` for the player's own ID with no way
    /// back. `cs_server`'s `_json_save` is atomic for the same reason.
    pub fn save(&mut self) {
        if !self.dirty || self.ephemeral {
            return;
        }
        let _ = std::fs::create_dir_all(&self.storage);
        let path = self.storage.join(SIDECAR);
        let json = format!(
            "{{ \"v\": 1, \"install\": \"{}\", \"id_hex\": \"{}\", \"claimed\": {}, \"asked\": {} }}\n",
            self.install,
            self.held.as_deref().map(hex).unwrap_or_default(),
            self.held.is_some(),
            self.asked,
        );
        let tmp = path.with_extension("json.tmp");
        let wrote = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &path));
        match wrote {
            Ok(()) => self.dirty = false,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                log::warn!("identity: cannot write {}: {e}", path.display());
            }
        }
    }

    /// The address the server will actually key this install by, right now.
    ///
    /// Not always [`Self::number`] — see [`wire_number`]. This is what the
    /// 「내 ID」 tile should show and what a claim's `was` field must carry.
    pub fn wire_address(&self) -> Vec<u8> {
        match &self.held {
            Some(id) => id.clone(),
            None => wire_number(self.number.as_bytes()),
        }
    }
}

/// Read the install's number, making one the first time.
///
/// Never fails in a way the caller has to handle: if storage is unwritable the
/// number is still returned, it just will not survive a restart. A wrong-but-
/// present identity is far better than a crash, and the failure is logged.
pub fn number(storage: &Path, virtual_clock: bool) -> String {
    if virtual_clock {
        return FIXED.to_owned();
    }
    let _ = std::fs::create_dir_all(storage);
    let path = storage.join(FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let got: String = text.trim().chars().filter(char::is_ascii_digit).collect();
        if got.len() >= 3 {
            return got;
        }
        // **Keep the corpse.** This install's whole identity is that string;
        // if it came back as zero bytes because a write was interrupted, the
        // line below is about to mint a *different* one and the mailbox on the
        // server is orphaned with no way to find out what it used to be. The
        // copy costs nothing and is the only evidence anyone would have.
        log::warn!("identity: {FILE} held {text:?}, which is not a number; making a new one");
        let _ = std::fs::rename(&path, path.with_extension("txt.bad"));
    }
    let made = mint();
    // Temp file then rename, for the same reason `Identity::save` does it: a
    // half-written `cs_id.txt` is not a corrupt file, it is a *different
    // player*. `std::fs::write` truncates first, so a crash in that window is
    // exactly the zero-byte case handled above.
    let tmp = path.with_extension("txt.tmp");
    let wrote = std::fs::write(&tmp, &made).and_then(|()| std::fs::rename(&tmp, &path));
    match wrote {
        Ok(()) => log::info!("identity: this install is {made} (new; stored in {FILE})"),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            log::error!("identity: could not store {made} in {path:?}: {e}");
        }
    }
    made
}

/// A fresh number.
///
/// No `rand` dependency on purpose — a new crate here is a *host* build-dep in
/// the android cross-build, which is what drags in the `dlltool` shim
/// (`build_android.ps1`). The clock plus a splitmix step is plenty: this only
/// has to avoid colliding among a handful of friends, not resist anyone.
fn mint() -> String {
    let mut x = seed();
    let mut out = String::from("010");
    // **The first digit after the prefix must not be `'0'`.**
    //
    // `getSKTPhoneNumber` does not pass the property through; it rebuilds it and
    // **eats leading zeros from index 3 onwards** (0xd1c34..0xd1c58). So a minted
    // `01001234567` reaches the server as `0101234567` — a different address from
    // the one this file logs and the one the player would be told. Measured over
    // 100,000 mints of the old generator: **9.87 % diverged**.
    //
    // That is the whole bug this feature exists to close, in miniature: the name
    // the player can read is not the name their letters go to. Skipping `'0'` in
    // this one position makes `stored == wire` for every number we mint from now
    // on. Installs made before this still carry the mismatch, which is what
    // [`wire_number`] is for.
    out.push((b'1' + (splitmix(&mut x) % 9) as u8) as char);
    for _ in 1..DIGITS {
        out.push((b'0' + (splitmix(&mut x) % 10) as u8) as char);
    }
    out
}

/// What `getSKTPhoneNumber` will actually put on the wire for a stored string.
///
/// A transcription of 0xd1b48, which is a **transform, not a pass-through**:
///
/// ```text
/// 0xd1c10  ldrb r3,[sp,#0x40]     ; prop[0]
/// 0xd1c14  cmp  r3,#0x30          ; '0'
/// 0xd1c18  beq  → pre = prop[0..3], k = 3
///          else  → pre = '0' + prop[0..2], k = 2
/// 0xd1c34..0xd1c58                ; while prop[k] == '0': k++
/// 0xd1c68  memcpy(sp, prop+k, 0x20-k)
/// 0xd1ca8  MC_knlSprintk(out, "%s%s", pre, rest)
/// ```
///
/// Two callers need this and neither can guess it:
///
/// * the 「내 ID」 tile, so an install minted before the fix above shows the
///   address its letters actually arrive at rather than the one in `cs_id.txt`;
/// * the `was` field of a server claim — the server keyed that mailbox by the
///   **wire** form, so asking it to adopt the stored form looks up an address
///   that never existed.
///
/// Once the planned entry hook on `getSKTPhoneNumber` lands (LETTER_ID.md §3)
/// this becomes the identity function for everything we mint, but it still
/// describes what shipped builds did.
pub fn wire_number(stored: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(stored.len() + 1);
    let k = if stored.first() == Some(&b'0') {
        out.extend_from_slice(&stored[..stored.len().min(3)]);
        3
    } else {
        out.push(b'0');
        out.extend_from_slice(&stored[..stored.len().min(2)]);
        2
    };
    let mut k = k.min(stored.len());
    while stored.get(k) == Some(&b'0') {
        k += 1;
    }
    out.extend_from_slice(&stored[k..]);
    out
}

/// A fresh install token: `n` hex characters from the same generator.
fn mint_hex(n: usize, extra: u64) -> String {
    let mut x = seed() ^ extra;
    let mut out = String::with_capacity(n);
    for _ in 0..n {
        out.push(char::from_digit((splitmix(&mut x) % 16) as u32, 16).unwrap_or('0'));
    }
    out
}

fn seed() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    now ^ (std::process::id() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

fn splitmix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

// ---- the smallest JSON reader that does this job ---------------------------
//
// `potions.rs` writes its sidecar with `format!` and reads it back with a
// hand-rolled scanner rather than pulling in serde, for the same reason there
// is no `rand`: every dependency added here is a host build-dep in the android
// cross-build. This file is written only by us, so a scanner that finds
// `"key":` and reads one token is enough.

/// The value of `"key"` as a string, quotes stripped if it had them.
fn field(text: &str, key: &str) -> Option<String> {
    let at = text.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = text[at..].trim_start().strip_prefix(':')?.trim_start();
    if let Some(body) = rest.strip_prefix('"') {
        return Some(body[..body.find('"')?].to_owned());
    }
    let end = rest
        .find(|c: char| c == ',' || c == '}' || c.is_whitespace())
        .unwrap_or(rest.len());
    Some(rest[..end].to_owned())
}

fn is_token(t: &str) -> bool {
    t.len() == TOKEN_HEX && t.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hex to bytes, or `None`.
///
/// Indexes **bytes**, not `char`s. The obvious `&text[i * 2..i * 2 + 2]` panics
/// on a non-char-boundary, and a hand-edited `"id_hex": "가가"` is six UTF-8
/// bytes — an even length, so the `% 2` guard waves it through and the slice
/// lands mid-character. This file's own contract is that a hand edit must never
/// stop the app from starting, and this is boot-path code.
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
    raw.chunks(2)
        .map(|p| Some(nibble(p[0])? << 4 | nibble(p[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cs_id_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn a_minted_number_looks_like_one() {
        let n = mint();
        assert_eq!(n.len(), 3 + DIGITS);
        assert!(n.starts_with("010"), "{n}");
        assert!(n.chars().all(|c| c.is_ascii_digit()), "{n}");
    }

    /// Two installs must not collide, and the same install must not drift.
    #[test]
    fn the_number_is_stable_once_stored_and_fresh_when_not() {
        let dir = scratch("stable");
        let first = number(&dir, false);
        let again = number(&dir, false);
        assert_eq!(first, again, "the stored number must be reused");

        let _ = std::fs::remove_file(dir.join(FILE));
        let other = number(&dir, false);
        assert_ne!(first, other, "a fresh install should not reuse a number");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The probe's determinism depends on this.
    #[test]
    fn a_virtual_clock_pins_the_number() {
        let dir = std::env::temp_dir();
        assert_eq!(number(&dir, true), FIXED);
        assert_eq!(number(&dir, true), FIXED);
    }

    /// Non-digits in a hand-edited file must not reach the guest's buffer.
    #[test]
    fn a_hand_edited_file_is_cleaned_up() {
        let dir = scratch("edit");
        std::fs::write(dir.join(FILE), " 010-1234-5678\n").unwrap();
        assert_eq!(number(&dir, false), "01012345678");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The probe must not write either file — a dump comparison depends on the
    /// bytes the guest sends being the same on every run.
    #[test]
    fn a_virtual_clock_pins_the_identity_and_writes_no_file() {
        let dir = scratch("virtual");
        let mut me = Identity::load(&dir, true);
        assert_eq!(me.number(), FIXED);
        assert_eq!(me.id(), None);
        assert!(!me.should_prompt(), "the probe must never open the prompt");
        // EUC-KR 가름, not `"가름".as_bytes()` — that is UTF-8 and would be
        // rejected, so the assertions below would pass without testing anything.
        me.propose(&[0xB0, 0xA1, 0xB8, 0xA7]).ok();
        me.note_prompt();
        me.confirm();
        me.save();
        assert!(!dir.join(SIDECAR).exists(), "the probe wrote {SIDECAR}");
        assert!(!dir.join(FILE).exists(), "the probe wrote {FILE}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sidecar must survive anything a hand edit can do to it, because the
    /// alternative is an app that does not start.
    #[test]
    fn the_id_sidecar_round_trips_and_survives_rubbish() {
        let dir = scratch("sidecar");
        let mut me = Identity::load(&dir, false);
        let token = me.install().to_owned();
        assert!(is_token(&token), "install token is {TOKEN_HEX} hex chars");
        assert!(me.should_prompt(), "a fresh install is asked once");

        // Korean bytes as the IME hands them over: 가름 = b0 a1 b8 a7.
        let garum = [0xB0u8, 0xA1, 0xB8, 0xA7];
        me.propose(&garum).unwrap();
        assert_eq!(me.pending(), Some(&garum[..]), "the proposal is what we ask about");
        assert_eq!(me.id(), None, "a proposal is not a name we hold");
        assert_eq!(me.wire(), me.number().as_bytes(), "unclaimed ids stay off the wire");
        me.save();
        let re = Identity::load(&dir, false);
        assert_eq!(re.install(), token, "the token must not drift");
        // **An unanswered proposal must not survive.** It kept `should_prompt`
        // false forever while `wire()` still reported the number, so the player
        // held a name that was on no screen, on no wire, and could not be
        // re-offered.
        assert_eq!(re.id(), None, "an unconfirmed proposal is not restored");
        assert!(re.should_prompt(), "and the prompt is still on offer");

        let mut re = re;
        re.propose(&garum).unwrap();
        re.confirm();
        re.save();
        let done = Identity::load(&dir, false);
        assert!(done.is_claimed());
        assert_eq!(done.id(), Some(&garum[..]), "the bytes come back exactly");
        assert_eq!(done.wire(), &garum[..], "a claimed id is what we report");
        assert!(!done.should_prompt(), "having an id ends the prompting");

        // **A failed rename must not cost the name we hold.** This is the 30 s
        // timeout in the settled design: propose, get nothing back, withdraw.
        let mut done = done;
        let narum = [0xB3u8, 0xAA, 0xB8, 0xA7];
        done.propose(&narum).unwrap();
        assert_eq!(done.wire(), &garum[..], "the held name stays on the wire meanwhile");
        done.withdraw();
        assert_eq!(done.id(), Some(&garum[..]), "withdrawing a proposal keeps the old name");
        assert_eq!(done.pending(), None);
        done.save();
        assert_eq!(Identity::load(&dir, false).id(), Some(&garum[..]), "and it survives a restart");
        // A rename that *does* succeed replaces it.
        let mut again = Identity::load(&dir, false);
        again.propose(&narum).unwrap();
        again.confirm();
        again.save();
        assert_eq!(Identity::load(&dir, false).id(), Some(&narum[..]), "a confirmed rename lands");

        // Anything a crash or a hand edit can leave behind. The multi-byte
        // `id_hex` used to panic (byte slicing a `&str` off a char boundary),
        // and a truncated file used to mint a fresh token on *every* launch
        // without ever writing it — which loses the install's only claim to its
        // own ID, permanently.
        for rubbish in [
            "",
            "not json at all",
            "{",
            "{ \"install\": 7 }",
            "\0\0\0",
            "{ \"v\": 1, \"install\": \"0011223344556",       // truncated mid-write
            "{ \"id_hex\": \"가름\" }",                        // even byte length, multi-byte
            "{ \"id_hex\": \"ab가가\", \"claimed\": true }",
            "{ \"install\": \"zzzz\", \"asked\": \"lots\" }",
        ] {
            std::fs::write(dir.join(SIDECAR), rubbish).unwrap();
            let mut got = Identity::load(&dir, false);
            assert!(is_token(got.install()), "rubbish {rubbish:?} still mints a token");
            assert_eq!(got.id(), None, "rubbish {rubbish:?} must not yield an id");
            assert_eq!(got.wire(), got.number().as_bytes());
            // and it must heal, or the token drifts every launch
            let minted = got.install().to_owned();
            got.save();
            let again = Identity::load(&dir, false);
            assert_eq!(again.install(), minted, "rubbish {rubbish:?} left a drifting token");
        }
        // An id that would not pass validation today is dropped, not trusted.
        std::fs::write(
            dir.join(SIDECAR),
            format!("{{ \"install\": \"{token}\", \"id_hex\": \"25\", \"claimed\": true }}"),
        )
        .unwrap();
        assert_eq!(Identity::load(&dir, false).id(), None, "'%' must not survive a reload");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `getSKTPhoneNumber` rebuilds the string rather than passing it through,
    /// and the difference decides where a player's letters land.
    #[test]
    fn the_wire_form_is_the_normalisation_the_guest_performs() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"01077778888", b"01077778888"), // untouched — the common case
            (b"01000000000", b"010"),         // the probe's FIXED collapses to three
            (b"00007", b"0007"),              // one zero eaten from index 3
            (b"abc", b"0abc"),                // no leading '0' → one is prepended
            (b"9", b"09"),
            (b"", b"0"),
            (&[0xB0, 0xA1, 0xB8, 0xA7], &[b'0', 0xB0, 0xA1, 0xB8, 0xA7]), // 가름 → 0가름
        ];
        for (stored, want) in cases {
            assert_eq!(
                wire_number(stored),
                want.to_vec(),
                "{:?} should reach the server as {:?}",
                String::from_utf8_lossy(stored),
                String::from_utf8_lossy(want)
            );
        }
    }

    /// The address we log and the address letters arrive at must be the same
    /// string, or the whole feature is pointless — that mismatch *is* the bug.
    #[test]
    fn a_minted_number_survives_the_guests_normalisation() {
        for _ in 0..2000 {
            let n = mint();
            assert_eq!(
                wire_number(n.as_bytes()),
                n.as_bytes().to_vec(),
                "{n} would reach the server as something else"
            );
        }
        // The old generator did not have this property; prove the guard is the
        // fourth character and not luck.
        assert_eq!(wire_number(b"01001234567"), b"0101234567".to_vec());
        assert!(mint().as_bytes()[3] != b'0');
    }

    /// The prompt has to stop asking, or a player who does not want an ID is
    /// nagged on every launch forever.
    #[test]
    fn the_prompt_gives_up_after_a_few_launches() {
        let dir = scratch("prompt");
        for launch in 0..MAX_PROMPTS {
            let mut me = Identity::load(&dir, false);
            assert!(me.should_prompt(), "launch {launch} should still ask");
            me.note_prompt();
            me.save();
        }
        assert!(!Identity::load(&dir, false).should_prompt(), "then it stops");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both guest buffers are 16 bytes and one of the two writes has no clamp,
    /// so this bound is not a preference.
    #[test]
    fn an_id_can_never_overrun_the_handshake_frame() {
        assert_eq!(ID_MAX_BYTES, 14);
        // `getSKTPhoneNumber` prepends '0' when the stored string does not
        // start with one, and the field is 16 bytes including the NUL.
        assert!(ID_MAX_BYTES + 1 + 1 <= 16, "a maximal id plus '0' plus NUL must fit");
        assert!(check_id(&[b'a'; ID_MAX_BYTES]).is_ok());
        assert_eq!(check_id(&[b'a'; ID_MAX_BYTES + 1]), Err(IdProblem::TooLong));
        // Seven Hangul syllables are exactly 14 bytes.
        let seven: Vec<u8> = (0..7).flat_map(|_| [0xB0u8, 0xA1]).collect();
        assert_eq!(seven.len(), ID_MAX_BYTES);
        assert!(check_id(&seven).is_ok());
    }

    /// Every byte the shipped renderer swallows must be refused, or the ID is
    /// partly invisible on the very screens that exist to show it.
    #[test]
    fn the_bytes_the_renderer_eats_are_refused() {
        // The table at 0x10595c, plus the ';' HF_IsEscape checks separately,
        // plus '%' because setPopUp formats its argument.
        for &b in b"&|${}opqrst;%" {
            assert_eq!(
                check_id(&[b'a', b, b'b']),
                Err(IdProblem::Escape(b)),
                "{:?} must be refused",
                b as char
            );
        }
        // The asymmetry is real: uppercase O..T are not in that table.
        for &b in b"OPQRST" {
            assert!(check_id(&[b]).is_ok(), "{:?} is fine", b as char);
        }
        assert_eq!(check_id(b""), Err(IdProblem::Empty));
        assert_eq!(check_id(b"a b"), Err(IdProblem::NotAllowed(b' ')));
        assert_eq!(check_id(b"a-b"), Err(IdProblem::NotAllowed(b'-')));
        // A lead byte with nothing after it must not be stored: the recipient
        // memcpy has no clamp, and half a syllable is not a name.
        assert_eq!(check_id(&[0xB0]), Err(IdProblem::Truncated));
        assert_eq!(check_id(&[0xB0, 0x20]), Err(IdProblem::NotAllowed(0x20)));
        // Mixed is allowed — the design says 한글·영문·숫자.
        assert!(check_id(&[0xB0, 0xA1, b'2', b'6']).is_ok());
    }

    /// The ID namespace and the minted-number namespace must not overlap.
    ///
    /// Every mailbox and save slot on the server today is keyed by a string
    /// `mint()` produced. If those are legal IDs, typing one is asking for
    /// somebody else's address — and there is no way for a player to know which
    /// eleven-digit strings are taken.
    #[test]
    fn a_name_that_is_only_digits_is_not_available() {
        for _ in 0..200 {
            let n = mint();
            assert_eq!(
                check_id(n.as_bytes()),
                Err(IdProblem::AllDigits),
                "{n} is the shape every mailbox key already has"
            );
        }
        assert_eq!(check_id(b"7"), Err(IdProblem::AllDigits));
        assert_eq!(normalise_id(b"01077778888"), Err(IdProblem::AllDigits));
        // Digits *within* a name are the design's own 한글·영문·숫자.
        assert!(check_id(b"GARUM26").is_ok());
        assert!(check_id(b"26GARUM").is_ok());
        assert!(check_id(&[0xB0, 0xA1, b'2', b'6']).is_ok());
        // A single non-digit is enough, which is what makes the rule teachable.
        assert!(check_id(b"A01077778888").is_ok());
    }

    /// A player typing `garum` must get a working ID, not a rejection they
    /// could never have predicted.
    #[test]
    fn a_typed_name_is_folded_onto_what_the_game_can_draw() {
        assert_eq!(normalise_id(b"garum").unwrap(), b"GARUM");
        assert_eq!(normalise_id(b"Garum2026").unwrap(), b"GARUM2026");
        // Every lowercase letter survives folding, including the six that are
        // escapes in their lowercase form.
        for &b in b"abcdefghijklmnopqrstuvwxyz" {
            let got = normalise_id(&[b]).expect("every letter must be usable");
            assert_eq!(got, vec![b.to_ascii_uppercase()]);
        }
        // Folding does not rescue what is genuinely not allowed.
        assert_eq!(normalise_id(b"a b"), Err(IdProblem::NotAllowed(b' ')));
        assert_eq!(normalise_id(b"100%"), Err(IdProblem::Escape(b'%')));
        assert_eq!(normalise_id(b"a&b"), Err(IdProblem::Escape(b'&')));
        assert_eq!(normalise_id(&[b'a'; ID_MAX_BYTES + 1]), Err(IdProblem::TooLong));
        // Hangul passes through untouched, and a trail byte is never folded.
        let garum = [0xB0u8, 0xA1, 0xB8, 0xA7];
        assert_eq!(normalise_id(&garum).unwrap(), garum.to_vec());
        let mixed = [0xB0u8, 0xA1, b'x'];
        assert_eq!(normalise_id(&mixed).unwrap(), vec![0xB0, 0xA1, b'X']);
        // And what comes out always passes the validator — that is the
        // contract the sidecar reload depends on.
        for raw in [&b"garum"[..], &garum[..], &mixed[..], b"ABC123"] {
            let folded = normalise_id(raw).unwrap();
            assert!(check_id(&folded).is_ok(), "{folded:?} must revalidate");
        }
    }
}
