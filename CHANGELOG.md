# Changelog

All notable changes to TideWM are documented here. Format loosely follows [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added
- Layout movement now interpolates window position and size as one interruption-safe rectangle. A retile begins from the current on-screen geometry even when another animation is still running, then non-uniformly scales the already-imported client surfaces directly toward the new layout slot. Popups, rounded clipping, borders, shadows, frost/water glass, depth overlays, close snapshots, visible winit/udev frames, and every capture path use the same sampled size. This adds no framebuffer or texture cache. `movement { animate_size = true|false }` controls it and defaults on for every animation preset; `resize`, `size`, and `animate-size` are accepted aliases. The maintainer manually verified rapid open/close retiles in the release nested backend: the surviving window, border, and shadow resized together without snapping or flicker. The two-Kitty session measured 118,408KiB PSS with 0KiB swap.
- Window lifecycle and layout-motion animation timing completes the next R2 foundation slice. The global `animations { }` block has independent `open`, `close`, and `movement` sub-blocks with enablement, duration, logical-pixel travel, start/end opacity, built-in easings, and CSS-compatible `cubic-bezier(x1,y1,x2,y2)` curves, plus a shared slowdown multiplier. Geometry and opacity can use separate durations and curves. Lifecycle origins can use a fixed offset, force an output edge, or choose the nearest edge with the same midpoint-distance and tie-order behavior as Hyprland's unforced `slide`. Named `tide`, `wave`, `riptide`, and `hypr-smooth` presets establish adjustable baselines; `hypr-smooth` mirrors the maintainer's real 300ms open/close geometry, 400ms fade and movement clocks, and exact custom Béziers. TideWM's trajectories can add one broad perpendicular swell or a configurable decaying wave over the ordinary eased path; amplitude, cycles, and decay remain independently tunable per transition. Logical focus/input/layout state changes immediately, and interruption-safe movement retargets from the real on-screen position. Closing now transfers the last already-imported surface textures from the normal frame walk into a bounded snapshot, covering both null-buffer unmap and direct xdg-role destruction while allowing Smithay to release live client state normally. The snapshot clones GPU handles rather than copying a window into a new framebuffer. Open/move transforms cover client surfaces, popups, rounded clipping, borders, shadows, water/frost glass, depth overlays, visible winit/udev output, screenshots/screencasts, and workspace-transition captures. Animation state is bounded to mapped windows plus in-flight closing windows and is pruned by the shared redraw clock even when an output is not presenting.
- Polished impulse presets replace the old debug-like circle/box look as the default: `water-drop` layers concentric wavefronts and an impact crater, `jelly` visibly jiggles an organic membrane, `bubble` adds a double rim and moving highlight, `splash` draws a lobed crown with spray peaks, and `tide` flows several wave bands through one impulse. `legacy` preserves the original stackable ring/square/droplet/cross renderer, and assigning `shapes` selects it automatically for compatibility. `map_preset`, `focus_preset`, and `urgent_preset` can choose a different appearance for each event. Automatic focus during initial map is coalesced into the map event by default, preventing the map and focus appearances from stacking; `focus_on_map = true` deliberately restores the layered look, while later real focus handoffs always animate normally. Two-color gradients plus `glow`, `wobble`, and `detail` controls remain globally and per-app adjustable. Adaptive sizing supports fixed pixels, window diagonal, width, height, shortest side, or longest side with scale and min/max clamps. New top/bottom/left/right/nearest-edge anchors have adjustable along-edge position and signed inward/outward distance. Reusable `ripple_preset <name> { }` blocks can contain every ripple field, inherit another named preset, and be selected globally, per trigger, or per app. Cycles and missing names degrade safely. Every polished preset is one bounded analytical shader element with no textures, buffers, or per-frame allocations.
- Configurable compositor mouse modifier via `pointer_modifier` (`mouse_modifier` and `drag_modifier` aliases). Left-drag moves floating windows or drag-swaps tiled windows; right-drag resizes either. It accepts Super/Alt/Ctrl/Shift combinations, checks physically-held keys to stay safe in nested sessions, hot-reloads, defaults to Super for existing configs, and the generated config points it at `$mod`.
- Rounded client geometry and analytical gradient borders (`src/decoration.rs`), completing the familiar visual half of Phase R2. The global `rounding { }` block supports one radius or independent CSS-order corner radii, Hyprland-style superellipse `power`, physical-pixel antialias width, real client clipping, floating-only scope, and fullscreen opt-in. Sparse `rule { rounding { } }` overrides merge field by field; `corners`, `geometry_corner_radius`, `clip_to_geometry`, and shorthand radius/on/off forms are accepted. The texture override follows niri’s clipped-surface architecture: texture coordinates are mapped back into the toplevel geometry while each main surface/subsurface element draws, and real xdg-popups remain separate and unclipped. The same resolved geometry now shapes borders, water/frost glass, and zero-radius shadows, preventing square effect layers from leaking through transparent corners. The global and per-app `border { }` blocks expose width, outside/center/inside placement, independent active/inactive/urgent start/end RGBA colors and opacity, gradient angle, radius offset, antialiasing, scope, and opt-in rotation/pulse animation. Focused, inactive, and urgent animation gates are independent; inactive borders can remain moving, stay visible but static, or disappear for a focus-ring-only setup. Equal endpoints make a solid border. Borders are one analytical pixel element per window with no textures or growing cache; static borders remain damage-driven, while animated borders use the shared redraw clock and an advancing commit counter. Popups → border → clipped surface → glass → shadow ordering is shared by winit, udev, screenshots/screencasts, backdrop captures, and workspace-transition captures. Release nested AMD GLES screenshots verified asymmetric clipping on transparent frost and opaque tiled clients, active aqua and dark inactive gradients, shadow/glass radius alignment, and live gradient advancement. The two-window animated session measured 107,494KiB PSS and 0KiB swap. Standalone udev compiles through the same path but still needs real-hardware visual verification.
- Adjustable analytical drop shadows (`src/shadow.rs`), the second Phase R2 decoration. TideWM combines niri’s CSS-like softness, signed spread, offset, draw-behind behavior, and active/inactive colors with Hyprland’s range aliases, render-power falloff, sharp mode, and scale. TideWM adds a configurable aqua urgent/bioluminescent state, independent active/inactive/urgent opacity multipliers, per-shadow corner radius, `floating_only`, and an opt-in fullscreen path. The global `shadow { }` block is inherited by sparse per-app `rule { shadow { } }` overrides; multiple matching sub-blocks merge field by field, and `shadow = on|off|none` is a shorthand. Colors retain alpha and accept CSS/Hyprland compact/legacy forms. Shadows are fixed-cost signed-distance fields with no shadow textures, blur framebuffers, or growing cache. `desktop_render_elements` now returns the renderer-concrete output element enum so each shadow can be placed immediately behind its own surface tree rather than in one incorrect all-shadows layer; glass-replaced floating windows get the same surface → glass → shadow ordering. Visible winit/udev frames, screenshots/screencasts, backdrop captures, and workspace-transition captures therefore share the effect. `draw_behind_window = false` is the color-safe default: the shader cuts out the actual window body, preventing the gray/cyan full-window filter previously seen through translucent apps. Shadows are ordinary decoration and remain enabled when `water_effects = false`. Unit tests cover the shader contract, every tuning field, RGBA parsing, sparse per-rule inheritance, and multi-rule merging. Verified in a release nested AMD GLES session with a centered client-native-alpha Kitty over an opaque control: the focused preset produced the configured soft aqua outside falloff with no shadow color across the client body/text; `cycle-focus` changed it in-place to the configured dark inactive shadow. The session measured 115,786KiB PSS and 0KiB swap. Standalone udev wiring compiles and shares the same element walk, but still needs a real-hardware visual pass.
- Frost and transparency tuning is now fully rule-aware. Every global `frost { }` field can be overridden per app with `rule { frost { } }`; matching blocks merge field by field over the global baseline. The bounded shader now exposes blur radius and strength, independent processed-layer opacity, saturation, contrast, brightness, banding-reduction noise and scale, vibrancy with a dark-pixel bias, optional tint color/alpha, and rounded-edge radius/softness. Neutral defaults (`tint_alpha = 0`, saturation/contrast/brightness `1`) add no color wash. The extra controls stay in the same fixed 25-tap pass and allocate no new textures. Window rules also gained `active_opacity`/`focused_opacity`, `inactive_opacity`/`unfocused_opacity`, and `fullscreen_opacity`; they multiply the existing base `opacity`, select at render time from real focus/fullscreen state, and therefore update without remapping the window. Fullscreen takes priority. This follows the useful parts of Hyprland's active/inactive/fullscreen opacity and contrast/noise/vibrancy model plus niri's per-window background-effect overrides, while preserving TideWM's existing last-rule-wins and bounded-render design. Release tests cover parsing, per-rule inheritance, and state-multiplier priority. The expanded neutral per-app frost was visually inspected in a nested AMD GLES session with Kitty client-native background alpha: glyphs remained opaque, no tint overlay appeared, and the backdrop stayed blurred. The active session measured 114,193KiB PSS with no swap.
- Selectable frosted glass (`src/frost_glass.rs`), the first Phase R2 decoration. A floating-window rule can now choose `glass = frost`, keep the existing `water` refraction, or use `none` for ordinary transparency. Explicit glass modes work with client-provided surface alpha, the preferred path because a terminal such as Kitty can make only its background translucent while keeping glyphs fully opaque. TideWM’s compositor `opacity` still multiplies the complete client surface; for backward compatibility, setting it below one without an explicit mode continues to imply water glass. The global `frost { }` block controls enablement, physical-pixel radius, saturation, brightness, tint color, and tint alpha; tint alpha defaults to zero for neutral, color-free frost while remaining adjustable. Hot reload re-resolves mapped windows and releases stale captures. Frost reuses R0.5’s pre-frame, reusable window-sized backdrop texture and runs a bounded 25-tap Gaussian/color pass with no additional per-frame texture allocation. The initial sparse 13-tap candidate was rejected during screenshot inspection because it produced stepped duplicate images around the wallpaper logo; the dense 5×5 weighting replaces those ghosts with overlapping samples. Capture work is limited to floating windows whose selected mode will actually consume it, and the first successful capture requests exactly one follow-up frame so an otherwise-static desktop cannot stall before displaying glass. The visible DRM/udev path uses the same shared glass replacement and pre-frame backdrop capture as winit; screenshots and workspace-transition captures share mode selection. `water_effects = false`, `frost.enabled = false`, deep schematic windows, and `glass = none` bypass the work. Unit tests guard the shader/tap contract, all frost config fields, mode parsing, and last-rule-wins behavior. Verified in a release nested session by capturing and directly inspecting the frosted output: backdrop edges softened while client-native alpha kept terminal glyphs fully opaque. The active frost window measured 108,587KiB PSS with no swap, far below the 1.5GiB target. Standalone DRM wiring compiles but has not yet had a real-hardware visual pass.
- Automatic attention depth and buoyancy (`src/depth.rs`), completing the first Phase R1 identity slice. Every mapped window starts at tier zero, sinks to tier one after configurable inactivity with reduced live-content opacity and an analytical cool-water wash, then switches to a cached box-and-title schematic at tier two and below. Focus, clicking, or keyboard input returns it to the surface immediately. Urgent windows keep a configurable cyan bioluminescent border through every tier. The global `depth { }` block exposes enable, timing, maximum tier, live opacity, wash, schematic, border, and urgent colors/alphas; `water_effects = false` remains the master bypass. State is bounded to one timestamp/tier/element identity per mapped toplevel, deep schematic buffers are evicted on resurfacing or unmap, and the backend timer’s inactivity scan is throttled to 10Hz. Visible winit/udev frames, screenshots, and workspace-transition captures all share the same depth replacement path. A release nested test used two-second tiers: the user observed the cool wash, schematic replacement, and immediate keyboard buoyancy working in sequence. The active schematic added about 4MiB PSS in that output size; two consecutive ten-cycle cache release/rebuild batches ended at exactly the same 131,671KiB PSS reading, with no per-cycle growth. Unit tests cover timing boundaries, cap/disable behavior, shader uniforms, config parsing, and generated defaults.
- Optional synchronized workspace motion for the water/glow transition. `workspace_motion = true` captures the incoming desktop as well as the outgoing one, then slides both edge-to-edge in the configured wave direction while the procedural effect stays above them. `workspace_motion_delay_ms` controls how long the water leads before desktop movement begins (`100`, `200`, and `300` are 0.1, 0.2, and 0.3 seconds); motion then eases across the remaining transition lifetime. It is disabled by default because the incoming capture raises the bounded transient allocation from one to two ARGB8888 full-output textures per actively transitioning output: about 15.8MiB total at 1080p or 63.2MiB at 4K. A failed second capture safely falls back to the original static incoming workspace rather than cancelling the switch.
- Captured wave workspace transitions (`src/workspace_transition.rs`), the third visible piece of Phase R1. A user workspace action captures the outgoing desktop after its submitted frame and keeps the incoming workspace live underneath. The default `style = water` is a real two-stage full-screen transition: a procedurally shaded body with moving caustic streaks, a curling/scalloped foam crest, and spray floods across the outgoing workspace; once water covers the complete output, its trailing edge continues in the same direction to reveal the incoming workspace. The earlier slimmer colored sinusoidal boundary remains available as `style = glow`. Forward and backward switches use opposite directions by default (`direction = auto`), or either direction can be forced; compositor chrome remains live above both styles. The default water body opacity is `0.88`, leaving the workspace subtly visible beneath it. Protocol-driven toplevel activation retains an immediate internal switch path so focus can still move to a hidden surface in the same dispatch. Transient state is bounded to one pending target and one ARGB8888 full-output texture per output by default (about 7.9MiB at 1080p or 31.6MiB at 4K); optional synchronized workspace motion adds the second bounded texture documented above. Newer requests replace rather than queue, and completion, lock entry, output removal, or another switch releases the entire transition. Both procedural styles are analytical shaders and allocate no textures beyond those captures. `water_effects = false` bypasses capture and animation completely. Winit, udev, and the separate screenshot path all carry the transition element. The `workspace_transition { }` block exposes `enabled`, `style`, `duration_ms`, `speed`, five easing curves, automatic/forced `direction`, wave geometry, shared water/front color, glow core/halo appearance, and water body opacity/depth, foam color/size/opacity, spray, and turbulence. Hot reload applies tuning to the next switch; disabling the block clears active/pending state without suppressing water-glass or ripples. Verified live in a release nested session in both directions, with exaggerated pink/green glow fronts, and with the water body visibly filling the output; the user approved the water look and requested a faster 600ms personal preset. The isolated `water_effects = false` session switched immediately, and ten completed capture cycles settled within 0.5MiB PSS of the pre-cycle reading rather than growing per switch. Unit tests guard the shader contract, direction mapping, config parsing/defaults, and texture-cost arithmetic.
- Ripple customization (`config::RippleConfig`, parsed from a new global `ripple { }` block and per-app `rule { ripple { } }` sub-blocks, matching the same precedence shape other rules already use). The droplet ripple shipped in the previous entry was a single hardcoded look -- now every visual knob is tunable: shape (`shapes = ring square droplet cross`, multiple shapes layer concurrently), color (`color = #RRGGBB` hex), peak radius, outline thickness, lifetime (`duration_ms`), peak transparency (`peak_alpha`), easing curve (`ease = linear|cubic-out|cubic-in-out|quad-out|exp-out`), anchor point (`anchor = center|cursor|topleft|topright|bottomleft|bottomright` plus an `offset = <dx>x<dy>`), z-order (`layer = above-all|above-windows|below-windows|below-all` so a ripple can sit over everything, over windows, between windows and wallpaper, or behind the wallpaper), and which events fire one (`triggers = map focus urgent`, map-only by default since every-focus-change ripples can feel busy). Per-app `rule { ripple { } }` overrides merge over the global block field-by-field so a rule can set just one knob and inherit the rest, and `rule { ripple = none }` is a one-line shorthand for "no ripple on this app." The shader was restructured into a single branching fragment program with a `u_shape` uniform (0=ring, 1=square, 2=droplet, 3=cross) rather than four compiled programs -- a ripple with multiple shapes produces one render element per shape per frame sharing the same bounding square, alpha, and tint. Radius easing applies to the progress value (not the alpha fade, which stays quadratic regardless -- an eased radius on top of an eased alpha reads as impact+bounce rather than water). Alpha handling switched to pre-multiplied (`vec4(u_tint * a, a)`) to match the `water_glass` shader's blend convention. The focus-change trigger is wired up: a ripple fires at the newly-focused window's configured anchor only on a real window-to-window handoff (not on the very first focus, where it would be redundant with the map ripple, and not on focus-dropped-to-None, which is noisy). The urgent-hint trigger parses but has no producer wired yet -- landing that needs xdg-protocol urgent-hint detection, which is a separate piece of work. `water_effects` remains the master identity toggle; `ripple { enabled = false }` is the per-scope kill, and both are checked at spawn time so either can suppress a ripple. Hot-reload picks up ripple config changes automatically (the whole `Config` struct is swapped on `Config::reload`). All three render paths (winit, udev, and the separate screenshot capture in `capture.rs`) now splice ripple elements by their configured layer: AboveAll frontmost, AboveWindows between chrome and windows, BelowWindows between windows and wallpaper, BelowAll at the very back. Unit tests cover the shader-source guard, easing monotonicity/boundedness across all five curves, lifetime/geometry, and per-shape element multiplicity. Live visual verification of the new knobs is the user's next nested-session step.
- Impulse ripple (`src/ripple.rs`), the second piece of Phase R1's identity slice: one shared primitive for a radial disturbance from a point, decaying over time. The first trigger wired up is a window mapping -- a droplet impact at the window's center, the most universally recognizable "water" cue and the easiest to verify (open a window, see the ripple). Focus-change and urgent-hint triggers are explicit later scope, see AGENT.md's Phase R1 entry. The primitive is purely analytical: closed-form radius/alpha given elapsed time, no per-frame simulation. Radius uses ease-out cubic (fast expansion that decelerates as it approaches peak, the shape a real droplet impact's wavefront makes on a still surface -- energy radiating outward against increasing circumference); alpha uses a quadratic fade so the ring stays visibly energetic for the first half and vanishes rather than trailing into noise. Visualized through a procedural GLES pixel shader (Smithay's `compile_custom_pixel_shader` / `render_pixel_shader_to`, confirmed in the pinned rev when this work started) that draws a soft expanding ring with a fixed 8-pixel half-width, so the ring's visual thickness stays constant as it grows rather than scaling with the element's bounding square. Default tint is pale cyan (TideWM's identity color); default peak radius 220 logical px; default lifetime 650ms. A new `Ripple` variant on `OutputRenderElements` carries the per-frame render element, which meant making the enum's renderer-concreteness decision (already taken for water-glass) carry through naturally. `Smallvil::ripples: Vec<Ripple>` is bounded by a 16-active-ripple cap (`MAX_ACTIVE_RIPPLES` in `spawn_window_map_ripple`) so rapid window mapping can't grow it unboundedly; finished ripples are `retain`-pruned as a side effect of every `ripple_frame_elements` call; `has_active_animation` now returns true while any ripple is still in flight, so both backends' idle redraw-arming loops keep the compositor ticking for the lifetime of each impulse (the same hook the toast fade already used). Render placement: above windows but below toast/overview/picker/tab-strip chrome in both backends' element chains, plus the separate screenshot capture path in `capture.rs::render_one_capture` -- without that, a screenshot mid-ripple would silently drop the ring from the captured frame, the same "separate render path forgot the new effect" bug class water-glass already addressed and session-lock's element ordering burned once before either. Gated on the existing `water_effects` config toggle, same as water-glass: both are R1 effects that aren't meaningful with the identity off. Build- and unit-test-clean (84 tests passing, including shader-source guards and lifetime/geometry tests for the new primitive); live visual verification in a nested session is the user's next step.
- Water-glass (`src/water_glass.rs`), Phase R1 of the render/visual-identity roadmap -- the first thing this roadmap produces that's actually visible on screen, and the first thing to sample the backdrop captured in R0.5. A floating window with an `opacity` window rule below 1.0 now renders its captured backdrop through a custom GLES fragment shader that perturbs the sample coordinate with a small position-based sine/cosine offset, giving a wavy refracted look instead of the flat content that would otherwise show through a semi-transparent window. Triggered by reusing the existing `opacity` rule rather than adding a new config key, since "what shows through a semi-transparent window" is exactly what that already means. The water-glass layer draws fully opaque at the window's own rect behind the window's own (semi-transparent) surface element: without that, the real undistorted backdrop would still show through underneath the distorted copy and read as a ghost rather than glass. The shader is deliberately static (no time uniform) for this first cut -- animating the ripple is a later polish pass once something is driving it through the R0 `Animation` clock. To make this possible at all, `OutputRenderElements` (the per-backend element enum both visible-frame loops and the screenshot path thread through) had to drop its generic-renderer parameter and become concrete over `GlesRenderer`: a `WaterGlass` variant draws via a custom `GlesTexProgram`, which has no generic-renderer equivalent. The enum was already only ever instantiated with `GlesRenderer` (there is no second renderer backend in this codebase), so making both concrete costs nothing in practice. `desktop_render_elements`'s `skip: Option<&WlSurface>` simultaneously became `skip: &[WlSurface]` so the same walk can pull every water-glass-eligible window out of its normal z-slot at once, not just one -- more than one can be eligible on the same output. Eligible windows (water-glass layer + their own semi-transparent surface element on top) get prepended ahead of the rest of the space walk, putting them topmost among windows for this first cut -- real multi-window z-order among them is deliberate later scope, see `water_glass.rs`'s module doc. The separate screenshot render path (`capture.rs::render_one_capture`) received the same water-glass substitution, not just both visible-frame loops: without it a screenshot of a water-glass window would show its plain unrefracted content instead of what's actually on screen, the exact "separate render path forgot the new effect" bug class this codebase already hit once with session-lock. `BackdropCapture` (`backdrop.rs`) now carries a stable per-window `Id` and an incrementing `CommitCounter` alongside the texture, so a `WaterGlassElement` built from it reports the right identity to the damage tracker -- a fresh `Id` every frame would leak an orphaned entry in the tracker's per-element bookkeeping for every frame this window is water-glass-eligible, never pruned. Same gating as R0.5's capture: `water_effects` config toggle off suppresses the whole thing. Build- and unit-test-clean (80 tests passing including a new shader-source guard for the substitution point and required uniforms); live visual verification in a nested session is the user's next step (per the project's standard nested-first testing loop). Two further R1 pieces remain after this: the impulse-ripple primitive and the first-pass depth/buoyancy model.
- Backdrop capture (`src/backdrop.rs`), Phase R0.5 of the render/visual-identity roadmap: `capture_backdrop` renders whatever sits behind a floating window's rect into an offscreen texture, the shared plumbing both the familiar frosted-glass blur and TideWM's own water-glass refraction will sample from once their shaders land (Phase R1/R2). `desktop_render_elements` gained a `skip` parameter so the "behind" list is built by walking the real element composition with the target window omitted, not by filtering an already-built list -- the same front-to-back z-order this project's session-lock element-order bug already burned once. Capture runs after the visible frame submits (same FBO-only timing `render_pending_captures`'s screenshot capture already established as safe -- interleaving offscreen work before `submit()` previously broke the winit backend's context lifecycle, confirmed live), so a captured backdrop is one frame behind, consumed building the *next* frame -- the same latency real blur-behind implementations elsewhere accept. Captured textures are stored per-window (`Smallvil::backdrop_textures`) and evicted on unmap/destroy alongside `window_opacity`. Nothing samples the stored texture yet -- capture and storage only, gated on the existing `water_effects` toggle (its first real reader). Verified live in a nested session: the captured region's content matched the real screen at the same position and orientation (a distinctly asymmetric wallpaper shape lined up exactly, ruling out the y-flip this codebase has hit once before in the screenshot capture path), the target window's own content was correctly absent from its own backdrop, and the visible frame was unaffected. Checked PSS/CPU across 10 floating-window open/toggle-floating/close cycles: flat, no growth, confirming the eviction path actually prevents the leak a `HashMap<WlSurface, GlesTexture>` would otherwise accumulate. Two open items, see AGENT.md's roadmap section for detail: a faint, unchased residual in the capture-vs-screenshot diff (concentrated on wallpaper detail, not spread uniformly), and fractional output scale is unverified since `scale` overrides are udev-only and this session's testing was nested-only.
- `Animation`, a small shared linear-interpolation primitive (`src/animation.rs`) -- the first piece of the render/visual-identity roadmap (AGENT.md's "Render and visual identity roadmap", Phase R0). Before this, nothing in the codebase interpolated a value over time at all; `toast.rs`'s fade was hand-rolled elapsed-time arithmetic, now rebuilt on top of this instead. `Smallvil::has_active_animation()` (`state.rs`) also replaces the toast-specific redraw check both backends used to duplicate, giving future water/decoration effects one shared place to plug into instead of each growing its own copy in `winit.rs` and `udev.rs`. No user-visible behavior change -- verified live in a nested session via `grim` screenshots: the config-reload toast still shows at full opacity and still fully clears once its lifetime elapses; a shot taken partway through the fade window showed visible partial transparency (not a hard cutoff), though the exact alpha at that point wasn't pinned to a known instant given unmeasured watcher-debounce latency between the config touch and the toast actually starting. Also checked PSS/CPU across 10 rapid trigger cycles (release build, nested): PSS settled ~10MB above pre-first-toast baseline (the toast's own rasterized texture, expected, not this change) and stayed flat across all 10 further triggers rather than climbing per-cycle, CPU stayed ~1.1-1.7% the whole time including mid-fade, and dropped back to the same idle baseline after each toast cleared rather than staying elevated -- no leak, no stuck redraw loop.
- The urgent-hint ripple trigger, the last of the three documented in AGENT.md's Phase R1 entry. Wired into `mark_urgent`, the existing downgrade path for an `xdg-activation-v1` request with a present-but-stale serial (declines to steal focus, marks the window urgent instead). Single-shot for now, matching the map/focus triggers -- a repeating "pulse until acknowledged" is later scope. Verified live with a throwaway two-window `smithay-client-toolkit` test client: map window A (capture its keyboard-enter serial), map window B (steals focus, staling A's serial), activate A with the now-stale token -- confirmed `fresh=false` and a `trigger=Urgent` ripple spawn via TideWM's own debug trace.

### Fixed
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
