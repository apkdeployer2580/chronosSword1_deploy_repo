//! File descriptors and `FILE*` handles for the guest.
//!
//! Two kinds of descriptor exist. Real ones are save files under the storage
//! root the host hands us. The other kind is the VFS: on the original app the
//! Java side opened `assets/data.vfs.mp3` as an `AssetFileDescriptor` and passed
//! the raw fd plus an offset and length down to `nativeInitVFS`, and the guest
//! then `dup`s it and reads it with stdio. We keep that blob in memory and hand
//! out a synthetic descriptor for it, so the guest's code path is unchanged
//! while the host is free to have the data come from an APK asset, a file, or
//! anywhere else.
//!
//! Every path the guest opens is resolved under the storage root and rejected if
//! it escapes — the guest builds its own paths from strings inside a 2011 game
//! binary and nothing else validates them.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::net::Sock;

/// `MIDASKernelManager::hash` — djb2, transcribed from the guest.
///
/// ```text
///   h = 5381; for c in name: h = h * 33 + c
/// ```
pub fn vfs_hash(name: &[u8]) -> u32 {
    name.iter().fold(5381u32, |h, &c| h.wrapping_mul(33).wrapping_add(c as u32))
}

/// Index of `data.vfs.mp3`: name hash -> (offset of the record's data relative
/// to the start of the archive, length).
///
/// The archive is a flat run of `[u32 hash][u32 size][size bytes]`, and the
/// guest finds a record by **scanning it from the beginning every single time**,
/// reading the two header words with a separate 4-byte `fread` each. With 2194
/// records that is up to 4388 stdio calls per lookup, and this game looks assets
/// up constantly — the title artwork alone streams in as a few dozen tiles.
/// Measured over a run to the main menu: **875 942** four-byte reads, all of
/// them re-walking the same directory.
///
/// Building the map once turns each lookup into a hash probe. The answers are
/// identical by construction: same hash function, same record order, first
/// match wins.
pub struct VfsIndex {
    map: std::collections::HashMap<u32, (u32, u32)>,
}

impl VfsIndex {
    pub fn build(vfs: &[u8], start: u64, len: u64) -> Self {
        let mut map = std::collections::HashMap::new();
        let end = ((start + len) as usize).min(vfs.len());
        let mut off = start as usize;
        // `cursor` mirrors the guest's running counter: it counts from the start
        // of the archive and is already past the 8-byte header when a record
        // matches, which is exactly what the guest stores through `outOffset`.
        let mut cursor: u32 = 0;
        while off + 8 <= end {
            let hash = u32::from_le_bytes(vfs[off..off + 4].try_into().unwrap());
            let size = u32::from_le_bytes(vfs[off + 4..off + 8].try_into().unwrap());
            if size == 0 || off + 8 + size as usize > end {
                break;
            }
            cursor += 8;
            map.entry(hash).or_insert((cursor, size));
            cursor += size;
            off += 8 + size as usize;
        }
        Self { map }
    }

    pub fn get(&self, hash: u32) -> Option<(u32, u32)> {
        self.map.get(&hash).copied()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

pub const O_ACCMODE: i32 = 3;
pub const O_RDONLY: i32 = 0;
pub const O_WRONLY: i32 = 1;
pub const O_RDWR: i32 = 2;
pub const O_CREAT: i32 = 0o100; // 0x40 — Linux value, which is what the guest passes
pub const O_TRUNC: i32 = 0o1000; // 0x200
pub const O_APPEND: i32 = 0o2000; // 0x400

/// First descriptor we hand out. Well clear of 0/1/2 so a guest bug that writes
/// to "stdout" cannot land on a real file.
const FD_BASE: i32 = 64;

/// Guest address space reserved for `FILE*` handles. The guest never
/// dereferences them — it only passes them back to `fread`/`fseek`/`fclose` —
/// so an opaque cookie is enough.
pub const FILE_COOKIE_BASE: u32 = 0xF11E_0000;

enum Backing {
    Host(File),
    Vfs,
    /// A TCP socket. Sockets live in the same table as files because the guest
    /// passes them to the same `read`/`write`/`close`, and because
    /// `MC_netSocket` refuses any descriptor >= 1024 (the `fd_set` limit), so
    /// there has to be exactly one number space to keep them low.
    Sock(Sock),
}

struct Entry {
    backing: Backing,
    pos: u64,
    /// For VFS descriptors: the window into the blob, as the Java side reported
    /// it via `AssetFileDescriptor.getStartOffset()/getLength()`.
    vfs_start: u64,
    vfs_len: u64,
}

pub struct Files {
    root: PathBuf,
    vfs: Arc<Vec<u8>>,
    open: HashMap<i32, Entry>,
    files: HashMap<u32, i32>,
    next_cookie: u32,
    pub errno: i32,
}

impl Files {
    pub fn new(root: PathBuf, vfs: Arc<Vec<u8>>) -> Self {
        let _ = std::fs::create_dir_all(&root);
        Self {
            root,
            vfs,
            open: HashMap::new(),
            files: HashMap::new(),
            next_cookie: FILE_COOKIE_BASE,
            errno: 0,
        }
    }

    /// Lowest free descriptor, POSIX-style.
    ///
    /// A monotonically increasing counter would be simpler and wrong: this game
    /// opens two descriptors for every asset it loads (`dup` + `fdopen`) and
    /// closes them again, so a counter passes 1024 during normal play — and
    /// `MC_netSocket` rejects any socket descriptor at or above that, which
    /// would break networking after a while of playing rather than at startup.
    fn alloc_fd(&self) -> i32 {
        let mut fd = FD_BASE;
        while self.open.contains_key(&fd) {
            fd += 1;
        }
        fd
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn vfs_len(&self) -> u64 {
        self.vfs.len() as u64
    }

    /// Descriptor for the whole asset blob, as `nativeInitVFS` would have got
    /// from `AssetFileDescriptor.getFileDescriptor()`.
    pub fn open_vfs(&mut self, start: u64, len: u64) -> i32 {
        let fd = self.alloc_fd();
        self.open
            .insert(fd, Entry { backing: Backing::Vfs, pos: 0, vfs_start: start, vfs_len: len });
        fd
    }

    // ---- sockets --------------------------------------------------------

    pub fn open_socket(&mut self) -> i32 {
        let fd = self.alloc_fd();
        self.open.insert(
            fd,
            Entry { backing: Backing::Sock(Sock::default()), pos: 0, vfs_start: 0, vfs_len: 0 },
        );
        fd
    }

    pub fn sock_mut(&mut self, fd: i32) -> Option<&mut Sock> {
        match self.open.get_mut(&fd) {
            Some(Entry { backing: Backing::Sock(s), .. }) => Some(s),
            _ => None,
        }
    }

    pub fn is_socket(&self, fd: i32) -> bool {
        matches!(self.open.get(&fd), Some(Entry { backing: Backing::Sock(_), .. }))
    }

    /// Resolve a guest path under the storage root. Returns `None` for anything
    /// that would escape it.
    pub fn resolve(&self, guest_path: &str) -> Option<PathBuf> {
        let p = guest_path.replace('\\', "/");
        let rel = match p.strip_prefix(&*self.root.to_string_lossy().replace('\\', "/")) {
            Some(r) => r.trim_start_matches('/').to_string(),
            None => p.trim_start_matches('/').to_string(),
        };
        let mut out = self.root.clone();
        for c in Path::new(&rel).components() {
            match c {
                Component::Normal(s) => out.push(s),
                Component::CurDir => {}
                // `..`, absolute roots and prefixes are all refusals rather than
                // something to normalise away: none of them can appear in a path
                // this game legitimately builds.
                _ => return None,
            }
        }
        Some(out)
    }

    pub fn open_path(&mut self, guest_path: &str, flags: i32, _mode: i32) -> i32 {
        let Some(path) = self.resolve(guest_path) else {
            self.errno = 13; // EACCES
            return -1;
        };
        let acc = flags & O_ACCMODE;
        let mut o = OpenOptions::new();
        o.read(acc == O_RDONLY || acc == O_RDWR);
        o.write(acc == O_WRONLY || acc == O_RDWR);
        if flags & O_CREAT != 0 {
            o.create(true);
        }
        if flags & O_TRUNC != 0 {
            o.truncate(true);
        }
        if flags & O_APPEND != 0 {
            o.append(true);
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match o.open(&path) {
            Ok(f) => {
                let fd = self.alloc_fd();
                self.open.insert(
                    fd,
                    Entry { backing: Backing::Host(f), pos: 0, vfs_start: 0, vfs_len: 0 },
                );
                fd
            }
            Err(e) => {
                self.errno = errno_of(&e);
                -1
            }
        }
    }

    pub fn dup(&mut self, fd: i32) -> i32 {
        let Some(e) = self.open.get(&fd) else {
            self.errno = 9; // EBADF
            return -1;
        };
        match &e.backing {
            Backing::Vfs => {
                let (s, l, pos) = (e.vfs_start, e.vfs_len, e.pos);
                let nfd = self.alloc_fd();
                self.open
                    .insert(nfd, Entry { backing: Backing::Vfs, pos, vfs_start: s, vfs_len: l });
                nfd
            }
            Backing::Host(f) => match f.try_clone() {
                Ok(c) => {
                    let pos = e.pos;
                    let nfd = self.alloc_fd();
                    self.open.insert(
                        nfd,
                        Entry { backing: Backing::Host(c), pos, vfs_start: 0, vfs_len: 0 },
                    );
                    nfd
                }
                Err(e) => {
                    self.errno = errno_of(&e);
                    -1
                }
            },
            // Duplicating a socket is not something the guest does.
            Backing::Sock(_) => {
                self.errno = 9;
                -1
            }
        }
    }

    pub fn close(&mut self, fd: i32) -> i32 {
        if let Some(Entry { backing: Backing::Sock(s), .. }) = self.open.get_mut(&fd) {
            s.shutdown();
        }
        if self.open.remove(&fd).is_some() {
            self.files.retain(|_, v| *v != fd);
            0
        } else {
            self.errno = 9;
            -1
        }
    }

    pub fn read(&mut self, fd: i32, buf: &mut [u8]) -> i64 {
        let Some(e) = self.open.get_mut(&fd) else {
            self.errno = 9;
            return -1;
        };
        match &mut e.backing {
            Backing::Vfs => {
                let start = (e.vfs_start + e.pos) as usize;
                let end = (e.vfs_start + e.vfs_len).min(self.vfs.len() as u64) as usize;
                if start >= end {
                    return 0;
                }
                let n = buf.len().min(end - start);
                buf[..n].copy_from_slice(&self.vfs[start..start + n]);
                e.pos += n as u64;
                n as i64
            }
            Backing::Host(f) => {
                if f.seek(SeekFrom::Start(e.pos)).is_err() {
                    self.errno = 5;
                    return -1;
                }
                match f.read(buf) {
                    Ok(n) => {
                        e.pos += n as u64;
                        n as i64
                    }
                    Err(err) => {
                        self.errno = errno_of(&err);
                        -1
                    }
                }
            }
            Backing::Sock(s) => match s.recv(buf) {
                Ok(n) => n as i64,
                Err(errno) => {
                    self.errno = errno;
                    -1
                }
            },
        }
    }

    pub fn write(&mut self, fd: i32, buf: &[u8]) -> i64 {
        let Some(e) = self.open.get_mut(&fd) else {
            self.errno = 9;
            return -1;
        };
        match &mut e.backing {
            Backing::Vfs => {
                self.errno = 9; // read-only asset
                -1
            }
            Backing::Host(f) => {
                if f.seek(SeekFrom::Start(e.pos)).is_err() {
                    self.errno = 5;
                    return -1;
                }
                match f.write(buf) {
                    Ok(n) => {
                        e.pos += n as u64;
                        n as i64
                    }
                    Err(err) => {
                        self.errno = errno_of(&err);
                        -1
                    }
                }
            }
            Backing::Sock(s) => match s.send(buf) {
                Ok(n) => n as i64,
                Err(errno) => {
                    self.errno = errno;
                    -1
                }
            },
        }
    }

    pub fn seek(&mut self, fd: i32, off: i64, whence: i32) -> i64 {
        let Some(e) = self.open.get_mut(&fd) else {
            self.errno = 9;
            return -1;
        };
        let end = match &e.backing {
            Backing::Vfs => e.vfs_len,
            Backing::Host(f) => f.metadata().map(|m| m.len()).unwrap_or(0),
            Backing::Sock(_) => {
                self.errno = 29; // ESPIPE
                return -1;
            }
        };
        let base = match whence {
            0 => 0i64,
            1 => e.pos as i64,
            2 => end as i64,
            _ => {
                self.errno = 22; // EINVAL
                return -1;
            }
        };
        let np = base + off;
        if np < 0 {
            self.errno = 22;
            return -1;
        }
        e.pos = np as u64;
        np
    }

    pub fn tell(&self, fd: i32) -> i64 {
        self.open.get(&fd).map(|e| e.pos as i64).unwrap_or(-1)
    }

    // ---- FILE* cookies --------------------------------------------------

    pub fn fdopen(&mut self, fd: i32) -> u32 {
        if !self.open.contains_key(&fd) {
            return 0;
        }
        let cookie = self.next_cookie;
        self.next_cookie += 0x40;
        self.files.insert(cookie, fd);
        cookie
    }

    pub fn file_fd(&self, cookie: u32) -> Option<i32> {
        self.files.get(&cookie).copied()
    }

    pub fn fclose(&mut self, cookie: u32) -> i32 {
        match self.files.remove(&cookie) {
            Some(fd) => self.close(fd),
            None => -1,
        }
    }
}

pub fn errno_of(e: &std::io::Error) -> i32 {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => 2,          // ENOENT
        PermissionDenied => 13, // EACCES
        AlreadyExists => 17,    // EEXIST
        _ => 5,                 // EIO
    }
}
