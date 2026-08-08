# TideWM repository audit

Audit date: 2026-08-08  
Audited commit: `aca94940b132` (`master`, matching `origin/master` when the audit began)  
Scope: the complete Rust source tree, build/package entry points, the Wave configuration/runtime, IPC/CLI, inline documentation, and the current roadmap in `TECHNICAL_REPORT.md`/`AGENT.md`.

This is a static code review, not a claim that every issue below was reproduced on hardware. Each item has a confidence label. No source file was changed, and nothing was committed. This report is the only intended working-tree change.

## Implementation handoff

- Updated: 2026-08-08
- Implementation branch: `ai/codex/report-fixes`
- Separate worktree: `/home/fiw/Proyects/TideWM-worktrees/report-fixes`
- Implementation head before this report-only update: `d9f5fcc`
- Current TideWM version on the branch: `0.90.39`
- Push status: local only; nothing from this branch has been pushed.

The finding text below is the original audit evidence. It is intentionally retained even when a finding is closed. Use this handoff ledger as the current status authority, then inspect the named commit and current code before changing a closed area. Do not repeat a fix merely because its original finding still says “confirmed.”

### Current totals

- Critical: all 7 closed.
- High: H-01 through H-44 closed. H-45 was re-audited as a stale false positive because the current udev path already processes connector `Changed` events and rescans/retries surface creation.
- Medium explicitly re-audited and closed: M-01, M-02, M-04, M-09, M-10, M-14 through M-21, and M-30.
- Medium still open or awaiting a fresh audit: M-03, M-05 through M-08, M-11 through M-13, M-22 through M-29, and M-31 through M-74.
- Performance opportunities P-01 through P-10: not worked in this branch unless a closed correctness fix incidentally reduced the same cost. Treat all ten as open until measured and re-audited. P-11 was added 2026-08-08 from real-hardware measurement (not static review) after merging through `d9f5fcc`; open, not yet root-caused.
- Lower-confidence items U-01 through U-16: not systematically re-audited. Treat them as investigation tasks, not established bugs.
- Formatter findings F-01 through F-04: open.
- The dedicated comment/documentation cleanup and the roadmap work are not finished.

### Closed Critical findings

| Findings | Commit | Resolution |
| --- | --- | --- |
| C-01, C-02 | `a64dd40` | Isolated session-lock rendering and capture from client content and pre-lock overlays. |
| C-03 | `f98b62b` | Rejected blocking gamma-control descriptors and enforced the bounded input contract. |
| C-04, C-07 | `17496f1` | Bounded Wave execution, eval traversal, cycles, and output size. |
| C-05 | `c833242` | Kept IPC connection leases for long-lived subscribers. |
| C-06 | `87214f8` | Bound portal sessions to their D-Bus owners and made lifecycle transitions atomic. |

### Closed High findings

| Findings | Commit | Resolution |
| --- | --- | --- |
| H-01 through H-03, H-06 | `9fd1621` | Repaired Classic/Ocean migration visibility, per-output island geometry, floating conversions, and overflow-prone workspace math. |
| H-04 | `c297555` | Routed window-group ownership and lifecycle through Ocean reefs. |
| H-05 | `c071777` | Made Ocean screen-pin ownership authoritative. |
| H-07, H-08 | `c83a859` | Prevented settled or stale floating animation state from sustaining redraws. |
| H-09 | `09cd864` | Stopped reactive glass from treating its own capture commit as new damage. |
| H-10, H-11 | `c4c63a6` | Delayed output publication until DRM construction succeeds and restored dirty state after transient render/queue failure. |
| H-12, H-13 | `32a0652` | Corrected XDG lifecycle events and made screencast window snapshots incremental. |
| H-14, H-24 | `654d685` | Rejected non-finite geometry configuration and made cascade safe for arbitrarily small live layout areas. |
| H-15 through H-19 | `895fa90` | Derived compositor UI size and coordinates from live logical output geometry and bounded picker/compass layout. |
| H-20 | `3b41126` | Bounded close-animation snapshot count and memory from live output area. |
| H-21 | `a1ffa1a` | Updated Ocean floating/tree ownership during keyboard resize. |
| H-22 | `42837ce` | Evaluated Ocean edge physics in camera/world coordinates using the live viewport. |
| H-23 | `b98df42` | Pruned empty workspace layout state and rejected meaningless runtime overrides. |
| H-25 | `5f328a8` | Replaced client-reachable recursive BSP walks and destruction with explicit heap-backed traversal. |
| H-26 through H-28 | `2cf8484` | Made Wave reload transactional and bounded includes, generated entries, handlers, and deferred actions. |
| H-29 | `13ddb2d` | Secured the fallback config directory by effective user, ownership, mode, and symlink checks. |
| H-30 | `0294a04` | Protected explicit IPC socket paths and rediscovered stale automatic sockets safely. |
| H-31, H-32 | `8748877` | Published replacement screencast nodes and terminated streams after daemon loss or incompatible size changes. |
| H-33 | `5be4a67` | Paired accessibility releases with the recipients of their matching presses. |
| H-34 | `90c7e9e` | Replaced the fixed udev redraw poll with damage wakeups, output VBlank pacing, and mode-derived retry periods. |
| H-35, H-40 | `6d6ee92` | Routed callbacks and presentation feedback through actual per-output rendered placements. |
| H-36, H-37, H-44 | `48e8d86` | Migrated fullscreen, maximize, Classic depth, and zero-output ownership safely across hotplug. |
| H-38, H-39, H-41 | `69a9271` | Resolved Ocean interaction, drag, action, and screencast output from the live presenting camera. |
| H-42, H-43 | `9522858` | Preserved floating restore geometry during output movement and stopped output-manager resources without invalidating other clients. |
| H-45 | no code change | False positive against the current tree: `UdevEvent::Changed` is handled and connector state is rescanned. |

### Closed Medium findings

| Findings | Commit | Resolution |
| --- | --- | --- |
| M-01, M-02 | `5ed7751` | Made output capture dimensions transform-aware while keeping regions in the upright offscreen coordinate space; sized and offset toplevel capture from its popup-inclusive bounds. DMA-BUF validation now uses the same dimensions. |
| M-04, M-09, M-10 | `d9f5fcc` | Pruned finished ripples without depending on rendering, made exponential easing land exactly on its endpoint, and derived overflow-safe ripple rectangles from the numeric type limits. |
| M-14, M-16, M-17, M-18 | `d9f5fcc` | Used live zoomed camera centers in both axes for admission/migration, sampled in-flight camera motion, and prevented fullscreen FitPlacement hit-test fall-through. |
| M-15 | `48e8d86` | Migrated Classic Depth Deck ownership on output disconnect. |
| M-19, M-20, M-21 | `d9f5fcc` | Clamped the pointer to the union of half-open live output rectangles, bounded gaps from each live slot, and invalidated resize topology after algorithm/tree changes. |
| M-30 | `9522858` | Removed stopped output-manager head resources without advancing or corrupting shared transaction serials. |

### Validation state

- After `d9f5fcc`, `cargo test --locked --all-features` passed all 356 compositor tests and all 6 `wavefmt` tests outside the restricted IPC socket sandbox.
- `cargo check --locked --all-features` passed after the final Medium batch.
- Strict all-target/all-feature Clippy passed at the preceding `0.90.38` capture checkpoint (`5ed7751`). Run it again before merging the eventual complete branch because the final `d9f5fcc` batch was test/check validated but did not receive another Clippy invocation before the work paused.
- Capture and geometry regression tests use deliberately arbitrary dimensions. No monitor resolution, refresh rate, GPU vendor, input device, or other configurable/hardware property was introduced as a fixed runtime assumption.
- Nested and real-DRM validation for the complete audit-fix series is still pending. Automated tests cannot prove mixed-output KMS/VBlank behavior, connector hotplug, rotated physical outputs, VRR, real tablet/touch mapping, or visual feel.

### Instructions for the next model

1. Work only in `ai/codex/report-fixes` and the separate worktree above. Confirm it is clean before editing and do not push unless the maintainer explicitly asks.
2. Start from the open Medium list above. Re-audit each finding against current code before implementing it; High fixes changed many referenced line numbers and may overlap later findings.
3. Keep hardware and user-configurable values dynamic. Do not hardcode a monitor resolution, refresh rate, output count, scale, transform, GPU, device, connector name, or desktop extent.
4. Prefer small signed commits. For each shipped milestone, bump the patch version in `Cargo.toml` and the TideWM package entry in `Cargo.lock`, and add a concise `CHANGELOG.md` entry.
5. Run focused tests while editing, then `cargo fmt --all -- --check`, full locked tests, and strict all-target/all-feature Clippy. Use `env -u NO_COLOR` for nested TideWM sessions.
6. Ask the maintainer for a visual check only when behavior or appearance cannot be judged from automated output. Record nested versus real-hardware verification separately.
7. Update this ledger whenever findings close. Do not mark an item complete from compilation alone or because a nearby fix appears related.

## Executive summary

The codebase compiles cleanly and its test suite is healthy, but the audit found several release-blocking issues concentrated in four areas:

1. Session-lock isolation is inconsistent in the udev and per-window capture paths. Client pixels, a client cursor surface, closing-window snapshots, and toasts can be composed above the lock; window-only PipeWire capture can continue through a lock.
2. Several animation/effect paths manufacture their own future damage. Full-tier float physics, ambient float state, and reactive glass can keep a nominally idle compositor rendering forever.
3. Live Classic/Ocean migration is not safe for multi-output, hidden workspaces, groups, pins, floating geometry, or large workspace IDs. Some cases leave visible stale windows; others strand spatial ownership.
4. Several client-controlled inputs are insufficiently bounded: a blocking gamma-table FD, unlimited IPC subscribers, recursive Wave evaluation/JSON conversion, very large window titles/toasts, unbounded close snapshots, and pathological BSP depth.

The current roadmap should pause new visual identity work until the critical/high findings are closed. The roadmap's standalone hardware pass is still valuable, but it should explicitly cover session lock, per-window capture, output hotplug failure, rotated capture, PipeWire restart, Classic/Ocean migration, and idle-frame accounting.

## Validation performed

- `cargo clippy --locked --all-targets --all-features`: clean, no warnings.
- `cargo test --locked --all-features`: all 323 main tests and all 6 `wavefmt` tests passed when run outside the restricted socket sandbox.
- The first sandboxed test run failed only three Unix-socket tests with `EPERM`; the approved rerun passed them.
- The repository started clean and synchronized with `origin/master`.

Passing tests do not cover most findings below because they involve live Wayland clients, lock state, multi-output transforms, DRM failure paths, hotplug, large/adversarial input, or long-lived frame scheduling.

## Labels

- **Critical**: security boundary bypass or practical compositor-wide denial of service.
- **High**: data/state corruption, major correctness failure, unbounded resource retention, or permanent idle rendering.
- **Medium**: meaningful edge-case bug, recovery failure, avoidable recurring work, or scalability problem.
- **Low**: cleanup, defensive hardening, or smaller inefficiency.
- **Confirmed**: the control/data flow is clear from the code.
- **Likely**: strong static evidence, but a protocol/library invariant may reduce impact.
- **Investigate**: plausible edge case retained because the audit was asked to record uncertain findings too.

## Critical findings

### C-01 — Per-window capture bypasses session lock

**Confidence: confirmed.** `src/capture.rs:303-401` handles per-window capture and returns before lock state is computed at `:404`. `ext-toplevel` captures and active-window PipeWire streams can therefore continue rendering a mapped window's pixels while the session is locked. Full-output capture takes the later lock-aware path, so the two capture modes disagree.

Fix direction: compute lock state before selecting capture type and either refuse window capture or render only lock content while locked. Add a test that starts a window stream, locks, commits new client content, and verifies that no post-lock pixels are delivered.

### C-02 — The udev lock frame can include client-controlled overlays above the lock

**Confidence: confirmed.** In `src/backend/udev.rs:1331-1339` the toast is built while locked; `:1371-1417` builds the pre-lock client cursor surface; and `:1549-1581` inserts cursor, toast, and closing-window elements before/above the lock elements. `closing_window_frame_elements` has no lock guard (`src/tide_core/state.rs:2099-2119`), and locking does not clear close animations. Because the render list is front-to-back, stale client pixels and an arbitrary client cursor surface can appear above the session lock. The winit and capture paths use a separate locked composition and do not have this exact structure.

Fix direction: make the locked composition an early, isolated branch. It should contain only lock surfaces and a compositor-owned cursor whose image cannot originate from a pre-lock client. Clear or suppress close snapshots and non-security toasts at lock entry.

### C-03 — Gamma control can block the compositor event loop indefinitely

**Confidence: confirmed.** `src/handlers/wlr_gamma_control.rs:141-153` synchronously reads an arbitrary client-provided FD in the compositor event loop. A client can pass a pipe whose writer remains open without sending bytes. The read blocks the entire WM indefinitely.

Fix direction: make the FD non-blocking and integrate it with the event loop, or copy it on a bounded worker with a deadline. Enforce the exact expected byte count and close/reject on timeout.

### C-04 — The Wave runtime has no instruction/time budget

**Confidence: confirmed.** There is no `mlua` hook or instruction limit. Config chunks execute directly in `src/tide_core/wave.rs:1941-1964` and `:2002-2025`; event handlers run in `src/tide_core/state.rs:10221-10257`; IPC eval runs at `:10298-10303`. A top-level script, event handler, include, or `tidectl eval` containing an infinite loop freezes the single compositor thread. The handler nesting cap only limits recursive event nesting; it does not interrupt a loop inside one handler.

Fix direction: install an instruction hook/deadline for every untrusted execution entry point, with separate sensible budgets for load, event, and eval. Cap queued actions as well.

### C-05 — IPC subscriber count is effectively unbounded

**Confidence: confirmed.** The 64-connection guard in `src/tide_core/ipc.rs:418-420` applies while accepting/converting a connection. A subscribed connection is moved into the subscriber map and the connection lease is dropped. Repeating that sequentially bypasses `MAX_CONNECTIONS`; every subscriber retains socket FDs, a calloop source, queue state, and a map entry. The per-subscriber byte cap does not cap the number of subscribers.

Fix direction: count live request connections plus subscribers under one global limit, and add a smaller explicit subscriber limit. Test 65 long-lived subscriptions, not 65 simultaneous handshakes.

### C-06 — Portal sessions can replace or stop another client's session

**Confidence: confirmed.** `src/screencast/portal.rs:157-185` inserts a session into the map before D-Bus object registration. A duplicate object path replaces an existing victim; when registration fails, cleanup removes the victim entry. `SessionObject::close` at `:374-401` does not validate caller ownership, so another bus client that knows a path can stop the session. `Start` has a check-then-act race at `:271-311`, allowing two workers for one session.

Fix direction: bind sessions to the unique D-Bus sender, reserve paths atomically, register before publishing, and make start/close transitions a single locked state-machine operation.

### C-07 — Recursive Wave-to-JSON conversion can stack-overflow the compositor

**Confidence: confirmed.** `lua_value_to_json` in `src/tide_core/wave.rs:1880-1935` recursively descends Lua tables without a visited set, depth limit, node limit, or response-size cap. A self-referential table returned by `tidectl eval` recurses until stack overflow; a deeply nested or huge acyclic table can exhaust stack or heap.

Fix direction: detect cycles by Lua table identity, set depth/node/output-byte limits, and return a structured eval error.

## High-severity correctness, lifecycle, and resource findings

### H-01 — Ocean to Classic migration leaves inactive windows mapped

**Confidence: confirmed.** `migrate_ocean_to_classic` moves every reef tree into Classic layouts (`src/tide_core/state.rs:8508-8527`) and chooses active workspaces (`:8529-8533`) but never unmaps the existing Ocean `Space` elements. The following Classic retile (`:8330-8334`) maps active trees but does not remove inactive ones. Ocean previously mapped the entire world (`:7089-7137`). Hidden-workspace windows can remain visible and input-active at stale world coordinates.

### H-02 — Classic to Ocean overlaps reefs from different outputs

**Confidence: confirmed.** `src/tide_core/state.rs:8399-8423` places workspace N at `x=(N-1)*stride` independently for each output. There is no output-specific world offset, so workspace 1 from every output lands at `(0,0)` in one shared Ocean world. Windows can overlap and appear from multiple cameras.

### H-03 — Floating geometry does not round-trip across engine migration

**Confidence: confirmed.** Classic→Ocean adds `reef_x` to an already-global floating X coordinate (`state.rs:8431-8441`). Ocean→Classic clamps absolute world coordinates directly into an output-local viewport (`:8554-8562`) without subtracting the reef origin or adding the destination output origin. Workspace >1 floaters land at an edge, and floaters from non-origin outputs can land near global `(0,0)` while still tagged to that output.

### H-04 — Groups can lose spatial ownership in Ocean

**Confidence: confirmed.** Migration moves only active group leaves into Ocean reefs while parked members stay in `groups` (`state.rs:8308-8312`, `:8367-8457`). Group operations still use Classic `self.layout` (`:8838-9030`), so tab cycling no-ops. Closing the active member cannot replace the Ocean leaf with the parked member (`:8906-8936`; `src/handlers/xdg_shell.rs:1322-1335`). A mapped group member can become stranded/invisible with no spatial owner.

### H-05 — Migrated Ocean pins have conflicting sources of truth

**Confidence: confirmed.** Classic→Ocean creates `screen_pins` and clears `self.pinned` (`state.rs:8442-8453`), but Ocean `toggle_pin` consults `self.pinned` (`:8064-8083`), as does move-grab re-anchoring (`src/grabs/move_grab.rs:66-73`). The first toggle of a migrated pin can reinsert the old bookkeeping instead of unpinning it; drag/fullscreen behavior can disagree with rendering.

### H-06 — Migration arithmetic overflows on accepted workspace IDs

**Confidence: confirmed.** `state.rs:8409` and `:8437` cast arbitrary IPC-accepted `u32` workspace numbers to `i32`, subtract, and multiply by viewport stride. Large IDs panic in debug and wrap in release, producing overlapping or nonsensical reef coordinates.

### H-07 — Full-tier float physics never idles when wave forcing is off

**Confidence: confirmed.** `sync_float_physics_bodies` recreates an at-rest body for every Full floater (`state.rs:1734-1756`), the update always requests redraw (`:1819-1850`), and finished bodies are removed (`:1857-1864`). The next tick recreates them. One Full floating window is enough for permanent frames plus HashMap churn. O(n²) collision checks at `:1919-1943`, repeated across up to eight 120 Hz substeps, amplify the cost.

### H-08 — Ambient float state can permanently animate a tiled window

**Confidence: confirmed.** Rendering stops applying ambient offset after a surface is tiled (`state.rs:1315-1331`), but `toggle_floating` never removes `window_float_ambient` (`:7386-7538`). `has_active_animation` treats any entry as permanently active (`:5016-5020`), and the ambient toggle refuses non-floating windows (`:1719-1731`). The invisible stale entry keeps full-rate rendering until close, refloat-and-toggle, or config disable.

### H-09 — Reactive glass creates a self-sustaining capture/redraw loop

**Confidence: confirmed.** Both backends recapture before rendered frames (`src/backend/winit.rs:237-240`, `src/backend/udev.rs:1314-1321`). `state.rs:5687-5774` increments the backdrop commit on every successful recapture even when scene content is unchanged. `src/visual/water_glass.rs:76-89` treats every commit as a new disturbance and resets `last_kick`; `state.rs:6014-6049` observes it and `:5048-5068` keeps pumping while reactive glass is active. Every animation frame therefore creates the next disturbance. Multiple glass windows also repeat whole-scene offscreen capture per window.

### H-10 — Output hotplug failure leaves a published ghost output

**Confidence: confirmed.** `src/backend/udev.rs:1026-1032` publishes, maps, and refreshes a new output before plane discovery and DRM compositor construction. Failures at `:1034-1039` or `:1062-1077` return without unmapping or removing the global. The compositor and screencast snapshots retain a phantom output.

### H-11 — A transient DRM render/queue failure can freeze a CRTC

**Confidence: confirmed.** `render_surface` clears the dirty flag at `src/backend/udev.rs:1309`; render/queue errors at `:1634-1643` do not restore it. Unless unrelated damage occurs, that output is never retried.

### H-12 — Window-open/close IPC events can be missing or duplicated

**Confidence: confirmed.** XDG role destruction always detaches (`src/handlers/xdg_shell.rs:52-64`), while detach always emits `WindowClosed` (`:1305-1377`) even when there was no mapped window/handle. A never-mapped role emits close without open; unmap emits close and later destruction can emit it again.

### H-13 — Screencast snapshot rebuild is O(all windows) on every surface commit

**Confidence: confirmed.** With the feature compiled, `src/handlers/xdg_shell.rs:667-668` unconditionally calls snapshot rebuild; `src/screencast/mod.rs:186-204` allocates and looks up all windows under a mutex. This happens for subsurface commits and even when screencast state is `None`.

### H-14 — Config numeric lowering accepts non-finite output scale

**Confidence: confirmed for Wave config; defensive only for the Wayland request.** Wave numeric lowering through `set_f64`/`set_opt_f64` (`src/tide_core/config.rs:6846-6865`) accepts NaN/infinity, including output scale around `:6425-6444`. These reach `Scale::Fractional` and output/Space geometry, where comparisons, transforms, division, and integer conversion no longer have sane semantics. Output management also checks only `scale <= 0` (`src/handlers/wlr_output_management.rs:527-537`), but its wire value is fixed-point and probably cannot encode NaN/infinity; still reject all non-finite values at the shared validation boundary.

### H-15 — Compass cue placement can panic on a small output

**Confidence: confirmed.** `src/visual/compass.rs:173-176` and `:224-225` call `clamp(half, screen-half)`. Rust panics when the lower bound exceeds the upper bound. Config permits cue size up to 1024 (`src/tide_core/config.rs:5153-5157`), so a legal cue on a smaller logical output triggers the panic.

### H-16 — Source picker coordinates are wrong on non-origin outputs

**Confidence: confirmed.** The picker is laid out output-locally (`src/visual/source_picker.rs:75-80`, `:98-110`) while hit testing uses incoming coordinates directly (`:114-121`, `:160-175`). Core passes global pointer coordinates (`src/tide_core/input.rs:1010-1013`). Hover/click is displaced by the output origin.

### H-17 — Source picker overflow lets Cancel select an invisible source

**Confidence: confirmed.** Panel height is capped but choices are neither truncated nor scrolled (`source_picker.rs:75-80`, `:432-507`). `row_at` checks only `choices.len()` (`:167-175`), and `click_at` tests a row before Cancel (`:148-157`). On a short output with many sources, the visible Cancel area can map to a clipped row. Keyboard navigation can also select invisible entries.

### H-18 — Minimap travel targets are wrong on non-origin outputs

**Confidence: confirmed.** State opens/updates the minimap with a global pointer (`state.rs:6614-6619`, `:6658-6687`). Cursor drawing subtracts output origin (`src/visual/minimap.rs:478-505`), but `world_point_at_last_location` divides the raw global coordinate by scale (`:508-515`). Click-to-travel is offset on secondary outputs.

### H-19 — CPU overlays mix physical mode size with logical coordinates

**Confidence: confirmed.** Minimap, overview, depth deck, and source picker allocate/render using physical mode dimensions while their placement/hit logic is logical: `state.rs:6611-6661`, `:9818-9877`, `:10098-10113`; `src/visual/minimap.rs:203-326`; `overview.rs:80-133`; `depth_deck.rs:33-135`; `source_picker.rs:69-110`, `:231-305`. At scale 2 they are roughly twice the intended logical size or cropped. The config error overlay already divides by output scale at `state.rs:5233-5239`.

### H-20 — Closing animations have no global count or memory budget

**Confidence: confirmed.** `src/visual/window_animation.rs:257-317` retains cloned GPU texture handles; `state.rs:2063-2095` stores one per distinct closing surface; cleanup is time-based, while configured duration can be 100,000 ms (`window_animation.rs:92-105`). Rapid unique map/unmap can retain a large number of client buffers for 100 seconds and exhaust GPU/system memory. Live-window snapshots are also recaptured every rendered frame (`state.rs:5493-5503`) with a new Vec and handle clones.

### H-21 — Ocean keyboard resize does not update Ocean ownership

**Confidence: confirmed.** `keyboard_resize` uses absence from Classic `self.layout` as the floating test (`state.rs:9727-9755`), so every Ocean window takes the Classic floating path. Ocean floaters receive a client configure but their authoritative `OceanSpace::floating` rect remains the old size; Ocean tiles receive a one-off size that the next retile replaces.

### H-22 — Ocean edge physics mixes global-output and world coordinates

**Confidence: confirmed.** Ocean floating rects come from world space (`state.rs:1693-1705`), but physics output bounds are Smithay global logical output geometry (`:1782-1801`). Edge collision compares them directly (`:1946-1961`) and ignores camera zoom. After pan/zoom, bounce edges no longer correspond to the viewport.

### H-23 — Runtime layout override maps grow forever on empty arbitrary workspaces

**Confidence: confirmed.** `Layouts::set_algorithm` and `adjust_master_ratio` insert unconditionally (`src/tide_core/layout.rs:487-490`, `:546-550`); `refresh_cascade_state` inserts an empty state (`:637-651`). Pruning happens only in `Layouts::remove` (`:452-470`). IPC can visit arbitrary empty workspace IDs and set layout/ratio, growing these maps indefinitely without ever creating a removable window.

### H-24 — Cascade can panic when usable pixels are fewer than rows/cells

**Confidence: confirmed.** `cascade_rects_from_state` calls `.clamp(1, remaining_h/w)` (`layout.rs:1743-1767`). Once remaining size is zero, Rust sees inverted bounds and panics. A layer-shell surface can leave a 1×1 usable area while enough toplevels create multiple rows/cells.

### H-25 — A skewed BSP is a client-reachable stack-overflow risk

**Confidence: likely.** Fallback insertion can create a linearly deep tree. `insert_into`, `remove_from`, `node_contains`, `find_window`, and collection are recursive throughout `layout.rs:1031-1457`. A client creating thousands of toplevels can make recursion depth proportional to window count and may abort the compositor.

### H-26 — Wave reload is not transactional

**Confidence: confirmed.** The two-pass resolver mutates the live Lua state. Environment installation clears handlers/actions/tracking, and `src/tide_core/wave.rs:2137-2150` clears old tracked globals before the authoritative second pass. If compilation/evaluation then fails, the Rust `Config` remains old but Lua handlers/globals are cleared or partially replaced. The stated “old config keeps running” behavior is therefore false for session Lua state.

There is a related cleanup bug in `resolve_uncycled`: an include sink is pushed at `wave.rs:2007`, but `compile_with?`/execution can return before the pop at `:2027`. A caught include failure can leave stale sinks/body tables retained by Lua closures until a later reload.

### H-27 — Wave actions and globals leak across eval/reload boundaries

**Confidence: confirmed.** `spawn()`/`action()` append to global `_actions` without checking collection mode. Top-level scripts execute twice during two-pass load, so actions can be queued twice and run on the next event. `tidectl eval action("quit")` mutates the queue but eval does not drain it, creating a delayed side effect; repeated calls grow memory. Arbitrary globals created by `script` are not tracked by `_vars`/`_blocks`, so removed scripts can leave large tables alive across reloads.

### H-28 — Wave includes/config generation have no aggregate bounds

**Confidence: likely.** Include cycles are detected, but there is no depth, file-count, total-byte, generated-entry, or action-queue cap. A deep acyclic chain risks stack overflow, and a finite but very large Lua loop can allocate an enormous config before an instruction hook could help unless output counts are also capped.

### H-29 — `/tmp` fallback config is cross-user and executable

**Confidence: confirmed edge case.** When both HOME and XDG config variables are unavailable, `config_path` (`src/tide_core/config.rs:6887-6898`) falls back to `/tmp/tidewm-config/config.wave`, without a UID-specific directory or explicit private permissions. On a multi-user system, one user can pre-create config that another session loads; Wave can spawn commands and register event actions.

### H-30 — `tidectl subscribe` deletes a refused socket and reconnects to the deleted path

**Confidence: confirmed.** `src/bin/tidectl.rs:274-285` handles `ConnectionRefused` by removing the socket, then immediately reconnecting to that same now-deleted path. It does not rediscover another live TideWM socket like the one-shot path. With explicit `--socket`, it also deletes the user-selected refused path.

### H-31 — Screencast reconnect publishes a new node that consumers never learn

**Confidence: confirmed.** The PipeWire worker reconnects and creates a fresh node (`src/screencast/pipewire_thread.rs:154-190`), but the node ID is returned/emitted only once (`src/screencast/dbus.rs:617-623`, `portal.rs:328-337`). Existing consumers remain attached to the dead node while the fresh worker stays “alive,” blocking recovery/restart. Reconnect count is not reset after successful recovery.

### H-32 — Portal streams cannot recover from output size changes/hotplug

**Confidence: confirmed.** A size mismatch closes `FrameTarget` (`pipewire_thread.rs:389-399`) and kills the worker, but the portal keeps `entry.stream = Some(dead handle)`. `Start` checks only `is_some()` (`portal.rs:271-273`), so that session cannot restart despite comments promising recovery.

### H-33 — Accessibility key grabs can lose the release event

**Confidence: confirmed.** `src/accessibility/mod.rs:182-209` chooses recipients from current modifiers before consulting global `suppressed_keys`; suppression release occurs at `:245-259`. For a grabbed Ctrl+A, releasing Ctrl before A means A-release no longer matches. The client receives press but never release while the compositor still suppresses it. Recipients must be recorded per pressed key/grab.

### H-34 — Native udev rendering is capped near 62.5 frames/s

**Confidence: confirmed.** The standalone backend's global poll is hard-coded to `Duration::from_millis(16)` at `src/backend/udev.rs:847`. `request_redraw()` only sets a core boolean; the poll consumes it and marks each CRTC dirty at `:769-778`. VBlank renders immediately only when that per-CRTC dirty bit was already set (`:582-632`). For an animation or a client that commits after its previous frame callback, the next dirty transition therefore waits up to 16 ms. A 120/144/240 Hz output cannot receive a new animated/client-driven frame on every VBlank; practical cadence is capped around 62.5 Hz.

The empty-frame retry itself correctly derives the output period (`udev.rs:1194-1201`), and presentation feedback uses the advertised mode, but neither removes the global dirty-poll bottleneck. Winit does re-arm from the host refresh (`src/backend/winit.rs:538-548`, clamped up to 360 Hz), so nested high-refresh scheduling is structurally better than the real DRM path.

Fix direction: make redraw propagation event-driven or run a per-output scheduler keyed to the next VBlank/refresh period. Keep each CRTC independently paced; do not replace 16 ms with one global fastest-output timer unless idle cost and slow-output backpressure are handled.

### H-35 — Mixed-refresh frame callbacks can be consumed by the wrong output

**Confidence: confirmed.** Udev `render_surface` iterates every `state.space.elements()` window and calls `window.send_frame(output, ..., |_| Some(output.clone()))` without checking `outputs_for_element` (`src/backend/udev.rs:1652-1659`). Smithay uses that closure to decide the surface's primary scanout output, so it always accepts whichever CRTC happens to render first. The surfaces map is a HashMap (`udev.rs:204`), making order unstable. A slow CRTC can throttle a window on a fast output, or a fast CRTC can drive a client visible only on a slow/output-off path. Winit has the same loop (`winit.rs:508-515`) but only one output. Presentation feedback correctly filters membership (`state.rs:4933-4947`), demonstrating the missing frame-callback filter.

### H-36 — Ocean unplug strands fullscreen and maximized windows on the dead connector

**Confidence: confirmed.** Disconnect retags only Ocean entry/pin hints (`backend/udev.rs:1161-1165`, `ocean.rs:659-677`); Classic migration does not touch Ocean ownership (`state.rs:8155-8288`). `FullscreenEntry.output` and `MaximizedEntry.output` remain the removed name. Ocean rendering drops fullscreen placement on every other output (`state.rs:2284-2296`); maximized remains protocol-maximized but loses its override because `:2298-2307` only applies it on the stale output.

### H-37 — Classic unplug can create two fullscreen owners on one output

**Confidence: confirmed.** `migrate_output_windows` blindly moves every source fullscreen entry to the fallback (`state.rs:8197-8204`, `:8262-8269`) without preempting an existing destination fullscreen. The following retile debug-asserts uniqueness (`:7361-7366`); debug builds panic, while release keeps contradictory state. `swap_workspaces` already contains the missing collision-preemption pattern (`:9340-9383`).

### H-38 — Ocean tiled drag/drop uses historical admission output, not the pointer output

**Confidence: confirmed.** Mouse and gesture start choose `entry_output` and its zoom (`input.rs:434-448`, `:1446-1463`). `OceanTileMoveGrab` converts deltas and hit-tests through that stored camera (`grabs/ocean_tile_move_grab.rs:69-119`, `:147-159`). A shared-world tile viewed through output B but admitted on A moves/drops in A's coordinate system. Floating smart attach does the same (`state.rs:4280-4329`) before output affinity is resynchronized (`move_grab.rs:238-246`).

### H-39 — Ocean admission affinity is incorrectly treated as interaction/presentation output

**Confidence: confirmed.** `ocean.rs:205-208` defines entry output as admission/focus affinity, but `preferred_output_for_toplevel` prioritizes it (`handlers/xdg_shell.rs:1490-1509`), so fullscreen/maximize on a window interacted with through another camera targets its original monitor (`:323-346`). `primary_output` also prefers it (`state.rs:3699-3758`). In a shared world, the monitor visibly hosting the interaction can differ from the action target.

### H-40 — Ocean presentation feedback uses world-Space overlap, not rendered camera placement

**Confidence: confirmed.** `state.rs:4926-4947` gates feedback with `Space::outputs_for_element`. Ocean maps windows at world rectangles (`:7123-7136`) but displays camera-transformed placements (`ocean.rs:1375-1438`). Feedback can be attached to the wrong monitor's VBlank or dropped although the window was visibly rendered elsewhere.

### H-41 — Ocean window screencast resolves the wrong output

**Confidence: confirmed.** `handlers/capture.rs:169-180` checks Classic ownership/tag state then takes `outputs_for_element(...).first()`. It never consults Ocean camera placements. A panned window visible on output B can have a world rect intersecting A or no physical output, causing wrong-output capture or failure. `.first()` is also nondeterministic for a straddling Classic window.

### H-42 — Live output movement corrupts fullscreen/maximized floating restore geometry

**Confidence: confirmed.** `translate_floating_windows_on_output` treats every visible floater alike (`state.rs:4468-4491`), reads its live Space rect—which is currently the fullscreen/maximized rect—translates it, and overwrites `FloatingTag.rect` (`:4482-4487`). It neither skips these states nor translates their separate restore entries. Later unfullscreen/unmaximize can restore stale or full-output geometry.

### H-43 — Binding a second output-manager invalidates existing client transactions

**Confidence: confirmed.** Manager bind increments shared `current_serial` and sends `done` only to the new manager (`handlers/wlr_output_management.rs:268-271`). Existing configurations then fail the stale-serial check (`:566-569`) despite no head change, and their manager never received the replacement serial.

### H-44 — Disconnecting the only output and reconnecting on another connector strands Classic windows

**Confidence: confirmed.** With no fallback, unplug skips migration (`backend/udev.rs:1156-1166`), leaving trees/tags keyed to the old connector. Mapping a later differently named connector only performs ordinary setup/retile (`:1027`, `:1127-1130`), which never adopts the old keys. Moving a cable from DP-1 to DP-2 can hide the entire existing session.

### H-45 — Connector `Changed` events are ignored

**Confidence: likely/confirmed dispatch gap.** `backend/udev.rs:1109-1187` handles add/remove/connect/disconnect and falls through at `:1186`; `DrmScanEvent::Changed` is ignored. Late EDID/mode-list changes, or a connector whose first setup failed temporarily, are not retried/upgraded until a physical unplug/replug.

## Medium-severity findings

### M-01 — Rotated region captures crop the wrong area

**Confidence: confirmed.** `src/handlers/screencopy.rs:63-69` transforms the requested logical crop using the real output transform, while `src/capture.rs:537-554` renders a Normal-oriented offscreen frame and applies that transformed crop. Rotated/flipped outputs capture the wrong region. Whole-output orientation/size is also explicitly unverified in the nearby comment.

### M-02 — Window capture clips popups despite including their elements

**Confidence: likely.** Capture size is the base window geometry (`capture.rs:311-319`), while the rendered element list can include popups (`:363-400`). Popup pixels outside the base rectangle are clipped.

### M-03 — DMA-BUF capture performs a synchronous GPU fence wait

**Confidence: confirmed.** `capture.rs:647-661` waits synchronously before returning DMA-BUF ownership. A slow or wedged GPU stalls input and protocol handling on the compositor thread.

### M-04 — Finished ripples can remain retained when rendering is skipped

**Confidence: confirmed.** `state.rs:4990-5008` only asks whether any ripple is active. The actual `retain` occurs in the render path at `:6963-6975`, after an early return when water effects are disabled. DPMS/skipped outputs or disabling effects mid-ripple can leave completed entries. Repeated enable/fire/disable can grow the Vec because the spawn cap counts unfinished entries only (`:6814-6816`).

### M-05 — Huge window titles cause needless full-string rasterization

**Confidence: confirmed.** `src/visual/tab_strip.rs:35-47` clones an untrusted client title. `draw_label` at `:89-118` walks and rasterizes every character even after the pen passes the visible segment; pixel clipping does not stop glyph work. `state.rs:2896-2905` also clones titles each depth frame before cache comparison.

### M-06 — Toast text width/allocation is unbounded

**Confidence: confirmed.** `src/visual/toast.rs:193-213` rasterizes every character and allocates from computed width×height. Failed-spawn messages include the full configured/IPC command (`src/tide_core/input.rs:2544-2551`). A huge string can cause extreme allocation, arithmetic overflow, or OOM.

### M-07 — Built-in wallpaper keeps a large permanent CPU backing

**Confidence: confirmed cost, not a leak.** `src/visual/wallpaper.rs:20-49`, `:101-132`, and `:192-199` retain roughly 33.2 MiB of CPU pixels for process lifetime, plus the likely GPU copy, even when an external layer-shell wallpaper fully covers it. Consider a smaller/lazy fallback or dropping CPU pixels after successful import.

### M-08 — Caustics recreates tracking state and reallocates after persistent errors

**Confidence: confirmed.** `src/visual/caustics.rs:208-241` creates a new damage tracker and `Id` on each dirty frame. The reusable texture is taken before render (`:176-200`); a bind/render failure drops it and causes another allocation/log on the next frame. Preserve resources on failure, reuse identity/tracking, and back off repeated errors.

### M-09 — `RippleEase::ExpOut` snaps at the endpoint

**Confidence: confirmed.** `src/visual/ripple.rs:257-260` evaluates to about 0.9933 at `t=1`, never 1. Workspace transition consumes it (`src/visual/workspace_transition.rs:370-385`) and disappears at duration, creating a small final jump. `visual/animation.rs:79-84` already shows endpoint special-casing.

### M-10 — Configured ripple radius can overflow `2*r`

**Confidence: confirmed.** Config accepts an arbitrary positive peak radius (`config.rs:5922-5924`); `visual/ripple.rs:433-441` and glass intersection at `state.rs:6024-6034` double it in `i32`. Values over `i32::MAX/2` panic in debug and wrap in release.

### M-11 — Backdrop capture scales poorly with glass-window count

**Confidence: confirmed.** `state.rs:5727-5756` rebuilds desktop render elements, maps, vectors, and tracker state once per eligible window per frame; `src/visual/backdrop.rs:53-83` creates another translated Vec/tracker. This repeats almost the whole scene for every glass window and becomes roughly O(glass windows × scene size), with additional cross-window effects that can approach quadratic behavior.

### M-12 — Tiled move commits even when the grab is cancelled

**Confidence: confirmed for Ocean/Classic tile grabs; likely for smart attach.** `src/grabs/ocean_tile_move_grab.rs:58-103`, `:278-285` always commits from `unset()`, including competing grabs, window death, backend cancellation, or teardown. It can swap/float using a stale last pointer. Classic `tile_move_grab.rs:73-93`, `:273-277` has the same unconditional swap. `move_grab.rs:238-245` similarly invokes smart attach from every unset path.

### M-13 — Closing the minimap loses the next click until pointer motion

**Confidence: confirmed.** The bug is documented at `src/visual/minimap.rs:37-45`. Close only removes the overlay/redraws (`state.rs:6670-6676`), without synthesizing pointer re-enter. A click before the next motion is discarded.

### M-14 — New Ocean-window reef selection ignores camera zoom

**Confidence: confirmed.** `src/tide_core/ocean.rs:689-702` calculates visible center as `origin + viewport/2`; viewport pixels should be divided by camera zoom. New windows can select the wrong nearest reef at zoom ≠ 1.

### M-15 — Output disconnect migration omits Classic Depth Deck entries

**Confidence: confirmed.** `state.rs:8155-8289` migrates trees, groups, and floating tags, but not the depth deck. Parked windows remain tagged to a dead output and become unreachable; later engine migration can restore them into dead-output keys and then drop unconsumed trees.

### M-16 — Ocean→Classic nearest assignment ignores Y

**Confidence: confirmed.** Output and workspace selection at `state.rs:8484-8495` and `:8543-8551` compare only center X in a deliberately two-dimensional world. Vertically separated reefs/cameras can be assigned arbitrarily.

### M-17 — Engine migration during camera motion uses the endpoint

**Confidence: confirmed.** `OceanSpace::drain_for_classic` copies stored camera origins (`ocean.rs:851-855`), but stored origins are animation targets while `camera()` samples the visible motion (`:398-423`). Mid-animation migration assigns from where the camera is going, not what the user sees.

### M-18 — Ocean fullscreen hit testing can click through scaled content

**Confidence: confirmed.** Normal Ocean hit testing has a root-surface fallback for stale configure/buffer geometry (`state.rs:3484-3495`); fullscreen hit testing at `:3545-3586` lacks it. Visually filled `FitPlacement` areas outside the old committed surface tree can pass input to content below.

### M-19 — Pointer clamping includes the exclusive right/bottom edge

**Confidence: confirmed.** `state.rs:7034-7040` clamps to `x+width`/`y+height`, but rectangle hit tests are half-open. Relative motion can land exactly outside every output and lose focus/input. This also does not solve gaps in L-shaped monitor layouts.

### M-20 — Gaps accept arbitrary signed `i32` values

**Confidence: confirmed.** Raw gaps are retained in config (`config.rs:1277`, `:6446-6448`); `layout::inset` performs `loc + gap` and `size - gap*2` (`layout.rs:1464-1472`). Large values overflow, gaps larger than half the slot move the forced 1px result outside it, and negative gaps expand/overlap windows.

### M-21 — Algorithm changes do not invalidate active resize topology

**Confidence: confirmed.** `Layouts::set_algorithm` does not bump `topology_revision` (`layout.rs:487-490`); split/cascade “current” checks do not verify the current algorithm (`:823-833`, `:1006-1013`). A live resize can remain valid after switching algorithms and mutate hidden BSP/cascade state. `insert_migrated_tree` also skips revision bump (`:682-684`).

### M-22 — XDG flutter records can survive client destruction forever

**Confidence: confirmed.** `note_toplevel_flutter` retains any record with `flips >= FLUTTER_FLOPS` regardless of liveness (`src/handlers/xdg_shell.rs:900-921`); permanent detach at `:1305-1378` does not clear `lifecycle_flutter`/`flutter_floated`.

### M-23 — Portal teardown can block while holding the session map

**Confidence: likely.** Portal close/disconnect removes and drops streams at `portal.rs:374-380`, `:425-435` while holding the map. `StreamHandle::drop` sends stop and synchronously joins (`pipewire_thread.rs:48-53`). A stuck PipeWire call can block all portal operations sharing the mutex.

### M-24 — Timed-out PipeWire startup can leak a worker thread

**Confidence: confirmed.** `pipewire_thread.rs:101-122` returns after five seconds without retaining or joining the worker. If the library call never returns, the thread and cloned compositor sender live forever.

### M-25 — Gamma controls retain disconnected outputs

**Confidence: confirmed.** The controls map owns `Output`/resources (`wlr_gamma_control.rs:41-45`), but output removal only cleans output-power state (`backend/udev.rs:1167-1178`). Long-lived gamma clients retain stale outputs and are not sent `failed`.

### M-26 — Serial-less XDG activation permits focus stealing

**Confidence: confirmed behavior; policy impact should be decided.** Serial-less tokens are accepted (`handlers/mod.rs:431-465`) and considered fresh (`state.rs:9049-9079`). Any ordinary client can mint one and focus itself. The justification that TideWM lacks urgency is stale because urgency handling now exists.

### M-27 — A swallowed parent is lost when the last output is gone

**Confidence: confirmed.** `handlers/xdg_shell.rs:1387-1405` removes the swallowed entry before selecting a fallback output. Closing the child during a zero-output interval returns early and permanently drops the hidden parent from management.

### M-28 — Cursor rendering reallocates/reuploads every frame

**Confidence: confirmed.** Named cursors clone and channel-swap the full image and create a `MemoryRenderBuffer` (`src/cursor.rs:162-193`); the fallback rasterizes/allocates at `:36-83`. Cache converted buffers per theme/icon/frame/scale.

### M-29 — Fractional cursor scale is truncated

**Confidence: confirmed.** `backend/udev.rs:1412` and `capture.rs:526` cast fractional scale to `u32`. At 1.25/1.5, scale-1 cursor frames are selected.

### M-30 — Output-management Stop leaves per-head resources tracked

**Confidence: confirmed.** Stop/destroy removes only the manager (`wlr_output_management.rs:296-312`). Refresh continues iterating its `HeadResources` (`:128-154`), growing stale vectors and potentially sending dynamic head events after Stop until each resource dies.

### M-31 — Xwayland startup has a TOCTOU race and unbounded waits

**Confidence: confirmed.** Probe waits without timeout (`src/xwayland.rs:76-107`); display selection is a file-existence race (`:111-120`); and `DISPLAY` is exported immediately after spawn without waiting for the socket (`:42-69`). Startup X clients can race the satellite, and immediate death leaves poisoned environment/PID state.

### M-32 — Arbitrary Wave environment entries can panic startup

**Confidence: confirmed.** `src/main.rs:85-94` passes config-provided keys/values directly to `std::env::set_var`, which panics on invalid names or embedded NUL. This should be a config diagnostic.

### M-33 — Startup session-environment helpers can hang the session

**Confidence: likely.** Synchronous `.status()` calls at `main.rs:183-235` have no timeout and run before the event loop. A wedged helper/D-Bus path delays the graphical session indefinitely.

### M-34 — Empty accessibility clients retain scarce slots

**Confidence: confirmed.** `ungrab`, `unwatch`, and empty `SetKeyGrabs` leave empty entries (`src/accessibility/dbus.rs:161-229`). Thirty-two still-connected callers can exhaust `MAX_CLIENTS` while owning no actual grab/watch.

### M-35 — Foreign-toplevel cleanup scans every handle at the frame timer rate

**Confidence: confirmed.** Both backends invoke it every tick (`backend/winit.rs:534`, `backend/udev.rs:844`); it scans and locks every handle (`handlers/wlr_foreign_toplevel.rs:246-263`) even though synchronous untracking normally removes entries.

### M-36 — Window rules are re-resolved/written on every root commit

**Confidence: confirmed.** `handlers/xdg_shell.rs:670-700` resolves all rules and rewrites opacity/glass maps before deciding whether title/app-id changed. High-frame-rate clients cause repeated rule scans and map writes.

### M-37 — DRM activation failure leaves libinput resumed

**Confidence: confirmed.** `backend/udev.rs:666-675` resumes input before activating DRM. If DRM activation fails, the function returns with input live and no functioning display.

### M-38 — Primary GPU removal leaves a permanently black live session

**Confidence: confirmed.** `backend/udev.rs:745-755` only logs removal of the driven GPU. With the single-GPU design, the compositor remains alive but can no longer render; terminating the session is more recoverable.

### M-39 — Lock confirmation can remain pending after the final output disappears

**Confidence: likely.** Confirmation is reevaluated from rendered frames (`state.rs:4810-4835`). Hot-unplug removes the output but does not invoke `try_confirm_lock`; with zero outputs there is no later frame to do it.

### M-40 — Output positions allow overflow-prone extremes

**Confidence: likely.** Output-management stores arbitrary fixed-point positions (`wlr_output_management.rs:507-513`) and later subtracts them at `:620-624`. `i32::MIN/MAX`-scale values can overflow translation/layout arithmetic. Bound the desktop extent and use checked/saturating operations.

### M-41 — IPC flush timer wakes at 62.5 Hz even with no subscribers

**Confidence: confirmed.** `src/tide_core/ipc.rs:359`, `:450-470` registers a recurring 16 ms timer unconditionally. It returns quickly when empty, but it prevents a truly quiescent event loop and conflicts with the idle CPU target. Arm it only while a subscriber has pending bytes, or integrate writable readiness.

### M-42 — Hidden-workspace windows are missing from IPC snapshots/events

**Confidence: confirmed current limitation.** Snapshot and window queries search `space.elements()` (`ipc.rs:278-288`, around `:961+`). Classic hides inactive-workspace windows from Space, so event payloads can be null and `windows` omits protocol-mapped clients. Bars and state restorers see an incomplete model.

### M-43 — The IPC socket can remain stale after signal termination

**Confidence: confirmed.** Socket unlink is primarily in Drop. Default SIGTERM/SIGKILL paths do not guarantee it, and there is no general termination signal cleanup. Stale rediscovery partly masks the issue but also creates the destructive CLI behavior in H-30.

### M-44 — `tidectl` socket I/O has no timeout or response limit

**Confidence: confirmed.** CLI connect/write/`read_to_end` is blocking and unbounded. A wedged or malicious same-user socket can hang the command and stream unlimited bytes into memory. The diagnostics external commands similarly have no timeout.

### M-45 — Config watcher misses external includes and deletion events

**Confidence: confirmed.** The watcher tracks only the main config parent tree (`config.rs:3699-3768`) while Wave supports absolute, `~`, parent, and symlinked includes. External targets do not hot-reload. The filter handles modify/create but not removal, so deleting an included `.wave` can leave the old config silently active until another event.

### M-46 — `swim_advance_target` can loop billions of times

**Confidence: likely.** `input.rs:239-260` loops `0..advances.unsigned_abs()`. A huge/non-finite gesture delta that saturates during conversion can produce an enormous integer and freeze the event loop. Use bounded arithmetic to compute the target directly.

### M-47 — `sync_tide` can expose stale/inconsistent workspace state

**Confidence: confirmed.** `state.rs:10264-10285` does not clear `tide.workspace` when there are no outputs. Under Ocean it still queries the Classic active workspace instead of using null/0 or an Ocean concept, disagreeing with IPC output JSON.

### M-48 — GPU vendor detection can select the wrong adapter

**Confidence: confirmed.** `wave.rs:1545-1575` accepts only five-character `cardN` names, excluding `card10+`, and takes the first unspecified `read_dir` result. Hybrid systems can expose the wrong vendor to hardware-conditional config.

### M-49 — Diagnostics report the oldest matching journal entries as newest

**Confidence: confirmed.** `tidectl_diagnostics` uses default `journalctl` ordering but labels/consumes the first records as newest. It should pass reverse ordering or change the wording.

### M-50 — Diagnostics memory budget is stale

**Confidence: confirmed.** The doctor warns at a hard-coded 1.5 GB, while `AGENT.md` now defines feature-scaled reference points and a 2 GB absolute ceiling. This can falsely flag an allowed Ocean setup and cannot judge the selected feature set.

### M-51 — One output's animation repaints every output

**Confidence: confirmed.** `has_active_animation()` is global across per-output transitions, caustics, windows, and overlays (`state.rs:4985-5039`). Both backends convert any active result into one global redraw flag (`winit.rs:520-528`, `udev.rs:825-838`) and then dirty every output (`winit.rs:214-217`, `udev.rs:773-777`). In a mixed-refresh multi-monitor setup, an animation on one monitor creates avoidable work/page flips on all static monitors.

### M-52 — Lock/DPMS does not suspend invisible animation work

**Confidence: confirmed.** The global animation predicate includes ambient/full physics, animated borders, and glass without considering lock or output power (`state.rs:5008-5038`, animated-border scan `:5395-5420`). Physics advances before lock-aware composition and requests redraw while bodies exist (`udev.rs:769-772`, `state.rs:1819-1850`). A lock screen or one remaining active CRTC can therefore keep repainting because of effects that are hidden or belong to a DPMS-off output.

### M-53 — Isolated caustics substantially under-run configured FPS

**Confidence: confirmed.** `caustics_active()` becomes true only after the interval is already due (`state.rs:6475-6485`). The backend then merely sets global `needs_redraw`, consumed on the following timer tick (`udev.rs:825-847`, `winit.rs:520-548`). On the 16 ms udev loop, a requested 60 fps can trend near one frame per ~48 ms, and 30 fps near ~64 ms, because it waits to become due and then waits another poll. This contradicts comments that the gate yields the configured rate.

### M-54 — Screencasting one output repaints every output at about 30 FPS

**Confidence: confirmed.** Each PipeWire cycle queues a capture, which calls global `request_redraw()` (`capture.rs:143-153`). Udev marks every CRTC dirty and renders visible frames before draining the targeted capture (`udev.rs:773-811`). On multi-4K setups, one output stream needlessly repaints the rest and compounds the full-frame readback/CPU copy at `capture.rs:721-775`.

### M-55 — Winit high-refresh timing drifts by render duration

**Confidence: confirmed.** Winit returns `TimeoutAction::ToDuration(period)` only after event polling, render, submit, captures, and cleanup (`winit.rs:158-548`). The next deadline is work completion plus one period, not a phase-anchored deadline. At 144 Hz, 2 ms work turns the target into roughly 112 Hz. The timer also runs global maintenance at the host refresh even while visually idle, up to the 360 Hz clamp.

### M-56 — Winit submits full-window damage on every dirty frame

**Confidence: confirmed.** `winit.rs:227-228` creates a full-size damage rectangle and `:453` submits it regardless of the damage tracker's result. `render_result.damage` is used only for presentation feedback. Small changes therefore ask the host to repaint the entire nested window at high refresh.

### M-57 — Nested refresh can remain stale after moving between host monitors

**Confidence: confirmed against the pinned Smithay event surface.** Refresh is sampled at startup and only re-read on `WinitEvent::Resized` (`winit.rs:87-90`, `:166-179`). The pinned Smithay winit backend exposes no moved event. Moving the nested window between 60 and 144 Hz monitors without a resize/scale change can preserve the old advertised refresh/timer indefinitely.

### M-58 — First frame after a cross-output move can misattribute presentation feedback

**Confidence: likely.** `take_presentation_feedback` filters via `space.outputs_for_element` (`state.rs:4933-4947`), but both backends call `space.refresh()` only at the end of their tick (`winit.rs:530`, `udev.rs:840`). A move/placement commit can request and render a frame before the membership cache refreshes.

### M-59 — Screencast timing is not exactly its advertised 30 FPS

**Confidence: confirmed, low impact.** PipeWire advertises exact 30/1 (`pipewire_thread.rs:434-460`) but triggers with integer `Duration::from_millis(1000/30)` (`:491-532`), i.e. 33 ms or 30.303 fps before overhead. Resetting the next deadline from `Instant::now()` also accumulates processing delay. Screencast is intentionally fixed at 30 fps and does not negotiate source refresh.

### M-60 — Fractional DRM refresh metadata is approximate

**Confidence: confirmed, low impact.** Mode selection uses integer `DrmMode::vrefresh()` (`udev.rs:883-897`) and exports `vrefresh()*1000` (`:983-985`). Rates such as 59.94/119.88 lose precision in advertised/presentation metadata and empty-frame estimates, although real KMS VBlank pacing remains hardware-controlled.

### M-61 — Absolute pointer/touch devices are hard-bound to the first output

**Confidence: confirmed behavior.** `touch_location` and `PointerMotionAbsolute` use `self.space.outputs().next()` (`input.rs:544-545`, `:1209-1215`). On multi-monitor systems, a touchscreen/tablet mapped to another connector controls the first output. There is no per-device output binding, “first” depends on Space iteration order, and output transform is not applied to device axes.

### M-62 — Live scale/transform leaves floaters and layer surfaces at stale fractional scale

**Confidence: confirmed.** Output management changes state and arranges/retiles (`wlr_output_management.rs:597-629`). Classic retile updates preferred scale for tiled windows (`state.rs:7164-7213`) but not ordinary floaters or floating fullscreen/maximized windows. Layer scale is explicitly set only once (`handlers/layer_shell.rs:166-175`, `state.rs:4642-4655`). Clients can keep rendering for the old scale until an unrelated drag/remap.

### M-63 — Transform/scale changes can push floating content and pins off-screen

**Confidence: confirmed.** The apply path translates floaters only when position changes (`wlr_output_management.rs:612-625`). Rotation or increased scale shrinks/swaps logical bounds while floating tags and restore rectangles remain unchanged; retile reconciles only fullscreen/maximized (`state.rs:7216-7282`). Ocean pins likewise render raw `viewport_loc` (`ocean.rs:1408-1427`).

### M-64 — `swap-workspaces` corrupts window-group ownership

**Confidence: confirmed.** `Layouts::swap_active` moves trees (`layout.rs:413-444`), but `Smallvil::swap_workspaces` never updates `WindowGroup.output/workspace` (`state.rs:9283-9527`). Those fields control reinsertion (`:1018-1024`, `:8951-8957`) and tab-strip/accessibility filtering (`:5140-5163`, `:9227-9236`). The strip can vanish and ungroup can reinsert parked tabs on the old monitor/workspace.

### M-65 — Cross-output workspace swap leaves floating-window scale stale

**Confidence: confirmed.** Swap retags/repositions/map-elements floaters (`state.rs:9418-9524`) but never calls `set_window_fractional_scale`; final retile updates tiled surfaces only. Crossing 1×→2× leaves the floater advertising 1× until another movement/remap.

### M-66 — Ocean preferred fractional scale is tied to admission, not visibility

**Confidence: confirmed.** `retile_ocean` selects `entry_output` for every world window (`state.rs:7123-7133`). A window admitted on scale-1 output A but visible through scale-2 output B keeps A's preferred scale. Simultaneous shared-world visibility needs a defined primary-placement policy.

### M-67 — Output changes/unplug do not rehome pointer or refresh pointer focus

**Confidence: confirmed.** Udev removes/migrates/unmaps then repairs only keyboard focus (`backend/udev.rs:1141-1183`). Output-management apply likewise omits the explicit scene-change pointer helper (`wlr_output_management.rs:597-632`; helper `state.rs:4126-4152`). `pointer.button` can use retained old Smithay focus (`input.rs:1819-1828`) before the next motion, sending the first click to the wrong or unmapped client. Pointer coordinates may also remain outside all surviving outputs.

### M-68 — Ocean disconnect can retain focus on an invisible off-camera window

**Confidence: confirmed.** `ocean.rs:659-677` deletes the camera and retags entries without reconciling which world area the fallback camera shows. All Ocean windows remain Space-mapped, and `window_is_visible` checks only Space presence (`state.rs:4191-4195`). Keyboard-focus repair can therefore retain content invisible on the fallback (`:4509-4559`).

### M-69 — Hidden-workspace float/pin/maximize rules silently fail

**Confidence: confirmed.** XDG map inserts a rule-targeted hidden workspace (`handlers/xdg_shell.rs:1063-1099`), but retile maps only the active tree. It then calls conversion helpers (`:1135-1167`) whose Classic path requires the window in `space.elements()` (`state.rs:7416-7427`) and assumes a tile on the active workspace (`:7451-7457`). `workspace=2` combined with float/pin/maximize can remain tiled and hidden.

### M-70 — Lock surface geometry is stale after live scale/transform

**Confidence: confirmed.** Lock surface size is configured at registration (`state.rs:4791-4799`). Output-management transform/scale rearranges/retiles (`wlr_output_management.rs:605-628`) without reconfiguring it. The compositor black fill remains safe, but the lock UI can be cropped or misaligned.

### M-71 — Output control/layer state is incompletely cleaned on unplug

**Confidence: confirmed/likely.** Power cleanup removes controls without sending `failed` (`wlr_output_power_management.rs:97-107`) while gamma has no unplug hook and retains an `Output`-keyed map (`wlr_gamma_control.rs:41-54`); udev invokes only power cleanup (`udev.rs:1175-1178`). Ordinary layer surfaces are not explicitly closed/unmapped (`udev.rs:1167-1170`), and later destruction searches only live outputs (`handlers/layer_shell.rs:187-200`), leaving stale old-Output layer-map state possible.

### M-72 — Live output scale/position ranges are too broad

**Confidence: confirmed.** Output management permits any positive fixed scale (`wlr_output_management.rs:527-537`), including values around 32768× that reduce a normal mode to roughly 1 logical pixel and reach arrange/retile (`:603-628`) plus H-24. Extreme i32 positions (`:507-514`) reach unchecked `loc+size` arithmetic (`state.rs:7034-7035`, `backend/udev.rs:995-999`).

### M-73 — Ocean ripples and lifecycle animation choose output from world-Space overlap

**Confidence: confirmed.** Ripple output selection uses `output_for_window` (`state.rs:6831-6838`), then applies that output's camera (`:6903-6913`) and stores the ripple per output (`:6980-6987`). Lifecycle animation offset similarly uses wrong output geometry (`:1997-2013`). A window viewed through B can animate on A or nowhere.

### M-74 — `locked_outputs` retains disconnected Output objects until unlock

**Confidence: confirmed, low impact.** Lock frames insert into the set (`state.rs:4810-4815`); disconnect removes surfaces/blank buffers but not the set (`backend/udev.rs:1167-1170`). It is normally bounded to the short Locking phase, but retains Output/userdata/layer-map state across hotplug until unlock clears it (`state.rs:4758-4764`).

## Performance and simplification opportunities

These are not all bugs, but they are concrete places where the code can be shorter or asymptotically better.

- **P-01 — Ocean world layout is recomputed repeatedly.** `state.rs:7104-7137` computes tiled layouts twice, and `ocean.rs:1375-1438` rebuilds the whole world again per output per frame, including off-camera reefs. Cache topology/world rectangles and cull before layout/render.
- **P-02 — Classic ownership queries become O(n²).** Whole-Space passes call recursive `layout.contains` for every window (`state.rs:7220-7317`). A `surface -> (output, workspace, role)` index would shorten several paths and make ownership O(1).
- **P-03 — Removing one window rebuilds every tree.** `layout.rs:452-470`, `:1063-1089` first scan for ownership, then recursively rebuild all trees. Find the owner once and touch one tree.
- **P-04 — `insert_into` repeatedly rescans subtrees.** `layout.rs:1031-1057` uses recursive `node_contains` while descending, becoming quadratic on skewed trees.
- **P-05 — Avoidable allocation helpers.** `window_count` clones every Window (`layout.rs:621-625`); `populated_workspaces` allocates full window Vecs (`:405-410`); depth rotate builds two index Vecs (`classic_depth.rs:123-151`); `entries_for` allocates for iteration (`:39-43`); `record_app_opened` performs cumulative duplicate scans (`ocean.rs:745-752`).
- **P-06 — Depth exemption is O(n²) every 100 ms.** `state.rs:2638-2669` builds a Vec and linearly searches it for each window. Use a set or indexed ownership.
- **P-07 — Disabling Classic depth performs two full retiles.** `state.rs:10379-10381` calls restore, which retiles even with an empty deck (`:10117-10128`), then reload retiles again at `:10539`.
- **P-08 — Wave merge is quadratic on large generated configs.** `waves::merge_into` retains/scans the full target for every scalar/bind/handler. Index merge keys or build a new final representation in one pass. Keyed block paths and reload diffing also clone entire bodies.
- **P-09 — Source picker rebuilds its CPU buffer on each hover-row change.** `source_picker.rs:125-142`; retain static pixels and repaint only changed rows if profiling shows it matters.
- **P-10 — `lua_value_to_json` silently drops mixed table keys.** When `raw_len > 0`, numeric sequence handling can ignore string keys. Define mixed-table semantics or return an error.
- **P-11 — Idle GPU floor is ~40-45% on real hardware and is not caused by `water_effects`.** Measured live on real AMD/amdgpu hardware (Renoir iGPU) at `d9f5fcc`, genuinely idle (pointer/window/workspace untouched, confirmed by flat PSS and flat VRAM across a 15s sampled window in both states): `water_effects = true` idled at 43-45% `gpu_busy_percent`, `water_effects = false` idled at 40-43% — a 2-3 point difference, not the ~20+ point gap the always-on caustics/water-glass/ripple loop would predict if it were the cause. A same-machine, same-config-tier comparison against a Hyprland session idled flat at 20-23% GPU busy for reference. RAM was not the problem either: TideWM PSS (~106-117 MB) stayed below Hyprland's (~147 MB) throughout. This points at something in the base render loop that costs GPU regardless of the water toggle — most likely full-scene redraw on frames with no real damage, since nothing in the merged H-34 fix (`90c7e9e`, damage/VBlank-driven udev redraw pacing) verified that an undamaged frame is actually skipped rather than just correctly *scheduled*. Not yet root-caused: no line number pinned, no confirmed control-flow path read. Fix direction: instrument or trace `render_output`/the udev per-CRTC render path (`backend/udev.rs`) at genuine idle and confirm whether it renders/submits a full frame when no output, client, or effect has pending damage; if so, add or fix an early skip.
- **P-12 — Live-reported: startup and ambient caustics rendering appear throttled/stuck until a window is opened; fullscreen-in-Ocean and workspace-switch re-trigger it.** Confidence: reported live on real hardware at this branch's head, not yet reproduced under tooling or read against code. On real hardware with `water_effects = true`: at compositor startup the WM visibly lags and autostart-spawned apps (wallpaper daemon, quickshell) appear noticeably later than the user's prior experience running TideWM. The ambient caustics effect (configured `fps = 24` in the live `water.wave`) visibly renders at roughly 3 fps instead, well below even the first `idle_fps` step-down tier (20 fps, meant to only apply after 60s of idle). Opening any application window fixes both symptoms back to normal speed. The degraded state can be re-triggered afterward by fullscreening a window in Ocean mode, or by switching workspace, and again resolves only after opening a new app or switching back.

  Suspected relation to P-11: the trigger/reset pattern (broken at startup and by fullscreen/workspace-switch, fixed by any window-map/commit event) is consistent with the same redraw-scheduling area as P-11 — plausibly the H-34 damage/VBlank-driven redraw rewrite (`90c7e9e`) not correctly driving frames for periodic/non-client-damage sources (the caustics timer, first-frame autostart rendering), only reaching normal cadence once a real client commit generates damage through what may now be the primary/only reliably-driven path. This is a hypothesis, not a finding: no code has been read for this entry, and it needs reproduction with an actual frame-timing trace (e.g. instrumenting the udev per-CRTC render/queue path and the caustics timer source, and logging when each requests vs. actually gets a frame) before attributing it to a specific function or commit.

## Lower-confidence and defensive findings to investigate

- **U-01 — Never-signalled DMA-BUF blockers may accumulate.** `handlers/compositor.rs:35-81` adds a source and transaction blocker for each not-ready commit. Confirm Smithay cleanup/coalescing under repeated hostile never-signalled fences.
- **U-02 — Ocean placement prefix assumes perfect map/stack consistency.** `ocean.rs:1385-1390` slices at `self.floating.len()` although the prefix is produced by `floating_stack.filter_map`. Add a debug invariant or derive the count from collected entries.
- **U-03 — Dredge can select a screen-pinned window's dormant world rect.** `ocean.rs:1231-1264`, `:1358-1372` include pins in `world_layouts`; a pinned window below the viewport may be moved/unpinned as “submerged.”
- **U-04 — Ocean directional focus may choose off-camera windows.** `state.rs:9632-9652` searches Space, where all Ocean world windows are mapped. Decide whether focus should travel the camera or only consider visible placements.
- **U-05 — Completed camera motions remain retained.** `ocean.rs:381-388`, `:425-427` retain completed records until another camera operation/output removal. Bounded by output count, but stale.
- **U-06 — Ocean pan grab may mutate an orphan camera after unplug.** `grabs/ocean_pan_grab.rs:45-50` pans by output name without rechecking that the output still exists.
- **U-07 — Resize constraints with min > max are handled inconsistently.** `grabs/resize_grab.rs:130-154` does `.max(min).min(max)`, producing a value below min if constraints are inverted. Smithay/protocol validation may prevent it; verify upstream.
- **U-08 — Resize-grab teardown may unwrap after resource death.** `resize_grab.rs:325-334` assumes the toplevel remains available. Likely safe for normal XDG lifecycle, worth defensive review.
- **U-09 — Cursor theme parsing assumes a nonempty frame list and safe delay sum.** `cursor.rs:203-223`; a malformed local theme may panic or overflow.
- **U-10 — One capture client can monopolize the global 64-slot queue.** `capture.rs:44-51`, `:143-153`; consider per-client/output fairness.
- **U-11 — Xwayland PID identity remains stale after satellite death.** No death/restart notification updates the recorded PID, so rules may misclassify later clients and X11 support stays silently dead.
- **U-12 — PipeWire early-return paths may retain stale chunk metadata.** `pipewire_thread.rs:319-325`, `:380-383` do not explicitly zero chunk size. Confirm PipeWire buffer reuse guarantees.
- **U-13 — Acyclic include depth and generated entries can still exhaust resources.** Retained separately from H-28 because practical thresholds need measurement.
- **U-14 — Config duration/radius/geometry parsers need a systematic finite/range audit.** `pseudo_tile_scale`, touchpad acceleration, mode refresh, ripple radius, gaps, and similar values are validated inconsistently; some NaNs survive `clamp` because comparisons with NaN are false.
- **U-15 — Output placement should be bounded as a desktop, not only as independent i32 coordinates.** Several transforms add/subtract positions and sizes later; checked desktop extents would centralize overflow defense.
- **U-16 — Overlapping/mirrored outputs have nondeterministic point ownership.** Hit paths take `space.output_under(pos).next()` (`state.rs:3424`, `:3531`, `:3643-3654`) while config/output management permit overlap. If mirroring is intended, define stable active-output priority; otherwise reject overlap.

## In-code documentation review

The main comment problem is not grammar; it is that inline comments often contain changelog history, old failed implementations, live-test stories, roadmap phase names, and comparisons with other compositors. Those details are useful, but TideWM already has `CHANGELOG.md`, `AGENT.md`, and technical docs for them. Keeping them inline hides the actual invariant and makes stale claims look authoritative.

Recommended rule:

- Keep protocol requirements, security boundaries, coordinate spaces, ownership invariants, render order, lock ordering, and non-obvious API traps.
- Keep the reason for an unusual operation when removing it would reintroduce a bug.
- Move chronology, verification stories, old implementation details, roadmap phase labels, and “we tried X” narratives to `CHANGELOG.md`/`AGENT.md`.
- Prefer one direct sentence. Use a short paragraph only when the invariant truly has multiple parts.
- Do not describe a known bug as intentional behavior. Use a short `TODO` until it is fixed.

### Comments that are factually stale or misleading

- `src/tide_core/wave.rs:1-15` says the old line parser still produces entries and describes Wave phases as future work. The rewrite is complete.
- `wave.rs:630`, `:1136`, `:1940` retain dead-code/future-phase explanations that no longer describe the implementation.
- `src/screencast/dbus.rs:1-18` says TideWM relies on the GNOME portal and has not been tested with real OBS; TideWM now has its own portal and project docs say OBS/Discord were verified.
- `src/backend/udev.rs:1-22` says output-disconnect windows remain orphaned; migration now exists, although this audit found holes in it.
- `src/visual/ripple.rs:1-20` presents workspace transition as future reuse, but the separate transition module is built.
- `src/visual/animation.rs:1-10` calls effects future work although the effect stack exists.
- `src/tide_core/state.rs:4979-4984` says active animation is “just a fading toast”; many systems are now checked.
- `src/tide_core/state.rs:7378-7385` says absence from Classic `Layouts` means floating. Ocean tiles, parked group members, depth-deck windows, and unmapped roles disprove it.
- `src/tide_core/state.rs:4642-4655` says layer scale is set once because an output never changes; live output-management scale invalidates that claim.
- `src/tide_core/layout.rs:294-300`, `:686-690` say `layout()` writes cascade state even though `&self` cannot; `refresh_cascade_state` writes it.
- `layout.rs:864-869` says cascade row/column resize is unbuilt although the implementation is directly above.
- `src/handlers/mod.rs:414-423` justifies activation behavior by claiming TideWM has no urgency indicator; it now has urgency handling.
- The migration overview at `state.rs:8299-8315` overclaims group correctness.
- `TECHNICAL_REPORT.md` still says 3 GB is the hard RAM ceiling while `AGENT.md` explicitly revised it to 2 GB. The doctor also retains the obsolete 1.5 GB threshold.
- `backend/winit.rs:146` and `state.rs:1810-1818` call both backend timers “~60 Hz”; winit now targets 30–360 Hz.
- `backend/udev.rs:825-835` claims rate gating yields configured effect FPS; M-53 shows the extra-poll under-run.
- `AGENT.md`'s high-refresh/scheduling text treats udev pacing as complete after a 120 Hz modeset, but a successful mode does not prove distinct-frame cadence (H-34).
- `backend/udev.rs:11-15` still says disconnected windows are not migrated; migration exists, though this audit found incomplete cases.

### Suggested concise replacements

| Location | Suggested comment |
| --- | --- |
| `wave.rs:1-15` | “Compiles Wave syntax to sandboxed Lua and lowers it into config entries. This module also owns includes, typed literals, session globals, eval, and event-handler registration.” |
| `state.rs:4979-4984` | “Prunes completed animations and reports whether another frame is needed.” |
| `state.rs:3678-3706` | “Chooses an action target. With focus-follows-mouse, prefer pointer output unless a layer owns focus; otherwise prefer the focused window. Fall back to any mapped output.” |
| `state.rs:8022-8036` | “If pinning caused this fullscreen window to float, undo that mechanical change when pinning is toggled off.” |
| `state.rs:8299-8315` | “Translate live spatial ownership in place. BSP trees move between Classic workspaces and Ocean reefs; floating rects and pins are converted; engine-only camera/bookmark/deck state is dropped.” |
| `ocean.rs:64-72` | “For anchored zooms, derive origin from interpolated zoom so the point under the viewport anchor stays fixed.” |
| `ocean.rs:988-993` | “Measure the real BSP slot and grow iteratively; eight rounds bound the work.” Remove the unnatural `ponytail` marker. |
| `layout.rs:267-318` | For runtime maps: “Per-workspace overrides; prune when the workspace tree becomes empty.” For rules: “Config-owned fallback by workspace number.” For revision: “Invalidates split grabs after structural changes.” |
| `layout.rs:1489-1501` | “The first tree-order window is master; remaining windows share the stack. Orientation selects the split axis.” |
| `layout.rs:1811-1821` | “Choose the row count whose grid aspect is closest to the output aspect in log space.” |
| `visual/water_glass.rs:316-324` | “Leave opaque regions empty because glass samples content behind the surface.” |
| `grabs/move_grab.rs:75-85` | “While smart attach is enabled, show the tiled target under the pointer.” |
| `grabs/tile_move_grab.rs:73-93` | “Hit-test immutable layout slots, not the visually moved Space element. Cross-output drops snap back.” |
| `backend/winit.rs:59-72` | “Winit permits one process-global EventLoop, so nested mode exposes one simulated output.” |
| `backend/udev.rs:93-104` | “Custom shaders make this render-element enum GLES-specific.” |
| `handlers/tearing_control.rs:1-26` | “Protocol state is tracked but not yet honored by KMS because the pinned Smithay API exposes no async-flip flag.” |
| `main.rs:150-171` | “Publish the Wayland/display environment to the user activation environment.” |
| `visual/overview.rs:259-269` | “Advance at least one pixel so zero-width glyphs do not overlap.” |

### Highest-priority comment blocks to shorten

The following blocks are large enough to materially obstruct code review. They should be handled in a dedicated comment-only change after the behavioral bugs are fixed:

- `src/tide_core/config.rs:3581-3626` — 46 lines.
- `src/visual/minimap.rs:1-45` — 45 lines; reduce to purpose, coordinate model, and input contract. Lines `:37-45` currently rationalize the lost-click bug and should become a short TODO.
- `src/handlers/mod.rs:619-661` — 43 lines; section essays also occur at `:108-116`, `:161-169`, `:230-240`, `:275-289`, `:332-348`, `:405-423`, `:493-504`, `:542-579`, `:689-715`.
- `src/tide_core/ipc.rs:1-37` — keep protocol modes and framing; move phase/history text.
- `src/accessibility/mod.rs:1-34` — keep the threading and suppression contract; remove implementation history/compositor comparison.
- `src/visual/overview.rs:1-33` — keep purpose and current limitations in roughly five lines.
- `src/tide_core/state.rs:6565-6597` — minimap design history.
- `src/screencast/portal.rs:1-32`, `:118-145` — shorten architecture and locking contracts; remove the stray `ponytail` word.
- `src/handlers/wlr_output_management.rs:1-31` — keep supported/unsupported protocol scope; remove historical machine-freeze narration.
- `src/tide_core/input.rs:1396-1425` — reduce to the live input invariant.
- `state.rs:3678-3706` — use the target-selection replacement above.
- `src/visual/ripple.rs:41-67` — shader contract can be about five lines: normalized UV, premultiplied alpha, and no `#version`.
- `src/tide_core/waves.rs:57-83` — retain only merge precedence/invariants.
- `src/handlers/tearing_control.rs:1-26` and `src/grabs/tile_move_grab.rs:1-26` — use the replacements above.

Additional dense/historical targets:

- `state.rs:1255-1274` is misplaced above `base_window_visual_sample`; move a 2–3 line stack/skip invariant to `desktop_render_elements` or delete it.
- `state.rs:1633-1642`, `:4339-4356`, `:4447-4462`, `:6840-6862`, `:7043-7062`, `:7897-7909`, `:8681-8698`, `:8896-8905`, `:8976-9010`, `:9034-9044`, `:10321-10329`, `:10502-10516` narrate incidents or repeat nearby code. Preserve only the active invariant.
- `src/visual/float_physics.rs:1-23`, `:70-76`, `:132-145`, `:160-169`, `:202-219`, `:252-260`, `:276-288`, `:330-336` is the most over-commented visual file. Keep equations, units, coordinate space, and rest conditions; move design chronology. Remove “rigid-body-ish.”
- `src/visual/caustics.rs:1-21`, `:51-58` — remove prose such as “absence of drama”; keep cache/failure behavior.
- `src/visual/error_overlay.rs:325-335`, `:346-354`, `:387-393` — retain formula/invariant, delete bug-story prose.
- `src/visual/water_glass.rs:1-15`, `:107-119`, `:236-242`, `:316-324` — keep shader inputs, animation condition, and opacity rule.
- `src/visual/backdrop.rs:1-14`, `:31-39`, `:46-52`; `compass.rs:1-12`; `welcome.rs:1-17`; `workspace_transition.rs:1-14`; `tab_strip.rs:1-13`; `swim.rs:1-15`, `:30-37`, `:140-147` — compress module histories into current responsibility.
- `src/grabs/ocean_tile_move_grab.rs:1-13`, `:58-68`, `:278-283` — remove the “fridge magnet needs a light” metaphor and teardown narrative. Do not claim every teardown should commit until M-12 is fixed.
- `src/screencast/dbus.rs:1-24`, `:136-174`; `portal.rs` above; `pipewire_thread.rs:267-286`, `:329-344`, `:491-500` — keep D-Bus thread ownership, transport limitation, and DRIVER requirement; move experiment logs.
- `src/backend/udev.rs:167-216`, `:1590-1608` — one sentence per field/invariant; move freeze history.
- `src/backend/udev.rs:987-994` — “Place unconfigured hotplug outputs after the current maximum right edge; summing widths overlaps after disconnect/replug.”
- `src/backend/udev.rs:1143-1153` and `src/handlers/wlr_output_management.rs:606-619` — retain operation order in two or three lines; remove narrated examples.
- `src/capture.rs:406-423`, `:537-549`, `:565-575`, `:688-716` — keep privacy, transform, and z-order facts; remove chronological debugging stories.
- `src/cursor.rs:1-13`, `:85-93`, `:120-156`, `:165-202` — reduce repeated cache/fallback/borrow explanations to one sentence per function.
- `src/handlers/xdg_shell.rs:1029-1039`, `:1067-1079`, `:1101-1115`, `:1197-1212`, `:1225-1230`, `:1262-1267`, `:1302-1317` — retain lifecycle ordering only.
- `src/xwayland.rs:1-19` — satellite architecture plus eager-start reason is enough.

Small cleanup examples: remove the duplicated `.map(str::to_string).map(str::to_string)` in diagnostics, the duplicated PipeWire comment, and dead `TZ` read in `local_now`. These are not performance problems, but they make generated-looking code easier to spot.

## Wave formatter bugs

These deserve their own section because a formatter is documentation tooling and can silently alter comments/config.

- **F-01 — Block-comment stripping inflates line counts.** `src/tide_core/wave_fmt.rs:14-53` replaces every character in a block-comment span with a newline. One long comment line becomes hundreds of logical lines. Preserve only real newline characters and replace other bytes with spaces.
- **F-02 — Escaped quotes break comment scanning.** Quote toggles at `wave_fmt.rs:23-25`, `:61-85` treat every `"` as a closing/opening quote. A quoted string containing an escaped quote followed by `#` or `--` can be truncated or misparsed.
- **F-03 — `--[[` is classified as a line comment before block-comment mode.** Formatting at `:103-134` calls line-comment splitting first, so a block opener at line start never enables `in_block_comment`; later comment lines can be interpreted/reindented as Wave code. Existing tests assert substrings/idempotence and miss assignments/braces inside the block.
- **F-04 — `wavefmt -w` is non-atomic.** `src/bin/wavefmt.rs:63-85` uses direct `fs::write`; a failure after truncation can damage the config. Write a sibling temporary file, sync as appropriate, then rename.

## Roadmap implications

The roadmap itself is reasonable, but audit evidence changes the order:

1. Fix lock/capture isolation, blocking client inputs, portal ownership, and unbounded subscriber/eval paths before packaging or broader promotion.
2. Fix the udev >60 Hz scheduler and the permanent redraw loops before “feel tuning”; tuning a path that cannot present above ~62.5 fps on real hardware will give misleading results.
3. Treat Classic↔Ocean migration as incomplete until multi-output, hidden workspace, group, pin, float, depth-deck, and active-animation cases pass live tests.
4. Expand the standalone hardware pass to cover mixed 60/120+/VRR outputs, hotplug during lock/grab/capture, rotated outputs, fractional scale, PipeWire restart, and zero-output intervals.
5. Defer the AUR package until at least the critical items and destructive `tidectl subscribe` behavior are fixed.

## Focused multi-monitor and high-refresh verdict

### Multi-monitor

**Verdict: the base udev architecture is genuinely multi-output, but the current implementation is not reliable across all multi-monitor paths.** Each CRTC has its own `SurfaceData`, damage tracker, pending/dirty state, KMS queue, VBlank completion, mode, scale, transform, and output object. Basic independent scanout is therefore structurally present. However, the audit found release-significant failures in:

- Classic↔Ocean migration (H-01 through H-06), and Ocean's static admission-output assumptions (H-38 through H-41).
- Mixed-refresh frame callbacks and presentation routing (H-35, H-40).
- Secondary-output compositor UIs and hit testing (H-16 through H-19).
- Rotated capture and fractional cursor scale (M-01, M-29).
- Output failure/hotplug rollback, fullscreen ownership, zero-output recovery, and retained subsystem state (H-10, H-36, H-37, H-42 through H-45, M-15, M-25, M-38, M-39, M-67 through M-74).
- Live mixed-scale/transform and workspace-swap state (M-62 through M-66).
- Global redraw/capture scheduling that repaints unrelated outputs (M-51, M-54).
- Absolute devices hard-bound to the first output (M-61).
- Half-open-boundary/L-shaped layout behavior (M-19).

Nested winit cannot verify multi-monitor behavior: it intentionally exposes one output because winit permits only one process-global EventLoop (`backend/winit.rs:59-73`). Real multi-monitor confidence therefore requires the udev backend and actual connectors.

### 120 Hz and higher

**Verdict: no for the standalone udev backend as written.** It can modeset a 120/144/240 Hz mode and KMS VBlank pacing remains per CRTC, but the global 16 ms dirty-transfer poll caps the production of new animated/client frames near 62.5 fps (H-34). The screen may scan at 120 Hz while TideWM supplies new content roughly every other VBlank.

Winit targets the host's reported 30–360 Hz rate and most animations are wall-clock based, so animation duration does not speed up or slow down with frame count. Its relative timer rearm and full-window submission still make the advertised refresh an upper bound rather than proof of achieved cadence (M-55 through M-57). Full float physics uses a fixed 120 Hz accumulator with up to eight catch-up substeps, which is numerically preferable, but it is driven only from the backend poll and currently has the idle-loop bug in H-07.

Other refresh-related limits:

- PipeWire screencast is intentionally fixed near 30 fps, not source refresh (M-59).
- Adaptive-sync/VRR config is only queried/logged; `use_vrr` is not called (`backend/udev.rs:958-969`). This is an acknowledged feature gap, not a newly introduced regression.
- Udev presentation feedback completes on VBlank, but uses handler-time and sequence 0 because the pinned DRM event provides no timestamp/sequence. Winit completes at submit time and marks VSync even though the host owns real presentation.
- Most visual animations sample `Instant`, which is good: once scheduling is fixed, they should remain time-correct at 60/120/144/240 Hz.

### Live verification matrix still required

Static review can prove the scheduler cap and coordinate/control-flow bugs, but cannot prove driver timing or visual smoothness. After fixes, test at least:

| Setup | What to verify |
| --- | --- |
| Single 120/144/240 Hz output | Measured presented-frame cadence during client animation, pointer motion, workspace transition, ripple, glass, and Full physics; no 16 ms cap. |
| 60 + 144 Hz outputs | A window on each output receives callbacks at its own CRTC cadence; animating one does not page-flip the other. |
| 60 + 120 Hz, mixed scale 1.0/1.5 | Pointer/focus, popup placement, overlays, capture, cursor size, floating move/resize across the boundary. |
| Normal + rotated output | Full and region screenshots, PipeWire stream orientation, overlay hit testing. |
| L-shaped layout with a gap | Relative pointer clamping never lands outside every output; focus does not disappear at exclusive edges. |
| Hotplug during lock/capture/grab | No ghost global, stale lock confirmation, stranded grab/window, leaked gamma/depth/portal state, or client pixels above lock. |
| DPMS one output during animation | Powered-off CRTC does no work; other outputs do not repaint for hidden effects. |
| Move nested window 60↔144 Hz | Advertised/timer refresh updates even without a resize, or the limitation is made explicit. |

Measure actual presentation cadence from DRM/page-flip traces or presentation feedback, not only the configured mode. A mode reading `120000 mHz` proves scanout rate, not that TideWM supplies 120 distinct frames per second.
