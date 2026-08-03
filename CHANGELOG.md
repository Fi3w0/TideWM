# Changelog

All notable changes to TideWM are documented here. Format loosely follows [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added
- Bioluminescent edge-glow compass for the Ocean engine (spatial roadmap S5's first slice). A window outside the output camera's viewport leaves a soft glow at the viewport edge in its direction: urgent windows glow bright cyan in any direction, physically deep (sunk or lower-reef) windows glow cool blue below. Nearer windows glow brighter; the cue fades to nothing at the configured `max_distance`. The cues are ambient and render-only -- camera travel stays on the existing pan/zoom/bookmark/depth actions, there is no click-to-travel, and no element is produced at all when nothing is off-screen, so an idle desktop ticks zero frames. One analytical GLES pixel shader, no texture or framebuffer, and a 16-cue cap so a crowded world cannot grow the element list. Configurable through the top-level `compass { }` block (`enabled` defaulting true under Ocean, `urgent_color`, `deep_color`, `max_distance`, `size`, `alpha`, `shape`); `water_effects` remains the master bypass. `shape` selects circle/arrow/chevron/ring/diamond for each cue, all drawn analytically in the same shader (no new textures); arrow and chevron point toward the window. Deep cues that are off to the side rather than below the viewport now glow too -- the earlier "deep only points down" restriction was an artifact of the first slice, not an intended limit -- and each cue's screen-space center clamps inside the output bounds so the full glow rect stays visible instead of half-clipped at the edge. The whole-world overview minimap is S5's remaining slice, not built here.
- Ocean now supports direct freeform canvas work rather than requiring every window to stay inside a reef layout. Starting the configured modifier+move or modifier+resize gesture on a reef tile detaches it at the exact same world rectangle and continues as a zoom-aware free drag; `toggle-floating` returns it to local tiling. Dragging genuinely empty canvas pans the camera directly at any zoom. The canvas button is configurable as `left`, `middle`, `right`, or `none`, and may optionally require `pointer_modifier`, so the interaction is a removable preset rather than a reserved hard-coded mouse action.
- `show_config_reload_toast = false` disables successful config-reload confirmations while deliberately retaining parse-error and warning diagnostics.
- `modifier_pan_fingers` touchpad option: a swipe with the configured finger count, held together with `pointer_modifier`, moves/pans exactly like `pointer_modifier`+left-drag, except the two-finger touch itself is the "grab" -- no button press at all. It starts the identical grab the mouse path would (`OceanTileMoveGrab`/`MoveSurfaceGrab`/`TileMoveGrab`/`OceanPanGrab` depending on what's under the touch: a tiled Ocean window with smart tiling swaps reef slots with the same lift-out-and-magnet-highlight behavior as the mouse drag, a tiled Classic window drag-swaps, a floating window moves, empty Ocean canvas pans), driven afterward by synthetic pointer motion built from the gesture's own delta and ended with a direct `unset_grab` rather than a real button release. This required moving `OceanTileMoveGrab`'s and `TileMoveGrab`'s drop/swap commit logic from `button()`'s release detection into `unset()`, so it fires exactly once no matter how the grab ends -- a real button release or this gesture's own `unset_grab` call -- rather than only the one path Smithay happened to reach it through before. The gesture only starts while the pointer isn't already grabbed, the same guard every mouse-driven grab-start already checks: `set_grab` unconditionally tears down and commits whatever grab it supersedes, so without this an unrelated concurrent mouse drag could get swapped/reattached from a stale position instead of a real release. Not yet live-verified: nested winit can't emit touchpad gesture events at all, the same standing limitation as swim and the other gesture-driven paths.

### Changed
- Completed the standalone udev/DRM hardware pass for the water and decoration stack on AMD: the wave workspace transition, map/focus ripples, animated gradient borders, rounded clipping, drop shadows, the open/close/move window animations, and frost glass all render correctly on the real backend, and interactive viscosity passed a first hands-on drag check at the default 1.0. The same session gave swim its first real-touchpad pass: a 3-finger horizontal swipe pans continuously across workspaces and settles cleanly. PSS stayed flat across the pass (135,414KiB against a 135,016KiB idle baseline, 0KiB swap). The water-glass shader variant itself (the `opacity`-rule path, as opposed to the frost path that shares its capture/glass pipeline) was not separately exercised and still owes its own hardware look.
- Verified the Classic Depth Deck on real AMD hardware (udev/tty session, Classic engine): `depth-down` parking, the `dive` overlay, and `depth-select` recall all work. The session also exercised the deliberate v1 exclusion live -- a focused floating window makes `depth-down`/`depth-up`/`sink-window` no-op by design, with recall through the deck overlay still available.
- TideWM-owned toast and configuration-warning UI is now modern rounded compositor chrome with a compact status mark, hierarchy, soft shadow, and themed border instead of a flat pill/full-width alert strip. Its panel, accent, urgent, and text colors derive automatically from the configured inactive/active/urgent border gradients; text switches light/dark from computed luminance, and corner radius follows configured window rounding. Theme reloads therefore recolor subsequent compositor UI without a separate fixed popup palette. A parse failure uses the persistent diagnostic alone rather than stacking a duplicate timed toast over its corner.

### Fixed
- `resize-to-monitor` sized the focused Ocean floating window to the output's raw resolution, pushing its border stroke past the visible screen edge -- the border draws outside the window's configured rect, not inside it. Now insets the target rect by `gaps`, the same margin `toggle-border-fullscreen` already applies to its own target.
- A dragged tiled Ocean window was rendering fixed at its original slot for the whole gesture, only ever appearing to move at the moment of the drop. Smart tiling keeps modifier-left drags of reef windows inside the tiling tree for tile-to-tile swaps (added this same span), and the tree stays frozen for the gesture's duration so the drop math has a stable slot to compare against -- but Ocean's renderer reads placement from that tree, never from `Space`'s live position, the same "`Space` is an input/protocol cache in Ocean, not spatial ownership" rule the S3 foundation established. The drag wrote its live position into `Space` (fine for hit-testing) but nothing carried it into the render path. `OceanSpace::set_tile_drag` now overrides the dragged window's placement rectangle directly for the gesture's duration (the same mechanism a genuinely floating window's own rect already gets), lifting it to the front as `PlacementKind::Floating` for the drag so it renders like it's picked up out of the grid -- note for anyone with `floating_only = true` on border/shadow/rounding: those now apply to the dragged window only while the drag is in progress, which is the intended "lifted out" read but is a side effect of the kind flip worth knowing about. The current swap target gets the existing active-border gradient as a magnet-style highlight -- no new shader or texture, the same per-window border element reused for a different reason. `smart_tiling`, `smart_tiling_snap_distance`, and `smart_tiling_preserve_size` remain the configuration surface. Floating windows released near a reef tile can still reattach automatically, with an optional per-window size preserved around the new slot -- this reattachment also had its own bug: the pointer-proximity check compared `distance_to_rect`'s squared result against `smart_tiling_snap_distance`, a linear screen-pixel threshold, so in practice a floating window would only reattach with the pointer within roughly the threshold's square root of the target instead of the configured distance. Fixed by taking the real distance at that one call site; `distance_to_rect`'s other callers only ever rank candidates against each other and are unaffected. Compile, clippy, fmt, and the 211-test suite pass; the drag itself is not live-verified -- nested winit provides no pointer-drag injection, the same documented limitation viscosity, connected-vessel resize, sway, and cascade resize all carry. Real-hardware verification remains.
- A successful config reload with lint warnings (a dropped keybind, a footgun) no longer stacks a timed "Configuration reloaded" toast on top of the persistent warning panel in the same corner. A hard parse failure already suppressed the toast for exactly this reason; the equivalent success-with-warnings path never had the same guard until now.
- Ocean now anchors an empty implicit starting reef to the current camera
  before its first window is inserted. Interactive Ocean move/resize viscosity
  also converts its world-space sample through the camera zoom before
  rendering, so floating windows keep their size while the canvas is zoomed.
- Ocean now materializes an implicit starting reef when the configured `home`
  bookmark sits outside every explicit reef. New windows therefore land in the
  visible starting area instead of being placed in a distant reef while the
  camera is still at an empty bookmark location.
- Ocean floating-window stacking is now an explicit deterministic front-to-back list. This makes freely overlapping windows raise on click/drag reliably instead of exposing `HashMap` iteration order as accidental z-order.
- Fixed a real-hardware freeze on releasing a drag over an Ocean floating window: `sync_visible_floating_window`'s Ocean branch called `primary_output()`, which unconditionally reads `pointer.current_location()`, from `MoveSurfaceGrab::unset()` -- fired synchronously from inside `PointerHandle::button()`'s own dispatch, which already holds the pointer's internal borrow. Self-reentrant deadlock, the same shape as the 0.15.1 `TileMoveGrab` incident, just a different call path into it: freezing the whole machine, no coredump, no VT-switch response, confirmed live via a nested repro and a symbol-resolved `gdb` backtrace of the hung process (`MoveSurfaceGrab::button` -> `unset` -> `sync_visible_floating_window` -> `primary_output` -> `current_location`). The Classic branch beside it only hit this same `primary_output()` fallback in a rare off-screen-drag edge case, which is why ordinary floating-window dragging never surfaced it -- Ocean's branch called it unconditionally on every drag release, and nothing exercised that live until `freeform_windows` made Ocean dragging common. Fixed by resolving the owning output from the window's own just-computed `rect` via `output_for_window` instead, which needs no pointer access at all and is arguably more correct regardless (a floating window's entry output should follow the window, not wherever the pointer happens to sit at release). Re-verified with repeated real drag/release cycles against the fix, no hang.

## [0.90.0] - 2026-08-01

Second major pre-release. `0.60.0` marked the feature-complete WM foundation
before visual work began. This milestone closes the accumulated R0-R3
render/identity/mechanical/API roadmap, Cascade and continuous Swim, the shared
placement architecture, and both Classic Depth and the Ocean engine through
S4's zoomable canvas navigation. The unreleased span contains 44 changelog
entries (29 additions, 14 fixes, and the source-tree reorganization). `1.0.0`
remains reserved for the Ocean compass/whole-world overview and broader
real-hardware stabilization rather than being inferred from commit count.

### Added
- Completed spatial-roadmap S4 as a genuinely zoomable Ocean canvas rather than vertically translated workspaces. Every output camera now has a continuous two-axis origin and scale; pointer-modifier scrolling zooms around the cursor, keyboard zoom uses the viewport center, and pan/bookmark/focus travel glides with a configurable small perpendicular current. The shared placement boundary now carries camera scale through rendering, capture, rounded decorations/effects, hit testing, floating move/resize grabs, fullscreen/maximize overrides, and screen pins. An optional analytical world grid behind windows moves and changes density with the camera, keeping scale and travel readable across empty wallpaper without a texture, framebuffer, or idle animation; `canvas_guides`, zoom, camera animation/sway, and structural depth all have independent bypasses. A small center marker appears only after camera movement and fades across a configurable 4.2 seconds of inactivity. `depth-down`/`depth-up` navigate meaningful physical Y coordinates only: reef origins and explicitly floating/sunk windows. A first nested pass caught local BSP tile-row Y values being treated as stops—the exact disguised-vertical-workspace failure the design forbids—and they are now excluded. `sink-window` puts the focused window just below the current output-derived viewport, `ocean-dredge-window` pulls the nearest lower floater into view, and `ocean-surface-window` returns one to world Y=0; focusing never auto-surfaces. IPC outputs now include `camera_zoom`. Every Ocean action is exposed for explicit Waves binding; enabling Ocean never inserts a hidden chord. The final 945×1018 release nested pass verified pan/zoom, marker appearance and expiry, topmost floating hover focus, empty-canvas unfocus, bare-key binding, sink/depth/dredge/surface, and clean shutdown with three real Kitty clients. The complete suite passed 201 non-socket tests (three unchanged IPC socket tests are denied by the restricted sandbox), clippy is warning-free, and the running session measured 96,647KiB PSS with 0KiB swap.
- Completed spatial-roadmap S3's Ocean foundation as a real second WM engine. `spatial_engine = classic|ocean` selects spatial ownership at startup; reload keeps the live engine and configured world shape, warning that a restart is required rather than moving windows between models. New `OceanSpace` owns continuous two-axis world geometry, named local BSP reefs, floating world rectangles, independent per-output cameras, and configured/runtime bookmarks. Omitted reef dimensions derive from real logical output sizes and expand for larger outputs instead of assuming 1920x1080. Ocean XDG map/unmap/remap state feeds the S2 `PlacedWindow` boundary directly, so both backends, captures, glass/depth, decorations, and animations see camera-translated content without any vertical-workspace emulation; pointer hit testing uses the same camera placement. `ocean-pan-*`, `ocean-bookmark:<name>`, and session-only `ocean-save-bookmark:<name>` actions provide the first keyboard control. Existing `workspace:N` binds become numeric reef/bookmark compatibility jumps in Ocean, not pages. Per-output camera/entry hints are cleaned or retargeted on hot-unplug. Reefs default to actual output geometry and remain locally tiled for fast work rather than copying DriftWM's native-size snap-cluster model.
- Completed spatial-roadmap S2's shared placement boundary. `src/tide_core/placement.rs` now defines the model-neutral `PlacedWindow` scene record shared rendering consumes: a window handle, global logical rectangle, fractional per-output view transform, authoritative-versus-preview role, committed-versus-fit content sizing, tiled/floating geometry class, and normal/fullscreen stack band. Classic is the first producer, translating its active Smithay `Space` plus bounded swim-neighbor previews into one front-to-back placement list. Both visible backends, screenshots/screencasts, water/frost backdrop capture and replacement, automatic visual depth, shadows/borders/rounding/window animations, and grouped tab strips now consume that same list instead of rediscovering workspace ownership and camera offsets independently. This keeps Classic workspaces and swim details outside the renderer and gives Ocean a concrete producer boundary for S3 without duplicating the visual pipeline. The scene is rebuilt from existing bounded state per frame; it adds no retained window registry, texture, framebuffer, or idle work. Two focused tests cover final view-transform rounding, alongside the existing swim/render suites.
- Added the opt-in Classic Depth Deck, the first implementation slice of TideWM's approved two-engine spatial design. `classic_depth { enabled = false }` is independent of automatic visual `depth { }` and all water effects. `depth-down`/`depth-up` are the workspace-like fast path: they rotate the focused tile through its workspace's deep windows in opposite directions, with the first Down parking the focused window when the deck is empty. Every action must be bound explicitly in Waves; while the feature is disabled, declared depth actions are removed from keyboard matching rather than swallowing keys as no-ops. `sink-window` remains the explicit park action, while `dive`, `depth-next`/`depth-prev`, `depth-select`, and `depth-cancel` provide a static title-card navigator. Direct moves use a new optional vertical pressure-wave transition (top-to-bottom for Down, bottom-to-top for Up): one analytical full-output shader element with undulating wake bands and bubble glints, no captured texture/framebuffer, configurable duration/color/alpha, and no idle frame cost. Recall swaps the selected deep window into the focused tile's exact layout leaf and parks the displaced window in the same deck slot, or inserts it normally when no compatible tile is focused. Grouped and geometry-override windows are deliberately unchanged in v1. Parked-window lifecycle is integrated with xdg unmap/destruction and preferred-output lookup, and hot-disabling the feature restores every parked window to its owning workspace tree. The separate future Ocean engine remains workspace-free; it does not replace Classic.
- Completed spatial-roadmap S0's continuous lateral workspace bridge. `SwimCamera` (`src/visual/swim.rs`) still keeps the existing per-output numbered workspace model authoritative, but `swim.neighbors` is now consumed: during a pan, only adjacent workspace strips intersecting the viewport are assembled from `Layouts` and `floating_workspace` and drawn alongside the active strip. Tiled, floating, fullscreen, maximized, pseudo-tiled, and grouped-window tab-strip content therefore slides into view before the half-spot crossing instead of exposing blank wallpaper. Neighbor windows remain absent from Smithay `Space`, preserving focus, hit testing, IPC visibility, protocol activation, and `apply_workspace_switch`'s established hide/show semantics; at rest no neighbor entries are selected or rendered. Hidden buffers are render-scaled to their current target geometry so an output/layout change cannot leave a stale-sized preview. The camera continues to advance the real workspace live at half a spot through `switch_workspace_immediate`, spring the residual offset to rest on release, respect scratchpad/overflow resistance, and yield to explicit gesture actions. The focused swim suite now covers 23 cases, including neighbor direction, configured bounds, idle-zero-work selection, and workspace edges. The real `GestureSwipeBegin/Update/End` path still requires a udev/libinput touchpad pass because nested winit cannot emit those events.
- Manual per-row/per-cell resize for cascade layout (`CascadeState`, `CascadeHit`, `src/grabs/cascade_resize_grab.rs`), completing the feature. A direct border drag (no modifier, `cascade_hit_test`) or a modifier+body drag on a tiled window (`cascade_resize_splits`, up to two boundaries for a two-axis corner-style drag) now adjusts a row's height or a cell's width, mirroring BSP's `hit_test_split`/`resize_splits`/`TileResizeGrab`/`TileWindowResizeGrab` shape but addressing a grid position instead of a tree path. Unlike BSP's connected-vessel propagation, a cascade drag only ever touches the two neighbors either side of the dragged boundary (`apply_paired_ratio`, the two-way version of `set_ratio`'s single split, redistributing the pair's own combined share with a 5%-of-combined floor on each side rather than 5% of the whole axis). Reflow behavior is row-scoped persistence: opening or closing a window re-resolves the row/cell partition, but any row whose window count didn't change keeps its manual ratios -- only the row that actually gained or lost a window resets to an equal share (`resolve_cascade_state`). `Layouts::layout()` stays read-only and resolves a state fresh each call without storing it (it has several read-only callers beyond the render path, e.g. ripple/animation anchoring and click-target lookups, discovered by trying to make the whole method `&mut self` and watching the compiler enumerate every caller); a new explicit `refresh_cascade_state`, called once per output from `retile_with_viscosity` right before its own `layout()` call, is the only place this bookkeeping actually advances. Unit tests cover row-scoped persistence across a reflow, the combined-share clamp, and manually asymmetric ratios still tiling exactly with no rounding gap. A nested smoke test confirmed cascade's ordinary rendering is unaffected by the refactor. Actual drag feel is not verified -- no pointer-drag injection is available in nested development, the same documented limitation as viscosity, connected-vessel resize, and sway.
- Cascade layout (`LayoutAlgorithm::Cascade`, `src/tide_core/layout.rs`), a third tiling algorithm alongside BSP and master, and the render roadmap's last unbuilt identity feature (renamed from its working name "basin-fill" in a design conversation with the maintainer). Windows wrap left to right, top to bottom into rows; the row count is chosen by scoring every candidate from one to the window count and picking whichever makes the resulting grid's column/row ratio land closest to the output's own aspect ratio, so an ultrawide monitor gets fewer, wider rows and a portrait one gets more, narrower ones. Row heights and each row's cell widths are computed from whatever space remains, the same rounding-gap-free pattern `layout_master`'s stack already uses on one axis, applied here on both. `default_layout = cascade` and the `layout:cascade` action select it; no default keybind is bound to it. Unit tests cover the zero/one-window cases, exact full-area tiling with no gap or overlap across a run of window counts on a non-round output size, row-count adaptation across wide/ultrawide/portrait aspect ratios, and the front-loaded remainder distribution. Verified live in a nested session: five real `kitty` clients on a near-square output landed in the predicted 3-row `[2, 2, 1]` arrangement.
- IPC event-stream/subscribe mode (`src/tide_core/ipc.rs`), Phase R3 of the render/visual-identity roadmap. The control socket was request-response only before this; it now also accepts `{"request": "subscribe", "events": [...]}` on a fresh connection, converting it into a long-lived push connection instead of a one-shot reply -- the intended path for a reactive widget (a waybar module, an eww `deflisten`, a QuickShell socket reader) that shouldn't have to poll `outputs`/`workspaces`/`windows`/`focused-window`. Omitting `events` (or sending an empty list) subscribes to all six channels; the ack echoes the resolved channel list so a typo'd filter is visible at handshake instead of silently matching nothing. Six emit chokepoints fire `window-opened`/`window-closed`/`window-changed`, `workspace-changed`, `focus-changed`, `urgent-changed`, `depth-changed`, and `config-reloaded` exactly where each becomes authoritative, not from the interactive keybind paths that happen to trigger them. A subscriber's unwritten-bytes queue is capped at 256KiB; exceeding it, or any write failure, retires the subscriber and its calloop sources rather than growing memory or leaking a source. Unit tests cover the filter rule, wire-format (de)serialization, and the kind/event-name mapping for every event variant. Verified live in a nested session with a throwaway socket client: accurate `workspace-changed` events from real `tidectl` actions, and correctly-ordered `focus-changed`/`window-opened`/`window-closed`/`focus-changed{null}` events from a real `kitty` client mapping and closing, with PSS completely flat across twenty connect/disconnect cycles and a forced ungraceful (RST) disconnect. Documented in `DOCUMENTATION.md`. Real-hardware (udev) verification remains, same as other IPC-adjacent work.
- Per-window depth/buoyancy override, the roadmap's other originally-scoped Phase R3 item. `rule { depth = false }` pins a matched window at tier zero forever -- it never dims or sinks under the automatic attention-depth model (Phase R1) regardless of inactivity, useful for a widget or player the user always wants live; `rule { depth = true }` affirms the normal automatic behavior (mainly useful to override an earlier matching rule's `false`). Last matching rule wins, same fold every other `Option<bool>` rule field uses; unset falls back to the global `depth { enabled }` value. Resolved live per window in `update_window_depths`'s existing 10Hz tick rather than cached at map time, so a hot-reloaded rule change takes effect on the next tick. Verified live in a nested session with a shortened `sink_after_ms`/`tier_interval_ms` and the IPC event-stream: an exempt window produced zero `depth-changed` events over 12 seconds of idle time while a matching non-exempt window sank through tier 1 and tier 2 as expected, with PSS unaffected. Documented in `DOCUMENTATION.md`.
- Optional floating-window sway (`src/visual/sway.rs`), the last mechanical Phase R2 slice. Dragging a floating window sideways now kicks a damped lateral oscillation, so the window swings side to side and settles back like it is sitting in water while the pointer keeps full authority over its real position. The offset is a closed-form function of the time since the last kick (`A·e^(-damping·t)·cos(2π·frequency·t)`), not a per-frame integrator: each drag step samples the current swing and adds the configured fraction of the pointer delta to it, so a continuous drag never accumulates unbounded displacement (`max_offset` caps it) and a settled window stops requesting frames entirely. Logical geometry, focus, and hit-testing stay immediate; only the shared render sample picks the offset up, which means surfaces, popups, borders, shadows, glass, depth overlays, screenshots, and transition captures all see it through the one existing element walk. The mechanic is explicitly opt-in: `sway { enabled = false }` by default, with `response`, `max_offset`, `frequency`, and `damping` knobs and a last-match-wins `rule { sway = true|false }` per-app override; `water_effects = false` remains the master bypass. State is one small record per swaying window, pruned by the shared animation clock at rest and evicted on unmap/destroy. Unit tests cover the oscillation shape, decay-to-rest, kick continuity and the displacement cap, plus config parsing/clamping/rule precedence and generated defaults. Smoke-tested in a nested session (clean startup and hot reload with the block enabled); nested drag-feel tuning and a real-hardware pointer pass remain, same as the other grab-touching slices.
- Connected-vessel BSP resize redistribution, the second mechanical Phase R2 slice. Direct split-border drags, modifier-right-drag tiled resize, and keyboard resize now drive a fixed chain containing the selected split plus parallel ancestors. The primary receives full pressure; each ancestor receives `falloff ^ tree-distance`, capped by `connected_vessels.max_splits` (four by default), while perpendicular splits stay fixed. Window-body and keyboard paths carry the target leaf's side at every ancestor so positive motion grows a right/bottom child instead of accidentally shrinking it; direct border drag keeps literal pointer direction. Ratios, spans, paths, and topology revision are captured at gesture start, preventing feedback math or stale paths after tree/bias changes. `connected_vessels { enabled, falloff, max_splits }` tunes the effect; `enabled = false`, `falloff = 0`, or `water_effects = false` restores one-split behavior, and master layout remains unchanged. The active grab owns only a bounded handle vector with no persistent history or render resources. Unit tests cover geometric damping, side-aware signs, pixel-to-ratio conversion, config parsing/clamping, and generated defaults. A release nested test forced four windows into one horizontal BSP chain, then grew the focused rightmost tile through keyboard resize: all three boundaries moved, with the root changing least and the nearest boundary most, and the session measured 116,635KiB PSS with 0KiB swap. Pointer-feel tuning and real-hardware verification remain.
- Interactive move/resize viscosity (`src/visual/viscosity.rs`), the first mechanical Phase R2 slice. The global `viscosity` value controls refresh-rate-independent exponential damping from `0.0` (immediate) through `4.0` (slowest), defaults to `1.0`, and can be overridden per app with `rule { viscosity = ... }` using the existing last-match-wins rule fold. All five pointer-grab paths use it: floating move, floating edge/modifier resize, tiled drag-to-swap, direct BSP split-border resize, and modifier-drag tiled resize. Logical geometry, layout ratios, drop hit-testing, and XDG resize targets still update immediately; only the shared render rectangle follows, so input never chases delayed state. Re-targeting samples the current on-screen rectangle first, including when an unrelated retile occurs mid-gesture. Each active mapped window keeps one small record with no motion history, texture, framebuffer, or per-frame allocation; the shared animation clock prunes it after settling and mapped-lifecycle cleanup evicts it on unmap/destroy. `water_effects = false`, global `viscosity = 0`, or a per-rule zero restores the immediate path, while tiled resize falls back to the ordinary layout-movement animation. Unit tests cover half-life behavior, interruption continuity, the zero path, global/rule parsing, clamping, and rule precedence. Nested visual tuning and real-hardware pointer-grab verification remain.
- Layout movement now interpolates window position and size as one interruption-safe rectangle. A retile begins from the current on-screen geometry even when another animation is still running, then non-uniformly scales the already-imported client surfaces directly toward the new layout slot. Popups, rounded clipping, borders, shadows, frost/water glass, depth overlays, close snapshots, visible winit/udev frames, and every capture path use the same sampled size. This adds no framebuffer or texture cache. `movement { animate_size = true|false }` controls it and defaults on for every animation preset; `resize`, `size`, and `animate-size` are accepted aliases. The maintainer manually verified rapid open/close retiles in the release nested backend: the surviving window, border, and shadow resized together without snapping or flicker. The two-Kitty session measured 118,408KiB PSS with 0KiB swap.
- Window lifecycle and layout-motion animation timing completes the next R2 foundation slice. The global `animations { }` block has independent `open`, `close`, and `movement` sub-blocks with enablement, duration, logical-pixel travel, start/end opacity, built-in easings, and CSS-compatible `cubic-bezier(x1,y1,x2,y2)` curves, plus a shared slowdown multiplier. Geometry and opacity can use separate durations and curves. Lifecycle origins can use a fixed offset, force an output edge, or choose the nearest edge with the same midpoint-distance and tie-order behavior as Hyprland's unforced `slide`. Named `tide`, `wave`, `riptide`, and `hypr-smooth` presets establish adjustable baselines; `hypr-smooth` mirrors the maintainer's real 300ms open/close geometry, 400ms fade and movement clocks, and exact custom Béziers. TideWM's trajectories can add one broad perpendicular swell or a configurable decaying wave over the ordinary eased path; amplitude, cycles, and decay remain independently tunable per transition. Logical focus/input/layout state changes immediately, and interruption-safe movement retargets from the real on-screen position. Closing now transfers the last already-imported surface textures from the normal frame walk into a bounded snapshot, covering both null-buffer unmap and direct xdg-role destruction while allowing Smithay to release live client state normally. The snapshot clones GPU handles rather than copying a window into a new framebuffer. Open/move transforms cover client surfaces, popups, rounded clipping, borders, shadows, water/frost glass, depth overlays, visible winit/udev output, screenshots/screencasts, and workspace-transition captures. Animation state is bounded to mapped windows plus in-flight closing windows and is pruned by the shared redraw clock even when an output is not presenting.
- Polished impulse presets replace the old debug-like circle/box look as the default: `water-drop` layers concentric wavefronts and an impact crater, `jelly` visibly jiggles an organic membrane, `bubble` adds a double rim and moving highlight, `splash` draws a lobed crown with spray peaks, and `tide` flows several wave bands through one impulse. `legacy` preserves the original stackable ring/square/droplet/cross renderer, and assigning `shapes` selects it automatically for compatibility. `map_preset`, `focus_preset`, and `urgent_preset` can choose a different appearance for each event. Automatic focus during initial map is coalesced into the map event by default, preventing the map and focus appearances from stacking; `focus_on_map = true` deliberately restores the layered look, while later real focus handoffs always animate normally. Two-color gradients plus `glow`, `wobble`, and `detail` controls remain globally and per-app adjustable. Adaptive sizing supports fixed pixels, window diagonal, width, height, shortest side, or longest side with scale and min/max clamps. New top/bottom/left/right/nearest-edge anchors have adjustable along-edge position and signed inward/outward distance. Reusable `ripple_preset <name> { }` blocks can contain every ripple field, inherit another named preset, and be selected globally, per trigger, or per app. Cycles and missing names degrade safely. Every polished preset is one bounded analytical shader element with no textures, buffers, or per-frame allocations.
- Configurable compositor mouse modifier via `pointer_modifier` (`mouse_modifier` and `drag_modifier` aliases). Left-drag moves floating windows or drag-swaps tiled windows; right-drag resizes either. It accepts Super/Alt/Ctrl/Shift combinations, checks physically-held keys to stay safe in nested sessions, hot-reloads, defaults to Super for existing configs, and the generated config points it at `$mod`.
- Rounded client geometry and analytical gradient borders (`src/visual/decoration.rs`), completing the familiar visual half of Phase R2. The global `rounding { }` block supports one radius or independent CSS-order corner radii, Hyprland-style superellipse `power`, physical-pixel antialias width, real client clipping, floating-only scope, and fullscreen opt-in. Sparse `rule { rounding { } }` overrides merge field by field; `corners`, `geometry_corner_radius`, `clip_to_geometry`, and shorthand radius/on/off forms are accepted. The texture override follows niri’s clipped-surface architecture: texture coordinates are mapped back into the toplevel geometry while each main surface/subsurface element draws, and real xdg-popups remain separate and unclipped. The same resolved geometry now shapes borders, water/frost glass, and zero-radius shadows, preventing square effect layers from leaking through transparent corners. The global and per-app `border { }` blocks expose width, outside/center/inside placement, independent active/inactive/urgent start/end RGBA colors and opacity, gradient angle, radius offset, antialiasing, scope, and opt-in rotation/pulse animation. Focused, inactive, and urgent animation gates are independent; inactive borders can remain moving, stay visible but static, or disappear for a focus-ring-only setup. Equal endpoints make a solid border. Borders are one analytical pixel element per window with no textures or growing cache; static borders remain damage-driven, while animated borders use the shared redraw clock and an advancing commit counter. Popups → border → clipped surface → glass → shadow ordering is shared by winit, udev, screenshots/screencasts, backdrop captures, and workspace-transition captures. Release nested AMD GLES screenshots verified asymmetric clipping on transparent frost and opaque tiled clients, active aqua and dark inactive gradients, shadow/glass radius alignment, and live gradient advancement. The two-window animated session measured 107,494KiB PSS and 0KiB swap. Standalone udev compiles through the same path but still needs real-hardware visual verification.
- Adjustable analytical drop shadows (`src/visual/shadow.rs`), the second Phase R2 decoration. TideWM combines niri’s CSS-like softness, signed spread, offset, draw-behind behavior, and active/inactive colors with Hyprland’s range aliases, render-power falloff, sharp mode, and scale. TideWM adds a configurable aqua urgent/bioluminescent state, independent active/inactive/urgent opacity multipliers, per-shadow corner radius, `floating_only`, and an opt-in fullscreen path. The global `shadow { }` block is inherited by sparse per-app `rule { shadow { } }` overrides; multiple matching sub-blocks merge field by field, and `shadow = on|off|none` is a shorthand. Colors retain alpha and accept CSS/Hyprland compact/legacy forms. Shadows are fixed-cost signed-distance fields with no shadow textures, blur framebuffers, or growing cache. `desktop_render_elements` now returns the renderer-concrete output element enum so each shadow can be placed immediately behind its own surface tree rather than in one incorrect all-shadows layer; glass-replaced floating windows get the same surface → glass → shadow ordering. Visible winit/udev frames, screenshots/screencasts, backdrop captures, and workspace-transition captures therefore share the effect. `draw_behind_window = false` is the color-safe default: the shader cuts out the actual window body, preventing the gray/cyan full-window filter previously seen through translucent apps. Shadows are ordinary decoration and remain enabled when `water_effects = false`. Unit tests cover the shader contract, every tuning field, RGBA parsing, sparse per-rule inheritance, and multi-rule merging. Verified in a release nested AMD GLES session with a centered client-native-alpha Kitty over an opaque control: the focused preset produced the configured soft aqua outside falloff with no shadow color across the client body/text; `cycle-focus` changed it in-place to the configured dark inactive shadow. The session measured 115,786KiB PSS and 0KiB swap. Standalone udev wiring compiles and shares the same element walk, but still needs a real-hardware visual pass.
- Frost and transparency tuning is now fully rule-aware. Every global `frost { }` field can be overridden per app with `rule { frost { } }`; matching blocks merge field by field over the global baseline. The bounded shader now exposes blur radius and strength, independent processed-layer opacity, saturation, contrast, brightness, banding-reduction noise and scale, vibrancy with a dark-pixel bias, optional tint color/alpha, and rounded-edge radius/softness. Neutral defaults (`tint_alpha = 0`, saturation/contrast/brightness `1`) add no color wash. The extra controls stay in the same fixed 25-tap pass and allocate no new textures. Window rules also gained `active_opacity`/`focused_opacity`, `inactive_opacity`/`unfocused_opacity`, and `fullscreen_opacity`; they multiply the existing base `opacity`, select at render time from real focus/fullscreen state, and therefore update without remapping the window. Fullscreen takes priority. This follows the useful parts of Hyprland's active/inactive/fullscreen opacity and contrast/noise/vibrancy model plus niri's per-window background-effect overrides, while preserving TideWM's existing last-rule-wins and bounded-render design. Release tests cover parsing, per-rule inheritance, and state-multiplier priority. The expanded neutral per-app frost was visually inspected in a nested AMD GLES session with Kitty client-native background alpha: glyphs remained opaque, no tint overlay appeared, and the backdrop stayed blurred. The active session measured 114,193KiB PSS with no swap.
- Selectable frosted glass (`src/visual/frost_glass.rs`), the first Phase R2 decoration. A floating-window rule can now choose `glass = frost`, keep the existing `water` refraction, or use `none` for ordinary transparency. Explicit glass modes work with client-provided surface alpha, the preferred path because a terminal such as Kitty can make only its background translucent while keeping glyphs fully opaque. TideWM’s compositor `opacity` still multiplies the complete client surface; for backward compatibility, setting it below one without an explicit mode continues to imply water glass. The global `frost { }` block controls enablement, physical-pixel radius, saturation, brightness, tint color, and tint alpha; tint alpha defaults to zero for neutral, color-free frost while remaining adjustable. Hot reload re-resolves mapped windows and releases stale captures. Frost reuses R0.5’s pre-frame, reusable window-sized backdrop texture and runs a bounded 25-tap Gaussian/color pass with no additional per-frame texture allocation. The initial sparse 13-tap candidate was rejected during screenshot inspection because it produced stepped duplicate images around the wallpaper logo; the dense 5×5 weighting replaces those ghosts with overlapping samples. Capture work is limited to floating windows whose selected mode will actually consume it, and the first successful capture requests exactly one follow-up frame so an otherwise-static desktop cannot stall before displaying glass. The visible DRM/udev path uses the same shared glass replacement and pre-frame backdrop capture as winit; screenshots and workspace-transition captures share mode selection. `water_effects = false`, `frost.enabled = false`, deep schematic windows, and `glass = none` bypass the work. Unit tests guard the shader/tap contract, all frost config fields, mode parsing, and last-rule-wins behavior. Verified in a release nested session by capturing and directly inspecting the frosted output: backdrop edges softened while client-native alpha kept terminal glyphs fully opaque. The active frost window measured 108,587KiB PSS with no swap, far below the 1.5GiB target. Standalone DRM wiring compiles but has not yet had a real-hardware visual pass.
- Automatic attention depth and buoyancy (`src/visual/depth.rs`), completing the first Phase R1 identity slice. Every mapped window starts at tier zero, sinks to tier one after configurable inactivity with reduced live-content opacity and an analytical cool-water wash, then switches to a cached box-and-title schematic at tier two and below. Focus, clicking, or keyboard input returns it to the surface immediately. Urgent windows keep a configurable cyan bioluminescent border through every tier. The global `depth { }` block exposes enable, timing, maximum tier, live opacity, wash, schematic, border, and urgent colors/alphas; `water_effects = false` remains the master bypass. State is bounded to one timestamp/tier/element identity per mapped toplevel, deep schematic buffers are evicted on resurfacing or unmap, and the backend timer’s inactivity scan is throttled to 10Hz. Visible winit/udev frames, screenshots, and workspace-transition captures all share the same depth replacement path. A release nested test used two-second tiers: the user observed the cool wash, schematic replacement, and immediate keyboard buoyancy working in sequence. The active schematic added about 4MiB PSS in that output size; two consecutive ten-cycle cache release/rebuild batches ended at exactly the same 131,671KiB PSS reading, with no per-cycle growth. Unit tests cover timing boundaries, cap/disable behavior, shader uniforms, config parsing, and generated defaults.
- Optional synchronized workspace motion for the water/glow transition. `workspace_motion = true` captures the incoming desktop as well as the outgoing one, then slides both edge-to-edge in the configured wave direction while the procedural effect stays above them. `workspace_motion_delay_ms` controls how long the water leads before desktop movement begins (`100`, `200`, and `300` are 0.1, 0.2, and 0.3 seconds); motion then eases across the remaining transition lifetime. It is disabled by default because the incoming capture raises the bounded transient allocation from one to two ARGB8888 full-output textures per actively transitioning output: about 15.8MiB total at 1080p or 63.2MiB at 4K. A failed second capture safely falls back to the original static incoming workspace rather than cancelling the switch.
- Captured wave workspace transitions (`src/visual/workspace_transition.rs`), the third visible piece of Phase R1. A user workspace action captures the outgoing desktop after its submitted frame and keeps the incoming workspace live underneath. The default `style = water` is a real two-stage full-screen transition: a procedurally shaded body with moving caustic streaks, a curling/scalloped foam crest, and spray floods across the outgoing workspace; once water covers the complete output, its trailing edge continues in the same direction to reveal the incoming workspace. The earlier slimmer colored sinusoidal boundary remains available as `style = glow`. Forward and backward switches use opposite directions by default (`direction = auto`), or either direction can be forced; compositor chrome remains live above both styles. The default water body opacity is `0.88`, leaving the workspace subtly visible beneath it. Protocol-driven toplevel activation retains an immediate internal switch path so focus can still move to a hidden surface in the same dispatch. Transient state is bounded to one pending target and one ARGB8888 full-output texture per output by default (about 7.9MiB at 1080p or 31.6MiB at 4K); optional synchronized workspace motion adds the second bounded texture documented above. Newer requests replace rather than queue, and completion, lock entry, output removal, or another switch releases the entire transition. Both procedural styles are analytical shaders and allocate no textures beyond those captures. `water_effects = false` bypasses capture and animation completely. Winit, udev, and the separate screenshot path all carry the transition element. The `workspace_transition { }` block exposes `enabled`, `style`, `duration_ms`, `speed`, five easing curves, automatic/forced `direction`, wave geometry, shared water/front color, glow core/halo appearance, and water body opacity/depth, foam color/size/opacity, spray, and turbulence. Hot reload applies tuning to the next switch; disabling the block clears active/pending state without suppressing water-glass or ripples. Verified live in a release nested session in both directions, with exaggerated pink/green glow fronts, and with the water body visibly filling the output; the user approved the water look and requested a faster 600ms personal preset. The isolated `water_effects = false` session switched immediately, and ten completed capture cycles settled within 0.5MiB PSS of the pre-cycle reading rather than growing per switch. Unit tests guard the shader contract, direction mapping, config parsing/defaults, and texture-cost arithmetic.
- Ripple customization (`config::RippleConfig`, parsed from a new global `ripple { }` block and per-app `rule { ripple { } }` sub-blocks, matching the same precedence shape other rules already use). The droplet ripple shipped in the previous entry was a single hardcoded look -- now every visual knob is tunable: shape (`shapes = ring square droplet cross`, multiple shapes layer concurrently), color (`color = #RRGGBB` hex), peak radius, outline thickness, lifetime (`duration_ms`), peak transparency (`peak_alpha`), easing curve (`ease = linear|cubic-out|cubic-in-out|quad-out|exp-out`), anchor point (`anchor = center|cursor|topleft|topright|bottomleft|bottomright` plus an `offset = <dx>x<dy>`), z-order (`layer = above-all|above-windows|below-windows|below-all` so a ripple can sit over everything, over windows, between windows and wallpaper, or behind the wallpaper), and which events fire one (`triggers = map focus urgent`, map-only by default since every-focus-change ripples can feel busy). Per-app `rule { ripple { } }` overrides merge over the global block field-by-field so a rule can set just one knob and inherit the rest, and `rule { ripple = none }` is a one-line shorthand for "no ripple on this app." The shader was restructured into a single branching fragment program with a `u_shape` uniform (0=ring, 1=square, 2=droplet, 3=cross) rather than four compiled programs -- a ripple with multiple shapes produces one render element per shape per frame sharing the same bounding square, alpha, and tint. Radius easing applies to the progress value (not the alpha fade, which stays quadratic regardless -- an eased radius on top of an eased alpha reads as impact+bounce rather than water). Alpha handling switched to pre-multiplied (`vec4(u_tint * a, a)`) to match the `water_glass` shader's blend convention. The focus-change trigger is wired up: a ripple fires at the newly-focused window's configured anchor only on a real window-to-window handoff (not on the very first focus, where it would be redundant with the map ripple, and not on focus-dropped-to-None, which is noisy). The urgent-hint trigger parses but has no producer wired yet -- landing that needs xdg-protocol urgent-hint detection, which is a separate piece of work. `water_effects` remains the master identity toggle; `ripple { enabled = false }` is the per-scope kill, and both are checked at spawn time so either can suppress a ripple. Hot-reload picks up ripple config changes automatically (the whole `Config` struct is swapped on `Config::reload`). All three render paths (winit, udev, and the separate screenshot capture in `capture.rs`) now splice ripple elements by their configured layer: AboveAll frontmost, AboveWindows between chrome and windows, BelowWindows between windows and wallpaper, BelowAll at the very back. Unit tests cover the shader-source guard, easing monotonicity/boundedness across all five curves, lifetime/geometry, and per-shape element multiplicity. Live visual verification of the new knobs is the user's next nested-session step.
- Impulse ripple (`src/visual/ripple.rs`), the second piece of Phase R1's identity slice: one shared primitive for a radial disturbance from a point, decaying over time. The first trigger wired up is a window mapping -- a droplet impact at the window's center, the most universally recognizable "water" cue and the easiest to verify (open a window, see the ripple). Focus-change and urgent-hint triggers are explicit later scope, see AGENT.md's Phase R1 entry. The primitive is purely analytical: closed-form radius/alpha given elapsed time, no per-frame simulation. Radius uses ease-out cubic (fast expansion that decelerates as it approaches peak, the shape a real droplet impact's wavefront makes on a still surface -- energy radiating outward against increasing circumference); alpha uses a quadratic fade so the ring stays visibly energetic for the first half and vanishes rather than trailing into noise. Visualized through a procedural GLES pixel shader (Smithay's `compile_custom_pixel_shader` / `render_pixel_shader_to`, confirmed in the pinned rev when this work started) that draws a soft expanding ring with a fixed 8-pixel half-width, so the ring's visual thickness stays constant as it grows rather than scaling with the element's bounding square. Default tint is pale cyan (TideWM's identity color); default peak radius 220 logical px; default lifetime 650ms. A new `Ripple` variant on `OutputRenderElements` carries the per-frame render element, which meant making the enum's renderer-concreteness decision (already taken for water-glass) carry through naturally. `Smallvil::ripples: Vec<Ripple>` is bounded by a 16-active-ripple cap (`MAX_ACTIVE_RIPPLES` in `spawn_window_map_ripple`) so rapid window mapping can't grow it unboundedly; finished ripples are `retain`-pruned as a side effect of every `ripple_frame_elements` call; `has_active_animation` now returns true while any ripple is still in flight, so both backends' idle redraw-arming loops keep the compositor ticking for the lifetime of each impulse (the same hook the toast fade already used). Render placement: above windows but below toast/overview/picker/tab-strip chrome in both backends' element chains, plus the separate screenshot capture path in `capture.rs::render_one_capture` -- without that, a screenshot mid-ripple would silently drop the ring from the captured frame, the same "separate render path forgot the new effect" bug class water-glass already addressed and session-lock's element ordering burned once before either. Gated on the existing `water_effects` config toggle, same as water-glass: both are R1 effects that aren't meaningful with the identity off. Build- and unit-test-clean (84 tests passing, including shader-source guards and lifetime/geometry tests for the new primitive); live visual verification in a nested session is the user's next step.
- Water-glass (`src/visual/water_glass.rs`), Phase R1 of the render/visual-identity roadmap -- the first thing this roadmap produces that's actually visible on screen, and the first thing to sample the backdrop captured in R0.5. A floating window with an `opacity` window rule below 1.0 now renders its captured backdrop through a custom GLES fragment shader that perturbs the sample coordinate with a small position-based sine/cosine offset, giving a wavy refracted look instead of the flat content that would otherwise show through a semi-transparent window. Triggered by reusing the existing `opacity` rule rather than adding a new config key, since "what shows through a semi-transparent window" is exactly what that already means. The water-glass layer draws fully opaque at the window's own rect behind the window's own (semi-transparent) surface element: without that, the real undistorted backdrop would still show through underneath the distorted copy and read as a ghost rather than glass. The shader is deliberately static (no time uniform) for this first cut -- animating the ripple is a later polish pass once something is driving it through the R0 `Animation` clock. To make this possible at all, `OutputRenderElements` (the per-backend element enum both visible-frame loops and the screenshot path thread through) had to drop its generic-renderer parameter and become concrete over `GlesRenderer`: a `WaterGlass` variant draws via a custom `GlesTexProgram`, which has no generic-renderer equivalent. The enum was already only ever instantiated with `GlesRenderer` (there is no second renderer backend in this codebase), so making both concrete costs nothing in practice. `desktop_render_elements`'s `skip: Option<&WlSurface>` simultaneously became `skip: &[WlSurface]` so the same walk can pull every water-glass-eligible window out of its normal z-slot at once, not just one -- more than one can be eligible on the same output. Eligible windows (water-glass layer + their own semi-transparent surface element on top) get prepended ahead of the rest of the space walk, putting them topmost among windows for this first cut -- real multi-window z-order among them is deliberate later scope, see `water_glass.rs`'s module doc. The separate screenshot render path (`capture.rs::render_one_capture`) received the same water-glass substitution, not just both visible-frame loops: without it a screenshot of a water-glass window would show its plain unrefracted content instead of what's actually on screen, the exact "separate render path forgot the new effect" bug class this codebase already hit once with session-lock. `BackdropCapture` (`backdrop.rs`) now carries a stable per-window `Id` and an incrementing `CommitCounter` alongside the texture, so a `WaterGlassElement` built from it reports the right identity to the damage tracker -- a fresh `Id` every frame would leak an orphaned entry in the tracker's per-element bookkeeping for every frame this window is water-glass-eligible, never pruned. Same gating as R0.5's capture: `water_effects` config toggle off suppresses the whole thing. Build- and unit-test-clean (80 tests passing including a new shader-source guard for the substitution point and required uniforms); live visual verification in a nested session is the user's next step (per the project's standard nested-first testing loop). Two further R1 pieces remain after this: the impulse-ripple primitive and the first-pass depth/buoyancy model.
- Backdrop capture (`src/visual/backdrop.rs`), Phase R0.5 of the render/visual-identity roadmap: `capture_backdrop` renders whatever sits behind a floating window's rect into an offscreen texture, the shared plumbing both the familiar frosted-glass blur and TideWM's own water-glass refraction will sample from once their shaders land (Phase R1/R2). `desktop_render_elements` gained a `skip` parameter so the "behind" list is built by walking the real element composition with the target window omitted, not by filtering an already-built list -- the same front-to-back z-order this project's session-lock element-order bug already burned once. Capture runs after the visible frame submits (same FBO-only timing `render_pending_captures`'s screenshot capture already established as safe -- interleaving offscreen work before `submit()` previously broke the winit backend's context lifecycle, confirmed live), so a captured backdrop is one frame behind, consumed building the *next* frame -- the same latency real blur-behind implementations elsewhere accept. Captured textures are stored per-window (`Smallvil::backdrop_textures`) and evicted on unmap/destroy alongside `window_opacity`. Nothing samples the stored texture yet -- capture and storage only, gated on the existing `water_effects` toggle (its first real reader). Verified live in a nested session: the captured region's content matched the real screen at the same position and orientation (a distinctly asymmetric wallpaper shape lined up exactly, ruling out the y-flip this codebase has hit once before in the screenshot capture path), the target window's own content was correctly absent from its own backdrop, and the visible frame was unaffected. Checked PSS/CPU across 10 floating-window open/toggle-floating/close cycles: flat, no growth, confirming the eviction path actually prevents the leak a `HashMap<WlSurface, GlesTexture>` would otherwise accumulate. Two open items, see AGENT.md's roadmap section for detail: a faint, unchased residual in the capture-vs-screenshot diff (concentrated on wallpaper detail, not spread uniformly), and fractional output scale is unverified since `scale` overrides are udev-only and this session's testing was nested-only.
- `Animation`, a small shared linear-interpolation primitive (`src/visual/animation.rs`) -- the first piece of the render/visual-identity roadmap (AGENT.md's "Render and visual identity roadmap", Phase R0). Before this, nothing in the codebase interpolated a value over time at all; `toast.rs`'s fade was hand-rolled elapsed-time arithmetic, now rebuilt on top of this instead. `Smallvil::has_active_animation()` (`state.rs`) also replaces the toast-specific redraw check both backends used to duplicate, giving future water/decoration effects one shared place to plug into instead of each growing its own copy in `winit.rs` and `udev.rs`. No user-visible behavior change -- verified live in a nested session via `grim` screenshots: the config-reload toast still shows at full opacity and still fully clears once its lifetime elapses; a shot taken partway through the fade window showed visible partial transparency (not a hard cutoff), though the exact alpha at that point wasn't pinned to a known instant given unmeasured watcher-debounce latency between the config touch and the toast actually starting. Also checked PSS/CPU across 10 rapid trigger cycles (release build, nested): PSS settled ~10MB above pre-first-toast baseline (the toast's own rasterized texture, expected, not this change) and stayed flat across all 10 further triggers rather than climbing per-cycle, CPU stayed ~1.1-1.7% the whole time including mid-fade, and dropped back to the same idle baseline after each toast cleared rather than staying elevated -- no leak, no stuck redraw loop.
- The urgent-hint ripple trigger, the last of the three documented in AGENT.md's Phase R1 entry. Wired into `mark_urgent`, the existing downgrade path for an `xdg-activation-v1` request with a present-but-stale serial (declines to steal focus, marks the window urgent instead). Single-shot for now, matching the map/focus triggers -- a repeating "pulse until acknowledged" is later scope. Verified live with a throwaway two-window `smithay-client-toolkit` test client: map window A (capture its keyboard-enter serial), map window B (steals focus, staling A's serial), activate A with the now-stale token -- confirmed `fresh=false` and a `trigger=Urgent` ripple spawn via TideWM's own debug trace.

### Changed
- Reorganized the Rust source tree by responsibility without changing public behavior. Configuration, state, input, layout, IPC, and the Waves parser now live under `src/tide_core/`; animation, water/decorative effects, and compositor-owned visual UI now live under `src/visual/`. Crate-root re-exports preserve the established internal module paths, keeping this a file-layout change rather than a broad semantic rewrite. Backends, protocol handlers, pointer grabs, capture, screencasting, accessibility, cursor handling, and XWayland remain in their existing locations.

### Fixed
- Ocean IPC state now reflects Ocean ownership instead of leaking Classic/Smithay presentation caches. `outputs` reports `active_workspace: null` plus the live `camera_origin`, `workspaces` is empty because Ocean has no real workspaces, and window entries consistently report `workspace: null`, `output: null`, their non-owning `entry_output` hint, and the real Ocean floating state. A nested two-reef test caught the old inconsistency: a home-reef window appeared output-owned only because its world rectangle happened to overlap the output's presentation coordinates, while an equally valid deep-reef window reported no output and a successful floating conversion still reported `floating: false`.
- Waves bindings are now fully authoritative. Parsed files begin with empty bind and submap tables, so built-in and feature-specific bindings never remain active underneath a user's declarations. Ordinary keys can be held as user-defined helper modifiers in multi-key chords (`P+H`, `P+Ctrl+H`), multiple helper layers can coexist, and bare actions such as `F = toggle-fullscreen` are valid. `Ctrl+Alt+Escape` remains outside the configurable table as the recovery invariant: it temporarily activates known-safe fallback bindings in memory until a successful reload or restart, without rewriting the user's file. The generated example uses Alt as its main modifier, Super for workspaces, Ctrl for Ocean movement, and P as an ordinary helper key; those are examples rather than compositor policy.
- Ocean pointer focus now follows the model-neutral placement order, fixing clicks and hover focus passing through a visually topmost floating window to a tiled window below it. `input.unfocus_on_empty = true` optionally clears keyboard focus when the pointer moves onto empty desktop or Ocean canvas; it defaults off.
- A newly-mapped window matching a placement window rule (`float`, `pin`, `maximize`, `fullscreen`, `pseudo_tile`, or the auto-float heuristic) could receive two protocol configures in quick succession -- the ordinary tiled-size one from `retile()`, then a different-sized one from the placement conversion -- visible on a terminal as a one-frame garble/re-flow of its text grid. `map_toplevel` now withholds the first configure, but only when a second one is guaranteed to follow (`rule.maximize`/`rule.fullscreen`/`rule.pseudo_tile`, or a float/pin/auto-float conversion carrying a rule-provided `position`/`size`); a plain float/pin/auto-float conversion with no rule-provided size still gets the tiled configure, since nothing else would configure it at all. Found on real AMD hardware.
- Locking the pointer (`wp-pointer-constraints-v1`, e.g. Minecraft's mouse look) left the system cursor visibly frozen at its last on-screen position instead of hidden, since neither the udev backend's frame render nor the screenshot/capture path checked for an active lock. Both now hide the cursor for the duration of a lock, the same as a client's own hide request. Found on real AMD hardware.
- The IPC event-stream's `depth` channel never reported a window resurfacing, only sinking. `note_depth_attention` (the immediate tier-zero reset on focus/keyboard input) reset the tier and requested a redraw but never emitted `DepthChanged` -- only the periodic `update_window_depths` tick did, and by the time that tick next ran, the tier was already back at zero with nothing to report as changed. Found by live-testing the just-added per-rule buoyancy override: a subscriber watched a window sink through tier 1 and tier 2 correctly, but switching focus back to it -- confirmed visually snapping back to live content in the same instant via a back-to-back action-plus-screenshot check -- produced no corresponding event. `note_depth_attention` now emits `DepthChanged { tier: 0 }` on an actual reset, matching the periodic path.
- Moving a floating frost/water-glass window no longer displays a backdrop captured for its previous position. Both backends now render the window-sized offscreen capture immediately before binding the visible output, so that same frame samples the current drag geometry while preserving the known winit rule that no FBO work may occur between visible bind and submit. Same-sized recaptures reuse the existing `GlesTexture` instead of allocating a new GPU buffer for every pointer-motion frame, removing the allocation churn that made frost drags hitch as well as the one-frame visual trail that looked like flicker.
- The generated ripple example said `color = #8EDDFF`, but Waves correctly treats a bare `#` as the start of a comment, so uncommenting that line passed an empty value to the color parser. The example now uses bare `8EDDFF`; quoted `"#8EDDFF"` remains valid when keeping the prefix matters. `DOCUMENTATION.md` now carries the complete ripple reference instead of leaving the generated comments as the only catalog.
- The map-ripple's "droplet at the window's center" premise was wrong for ordinary tiled windows since the trigger first landed: `retile()` only sends a configure proposing a tiled window's new size, it doesn't wait for the client to ack and commit a matching buffer, so reading `window.geometry()` synchronously right after (as the map trigger does) saw the window's pre-tile size at its already-updated post-tile location. Every map ripple on a plain tiled window anchored near the gap offset (observed live: `(8, 8)`) instead of the window's real center. Same class of bug as `TileMoveGrab::drop`'s documented "space reflects where this is being moved to, not its real slot" gap; fixed the same way, reading the window's rect from `Layouts::layout()` (the same authoritative target `retile()` itself just computed) instead of `window.geometry()` when the window is tiled and not fullscreen/pseudo-tiled. First-map floating conversions had the same lag through a different path; they now anchor from `FloatingTag::rect`, which already holds the configure target, including an exact rule-provided position and size.
- `ripple { color = #RRGGBB }` silently failed to parse unless quoted: `config.wave`'s grammar strips a bare `#` as a comment outside quotes, so the documented hex syntax only ever worked as `color = "#RRGGBB"`. `parse_ripple_color` now also accepts bare `RRGGBB` (no quotes needed) and Hyprland-style `rgb(RRGGBB)`/`rgba(RRGGBB, AA)`, and a `debug`-level trace now logs a ripple's resolved trigger/color/shapes/layer at spawn so a config value silently not taking effect is distinguishable from the ripple not spawning at all.
- Forming a window group left a dead hole where the absorbed window's tile had been: `group_with` removed that window's leaf from the tree but never retiled, so the group kept its old smaller rect and the freed space sat on screen empty until some unrelated retile happened to run. Every other group path (ungroup, tab promotion, close-cleanup) already ends in a retile; this one now does too. Found during a live IPC-driven test pass on real hardware, where the hole stayed up for seconds until a layout switch healed it.
- Scratchpad entries in `tidectl workspaces` and `tidectl windows` now read `scratchpad` / `scratchpad:<name>` instead of leaking the synthetic internal workspace number (a named scratchpad printed as `workspace=4294963200`). The `windows` query's JSON also gained the same `scratchpad` field `workspaces` already had, so bars and scripts don't need to know the numbering scheme either.
- A live display-manager session logged nothing at all from the compositor: SDDM wires the session's stdout to `/dev/null` and only stderr into `wayland-session.log`, and `tracing_subscriber`'s `fmt()` writes to stdout by default. All logging now goes to stderr, where a session log can actually catch it.
- A screencast-enabled build froze the entire machine (mouse, keyboard, VT switch all dead, power-cycle needed) seconds into a real SDDM login. The startup check that restarts a stale `xdg-desktop-portal.service` ran synchronously before the event loop started -- and the restarted portal's own backend is a Wayland client of this compositor, so the portal waited for a compositor whose only thread was waiting for the portal. The restart job is now enqueued with `systemctl --no-block` and the compositor proceeds straight into its event loop. Root-caused from the journal of the frozen session (the portal stop/start lines were the last thing logged before the hard reset); the fix itself still needs a fresh SDDM login on real hardware to be called verified.

## [0.60.0] - 2026-07-24

First pre-release. The milestone this number marks: the WM foundation is feature-complete and the core of it is now tested on real hardware — AMD end to end (including OBS/Discord screencasting on a standalone session), Nvidia through a full nested-backend pass on an RTX 3060. Next up is a code-optimization pass, then `render/` finally starts: the water effects TideWM exists for.

### Added
- Screencast portal now lists monitor, window, and virtual sources with a compositor-owned picker instead of grabbing the first monitor.
- Touchpad swipe/pinch gestures can trigger any compositor action. Verified live on real hardware (external USB Apple Magic Trackpad and a ThinkPad's built-in touchpad): all four swipe directions and `pinch_in` confirmed, `pinch_out` not yet confirmed. See AGENT.md's Phase M gesture section for the full account.
- Per-window `opacity` window rule.
- `xdg-toplevel-icon-v1` support.
- Named scratchpads: `toggle-scratchpad:<name>` / `move-to-scratchpad:<name>` action variants, any number of them, on top of the existing single scratchpad (which stays the bare `toggle-scratchpad`). Each name is just another reserved workspace under the hood -- same hide/show machinery, no new data structure. The IPC `workspaces` query tags scratchpad entries with a `scratchpad` name field so bars can label or hide them.
- Per-workspace and per-output gap overrides: repeatable `workspace_gaps = <N|name> <pixels>` lines (names resolve through `workspace_name` aliases) and a `gaps` key inside `output` blocks. Workspace beats output beats the global `gaps`.
- Window swallowing: a tiled window matching a `swallow = true` window rule is hidden when a window its process spawned maps, and that child takes over its exact tile; closing the child puts it back in the same slot. PID ancestry is read from `/proc`, so it works for any terminal without shell integration. Verified live in a nested session with grim screenshots: exact-slot replacement and restore, uninvolved tiles untouched.

### Fixed
- Fullscreen windows no longer render beneath layer-shell bars/launchers, in both live frames and screenshots.
- A crashed session-lock client now fails closed (the compositor exits) instead of leaving the session in an unclear state.
- PipeWire screencasting now actually produces frames. The producer stream never called `pw_stream_trigger_process()`, which a `StreamFlags::DRIVER` stream needs to start each graph cycle, so `process()` simply never fired. Verified under the nested winit backend against a real PipeWire daemon with a direct consumer: correct, live-updating frames over the MemFd/SHM path, PSS flat over a sustained stream. See AGENT.md's Screencasting section for the full root-cause writeup.
- Real OBS screencasting over the udev/DRM backend, on real hardware, fixed and verified live: OBS's log showed `pipewire: ... error: no more input formats` (a PipeWire negotiation error), but the actual bug was one level up, in `xdg-desktop-portal.service` itself. A systemd `--user` manager that outlives a session switch (SDDM "switch session", any relogin that doesn't tear the user manager down) keeps whatever `xdg-desktop-portal.service` instance was already running from the previous login, with the previous desktop's `XDG_CURRENT_DESKTOP` baked into its own already-running process environment -- `dbus-update-activation-environment`/`systemctl --user import-environment` (already correct, see the "Session environment" section of AGENT.md) only affect *future* activations, not a process already running. Confirmed live on this machine: the frontend had `XDG_CURRENT_DESKTOP=Hyprland` in `/proc/<pid>/environ` from an earlier Hyprland login on the same systemd user manager, silently routing every screencast request (OBS included) to `xdg-desktop-portal-hyprland` instead of this compositor's own backend -- with no error anywhere on TideWM's side to see, since the request never reached it. `main.rs` now detects this (checks the running frontend's actual `/proc` environment, not just the activation environment) and restarts only the frontend, once, at startup, when it's stale. Discord's own screen-share also confirmed working end to end on the same real hardware session.

- Keys no longer get stuck "held" in a nested session when the host compositor takes keyboard focus mid-chord (e.g. the host's own Super+L lock shortcut): the release the host swallowed left xkb's modifier state -- and the `wl_keyboard.modifiers` every client is told -- carrying a phantom Super, so plain drags acted as Super-drags and kitty decoded typing as CSI-u escape sequences. On host focus loss TideWM now synthesizes releases for everything still marked pressed, the same all-released semantics `wl_keyboard.leave` implies. Found by the Nvidia nested test pass, which also identified the root cause.
- Resizing the nested window (or applying a wlr-output-management transform/scale change) now actually retiles: the layer map's cached usable-area zone was only ever recomputed on layer-surface events, so `retile()` faithfully laid tiles out into the *old* output size while the wallpaper filled the new one. Reproduced live with a host-side resize, fixed by arranging the layer map at both mode-change sites.
- The nested backend now queries the host monitor's real refresh rate (was hardcoded 60Hz for both the advertised mode and the render-loop cadence -- the loop stays a bounded timer, just at the real rate), forwards the host's real scale factor including fractional (was always 1, which mis-sized anything DPI-aware on a scaled host monitor), and titles its window "TideWM" instead of the "Smithay" default.
- Screenshots taken inside a nested (winit) TideWM session are no longer vertically flipped. The output-capture render inherited the output's advertised transform, and the winit backend advertises `Flipped180` purely to cancel its EGL surface's y-orientation at present time -- baking that into the offscreen capture texture inverted the readback. Output captures now render with an explicit `Normal` transform, same as the window-capture path always did, and the capture privacy black-out rects follow the same transform so they stay aligned. Verified with grim in a nested session, full-output and region captures both. The udev backend (a normal, unrotated output) was never affected. Nested screencast frames shared the same pixels, so their orientation changes identically; real OBS/Discord screencasting was verified on udev and is untouched.

### Known issues
- DMA-BUF export is still disabled and still fails on real hardware, unrelated to the fixes above; MemFd/SHM is the supported transport.
- Portal virtual sources mirror the desktop instead of creating a real headless output.
- winit 0.30.13 (upstream, nested backend only) can panic with `failed to get pointer data` when the host changes seat capabilities under it (seen with a host session lock); crashes the nested dev process, not fixable from TideWM's side.
- A lone tiled window not filling the output, seen once in the Nvidia nested pass on a 125% host monitor, could not be reproduced at scale 1 and is plausibly the scale bug fixed above; needs a retest on a fractional-scale host.

## [0.58.0] - 2026-07-22

### Added
- A real `xdg-desktop-portal` ScreenCast backend (`org.freedesktop.impl.portal.ScreenCast`), self-contained, no `xdg-desktop-portal-gnome`/GTK4 chain needed. v1 is monitor-only, one stream, no source picker yet.
- Ships `share/xdg-desktop-portal/tidewm.portal` and `tidewm-portals.conf` for install.

## [0.57.0] - 2026-07-22

### Added
- Persistent on-screen panel for config parse errors, replacing the old timed toast for that case.
- Built-in 4K Tide wallpaper fallback, replaced by any layer-shell wallpaper tool once one is running.
- `wp-security-context-v1`: sandboxed clients can't see session-lock, IME, clipboard, capture, or output-management globals.
- Per-window capture/screencast source selection, and a full-output DMA-BUF screenshot fast path on DRM sessions.
- Daily-use additions: `resize-*` actions, IPC batch requests, regex window matching, initial fullscreen/maximize rules, per-window `block_capture`.

### Fixed
- Nested sessions no longer leak the host compositor's desktop identity into spawned children.
- Interactive move/resize now needs a real held Super key, not just a latched modifier.

## [0.56.0] - 2026-07-22

### Added
- Output screencasting over PipeWire (SHM-backed).
- AccessKit/AT-SPI tree for TideWM's own UI: workspaces, overview, toasts.
- Compositor workspace swipes on the touchpad.
- Primary-selection protocol support.

### Fixed
- Floating-window output-disconnect migration, popup null-buffer lifecycle, touch-tap focus, capture cursor parity.
- Process lifecycle hardening: children are reaped via SIGCHLD, IPC/capture/DBus connections get bounds and idle timeouts.

## [0.55.0] - 2026-07-22

### Added
- `org.freedesktop.a11y.KeyboardMonitor` (behind the `accessibility` feature): lets a screen reader like Orca grab or watch keys system-wide, ported from niri's implementation.

## [0.54.0] - 2026-07-21

### Added
- Window-rule and layout tier: `no_focus`, `position`, `size` rules; `master_orientation`; `workspace_auto_back_and_forth`; `toggle-dpms` action; `cursor_hide_after_ms`; `bsp_split_bias`; named workspaces; per-namespace layer-shell capture exclusion.
- `raise-window`/`lower-window`, most-recently-used `cycle-focus`, urgent-window tracking, auto-float for dialogs, no-modifier floating edge-resize, touchscreen input, disconnected-output window migration.

## [0.52.1] - 2026-07-21

### Fixed
- Touchpad config now hot-reloads for an already-connected device, not just a newly plugged one.

## [0.52.0] - 2026-07-21

### Added
- Replaced TOML config with Waves, TideWM's own line-based format (`config.wave`), closer to Hyprland's syntax.
- `$wave(a, b, c)`: resolves to the first installed candidate, used to make `terminal` portable across machines.
- `cursor_always_visible` config key.

## [0.51.3] - 2026-07-21

### Fixed
- `wl_output` global leak on monitor disconnect.
- A fence-wait error that could hang the compositor forever on a bad GPU fence.
- A startup panic when no outputs are mapped yet.

## [0.51.2] - 2026-07-21

### Tests
- First real multi-monitor hardware test: hotplug connect works; disconnect left a stale `wl_output` global (fixed in 0.51.3).
- `ext-session-lock-v1` verified live against `hyprlock`, including a real unlock.

## [0.51.1] - 2026-07-21

### Fixed
- The welcome hint's "delete to dismiss" setting doing nothing.

### Tests
- Real-hardware pass on AMD: DPMS, gamma/night-light, workspace switching, pseudo-tiling, and pin all confirmed working. Flat ~50MB PSS over 30 minutes.

## [0.51.0] - 2026-07-21

### Added
- First-run welcome hint, replacing the old auto-spawned terminal.
- Real CLI flags: `-c`/`--config`, `-v`/`--version`, `-h`/`--help`; `-s`/`--spawn` for a one-off launch command.
- Default terminal changed to `kitty`.

## [0.50.0] - 2026-07-21

### Added
- Keyboard layout config (`xkb_layout` and friends).
- Touchpad config: tap-to-click, natural scroll, accel, click/scroll method.

### Fixed
- A startup crash on an invalid keymap.

## [0.49.0] - 2026-07-20

### Added
- Workspace overview (`Super+O`): a schematic grid of every workspace, boxes rather than live thumbnails.

## [0.48.0] - 2026-07-20

### Added
- Second tiling algorithm, master/stack, alongside the existing adaptive BSP. `default_layout` config key, runtime switch, and ratio keybinds.

## [0.47.0] - 2026-07-20

### Added
- `[env]` block and `$variable` substitution in config, Hyprland's `$mainMod` idea.

## [0.46.0] - 2026-07-20

### Added
- Multi-file config via `include`.

## [0.45.0] - 2026-07-20

### Added
- Submaps: temporary keybind layers (sway/Hyprland's "mode"), plus a default vim-motion nav submap on `Super+N`.

## [0.44.0] - 2026-07-20

### Added
- `zwlr-gamma-control-manager-v1` for night-light tools (wlsunset, gammastep).

## [0.43.0] - 2026-07-20

### Added
- `wlr-output-power-management-unstable-v1` (DPMS), with a real DRM CRTC power hook on the udev backend.

## [0.42.0] - 2026-07-20

### Added
- Session environment export (`WAYLAND_DISPLAY`, `XDG_CURRENT_DESKTOP`) pushed to systemd/D-Bus at startup.

## [0.41.0] - 2026-07-20

### Added
- `tidectl`, a small CLI over the IPC socket.

## [0.40.0] - 2026-07-20

### Added
- `wlr-output-management-unstable-v1`: live position/transform/scale changes for kanshi, wlr-randr, wdisplays.

## [0.39.0] - 2026-07-20

### Added
- `wp-cursor-shape-v1`, plus a real per-shape xcursor lookup instead of always showing the default arrow.

## [0.38.0] - 2026-07-20

### Fixed
- A startup crash when both `XDG_CONFIG_HOME` and `HOME` are unset.

### Audited
- Distro-portability pass: no hardcoded paths, no unguarded GPU-vendor assumptions.

## [0.37.0] - 2026-07-20

### Added
- `wlr-foreign-toplevel-management-v1`, for waybar's `wlr/taskbar` and ags v1.

## [0.36.0] - 2026-07-20

### Added
- Screencast DBus interface scaffolding (`org.gnome.Mutter.ScreenCast`). PipeWire streaming itself wasn't implemented yet at this point.

## [0.35.0] - 2026-07-20

### Added
- IME support: `zwp-text-input-v3`, `zwp-input-method-v2`, `zwp-virtual-keyboard-v1`.

## [0.34.0] - 2026-07-20

### Changed
- README rewritten to match the structure of other well-known compositor READMEs.

## [0.33.0] - 2026-07-20

### Changed
- Repo hygiene: stopped tracking IDE files, expanded `.gitignore`.

## [0.32.0] - 2026-07-20

### Added
- `zwp-keyboard-shortcuts-inhibit-v1` (VM/remote-desktop key capture) and `zwp-pointer-gestures-v1`.

## [0.31.0] - 2026-07-20

### Added
- `ext-foreign-toplevel-list-v1`: read-only window list for taskbars and switchers.

## [0.30.0] - 2026-07-20

### Added
- Declared MSRV (1.86). Added a `.desktop` session entry for display managers.

### Fixed
- New windows opening on the wrong monitor when spawned via a keybind.

## [0.29.0] - 2026-07-20

### Added
- `ext-session-lock-v1`: real screen-lock support (swaylock, hyprlock).
- `zxdg-decoration-manager-v1` plus KDE's decoration protocol, both enforced server-side.
- Per-app window rules (`[[window_rule]]`): workspace/output/float/pseudo_tile/pin applied on map.
- `xdg-activation-v1`, `wp-single-pixel-buffer-v1`, `wp-presentation-time`, `wp-fractional-scale-v1`.
- `wp-pointer-constraints-v1` + `wp-relative-pointer-v1` for FPS-style mouse look, verified live against real Minecraft.

## [0.28.0] - 2026-07-19

### Removed
- The built-in hotkey-overlay cheat sheet. A first-party UI like that belongs outside a window manager, not inside one.

## [0.27.0] - 2026-07-19

### Added
- `wlr-data-control-unstable-v1` for clipboard managers (cliphist, wl-clip-persist).

## [0.26.0] - 2026-07-19

### Added
- Screenshots: `wlr-screencopy-unstable-v1` and `ext-image-copy-capture-v1`, output and region capture.
- Lid-switch and tablet-mode events can now trigger config actions.
- Window groups/tabs: merge windows into one tile, tab-strip UI, cycle/ungroup.
- Hotkey-overlay cheat sheet (removed again in 0.28.0).

## [0.25.0] - 2026-07-19

### Added
- Real xcursor-theme cursor on the udev backend, replacing the placeholder dot.

## [0.24.0] - 2026-07-19

### Fixed
- Explicit restoration state for fullscreen/maximize/floating/tiling/pinning across every transition: workspace swap, output change, interactive grabs.
- Unbounded memory growth from empty workspace trees on arbitrary workspace IDs.

## [0.23.0] - 2026-07-19

### Fixed
- Explicit XDG popup grab lifecycle: correct pointer/keyboard handoff, dismiss-on-outside-click, deadlock-safe teardown.

## [0.22.0] - 2026-07-19

### Fixed
- Centralized keyboard focus and XDG activation authority, replacing several places that used to set it independently.

## [0.21.0] - 2026-07-19

### Fixed
- Explicit XDG toplevel and layer-shell buffer lifecycle: nothing tiles, renders, or focuses before a real buffer maps.
- Pinned Smithay to niri's known-good revision for a required layer-shell lifecycle fix.

## [0.20.1] - 2026-07-19

### Measured
- First real-hardware idle footprint: ~60MB PSS, ~1% CPU, 0% GPU at idle on AMD. Same machine's Hyprland at idle: ~137MB PSS, ~2.8% CPU, ~16% GPU.

## [0.20.0] - 2026-07-19

### Added
- Closing the focused window now refocuses whatever the pointer is already over.

## [0.19.0] - 2026-07-19

### Added
- Tiled-window resize via `Super`+right-drag.
- Re-enabled tiled-window drag-to-swap after the deadlock fix in 0.15.1/0.16.0, pending a hardware retest.

## [0.18.0] - 2026-07-18

### Added
- Idle-inhibit and idle-notify (`zwp-idle-inhibit-manager-v1`, `ext-idle-notify-v1`), verified live against `hypridle`.

## [0.17.0] - 2026-07-18

### Fixed
- Render-timing hardening on the udev backend: GPU fence waits, empty-frame retry, DMA-BUF readiness blocking.
- Fullscreen state not surviving a floating/tiled transition or a cross-output workspace swap.

## [0.16.0] - 2026-07-18

### Fixed
- Root-caused and fixed the 0.15.1 hardware freeze: a self-deadlock in the tiled drag-to-swap grab. Kept disabled pending a real retest.
- Hardened all four interactive pointer grabs against a client being destroyed mid-drag.
- A crash reachable by a client with two mapped surfaces racing a grab.

## [0.15.1] - 2026-07-18

### Fixed
- Disabled tiled-window drag-to-swap after it froze the entire machine on its first real-hardware test. Pseudo-tiling, shipped the same version, was unaffected and stayed on.

## [0.15.0] - 2026-07-18

### Added
- Interactive tiled-window drag-to-swap (`Super`+left-drag) and pseudo-tiling (`Super+Shift+P`). See 0.15.1: the drag feature was disabled immediately after a hardware freeze.

## [0.14.0] - 2026-07-18

### Added
- Scratchpad, pin (`toggle-pin`), and cross-monitor workspace swap.

## [0.13.0] - 2026-07-18

### Added
- Minimal IPC/control socket, the first version of what `tidectl` now runs on.

## [0.12.0] - 2026-07-18

### Added
- `spawn_at_startup` config list and per-output config (`[[output]]`: resolution, position, scale, transform).

## [0.11.0] - 2026-07-18

### Added
- udev/DRM backend verified on real hardware for the first time (AMD): modeset, input, and VT switching all working.
- Focus-follows-mouse.

## [0.10.0] - 2026-07-17

### Added
- Fullscreen and maximize.
- Workspaces: one independent tiling tree per output, `Super+1..9,0` to switch.

## [0.9.0] - 2026-07-17

### Added
- XWayland support via `xwayland-satellite`.

## [0.8.0] - 2026-07-17

### Added
- Multi-monitor tiling: one tiling tree per output. Runtime output hotplug on the udev backend.

## [0.7.0] - 2026-07-17

### Added
- wlr-layer-shell support: bars, launchers, lock screens.

## [0.6.0] - 2026-07-17

### Added
- Directional focus/swap (`Super+hjkl`), split-ratio drag-resize.

## [0.5.0] - 2026-07-17

### Added
- udev/DRM backend: standalone TTY session, no host compositor required. The first real backend beyond the winit dev scaffold.

## [0.4.5] - 2026-07-17

### Added
- Release profile tuning cut the binary from 10.9MB to 6.6MB.

### Fixed
- A bad memory measurement (debug build plus raw RSS instead of release plus PSS) that made TideWM look far heavier than it actually is.

## [0.4.4] - 2026-07-17

### Fixed
- Floating windows falling behind the tiled layer on every retile.

## [0.4.3] - 2026-07-17

### Added
- Compositor-level `Super`+drag to move/resize floating windows.
- `Super+Tab` focus cycling.

## [0.4.2] - 2026-07-17

### Fixed
- Idle CPU pinned near 99% from an unthrottled redraw loop. Dropped to ~2%.

## [0.4.1] - 2026-07-17

### Added
- Floating-window toggle (`Super+V`). Windows now auto-focus on map.

## [0.4.0] - 2026-07-17

### Added
- Dynamic dwindle-style tiling layout engine.

## [0.3.1] - 2026-07-17

### Fixed
- The redraw loop compositing every frame unconditionally, even fully idle.

## [0.3.0] - 2026-07-17

### Added
- Config hot-reload and the first on-screen toast notification.

## [0.2.0] - 2026-07-17

### Added
- TOML config system and compositor-level keybinds.

## [0.1.0] - 2026-07-17

Initial scaffold. No water yet, this is the plumbing.

### Added
- Winit backend, xdg-shell support, basic move/resize grabs, adapted from Smithay's `smallvil` example.
