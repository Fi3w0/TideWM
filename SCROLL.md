# Scroll engine — initial implementation

Scroll is TideWM's experimental third spatial engine. Each monitor has its own
strip; windows never become part of a strip spanning physical monitors. Window
ownership, groups, protocols and the render/effect pipeline remain shared.

## Try it

Build/install through your normal packaging workflow, then select the engine:

```sh
tidectl engine scroll
tidectl engine classic
tidectl engine ocean
```

Switching changes the running session without relogging and does not edit config.
A subsequent reload follows the `engine` selected in the file. Locked sessions,
exclusive keyboard layers and active grabs block a switch. No keys are inserted.

To select Scroll in the configuration:

```wave
engine = scroll
scroll {
    workspaces = separate
    orientation = horizontal
    tile_proportion = 0.5
    center_focused = false
    gesture_distance = 1200
}
```

`workspaces = separate` retains an independent strip position for each numbered
or named workspace. Use only one workspace on each output for a simple two-strip
workflow. `sections` presents the output's tiled workspace contents consecutively;
selecting a workspace navigates to its section. Names remain the existing
workspace names. Sections have single workspace ownership, not overlapping tag
filters. Scratchpads stay separate. Floating windows keep their current workspace
overlay behavior rather than being translated into the tiled strip.

The primary axis can be horizontal or vertical. Both axes at once belong to
Ocean's 2D model and are not implemented by Scroll. The default tile proportion
is half the usable viewport; opening more tiles does not reduce existing widths.
`center_focused` optionally centers a focused tile; otherwise only enough of the
view moves to reveal it. The extent can be changed with existing keyboard resize
actions along the strip axis. Viewport movement uses existing window-motion
animation settings; direct gesture updates follow the finger.

Existing directional focus actions navigate tiled strip neighbors. The
`scroll-left/right/up/down` actions move half a viewport along the selected axis.
The configured workspace-swipe finger count controls direct scrolling when no
explicit swipe action takes priority. Cancelling restores its starting offset.
Gesture distance is configurable sensitivity, independent of output resolution.

## Engine and feature boundaries

Classic and Scroll share the existing workspace registry and membership trees;
Scroll adds viewport and requested-size metadata. Returning to Classic restores
the retained BSP structure. Ocean conversions use the existing reef migration,
including its loss of model-specific camera/bookmark history. All three engines
produce the shared placed-window scene. Offscreen tiles retain live protocol
ownership; output rendering and pointer lookup filter by their owning monitor.

Existing floating windows, pins, fullscreen, tab groups, Depth Deck and effects
use the shared paths. This is an initial scrolling policy, not all of niri's
column interactions: multi-window stacked columns, consume/expel operations,
scroll-specific pointer resizing, drag insertion previews and velocity-based
release snapping are not implemented. Tiled drag currently retains TideWM's
existing swap/transfer behavior. Per-section floating canvas placement and an
all-engine unified overview need further design/work.

## Activation and notifications

`tidectl focus-window ID` reveals an exact live window; `focus-app APP_ID` picks
the most recently focused matching app, including hidden workspace windows.
`tidectl --json resolve-window --pid PID` finds a notification producer's nearest
window-owning ancestor. Exact window/app selectors can narrow the result.
Ambiguous terminal servers return candidate IDs instead of guessing a session.
See DOCUMENTATION.md for wire requests and mapping-lifetime limits.

Notification presentation stays in the shell. Abyssal's companion change supports
an explicit window-ID hint, app-ID hint or exact desktop-entry/app-ID match;
application-provided actions keep priority. Its optional `tide-notify` script
resolves a target before sending the notification. Claude hooks are not installed
by this compositor change.

## Verification

- Geometry/protocol-state regressions exercise independent output offsets,
  sections, scratchpad separation, small/extreme coordinates, hidden activation,
  ambiguity, lock refusal and all six directed engine transitions.
- Private virtual-host nested GLES checks use actual GTK clients and captures:
  stable tile sizes during scrolling, horizontal/vertical navigation, sections,
  hidden workspace activation, PID lookup, group-tab migration, scrolling away from a fullscreen column, fullscreen
  restoration and shared frost captures. The native desktop is not involved.
- Physical dual-output drag/scale/hotplug, actual touchpad gesture feel and the
  complete feature matrix remain native validation work. Nested winit exposes
  only one output, so it cannot establish those results. No CPU/GPU or memory
  improvement is claimed from a debug rendering run.

Reference implementations inspected: niri `e5f9cf628884c6c31d6083f5723403b790bae881`
(`layout/scrolling.rs`, workspace/monitor activation) and AngelOS
`672857addc5f647a2202433c561212b181e30965` (dock/window-ID activation). The code uses
TideWM's existing Smithay pin and dependencies; niri is not added as a dependency.
