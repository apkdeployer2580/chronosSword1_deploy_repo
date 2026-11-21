//! High-level emulation of everything outside the guest binary.
//!
//! Three surfaces, all discovered by reading the shipped `.so` rather than
//! guessed at:
//!
//! 1. **79 imported symbols** — libc, `ceil`/`floor`/`pow`, and 19 GLES 1.x
//!    entry points. Each gets a one-word trampoline in the HLE window and the
//!    loader points its `R_ARM_JUMP_SLOT` there, so a guest `bl` through the PLT
//!    lands on an address the run loop recognises before it ever fetches an
//!    instruction from it.
//! 2. **The JNI function table** — the guest calls exactly twenty `JNIEnv` slots
//!    and one `JavaVM` slot. Unused slots still get a trampoline, which aborts
//!    with the slot name if it is ever reached, so an unexpected call is a clear
//!    error rather than a jump to zero.
//! 3. **`co.kr.eamobile.CletEntry`'s static methods** — the ~50 callbacks the
//!    original Java layer provided, re-implemented here.

use crate::audio::Bus;
use crate::fmt::VaList;
use crate::gl;
use crate::mem::{HLE_BASE, JNI_BASE};
use crate::{Phase, Vm, WIPI_H, WIPI_W};
use arm32_cpu::{reg, Memory, Mode};

// ---- trampoline address space -------------------------------------------

const TRAMP_IMPORT: usize = 0;
const TRAMP_JNIENV: usize = 256;
const TRAMP_JAVAVM: usize = 512;
const TRAMP_COUNT: usize = 528;

/// Guest word holding `__stack_chk_guard`. It is a data import, not a function,
/// so it must resolve to storage rather than to a trampoline.
const STACK_GUARD_ADDR: u32 = HLE_BASE + 0x8000;

pub fn tramp_addr(idx: usize) -> u32 {
    HLE_BASE + (TRAMP_IMPORT + idx) as u32 * 4
}

pub fn tramp_index(pc: u32) -> Option<usize> {
    if pc < HLE_BASE {
        return None;
    }
    let off = (pc - HLE_BASE) / 4;
    ((pc - HLE_BASE) % 4 == 0 && (off as usize) < TRAMP_COUNT).then_some(off as usize)
}

/// Imports that are objects rather than functions.
pub fn data_import_addr() -> impl Fn(&str) -> Option<u32> {
    |name: &str| match name {
        "__stack_chk_guard" => Some(STACK_GUARD_ADDR),
        _ => None,
    }
}

// ---- JNI bookkeeping -----------------------------------------------------

#[derive(Clone, Debug)]
pub enum JObject {
    Class(String),
    /// A Java string, kept as **raw bytes**. The guest round-trips its own
    /// text through `NewStringUTF`/`GetStringUTFChars`, and that text is
    /// EUC-KR: decoding it into a Rust `String` on the way in would hand
    /// different bytes back on the way out (see `mem::cstr_bytes`).
    Str(Vec<u8>),
    ByteArray(Vec<u8>),
    FileDescriptor(i32),
    Plain,
}

const HANDLE_BASE: u32 = 0x4A00_0000;
const METHOD_BASE: u32 = 0x4D00_0000;

#[derive(Default)]
pub struct JniState {
    pub env_ptr: u32,
    pub vm_ptr: u32,
    pub this_obj: u32,
    pub objects: Vec<JObject>,
    /// (name, signature) for every `GetStaticMethodID` the guest asked for.
    pub methods: Vec<(String, String)>,
    /// Guest allocations handed out by `GetStringUTFChars` /
    /// `GetByteArrayElements`, so `Release*` can free them and copy back.
    pub pinned: Vec<(u32, u32)>,
    /// The asset name <-> id registry `MidasAssetManager` kept in two
    /// `Hashtable`s. Ids are handed out in first-seen order starting at 1 — not
    /// derived from the name — and the guest depends on the round trip:
    /// `CUSTOM_knlGetResourceID` turns a name into an id, and
    /// `loadAssetFromVFS` turns that id straight back into the name it hashes
    /// to find the record in data.vfs.mp3. Keys are raw bytes for the same
    /// reason `JObject::Str` is.
    pub asset_ids: std::collections::HashMap<Vec<u8>, i32>,
    pub asset_names: std::collections::HashMap<i32, Vec<u8>>,
}

impl JniState {
    fn handle(&mut self, o: JObject) -> u32 {
        self.objects.push(o);
        HANDLE_BASE + (self.objects.len() - 1) as u32
    }
    pub fn new_string(&mut self, s: &str) -> u32 {
        self.handle(JObject::Str(s.as_bytes().to_vec()))
    }
    pub fn new_string_bytes(&mut self, b: Vec<u8>) -> u32 {
        self.handle(JObject::Str(b))
    }
    pub fn new_file_descriptor(&mut self, fd: i32) -> u32 {
        self.handle(JObject::FileDescriptor(fd))
    }
    pub fn get(&self, h: u32) -> Option<&JObject> {
        if h < HANDLE_BASE {
            return None;
        }
        self.objects.get((h - HANDLE_BASE) as usize)
    }
    fn get_mut(&mut self, h: u32) -> Option<&mut JObject> {
        if h < HANDLE_BASE {
            return None;
        }
        self.objects.get_mut((h - HANDLE_BASE) as usize)
    }
}

/// Names for the `JNIEnv` slots the guest reaches, used both to dispatch and to
/// name a slot in the error for one we did not expect.
fn jni_slot_name(idx: usize) -> &'static str {
    match idx {
        4 => "GetVersion",
        6 => "FindClass",
        21 => "NewGlobalRef",
        22 => "DeleteGlobalRef",
        23 => "DeleteLocalRef",
        94 => "GetFieldID",
        100 => "GetIntField",
        113 => "GetStaticMethodID",
        115 => "CallStaticObjectMethodV",
        130 => "CallStaticIntMethodV",
        133 => "CallStaticLongMethodV",
        142 => "CallStaticVoidMethodV",
        167 => "NewStringUTF",
        169 => "GetStringUTFChars",
        170 => "ReleaseStringUTFChars",
        176 => "NewByteArray",
        184 => "GetByteArrayElements",
        192 => "ReleaseByteArrayElements",
        208 => "SetByteArrayRegion",
        219 => "GetJavaVM",
        _ => "",
    }
}

impl Vm {
    pub(crate) fn build_jni(&mut self) -> Result<(), String> {
        // JNIEnv: a pointer to a 233-entry function table.
        let env = JNI_BASE;
        let env_table = JNI_BASE + 8;
        self.mem.write_bytes(env, &env_table.to_le_bytes());
        for i in 0..233usize {
            let t = HLE_BASE + (TRAMP_JNIENV + i) as u32 * 4;
            self.mem.write_bytes(env_table + (i * 4) as u32, &t.to_le_bytes());
        }

        // JavaVM: same shape, 8 entries.
        let vm = JNI_BASE + 0x1000;
        let vm_table = JNI_BASE + 0x1008;
        self.mem.write_bytes(vm, &vm_table.to_le_bytes());
        for i in 0..8usize {
            let t = HLE_BASE + (TRAMP_JAVAVM + i) as u32 * 4;
            self.mem.write_bytes(vm_table + (i * 4) as u32, &t.to_le_bytes());
        }

        self.jni.env_ptr = env;
        self.jni.vm_ptr = vm;
        self.jni.objects.push(JObject::Plain); // handle 0 is never valid
        self.jni.this_obj = self.jni.handle(JObject::Plain);
        Ok(())
    }

    // ---- argument plumbing ----------------------------------------------

    fn arg(&mut self, i: usize) -> u32 {
        if i < 4 {
            self.cpu.reg_get(Mode::User, i as u8)
        } else {
            let sp = self.cpu.reg_get(Mode::User, reg::SP);
            self.mem.r32(sp + ((i - 4) * 4) as u32)
        }
    }

    fn argf(&mut self, i: usize) -> f32 {
        f32::from_bits(self.arg(i))
    }

    /// A 64-bit argument occupying slots `i` and `i+1`. AAPCS aligns these to an
    /// even slot, which every call site here already satisfies.
    fn arg64(&mut self, i: usize) -> u64 {
        let lo = self.arg(i) as u64;
        let hi = self.arg(i + 1) as u64;
        (hi << 32) | lo
    }

    fn argd(&mut self, i: usize) -> f64 {
        f64::from_bits(self.arg64(i))
    }

    fn ret(&mut self, v: u32) {
        self.cpu.reg_set(Mode::User, 0, v);
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
    }

    fn ret64(&mut self, v: u64) {
        self.cpu.reg_set(Mode::User, 1, (v >> 32) as u32);
        self.ret(v as u32);
    }

    fn retd(&mut self, v: f64) {
        self.ret64(v.to_bits());
    }

    fn ret_void(&mut self) {
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
    }

    fn read_bytes(&self, addr: u32, len: u32) -> Vec<u8> {
        self.mem.slice(addr, len).map(|s| s.to_vec()).unwrap_or_default()
    }

    /// Copy the varargs of a C-style variadic call into a contiguous guest
    /// buffer so the formatter can walk it as a plain `va_list`. Needed for
    /// `__android_log_print`, where the first vararg is still in r3.
    fn gather_varargs(&mut self, first_reg: usize) -> u32 {
        const WORDS: u32 = 48;
        let dst = self.scratch;
        let mut n = 0u32;
        for i in first_reg..4 {
            let v = self.arg(i);
            self.mem.write_bytes(dst + n * 4, &v.to_le_bytes());
            n += 1;
        }
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        for k in 0..(WORDS - n) {
            let v = self.mem.r32(sp + k * 4);
            self.mem.write_bytes(dst + (n + k) * 4, &v.to_le_bytes());
        }
        dst
    }

    /// Format into raw bytes. Never through `String` — see `fmt.rs`.
    fn format_va(&mut self, fmt_ptr: u32, va: u32) -> Vec<u8> {
        let f = self.mem.cstr_bytes(fmt_ptr);
        // The closures need read access while `va.ptr` advances; borrow the
        // memory once instead of borrowing `self` twice.
        let mem = &self.mem;
        let read_u32 = |a: u32| {
            mem.slice(a, 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0)
        };
        let cstr = |a: u32| mem.cstr_bytes(a);
        let mut list = VaList { ptr: va, read_u32: &read_u32, cstr: &cstr };
        crate::fmt::vsnprintf(&f, &mut list)
    }

    fn logline(&mut self, s: String) {
        log::info!("[guest] {s}");
        if self.log.len() < 4096 {
            self.log.push(s);
        }
    }

    // ---- dispatch --------------------------------------------------------

    pub(crate) fn dispatch(&mut self, id: usize) -> Result<(), String> {
        if id >= TRAMP_JAVAVM {
            return self.javavm(id - TRAMP_JAVAVM);
        }
        if id >= TRAMP_JNIENV {
            return self.jnienv(id - TRAMP_JNIENV);
        }
        let name = match self.imports.get(id) {
            Some(n) => n.clone(),
            None => return Err(format!("call to unknown import trampoline #{id}")),
        };
        if self.trace {
            // Cheap enough to leave in: `trace` is off unless CS_TRACE is set,
            // and the branch is one predictable load per import call, not per
            // guest instruction.
            let a: Vec<String> = (0..4).map(|i| format!("{:#x}", self.arg(i))).collect();
            let lr = self.cpu.reg_get(Mode::User, reg::LR);
            log::debug!("-> {name}({}) from {}", a.join(", "), self.describe(lr));
        }
        let r = self.import(&name);
        if self.trace && r.is_ok() {
            log::debug!("<- {name} = {:#x}", self.cpu.reg_get(Mode::User, 0));
        }
        r
    }

    fn import(&mut self, name: &str) -> Result<(), String> {
        match name {
            // ---- memory ---------------------------------------------------
            "malloc" | "_Znwj" => {
                let n = self.arg(0);
                let p = self.heap.alloc(n).unwrap_or(0);
                self.ret(p);
            }
            "calloc" => {
                let (n, sz) = (self.arg(0), self.arg(1));
                let total = n.saturating_mul(sz);
                let p = self.heap.alloc(total).unwrap_or(0);
                if p != 0 {
                    if let Some(s) = self.mem.slice_mut(p, total) {
                        s.fill(0);
                    }
                }
                self.ret(p);
            }
            "free" | "_ZdlPv" => {
                let p = self.arg(0);
                if p != 0 {
                    self.heap.free(p);
                }
                self.ret_void();
            }
            "memcpy" | "memmove" => {
                let (d, s, n) = (self.arg(0), self.arg(1), self.arg(2));
                let src = self.read_bytes(s, n);
                self.mem.write_bytes(d, &src);
                self.ret(d);
            }
            "memset" => {
                let (d, c, n) = (self.arg(0), self.arg(1), self.arg(2));
                if let Some(s) = self.mem.slice_mut(d, n) {
                    s.fill(c as u8);
                }
                self.ret(d);
            }

            // ---- strings --------------------------------------------------
            "strlen" => {
                let s = self.arg(0);
                let v = self.mem.cstr_len(s);
                self.ret(v);
            }
            "strcmp" | "strncmp" => {
                // C compares as *unsigned char*, which is what comparing the
                // raw bytes gives. Going through UTF-8 would both reorder and
                // merge strings that differ only in their high bytes.
                let (a, b) = (self.arg(0), self.arg(1));
                let (mut x, mut y) = (self.mem.cstr_bytes(a), self.mem.cstr_bytes(b));
                if name == "strncmp" {
                    let n = self.arg(2) as usize;
                    x.truncate(n);
                    y.truncate(n);
                }
                let r = match x.cmp(&y) {
                    std::cmp::Ordering::Less => -1i32,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                };
                self.ret(r as u32);
            }
            "strcpy" => {
                let (d, s) = (self.arg(0), self.arg(1));
                let mut b = self.mem.cstr_bytes(s);
                b.push(0);
                self.mem.write_bytes(d, &b);
                self.ret(d);
            }
            "strncpy" => {
                let (d, s, n) = (self.arg(0), self.arg(1), self.arg(2));
                let v = self.mem.cstr_bytes(s);
                let mut b = vec![0u8; n as usize];
                let k = v.len().min(n as usize);
                b[..k].copy_from_slice(&v[..k]);
                self.mem.write_bytes(d, &b);
                self.ret(d);
            }
            "strcat" => {
                let (d, s) = (self.arg(0), self.arg(1));
                let mut b = self.mem.cstr_bytes(d);
                b.extend_from_slice(&self.mem.cstr_bytes(s));
                b.push(0);
                self.mem.write_bytes(d, &b);
                self.ret(d);
            }
            "strstr" => {
                let (h, n) = (self.arg(0), self.arg(1));
                let hay = self.mem.cstr_bytes(h);
                let nee = self.mem.cstr_bytes(n);
                let r = if nee.is_empty() {
                    h
                } else {
                    hay.windows(nee.len())
                        .position(|w| w == nee.as_slice())
                        .map(|i| h + i as u32)
                        .unwrap_or(0)
                };
                self.ret(r);
            }
            "strdup" => {
                let s = self.arg(0);
                let mut b = self.mem.cstr_bytes(s);
                b.push(0);
                let p = self.heap.alloc(b.len() as u32).unwrap_or(0);
                if p != 0 {
                    self.mem.write_bytes(p, &b);
                }
                self.ret(p);
            }
            "strerror" => {
                let e = self.arg(0) as i32;
                let msg = match e {
                    2 => "No such file or directory",
                    5 => "I/O error",
                    9 => "Bad file descriptor",
                    13 => "Permission denied",
                    17 => "File exists",
                    22 => "Invalid argument",
                    _ => "Unknown error",
                };
                let addr = self.scratch + 512;
                let mut b = msg.as_bytes().to_vec();
                b.push(0);
                self.mem.write_bytes(addr, &b);
                self.ret(addr);
            }
            "vsnprintf" => {
                let (buf, size, fmtp, ap) = (self.arg(0), self.arg(1), self.arg(2), self.arg(3));
                let mut b = self.format_va(fmtp, ap);
                let cap = (size as usize).saturating_sub(1);
                b.truncate(cap);
                let n = b.len() as u32;
                b.push(0);
                if buf != 0 {
                    self.mem.write_bytes(buf, &b);
                }
                self.ret(n);
            }
            "qsort" => {
                let (base, nmemb, size, cmp) = (self.arg(0), self.arg(1), self.arg(2), self.arg(3));
                self.qsort(base, nmemb, size, cmp)?;
                self.ret_void();
            }

            // ---- math (soft-float ABI: doubles arrive in register pairs) ----
            "ceil" => {
                let v = self.argd(0);
                self.retd(v.ceil());
            }
            "floor" => {
                let v = self.argd(0);
                self.retd(v.floor());
            }
            "pow" => {
                let (x, y) = (self.argd(0), self.argd(2));
                self.retd(x.powf(y));
            }

            // ---- time -----------------------------------------------------
            "ftime" => {
                // struct timeb { time_t time; unsigned short millitm;
                //                short timezone; short dstflag; }
                let p = self.arg(0);
                let now = self.now_ms();
                self.mem.write_bytes(p, &((now / 1000) as u32).to_le_bytes());
                self.mem.write_bytes(p + 4, &((now % 1000) as u16).to_le_bytes());
                self.mem.write_bytes(p + 6, &0u16.to_le_bytes());
                self.mem.write_bytes(p + 8, &0u16.to_le_bytes());
                self.ret(0);
            }

            // ---- files ----------------------------------------------------
            "open" => {
                let (p, flags, mode) = (self.arg(0), self.arg(1) as i32, self.arg(2) as i32);
                let path = self.mem.cstr(p);
                let fd = self.files.open_path(&path, flags, mode);
                self.ret(fd as u32);
            }
            "close" => {
                let fd = self.arg(0) as i32;
                let r = self.files.close(fd);
                self.ret(r as u32);
            }
            "read" => {
                let (fd, buf, n) = (self.arg(0) as i32, self.arg(1), self.arg(2));
                let mut tmp = vec![0u8; n as usize];
                let got = self.files.read(fd, &mut tmp);
                if got > 0 {
                    self.mem.write_bytes(buf, &tmp[..got as usize]);
                }
                self.ret(got as u32);
            }
            "write" => {
                let (fd, buf, n) = (self.arg(0) as i32, self.arg(1), self.arg(2));
                let data = self.read_bytes(buf, n);
                let got = self.files.write(fd, &data);
                self.ret(got as u32);
            }
            "lseek" => {
                let (fd, off, wh) = (self.arg(0) as i32, self.arg(1) as i32, self.arg(2) as i32);
                let r = self.files.seek(fd, off as i64, wh);
                self.ret(r as u32);
            }
            "dup" => {
                let fd = self.arg(0) as i32;
                let r = self.files.dup(fd);
                self.ret(r as u32);
            }
            "fdopen" => {
                let fd = self.arg(0) as i32;
                let c = self.files.fdopen(fd);
                self.ret(c);
            }
            "fclose" => {
                let c = self.arg(0);
                let r = self.files.fclose(c);
                self.ret(r as u32);
            }
            "fread" => {
                let (buf, size, count, file) = (self.arg(0), self.arg(1), self.arg(2), self.arg(3));
                let total = size.saturating_mul(count);
                let Some(fd) = self.files.file_fd(file) else {
                    self.ret(0);
                    return Ok(());
                };
                let mut tmp = vec![0u8; total as usize];
                let got = self.files.read(fd, &mut tmp).max(0) as u32;
                if got > 0 {
                    self.mem.write_bytes(buf, &tmp[..got as usize]);
                }
                self.ret(if size == 0 { 0 } else { got / size });
            }
            "fseek" => {
                let (file, off, wh) = (self.arg(0), self.arg(1) as i32, self.arg(2) as i32);
                let r = match self.files.file_fd(file) {
                    Some(fd) => {
                        if self.files.seek(fd, off as i64, wh) < 0 {
                            -1
                        } else {
                            0
                        }
                    }
                    None => -1,
                };
                self.ret(r as u32);
            }
            "stat" => {
                let (p, buf) = (self.arg(0), self.arg(1));
                let path = self.mem.cstr(p);
                let r = self.do_stat(&path, buf);
                self.ret(r as u32);
            }
            "access" => {
                let p = self.arg(0);
                let path = self.mem.cstr(p);
                let ok = self.files.resolve(&path).map(|q| q.exists()).unwrap_or(false);
                self.ret(if ok { 0 } else { u32::MAX });
            }
            "mkdir" => {
                let p = self.arg(0);
                let path = self.mem.cstr(p);
                let r = match self.files.resolve(&path) {
                    Some(q) => std::fs::create_dir_all(q).map(|_| 0).unwrap_or(-1),
                    None => -1,
                };
                self.ret(r as u32);
            }
            "rmdir" => {
                let p = self.arg(0);
                let path = self.mem.cstr(p);
                let r = match self.files.resolve(&path) {
                    Some(q) => std::fs::remove_dir_all(q).map(|_| 0).unwrap_or(-1),
                    None => -1,
                };
                self.ret(r as u32);
            }
            "unlink" => {
                let p = self.arg(0);
                let path = self.mem.cstr(p);
                let r = match self.files.resolve(&path) {
                    Some(q) => std::fs::remove_file(q).map(|_| 0).unwrap_or(-1),
                    None => -1,
                };
                self.ret(r as u32);
            }
            "rename" => {
                let (a, b) = (self.arg(0), self.arg(1));
                let (pa, pb) = (self.mem.cstr(a), self.mem.cstr(b));
                let r = match (self.files.resolve(&pa), self.files.resolve(&pb)) {
                    (Some(x), Some(y)) => std::fs::rename(x, y).map(|_| 0).unwrap_or(-1),
                    _ => -1,
                };
                self.ret(r as u32);
            }
            "chdir" => self.ret(0),
            "fcntl" => {
                // F_GETFL = 3, F_SETFL = 4. The guest uses these only to put the
                // socket into non-blocking mode, which std::net already did at
                // connect time, so tracking the flag is enough.
                let (fd, cmd, arg) = (self.arg(0) as i32, self.arg(1) as i32, self.arg(2));
                let r = match cmd {
                    3 => self.files.sock_mut(fd).map(|s| if s.nonblocking { 0x800 } else { 0 }),
                    4 => self.files.sock_mut(fd).map(|s| {
                        s.nonblocking = arg & 0x800 != 0;
                        0
                    }),
                    _ => Some(0),
                };
                self.ret(r.unwrap_or(0) as u32);
            }

            // ---- sockets ---------------------------------------------------
            //
            // Real TCP. The original service is long gone, but the guest's
            // network code is intact and a private server only has to listen —
            // see net.rs.
            "socket" => {
                let fd = self.files.open_socket();
                self.ret(fd as u32);
            }
            "connect" => {
                let (fd, addr, _len) = (self.arg(0) as i32, self.arg(1), self.arg(2));
                let sa = self.read_bytes(addr, 16);
                let Some((ip, port)) = crate::net::parse_sockaddr_in(&sa) else {
                    self.files.errno = crate::net::EINVAL;
                    self.ret(u32::MAX);
                    return Ok(());
                };
                let r = match self.files.sock_mut(fd) {
                    Some(s) => match s.connect(ip, port) {
                        Ok(()) => 0,
                        Err(e) => {
                            self.files.errno = e;
                            -1
                        }
                    },
                    None => {
                        self.files.errno = crate::net::EBADF;
                        -1
                    }
                };
                if r == 0 {
                    log::info!(
                        "guest connected to {}.{}.{}.{}:{port}",
                        ip >> 24,
                        (ip >> 16) & 0xff,
                        (ip >> 8) & 0xff,
                        ip & 0xff
                    );
                }
                self.ret(r as u32);
            }
            "recv" => {
                let (fd, buf, n) = (self.arg(0) as i32, self.arg(1), self.arg(2));
                let mut tmp = vec![0u8; n as usize];
                let got = self.files.read(fd, &mut tmp);
                if got > 0 {
                    self.mem.write_bytes(buf, &tmp[..got as usize]);
                }
                self.ret(got as u32);
            }
            "shutdown" => {
                let fd = self.arg(0) as i32;
                if let Some(s) = self.files.sock_mut(fd) {
                    s.shutdown();
                }
                self.ret(0);
            }
            // The three options MC_netSocket sets (SO_REUSEADDR, SO_KEEPALIVE,
            // SO_LINGER) are all things std::net either does or does not need;
            // reporting success keeps the guest on its normal path.
            "setsockopt" => self.ret(0),
            "getsockopt" => {
                // The only one read is SO_ERROR after connect, and a connect
                // that returned 0 has no error.
                let (optval, optlen) = (self.arg(3), self.arg(4));
                if optval != 0 {
                    self.mem.write_bytes(optval, &0u32.to_le_bytes());
                }
                if optlen != 0 {
                    self.mem.write_bytes(optlen, &4u32.to_le_bytes());
                }
                self.ret(0);
            }
            "select" => {
                let (nfds, rfds, wfds, efds) =
                    (self.arg(0) as i32, self.arg(1), self.arg(2), self.arg(3));
                let n = self.do_select(nfds, rfds, wfds, efds);
                self.ret(n as u32);
            }
            "inet_addr" => {
                let p = self.arg(0);
                let text = self.mem.cstr(p);
                // Returns network byte order, which on this little-endian guest
                // means the first octet ends up in the low byte.
                let v = text
                    .parse::<std::net::Ipv4Addr>()
                    .map(|a| u32::from_le_bytes(a.octets()))
                    .unwrap_or(u32::MAX);
                self.ret(v);
            }

            // ---- runtime odds and ends -------------------------------------
            "__errno" => {
                let e = self.files.errno;
                let w = self.errno_word;
                self.mem.write_bytes(w, &e.to_le_bytes());
                self.ret(w);
            }
            "__android_log_print" => {
                let (tagp, fmtp) = (self.arg(1), self.arg(2));
                let tag = self.mem.cstr(tagp);
                let va = self.gather_varargs(3);
                let msg = self.format_va(fmtp, va);
                // Only the log line is decoded, and it is the one place a lossy
                // conversion is harmless: nothing reads it back.
                let text = String::from_utf8_lossy(&msg).trim_end().to_string();
                self.logline(format!("{tag}: {text}"));
                self.ret(0);
            }
            "__gnu_Unwind_Find_exidx" => {
                // (pc, int* pcount) -> exidx table base
                let pcount = self.arg(1);
                let (base, len) = self.elf.exidx;
                self.mem.write_bytes(pcount, &(len / 8).to_le_bytes());
                self.ret(base);
            }
            "pthread_mutex_lock" | "pthread_mutex_unlock" => self.ret(0),
            "abort" | "__stack_chk_fail" => {
                return Err(format!("guest called {name}"));
            }
            "__cxa_begin_cleanup" | "__cxa_call_unexpected" | "__cxa_type_match" => {
                return Err(format!("guest threw a C++ exception (via {name})"));
            }

            // ---- OpenGL ES 1.x ---------------------------------------------
            _ if name.starts_with("gl") => return self.gles(name),

            other => return Err(format!("unimplemented import: {other}")),
        }
        Ok(())
    }

    /// `select(2)` over the guest's `fd_set`s.
    ///
    /// The guest only ever polls sockets here (its file I/O is synchronous), so
    /// anything else in a set is reported not-ready rather than guessed at.
    fn do_select(&mut self, nfds: i32, rfds: u32, wfds: u32, efds: u32) -> i32 {
        const WORDS: usize = (crate::net::FD_SETSIZE / 32) as usize;
        let load = |vm: &Vm, p: u32| -> Vec<u32> {
            if p == 0 {
                return vec![0; WORDS];
            }
            (0..WORDS)
                .map(|i| {
                    vm.mem
                        .slice(p + (i * 4) as u32, 4)
                        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                        .unwrap_or(0)
                })
                .collect()
        };
        let want_r = load(self, rfds);
        let want_w = load(self, wfds);

        let mut out_r = vec![0u32; WORDS];
        let mut out_w = vec![0u32; WORDS];
        let mut ready = 0;

        for fd in 0..nfds.min(crate::net::FD_SETSIZE as i32) {
            if crate::net::fd_isset(&want_r, fd) {
                if self.files.sock_mut(fd).is_some_and(|s| s.readable()) {
                    crate::net::fd_set(&mut out_r, fd);
                    ready += 1;
                }
            }
            if crate::net::fd_isset(&want_w, fd) {
                let w = self.files.sock_mut(fd).is_some_and(|s| s.writable());
                if w {
                    crate::net::fd_set(&mut out_w, fd);
                    ready += 1;
                }
            }
        }

        let mut store = |p: u32, v: &[u32]| {
            if p == 0 {
                return;
            }
            for (i, w) in v.iter().enumerate() {
                self.mem.write_bytes(p + (i * 4) as u32, &w.to_le_bytes());
            }
        };
        store(rfds, &out_r);
        store(wfds, &out_w);
        // Nothing ever asks for exceptions; clear the set so a stale bit cannot
        // make the guest think an error arrived.
        store(efds, &vec![0u32; WORDS]);
        ready
    }

    fn do_stat(&mut self, path: &str, buf: u32) -> i32 {
        let Some(p) = self.files.resolve(path) else {
            self.files.errno = 13;
            return -1;
        };
        let Ok(md) = std::fs::metadata(&p) else {
            self.files.errno = 2;
            return -1;
        };
        // bionic's 32-bit ARM `struct stat` is 104 bytes; the guest reads
        // st_mode at 16, st_size at 48 and st_mtime at 80 (verified against
        // MC_fsFileAttribute's `ldr r2, [sp, #0x50]` / `ldr r2, [sp, #0x30]`).
        if let Some(s) = self.mem.slice_mut(buf, 104) {
            s.fill(0);
        }
        let mode: u32 = if md.is_dir() { 0o040755 } else { 0o100644 };
        let size = md.len();
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);
        self.mem.write_bytes(buf + 16, &mode.to_le_bytes());
        self.mem.write_bytes(buf + 48, &size.to_le_bytes());
        self.mem.write_bytes(buf + 80, &mtime.to_le_bytes());
        0
    }

    /// `qsort` with a guest comparator.
    ///
    /// Sorting a permutation first keeps the array untouched while the
    /// comparator runs, so the pointers it is handed stay valid — the guest's
    /// comparator dereferences them.
    fn qsort(&mut self, base: u32, n: u32, size: u32, cmp: u32) -> Result<(), String> {
        if n < 2 || size == 0 {
            return Ok(());
        }
        let n = n as usize;
        let mut idx: Vec<usize> = (0..n).collect();
        let mut buf = idx.clone();

        let mut width = 1;
        while width < n {
            let mut i = 0;
            while i < n {
                let (lo, mid, hi) = (i, (i + width).min(n), (i + 2 * width).min(n));
                let (mut a, mut b, mut k) = (lo, mid, lo);
                while a < mid && b < hi {
                    let pa = base + (idx[a] as u32) * size;
                    let pb = base + (idx[b] as u32) * size;
                    let r = self.call(cmp, &[pa, pb])? as i32;
                    if r <= 0 {
                        buf[k] = idx[a];
                        a += 1;
                    } else {
                        buf[k] = idx[b];
                        b += 1;
                    }
                    k += 1;
                }
                while a < mid {
                    buf[k] = idx[a];
                    a += 1;
                    k += 1;
                }
                while b < hi {
                    buf[k] = idx[b];
                    b += 1;
                    k += 1;
                }
                i += 2 * width;
            }
            std::mem::swap(&mut idx, &mut buf);
            width *= 2;
        }

        let elems: Vec<Vec<u8>> =
            (0..n).map(|i| self.read_bytes(base + (i as u32) * size, size)).collect();
        for (dst, &src) in idx.iter().enumerate() {
            self.mem.write_bytes(base + (dst as u32) * size, &elems[src]);
        }
        Ok(())
    }

    fn gles(&mut self, name: &str) -> Result<(), String> {
        match name {
            "glViewport" => {
                let (x, y, w, h) = (
                    self.arg(0) as i32,
                    self.arg(1) as i32,
                    self.arg(2) as i32,
                    self.arg(3) as i32,
                );
                self.gl.viewport = (x, y, w, h);
            }
            "glOrthof" => {
                let (l, r, b, t) = (self.argf(0), self.argf(1), self.argf(2), self.argf(3));
                self.gl.ortho = (l, r, b, t);
            }
            "glMatrixMode" | "glLoadIdentity" | "glCullFace" | "glEnable" | "glDisable" => {}
            "glEnableClientState" | "glDisableClientState" => {
                let cap = self.arg(0);
                if cap == 0x8078 {
                    self.gl.texcoord_enabled = name == "glEnableClientState";
                }
            }
            "glColor4f" => {
                let c = [self.argf(0), self.argf(1), self.argf(2), self.argf(3)];
                self.gl.color = c;
            }
            "glGenTextures" => {
                let (n, p) = (self.arg(0), self.arg(1));
                for i in 0..n {
                    let id = self.gl.gen_texture();
                    self.mem.write_bytes(p + i * 4, &id.to_le_bytes());
                }
            }
            "glDeleteTextures" => {
                let (n, p) = (self.arg(0), self.arg(1));
                for i in 0..n {
                    let id = self.mem.r32(p + i * 4);
                    self.gl.delete_texture(id);
                }
            }
            "glBindTexture" => {
                let id = self.arg(1);
                self.gl.bound = id;
            }
            "glTexParameterf" => {
                let (pname, v) = (self.arg(1), self.argf(2));
                self.gl.tex_parameter(pname, v);
            }
            "glTexImage2D" => {
                let (w, h) = (self.arg(3) as i32, self.arg(4) as i32);
                let (format, ty) = (self.arg(6), self.arg(7));
                self.gl.tex_image(w, h, format, ty);
            }
            "glTexSubImage2D" => {
                let (x, y) = (self.arg(2) as i32, self.arg(3) as i32);
                let (w, h) = (self.arg(4) as i32, self.arg(5) as i32);
                let (format, ty, pixels) = (self.arg(6), self.arg(7), self.arg(8));
                let bytes = (w.max(0) * h.max(0)) as u32 * gl::bytes_per_texel(format, ty) as u32;
                let data = self.read_bytes(pixels, bytes);
                if gl::gl_debug() {
                    // The source address matters as much as the rectangle: if it
                    // alternates, the guest is double-buffering and every frame
                    // shows only that buffer's share of an incremental redraw.
                    let lit = data.chunks_exact(2).filter(|p| p != &[0, 0]).count();
                    log::info!("  <- src={pixels:#x} lit={lit}/{}", data.len() / 2);
                }
                self.gl.tex_sub_image(x, y, w, h, format, ty, &data);
            }
            "glVertexPointer" | "glTexCoordPointer" => {
                let a = gl::Array {
                    size: self.arg(0) as i32,
                    ty: self.arg(1),
                    stride: self.arg(2) as i32,
                    ptr: self.arg(3),
                };
                if name == "glVertexPointer" {
                    self.gl.vertex_array = a;
                } else {
                    self.gl.texcoord_array = a;
                }
            }
            "glDrawArrays" => {
                let (mode, first, count) = (self.arg(0), self.arg(1) as i32, self.arg(2) as i32);
                // The vertex and texcoord arrays live in guest memory; hand the
                // rasteriser a reader rather than copying them.
                let mem = &self.mem;
                let read = |a: u32| {
                    mem.slice(a, 4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                        .unwrap_or(0.0)
                };
                self.gl.draw_arrays(mode, first, count, &read);
            }
            other => return Err(format!("unimplemented GL call: {other}")),
        }
        self.ret_void();
        Ok(())
    }

    // ---- JavaVM ----------------------------------------------------------

    fn javavm(&mut self, slot: usize) -> Result<(), String> {
        match slot {
            4 | 6 => {
                // AttachCurrentThread(vm, &env, args) / GetEnv(vm, &env, version)
                let out = self.arg(1);
                let env = self.jni.env_ptr;
                self.mem.write_bytes(out, &env.to_le_bytes());
                self.ret(0);
            }
            5 => self.ret(0), // DetachCurrentThread
            other => return Err(format!("unimplemented JavaVM slot {other}")),
        }
        Ok(())
    }

    // ---- JNIEnv ----------------------------------------------------------

    fn jnienv(&mut self, slot: usize) -> Result<(), String> {
        match slot {
            4 => self.ret(0x0001_0006), // GetVersion -> JNI_VERSION_1_6
            6 => {
                // FindClass(env, name)
                let n = self.arg(1);
                let name = self.mem.cstr(n);
                let h = self.jni.handle(JObject::Class(name));
                self.ret(h);
            }
            21 | 25 => {
                // NewGlobalRef / NewLocalRef — handles here are already stable.
                let o = self.arg(1);
                self.ret(o);
            }
            22 | 23 => self.ret_void(), // Delete{Global,Local}Ref
            94 => {
                // GetFieldID(env, cls, name, sig). The only field the guest ever
                // asks for is FileDescriptor.descriptor.
                let np = self.arg(2);
                let name = self.mem.cstr_bytes(np);
                let h = self.jni.handle(JObject::Str(name));
                self.ret(h);
            }
            100 => {
                // GetIntField(env, obj, fieldID)
                let o = self.arg(1);
                let v = match self.jni.get(o) {
                    Some(JObject::FileDescriptor(fd)) => *fd,
                    _ => -1,
                };
                self.ret(v as u32);
            }
            113 => {
                // GetStaticMethodID(env, cls, name, sig)
                let (np, sp) = (self.arg(2), self.arg(3));
                let name = self.mem.cstr(np);
                let sig = self.mem.cstr(sp);
                self.jni.methods.push((name, sig));
                self.ret(METHOD_BASE + (self.jni.methods.len() - 1) as u32);
            }
            115 | 130 | 133 | 142 => {
                let mid = self.arg(2);
                let va = self.arg(3);
                self.call_static(slot, mid, va)?;
            }
            167 => {
                // NewStringUTF(env, chars). Raw bytes in, raw bytes out again
                // through GetStringUTFChars — the guest's text is EUC-KR and a
                // decode here would rewrite it (the "썍몌예ㅃ" corruption).
                let p = self.arg(1);
                let s = self.mem.cstr_bytes(p);
                let h = self.jni.new_string_bytes(s);
                self.ret(h);
            }
            169 => {
                // GetStringUTFChars(env, str, isCopy) -> char*
                let o = self.arg(1);
                let mut b = match self.jni.get(o) {
                    Some(JObject::Str(s)) => s.clone(),
                    _ => Vec::new(),
                };
                b.push(0);
                let p = self.heap.alloc(b.len() as u32).unwrap_or(0);
                if p != 0 {
                    self.mem.write_bytes(p, &b);
                    self.jni.pinned.push((p, o));
                }
                let is_copy = self.arg(2);
                if is_copy != 0 {
                    self.mem.write_bytes(is_copy, &1u8.to_le_bytes());
                }
                self.ret(p);
            }
            170 => {
                // ReleaseStringUTFChars(env, str, chars)
                let p = self.arg(2);
                self.jni.pinned.retain(|&(a, _)| a != p);
                if p != 0 {
                    self.heap.free(p);
                }
                self.ret_void();
            }
            176 => {
                // NewByteArray(env, len)
                let n = self.arg(1);
                let h = self.jni.handle(JObject::ByteArray(vec![0u8; n as usize]));
                self.ret(h);
            }
            184 => {
                // GetByteArrayElements(env, arr, isCopy) -> jbyte*
                let o = self.arg(1);
                let data = match self.jni.get(o) {
                    Some(JObject::ByteArray(v)) => v.clone(),
                    _ => Vec::new(),
                };
                let p = self.heap.alloc(data.len().max(1) as u32).unwrap_or(0);
                if p != 0 {
                    self.mem.write_bytes(p, &data);
                    self.jni.pinned.push((p, o));
                }
                self.ret(p);
            }
            192 => {
                // ReleaseByteArrayElements(env, arr, elems, mode)
                let (o, p, mode) = (self.arg(1), self.arg(2), self.arg(3) as i32);
                let len = match self.jni.get(o) {
                    Some(JObject::ByteArray(v)) => v.len() as u32,
                    _ => 0,
                };
                if mode != 2 /* JNI_ABORT */ && len > 0 {
                    let data = self.read_bytes(p, len);
                    if let Some(JObject::ByteArray(v)) = self.jni.get_mut(o) {
                        *v = data;
                    }
                }
                self.jni.pinned.retain(|&(a, _)| a != p);
                if p != 0 {
                    self.heap.free(p);
                }
                self.ret_void();
            }
            208 => {
                // SetByteArrayRegion(env, arr, start, len, buf)
                let (o, start, len, buf) = (self.arg(1), self.arg(2), self.arg(3), self.arg(4));
                let data = self.read_bytes(buf, len);
                if let Some(JObject::ByteArray(v)) = self.jni.get_mut(o) {
                    let s = start as usize;
                    if s + data.len() <= v.len() {
                        v[s..s + data.len()].copy_from_slice(&data);
                    }
                }
                self.ret_void();
            }
            219 => {
                // GetJavaVM(env, JavaVM**)
                let out = self.arg(1);
                let vm = self.jni.vm_ptr;
                self.mem.write_bytes(out, &vm.to_le_bytes());
                self.ret(0);
            }
            other => {
                let n = jni_slot_name(other);
                let pretty = if n.is_empty() { String::new() } else { format!(" ({n})") };
                return Err(format!("guest called unimplemented JNIEnv slot {other}{pretty}"));
            }
        }
        Ok(())
    }

    /// Decode a `CallStatic*MethodV` and run the corresponding `CletEntry`
    /// static method.
    fn call_static(&mut self, slot: usize, mid: u32, va: u32) -> Result<(), String> {
        let idx = mid.wrapping_sub(METHOD_BASE) as usize;
        let Some((name, sig)) = self.jni.methods.get(idx).cloned() else {
            return Err(format!("CallStatic*Method with unknown method id {mid:#x}"));
        };

        // Read the arguments per the signature the guest registered.
        let mut args: Vec<u64> = Vec::new();
        let mut p = va;
        let params = sig.split(')').next().unwrap_or("").trim_start_matches('(').to_string();
        let mut chars = params.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                'J' | 'D' => {
                    p = (p + 7) & !7;
                    let lo = self.mem.r32(p) as u64;
                    let hi = self.mem.r32(p + 4) as u64;
                    p += 8;
                    args.push((hi << 32) | lo);
                }
                'L' => {
                    for d in chars.by_ref() {
                        if d == ';' {
                            break;
                        }
                    }
                    args.push(self.mem.r32(p) as u64);
                    p += 4;
                }
                '[' => {
                    // Skip the element type, which may itself be an object.
                    if let Some(&e) = chars.peek() {
                        chars.next();
                        if e == 'L' {
                            for d in chars.by_ref() {
                                if d == ';' {
                                    break;
                                }
                            }
                        }
                    }
                    args.push(self.mem.r32(p) as u64);
                    p += 4;
                }
                _ => {
                    args.push(self.mem.r32(p) as u64);
                    p += 4;
                }
            }
        }

        if self.trace {
            log::debug!("CletEntry.{name}{sig} {args:x?}");
        }
        let v = self.clet_static(&name, &args)?;
        match slot {
            142 => self.ret_void(),
            133 => self.ret64(v),
            _ => self.ret(v as u32),
        }
        Ok(())
    }

    /// Run `f` on the mixer and turn its success into the WIPI convention
    /// (0 = ok, -1 = no such clip).
    fn audio(&mut self, f: impl FnOnce(&mut crate::audio::Mixer) -> bool) -> u64 {
        match self.cfg.audio.lock() {
            Ok(mut m) => {
                if f(&mut m) {
                    0
                } else {
                    u64::from(u32::MAX)
                }
            }
            Err(_) => u64::from(u32::MAX),
        }
    }

    fn audio_volume(&mut self, bus: Bus) -> u64 {
        self.cfg.audio.lock().map(|m| m.volume(bus)).unwrap_or(0) as u32 as u64
    }

    fn audio_set_volume(&mut self, bus: Bus, v: i32) -> u64 {
        if let Ok(mut m) = self.cfg.audio.lock() {
            m.set_volume(bus, v);
        }
        0
    }

    /// The `co.kr.eamobile.CletEntry` statics, re-implemented natively.
    fn clet_static(&mut self, name: &str, args: &[u64]) -> Result<u64, String> {
        let s = |v: Option<&u64>| v.copied().unwrap_or(0);
        let out = match name {
            // ---- device / environment -------------------------------------
            // These are the answers `MidasDeviceInfo` gave, and the game does
            // branch on them: `UM_getPhoneModel` feeds a `strcmp` in
            // `onStartApp`, and there are model-prefix tests for "IM", "LG",
            // "SK-", "SCH", "SPH" and friends elsewhere. Reporting a plausible
            // modern Samsung — the device this actually runs on — keeps it on
            // the same path a real handset would take, rather than an
            // unrecognised-device fallback nothing was tested against.
            "getOSVersion" => self.cfg.host.os_version as u64,
            "getOpenGLESVersion" => {
                let v = self.cfg.host.gles_version.clone();
                self.jni.new_string(&v) as u64
            }
            "isFBOSupported" => 1,
            "getManufacturer" => {
                let v = self.cfg.host.manufacturer.clone();
                self.jni.new_string(&v) as u64
            }
            "getModel" => {
                let v = self.cfg.host.model.clone();
                self.jni.new_string(&v) as u64
            }
            // No SIM identity is available and none is needed: the guest only
            // forwards these to the (dead) billing and account services.
            "getIMEI" | "getIMSI" | "getMin" => self.jni.new_string("") as u64,
            // `MC_netConnect` gates the whole socket path on this: it rejects
            // anything outside `{1, 2}` with -1 and returns before creating a
            // socket, so answering 0 made every network feature fail with the
            // guest's own "문제가 발생하여 접속종료 되었습니다" and no `connect`
            // ever reaching the host. 1 = a usable data connection; where it
            // actually goes is `net::redirect_target`'s business.
            "isNetworkAvailable" => 1,
            // Roaming would put the game on its extra-charge warning path, and
            // airplane mode is another refuse-to-connect gate. Both stay off.
            "isNetworkRoaming" | "isAirplaneModeOn" => 0,

            // ---- memory ---------------------------------------------------
            "totalMemory" => self.heap.total() as u64,
            "freeMemory" => self.heap.free_bytes() as u64,
            "getTotalInternalMemorySize" | "getTotalExternalMemorySize" => 512 * 1024 * 1024,
            "getAvailableInternalMemorySize" | "getAvailableExternalMemorySize" => {
                256 * 1024 * 1024
            }

            // ---- assets ---------------------------------------------------
            //
            // Everything this build needs lives in data.vfs.mp3, which the guest
            // reads itself through the VFS descriptor. The AssetManager path is
            // only a fallback for loose files, and there are none.
            "getAssetSize" => u64::from(u32::MAX),
            "loadAsset" => u64::from(u32::MAX),
            "getUniqueNumberFromStrings" => {
                let key = match self.jni.get(s(args.first()) as u32) {
                    Some(JObject::Str(v)) => v.clone(),
                    _ => Vec::new(),
                };
                let next = self.jni.asset_ids.len() as i32 + 1;
                let fresh = !self.jni.asset_ids.contains_key(&key);
                if fresh && self.trace {
                    log::debug!("asset #{next} = {:?}", String::from_utf8_lossy(&key));
                }
                let id = *self.jni.asset_ids.entry(key.clone()).or_insert(next);
                self.jni.asset_names.entry(id).or_insert(key);
                id as u32 as u64
            }
            "getUniqueAssetNameFromID" => {
                let id = s(args.first()) as i32;
                match self.jni.asset_names.get(&id).cloned() {
                    Some(n) => self.jni.new_string_bytes(n) as u64,
                    // Java would return null here, and the guest checks for it.
                    None => 0,
                }
            }

            // ---- audio ----------------------------------------------------
            //
            // The guest never sees PCM. It finds a clip in the VFS and passes
            // the byte range down (`putMusicClip(clipId, offset, length)`),
            // which is what `MediaPlayer.setDataSource(fd, offset, length)`
            // consumed on the original. Here the same range is decoded by
            // `audio::Mixer`.
            "putMusicClip" | "putSoundClip" | "putClip" => {
                let id = s(args.first()) as i64;
                let (off, len, bus) = match name {
                    "putMusicClip" => (s(args.get(1)), s(args.get(2)), Bus::Music),
                    "putSoundClip" => (s(args.get(1)), s(args.get(2)), Bus::Sound),
                    // putClip(J I I [B I I I I I): a raw byte[] rather than a
                    // VFS range. Nothing in this build has been seen taking
                    // that path, so it is reported as a failure the guest
                    // already knows how to handle rather than guessed at.
                    _ => (0, 0, Bus::Media),
                };
                if len == 0 {
                    u64::from(u32::MAX)
                } else if self.audio_sources.get(&id) == Some(&(off, len))
                    && self.audio(|m| m.reset(id)) == 0
                {
                    // Already decoded, and the guest is asking for the same byte
                    // range again — it does that on every map change, and each
                    // one used to be a fresh Vorbis decode. Only the playback
                    // state is redone, which is all `put` would have changed.
                    0
                } else {
                    let data = self.vfs_slice(off, len).to_vec();
                    // Off the frame thread when the host asked for it: this is
                    // the call that stalls the frame a dialogue ends on. A
                    // worker that has gone away falls back to decoding inline
                    // rather than losing the clip.
                    let mut ok = false;
                    if self.cfg.async_audio {
                        if self.audio_decoder.is_none() {
                            self.audio_decoder =
                                Some(crate::audio::Decoder::start(self.cfg.audio.clone()));
                        }
                        if let Some(dec) = self.audio_decoder.as_ref() {
                            ok = dec.submit(id, data.clone(), bus);
                        }
                        if ok {
                            if let Ok(mut m) = self.cfg.audio.lock() {
                                m.put_pending(id, bus);
                            }
                        }
                    }
                    if !ok {
                        ok = self
                            .cfg
                            .audio
                            .lock()
                            .map(|mut m| m.put(id, &data, bus))
                            .unwrap_or(false);
                    }
                    if ok {
                        self.audio_sources.insert(id, (off, len));
                        0
                    } else {
                        u64::from(u32::MAX)
                    }
                }
            }
            "clipPlay" | "clipPlaySound" | "clipPlayMusic" => {
                let id = s(args.first()) as i64;
                let looping = s(args.get(1)) != 0;
                self.audio(|m| m.play(id, looping))
            }
            "clipStop" | "clipStopSound" | "clipStopMusic" => {
                let id = s(args.first()) as i64;
                self.audio(|m| m.stop(id))
            }
            "clipPause" | "clipPauseSound" | "clipPauseMusic" => {
                let id = s(args.first()) as i64;
                self.audio(|m| m.pause(id))
            }
            "clipResume" | "clipResumeSound" | "clipResumeMusic" => {
                let id = s(args.first()) as i64;
                self.audio(|m| m.resume(id))
            }
            "clipFree" | "clipFreeSound" | "clipFreeMusic" => {
                let id = s(args.first()) as i64;
                self.audio_sources.remove(&id);
                self.audio(|m| m.free(id))
            }
            "getClipStatus" => {
                let id = s(args.first()) as i64;
                let st = self
                    .cfg
                    .audio
                    .lock()
                    .map(|m| m.status(id))
                    .unwrap_or(crate::audio::MEDIA_NOT_EXIST);
                st as u32 as u64
            }
            "getVolume" => self.audio_volume(Bus::Media),
            "getSoundVolume" => self.audio_volume(Bus::Sound),
            "getMusicVolume" => self.audio_volume(Bus::Music),
            "setVolume" => self.audio_set_volume(Bus::Media, s(args.first()) as i32),
            "setSoundVolume" => self.audio_set_volume(Bus::Sound, s(args.first()) as i32),
            "setMusicVolume" => self.audio_set_volume(Bus::Music, s(args.first()) as i32),
            "attachBufferIDtoResourceID" | "detachBufferIDfromResourceID" => 0,

            // ---- misc -----------------------------------------------------
            "vibration" => 0,
            "exitApp" => {
                self.phase = Phase::Exited;
                0
            }
            "initMTX" => u64::from(u32::MAX),
            // The carrier-billing call, and the reason a cash purchase used to
            // hang forever. See `complete_mtx`.
            "requestMTX" => self.complete_mtx()?,
            "phnCallPlace" | "loadWebpage" => u64::from(u32::MAX),

            other => {
                log::warn!("CletEntry.{other} is not implemented; returning 0");
                0
            }
        };
        Ok(out)
    }

    /// Complete `AndroidService::requestMTX` — the carrier-billing round trip —
    /// as an immediate success, so a cash purchase finishes, for free.
    ///
    /// This, not the socket, is what the cash shop actually waits on, and
    /// returning -1 without ever calling back is what used to hang it. The
    /// shipped flow is: 예 in the shop's deal popup ->
    /// `NpcUI_merchantSelectKeyPressed` sets `netSocket+0x17/+0x18` and tail-jumps
    /// to `MainFrame_setNopenChargeOnCASH`, which sets `isGameServer = 5` and
    /// `netSocket+0x15`, registers `MTX_Result_CB` through `MX_mtxSetCB`, and
    /// tail-jumps to `MX_mtxRequest`. That sets `bProgress = 1` and hands off to
    /// this import; on the original phone the platform put up the carrier dialog
    /// and invoked the callback later. With no callback, all four flags stay set
    /// and the shop's key handler refuses to leave that state — the screen stops
    /// responding and nothing ever resolves.
    ///
    /// So the host plays the platform. `MTX_Result_CB` (0xa5ba4) branches on `r1`
    /// alone — zero is success — and overwrites `r0` on both paths before using
    /// it; `r2` is read only to tell a user cancel (-10) from a plain failure.
    /// Everything about *what* is bought is decided by guest code afterwards
    /// (`NpcUI_setBoughtItemOnCASH` and the three other cash sinks), which also
    /// autosaves and disconnects. Nothing is decided here but "it succeeded".
    ///
    /// Note this covers all four cash sinks, not only the shop: 오브 제거, 즉시
    /// 부활 and 헌 상자 reach the same callback. That is deliberate — they are
    /// carrier purchases against a service that no longer exists, so the only
    /// alternatives are "free" or "permanently broken".
    fn complete_mtx(&mut self) -> Result<u64, String> {
        let decline = u64::from(u32::MAX);
        if !crate::net::free_cash() {
            return Ok(decline);
        }
        let (Some(cb_p), Some(prog_p)) = (self.symbol("mMTXCB"), self.symbol("bProgress")) else {
            return Ok(decline);
        };
        let cb = self.mem.r32(cb_p);
        if cb == 0 {
            // `MX_mtxRequest` would not have got this far, but a guest whose
            // symbols do not match must not be handed a callback of zero.
            return Ok(decline);
        }
        // `MX_mtxRequest` set this on the way in and only the platform clears
        // it; left at 1 it declines every later charge with -1.
        self.mem.write_bytes(prog_p, &0u32.to_le_bytes());
        let param = self.symbol("cbParam").map_or(0, |p| self.mem.r32(p));
        log::info!("free cash: completing MTX as success via {}", self.describe(cb));
        self.call(cb, &[param, 0, 0])?;
        Ok(0)
    }
}

/// Compile-time sanity: the trampoline window must hold every trampoline plus
/// the data slot that follows them.
const _: () = assert!((TRAMP_COUNT as u32) * 4 < STACK_GUARD_ADDR - HLE_BASE);
const _: () = assert!(WIPI_W == 400 && WIPI_H == 240);
