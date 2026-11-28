//! (Diagnostics) Which physical page a title address shows, as the HLE's
//! memory module knows it (`victory_hle::modules::xmem::physical_of`, given
//! at start-up): to tell host copies that hold the same physical memory
//! through different addresses (the console maps one physical page at many
//! addresses: page-mapping copies, banks, read-only aliases).

use std::sync::OnceLock;

static PHYSICAL: OnceLock<fn(usize) -> Option<(usize, usize)>> = OnceLock::new();

/// Gives the lookup: an address to (the section, the page in it).
pub fn set_physical(f: fn(usize) -> Option<(usize, usize)>) {
    let _ = PHYSICAL.set(f);
}

/// The physical page `at` shows, if the lookup is given and `at` is in a view.
pub(crate) fn physical_of(at: usize) -> Option<(usize, usize)> {
    PHYSICAL.get().and_then(|f| f(at))
}
