# TideWM spatial model

TideWM is one compositor with three spatial engines. The engine decides where windows live and how you move between them. Everything else stays the same whichever one you pick: water, glass, animations, rules, IPC and protocols.

Pick one with `engine = classic|ocean|scroll` in `config.wave`, or switch the running session with `tidectl engine <name>`. Either way, every open window moves across in place, with no restart.

## Classic

Numbered workspaces per monitor, each with its own tiling tree (BSP, master/stack, cascade or floating).

Classic adds depth through the **Depth Deck**, an opt-in per-workspace parking spot (`classic_depth { enabled = true }`):

- **Surface:** your normal tiled and floating windows.
- **Shallows:** windows you haven't touched in a while dim and cool down a little. This is visual only (see `depth { }`) and never changes the layout.
- **Deep Deck:** windows you parked out of the layout. They stay alive and stay on that workspace.

How you use it:

- **Depth Down / Up** rotates the focused tile through the deck, with a vertical pressure wave showing the direction.
- **Dive** opens the whole deck so you can pick any window.
- **Recall:** if a tile is focused, the chosen window swaps into that tile and the old one takes its place in the deck. On an empty workspace, it simply comes back as a tile.

Nothing parks itself automatically.

## Ocean

Ocean has no numbered workspaces. It's one continuous 2D world, and each monitor has its own camera that pans and zooms.

- **Separate by default:** each monitor sees only its own windows and keeps its own tiling. Set `ocean { shared_canvas = true }` and a camera can travel over to another monitor's windows. Windows pinned to a monitor always stay on it.
- **X** is moving sideways between working areas. **Y** is depth, not a stack of disguised workspaces.
- **Reefs** are local tiling areas (BSP, master/stack or cascade) placed in the world. They help organize things but don't fence you in. A move or resize drag lifts a tile out into a freely placed window, and dragging empty canvas pans the camera.
- **Smart tiling:** a tiled drag can swap tiles inside its reef, and a floating window dropped near a tile snaps back into the reef. The target tile lights up while you aim.
- **Touchpad:** `modifier_pan_fingers` gives every mouse drag gesture a touchpad equivalent, no button press needed.
- **Bookmarks** are named places to return to. Tools that expect workspaces can use them.
- **Optional orientation aids:** a light guide grid in empty space, and a small center marker that appears after the camera moves and fades after a few seconds.
- **Depth travel:** Depth Down/Up visits reef origins and windows you placed or sank on purpose; ordinary tile rows are skipped. Sinking, dredging and surfacing are always explicit, and focusing a deep window never drags it to the surface.
- **Compass:** a soft glow at the screen edge points toward urgent windows and deep windows that are off screen.
- **Minimap:** an overview of the whole world.

## Scroll

Each monitor gets a strip of apps you slide along, niri-style. Under the hood Scroll shares Classic's per-monitor, per-workspace window registry. It adds only a scroll position and per-app sizes, so switching between Classic and Scroll keeps your trees, insertion order and groups exactly as they were.

By default each workspace has its own strip. With `scroll { workspaces = sections }`, a monitor's workspaces are laid end to end in one strip. Scratchpads stay separate. Floating windows float above the strip, or with `scroll { float_anchor = strip }` keep their place on it.

Configuration and keys: [TECHNICAL_REPORT.md](TECHNICAL_REPORT.md#scroll-engine).

## Switching engines

- **Classic ↔ Scroll:** the same trees, just laid out differently. Nothing is lost.
- **Classic or Scroll → Ocean:** each monitor's workspaces become reefs on their own island of the world, spaced by your configured gaps. The camera lands on the reef of the workspace you were looking at. Floating windows keep their place, pins become Ocean screen pins, and parked Depth Deck windows are recalled first. Groups and fullscreen or maximized windows carry across.
- **Ocean → Classic or Scroll:** reefs, from left to right, become workspaces 1..N on the monitor whose camera is closest. Floating windows land on the nearest reef, clamped onto the screen. Bookmarks and camera history have no workspace equivalent and are dropped.
- Master/stack and cascade reefs run as BSP inside Ocean. Switching back restores each workspace's layout where it can.
- A switch is refused while the screen is locked, while a launcher or other exclusive-keyboard layer is open, or during a drag. It waits if no monitor is connected. TideWM never guesses a screen size.

## What's shared

All three engines share the compositor core, window registry, Wayland protocols, rendering, effects, input, IPC and accessibility. Each owns only its spatial state:

- **Classic:** workspace trees per monitor, the Depth Deck and restore slots.
- **Scroll:** Classic's trees plus each strip's scroll position and per-app sizes.
- **Ocean:** reef trees, world positions, cameras, bookmarks and depth.

Each engine hands the renderer the same list of placed windows (`src/tide_core/placement.rs`): which window goes where on screen, its zoom, and whether it is tiled, floating or fullscreen. That's why every effect works the same everywhere.

Keybinds always come from your config. Picking an engine or enabling the Depth Deck never adds hidden shortcuts. `Ctrl+Alt+Escape` stays reserved as an emergency way out of a config that locked you out.
