//! (Diagnostics) `VICTORY_CLOCK_SCAN=<folder>`: snapshots of a range of the
//! title's memory, to find where it keeps values that change over time (its
//! clock of the day). The range is read from `<folder>/range` ("address
//! length", hex), written once the image's base is known (the log's "image
//! mapped" line); each time a file `snap` appears there it is taken away and
//! the range is written to `snap-<n>-f<frame>.bin`.
//!
//! A file `settime` holding `HH:MM` sets the title's clock of the day: the
//! hour, minute and second (u32 each) of the struct at `.data` + 0x437add0
//! of game_scarlett_master.exe v1.0.2.1 (`.data` at RVA 0x67e2000; found
//! 2026-11-28 from phone readings against snapshots, and confirmed by
//! setting 21:30 and 10:10: phone, sky and sun follow). The struct at
//! +0xcfd898 (with milliseconds and minutes of the day) and the floats at
//! +0xdb1e80 and +0x1d007c8 are copies the title makes from it every frame.
//! It is written for 30 frames, then the clock runs on. A file `poke` (lines
//! "offset type value") writes anything in `.data`, every frame while it is
//! there. `range` ("start length", hex) says where `.data` is.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

fn folder() -> Option<&'static PathBuf> {
    static FOLDER: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOLDER.get_or_init(|| std::env::var_os("VICTORY_CLOCK_SCAN").map(PathBuf::from)).as_ref()
}

/// Where `.data` starts, from `range`.
fn data_start(folder: &std::path::Path) -> Option<u64> {
    let range = std::fs::read_to_string(folder.join("range")).ok()?;
    u64::from_str_radix(range.split_whitespace().next()?.trim_start_matches("0x"), 16).ok()
}

/// The clock of the day being set: (hour, minute, frames left).
static SETTING: std::sync::Mutex<Option<(u32, u32, u32)>> = std::sync::Mutex::new(None);

fn set_time(folder: &std::path::Path) {
    if folder.join("settime").exists() {
        let text = std::fs::read_to_string(folder.join("settime")).unwrap_or_default();
        let _ = std::fs::remove_file(folder.join("settime"));
        let mut hm = text.trim().split(':').map(|v| v.trim().parse::<u32>().ok());
        if let (Some(Some(h)), Some(Some(m))) = (hm.next(), hm.next())
            && h < 24
            && m < 60
        {
            *SETTING.lock().unwrap_or_else(|e| e.into_inner()) = Some((h, m, 30));
            tracing::info!(h, m, "clock scan: setting the clock of the day");
        }
    }
    let mut setting = SETTING.lock().unwrap_or_else(|e| e.into_inner());
    let Some((h, m, left)) = *setting else { return };
    let Some(data) = data_start(folder) else { return };
    let at = data + 0x437add0;
    let words: [u32; 3] = [h, m, 0];
    for (i, w) in words.iter().enumerate() {
        let bytes = w.to_le_bytes();
        crate::host::copy_mapped((at as usize + i * 4) as *mut u8, bytes.as_ptr(), 4, true);
    }
    *setting = (left > 1).then_some((h, m, left - 1));
}

/// `poke`: lines "offset type value" (offset in hex from the start of
/// `range`; type u32, f32 or f64), written every frame while the file is
/// there. Read again when it changes.
fn poke(folder: &std::path::Path) {
    static LAST: std::sync::Mutex<(Option<std::time::SystemTime>, Vec<(u64, Vec<u8>)>)> = std::sync::Mutex::new((None, Vec::new()));
    let path = folder.join("poke");
    let Ok(meta) = std::fs::metadata(&path) else {
        LAST.lock().unwrap_or_else(|e| e.into_inner()).1.clear();
        return;
    };
    let Some(data) = data_start(folder) else { return };
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let changed = meta.modified().ok();
    if last.0 != changed {
        last.0 = changed;
        last.1.clear();
        for line in std::fs::read_to_string(&path).unwrap_or_default().lines() {
            let p: Vec<&str> = line.split_whitespace().collect();
            let [off, kind, value] = p[..] else { continue };
            let Ok(off) = u64::from_str_radix(off.trim_start_matches("0x"), 16) else { continue };
            let bytes = match kind {
                "u32" => value.parse::<u32>().ok().map(|v| v.to_le_bytes().to_vec()),
                "f32" => value.parse::<f32>().ok().map(|v| v.to_le_bytes().to_vec()),
                "f64" => value.parse::<f64>().ok().map(|v| v.to_le_bytes().to_vec()),
                _ => None,
            };
            if let Some(b) = bytes {
                last.1.push((data + off, b));
            }
        }
        tracing::info!(writes = last.1.len(), "clock scan: poke read");
    }
    for (at, b) in &last.1 {
        crate::host::copy_mapped(*at as *mut u8, b.as_ptr(), b.len(), true);
    }
}

/// Once a frame.
pub(crate) fn frame(frame: u64) {
    let Some(folder) = folder() else { return };
    set_time(folder);
    poke(folder);
    if !folder.join("snap").exists() {
        return;
    }
    let _ = std::fs::remove_file(folder.join("snap"));
    let Ok(range) = std::fs::read_to_string(folder.join("range")) else {
        tracing::warn!("clock scan: no range file");
        return;
    };
    let mut parts = range.split_whitespace().map(|p| u64::from_str_radix(p.trim_start_matches("0x"), 16).ok());
    let (Some(Some(at)), Some(Some(len))) = (parts.next(), parts.next()) else {
        tracing::warn!(range, "clock scan: range not understood");
        return;
    };
    let len = len.min(256 << 20) as usize;
    let mut bytes = vec![0u8; len];
    crate::host::copy_mapped(bytes.as_mut_ptr(), at as *const u8, len, false);
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
    // The picture of the same moment too (with `VICTORY_CAPTURE`): the
    // clock the title shows (its phone) dates the snapshot.
    crate::capture::ask(crate::queue::presented() + 1);
    let path = folder.join(format!("snap-{n}-f{frame}.bin"));
    let _ = std::fs::write(&path, &bytes);
    tracing::info!(n, frame, at = format_args!("{at:#x}"), len, "clock scan: snapshot written");
}
