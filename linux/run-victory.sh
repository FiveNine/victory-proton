#!/usr/bin/env bash
# Runs the VIctory (Belobug AMD) fork build under GE-Proton (vkd3d-proton for D3D12).
# Usage: ./run-victory.sh                 -> runs the game
#        ./run-victory.sh --background    -> runs it hands-off (keyboard ignored)
#        ./run-victory.sh <victory args>  -> e.g. ./run-victory.sh config
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
# VICTORY_PROTON picks the Proton: "experimental" (Steam's Proton -
# Experimental, newest vkd3d-proton), "11" (Proton 11.0) or "ge" (GE-Proton10-34).
STEAM="$HOME/.local/share/Steam"
case "${VICTORY_PROTON:=ge-victory}" in
  experimental) PROTON="$STEAM/steamapps/common/Proton - Experimental/proton" ;;
  11) PROTON="$STEAM/steamapps/common/Proton 11.0/proton" ;;
  ge) PROTON="$STEAM/compatibilitytools.d/GE-Proton10-34/proton" ;;
  # GE-Proton10-34 with one wineserver change, wine-mirror/wine 3217449b1f
  # (memory views in a tree, not a list): with the title's tens of
  # thousands of streamed views each map walked them all, holding Wine's
  # memory lock. Built from GE's own patched source, victory-data/.
  # The default (2026-11-26: the first in-world cinematic 2-8 -> 15-28 fps,
  # a page map 294 -> 13 us); VICTORY_PROTON=ge is the stock one.
  ge-victory) PROTON="$HERE/victory-data/GE-Proton10-34-victory/proton" ;;
  *) echo "unknown VICTORY_PROTON=$VICTORY_PROTON" >&2; exit 1 ;;
esac
# VICTORY_BUILD=release for the plain build; diag has the debugging switches.
# VICTORY_FORK=victory-fork for the older fxceless fork.
# VICTORY_FORK=victory-dexnom for the DexNom fork (a release build).
# VICTORY_CONSOLE=series-s makes the title budget less memory (Series S).
FORK="${VICTORY_FORK:-victory-7900xtx}"
case "$FORK" in victory-dexnom) BUILD_DEFAULT=release ;; *) BUILD_DEFAULT=diag ;; esac
EXE="$HERE/$FORK/target/x86_64-pc-windows-msvc/${VICTORY_BUILD:-$BUILD_DEFAULT}/victory.exe"
GAME="$HERE/GTA6.Xbox.v1.0.2.1"

export STEAM_COMPAT_CLIENT_INSTALL_PATH="$HOME/.local/share/Steam"
# One prefix per Proton (GE keeps the original one).
if [ "${VICTORY_PROTON:-ge}" = ge ] || [ "${VICTORY_PROTON:-ge}" = ge-victory ]; then
  export STEAM_COMPAT_DATA_PATH="$HERE/proton-prefix"
else
  export STEAM_COMPAT_DATA_PATH="$HERE/proton-prefix-$VICTORY_PROTON"
fi
mkdir -p "$STEAM_COMPAT_DATA_PATH"

# Windows-style paths for the tool (Proton maps / to Z:).
winpath() { echo "Z:${1//\//\\}"; }
export VICTORY_GAME_DIR="$(winpath "$GAME")"
# Each fork keeps its own shader/pipeline cache (and crash list); saves are shared.
CACHE="$HERE/victory-data/cache"
[ "$FORK" = victory-amd ] || CACHE="$HERE/victory-data/cache-$FORK"
# VICTORY_CACHE_PATH overrides it (experiments with a scratch cache).
CACHE="${VICTORY_CACHE_PATH:-$CACHE}"
export VICTORY_CACHE_DIR="$(winpath "$CACHE")"
export VICTORY_DATA_DIR="$(winpath "$HERE/victory-data/data")"
mkdir -p "$CACHE" "$HERE/victory-data/data"

export VICTORY_LOG="${VICTORY_LOG:-info}"
# Fork speed switches (see README); unset these to compare against default.
export VICTORY_RELAY="${VICTORY_RELAY:-2}"
# As the AMD fork's README runs it.
export VICTORY_PRESENT_TARGET="${VICTORY_PRESENT_TARGET-1x1:1}"
export VICTORY_HAIR="${VICTORY_HAIR-1}"
# Shaders that crash RADV in pipeline creation (vertex 089d6271be6650ef,
# mesh d4111a6e6ea49d91, compute 4e296b1885c37b29)
# (Wine "Assertion failed" dialogs on entering the world); its draws are left out.
export VICTORY_SKIP_PIPELINES="${VICTORY_SKIP_PIPELINES-089d6271be6650ef,d4111a6e6ea49d91,4e296b1885c37b29}"
# Those three skips, VICTORY_PRESENT_TARGET and VICTORY_HAIR are for the AMD
# fork on the old vkd3d; DexNom needs none of them.
if [ "$FORK" = victory-dexnom ]; then
  unset VICTORY_SKIP_PIPELINES VICTORY_PRESENT_TARGET VICTORY_HAIR
fi
# The 7900XTX fork: its scripts/play.ps1 settings (not -Safe), each one
# overridable from the environment. Plus CARD_BUFFERS_MIN_KB=64: every title
# buffer's copy in the card's memory, not only those of 4 MiB and more
# (2026-11-26: world 21.9 -> 23.0-23.8 fps over five and two runs).
if [ "$FORK" = victory-7900xtx ]; then
  unset VICTORY_PRESENT_TARGET VICTORY_HAIR
  # The title's "dummy" ray-tracing resolves (shader_binning_utils:
  # CS_DummyResolve, three variants) wrote black over the ray-traced
  # lighting and reflections here: rooms far too dark, black tiles on
  # windows and pictures (2026-11-27, interrogation scene). Left out.
  export VICTORY_SKIP_PIPELINES="${VICTORY_SKIP_PIPELINES_7900:-44a9625f01769f33,38b8a084895dbeae,a9c43aa1a29e7b32}"
  for kv in TEXTURE_NO_WAIT=1 RELAY=2 WIDE_BACK=1 CARD_REHASH_RUNS=1 JOIN_COPIES=1 \
            INDIRECT_WINDOW=1 RELAY_COPY=1 CARD_STEPS=2 INDIRECT_BATCH=2 MAP_THREADS=1 \
            TITLE_PRIORITIES=0 CARD_AWAKE=1 SHOW_AFTER=1 AMBIENT=0.8 D3D12_DRED=light \
            THIN=1 JOIN_PARTS=8 UPLOAD_AHEAD=1 CARD_BUFFERS_MIN_KB=64; do
    k="VICTORY_${kv%%=*}"
    [ -n "${!k+x}" ] || export "$k=${kv#*=}"
  done
fi

# --background: ignore the real keyboard so you can use the PC, and press
# Enter every 4 s for the first 4 minutes to get through the menus.
if [ "${1:-}" = "--background" ]; then
  shift
  export VICTORY_KEYBOARD=0
  export VICTORY_KEYS="${VICTORY_KEYS:-$(seq 15 4 240 | sed 's/$/:Enter/' | paste -sd,)}"
fi

# Keyboard as a pad (WASD, Enter = A, Backspace = B, Esc = Menu; README),
# read without the focus check that fails under XWayland.
# VICTORY_PAD=1: a virtual controller (sudo python3 keyboard-pad.py) drives
# the game, so it does not read the keyboard itself.
if [ "${VICTORY_PAD:-0}" = 1 ]; then VICTORY_KEYBOARD=0; fi
export VICTORY_KEYBOARD="${VICTORY_KEYBOARD:-always}"
# The title's own watchdog ends it after ~30 s without a frame (long shader
# compiles do that here); builds with Marvisak/VIctory#454 can switch it off.
export VICTORY_NO_WATCHDOG="${VICTORY_NO_WATCHDOG-1}"
# How long a frame may wait for pipelines still being made (DexNom: 12 s by
# default, for Windows). Here many never finish (RADV crashes or hangs on
# them), so frames would crawl. 50 ms: with 500 a frame stood half a second
# whenever a new area's pipelines were being made (cold-cache test
# 2026-11-26: median 11.3 fps with 500, 18.1 with 50, same draws dropped).
export VICTORY_PIPELINE_WAIT_MS="${VICTORY_PIPELINE_WAIT_MS:-50}"
# Translated loops stop after this many iterations. 0: the 7900XTX fork's
# own limits by stage (pixel 128, vertex/mesh 256, compute 4,096). 65536 (set
# for DexNom, whose default was 2^20) let rays caught in broken structures
# loop 16-500 times longer: with ray tracing on (VICTORY_BVH_BUILDER=1) the
# cinematic went 2.3 -> 7.6 fps with 0, and 20.9 -> 21.9 fps without it
# (2026-11-26).
export VICTORY_SHADER_LOOP_LIMIT="${VICTORY_SHADER_LOOP_LIMIT:-0}"
# Memory caps (2026-11-26): a 10-minute session with ray tracing ran the
# card's 16 GB and the 15 GB of system memory it may use both full ("Not
# enough memory for command submission", device lost). Copies of title
# memory outside buffers had grown 4.3 -> 8.6 GB in four minutes, and the
# ray-tracing structures' memory on the card may take 4 GB.
export VICTORY_WINDOW_BUDGET_MB="${VICTORY_WINDOW_BUDGET_MB:-4096}"
export VICTORY_MEMORY_CARD_MB="${VICTORY_MEMORY_CARD_MB:-2048}"
# Output 1080p rendered at 1080p (the title's own: 4K rendered at 1440p).
# 200 s with ray tracing: 22.5 fps against 20.0, the heavy last part 22.2
# against 14.6. VICTORY_OUTPUT=3840x2160 VICTORY_RENDER_SCALE=0.6667 for 4K.
export VICTORY_OUTPUT="${VICTORY_OUTPUT:-1920x1080}"
export VICTORY_RENDER_SCALE="${VICTORY_RENDER_SCALE:-1.0}"
# vkd3d-proton master's VK_EXT_descriptor_heap path stays on: it gives a
# 2,097,152-view heap (1,000,000 without it), so the 7900XTX fork's
# per-draw tables are not refused (~1,500 indirect draws a frame were
# dropped). The address-0 faults once blamed on it were unset root views,
# now bound to zeros. VKD3D_DISABLE_EXTENSIONS=VK_EXT_descriptor_heap: off.
# RADV's compiled shaders: Mesa's cache defaults to 1 GB and this game fills
# it (1.1 GB on 2026-11-26), so shaders of places seen before were evicted
# and compiled again on every visit (objects invisible for seconds, frames
# starved). 16 GB keeps them.
export MESA_SHADER_CACHE_MAX_SIZE="${MESA_SHADER_CACHE_MAX_SIZE:-16G}"

if [ $# -eq 0 ]; then
  set -- run "$(winpath "$GAME/game_scarlett_master.exe")" --execute --console "${VICTORY_CONSOLE:-series-x}"
fi
# First run: let Proton build the prefix (installs vkd3d-proton / DXVK).
if [ ! -d "$STEAM_COMPAT_DATA_PATH/pfx" ]; then
  "$PROTON" run cmd /c exit >/dev/null 2>&1 || true
fi

# Then call Proton's own Wine directly so console output is kept.
export WINEPREFIX="$STEAM_COMPAT_DATA_PATH/pfx"
# Microsoft's HLSL compiler (copied from The Witcher 3's install into
# victory-data/dlls): Wine's own cannot build VIctory's helper shaders
# (indirect clamp, depth twins, ambient light, rectangle lists).
DLL="$HERE/victory-data/dlls/d3dcompiler_47.dll"
[ -f "$DLL" ] && cp -u "$DLL" "$(dirname "$EXE")/"
# vkd3d-proton from Proton Experimental (build 5679) next to victory.exe:
# GE-Proton10-34's (5122) translates some of the title's shaders to invalid
# SPIR-V that crashes RADV, and a crash mid-compile leaves the driver's locks
# held, so later pipelines never finish (black world). But with it the
# process grew 6 -> 30 GB in 15 s as the world's shaders were translated
# (2026-11-25), so the default is the running Proton's own (VICTORY_VKD3D=new
# for the newer one); the crash list takes care of the old one's crashes.
# VICTORY_VKD3D=5153 tries Proton 11.0's build (between the two).
# Default: vkd3d-proton built from source (master, 2026-11-26): no RADV
# crashes or hangs on the title's shaders, no per-pipeline memory blow-up.
case "${VICTORY_VKD3D:-master}" in
  new) VKD3D="$HERE/victory-data/dlls/vkd3d-5679" ;;
  5153) VKD3D="$HERE/victory-data/dlls/vkd3d-5153" ;;
  master) VKD3D="$HERE/victory-data/dlls/vkd3d-master" ;;
  *) VKD3D="" ;;
esac
if [ -n "$VKD3D" ] && [ -d "$VKD3D" ]; then
  cp -f "$VKD3D"/d3d12.dll "$VKD3D"/d3d12core.dll "$(dirname "$EXE")/"
else
  rm -f "$(dirname "$EXE")/d3d12.dll" "$(dirname "$EXE")/d3d12core.dll"
fi
export WINEDLLOVERRIDES="d3d12,d3d12core,dxgi,d3d11,d3d10core=n,b;d3dcompiler_47=n,b"
export WINEFSYNC=1 WINEDEBUG="${WINEDEBUG:--all}"
LOG="$HERE/victory-data/last-run.log"
echo "log: $LOG"
# Wine ignores the below-normal priority VIctory gives its pipeline workers,
# so 18 compile threads ran at the game's own priority and starved it while
# a new area's shaders were made. Linux lets us lower our own threads: they
# can get nice VICTORY_COMPILE_NICE (off by default: at 10 a cold-cache test
# ran worse, median 10.7 fps vs 18.1, as pipelines came later).
NICE="${VICTORY_COMPILE_NICE:-0}"
if [ "$NICE" != 0 ]; then
  (
    pid=""
    for _ in $(seq 60); do
      for p in $(pgrep -f 'victory\.exe'); do
        case "$(tr '\0' '\n' < /proc/$p/cmdline 2>/dev/null | head -1)" in *victory.exe) pid=$p ;; esac
      done
      [ -n "$pid" ] && break
      sleep 1
    done
    while [ -n "$pid" ] && [ -d /proc/$pid ]; do
      for t in /proc/$pid/task/*; do
        case "$(cat $t/comm 2>/dev/null)" in
          victory-pipelin*) renice -n "$NICE" -p "${t##*/}" >/dev/null 2>&1 ;;
        esac
      done
      sleep 3
    done
  ) &
fi
"$(dirname "$PROTON")/files/bin/wine" "$EXE" "$@" 2>&1 | tee "$LOG"
