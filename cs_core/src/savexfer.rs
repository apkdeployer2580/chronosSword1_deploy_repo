//! The three rows at the bottom of 환경설정 — 세이브 내보내기, 세이브
//! 불러오기, 세이브 되돌리기 — under the title menu and in the in-game menu.
//! The file and everything that touches the disk is [`crate::savebundle`];
//! this is the conversation.
//!
//! ```text
//!   내보내기   (in game: the game's own save first, unless it forbids saving
//!              here) -> bundle -> the phone's "save as" -> 확인 message
//!   불러오기   question -> the phone's "open" -> checked -> question naming the
//!              file's date and characters -> 예: staged, the game closes; the
//!              next start swaps the files and keeps the old ones in the vault
//!   되돌리기   question naming the newest vault generation -> 예: staged, closes
//! ```
//!
//! # The rows and the modal are the cave's
//!
//! The rows are drawn and keyed by the options feature of the code cave
//! (`cave/src/opt.c`), which offers them only while the cave's `savexfer`
//! feature is in. A press reaches here through an entry hook on the cave's
//! `opt_xfer_action`; the answer to a question through one on
//! `opt_xfer_answer`. The modal itself — a message with 확인, or a question
//! with 예/아니오, over the options list — is the cave's `opt_modal`, raised
//! through `opt_modal_show` with a `Vm::call`, and it consumes every key
//! while it is up, so nothing underneath moves. No guest popup is borrowed:
//! the title menu's `setPopUp` does not draw inside the in-game menu, and the
//! in-game menu's message box takes a string-table index, not text.
//!
//! # Raising one later
//!
//! The phone's file screens answer asynchronously (`DocStatus`, relayed by
//! the shell), so their result is acted on at the start of the next
//! [`crate::Vm::frame`] — between frames, where a `Vm::call` is over a
//! quiescent CPU. The cave refuses a modal when the options screen is no
//! longer showing a row of ours (leaving the screen zeroes the selected row),
//! so a late answer cannot land on another screen.

use std::path::PathBuf;

use crate::savebundle as sb;
use crate::{Phase, Vm};
use arm32_cpu::{reg, Memory, Mode};

/// What the shell is asked to do with the phone's document screens. `id`
/// comes back with the answer ([`Vm::save_doc_done`]): a copy can outlive its
/// screen, so an answer is only ever taken for the request it names, and each
/// request has a file of its own ([`sb::export_file`], [`sb::import_file`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocRequest {
    /// Copy `file` to a document the player names, suggesting `name`.
    Export { id: u32, file: PathBuf, name: String },
    /// Copy a document the player picks to `file`, at most
    /// [`sb::BUNDLE_MAX`] + 1 bytes.
    Import { id: u32, file: PathBuf },
}

impl DocRequest {
    pub fn id(&self) -> u32 {
        match self {
            DocRequest::Export { id, .. } | DocRequest::Import { id, .. } => *id,
        }
    }
}

/// How the shell's document screen ended. The codes are `CsSave.java`'s.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DocStatus {
    Done,
    Cancelled,
    Failed,
}

impl DocStatus {
    pub fn from_code(code: i32) -> Self {
        match code {
            0 => DocStatus::Done,
            1 => DocStatus::Cancelled,
            _ => DocStatus::Failed,
        }
    }
}

/// Which row was pressed. The numbers are the cave's (`opt_xfer_action`'s
/// argument, `kind - ROW_EXPORT`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Export,
    Import,
    Restore,
}

impl Action {
    pub fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Action::Export),
            1 => Some(Action::Import),
            2 => Some(Action::Restore),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Flow {
    #[default]
    Idle,
    /// Waiting for the shell's save-as, request `id`. `fresh` is whether the
    /// game saved just before — false where it forbids saving, so the export
    /// is the last save and the message says so.
    Exporting { id: u32, fresh: bool },
    /// The first question is up.
    AskImport,
    /// Waiting for the shell's open, request `id`.
    Picking { id: u32 },
    /// The question naming the picked file is up.
    AskApply,
    /// The question naming the vault generation is up.
    AskRestore,
}

/// Per-`Vm` state.
pub struct SaveXfer {
    flow: Flow,
    pub(crate) request: Option<DocRequest>,
    result: Option<DocStatus>,
    /// The bundle the last question is about.
    ready: Option<Vec<u8>>,
    /// A guest buffer the modal's text goes through, allocated once.
    text: Option<u32>,
    /// The number the next document request gets. Never reused in a run, so
    /// a late answer cannot be mistaken for a later request's.
    next_id: u32,
    /// When the request in flight was made (wall ms), for [`PATIENCE_MS`].
    since_ms: u64,
    /// The phone's offset from UTC, for the dates in the questions and the
    /// suggested file name. The shell sets it from `TimeZone`; the probe keeps
    /// Korea's.
    pub utc_offset_min: i32,
    pub exports: u64,
    pub picks: u64,
    pub staged: u64,
    pub messages: u64,
    pub questions: u64,
    pub saved_first: u64,
}

impl Default for SaveXfer {
    fn default() -> Self {
        Self {
            flow: Flow::Idle,
            request: None,
            result: None,
            ready: None,
            text: None,
            next_id: 1,
            since_ms: 0,
            utc_offset_min: 9 * 60,
            exports: 0,
            picks: 0,
            staged: 0,
            messages: 0,
            questions: 0,
            saved_first: 0,
        }
    }
}

// ---- cave symbols -----------------------------------------------------------------

/// The cave's entry the host sits on for a press; `r0` is the row.
pub const CAVE_ACTION: &str = "opt_xfer_action";
/// The cave's entry the host sits on for an answer; `r0` is 1 for 예.
pub const CAVE_ANSWER: &str = "opt_xfer_answer";
/// `opt_modal_show(kind, text)`; 0 when the options screen is not up.
const CAVE_SHOW: &str = "opt_modal_show";
const MODAL_MESSAGE: u32 = 1;
const MODAL_QUESTION: u32 = 2;
/// The cave's text buffer is `OPT_MODAL_TEXT` (156) bytes; ours holds the
/// longest built question with room to spare.
const TEXT_MAX: usize = 150;

// ---- strings (EUC-KR; `;` is the modal's line break, at most three lines) -------

/// `불러온 세이브로 바뀝니다.;지금 세이브는 보관됩니다.;파일을 고르시겠습니까?`
const Q_IMPORT: &[u8] = &[
    0xba, 0xd2, 0xb7, 0xaf, 0xbf, 0xc2, 0x20, 0xbc, 0xbc, 0xc0, 0xcc, 0xba, 0xea, 0xb7, 0xce,
    0x20, 0xb9, 0xd9, 0xb2, 0xf2, 0xb4, 0xcf, 0xb4, 0xd9, 0x2e, 0x3b, 0xc1, 0xf6, 0xb1, 0xdd,
    0x20, 0xbc, 0xbc, 0xc0, 0xcc, 0xba, 0xea, 0xb4, 0xc2, 0x20, 0xba, 0xb8, 0xb0, 0xfc, 0xb5,
    0xcb, 0xb4, 0xcf, 0xb4, 0xd9, 0x2e, 0x3b, 0xc6, 0xc4, 0xc0, 0xcf, 0xc0, 0xbb, 0x20, 0xb0,
    0xed, 0xb8, 0xa3, 0xbd, 0xc3, 0xb0, 0xda, 0xbd, 0xc0, 0xb4, 0xcf, 0xb1, 0xee, 0x3f,
];
/// `세이브를 내보냈습니다.`
const M_EXPORTED: &[u8] = &[
    0xbc, 0xbc, 0xc0, 0xcc, 0xba, 0xea, 0xb8, 0xa6, 0x20, 0xb3, 0xbb, 0xba, 0xb8, 0xb3, 0xc2,
    0xbd, 0xc0, 0xb4, 0xcf, 0xb4, 0xd9, 0x2e,
];
/// `저장할 수 없는 상황이라;마지막 저장을 내보냈습니다.`
const M_EXPORTED_LAST: &[u8] = &[
    0xc0, 0xfa, 0xc0, 0xe5, 0xc7, 0xd2, 0x20, 0xbc, 0xf6, 0x20, 0xbe, 0xf8, 0xb4, 0xc2, 0x20,
    0xbb, 0xf3, 0xc8, 0xb2, 0xc0, 0xcc, 0xb6, 0xf3, 0x3b, 0xb8, 0xb6, 0xc1, 0xf6, 0xb8, 0xb7,
    0x20, 0xc0, 0xfa, 0xc0, 0xe5, 0xc0, 0xbb, 0x20, 0xb3, 0xbb, 0xba, 0xb8, 0xb3, 0xc2, 0xbd,
    0xc0, 0xb4, 0xcf, 0xb4, 0xd9, 0x2e,
];
/// `내보내지 못했습니다.`
const M_EXPORT_FAILED: &[u8] = &[
    0xb3, 0xbb, 0xba, 0xb8, 0xb3, 0xbb, 0xc1, 0xf6, 0x20, 0xb8, 0xf8, 0xc7, 0xdf, 0xbd, 0xc0,
    0xb4, 0xcf, 0xb4, 0xd9, 0x2e,
];
/// `내보낼 세이브가 없습니다.`
const M_NO_SAVE: &[u8] = &[
    0xb3, 0xbb, 0xba, 0xb8, 0xb3, 0xbe, 0x20, 0xbc, 0xbc, 0xc0, 0xcc, 0xba, 0xea, 0xb0, 0xa1,
    0x20, 0xbe, 0xf8, 0xbd, 0xc0, 0xb4, 0xcf, 0xb4, 0xd9, 0x2e,
];
/// `세이브 파일이 아닙니다.`
const M_BAD_FILE: &[u8] = &[
    0xbc, 0xbc, 0xc0, 0xcc, 0xba, 0xea, 0x20, 0xc6, 0xc4, 0xc0, 0xcf, 0xc0, 0xcc, 0x20, 0xbe,
    0xc6, 0xb4, 0xd5, 0xb4, 0xcf, 0xb4, 0xd9, 0x2e,
];
/// `캐릭터가 없는 세이브입니다.`
const M_NO_CHARACTER: &[u8] = &[
    0xc4, 0xb3, 0xb8, 0xaf, 0xc5, 0xcd, 0xb0, 0xa1, 0x20, 0xbe, 0xf8, 0xb4, 0xc2, 0x20, 0xbc,
    0xbc, 0xc0, 0xcc, 0xba, 0xea, 0xc0, 0xd4, 0xb4, 0xcf, 0xb4, 0xd9, 0x2e,
];
/// `파일을 읽지 못했습니다.`
const M_READ_FAILED: &[u8] = &[
    0xc6, 0xc4, 0xc0, 0xcf, 0xc0, 0xbb, 0x20, 0xc0, 0xd0, 0xc1, 0xf6, 0x20, 0xb8, 0xf8, 0xc7,
    0xdf, 0xbd, 0xc0, 0xb4, 0xcf, 0xb4, 0xd9, 0x2e,
];
/// `되돌릴 세이브가 없습니다.`
const M_NO_VAULT: &[u8] = &[
    0xb5, 0xc7, 0xb5, 0xb9, 0xb8, 0xb1, 0x20, 0xbc, 0xbc, 0xc0, 0xcc, 0xba, 0xea, 0xb0, 0xa1,
    0x20, 0xbe, 0xf8, 0xbd, 0xc0, 0xb4, 0xcf, 0xb4, 0xd9, 0x2e,
];
/// `적용하지 못했습니다.`
const M_STAGE_FAILED: &[u8] = &[
    0xc0, 0xfb, 0xbf, 0xeb, 0xc7, 0xcf, 0xc1, 0xf6, 0x20, 0xb8, 0xf8, 0xc7, 0xdf, 0xbd, 0xc0,
    0xb4, 0xcf, 0xb4, 0xd9, 0x2e,
];
/// `파일을 옮기는 중입니다.;잠시 후 다시 눌러 주세요.`
const M_BUSY: &[u8] = &[
    0xc6, 0xc4, 0xc0, 0xcf, 0xc0, 0xbb, 0x20, 0xbf, 0xc5, 0xb1, 0xe2, 0xb4, 0xc2, 0x20, 0xc1,
    0xdf, 0xc0, 0xd4, 0xb4, 0xcf, 0xb4, 0xd9, 0x2e, 0x3b, 0xc0, 0xe1, 0xbd, 0xc3, 0x20, 0xc8,
    0xc4, 0x20, 0xb4, 0xd9, 0xbd, 0xc3, 0x20, 0xb4, 0xad, 0xb7, 0xaf, 0x20, 0xc1, 0xd6, 0xbc,
    0xbc, 0xbf, 0xe4, 0x2e,
];
/// ` 캐릭터 ` — the apply question's first line is `<date> 캐릭터 <n>명`.
const APPLY_CHARS: &[u8] = &[0x20, 0xc4, 0xb3, 0xb8, 0xaf, 0xc5, 0xcd, 0x20];
/// `명`
const APPLY_COUNT: &[u8] = &[0xb8, 0xed];
/// `;적용하려면 종료해야 합니다.;지금 종료하시겠습니까?`
const APPLY_TAIL: &[u8] = &[
    0x3b, 0xc0, 0xfb, 0xbf, 0xeb, 0xc7, 0xcf, 0xb7, 0xc1, 0xb8, 0xe9, 0x20, 0xc1, 0xbe, 0xb7,
    0xe1, 0xc7, 0xd8, 0xbe, 0xdf, 0x20, 0xc7, 0xd5, 0xb4, 0xcf, 0xb4, 0xd9, 0x2e, 0x3b, 0xc1,
    0xf6, 0xb1, 0xdd, 0x20, 0xc1, 0xbe, 0xb7, 0xe1, 0xc7, 0xcf, 0xbd, 0xc3, 0xb0, 0xda, 0xbd,
    0xc0, 0xb4, 0xcf, 0xb1, 0xee, 0x3f,
];
/// ` 상태로` — the restore question's first line is `<date> 상태로`.
const RESTORE_SUFFIX: &[u8] = &[0x20, 0xbb, 0xf3, 0xc5, 0xc2, 0xb7, 0xce];
/// `;되돌리려면 종료해야 합니다.;지금 종료하시겠습니까?`
const RESTORE_TAIL: &[u8] = &[
    0x3b, 0xb5, 0xc7, 0xb5, 0xb9, 0xb8, 0xae, 0xb7, 0xc1, 0xb8, 0xe9, 0x20, 0xc1, 0xbe, 0xb7,
    0xe1, 0xc7, 0xd8, 0xbe, 0xdf, 0x20, 0xc7, 0xd5, 0xb4, 0xcf, 0xb4, 0xd9, 0x2e, 0x3b, 0xc1,
    0xf6, 0xb1, 0xdd, 0x20, 0xc1, 0xbe, 0xb7, 0xe1, 0xc7, 0xcf, 0xbd, 0xc3, 0xb0, 0xda, 0xbd,
    0xc0, 0xb4, 0xcf, 0xb1, 0xee, 0x3f,
];

/// The apply question for a bundle exported at `stamp` with `characters`.
fn apply_question(stamp: &str, characters: usize) -> Vec<u8> {
    let mut v = stamp.as_bytes().to_vec();
    v.extend_from_slice(APPLY_CHARS);
    v.push(b'0' + characters.min(9) as u8);
    v.extend_from_slice(APPLY_COUNT);
    v.extend_from_slice(APPLY_TAIL);
    v
}

/// The restore question for a generation taken at `stamp`.
fn restore_question(stamp: &str) -> Vec<u8> {
    let mut v = stamp.as_bytes().to_vec();
    v.extend_from_slice(RESTORE_SUFFIX);
    v.extend_from_slice(RESTORE_TAIL);
    v
}

/// `크로노스소드_세이브_20261028_1430.cssave`: what the save-as screen offers.
fn suggested_name(stamp: &str) -> String {
    let digits: String = stamp.chars().filter(|c| c.is_ascii_digit()).collect();
    let (date, time) = digits.split_at(digits.len().min(8));
    format!("크로노스소드_세이브_{date}_{time}.{}", sb::EXT)
}

// ---- the modal ------------------------------------------------------------------------

/// Put `text` in front of the player through the cave's modal. False when
/// the cave refused — the options screen is not up — or is not there.
fn show(vm: &mut Vm, kind: u32, text: &[u8]) -> Result<bool, String> {
    let Some(f) = vm.symbol(CAVE_SHOW) else {
        log::warn!("savexfer: the cave's modal is not there; dropping a message");
        return Ok(false);
    };
    let buf = match vm.savexfer.text {
        Some(p) => p,
        None => {
            let p = vm.heap.alloc(TEXT_MAX as u32 + 1).ok_or("no guest heap for the modal text")?;
            vm.savexfer.text = Some(p);
            p
        }
    };
    let n = text.len().min(TEXT_MAX);
    vm.mem.write_bytes(buf, &text[..n]);
    vm.mem.w8(buf + n as u32, 0);
    let shown = vm.call(f, &[kind, buf])? != 0;
    if !shown {
        log::info!("savexfer: the options screen is gone; a modal was dropped");
    }
    Ok(shown)
}

fn tell(vm: &mut Vm, msg: &[u8]) -> Result<(), String> {
    if show(vm, MODAL_MESSAGE, msg)? {
        vm.savexfer.messages += 1;
    }
    Ok(())
}

fn ask(vm: &mut Vm, msg: &[u8]) -> Result<bool, String> {
    let up = show(vm, MODAL_QUESTION, msg)?;
    if up {
        vm.savexfer.questions += 1;
    }
    Ok(up)
}

// ---- the hooks ---------------------------------------------------------------------------

impl Vm {
    /// Entry hook on the cave's `opt_xfer_action`: a save row was pressed.
    /// Serves, so the cave's own body — the "not available" message for a
    /// build without this host — never runs.
    pub(crate) fn host_xfer_action(&mut self) -> Result<bool, String> {
        let code = self.cpu.reg_get(Mode::User, 0);
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        if let Some(action) = Action::from_code(code) {
            start(self, action)?;
        } else {
            log::warn!("savexfer: the cave pressed row {code}, which is not one of ours");
        }
        self.cpu.reg_set(Mode::User, 0, 0);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Entry hook on the cave's `opt_xfer_answer`: the question was answered.
    pub(crate) fn host_xfer_answer(&mut self) -> Result<bool, String> {
        let yes = self.cpu.reg_get(Mode::User, 0) != 0;
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        answered(self, yes)?;
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// The shell's next request for a document screen, if any.
    pub fn take_save_doc_request(&mut self) -> Option<DocRequest> {
        self.savexfer.request.take()
    }

    /// How the document screen for request `id` ended. Acted on at the next
    /// frame, so this may be called whenever the shell hears of it.
    pub fn save_doc_done(&mut self, id: u32, status: DocStatus) {
        done(self, id, status);
    }
}

// ---- the rows ---------------------------------------------------------------------------

/// How long a request in flight holds the rows. A screen that closed hands
/// its copy to a thread of its own, and a cloud document may still be
/// downloading when the game is back and the rows can be pressed; a press
/// then is refused, not taken as a new request. The shell answers every
/// request exactly once, so this is only the way out if an answer is lost —
/// and since answers carry their request's number, a request given up on
/// can still answer later without being mistaken for the one after it.
const PATIENCE_MS: u64 = 120_000;

fn wall_ms(vm: &Vm) -> u64 {
    vm.wall_now_ms().unwrap_or(0).max(0) as u64
}

/// A number for a new document request.
fn new_request(vm: &mut Vm) -> u32 {
    let id = vm.savexfer.next_id;
    vm.savexfer.next_id = id.wrapping_add(1).max(1);
    vm.savexfer.since_ms = wall_ms(vm);
    id
}

/// A save row was pressed.
pub fn start(vm: &mut Vm, action: Action) -> Result<(), String> {
    match vm.savexfer.flow {
        Flow::Idle => {}
        Flow::Exporting { id, .. } | Flow::Picking { id } => {
            let age = wall_ms(vm).saturating_sub(vm.savexfer.since_ms);
            if age < PATIENCE_MS {
                log::info!("savexfer: request {id} is still copying ({age} ms); a press waits for it");
                return tell(vm, M_BUSY);
            }
            log::warn!("savexfer: request {id} was never answered in {age} ms; giving up on it");
            vm.savexfer.flow = Flow::Idle;
        }
        // A question is up only with its modal over the list, and a row
        // cannot be pressed under a modal; so the question was dropped with
        // the screen it was on, and its answer is not coming.
        stale @ (Flow::AskImport | Flow::AskApply | Flow::AskRestore) => {
            log::info!("savexfer: dropping a stale {stale:?}");
            vm.savexfer.flow = Flow::Idle;
        }
    }
    vm.savexfer.result = None;
    vm.savexfer.ready = None;
    match action {
        Action::Export => export(vm),
        Action::Import => {
            if ask(vm, Q_IMPORT)? {
                vm.savexfer.flow = Flow::AskImport;
            }
            Ok(())
        }
        Action::Restore => restore(vm),
    }
}

/// Whether the options screen is the in-game menu's (`_menuState == 0`) or
/// the title menu's (4). The same test the cave's `opt_in_game` makes.
fn in_game(vm: &mut Vm) -> bool {
    vm.symbol("_menuState").is_some_and(|s| vm.mem.r32(s) == 0)
}

/// In game, run the game's own save first, so the file holds the moment the
/// row was pressed — the HUD's SAVE button and the save key (`GameFrame_
/// gamekey`, key -10) are `GameFrame_getCanSavable`, then the three writers.
/// Where the game forbids saving (`getCanSavable` is a byte the story sets),
/// the export does not get to overrule it. On the title there is no open
/// game: the files are the save.
fn save_first(vm: &mut Vm) -> Result<bool, String> {
    if !in_game(vm) {
        return Ok(true);
    }
    let (Some(can), Some(hero), Some(game), Some(pet), Some(get_hero)) = (
        vm.symbol("GameFrame_getCanSavable"),
        vm.symbol("GameFrame_save_hero_data"),
        vm.symbol("GameFrame_saveGame"),
        vm.symbol("GameFrame_savePetData"),
        vm.symbol("MP_getHero"),
    ) else {
        return Ok(false);
    };
    if vm.call(can, &[])? == 0 {
        log::info!("savexfer: the game forbids saving here; exporting the last save");
        return Ok(false);
    }
    // The save key also refuses mid-swing (`hero[0xc1] == 4`); the in-game
    // menu stops the world, but the check is one byte.
    let h = vm.call(get_hero, &[0])?;
    if h != 0 && vm.mem.r8(h + 0xc1) == 4 {
        log::info!("savexfer: the hero is mid-attack; exporting the last save");
        return Ok(false);
    }
    let a = vm.call(hero, &[0])?;
    let b = vm.call(game, &[])?;
    vm.call(pet, &[])?;
    if a == 0 || b == 0 {
        log::warn!("savexfer: the game's own save reported failure ({a}, {b}); exporting what is on disk");
        return Ok(false);
    }
    vm.savexfer.saved_first += 1;
    Ok(true)
}

fn stamp_now(vm: &Vm) -> String {
    let ms = vm.wall_now_ms().unwrap_or(0).max(0) as u64;
    sb::local_stamp(ms, vm.savexfer.utc_offset_min)
}

fn export(vm: &mut Vm) -> Result<(), String> {
    let fresh = save_first(vm)?;
    let stamp = stamp_now(vm);
    // A file of the save that is there and cannot be read is not left out: a
    // bundle without it would read, on the other phone, as "no chest".
    let snap = match sb::snapshot(&vm.cfg.storage, &stamp) {
        Ok(Some(s)) => s,
        Ok(None) => return tell(vm, M_NO_SAVE),
        Err(e) => {
            log::warn!("savexfer: cannot read the save to export it: {e}");
            return tell(vm, M_EXPORT_FAILED);
        }
    };
    // And one the other phone would refuse is not worth the save-as screen.
    let characters = match snap.check {
        Ok(0) => return tell(vm, M_NO_SAVE),
        Ok(n) => n,
        Err(why) => {
            log::warn!("savexfer: the save on disk is not one an import accepts ({why}); not exporting it");
            return tell(vm, M_EXPORT_FAILED);
        }
    };
    let id = new_request(vm);
    let file = sb::export_file(&vm.cfg.storage, id);
    if let Err(e) = sb::write_atomically(&file, &snap.bytes) {
        log::warn!("savexfer: cannot write the export: {e}");
        return tell(vm, M_EXPORT_FAILED);
    }
    log::info!(
        "savexfer: export {id} of {characters} character(s), {} bytes, ready for the save-as screen",
        snap.bytes.len()
    );
    vm.savexfer.request = Some(DocRequest::Export { id, file, name: suggested_name(&stamp) });
    vm.savexfer.flow = Flow::Exporting { id, fresh };
    vm.savexfer.exports += 1;
    Ok(())
}

fn restore(vm: &mut Vm) -> Result<(), String> {
    let Some((ms, path)) = sb::newest_vault(&vm.cfg.storage) else {
        return tell(vm, M_NO_VAULT);
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) if sb::parse(&b).is_ok() => b,
        _ => {
            log::warn!("savexfer: {} is not a usable generation", path.display());
            return tell(vm, M_BAD_FILE);
        }
    };
    let stamp = sb::local_stamp(ms, vm.savexfer.utc_offset_min);
    if ask(vm, &restore_question(&stamp))? {
        vm.savexfer.ready = Some(bytes);
        vm.savexfer.flow = Flow::AskRestore;
    }
    Ok(())
}

/// One of our questions was answered. The cave has already taken the modal
/// down, so a message raised here goes straight up.
pub fn answered(vm: &mut Vm, yes: bool) -> Result<(), String> {
    let flow = vm.savexfer.flow;
    if !matches!(flow, Flow::AskImport | Flow::AskApply | Flow::AskRestore) {
        // Only questions are answered (the cave reports nothing for a
        // message), and none is up: in particular, a request still copying
        // is not given up on here.
        log::info!("savexfer: an answer with no question waiting ({flow:?})");
        return Ok(());
    }
    vm.savexfer.flow = Flow::Idle;
    match flow {
        Flow::AskImport if yes => {
            let id = new_request(vm);
            let file = sb::import_file(&vm.cfg.storage, id);
            vm.savexfer.request = Some(DocRequest::Import { id, file });
            vm.savexfer.flow = Flow::Picking { id };
            vm.savexfer.picks += 1;
        }
        Flow::AskApply | Flow::AskRestore if yes => {
            let bytes = vm.savexfer.ready.take().unwrap_or_default();
            match sb::stage(&vm.cfg.storage, &bytes) {
                Ok(()) => {
                    vm.savexfer.staged += 1;
                    log::info!(
                        "savexfer: {} staged for the next start; closing",
                        if flow == Flow::AskApply { "import" } else { "restore" }
                    );
                    vm.phase = Phase::Exited;
                }
                Err(sb::StageFailed::Cancelled(e)) => {
                    log::warn!("savexfer: cannot stage: {e}");
                    tell(vm, M_STAGE_FAILED)?;
                }
                // The stage may be on the disk, and the next start would apply
                // it over anything played from here: play stops, as it would
                // have on 예 — and if the stage is there, it is what was asked.
                Err(sb::StageFailed::Unsure(e)) => {
                    log::warn!("savexfer: the stage may be on the disk and could not be taken back ({e}); closing");
                    vm.phase = Phase::Exited;
                }
            }
        }
        _ => {
            vm.savexfer.ready = None;
            log::info!("savexfer: 아니오 ({flow:?})");
        }
    }
    Ok(())
}

/// The shell's document screen for request `id` ended, and its copy with it.
/// Taken only if `id` is the request in flight. An answer for any other —
/// one given up on after [`PATIENCE_MS`] — changes nothing but its own file,
/// which nobody else uses and nothing will read now.
pub fn done(vm: &mut Vm, id: u32, status: DocStatus) {
    let current = match vm.savexfer.flow {
        Flow::Exporting { id, .. } | Flow::Picking { id } => Some(id),
        _ => None,
    };
    if current == Some(id) {
        log::info!("savexfer: document screen {id} {status:?}");
        vm.savexfer.result = Some(status);
        return;
    }
    log::info!("savexfer: a late answer ({status:?}) for request {id}, which is no longer waited for");
    for f in [sb::export_file(&vm.cfg.storage, id), sb::import_file(&vm.cfg.storage, id)] {
        let _ = std::fs::remove_file(f);
    }
}

/// The start of a frame: act on the document screen's result, if one came.
pub fn deliver(vm: &mut Vm) -> Result<(), String> {
    let Some(status) = vm.savexfer.result.take() else { return Ok(()) };
    let flow = std::mem::take(&mut vm.savexfer.flow);
    match flow {
        Flow::Exporting { id, fresh } => {
            let _ = std::fs::remove_file(sb::export_file(&vm.cfg.storage, id));
            match status {
                DocStatus::Done => tell(vm, if fresh { M_EXPORTED } else { M_EXPORTED_LAST })?,
                DocStatus::Cancelled => {}
                DocStatus::Failed => tell(vm, M_EXPORT_FAILED)?,
            }
        }
        Flow::Picking { id } => {
            let file = sb::import_file(&vm.cfg.storage, id);
            match status {
                DocStatus::Done => picked(vm, &file)?,
                DocStatus::Cancelled | DocStatus::Failed => {
                    // A copy that failed part-way may have left half a file.
                    let _ = std::fs::remove_file(&file);
                    if status == DocStatus::Failed {
                        tell(vm, M_READ_FAILED)?;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// The picked file is in `file` ([`sb::import_file`]): check it and ask.
fn picked(vm: &mut Vm, file: &std::path::Path) -> Result<(), String> {
    let bytes = std::fs::read(file);
    let _ = std::fs::remove_file(file);
    let bytes = match bytes {
        Ok(b) => b,
        Err(e) => {
            log::warn!("savexfer: the picked file did not arrive: {e}");
            return tell(vm, M_READ_FAILED);
        }
    };
    match sb::parse_import(&bytes) {
        Ok(b) => {
            log::info!(
                "savexfer: picked a save exported {:?} — {} character(s), {} file(s)",
                b.exported,
                b.characters,
                b.files.len()
            );
            if ask(vm, &apply_question(&b.exported, b.characters))? {
                vm.savexfer.ready = Some(bytes);
                vm.savexfer.flow = Flow::AskApply;
            }
            Ok(())
        }
        Err(sb::Reject::NoCharacter) => tell(vm, M_NO_CHARACTER),
        Err(sb::Reject::NotBundle(why)) => {
            log::warn!("savexfer: the picked file is not a save: {why}");
            tell(vm, M_BAD_FILE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk EUC-KR and split on `;`, failing on a half syllable.
    fn runs(msg: &[u8]) -> Vec<&[u8]> {
        assert!(!msg.contains(&0), "an interior NUL truncates the modal");
        let mut i = 0;
        while i < msg.len() {
            if msg[i] >= 0x80 {
                let t = *msg.get(i + 1).expect("a lead byte with no trail byte");
                assert!((0xa1..=0xfe).contains(&t), "trail byte {t:#04x} at {i}");
                i += 2;
            } else {
                i += 1;
            }
        }
        msg.split(|b| *b == b';').collect()
    }

    /// Pixels a run takes in the game's font: 10 per syllable, 6 per ASCII
    /// glyph, 5 per space (the strides measured for `pen_offset`).
    fn width(run: &[u8]) -> usize {
        let mut w = 0;
        let mut i = 0;
        while i < run.len() {
            if run[i] >= 0x80 {
                w += 10;
                i += 2;
            } else {
                w += if run[i] == b' ' { 5 } else { 6 };
                i += 1;
            }
        }
        w
    }

    /// The modal box is 0xb0 wide and the text is wrapped at 0xa8; every
    /// message and both built questions stay inside that, three lines at
    /// most, with no byte the font treats as a colour escape.
    #[test]
    fn every_modal_line_fits_the_box() {
        let fixed: [&[u8]; 11] = [
            Q_IMPORT,
            M_EXPORTED,
            M_EXPORTED_LAST,
            M_EXPORT_FAILED,
            M_NO_SAVE,
            M_BAD_FILE,
            M_NO_CHARACTER,
            M_READ_FAILED,
            M_NO_VAULT,
            M_STAGE_FAILED,
            M_BUSY,
        ];
        let stamp = "2026-10-28 14:30";
        let built = [apply_question(stamp, 9), restore_question(stamp)];
        for m in fixed.iter().copied().chain(built.iter().map(|v| v.as_slice())) {
            assert!(m.len() <= TEXT_MAX, "{m:02x?} is longer than the text buffer");
            let lines = runs(m);
            assert!(!lines.is_empty() && lines.len() <= 3, "{} lines in {m:02x?}", lines.len());
            for (i, r) in lines.iter().enumerate() {
                assert!(!r.is_empty(), "an empty line in {m:02x?}");
                assert!(width(r) <= 0xa8, "line {i} of {m:02x?} is {} px", width(r));
                for &c in r.iter() {
                    // `HF_IsEscape`: `|`, `&`, `$`, `{`, `}`, `o`..`t` and `;`
                    // are zero-width colour escapes to the font.
                    assert!(
                        !matches!(c, b'|' | b'&' | b'$' | b'{' | b'}' | b'o'..=b't'),
                        "{:?} in {m:02x?} is a colour escape",
                        c as char
                    );
                }
            }
        }
        assert_eq!(runs(&apply_question(stamp, 3)).len(), 3);
        assert_eq!(runs(&restore_question(stamp)).len(), 3);
    }

    /// A fresh storage directory for one test, so nothing it writes outlives
    /// it or meets another test's files.
    fn fresh_storage(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cs_savexfer_{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The options screen with the export row selected, as the cave's tests
    /// set it up (`cave.rs`): the record writers stubbed, the LCD sized.
    fn on_the_export_row(vm: &mut Vm) {
        let lcd = vm.symbol("_lcdG").unwrap();
        vm.mem.w32(lcd + 0x4c, 400);
        vm.mem.w32(lcd + 0x50, 240);
        for stub in ["MX_sdSaveData", "MX_sdLoadData"] {
            let a = vm.symbol(stub).unwrap();
            vm.mem.w32(a, 0xe12f_ff1e); // bx lr
        }
        let loaded = vm.symbol("opt_loaded").unwrap();
        vm.mem.write_bytes(loaded, &[1]);
        vm.mem.w32(vm.symbol("_menuState").unwrap(), 4);
        vm.mem.write_bytes(vm.symbol("_menuListKey").unwrap(), &[13]);
        vm.mem.w32(vm.symbol("_menuPoint").unwrap(), 7);
        vm.mem.write_bytes(vm.symbol("opt_modal").unwrap(), &[0]);
    }

    fn modal(vm: &mut Vm) -> (u8, Vec<u8>) {
        let m = vm.symbol("opt_modal").unwrap();
        (vm.mem.r8(m), vm.mem.cstr_bytes(m + 4))
    }

    fn close_modal(vm: &mut Vm) {
        let m = vm.symbol("opt_modal").unwrap();
        vm.mem.write_bytes(m, &[0]);
    }

    /// Press a row the way the cave does: `r0` the row, `LR` where the cave
    /// would continue. A served entry hook must jump there.
    fn press(vm: &mut Vm, action: u32) {
        vm.cpu.reg_set(Mode::User, 0, action);
        vm.cpu.reg_set(Mode::User, reg::LR, 0x0bad_0000);
        vm.cpu.reg_set(Mode::User, reg::PC, 0x0100_0000);
        assert!(vm.host_xfer_action().unwrap(), "the press is served");
        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x0bad_0000, "a served entry hook must jump to LR");
    }

    fn answer(vm: &mut Vm, yes: bool) {
        vm.cpu.reg_set(Mode::User, 0, yes as u32);
        vm.cpu.reg_set(Mode::User, reg::LR, 0x0bad_0004);
        assert!(vm.host_xfer_answer().unwrap());
        assert_eq!(vm.cpu.reg_get(Mode::User, reg::PC), 0x0bad_0004);
    }

    /// The whole conversation, driven through the two hooks and the shell's
    /// answers, against the cave's modal in a loaded guest: nothing to export
    /// on an empty storage; nothing to restore; an import whose file screen
    /// failed; one whose file is not a save; one whose file is a save, down to
    /// the staged bundle and the exit request.
    #[test]
    fn the_rows_talk_through_the_caves_modal() {
        let mut vm = crate::patch::tests::loaded_guest();
        vm.cfg.storage = fresh_storage("rows");
        on_the_export_row(&mut vm);
        let storage = vm.cfg.storage.clone();

        // Export with no save at all.
        press(&mut vm, 0);
        assert_eq!(modal(&mut vm), (1, M_NO_SAVE.to_vec()));
        assert!(vm.take_save_doc_request().is_none());
        // Restore with an empty vault.
        on_the_export_row(&mut vm);
        press(&mut vm, 2);
        assert_eq!(modal(&mut vm), (1, M_NO_VAULT.to_vec()));

        // Import: the question, 아니오, nothing asked of the shell.
        on_the_export_row(&mut vm);
        press(&mut vm, 1);
        assert_eq!(modal(&mut vm), (2, Q_IMPORT.to_vec()));
        close_modal(&mut vm);
        answer(&mut vm, false);
        assert!(vm.take_save_doc_request().is_none());
        assert_eq!(vm.savexfer.flow, Flow::Idle);

        // Import: 예, the shell's open screen failed.
        press(&mut vm, 1);
        close_modal(&mut vm);
        answer(&mut vm, true);
        let req = vm.take_save_doc_request().unwrap();
        assert_eq!(req, DocRequest::Import { id: req.id(), file: sb::import_file(&storage, req.id()) });
        vm.save_doc_done(req.id(), DocStatus::Failed);
        deliver(&mut vm).unwrap();
        assert_eq!(modal(&mut vm), (1, M_READ_FAILED.to_vec()));

        // Import: the picked file is a photo.
        on_the_export_row(&mut vm);
        press(&mut vm, 1);
        close_modal(&mut vm);
        answer(&mut vm, true);
        let id = vm.take_save_doc_request().unwrap().id();
        std::fs::write(sb::import_file(&storage, id), b"\x89PNG not a save").unwrap();
        vm.save_doc_done(id, DocStatus::Done);
        deliver(&mut vm).unwrap();
        assert_eq!(modal(&mut vm), (1, M_BAD_FILE.to_vec()));
        assert!(!sb::import_file(&storage, id).exists(), "the picked file is consumed");

        // Import: a real bundle, two characters, exported on a known date.
        let src = fresh_storage("rows_src");
        std::fs::write(src.join("kickass"), sb::tests::container(2, 7)).unwrap();
        let bundle = sb::snapshot(&src, "2026-10-28 14:30").unwrap().unwrap().bytes;
        on_the_export_row(&mut vm);
        press(&mut vm, 1);
        close_modal(&mut vm);
        answer(&mut vm, true);
        let id = vm.take_save_doc_request().unwrap().id();
        std::fs::write(sb::import_file(&storage, id), &bundle).unwrap();
        vm.save_doc_done(id, DocStatus::Done);
        deliver(&mut vm).unwrap();
        assert_eq!(modal(&mut vm), (2, apply_question("2026-10-28 14:30", 2)));
        assert_eq!(vm.savexfer.flow, Flow::AskApply);
        close_modal(&mut vm);
        answer(&mut vm, true);
        assert_eq!(std::fs::read(storage.join(sb::PENDING)).unwrap(), bundle, "staged byte for byte");
        assert_eq!(vm.phase, Phase::Exited, "the game asks to close");
        assert_eq!(vm.savexfer.staged, 1);

        // And the next start applies it, keeping nothing (there was nothing).
        let r = sb::apply_pending(&storage, 1);
        assert_eq!(r.outcome, sb::Outcome::Applied, "{:?}", r.log);
        assert!(r.log.iter().any(|l| l.contains("applied")), "{:?}", r.log);
        assert_eq!(std::fs::read(storage.join("kickass")).unwrap(), sb::tests::container(2, 7));
    }

    /// Export on the title menu with a save on disk: the bundle is written
    /// for the shell, and the shell's three answers map to the right modal.
    #[test]
    fn an_export_waits_for_the_shell_and_reports_what_it_said() {
        let mut vm = crate::patch::tests::loaded_guest();
        vm.cfg.storage = fresh_storage("export");
        let storage = vm.cfg.storage.clone();
        std::fs::write(storage.join("kickass"), sb::tests::container(1, 3)).unwrap();
        for (status, want) in [
            (DocStatus::Done, Some(M_EXPORTED)),
            (DocStatus::Cancelled, None),
            (DocStatus::Failed, Some(M_EXPORT_FAILED)),
        ] {
            on_the_export_row(&mut vm);
            press(&mut vm, 0);
            assert_eq!(modal(&mut vm).0, 0, "nothing to say until the shell answers");
            let Some(DocRequest::Export { id, file, name }) = vm.take_save_doc_request() else {
                panic!("an export request")
            };
            assert!(name.ends_with(".cssave"));
            let b = sb::parse_import(&std::fs::read(&file).unwrap()).unwrap();
            assert_eq!(b.characters, 1);
            assert_eq!(b.files[0].1, sb::tests::container(1, 3));
            assert_eq!(file, sb::export_file(&storage, id));
            vm.save_doc_done(id, status);
            deliver(&mut vm).unwrap();
            assert!(!file.exists(), "the export file is swept after the screen");
            match want {
                Some(m) => assert_eq!(modal(&mut vm), (1, m.to_vec()), "{status:?}"),
                None => assert_eq!(modal(&mut vm).0, 0, "{status:?}: no modal"),
            }
        }
        assert_eq!(vm.savexfer.exports, 3);
        assert_eq!(vm.savexfer.saved_first, 0, "the title menu has no game to save first");
    }

    /// The second review's first finding. The screen closes and the game is
    /// back while a cloud document is still copying; a press then must not
    /// start over and take the old copy's answer as its own. It is refused
    /// while the copy runs, and the copy's answer is still taken. If an
    /// answer is never coming, the rows come back after `PATIENCE_MS` — and
    /// the given-up request's late answer touches nothing but its own file.
    #[test]
    fn a_copy_that_outlives_its_screen_keeps_its_answer() {
        let mut vm = crate::patch::tests::loaded_guest();
        vm.cfg.storage = fresh_storage("inflight");
        let storage = vm.cfg.storage.clone();
        std::fs::write(storage.join("kickass"), sb::tests::container(1, 3)).unwrap();
        let src = fresh_storage("inflight_src");
        std::fs::write(src.join("kickass"), sb::tests::container(2, 7)).unwrap();
        let bundle = sb::snapshot(&src, "2026-10-28 14:30").unwrap().unwrap().bytes;

        // 불러오기, 예: the open screen is asked for, and then closes; the copy
        // goes on without an answer yet.
        on_the_export_row(&mut vm);
        press(&mut vm, 1);
        close_modal(&mut vm);
        answer(&mut vm, true);
        let first = vm.take_save_doc_request().unwrap().id();

        // Every row, pressed meanwhile: refused, and nothing asked of the shell.
        for row in 0..3 {
            on_the_export_row(&mut vm);
            press(&mut vm, row);
            assert_eq!(modal(&mut vm), (1, M_BUSY.to_vec()), "row {row}");
            assert!(vm.take_save_doc_request().is_none(), "row {row}");
            assert_eq!(vm.savexfer.flow, Flow::Picking { id: first }, "row {row}");
        }
        // Dismissing the busy message is not an answer to anything.
        close_modal(&mut vm);
        answer(&mut vm, true);
        assert_eq!(vm.savexfer.flow, Flow::Picking { id: first });

        // The copy lands: its answer is taken, for its own file.
        std::fs::write(sb::import_file(&storage, first), &bundle).unwrap();
        vm.save_doc_done(first, DocStatus::Done);
        deliver(&mut vm).unwrap();
        assert_eq!(modal(&mut vm), (2, apply_question("2026-10-28 14:30", 2)));
        close_modal(&mut vm);
        answer(&mut vm, false);

        // An export whose answer never comes holds the rows only so long.
        on_the_export_row(&mut vm);
        press(&mut vm, 0);
        let Some(DocRequest::Export { id: lost, file: lost_file, .. }) = vm.take_save_doc_request() else {
            panic!("an export request")
        };
        vm.savexfer.since_ms = vm.savexfer.since_ms.saturating_sub(PATIENCE_MS);
        on_the_export_row(&mut vm);
        press(&mut vm, 0);
        let Some(DocRequest::Export { id: next, file: next_file, .. }) = vm.take_save_doc_request() else {
            panic!("a new export request once the old one is given up on")
        };
        assert_ne!(next, lost);
        assert_ne!(next_file, lost_file, "each request has a file of its own");
        assert!(lost_file.exists() && next_file.exists());

        // The given-up request answers after all: ignored, and its file goes.
        vm.save_doc_done(lost, DocStatus::Done);
        deliver(&mut vm).unwrap();
        assert_eq!(modal(&mut vm).0, 0, "no message for a request nobody waits for");
        assert!(!lost_file.exists());
        assert!(next_file.exists(), "the request in flight keeps its file");
        assert_eq!(vm.savexfer.flow, Flow::Exporting { id: next, fresh: true });

        // And the one in flight still gets its own answer.
        vm.save_doc_done(next, DocStatus::Done);
        deliver(&mut vm).unwrap();
        assert_eq!(modal(&mut vm), (1, M_EXPORTED.to_vec()));
        assert!(!next_file.exists());
    }

    /// What a start sweeps: every request's file, of this naming and the
    /// single files of earlier builds, and nothing else.
    #[test]
    fn a_start_sweeps_every_document_file() {
        let s = fresh_storage("sweep");
        for f in [
            sb::export_file(&s, 1),
            sb::import_file(&s, 7),
            s.join("cs_export_3.cssave.xfer"),
            s.join("cs_export.cssave"),
            s.join("cs_import_in.cssave"),
        ] {
            std::fs::write(&f, b"x").unwrap();
        }
        std::fs::write(s.join("kickass"), sb::tests::container(1, 1)).unwrap();
        std::fs::write(s.join(crate::vault::FILE), b"chest").unwrap();
        let r = sb::apply_pending(&s, 1);
        assert_eq!(r.outcome, sb::Outcome::Idle);
        let mut left: Vec<String> =
            std::fs::read_dir(&s).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, vec![crate::vault::FILE.to_string(), "kickass".to_string()]);
    }

    /// 되돌리기 with a vault: the question names the generation's date, 예
    /// stages it and asks to close.
    #[test]
    fn a_restore_stages_the_newest_generation() {
        let mut vm = crate::patch::tests::loaded_guest();
        vm.cfg.storage = fresh_storage("restore");
        let storage = vm.cfg.storage.clone();
        // 2026-10-25 05:30 UTC, which is 14:30 in Korea (the test VM's offset).
        let ms = 1_792_906_200_000u64;
        let gen = storage.join(sb::VAULT_DIR).join(format!("{ms:014}.{}", sb::EXT));
        let src = fresh_storage("restore_src");
        std::fs::write(src.join("kickass"), sb::tests::container(1, 1)).unwrap();
        let bundle = sb::snapshot(&src, "").unwrap().unwrap().bytes;
        sb::write_atomically(&gen, &bundle).unwrap();
        on_the_export_row(&mut vm);
        press(&mut vm, 2);
        assert_eq!(modal(&mut vm), (2, restore_question("2026-10-25 14:30")));
        close_modal(&mut vm);
        answer(&mut vm, true);
        assert_eq!(std::fs::read(storage.join(sb::PENDING)).unwrap(), bundle);
        assert_eq!(vm.phase, Phase::Exited);
    }

    /// 예 to a restore whose stage cannot be written. When the stage is surely
    /// not on the disk the player is told and plays on; when it may be, play
    /// stops — the next start would apply it over anything played.
    #[test]
    fn a_stage_that_fails_is_taken_back_or_play_stops() {
        for (name, unsure) in [("stage_cancelled", false), ("stage_unsure", true)] {
            let mut vm = crate::patch::tests::loaded_guest();
            vm.cfg.storage = fresh_storage(name);
            let storage = vm.cfg.storage.clone();
            let gen = storage.join(sb::VAULT_DIR).join(format!("{:014}.{}", 1_792_906_200_000u64, sb::EXT));
            let src = fresh_storage(&format!("{name}_src"));
            std::fs::write(src.join("kickass"), sb::tests::container(1, 1)).unwrap();
            sb::write_atomically(&gen, &sb::snapshot(&src, "").unwrap().unwrap().bytes).unwrap();
            // The write's temp file cannot be made, so nothing reaches the
            // stage's name; or that name holds a directory nothing can replace
            // or remove.
            let obstacle = match unsure {
                false => storage.join(format!("{}.xfer", sb::PENDING)),
                true => storage.join(sb::PENDING),
            };
            std::fs::create_dir_all(obstacle.join("x")).unwrap();
            on_the_export_row(&mut vm);
            press(&mut vm, 2);
            close_modal(&mut vm);
            answer(&mut vm, true);
            assert_eq!(vm.savexfer.staged, 0, "{name}");
            if unsure {
                assert_eq!(vm.phase, Phase::Exited, "play stops");
            } else {
                assert_eq!(modal(&mut vm), (1, M_STAGE_FAILED.to_vec()));
                assert_ne!(vm.phase, Phase::Exited, "the player plays on");
                assert!(!storage.join(sb::PENDING).exists());
            }
        }
    }

    #[test]
    fn the_suggested_name_carries_the_stamp() {
        assert_eq!(suggested_name("2026-10-28 14:30"), "크로노스소드_세이브_20261028_1430.cssave");
    }

    #[test]
    fn document_status_codes_are_the_java_ones() {
        assert_eq!(DocStatus::from_code(0), DocStatus::Done);
        assert_eq!(DocStatus::from_code(1), DocStatus::Cancelled);
        assert_eq!(DocStatus::from_code(2), DocStatus::Failed);
        assert_eq!(DocStatus::from_code(-7), DocStatus::Failed);
        assert_eq!(Action::from_code(0), Some(Action::Export));
        assert_eq!(Action::from_code(2), Some(Action::Restore));
        assert_eq!(Action::from_code(3), None);
    }
}
