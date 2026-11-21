//! 공용창고 — one item chest shared by every save slot on this install.
//!
//! The chest itself is the easy half. The hard half, and the reason this module
//! exists separately from the screen that drives it, is that **moving an item
//! between the bag and the chest is two writes to two different files** and the
//! process can die between them:
//!
//! * the bag lives in the guest's save container (`kickass`), written by
//!   `GameFrame_save_hero_data` + `GameFrame_saveGame` — already *two*
//!   independent `MX_sdSaveData` calls of its own;
//! * the chest lives here, in [`FILE`] next to `cs_potions.json`.
//!
//! Whichever order those go in, a crash in the gap either loses the item or
//! duplicates it. There is no ordering that avoids both, because the two stores
//! cannot be written atomically together. So the chest carries an **in-flight
//! slot** and the move is three steps:
//!
//! | | deposit (bag → chest) | withdraw (chest → bag) |
//! |---|---|---|
//! | 1 | `pending = X`, list unchanged | `pending = X`, X still listed |
//! | 2 | the guest removes X from the bag and saves | the guest puts X in the bag and saves |
//! | 3 | `pending = none`, X **added** to the list | `pending = none`, X **removed** from the list |
//!
//! After step 1 the chest file records exactly what is in flight, so a resume
//! only has to answer one question — **is X in that slot's bag?** — and
//! [`verdict`] turns the answer into commit or roll back. Either way X ends up
//! in exactly one place. The four cells are the four rows of
//! `the_four_ways_a_transfer_can_die`.
//!
//! Two rules make that question answerable.
//!
//! **X is identified by the 64-bit stamp the record carries** (`item + 0xd0`,
//! the same value `Item_saveStorage` writes and `Item_loadStorage` matches
//! equipment against). For a deposit that is the stamp the item already has.
//! For a withdraw the item does not exist yet, so the stamp is **chosen before
//! step 1** and written into the record; step 2 then creates the item and
//! stamps it with exactly that. See [`Vault::arm_withdraw`].
//!
//! **The chest's own ids are not the game's stamps.** The game mints a stamp
//! from `MC_knlCurrentTime()` and only dedupes against the immediately previous
//! one, so stamps are unique *within a run* and nothing more — this repository
//! has already been bitten by that once (CLAUDE.md, 「인벤의 둔기가 주무기
//! 자리를 뺏던 것」). A chest is precisely a place where items minted in
//! different runs and different slots meet and then leave for a third, so it
//! numbers its own contents with [`Store::next_id`] and never reuses a number.
//!
//! ## Gold rides the same protocol
//!
//! The chest also holds **gold** (one number, `gold N` in the file), moved in
//! and out through the same three steps with the same in-flight slot. The
//! question a resume asks is different, because gold has no stamp: step 1
//! records the hero's gold **before** the move and the amount, so a resume
//! reads that slot's hero and asks *is it still `before`, or already
//! `before ∓ amount`?* — [`gold_verdict`]. Neither answer means the save
//! changed in some other way in between, and that is the one case the
//! protocol cannot decide; it rolls back rather than mint gold, and says so
//! in the log. At most one move of either kind is in flight at a time
//! ([`Vault::in_flight`]).
//!
//! ## An unreadable chest is not an empty chest
//!
//! [`Vault::open`] refuses to guess. A file that exists but does not parse puts
//! the vault in [`State::Broken`], where every operation says no and **nothing
//! is ever written**. The alternative — treating it as empty — is the bug the
//! letter server shipped and CLAUDE.md records: the first write after the
//! misread makes the empty view permanent. Here that would be the whole chest.

use std::path::{Path, PathBuf};

/// `cs_vault.txt`, beside `cs_potions.json` in the app's storage directory.
///
/// A host file rather than a record in the guest's `kickass` container, which
/// is where 외형변경 (`CSSKIN01`) and the extra 환경설정 rows (`CSOPT001`) put
/// theirs. The protocol above is the reason: `MX_sdSaveData` rewrites the
/// *whole* container every call, so a chest living inside it would share a
/// failure domain with the very save it is being reconciled against — a torn
/// container could lose both sides at once. A separate file replaced by
/// `rename` cannot tear at all.
pub const FILE: &str = "cs_vault.txt";

/// The game's own serialised item, byte for byte: what `Item_saveStorage`
/// emits per bag entry and what `Item_loadStorage` reads back.
///
/// Storing the game's record rather than a format of our own means the chest
/// round-trips every per-instance field the save does — enchant level, the
/// equip flag, the six stat halfwords, the stamp — without this module having
/// to know what any of them mean, and it cannot drift from what the game
/// believes an item is.
pub const REC: usize = 0x24;

/// Field offsets inside a [`Record`], read off `Item_saveStorage` (writer) and
/// `Item_loadStorage` (reader), which agree.
pub mod rec {
    /// `item[0x96]` — kind. `ITEM_CreateItem` argument 0.
    pub const KIND: usize = 0;
    /// `item[0]` — index within the kind, signed. `ITEM_CreateItem` argument 1.
    pub const IDX: usize = 1;
    /// `item[0x97]` — stack count, signed. `ITEM_CreateItem` argument 2.
    pub const COUNT: usize = 2;
    /// The tab the entry was found in. Written by the save loop's own counter,
    /// not by the item; the chest keeps it only so a record is a record.
    pub const TAB: usize = 3;
    /// `item[0xd0]` — the 64-bit stamp, **little-endian**: `UM_Int64ToByte`
    /// stores `r0` (the low word) at +0..3 and `r1` at +4..7, low byte first
    /// in each. `UM_Int16ToByte` is little-endian too. Worth writing down
    /// because the same binary's *network* helpers (`UM_ByteToShort_BIG`) are
    /// big-endian, so neither order can be assumed from the other.
    pub const STAMP: usize = 28;
}

/// One serialised item, 36 bytes.
pub type Record = [u8; REC];

/// Read the stamp a record carries.
pub fn stamp_of(r: &Record) -> i64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&r[rec::STAMP..rec::STAMP + 8]);
    i64::from_le_bytes(v)
}

/// Overwrite a record's stamp. Used by [`Vault::arm_withdraw`], which must fix
/// the identity of an item that does not exist yet.
pub fn set_stamp(r: &mut Record, stamp: i64) {
    r[rec::STAMP..rec::STAMP + 8].copy_from_slice(&stamp.to_le_bytes());
}

/// An item sitting in the chest.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Held {
    /// Chest-issued, monotonic, never reused. Not the game's stamp.
    pub id: u64,
    pub rec: Record,
}

/// Which direction the in-flight move is going.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// Bag → chest. Step 3 **adds** to the list.
    Deposit,
    /// Chest → bag. Step 3 **removes** from the list.
    Withdraw,
}

impl Op {
    fn tag(self) -> &'static str {
        match self {
            Op::Deposit => "deposit",
            Op::Withdraw => "withdraw",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "deposit" => Some(Op::Deposit),
            "withdraw" => Some(Op::Withdraw),
            _ => None,
        }
    }
}

/// The in-flight slot: at most one move is ever half-done.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Pending {
    pub op: Op,
    /// The save slot whose bag is the other side, as `GameFrame_load_hero_data`
    /// numbers them (1-based). Resuming means loading *this* slot to ask its
    /// bag about [`Pending::stamp`].
    pub slot: u8,
    /// The chest id this record has (deposit: the one it will get; withdraw:
    /// the one it already has).
    pub id: u64,
    pub rec: Record,
}

impl Pending {
    /// The stamp the bag side carries, which is the whole question a resume
    /// asks. Always the record's own — [`Vault::arm_withdraw`] makes sure of
    /// that for the direction where the item does not exist yet.
    pub fn stamp(&self) -> i64 {
        stamp_of(&self.rec)
    }
}

/// Turn "is the item in that slot's bag?" into commit or roll back.
///
/// The one place the protocol's table lives. Pure so the four cells can be
/// tested without a VM, a save file, or a crash.
///
/// * **deposit** — the bag still has it, so step 2 never landed: roll back and
///   the item is simply never deposited. If the bag does not, step 2 landed and
///   the item exists nowhere else: commit, or it is lost.
/// * **withdraw** — the mirror. The bag has it means step 2 landed, so the list
///   entry is a duplicate: commit the removal. The bag does not means step 2
///   never landed: roll back and the item stays in the chest.
pub fn verdict(op: Op, in_bag: bool) -> bool {
    match op {
        Op::Deposit => !in_bag,
        Op::Withdraw => in_bag,
    }
}

/// What a resume did, for the log and for the tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Settled {
    /// Step 3 ran: the chest now reflects the move.
    Committed,
    /// Step 2 never landed: the chest is back where it started.
    RolledBack,
}

/// The most gold the game itself lets a hero hold — the literal
/// `ProcRecvPacket_Single+0x6e4` clamps to after crediting a letter's gold
/// (`cmp r3, sb` against `0x3b9ac9ff`). The chest keeps the same ceiling, so
/// neither side can be pushed past what the game's own arithmetic expects.
pub const GOLD_CAP: u32 = 999_999_999;

/// The in-flight slot for a gold move. Like [`Pending`], at most one is ever
/// half-done, and never alongside an item move.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GoldPending {
    pub op: Op,
    /// The save slot whose hero is the other side, 1-based.
    pub slot: u8,
    /// How much is moving.
    pub amount: u32,
    /// The hero's gold as it stood **before** step 2. Gold has no stamp, so
    /// this is what a resume compares against; see [`gold_verdict`].
    pub before: u32,
}

impl GoldPending {
    /// The hero's gold once step 2 has run.
    pub fn after(&self) -> u32 {
        match self.op {
            Op::Deposit => self.before.wrapping_sub(self.amount),
            Op::Withdraw => self.before.wrapping_add(self.amount),
        }
    }
}

/// Turn "what is that slot's hero holding now?" into commit, roll back, or
/// *cannot tell*.
///
/// `Some(true)` — the hero already holds the post-move amount, so step 2
/// landed and the chest must follow or the gold exists nowhere (deposit) or
/// twice (withdraw). `Some(false)` — the hero still holds the pre-move amount,
/// so step 2 never ran and the chest stays as it was. `None` — neither, which
/// only happens if the save moved on between the crash and the resume; the
/// caller rolls back, because guessing is the one thing that can mint gold.
///
/// `amount` is never zero (refused before step 1), so the two answers cannot
/// coincide.
pub fn gold_verdict(p: &GoldPending, now: u32) -> Option<bool> {
    if now == p.after() {
        Some(true)
    } else if now == p.before {
        Some(false)
    } else {
        None
    }
}

/// Which step of the chest screen is on the main menu's letter frame.
///
/// The screen is not drawn by us: it is the letter's own two widgets driven
/// from a different place — `_menuDepthState == 0` is the generic three-slot
/// list and `_menuState == 2, _menuDepthState == 2, _mLetterState == 4` is the
/// bag grid. This says which of them the player is looking at *and* that the
/// frame belongs to the chest rather than to 편지, which is the whole gate on
/// every hook in `patch.rs::host_vault_*`.
///
/// [`Screen::Off`] is re-asserted by the thing that draws the 3x3 tile menu
/// (`host_my_id_tile`), so "the tile grid is on screen" always means "not in
/// the chest" and no exit path can be forgotten.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    /// The letter frame is 편지's, or nothing is on it.
    Off,
    /// The three save slots. A bag belongs to a slot, so one is picked before
    /// there is anything to move.
    Slots,
    /// The item grid, showing one of the two [`Panel`]s.
    Grid,
}

/// Which side of the transfer the grid is showing. One grid, two lists — the
/// 「싱글창고 / 계정창고」 the feature was asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panel {
    /// The loaded slot's own bag: the guest's live `_storage`.
    Bag,
    /// The chest, materialised into a storage image of its own.
    Chest,
}

impl Panel {
    /// The other one. The title band is two words and touching either picks it,
    /// so this is also what a touch on the inactive half means.
    pub fn other(self) -> Self {
        match self {
            Panel::Bag => Panel::Chest,
            Panel::Chest => Panel::Bag,
        }
    }
}

/// The parsed file.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Store {
    pub items: Vec<Held>,
    pub pending: Option<Pending>,
    /// Gold held by the chest. `gold N` in the file, absent when zero, so a
    /// chest that never held gold reads and writes exactly as it did before
    /// gold existed.
    pub gold: u32,
    /// The gold move half-done, if any. Never set while `pending` is.
    pub gold_pending: Option<GoldPending>,
    /// The next chest id to issue. Monotonic across the file's whole life;
    /// never recomputed from `items`, so a number is not reused after the item
    /// that had it leaves.
    pub next_id: u64,
}

enum State {
    Ready(Store),
    /// The file is there and did not parse. Why, for the log and the screen.
    Broken(String),
}

pub struct Vault {
    state: State,
    path: PathBuf,
}

/// How many items fit. One page of the guest's bag panel is 4x5, so the
/// default needs no paging UI at all; the knob is there to grow it once one
/// exists.
pub fn slots() -> usize {
    crate::patch::knob("CS_VAULT_SLOTS", 20).clamp(1, 200) as usize
}

impl Vault {
    /// Read the chest, or start an empty one if there is no file yet.
    ///
    /// The distinction that matters is the third case: a file that is there and
    /// unreadable is neither of those. See the module docs.
    pub fn open(storage: &Path) -> Self {
        let path = storage.join(FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self { state: State::Ready(Store::default()), path };
            }
            Err(e) => {
                let why = format!("cannot read {}: {e}", path.display());
                log::error!("vault: {why}");
                return Self { state: State::Broken(why), path };
            }
        };
        match parse(&text) {
            Ok(store) => {
                log::info!(
                    "vault: {} item(s){}",
                    store.items.len(),
                    match &store.pending {
                        Some(p) =>
                            format!(", and a {} for slot {} left in flight", p.op.tag(), p.slot),
                        None => String::new(),
                    }
                );
                Self { state: State::Ready(store), path }
            }
            Err(why) => {
                // **Not** an empty chest. Nothing is written from here on, so
                // the file stays exactly as it is and can be repaired by hand.
                log::error!(
                    "vault: {} does not parse ({why}); refusing to touch it",
                    path.display()
                );
                Self { state: State::Broken(why), path }
            }
        }
    }

    /// Why the chest is out of order, if it is. The screen shows this instead
    /// of a list.
    pub fn broken(&self) -> Option<&str> {
        match &self.state {
            State::Broken(why) => Some(why),
            State::Ready(_) => None,
        }
    }

    fn store(&self) -> Option<&Store> {
        match &self.state {
            State::Ready(s) => Some(s),
            State::Broken(_) => None,
        }
    }

    pub fn items(&self) -> &[Held] {
        match &self.state {
            State::Ready(s) => &s.items,
            State::Broken(_) => &[],
        }
    }

    pub fn pending(&self) -> Option<&Pending> {
        self.store().and_then(|s| s.pending.as_ref())
    }

    /// The gold move in flight, if any.
    pub fn gold_pending(&self) -> Option<GoldPending> {
        self.store().and_then(|s| s.gold_pending)
    }

    /// Is any move — item or gold — half-done? Nothing new is armed while one
    /// is: the file has one in-flight slot's worth of certainty, not two.
    pub fn in_flight(&self) -> bool {
        self.store().is_some_and(|s| s.pending.is_some() || s.gold_pending.is_some())
    }

    /// Gold in the chest. Zero for a broken chest, which shows nothing —
    /// callers refuse every move on `broken()` first.
    pub fn gold(&self) -> u32 {
        self.store().map_or(0, |s| s.gold)
    }

    /// Is there room for one more? A deposit that would overflow is refused
    /// before step 1, so the bag keeps the item.
    pub fn has_room(&self) -> bool {
        self.store().is_some_and(|s| s.items.len() < slots())
    }

    /// The highest stamp anywhere in the chest, so a withdraw can pick one
    /// above everything it might meet. `None` when the chest is empty.
    pub fn max_stamp(&self) -> Option<i64> {
        self.items().iter().map(|h| stamp_of(&h.rec)).max()
    }

    /// Step 1 of a deposit: record what is about to leave the bag.
    ///
    /// The list is **not** touched — the item is still in the bag at this
    /// point, and writing it into the list here is exactly the duplicate this
    /// protocol exists to prevent.
    pub fn arm_deposit(&mut self, slot: u8, rec: Record) -> Result<u64, String> {
        let s = self.ready_mut()?;
        if s.pending.is_some() || s.gold_pending.is_some() {
            return Err("a move is already in flight".into());
        }
        if s.items.len() >= slots() {
            return Err("the chest is full".into());
        }
        let id = s.next_id;
        s.next_id += 1;
        s.pending = Some(Pending { op: Op::Deposit, slot, id, rec });
        self.write()?;
        Ok(id)
    }

    /// Step 1 of a withdraw: record what is about to appear in the bag, and
    /// **fix its stamp now**.
    ///
    /// The item does not exist yet, so nothing else can tell a resume which bag
    /// entry belongs to this move. `stamp` is chosen by the caller to be above
    /// everything already in the destination bag and in the chest (and above
    /// the guest's own last-issued value), then written into the record; step 2
    /// creates the item and puts that same stamp on it, the way
    /// `Item_loadStorage` does after `ITEM_CreateItem`.
    ///
    /// Re-stamping rather than keeping the chest copy's stamp is deliberate.
    /// The old one was minted in some other run and may already be on an item
    /// in the destination bag — the equipment-restore bug in CLAUDE.md is what
    /// that collision looks like from the player's side.
    pub fn arm_withdraw(&mut self, slot: u8, id: u64, stamp: i64) -> Result<Record, String> {
        let s = self.ready_mut()?;
        if s.pending.is_some() || s.gold_pending.is_some() {
            return Err("a move is already in flight".into());
        }
        let Some(held) = s.items.iter().find(|h| h.id == id) else {
            return Err(format!("no item {id} in the chest"));
        };
        let mut rec = held.rec;
        set_stamp(&mut rec, stamp);
        s.pending = Some(Pending { op: Op::Withdraw, slot, id, rec });
        self.write()?;
        Ok(rec)
    }

    /// Step 3, or the resume that stands in for it: apply [`verdict`] and clear
    /// the in-flight slot.
    ///
    /// `in_bag` is the answer to "does slot [`Pending::slot`] hold an item
    /// stamped [`Pending::stamp`]". On the happy path the caller has just made
    /// that true (or false) itself and passes what it did; after a crash it
    /// loads the slot and looks.
    pub fn settle(&mut self, in_bag: bool) -> Result<Settled, String> {
        let s = self.ready_mut()?;
        let Some(p) = s.pending.take() else {
            return Err("nothing is in flight".into());
        };
        let commit = verdict(p.op, in_bag);
        let what = if commit {
            match p.op {
                Op::Deposit => s.items.push(Held { id: p.id, rec: p.rec }),
                Op::Withdraw => s.items.retain(|h| h.id != p.id),
            }
            Settled::Committed
        } else {
            Settled::RolledBack
        };
        self.write()?;
        log::info!(
            "vault: {} for slot {} {} (the bag {} it)",
            p.op.tag(),
            p.slot,
            match what {
                Settled::Committed => "committed",
                Settled::RolledBack => "rolled back",
            },
            if in_bag { "has" } else { "does not have" }
        );
        Ok(what)
    }

    /// Step 1 of a gold move: record the amount and the hero's gold as it
    /// stands, so a resume has something to compare against.
    ///
    /// Everything that can refuse does so here, before the file changes: a
    /// zero amount (the two answers of [`gold_verdict`] would coincide), a
    /// deposit the chest cannot hold within [`GOLD_CAP`], a withdraw the chest
    /// does not have, and the hero's side of each — not enough to deposit, no
    /// room to receive — which the caller reads off the hero and passes in as
    /// `before`.
    pub fn arm_gold(&mut self, op: Op, slot: u8, amount: u32, before: u32) -> Result<(), String> {
        let s = self.ready_mut()?;
        if s.pending.is_some() || s.gold_pending.is_some() {
            return Err("a move is already in flight".into());
        }
        if amount == 0 {
            return Err("nothing to move".into());
        }
        match op {
            Op::Deposit => {
                if u64::from(s.gold) + u64::from(amount) > u64::from(GOLD_CAP) {
                    return Err("the chest cannot hold that much gold".into());
                }
                if before < amount {
                    return Err("the hero does not have that much".into());
                }
            }
            Op::Withdraw => {
                if s.gold < amount {
                    return Err("the chest does not have that much".into());
                }
                if u64::from(before) + u64::from(amount) > u64::from(GOLD_CAP) {
                    return Err("the hero cannot hold that much gold".into());
                }
            }
        }
        s.gold_pending = Some(GoldPending { op, slot, amount, before });
        self.write()
    }

    /// Step 3 of a gold move, or the resume that stands in for it.
    ///
    /// `now` is that slot's hero gold as it is (or as the caller has just made
    /// it). [`gold_verdict`] decides; an answer of *cannot tell* is treated as
    /// roll back and logged as an error — the chest never mints gold on a
    /// guess.
    pub fn settle_gold(&mut self, now: u32) -> Result<Settled, String> {
        let Some(p) = self.gold_pending() else {
            return Err("no gold is in flight".into());
        };
        let verdict = gold_verdict(&p, now);
        if verdict.is_none() {
            log::error!(
                "vault: gold {} of {} for slot {} cannot be settled: the hero holds {now}, \
                 not {} or {}; rolled back rather than guess",
                p.op.tag(),
                p.amount,
                p.slot,
                p.before,
                p.after()
            );
        }
        self.settle_gold_with(verdict.unwrap_or(false))
    }

    /// Step 3 with the answer supplied, for a slot that no longer exists: the
    /// caller has decided from the slot list, not from the hero's gold.
    pub fn settle_gold_as(&mut self, commit: bool) -> Result<Settled, String> {
        if self.gold_pending().is_none() {
            return Err("no gold is in flight".into());
        }
        self.settle_gold_with(commit)
    }

    fn settle_gold_with(&mut self, commit: bool) -> Result<Settled, String> {
        let s = self.ready_mut()?;
        let Some(p) = s.gold_pending.take() else {
            return Err("no gold is in flight".into());
        };
        let what = if commit {
            s.gold = match p.op {
                Op::Deposit => s.gold.saturating_add(p.amount).min(GOLD_CAP),
                Op::Withdraw => s.gold.saturating_sub(p.amount),
            };
            Settled::Committed
        } else {
            Settled::RolledBack
        };
        self.write()?;
        log::info!(
            "vault: gold {} of {} for slot {} {}",
            p.op.tag(),
            p.amount,
            p.slot,
            if commit { "committed" } else { "rolled back" }
        );
        Ok(what)
    }

    fn ready_mut(&mut self) -> Result<&mut Store, String> {
        match &mut self.state {
            State::Ready(s) => Ok(s),
            State::Broken(why) => Err(why.clone()),
        }
    }

    /// Replace the file atomically: write a sibling, then `rename` over the
    /// original. `std::fs::rename` replaces on both Windows and Android, so a
    /// reader (or a crash) sees either the old file whole or the new one whole
    /// — never a half-written chest, which the protocol above depends on and
    /// which a plain `fs::write` cannot promise.
    fn write(&mut self) -> Result<(), String> {
        let Some(store) = self.store() else {
            return Err("the chest is out of order".into());
        };
        let text = render(store);
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("cannot replace {}: {e}", self.path.display()))
    }
}

fn render(s: &Store) -> String {
    let mut out = String::from("cs_vault 1\n");
    out.push_str(&format!("next {}\n", s.next_id));
    for h in &s.items {
        out.push_str(&format!("item {} {}\n", h.id, hex(&h.rec)));
    }
    if let Some(p) = &s.pending {
        out.push_str(&format!("pending {} {} {} {}\n", p.op.tag(), p.slot, p.id, hex(&p.rec)));
    }
    // Only when there is something to say: a chest that never held gold keeps
    // writing a file an older build can still read.
    if s.gold != 0 {
        out.push_str(&format!("gold {}\n", s.gold));
    }
    if let Some(p) = &s.gold_pending {
        out.push_str(&format!(
            "pending_gold {} {} {} {}\n",
            p.op.tag(),
            p.slot,
            p.amount,
            p.before
        ));
    }
    out
}

fn hex(r: &Record) -> String {
    r.iter().map(|b| format!("{b:02x}")).collect()
}

/// Byte-wise. Slicing the `&str` in twos panics when a multi-byte character
/// straddles a pair — and this runs in `Vm::new`, so a file with one would
/// close the app at every start. `from_str_radix` would also take a `+`.
fn unhex(s: &str) -> Option<Record> {
    let b = s.as_bytes();
    if b.len() != REC * 2 {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut r = [0u8; REC];
    for (out, pair) in r.iter_mut().zip(b.chunks_exact(2)) {
        *out = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(r)
}

/// Whether `bytes` is a chest file [`Vault::open`] would read as one — the
/// question a 세이브 불러오기 asks of the chest it carries before installing
/// it, since a chest the next start finds [`State::Broken`] is locked until
/// someone repairs it by hand.
pub fn check(bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "not text".to_string())?;
    parse(text).map(|_| ())
}

/// Line-oriented on purpose: a chest is a list, and a list in the hand-rolled
/// JSON this crate uses elsewhere would need a real parser. Every line must be
/// understood — an unknown one is an error, not something to skip, because
/// skipping is how a newer file silently loses items to an older build.
fn parse(text: &str) -> Result<Store, String> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    match lines.next() {
        Some("cs_vault 1") => {}
        Some(other) => return Err(format!("unknown header {other:?}")),
        None => return Err("empty file".into()),
    }
    let mut s = Store::default();
    let mut seen_next = false;
    let mut seen_gold = false;
    for line in lines {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            ["next", n] => {
                s.next_id = n.parse().map_err(|_| format!("bad next {n:?}"))?;
                seen_next = true;
            }
            ["item", id, r] => {
                let id: u64 = id.parse().map_err(|_| format!("bad item id {id:?}"))?;
                let rec = unhex(r).ok_or_else(|| format!("bad item record {r:?}"))?;
                s.items.push(Held { id, rec });
            }
            ["gold", n] => {
                if seen_gold {
                    return Err("two gold lines".into());
                }
                seen_gold = true;
                let g: u32 = n.parse().map_err(|_| format!("bad gold {n:?}"))?;
                if g > GOLD_CAP {
                    return Err(format!("gold {g} is above the cap"));
                }
                s.gold = g;
            }
            ["pending_gold", op, slot, amount, before] => {
                if s.gold_pending.is_some() {
                    return Err("two pending_gold lines".into());
                }
                s.gold_pending = Some(GoldPending {
                    op: Op::parse(op).ok_or_else(|| format!("bad op {op:?}"))?,
                    slot: slot.parse().map_err(|_| format!("bad slot {slot:?}"))?,
                    amount: amount.parse().map_err(|_| format!("bad amount {amount:?}"))?,
                    before: before.parse().map_err(|_| format!("bad before {before:?}"))?,
                });
            }
            ["pending", op, slot, id, r] => {
                if s.pending.is_some() {
                    return Err("two pending lines".into());
                }
                s.pending = Some(Pending {
                    op: Op::parse(op).ok_or_else(|| format!("bad op {op:?}"))?,
                    slot: slot.parse().map_err(|_| format!("bad slot {slot:?}"))?,
                    id: id.parse().map_err(|_| format!("bad pending id {id:?}"))?,
                    rec: unhex(r).ok_or_else(|| format!("bad pending record {r:?}"))?,
                });
            }
            _ => return Err(format!("unknown line {line:?}")),
        }
    }
    if !seen_next {
        return Err("no next line".into());
    }
    // Ids are the chest's index; two of anything means the file is not one this
    // build wrote, and a withdraw would then be ambiguous.
    let mut ids: Vec<u64> = s.items.iter().map(|h| h.id).collect();
    ids.sort_unstable();
    let n = ids.len();
    ids.dedup();
    if ids.len() != n {
        return Err("duplicate item ids".into());
    }
    if let Some(max) = ids.last() {
        if *max >= s.next_id {
            return Err(format!("item id {max} is not below next {}", s.next_id));
        }
    }
    if let Some(p) = &s.pending {
        // A deposit's id is freshly issued and so must not be listed; a
        // withdraw's must be, because step 3 removes it.
        let listed = s.items.iter().any(|h| h.id == p.id);
        match (p.op, listed) {
            (Op::Deposit, true) => return Err("a deposit's id is already listed".into()),
            (Op::Withdraw, false) => return Err("a withdraw's id is not listed".into()),
            _ => {}
        }
        if p.id >= s.next_id {
            return Err(format!("pending id {} is not below next {}", p.id, s.next_id));
        }
    }
    if let Some(g) = &s.gold_pending {
        if s.pending.is_some() {
            return Err("an item and a gold move both in flight".into());
        }
        if g.amount == 0 {
            return Err("a gold move of nothing".into());
        }
        // The same refusals `arm_gold` makes, so a file cannot describe a move
        // the chest would never have started.
        let (gold, amount, before) = (u64::from(s.gold), u64::from(g.amount), u64::from(g.before));
        let fits = match g.op {
            Op::Deposit => before >= amount && gold + amount <= u64::from(GOLD_CAP),
            Op::Withdraw => gold >= amount && before + amount <= u64::from(GOLD_CAP),
        };
        if !fits {
            return Err("a gold move the chest could not have armed".into());
        }
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec_with(stamp: i64, kind: u8) -> Record {
        let mut r = [0u8; REC];
        r[rec::KIND] = kind;
        r[rec::IDX] = 3;
        r[rec::COUNT] = 1;
        set_stamp(&mut r, stamp);
        r
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cs_vault_test_{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_stamp_survives_the_round_trip_little_endian() {
        // `UM_Int64ToByte` is little-endian, which is the order
        // `Item_saveStorage` puts on disk and `Item_loadStorage` reads back.
        let mut r = [0u8; REC];
        set_stamp(&mut r, 0x0102_0304_0506_0708);
        assert_eq!(&r[rec::STAMP..rec::STAMP + 8], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(stamp_of(&r), 0x0102_0304_0506_0708);
        set_stamp(&mut r, -1);
        assert_eq!(stamp_of(&r), -1);
    }

    /// The protocol's whole table, with no VM in sight.
    #[test]
    fn the_four_ways_a_transfer_can_die() {
        // step 2 never landed -> the bag still has it -> nothing was deposited
        assert!(!verdict(Op::Deposit, true));
        // step 2 landed -> the bag lost it -> it must land in the chest
        assert!(verdict(Op::Deposit, false));
        // step 2 landed -> the bag has it -> the chest copy is the duplicate
        assert!(verdict(Op::Withdraw, true));
        // step 2 never landed -> the bag has nothing -> the chest keeps it
        assert!(!verdict(Op::Withdraw, false));
    }

    #[test]
    fn a_deposit_that_lands_moves_the_item_exactly_once() {
        let d = tmp("deposit");
        let mut v = Vault::open(&d);
        assert!(v.items().is_empty());
        let id = v.arm_deposit(2, rec_with(0x1111, 7)).unwrap();

        // Re-opened mid-flight: the list is still empty and the move is on record.
        let v2 = Vault::open(&d);
        assert!(v2.items().is_empty());
        let p = v2.pending().unwrap();
        assert_eq!((p.op, p.slot, p.id, p.stamp()), (Op::Deposit, 2, id, 0x1111));

        assert_eq!(v.settle(false).unwrap(), Settled::Committed);
        let v3 = Vault::open(&d);
        assert_eq!(v3.items().len(), 1);
        assert_eq!(v3.items()[0].id, id);
        assert!(v3.pending().is_none());
    }

    #[test]
    fn a_deposit_that_did_not_land_leaves_the_chest_alone() {
        let d = tmp("deposit_rollback");
        let mut v = Vault::open(&d);
        v.arm_deposit(1, rec_with(0x2222, 7)).unwrap();
        // The bag still has it, so step 2 never ran.
        assert_eq!(v.settle(true).unwrap(), Settled::RolledBack);
        assert!(Vault::open(&d).items().is_empty());
    }

    #[test]
    fn a_withdraw_that_did_not_land_keeps_the_item_listed() {
        let d = tmp("withdraw_rollback");
        let mut v = Vault::open(&d);
        let id = v.arm_deposit(1, rec_with(0x3333, 7)).unwrap();
        v.settle(false).unwrap();

        let rec = v.arm_withdraw(1, id, 0x4444).unwrap();
        assert_eq!(stamp_of(&rec), 0x4444, "the new stamp is what the bag will carry");
        // Mid-flight the item is still listed — that is what makes a rollback
        // free.
        assert_eq!(Vault::open(&d).items().len(), 1);
        assert_eq!(v.settle(false).unwrap(), Settled::RolledBack);
        let after = Vault::open(&d);
        assert_eq!(after.items().len(), 1);
        assert_eq!(after.items()[0].id, id);
        // Rolled back, so the chest copy keeps the stamp it had.
        assert_eq!(stamp_of(&after.items()[0].rec), 0x3333);
    }

    #[test]
    fn a_withdraw_that_landed_removes_it() {
        let d = tmp("withdraw_commit");
        let mut v = Vault::open(&d);
        let id = v.arm_deposit(1, rec_with(0x5555, 7)).unwrap();
        v.settle(false).unwrap();
        v.arm_withdraw(3, id, 0x6666).unwrap();
        assert_eq!(v.settle(true).unwrap(), Settled::Committed);
        assert!(Vault::open(&d).items().is_empty());
    }

    #[test]
    fn ids_are_never_reused() {
        let d = tmp("ids");
        let mut v = Vault::open(&d);
        let a = v.arm_deposit(1, rec_with(1, 7)).unwrap();
        v.settle(false).unwrap();
        v.arm_withdraw(1, a, 2).unwrap();
        v.settle(true).unwrap();
        let b = v.arm_deposit(1, rec_with(3, 7)).unwrap();
        assert_ne!(a, b, "the emptied chest must not hand the number back");
        assert!(b > a);
    }

    #[test]
    fn only_one_move_is_ever_in_flight() {
        let d = tmp("one_flight");
        let mut v = Vault::open(&d);
        v.arm_deposit(1, rec_with(1, 7)).unwrap();
        assert!(v.arm_deposit(1, rec_with(2, 7)).is_err());
        assert!(v.arm_withdraw(1, 0, 3).is_err());
    }

    /// The letter server's bug, in this file's shape: a chest that does not
    /// parse must not read as an empty one, because the next write would then
    /// make it so.
    #[test]
    fn an_unreadable_chest_refuses_every_write() {
        let d = tmp("broken");
        std::fs::write(d.join(FILE), "cs_vault 1\nnext 2\nitem 1 nonsense\n").unwrap();
        let before = std::fs::read(d.join(FILE)).unwrap();

        let mut v = Vault::open(&d);
        assert!(v.broken().is_some());
        assert!(v.items().is_empty(), "shows nothing, but that is not the same as holding nothing");
        assert!(v.arm_deposit(1, rec_with(1, 7)).is_err());
        assert!(v.arm_withdraw(1, 1, 2).is_err());
        assert!(v.settle(true).is_err());

        assert_eq!(std::fs::read(d.join(FILE)).unwrap(), before, "the file was not touched");
    }

    #[test]
    fn a_file_from_a_newer_build_is_broken_not_half_read() {
        let d = tmp("unknown_line");
        std::fs::write(
            d.join(FILE),
            format!("cs_vault 1\nnext 2\nitem 1 {}\nlocker 4 something\n", hex(&rec_with(9, 7))),
        )
        .unwrap();
        assert!(Vault::open(&d).broken().is_some());
    }

    #[test]
    fn the_file_round_trips() {
        let d = tmp("round_trip");
        let mut v = Vault::open(&d);
        let a = v.arm_deposit(1, rec_with(0x7777, 9)).unwrap();
        v.settle(false).unwrap();
        let b = v.arm_deposit(2, rec_with(0x8888, 0x0a)).unwrap();
        v.settle(false).unwrap();
        v.arm_withdraw(3, a, 0x9999).unwrap();

        let re = Vault::open(&d);
        assert!(re.broken().is_none());
        assert_eq!(re.items().len(), 2);
        assert_eq!(re.items()[0].id, a);
        assert_eq!(re.items()[1].id, b);
        assert_eq!(stamp_of(&re.items()[1].rec), 0x8888);
        let p = re.pending().unwrap();
        assert_eq!((p.op, p.slot, p.id, p.stamp()), (Op::Withdraw, 3, a, 0x9999));
    }

    #[test]
    fn a_full_chest_refuses_the_deposit_before_the_bag_loses_it() {
        let d = tmp("full");
        let mut v = Vault::open(&d);
        for i in 0..slots() {
            v.arm_deposit(1, rec_with(i as i64, 7)).unwrap();
            v.settle(false).unwrap();
        }
        assert!(!v.has_room());
        assert!(v.arm_deposit(1, rec_with(999, 7)).is_err());
        // Nothing in flight, so the screen can say "full" and the bag is intact.
        assert!(v.pending().is_none());
    }

    #[test]
    fn gold_answers_the_four_cells_and_refuses_to_guess() {
        let p = GoldPending { op: Op::Deposit, slot: 1, amount: 300, before: 1000 };
        assert_eq!(gold_verdict(&p, 1000), Some(false), "step 2 never ran");
        assert_eq!(gold_verdict(&p, 700), Some(true), "step 2 landed");
        assert_eq!(gold_verdict(&p, 850), None, "the save moved on: no answer");
        let w = GoldPending { op: Op::Withdraw, slot: 2, amount: 300, before: 1000 };
        assert_eq!(gold_verdict(&w, 1300), Some(true));
        assert_eq!(gold_verdict(&w, 1000), Some(false));
        assert_eq!(gold_verdict(&w, 0), None);
    }

    #[test]
    fn a_gold_deposit_moves_the_amount_exactly_once() {
        let d = tmp("gold_deposit");
        let mut v = Vault::open(&d);
        v.arm_gold(Op::Deposit, 1, 300, 1000).unwrap();

        // Re-opened mid-flight: nothing credited yet, and the move on record.
        let re = Vault::open(&d);
        assert_eq!(re.gold(), 0);
        assert_eq!(
            re.gold_pending(),
            Some(GoldPending { op: Op::Deposit, slot: 1, amount: 300, before: 1000 })
        );
        assert!(re.in_flight());

        assert_eq!(v.settle_gold(700).unwrap(), Settled::Committed);
        let re = Vault::open(&d);
        assert_eq!(re.gold(), 300);
        assert!(!re.in_flight());

        // And out again, into a different slot.
        v.arm_gold(Op::Withdraw, 3, 200, 50).unwrap();
        assert_eq!(v.settle_gold(250).unwrap(), Settled::Committed);
        assert_eq!(Vault::open(&d).gold(), 100);
    }

    #[test]
    fn gold_that_did_not_land_stays_where_it_was() {
        let d = tmp("gold_rollback");
        let mut v = Vault::open(&d);
        v.arm_gold(Op::Deposit, 1, 300, 1000).unwrap();
        // The hero still holds the lot, so step 2 never ran.
        assert_eq!(v.settle_gold(1000).unwrap(), Settled::RolledBack);
        assert_eq!(v.gold(), 0);

        v.arm_gold(Op::Deposit, 1, 300, 1000).unwrap();
        v.settle_gold(700).unwrap();
        v.arm_gold(Op::Withdraw, 1, 100, 700).unwrap();
        assert_eq!(v.settle_gold(700).unwrap(), Settled::RolledBack);
        assert_eq!(v.gold(), 300, "the chest keeps what never left it");
    }

    /// The one cell gold has that items do not: the hero holds neither the
    /// old amount nor the new one. The chest must not mint on that.
    #[test]
    fn gold_the_protocol_cannot_place_is_never_minted() {
        let d = tmp("gold_unsure");
        let mut v = Vault::open(&d);
        v.arm_gold(Op::Deposit, 1, 300, 1000).unwrap();
        assert_eq!(v.settle_gold(999).unwrap(), Settled::RolledBack);
        assert_eq!(v.gold(), 0);
        assert!(!v.in_flight(), "and it is not left in flight either");

        // A supplied answer, for a slot the slot list says is gone.
        v.arm_gold(Op::Deposit, 1, 300, 1000).unwrap();
        assert_eq!(v.settle_gold_as(true).unwrap(), Settled::Committed);
        assert_eq!(v.gold(), 300);
        assert!(v.settle_gold_as(true).is_err(), "nothing in flight");
    }

    #[test]
    fn gold_refuses_before_step_one() {
        let d = tmp("gold_refuse");
        let mut v = Vault::open(&d);
        assert!(v.arm_gold(Op::Deposit, 1, 0, 1000).is_err(), "nothing to move");
        assert!(v.arm_gold(Op::Deposit, 1, 1001, 1000).is_err(), "more than the hero has");
        assert!(v.arm_gold(Op::Withdraw, 1, 1, 0).is_err(), "more than the chest has");
        assert!(v.arm_gold(Op::Deposit, 1, GOLD_CAP, GOLD_CAP).is_ok(), "exactly the cap fits");
        v.settle_gold(0).unwrap();
        assert!(v.arm_gold(Op::Deposit, 1, 1, 1).is_err(), "the chest over the cap");
        assert!(v.arm_gold(Op::Withdraw, 1, 1, GOLD_CAP).is_err(), "the hero over the cap");
        assert!(!v.in_flight(), "a refusal leaves nothing in flight");
        assert_eq!(
            std::fs::read_to_string(d.join(FILE)).unwrap(),
            format!("cs_vault 1\nnext 0\ngold {GOLD_CAP}\n")
        );
    }

    #[test]
    fn one_move_of_either_kind_is_in_flight_at_a_time() {
        let d = tmp("gold_one_flight");
        let mut v = Vault::open(&d);
        v.arm_gold(Op::Deposit, 1, 10, 10).unwrap();
        assert!(v.arm_deposit(1, rec_with(1, 7)).is_err());
        assert!(v.arm_gold(Op::Deposit, 1, 10, 10).is_err());
        v.settle_gold(0).unwrap();
        v.arm_deposit(1, rec_with(1, 7)).unwrap();
        assert!(v.arm_gold(Op::Withdraw, 1, 5, 0).is_err());
        assert!(v.settle_gold(0).is_err(), "no gold is in flight");
        assert!(v.settle_gold_as(true).is_err());
    }

    /// A chest that never held gold keeps writing the file the previous build
    /// wrote, and reads that build's files as holding none.
    #[test]
    fn a_chest_without_gold_writes_the_file_it_always_did() {
        let d = tmp("gold_compat");
        let mut v = Vault::open(&d);
        v.arm_deposit(1, rec_with(1, 7)).unwrap();
        v.settle(false).unwrap();
        let text = std::fs::read_to_string(d.join(FILE)).unwrap();
        assert!(!text.contains("gold"), "{text}");
        assert_eq!(Vault::open(&d).gold(), 0);

        v.arm_gold(Op::Deposit, 2, 40, 100).unwrap();
        let re = Vault::open(&d);
        assert_eq!(
            re.gold_pending().map(|g| (g.op, g.slot, g.amount, g.before)),
            Some((Op::Deposit, 2, 40, 100))
        );
        assert_eq!(re.items().len(), 1, "the item is still there beside it");
        v.settle_gold(60).unwrap();
        let text = std::fs::read_to_string(d.join(FILE)).unwrap();
        assert!(text.contains("\ngold 40\n"), "{text}");
        assert!(!text.contains("pending_gold"), "{text}");
        assert_eq!(Vault::open(&d).gold(), 40);
    }

    /// A record is hex, but the file is whatever reached the disk — 세이브
    /// 불러오기 brings it from another phone. One whose 72 bytes are not 72
    /// ASCII hex digits reads as broken, never as a panic: `Vault::open` runs
    /// in `Vm::new`, so a panic there closed the app at every start.
    #[test]
    fn a_record_that_is_not_ascii_hex_is_broken_not_a_panic() {
        let d = tmp("not_ascii_hex");
        let r = hex(&rec_with(1, 7));
        // A record's byte length, with a two- and a three-byte character
        // straddling the pairs a `&str` slice cuts, and the `+` that
        // `from_str_radix` takes.
        let two = format!("a\u{e9}{}", &r[3..]);
        let three = format!("a\u{ac00}{}", &r[4..]);
        let plus = format!("+f{}", &r[2..]);
        for bad in [&two, &three, &plus] {
            assert_eq!(bad.len(), REC * 2);
            assert_eq!(unhex(bad), None, "{bad:?}");
            for text in [
                format!("cs_vault 1\nnext 2\nitem 1 {bad}\n"),
                format!("cs_vault 1\nnext 2\nitem 1 {r}\npending withdraw 1 1 {bad}\n"),
            ] {
                assert!(check(text.as_bytes()).is_err(), "{text:?}");
                std::fs::write(d.join(FILE), &text).unwrap();
                assert!(Vault::open(&d).broken().is_some(), "{text:?}");
            }
        }
        assert_eq!(unhex(&r), Some(rec_with(1, 7)));
        assert_eq!(unhex(&r.to_uppercase()), Some(rec_with(1, 7)), "the case was never part of the format");
        assert!(check(b"\xffcs_vault 1\nnext 1\n").is_err(), "not text");
        assert!(check(b"cs_vault 1\nnext 1\n").is_ok());
    }

    #[test]
    fn contradictory_gold_lines_are_broken() {
        let d = tmp("gold_contradictions");
        let r = hex(&rec_with(1, 7));
        for bad in [
            // two gold lines
            "cs_vault 1\nnext 1\ngold 5\ngold 6\n".to_string(),
            // an item and a gold move both in flight
            format!(
                "cs_vault 1\nnext 2\nitem 1 {r}\npending withdraw 1 1 {r}\npending_gold deposit 1 5 5\n"
            ),
            // a move of nothing
            "cs_vault 1\nnext 1\npending_gold deposit 1 0 5\n".to_string(),
            // more than the hero had
            "cs_vault 1\nnext 1\npending_gold deposit 1 6 5\n".to_string(),
            // more than the chest has
            "cs_vault 1\nnext 1\ngold 3\npending_gold withdraw 1 4 0\n".to_string(),
            // over the cap
            format!("cs_vault 1\nnext 1\ngold {}\n", u64::from(GOLD_CAP) + 1),
            // not a direction
            "cs_vault 1\nnext 1\npending_gold sideways 1 1 1\n".to_string(),
        ] {
            std::fs::write(d.join(FILE), &bad).unwrap();
            assert!(Vault::open(&d).broken().is_some(), "should have refused {bad:?}");
        }
    }

    #[test]
    fn contradictory_files_are_broken() {
        let d = tmp("contradictions");
        let r = hex(&rec_with(1, 7));
        for bad in [
            // a deposit's id must not already be listed
            format!("cs_vault 1\nnext 2\nitem 1 {r}\npending deposit 1 1 {r}\n"),
            // a withdraw's id must be listed
            format!("cs_vault 1\nnext 3\nitem 1 {r}\npending withdraw 1 2 {r}\n"),
            // ids must be below next
            format!("cs_vault 1\nnext 1\nitem 1 {r}\n"),
            // no two items share an id
            format!("cs_vault 1\nnext 2\nitem 1 {r}\nitem 1 {r}\n"),
            // next is not optional
            format!("cs_vault 1\nitem 1 {r}\n"),
        ] {
            std::fs::write(d.join(FILE), &bad).unwrap();
            assert!(Vault::open(&d).broken().is_some(), "should have refused {bad:?}");
        }
    }
}
