# Running it on Linux (Proton)

Tested on one machine: Arch Linux, KDE Plasma (Wayland), Ryzen 9 9950X3D,
30 GB RAM, Radeon RX 9070 XT (RADV, Mesa 26.3), GE-Proton10-34, the Series X
profile. This repository.

## What is needed

- **This branch, built for Windows** from Linux:
  `cargo xwin build --locked --target x86_64-pc-windows-msvc --profile diag -p victory --features diagnostics --bins`
  (the `diag` profile keeps the diagnostics switches the script uses).
- **GE-Proton10-34**, and preferably the wineserver patch below.
- **vkd3d-proton built from master** (`d3d12.dll`, `d3d12core.dll`). The
  builds in GE-Proton and Proton 11 crash or hang RADV on hundreds of the
  title's shaders; Proton Experimental's grew the process to 30 GB in
  seconds. The script copies them from `victory-data/dlls/vkd3d-master`.
- **Microsoft's `d3dcompiler_47.dll`** in `victory-data/dlls/`: Wine's
  cannot build VIctory's helper shaders. It must be a **Windows 10 SDK
  one, version 10.0.x** (tested: 10.0.18362.1 here; 10.0.17134.12 and
  10.0.28000.2705 reported working). Many older games ship the Windows 8.1
  one, **6.3.9600**: it does not know `cs_5_1`, so the indirect argument
  clamp is never made, every indirect draw is dropped and the world is
  black while the menus work (log: `error X3506: unrecognized compiler
  target 'cs_5_1'`, issue #1). The run script checks the version and stops
  on an older one.
- Your own dump of the title.

[`run-victory.sh`](run-victory.sh) sets everything else: the prefix, the DLL
overrides, the fork's speed switches, a 16 GB Mesa shader cache (the 1 GB
default is full after one session, and evicted shaders were compiled again
on every visit), and the keyboard as a pad. It expects the game, the fork
and `victory-data/` next to it; `./run-victory.sh --help` style arguments go
to `victory.exe`.

Ray tracing: `VICTORY_BVH_BUILDER=1 ./run-victory.sh`.

## The wineserver patch

[`wineserver-views-rbtree.patch`](wineserver-views-rbtree.patch): Wine's
server keeps a process's memory views in a list, and every map and unmap
walks it while the process holds Wine's memory lock. The title streams in
64 KiB views and has tens of thousands of them, so each map cost ~300 us and
streaming stalled everything (objects popping in late, the first cinematic at
2-8 fps). The patch keeps the views in a tree as well, as upstream Wine does
since wine-mirror/wine `3217449b1f`, which is in no Proton release yet. It
applies to GE-Proton10-34's own patched Wine source (`server/`); build the
server and put `wineserver` in a copy of GE-Proton
(`VICTORY_PROTON=ge-victory`, the script's default). A page map: 294 -> 13 us;
the cinematic: 2-8 -> 15-28 fps.

## Where it stands (2026-11-26)

| | Ray tracing off | Ray tracing on |
|---|---|---|
| Prologue, first scene (prison) | ~27 fps | ~17-20 fps |
| First in-world cinematic | ~21 fps | ~13 fps |

Ray tracing on was 2-3 fps in the world and ~5 in the cinematic at the start
of the day. With it off some ~340 draws a frame that need it are left out, so
ray tracing on is the complete picture. Commits of this branch:

- `5e013ab`: the ray-tracing node test reads a node in four 16-byte loads
  instead of 28 single dwords (vkd3d-proton only vectorizes a raw load whose
  offset is visibly `(x << 4) + 16k`).
- `8eaee79`, `91184ce`: a node, and each access to title memory by address,
  runs once per copy the wave's lanes name, with that copy's view the wave's.
  Given per lane, RADV put every load in a loop of its own around a
  descriptor read, one after another.
- `2baa1dd`, `f81abde`: walks through ray-tracing structures stop after
  1,024 steps (rays sent into structures the builder did not get right
  walked 4,096 and held their waves), and end of themselves with what they
  found, instead of leaving the program without writing a pixel (bad
  reflections).
- `473d815`: the runtime records its own placeholder reservations (Wine has
  no `NtQueryVirtualMemory` class 7; the test failed and cost 10,000
  queries a frame under Wine's lock).
- `1390a72`: 16-bit data of integer views in descriptor tables keeps its bits.
- `a77c61a`, `9710679`, `4ed1467`, `f8db065`: diagnostics
  (`VICTORY_GPU_PROFILE=1`: GPU time by pipeline; `VICTORY_LOCK_PROBE=1`;
  `VICTORY_RT_SKIP=1`).

Known wrong: NPCs' mouths do not move when they talk; the splash video
judders; some reflections are still off (parts of the title's ray-tracing
structures are missing, 600-2,000 pages a frame); lighting is not yet right
everywhere. The CPU side of the memory emulation (~30 ms preparing, ~24 ms
copying back a frame) is what limits both modes now.

## Tools that helped

- `VICTORY_GPU_PROFILE=1`: GPU time per pipeline (times about 2x too high,
  the ranking holds).
- RADV's real shader code: record pipelines with Steam's Fossilize layer
  (needs `VKD3D_DISABLE_EXTENSIONS=VK_EXT_descriptor_heap`), then
  `fossilize_replay --graphics-pipeline-range 0 0` with
  `RADV_DEBUG=shaders,nocache`.
- The console code of a shader: `victory.exe shader-info --disasm shaders.rpf`
  under Wine, filtered by `#<hash>`.
- `MESA_VK_TRACE=rgp` reset the GPU here: avoid.
