# victory-proton

[VIctory](https://github.com/Marvisak/VIctory), the compatibility layer that runs
an Xbox Series title from files the user supplies, running on **Linux under
Proton**, ray tracing included.

Built on [avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX)
(its original README: [docs/README-7900XTX-fork.md](docs/README-7900XTX-fork.md)),
with the changes Linux needed and a much faster ray-tracing path.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's developer
or publisher.

## Where it stands (2026-11-26)

Tested on one machine: Arch Linux, KDE Plasma (Wayland), Ryzen 9 9950X3D,
30 GB RAM, Radeon RX 9070 XT (RADV, Mesa 26.3), GE-Proton10-34, 4K output,
the Series X profile.

| | Ray tracing off | Ray tracing on |
|---|---|---|
| Prologue, first scene (prison) | ~27 fps | ~17-20 fps |
| Open world, first boat ride | | ~20-25 fps |
| First in-world cinematic | ~21 fps | ~13 fps |

At the start of the day ray tracing ran at 2-3 fps in the world and ~5 in the
cinematic, and streaming stalled the cinematic at 2-8 fps even without it.
With ray tracing off some ~340 draws a frame that need it are left out, so
ray tracing on is the complete picture.

What changed:

- **Streaming:** Wine's server kept a process's memory views in a list that
  every map walked under Wine's memory lock; the title has tens of thousands
  of streamed 64 KiB views. [A wineserver patch](linux/wineserver-views-rbtree.patch)
  (as upstream Wine `3217449b1f`, in no Proton release yet): a page map
  294 -> 13 us, the cinematic 2-8 -> 15-28 fps. The runtime also records its
  own placeholder reservations (`473d815`): Wine has no
  `NtQueryVirtualMemory` class 7, and the failing test cost 10,000 queries a
  frame under the same lock.
- **Ray tracing**, where the console's hardware node test is emulated in
  shaders:
  - `5e013ab`: a node is read in four 16-byte loads instead of 28 single
    dwords (vkd3d-proton only vectorizes a raw load whose offset is visibly
    `(x << 4) + 16k`).
  - `8eaee79`, `91184ce`: a node, and each access to title memory by address,
    runs once per copy the wave's lanes name, with that copy's view the
    wave's. Given per lane, RADV put every load in a loop of its own around a
    descriptor read, one after another.
  - `2baa1dd`, `f81abde`: walks through ray-tracing structures stop after
    1,024 steps (rays sent into broken structures walked 4,096 and held their
    waves) and end with what they found instead of leaving the program
    without writing their pixel.
- `6eb38e3`: programs that trace rays store a NaN as 0 (black blocks spread
  over reflections on windows, framed pictures and floors).
- Merged avatarmanz's main again and, from [Belobug's AMD fork](https://github.com/Belobug/VIctory-Belobug-AMD)
  (an RX 9070 XT build like this one): streamed textures capped only when VRAM
  is over 92 % full, a pooled indirect buffer, descriptor slots given back
  sooner (scenes of 10,000+ draws), the texture upload fix for flicker, its HUD.
- `1390a72`: 16-bit data of integer views in descriptor tables keeps its bits.
- Diagnostics: `VICTORY_GPU_PROFILE=1` (GPU time by pipeline),
  `VICTORY_LOCK_PROBE=1`, `VICTORY_RT_SKIP=1`.

Known wrong: NPCs' mouths do not move when they talk; the splash video
judders; some reflections are still off (parts of the title's ray-tracing
structures are missing, 600-2,000 pages a frame); lighting is not right
everywhere yet. The CPU side of the memory emulation (~30 ms preparing,
~24 ms copying back a frame) limits both modes now.

## How to run it

See [linux/README.md](linux/README.md): what to build and install
(this code built for Windows with `cargo xwin`, GE-Proton with the wineserver
patch, vkd3d-proton from master, Microsoft's `d3dcompiler_47.dll`), and
[linux/run-victory.sh](linux/run-victory.sh), which sets up the rest.
Ray tracing: `VICTORY_BVH_BUILDER=1 ./run-victory.sh`.

## License

GPL-3.0-or-later, as VIctory ([LICENSE](LICENSE)).
