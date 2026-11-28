//! (Diagnostics) `VICTORY_SLOT_TRAP=1`: when the title's CPU reads the slots
//! the ray-tracing structure builder stores structures' compacted sizes into.
//!
//! The title asks the builder for a structure's compacted size (a program of
//! one thread stores it into a slot of a small title buffer), reads it on the
//! CPU, takes memory of that size and clones the structure into it. Clones
//! went over live structures behind their targets: the title had sized them
//! from another value than the one the card stored. This says when the
//! title read, and what it read.
//!
//! The 4 KiB page holding a slot is given the guard flag; the first access
//! clears it and raises a guard fault. One of the title's is logged (the
//! frame, the address, what the slot holds then, read or write); one of our
//! own copies (`crate::safecopy`) is not. Either way the access is let go on
//! by one instruction (the trap flag) and the guard is set again after it.
//! The guards are set again once a frame too (the write watch changes the
//! page's protection and drops the flag).

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{EXCEPTION_SINGLE_STEP, NTSTATUS, STATUS_GUARD_PAGE_VIOLATION};
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, VirtualProtect, VirtualQuery};

const PAGE: usize = 4096;
const TRAP: u32 = 0x100;

/// Pages guarded: the slots in each, with the structure last asked into
/// the slot and the frame it was asked in.
static ARMED: Mutex<Option<HashMap<usize, HashMap<usize, (u64, u64)>>>> = Mutex::new(None);
static SAID: AtomicU32 = AtomicU32::new(0);
/// Guards asked for, set, refused; faults of ours, of the title's.
static COUNTS: [AtomicU32; 5] = [const { AtomicU32::new(0) }; 5];

thread_local! {
    /// The page this thread's last guarded access was on, to guard again
    /// after the instruction.
    static STEPPING: Cell<usize> = const { Cell::new(0) };
}

/// The title addresses that show the same physical page as an address
/// (`victory_hle::modules::xmem::aliases_of`, given at start-up).
static ALIASES: OnceLock<fn(usize) -> Vec<usize>> = OnceLock::new();

/// Gives the trap the lookup of addresses that show the same page.
pub fn set_aliases(f: fn(usize) -> Vec<usize>) {
    let _ = ALIASES.set(f);
}

/// Whether `[at, at + len)` holds a slot the builder was asked a size into;
/// a copy out of it is logged (where the title takes the sizes to).
pub(crate) fn copied(src: usize, dst: usize, len: usize, how: &str) {
    if !on() {
        return;
    }
    let armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
    let Some(a) = armed.as_ref() else { return };
    // The slots in the source, and where each goes.
    let mut to: Vec<(usize, (u64, u64))> = Vec::new();
    for p in (src & !(PAGE - 1)..=(src + len.max(1) - 1) & !(PAGE - 1)).step_by(PAGE) {
        if let Some(slots) = a.get(&p) {
            to.extend(slots.iter().filter(|(s, _)| (src..src + len).contains(s)).map(|(&s, &v)| (dst + (s - src), v)));
        }
    }
    let hit = !to.is_empty();
    // (The copies land in a ring of read-back memory on pages the title's
    // threads use all the time: guarding them crashed the title in its first
    // frame, so they are only logged.)
    drop(armed);
    if hit {
        static SAID: AtomicU32 = AtomicU32::new(0);
        if SAID.fetch_add(1, Ordering::Relaxed) < 400 {
            tracing::info!("slot trap: copy {how} of {src:#x}+{len:#x} to {dst:#x} in frame {}", crate::frame::current());
        }
    }
}

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SLOT_TRAP").is_some_and(|v| v == "1"))
}

/// The builder was asked for `structure`'s compacted size into `slot`.
pub(crate) fn arm(slot: u64, structure: u64, frame: u64) {
    if !on() {
        return;
    }
    install();
    // The slot's own address and every other one that shows its page.
    let mut at = vec![slot as usize];
    if let Some(f) = ALIASES.get() {
        for a in f(slot as usize) {
            if !at.contains(&a) {
                at.push(a);
            }
        }
    }
    static SAID_ALIASES: AtomicU32 = AtomicU32::new(0);
    if SAID_ALIASES.fetch_add(1, Ordering::Relaxed) < 4 {
        tracing::info!("slot trap: {slot:#x} is shown at {:x?}", at);
    }
    let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
    for &a in &at {
        let page = a & !(PAGE - 1);
        armed.get_or_insert_with(HashMap::new).entry(page).or_default().insert(a & !7, (structure, frame));
    }
    drop(armed);
    for &a in &at {
        guard(a & !(PAGE - 1));
    }
}

/// Once a frame: the guards again.
pub(crate) fn frame() {
    if !on() {
        return;
    }
    static FRAMES: AtomicU32 = AtomicU32::new(0);
    if FRAMES.fetch_add(1, Ordering::Relaxed) % 300 == 299 {
        let c: Vec<u32> = COUNTS.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        tracing::info!("slot trap: guards asked {} set {} refused {}; faults of ours {} of the title's {}", c[0], c[1], c[2], c[3], c[4]);
    }
    let pages: Vec<usize> = ARMED.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(Vec::new(), |a| a.keys().copied().collect());
    for page in pages {
        guard(page);
    }
}

fn guard(page: usize) -> bool {
    COUNTS[0].fetch_add(1, Ordering::Relaxed);
    // SAFETY: a query and a protection change of a committed page of the
    // title's; nothing else is touched.
    let (done, protect) = unsafe {
        let mut info: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
        if VirtualQuery(page as *const _, &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) == 0 || info.State != MEM_COMMIT {
            (false, 0)
        } else {
            let mut old = 0;
            (VirtualProtect(page as *const _, PAGE, info.Protect | PAGE_GUARD, &mut old) != 0, info.Protect)
        }
    };
    COUNTS[if done { 1 } else { 2 }].fetch_add(1, Ordering::Relaxed);
    static FIRST: std::sync::Once = std::sync::Once::new();
    FIRST.call_once(|| tracing::info!("slot trap: first guard of {page:#x}: set {done}, protection was {protect:#x}"));
    done
}

fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler looks at guard and single-step faults of pages
    // it guards, and leaves every other fault to the next handler.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_fault));
    });
}

unsafe extern "system" fn on_fault(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the system passes the exception being dispatched.
    let (r, context) = unsafe { (&*(*info).ExceptionRecord, (*info).ContextRecord) };
    if r.ExceptionCode == EXCEPTION_SINGLE_STEP as NTSTATUS {
        let page = STEPPING.with(|s| s.replace(0));
        if page == 0 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        guard(page);
        // SAFETY: the context the system passes.
        unsafe { (*context).EFlags &= !TRAP };
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    if r.ExceptionCode != STATUS_GUARD_PAGE_VIOLATION as NTSTATUS || r.NumberParameters < 2 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let at = r.ExceptionInformation[1];
    let page = at & !(PAGE - 1);
    let slots = {
        let armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
        match armed.as_ref().and_then(|a| a.get(&page)) {
            Some(s) => s.clone(),
            None => return EXCEPTION_CONTINUE_SEARCH,
        }
    };
    let ours = crate::safecopy::failing_exit(r.ExceptionAddress as usize).is_some();
    COUNTS[if ours { 3 } else { 4 }].fetch_add(1, Ordering::Relaxed);
    if !ours && SAID.fetch_add(1, Ordering::Relaxed) < 20000 {
        let write = r.ExceptionInformation[0] == 1;
        // (The fault cleared the guard: the page reads as it is.)
        let slot = at & !7;
        // SAFETY: a committed page of the title's, no longer guarded.
        let value = unsafe { std::ptr::read_volatile(slot as *const u64) };
        let asked = slots.get(&slot).copied();
        tracing::info!(
            "slot trap: frame {} {} {at:#x} holds {value:#x}; asked {} rip {:#x} thread {}",
            crate::frame::current(),
            if write { "write" } else { "read" },
            asked.map_or("not a slot".to_owned(), |(s, f)| format!("of {s:#x} in frame {f}")),
            r.ExceptionAddress as usize,
            // SAFETY: plain call.
            unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
        );
    }
    STEPPING.with(|s| s.set(page));
    // SAFETY: the context the system passes: one instruction, then the
    // single-step fault above.
    unsafe { (*context).EFlags |= TRAP };
    EXCEPTION_CONTINUE_EXECUTION
}
