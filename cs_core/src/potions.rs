//! The cash 비약 the original never sold, and the buffs they leave behind.
//!
//! Four items appended to the cash shop's list and bought like anything else:
//! 성장 (x2 exp), 슈퍼성장 (x4 exp), 종결자 (equipment level requirements down
//! by five) — three timers held **here** rather than in the guest — and
//! 작명 ([`Effect::Rename`]), which is not a timer at all and holds nothing.
//!
//! Two decisions shape the whole module.
//!
//! **They are not bag items.** Buying one never reaches
//! `ItemBag_checkAndSetItem`, so no item id has to be invented, no icon added
//! and no use-handler written: `patch::host_cash_potion_buy` intercepts the one
//! function every cash purchase converges on and turns it into a timer. The
//! shop entries still carry a real item id (0x23, 고대비약) because the shop
//! picks its icon by id alone — `getImage(*_imgITMIcon, id)` — and an id outside
//! that array would read past it.
//!
//! **The clock is the wall clock.** A week is not a session. `Vm::now_ms` is
//! process-relative, so storing "milliseconds left" would stop the timer
//! whenever the app is closed and a seven-day buff would effectively be
//! permanent. What is stored is an **absolute epoch-millisecond expiry**, which
//! also means nothing has to be written when the app exits — see
//! [`crate::Vm::wall_now_ms`], which keeps the headless probe deterministic by
//! answering from the virtual clock instead.
//!
//! The guest's own buff table (`hero + 0x2ec + kind*0x10`) is deliberately
//! unused: its slots are allocated to skill buffs, `HERO_Reset_BUffAll` wipes it
//! on every death and event transition, and it is not saved. None of that
//! survives "a week, across restarts".

use std::path::Path;

/// Which of the three. The discriminants are the sidecar's field order and the
/// index into [`PotionState::until`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// 성장의 비약 — experience x2.
    Exp2 = 0,
    /// 슈퍼성장의 비약 — experience x4.
    Exp4 = 1,
    /// 종결자의 비약 — equipment level requirements down by
    /// [`level_delta`].
    Finisher = 2,
}

pub const KINDS: [Kind; 3] = [Kind::Exp2, Kind::Exp4, Kind::Finisher];

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Exp2 => "exp2_until",
            Kind::Exp4 => "exp4_until",
            Kind::Finisher => "finisher_until",
        }
    }

    /// What `--potion` and the log line call it.
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Exp2 => "exp2",
            Kind::Exp4 => "exp4",
            Kind::Finisher => "finisher",
        }
    }
}

/// What buying one row actually does.
///
/// The three original 비약 are timers; 작명의 비약 is not. It fires once, holds
/// nothing, and so has no slot in [`PotionState`], no HUD line and no sidecar
/// field — the pet's new name is state the *guest* owns and saves
/// (`GameFrame_savePetData`). Keeping that out of the timer machinery is the
/// whole reason this enum exists rather than a fourth [`Kind`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// A week-long buff, held here under this [`Kind`].
    Timed(Kind),
    /// 작명의 비약 — reopen the pet-naming IME the story shows exactly once.
    /// See `patch::Vm::open_pet_rename`.
    Rename,
}

/// One row appended to `_merchant`. Everything the shop draws comes out of that
/// array — name, description, price and grade colour are the *file's*, not the
/// item database's — so a new entry needs no game data at all.
pub struct Shelf {
    pub effect: Effect,
    /// Item id, and therefore the icon: `getImage(*_imgITMIcon, id)`.
    pub id: u8,
    /// Sub-type. The shipped file never goes above 2, so 0x10 and up cannot
    /// collide with a real 고대비약 and `(id, sub)` identifies ours exactly.
    pub sub: u8,
    pub price: i32,
    pub name: &'static [u8],
    pub desc: &'static [u8],
    /// 4 is what all twelve shipped cash items use.
    pub grade: u8,
}

/// EUC-KR, byte literals, never `str`. A `String` round trip would replace every
/// byte over 0x7f with U+FFFD and the game's font would happily draw the result
/// as plausible-looking gibberish — see the CLAUDE.md rule.
///
/// The descriptions say 일주일 because that is the shipped duration;
/// `CS_POTION_DAYS` is a test knob and does not rewrite them.
pub const SHELF: [Shelf; 4] = [
    Shelf {
        effect: Effect::Timed(Kind::Exp2),
        id: 0x23,
        sub: 0x10,
        price: 1000,
        // 성장의 비약
        name: b"\xbc\xba\xc0\xe5\xc0\xc7 \xba\xf1\xbe\xe0",
        // 일주일 동안 획득 경험치가 2배가 됩니다
        desc: b"\xc0\xcf\xc1\xd6\xc0\xcf \xb5\xbf\xbe\xc8 \xc8\xb9\xb5\xe6 \
               \xb0\xe6\xc7\xe8\xc4\xa1\xb0\xa1 2\xb9\xe8\xb0\xa1 \xb5\xcb\xb4\xcf\xb4\xd9",
        grade: 4,
    },
    Shelf {
        effect: Effect::Timed(Kind::Exp4),
        id: 0x23,
        sub: 0x11,
        price: 2000,
        // 슈퍼성장의 비약
        name: b"\xbd\xb4\xc6\xdb\xbc\xba\xc0\xe5\xc0\xc7 \xba\xf1\xbe\xe0",
        // 일주일 동안 획득 경험치가 4배가 됩니다
        desc: b"\xc0\xcf\xc1\xd6\xc0\xcf \xb5\xbf\xbe\xc8 \xc8\xb9\xb5\xe6 \
               \xb0\xe6\xc7\xe8\xc4\xa1\xb0\xa1 4\xb9\xe8\xb0\xa1 \xb5\xcb\xb4\xcf\xb4\xd9",
        grade: 4,
    },
    Shelf {
        effect: Effect::Timed(Kind::Finisher),
        id: 0x23,
        sub: 0x12,
        price: 1500,
        // 종결자의 비약
        name: b"\xc1\xbe\xb0\xe1\xc0\xda\xc0\xc7 \xba\xf1\xbe\xe0",
        // 일주일 동안 장비 요구레벨이 5 낮아집니다
        desc: b"\xc0\xcf\xc1\xd6\xc0\xcf \xb5\xbf\xbe\xc8 \xc0\xe5\xba\xf1 \
               \xbf\xe4\xb1\xb8\xb7\xb9\xba\xa7\xc0\xcc 5 \xb3\xb7\xbe\xc6\xc1\xfd\xb4\xcf\xb4\xd9",
        grade: 4,
    },
    Shelf {
        effect: Effect::Rename,
        id: 0x23,
        sub: 0x13,
        price: 1000,
        // 작명의 비약
        name: b"\xc0\xdb\xb8\xed\xc0\xc7 \xba\xf1\xbe\xe0",
        // 펫의 이름을 다시 지어줄 수 있습니다
        desc: b"\xc6\xea\xc0\xc7 \xc0\xcc\xb8\xa7\xc0\xbb \xb4\xd9\xbd\xc3 \
               \xc1\xf6\xbe\xee\xc1\xd9 \xbc\xf6 \xc0\xd6\xbd\xc0\xb4\xcf\xb4\xd9",
        grade: 4,
    },
];

/// Whether the two growth 비약 are on the shelf. **Off**, by request.
///
/// They are not removed, only unshelved: the cave's 환경설정 row 경험치배율
/// (기본/X2/X4) does the same thing for free, at any time, and the two
/// *multiply together* — so selling them is selling the player something they
/// already have, twice. Everything behind them stays wired up, because a buff
/// bought before this change is still in `cs_potions.json` and still has days
/// to run; it keeps its HUD line and its multiplier until it expires, which is
/// the difference between withdrawing a product and confiscating one.
///
/// `CS_POTION_GROWTH=1` puts them back.
pub fn growth_enabled() -> bool {
    crate::patch::knob("CS_POTION_GROWTH", 0) != 0
}

/// The rows this shop opening should show.
///
/// 작명의 비약 is listed **only when there is a pet** (`hero[0x62c]`, the flag
/// `Event_getPet` sets). An item that cannot do anything is worse than an item
/// that is not for sale, and gating the listing rather than the purchase means
/// the failure case does not have to be designed at all: a row that is not on
/// the shelf cannot be selected, so `patch::Vm::host_cash_potion_buy` never sees
/// it. The growth pair is unshelved the same way and for the same reason.
pub fn shelf_for(has_pet: bool) -> impl Iterator<Item = &'static Shelf> {
    SHELF.iter().filter(move |s| match s.effect {
        Effect::Timed(Kind::Exp2 | Kind::Exp4) => growth_enabled(),
        Effect::Timed(_) => true,
        Effect::Rename => rename_enabled() && has_pet,
    })
}

/// HUD labels and the three unit words, EUC-KR.
///
/// The leading `r` and the `&` before the first number are the font's
/// **zero-width colour escapes** (`HF_IsEscape` + `HF_getColorChar`), copied
/// from the game's own strings — `getInCodeString(0x157)` is `r요구레벨 &%d`.
/// They cost no pixels and no width.
const HUD_LABEL: [&[u8]; 3] = [
    b"\xbc\xba\xc0\xe5 x2",      // 성장 x2
    b"\xbc\xba\xc0\xe5 x4",      // 성장 x4
    b"\xc1\xbe\xb0\xe1\xc0\xda", // 종결자
];
const HUD_DAY: &[u8] = b"\xc0\xcf"; // 일
const HUD_HOUR: &[u8] = b"\xbd\xc3\xb0\xa3"; // 시간
const HUD_MIN: &[u8] = b"\xba\xd0"; // 분

/// The sidecar file beside the save. Public because the save bundle
/// (`savebundle.rs`) carries it with the container.
pub const FILE: &str = "cs_potions.json";
const SIDECAR: &str = FILE;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

/// Expiry of each buff, as absolute epoch milliseconds.
///
/// `0` is "never owned"; [`i64::MAX`] is the permanent mode
/// (`CS_POTION_DAYS=0`), which needs no special case anywhere else because a
/// comparison against it is always false.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct PotionState {
    until: [i64; 3],
    /// Set by `grant`, cleared by `save`. The expiry is absolute, so a run that
    /// bought nothing has nothing to write.
    dirty: bool,
}

impl PotionState {
    /// Read the sidecar. A missing, unreadable or malformed file is not an
    /// error — it means "no buffs" and warns once. Failing here would mean the
    /// app does not start.
    pub fn load(storage: &Path) -> Self {
        let path = storage.join(SIDECAR);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                log::warn!("potions: cannot read {}: {e}; starting with none", path.display());
                return Self::default();
            }
        };
        let mut s = Self::default();
        for k in KINDS {
            match number(&text, k.key()) {
                Some(v) => s.until[k as usize] = v,
                None => log::warn!("potions: {} has no {}; treated as none", SIDECAR, k.key()),
            }
        }
        s
    }

    /// Write the sidecar if anything changed. Plain `std::fs::write`, no
    /// buffering — same reason `files.rs` writes saves unbuffered: the process
    /// can be killed at any moment and `cs_android` exits by `process::exit`.
    pub fn save(&mut self, storage: &Path) {
        if !self.dirty {
            return;
        }
        let _ = std::fs::create_dir_all(storage);
        let path = storage.join(SIDECAR);
        let json = format!(
            "{{ \"v\": 1, \"{}\": {}, \"{}\": {}, \"{}\": {} }}\n",
            Kind::Exp2.key(),
            self.until[0],
            Kind::Exp4.key(),
            self.until[1],
            Kind::Finisher.key(),
            self.until[2],
        );
        match std::fs::write(&path, json) {
            Ok(()) => self.dirty = false,
            Err(e) => log::warn!("potions: cannot write {}: {e}", path.display()),
        }
    }

    /// Add one purchase. Buying again while a buff is live **extends** it rather
    /// than restarting it, which is the usual convention for this kind of item
    /// and is also the only behaviour that cannot lose the player time.
    pub fn grant(&mut self, kind: Kind, now: Option<i64>) {
        let i = kind as usize;
        self.until[i] = match (duration_ms(), now) {
            // Permanent mode, or a clock we cannot read: no expiry to compute.
            (None, _) | (_, None) => i64::MAX,
            (Some(d), Some(now)) => self.until[i].max(now).saturating_add(d),
        };
        self.dirty = true;
        log::info!("potions: {} granted, until {}", kind.tag(), self.until[i]);
    }

    /// Is it running? `now = None` means the host clock could not be read — the
    /// buff is then treated as live, because losing something bought is worse
    /// than a few minutes of it outliving its week.
    pub fn active(&self, kind: Kind, now: Option<i64>) -> bool {
        let until = self.until[kind as usize];
        until != 0 && now.is_none_or(|n| n < until)
    }

    pub fn any(&self, now: Option<i64>) -> bool {
        KINDS.iter().any(|&k| self.active(k, now))
    }

    /// 1, 2 or 4. Both experience potions can be held at once; the larger wins
    /// and each expires on its own, so a x4 running out drops back to x2 rather
    /// than to nothing.
    pub fn exp_mul(&self, now: Option<i64>) -> u32 {
        if self.active(Kind::Exp4, now) {
            4
        } else if self.active(Kind::Exp2, now) {
            2
        } else {
            1
        }
    }

    /// One ASCII line per running buff, for the boot log. This is how "the
    /// week kept running while the app was closed" is checked on a device:
    /// buy, quit, relaunch, and read the remaining time back out of logcat.
    pub fn describe(&self, now: Option<i64>) -> Vec<String> {
        KINDS
            .iter()
            .filter(|&&k| self.active(k, now))
            .map(|&k| match self.remaining(k, now) {
                Some(left) => {
                    format!("{} {}d {}h left", k.tag(), left / DAY_MS, (left % DAY_MS) / HOUR_MS)
                }
                None => format!("{} (no expiry)", k.tag()),
            })
            .collect()
    }

    /// Milliseconds left, or `None` when it never runs out (permanent mode, or
    /// an unreadable clock).
    fn remaining(&self, kind: Kind, now: Option<i64>) -> Option<i64> {
        let until = self.until[kind as usize];
        match now {
            Some(n) if until != i64::MAX => Some((until - n).max(0)),
            _ => None,
        }
    }

    /// One HUD line in EUC-KR, or `None` if that buff is not running.
    ///
    /// Three formats, because a week and ten minutes cannot share one: days and
    /// hours, hours and minutes, then minutes. Both parts are floored — 6일
    /// 23시간 is followed by 6일 22시간 — and seconds are never shown: on a
    /// seven-day timer a digit that changes every second is noise, and it would
    /// make the HUD differ frame to frame for no reason.
    pub fn hud_line(&self, kind: Kind, now: Option<i64>) -> Option<Vec<u8>> {
        if !self.active(kind, now) {
            return None;
        }
        let mut s = Vec::with_capacity(32);
        s.push(b'r');
        s.extend_from_slice(HUD_LABEL[kind as usize]);
        if let Some(left) = self.remaining(kind, now) {
            s.extend_from_slice(b" &");
            let (a, unit_a, b, unit_b) = if left >= DAY_MS {
                (left / DAY_MS, HUD_DAY, (left % DAY_MS) / HOUR_MS, Some(HUD_HOUR))
            } else if left >= HOUR_MS {
                (left / HOUR_MS, HUD_HOUR, (left % HOUR_MS) / 60_000, Some(HUD_MIN))
            } else {
                (left / 60_000, HUD_MIN, 0, None)
            };
            s.extend_from_slice(a.to_string().as_bytes());
            s.extend_from_slice(unit_a);
            if let Some(unit_b) = unit_b {
                s.push(b' ');
                s.extend_from_slice(b.to_string().as_bytes());
                s.extend_from_slice(unit_b);
            }
        }
        Some(s)
    }
}

/// Pull `"key": <integer>` out of the sidecar without a JSON crate. The file is
/// three numbers written by the line above; anything else it could contain is
/// treated as absent.
fn number(text: &str, key: &str) -> Option<i64> {
    let at = text.find(&format!("\"{key}\""))?;
    let rest = text[at + key.len() + 2..].trim_start().strip_prefix(':')?.trim_start();
    // Byte-wise, so the end index is always a char boundary whatever follows
    // the number.
    let b = rest.as_bytes();
    let mut end = usize::from(b.first() == Some(&b'-'));
    while b.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    rest[..end].parse().ok()
}

// ---- knobs --------------------------------------------------------------
//
// Read from the environment *and* baked in at build time: the device has no
// environment to set. Same shape as `CS_SERVER_HOST`. Changing a build-time
// value used to need `cargo clean -p cs_core` — `build.rs` now tells cargo
// about every one of these, so a plain rebuild is enough.

fn knob(name: &'static str, default: i32) -> i32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| match name {
            "CS_POTIONS" => option_env!("CS_POTIONS").and_then(|v| v.parse().ok()),
            "CS_POTION_DAYS" => option_env!("CS_POTION_DAYS").and_then(|v| v.parse().ok()),
            "CS_POTION_MINUTES" => option_env!("CS_POTION_MINUTES").and_then(|v| v.parse().ok()),
            "CS_POTION_HUD" => option_env!("CS_POTION_HUD").and_then(|v| v.parse().ok()),
            "CS_POTION_EXP" => option_env!("CS_POTION_EXP").and_then(|v| v.parse().ok()),
            "CS_POTION_LEVEL" => option_env!("CS_POTION_LEVEL").and_then(|v| v.parse().ok()),
            "CS_POTION_LEVEL_DELTA" => {
                option_env!("CS_POTION_LEVEL_DELTA").and_then(|v| v.parse().ok())
            }
            "CS_POTION_RENAME" => option_env!("CS_POTION_RENAME").and_then(|v| v.parse().ok()),
            _ => None,
        })
        .unwrap_or(default)
}

/// The whole feature. Off restores the shipped twelve-item shop exactly, and
/// every patch declines.
///
/// It rides `CS_FREE_CASH` as well, and not for tidiness: with the charge
/// answering off, `AndroidService::requestMTX` is refused, `MTX_Result_CB` is
/// never called, and `NpcUI_setBoughtItemOnCASH` — where the buff is granted —
/// is unreachable. Listing three items that cannot be bought would be worse
/// than not listing them.
pub fn enabled() -> bool {
    knob("CS_POTIONS", 1) != 0 && crate::net::free_cash()
}

/// How long one purchase lasts, or `None` for permanent (`CS_POTION_DAYS=0`,
/// the 오토루팅 flavour). `CS_POTION_MINUTES` overrides it — walking the expiry
/// path on a device is impossible otherwise.
pub fn duration_ms() -> Option<i64> {
    let minutes = knob("CS_POTION_MINUTES", 0);
    if minutes > 0 {
        return Some(minutes as i64 * 60_000);
    }
    match knob("CS_POTION_DAYS", 7) {
        0 => None,
        d => Some(d.max(0) as i64 * DAY_MS),
    }
}

/// 0 off, 1 (default) in the in-game menu, 2 there and on the world screen too.
///
/// The menu is where this was asked for and it is also the only place with room:
/// `GameFrame_drawGameUI` puts WORLD / SHOP / MAP / SAVE across x 95..305 at
/// y 0..12, which the first buff line would sit on top of (measured in a probe
/// dump, not assumed). 2 is there for anyone who would rather have the timers
/// than those four labels.
pub fn hud_mode() -> i32 {
    knob("CS_POTION_HUD", 1)
}

pub fn exp_enabled() -> bool {
    knob("CS_POTION_EXP", 1) != 0
}

pub fn level_enabled() -> bool {
    knob("CS_POTION_LEVEL", 1) != 0
}

/// How many levels 종결자 takes off a requirement. Clamped: a negative value
/// would raise requirements, and past 99 every gate is open anyway.
pub fn level_delta() -> i32 {
    knob("CS_POTION_LEVEL_DELTA", 5).clamp(0, 99)
}

/// 작명의 비약. `CS_POTION_RENAME=0` removes the **row**, not just the effect,
/// which is what restores the previous three-item shop exactly — unlike
/// `CS_POTION_EXP`/`CS_POTION_LEVEL`, whose items still make sense on the shelf
/// (they are timers that can be bought and simply do nothing), an item that
/// cannot open the naming screen has no reason to be there.
pub fn rename_enabled() -> bool {
    knob("CS_POTION_RENAME", 1) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_700_000_000_000;

    fn week() -> i64 {
        7 * DAY_MS
    }

    #[test]
    fn grant_extends_rather_than_restarts() {
        let mut s = PotionState::default();
        s.grant(Kind::Exp2, Some(T0));
        assert_eq!(s.until[0], T0 + week());
        // Half a week later, a second purchase adds a whole week to what is
        // left rather than resetting the clock.
        s.grant(Kind::Exp2, Some(T0 + week() / 2));
        assert_eq!(s.until[0], T0 + 2 * week());
    }

    #[test]
    fn expired_is_not_active_but_unreadable_clock_is() {
        let mut s = PotionState::default();
        s.grant(Kind::Finisher, Some(T0));
        assert!(s.active(Kind::Finisher, Some(T0 + week() - 1)));
        assert!(!s.active(Kind::Finisher, Some(T0 + week())));
        // A clock we cannot read must not silently expire what was bought.
        assert!(s.active(Kind::Finisher, None));
        // But it cannot invent a buff that was never bought.
        assert!(!PotionState::default().active(Kind::Finisher, None));
    }

    #[test]
    fn exp_multiplier_takes_the_larger_and_falls_back() {
        let mut s = PotionState::default();
        assert_eq!(s.exp_mul(Some(T0)), 1);
        s.grant(Kind::Exp2, Some(T0));
        assert_eq!(s.exp_mul(Some(T0)), 2);
        s.until[Kind::Exp4 as usize] = T0 + HOUR_MS;
        assert_eq!(s.exp_mul(Some(T0)), 4);
        // x4 runs out first: back to x2, not to nothing.
        assert_eq!(s.exp_mul(Some(T0 + HOUR_MS)), 2);
    }

    #[test]
    fn hud_line_picks_a_format_per_magnitude() {
        let mut s = PotionState::default();
        s.until[Kind::Exp2 as usize] = T0 + 6 * DAY_MS + 23 * HOUR_MS + 59_000;
        let d = s.hud_line(Kind::Exp2, Some(T0)).unwrap();
        assert!(d.ends_with(HUD_HOUR) && d.starts_with(b"r"));
        assert_eq!(&d[d.len() - HUD_HOUR.len() - 2..d.len() - HUD_HOUR.len()], b"23");

        s.until[Kind::Exp2 as usize] = T0 + 5 * HOUR_MS + 12 * 60_000;
        let h = s.hud_line(Kind::Exp2, Some(T0)).unwrap();
        assert!(h.ends_with(HUD_MIN));

        s.until[Kind::Exp2 as usize] = T0 + 47 * 60_000;
        let m = s.hud_line(Kind::Exp2, Some(T0)).unwrap();
        assert!(m.ends_with(HUD_MIN) && m.len() < h.len());

        // Nothing running draws nothing at all.
        assert!(s.hud_line(Kind::Finisher, Some(T0)).is_none());
        // Permanent: the name, with no countdown behind it.
        s.until[Kind::Finisher as usize] = i64::MAX;
        assert_eq!(s.hud_line(Kind::Finisher, Some(T0)).unwrap(), {
            let mut v = vec![b'r'];
            v.extend_from_slice(HUD_LABEL[2]);
            v
        });
    }

    #[test]
    fn sidecar_round_trips_and_survives_rubbish() {
        let dir = std::env::temp_dir().join("cs_potions_test");
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = PotionState::default();
        s.grant(Kind::Exp4, Some(T0));
        s.save(&dir);
        assert_eq!(PotionState::load(&dir).until, s.until);

        std::fs::write(dir.join(SIDECAR), "not json at all").unwrap();
        assert_eq!(PotionState::load(&dir), PotionState::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shop_rows_fit_the_arrays_they_are_written_into() {
        for s in &SHELF {
            assert!(s.name.len() < 0x28, "name must fit its 0x28 slot with a NUL");
            assert!(s.desc.len() < 0xff, "desc must fit its 0xff slot with a NUL");
            assert!(s.sub >= 0x10, "sub-types below 0x10 belong to the shipped file");
        }
        // `(id, sub)` is what `host_cash_potion_buy` identifies a row by, so two
        // rows sharing a pair would make one of them unbuyable.
        for (i, a) in SHELF.iter().enumerate() {
            for b in &SHELF[i + 1..] {
                assert!(a.id != b.id || a.sub != b.sub, "two rows share (id, sub)");
            }
        }
    }

    #[test]
    fn the_naming_row_is_only_shelved_once_there_is_a_pet() {
        let rename = |has_pet| shelf_for(has_pet).filter(|s| s.effect == Effect::Rename).count();
        assert_eq!(rename(false), 0);
        // With a pet, exactly one naming row — knob permitting, which is the
        // default. Guarded so setting `CS_POTION_RENAME=0` in a shell does not
        // fail the suite.
        assert_eq!(rename(true), usize::from(rename_enabled()));
        // ...and the pet has no bearing on which timers are shelved.
        let timers = |p| shelf_for(p).filter(|s| matches!(s.effect, Effect::Timed(_))).count();
        assert_eq!(timers(false), timers(true));
    }

    /// The growth pair is off the shelf, and nothing else went with it.
    ///
    /// Unshelving is not removal: `SHELF` still carries all four rows and
    /// `Kind::Exp2`/`Exp4` are still live, because a buff bought before the
    /// change is in `cs_potions.json` with days left and keeps running. What
    /// has to hold is that the shop no longer offers them and 종결자의 비약
    /// still does.
    #[test]
    fn the_growth_pair_is_off_the_shelf_but_not_gone() {
        assert_eq!(SHELF.len(), 4, "the rows are still declared");
        let shelved: Vec<Effect> = shelf_for(false).map(|s| s.effect).collect();
        let growth = [Effect::Timed(Kind::Exp2), Effect::Timed(Kind::Exp4)];
        for g in growth {
            assert_eq!(
                shelved.contains(&g),
                growth_enabled(),
                "{g:?} follows CS_POTION_GROWTH and nothing else"
            );
        }
        assert!(shelved.contains(&Effect::Timed(Kind::Finisher)), "종결자의 비약 is still sold");
        // And the machinery behind the pair is untouched, so an unexpired buff
        // still has a name to be saved and reloaded under.
        for k in [Kind::Exp2, Kind::Exp4] {
            assert!(!k.tag().is_empty());
        }
    }
}
