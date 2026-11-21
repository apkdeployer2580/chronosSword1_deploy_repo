//! 세이브 내보내기 / 불러오기 / 되돌리기 — the file that carries a save from
//! one phone to another, and everything that touches the disk on its behalf.
//! The menu side (the rows, the modal, the phone's file screens) is
//! [`crate::savexfer`]; this is pure files, with no guest in sight.
//!
//! # What a save is, on disk
//!
//! The guest keeps everything in **one** MIDAS container, `kickass`, at the
//! storage root. Its layout is what `_midas_funcSdReadHeaderEx` and
//! `_midas_funcSdLoadData` read, and [`check_container`] restates it: a table
//! of 20-byte `[name 16][u16 capacity][u16 size]` entries, then the records'
//! bodies one after another at their capacities. The names are `AGREE`,
//! `OPTIONDATA`, and per slot `HERODATA00n` / `GAMEDATA00n` / `PETDATA00n`;
//! the cave's `CSOPT001` and `CSSKIN01` live there too. The host keeps two
//! more files of the player's things beside it: the 공용창고 (`cs_vault.txt`,
//! its gold and its in-flight slot included) and the three timed 비약
//! (`cs_potions.json`). The install's identity (`cs_id.txt`,
//! `cs_identity.json`) does **not** travel: it is the key to a mailbox on the
//! private server, and two phones holding one key is the confusion the
//! identity exists to prevent.
//!
//! The container's records are DES-encrypted with a key that is a constant in
//! the binary (`MX_desInit` is handed a `.rodata` string from `onStartApp`),
//! so a save is not bound to the phone: the bytes open anywhere this build
//! runs. Nothing here decrypts anything — a bundle carries files byte for
//! byte, which is also why restoring one needs no guest and no key.
//!
//! # The bundle
//!
//! An uncompressed zip, so a support request can be opened with any tool:
//! `manifest.txt` first (`chronos save bundle 1`, the export time, how many
//! characters), then the members of [`MEMBERS`] that existed. Reading is
//! strict on purpose — only what [`zip`] writes is accepted: stored entries,
//! no encryption, no data descriptors, every CRC checked, every name on the
//! list, none twice. A bundle offered for **import** is held to more: its
//! container must be one the guest can read, record by record, and must hold
//! a character, and the chest it carries must be one the vault reads
//! ([`parse_import`]).
//!
//! # Applying one: at the next start, never live, and never half
//!
//! The guest holds the loaded character in its heap and `MX_sdSaveData`
//! rewrites the whole container on every save, so bytes swapped under a
//! running game are overwritten by the next autosave. So the menu only
//! *stages* a bundle ([`stage`]) and closes the game, and [`apply_pending`]
//! swaps the files at the next start — called by the shell before it reads
//! any of them, and before the guest exists.
//!
//! A bundle is up to three files and no two of them can be replaced together,
//! so an apply that stops in the middle leaves a save that is half one and
//! half the other — a character from one phone with another phone's chest,
//! whose in-flight transfer then settles against the wrong bag. The apply is
//! therefore journalled, and **the game only ever starts on a disk with no
//! journal on it**:
//!
//! 1. the staged bundle is parsed again (integrity, not policy, here);
//! 2. the files it will replace are read — **a read that fails for any reason
//!    but "not there" stops everything**: a file that cannot be read is not a
//!    file that does not exist, and treating it as absent would apply over it
//!    with no copy kept;
//! 3. those files go into the vault (for 세이브 되돌리기) **and** into
//!    [`BEFORE`] (for the journal); if either cannot be written, nothing is
//!    applied;
//! 4. the stage is renamed to [`APPLYING`] — the commit point;
//! 5. every member is written (temp + fsync + rename) or, when the bundle does
//!    not have it, removed;
//! 6. on success the journal goes — `APPLYING` first, then `BEFORE` — and the
//!    directory is synced before the game may start, because a removal that
//!    a power cut takes back is the journal returning. On a failure the files
//!    from `BEFORE` are written back the same way, and if that works the
//!    journal goes too: nothing changed.
//!
//! When neither direction can be finished, the journal stays and the start is
//! [`Outcome::Blocked`]: the shell does not boot the guest. The next start
//! finds `APPLYING` and tries again — forward first, since that is what the
//! player confirmed, then back. Both are rewrites of whole files from bundles
//! that are already on disk, so trying again is harmless however often it
//! happens. A start that died in step 5 lands in the same place.
//!
//! The rule that the journal must be **gone** before the game runs is not
//! tidiness. A journal left beside a running game would be applied again at
//! the next start, on top of everything played in between. The stage is the
//! same promise one step earlier, so the same rule holds for it: wherever the
//! game goes on without keeping it — a [`stage`] that failed, an apply given
//! up on — it is removed and the removal synced, or play stops. And every
//! start syncs the directory before reading it, so a removal an earlier run
//! could not make durable is made durable before anything is decided on it.
//!
//! # The vault
//!
//! `cs_save_vault/<14-digit unix ms>.cssave`, the same format, one per apply,
//! newest [`VAULT_KEEP`] kept. 세이브 되돌리기 stages the newest — the save as
//! it was just before the last import or restore — so pressing it twice
//! swaps back. Applies are rare (a player action each), so a flat window is
//! enough here; the game's own saves never touch the vault.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// The suffix the shell suggests and the vault uses.
pub const EXT: &str = "cssave";
/// A bundle's first manifest line. The number is the format, not the build.
const MAGIC_LINE: &str = "chronos save bundle 1";
const MANIFEST: &str = "manifest.txt";

/// A bundle the menu accepted, waiting for the next start.
pub const PENDING: &str = "cs_save_import.cssave";
/// [`PENDING`] once the apply has committed — the journal. While it exists the
/// files on disk may be a mix, and the game does not start.
pub const APPLYING: &str = "cs_save_import.applying";
/// What was on disk before the committed apply, as a bundle: the journal's
/// way back. Written before the commit, removed after [`APPLYING`].
pub const BEFORE: &str = "cs_save_import.before";
/// The files a document screen works on carry the request's number, because a
/// copy can outlive its screen: a cloud provider downloads inside
/// `openInputStream`, after the screen has closed and the game is back. A
/// second request made meanwhile must not share the first one's file — the
/// first copy would write into it, or delete it, under the second.
const EXPORT_PREFIX: &str = "cs_export_";
const IMPORT_PREFIX: &str = "cs_import_in_";

/// The export the shell copies to the player's chosen document, for request
/// `id`.
pub fn export_file(storage: &Path, id: u32) -> PathBuf {
    storage.join(format!("{EXPORT_PREFIX}{id}.{EXT}"))
}

/// Where the shell copies the document the player picked, for request `id`.
pub fn import_file(storage: &Path, id: u32) -> PathBuf {
    storage.join(format!("{IMPORT_PREFIX}{id}.{EXT}"))
}

/// Whether `name` is a document screen's file — of any request, any run,
/// and its temp file too. Nothing else at the root starts this way.
fn is_doc_file(name: &str) -> bool {
    // Without the trailing underscore, so the single files earlier builds
    // used (`cs_export.cssave`, `cs_import_in.cssave`) go too.
    name.starts_with(&EXPORT_PREFIX[..EXPORT_PREFIX.len() - 1])
        || name.starts_with(&IMPORT_PREFIX[..IMPORT_PREFIX.len() - 1])
}

pub const VAULT_DIR: &str = "cs_save_vault";
/// Generations the vault keeps. Five applies back is far past "undo the one I
/// just did", and each is a few tens of kilobytes.
pub const VAULT_KEEP: usize = 5;

/// One member's ceiling. A three-slot container is ~30 KB; the chest file a
/// few. A megabyte is not a save.
const MEMBER_MAX: usize = 1 << 20;
/// A whole bundle's — and what the shell is asked to copy at most.
pub const BUNDLE_MAX: usize = 4 << 20;

/// The first four bytes of a container in the current layout
/// (`_midas_funcSdReadHeaderEx` checks `ff`, `V`, `0`, `0`).
const CONTAINER_MAGIC: [u8; 4] = [0xff, 0x56, 0x30, 0x00];
/// `[name 16][u16 capacity][u16 size]`, little-endian halves.
const CONTAINER_ENTRY: usize = 20;
/// Entries in the table after the magic (`COUNT_OF_SD_FILE = 0x28`), and in
/// the older layout with no magic (`0x14`, the table from byte 0). The guest
/// reads both and writes back whichever it read (`_midas_funcSdWriteHeader`).
const ENTRIES_MAGIC: usize = 0x28;
const ENTRIES_LEGACY: usize = 0x14;
/// Every capacity the guest writes is the size rounded up to this
/// (`_midas_funcSdWriteFile`: `(size + 0xff) & ~0xff`), and the body is padded
/// out to it (`_midas_funcSdWriteRemain`).
const CONTAINER_GRAIN: usize = 0x100;
/// The sizes a hero record — the record that makes a slot a character — is
/// ever written at: `GameFrame_save_hero_data` writes 0x1807, and the code
/// cave's ten-bag save (`cave/src/bag10.c`, `HERO_SAVE_LEN`) 0x2407. A hero
/// record of any other size is not one the game's loader can read.
pub const HERO_RECORD_SIZES: [usize; 2] = [0x1807, 0x2407];

/// Everything a bundle can carry, all at the storage root. The name is both
/// the file's name and the zip entry's.
pub const MEMBERS: [&str; 3] = ["kickass", crate::vault::FILE, crate::potions::FILE];
const KICKASS: usize = 0;

/// Where a member lives under `storage`.
pub fn member_path(storage: &Path, name: &str) -> PathBuf {
    storage.join(name)
}

/// A bundle, read.
#[derive(Debug)]
pub struct Bundle {
    /// `YYYY-MM-DD HH:MM` on the phone that wrote it, filtered to those
    /// characters — it goes into a guest popup.
    pub exported: String,
    /// `(index into MEMBERS, bytes)`.
    pub files: Vec<(usize, Vec<u8>)>,
    /// `HERODATA` records in its `kickass`, when the container is one the
    /// guest reads; 0 otherwise.
    pub characters: usize,
}

/// Why a file is not something to apply.
#[derive(Debug, PartialEq, Eq)]
pub enum Reject {
    /// Not a bundle, or a damaged one; the reason is for the log.
    NotBundle(String),
    /// A sound bundle whose container holds no character. Policy, not
    /// integrity — the menu refuses it, a restore of the vault does not.
    NoCharacter,
}

// ---- the container ----------------------------------------------------------

/// Check a `kickass` against the layout the guest reads, record by record, and
/// count its characters. `Err` says what is wrong.
///
/// The rules are the loader's own (`_midas_funcSdLoadData`): a record's body
/// starts after the table plus the capacities of **every** earlier entry, in
/// table order, and is `size` bytes long; `size == 0` is a deleted record
/// (`_midas_funcSdDeleteFile` zeroes only that field). On top of the loader,
/// what every container the guest writes also is: names printable and
/// NUL-padded, empty entries all zero, capacities a multiple of
/// [`CONTAINER_GRAIN`] with the body padded out to them, no name twice. All 21
/// containers in the probe's storages pass exactly, at every size from one
/// record (1060 bytes) to a character (10788).
///
/// A character is a live `HERODATA` record at one of [`HERO_RECORD_SIZES`].
/// A table that names a hero whose body is not in the file — a header with
/// nothing behind it — is not a save.
pub fn check_container(k: &[u8]) -> Result<usize, String> {
    let (table, count) = if k.len() >= 4 && k[..4] == CONTAINER_MAGIC {
        (CONTAINER_MAGIC.len(), ENTRIES_MAGIC)
    } else {
        (0, ENTRIES_LEGACY)
    };
    let header = table + count * CONTAINER_ENTRY;
    if k.len() < header {
        return Err(format!("{} bytes, shorter than its {header}-byte table", k.len()));
    }
    let mut body = header;
    let mut names: Vec<&[u8]> = Vec::new();
    let mut characters = 0;
    for i in 0..count {
        let e = &k[table + i * CONTAINER_ENTRY..table + (i + 1) * CONTAINER_ENTRY];
        let name = &e[..16];
        let cap = u16::from_le_bytes([e[16], e[17]]) as usize;
        let size = u16::from_le_bytes([e[18], e[19]]) as usize;
        if name[0] == 0 {
            if e.iter().any(|&b| b != 0) {
                return Err(format!("entry {i} has no name but is not empty"));
            }
            continue;
        }
        let n = name.iter().position(|&c| c == 0).unwrap_or(16);
        if !name[..n].iter().all(u8::is_ascii_graphic) || name[n..].iter().any(|&c| c != 0) {
            return Err(format!("entry {i} is not a record name"));
        }
        let label = String::from_utf8_lossy(&name[..n]).into_owned();
        if names.contains(&name) {
            return Err(format!("{label} twice"));
        }
        names.push(name);
        if cap % CONTAINER_GRAIN != 0 {
            return Err(format!("{label}: capacity {cap:#x} is not whole blocks"));
        }
        if size > cap {
            return Err(format!("{label}: {size} bytes in a {cap}-byte record"));
        }
        if body + cap > k.len() {
            return Err(format!("{label}: its body at {body}..{} runs past the end ({})", body + cap, k.len()));
        }
        if size > 0 && name.starts_with(b"HERODATA") {
            if !HERO_RECORD_SIZES.contains(&size) {
                return Err(format!("{label}: a hero record of {size:#x} bytes"));
            }
            characters += 1;
        }
        body += cap;
    }
    Ok(characters)
}

/// How many characters a `kickass` holds, or `None` if it is not a container
/// the guest reads. See [`check_container`].
pub fn characters_in(kickass: &[u8]) -> Option<usize> {
    check_container(kickass).ok()
}

// ---- the zip ----------------------------------------------------------------

/// CRC-32 (IEEE), the one zip uses.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 };
        }
    }
    !c
}

/// 1980-01-01, the earliest DOS date: the entries carry no time of their own
/// (the manifest does), and a fixed stamp keeps two exports of the same files
/// byte-identical outside the manifest.
const DOS_DATE: u16 = (1 << 5) | 1;

fn put16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// A local file header and the data after it.
fn local_entry(out: &mut Vec<u8>, name: &str, data: &[u8]) {
    put32(out, 0x0403_4b50);
    put16(out, 20); // version needed
    put16(out, 0); // flags
    put16(out, 0); // stored
    put16(out, 0); // time
    put16(out, DOS_DATE);
    put32(out, crc32(data));
    put32(out, data.len() as u32);
    put32(out, data.len() as u32);
    put16(out, name.len() as u16);
    put16(out, 0); // extra
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(data);
}

/// A central directory entry for the local entry at `offset`.
fn central_entry(out: &mut Vec<u8>, name: &str, data: &[u8], offset: u32) {
    put32(out, 0x0201_4b50);
    put16(out, 20); // made by
    put16(out, 20); // needed
    put16(out, 0);
    put16(out, 0);
    put16(out, 0);
    put16(out, DOS_DATE);
    put32(out, crc32(data));
    put32(out, data.len() as u32);
    put32(out, data.len() as u32);
    put16(out, name.len() as u16);
    put16(out, 0); // extra
    put16(out, 0); // comment
    put16(out, 0); // disk
    put16(out, 0); // internal attributes
    put32(out, 0); // external attributes
    put32(out, offset);
    out.extend_from_slice(name.as_bytes());
}

fn end_record(out: &mut Vec<u8>, count: u16, cd_len: u32, cd_off: u32) {
    put32(out, 0x0605_4b50);
    put16(out, 0);
    put16(out, 0);
    put16(out, count);
    put16(out, count);
    put32(out, cd_len);
    put32(out, cd_off);
    put16(out, 0); // comment
}

/// Write an uncompressed zip.
pub fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = out.len() as u32;
        local_entry(&mut out, name, data);
        central_entry(&mut central, name, data, offset);
    }
    let (cd_off, cd_len) = (out.len() as u32, central.len() as u32);
    out.extend_from_slice(&central);
    end_record(&mut out, entries.len() as u16, cd_len, cd_off);
    out
}

fn get16(b: &[u8], i: usize) -> Option<usize> {
    b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as usize)
}

fn get32(b: &[u8], i: usize) -> Option<usize> {
    b.get(i..i + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
}

/// The most entries a bundle has: the manifest and every member.
const ENTRIES_MAX: usize = MEMBERS.len() + 1;

/// Read a zip [`zip`] could have written. Anything else is an error.
///
/// The directory is checked **whole** before any entry is checksummed or
/// copied. A directory can name one stretch of data any number of times, and
/// a reader that copies as it goes turns a 130 KB file into megabytes of
/// allocation — a few thousand entries more and the app is killed for memory
/// with the 4 MB ceiling never crossed. So first: at most [`ENTRIES_MAX`]
/// entries, no name twice, no entry over [`MEMBER_MAX`], the sizes together
/// under [`BUNDLE_MAX`], and the entries laid out one after another as
/// [`zip`] lays them, none inside another's data. Only then the CRCs and the
/// copies, which are therefore bounded by the input.
pub fn unzip(b: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let bad = |why: &str| Err(why.to_string());
    if b.len() < 22 {
        return bad("shorter than an end record");
    }
    // The end record is the last 22 bytes plus its comment; we write none but
    // tolerate one, the way every reader does.
    let lo = b.len().saturating_sub(22 + 0xffff);
    let eocd = (lo..=b.len() - 22)
        .rev()
        .find(|&i| get32(b, i) == Some(0x0605_4b50) && get16(b, i + 20).map(|c| i + 22 + c) == Some(b.len()))
        .ok_or_else(|| "no end record".to_string())?;
    let count = get16(b, eocd + 10).unwrap_or(usize::MAX);
    if count > ENTRIES_MAX {
        return Err(format!("{count} entries; a bundle has at most {ENTRIES_MAX}"));
    }
    let cd_len = get32(b, eocd + 12).unwrap_or(usize::MAX);
    let cd_off = get32(b, eocd + 16).unwrap_or(usize::MAX);
    if cd_off.checked_add(cd_len).map_or(true, |end| end > eocd) {
        return bad("central directory out of bounds");
    }

    struct Entry {
        name: String,
        crc: u32,
        /// The local header, and where its data starts and ends.
        local: usize,
        start: usize,
        end: usize,
    }
    let mut entries: Vec<Entry> = Vec::with_capacity(count);
    let mut total = 0usize;
    let mut p = cd_off;
    for _ in 0..count {
        if get32(b, p) != Some(0x0201_4b50) {
            return bad("broken central directory");
        }
        let field = |o: usize| get16(b, p + o).unwrap_or(usize::MAX);
        let (flags, method) = (field(8), field(10));
        let crc = get32(b, p + 16).unwrap_or(0) as u32;
        let (csize, size) = (get32(b, p + 20).unwrap_or(usize::MAX), get32(b, p + 24).unwrap_or(usize::MAX));
        let (n, extra, comment) = (field(28), field(30), field(32));
        let local = get32(b, p + 42).unwrap_or(usize::MAX);
        // Bit 0 is encryption, bit 3 a trailing data descriptor; neither is
        // ours.
        if flags & 0x0009 != 0 || method != 0 || csize != size {
            return bad("an entry that is not stored plainly");
        }
        let name = b.get(p + 46..p + 46 + n).ok_or_else(|| "name out of bounds".to_string())?;
        let name = std::str::from_utf8(name).map_err(|_| "a name that is not UTF-8".to_string())?.to_string();
        p += 46 + n + extra + comment;
        if p > eocd {
            return bad("central directory overruns");
        }
        if entries.iter().any(|e| e.name == name) {
            return Err(format!("{name} twice"));
        }
        if size > MEMBER_MAX {
            return Err(format!("{name} is {size} bytes"));
        }
        total += size;
        if total > BUNDLE_MAX {
            return Err(format!("{total} bytes of entries, over the {BUNDLE_MAX} B ceiling"));
        }
        if get32(b, local) != Some(0x0403_4b50) {
            return bad("broken local header");
        }
        let (ln, lx) = (get16(b, local + 26).unwrap_or(0), get16(b, local + 28).unwrap_or(0));
        if b.get(local + 30..local + 30 + ln) != Some(name.as_bytes()) {
            return bad("local and central names differ");
        }
        let start = local + 30 + ln + lx;
        let end = start
            .checked_add(size)
            .filter(|&end| end <= cd_off)
            .ok_or_else(|| "entry data out of bounds".to_string())?;
        entries.push(Entry { name, crc, local, start, end });
    }
    // One after another, as `zip` writes them, in whatever order the
    // directory lists them. An entry whose header sits inside another's data
    // is the same bytes counted twice.
    let mut order: Vec<&Entry> = entries.iter().collect();
    order.sort_by_key(|e| e.local);
    if order.windows(2).any(|w| w[1].local < w[0].end) {
        return bad("entries that overlap");
    }

    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let data = &b[e.start..e.end];
        if crc32(data) != e.crc {
            return Err(format!("{}: CRC mismatch", e.name));
        }
        out.push((e.name, data.to_vec()));
    }
    Ok(out)
}

// ---- bundles ----------------------------------------------------------------

/// Keep only what a `YYYY-MM-DD HH:MM` stamp is made of, so a hand-edited
/// manifest cannot put arbitrary bytes into a guest popup.
fn clean_stamp(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | ' ')).take(16).collect()
}

fn characters_of(files: &[(usize, Vec<u8>)]) -> usize {
    files.iter().find(|(i, _)| *i == KICKASS).and_then(|(_, b)| characters_in(b)).unwrap_or(0)
}

/// Assemble a bundle from member files.
fn build(files: &[(usize, Vec<u8>)], exported: &str) -> Vec<u8> {
    let manifest = format!(
        "{MAGIC_LINE}\nexported {}\ncharacters {}\n",
        clean_stamp(exported),
        characters_of(files)
    );
    let mut entries: Vec<(&str, &[u8])> = vec![(MANIFEST, manifest.as_bytes())];
    entries.extend(files.iter().map(|(i, b)| (MEMBERS[*i], b.as_slice())));
    zip(&entries)
}

/// Read a file, telling "not there" (`Ok(None)`) from "cannot be read".
fn read_if_there(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Whether `path` exists, telling "no" from "cannot tell".
fn exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Remove a file; one that is already gone is removed.
fn remove(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The members on disk under `storage`, in [`MEMBERS`] order. A member that
/// is not there is left out; one that is there and **cannot be read** is an
/// error, never a gap — a gap would read as "this phone has no chest", and
/// both the export and the pre-image of an apply would then carry that.
fn present(storage: &Path) -> Result<Vec<(usize, Vec<u8>)>, String> {
    let mut out = Vec::new();
    for (i, m) in MEMBERS.iter().enumerate() {
        if let Some(b) = read_if_there(&member_path(storage, m))? {
            out.push((i, b));
        }
    }
    Ok(out)
}

/// What [`snapshot`] made.
pub struct Snapshot {
    pub bytes: Vec<u8>,
    /// [`check_container`] on the `kickass` it carries and [`check_chest`] on
    /// its chest: the character count, or why the receiving phone would
    /// refuse it.
    pub check: Result<usize, String>,
}

/// A bundle of what is on disk now, for 세이브 내보내기. `Ok(None)` when there
/// is no save at all; `Err` when a file of the save is there but cannot be
/// read, in which case nothing is exported.
pub fn snapshot(storage: &Path, exported: &str) -> Result<Option<Snapshot>, String> {
    let files = present(storage)?;
    let Some((_, kickass)) = files.iter().find(|(i, _)| *i == KICKASS) else {
        return Ok(None);
    };
    let check = check_container(kickass).and_then(|n| check_chest(&files).map(|()| n));
    Ok(Some(Snapshot { bytes: build(&files, exported), check }))
}

/// Read a bundle. Integrity only — the zip, the manifest, the member list.
/// What the container holds is [`parse_import`]'s question: a vault
/// generation or a journal's pre-image is whatever was on disk, and putting it
/// back must not depend on it passing anything.
pub fn parse(bytes: &[u8]) -> Result<Bundle, Reject> {
    let no = |why: String| Reject::NotBundle(why);
    if bytes.len() > BUNDLE_MAX {
        return Err(no(format!("{} bytes, over the {BUNDLE_MAX} B ceiling", bytes.len())));
    }
    let entries = unzip(bytes).map_err(no)?;
    let manifest = entries
        .iter()
        .find(|(n, _)| n == MANIFEST)
        .ok_or_else(|| no("no manifest".to_string()))?;
    let manifest = std::str::from_utf8(&manifest.1).map_err(|_| no("manifest is not text".to_string()))?;
    let mut lines = manifest.lines();
    if lines.next().map(str::trim) != Some(MAGIC_LINE) {
        return Err(no("not a chronos save bundle (or a newer format)".to_string()));
    }
    let mut exported = String::new();
    for l in lines {
        if let Some(("exported", v)) = l.split_once(' ') {
            exported = clean_stamp(v.trim());
        }
    }
    // `unzip` has already refused a name twice and an entry over the member
    // ceiling, before it copied anything.
    let mut files: Vec<(usize, Vec<u8>)> = Vec::new();
    for (name, data) in entries {
        if name == MANIFEST {
            continue;
        }
        let i = MEMBERS
            .iter()
            .position(|m| *m == name)
            .ok_or_else(|| no(format!("unexpected entry {name:?}")))?;
        files.push((i, data));
    }
    let characters = characters_of(&files);
    Ok(Bundle { exported, files, characters })
}

/// A bundle offered from outside, for 세이브 불러오기: [`parse`], and then its
/// container must be one the guest reads record by record
/// ([`check_container`]) and must hold a character — a bundle with none would
/// only replace every character on this phone with nothing.
pub fn parse_import(bytes: &[u8]) -> Result<Bundle, Reject> {
    let b = parse(bytes)?;
    let Some((_, kickass)) = b.files.iter().find(|(i, _)| *i == KICKASS) else {
        return Err(Reject::NoCharacter);
    };
    let characters = check_container(kickass).map_err(|why| Reject::NotBundle(format!("kickass: {why}")))?;
    check_chest(&b.files).map_err(Reject::NotBundle)?;
    match characters {
        0 => Err(Reject::NoCharacter),
        _ => Ok(b),
    }
}

/// The chest a bundle carries, if it carries one, must be one
/// [`crate::vault::Vault::open`] reads. One it would not is a chest the next
/// start finds broken — locked, every move refused, until someone repairs it
/// by hand — so it is refused here, before anything is staged. (Before
/// `vault::unhex` read byte-wise, some such files also closed the app at
/// every start, which put 세이브 되돌리기 out of reach.)
fn check_chest(files: &[(usize, Vec<u8>)]) -> Result<(), String> {
    match files.iter().find(|(i, _)| MEMBERS[*i] == crate::vault::FILE) {
        Some((_, b)) => crate::vault::check(b).map_err(|why| format!("{}: {why}", crate::vault::FILE)),
        None => Ok(()),
    }
}

// ---- files --------------------------------------------------------------------

/// Make the names in a directory durable — a rename into it, a removal from
/// it. A file's own `sync_all` does not: after a power cut a removed name can
/// come back, and the journal is a name. A failure is an error, never
/// skipped; a filesystem that cannot sync a directory at all says EINVAL, and
/// that one is let through, as is a directory that is not there (a first start
/// has no names to keep). Unix only — Windows (the probe) has no handle to
/// sync a directory with, and NTFS journals its names.
fn sync_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
        let err = |e: std::io::Error| format!("{} (syncing the directory): {e}", dir.display());
        let d = match std::fs::File::open(dir) {
            Ok(d) => d,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(err(e)),
        };
        match d.sync_all() {
            Err(e) if e.kind() != ErrorKind::InvalidInput => return Err(err(e)),
            _ => {}
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Temp + fsync + rename + directory sync, so an interrupted write leaves the
/// previous file and a finished one survives a power cut.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new(""));
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".xfer");
    let tmp = PathBuf::from(tmp);
    let err = |e: std::io::Error| format!("{}: {e}", tmp.display());
    let mut f = std::fs::File::create(&tmp).map_err(err)?;
    f.write_all(bytes).map_err(err)?;
    f.sync_all().map_err(err)?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    sync_dir(dir)
}

/// Make the members on disk exactly a bundle's: its own files written, the
/// ones it does not have removed. Stops at the first failure. Ends with the
/// directory synced, so the removals are as durable as the writes before the
/// journal that guards them goes.
fn apply(storage: &Path, files: &[(usize, Vec<u8>)]) -> Result<(usize, usize), String> {
    let (mut written, mut removed) = (0, 0);
    for (i, m) in MEMBERS.iter().enumerate() {
        let path = member_path(storage, m);
        match files.iter().find(|(j, _)| *j == i) {
            Some((_, data)) => {
                write_atomically(&path, data)?;
                written += 1;
            }
            None => {
                if exists(&path)? {
                    remove(&path)?;
                    removed += 1;
                }
            }
        }
    }
    sync_dir(storage)?;
    Ok((written, removed))
}

/// Remove a file and make the removal durable, so a power cut cannot bring it
/// back. What [`stage`] writes is a promise to apply it at the next start:
/// wherever the game goes on without keeping that promise, the stage must be
/// gone from the **disk**, or the next start applies it over everything played
/// in between.
fn remove_durably(path: &Path) -> Result<(), String> {
    remove(path)?;
    sync_dir(path.parent().unwrap_or(Path::new("")))
}

/// Why [`stage`] did not stage.
#[derive(Debug, PartialEq, Eq)]
pub enum StageFailed {
    /// Nothing is staged, on the disk: the game may go on.
    Cancelled(String),
    /// A stage may be on the disk and could not be surely taken back. The next
    /// start would apply it over anything played from here, so play stops —
    /// which is what the player asked for when they said 예.
    Unsure(String),
}

/// Accept a bundle for the next start.
///
/// A write that fails may still have renamed the stage into place — a failed
/// directory sync comes after the rename — so a failure takes the stage back,
/// durably, before it lets the game go on.
pub fn stage(storage: &Path, bytes: &[u8]) -> Result<(), StageFailed> {
    let path = storage.join(PENDING);
    let Err(why) = write_atomically(&path, bytes) else {
        return Ok(());
    };
    match remove_durably(&path) {
        Ok(()) => Err(StageFailed::Cancelled(why)),
        Err(e) => Err(StageFailed::Unsure(format!("{why}; and taking the stage back: {e}"))),
    }
}

// ---- the vault ------------------------------------------------------------------

/// A generation's time, if `name` is one. Everything else in the directory is
/// ignored, never deleted.
pub fn parse_gen(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".cssave")?;
    (stem.len() == 14 && stem.bytes().all(|c| c.is_ascii_digit())).then(|| stem.parse().ok()).flatten()
}

/// The vault's generations, oldest first.
pub fn vault_list(storage: &Path) -> Vec<(u64, PathBuf)> {
    let dir = storage.join(VAULT_DIR);
    let mut v: Vec<(u64, PathBuf)> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| parse_gen(&e.file_name().to_string_lossy()).map(|ms| (ms, e.path())))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn newest_vault(storage: &Path) -> Option<(u64, PathBuf)> {
    vault_list(storage).pop()
}

/// Keep `files` — what is on disk now, already read — as a vault generation.
/// `Ok(None)` when there is nothing to keep, or when it is already the newest
/// generation.
fn vault_keep(storage: &Path, files: &[(usize, Vec<u8>)], now_ms: u64) -> Result<Option<PathBuf>, String> {
    if files.is_empty() {
        return Ok(None);
    }
    let newest = newest_vault(storage);
    if let Some((_, path)) = &newest {
        if let Ok(b) = std::fs::read(path).map_err(|_| ()).and_then(|b| parse(&b).map_err(|_| ())) {
            if b.files == files {
                return Ok(None);
            }
        }
    }
    // Monotonic even if the clock went back, so the newest by name is the
    // newest by fact.
    let ms = newest.map_or(now_ms, |(t, _)| now_ms.max(t + 1));
    let path = storage.join(VAULT_DIR).join(format!("{ms:014}.{EXT}"));
    write_atomically(&path, &build(files, ""))?;
    Ok(Some(path))
}

/// Remove all but the newest [`VAULT_KEEP`] generations.
fn vault_rotate(storage: &Path) -> Vec<String> {
    let list = vault_list(storage);
    let extra = list.len().saturating_sub(VAULT_KEEP);
    list.into_iter()
        .take(extra)
        .map(|(_, p)| match std::fs::remove_file(&p) {
            Ok(()) => format!("savexfer: vault rotated out {}", p.display()),
            Err(e) => format!("savexfer: cannot rotate out {}: {e}", p.display()),
        })
        .collect()
}

// ---- the start ------------------------------------------------------------------

/// Host wall-clock milliseconds, for the vault's names at a start, before any
/// guest clock exists.
pub fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What a start did about a staged save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing was staged and nothing was left over.
    Idle,
    /// The staged save is what is on disk now.
    Applied,
    /// The staged save was not applied, and what is on disk is the save from
    /// before — never touched, or written back. The shell tells the player.
    NotApplied,
    /// The files on disk may be a mix of two saves, and neither could be
    /// finished. **The game must not start**: the journal is kept, and the
    /// next start tries again.
    Blocked,
}

/// [`apply_pending`]'s answer: the outcome, and one line per step for the log.
pub struct Report {
    pub outcome: Outcome,
    pub log: Vec<String>,
}

impl Report {
    /// Whether the game may start on what is on disk.
    pub fn may_start(&self) -> bool {
        self.outcome != Outcome::Blocked
    }
}

/// Read and parse a journal file. `Ok(None)` when it is not there.
fn read_bundle(path: &Path) -> Result<Option<Bundle>, String> {
    match read_if_there(path)? {
        None => Ok(None),
        Some(b) => parse(&b).map(Some).map_err(|e| format!("{}: {e:?}", path.display())),
    }
}

/// Take the journal down after the disk became one whole save. `APPLYING`
/// first: once it is gone nothing will be applied again. If it cannot go, the
/// start is blocked — a journal beside a running game would be applied again
/// at the next start, over everything played in between.
///
/// "Gone" means gone on the disk, not in the page cache: a removal that a
/// power cut takes back is that same journal, found at the next start beside
/// everything played since. So the directory is synced before the game may
/// start, and a sync that fails blocks it like a removal that fails.
fn close_journal(storage: &Path, done: Outcome, log: &mut Vec<String>) -> Outcome {
    if let Err(e) = remove_durably(&storage.join(APPLYING)) {
        log.push(format!("savexfer: cannot clear the journal ({e}); not starting the game on it"));
        return Outcome::Blocked;
    }
    if let Err(e) = remove(&storage.join(BEFORE)) {
        // Harmless with `APPLYING` gone: the next start sweeps it.
        log.push(format!("savexfer: cannot remove the pre-image ({e}); the next start sweeps it"));
    }
    if done == Outcome::Applied {
        log.extend(vault_rotate(storage));
    }
    done
}

/// With the journal committed: make the disk one whole save — `target` if it
/// can be finished, otherwise `before` — and close the journal; or report the
/// start blocked with the journal kept.
fn settle(storage: &Path, target: Option<&Bundle>, before: Option<&Bundle>, log: &mut Vec<String>) -> Outcome {
    if let Some(t) = target {
        match apply(storage, &t.files) {
            Ok((w, d)) => {
                log.push(format!(
                    "savexfer: applied {} — {} character(s), {w} file(s) written, {d} removed",
                    if t.exported.is_empty() {
                        "a vault generation".to_string()
                    } else {
                        format!("the save exported {:?}", t.exported)
                    },
                    t.characters,
                ));
                return close_journal(storage, Outcome::Applied, log);
            }
            Err(e) => log.push(format!("savexfer: the apply stopped part-way ({e}); putting back the save from before")),
        }
    }
    let Some(b) = before else {
        log.push("savexfer: there is no pre-image to put back; not starting the game on a mixed save".to_string());
        return Outcome::Blocked;
    };
    match apply(storage, &b.files) {
        Ok(_) => {
            log.push("savexfer: the save from before is back; nothing changed".to_string());
            close_journal(storage, Outcome::NotApplied, log)
        }
        Err(e) => {
            log.push(format!(
                "savexfer: the save from before could not be put back either ({e}); \
                 not starting the game on a mixed save — the next start tries again"
            ));
            Outcome::Blocked
        }
    }
}

/// Apply a staged save, or finish one an earlier start left unfinished. The
/// shell calls this before it reads any file of the player's, and the probe
/// at the same point. See the module docs for the protocol and why the game
/// must not start on [`Outcome::Blocked`].
pub fn apply_pending(storage: &Path, now_ms: u64) -> Report {
    let mut log = Vec::new();
    let done = |outcome, log| Report { outcome, log };
    // Nothing of the last run's document screens is alive at a start: its
    // copies died with its process.
    for e in std::fs::read_dir(storage).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if is_doc_file(&name) && std::fs::remove_file(e.path()).is_ok() {
            log.push(format!("savexfer: removed a stale {name}"));
        }
    }
    // Whatever names the last run changed are made durable before anything
    // here reads them. A run that removed a stage or a journal and could not
    // sync the removal blocked its start; without this, the next start in the
    // same boot would see the removal in the page cache, start the game, and a
    // power cut later could bring the file back — to be applied over what was
    // played. With it, "the next start retries" is true.
    if let Err(e) = sync_dir(storage) {
        log.push(format!("savexfer: cannot make the save directory durable ({e}); not starting the game"));
        return done(Outcome::Blocked, log);
    }
    let (pending, applying, before) = (storage.join(PENDING), storage.join(APPLYING), storage.join(BEFORE));

    // An apply that did not finish: interrupted, or failed and not put back.
    match exists(&applying) {
        Err(e) => {
            log.push(format!("savexfer: cannot tell whether an apply is unfinished ({e}); not starting the game"));
            return done(Outcome::Blocked, log);
        }
        Ok(true) => {
            log.push("savexfer: an earlier apply did not finish; finishing it".to_string());
            let target = read_bundle(&applying).unwrap_or_else(|e| {
                log.push(format!("savexfer: the journal cannot be read ({e})"));
                None
            });
            let back = read_bundle(&before).unwrap_or_else(|e| {
                log.push(format!("savexfer: the pre-image cannot be read ({e})"));
                None
            });
            let outcome = settle(storage, target.as_ref(), back.as_ref(), &mut log);
            return done(outcome, log);
        }
        Ok(false) => {}
    }

    let bytes = match read_if_there(&pending) {
        Ok(Some(b)) => Ok(b),
        Ok(None) => {
            // A pre-image with no journal is what an apply leaves when it stops
            // between its two removals, or before its commit: nothing refers
            // to it.
            if exists(&before).unwrap_or(false) && remove(&before).is_ok() {
                log.push("savexfer: removed a pre-image no journal refers to".to_string());
            }
            return done(Outcome::Idle, log);
        }
        Err(e) => Err(format!("savexfer: the staged save cannot be read ({e}); nothing changed")),
    };
    // Giving up starts the game on the save from before, so the stage must be
    // gone from the disk first — a read that failed once may work at the next
    // start, and would then apply the stage over what is played in between.
    // If it cannot surely go, the start is blocked and the next one retries.
    let abandon = |why: String, mut log: Vec<String>| {
        log.push(why);
        let _ = remove(&before);
        match remove_durably(&pending) {
            Ok(()) => done(Outcome::NotApplied, log),
            Err(e) => {
                log.push(format!(
                    "savexfer: cannot take the staged save back ({e}); not starting the game, \
                     since the next start would apply it over what is played"
                ));
                done(Outcome::Blocked, log)
            }
        }
    };
    let bytes = match bytes {
        Ok(b) => b,
        Err(why) => return abandon(why, log),
    };
    let target = match parse(&bytes) {
        Ok(b) => b,
        Err(e) => return abandon(format!("savexfer: the staged save is unusable ({e:?}); nothing changed"), log),
    };
    // No destructive write without its pre-image — and a file that cannot be
    // read has no pre-image.
    let current = match present(storage) {
        Ok(c) => c,
        Err(e) => {
            return abandon(format!("savexfer: cannot read the save it would replace ({e}); nothing changed"), log)
        }
    };
    match vault_keep(storage, &current, now_ms) {
        Ok(Some(p)) => log.push(format!("savexfer: the save it replaces is in {}", p.display())),
        Ok(None) => log.push("savexfer: the save it replaces is already the newest in the vault".to_string()),
        Err(e) => return abandon(format!("savexfer: cannot keep the current save ({e}); nothing changed"), log),
    }
    if let Err(e) = write_atomically(&before, &build(&current, "")) {
        return abandon(format!("savexfer: cannot write the pre-image ({e}); nothing changed"), log);
    }
    if let Err(e) = std::fs::rename(&pending, &applying) {
        return abandon(format!("savexfer: cannot commit the apply ({e}); nothing changed"), log);
    }
    let back = Bundle { exported: String::new(), characters: characters_of(&current), files: current };
    let outcome = settle(storage, Some(&target), Some(&back), &mut log);
    done(outcome, log)
}

// ---- time -------------------------------------------------------------------------

/// `YYYY-MM-DD HH:MM` for unix milliseconds at a UTC offset in minutes.
pub fn local_stamp(ms: u64, offset_min: i32) -> String {
    let secs = (ms / 1000) as i64 + offset_min as i64 * 60;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cs_savebundle_{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// One table entry.
    fn entry(name: &str, cap: u16, size: u16) -> [u8; CONTAINER_ENTRY] {
        let mut e = [0u8; CONTAINER_ENTRY];
        e[..name.len()].copy_from_slice(name.as_bytes());
        e[16..18].copy_from_slice(&cap.to_le_bytes());
        e[18..20].copy_from_slice(&size.to_le_bytes());
        e
    }

    /// A container laid out the way the guest writes one: the magic, a
    /// 40-entry table of `entries`, the bodies at their capacities.
    fn container_of(entries: &[(&str, u16, u16)], fill: u8) -> Vec<u8> {
        let mut v = CONTAINER_MAGIC.to_vec();
        for (name, cap, size) in entries {
            v.extend_from_slice(&entry(name, *cap, *size));
        }
        v.resize(CONTAINER_MAGIC.len() + ENTRIES_MAGIC * CONTAINER_ENTRY, 0);
        for (_, cap, _) in entries {
            v.extend(std::iter::repeat(fill).take(*cap as usize));
        }
        v
    }

    /// `AGREE`, `OPTIONDATA`, then `HERODATA00n`/`GAMEDATA00n` per character,
    /// at the sizes the game writes them.
    pub(crate) fn container(chars: usize, fill: u8) -> Vec<u8> {
        let names: Vec<(String, u16, u16)> = (1..=chars)
            .flat_map(|n| [(format!("HERODATA{n:03}"), 6400, 0x1807), (format!("GAMEDATA{n:03}"), 3072, 0xc00)])
            .collect();
        let mut entries = vec![("AGREE", 256, 1), ("OPTIONDATA", 256, 20)];
        entries.extend(names.iter().map(|(n, c, s)| (n.as_str(), *c, *s)));
        container_of(&entries, fill)
    }

    fn write(storage: &Path, name: &str, bytes: &[u8]) {
        let p = member_path(storage, name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn read(storage: &Path, name: &str) -> Option<Vec<u8>> {
        std::fs::read(member_path(storage, name)).ok()
    }

    /// A directory where `write_atomically` wants its temp file: the write of
    /// `name` fails, while reading `name` still works. Portable, unlike
    /// permissions.
    fn block_writes_to(storage: &Path, name: &str) {
        std::fs::create_dir_all(storage.join(format!("{name}.xfer")).join("obstacle")).unwrap();
    }

    fn unblock_writes_to(storage: &Path, name: &str) {
        std::fs::remove_dir_all(storage.join(format!("{name}.xfer"))).unwrap();
    }

    fn journal(storage: &Path) -> (bool, bool) {
        (storage.join(APPLYING).exists(), storage.join(BEFORE).exists())
    }

    #[test]
    fn crc32_matches_the_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn zip_round_trips_and_rejects_damage() {
        let z = zip(&[("a.txt", b"hello"), ("b", &[0u8, 1, 2, 255])]);
        let e = unzip(&z).unwrap();
        assert_eq!(e, vec![("a.txt".to_string(), b"hello".to_vec()), ("b".to_string(), vec![0, 1, 2, 255])]);
        // One flipped byte in an entry's data is a CRC mismatch.
        let mut bad = z.clone();
        bad[30 + 5] ^= 1;
        assert!(unzip(&bad).unwrap_err().contains("CRC"));
        // Truncation loses the end record.
        assert!(unzip(&z[..z.len() - 1]).is_err());
        assert!(unzip(b"").is_err());
        assert!(unzip(&[0u8; 100]).is_err());
    }

    /// One local entry, and a directory that names it `count` times.
    fn named_over_and_over(data: &[u8], count: usize) -> Vec<u8> {
        let mut out = Vec::new();
        local_entry(&mut out, "kickass", data);
        let mut central = Vec::new();
        for _ in 0..count {
            central_entry(&mut central, "kickass", data, 0);
        }
        let (off, len) = (out.len() as u32, central.len() as u32);
        out.extend_from_slice(&central);
        end_record(&mut out, count as u16, len, off);
        out
    }

    /// The second review's finding: a directory that names one entry
    /// thousands of times. Copied as it was read, 64 KB of data in a 170 KB
    /// file became 125 MB of allocation. Now it is refused from the directory
    /// alone, before a byte is copied or checksummed — and so is every smaller
    /// way of counting the same data twice.
    #[test]
    fn a_directory_is_checked_whole_before_anything_is_copied() {
        let data = vec![7u8; 64 * 1024];
        let many = named_over_and_over(&data, 2000);
        assert!(many.len() < 200 * 1024, "{} bytes", many.len());
        let why = unzip(&many).unwrap_err();
        assert!(why.contains("at most"), "{why}");
        assert!(matches!(parse(&many), Err(Reject::NotBundle(_))));
        // Within the entry count: the same name twice.
        assert!(unzip(&named_over_and_over(&data, 2)).unwrap_err().contains("twice"));
        assert_eq!(unzip(&named_over_and_over(&data, 1)).unwrap().len(), 1, "once is a zip");

        // A second entry whose header sits inside the first one's data: two
        // names, the same bytes.
        let inner_data = b"chest".as_slice();
        let mut inner = Vec::new();
        local_entry(&mut inner, crate::vault::FILE, inner_data);
        let mut outer_data = vec![0u8; 16];
        let inner_at = 30 + "kickass".len() + outer_data.len();
        outer_data.extend_from_slice(&inner);
        let mut z = Vec::new();
        local_entry(&mut z, "kickass", &outer_data);
        let mut central = Vec::new();
        central_entry(&mut central, "kickass", &outer_data, 0);
        central_entry(&mut central, crate::vault::FILE, inner_data, inner_at as u32);
        let (off, len) = (z.len() as u32, central.len() as u32);
        z.extend_from_slice(&central);
        end_record(&mut z, 2, len, off);
        assert!(unzip(&z).unwrap_err().contains("overlap"));

        // An entry over the member ceiling is refused by its declared size.
        let big = zip(&[("kickass", &vec![0u8; MEMBER_MAX + 1])]);
        assert!(unzip(&big).unwrap_err().contains("bytes"));

        // The directory's order is not the data's: listed backwards is fine,
        // as long as nothing overlaps.
        let mut z = Vec::new();
        local_entry(&mut z, MANIFEST, b"m");
        let second = z.len() as u32;
        local_entry(&mut z, "kickass", b"k");
        let mut central = Vec::new();
        central_entry(&mut central, "kickass", b"k", second);
        central_entry(&mut central, MANIFEST, b"m", 0);
        let (off, len) = (z.len() as u32, central.len() as u32);
        z.extend_from_slice(&central);
        end_record(&mut z, 2, len, off);
        assert_eq!(unzip(&z).unwrap().len(), 2);
    }

    /// The synthetic container has the sizes the probe's real ones have — the
    /// shape the check was derived from.
    #[test]
    fn the_synthetic_container_is_the_size_the_guest_writes() {
        assert_eq!(container(0, 0).len(), 4 + 800 + 256 + 256);
        assert_eq!(container(1, 0).len() - container(0, 0).len(), 6400 + 3072);
        assert_eq!(check_container(&container(0, 0)), Ok(0));
        assert_eq!(check_container(&container(3, 7)), Ok(3));
    }

    /// The probe's own containers, as the guest wrote them, all pass — the
    /// check is the guest's layout, not a guess at it. Skipped when no probe
    /// output is lying around.
    #[test]
    fn every_container_the_probe_has_written_passes() {
        // Every `out*` directory at the repository root: the probe's default
        // output and the A/B runs' named ones.
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut seen = 0;
        let mut stack: Vec<PathBuf> = std::fs::read_dir(&repo)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_name().to_string_lossy().starts_with("out"))
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.file_name().is_some_and(|n| n == "kickass") {
                    let b = std::fs::read(&p).unwrap();
                    assert!(check_container(&b).is_ok(), "{}: {:?}", p.display(), check_container(&b));
                    seen += 1;
                }
            }
        }
        eprintln!("{seen} probe containers checked");
    }

    /// Everything the review found, and its neighbours: a table that names a
    /// hero with nothing behind it (the 804-byte file), and every other way a
    /// record can point outside the file or be something the guest never
    /// writes.
    #[test]
    fn a_container_is_checked_record_by_record() {
        // The review's file: a table naming HERODATA001 and no body at all.
        let mut header_only = CONTAINER_MAGIC.to_vec();
        header_only.extend_from_slice(&entry("HERODATA001", 6400, 0x1807));
        header_only.resize(804, 0);
        assert_eq!(header_only.len(), 804);
        let why = check_container(&header_only).unwrap_err();
        assert!(why.contains("runs past the end"), "{why}");
        let z = zip(&[(MANIFEST, format!("{MAGIC_LINE}\n").as_bytes()), ("kickass", &header_only)]);
        assert!(matches!(parse_import(&z), Err(Reject::NotBundle(_))), "the review's bundle is refused");

        // A body cut short by one byte.
        let full = container(1, 7);
        assert!(check_container(&full[..full.len() - 1]).unwrap_err().contains("runs past the end"));
        // More data than room.
        assert!(check_container(&container_of(&[("AGREE", 256, 257)], 0)).unwrap_err().contains("bytes in a"));
        // A capacity that is not whole blocks.
        assert!(check_container(&container_of(&[("AGREE", 200, 1)], 0)).unwrap_err().contains("whole blocks"));
        // A hero record no loader reads.
        let odd = container_of(&[("HERODATA001", 6400, 0x1000)], 0);
        assert!(check_container(&odd).unwrap_err().contains("hero record"));
        assert_eq!(check_container(&container_of(&[("HERODATA001", 0x2500, 0x2407)], 0)), Ok(1), "ten bags");
        // A deleted hero (size 0) is not a character, and its room still
        // counts toward where the next body starts.
        let deleted = container_of(&[("HERODATA001", 6400, 0), ("GAMEDATA001", 3072, 0xc00)], 0);
        assert_eq!(check_container(&deleted), Ok(0));
        assert!(check_container(&deleted[..deleted.len() - 1]).is_err());
        // The same name twice.
        let twice = container_of(&[("AGREE", 256, 1), ("AGREE", 256, 1)], 0);
        assert!(check_container(&twice).unwrap_err().contains("twice"));
        // Names: printable, then NUL to the end of the field.
        let mut garbled = container(1, 7);
        garbled[4] = 0x01;
        assert!(check_container(&garbled).is_err());
        let mut tail = container(1, 7);
        tail[4 + 12] = b'x'; // after `AGREE\0`
        assert!(check_container(&tail).is_err());
        // An entry with no name must be all zero.
        let mut ghost = container(0, 0);
        ghost[4 + 2 * CONTAINER_ENTRY + 16] = 1;
        assert!(check_container(&ghost).unwrap_err().contains("no name"));
        // Strangers.
        assert!(check_container(b"PK\x03\x04").is_err());
        assert!(check_container(&container(1, 7)[..800]).is_err(), "shorter than the table");
        // The older layout the guest still reads: no magic, twenty entries.
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&entry("HERODATA001", 6400, 0x1807));
        legacy.resize(ENTRIES_LEGACY * CONTAINER_ENTRY, 0);
        legacy.resize(legacy.len() + 6400, 9);
        assert_eq!(check_container(&legacy), Ok(1));
    }

    /// `HERO_RECORD_SIZES` is what the game writes, read off the game: the
    /// shipped hero save clears a 0x1807-byte record (`mov r2,#0x1800` /
    /// `add r2,r2,#7` before its `memset`), and the cave's ten-bag save is
    /// `HERO_SAVE_LEN`.
    #[test]
    fn the_hero_record_sizes_are_the_ones_the_game_writes() {
        let vm = crate::patch::tests::loaded_guest();
        let f = vm.symbol("GameFrame_save_hero_data").unwrap();
        assert_eq!(vm.mem.peek32(f + 0x64), 0xe3a0_2b06, "mov r2, #0x1800");
        assert_eq!(vm.mem.peek32(f + 0x68), 0xe282_2007, "add r2, r2, #7");
        let cave = include_str!("../../cave/src/bag10.c");
        assert!(cave.contains("#define HERO_SAVE_LEN 0x2407"));
        assert_eq!(HERO_RECORD_SIZES, [0x1807, 0x2407]);
    }

    #[test]
    fn a_snapshot_parses_back_to_the_same_files() {
        let s = dir("snapshot");
        assert!(snapshot(&s, "2026-10-28 14:30").unwrap().is_none(), "no kickass, no save");
        write(&s, "kickass", &container(2, 9));
        write(&s, crate::vault::FILE, b"cs_vault 1\nnext 1\n");
        let snap = snapshot(&s, "2026-10-28 14:30").unwrap().unwrap();
        assert_eq!(snap.check, Ok(2));
        let b = parse_import(&snap.bytes).unwrap();
        assert_eq!(b.exported, "2026-10-28 14:30");
        assert_eq!(b.characters, 2);
        assert_eq!(b.files.len(), 2);
        assert_eq!(b.files[0].1, container(2, 9));
    }

    /// A file that is there but cannot be read is not a file that is not
    /// there: the export refuses rather than leave the chest out, and an
    /// apply refuses rather than replace it with no copy kept.
    #[test]
    fn a_file_that_cannot_be_read_stops_the_export_and_the_apply() {
        let src = dir("unreadable_src");
        write(&src, "kickass", &container(2, 2));
        let bundle = snapshot(&src, "2026-10-28 14:30").unwrap().unwrap().bytes;

        let s = dir("unreadable");
        write(&s, "kickass", &container(1, 1));
        // A chest that exists and cannot be read: a directory in its place.
        std::fs::create_dir_all(s.join(crate::vault::FILE).join("x")).unwrap();
        assert!(snapshot(&s, "").is_err(), "the export does not go out without the chest");

        stage(&s, &bundle).unwrap();
        let r = apply_pending(&s, 1);
        assert_eq!(r.outcome, Outcome::NotApplied, "{:?}", r.log);
        assert!(r.log.iter().any(|l| l.contains("cannot read the save it would replace")), "{:?}", r.log);
        assert_eq!(read(&s, "kickass"), Some(container(1, 1)), "nothing was replaced");
        assert_eq!(journal(&s), (false, false));
        assert!(!s.join(PENDING).exists(), "the request is dropped, not retried at every start");
        assert!(vault_list(&s).is_empty(), "no partial pre-image was kept");
    }

    #[test]
    fn parse_refuses_what_it_did_not_write() {
        let manifest = format!("{MAGIC_LINE}\n");
        let k = container(1, 1);
        // An entry not on the list.
        let z = zip(&[(MANIFEST, manifest.as_bytes()), ("../evil", b"x")]);
        assert!(matches!(parse(&z), Err(Reject::NotBundle(_))));
        // A member twice.
        let z = zip(&[(MANIFEST, manifest.as_bytes()), ("kickass", &k), ("kickass", &k)]);
        assert!(matches!(parse(&z), Err(Reject::NotBundle(_))));
        // No manifest, or a different format.
        assert!(matches!(parse(&zip(&[("kickass", &k)])), Err(Reject::NotBundle(_))));
        let z = zip(&[(MANIFEST, b"chronos save bundle 2\n"), ("kickass", &k)]);
        assert!(matches!(parse(&z), Err(Reject::NotBundle(_))));
        let z = zip(&[(MANIFEST, b"hl5 save bundle 1\n"), ("kickass", &k)]);
        assert!(matches!(parse(&z), Err(Reject::NotBundle(_))), "the sister port's bundle is not ours");
        // A kickass that is not a container: our own pre-images carry what was
        // on disk whatever it is, so integrity passes; an import does not.
        let z = zip(&[(MANIFEST, manifest.as_bytes()), ("kickass", b"not a save")]);
        assert!(parse(&z).is_ok());
        assert!(matches!(parse_import(&z), Err(Reject::NotBundle(_))));
        // A sound bundle with no character: integrity passes, policy refuses.
        let z = zip(&[(MANIFEST, manifest.as_bytes()), ("kickass", &container(0, 1))]);
        assert!(parse(&z).is_ok());
        assert_eq!(parse_import(&z).unwrap_err(), Reject::NoCharacter);
        // No kickass at all.
        let z = zip(&[(MANIFEST, manifest.as_bytes()), (crate::vault::FILE, b"x")]);
        assert_eq!(parse_import(&z).unwrap_err(), Reject::NoCharacter);
        // A stamp cannot smuggle bytes into a popup.
        let m = format!("{MAGIC_LINE}\nexported 2026-10-28 14:30^\x01;evil\n");
        let z = zip(&[(MANIFEST, m.as_bytes()), ("kickass", &k)]);
        assert_eq!(parse(&z).unwrap().exported, "2026-10-28 14:30");
    }

    /// The chest a bundle carries is the receiving phone's chest from the next
    /// start on, so one the vault cannot read is refused before anything is
    /// staged — and the export does not make one either.
    #[test]
    fn an_import_whose_chest_the_vault_cannot_read_is_refused() {
        let manifest = format!("{MAGIC_LINE}\n");
        let k = container(1, 1);
        let rec = "0".repeat(crate::vault::REC * 2);
        let good = format!("cs_vault 1\nnext 2\nitem 1 {rec}\n");
        let z = zip(&[(MANIFEST, manifest.as_bytes()), ("kickass", &k), (crate::vault::FILE, good.as_bytes())]);
        assert_eq!(parse_import(&z).unwrap().characters, 1);
        // The one that panicked `vault::unhex`: a two-byte character across a
        // hex pair, at a record's byte length.
        let straddle = format!("cs_vault 1\nnext 2\nitem 1 a\u{e9}{}\n", &rec[3..]);
        let bads: [&[u8]; 3] = [straddle.as_bytes(), b"chest", b"\xff\xfe"];
        for bad in bads {
            let z = zip(&[(MANIFEST, manifest.as_bytes()), ("kickass", &k), (crate::vault::FILE, bad)]);
            assert!(parse(&z).is_ok(), "integrity passes: a pre-image carries whatever was on disk");
            match parse_import(&z) {
                Err(Reject::NotBundle(why)) => assert!(why.contains(crate::vault::FILE), "{why}"),
                other => panic!("{bad:?} was not refused: {other:?}"),
            }
        }
        let s = dir("broken_chest_export");
        write(&s, "kickass", &k);
        write(&s, crate::vault::FILE, straddle.as_bytes());
        assert!(snapshot(&s, "").unwrap().unwrap().check.is_err(), "the export would make what the import refuses");
    }

    /// The whole import: the old save into the vault, the new files in, the
    /// ones the bundle does not have removed, no journal left.
    #[test]
    fn a_staged_import_is_applied_at_the_next_start_with_its_pre_image_kept() {
        let src = dir("import_src");
        write(&src, "kickass", &container(2, 2));
        write(&src, crate::potions::FILE, b"{}");
        let bundle = snapshot(&src, "2026-10-28 14:30").unwrap().unwrap().bytes;

        let dst = dir("import_dst");
        write(&dst, "kickass", &container(1, 1));
        write(&dst, crate::vault::FILE, b"dst chest");
        stage(&dst, &bundle).unwrap();
        let r = apply_pending(&dst, 1_000);
        assert_eq!(r.outcome, Outcome::Applied, "{:?}", r.log);
        assert!(r.may_start());

        assert_eq!(read(&dst, "kickass"), Some(container(2, 2)));
        assert_eq!(read(&dst, crate::potions::FILE).as_deref(), Some(&b"{}"[..]));
        assert_eq!(read(&dst, crate::vault::FILE), None, "the source had no chest");
        assert!(!dst.join(PENDING).exists());
        assert_eq!(journal(&dst), (false, false));

        // The pre-image is the newest generation and restores exactly.
        let (ms, gen) = newest_vault(&dst).unwrap();
        assert_eq!(ms, 1_000);
        let before = std::fs::read(&gen).unwrap();
        assert!(parse(&before).is_ok());
        stage(&dst, &before).unwrap();
        assert_eq!(apply_pending(&dst, 2_000).outcome, Outcome::Applied);
        assert_eq!(read(&dst, "kickass"), Some(container(1, 1)));
        assert_eq!(read(&dst, crate::vault::FILE).as_deref(), Some(&b"dst chest"[..]));
        assert_eq!(read(&dst, crate::potions::FILE), None);
        // And the restore kept what it replaced: pressing it again swaps back.
        let (ms2, _) = newest_vault(&dst).unwrap();
        assert_eq!(ms2, 2_000);
    }

    /// The review's first finding: the container is replaced, the next file
    /// cannot be written. The save from before goes back, the journal goes,
    /// and the game may start on a disk that is one whole save.
    #[test]
    fn a_failed_apply_puts_the_save_from_before_back() {
        let src = dir("rollback_src");
        write(&src, "kickass", &container(2, 2));
        write(&src, crate::vault::FILE, b"src chest");
        let bundle = snapshot(&src, "2026-10-28 14:30").unwrap().unwrap().bytes;

        let s = dir("rollback");
        write(&s, "kickass", &container(1, 1));
        block_writes_to(&s, crate::vault::FILE);
        stage(&s, &bundle).unwrap();
        let r = apply_pending(&s, 1);
        assert_eq!(r.outcome, Outcome::NotApplied, "{:?}", r.log);
        assert!(r.log.iter().any(|l| l.contains("stopped part-way")), "{:?}", r.log);
        assert!(r.may_start());
        assert_eq!(read(&s, "kickass"), Some(container(1, 1)), "the character from before is back");
        assert_eq!(read(&s, crate::vault::FILE), None, "and no chest, as before");
        assert_eq!(journal(&s), (false, false));
    }

    /// When the save from before cannot be put back either, the start is
    /// blocked and the journal stays — at every start, until one of the two
    /// directions can be finished. Then the player's import lands whole.
    #[test]
    fn an_apply_that_cannot_be_finished_or_undone_blocks_the_start_until_it_can() {
        let src = dir("blocked_src");
        write(&src, "kickass", &container(2, 2));
        write(&src, crate::vault::FILE, b"src chest");
        let bundle = snapshot(&src, "2026-10-28 14:30").unwrap().unwrap().bytes;

        let s = dir("blocked");
        write(&s, "kickass", &container(1, 1));
        write(&s, crate::vault::FILE, b"dst chest");
        block_writes_to(&s, crate::vault::FILE);
        stage(&s, &bundle).unwrap();
        let r = apply_pending(&s, 1);
        assert_eq!(r.outcome, Outcome::Blocked, "{:?}", r.log);
        assert!(!r.may_start());
        assert_eq!(journal(&s), (true, true), "both halves of the journal are kept");

        // The next start, the obstacle still there: blocked again, nothing lost.
        let r = apply_pending(&s, 2);
        assert_eq!(r.outcome, Outcome::Blocked, "{:?}", r.log);
        assert!(r.log.iter().any(|l| l.contains("did not finish")), "{:?}", r.log);
        assert_eq!(journal(&s), (true, true));

        // The obstacle gone: the start after finishes what the player asked.
        unblock_writes_to(&s, crate::vault::FILE);
        let r = apply_pending(&s, 3);
        assert_eq!(r.outcome, Outcome::Applied, "{:?}", r.log);
        assert_eq!(read(&s, "kickass"), Some(container(2, 2)));
        assert_eq!(read(&s, crate::vault::FILE).as_deref(), Some(&b"src chest"[..]));
        assert_eq!(journal(&s), (false, false));
        // And what it replaced is in the vault, whole.
        let (_, gen) = newest_vault(&s).unwrap();
        let kept = parse(&std::fs::read(gen).unwrap()).unwrap();
        assert_eq!(kept.files, vec![(0, container(1, 1)), (1, b"dst chest".to_vec())]);
    }

    /// A start that died while writing (the journal is there, the files are
    /// whatever they are) finishes the apply at the next start.
    #[test]
    fn an_interrupted_apply_is_finished_at_the_next_start() {
        let src = dir("interrupted_src");
        write(&src, "kickass", &container(3, 3));
        let bundle = snapshot(&src, "").unwrap().unwrap().bytes;
        let s = dir("interrupted");
        write(&s, "kickass", &container(1, 1));
        let pre = snapshot(&s, "").unwrap().unwrap().bytes;
        std::fs::write(s.join(APPLYING), &bundle).unwrap();
        std::fs::write(s.join(BEFORE), &pre).unwrap();
        let r = apply_pending(&s, 1);
        assert_eq!(r.outcome, Outcome::Applied, "{:?}", r.log);
        assert_eq!(read(&s, "kickass"), Some(container(3, 3)));
        assert_eq!(journal(&s), (false, false));

        // A journal that cannot be read is put back from the pre-image.
        std::fs::write(s.join(APPLYING), b"torn").unwrap();
        std::fs::write(s.join(BEFORE), &pre).unwrap();
        let r = apply_pending(&s, 2);
        assert_eq!(r.outcome, Outcome::NotApplied, "{:?}", r.log);
        assert_eq!(read(&s, "kickass"), Some(container(1, 1)));
        assert_eq!(journal(&s), (false, false));

        // With neither readable there is no way to know what the disk is.
        std::fs::write(s.join(APPLYING), b"torn").unwrap();
        let r = apply_pending(&s, 3);
        assert_eq!(r.outcome, Outcome::Blocked, "{:?}", r.log);
    }

    #[test]
    fn an_unusable_stage_changes_nothing_and_is_not_retried() {
        let s = dir("bad_stage");
        write(&s, "kickass", &container(1, 5));
        std::fs::write(s.join(PENDING), b"garbage").unwrap();
        let r = apply_pending(&s, 1);
        assert_eq!(r.outcome, Outcome::NotApplied);
        assert!(r.log.iter().any(|l| l.contains("unusable")), "{:?}", r.log);
        assert_eq!(read(&s, "kickass"), Some(container(1, 5)));
        assert!(!s.join(PENDING).exists(), "consumed, so it cannot loop");
        assert!(vault_list(&s).is_empty());
        assert_eq!(apply_pending(&s, 2).outcome, Outcome::Idle);
    }

    /// A pre-image with no journal — what a start leaves when it stops between
    /// removing the two — is swept, and the start is idle.
    /// What a failed `stage` leaves: surely nothing — or it says it cannot be
    /// sure, and the menu stops play.
    #[test]
    fn a_failed_stage_is_taken_back_or_reported_unsure() {
        let s = dir("stage_fail");
        // The temp file cannot be made: nothing reaches the stage's name.
        block_writes_to(&s, PENDING);
        assert!(matches!(stage(&s, b"x"), Err(StageFailed::Cancelled(_))));
        assert!(!s.join(PENDING).exists());
        unblock_writes_to(&s, PENDING);
        // The stage's name holds something that can be neither replaced nor
        // removed: whether a stage is there cannot be settled.
        std::fs::create_dir_all(s.join(PENDING).join("x")).unwrap();
        assert!(matches!(stage(&s, b"x"), Err(StageFailed::Unsure(_))));
    }

    /// Giving up on a stage starts the game on the save from before, so the
    /// stage must surely be gone first. One that cannot be taken back blocks
    /// the start — every start, until it can.
    #[test]
    fn a_stage_that_cannot_be_taken_back_blocks_the_start() {
        let s = dir("stage_stuck");
        write(&s, "kickass", &container(1, 5));
        // A stage that is there and can be neither read nor removed.
        std::fs::create_dir_all(s.join(PENDING).join("x")).unwrap();
        for at in 1..=2 {
            let r = apply_pending(&s, at);
            assert_eq!(r.outcome, Outcome::Blocked, "{:?}", r.log);
            assert!(r.log.iter().any(|l| l.contains("cannot take the staged save back")), "{:?}", r.log);
        }
        assert_eq!(read(&s, "kickass"), Some(container(1, 5)), "nothing was replaced");
        assert_eq!(journal(&s), (false, false));
        std::fs::remove_dir_all(s.join(PENDING)).unwrap();
        assert_eq!(apply_pending(&s, 3).outcome, Outcome::Idle);
    }

    #[test]
    fn a_pre_image_no_journal_refers_to_is_swept() {
        let s = dir("stale_before");
        write(&s, "kickass", &container(1, 5));
        std::fs::write(s.join(BEFORE), b"left behind").unwrap();
        let r = apply_pending(&s, 1);
        assert_eq!(r.outcome, Outcome::Idle);
        assert_eq!(journal(&s), (false, false));
        assert_eq!(read(&s, "kickass"), Some(container(1, 5)));
    }

    #[test]
    fn the_vault_keeps_the_newest_five_and_ignores_strangers() {
        let s = dir("vault");
        for n in 0..8u8 {
            write(&s, "kickass", &container(1, n));
            vault_keep(&s, &present(&s).unwrap(), 100 + n as u64).unwrap();
            vault_rotate(&s);
        }
        std::fs::write(s.join(VAULT_DIR).join("notes.txt"), b"mine").unwrap();
        let list = vault_list(&s);
        assert_eq!(list.iter().map(|(ms, _)| *ms).collect::<Vec<_>>(), vec![103, 104, 105, 106, 107]);
        assert!(s.join(VAULT_DIR).join("notes.txt").exists(), "never deleted, never listed");
        // The same files twice is one generation.
        assert_eq!(vault_keep(&s, &present(&s).unwrap(), 999).unwrap(), None);
        // A clock that went backwards still sorts after the newest.
        write(&s, "kickass", &container(1, 99));
        let p = vault_keep(&s, &present(&s).unwrap(), 5).unwrap().unwrap();
        assert_eq!(parse_gen(&p.file_name().unwrap().to_string_lossy()), Some(108));
    }

    #[test]
    fn generation_names_parse_strictly() {
        assert_eq!(parse_gen("01761400000000.cssave"), Some(1_761_400_000_000));
        assert_eq!(parse_gen("1761400000000.cssave"), None, "13 digits");
        assert_eq!(parse_gen("0176140000000x.cssave"), None);
        assert_eq!(parse_gen("01761400000000.zip"), None);
        assert_eq!(parse_gen("01761400000000.h5save"), None, "the sister port's suffix");
    }

    #[test]
    fn stamps_are_local_civil_time() {
        // 2026-10-25 05:30 UTC.
        let ms = 1_792_906_200_000;
        assert_eq!(local_stamp(ms, 0), "2026-10-25 05:30");
        assert_eq!(local_stamp(ms, 540), "2026-10-25 14:30");
        assert_eq!(local_stamp(ms, -360), "2026-10-24 23:30");
        assert_eq!(local_stamp(0, 0), "1970-01-01 00:00");
        assert_eq!(local_stamp(951_782_400_000, 0), "2000-02-29 00:00");
    }
}
