use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use smithay::{
    desktop::Window,
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle},
};

use crate::{config::WorkspaceRef, layout::Layouts};

use super::{
    clamp_rect_visible, is_scratchpad_workspace, swapped_group_owner, FloatingTag, Smallvil,
    NAMED_SCRATCHPAD_BASE, SCRATCHPAD_WORKSPACE,
};

/// Workspace-owned window placement: layout trees, floating tags, pins,
/// scratchpads and the Classic Depth Deck (plan step 8c). Lives in
/// `SpatialState::workspaces`.
pub struct WorkspaceState {
    pub layout: Layouts,
    /// (output, workspace) tag for every *floating* window. Tiled windows
    /// don't need one -- their workspace is implicit in which `Layouts` tree
    /// holds them -- but a floating window isn't tracked in `Layouts` at
    /// all, so `switch_workspace` needs this to know which floating windows
    /// belong to the workspace it's hiding or showing.
    pub floating_workspace: HashMap<WlSurface, FloatingTag>,
    /// Windows exempt from `switch_workspace`'s hide/show cycle -- stay
    /// mapped and visible no matter which workspace is active on their
    /// output. Always floating (`toggle_pin` un-tiles first if needed; a
    /// tiled pinned window has no coherent meaning when only one
    /// workspace's tree is ever rendered per output at a time).
    pub pinned: HashSet<WlSurface>,
    /// Tiled windows that keep `config.pseudo_tile_scale` of their tile's
    /// size instead of filling it -- a rect override `retile()` applies,
    /// same shape as the fullscreen override. Stays in its `Layouts` slot
    /// throughout, unlike floating: only the rendered rect changes.
    pub pseudo_tiled: HashSet<WlSurface>,
    /// Per-output "workspace to return to" for `toggle_scratchpad`, so
    /// toggling the scratchpad off doesn't strand you on some fixed
    /// fallback workspace regardless of where you actually were.
    pub(super) scratchpad_previous: HashMap<String, u32>,
    /// Named scratchpads (Hyprland's named "special workspaces"):
    /// each name is lazily assigned its own reserved workspace number from
    /// `NAMED_SCRATCHPAD_BASE` upward on first use, then behaves exactly
    /// like the unnamed scratchpad does -- same `switch_workspace` hide/show
    /// machinery, no parallel data structure. Session-scoped: numbers are
    /// allocated in first-use order, which only matters to IPC consumers,
    /// and those should key on the `scratchpad` name field instead.
    pub(super) scratchpad_named: HashMap<String, u32>,
    /// Per-output previously-active workspace, updated on every real
    /// `switch_workspace` call -- drives `config.workspace_auto_back_and_forth`
    /// (niri's own feature of the same name). Separate from
    /// `scratchpad_previous` above: that one only tracks the scratchpad's
    /// own toggle, this tracks every ordinary workspace switch.
    pub(super) workspace_previous: HashMap<String, u32>,
    /// Output that received the last `workspace-up`/`workspace-down` step.
    /// Used only while no visible window holds focus (an empty workspace):
    /// `primary_output` then falls back to the pointer, which may be on the
    /// other monitor and would step the wrong screen on the way back.
    pub(super) workspace_step_output: Option<String>,
    /// (output, workspace) pairs that have already fired their
    /// `workspace_rule { on_created_empty }` command (see
    /// `apply_workspace_switch`). TideWM's numbered workspaces always
    /// exist as addressable slots -- there is no real create/destroy
    /// lifecycle to hook a one-shot event on the way Hyprland can -- so
    /// this set is what makes the closest honest analog to that a
    /// once-per-process-lifetime event instead of firing on every later
    /// visit to an empty workspace. Pruned per-output in
    /// `remove_workspace_transition_output` on disconnect, same
    /// "connector name might mean something else on replug" reasoning as
    /// that function's other entries.
    pub(super) workspace_created_empty_fired: HashSet<(String, u32)>,
    /// Output-pin bindings (`rule { pin_output }`, `toggle-output-pin`),
    /// keyed by surface, valued by connector name. No entry means "not
    /// pinned" -- the common case costs nothing. Kept independent of
    /// whether the named output is currently connected: a disconnect
    /// leaves the entry alone (the window falls back like any other, but
    /// still remembers where it belongs), and reconnecting that output
    /// recalls the window via `recall_pinned_windows_to`. Cleared
    /// per-surface in `detach_mapped_toplevel`, same lifecycle as
    /// `window_tags`.
    pub output_pins: HashMap<WlSurface, String>,
    /// Structural parked-window state for Classic workspaces. This remains
    /// empty unless the opt-in `classic_depth` feature is used.
    pub classic_depth: crate::classic_depth::ClassicDepthDeck,
}

impl Smallvil {
    /// Prospective placement must evaluate live workspace matches at its
    /// destination before previewing or mutating the window's ownership.
    pub(super) fn resolve_window_rules_on_workspace(
        &self,
        surface: &WlSurface,
        on_workspace: Option<u32>,
    ) -> crate::config::WindowRule {
        // How long after compositor launch a newly-mapped window still
        // counts as `at_startup` (niri) -- long enough to cover a
        // `spawn`-launched app's typical cold-start time, short enough to
        // not catch something the user opens by hand during a slow login.
        // Picked without access to niri's own real default (the reference
        // clone is outside this sandbox's allowed paths); revisit if a
        // real config needs a different window.
        const STARTUP_GRACE_PERIOD: Duration = Duration::from_secs(5);

        let (app_id, title) = self.toplevel_identity(surface);
        let pid = self.client_pid(surface);
        let is_xwayland = self.is_xwayland_surface(surface);
        let urgent = self.urgent.contains(surface);
        // Falls back to the current identity for a surface this compositor
        // process never saw map (nothing recorded it), which just makes
        // initial_class/initial_title behave like their live counterparts
        // in that edge case rather than never matching at all.
        let (initial_app_id, initial_title) = self
            .initial_toplevel_identity
            .get(surface)
            .cloned()
            .unwrap_or_else(|| (app_id.clone(), title.clone()));
        let at_startup = self.start_time.elapsed() < STARTUP_GRACE_PERIOD;
        let tags = self.window_tags.get(surface);
        self.config
            .resolve_window_rules(crate::config::WindowMatchFacts {
                app_id: app_id.as_deref(),
                title: title.as_deref(),
                pid,
                is_xwayland,
                urgent,
                initial_app_id: initial_app_id.as_deref(),
                initial_title: initial_title.as_deref(),
                at_startup,
                tags,
                on_workspace,
            })
    }

    /// The workspace a mapped window currently belongs to, tiled or
    /// floating -- `None` under Ocean (no workspace concept) or for a
    /// window not currently tracked by either. Feeds `workspace_rule { }`
    /// decoration-base resolution below.
    pub(super) fn workspace_of_surface(&self, surface: &WlSurface) -> Option<u32> {
        self.spatial
            .workspaces
            .layout
            .workspace_of(surface)
            .or_else(|| {
                self.spatial
                    .workspaces
                    .floating_workspace
                    .get(surface)
                    .map(|tag| tag.workspace)
            })
    }

    /// Resolves a `"workspace:N"`/`"move-to-workspace:N"` target to a real
    /// workspace number: a `Number` passes through, a `Name` looks itself
    /// up in `config.workspace_names`. Warns and returns `None` for an
    /// unknown name, the same "bad input, log and no-op" convention an
    /// invalid numeric workspace used before names existed.
    pub(crate) fn resolve_workspace_ref(&self, r: &WorkspaceRef) -> Option<u32> {
        match r {
            WorkspaceRef::Number(n) => Some(*n),
            WorkspaceRef::Name(name) => {
                let resolved = self.config.workspace_names.get(name).copied();
                if resolved.is_none() {
                    tracing::warn!(name, "Unknown workspace name, ignoring");
                }
                resolved
            }
        }
    }

    /// Workspace numbers shared by IPC and ext-workspace: populated and active
    /// entries plus the optional configured `1..=N` range.
    pub(crate) fn advertised_workspaces(&self, output: &str) -> Vec<u32> {
        let mut numbers: Vec<u32> = self
            .spatial
            .workspaces
            .layout
            .populated_workspaces()
            .into_iter()
            .filter(|(o, _)| o == output)
            .map(|(_, w)| w)
            .chain(std::iter::once(
                self.spatial.workspaces.layout.active_workspace(output),
            ))
            .chain(
                self.spatial
                    .workspaces
                    .floating_workspace
                    .values()
                    .filter(|tag| tag.output == output)
                    .map(|tag| tag.workspace),
            )
            .collect();
        if let Some(count) = self.config.workspace_count {
            numbers.extend(1..=count);
        }
        numbers.sort_unstable();
        numbers.dedup();
        numbers
    }

    /// Switches one output's visible workspace without destroying hidden
    /// content. Pinned windows remain visible; exclusive layers block the action.
    /// `workspace-up`/`workspace-down`: step one numbered workspace on the
    /// focused output. Scroll can animate the step as a vertical camera
    /// slide (`scroll.workspace_scroll`), keeping columns horizontal; every
    /// other engine and configuration uses the ordinary switch. Scratchpad
    /// workspaces are reserved and never participate.
    pub(crate) fn workspace_step(&mut self, step: i32) {
        if step == 0 || self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean {
            return;
        }
        // With a visible focused window, follow it. On an empty workspace
        // there is no focus, and `primary_output` falls back to the pointer
        // or the first output -- which may be the other monitor. Reuse the
        // output of the previous step in exactly that case so Down then Up
        // returns the same screen.
        let output = if self.focused_window_surface().is_some() {
            self.primary_output()
        } else {
            self.spatial
                .workspaces
                .workspace_step_output
                .as_deref()
                .and_then(|name| self.output_by_name(name))
                .or_else(|| self.primary_output())
        };
        let Some(output) = output else {
            return;
        };
        let output_name = output.name();
        self.spatial.workspaces.workspace_step_output = Some(output_name.clone());
        let current = self
            .spatial
            .workspaces
            .layout
            .active_workspace(&output_name);
        if is_scratchpad_workspace(current) {
            return;
        }
        let target = (i64::from(current) + i64::from(step)).clamp(1, i64::from(u32::MAX)) as u32;
        if target == current {
            return;
        }

        let animate = self.spatial.engine.projection()
            == Some(crate::config::WorkspaceProjection::Scroll)
            && self.config.scroll.workspace_scroll
            && self.config.scroll.workspaces == crate::scroll::WorkspaceMode::Separate;
        self.render
            .motion
            .workspace_scroll_cameras
            .remove(&output_name);
        if !animate {
            self.switch_workspace(&output, target);
            return;
        }

        self.switch_workspace_immediate(&output, target);
        if self
            .spatial
            .workspaces
            .layout
            .active_workspace(&output_name)
            != target
        {
            return;
        }
        let from = if step > 0 { -1.0 } else { 1.0 };
        let duration =
            Duration::from_millis(u64::from(self.config.scroll.workspace_scroll_duration_ms));
        self.render.motion.workspace_scroll_cameras.insert(
            output_name,
            crate::swim::WorkspaceScrollCamera::start(from, current, duration),
        );
        self.request_redraw();
    }

    pub fn switch_workspace(&mut self, output: &Output, workspace: u32) {
        if self.exclusive_layer().is_some() {
            return;
        }
        let output_name = output.name();
        let current = self
            .spatial
            .workspaces
            .layout
            .active_workspace(&output_name);
        // Scroll sections slide the shared strip to the section instead of
        // capturing a transition. Separate strips switch like Classic, with
        // the configured workspace animation.
        if self.spatial.engine.projection() == Some(crate::config::WorkspaceProjection::Scroll)
            && self.config.scroll.workspaces == crate::scroll::WorkspaceMode::Sections
        {
            let target = if current == workspace && self.config.workspace_auto_back_and_forth {
                self.spatial
                    .workspaces
                    .workspace_previous
                    .get(&output_name)
                    .copied()
                    .unwrap_or(workspace)
            } else {
                workspace
            };
            self.switch_workspace_immediate(output, target);
            return;
        }
        // A second request before the first one's outgoing capture ran can
        // cancel it by selecting the still-visible workspace. Treating
        // this as auto-back-and-forth would instead jump somewhere the
        // user did not ask to see.
        if self
            .render
            .motion
            .pending_workspace_transitions
            .contains_key(&output_name)
            && current == workspace
        {
            self.render
                .motion
                .pending_workspace_transitions
                .remove(&output_name);
            self.request_redraw();
            return;
        }
        let workspace = if current != workspace {
            workspace
        } else if self.config.workspace_auto_back_and_forth {
            // niri's `workspace-auto-back-and-forth`: re-selecting the
            // already-active workspace jumps back to whichever one was
            // active immediately before it, instead of no-opping.
            match self.spatial.workspaces.workspace_previous.get(&output_name) {
                Some(&previous) if previous != current => previous,
                _ => return,
            }
        } else {
            return;
        };

        if (self.config.water_effects && self.config.workspace_transition.enabled)
            || (self.config.animations.enabled && self.config.animations.workspace.enabled)
        {
            // A new switch supersedes any wipe already running on this
            // output. That keeps both latency and texture memory bounded:
            // never a queue, never more than one full-output capture.
            self.render
                .motion
                .workspace_transitions
                .remove(&output_name);
            self.render.motion.workspace_glides.remove(&output_name);
            self.render
                .motion
                .pending_workspace_transitions
                .insert(output_name, workspace);
            self.request_redraw();
            return;
        }

        self.apply_workspace_switch(output, current, workspace);
    }

    /// Immediate variant for protocol-driven activation: focus must land
    /// on the newly visible surface in the same dispatch, so this path
    /// cannot wait for the render loop's outgoing-frame capture. Also the
    /// right entry point for a swim crossing: swim already renders the
    /// outgoing/incoming content itself via the continuous camera offset,
    /// so a wave-transition capture on top would be redundant work and
    /// would fight the camera for what the next frame shows.
    pub(crate) fn switch_workspace_immediate(&mut self, output: &Output, workspace: u32) {
        if self.exclusive_layer().is_some() {
            return;
        }
        let output_name = output.name();
        let current = self
            .spatial
            .workspaces
            .layout
            .active_workspace(&output_name);
        if current == workspace {
            return;
        }
        self.render
            .motion
            .pending_workspace_transitions
            .remove(&output_name);
        self.render
            .motion
            .workspace_transitions
            .remove(&output_name);
        self.render.motion.workspace_glides.remove(&output_name);
        self.apply_workspace_switch(output, current, workspace);
    }

    pub(super) fn apply_workspace_switch(&mut self, output: &Output, current: u32, workspace: u32) {
        self.close_depth_deck();
        let output_name = output.name();
        // Any switch supersedes an in-flight vertical workspace slide. The
        // step action re-inserts its camera after this call returns.
        self.render
            .motion
            .workspace_scroll_cameras
            .remove(&output_name);
        self.spatial
            .workspaces
            .workspace_previous
            .insert(output_name.clone(), current);

        // Hide everything on the outgoing workspace. Tiled windows come
        // from the tree; floating ones from the tag, snapshotting each
        // one's latest position first so it comes back exactly where it was.
        for window in self
            .spatial
            .workspaces
            .layout
            .windows_in(&output_name, current)
        {
            self.space.unmap_elem(&window);
        }
        // Pinned windows are exempt from this whole hide/show cycle -- they
        // stay mapped and visible regardless of which workspace tag they
        // happen to carry, so both loops below skip them entirely.
        let outgoing_floating: Vec<WlSurface> = self
            .spatial
            .workspaces
            .floating_workspace
            .iter()
            .filter(|(surface, tag)| {
                tag.output == output_name
                    && tag.workspace == current
                    && !self.spatial.workspaces.pinned.contains(*surface)
            })
            .map(|(surface, _)| surface.clone())
            .collect();
        for surface in &outgoing_floating {
            let window = self
                .space
                .elements()
                .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
                .cloned();
            let Some(window) = window else { continue };
            if !self.fullscreen.contains_key(surface) && !self.maximized.contains_key(surface) {
                if let Some(rect) = self.space.element_geometry(&window) {
                    if let Some(tag) = self.spatial.workspaces.floating_workspace.get_mut(surface) {
                        tag.rect = rect;
                    }
                }
            }
            self.space.unmap_elem(&window);
        }

        self.spatial
            .workspaces
            .layout
            .set_active_workspace(&output_name, workspace);

        // Show everything belonging to the new workspace. Tiled windows
        // come back through retile() below (it only ever walks the
        // *active* workspace's tree per output, so switching that above is
        // enough); floating ones need an explicit map at their saved rect,
        // using the `Window` handle the tag itself holds -- a hidden
        // floating window was `unmap_elem`'d, so it isn't in
        // `space.elements()` to look up anymore, unlike a tiled one (whose
        // `Window` always lives on in its `Layouts` tree regardless).
        let incoming_floating: Vec<(Window, Point<i32, Logical>)> = self
            .spatial
            .workspaces
            .floating_workspace
            .iter()
            .filter(|(surface, tag)| {
                tag.output == output_name
                    && tag.workspace == workspace
                    && !self.spatial.workspaces.pinned.contains(*surface)
            })
            .map(|(_, tag)| (tag.window.clone(), tag.rect.loc))
            .collect();
        for (window, loc) in incoming_floating {
            self.space.map_element(window, loc, false);
        }

        self.retile();

        self.check_workspace_created_empty(&output_name, workspace);
        self.refocus_after_hide(&output_name);

        self.refresh_ext_workspaces();
        self.emit_ipc_event(crate::ipc::IpcEvent::WorkspaceChanged {
            output: output_name,
            from: current,
            to: workspace,
        });

        self.request_redraw();
    }

    /// Fires `workspace_rule { on_created_empty }` the first time this
    /// (output, workspace) pair is switched into while it has zero
    /// windows -- see `workspace_created_empty_fired`'s own doc for why
    /// "first empty switch-into" is the closest honest analog available,
    /// given TideWM's numbered workspaces have no real create/destroy
    /// lifecycle to hook a one-shot event on the way Hyprland can. Only
    /// marks the pair fired once a command actually ran, so a
    /// `workspace_rule` added after an earlier empty visit still gets its
    /// chance on the next one.
    fn check_workspace_created_empty(&mut self, output_name: &str, workspace: u32) {
        let key = (output_name.to_string(), workspace);
        if self
            .spatial
            .workspaces
            .workspace_created_empty_fired
            .contains(&key)
        {
            return;
        }
        let Some(cmd) = self
            .config
            .resolve_workspace_rule(workspace)
            .on_created_empty
        else {
            return;
        };
        let has_tiled = !self
            .spatial
            .workspaces
            .layout
            .windows_in(output_name, workspace)
            .is_empty();
        let has_floating = self
            .spatial
            .workspaces
            .floating_workspace
            .values()
            .any(|tag| tag.output == output_name && tag.workspace == workspace);
        if has_tiled || has_floating {
            return;
        }
        self.spatial
            .workspaces
            .workspace_created_empty_fired
            .insert(key);
        if let Err(err) = crate::spawn(&cmd) {
            tracing::warn!(
                %err,
                cmd,
                workspace,
                "Failed to spawn workspace_rule on_created_empty command"
            );
        }
    }

    /// Moves `surface` to `workspace` on whichever output it's currently on.
    /// Stays on the current workspace rather than following (matching i3's
    /// default `Mod+Shift+N` behavior) -- the simpler of the two common
    /// conventions, easy to flip later if it feels wrong in practice.
    pub fn move_to_workspace(&mut self, surface: &WlSurface, workspace: u32) {
        let Some(output) = self
            .spatial
            .workspaces
            .layout
            .output_of(surface)
            .map(str::to_string)
        else {
            // Not tiled -- must be floating, or not tracked at all.
            let Some(tag) = self.spatial.workspaces.floating_workspace.get(surface) else {
                return;
            };
            let output = tag.output.clone();
            // The window's *own* current workspace, not the output's active
            // one -- it may already be sitting hidden on some other
            // workspace (having been moved there previously), in which case
            // the active workspace tells us nothing about its visibility.
            let current = tag.workspace;
            if workspace == current {
                return;
            }
            let active = self.spatial.workspaces.layout.active_workspace(&output);
            if let Some(tag) = self.spatial.workspaces.floating_workspace.get_mut(surface) {
                tag.workspace = workspace;
            }
            // The workspace number just changed, and `rule { on_workspace }`
            // is a live match criterion feeding the cached opacity/glass
            // maps -- re-resolve now rather than waiting for some later
            // unrelated trigger.
            self.refresh_window_opacity_and_glass_for(surface);

            // Pinned windows stay mapped and visible regardless of which
            // workspace they're nominally tagged with -- same exemption
            // `switch_workspace` applies, nothing to show or hide here.
            if self.spatial.workspaces.pinned.contains(surface) {
                return;
            }

            if current == active && workspace != active {
                // Leaving the visible workspace: hide it.
                let window = self
                    .space
                    .elements()
                    .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
                    .cloned();
                if let Some(window) = window {
                    if !self.fullscreen.contains_key(surface)
                        && !self.maximized.contains_key(surface)
                    {
                        if let Some(rect) = self.space.element_geometry(&window) {
                            if let Some(tag) =
                                self.spatial.workspaces.floating_workspace.get_mut(surface)
                            {
                                tag.rect = rect;
                            }
                        }
                    }
                    self.space.unmap_elem(&window);
                }
                self.refocus_after_hide(&output);
            } else if current != active && workspace == active {
                // Entering the visible workspace: show it at its saved rect.
                // (`current != active` is what tells this case apart from
                // "already visible, just retagging its number" -- comparing
                // only `workspace == active` would wrongly skip this too.)
                if let Some(tag) = self.spatial.workspaces.floating_workspace.get(surface) {
                    self.space
                        .map_element(tag.window.clone(), tag.rect.loc, false);
                }
                // Fullscreen/maximized tags intentionally retain their
                // normal restore rectangle while hidden. Reconcile the
                // output-owned placement immediately instead of exposing
                // that normal location with a viewport-sized/stateful
                // buffer until some unrelated future retile.
                if self.fullscreen.contains_key(surface) || self.maximized.contains_key(surface) {
                    self.retile();
                }
            }
            self.request_redraw();
            return;
        };

        // Same distinction as the floating branch above: the window's own
        // current workspace, not the output's active one, since it may
        // already be hidden on some other workspace than whichever is
        // active right now.
        let current = self
            .spatial
            .workspaces
            .layout
            .workspace_of(surface)
            .unwrap_or_else(|| self.spatial.workspaces.layout.active_workspace(&output));
        if workspace == current {
            return;
        }
        let active = self.spatial.workspaces.layout.active_workspace(&output);
        // Hidden tiled windows remain in the tree but not in Space.
        let Some(window) = self.spatial.workspaces.layout.window_of(surface) else {
            return;
        };
        self.spatial.workspaces.layout.remove(surface);
        self.spatial
            .workspaces
            .layout
            .insert(&output, workspace, window.clone(), None);
        // See the floating branch above: `on_workspace` is a live match
        // criterion feeding cached opacity/glass.
        self.refresh_window_opacity_and_glass_for(surface);

        if current == active {
            // Hide the departing tile before filling its active-tree slot.
            self.space.unmap_elem(&window);
            self.retile();
            self.refocus_after_hide(&output);
        } else {
            // Retile maps the target only when its new workspace is active.
            self.retile();
        }
    }

    /// Toggles a reserved scratchpad workspace. Entering remembers only a
    /// non-scratchpad origin; switching directly between named scratchpads
    /// therefore preserves the real workspace used when toggling back.
    pub fn toggle_scratchpad(&mut self, output: &Output, name: Option<&str>) {
        let Some(target) = self.scratchpad_workspace(name) else {
            return;
        };
        let output_name = output.name();
        let current = self
            .spatial
            .workspaces
            .layout
            .active_workspace(&output_name);
        if current == target {
            let previous = self
                .spatial
                .workspaces
                .scratchpad_previous
                .get(&output_name)
                .copied()
                .unwrap_or(1);
            self.switch_workspace(output, previous);
        } else {
            if !is_scratchpad_workspace(current) {
                self.spatial
                    .workspaces
                    .scratchpad_previous
                    .insert(output_name, current);
            }
            self.switch_workspace(output, target);
        }
    }

    /// Moves `surface` to the (possibly named) scratchpad on whichever
    /// output it's currently on -- just `move_to_workspace` with the
    /// reserved scratchpad number, so it works for both tiled and floating
    /// windows exactly like moving to any other workspace does.
    pub fn move_to_scratchpad(&mut self, surface: &WlSurface, name: Option<&str>) {
        if let Some(workspace) = self.scratchpad_workspace(name) {
            self.move_to_workspace(surface, workspace);
        }
    }

    /// The effective tiling gap for (output, workspace): a per-workspace
    /// `workspace_gaps` override wins, then the output's own `gaps`, then
    /// the global `gaps`. Every layout-geometry call site routes through
    /// this so the three levels can't drift apart.
    pub(crate) fn gaps_for(&self, output_name: &str, workspace: u32) -> i32 {
        if let Some(&gaps) = self.config.workspace_gaps.get(&workspace) {
            return gaps;
        }
        self.config
            .outputs
            .iter()
            .find(|o| o.name == output_name)
            .and_then(|o| o.gaps)
            .unwrap_or(self.config.gaps)
    }

    /// The effective adaptive-sync preference for `output_name`: its own
    /// `output { }` override if set, else the global `adaptive_sync`
    /// default. The udev render path consumes this resolution and applies it
    /// through Smithay's DRM compositor.
    pub(crate) fn adaptive_sync_for(&self, output_name: &str) -> crate::config::AdaptiveSync {
        self.config
            .outputs
            .iter()
            .find(|o| o.name == output_name)
            .and_then(|o| o.adaptive_sync)
            .unwrap_or(self.config.adaptive_sync)
    }

    /// The reserved workspace number for scratchpad `name`, allocating one
    /// on first use. `None` (the bare `toggle-scratchpad` action) is the
    /// classic unnamed scratchpad, workspace `SCRATCHPAD_WORKSPACE`.
    /// Returns `None` only if all 4096 named slots are somehow exhausted.
    fn scratchpad_workspace(&mut self, name: Option<&str>) -> Option<u32> {
        let Some(name) = name else {
            return Some(SCRATCHPAD_WORKSPACE);
        };
        if let Some(&workspace) = self.spatial.workspaces.scratchpad_named.get(name) {
            return Some(workspace);
        }
        let next = NAMED_SCRATCHPAD_BASE
            .checked_add(self.spatial.workspaces.scratchpad_named.len() as u32)?;
        self.spatial
            .workspaces
            .scratchpad_named
            .insert(name.to_string(), next);
        Some(next)
    }

    /// The name of the named scratchpad `workspace` belongs to, if any.
    /// Used by the IPC `workspaces` query so bars can label (or hide)
    /// scratchpad entries instead of showing a raw reserved number.
    pub(crate) fn scratchpad_name_of(&self, workspace: u32) -> Option<&str> {
        self.spatial
            .workspaces
            .scratchpad_named
            .iter()
            .find(|(_, &ws)| ws == workspace)
            .map(|(name, _)| name.as_str())
    }

    /// Toggles `surface` pinned: exempt from every workspace's hide/show
    /// cycle on its output, staying mapped and visible no matter which
    /// workspace is active. Un-tiles first if `surface` is currently
    /// tiled (reusing `toggle_floating`'s own tiled-to-floating path) --
    /// pinning only ever makes sense for a floating window, since only one
    /// workspace's tiling tree is ever rendered per output at a time.
    /// Unpinning doesn't re-tile it back; it just stays floating in place,
    /// matching Hyprland's own pin behavior.
    pub fn toggle_pin(&mut self, surface: &WlSurface) {
        self.physics.buoyancy_dirty = true;
        if let Some(was_pinned) = self.fullscreen.get(surface).map(|entry| entry.was_pinned) {
            // Fullscreen temporarily suspends pinning. A toggle changes the
            // mode to restore on exit without making two placement owners
            // authoritative at once.
            let wants_pinned = !was_pinned;
            if wants_pinned {
                if self.spatial.workspaces.layout.contains(surface)
                    || self.spatial.ocean.is_tiled(surface)
                {
                    self.toggle_floating(surface);
                    if let Some(entry) = self.fullscreen.get_mut(surface) {
                        entry.pin_floated_it = true;
                    }
                }
            } else if self
                .fullscreen
                .get(surface)
                .is_some_and(|e| e.pin_floated_it)
            {
                // Undo specifically the float *this* pin toggle caused,
                // not a general "unpinning re-tiles" rule -- that's
                // deliberately not how the non-fullscreen case above
                // works (see this function's own doc comment: unpinning
                // normally leaves a window floating in place, matching
                // Hyprland). This differs because the whole pin-then-
                // unpin round trip happened without the window ever being
                // visibly floating (fullscreen was still covering it the
                // entire time) -- there's no "user saw it floating and
                // might want to keep it that way" moment to preserve, so
                // reverting the mechanical side effect back to how it
                // started is the least surprising outcome once fullscreen
                // ends. Only fires when this toggle is the one that
                // floated it; a window already floating before it got
                // pinned is left alone, same as the normal case.
                self.toggle_floating(surface);
            }
            if let Some(entry) = self.fullscreen.get_mut(surface) {
                entry.was_pinned = wants_pinned;
                if !wants_pinned {
                    entry.pin_floated_it = false;
                }
            }
            self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
                surface: surface.clone(),
            });
            self.request_redraw();
            return;
        }
        if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Ocean {
            // Ocean's viewport anchor is the only pin authority. Drop any
            // stale Classic marker left by an interrupted migration before
            // deciding whether this is a pin or unpin operation.
            self.spatial.workspaces.pinned.remove(surface);
            if self.spatial.ocean.is_screen_pinned(surface) {
                self.spatial.ocean.unpin_from_screen(surface);
            } else {
                if self.spatial.ocean.is_tiled(surface) {
                    self.toggle_floating(surface);
                }
                let output = self
                    .spatial
                    .ocean
                    .entry_output(surface)
                    .and_then(|name| self.output_by_name(name))
                    .or_else(|| self.primary_output());
                let Some(output) = output else {
                    return;
                };
                if !self.spatial.ocean.pin_to_screen(surface, &output.name()) {
                    return;
                }
            }
            self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
                surface: surface.clone(),
            });
            self.request_redraw();
            return;
        }
        if self.spatial.workspaces.pinned.remove(surface) {
            // While pinned, this window stayed mapped regardless of which
            // workspace was actually active -- so its FloatingTag may still
            // say whatever workspace was active back when it got pinned,
            // not wherever it visually is now. Without reconciling that
            // here, switch_workspace's hide loop would look for it on the
            // stale workspace and leave it stuck visible on every other
            // one until the user happens to pass through that specific
            // workspace number again.
            let window = self
                .space
                .elements()
                .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
                .cloned();
            if let Some(output) = window.as_ref().and_then(|w| self.output_for_window(w)) {
                let active = self
                    .spatial
                    .workspaces
                    .layout
                    .active_workspace(&output.name());
                if let Some(tag) = self.spatial.workspaces.floating_workspace.get_mut(surface) {
                    tag.output = output.name();
                    tag.workspace = active;
                }
            }
            self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
                surface: surface.clone(),
            });
            self.request_redraw();
            return;
        }
        if self.spatial.workspaces.layout.contains(surface) {
            self.toggle_floating(surface);
        }
        self.spatial.workspaces.pinned.insert(surface.clone());
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
        self.request_redraw();
    }

    /// Toggles `surface` pinned to whichever output it's currently
    /// rendered on -- the interactive counterpart to `rule { pin_output }`.
    /// Unlike `toggle_pin` (workspace-sticky, floating-only by necessity),
    /// this has nothing to do with tiling state: a tiled window pins just
    /// as well as a floating one, so nothing here changes float/tile
    /// status. No-op if the window isn't currently rendered anywhere (not
    /// mapped, or its output vanished).
    pub fn toggle_output_pin(&mut self, surface: &WlSurface) {
        if self
            .spatial
            .workspaces
            .output_pins
            .remove(surface)
            .is_some()
        {
            self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
                surface: surface.clone(),
            });
            self.request_redraw();
            return;
        }
        let Some(output) = self.rendered_output_for_surface(surface) else {
            return;
        };
        self.spatial
            .workspaces
            .output_pins
            .insert(surface.clone(), output.name());
        self.emit_ipc_event(crate::ipc::IpcEvent::WindowChanged {
            surface: surface.clone(),
        });
        self.request_redraw();
    }

    /// The hotplug half of output pinning (`rule { pin_output }`,
    /// `toggle-output-pin`): brings back any window pinned to `to_output`
    /// that's currently sitting somewhere else because its own output was
    /// disconnected when it last mapped or moved. A pin is never cleared by
    /// `migrate_output_windows` falling a window back -- it's the only
    /// memory of where the window belongs -- so this is what actually acts
    /// on that memory once the named output reconnects. Distinct from
    /// `adopt_orphaned_output_windows` just below: that one recovers
    /// windows stranded on a now-dead output name onto *whatever* output
    /// next appears; this one is selective, pulling only pinned windows
    /// toward the specific output their pin names. Call order between the
    /// two doesn't matter -- they act on disjoint window sets in the
    /// common case. Called from `backend/udev.rs`'s output-added path,
    /// same single call site `adopt_orphaned_output_windows` has -- the
    /// winit backend's one simulated output never hotplugs (see its own
    /// `output_count` comment), so there's nothing to recall there.
    pub(crate) fn recall_pinned_windows_to(&mut self, to_output_name: &str) {
        let Some(to_output) = self.output_by_name(to_output_name) else {
            return;
        };
        let displaced: Vec<WlSurface> = self
            .spatial
            .workspaces
            .output_pins
            .iter()
            .filter(|(surface, pinned)| {
                pinned.as_str() == to_output_name
                    && self
                        .spatial
                        .workspaces
                        .layout
                        .output_of(surface)
                        .map(|current| current != to_output_name)
                        .unwrap_or_else(|| {
                            self.spatial
                                .workspaces
                                .floating_workspace
                                .get(*surface)
                                .is_some_and(|tag| tag.output != to_output_name)
                        })
            })
            .map(|(surface, _)| surface.clone())
            .collect();
        if displaced.is_empty() {
            return;
        }
        for surface in &displaced {
            tracing::info!(output = to_output_name, "Recalling output-pinned window");
            self.move_window_to_output(surface, &to_output);
        }
        self.retile();
    }

    /// Pin ownership follows the active spatial engine. Classic keeps its
    /// workspace exemption in `pinned`; Ocean owns viewport anchoring in
    /// `screen_pins`. Keeping this query centralized prevents migration from
    /// leaving two disagreeing sources of truth.
    pub(crate) fn window_is_pinned(&self, surface: &WlSurface) -> bool {
        match self.spatial.engine.ownership() {
            crate::config::SpatialOwnership::Workspaces => {
                self.spatial.workspaces.pinned.contains(surface)
            }
            crate::config::SpatialOwnership::Ocean => self.spatial.ocean.is_screen_pinned(surface),
        }
    }

    /// The mapped output named `name`, if any. Used to resolve
    /// `Action::SwapWorkspacesWithOutput`'s config-supplied output name
    /// into a real `Output`.
    pub(crate) fn output_by_name(&self, name: &str) -> Option<Output> {
        self.space.outputs().find(|o| o.name() == name).cloned()
    }

    /// Swaps `output_a`'s and `output_b`'s currently-active workspace
    /// *content* -- the tiled tree and every tagged floating window trade
    /// places, relocating onto the other monitor's screen area, while
    /// each output keeps its own workspace-number bookkeeping untouched
    /// Swaps the two outputs' active per-output workspaces. No-op for one
    /// output or while an exclusive-interactivity layer owns navigation.
    pub fn swap_workspaces(&mut self, output_a: &Output, output_b: &Output) {
        if self.exclusive_layer().is_some() {
            return;
        }
        let name_a = output_a.name();
        let name_b = output_b.name();
        if name_a == name_b {
            return;
        }
        let ws_a = self.spatial.workspaces.layout.active_workspace(&name_a);
        let ws_b = self.spatial.workspaces.layout.active_workspace(&name_b);

        // Every window whose workspace *number* is about to change (tiled
        // content at ws_a/ws_b is renumbered to the other output's active
        // number by `swap_active` below; floating tags are retagged the
        // same way further down). `rule { on_workspace }` is a live match
        // criterion feeding the cached opacity/glass maps, so each of these
        // needs a re-resolve once the swap actually lands -- see the loop
        // at the end of this function.
        let on_workspace_changed: Vec<WlSurface> = self
            .spatial
            .workspaces
            .layout
            .windows_in(&name_a, ws_a)
            .into_iter()
            .chain(self.spatial.workspaces.layout.windows_in(&name_b, ws_b))
            .filter_map(|window| window.toplevel().map(|t| t.wl_surface().clone()))
            .chain(
                self.spatial
                    .workspaces
                    .floating_workspace
                    .iter()
                    .filter(|(_, tag)| {
                        (tag.output == name_a && tag.workspace == ws_a)
                            || (tag.output == name_b && tag.workspace == ws_b)
                    })
                    .map(|(surface, _)| surface.clone()),
            )
            .collect();

        // Capture tiled membership before swapping the trees. Fullscreen is
        // an override keyed outside `Layouts`, so moving a tree does not move
        // its FullscreenEntry automatically.
        let fullscreen_tiled_a: Vec<WlSurface> = self
            .spatial
            .workspaces
            .layout
            .windows_in(&name_a, ws_a)
            .into_iter()
            .filter_map(|window| window.toplevel().map(|t| t.wl_surface().clone()))
            .filter(|surface| self.fullscreen.contains_key(surface))
            .collect();
        let fullscreen_tiled_b: Vec<WlSurface> = self
            .spatial
            .workspaces
            .layout
            .windows_in(&name_b, ws_b)
            .into_iter()
            .filter_map(|window| window.toplevel().map(|t| t.wl_surface().clone()))
            .filter(|surface| self.fullscreen.contains_key(surface))
            .collect();

        let fullscreen_floating_a: Vec<WlSurface> = self
            .spatial
            .workspaces
            .floating_workspace
            .iter()
            .filter(|(surface, tag)| {
                tag.output == name_a
                    && tag.workspace == ws_a
                    && !self.spatial.workspaces.pinned.contains(*surface)
                    && self.fullscreen.contains_key(*surface)
            })
            .map(|(surface, _)| surface.clone())
            .collect();
        let fullscreen_floating_b: Vec<WlSurface> = self
            .spatial
            .workspaces
            .floating_workspace
            .iter()
            .filter(|(surface, tag)| {
                tag.output == name_b
                    && tag.workspace == ws_b
                    && !self.spatial.workspaces.pinned.contains(*surface)
                    && self.fullscreen.contains_key(*surface)
            })
            .map(|(surface, _)| surface.clone())
            .collect();
        let moving_fullscreen_a: Vec<WlSurface> = fullscreen_tiled_a
            .iter()
            .chain(&fullscreen_floating_a)
            .cloned()
            .collect();
        let moving_fullscreen_b: Vec<WlSurface> = fullscreen_tiled_b
            .iter()
            .chain(&fullscreen_floating_b)
            .cloned()
            .collect();

        // A destination may already own a fullscreen entry on a hidden
        // workspace. If fullscreen content is arriving there, pre-empt that
        // non-moving entry first so the swap cannot create two fullscreen
        // owners for one output. The active fullscreen on the other side is
        // excluded because it is vacating the destination in this same swap.
        let mut preempt = Vec::new();
        if !moving_fullscreen_a.is_empty() {
            if let Some(surface) = self
                .fullscreen
                .iter()
                .find(|(surface, entry)| {
                    entry.output == name_b && !moving_fullscreen_b.contains(*surface)
                })
                .map(|(surface, _)| surface.clone())
            {
                preempt.push(surface);
            }
        }
        if !moving_fullscreen_b.is_empty() {
            if let Some(surface) = self
                .fullscreen
                .iter()
                .find(|(surface, entry)| {
                    entry.output == name_a && !moving_fullscreen_a.contains(*surface)
                })
                .map(|(surface, _)| surface.clone())
            {
                preempt.push(surface);
            }
        }
        for surface in preempt {
            let toplevel = self
                .mapped_toplevel_window(&surface)
                .and_then(|window| window.toplevel().cloned());
            if let Some(toplevel) = toplevel {
                self.do_unfullscreen(&toplevel);
            } else {
                self.fullscreen.remove(&surface);
            }
        }

        self.spatial.workspaces.layout.swap_active(&name_a, &name_b);

        // Groups key their owner off the tree their leaf lives in, which
        // `swap_active` just moved to the other output. Retag the two
        // participating owners or tab-strip filtering, ungroup
        // reinsertion, activation, and the accessibility snapshot keep
        // resolving the pre-swap output/workspace. Classic-only: under
        // Ocean no tree moved, and the fields are migration cache, not
        // ownership.
        if self.spatial.engine.ownership() == crate::config::SpatialOwnership::Workspaces {
            for group in &mut self.groups {
                if let Some((output, workspace)) = swapped_group_owner(
                    &group.output,
                    group.workspace,
                    &name_a,
                    ws_a,
                    &name_b,
                    ws_b,
                ) {
                    group.output = output;
                    group.workspace = workspace;
                }
            }
        }

        let delta = match (
            self.space.output_geometry(output_a),
            self.space.output_geometry(output_b),
        ) {
            (Some(geo_a), Some(geo_b)) => Some(geo_b.loc - geo_a.loc),
            _ => None,
        };
        let bounds_a = self
            .output_tiling_area(output_a)
            .or_else(|| self.space.output_geometry(output_a));
        let bounds_b = self
            .output_tiling_area(output_b)
            .or_else(|| self.space.output_geometry(output_b));

        // FloatingTag.rect is a hide-time snapshot and deliberately stale
        // while visible. Capture current non-fullscreen geometry before the
        // ownership mutation so a workspace swap cannot snap a moved/resized
        // floater back to its old position. A fullscreen tag retains its
        // windowed restore rect instead of the viewport geometry.
        let live_floating_rects: Vec<(WlSurface, Rectangle<i32, Logical>)> = self
            .space
            .elements()
            .filter_map(|window| {
                let surface = window.toplevel()?.wl_surface().clone();
                if self.fullscreen.contains_key(&surface) || self.maximized.contains_key(&surface) {
                    None
                } else {
                    self.space
                        .element_geometry(window)
                        .map(|rect| (surface, rect))
                }
            })
            .collect();
        for surface in fullscreen_tiled_a {
            if let Some(entry) = self.fullscreen.get_mut(&surface) {
                entry.move_to_output(name_b.clone(), delta);
                if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, bounds_b) {
                    *rect = clamp_rect_visible(*rect, bounds);
                }
            }
        }
        for surface in fullscreen_tiled_b {
            if let Some(entry) = self.fullscreen.get_mut(&surface) {
                entry.move_to_output(
                    name_a.clone(),
                    delta.map(|delta| (-delta.x, -delta.y).into()),
                );
                if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, bounds_a) {
                    *rect = clamp_rect_visible(*rect, bounds);
                }
            }
        }

        // Floating windows aren't tracked by `Layouts` at all, so they need
        // their own retag (their content-group's identity moved to the
        // other output) and reposition (translate by the delta between the
        // two outputs' origins, preserving relative on-screen placement --
        // they were never unmapped, so nothing else will move them).
        let mut moved: Vec<(Window, Point<i32, Logical>)> = Vec::new();
        for (surface, tag) in &mut self.spatial.workspaces.floating_workspace {
            // Pinning exempts a window from workspace membership; including
            // it only when its intentionally stale nominal workspace happened
            // to be active made cross-output behavior nondeterministic.
            if self.spatial.workspaces.pinned.contains(surface) {
                continue;
            }
            if tag.output == name_a && tag.workspace == ws_a {
                if let Some((_, rect)) = live_floating_rects
                    .iter()
                    .find(|(candidate, _)| candidate == surface)
                {
                    tag.rect = *rect;
                }
                tag.output = name_b.clone();
                tag.workspace = ws_b;
                if let Some(delta) = delta {
                    tag.rect.loc += delta;
                    if let Some(bounds) = bounds_b {
                        tag.rect = clamp_rect_visible(tag.rect, bounds);
                    }
                    moved.push((tag.window.clone(), tag.rect.loc));
                }
                if let Some(entry) = self.fullscreen.get_mut(surface) {
                    entry.move_to_output(name_b.clone(), delta);
                    if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, bounds_b) {
                        *rect = clamp_rect_visible(*rect, bounds);
                    }
                }
                if let Some(entry) = self.maximized.get_mut(surface) {
                    entry.move_to_output(name_b.clone(), delta);
                    if let Some(bounds) = bounds_b {
                        entry.restore_rect = clamp_rect_visible(entry.restore_rect, bounds);
                    }
                }
            } else if tag.output == name_b && tag.workspace == ws_b {
                if let Some((_, rect)) = live_floating_rects
                    .iter()
                    .find(|(candidate, _)| candidate == surface)
                {
                    tag.rect = *rect;
                }
                tag.output = name_a.clone();
                tag.workspace = ws_a;
                if let Some(delta) = delta {
                    tag.rect.loc -= delta;
                    if let Some(bounds) = bounds_a {
                        tag.rect = clamp_rect_visible(tag.rect, bounds);
                    }
                    moved.push((tag.window.clone(), tag.rect.loc));
                }
                if let Some(entry) = self.fullscreen.get_mut(surface) {
                    entry.move_to_output(
                        name_a.clone(),
                        delta.map(|delta| (-delta.x, -delta.y).into()),
                    );
                    if let (Some(rect), Some(bounds)) = (&mut entry.restore_rect, bounds_a) {
                        *rect = clamp_rect_visible(*rect, bounds);
                    }
                }
                if let Some(entry) = self.maximized.get_mut(surface) {
                    entry.move_to_output(
                        name_a.clone(),
                        delta.map(|delta| (-delta.x, -delta.y).into()),
                    );
                    if let Some(bounds) = bounds_a {
                        entry.restore_rect = clamp_rect_visible(entry.restore_rect, bounds);
                    }
                }
            }
        }
        for (window, loc) in moved {
            self.space.map_element(window, loc, false);
        }

        for surface in &on_workspace_changed {
            self.refresh_window_opacity_and_glass_for(surface);
        }

        self.retile();
    }

    /// Shows/hides the workspace overview on the current output (see
    /// `overview.rs`, `Action::ToggleOverview`). Built fresh each time it's
    /// toggled on -- it doesn't animate or otherwise need to stay in sync
    /// with changes made while it's open, so there's nothing to cache
    /// across frames the way `tab_strip_elements` caches per group.
    pub fn toggle_overview(&mut self) {
        self.close_depth_deck();
        if self.overview.take().is_some() {
            self.request_redraw();
            return;
        }

        let Some(output) = self.primary_output() else {
            return;
        };
        let Some(output_geometry) = self.space.output_geometry(&output) else {
            return;
        };
        let output_name = output.name();
        let active_workspace = self
            .spatial
            .workspaces
            .layout
            .active_workspace(&output_name);

        // Every workspace this output has ever populated, plus the active
        // one even if it's currently empty, so there's always at least one
        // cell to show.
        let mut workspaces: Vec<u32> = self
            .spatial
            .workspaces
            .layout
            .populated_workspaces()
            .into_iter()
            .filter(|(name, _)| name == &output_name)
            .map(|(_, workspace)| workspace)
            .collect();
        if !workspaces.contains(&active_workspace) {
            workspaces.push(active_workspace);
        }
        workspaces.sort_unstable();

        const CELL_GAP: i32 = 12;
        let cell_count = workspaces.len() as i32;
        let cell_w = (output_geometry.size.w - CELL_GAP * (cell_count + 1)) / cell_count.max(1);
        let cell_h = output_geometry.size.h - CELL_GAP * 2;

        let cells: Vec<crate::overview::OverviewCell> = workspaces
            .iter()
            .enumerate()
            .map(|(i, &workspace)| {
                let x = CELL_GAP + i as i32 * (cell_w + CELL_GAP);
                let area =
                    Rectangle::new((x, CELL_GAP).into(), (cell_w.max(1), cell_h.max(1)).into());
                // Reuses whichever algorithm (BSP or master) is actually
                // active for this workspace, computed directly at the
                // cell's own smaller size -- no separate scaling math
                // needed, `Layouts::layout` already produces proportional
                // rects for whatever area it's given.
                let windows = self
                    .spatial
                    .workspaces
                    .layout
                    .layout(&output_name, workspace, area, 4)
                    .into_iter()
                    .filter_map(|(window, rect)| {
                        let surface = window.toplevel()?.wl_surface().clone();
                        Some((rect, crate::tab_strip::window_title(&surface)))
                    })
                    .collect();
                crate::overview::OverviewCell {
                    workspace,
                    area,
                    active: workspace == active_workspace,
                    windows,
                }
            })
            .collect();

        self.overview = Some(crate::overview::Overview::build(
            output_name,
            &cells,
            (output_geometry.size.w, output_geometry.size.h),
        ));
        self.request_redraw();
    }
}
