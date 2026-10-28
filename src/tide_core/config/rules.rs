use super::*;

/// Match facts `resolve_window_rules` needs for one window, bundled so the
/// function stays under clippy's argument-count lint as match criteria
/// (initial_class/initial_title/at_startup) keep growing. Same facts
/// `Smallvil::resolve_window_rules_for` gathers per surface.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WindowMatchFacts<'a> {
    pub app_id: Option<&'a str>,
    pub title: Option<&'a str>,
    pub pid: Option<i32>,
    pub is_xwayland: bool,
    pub urgent: bool,
    pub initial_app_id: Option<&'a str>,
    pub initial_title: Option<&'a str>,
    pub at_startup: bool,
    /// The window's current tag set, if it has ever carried one --
    /// `None` rather than an empty set for the common untagged case, so
    /// resolving rules for most windows borrows nothing new. See
    /// `WindowRule::matches_tags`.
    pub tags: Option<&'a HashSet<String>>,
    /// The window's current workspace number, from
    /// `Smallvil::workspace_of_surface` -- `None` if untracked by either
    /// `Layouts` or `floating_workspace`, or under Ocean. See
    /// `WindowRule::matches_on_workspace`.
    pub on_workspace: Option<u32>,
}

/// One `rule { }` entry: match newly-mapped windows by `app_id`/
/// `title`/`pid`/`xwayland` (i3/sway's `for_window`, Hyprland's
/// `windowrule` idea) and apply
/// a placement before the window's first tile/floating rect is ever
/// computed, so a matched window never visibly starts in its default spot
/// and jumps. `app_id` matches the whole string exactly (app IDs are
/// stable identifiers, not prose); `title` matches case-insensitively
/// anywhere in the string (titles change constantly -- exact matching
/// would be nearly useless for them). See `Config::resolve_window_rules`
/// for how multiple matching entries combine, and `handlers/xdg_shell.rs`'s
/// `map_toplevel` for where this actually applies.
#[derive(Debug, Clone, Default)]
pub struct WindowRule {
    pub app_id: Option<String>,
    pub title: Option<String>,
    /// Compiled once while loading the config. Keeping the compiled form is
    /// important because capture rules may be checked for every recorded
    /// frame, not only when the window first maps.
    pub app_id_regex: Option<regex::Regex>,
    pub title_regex: Option<regex::Regex>,
    /// Exact PID match (sway's `[pid=...]` criterion). Never `Some` unless
    /// a window's real client PID is known at match time; a dead/unknown
    /// client never satisfies this even if the rule would otherwise match.
    pub pid: Option<i32>,
    /// Tri-state XWayland match (Hyprland's `xwayland:1`/`xwayland:0`).
    /// `None` matches either kind of window; `Some(true)`/`Some(false)`
    /// requires the window to be (or not be) an X11 client running
    /// through `xwayland-satellite`. See `Smallvil::is_xwayland_surface`.
    pub is_xwayland: Option<bool>,
    /// Tri-state urgent-hint match (sway's `urgent=latest`/Hyprland tag
    /// idea, narrowed to a plain bool here). Unlike every other field in
    /// this struct, this one is re-checked live: `Smallvil::mark_urgent`
    /// and the urgent-clear branch of `reconcile_keyboard_focus` both
    /// re-resolve rules for the affected surface, so a rule keyed on this
    /// takes effect immediately when urgency changes, not only at map
    /// time. See `Smallvil::resolve_window_rules_for`, the one place that
    /// gathers current urgency (`self.urgent.contains(surface)`) alongside
    /// every other match fact.
    pub urgent: Option<bool>,
    /// Live match on the window's *current* workspace number (Hyprland
    /// `onworkspace`, recalled from memory rather than read from a local
    /// clone -- the reference repos are outside this sandbox's allowed
    /// paths). Distinct from `workspace` below, which is a one-shot
    /// initial-placement effect: this is a match criterion, re-checked on
    /// every resolve via `Smallvil::workspace_of_surface`, so a rule keyed
    /// on it applies to whatever happens to live on that workspace number
    /// right now, on any output (same "workspace number on any output"
    /// convention `workspace_gaps`/`workspace_rule { }` already use), and
    /// stops applying the moment a window leaves. This is how "per-
    /// workspace persistent window-rule overrides" are built: `rule {
    /// on_workspace = N; ... }` layers its effects on top of
    /// `workspace_rule { }`'s own border/rounding/shadow/layout base for
    /// workspace N, the same precedence chain `border_config_for_surface`
    /// already runs, just reusable for the rest of `WindowRule`'s effect
    /// surface (opacity, glass, viscosity, tags, ...) instead of being
    /// limited to decoration. `None` under Ocean, which has no workspace
    /// concept -- an `on_workspace` rule simply never matches there. See
    /// `WindowRule::matches_on_workspace`.
    pub on_workspace: Option<u32>,
    /// Spawn-time-only class match (Hyprland `initialClass`). Checked
    /// against the app_id a window reported at its *first* map, captured
    /// once into `Smallvil::initial_toplevel_identity` and never updated
    /// again -- unlike `app_id`, a later `set_app_id` from the client can't
    /// make this stop or start matching. Case-insensitive exact match, same
    /// semantics as `app_id`. See `WindowRule::matches_initial`.
    pub initial_class: Option<String>,
    /// Spawn-time-only title match (Hyprland `initialTitle`). Same capture
    /// as `initial_class` but for title, substring match like `title`.
    pub initial_title: Option<String>,
    pub initial_class_regex: Option<regex::Regex>,
    pub initial_title_regex: Option<regex::Regex>,
    /// Tri-state match on whether the window mapped during the compositor's
    /// own startup grace window (niri `at-startup`) -- see
    /// `Smallvil::resolve_window_rules_for`'s `at_startup` fact. Lets a rule
    /// target only session-autostarted apps (`spawn`) differently from ones
    /// launched later by hand.
    pub at_startup: Option<bool>,
    /// Initial workspace number -- same numbering `workspace:N` keybinds
    /// use, including 0 (the reserved scratchpad) if you want a window to
    /// always start hidden.
    pub workspace: Option<u32>,
    /// Initial connector, or `focused` (`camera`/`current`) resolved at map
    /// time from retained window/camera focus rather than the pointer output.
    /// Unavailable connectors fall back through parent and primary output.
    /// Persistent `pin_output` never interprets these dynamic aliases.
    pub output: Option<String>,
    /// Persistently pins the window to an output by connector name.
    /// Neither niri (`window-rule { open-on-output }`) nor Hyprland
    /// (`windowrule = monitor:`) actually persists this at the window
    /// level -- both are spawn-time-only placement, the same thing
    /// `output` above already gives TideWM. niri's real *persistent*
    /// binding is a named workspace's `open-on-output`, which has nowhere
    /// to hang in TideWM's per-output-numbered-workspace model (a
    /// workspace number already exists independently on every output).
    /// This is TideWM's own adaptation, at the window instead: takes
    /// priority over `output` for initial placement (see `map_toplevel`)
    /// and is remembered in `WorkspaceState::output_pins` for the surface's
    /// whole lifetime, independent of whether the named output is
    /// currently connected. A disconnect still falls the window back like
    /// any other window on that output (`migrate_output_windows` can't do
    /// anything else -- the output is physically gone), but reconnecting
    /// the same output recalls it (see `Smallvil::recall_pinned_windows_to`,
    /// called from the udev backend's output-added path alongside the
    /// existing `adopt_orphaned_output_windows` -- winit's one simulated
    /// output never hotplugs). See the `toggle-output-pin` action
    /// for the interactive equivalent, and the DOCUMENTATION.md `rule { }`
    /// entry for the explicit `swap-workspaces-with-output` exception:
    /// that's an explicit user action that moves everything on the swapped
    /// workspace, pins included, deliberately left alone rather than given
    /// special per-window exemption logic.
    pub pin_output: Option<String>,
    pub float: bool,
    /// No-op unless the window ends up tiled -- pseudo-tiling only has
    /// meaning as a rect override on a real tile, same as the interactive
    /// `toggle-pseudo-tile` keybind. Ignored if this rule (or another
    /// matching one) also sets `float`/`pin`.
    pub pseudo_tile: bool,
    /// Implies `float` -- pinning only has meaning for a floating window,
    /// the same invariant `toggle-pin` enforces interactively.
    pub pin: bool,
    /// Overrides the implicit auto-float heuristic (see `map_toplevel`'s
    /// `has_parent`/`is_fixed_size` check) back to tiled for a window that
    /// would otherwise auto-float -- the one flag in this struct that
    /// means "force tiled" rather than "force floating," so it's the only
    /// way to counteract the heuristic per-app. No effect if `float`/`pin`
    /// also match, same as niri's own `open-floating false` precedence.
    pub tile: bool,
    /// Per-app drag/keyboard snap opt-out or reaffirmation. Unset inherits
    /// the workspace rule; the global `snap.enabled` value is the master.
    pub snap: Option<bool>,
    /// Suppresses the automatic focus-on-map a newly-mapped window
    /// normally gets (niri's `open-focused false`) -- useful for a
    /// background/scanner-style app that shouldn't steal focus from
    /// whatever's currently active. Leaves prior focus untouched entirely,
    /// rather than picking some other window to focus instead.
    pub no_focus: bool,
    pub maximize: bool,
    pub fullscreen: bool,
    pub block_capture: bool,
    /// Marks matching windows as swallowers (Hyprland's
    /// `misc:enable_swallow`, i3's window swallowing): a tiled window this
    /// rule matches gets hidden when a window whose process it spawned
    /// maps (PID ancestry via `/proc`), which takes over its tile; closing
    /// the child puts the swallower back in that exact slot. The classic
    /// use is `app_id = kitty, swallow = true` so a video/image viewer
    /// launched from a terminal replaces it instead of splitting it.
    /// Tiled swallowers only -- a floating terminal keeps both visible.
    pub swallow: bool,
    /// Per-window render alpha in the inclusive range 0.0..=1.0. Applied
    /// as a base multiplier to the complete surface tree (including
    /// subsurfaces and popups).
    pub opacity: Option<f32>,
    /// State-specific opacity multipliers. These multiply `opacity` when
    /// it is set, matching Hyprland's active/inactive/fullscreen model.
    /// Fullscreen wins over focus state.
    pub active_opacity: Option<f32>,
    pub inactive_opacity: Option<f32>,
    pub fullscreen_opacity: Option<f32>,
    /// Which captured-backdrop shader sits behind this floating window.
    /// Explicit modes also work with client-provided surface alpha, which
    /// keeps glyphs/foreground pixels opaque. Missing preserves the original
    /// behavior where compositor `opacity` below one implies water;
    /// `Plain` disables backdrop substitution while preserving `opacity`.
    pub glass: Option<GlassMode>,
    /// Custom effect by definition name, or `shader = none` to clear an
    /// earlier match. Last matching rule wins. The name resolves live
    /// against `shaders { enabled }` and `shader_definitions`, so a typo
    /// or a removed definition leaves the window on its normal path.
    pub shader: Option<ShaderAssignment>,
    /// Per-window interactive move/resize damping. Last matching rule wins;
    /// `0.0` disables damping for the matched app.
    pub viscosity: Option<f64>,
    /// Per-app opt-in/out for floating sway. Last matching rule wins;
    /// unset falls back to the global `sway { enabled }` value.
    pub sway: Option<bool>,
    /// Per-app tier override for cosmetic float physics (F1). Last
    /// matching rule wins; unset falls back to the global
    /// `float_physics { tier }` value. When resolved `light` or `full`, it
    /// takes over from `sway` for that window -- the two never stack.
    pub float_physics: Option<FloatPhysicsTier>,
    /// Apparent render-only weight for a floating window, from fully light
    /// (`0`) to fully heavy (`1`). Last matching rule wins.
    pub weight: Option<f32>,
    /// Per-app buoyancy override for the automatic depth/attention system
    /// (Phase R1). `Some(false)` pins the matched window at tier zero
    /// forever -- it never dims or sinks regardless of inactivity, useful
    /// for a widget or player the user always wants to see live. `Some(true)`
    /// affirms the normal automatic behavior (mainly useful to override an
    /// earlier matching rule's `false`). Unset inherits the global
    /// `depth { enabled }` behavior. Last matching rule wins.
    pub depth: Option<bool>,
    pub ebb: Option<super::EbbMode>,
    /// Per-app frost overrides. Unset fields inherit the global
    /// `frost { }` block, and multiple matching rules merge field by field.
    pub frost: Option<FrostOverrides>,
    /// Per-app analytical-shadow overrides. Unset fields inherit the global
    /// `shadow { }` block; matching rule sub-blocks merge field by field.
    pub shadow: Option<ShadowOverrides>,
    /// Per-app rounded-geometry overrides. Unset fields inherit the global
    /// `rounding { }` block; matching rules merge field by field.
    pub rounding: Option<RoundingOverrides>,
    /// Per-app analytical-border overrides.
    pub border: Option<BorderOverrides>,
    /// Exact floating placement (top-left corner), `<x>x<y>` -- the same
    /// syntax `output { }`'s `position` already uses. No-op unless the
    /// window ends up floating (from `float`/`pin`/the auto-float
    /// heuristic), same "only means something once floating" restriction
    /// `pseudo_tile` has in reverse for tiled.
    pub position: Option<(i32, i32)>,
    /// Exact floating size, `<width>x<height>` (same syntax as `position`).
    pub size: Option<(i32, i32)>,
    /// Per-app ripple overrides; `None` fields inherit the global
    /// `ripple { }` block. Set `enabled = false` inside this sub-block to
    /// suppress ripples entirely for matching windows. See `RippleConfig`.
    pub ripple: Option<RippleConfig>,
    /// Remember this app's last floating size across launches (Hyprland
    /// `persistentsize`). Recorded from the floating rect at close time
    /// (`Smallvil::detach_mapped_toplevel`, keyed by app_id in
    /// `Smallvil::remembered_floating_sizes`) and reapplied at the next map
    /// in place of the client's own natural size, unless this same rule
    /// also sets an explicit `size`. In-memory only -- does not survive a
    /// compositor restart, same "session, not disk" scope as the rest of
    /// TideWM's runtime state.
    pub persistent_size: bool,
    /// Hard floating-size bounds in logical pixels (niri `min-width` /
    /// `max-width` / `min-height` / `max-height`), tighter than whatever
    /// the client's own xdg `min_size`/`max_size` hints already are, never
    /// looser when the intersection is feasible. Conflicting bounds favor
    /// the minimum, matching the existing interactive client-hint policy.
    /// Applied to natural, explicit, and remembered floating placement,
    /// pointer/keyboard resize, snapping, and live rule/client-hint changes.
    /// Fullscreen/maximized geometry stays output-owned; normal restoration
    /// and tiled-to-floating conversion apply the current bounds.
    pub min_width: Option<i32>,
    pub max_width: Option<i32>,
    pub min_height: Option<i32>,
    pub max_height: Option<i32>,
    /// Multiplies libinput scroll-wheel/finger axis deltas while this
    /// window has pointer focus (niri `scroll-factor`). `1.0` is unset's
    /// implicit value; only the continuous axis amount is scaled, not
    /// discrete wheel-click (`v120`) counts. See the `PointerAxis` arm in
    /// `tide_core/input.rs`.
    pub scroll_factor: Option<f64>,
    /// Arbitrary string tags this rule requires (Hyprland's `tag:name`
    /// criterion, niri's marks): the rule matches when the window carries
    /// any tag listed here. Checked against the window's CURRENT tag set
    /// (`Smallvil::window_tags`), never a spawn-time snapshot -- a tag
    /// added mid-session (`tag-window`, `untag-window`, or another rule's
    /// `add_tag`) takes effect on the very next resolve, the same "live"
    /// character `urgent` already has. See `WindowRule::matches_tags`.
    pub tag: Vec<String>,
    /// Tags to add to a matching window the moment it first maps
    /// (Hyprland's `tag = +name`). Every matching rule's list accumulates
    /// rather than last-wins, since tags are naturally additive -- see
    /// `Config::resolve_window_rules`. There is no rule-level tag removal;
    /// `untag-window` is the only way to remove one, keeping this
    /// deliberately narrow to what the task actually needs.
    pub add_tag: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlassMode {
    Water,
    Frost,
    Plain,
}

/// Resolved compositor opacity multipliers for one window. A missing entry
/// means the window stays at its client-provided opacity.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WindowOpacity {
    pub base: Option<f32>,
    pub active: Option<f32>,
    pub inactive: Option<f32>,
    pub fullscreen: Option<f32>,
}

impl WindowOpacity {
    pub fn from_rule(rule: &WindowRule) -> Option<Self> {
        let opacity = Self {
            base: rule.opacity,
            active: rule.active_opacity,
            inactive: rule.inactive_opacity,
            fullscreen: rule.fullscreen_opacity,
        };
        (opacity.base.is_some()
            || opacity.active.is_some()
            || opacity.inactive.is_some()
            || opacity.fullscreen.is_some())
        .then_some(opacity)
    }

    pub fn alpha(self, focused: bool, fullscreen: bool) -> f32 {
        let state = if fullscreen {
            self.fullscreen
        } else if focused {
            self.active
        } else {
            self.inactive
        };
        (self.base.unwrap_or(1.0) * state.unwrap_or(1.0)).clamp(0.0, 1.0)
    }
}

impl WindowRule {
    /// A rule with no identifying criterion at all never matches anything,
    /// rather than silently matching every window -- a blank rule is far
    /// more likely to be a config mistake than an intentional "match all".
    pub(crate) fn matches(
        &self,
        app_id: Option<&str>,
        title: Option<&str>,
        pid: Option<i32>,
        is_xwayland: bool,
        urgent: bool,
    ) -> bool {
        if self.app_id.is_none()
            && self.title.is_none()
            && self.app_id_regex.is_none()
            && self.title_regex.is_none()
            && self.pid.is_none()
            && self.is_xwayland.is_none()
            && self.urgent.is_none()
        {
            return false;
        }
        if let Some(want) = &self.app_id {
            let Some(app_id) = app_id else { return false };
            // Case-insensitive: app_ids are conventionally lowercase but
            // real clients vary (and xwayland-satellite synthesizes them
            // from WM_CLASS, which is free-form). A rule written as `mpv`
            // silently missing a client reporting `MPV` is a footgun.
            if !app_id.eq_ignore_ascii_case(want) {
                return false;
            }
        }
        if let Some(want) = &self.title {
            let Some(title) = title else { return false };
            if !title.to_lowercase().contains(&want.to_lowercase()) {
                return false;
            }
        }
        if let Some(pattern) = &self.app_id_regex {
            let Some(app_id) = app_id else { return false };
            if !pattern.is_match(app_id) {
                return false;
            }
        }
        if let Some(pattern) = &self.title_regex {
            let Some(title) = title else { return false };
            if !pattern.is_match(title) {
                return false;
            }
        }
        if let Some(want) = self.pid {
            if pid != Some(want) {
                return false;
            }
        }
        if let Some(want) = self.is_xwayland {
            if want != is_xwayland {
                return false;
            }
        }
        if let Some(want) = self.urgent {
            if want != urgent {
                return false;
            }
        }
        true
    }

    /// The spawn-time-only criteria (`initial_class`/`initial_title`,
    /// `at_startup`), split out from `matches` so the facts a rule needs at
    /// ordinary live-resolve time (app_id/title/pid/xwayland/urgent) don't
    /// change shape -- keeps every existing `matches` test call site working
    /// unchanged. `matches`'s own blank-rule guard only looks at the live
    /// fields, so a rule made of only these criteria correctly fails
    /// `matches` on its own; `resolve_window_rules` is what combines this
    /// with `has_initial_criteria`/`has_live_criteria` to decide whether a
    /// rule applies at all. See those two and the blank-rule test above.
    pub(crate) fn matches_initial(
        &self,
        initial_app_id: Option<&str>,
        initial_title: Option<&str>,
        at_startup: bool,
    ) -> bool {
        if let Some(want) = &self.initial_class {
            let Some(app_id) = initial_app_id else {
                return false;
            };
            if !app_id.eq_ignore_ascii_case(want) {
                return false;
            }
        }
        if let Some(want) = &self.initial_title {
            let Some(title) = initial_title else {
                return false;
            };
            if !title.to_lowercase().contains(&want.to_lowercase()) {
                return false;
            }
        }
        if let Some(pattern) = &self.initial_class_regex {
            let Some(app_id) = initial_app_id else {
                return false;
            };
            if !pattern.is_match(app_id) {
                return false;
            }
        }
        if let Some(pattern) = &self.initial_title_regex {
            let Some(title) = initial_title else {
                return false;
            };
            if !pattern.is_match(title) {
                return false;
            }
        }
        if let Some(want) = self.at_startup {
            if want != at_startup {
                return false;
            }
        }
        true
    }

    /// Whether any live-resolve-time criterion (app_id/title/pid/xwayland/
    /// urgent, or their regex variants) is set. Mirrors `matches`'s own
    /// blank-rule guard; `resolve_window_rules` uses this to tell a rule
    /// using only spawn-time criteria apart from a genuinely empty one,
    /// since `matches` itself has no way to vacuously accept every window.
    pub(super) fn has_live_criteria(&self) -> bool {
        self.app_id.is_some()
            || self.title.is_some()
            || self.app_id_regex.is_some()
            || self.title_regex.is_some()
            || self.pid.is_some()
            || self.is_xwayland.is_some()
            || self.urgent.is_some()
    }

    /// Same idea as `has_live_criteria` but for the spawn-time-only fields
    /// `matches_initial` checks.
    pub(super) fn has_initial_criteria(&self) -> bool {
        self.initial_class.is_some()
            || self.initial_title.is_some()
            || self.initial_class_regex.is_some()
            || self.initial_title_regex.is_some()
            || self.at_startup.is_some()
    }

    /// Same idea as `has_live_criteria`/`has_initial_criteria`, gating
    /// `matches_tags`.
    pub(super) fn has_tag_criteria(&self) -> bool {
        !self.tag.is_empty()
    }

    /// Live tag-set match (the `tag` field): whether the window's CURRENT
    /// tag set contains any tag this rule lists. Split out from `matches`
    /// for the same reason `matches_initial` is its own method -- so
    /// `matches`'s five-argument shape, and every existing direct-call
    /// test site, stays unchanged. Unlike `matches_initial`, this
    /// re-checks live state on every resolve rather than a spawn-time
    /// snapshot: a tag added mid-session (`tag-window`/`untag-window`, or
    /// another rule's `add_tag`) takes effect on the very next resolve,
    /// the same "live" character `urgent` already has. See
    /// `Smallvil::resolve_window_rules_for`.
    pub(crate) fn matches_tags(&self, tags: Option<&HashSet<String>>) -> bool {
        tags.is_some_and(|tags| self.tag.iter().any(|t| tags.contains(t)))
    }

    /// Same idea as `has_tag_criteria`, gating `matches_on_workspace`.
    pub(super) fn has_on_workspace_criteria(&self) -> bool {
        self.on_workspace.is_some()
    }

    /// Live current-workspace match (the `on_workspace` field): whether the
    /// window's CURRENT workspace number equals this rule's. `None` (Ocean,
    /// or an untracked window) never matches. Split out from `matches` for
    /// the same reason `matches_tags` is its own method.
    pub(crate) fn matches_on_workspace(&self, workspace: Option<u32>) -> bool {
        self.on_workspace.is_some() && self.on_workspace == workspace
    }
}

/// Matches a layer-shell surface by its `namespace` (the string a client
/// passes to `get_layer_surface`, e.g. a bar setting `"waybar"`) -- a layer
/// surface has no app_id/title the way an xdg toplevel does, so namespace
/// is the only thing to match on. Effects today, Hyprland `layerrule`
/// parity items from the 2026-08-06 config audit:
/// - `block_capture`: opt a sensitive layer surface (a password-manager
///   quick-access panel, say) out of screenshots/screencasts without
///   hiding it from the user's own screen -- niri's
///   `layer-rule { block-out-from ... }`. See
///   `Smallvil::render_one_capture`'s excluded-rect pass.
/// - `z_order`: reorders a namespace within its own wlr layer stratum
///   (Background/Bottom/Top/Overlay) instead of the client's natural
///   stacking order -- Hyprland's `order`. Higher sorts more to the front.
/// - `dim_around` / `dim_amount`: while a matching layer surface on the
///   Overlay or Top stratum is mapped, dim everything rendered behind it
///   (lower layers, the wallpaper, and every non-fullscreen window) --
///   Hyprland's `dimaround`. `dim_amount` (0.0-1.0) is the overlay alpha;
///   unset defaults to a fixed value at the resolution site.
/// - `above_lock_screen`: keep a matching layer surface rendered on top
///   of the session-lock surface instead of being blanked with everything
///   else -- Hyprland's `abovelock`. Deliberately render-only: `surface_under`
///   never hit-tests anything but the lock surface itself while locked, so
///   this cannot be used to route input past the lock.
/// - `blur`: frost-glass the matched surface's own backdrop -- Hyprland's
///   `layerrule = blur`. Reuses the same captured-backdrop pipeline R2's
///   window frost already built (`Smallvil::capture_layer_backdrops`,
///   `layer_glass_elements`), keyed into the same `backdrop_textures` map
///   windows use since the key type (`WlSurface`) and namespaces never
///   collide. Deliberately gated on `frost.enabled`, not `water_effects`:
///   this is general decoration, the same call the shadow/rounding/border
///   precedent already makes, not part of the water identity. Always
///   frost -- there's no water-refraction mode for a bar, matching
///   Hyprland's own single "blur" effect rather than TideWM windows'
///   water/frost choice. `ignore_alpha` optionally masks frost with a
///   native alpha capture of the complete layer surface tree, discarding
///   blur where the composed client alpha is below the configured threshold.
#[derive(Debug, Clone, Default)]
pub struct LayerRule {
    pub namespace: Option<String>,
    pub block_capture: bool,
    pub z_order: Option<i32>,
    pub dim_around: bool,
    pub dim_amount: Option<f32>,
    pub above_lock_screen: bool,
    pub blur: bool,
    pub ignore_alpha: Option<f32>,
    /// Custom effect for the matched layer, or `none` to clear an earlier
    /// match. Layers never inherit a window or workspace shader.
    pub shader: Option<ShaderAssignment>,
}

impl LayerRule {
    /// A rule with no `namespace` never matches, same "blank rule matches
    /// nothing" precedent `WindowRule::matches` sets.
    pub(crate) fn matches(&self, namespace: &str) -> bool {
        match &self.namespace {
            Some(want) => namespace.contains(want.as_str()),
            None => false,
        }
    }
}

/// Per-workspace overrides, matched by raw workspace number (numeric-only
/// for now -- `workspace_name` aliases are a possible follow-up, not
/// wired here). Hyprland's `workspace_rule` block is the closest analog;
/// TideWM's numbered-per-output model has no discrete create/destroy
/// lifecycle to hang Hyprland's `default_name` on (`workspace_name`
/// already covers naming a workspace, unconditionally, so it isn't
/// duplicated here), which leaves the remaining pieces this covers:
/// appearance overrides, a default layout algorithm, and
/// `on_created_empty`. Applies to the workspace number on any output --
/// same convention `workspace_gaps` already established, not a second
/// addressing scheme.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceRule {
    pub workspace: Option<u32>,
    /// Default tiling algorithm for this workspace. Loses to an explicit
    /// runtime `layout:<algo>` action on the same (output, workspace) --
    /// see `Layouts::algorithm`'s fallback chain -- since that's the user
    /// actively choosing something for *this* output's instance of the
    /// workspace, not the workspace's own resting default.
    pub layout: Option<LayoutAlgorithm>,
    /// Layered as a base underneath any matching `rule { }`
    /// override for a window that happens to live on this workspace (see
    /// `Smallvil::border_config_for_surface`), not merged field-by-field
    /// against other matching `workspace_rule` blocks -- only one
    /// `workspace_rule` block is expected to name a given workspace
    /// number, so last-match-wins is the whole story.
    pub border: Option<BorderOverrides>,
    pub rounding: Option<RoundingOverrides>,
    pub shadow: Option<ShadowOverrides>,
    /// Per-workspace snap opt-in or opt-out. Window rules take precedence.
    pub snap: Option<bool>,
    /// Default custom effect for windows on this workspace. A window rule's
    /// own `shader` (including `none`) takes precedence.
    pub shader: Option<ShaderAssignment>,
    /// Command run the first time this workspace becomes active while
    /// having zero windows, once per (output, workspace) pair for the
    /// process lifetime (`WorkspaceState::workspace_created_empty_fired`) --
    /// the closest honest analog to Hyprland's real create/destroy
    /// lifecycle event given TideWM's workspaces always exist as
    /// addressable numbers rather than being created on first reference.
    /// Does not repeat on every later empty-workspace visit.
    pub on_created_empty: Option<String>,
}

pub(super) fn lower_window_rule_block(body: &[waves::Entry]) -> WindowRule {
    let mut rule = WindowRule::default();
    for entry in body {
        match entry {
            waves::Entry::Assign(key, value) => match key.as_str() {
                "app_id" => rule.app_id = Some(value.clone()),
                "title" => rule.title = Some(value.clone()),
                "app_id_regex" => match regex::Regex::new(value) {
                    Ok(pattern) => rule.app_id_regex = Some(pattern),
                    Err(err) => tracing::warn!(value, %err, "Invalid app_id_regex, ignoring"),
                },
                "title_regex" => match regex::Regex::new(value) {
                    Ok(pattern) => rule.title_regex = Some(pattern),
                    Err(err) => tracing::warn!(value, %err, "Invalid title_regex, ignoring"),
                },
                "pid" => match value.parse() {
                    Ok(n) => rule.pid = Some(n),
                    Err(_) => tracing::warn!(value, "Expected an integer PID, ignoring"),
                },
                "xwayland" | "is_xwayland" => set_opt_bool(&mut rule.is_xwayland, key, value),
                "urgent" => set_opt_bool(&mut rule.urgent, key, value),
                "initial_class" => rule.initial_class = Some(value.clone()),
                "initial_title" => rule.initial_title = Some(value.clone()),
                "initial_class_regex" => match regex::Regex::new(value) {
                    Ok(pattern) => rule.initial_class_regex = Some(pattern),
                    Err(err) => {
                        tracing::warn!(value, %err, "Invalid initial_class_regex, ignoring")
                    }
                },
                "initial_title_regex" => match regex::Regex::new(value) {
                    Ok(pattern) => rule.initial_title_regex = Some(pattern),
                    Err(err) => {
                        tracing::warn!(value, %err, "Invalid initial_title_regex, ignoring")
                    }
                },
                "at_startup" => set_opt_bool(&mut rule.at_startup, key, value),
                "workspace" => match value.parse() {
                    Ok(n) => rule.workspace = Some(n),
                    Err(_) => tracing::warn!(value, "Expected a workspace number, ignoring"),
                },
                "on_workspace" => match value.parse() {
                    Ok(n) => rule.on_workspace = Some(n),
                    Err(_) => tracing::warn!(value, "Expected an on_workspace number, ignoring"),
                },
                "output" => rule.output = Some(value.clone()),
                "pin_output" => rule.pin_output = Some(value.clone()),
                "float" => set_bool(&mut rule.float, key, value),
                "pseudo_tile" => set_bool(&mut rule.pseudo_tile, key, value),
                "pin" => set_bool(&mut rule.pin, key, value),
                "tile" => set_bool(&mut rule.tile, key, value),
                "snap" => set_opt_bool(&mut rule.snap, key, value),
                "no_focus" => set_bool(&mut rule.no_focus, key, value),
                "maximize" => set_bool(&mut rule.maximize, key, value),
                "fullscreen" => set_bool(&mut rule.fullscreen, key, value),
                "block_capture" => set_bool(&mut rule.block_capture, key, value),
                "swallow" => set_bool(&mut rule.swallow, key, value),
                "opacity" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => rule.opacity = Some(value.clamp(0.0, 1.0)),
                    _ => {
                        tracing::warn!(value, "Expected a finite opacity from 0.0 to 1.0, ignoring")
                    }
                },
                "active_opacity" | "focused_opacity" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => {
                        rule.active_opacity = Some(value.clamp(0.0, 1.0))
                    }
                    _ => tracing::warn!(value, "Expected active_opacity from 0.0 to 1.0, ignoring"),
                },
                "inactive_opacity" | "unfocused_opacity" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => {
                        rule.inactive_opacity = Some(value.clamp(0.0, 1.0))
                    }
                    _ => {
                        tracing::warn!(value, "Expected inactive_opacity from 0.0 to 1.0, ignoring")
                    }
                },
                "fullscreen_opacity" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => {
                        rule.fullscreen_opacity = Some(value.clamp(0.0, 1.0))
                    }
                    _ => tracing::warn!(
                        value,
                        "Expected fullscreen_opacity from 0.0 to 1.0, ignoring"
                    ),
                },
                "glass" | "glass_mode" => match value.as_str() {
                    "water" => rule.glass = Some(GlassMode::Water),
                    "frost" => rule.glass = Some(GlassMode::Frost),
                    "none" | "plain" => rule.glass = Some(GlassMode::Plain),
                    _ => tracing::warn!(
                        value,
                        "Expected a rule glass mode: water frost none, ignoring"
                    ),
                },
                "shader" => match ShaderAssignment::parse(value) {
                    Some(assignment) => rule.shader = Some(assignment),
                    None => {
                        tracing::warn!(value, "Expected a shader definition name or none, ignoring")
                    }
                },
                "viscosity" => match parse_viscosity(value) {
                    Some(value) => rule.viscosity = Some(value),
                    None => {
                        tracing::warn!(value, "Expected rule viscosity from 0.0 to 4.0, ignoring")
                    }
                },
                "sway" => set_opt_bool(&mut rule.sway, key, value),
                "float_physics" => match value.as_str() {
                    "off" => rule.float_physics = Some(FloatPhysicsTier::Off),
                    "light" => rule.float_physics = Some(FloatPhysicsTier::Light),
                    "full" => rule.float_physics = Some(FloatPhysicsTier::Full),
                    _ => tracing::warn!(
                        value,
                        "Expected a rule float_physics tier: off light full, ignoring"
                    ),
                },
                "weight" => match value.parse::<f32>() {
                    Ok(value) if value.is_finite() => rule.weight = Some(value.clamp(0.0, 1.0)),
                    _ => tracing::warn!(value, "Expected rule weight from 0 to 1, ignoring"),
                },
                "ebb" => match value.as_str() {
                    "off" => rule.ebb = Some(super::EbbMode::Off),
                    "low" => rule.ebb = Some(super::EbbMode::Low),
                    "balanced" => rule.ebb = Some(super::EbbMode::Balanced),
                    "aggressive" => rule.ebb = Some(super::EbbMode::Aggressive),
                    _ => tracing::warn!(value, "Unknown Ebb rule mode, ignoring"),
                },
                "depth" => set_opt_bool(&mut rule.depth, key, value),
                "shadow" => match value.as_str() {
                    "true" | "on" => {
                        rule.shadow
                            .get_or_insert_with(ShadowOverrides::default)
                            .enabled = Some(true);
                    }
                    "false" | "off" | "none" => {
                        rule.shadow
                            .get_or_insert_with(ShadowOverrides::default)
                            .enabled = Some(false);
                    }
                    _ => tracing::warn!(
                        value,
                        "Expected rule shadow to be true/false/on/off/none, ignoring"
                    ),
                },
                "rounding" => {
                    let overrides = rule.rounding.get_or_insert_with(RoundingOverrides::default);
                    match value.trim().to_ascii_lowercase().as_str() {
                        "true" | "on" => overrides.enabled = Some(true),
                        "false" | "off" | "none" => overrides.enabled = Some(false),
                        _ => match parse_corner_radii(value) {
                            Some([tl, tr, br, bl]) => {
                                overrides.enabled = Some(true);
                                overrides.top_left = Some(tl);
                                overrides.top_right = Some(tr);
                                overrides.bottom_right = Some(br);
                                overrides.bottom_left = Some(bl);
                            }
                            None => tracing::warn!(
                                value,
                                "Expected rule rounding on|off or one/four radii, ignoring"
                            ),
                        },
                    }
                }
                "clip_to_geometry" | "clip-to-geometry" => match parse_bool(value) {
                    Some(value) => {
                        rule.rounding
                            .get_or_insert_with(RoundingOverrides::default)
                            .clip = Some(value);
                    }
                    None => tracing::warn!(value, "Expected clip_to_geometry boolean, ignoring"),
                },
                "border" => match value.as_str() {
                    "true" | "on" => {
                        rule.border
                            .get_or_insert_with(BorderOverrides::default)
                            .enabled = Some(true);
                    }
                    "false" | "off" | "none" => {
                        rule.border
                            .get_or_insert_with(BorderOverrides::default)
                            .enabled = Some(false);
                    }
                    _ => tracing::warn!(
                        value,
                        "Expected rule border true/false/on/off/none, ignoring"
                    ),
                },
                "position" => match parse_position(value) {
                    Some(pos) => rule.position = Some(pos),
                    None => {
                        tracing::warn!(value, "Expected <x>x<y> for a rule's position, ignoring")
                    }
                },
                "size" => match parse_position(value) {
                    Some(size) => rule.size = Some(size),
                    None => tracing::warn!(
                        value,
                        "Expected <width>x<height> for a rule's size, ignoring"
                    ),
                },
                "persistent_size" => set_bool(&mut rule.persistent_size, key, value),
                "min_width" => match value.parse::<i32>() {
                    Ok(n) if n > 0 => rule.min_width = Some(n),
                    _ => tracing::warn!(value, "Expected a positive min_width, ignoring"),
                },
                "max_width" => match value.parse::<i32>() {
                    Ok(n) if n > 0 => rule.max_width = Some(n),
                    _ => tracing::warn!(value, "Expected a positive max_width, ignoring"),
                },
                "min_height" => match value.parse::<i32>() {
                    Ok(n) if n > 0 => rule.min_height = Some(n),
                    _ => tracing::warn!(value, "Expected a positive min_height, ignoring"),
                },
                "max_height" => match value.parse::<i32>() {
                    Ok(n) if n > 0 => rule.max_height = Some(n),
                    _ => tracing::warn!(value, "Expected a positive max_height, ignoring"),
                },
                "scroll_factor" => match value.parse::<f64>() {
                    Ok(n) if n.is_finite() && n >= 0.0 => rule.scroll_factor = Some(n),
                    _ => tracing::warn!(value, "Expected a non-negative scroll_factor, ignoring"),
                },
                // List-shaped: `tag = ["work", "browser"]` on one line, or
                // repeated `tag = "work"` lines both accumulate, same dual
                // form the top-level `spawn` key supports.
                "tag" => match parse_list_value(value) {
                    Some(items) => rule.tag.extend(items),
                    None if value.trim().starts_with('[') => {
                        tracing::warn!("Expected a non-empty rule tag list, ignoring")
                    }
                    None if !value.trim().is_empty() => rule.tag.push(value.trim().to_string()),
                    None => tracing::warn!("Expected a non-empty rule tag, ignoring"),
                },
                "add_tag" => match parse_list_value(value) {
                    Some(items) => rule.add_tag.extend(items),
                    None if value.trim().starts_with('[') => {
                        tracing::warn!("Expected a non-empty rule add_tag list, ignoring")
                    }
                    None if !value.trim().is_empty() => rule.add_tag.push(value.trim().to_string()),
                    None => tracing::warn!("Expected a non-empty rule add_tag, ignoring"),
                },
                "ripple" if value == "none" => {
                    // Shorthand for a rule that matches the window but
                    // suppresses any ripple on it. Equivalent to a full
                    // `ripple { enabled = false }` sub-block.
                    rule.ripple = Some(RippleConfig {
                        enabled: Some(false),
                        ..Default::default()
                    });
                }
                other => tracing::warn!(key = %other, "Unknown key in `rule` block, ignoring"),
            },
            waves::Entry::Block(keyword, _, ripple_body) if keyword == "ripple" => {
                // Per-app ripple overrides. Start from a fresh empty
                // `RippleConfig` (every field `None`) so unset fields
                // inherit the global `ripple { }` block via `merge_over`.
                let mut overrides = RippleConfig::default();
                apply_ripple_block(&mut overrides, ripple_body);
                rule.ripple = Some(overrides);
            }
            waves::Entry::Block(keyword, _, frost_body) if keyword == "frost" => {
                let mut overrides = FrostOverrides::default();
                apply_frost_override_block(&mut overrides, frost_body);
                rule.frost = Some(overrides);
            }
            waves::Entry::Block(keyword, _, shadow_body) if keyword == "shadow" => {
                let mut overrides = ShadowOverrides::default();
                apply_shadow_override_block(&mut overrides, shadow_body);
                rule.shadow = Some(overrides);
            }
            waves::Entry::Block(keyword, _, rounding_body)
                if keyword == "rounding" || keyword == "corners" =>
            {
                let mut overrides = RoundingOverrides::default();
                apply_rounding_override_block(&mut overrides, rounding_body);
                rule.rounding = Some(overrides);
            }
            waves::Entry::Block(keyword, _, border_body) if keyword == "border" => {
                let mut overrides = BorderOverrides::default();
                apply_border_override_block(&mut overrides, border_body);
                rule.border = Some(overrides);
            }
            _ => tracing::warn!("Unexpected entry in `rule` block, ignoring"),
        }
    }
    rule
}

pub(super) fn lower_workspace_rule_block(body: &[waves::Entry]) -> WorkspaceRule {
    let mut rule = WorkspaceRule::default();
    for entry in body {
        match entry {
            waves::Entry::Assign(key, value) => match key.as_str() {
                "workspace" => match value.parse() {
                    Ok(n) => rule.workspace = Some(n),
                    Err(_) => tracing::warn!(value, "Expected a workspace number, ignoring"),
                },
                "layout" => match parse_layout_algorithm(value) {
                    Some(algo) => rule.layout = Some(algo),
                    None => tracing::warn!(
                        value,
                        "Expected a workspace_rule layout: bsp master cascade, ignoring"
                    ),
                },
                "on_created_empty" => rule.on_created_empty = Some(value.clone()),
                "snap" => set_opt_bool(&mut rule.snap, key, value),
                "shader" => match ShaderAssignment::parse(value) {
                    Some(assignment) => rule.shader = Some(assignment),
                    None => {
                        tracing::warn!(value, "Expected a shader definition name or none, ignoring")
                    }
                },
                "shadow" => match value.as_str() {
                    "true" | "on" => {
                        rule.shadow
                            .get_or_insert_with(ShadowOverrides::default)
                            .enabled = Some(true);
                    }
                    "false" | "off" | "none" => {
                        rule.shadow
                            .get_or_insert_with(ShadowOverrides::default)
                            .enabled = Some(false);
                    }
                    _ => tracing::warn!(
                        value,
                        "Expected workspace_rule shadow to be true/false/on/off/none, ignoring"
                    ),
                },
                "rounding" => {
                    let overrides = rule.rounding.get_or_insert_with(RoundingOverrides::default);
                    match value.trim().to_ascii_lowercase().as_str() {
                        "true" | "on" => overrides.enabled = Some(true),
                        "false" | "off" | "none" => overrides.enabled = Some(false),
                        _ => match parse_corner_radii(value) {
                            Some([tl, tr, br, bl]) => {
                                overrides.enabled = Some(true);
                                overrides.top_left = Some(tl);
                                overrides.top_right = Some(tr);
                                overrides.bottom_right = Some(br);
                                overrides.bottom_left = Some(bl);
                            }
                            None => tracing::warn!(
                                value,
                                "Expected workspace_rule rounding on|off or one/four radii, ignoring"
                            ),
                        },
                    }
                }
                "border" => match value.as_str() {
                    "true" | "on" => {
                        rule.border
                            .get_or_insert_with(BorderOverrides::default)
                            .enabled = Some(true);
                    }
                    "false" | "off" | "none" => {
                        rule.border
                            .get_or_insert_with(BorderOverrides::default)
                            .enabled = Some(false);
                    }
                    _ => tracing::warn!(
                        value,
                        "Expected workspace_rule border true/false/on/off/none, ignoring"
                    ),
                },
                other => {
                    tracing::warn!(key = %other, "Unknown key in `workspace_rule` block, ignoring")
                }
            },
            waves::Entry::Block(keyword, _, shadow_body) if keyword == "shadow" => {
                let mut overrides = ShadowOverrides::default();
                apply_shadow_override_block(&mut overrides, shadow_body);
                rule.shadow = Some(overrides);
            }
            waves::Entry::Block(keyword, _, rounding_body)
                if keyword == "rounding" || keyword == "corners" =>
            {
                let mut overrides = RoundingOverrides::default();
                apply_rounding_override_block(&mut overrides, rounding_body);
                rule.rounding = Some(overrides);
            }
            waves::Entry::Block(keyword, _, border_body) if keyword == "border" => {
                let mut overrides = BorderOverrides::default();
                apply_border_override_block(&mut overrides, border_body);
                rule.border = Some(overrides);
            }
            _ => tracing::warn!("Unexpected entry in `workspace_rule` block, ignoring"),
        }
    }
    rule
}

pub(super) fn lower_layer_rule_block(body: &[waves::Entry]) -> LayerRule {
    let mut rule = LayerRule::default();
    for entry in body {
        let waves::Entry::Assign(key, value) = entry else {
            tracing::warn!("Unexpected entry in `layer_rule` block, ignoring");
            continue;
        };
        match key.as_str() {
            "namespace" => rule.namespace = Some(value.clone()),
            "block_capture" => set_bool(&mut rule.block_capture, key, value),
            "z_order" => match value.parse::<i32>() {
                Ok(value) => rule.z_order = Some(value),
                _ => tracing::warn!(value, "Expected layer_rule.z_order as an integer, ignoring"),
            },
            "dim_around" => set_bool(&mut rule.dim_around, key, value),
            "dim_amount" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => rule.dim_amount = Some(value.clamp(0.0, 1.0)),
                _ => tracing::warn!(
                    value,
                    "Expected layer_rule.dim_amount from 0.0 to 1.0, ignoring"
                ),
            },
            "above_lock_screen" => set_bool(&mut rule.above_lock_screen, key, value),
            "blur" => set_bool(&mut rule.blur, key, value),
            "shader" => match ShaderAssignment::parse(value) {
                Some(assignment) => rule.shader = Some(assignment),
                None => {
                    tracing::warn!(value, "Expected a shader definition name or none, ignoring")
                }
            },
            "ignore_alpha" => match value.parse::<f32>() {
                Ok(value) if value.is_finite() => {
                    rule.ignore_alpha = Some(value.clamp(0.0, 1.0));
                }
                _ => tracing::warn!(
                    value,
                    "Expected layer_rule.ignore_alpha from 0.0 to 1.0, ignoring"
                ),
            },
            other => tracing::warn!(key = %other, "Unknown key in `layer_rule` block, ignoring"),
        }
    }
    rule
}
