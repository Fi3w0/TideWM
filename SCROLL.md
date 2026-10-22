# Scroll

Scroll is TideWM's third spatial engine, next to Classic and Ocean. Every monitor gets its own strip of apps that you slide along, niri-style. New apps join the strip instead of shrinking the ones already there. Each strip belongs to one monitor: an app never ends up half on one screen and half on another.

Groups, glass, shadows, animations, screenshots and screen sharing all work in Scroll exactly as they do in the other engines.

## Try it

Switch the running session, no relog needed:

```sh
tidectl engine scroll
tidectl engine classic   # or ocean, to go back
```

This doesn't touch your config file. To make Scroll your default, set it in `config.wave`:

```wave
engine = scroll
```

You can also bind the switch, for example `bind $mod+Ctrl+S { engine:scroll }`. A switch is refused while the screen is locked, while a launcher or other exclusive-keyboard layer is open, or during a drag.

## Configuration

Every key is optional, reloads live and is measured against the monitor's usable area. These are the defaults:

```wave
scroll {
    workspaces       = separate     # or sections
    orientation      = horizontal   # or vertical
    tile_proportion  = 0.5          # 0.1 – 1.0 of the viewport
    center_focused   = false
    gesture_distance = 1200
}
```

- **`tile_proportion`** is how much of the screen a new app takes:
  - `1.0` fills the screen;
  - `0.94` leaves a narrow peek of the neighbors on each side;
  - `0.85` shows more of them;
  - `0.5` fits two apps side by side.

  Apps you resized by hand keep their own size.
- **`center_focused`:** with `true`, the focused app always sits in the middle, even the first, last or only one. With `false`, the strip moves only as far as needed to bring the app into view.
- **`orientation`:** strips can run horizontally or vertically. For a free 2D layout, use Ocean.
- **`workspaces`:**
  - `separate` (the default) gives each workspace its own strip with its own scroll position.
  - `sections` lays a monitor's workspaces end to end in one continuous strip, and switching workspace jumps to its section.

  In both modes, scratchpads stay separate and floating windows float above the strip as usual.
- **`gesture_distance`:** how far you swipe on the touchpad to move one screen's worth. The swipe uses your configured workspace-swipe finger count.
- **Spacing** comes from the global `gaps`.

A nearly full-width setup with neighbor peeks:

```wave
scroll {
    tile_proportion = 0.94
    center_focused  = true
}
gaps = 8
```

## Moving around

Scroll uses your existing binds. It adds none of its own.

| Action | In Scroll |
| --- | --- |
| `focus-left/right` (`up/down` for vertical) | Focus the neighbor app and slide it into view |
| `swap-left/right` | Move the focused app along the strip |
| `resize-left/right` (`up/down` for vertical) | Make the focused app narrower or wider |
| `scroll-left/right/up/down` | Slide the strip half a screen |
| Touchpad swipe | Slide the strip with your fingers; let go and it stays |

Focusing an app with the mouse never moves the strip: hovering with `focus_follows_mouse`, clicking, touch and tablet pens all leave it alone. So you can cross to another monitor or click a neighbor's peek without anything sliding away. Keyboard focus, `tidectl focus-window` / `focus-app` and notification clicks do bring the app into view.

When the strip slides under a still mouse, the app now under the pointer gets the wheel and clicks right away, without nudging the mouse.

## Limits

Scroll is a scrolling layout, not a full copy of niri. Not there yet:

- several apps stacked in one column, and consume/expel;
- resizing columns by dragging their edge with the mouse;
- drop previews and velocity snapping when dragging. A tiled drag swaps or moves windows the same way it does in Classic.

Switching between Classic and Scroll keeps your layout trees intact. Switching to or from Ocean converts the layout, and Ocean's camera history and bookmarks don't carry over.

## For tools and scripts

- `tidectl outputs` (and `--json`) includes a `scroll` object per monitor with `offset`, `orientation` and `workspaces`. With centering on, the offset can be negative.
- `tidectl focus-window ID` and `focus-app APP_ID` bring an app into view, even on a hidden workspace.
- `tidectl --json resolve-window --pid PID` finds the window that owns a process, for notification daemons.

Wire-level details: [DOCUMENTATION.md](DOCUMENTATION.md#ipc-and-tidectl).
