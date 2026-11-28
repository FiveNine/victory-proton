//! Title memory by address, for translated shaders that read and write
//! through raw 40-bit addresses (`victory_shader::emit::SLOT_MEMORY`: the
//! console's ray-tracing structure builder and the programs that walk its
//! structures; the structures themselves lie in plain title memory, in no
//! buffer object).
//!
//! A 64 KiB page in a title buffer that holds all of it is in that buffer's
//! copy ([`crate::shadow`]), which every other access uses. A page outside
//! buffers has a page of its own in an **arena**: memory of ours with a copy
//! on the card ([`crate::shadow::by_address`]). The streamed structures lie
//! anywhere between other data (6.6 GB of title memory in 4 MiB windows, run
//! light-1; 8 GB in windows of 256 KiB, light-2), so a page is what is kept.
//!
//! This module keeps what a shader needs to find the pages:
//!
//! - the **directory**: per 64 KiB page of title address space the number
//!   of the copy that holds the page (0: none) and the page's byte offset
//!   in it; one for all lists, entries only ever go from empty to filled.
//!   The CPU writes it in memory of its own and shaders read a copy of that
//!   on the card, brought up to date as title memory is (over the bus, every
//!   step of every ray waited for it);
//! - per view heap a run of slots with a raw unordered access view of each
//!   numbered copy (slot 0 of the run: a null view, for pages without a
//!   copy);
//! - the **faults**: the page numbers shaders found empty, read back here
//!   once a frame, so the pages are there from the next frame on. A pointer a
//!   shader takes out of memory cannot be known before it runs.
//!
//! **A page that is not there when a shader touches it is a lost access**:
//! the read gives 0, the store is dropped, and the fault note only helps
//! the next frame. For the structure builder that is the same as a dropped
//! stage, so before one of its dispatches the host makes ready what it can
//! see the dispatch name:
//!
//! - 256 KiB from each root view outside buffers, and where title memory has a
//!   packed ("dehydrated") structure there, all of it: the title ships its
//!   structures packed, and the builder's rehydration stages unpack each in
//!   place once it is streamed in;
//! - 4 MiB from each unordered access view outside buffers (structures built);
//! - the page of each pair of root constants that is a title address and of
//!   each root constant that is one in 256-byte units;
//! - the first page of every structure a list's upload memory names
//!   (a top-level build's instances).
//!
//! **What comes back to title memory**: what the builder stores in title
//! buffers of at most [`BACK`] bytes it names (the sizes the title reads to
//! make a structure smaller). Structures and scratch memory stay on the card
//! (the first whole run of the builder wrote garbage over the title's
//! objects). So an arena's page is the only place a structure is whole, and
//! title memory must not go over it: of a page the title wrote only the
//! dwords that changed since they were last taken go up. A packed structure
//! goes up whole when the builder comes to it, whatever is known of it: the
//! same bytes streamed in again do not differ from those last taken, and the
//! card holds them unpacked (run light-3: a quarter of the structures
//! unpacked a second time, into garbage all over their neighbours).
//!
//! **Stores are bounded**: a structure the builder misreads makes it store
//! anywhere. Pages of title buffers take its stores only once one of its
//! dispatches was given that buffer as an unordered access view (its scratch
//! memory, its results); what else a store reaches is an arena's.
//!
//! A packed structure is gone when title memory has no header of it any more
//! (looked at over [`SWEEP`] frames); the pages it alone has are given back.
//!
//! shortcut: a page held for anything else is kept for the rest of the run.
//! shortcut: pages a list does not name are looked at again in turn, over
//! [`SWEEP`] frames.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use victory_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES, MEMORY_READ_ONLY};
use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::host::HostHeap;
use crate::host::{HostList, host};
use crate::shadow::Shadow;

/// Copies that can be numbered (the run of heap slots is one longer).
const COPIES: u32 = (1 << 12) - 1;
/// A directory page.
const PAGE: u64 = 1 << 16;
/// An arena's bytes.
const ARENA: u64 = 64 << 20;
/// What is compared at a time.
const PIECE: usize = 4096;
/// Pages held, at most (3 GiB).
const MOST: usize = 3 << 14;

/// Pages of title memory with small title buffers in them, at most.
const WINDOWS: usize = 512;
/// The biggest packed structure taken for one.
const PACKED: u64 = 64 << 20;
/// For so many frames what a page holds may still be on its way.
const YOUNG: u64 = 4;
/// Free pages there are, at least, when a list is recorded.
const SPARE: usize = 1024;
/// A packed structure not named for so many frames is another one.
const AGAIN: u64 = 8;
/// From a root view; from one that is written (a structure being built).
const VIEW: u64 = 256 << 10;
const BUILT: u64 = 4 << 20;
/// The biggest title buffer whose builder results come back.
const BACK: u64 = 256 << 10;
/// Pages taken from the faults, of one frame and in all, at most: a ray in a
/// structure that is not right goes anywhere, and 2,048 a frame took all the
/// pages there are in the first seconds (run light-4).
///
/// In all it was 2,048 while structures were not right: the bound was met in
/// the first minute and what rays touched after it had no copy for the rest
/// of the run (165 pages a frame, each a hole in the scene for that ray).
/// With lanes reading their own copies (`victory-shader`, translator version
/// 14) a run asks for about 3,200 in three minutes and then little more, and
/// what is left without a copy is about 30 pages a frame that are not the
/// title's memory at all (2026-11-28).
const FAULTS: usize = 128;
const FAULTS_ALL: usize = 16384;

/// Pages taken from the faults in all (`VICTORY_BVH_FAULTS_ALL=<n>`; unset:
/// [`FAULTS_ALL`]). With ray tracing the bound is met in the first minute
/// (run play-1126-0921: at 63 s), and from then on 130-500 pages a frame that
/// the walking programs touch have no copy for the rest of the run.
fn faults_all() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_BVH_FAULTS_ALL").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(FAULTS_ALL)
    })
}
/// Pages that keep faulting: past [`FAULTS_ALL`] a page is still taken once
/// it has faulted in [`persist`]`.0` frames, up to `.1` such pages. Most
/// faults are pages a garbage pointer reached once; the cap in all went on
/// those in the first seconds, and the pages rays read every frame read
/// zeros for the rest of the run: structures with holes, and ray-traced
/// light that pulsed from frame to frame indoors.
/// `VICTORY_FAULT_PERSIST=<frames>,<pages>` (unset or 0 frames: off).
///
/// Off by default: with ray tracing's light band (`VICTORY_AMBIENT_RT`) the
/// room looked better without it, and its 16,384 pages (1 GB more of title
/// memory and card memory) pushed the PC into swap: frames over 100 ms 8.5%
/// against 2.4%, the worst 2,052 against 727 ms (2026-11-28). `3,16384` was
/// the old default.
fn persist() -> (u32, usize) {
    static SET: std::sync::OnceLock<(u32, usize)> = std::sync::OnceLock::new();
    *SET.get_or_init(|| {
        let set = std::env::var("VICTORY_FAULT_PERSIST").unwrap_or_default();
        let mut n = set.split(',').map(|v| v.trim().parse::<usize>().ok());
        (n.next().flatten().unwrap_or(0) as u32, n.next().flatten().unwrap_or(16384))
    })
}

/// What is made ready from an address a builder dispatch names in its
/// constants (`VICTORY_BVH_NAMED_KB=<n>`; unset: one page, as before). A
/// batched build names its structures and scratch memory there and in no
/// view: past the first page its stores found no copy and were dropped, and
/// a structure built once kept the holes (dumps of 2026-11-28: of the
/// instances of structures built at run time 108 named a whole one, 522 one
/// without its header).
/// `VICTORY_BVH_GEOMETRY=0`: the index and vertex buffers the leaf
/// unpacking stage reads are not made ready (as before).
fn geometry_ready() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_GEOMETRY").is_none_or(|v| v != "0"))
}

/// (Diagnostics) the first tables read are logged.
#[cfg(feature = "diagnostics")]
fn geometry_said() -> bool {
    static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 40
}

/// The geometry records at `table` in title memory, while they look like
/// D3D12 triangle records (48 bytes: transform, index format, vertex
/// format, index count, vertex count, index buffer, vertex buffer, stride;
/// the leaf stage's third view, as 0x134850000 in run geo3):
/// each one's index buffer and its bytes (0 without one) and vertex buffer
/// and its bytes.
fn geometry_records(table: u64) -> Vec<(u64, u64, u64, u64)> {
    const RECORD: usize = 48;
    const MOST: usize = 1024;
    const BIGGEST: u64 = 64 << 20;
    let title = |a: u64| (1 << 32..1 << 40).contains(&a);
    let mut out = Vec::new();
    let mut bytes = [0u8; RECORD];
    for i in 0..MOST {
        crate::host::copy_mapped(bytes.as_mut_ptr(), (table as usize + i * RECORD) as *const u8, RECORD, false);
        let d = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        let q = |at: usize| u64::from(d(at)) | u64::from(d(at + 4)) << 32;
        let (index_format, vertex_format, indices, vertices) = (d(8), d(12), u64::from(d(16)), u64::from(d(20)));
        let (ib, vb, stride) = (q(24), q(32), q(40));
        // The title's records carry the console's own format codes (0x35 for
        // its 16-bit indices, run geo3); the program takes 42 (R32_UINT) for
        // 32-bit indices and any other for 16-bit ones. A record past the
        // last has no buffers.
        let index_size = match (index_format, ib) {
            (_, 0) => 0,
            (42, _) => 4,
            _ => 2,
        };
        let _ = vertex_format;
        if !title(vb)
            || !(4..=256).contains(&stride)
            || vertices == 0
            || vertices >= 1 << 24
            || indices >= 1 << 26
            || (index_size > 0 && (!title(ib) || indices == 0))
        {
            break;
        }
        let ib_len = (indices * index_size).min(BIGGEST);
        let vb_len = (vertices * stride).min(BIGGEST);
        out.push((if index_size > 0 { ib } else { 0 }, if index_size > 0 { ib_len } else { 0 }, vb, vb_len));
    }
    out
}

/// The slots the builder was asked structures' compacted sizes into (8-byte
/// aligned addresses), for [`is_size_slot`].
static SIZE_SLOTS: std::sync::RwLock<Option<std::collections::HashSet<u64>>> = std::sync::RwLock::new(None);

fn note_size_slot(slot: u64) {
    let mut set = SIZE_SLOTS.write().unwrap_or_else(|e| e.into_inner());
    let set = set.get_or_insert_with(Default::default);
    if set.len() > 1 << 16 {
        set.clear();
    }
    set.insert(slot & !7);
}

/// Whether `[at, at + len)` holds a slot the builder was asked a
/// structure's compacted size into.
///
/// The title copies each answer into read-back memory with a buffer copy
/// behind the dispatch that writes it, and reads it there once told the
/// submission ran. A copy into memory without a host copy is done at
/// submission, on the CPU (`crate::host::place`), before any dispatch of
/// the submission ran: the title read the slot's previous answer, sized
/// the clone's target from it, and the clone (which copies the size the
/// structure has) ran over the structures behind its target. Run ring2,
/// 2026-11-28: the answer for 0xdaecd1f00 was 0x2a040 and reached title
/// memory two frames after the question; the title had reserved 0x2300
/// from the slot's old 0x2280. Such copies are made once the list has run.
pub(crate) fn is_size_slot(at: u64, len: u64) -> bool {
    if len == 0 || len > 4096 || !slot_copies_late() {
        return false;
    }
    let set = SIZE_SLOTS.read().unwrap_or_else(|e| e.into_inner());
    let Some(set) = set.as_ref() else { return false };
    (at & !7..at + len).step_by(8).any(|a| set.contains(&a))
}

/// `VICTORY_SLOT_COPY_LATE=0`: copies out of size slots are made at
/// submission, as before.
fn slot_copies_late() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SLOT_COPY_LATE").is_none_or(|v| v != "0"))
}

fn named_len() -> u64 {
    static N: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_BVH_NAMED_KB").ok().and_then(|v| v.trim().parse::<u64>().ok()).map_or(1, |kb| (kb << 10).clamp(1, 16 << 20))
    })
}

/// `VICTORY_BVH_TITLE_QUIET=1` (a test): once a page the builder was given to
/// write a structure in has been taken, what the title's CPU writes there
/// later no longer goes up over the card's bytes.
fn title_quiet() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_TITLE_QUIET").is_some_and(|v| v == "1"))
}

thread_local! {
    /// The dispatch [`ready`] was last asked about on this thread is a clone
    /// that is not to run.
    static REFUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the dispatch just made ready is a clone refused by the guard
/// (asked once: it is forgotten).
pub(crate) fn refused() -> bool {
    REFUSED.with(|r| r.replace(false))
}

/// Whether a clone from a source the host has not seen made is left out
/// (`VICTORY_BVH_CLONE_GUARD=1`).
fn clone_guard() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_CLONE_GUARD").is_some_and(|v| v == "1"))
}

/// Whether what the builder's starting stage is given in its table is made
/// ready (`VICTORY_BVH_TABLES=1`; off: in two runs each way the structures
/// without a header in the title's scene were 345 and 389 without it, 391
/// and 226 with it, and only a dozen pages a run came for the tables: the
/// pages were nearly always there already).
fn tables_now() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_TABLES").is_some_and(|v| v == "1"))
}

/// Whether a structure's header goes up when a builder dispatch names it
/// and a top-level build's instances go up at its submission ([`Memory::header`];
/// `VICTORY_BVH_HEADERS=1`, a test; off: the title's CPU does not write those
/// words, 5 runs of them went up in a run, and the count of structures
/// without a header did not move).
fn header_now() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_HEADERS").is_some_and(|v| v == "1"))
}

/// Frames over which every page is looked at once.
const SWEEP: usize = 128;

/// What a draw or dispatch does with title memory by address.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Use {
    Reads,
    /// Stores by address, as the structure builder; `back`: its results in
    /// small title buffers come back.
    Builds { back: bool },
}

/// A page of title memory in an arena.
struct Held {
    /// Where in the arena's memory, and the arena's place in `copies`.
    at: usize,
    copy: usize,
    /// The write-watch asking after which it was last taken (0: never).
    taken: u64,
    /// The frame it was last looked at in, and the one it came in.
    looked: u64,
    born: u64,
    /// Packed structures that have bytes in it, and whether it is held for
    /// them alone.
    owners: u32,
    theirs: bool,
}

/// A packed structure the builder came to.
struct Packed {
    /// The frame it last did.
    frame: u64,
    len: u64,
    /// Its first two dwords in title memory.
    header: u64,
}

struct Memory {
    /// The CPU's entries (number, offset), and the copy shaders read.
    directory: (*mut u32, Arc<Shadow>),
    faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`.
    copies: Vec<Arc<Shadow>>,
    /// Places of title buffers' copies among `copies`.
    numbers: HashMap<usize, usize>,
    /// Arenas: place in `copies`, address.
    arenas: Vec<(usize, usize)>,
    /// By title page.
    pages: HashMap<u64, Held>,
    /// Their numbers in the order they came, and where the sweep is.
    order: Vec<u64>,
    sweep: usize,
    /// Windows of title memory itself ([`Memory::map`]).
    windows: usize,
    /// Packed structures the builder came to: the frame it last did.
    packed: HashMap<u64, Packed>,
    /// Their addresses in the order they came, and where the sweep is.
    packed_order: Vec<u64>,
    packed_sweep: usize,
    /// Places of pages that are free: the copy, the address in the arena.
    free: Vec<(usize, usize)>,
    /// Copies of title buffers the builder may store in.
    open: std::collections::HashSet<usize>,
    /// Pages the builder was given to write a structure in (a written view
    /// outside buffers).
    built_pages: std::collections::HashSet<u64>,
    /// Structures the host has seen the builder make: started from a row of
    /// its starting stage's table, asked for their size (a build that ran
    /// to its end), or cloned from one of these.
    known: std::collections::HashSet<u64>,
    /// Clones seen, and those refused ([`clone_guard`]).
    clones: (u64, u64),
    /// Runs of header dwords sent up for a builder dispatch ([`Memory::header`]),
    /// and pages of instances sent up for a top-level build.
    headers: u64,
    instances: u64,
    /// Structures the builder's starting stage was given in its table, and
    /// the pages that came for them.
    started: (u64, u64),
    /// Pages taken from faults.
    faulted: usize,
    /// Pages that faulted: the frames they did in, and the last.
    fault_seen: HashMap<u64, (u32, u64)>,
    /// Pages taken for faulting in several frames, and for a dropped store.
    persisted: usize,
    stored: usize,
    /// Structures whose compacted size the builder was asked for: where the
    /// size goes ([`Memory::size_of_copy`]).
    size_slots: HashMap<u64, u64>,
    /// Copies of structures over 4 MiB readied whole.
    copies_sized: u64,
    /// Microseconds readying them, and the longest once, since the last line.
    sized_us: (u64, u64),
    /// Microseconds taking faults, and the longest once, since the last line.
    faults_us: (u64, u64),
    /// Per host view heap (by address): the first slot of its run, and how
    /// many copies have their view there.
    heaps: HashMap<usize, (u32, u32)>,
    /// The frame the faults were last read in.
    frame: u64,
    /// Pages asked for this frame that are not the title's memory.
    refused: std::collections::HashSet<u64>,
    stats: Stats,
    #[cfg(feature = "diagnostics")]
    dump: dump::Dump,
    #[cfg(feature = "diagnostics")]
    trace: trace::Trace,
    #[cfg(feature = "diagnostics")]
    stages: stages::Stages,
}

/// Since the last report.
#[derive(Default)]
struct Stats {
    from: Option<(std::time::Instant, u64)>,
    uses: [u64; 2],
    faults: u64,
    unknown: u64,
    back: u64,
    /// Pages looked at, bytes that changed, packed structures sent.
    looked: u64,
    changed: u64,
    packed: u64,
    packed_bytes: u64,
    gone: u64,
    /// Geometry records whose index and vertex buffers were made ready for
    /// the leaf unpacking stage.
    geometry: u64,
    /// Pages that came for an address in constants, a written view, another
    /// view, a structure named in upload memory.
    came: [u64; 4],
    micros: u64,
    /// Pages found without a copy that were left so because of the bounds on
    /// pages taken from faults, and the different pages faulted.
    left: u64,
    distinct: std::collections::HashSet<u64>,
}

// SAFETY: the mappings are of memory the struct owns; all access is under
// the mutex.
unsafe impl Send for Memory {}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// What a draw binds.
pub(crate) struct Ready {
    pub(crate) directory: u64,
    pub(crate) faults: u64,
    /// The view heap slot of the null view; copy `n`'s view is `n` further.
    pub(crate) base: u32,
}

impl Memory {
    fn new() -> Option<Memory> {
        let host = host()?;
        let bytes = u64::from(MEMORY_PAGES) * 8;
        // Zeroed, and watched for writes as the title's memory is.
        let entries = crate::upload::reserve(bytes as usize)?;
        let directory = crate::shadow::by_address(entries as u64, bytes)?;
        crate::host::name(&directory.buffer, || "title memory by address: directory".into());
        let (faults, data) = host.shared_buffer(u64::from(MEMORY_FAULTS) * 4)?;
        crate::host::name(&faults, || "title memory by address: faults".into());
        // SAFETY: a fresh mapping of at least that many bytes.
        unsafe { std::ptr::write_bytes(data, 0, MEMORY_FAULTS as usize * 4) };
        // SAFETY: a live buffer.
        let gpu = unsafe { faults.GetGPUVirtualAddress() };
        tracing::info!("title memory by address: a shader reads or writes through raw addresses");
        Some(Memory {
            directory: (entries as *mut u32, directory),
            faults: (faults, data.cast::<u32>(), gpu),
            copies: Vec::new(),
            numbers: HashMap::new(),
            arenas: Vec::new(),
            pages: HashMap::new(),
            order: Vec::new(),
            sweep: 0,
            windows: 0,
            packed: HashMap::new(),
            packed_order: Vec::new(),
            packed_sweep: 0,
            free: Vec::new(),
            open: Default::default(),
            built_pages: Default::default(),
            known: Default::default(),
            clones: (0, 0),
            headers: 0,
            instances: 0,
            started: (0, 0),
            faulted: 0,
            fault_seen: HashMap::new(),
            persisted: 0,
            stored: 0,
            size_slots: HashMap::new(),
            copies_sized: 0,
            sized_us: (0, 0),
            faults_us: (0, 0),
            heaps: HashMap::new(),
            frame: u64::MAX,
            refused: Default::default(),
            stats: Stats::default(),
            #[cfg(feature = "diagnostics")]
            dump: Default::default(),
            #[cfg(feature = "diagnostics")]
            trace: Default::default(),
            #[cfg(feature = "diagnostics")]
            stages: Default::default(),
        })
    }

    /// The directory's entry of `page`.
    fn entry(&self, page: u64) -> *mut u32 {
        debug_assert!(page < u64::from(MEMORY_PAGES));
        // SAFETY: inside the directory.
        unsafe { self.directory.0.add(page as usize * 2) }
    }

    /// The place in `copies` of the copy that holds `page`, if one does.
    fn copy_of(&self, page: u64) -> Option<usize> {
        if page >= u64::from(MEMORY_PAGES) {
            return None;
        }
        // SAFETY: an entry of the directory.
        ((unsafe { self.entry(page).read() } & !MEMORY_READ_ONLY) as usize).checked_sub(1)
    }

    /// Whether `page` can be given a place: in the directory, not turned down
    /// this frame, and the title's GPU memory. (A shader's pointer, or a
    /// constant taken for an address, may be anything.)
    fn possible(&mut self, page: u64) -> bool {
        // Not the last entry: addresses past the directory read it, and it
        // stays empty.
        if page < 0x1_0000 || page >= u64::from(MEMORY_PAGES) - 1 || self.refused.contains(&page) {
            return false;
        }
        let at = (page * PAGE) as usize;
        let ours = crate::texture::committed(at as *const u8, PAGE as usize)
            && crate::watch::writes(at, at + PAGE as usize, true).is_some();
        if !ours {
            self.refused.insert(page);
            self.stats.unknown += 1;
        }
        ours
    }

    /// Gives a copy a number, if one is left.
    fn number(&mut self, shadow: Arc<Shadow>) -> Option<usize> {
        let key = Arc::as_ptr(&shadow) as usize;
        if let Some(&n) = self.numbers.get(&key) {
            return Some(n);
        }
        if self.copies.len() >= COPIES as usize {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(COPIES, "title memory by address: no number left for another copy; its pages read zeros"));
            return None;
        }
        // SAFETY: a live buffer.
        let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
        if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            return None;
        }
        self.copies.push(shadow);
        let n = self.copies.len() - 1;
        self.numbers.insert(key, n);
        Some(n)
    }

    /// Gives `page` a place, if it has none and the memory is the title's;
    /// whether it has one then.
    fn map(&mut self, page: u64) -> bool {
        if self.copy_of(page).is_some() {
            return true;
        }
        if !self.possible(page) {
            return false;
        }
        let start = page * PAGE;
        if crate::shadow::in_buffer(start, PAGE).is_some() {
            let Some(shadow) = crate::shadow::find(start, PAGE) else {
                return false;
            };
            let fresh = !self.numbers.contains_key(&(Arc::as_ptr(&shadow) as usize));
            let Some(n) = self.number(shadow.clone()) else {
                return false;
            };
            // Every page the copy holds whole, not only the one asked for.
            // shortcut: a page of a title buffer inside this one that has a
            // copy of its own is given this copy too.
            if fresh {
                let (first, len) = shadow.span();
                let mut at = first.next_multiple_of(PAGE);
                while at + PAGE <= first + len && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
                    let entry = self.entry(at / PAGE);
                    // SAFETY: an entry of the directory.
                    if unsafe { entry.read() } == 0 {
                        close(entry, n, at - first);
                    }
                    at += PAGE;
                }
            }
            // SAFETY: as above.
            if unsafe { self.entry(page).read() } == 0 {
                close(self.entry(page), n, start - shadow.span().0);
            }
            return true;
        }
        // Small title buffers in it: what the builder stores in them comes
        // back, so title memory itself.
        if self.windows < WINDOWS && crate::shadow::small_buffer_in(start, PAGE, BACK) {
            if let Some(shadow) = crate::shadow::by_address(start, PAGE)
                && let Some(n) = self.number(shadow)
            {
                self.windows += 1;
                fill(self.entry(page), n, 0);
                return true;
            }
            return false;
        }
        if self.pages.len() >= MOST {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(MOST, "title memory by address: no more pages are taken; others read zeros"));
            return false;
        }
        if let Some((copy, at)) = self.free.pop() {
            self.hold(page, at, copy);
            return true;
        }
        if !self.another() {
            return false;
        }
        let Some((copy, at)) = self.free.pop() else {
            return false;
        };
        self.hold(page, at, copy);
        true
    }

    /// Another arena's pages.
    fn another(&mut self) -> bool {
        let Some(at) = crate::upload::reserve(ARENA as usize) else {
            return false;
        };
        let Some(n) = crate::shadow::by_address(at as u64, ARENA).and_then(|s| self.number(s)) else {
            return false;
        };
        self.arenas.push((n, at));
        // The lowest first.
        self.free.extend((0..(ARENA / PAGE) as usize).rev().map(|i| (n, at + i * PAGE as usize)));
        true
    }

    /// `page` is held at `at` in the arena that is `copy`.
    fn hold(&mut self, page: u64, at: usize, copy: usize) {
        self.pages.insert(page, Held { at, copy, taken: 0, looked: u64::MAX, born: self.frame, owners: 0, theirs: false });
        self.order.push(page);
        fill(self.entry(page), copy, at as u64 - self.copies[copy].span().0);
    }

    /// The packed structure at `address` is gone: the pages it alone had are
    /// given back.
    fn gone(&mut self, address: u64) {
        let Some(packed) = self.packed.remove(&address) else { return };
        for page in address / PAGE..=(address + packed.len - 1) / PAGE {
            let Some(held) = self.pages.get_mut(&page) else { continue };
            held.owners = held.owners.saturating_sub(1);
            if held.owners == 0 && held.theirs {
                self.free.push((held.copy, held.at));
                self.pages.remove(&page);
                // SAFETY: an entry of the directory, under the mutex.
                unsafe { self.entry(page).write_volatile(0) };
            }
        }
        self.stats.gone += 1;
    }

    /// Whether the packed structure at `address` is still in title memory.
    fn there(&self, address: u64) -> bool {
        let mut header = [0u8; 8];
        crate::texture::committed(address as *const u8, 8) && {
            crate::host::copy_mapped(header.as_mut_ptr(), address as *const u8, 8, false);
            self.packed.get(&address).is_some_and(|p| p.header == u64::from_le_bytes(header))
        }
    }

    /// Looks at some of the pages and packed structures again.
    fn look_again(&mut self) {
        for _ in 0..self.order.len().div_ceil(SWEEP) {
            self.sweep = (self.sweep + 1) % self.order.len();
            let page = self.order[self.sweep];
            if self.pages.contains_key(&page) {
                self.bring(page, page + 1);
            } else {
                self.order.swap_remove(self.sweep);
                if self.order.is_empty() {
                    break;
                }
            }
        }
        for _ in 0..self.packed_order.len().div_ceil(SWEEP) {
            self.packed_sweep = (self.packed_sweep + 1) % self.packed_order.len();
            let address = self.packed_order[self.packed_sweep];
            if !self.packed.contains_key(&address) {
                self.packed_order.swap_remove(self.packed_sweep);
                if self.packed_order.is_empty() {
                    break;
                }
            } else if !self.there(address) {
                self.gone(address);
            }
        }
    }

    /// A dispatch of the builder was given `address` to write: if it is in a
    /// title buffer's copy, stores by address reach that from here on.
    fn opens(&mut self, address: u64) {
        let Some(n) = self.map(address / PAGE).then(|| self.copy_of(address / PAGE)).flatten() else {
            return;
        };
        if self.pages.contains_key(&(address / PAGE)) || !self.open.insert(n) {
            return;
        }
        // (Its pages hold what the CPU writes beside the builder's scratch.)
        self.copies[n].keep_stores();
        let (first, len) = self.copies[n].span();
        tracing::info!(
            start = format_args!("{first:#x}"),
            len,
            "title memory by address: the structure builder may store in a title buffer"
        );
        for page in first.div_ceil(PAGE)..(first + len) / PAGE {
            if self.copy_of(page) == Some(n) {
                fill(self.entry(page), n, page * PAGE - first);
            }
        }
    }

    /// [`Memory::map`] of the pages of `[address, address + len)`, up to the
    /// first that is not the title's, and what the title wrote there since
    /// they were taken goes into the arenas.
    fn ready(&mut self, address: u64, len: u64) {
        let first = address / PAGE;
        let mut end = first;
        while end * PAGE < address + len && self.map(end) {
            end += 1;
        }
        self.bring(first, end);
    }

    /// Takes the pages `[first, end)` that are in arenas from title memory
    /// where it wrote them.
    fn bring(&mut self, first: u64, end: u64) {
        // Once a frame: the builder names the same pages list after list.
        let frame = self.frame;
        if !(first..end).any(|page| self.pages.get(&page).is_some_and(|h| h.looked != frame)) {
            return;
        }
        let started = std::time::Instant::now();
        let answer = crate::watch::writes((first * PAGE) as usize, (end * PAGE) as usize, true);
        for page in first..end {
            let Some(held) = self.pages.get_mut(&page) else {
                continue;
            };
            if held.looked == frame {
                continue;
            }
            held.looked = frame;
            self.stats.looked += 1;
            if held.taken != 0 && title_quiet() && self.built_pages.contains(&page) {
                continue;
            }
            let wrote = answer
                .as_ref()
                .map(|(_, w)| w.binary_search_by_key(&(page as usize), |w| w.0).map_or(0, |i| w[i].1));
            let now = answer.as_ref().map_or(1, |a| a.0);
            let stale = held.taken == 0 || wrote.is_none_or(|w| w > held.taken);
            if !stale {
                continue;
            }
            let never = std::mem::replace(&mut held.taken, now.max(1)) == 0;
            let shadow = &self.copies[held.copy];
            let base = held.at - shadow.span().0 as usize;
            let mut title = [0u8; PIECE];
            for piece in (0..PAGE as usize).step_by(PIECE) {
                let from = (page * PAGE) as usize + piece;
                // (Pages that are not there read as zeros.)
                crate::host::copy_mapped(title.as_mut_ptr(), from as *const u8, PIECE, false);
                // SAFETY: a page of an arena of ours, under the mutex.
                let ours = unsafe { std::slice::from_raw_parts_mut((held.at + piece) as *mut u8, PIECE) };
                self.stats.changed += merge(&title, ours, never) as u64 * 4;
            }
            if never {
                shadow.resend(base, base + PAGE as usize);
            }
        }
        self.stats.micros += started.elapsed().as_micros() as u64;
    }

    /// A builder dispatch names the structure at `at`: the dwords of its
    /// header (64 bytes) that the title's CPU wrote since they were last
    /// taken go up now.
    ///
    /// The console's own code writes a new structure's header on the CPU
    /// (its version and kind, where its elements and parents lie, its
    /// compression word; no root yet) and only then records the stages that
    /// build it; the stages fill in the rest. A page is otherwise taken once
    /// a frame and named once a list ([`Memory::bring`]), so of the
    /// structures built in one frame on one page only the first had its
    /// header on the card: the others kept zeros in those dwords for good
    /// (trace of 2026-11-28: of 390 builds followed, in 195 they never
    /// showed), and a ray that left its short stack in one read parent
    /// pointers from nowhere.
    fn header(&mut self, at: u64) {
        const HEADER: usize = 64;
        let page = at / PAGE;
        let within = (at % PAGE) as usize;
        let Some(held) = self.pages.get(&page) else { return };
        // (A page not yet taken goes up whole when it is.)
        if held.taken == 0 || within + HEADER > PAGE as usize {
            return;
        }
        let mut title = [0u8; HEADER];
        crate::host::copy_mapped(title.as_mut_ptr(), at as *const u8, HEADER, false);
        // SAFETY: a page of an arena of ours, under the mutex.
        let ours = unsafe { std::slice::from_raw_parts_mut((held.at + within) as *mut u8, HEADER) };
        if *ours == title {
            return;
        }
        let shadow = &self.copies[held.copy];
        let base = held.at - shadow.span().0 as usize + within;
        let mut run: Option<usize> = None;
        for i in 0..=HEADER / 4 {
            let differs = i < HEADER / 4 && ours[i * 4..i * 4 + 4] != title[i * 4..i * 4 + 4];
            match (differs, run) {
                (true, None) => run = Some(i),
                (false, Some(from)) => {
                    ours[from * 4..i * 4].copy_from_slice(&title[from * 4..i * 4]);
                    shadow.resend(base + from * 4, base + i * 4);
                    self.stats.changed += ((i - from) * 4) as u64;
                    self.headers += 1;
                    run = None;
                }
                _ => {}
            }
        }
    }

    /// The host has seen the structure at `at` made.
    fn know(&mut self, at: u64) {
        if self.known.len() > 1 << 18 {
            self.known.clear();
        }
        self.known.insert(at);
    }

    /// The bytes of a packed structure title memory has at `address` by now,
    /// and its first two dwords.
    fn packed_at(&self, address: u64) -> Option<(u64, u64)> {
        if address & 0xFF != 0 {
            return None;
        }
        let mut header = [0u8; 8];
        crate::host::copy_mapped(header.as_mut_ptr(), address as *const u8, 8, false);
        let header = u64::from_le_bytes(header);
        Some((packed_len(header)?, header))
    }

    /// At the submission of a list whose builder dispatch was given `address`
    /// outside buffers, in the order of its copies: what title memory has
    /// there now goes up. When the dispatch is recorded it may not be there
    /// yet: the title's queue waits for its streaming. (Nothing is asked of
    /// the write watch here: the submission's asking stands.)
    fn land(&mut self, address: u64) {
        let page = address / PAGE;
        if !crate::texture::committed((page * PAGE) as *const u8, PAGE as usize) {
            return;
        }
        let Some((len, header)) = self.packed_at(address) else {
            // Anything else: what came since its page was first taken.
            for page in page..=(address + VIEW - 1) / PAGE {
                if self.pages.get(&page).is_some_and(|h| h.born + YOUNG >= self.frame)
                    && !(title_quiet() && self.built_pages.contains(&page))
                {
                    self.take(page, 0, PAGE as usize, false);
                }
            }
            return;
        };
        if let Some(known) = self.packed.get_mut(&address) {
            let soon = known.frame + AGAIN >= self.frame;
            known.frame = self.frame;
            if known.header == header && soon {
                return;
            }
            if known.header != header {
                self.gone(address);
            }
        }
        let last = (address + len - 1) / PAGE;
        if !self.packed.contains_key(&address) {
            self.packed.insert(address, Packed { frame: self.frame, len, header });
            self.packed_order.push(address);
            for page in page..=last {
                let new = self.copy_of(page).is_none();
                if new && !self.place(page) {
                    continue;
                }
                if let Some(held) = self.pages.get_mut(&page) {
                    held.owners += 1;
                    held.theirs |= new;
                }
            }
            self.stats.packed_bytes += len;
        }
        for page in page..=last {
            let from = address.max(page * PAGE) - page * PAGE;
            let to = (address + len).min((page + 1) * PAGE) - page * PAGE;
            self.take(page, from as usize, to as usize, true);
        }
        self.stats.packed += 1;
    }

    /// A place in an arena for a page of committed title memory, at a
    /// submission: only one there is already (the list brings up the arenas
    /// it knew when it was recorded).
    fn place(&mut self, page: u64) -> bool {
        if page < 0x1_0000
            || page >= u64::from(MEMORY_PAGES) - 1
            || !crate::texture::committed((page * PAGE) as *const u8, PAGE as usize)
            || crate::shadow::in_buffer(page * PAGE, PAGE).is_some()
        {
            return false;
        }
        let Some((copy, at)) = self.free.pop() else {
            return false;
        };
        self.hold(page, at, copy);
        crate::watch::wrote(self.entry(page) as usize, 8);
        self.take(page, 0, PAGE as usize, true);
        true
    }

    /// Bytes `from..to` of `page` from title memory into its arena's page, at
    /// a submission; `whole`: sent whether they differ from those last taken
    /// or not.
    fn take(&mut self, page: u64, from: usize, to: usize, whole: bool) {
        let Some(held) = self.pages.get(&page) else { return };
        let shadow = &self.copies[held.copy];
        let base = held.at - shadow.span().0 as usize;
        let mut title = [0u8; PIECE];
        let mut at = from;
        while at < to {
            let n = (PIECE - at % PIECE).min(to - at);
            crate::host::copy_mapped(title.as_mut_ptr(), (page as usize * PAGE as usize + at) as *const u8, n, false);
            // SAFETY: a page of an arena of ours, under the mutex.
            let ours = unsafe { std::slice::from_raw_parts_mut((held.at + at) as *mut u8, n) };
            if *ours != title[..n] {
                ours.copy_from_slice(&title[..n]);
                crate::watch::wrote(held.at + at, n);
            }
            at += n;
        }
        if whole {
            shadow.resend(base + from, base + to);
        }
    }

    /// The size of a structure the builder copies (compacts) from `from`: what
    /// the title read for it from the slot its size was asked into, if that
    /// is a size over 4 MiB and under 64 MiB. The copy wrote it whole: past
    /// the 4 MiB readied from a written view its stores were dropped and the
    /// structure stood cut off (rays went through big geometry).
    fn size_of_copy(&self, from: u64) -> Option<u64> {
        if from < 1 << 32 || crate::shadow::in_buffer(from, 1).is_some() {
            return None;
        }
        let slot = *self.size_slots.get(&from)?;
        let mut b = [0u8; 8];
        crate::host::copy_mapped(b.as_mut_ptr(), slot as *const u8, 8, false);
        let len = u64::from_le_bytes(b);
        (BUILT < len && len <= 64 << 20).then_some(len)
    }

    /// The pages shaders found empty since the last frame.
    fn take_faults(&mut self) {
        let mut pages = Vec::new();
        let mut stores = std::collections::HashSet::new();
        for i in 1..MEMORY_FAULTS as usize {
            // SAFETY: inside the faults' mapping; the GPU may be writing,
            // a number missed now is written again next frame.
            let at = unsafe { self.faults.1.add(i) };
            // SAFETY: as above.
            let page = unsafe { at.read_volatile() };
            if page != 0 {
                // (The shader's marks above the page's number: a store in
                // bit 31, a program that traces rays in bit 30.)
                let noted = page;
                let page = noted & 0x3FFF_FFFF;
                if noted & 1 << 31 != 0 {
                    stores.insert(u64::from(page));
                }
                pages.push(u64::from(page));
                // SAFETY: as above.
                unsafe { at.write_volatile(0) };
            }
        }
        if pages.is_empty() {
            return;
        }
        pages.sort_unstable();
        pages.dedup();
        // `VICTORY_BVH_STORES=1`: the pages a store found without a copy
        // (each a dropped store), and whether one was there by then.
        #[cfg(feature = "diagnostics")]
        {
            static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_STORES").is_some_and(|v| v == "1"))
                && !stores.is_empty()
                && SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20000
            {
                let mut text = String::new();
                let mut list: Vec<u64> = stores.iter().copied().collect();
                list.sort_unstable();
                for page in list {
                    let at = page * PAGE;
                    let held = self.copy_of(page).is_some();
                    let buffer = crate::shadow::in_buffer(at, 1).is_some();
                    text += &format!(" {at:#x}{}{}", if held { "+" } else { "" }, if buffer { "b" } else { "" });
                }
                tracing::info!("bvh stores: frame {} dropped on{text}", self.frame);
            }
        }
        let mut made = 0;
        let all = faults_all();
        for &page in &pages {
            self.stats.distinct.insert(page);
            if self.copy_of(page).is_some() {
                continue;
            }
            // A store that found no page was dropped: the structure it wrote
            // has a hole until it is written again. Those pages are few and
            // taken past the cap in all. So is a page that keeps faulting
            // ([`persist`]).
            let stored = stores.contains(&page);
            let (frames, most) = persist();
            let seen = {
                if self.fault_seen.len() > 1 << 20 {
                    self.fault_seen.retain(|_, s| s.0 >= 2);
                }
                let frame = self.frame;
                let e = self.fault_seen.entry(page).or_insert((0, u64::MAX));
                if e.1 != frame {
                    *e = (e.0 + 1, frame);
                }
                e.0
            };
            let kept = frames > 0 && seen >= frames && self.persisted < most;
            let past_cap = self.faulted >= all;
            if made < FAULTS && (!past_cap || stored || kept) {
                if self.map(page) {
                    made += 1;
                    self.faulted += 1;
                    if past_cap && stored {
                        self.stored += 1;
                    } else if past_cap {
                        self.persisted += 1;
                    }
                    self.bring(page, page + 1);
                }
            } else {
                self.stats.left += 1;
            }
        }
        self.stats.faults += pages.len() as u64;
        // The first frames that leave pages without a copy: all of them, and
        // which of them hold a packed structure's first bytes or lie in a
        // title buffer (to tell walked structures from stray pointers).
        static WHOLE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if made < pages.len() && self.faulted >= faults_all() && WHOLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3 {
            let mut text = String::new();
            for &page in pages.iter().filter(|&&p| self.copy_of(p).is_none()) {
                let at = page * PAGE;
                let committed = crate::texture::committed(at as *const u8, PAGE as usize);
                let buffer = crate::shadow::in_buffer(at, 1).is_some();
                let packed = committed && self.packed_at(at).is_some();
                text += &format!(" {at:#x}{}{}{}", if committed { "" } else { "?" }, if buffer { "b" } else { "" }, if packed { "p" } else { "" });
            }
            tracing::warn!(frame = self.frame, "title memory by address: pages left without a copy by the bounds (?: not committed, b: in a title buffer, p: a packed structure there):{text}");
        }
        // Each is an access that read 0 or was dropped.
        static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let said = SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if said < 40 || said.is_multiple_of(100) {
            let some: Vec<String> = pages
                .iter()
                .step_by(pages.len().div_ceil(12))
                .map(|&p| format!("{:#x}", p * PAGE))
                .collect();
            tracing::warn!(
                frame = self.frame,
                pages = pages.len(),
                made,
                held = self.pages.len(),
                ?some,
                "title memory by address: shaders touched pages that had no copy (those accesses read 0 or were dropped); those that are the title's are there now"
            );
        }
    }

    /// The structures named in a list's upload memory around `address`
    /// (instances of a top-level build: a structure's address is their last
    /// 8 bytes): the first page of each.
    fn named_in_upload(&mut self, address: u64) {
        let Some((from, len)) = crate::upload::block(address as usize) else {
            return;
        };
        // SAFETY: the block is handed out and stays mapped; the title wrote
        // it before it recorded the dispatch.
        let words = unsafe { std::slice::from_raw_parts(from as *const u64, len / 8) };
        let mut seen = std::collections::HashSet::new();
        for &word in words {
            #[cfg(feature = "diagnostics")]
            if word & 0xFF == 0 && (1 << 32..1 << 37).contains(&word) {
                if self.dump.descs.0 != self.frame {
                    self.dump.descs = (self.frame, Default::default());
                }
                self.dump.descs.1.insert(word);
            }
            if word & 0xFF == 0 && (1 << 32..1 << 37).contains(&word) && seen.insert(word / PAGE) && self.map(word / PAGE) {
                let page = word / PAGE;
                self.bring(page, page + 1);
            }
        }
    }

    /// A small title buffer whose results come back, if its pages are
    /// windows of title memory itself (or a copy of the buffer's own).
    fn small(&mut self, list: &HostList, start: u64, len: u64) {
        let mut at = start;
        while at < start + len {
            let page = at / PAGE;
            let n = ((page + 1) * PAGE).min(start + len) - at;
            self.map(page);
            if let Some(k) = self.copy_of(page)
                && !self.pages.contains_key(&page)
            {
                self.copies[k].refresh(list, at, n);
                self.copies[k].gpu_writes(list, at, n);
                self.stats.back += 1;
            }
            at += n;
        }
    }

    /// Every copy is brought up to date when `list` is submitted, after what
    /// it deferred so far (once until a copy of the title's intervenes).
    fn bring_up(&self, list: &HostList) {
        let (start, len) = self.directory.1.span();
        self.directory.1.refresh(list, start, len);
        for shadow in &self.copies {
            let (start, len) = shadow.span();
            shadow.refresh(list, start, len);
        }
    }

    /// Every ten seconds or so, what was used.
    fn report(&mut self) {
        let now = std::time::Instant::now();
        let (since, frame) = *self.stats.from.get_or_insert((now, self.frame));
        if now.duration_since(since).as_secs_f64() < 10.0 {
            return;
        }
        let frames = self.frame.wrapping_sub(frame).max(1) as f64;
        let s = std::mem::take(&mut self.stats);
        if s.geometry > 0 {
            tracing::info!(records = s.geometry, "title memory by address: index and vertex buffers made ready for the leaf unpacking stage since the last line");
        }
        tracing::info!(
            "title memory by address: {} pages ({} MB) in {} arenas, {} copies in all; per frame {:.0} dispatches that read, {:.0} that build, \
             {:.1} pages that had no copy, {:.1} pages turned down, {:.1} results brought back; {:.0} pages looked at, {:.1} KB changed, \
             {:.1} packed structures ({} there), {:.2} ms; {} windows; since the last line {} KB of packed structures came and {} of them went, \
             and pages for constants {}, views written {}, others {}, upload memory {}, faults in all {}; \
             since the last line {} different pages faulted, {:.1} a frame left without a copy by the bounds ({} in all)",
            self.pages.len(),
            (self.pages.len() as u64 * PAGE) >> 20,
            self.arenas.len(),
            self.copies.len(),
            s.uses[0] as f64 / frames,
            s.uses[1] as f64 / frames,
            s.faults as f64 / frames,
            s.unknown as f64 / frames,
            s.back as f64 / frames,
            s.looked as f64 / frames,
            s.changed as f64 / 1024.0 / frames,
            s.packed as f64 / frames,
            self.packed.len(),
            s.micros as f64 / 1000.0 / frames,
            self.windows,
            s.packed_bytes >> 10,
            s.gone,
            s.came[0],
            s.came[1],
            s.came[2],
            s.came[3],
            self.faulted,
            s.distinct.len(),
            s.left as f64 / frames,
            faults_all()
        );
        tracing::info!(
            persisted = self.persisted,
            stored = self.stored,
            copies_sized = self.copies_sized,
            sized_ms = self.sized_us.0 as f64 / 1000.0,
            sized_longest_ms = self.sized_us.1 as f64 / 1000.0,
            faults_ms = self.faults_us.0 as f64 / 1000.0,
            faults_longest_ms = self.faults_us.1 as f64 / 1000.0,
            headers = self.headers,
            instance_pages = self.instances,
            started = self.started.0,
            clones = self.clones.0,
            clones_refused = self.clones.1,
            started_pages = self.started.1,
            "title memory by address: pages taken past the cap in all (for faulting in several frames, for a dropped store); big structures' copies readied whole"
        );
        self.sized_us = (0, 0);
        self.faults_us = (0, 0);
        self.stats.from = Some((now, self.frame));
    }
}

/// Whether a structure over 4 MiB that the builder compacts is readied whole
/// ([`Memory::size_of_copy`]; `VICTORY_BVH_COPY_WHOLE=0`: not).
fn copy_whole() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_COPY_WHOLE").is_none_or(|v| v != "0"))
}

/// A list with this key ([`HostList::used`]) asks the builder for sizes.
const SIZES: usize = usize::MAX - (9 << 33);

/// Whether a submission that asks for the sizes of structures is waited for,
/// and landed, before the title goes on (`VICTORY_BVH_SIZES_WAIT=1`; off).
///
/// Off: it does not help. In run light-15, with it, 33 of 154 copies had the
/// slot's old size still (all of them recorded one frame after the size was
/// asked for) against 53 of 230 in light-11, 48 of 275 dynamic structures
/// were good, and the title asks in every frame, so every frame waits (109 ms
/// against 90). The title reads the slot before our submission of the asking
/// list is through, or reads it for another purpose; not known.
///
/// The title asks for a new structure's compacted size (a program of one
/// thread stores it into a small buffer), reads it a frame or two later
/// without looking whether it is there, takes memory of that size and copies
/// the structure into it. With submissions that land up to two frames late it
/// read what the slot held before, some other structure's size, and the copy
/// wrote over its neighbours: of 230 such copies of run light-11, 99 were
/// asked for two frames earlier and landed in that frame, none of those with
/// the right size.
fn sizes_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_SIZES_WAIT").is_some_and(|v| v == "1"))
}

/// Whether a dispatch of the builder is its program of a structure's size:
/// one thread, the structure's address and what to store (1: the compacted
/// size) as constants, a small title buffer to store into.
fn asks_size(written: &[u64], constants: &[u32], threads: Option<[u32; 3]>) -> bool {
    threads == Some([1, 1, 1])
        && matches!(*constants, [_, hi, 1..=3, ..] if (1..0x20).contains(&hi))
        && written.first().is_some_and(|&at| crate::shadow::in_buffer(at, 1).is_some_and(|b| b.1 <= BACK))
}

/// Whether one of `lists` asks for sizes.
pub(crate) fn asked_sizes<'a>(lists: impl Iterator<Item = &'a HostList>) -> bool {
    let mut lists = lists;
    let asked = sizes_wait() && lists.any(|l| l.used(SIZES));
    if asked {
        static TIMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = TIMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n.is_power_of_two() || n.is_multiple_of(100) {
            tracing::info!(n, frame = crate::frame::current(), "submissions that ask for structures' sizes, waited for");
        }
    }
    asked
}

/// [`fill`] of a page stores by address do not reach.
fn close(entry: *mut u32, n: usize, offset: u64) {
    fill(entry, n | MEMORY_READ_ONLY as usize, offset);
}

/// The offset first: a shader that sees the number sees it too.
fn fill(entry: *mut u32, n: usize, offset: u64) {
    // The ray-tracing node test reads a node's 16-byte quarters as such
    // (`victory_shader::emit` `bvh_node_words`): a page on less than 16
    // bytes in its copy would read the wrong bytes there.
    if offset % 16 != 0 {
        static SEEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 4 {
            tracing::warn!(offset, "a page of title memory lies on less than 16 bytes in its copy");
        }
    }
    // SAFETY: an entry of the directory, under the mutex.
    unsafe {
        entry.add(1).write_volatile(offset as u32);
        entry.write_volatile(n as u32 + 1);
    }
}

/// The bytes of a packed structure whose first two dwords these are: the
/// version (6) with the marks of a bottom level and of one that is packed,
/// and its size unpacked in 64-byte nodes.
fn packed_len(header: u64) -> Option<u64> {
    let (version, nodes) = (header as u32, header >> 32);
    let marks = version >> 16;
    // The header counts its nodes of 64 bytes without the last one: the
    // builder's own program of a structure's size stores one more.
    let len = (nodes + 1) * 64;
    (version & 0xFFFF == 6 && marks & 0x8100 == 0x8100 && nodes > 0 && len <= PACKED).then_some(len)
}

/// Puts what `title` holds into `ours` (what was last taken): the dwords that
/// differ. How many.
fn merge(title: &[u8; PIECE], ours: &mut [u8], never: bool) -> usize {
    if never {
        ours.copy_from_slice(title);
        return PIECE / 4;
    }
    if ours == title {
        return 0;
    }
    let mut changed = 0;
    for (t, o) in title.chunks_exact(4).zip(ours.chunks_exact_mut(4)) {
        if t != o {
            o.copy_from_slice(t);
            changed += 1;
        }
    }
    changed
}

/// Before a draw whose shader reads title memory by address: what its
/// root arguments name is made ready (`addresses`: its root views, of them
/// `written` those it writes; `constants`: its root constants, see the
/// module), every numbered copy
/// has its view in `heap`'s run and is brought up to date when `list` is
/// submitted. None if the host cannot hold it; the draw is then dropped.
pub(crate) fn ready(
    list: &HostList,
    heap: &HostHeap,
    addresses: &[u64],
    written: &[u64],
    constants: &[u32],
    use_: Use,
    threads: Option<[u32; 3]>,
) -> Option<Ready> {
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let m = guard.as_mut()?;
    let frame = crate::frame::current();
    if m.frame != frame {
        m.frame = frame;
        m.refused.clear();
        let started = std::time::Instant::now();
        m.take_faults();
        let us = started.elapsed().as_micros() as u64;
        m.faults_us = (m.faults_us.0 + us, m.faults_us.1.max(us));
        m.look_again();
        m.report();
        #[cfg(feature = "diagnostics")]
        m.trace.look(frame);
        #[cfg(feature = "diagnostics")]
        crate::slottrap::frame();
        #[cfg(feature = "diagnostics")]
        crate::clockscan::frame(frame);
        #[cfg(feature = "diagnostics")]
        m.stages_look(frame);
    }
    #[cfg(feature = "diagnostics")]
    if use_ != Use::Reads {
        m.trace.dispatch(frame, list, addresses, written, constants, threads);
    }
    let builds = use_ != Use::Reads;
    if builds && sizes_wait() && asks_size(written, constants, threads) {
        list.first_use(SIZES);
    }
    if builds
        && asks_size(written, constants, threads)
        && let [lo, hi, 1, ..] = *constants
        && let Some(&slot) = written.first()
    {
        if m.size_slots.len() > 1 << 16 {
            m.size_slots.clear();
        }
        m.size_slots.insert(u64::from(lo) | u64::from(hi) << 32, slot);
        note_size_slot(slot);
        m.know(u64::from(lo) | u64::from(hi) << 32);
        #[cfg(feature = "diagnostics")]
        crate::slottrap::arm(slot, u64::from(lo) | u64::from(hi) << 32, frame);
    }
    if builds
        && threads == Some([32, 1, 1])
        && let [from, to] = *written
        && copy_whole()
        && let Some(len) = m.size_of_copy(from)
        && list.first_use(usize::MAX - (6 << 33) - (to >> 2 & 0x7FFF_FFFF) as usize)
    {
        let started = std::time::Instant::now();
        m.ready(from, len);
        m.ready(to, len);
        m.copies_sized += 1;
        let us = started.elapsed().as_micros() as u64;
        m.sized_us = (m.sized_us.0 + us, m.sized_us.1.max(us));
    }
    while builds && m.free.len() < SPARE && m.pages.len() + m.free.len() < MOST && m.another() {}
    m.stats.uses[usize::from(builds)] += 1;
    // Once per list and page named: the builder dispatches hundreds of times
    // a frame with the same arguments.
    const READ: usize = usize::MAX - 0x6d;
    let named = |kind: usize, at: u64| list.first_use(usize::MAX - (kind << 26) - (at / PAGE) as usize);
    // A title address in two constants (low dword, then bits 32 to 39), or
    // in one in 256-byte units (the builder's alone). A constant that only
    // looks like one names memory nothing reads; one that is not the title's
    // memory is refused.
    let held = |m: &Memory| m.pages.len() as u64;
    let before = held(m);
    for (i, &c) in constants.iter().enumerate() {
        if let Some(&high) = constants.get(i + 1)
            && (1..=0x1F).contains(&high)
            && c >= 0x1_0000
        {
            let at = u64::from(c) | u64::from(high) << 32;
            if named(1, at) {
                m.ready(at, if builds { named_len() } else { 1 });
            }
        }
        if builds && (0x0100_0000..0x2000_0000).contains(&c) {
            let at = u64::from(c) << 8;
            if named(1, at) {
                m.ready(at, named_len());
            }
        }
    }
    // The builder's stage that unpacks a streamed structure's leaves (groups
    // of 1,024 threads, 2,048 leaves a group): it reads each triangle's
    // indices and corners from the title's own index and vertex buffers,
    // through 48-byte geometry records (as D3D12's triangles: transform,
    // index format, vertex format, index count, vertex count, index buffer,
    // vertex buffer and stride). No view names those buffers, so in the
    // builder's first frames, when it unpacks hundreds of structures, most
    // of their pages had no copy: the reads gave 0, the corners came out at
    // one point, and the unpacking is not done again (run wall3: 498 of
    // 1,805 structures, the house's walls and floors among them, with most
    // leaves so). The records are the title's CPU's: what they name is made
    // ready here.
    // (Its constants: three zeros, the count, the three axes' scales, 1.)
    if builds
        && threads == Some([1024, 1, 1])
        && geometry_ready()
        && let [0, 0, 0, _, _, _, _, 1, ..] = *constants
    {
        for &table in addresses {
            if table < 1 << 32 || table >= 1 << 40 || table & 7 != 0 || !named(3, table) {
                continue;
            }
            let records = geometry_records(table);
            #[cfg(feature = "diagnostics")]
            if m.stages_on() || geometry_said() {
                let text: Vec<String> = records.iter().take(4).map(|g| format!("ib {:#x}+{:#x} vb {:#x}+{:#x}", g.0, g.1, g.2, g.3)).collect();
                let mut raw = [0u32; 24];
                crate::host::copy_mapped(raw.as_mut_ptr().cast(), table as *const u8, 96, false);
                let raw: Vec<String> = raw.iter().map(|w| format!("{w:08x}")).collect();
                tracing::info!(
                    "bvh geometry: frame {frame} table {table:#x} of views {addresses:x?} constants {constants:x?}: {} records; {}; raw {}",
                    records.len(),
                    text.join(", "),
                    raw.join(" ")
                );
            }
            for (ib, ib_len, vb, vb_len) in records {
                if ib_len > 0 {
                    m.ready(ib, ib_len);
                }
                m.ready(vb, vb_len);
                m.stats.geometry += 1;
            }
        }
    }
    // The builder's clone: two views, both written (the source, the
    // target), four constants. It copies (source.total + 1) * 64 bytes, the
    // count read from the source's own header: from a source that is no
    // structure it copies megabytes to gigabytes over what lies behind the
    // target (trace of 2026-11-28: 287 of 1,083 clones in a run had such a
    // source, 123 of them a count over 64 MiB; all at the title's scene
    // changes, after which the structures of its scene decayed). A clone
    // whose source the host has not seen made is not run.
    REFUSED.with(|r| r.set(false));
    if builds
        && threads == Some([32, 1, 1])
        && constants.len() == 4
        && let [from, to] = *written
        && addresses.len() == 2
        && from >= 1 << 32
        && to >= 1 << 32
    {
        let known = m.known.contains(&from) || m.packed.contains_key(&from);
        m.clones.0 += 1;
        #[cfg(feature = "diagnostics")]
        if m.stages_on() {
            tracing::info!("bvh clone: frame {frame} {from:#x} -> {to:#x} known={known}");
        }
        if known {
            m.know(to);
        } else if clone_guard() {
            m.clones.1 += 1;
            m.known.remove(&to);
            REFUSED.with(|r| r.set(true));
        }
    }
    // The builder's stage that starts structures: one thread a structure,
    // each storing a header (its kind, where its elements and parents lie,
    // empty bounds, no root) at an address it takes from a table of 40-byte
    // rows (the structure, a place for its bounds, the header's words), with
    // the count in its first constant and nothing given to write. No view
    // and no constant names the structures, so a page of theirs that had no
    // copy yet, or was a title buffer's the builder had not been given,
    // lost the store, and the stages after it built a structure without
    // those words: no version, no parents (trace of 2026-11-28: in 195 of
    // 390 builds followed they never showed; of the structures the title
    // had in its scene 290 to 390 were such). The rows are the title's
    // CPU's: what they name is made ready here.
    if builds
        && written.is_empty()
        && threads == Some([32, 1, 1])
        && let (Some(&table), Some(&n)) = (addresses.first(), constants.first())
        && (1..=4096).contains(&n)
        && table >= 1 << 32
    {
        const ROW: usize = 40;
        let mut rows = vec![0u8; n as usize * ROW];
        for (i, piece) in rows.chunks_mut(PIECE).enumerate() {
            crate::host::copy_mapped(piece.as_mut_ptr(), (table as usize + i * PIECE) as *const u8, piece.len(), false);
        }
        let word = |row: &[u8], at: usize| u64::from_le_bytes([row[at], row[at + 1], row[at + 2], row[at + 3], row[at + 4], row[at + 5], row[at + 6], row[at + 7]]);
        // (A table of another stage with this shape: its rows name nothing.)
        let named = |a: u64| (1 << 32..1 << 37).contains(&a);
        #[cfg(feature = "diagnostics")]
        if m.stages_on() {
            let dests: Vec<String> = rows.chunks_exact(ROW).map(|r| format!("{:x}", word(r, 0))).collect();
            tracing::info!("bvh table: frame {frame} after dispatch {} list {:x} rows {n}: {}", m.stages_n(), std::ptr::from_ref(list) as usize & 0xFF_FFFF, dests.join(" "));
        }
        if rows.chunks_exact(ROW).all(|row| named(word(row, 0)) && word(row, 0) & 0xFF == 0) {
            let before = m.pages.len();
            for row in rows.chunks_exact(ROW) {
                let (structure, bounds) = (word(row, 0), word(row, 8));
                m.know(structure);
                if !tables_now() {
                    continue;
                }
                m.ready(structure, 64);
                m.opens(structure);
                if named(bounds) {
                    m.ready(bounds, 24);
                    m.opens(bounds);
                }
            }
            m.started.0 += u64::from(n);
            m.started.1 += (m.pages.len() - before) as u64;
        }
    }
    if builds && header_now() {
        // At the submission: what the title's CPU wrote for the dispatch may
        // not be there yet when it is recorded ([`Memory::land`]).
        let later = |at: u64| {
            if list.first_use(usize::MAX - (12 << 33) - (at >> 8 & 0x7FFF_FFFF) as usize) {
                list.defer_host(move || {
                    if let Some(m) = MEMORY.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                        m.header(at);
                    }
                });
            }
        };
        for (i, &c) in constants.iter().enumerate() {
            if let Some(&high) = constants.get(i + 1)
                && (1..=0x1F).contains(&high)
                && c >= 0x1_0000
                && c & 0xFF == 0
            {
                later(u64::from(c) | u64::from(high) << 32);
            }
            if (0x0100_0000..0x2000_0000).contains(&c) {
                later(u64::from(c) << 8);
            }
        }
        for &at in written.iter().filter(|&&a| a >= 1 << 32 && a & 0xFF == 0) {
            later(at);
        }
        // A top-level build (the structure, its instances and how many of
        // them in its constants): the instances are the title's CPU's, in a
        // title buffer, and the part of them on a page the buffer does not
        // hold whole is in an arena, where it was as old as the page's last
        // look (dump of 2026-11-28: all 906 instances on such a page
        // differed from title memory). It goes up whole at the submission.
        if threads == Some([32, 1, 1])
            && let [lo, hi, dlo, dhi, _, _, _, n, ..] = *constants
            && (1..0x20).contains(&hi)
            && (1..0x20).contains(&dhi)
            && (1..200_000).contains(&n)
            && lo & 0xFF == 0
        {
            let (from, to) = (u64::from(dlo) | u64::from(dhi) << 32, (u64::from(dlo) | u64::from(dhi) << 32) + u64::from(n) * 64);
            if crate::shadow::in_buffer(from, 1).is_some() && list.first_use(usize::MAX - (13 << 33) - (from >> 8 & 0x7FFF_FFFF) as usize) {
                list.defer_host(move || {
                    if let Some(m) = MEMORY.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                        for page in from / PAGE..=(to - 1) / PAGE {
                            if m.pages.get(&page).is_some_and(|h| h.taken != 0) {
                                let a = from.max(page * PAGE) - page * PAGE;
                                let b = to.min((page + 1) * PAGE) - page * PAGE;
                                m.take(page, a as usize, b as usize, true);
                                m.instances += 1;
                            }
                        }
                    }
                });
            }
        }
    }
    let mut came = [held(m) - before, 0, 0, 0];
    for &at in addresses.iter().filter(|&&a| a >= 1 << 32) {
        // (Several structures to a page.)
        let mut packed = false;
        if builds
            && list.first_use(usize::MAX - (3 << 33) - (at >> 8) as usize)
            && crate::shadow::in_buffer(at, 1).is_none()
            && m.possible(at / PAGE)
        {
            // What it needs, as far as it is known now; the rest when the
            // list is submitted ([`Memory::land`]).
            if let Some((len, _)) = m.packed_at(at) {
                packed = true;
                for page in at / PAGE..=(at + len - 1) / PAGE {
                    let new = m.copy_of(page).is_none();
                    if m.map(page)
                        && new
                        && let Some(held) = m.pages.get_mut(&page)
                    {
                        held.theirs = true;
                    }
                }
                m.bring(at / PAGE, (at + len - 1) / PAGE + 1);
            }
            list.defer_host(move || {
                if let Some(m) = MEMORY.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    m.land(at);
                }
            });
            m.bring_up(list);
        }
        if builds && written.contains(&at) && list.first_use(usize::MAX - (4 << 33) - (at >> 2 & 0x7FFF_FFFF) as usize) {
            if let Some((start, len)) = crate::shadow::in_buffer(at, 1)
                && len <= BACK
                && use_ == (Use::Builds { back: true })
                && list.first_use(usize::MAX - (5 << 33) - (start >> 2 & 0x7FFF_FFFF) as usize)
            {
                m.small(list, start, len);
            }
            m.opens(at);
            if !packed && crate::shadow::in_buffer(at, 1).is_none() {
                #[cfg(feature = "diagnostics")]
                {
                    let built = m.dump.built.entry(at).or_insert((frame, frame, 0));
                    (built.1, built.2) = (frame, built.2 + 1);
                }
                let before = held(m);
                m.ready(at, BUILT);
                came[1] += held(m).saturating_sub(before);
                if title_quiet() {
                    for page in at / PAGE..=(at + BUILT - 1) / PAGE {
                        if m.pages.contains_key(&page) {
                            m.built_pages.insert(page);
                        }
                    }
                }
            }
        }
        if packed || !named(2, at) {
            continue;
        }
        let before = held(m);
        match crate::shadow::in_buffer(at, 1) {
            Some(_) => {
                m.ready(at, 1);
                m.named_in_upload(at);
                came[3] += held(m).saturating_sub(before);
            }
            None => {
                m.ready(at, if builds { VIEW } else { PAGE });
                came[2] += held(m).saturating_sub(before);
            }
        }
    }
    for (sum, n) in m.stats.came.iter_mut().zip(came) {
        *sum += n;
    }
    let key = std::ptr::from_ref(heap) as usize;
    let (base, made) = match m.heaps.get(&key) {
        Some(&run) => run,
        None => {
            // Held for the heap's life: not given back with a list's slots.
            let base = heap.dynamic(COPIES + 1)?;
            // Every slot of the run: a number shows no view of another kind.
            for n in 0..=COPIES {
                heap.null_raw_uav_at(base + n);
            }
            (base, 0)
        }
    };
    for (n, shadow) in m.copies.iter().enumerate().skip(made as usize) {
        let (_, len) = shadow.span();
        heap.raw_uav_at(base + 1 + n as u32, &shadow.buffer, 0, len);
    }
    m.heaps.insert(key, (base, m.copies.len() as u32));
    // Every copy by every list that has come to by-address memory, whoever
    // numbered it: an arena another list made while this one was being
    // recorded was brought up to date by that list's submission alone, which
    // may run after this one's (structures unpacked from zeros, and then
    // written over by the arena's first upload: in frames that stream much in
    // half of them, run light-8).
    let first_read = list.first_use(READ);
    if list.first_use(usize::MAX - (6 << 33) - m.copies.len()) {
        m.bring_up(list);
    }
    if first_read {
        list.keep(&m.faults.0);
        #[cfg(feature = "diagnostics")]
        m.dump(list, addresses);
    }
    #[cfg(feature = "diagnostics")]
    if use_ == Use::Reads {
        m.dump.walked(addresses);
    }
    #[cfg(feature = "diagnostics")]
    if builds {
        m.stages(list, frame, addresses, written, constants, threads);
    }
    #[cfg(feature = "diagnostics")]
    if builds
        && threads == Some([32, 1, 1])
        && let [lo, hi, dlo, dhi, _, _, _, n, ..] = *constants
        && (1..0x20).contains(&hi)
        && (1..0x20).contains(&dhi)
        && (1..200_000).contains(&n)
        && lo & 0xFF == 0
    {
        m.dump.tops.insert(u64::from(lo) | u64::from(hi) << 32, (u64::from(dlo) | u64::from(dhi) << 32, n, frame));
        // The structures its instances name, as title memory has them now.
        if m.stages_on() && list.first_use(usize::MAX - (11 << 33) - (lo >> 8) as usize) {
            let at = u64::from(dlo) | u64::from(dhi) << 32;
            let mut bytes = vec![0u8; n as usize * 64];
            for (i, piece) in bytes.chunks_mut(PIECE).enumerate() {
                crate::host::copy_mapped(piece.as_mut_ptr(), (at as usize + i * PIECE) as *const u8, piece.len(), false);
            }
            for desc in bytes.chunks_exact(64) {
                let named = u64::from_le_bytes([desc[56], desc[57], desc[58], desc[59], desc[60], desc[61], desc[62], desc[63]]) & !0xFF;
                if named != 0 {
                    m.stages_named(named);
                }
            }
        }
    }
    let directory = &m.directory.1;
    Some(Ready {
        directory: directory.gpu(directory.span().0),
        faults: m.faults.2,
        base,
    })
}

/// `VICTORY_BVH_TRACE=1`: the sizes the builder is asked for (its program of
/// one thread that stores a structure's compacted size) and when title memory
/// has them, and the copies to a place of that size.
#[cfg(feature = "diagnostics")]
mod trace {
    use super::*;

    #[derive(Default)]
    pub(super) struct Trace {
        /// Frame, where the size goes, the structure, what title memory had.
        asked: Vec<(u64, u64, u64, u64)>,
        /// By structure, what its size was when last seen to change.
        sizes: HashMap<u64, (u64, u64)>,
        said: u32,
    }

    fn on() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_TRACE").is_some_and(|v| v == "1"))
    }

    fn value(at: u64) -> u64 {
        let mut b = [0u8; 8];
        crate::host::copy_mapped(b.as_mut_ptr(), at as *const u8, 8, false);
        u64::from_le_bytes(b)
    }

    impl Trace {
        fn say(&mut self, text: String) {
            self.said += 1;
            if self.said < 6000 {
                tracing::info!("bvh trace: {text}");
            }
        }

        pub(super) fn look(&mut self, frame: u64) {
            if !on() {
                return;
            }
            let mut gone = Vec::new();
            for i in 0..self.asked.len() {
                let (then, at, structure, was) = self.asked[i];
                let now = value(at);
                if now != was {
                    self.sizes.insert(structure, (now, frame));
                    gone.push(i);
                    self.say(format!("frame {frame}: size of {structure:#x} at {at:#x} asked in {then}: {was:#x} -> {now:#x}"));
                } else if frame > then + 60 {
                    gone.push(i);
                    self.say(format!("frame {frame}: size of {structure:#x} at {at:#x} asked in {then}: still {was:#x}"));
                }
            }
            for i in gone.into_iter().rev() {
                self.asked.swap_remove(i);
            }
        }

        pub(super) fn dispatch(
            &mut self,
            frame: u64,
            list: &HostList,
            addresses: &[u64],
            written: &[u64],
            constants: &[u32],
            threads: Option<[u32; 3]>,
        ) {
            if !on() {
                return;
            }
            let _ = addresses;
            let list = std::ptr::from_ref(list) as usize & 0xFFFF_FFFF;
            if threads == Some([1, 1, 1])
                && let [lo, hi, mode, ..] = *constants
                && (1..=3).contains(&mode)
                && hi < 0x20
                && let Some(&at) = written.first()
                && crate::shadow::in_buffer(at, 1).is_some_and(|b| b.1 <= BACK)
            {
                let structure = u64::from(lo) | u64::from(hi) << 32;
                let was = value(at);
                self.say(format!("frame {frame} list {list:x}: asks mode {mode} of {structure:#x} to {at:#x}, there {was:#x}"));
                if mode == 1 {
                    self.asked.push((frame, at, structure, was));
                }
            } else if threads == Some([32, 1, 1])
                && let [from, to] = *written
                && [from, to].iter().all(|&a| a >= 1 << 32 && crate::shadow::in_buffer(a, 1).is_none())
            {
                let known = self.sizes.get(&from).copied();
                self.say(format!("frame {frame} list {list:x}: copies {from:#x} to {to:#x} constants {:x?}, its size was {known:x?}", &constants[..constants.len().min(4)]));
            }
        }
    }
}

/// `VICTORY_BVH_STAGES=<frames>,<structures>`: so many frames after the
/// builder's first dispatch, the first
/// so many structures the builder is newly given to write are followed: the
/// 64 bytes at each one's address (its header) as the card holds them before
/// every builder dispatch that names it, and before the first of each frame, logged when they change, with the dispatch they changed
/// before. To see which stage writes a header and which writes over it.
#[cfg(feature = "diagnostics")]
mod stages {
    use super::*;

    const SLOTS: u32 = 1 << 18;
    /// Frames a structure is looked at once a frame after it is first
    /// seen, and frames every dispatch that names it is.
    const WATCH: u64 = 12;
    const CLOSE: u64 = 4;

    #[derive(Default)]
    pub(super) struct Stages {
        to: Option<(ID3D12Resource, usize)>,
        next: u32,
        /// Structures still watched: the address and the frame it was
        /// first given in.
        recent: Vec<(u64, u64)>,
        seen: std::collections::HashSet<u64>,
        named: std::collections::HashSet<u64>,
        /// Structures looked at once a frame for the rest of the run.
        long: Vec<u64>,
        /// Due frame, slot, structure, whether it is said unchanged too,
        /// what the dispatch was.
        pending: std::collections::VecDeque<(u64, u32, u64, bool, String)>,
        last: HashMap<u64, [u8; 64]>,
        /// The frame whose first dispatch was taken; dispatches counted.
        frame: u64,
        n: u64,
        /// The frame of the builder's first dispatch.
        began: u64,
    }

    fn set() -> Option<(u64, usize)> {
        settings().map(|s| (s.0, s.1))
    }

    /// `VICTORY_BVH_STAGES=<frames>,<frames>[,<structures>]`: the third, how
    /// many structures the instances name are watched once a frame for the
    /// rest of the run (unset: 160).
    fn settings() -> Option<(u64, usize, usize)> {
        static SET: std::sync::OnceLock<Option<(u64, usize, usize)>> = std::sync::OnceLock::new();
        *SET.get_or_init(|| {
            let v = std::env::var("VICTORY_BVH_STAGES").ok()?;
            let mut n = v.split(',').map(|x| x.trim().parse::<u64>().ok());
            Some((
                n.next().flatten()?,
                n.next().flatten().unwrap_or(16) as usize,
                n.next().flatten().unwrap_or(160) as usize,
            ))
        })
    }

    static STAGE: Mutex<String> = Mutex::new(String::new());

    /// The builder pipeline whose dispatch is being recorded.
    pub(crate) fn note(name: &str) {
        if set().is_some() {
            let mut s = STAGE.lock().unwrap_or_else(|e| e.into_inner());
            s.clear();
            s.extend(name.chars().take(120));
        }
    }

    impl Memory {
        pub(super) fn stages_look(&mut self, frame: u64) {
            let Some((_, data)) = self.stages.to else { return };
            while self.stages.pending.front().is_some_and(|p| p.0 <= frame) {
                let Some((_, slot, at, always, what)) = self.stages.pending.pop_front() else { break };
                let mut now = [0u8; 64];
                // SAFETY: inside the mapping; the copy ran frames ago.
                unsafe { std::ptr::copy_nonoverlapping((data + slot as usize * 64) as *const u8, now.as_mut_ptr(), 64) };
                if always || self.stages.last.get(&at) != Some(&now) {
                    self.stages.last.insert(at, now);
                    let words: Vec<String> = now.chunks(4).map(|c| format!("{:08x}", u32::from_le_bytes([c[0], c[1], c[2], c[3]]))).collect();
                    tracing::info!("bvh stage: {at:#x} = {} before {what}", words.join(" "));
                }
            }
        }

        pub(super) fn stages_on(&self) -> bool {
            set().is_some()
        }

        pub(super) fn stages_n(&self) -> u64 {
            self.stages.n
        }

        /// An instance of a top-level build names the structure at `at`.
        pub(super) fn stages_named(&mut self, at: u64) {
            let Some((after, long_after)) = set() else { return };
            let frame = self.frame;
            if self.stages.began == 0 || frame < self.stages.began + after || !self.stages.named.insert(at) {
                return;
            }
            let known = self.stages.seen.contains(&at);
            tracing::info!("bvh stage: instance names {at:#x} from frame {frame} written-before={known}");
            // The first of the builder's own: watched for the rest of the run.
            if self.stages.long.len() < settings().map_or(160, |s| s.2) && (0xd9..0xdd).contains(&(at >> 28)) && frame >= self.stages.began + long_after as u64 {
                self.stages.long.push(at);
            } else {
                self.stages.recent.push((at, frame));
            }
        }

        pub(super) fn stages(
            &mut self,
            list: &HostList,
            frame: u64,
            addresses: &[u64],
            written: &[u64],
            constants: &[u32],
            threads: Option<[u32; 3]>,
        ) {
            let Some((after, _)) = set() else { return };
            if self.stages.began == 0 {
                self.stages.began = frame;
            }
            let start = self.stages.began + after;
            if frame < start {
                self.stages.seen.extend(written);
                return;
            }
            for &w in written {
                if self.stages.seen.insert(w) && w >= 1 << 32 && w & 0xFF == 0 && crate::shadow::in_buffer(w, 1).is_none() {
                    self.stages.recent.push((w, frame));
                }
            }
            self.stages.n += 1;
            let first = std::mem::replace(&mut self.stages.frame, frame) != frame;
            if first {
                self.stages.recent.retain(|r| frame <= r.1 + WATCH);
            }
            let names = |at: u64| {
                written.contains(&at)
                    || addresses.contains(&at)
                    || constants.iter().any(|&c| u64::from(c) << 8 == at)
                    || constants.windows(2).any(|c| u64::from(c[0]) | u64::from(c[1]) << 32 == at)
            };
            let mut which: Vec<(u64, bool)> = self
                .stages
                .recent
                .iter()
                .filter_map(|r| {
                    let close = frame <= r.1 + CLOSE && names(r.0);
                    (first || close).then_some((r.0, close))
                })
                .collect();
            if first {
                which.extend(self.stages.long.iter().map(|&at| (at, false)));
            }
            // `VICTORY_BVH_STAGE_OF=<hash>[,<hash>...]`: every dispatch of those
            // stages, what each is given to write (a clone's source and target).
            static OF: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
            if let Some(of) = OF.get_or_init(|| std::env::var("VICTORY_BVH_STAGE_OF").ok())
                && of.split(',').any(|o| !o.is_empty() && STAGE.lock().unwrap_or_else(|e| e.into_inner()).contains(o))
            {
                which.extend(written.iter().filter(|&&w| w >= 1 << 32).map(|&w| (w, true)));
                // At a clone: the slot its source's compacted size was asked
                // into, as the card holds it.
                if let [from, _] = *written
                    && let Some(&slot) = self.size_slots.get(&from)
                {
                    which.push((slot, true));
                }
            }
            if which.is_empty() {
                return;
            }
            if self.stages.to.is_none() {
                let Some(host) = host() else { return };
                let Some((to, data)) = host.shared_buffer(u64::from(SLOTS) * 64) else { return };
                self.stages.to = Some((to, data as usize));
            }
            let Some((to, _)) = self.stages.to.clone() else { return };
            let stage = STAGE.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let what = format!(
                "frame {frame} list {:x} dispatch {} {threads:?} constants {:x?} written {written:x?} views {addresses:x?} of {stage}",
                std::ptr::from_ref(list) as usize & 0xFF_FFFF,
                self.stages.n,
                &constants[..constants.len().min(8)]
            );
            let turn = |buffer: &ID3D12Resource, before, after| D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                        // SAFETY: the same pointer, not counted; the barrier
                        // is used while the buffer lives.
                        pResource: unsafe { std::mem::transmute_copy(buffer) },
                        Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                        StateBefore: before,
                        StateAfter: after,
                    }),
                },
            };
            let wait = D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
                },
            };
            // SAFETY: an open list; a barrier that names no resource.
            unsafe { list.list.ResourceBarrier(&[wait]) };
            for (at, close) in which {
                let page = at / PAGE;
                let Some(n) = self.copy_of(page) else { continue };
                let shadow = &self.copies[n];
                let offset = match self.pages.get(&page) {
                    Some(held) => (held.at as u64 - shadow.span().0) + at % PAGE,
                    None => at - shadow.span().0,
                };
                // (A ring: a slot is read 8 frames after it is written.)
                let slot = self.stages.next % SLOTS;
                self.stages.next = self.stages.next.wrapping_add(1);
                // SAFETY: an open list; live buffers; 64 bytes inside both.
                unsafe {
                    list.list.ResourceBarrier(&[turn(&shadow.buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                    list.list.CopyBufferRegion(&to, u64::from(slot) * 64, &shadow.buffer, offset, 64);
                    list.list.ResourceBarrier(&[turn(&shadow.buffer, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON)]);
                }
                self.stages.pending.push_back((frame + 8, slot, at, close, what.clone()));
            }
            list.keep(&to);
        }
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) use stages::note as note_stage;

/// `VICTORY_MEMORY_DUMP=<folder>`: when a file `go` appears there, the arenas
/// as the card holds them are copied out and written (`arena-<n>.bin`), with
/// `pages.txt` (title page, arena, offset) and the structures the walking
/// programs were given: to look at the structures offline.
#[cfg(feature = "diagnostics")]
mod dump {
    use super::*;

    #[derive(Default)]
    pub(super) struct Dump {
        walked: std::collections::BTreeSet<u64>,
        /// Views the builder was given to write outside buffers: the first
        /// and the last frame, how often.
        pub(super) built: std::collections::BTreeMap<u64, (u64, u64, u64)>,
        /// The structures upload memory named to the builder in the last
        /// frame it did (a top-level build's instances).
        pub(super) descs: (u64, std::collections::BTreeSet<u64>),
        /// Top-level builds: the structure, where its instances are and how
        /// many, the frame (a dispatch with both addresses and a count in its
        /// constants).
        pub(super) tops: HashMap<u64, (u64, u32, u64)>,
        /// The instances of each top-level build as title memory held them
        /// when the copies were recorded: the structure, where they are, how
        /// many, the frame it was last built in, the bytes. (Read when the
        /// dump is written, 12 frames later, a structure built or moved in
        /// between looked like one the instances name and that is not there.)
        pub(super) taken: Vec<(u64, u64, u32, u64, Vec<u8>)>,
        /// The frame the copies were recorded in.
        pub(super) taken_at: u64,
        /// Packed (streamed) structures as title memory held them when the
        /// copies were recorded: to compare with the card's (pages the card
        /// holds as zeros that title memory filled).
        pub(super) titles: Vec<(u64, Vec<u8>)>,
        /// The frame it is written at, and the copies.
        due: Option<(u64, Vec<(ID3D12Resource, usize)>)>,
        /// Dumps taken so far, and the folder the one being written goes to.
        taken_n: u32,
        to: Option<std::path::PathBuf>,
    }

    impl Dump {
        pub(super) fn walked(&mut self, addresses: &[u64]) {
            if self.walked.len() < 64 {
                self.walked.extend(addresses);
            }
        }
    }

    fn text_frame(dump: &mut Dump, frame: u64) {
        dump.taken_at = frame;
    }

    fn folder() -> Option<&'static std::path::PathBuf> {
        static FOLDER: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
        FOLDER.get_or_init(|| std::env::var_os("VICTORY_MEMORY_DUMP").map(Into::into)).as_ref()
    }

    fn turn(buffer: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    // SAFETY: the same pointer, not counted; the barrier is
                    // used while the buffer lives.
                    pResource: unsafe { std::mem::transmute_copy(buffer) },
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: before,
                    StateAfter: after,
                }),
            },
        }
    }

    impl Memory {
        /// At a list's first use: the copies are recorded into it.
        pub(super) fn dump(&mut self, list: &HostList, _: &[u64]) {
            let Some(folder) = folder() else { return };
            if let Some((due, _)) = &self.dump.due {
                if self.frame < *due {
                    return;
                }
                let copies = self.dump.due.take().map(|d| d.1).unwrap_or_default();
                let folder = self.dump.to.take().unwrap_or_else(|| folder.clone());
                let folder = &folder;
                let mut text = String::new();
                for a in &self.dump.walked {
                    text += &format!("walked {a:#x}\n");
                }
                for (a, (first, last, n)) in &self.dump.built {
                    text += &format!("built {a:#x} {first} {last} {n}\n");
                }
                for (a, p) in &self.packed {
                    text += &format!("packed {a:#x} {} {}\n", p.len, p.frame);
                }
                for a in &self.dump.descs.1 {
                    text += &format!("desc {a:#x} {}\n", self.dump.descs.0);
                }
                for (a, bytes) in std::mem::take(&mut self.dump.titles) {
                    let _ = std::fs::write(folder.join(format!("title-{a:x}.bin")), &bytes);
                }
                for (dest, at, n, frame, bytes) in &self.dump.taken {
                    let _ = std::fs::write(folder.join(format!("instances-{dest:x}.bin")), bytes);
                    let buffer = crate::shadow::in_buffer(*at, 1);
                    text += &format!("top {dest:#x} {at:#x} {n} {frame} {buffer:x?}\n");
                }
                text += &format!("taken {}\n", self.dump.taken_at);
                text += &format!("frame {}\n", self.frame);
                for (page, held) in &self.pages {
                    let arena = self.arenas.iter().position(|a| a.0 == held.copy).unwrap_or(usize::MAX);
                    let at = held.at - self.arenas[arena].1;
                    text += &format!("{:#x} {arena} {at:#x}\n", page * PAGE);
                }
                let _ = std::fs::write(folder.join("pages.txt"), text);
                for (n, (_, data)) in copies.iter().enumerate() {
                    // SAFETY: a mapping of an arena's bytes; the copies ran
                    // frames ago.
                    let bytes = unsafe { std::slice::from_raw_parts(*data as *const u8, ARENA as usize) };
                    let _ = std::fs::write(folder.join(format!("arena-{n}.bin")), bytes);
                }
                tracing::info!(arenas = copies.len(), "title memory by address: the arenas are written out");
                return;
            }
            // A dump each time a file `go` appears (it is taken away): the
            // first into the folder itself, the next into `2`, `3`... in it.
            if !folder.join("go").exists() {
                return;
            }
            let _ = std::fs::remove_file(folder.join("go"));
            self.dump.taken_n += 1;
            let to = if self.dump.taken_n == 1 { folder.clone() } else { folder.join(self.dump.taken_n.to_string()) };
            let _ = std::fs::create_dir_all(&to);
            tracing::info!(n = self.dump.taken_n, frame = self.frame, "title memory by address: a dump asked for");
            self.dump.to = Some(to);
            self.dump.walked.clear();
            let Some(host) = host() else { return };
            let mut copies = Vec::new();
            for &(n, _) in &self.arenas {
                let buffer = &self.copies[n].buffer;
                let Some((to, data)) = host.shared_buffer(ARENA) else { break };
                // SAFETY: an open list; live buffers of those sizes. The list
                // has not used the arena yet.
                unsafe {
                    list.list.ResourceBarrier(&[turn(buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                    list.list.CopyBufferRegion(&to, 0, buffer, 0, ARENA);
                    list.list.ResourceBarrier(&[turn(buffer, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON)]);
                }
                list.keep(&to);
                copies.push((to, data as usize));
            }
            self.dump.taken = self
                .dump
                .tops
                .iter()
                .map(|(&dest, &(at, n, frame))| {
                    let mut bytes = vec![0u8; n as usize * 64];
                    for (i, piece) in bytes.chunks_mut(PIECE).enumerate() {
                        crate::host::copy_mapped(piece.as_mut_ptr(), (at as usize + i * PIECE) as *const u8, piece.len(), false);
                    }
                    (dest, at, n, frame, bytes)
                })
                .collect();
            // (At most 768 MB of them, and only with
            // `VICTORY_MEMORY_DUMP_TITLES=1`: they are held in RAM until written.)
            let mut room: usize = if std::env::var_os("VICTORY_MEMORY_DUMP_TITLES").is_some_and(|v| v == "1") { 768 << 20 } else { 0 };
            let mut titles = Vec::new();
            for (&a, p) in &self.packed {
                let len = p.len as usize;
                if len > room {
                    continue;
                }
                room -= len;
                let mut bytes = vec![0u8; len];
                for (i, piece) in bytes.chunks_mut(PIECE).enumerate() {
                    crate::host::copy_mapped(piece.as_mut_ptr(), (a as usize + i * PIECE) as *const u8, piece.len(), false);
                }
                titles.push((a, bytes));
            }
            self.dump.titles = titles;
            text_frame(&mut self.dump, self.frame);
            self.dump.due = Some((self.frame + 12, copies));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_piece_takes_what_changed() {
        let mut title = [7u8; PIECE];
        let mut ours = vec![0u8; PIECE];
        assert_eq!(merge(&title, &mut ours, true), PIECE / 4);
        assert_eq!(ours, title);
        assert_eq!(merge(&title, &mut ours, false), 0);
        // A dword of it.
        title[41] = 8;
        assert_eq!(merge(&title, &mut ours, false), 1);
        assert_eq!(ours, title);
    }

    #[test]
    fn a_packed_structure_is_known_by_its_header() {
        let header = |version: u32, nodes: u32| u64::from(version) | u64::from(nodes) << 32;
        assert_eq!(packed_len(header(0x8102_0006, 0x9a1a)), Some(0x9a1b * 64));
        // Unpacked, a top level, another version, no size, too big.
        assert_eq!(packed_len(header(0x8002_0006, 0x9a1a)), None);
        assert_eq!(packed_len(header(0x0000_0006, 5)), None);
        assert_eq!(packed_len(header(0x8102_0007, 5)), None);
        assert_eq!(packed_len(header(0x8102_0006, 0)), None);
        assert_eq!(packed_len(header(0x8102_0006, 1 << 21)), None);
    }
}
