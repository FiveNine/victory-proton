//! Diagnostics: the card's time by compute pipeline (`VICTORY_GPU_PROFILE=1`).
//!
//! Every dispatch is put between two timestamps of one query heap, resolved
//! into memory the PC reads; a thread of ours sums what came back by
//! pipeline name and logs the costliest every 5 s, in milliseconds of the
//! card's time per second. Dispatches on the card overlap, so a time is the
//! stretch from the one stamp to the other, not a cost alone: the large ones
//! are what to look at.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::{
    D3D12_QUERY_HEAP_DESC, D3D12_QUERY_HEAP_TYPE_TIMESTAMP, D3D12_QUERY_TYPE_TIMESTAMP, ID3D12GraphicsCommandList,
    ID3D12QueryHeap, ID3D12Resource,
};

/// Dispatches in the ring: a pair of stamps each.
const SLOTS: usize = 1 << 16;

struct Prof {
    heap: ID3D12QueryHeap,
    _buffer: ID3D12Resource,
    back: usize,
    /// The name of the pipeline of each slot.
    names: Vec<Mutex<&'static str>>,
    next: AtomicU64,
    frequency: f64,
}

// SAFETY: D3D12 objects are free-threaded; the mapping is read as atomics.
unsafe impl Send for Prof {}
// SAFETY: as above.
unsafe impl Sync for Prof {}

/// Whether the switch is on.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GPU_PROFILE").is_some_and(|v| v == "1"))
}

fn prof() -> Option<&'static Prof> {
    static PROF: OnceLock<Option<Prof>> = OnceLock::new();
    PROF.get_or_init(|| {
        let host = crate::host::host()?;
        let (buffer, back) = host.shared_buffer(16 * SLOTS as u64)?;
        // SAFETY: plain object creation; a fresh mapping of that size.
        let heap = unsafe {
            std::ptr::write_bytes(back, 0, 16 * SLOTS);
            let mut heap: Option<ID3D12QueryHeap> = None;
            host.device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: 2 * SLOTS as u32, NodeMask: 0 },
                    &mut heap,
                )
                .ok()?;
            heap?
        };
        let frequency = host.timestamp_frequency()? as f64;
        let p = Prof {
            heap,
            _buffer: buffer,
            back: back as usize,
            names: (0..SLOTS).map(|_| Mutex::new("")).collect(),
            next: AtomicU64::new(0),
            frequency,
        };
        let _ = std::thread::Builder::new().name("victory-gpu-profile".into()).spawn(collect);
        tracing::info!(frequency, "GPU profile on (VICTORY_GPU_PROFILE=1)");
        Some(p)
    })
    .as_ref()
}

/// A dispatch of pipeline `name` is about to be recorded in `list`: its
/// slot, for [`after`].
pub(crate) fn before(list: &ID3D12GraphicsCommandList, name: &str) -> Option<u32> {
    let p = prof()?;
    let slot = (p.next.fetch_add(1, Ordering::Relaxed) % SLOTS as u64) as u32;
    *p.names[slot as usize].lock().unwrap_or_else(|e| e.into_inner()) = intern(name);
    // SAFETY: an open list; a query of the heap.
    unsafe { list.EndQuery(&p.heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * slot) };
    Some(slot)
}

/// The dispatch of `slot` was recorded.
pub(crate) fn after(list: &ID3D12GraphicsCommandList, slot: u32) {
    let Some(p) = prof() else { return };
    // SAFETY: an open list; queries of the heap and room in the buffer.
    unsafe {
        list.EndQuery(&p.heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * slot + 1);
        list.ResolveQueryData(&p.heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * slot, 2, &p._buffer, u64::from(slot) * 16);
    }
}

fn intern(name: &str) -> &'static str {
    static NAMES: Mutex<Option<HashMap<String, &'static str>>> = Mutex::new(None);
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    let names = names.get_or_insert_with(HashMap::new);
    if let Some(&n) = names.get(name) {
        return n;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    names.insert(name.to_owned(), leaked);
    leaked
}

/// Sums the stamps that came back, by name, and logs every 5 s.
fn collect() {
    let mut sums: HashMap<&'static str, (f64, u64)> = HashMap::new();
    let mut since = std::time::Instant::now();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let Some(p) = prof() else { return };
        let words = p.back as *const AtomicU64;
        for slot in 0..SLOTS {
            // SAFETY: inside the mapping; the card writes whole quadwords.
            let (a, b) = unsafe { (&*words.add(2 * slot), &*words.add(2 * slot + 1)) };
            let (t0, t1) = (a.load(Ordering::Acquire), b.load(Ordering::Acquire));
            if t0 == 0 || t1 == 0 {
                continue;
            }
            a.store(0, Ordering::Relaxed);
            b.store(0, Ordering::Relaxed);
            if t1 < t0 {
                continue;
            }
            let name = *p.names[slot].lock().unwrap_or_else(|e| e.into_inner());
            let e = sums.entry(name).or_default();
            e.0 += (t1 - t0) as f64 / p.frequency * 1e3;
            e.1 += 1;
        }
        let secs = since.elapsed().as_secs_f64();
        if secs >= 5.0 {
            let mut all: Vec<(&str, (f64, u64))> = sums.drain().collect();
            all.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
            let total: f64 = all.iter().map(|x| x.1.0).sum();
            tracing::info!("GPU profile: {:.1} ms/s of draws and dispatches over {:.1} s", total / secs, secs);
            for (name, (ms, n)) in all.iter().take(40) {
                tracing::info!("GPU profile: {:8.1} ms/s {:7.0} dispatches/s  {name}", ms / secs, *n as f64 / secs);
            }
            since = std::time::Instant::now();
        }
    }
}
