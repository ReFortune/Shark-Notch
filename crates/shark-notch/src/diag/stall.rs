//! Where does the render thread stand when one Direct2D call takes far too long?
//!
//! The frame-time report says *that* a frame spent 800 ms inside `EndDraw`; this says what the thread
//! was doing at the time. The self-test's watchdog thread calls [`Watcher::poll`] every few
//! milliseconds. When the render thread has been inside `EndDraw` past a threshold it stops that
//! thread for a few microseconds, copies its registers and the top of its stack, lets it go, and only
//! then (the thread running again, so no lock it held can block us) works out which modules the
//! addresses on the stack belong to.
//!
//! Nothing here touches any other process, nothing runs outside `--selftest`, and the thread is only
//! ever stopped and started again (no code is injected, no hook installed). What comes out is a list
//! of module names with offsets (`ntdll!NtWaitForSingleObject+0x14`, `d3d10warp.dll+0x2f1a40`), which
//! is enough to tell "computing in the software renderer" from "waiting for something".

use std::ffi::c_void;
use std::sync::atomic::Ordering;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
#[cfg(target_arch = "x86_64")]
use windows::Win32::System::Diagnostics::Debug::{
    CONTEXT, CONTEXT_CONTROL_AMD64, CONTEXT_FLAGS, CONTEXT_INTEGER_AMD64, GetThreadContext,
};
use windows::Win32::System::Memory::{MEM_IMAGE, MEMORY_BASIC_INFORMATION, VirtualQuery};
use windows::Win32::System::ProcessStatus::{
    EnumProcessModules, GetModuleBaseNameW, GetModuleInformation, MODULEINFO,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT,
    THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};
use windows::Win32::System::WindowsProgramming::QueryThreadCycleTime;

use crate::gfx::stage::{END_DRAW_CYCLES, END_DRAW_SINCE_US};
use crate::win::clock;

/// Look after the thread has been inside `EndDraw` this long (ms), and again at the later marks.
const LOOK_AT_MS: [f64; 4] = [40.0, 120.0, 300.0, 600.0];
/// How much of the stack above the stopped thread's stack pointer to copy (8-byte words).
const STACK_WORDS: usize = 512;
/// How many return addresses to keep from one sample.
const MAX_CALLS: usize = 40;

/// One look at the render thread inside `EndDraw`.
pub struct Sample {
    /// When it was taken, on the `clock` timeline (seconds).
    pub at: f64,
    /// How long the thread had been inside `EndDraw`.
    pub inside_ms: f64,
    /// CPU time the thread itself had used since it went in (the rest it spent waiting).
    pub ran_ms: f64,
    /// Where it was executing.
    pub top: String,
    /// The return addresses found on its stack, innermost first, a run of one module folded to one.
    pub calls: Vec<String>,
}

impl Sample {
    pub fn line(&self, start: f64) -> String {
        format!(
            "+{:.2}s, {:.0} ms into EndDraw, the thread itself ran {:.1} ms of it: at {}; called from {}",
            self.at - start,
            self.inside_ms,
            self.ran_ms,
            self.top,
            if self.calls.is_empty() {
                "(nothing recognisable on the stack)".to_string()
            } else {
                self.calls.join(" <- ")
            }
        )
    }
}

/// Watches the thread that created it (the one running the message loop and the frames).
pub struct Watcher {
    thread: HANDLE,
    cycles_per_ms: f64,
    /// The `EndDraw` call being tracked (its entry time), 0 when none.
    since: u64,
    /// Samples taken for it.
    taken: usize,
}

impl Watcher {
    /// Open `thread_id` for looking at. `cycles_per_ms` converts its cycle counter to milliseconds.
    pub fn new(thread_id: u32, cycles_per_ms: f64) -> Option<Watcher> {
        let thread = unsafe {
            OpenThread(
                THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME | THREAD_QUERY_INFORMATION,
                false,
                thread_id,
            )
        }
        .ok()?;
        Some(Watcher {
            thread,
            cycles_per_ms: cycles_per_ms.max(1.0),
            since: 0,
            taken: 0,
        })
    }

    /// Call every few milliseconds. Returns a sample when this call took one.
    pub fn poll(&mut self) -> Option<Sample> {
        let since = END_DRAW_SINCE_US.load(Ordering::Relaxed);
        if since == 0 {
            self.since = 0;
            return None;
        }
        if since != self.since {
            self.since = since;
            self.taken = 0;
        }
        let inside_ms = (clock::now() * 1e6 - since as f64) / 1e3;
        let due = *LOOK_AT_MS.get(self.taken)?;
        if inside_ms < due {
            return None;
        }
        self.taken += 1;
        let entered_with = END_DRAW_CYCLES.load(Ordering::Relaxed);
        let raw = capture(self.thread)?;
        // After the thread has been released again: the rest allocates and takes locks.
        let mut cycles = 0u64;
        unsafe {
            let _ = QueryThreadCycleTime(self.thread, &mut cycles);
        }
        let mods = modules();
        let top = describe(&mods, raw.rip);
        let mut calls: Vec<(String, String)> = Vec::new();
        for &word in &raw.words {
            let a = word as usize;
            if !(0x10000..=0x7fff_ffff_ffff).contains(&a) {
                continue;
            }
            if let Some(c) = describe_call(&mods, a) {
                calls.push(c);
                if calls.len() >= MAX_CALLS {
                    break;
                }
            }
        }
        Some(Sample {
            at: clock::now(),
            inside_ms,
            ran_ms: cycles.saturating_sub(entered_with) as f64 / self.cycles_per_ms,
            top: top.map_or_else(|| format!("{:#x}", raw.rip), |(_, label)| label),
            calls: fold(calls),
        })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.thread);
        }
    }
}

/// Registers and stack of a thread, copied while it was stopped.
struct Raw {
    rip: usize,
    words: Vec<u64>,
}

/// Stop `thread`, copy its instruction pointer and the top of its stack, start it again.
///
/// Everything that allocates is done before the thread is stopped and after it is running again: it
/// may hold the heap's lock, or the loader's, and a watcher that waited for one of those would wait
/// forever.
#[cfg(target_arch = "x86_64")]
fn capture(thread: HANDLE) -> Option<Raw> {
    let mut words = vec![0u64; STACK_WORDS];
    let mut ctx = CONTEXT {
        ContextFlags: CONTEXT_FLAGS(CONTEXT_CONTROL_AMD64.0 | CONTEXT_INTEGER_AMD64.0),
        ..Default::default()
    };
    let mut read = 0usize;
    let got;
    unsafe {
        if SuspendThread(thread) == u32::MAX {
            return None;
        }
        got = GetThreadContext(thread, &mut ctx).is_ok();
        if got {
            let _ = ReadProcessMemory(
                GetCurrentProcess(),
                ctx.Rsp as *const c_void,
                words.as_mut_ptr().cast(),
                words.len() * 8,
                Some(&mut read),
            );
        }
        ResumeThread(thread);
    }
    if !got {
        return None;
    }
    words.truncate(read / 8);
    Some(Raw {
        rip: ctx.Rip as usize,
        words,
    })
}

/// Other architectures: nothing to look at (the self-test then simply has no stall samples).
#[cfg(not(target_arch = "x86_64"))]
fn capture(_thread: HANDLE) -> Option<Raw> {
    None
}

/// A loaded module: where it is and what it is called.
struct Module {
    name: String,
    base: usize,
    size: usize,
}

fn modules() -> Vec<Module> {
    let mut handles = [HMODULE::default(); 512];
    let mut needed = 0u32;
    let mut out = Vec::new();
    unsafe {
        let me = GetCurrentProcess();
        if EnumProcessModules(
            me,
            handles.as_mut_ptr(),
            std::mem::size_of_val(&handles) as u32,
            &mut needed,
        )
        .is_err()
        {
            return out;
        }
        let n = (needed as usize / std::mem::size_of::<HMODULE>()).min(handles.len());
        for &h in &handles[..n] {
            let mut info = MODULEINFO::default();
            if GetModuleInformation(me, h, &mut info, std::mem::size_of::<MODULEINFO>() as u32)
                .is_err()
            {
                continue;
            }
            let mut name = [0u16; 128];
            let len = GetModuleBaseNameW(me, Some(h), &mut name) as usize;
            out.push(Module {
                name: String::from_utf16_lossy(&name[..len.min(name.len())]),
                base: info.lpBaseOfDll as usize,
                size: info.SizeOfImage as usize,
            });
        }
    }
    out
}

/// `(module, label)` for an address inside a loaded image: `module!Export+0x14` when it sits right
/// behind an exported function (a system call stub, say), `module+0xRVA` otherwise.
fn describe(mods: &[Module], addr: usize) -> Option<(String, String)> {
    let m = mods
        .iter()
        .find(|m| addr >= m.base && addr < m.base + m.size)?;
    let rva = (addr - m.base) as u32;
    let label = match nearest_export(m, rva) {
        Some((name, off)) if off <= 0x400 => format!("{}!{name}+{off:#x}", m.name),
        _ => format!("{}+{rva:#x}", m.name),
    };
    Some((m.name.clone(), label))
}

/// Is `addr` plausibly a return address: executable memory, right behind a `call`?
fn describe_call(mods: &[Module], addr: usize) -> Option<(String, String)> {
    let (is_image, alloc_base) = executable(addr)?;
    if !looks_like_return(addr) {
        return None;
    }
    if is_image {
        describe(mods, addr)
    } else {
        // Executable memory that belongs to no module: code generated at run time. A software
        // renderer compiles its shaders this way.
        Some((
            "(generated code)".into(),
            format!("generated code@{alloc_base:#x}+{:#x}", addr - alloc_base),
        ))
    }
}

/// `(is it part of a loaded image, allocation base)` when `addr` is in executable memory.
fn executable(addr: usize) -> Option<(bool, usize)> {
    let mut mbi = MEMORY_BASIC_INFORMATION::default();
    let n = unsafe {
        VirtualQuery(
            Some(addr as *const c_void),
            &mut mbi,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    // PAGE_EXECUTE, _READ, _READWRITE and _WRITECOPY are 0x10, 0x20, 0x40 and 0x80.
    (n != 0 && mbi.Protect.0 & 0xF0 != 0 && mbi.State.0 == 0x1000)
        .then_some((mbi.Type == MEM_IMAGE, mbi.AllocationBase as usize))
}

/// Do the bytes in front of `addr` end in a `call` instruction (`E8 rel32` or `FF /2`)?
fn looks_like_return(addr: usize) -> bool {
    let mut b = [0u8; 7];
    let mut read = 0usize;
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            (addr - 7) as *const c_void,
            b.as_mut_ptr().cast(),
            b.len(),
            Some(&mut read),
        )
    };
    ok.is_ok() && read == b.len() && call_ends_here(&b)
}

/// `b` holds the seven bytes in front of a candidate return address, `b[6]` the last one.
fn call_ends_here(b: &[u8; 7]) -> bool {
    // call rel32 is five bytes; call r/m64 (FF /2) is two to seven, its ModRM byte right after FF.
    b[2] == 0xE8
        || [5usize, 4, 3, 1, 0]
            .iter()
            .any(|&i| b[i] == 0xFF && b[i + 1] & 0x38 == 0x10)
}

/// Fold a run of addresses in one module into its first (innermost) entry, with a count.
fn fold(calls: Vec<(String, String)>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut last: Option<String> = None;
    let mut run = 0usize;
    for (module, label) in calls {
        if last.as_deref() == Some(module.as_str()) {
            run += 1;
            if let Some(l) = out.last_mut() {
                // Rewrite the "(+n)" suffix of the run's entry.
                let base = l.split(" (+").next().unwrap_or("").to_string();
                *l = format!("{base} (+{run})");
            }
        } else {
            last = Some(module);
            run = 0;
            out.push(label);
        }
    }
    out
}

/// The exported function of `m` that starts closest below `rva`, with the distance to it.
fn nearest_export(m: &Module, rva: u32) -> Option<(String, u32)> {
    let rd32 = |off: usize| -> Option<u32> {
        (off.checked_add(4)? <= m.size)
            .then(|| unsafe { ((m.base + off) as *const u32).read_unaligned() })
    };
    let rd16 = |off: usize| -> Option<u16> {
        (off.checked_add(2)? <= m.size)
            .then(|| unsafe { ((m.base + off) as *const u16).read_unaligned() })
    };
    if rd16(0)? != 0x5A4D {
        return None; // "MZ"
    }
    let nt = rd32(0x3C)? as usize;
    if rd32(nt)? != 0x0000_4550 || rd16(nt + 24)? != 0x20B {
        return None; // "PE\0\0", and a 64-bit optional header
    }
    // Data directory 0 is the export table: 112 bytes into the optional header, after the 24 bytes
    // of signature and file header.
    let dir = rd32(nt + 24 + 112)? as usize;
    let dir_size = rd32(nt + 24 + 116)? as usize;
    if dir == 0 {
        return None;
    }
    let functions = rd32(dir + 20)? as usize;
    let names = rd32(dir + 24)? as usize;
    let (addr_of_functions, addr_of_names, addr_of_ordinals) = (
        rd32(dir + 28)? as usize,
        rd32(dir + 32)? as usize,
        rd32(dir + 36)? as usize,
    );
    let mut best: Option<(u32, usize)> = None;
    for i in 0..names {
        let ordinal = rd16(addr_of_ordinals + 2 * i)? as usize;
        if ordinal >= functions {
            continue;
        }
        let f = rd32(addr_of_functions + 4 * ordinal)?;
        let forwarded = (dir..dir + dir_size).contains(&(f as usize));
        if !forwarded && f <= rva && best.is_none_or(|(b, _)| f > b) {
            best = Some((f, i));
        }
    }
    let (f, i) = best?;
    let name_at = rd32(addr_of_names + 4 * i)? as usize;
    let mut name = String::new();
    for k in 0..96 {
        let c =
            (name_at + k < m.size).then(|| unsafe { *((m.base + name_at + k) as *const u8) })?;
        if c == 0 {
            break;
        }
        name.push(c as char);
    }
    Some((name, rva - f))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_is_recognised_by_the_bytes_in_front_of_the_return_address() {
        // call rel32: E8 xx xx xx xx
        assert!(call_ends_here(&[0, 0, 0xE8, 1, 2, 3, 4]));
        // call rax: FF D0
        assert!(call_ends_here(&[0, 0, 0, 0, 0, 0xFF, 0xD0]));
        // call [rip+disp32]: FF 15 xx xx xx xx
        assert!(call_ends_here(&[0, 0xFF, 0x15, 1, 2, 3, 4]));
        // call [rax+8]: FF 50 08
        assert!(call_ends_here(&[0, 0, 0, 0, 0xFF, 0x50, 0x08]));
        // jmp [rip+disp32] (FF /4) and plain data are not calls
        assert!(!call_ends_here(&[0, 0xFF, 0x25, 1, 2, 3, 4]));
        assert!(!call_ends_here(&[0x90; 7]));
    }

    #[test]
    fn runs_in_one_module_fold_into_one_entry() {
        let calls = vec![
            ("a.dll".to_string(), "a.dll+0x1".to_string()),
            ("a.dll".to_string(), "a.dll+0x2".to_string()),
            ("a.dll".to_string(), "a.dll+0x3".to_string()),
            ("b.dll".to_string(), "b.dll+0x9".to_string()),
            ("a.dll".to_string(), "a.dll+0x4".to_string()),
        ];
        assert_eq!(fold(calls), ["a.dll+0x1 (+2)", "b.dll+0x9", "a.dll+0x4"]);
    }

    #[test]
    fn an_exported_function_is_named_from_its_address() {
        // kernel32 forwards GetCurrentProcessId to KERNELBASE, so GetProcAddress hands back an
        // address in whichever module really holds it; that module must be in the snapshot, and the
        // address is the very start of an exported function.
        use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
        use windows::core::{s, w};
        let mods = modules();
        assert!(!mods.is_empty());
        let k = unsafe { GetModuleHandleW(w!("kernel32.dll")) }.expect("kernel32 is loaded");
        let f = unsafe { GetProcAddress(k, s!("GetCurrentProcessId")) }.expect("exported");
        let (module, label) = describe(&mods, f as usize).expect("inside a loaded module");
        assert!(
            label.starts_with(&module) && label.contains('!') && label.ends_with("+0x0"),
            "{module}: {label}"
        );
    }

    #[test]
    fn the_watcher_sees_nothing_while_no_frame_is_being_drawn() {
        let mut w = Watcher::new(
            unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
            3e6,
        )
        .expect("own thread can be opened");
        assert!(w.poll().is_none());
    }
}
