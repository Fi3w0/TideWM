//! TideWM's dynamic BSP, master, and cascade tiling layouts.
//!
//! The BSP layout follows the spirit of
//! Hyprland's "dwindle": each new window splits the target leaf in two, and
//! split orientation is decided by the leaf's own aspect ratio at layout
//! time (wide leaf -> side-by-side, tall leaf -> stacked), not baked into
//! the tree. Removing a window collapses its sibling back up.
//!
//! This module only computes geometry; applying it (sending configures,
//! moving elements in `Space`) is the caller's job (see `Smallvil::retile`).
//!
//! Each output gets its own independent `BspLayout` tree (see `Layouts`):
//! monitors don't share a tiling space, matching every other tiling WM's
//! convention. Trees are keyed by output name (a stable `String`, not the
//! `Output` type itself) so this module stays decoupled from Wayland/Smithay
//! desktop-state types beyond what it already touches.

use std::collections::HashMap;

use smithay::{
    desktop::Window,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle},
};

use crate::config::{LayoutAlgorithm, MasterOrientation, SplitBias};

enum Node {
    Leaf(Window),
    Split {
        /// Fraction of the split's area given to `first`.
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// Which child a `hit_test_split` path descends into at each `Split`, so
/// the same split can be found again later to mutate its ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    First,
    Second,
}

/// Which pair of edges a split's shared border runs along, matching the
/// side-by-side-vs-stacked choice `split()` makes from the area's own
/// aspect ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// A split boundary found under a point, addressed by the path of
/// `Side`s from the root that reaches it. `area` is that split's own
/// (pre-gap, pre-ratio) area, fixed at hit-test time so ratio math during
/// a drag stays stable even though the area itself derives from the ratio
/// being dragged. `output`/`workspace` identify which tree this boundary
/// belongs to, so a caller holding onto a `SplitHit` (e.g. across a drag)
/// doesn't need to re-derive it later.
#[derive(Debug, Clone)]
pub struct SplitHit {
    pub output: String,
    pub workspace: u32,
    pub path: Vec<Side>,
    pub axis: Axis,
    pub area: Rectangle<i32, Logical>,
    /// Full tiling area at grab start. Connected-vessel propagation uses
    /// this to recover each ancestor split's stable span.
    root_area: Rectangle<i32, Logical>,
    /// Which child contains the resized window. Set for body/keyboard
    /// resize, absent for a directly dragged split border.
    target_side: Option<Side>,
    topology_revision: u64,
}

/// One split participating in a connected-vessel resize. `weight` includes
/// both spatial falloff and the sign needed to grow the target leaf when it
/// sits in this split's second child.
#[derive(Debug, Clone)]
pub(crate) struct SplitResizeHandle {
    pub(crate) hit: SplitHit,
    start_ratio: f32,
    weight: f32,
}

impl SplitResizeHandle {
    pub(crate) fn ratio_for_delta(&self, delta_pixels: f64) -> Option<f32> {
        let span = match self.hit.axis {
            Axis::Horizontal => self.hit.area.size.w,
            Axis::Vertical => self.hit.area.size.h,
        };
        (span > 0).then(|| self.start_ratio + (delta_pixels as f32 * self.weight) / span as f32)
    }
}

#[derive(Default)]
pub struct BspLayout {
    root: Option<Node>,
    /// Number of leaves in `root`. Keeping this beside the tree avoids
    /// allocating a cloned `Vec<Window>` just to answer routine occupancy
    /// and count queries.
    len: usize,
}

impl BspLayout {
    pub(crate) fn is_empty(&self) -> bool {
        debug_assert_eq!(self.root.is_none(), self.len == 0);
        self.len == 0
    }

    pub(crate) fn len(&self) -> usize {
        debug_assert_eq!(self.root.is_none(), self.len == 0);
        self.len
    }

    /// Inserts `window`, splitting the leaf containing `target` (usually the
    /// currently focused surface). If `target` isn't part of the tree (or is
    /// `None`, e.g. nothing focused yet), falls back to splitting the
    /// last leaf in tree order, so windows still stack predictably.
    pub fn insert(&mut self, window: Window, target: Option<&WlSurface>) {
        self.root = Some(match self.root.take() {
            None => Node::Leaf(window),
            Some(root) => insert_into(root, window, target),
        });
        self.len += 1;
    }

    /// Removes the window backed by `surface`, collapsing its sibling up
    /// into its former parent's place. No-op if `surface` isn't tiled.
    pub fn remove(&mut self, surface: &WlSurface) -> bool {
        let Some(root) = self.root.take() else {
            return false;
        };
        let (root, removed) = remove_from(root, surface);
        self.root = root;
        if removed {
            self.len -= 1;
        }
        debug_assert_eq!(self.root.is_none(), self.len == 0);
        removed
    }

    pub fn contains(&self, surface: &WlSurface) -> bool {
        self.root
            .as_ref()
            .is_some_and(|root| node_contains(root, surface))
    }

    /// The `Window` in this tree backing `surface`, if any.
    pub fn window(&self, surface: &WlSurface) -> Option<Window> {
        find_window(self.root.as_ref()?, surface)
    }

    /// Every window in the tree, in tree order. Used to find which windows
    /// need hiding/showing on a workspace switch (`Smallvil::switch_workspace`),
    /// where only the `Window`s themselves matter, not their geometry.
    pub fn windows(&self) -> Vec<Window> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            collect_windows(root, &mut out);
        }
        out
    }

    /// Swaps the windows backed by `a` and `b` in place: their positions in
    /// the tree (and so their slots in the layout) don't move, just which
    /// `Window` each leaf holds. No-op if either surface isn't tiled.
    pub fn swap(&mut self, a: &WlSurface, b: &WlSurface) {
        let Some(root) = &mut self.root else { return };
        let (Some(window_a), Some(window_b)) = (find_window(root, a), find_window(root, b)) else {
            return;
        };
        swap_leaves(root, a, &window_a, b, &window_b);
    }

    /// Replaces whichever `Window` occupies `old`'s leaf with `new_window`,
    /// without otherwise touching tree structure -- unlike `swap`, the
    /// replacement doesn't need to already be tiled anywhere itself. Used to
    /// swap which tab of a window group is visible in its shared slot (see
    /// `Smallvil::cycle_tab`/`ungroup`). No-op if `old` isn't tiled here.
    pub fn replace_leaf(&mut self, old: &WlSurface, new_window: &Window) -> bool {
        self.root
            .as_mut()
            .is_some_and(|root| replace_leaf(root, old, new_window))
    }

    /// The on-screen rectangle for every tiled window, gaps already applied.
    pub fn layout(
        &self,
        area: Rectangle<i32, Logical>,
        gap: i32,
        bias: SplitBias,
    ) -> Vec<(Window, Rectangle<i32, Logical>)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            collect(root, area, bias, &mut out);
        }
        for (_, rect) in &mut out {
            *rect = inset(*rect, gap);
        }
        out
    }

    /// Finds the split boundary nearest `point`, if `point` falls within
    /// `gap` (or 4px, whichever is larger) of one. Boundaries are checked
    /// from the root down, and the walk follows only whichever child's area
    /// contains `point`, so this always finds the *closest*
    /// enclosing boundary rather than some arbitrary ancestor's.
    pub fn hit_test_split(
        &self,
        area: Rectangle<i32, Logical>,
        gap: i32,
        point: Point<f64, Logical>,
        bias: SplitBias,
    ) -> Option<SplitHit> {
        hit_test(
            self.root.as_ref()?,
            area,
            area,
            gap,
            point,
            bias,
            Vec::new(),
        )
    }

    /// The nearest enclosing split for each axis along the path from the
    /// root down to the leaf backing `target`, so a tiled window can be
    /// resized by dragging its own body -- Hyprland's own convention for
    /// `bindm ... resizewindow` -- instead of needing to hit the shared
    /// border exactly (see `hit_test_split`, the click-precisely variant).
    /// At most one entry per axis: a window split only horizontally so far
    /// yields one entry, one split both ways yields two, and a lone window
    /// filling the whole output yields none. The *nearest* (deepest) split
    /// of each axis wins over any more distant ancestor, since that's the
    /// one whose ratio directly controls this window's size in that axis
    /// rather than resizing some larger subtree it happens to sit in.
    pub fn resize_splits(
        &self,
        target: &WlSurface,
        area: Rectangle<i32, Logical>,
        bias: SplitBias,
    ) -> Vec<SplitHit> {
        let mut found = [None, None];
        if let Some(root) = &self.root {
            if node_contains(root, target) {
                collect_resize_splits(root, target, area, area, bias, &mut found);
            }
        }
        found.into_iter().flatten().collect()
    }

    /// The current ratio of the split addressed by `path` (see `SplitHit`).
    pub fn ratio_at(&self, path: &[Side]) -> Option<f32> {
        ratio_at(self.root.as_ref()?, path)
    }

    /// Sets the ratio of the split addressed by `path`, clamped so neither
    /// side ever collapses to nothing.
    pub fn set_ratio(&mut self, path: &[Side], ratio: f32) {
        if let Some(root) = &mut self.root {
            set_ratio_at(root, path, ratio.clamp(0.05, 0.95));
        }
    }

    /// Resizes the nearest split on `axis` around `target`, interpreting a
    /// positive pixel delta as growing the target leaf regardless of which
    /// side of that split owns it. Used by Ocean keyboard resize, whose reef
    /// trees are `BspLayout`s without Classic's output/workspace registry.
    pub(crate) fn resize_target_by(
        &mut self,
        target: &WlSurface,
        area: Rectangle<i32, Logical>,
        bias: SplitBias,
        axis: Axis,
        delta_pixels: f64,
    ) -> bool {
        let Some(hit) = self
            .resize_splits(target, area, bias)
            .into_iter()
            .find(|hit| hit.axis == axis)
        else {
            return false;
        };
        let span = match axis {
            Axis::Horizontal => hit.area.size.w,
            Axis::Vertical => hit.area.size.h,
        };
        let (Some(current), Some(target_side)) = (self.ratio_at(&hit.path), hit.target_side) else {
            return false;
        };
        if span <= 0 {
            return false;
        }
        let sign = if target_side == Side::First {
            1.0
        } else {
            -1.0
        };
        self.set_ratio(
            &hit.path,
            current + delta_pixels as f32 * sign / span as f32,
        );
        true
    }
}

impl Drop for BspLayout {
    fn drop(&mut self) {
        // `Box<Node>` would otherwise recursively drop a skewed client-built
        // tree. Drain it with an explicit heap stack so compositor shutdown
        // and workspace cleanup have the same bounded call-stack behavior as
        // the runtime walks below.
        let Some(root) = self.root.take() else { return };
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            if let Node::Split { first, second, .. } = node {
                pending.push(*first);
                pending.push(*second);
            }
        }
    }
}

/// One independent `BspLayout` tree per (output, workspace) pair, plus which
/// workspace is currently active (visible) on each output. Workspaces are
/// lazily created, not pre-declared -- a tree springs into existence the
/// first time something is inserted into it, same as `by_output` used to
/// work with just outputs, and an output not yet in `active` defaults to
/// workspace `1` (see `active_workspace`).
///
/// A direct surface-owner index lets operations such as `contains`,
/// `output_of`, `remove`, and `swap` address hidden tiled windows without
/// scanning every workspace. Operations that render or address one specific
/// tree (`insert`, `layout`, `hit_test_split`, `ratio_at`, `set_ratio`) still
/// take `workspace` explicitly.
#[derive(Default)]
pub struct Layouts {
    trees: HashMap<(String, u32), BspLayout>,
    /// Direct surface -> tree ownership. Geometry remains authoritative in
    /// `trees`; this index prevents common membership/output/workspace
    /// queries from walking every workspace tree.
    #[allow(clippy::mutable_key_type)]
    owners: HashMap<WlSurface, (String, u32)>,
    active: HashMap<String, u32>,
    /// Per-(output, workspace) tiling algorithm override; a workspace not
    /// in this map uses `default_algorithm` (seeded from the `default_layout`
    /// config key, see `Smallvil::new`/`reload_config`). Pruned alongside
    /// `trees` in `remove()` for the same reason `trees` itself is pruned
    /// (see that method's comment): switching algorithms is an IPC-reachable
    /// action, so an unbounded map keyed by arbitrary workspace numbers is
    /// the same unchecked-growth shape as the tree registry already guards
    /// against. This does mean toggling a workspace to master, then closing
    /// every window on it, forgets the choice -- a deliberate trade for
    /// bounded memory over remembering a setting for a workspace with
    /// nothing left tiled on it.
    algorithms: HashMap<(String, u32), LayoutAlgorithm>,
    default_algorithm: LayoutAlgorithm,
    /// Per-workspace-number default from `[[workspace_rule]]`, one level
    /// weaker than an explicit `algorithms` entry (a live `layout:<algo>`
    /// action on this specific (output, workspace) is the user actively
    /// choosing something right now, so it should keep winning) but one
    /// level stronger than the global `default_algorithm`. Set wholesale
    /// from `Smallvil::new`/`reload_config`, same as `default_algorithm`
    /// itself -- not pruned per-entry, since it mirrors config state
    /// rather than runtime state.
    workspace_algorithm_overrides: HashMap<u32, LayoutAlgorithm>,
    /// Per-(output, workspace) master/stack split fraction (master mode
    /// only; meaningless, and ignored, under BSP). Defaults to 0.5 for any
    /// key not present. Pruned the same way and for the same reason as
    /// `algorithms`.
    master_ratio: HashMap<(String, u32), f32>,
    /// Per-(output, workspace) manual resize state for cascade layout
    /// (cascade mode only; meaningless, and ignored, under BSP/master).
    /// Populated as a side effect of `layout()`'s cascade branch every time
    /// it resolves a row/cell partition, so a hit-test or resize can always
    /// read the partition the *next* frame will actually render rather than
    /// recomputing it separately and risking drift between the two. Pruned
    /// the same way and for the same reason as `algorithms`/`master_ratio`.
    cascade_state: HashMap<(String, u32), CascadeState>,
    /// Which side the master pane sits on under `LayoutAlgorithm::Master`,
    /// global (not per-workspace, unlike `master_ratio` -- a taste setting,
    /// not something interactively adjusted per split). Set from
    /// `config.master_orientation` at startup and on every reload, same as
    /// `default_algorithm`.
    master_orientation: MasterOrientation,
    /// Manual override for every BSP tree's per-split axis choice (BSP
    /// mode only; ignored under master, which never calls `split()` at
    /// all). Global, same "taste setting, not per-workspace" reasoning as
    /// `master_orientation`. Set from `config.bsp_split_bias` at startup
    /// and on every reload.
    split_bias: SplitBias,
    /// Monotonic identity for the current collection of BSP trees. Pointer
    /// grabs retain the value they started with so a window insertion,
    /// removal, leaf swap, or workspace-content swap cannot make an old
    /// split path silently address an unrelated node.
    topology_revision: u64,
}

impl Layouts {
    /// Whether `surface` is tiled on any (output, workspace) tree, visible
    /// or not.
    pub fn contains(&self, surface: &WlSurface) -> bool {
        self.owners.contains_key(surface)
    }

    /// The name of the output whose tree currently holds `surface`, if any,
    /// regardless of whether that tree's workspace is the active one.
    pub fn output_of(&self, surface: &WlSurface) -> Option<&str> {
        self.owners.get(surface).map(|(name, _)| name.as_str())
    }

    /// The workspace number of whichever tree currently holds `surface`, if
    /// any, regardless of whether that tree's workspace is the active one.
    /// Used to tell "already on the active workspace" apart from "tiled,
    /// but on some other hidden one" -- the two look identical if you only
    /// ever compare against `active_workspace`.
    pub fn workspace_of(&self, surface: &WlSurface) -> Option<u32> {
        self.owners.get(surface).map(|(_, workspace)| *workspace)
    }

    /// The `Window` handle backing `surface`, wherever it's tiled. Unlike
    /// looking it up through `Smallvil::space`, this finds it even while
    /// hidden on a non-active workspace -- a hidden tiled window is still
    /// held by its tree, just not mapped, so `space.elements()` won't have
    /// it anymore (see `Smallvil::switch_workspace`'s own note on this).
    pub fn window_of(&self, surface: &WlSurface) -> Option<Window> {
        self.trees.get(self.owners.get(surface)?)?.window(surface)
    }

    fn key_of(&self, surface: &WlSurface) -> Option<(String, u32)> {
        self.owners.get(surface).cloned()
    }

    /// The workspace currently visible on `output`. Defaults to `1` for an
    /// output that's never had a workspace switch on it.
    pub fn active_workspace(&self, output: &str) -> u32 {
        self.active.get(output).copied().unwrap_or(1)
    }

    /// Marks `workspace` as the visible one on `output`. Purely bookkeeping
    /// -- callers (see `Smallvil::switch_workspace`) are responsible for
    /// actually hiding/showing the right windows around this call.
    pub fn set_active_workspace(&mut self, output: &str, workspace: u32) {
        self.active.insert(output.to_string(), workspace);
    }

    /// Inserts `window` into `output`'s tree for `workspace` (created empty
    /// if this is the first window ever tiled there).
    pub fn insert(
        &mut self,
        output: &str,
        workspace: u32,
        window: Window,
        target: Option<&WlSurface>,
    ) {
        let key = (output.to_string(), workspace);
        let surface = window
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().clone());
        self.trees
            .entry(key.clone())
            .or_default()
            .insert(window, target);
        if let Some(surface) = surface {
            let old = self.owners.insert(surface, key);
            debug_assert!(old.is_none(), "a tiled surface must have exactly one owner");
        }
        self.bump_topology_revision();
    }

    /// Every window tiled on `output`'s `workspace` tree. Used by
    /// `Smallvil::switch_workspace` to find what needs hiding/showing.
    pub fn windows_in(&self, output: &str, workspace: u32) -> Vec<Window> {
        self.trees
            .get(&(output.to_string(), workspace))
            .map(|l| l.windows())
            .unwrap_or_default()
    }

    /// Every `(output, workspace)` pair with at least one window tiled on
    /// it, visible or not. Used by the IPC `workspaces` query. A freshly
    /// switched-to but still-empty workspace isn't included -- there's
    /// nothing to report beyond "it's active," which the caller already
    /// knows from `active_workspace`.
    pub fn populated_workspaces(&self) -> Vec<(String, u32)> {
        self.trees
            .iter()
            .filter(|(_, tree)| !tree.is_empty())
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Swaps `output_a`'s and `output_b`'s currently-*active* trees:
    /// whatever's tiled on `output_a`'s active workspace becomes the
    /// content of `output_b`'s active workspace slot, and vice versa. Each
    /// output keeps its own workspace *number* (a per-output slot label,
    /// see this struct's own doc comment) -- only the tiled content
    /// trades places, so windows physically relocate onto the other
    /// monitor's screen area once the caller retiles. Doesn't touch
    /// `active`, since neither output's active workspace *number*
    /// changes, just what's in it. Floating windows aren't tracked here
    /// at all (see `Smallvil::floating_workspace`) -- the caller
    /// (`Smallvil::swap_workspaces`) retags and repositions those itself.
    pub fn swap_active(&mut self, output_a: &str, output_b: &str) {
        let ws_a = self.active_workspace(output_a);
        let ws_b = self.active_workspace(output_b);
        let tree_a = self
            .trees
            .remove(&(output_a.to_string(), ws_a))
            .unwrap_or_default();
        let tree_b = self
            .trees
            .remove(&(output_b.to_string(), ws_b))
            .unwrap_or_default();
        // Do not materialize empty trees. Workspace IDs are accepted over
        // IPC, so repeatedly swapping arbitrary empty active workspaces must
        // not grow this registry forever.
        let changed = !tree_a.is_empty() || !tree_b.is_empty();
        if !tree_b.is_empty() {
            index_tree_owners(&mut self.owners, output_a, ws_a, &tree_b);
            self.trees.insert((output_a.to_string(), ws_a), tree_b);
        }
        if !tree_a.is_empty() {
            index_tree_owners(&mut self.owners, output_b, ws_b, &tree_a);
            self.trees.insert((output_b.to_string(), ws_b), tree_a);
        }
        if changed {
            self.bump_topology_revision();
        }
    }

    /// Removes the window backed by `surface` from whichever tree holds it.
    /// No-op if `surface` isn't tiled anywhere.
    pub fn remove(&mut self, surface: &WlSurface) {
        let Some(key) = self.owners.get(surface).cloned() else {
            return;
        };
        let removed = self
            .trees
            .get_mut(&key)
            .is_some_and(|layout| layout.remove(surface));
        debug_assert!(removed, "layout owner index must point at the owning tree");
        self.owners.remove(surface);
        if !removed {
            return;
        }
        // Workspace IDs can come from IPC, so retaining an empty tree for
        // every ID a window ever visited creates unbounded memory growth and
        // progressively slows all ownership scans.
        if self.trees.get(&key).is_some_and(BspLayout::is_empty) {
            self.trees.remove(&key);
        }
        // `algorithms`/`master_ratio` are keyed the same way and reachable
        // through the same IPC action surface (see their own field docs) --
        // prune them alongside `trees` so they can't outlive it.
        if !self.trees.contains_key(&key) {
            self.algorithms.remove(&key);
            self.master_ratio.remove(&key);
            self.cascade_state.remove(&key);
        }
        self.bump_topology_revision();
    }

    /// The tiling algorithm active for `output`'s `workspace`: an explicit
    /// `set_algorithm` override first, then a `[[workspace_rule]]` default
    /// for this workspace number, then the global configured default.
    pub fn algorithm(&self, output: &str, workspace: u32) -> LayoutAlgorithm {
        self.algorithms
            .get(&(output.to_string(), workspace))
            .copied()
            .or_else(|| self.workspace_algorithm_overrides.get(&workspace).copied())
            .unwrap_or(self.default_algorithm)
    }

    /// Overrides `output`'s `workspace` to use `algorithm` instead of the
    /// configured default (see `algorithm`'s own doc, and this struct's
    /// `algorithms` field doc for the pruning this participates in).
    pub fn set_algorithm(&mut self, output: &str, workspace: u32, algorithm: LayoutAlgorithm) {
        if self
            .algorithms
            .insert((output.to_string(), workspace), algorithm)
            != Some(algorithm)
        {
            self.bump_topology_revision();
        }
    }

    /// Sets the fallback `algorithm` uses for any (output, workspace)
    /// without its own override. Called once at startup and again on every
    /// config reload (`Smallvil::new`/`reload_config`) -- not cached inside
    /// `Config` and read fresh each `retile()`, since `Layouts` (not
    /// `Config`) is what actually needs it at layout time.
    pub fn set_default_algorithm(&mut self, algorithm: LayoutAlgorithm) {
        if self.default_algorithm != algorithm {
            self.default_algorithm = algorithm;
            self.bump_topology_revision();
        }
    }

    /// Sets the whole `[[workspace_rule]]`-derived per-workspace-number
    /// layout map at once (see `algorithm`'s fallback chain and this
    /// struct's `workspace_algorithm_overrides` field doc). Called once at
    /// startup and again on every config reload, same as
    /// `set_default_algorithm`.
    pub fn set_workspace_algorithm_overrides(&mut self, overrides: HashMap<u32, LayoutAlgorithm>) {
        if self.workspace_algorithm_overrides != overrides {
            self.workspace_algorithm_overrides = overrides;
            self.bump_topology_revision();
        }
    }

    /// Sets which side the master pane sits on for every workspace under
    /// `LayoutAlgorithm::Master`. Called once at startup and again on every
    /// config reload, same as `set_default_algorithm`.
    pub fn set_master_orientation(&mut self, orientation: MasterOrientation) {
        self.master_orientation = orientation;
    }

    /// Sets the manual per-split axis override for every BSP tree. Called
    /// once at startup and again on every config reload, same as
    /// `set_master_orientation`.
    pub fn set_split_bias(&mut self, bias: SplitBias) {
        if self.split_bias != bias {
            self.split_bias = bias;
            // A live grab's stored axis and split spans were computed under
            // the old bias. Invalidate it instead of driving geometry with
            // stale metadata after a config reload.
            self.bump_topology_revision();
        }
    }

    /// The master/stack split fraction for `output`'s `workspace` (master
    /// mode only). Defaults to an even 0.5 split.
    pub fn master_ratio(&self, output: &str, workspace: u32) -> f32 {
        self.master_ratio
            .get(&(output.to_string(), workspace))
            .copied()
            .unwrap_or(0.5)
    }

    /// Nudges `output`'s `workspace` master/stack ratio by `delta` (positive
    /// grows the master pane), clamped so neither side can collapse to
    /// nothing -- same clamp bounds `BspLayout::set_ratio` uses for the
    /// analogous BSP split ratio. Recorded regardless of which algorithm is
    /// currently active for this (output, workspace), so adjusting it while
    /// BSP is active (a visible no-op today) still takes effect immediately
    /// if the workspace is later switched to master.
    pub fn adjust_master_ratio(&mut self, output: &str, workspace: u32, delta: f32) {
        let new_ratio = (self.master_ratio(output, workspace) + delta).clamp(0.05, 0.95);
        self.master_ratio
            .insert((output.to_string(), workspace), new_ratio);
    }

    /// Swaps `a` and `b` if -- and only if -- they're tiled in the *same*
    /// (output, workspace) tree. Two windows in different trees have no
    /// single split ratio a swap could mean, so this intentionally no-ops
    /// rather than picking an arbitrary tree.
    pub fn swap(&mut self, a: &WlSurface, b: &WlSurface) {
        let (Some(key_a), Some(key_b)) = (self.key_of(a), self.key_of(b)) else {
            return;
        };
        if key_a != key_b {
            return;
        }
        if let Some(layout) = self.trees.get_mut(&key_a) {
            layout.swap(a, b);
            self.bump_topology_revision();
        }
    }

    /// Replaces whichever `Window` occupies `old`'s leaf with `new_window`,
    /// wherever `old` is tiled. See `BspLayout::replace_leaf`. No-op if
    /// `old` isn't tiled anywhere.
    pub fn replace_leaf(&mut self, old: &WlSurface, new_window: &Window) {
        let Some(key) = self.key_of(old) else { return };
        let replaced = self
            .trees
            .get_mut(&key)
            .is_some_and(|layout| layout.replace_leaf(old, new_window));
        if replaced {
            self.owners.remove(old);
            if let Some(surface) = new_window
                .toplevel()
                .map(|toplevel| toplevel.wl_surface().clone())
            {
                let previous = self.owners.insert(surface, key);
                debug_assert!(
                    previous.is_none(),
                    "a replacement surface must not already have a tiled owner"
                );
            }
            self.bump_topology_revision();
        }
    }

    /// The on-screen rectangle for every window tiled on `output`'s
    /// `workspace` tree, using whichever algorithm is active there (see
    /// `algorithm`). Either way the *tree* itself (membership, insertion
    /// order, groups) is unchanged -- only the geometry this computes
    /// differs.
    pub fn layout(
        &self,
        output: &str,
        workspace: u32,
        area: Rectangle<i32, Logical>,
        gap: i32,
    ) -> Vec<(Window, Rectangle<i32, Logical>)> {
        let Some(tree) = self.trees.get(&(output.to_string(), workspace)) else {
            return Vec::new();
        };
        match self.algorithm(output, workspace) {
            LayoutAlgorithm::Bsp => tree.layout(area, gap, self.split_bias),
            LayoutAlgorithm::Master => layout_master(
                tree.windows(),
                area,
                gap,
                self.master_ratio(output, workspace),
                self.master_orientation,
            ),
            LayoutAlgorithm::Cascade => {
                let windows = tree.windows();
                let target_aspect = area.size.w as f32 / (area.size.h as f32).max(1.0);
                let key = (output.to_string(), workspace);
                let state = resolve_cascade_state(
                    self.cascade_state.get(&key),
                    windows.len(),
                    target_aspect,
                );
                layout_cascade(windows, area, gap, &state)
            }
        }
    }

    /// Window count currently tiled in `output`'s `workspace`, regardless
    /// of algorithm. Used by `retile_with_viscosity` to refresh cascade's
    /// manual-resize state (`refresh_cascade_state`) before rendering it.
    pub fn window_count(&self, output: &str, workspace: u32) -> usize {
        self.trees
            .get(&(output.to_string(), workspace))
            .map(BspLayout::len)
            .unwrap_or(0)
    }

    /// Refreshes and stores the resolved cascade manual-resize state for
    /// `output`'s `workspace` against `window_count`/`target_aspect`, so
    /// `cascade_hit_test`/`cascade_resize_splits` (and the next `layout()`
    /// call) read the same partition. `layout()` itself stays read-only and
    /// resolves a state fresh each call without storing it, since it's
    /// called from several read-only query sites (ripple/click-target
    /// lookups) that shouldn't advance this bookkeeping -- only
    /// `retile_with_viscosity`, the one authoritative render-driving
    /// caller, actually needs to. No-op under any other algorithm.
    pub fn refresh_cascade_state(
        &mut self,
        output: &str,
        workspace: u32,
        window_count: usize,
        target_aspect: f32,
    ) {
        let key = (output.to_string(), workspace);
        if window_count == 0 {
            self.cascade_state.remove(&key);
            return;
        }
        if self.algorithm(output, workspace) != LayoutAlgorithm::Cascade {
            return;
        }
        let state =
            resolve_cascade_state(self.cascade_state.get(&key), window_count, target_aspect);
        self.cascade_state.insert(key, state);
    }

    // -- Engine migration (S6) -------------------------------------------------

    /// Drains every populated workspace tree for engine migration to Ocean.
    /// Returns `(output, workspace, tree)` triples, removing the tree and
    /// all associated per-workspace state (algorithm, master ratio, cascade
    /// state) from this `Layouts`. Empty trees are skipped -- they were
    /// never real content, just lazily-created stubs.
    pub(crate) fn drain_for_migration(&mut self) -> Vec<(String, u32, BspLayout)> {
        let keys: Vec<(String, u32)> = self
            .trees
            .iter()
            .filter(|(_, tree)| !tree.is_empty())
            .map(|(k, _)| k.clone())
            .collect();
        let drained = keys
            .into_iter()
            .filter_map(|key| {
                self.algorithms.remove(&key);
                self.master_ratio.remove(&key);
                self.cascade_state.remove(&key);
                let tree = self.trees.remove(&key)?;
                Some((key.0, key.1, tree))
            })
            .collect();
        self.owners.clear();
        drained
    }

    /// Inserts a pre-populated workspace tree, used by engine migration from
    /// Ocean where a reef's tree moves whole into a workspace slot. Does
    /// not set an algorithm -- the caller should follow up with
    /// `set_algorithm` or rely on `default_algorithm`.
    pub(crate) fn insert_migrated_tree(&mut self, output: String, workspace: u32, tree: BspLayout) {
        let key = (output, workspace);
        if let Some(old) = self.trees.remove(&key) {
            unindex_tree_owners(&mut self.owners, &old);
        }
        index_tree_owners(&mut self.owners, &key.0, key.1, &tree);
        self.trees.insert(key, tree);
        self.bump_topology_revision();
    }

    /// Finds the cascade grid boundary nearest `point`, mirroring
    /// `hit_test_split`'s shape but walking the last-resolved row/cell
    /// partition (`cascade_state`, populated by `layout()`) instead of a
    /// tree, since cascade has none. `None` under any other algorithm, or
    /// if `layout()` hasn't resolved a partition for this workspace yet.
    pub fn cascade_hit_test(
        &self,
        output: &str,
        workspace: u32,
        area: Rectangle<i32, Logical>,
        gap: i32,
        point: Point<f64, Logical>,
    ) -> Option<CascadeHit> {
        if self.algorithm(output, workspace) != LayoutAlgorithm::Cascade {
            return None;
        }
        let state = self.cascade_state.get(&(output.to_string(), workspace))?;
        let rects = cascade_rects_from_state(state, area);
        let threshold = (gap as f64).max(4.0);

        let mut index = 0;
        for (i, &cols) in state.row_counts.iter().enumerate() {
            let row_rect = rects[index];
            if i + 1 < state.row_counts.len() {
                let boundary = (row_rect.loc.y + row_rect.size.h) as f64;
                if (point.y - boundary).abs() <= threshold
                    && point.x >= area.loc.x as f64
                    && point.x <= (area.loc.x + area.size.w) as f64
                {
                    return Some(CascadeHit {
                        output: output.to_string(),
                        workspace,
                        axis: Axis::Vertical,
                        row: i,
                        col: None,
                        area,
                        start_ratio: state.row_ratios[i],
                        topology_revision: self.topology_revision,
                    });
                }
            }
            for j in 0..cols {
                if j + 1 < cols {
                    let cell_rect = rects[index + j];
                    let boundary = (cell_rect.loc.x + cell_rect.size.w) as f64;
                    if (point.x - boundary).abs() <= threshold
                        && point.y >= row_rect.loc.y as f64
                        && point.y <= (row_rect.loc.y + row_rect.size.h) as f64
                    {
                        return Some(CascadeHit {
                            output: output.to_string(),
                            workspace,
                            axis: Axis::Horizontal,
                            row: i,
                            col: Some(j),
                            area,
                            start_ratio: state.cell_ratios[i][j],
                            topology_revision: self.topology_revision,
                        });
                    }
                }
            }
            index += cols;
        }
        None
    }

    /// The cascade boundaries adjacent to `surface`'s own cell: its right
    /// edge (if it isn't the last column in its row) and its bottom edge
    /// (if its row isn't the last one), each independently draggable --
    /// cascade's counterpart to `resize_splits`, minus BSP's
    /// connected-vessel propagation, since a cascade boundary only ever
    /// touches its two immediate neighbors.
    pub fn cascade_resize_splits(
        &self,
        output: &str,
        workspace: u32,
        area: Rectangle<i32, Logical>,
        surface: &WlSurface,
    ) -> Vec<CascadeHit> {
        if self.algorithm(output, workspace) != LayoutAlgorithm::Cascade {
            return Vec::new();
        }
        let key = (output.to_string(), workspace);
        let Some(state) = self.cascade_state.get(&key) else {
            return Vec::new();
        };
        let Some(tree) = self.trees.get(&key) else {
            return Vec::new();
        };
        let windows = tree.windows();
        let Some(mut index) = windows.iter().position(|w| is_window(w, surface)) else {
            return Vec::new();
        };

        let mut row = 0;
        for (r, &cols) in state.row_counts.iter().enumerate() {
            if index < cols {
                row = r;
                break;
            }
            index -= cols;
        }
        let col = index;

        let mut hits = Vec::new();
        if col + 1 < state.row_counts[row] {
            hits.push(CascadeHit {
                output: output.to_string(),
                workspace,
                axis: Axis::Horizontal,
                row,
                col: Some(col),
                area,
                start_ratio: state.cell_ratios[row][col],
                topology_revision: self.topology_revision,
            });
        }
        if row + 1 < state.row_counts.len() {
            hits.push(CascadeHit {
                output: output.to_string(),
                workspace,
                axis: Axis::Vertical,
                row,
                col: None,
                area,
                start_ratio: state.row_ratios[row],
                topology_revision: self.topology_revision,
            });
        }
        hits
    }

    /// Whether a cascade boundary captured by an active pointer grab still
    /// names a live one -- the cascade counterpart to `split_is_current`.
    /// Ratio updates deliberately do not change the revision, so the grab
    /// can keep driving its own boundary.
    pub fn cascade_hit_is_current(&self, hit: &CascadeHit) -> bool {
        self.active_workspace(&hit.output) == hit.workspace
            && self.algorithm(&hit.output, hit.workspace) == LayoutAlgorithm::Cascade
            && self.topology_revision == hit.topology_revision
            && self
                .cascade_state
                .get(&(hit.output.clone(), hit.workspace))
                .is_some_and(|state| match hit.col {
                    Some(col) => state.row_counts.get(hit.row).is_some_and(|&c| col + 1 < c),
                    None => hit.row + 1 < state.row_counts.len(),
                })
    }

    /// Applies `new_ratio` to `hit`'s boundary, redistributing its paired
    /// neighbor (`apply_paired_ratio`) to keep their combined share
    /// constant. No-op if `hit` no longer names a live boundary.
    pub fn set_cascade_ratio(&mut self, hit: &CascadeHit, new_ratio: f32) {
        let Some(state) = self
            .cascade_state
            .get_mut(&(hit.output.clone(), hit.workspace))
        else {
            return;
        };
        match hit.col {
            Some(col) => {
                let Some(cells) = state.cell_ratios.get_mut(hit.row) else {
                    return;
                };
                if col + 1 >= cells.len() {
                    return;
                }
                apply_paired_ratio(cells, col, new_ratio);
            }
            None => {
                if hit.row + 1 >= state.row_ratios.len() {
                    return;
                }
                apply_paired_ratio(&mut state.row_ratios, hit.row, new_ratio);
            }
        }
    }

    /// Finds the split boundary nearest `point` within `output`'s `workspace`
    /// tree; see `BspLayout::hit_test_split`. Always `None` under master or
    /// cascade mode: neither's geometry (`layout_master`/`layout_cascade`)
    /// follows the tree's own shape, so a BSP split boundary found here
    /// wouldn't correspond to any actual border on screen to drag. Cascade's
    /// own row/column resize is unbuilt scope, not an oversight here.
    pub fn hit_test_split(
        &self,
        output: &str,
        workspace: u32,
        area: Rectangle<i32, Logical>,
        gap: i32,
        point: Point<f64, Logical>,
    ) -> Option<SplitHit> {
        if self.algorithm(output, workspace) != LayoutAlgorithm::Bsp {
            return None;
        }
        let mut hit = self
            .trees
            .get(&(output.to_string(), workspace))?
            .hit_test_split(area, gap, point, self.split_bias)?;
        hit.output = output.to_string();
        hit.workspace = workspace;
        hit.topology_revision = self.topology_revision;
        Some(hit)
    }

    /// The nearest enclosing split per axis for `surface` within `output`'s
    /// `workspace` tree; see `BspLayout::resize_splits`. Always empty under
    /// master or cascade mode, same reasoning as `hit_test_split`.
    pub fn resize_splits(
        &self,
        output: &str,
        workspace: u32,
        area: Rectangle<i32, Logical>,
        surface: &WlSurface,
    ) -> Vec<SplitHit> {
        if self.algorithm(output, workspace) != LayoutAlgorithm::Bsp {
            return Vec::new();
        }
        let Some(tree) = self.trees.get(&(output.to_string(), workspace)) else {
            return Vec::new();
        };
        tree.resize_splits(surface, area, self.split_bias)
            .into_iter()
            .map(|mut hit| {
                hit.output = output.to_string();
                hit.workspace = workspace;
                hit.topology_revision = self.topology_revision;
                hit
            })
            .collect()
    }

    pub fn ratio_at(&self, output: &str, workspace: u32, path: &[Side]) -> Option<f32> {
        self.trees
            .get(&(output.to_string(), workspace))?
            .ratio_at(path)
    }

    pub fn set_ratio(&mut self, output: &str, workspace: u32, path: &[Side], ratio: f32) {
        if let Some(layout) = self.trees.get_mut(&(output.to_string(), workspace)) {
            layout.set_ratio(path, ratio);
        }
    }

    /// Builds the fixed set of parallel splits driven by one resize
    /// gesture. The primary split receives the full pointer displacement;
    /// same-axis ancestors receive geometrically damped pressure until
    /// `max_splits` is reached. A border drag moves every boundary in the
    /// pointer direction. A window-body/keyboard resize instead signs each
    /// ancestor from the target leaf's side, so positive displacement grows
    /// that window even when it lives in a split's second child.
    pub(crate) fn connected_resize_handles(
        &self,
        hit: &SplitHit,
        falloff: f32,
        max_splits: u8,
    ) -> Vec<SplitResizeHandle> {
        if !self.split_is_current(hit) {
            return Vec::new();
        }
        let Some(tree) = self.trees.get(&(hit.output.clone(), hit.workspace)) else {
            return Vec::new();
        };
        let Some(root) = tree.root.as_ref() else {
            return Vec::new();
        };
        let mut path_splits = Vec::new();
        collect_split_path(
            root,
            hit.root_area,
            self.split_bias,
            &hit.path,
            Vec::new(),
            &mut path_splits,
        );

        let falloff = falloff.clamp(0.0, 1.0);
        let max_splits = usize::from(max_splits.max(1));
        let primary_depth = hit.path.len();
        path_splits
            .into_iter()
            .rev()
            .filter(|split| split.axis == hit.axis)
            .filter_map(|split| {
                let distance = primary_depth.checked_sub(split.path.len())?;
                let magnitude = propagation_weight(falloff, distance);
                if magnitude <= f32::EPSILON {
                    return None;
                }
                let target_side = if hit.target_side.is_some() {
                    if distance == 0 {
                        hit.target_side
                    } else {
                        hit.path.get(split.path.len()).copied()
                    }
                } else {
                    None
                };
                Some(SplitResizeHandle {
                    hit: SplitHit {
                        output: hit.output.clone(),
                        workspace: hit.workspace,
                        path: split.path,
                        axis: split.axis,
                        area: split.area,
                        root_area: hit.root_area,
                        target_side,
                        topology_revision: hit.topology_revision,
                    },
                    start_ratio: split.ratio,
                    weight: signed_resize_weight(magnitude, target_side),
                })
            })
            .take(max_splits)
            .collect()
    }

    /// Whether a split captured by an active pointer grab still names the
    /// same visible tree topology. Ratio updates deliberately do not change
    /// the revision, so the grab can keep driving its own split.
    pub fn split_is_current(&self, hit: &SplitHit) -> bool {
        self.active_workspace(&hit.output) == hit.workspace
            && self.algorithm(&hit.output, hit.workspace) == LayoutAlgorithm::Bsp
            && self.topology_revision == hit.topology_revision
            && self
                .trees
                .get(&(hit.output.clone(), hit.workspace))
                .is_some_and(|tree| tree.ratio_at(&hit.path).is_some())
    }

    fn bump_topology_revision(&mut self) {
        self.topology_revision = self.topology_revision.wrapping_add(1);
    }
}

#[allow(clippy::mutable_key_type)]
fn index_tree_owners(
    owners: &mut HashMap<WlSurface, (String, u32)>,
    output: &str,
    workspace: u32,
    tree: &BspLayout,
) {
    for surface in tree.windows().into_iter().filter_map(|window| {
        window
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().clone())
    }) {
        owners.insert(surface, (output.to_string(), workspace));
    }
}

#[allow(clippy::mutable_key_type)]
fn unindex_tree_owners(owners: &mut HashMap<WlSurface, (String, u32)>, tree: &BspLayout) {
    for surface in tree.windows().into_iter().filter_map(|window| {
        window
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().clone())
    }) {
        owners.remove(&surface);
    }
}

fn insert_into(mut root: Node, window: Window, target: Option<&WlSurface>) -> Node {
    let path = target
        .and_then(|target| find_path(&root, target))
        .unwrap_or_else(|| last_leaf_path(&root));
    let leaf = node_mut_at_path(&mut root, &path).expect("a discovered BSP path must remain valid");
    let old = std::mem::replace(leaf, Node::Leaf(window.clone()));
    let Node::Leaf(existing) = old else {
        unreachable!("BSP insertion paths always terminate at a leaf")
    };
    *leaf = Node::Split {
        ratio: 0.5,
        first: Box::new(Node::Leaf(existing)),
        second: Box::new(Node::Leaf(window)),
    };
    root
}

struct RemoveFrame {
    ratio: f32,
    descended: Side,
    sibling: Node,
}

fn remove_from(node: Node, target: &WlSurface) -> (Option<Node>, bool) {
    let Some(path) = find_path(&node, target) else {
        return (Some(node), false);
    };
    if path.is_empty() {
        return (None, true);
    }

    let mut frames = Vec::with_capacity(path.len());
    let mut current = node;
    for side in path {
        let Node::Split {
            ratio,
            first,
            second,
        } = current
        else {
            unreachable!("a discovered BSP path must remain valid")
        };
        match side {
            Side::First => {
                frames.push(RemoveFrame {
                    ratio,
                    descended: side,
                    sibling: *second,
                });
                current = *first;
            }
            Side::Second => {
                frames.push(RemoveFrame {
                    ratio,
                    descended: side,
                    sibling: *first,
                });
                current = *second;
            }
        }
    }
    debug_assert!(matches!(current, Node::Leaf(_)));

    let mut rebuilt = None;
    while let Some(frame) = frames.pop() {
        rebuilt = Some(match rebuilt {
            None => frame.sibling,
            Some(child) => match frame.descended {
                Side::First => Node::Split {
                    ratio: frame.ratio,
                    first: Box::new(child),
                    second: Box::new(frame.sibling),
                },
                Side::Second => Node::Split {
                    ratio: frame.ratio,
                    first: Box::new(frame.sibling),
                    second: Box::new(child),
                },
            },
        });
    }
    (rebuilt, true)
}

fn node_contains(node: &Node, target: &WlSurface) -> bool {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        match node {
            Node::Leaf(window) if is_window(window, target) => return true,
            Node::Leaf(_) => {}
            Node::Split { first, second, .. } => {
                pending.push(second);
                pending.push(first);
            }
        }
    }
    false
}

fn is_window(window: &Window, target: &WlSurface) -> bool {
    window
        .toplevel()
        .map(|t| t.wl_surface() == target)
        .unwrap_or(false)
}

fn find_path(node: &Node, target: &WlSurface) -> Option<Vec<Side>> {
    let mut pending = vec![(node, 0_usize, None)];
    let mut path = Vec::new();
    while let Some((node, parent_depth, side)) = pending.pop() {
        path.truncate(parent_depth);
        if let Some(side) = side {
            path.push(side);
        }
        match node {
            Node::Leaf(window) if is_window(window, target) => return Some(path.clone()),
            Node::Leaf(_) => {}
            Node::Split { first, second, .. } => {
                let depth = path.len();
                pending.push((second, depth, Some(Side::Second)));
                pending.push((first, depth, Some(Side::First)));
            }
        }
    }
    None
}

fn last_leaf_path(mut node: &Node) -> Vec<Side> {
    let mut path = Vec::new();
    while let Node::Split { second, .. } = node {
        path.push(Side::Second);
        node = second;
    }
    path
}

fn node_at_path<'a>(mut node: &'a Node, path: &[Side]) -> Option<&'a Node> {
    for side in path {
        let Node::Split { first, second, .. } = node else {
            return None;
        };
        node = match side {
            Side::First => first,
            Side::Second => second,
        };
    }
    Some(node)
}

fn node_mut_at_path<'a>(mut node: &'a mut Node, path: &[Side]) -> Option<&'a mut Node> {
    for side in path {
        let Node::Split { first, second, .. } = node else {
            return None;
        };
        node = match side {
            Side::First => first,
            Side::Second => second,
        };
    }
    Some(node)
}

fn find_window(node: &Node, target: &WlSurface) -> Option<Window> {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        match node {
            Node::Leaf(window) if is_window(window, target) => return Some(window.clone()),
            Node::Leaf(_) => {}
            Node::Split { first, second, .. } => {
                pending.push(second);
                pending.push(first);
            }
        }
    }
    None
}

/// Swaps the two windows in a single tree pass, matching each leaf against
/// its pre-swap surface (`a`/`b`, fixed for the whole call) rather than its
/// current contents. Matching against live contents would let the first
/// write's result collide with the second match -- e.g. writing `window_b`
/// into `a`'s leaf and then matching leaves against surface `b` would also
/// match that leaf a second time, duplicating `window_b` into both slots.
fn swap_leaves(
    node: &mut Node,
    a: &WlSurface,
    window_a: &Window,
    b: &WlSurface,
    window_b: &Window,
) {
    let (Some(path_a), Some(path_b)) = (find_path(node, a), find_path(node, b)) else {
        return;
    };
    if let Some(Node::Leaf(window)) = node_mut_at_path(node, &path_a) {
        *window = window_b.clone();
    }
    if let Some(Node::Leaf(window)) = node_mut_at_path(node, &path_b) {
        *window = window_a.clone();
    }
}

/// Single-sided counterpart to `swap_leaves`: overwrites the leaf currently
/// holding `old` with `new_window`, unconditionally (`new_window` need not
/// already be part of this tree).
fn replace_leaf(node: &mut Node, old: &WlSurface, new_window: &Window) -> bool {
    let Some(path) = find_path(node, old) else {
        return false;
    };
    if let Some(Node::Leaf(window)) = node_mut_at_path(node, &path) {
        *window = new_window.clone();
        true
    } else {
        false
    }
}

struct PathSplit {
    path: Vec<Side>,
    axis: Axis,
    area: Rectangle<i32, Logical>,
    ratio: f32,
}

/// Records every split from the root through `target_path`, including the
/// addressed split itself. Areas are derived from the ratios captured at
/// grab start, so every connected handle keeps stable pixel-to-ratio math
/// for the gesture's full lifetime.
fn collect_split_path(
    mut node: &Node,
    mut area: Rectangle<i32, Logical>,
    bias: SplitBias,
    target_path: &[Side],
    mut path: Vec<Side>,
    out: &mut Vec<PathSplit>,
) {
    for next_side in target_path.iter().copied().map(Some).chain([None]) {
        let Node::Split {
            ratio,
            first,
            second,
        } = node
        else {
            return;
        };
        out.push(PathSplit {
            path: path.clone(),
            axis: split_axis(area, bias),
            area,
            ratio: *ratio,
        });
        let Some(side) = next_side else { return };
        let (first_area, second_area) = split(area, *ratio, bias);
        path.push(side);
        match side {
            Side::First => {
                node = first;
                area = first_area;
            }
            Side::Second => {
                node = second;
                area = second_area;
            }
        }
    }
}

fn propagation_weight(falloff: f32, distance: usize) -> f32 {
    falloff.clamp(0.0, 1.0).powi(distance as i32)
}

fn signed_resize_weight(magnitude: f32, target_side: Option<Side>) -> f32 {
    match target_side {
        Some(Side::Second) => -magnitude,
        Some(Side::First) | None => magnitude,
    }
}

/// Pre-order walk down the true path to `target` (deciding direction with
/// `node_contains`, same as `insert_into`), recording each `Split` crossed
/// into `horizontal`/`vertical` by its axis. Writes happen on the way down,
/// so a deeper split of the same axis always overwrites a shallower one --
/// by the time the walk reaches `target`'s leaf, each slot holds the
/// nearest enclosing split of that axis, not the outermost.
fn collect_resize_splits(
    node: &Node,
    target: &WlSurface,
    mut area: Rectangle<i32, Logical>,
    root_area: Rectangle<i32, Logical>,
    bias: SplitBias,
    found: &mut [Option<SplitHit>; 2],
) {
    let Some(target_path) = find_path(node, target) else {
        return;
    };
    let mut node = node;
    let mut path = Vec::with_capacity(target_path.len());
    for target_side in target_path {
        let Node::Split {
            ratio,
            first,
            second,
        } = node
        else {
            return;
        };
        let (first_area, second_area) = split(area, *ratio, bias);
        let axis = split_axis(area, bias);
        let hit = SplitHit {
            output: String::new(),
            workspace: 0,
            path: path.clone(),
            axis,
            area,
            root_area,
            target_side: Some(target_side),
            topology_revision: 0,
        };
        match axis {
            Axis::Horizontal => found[0] = Some(hit),
            Axis::Vertical => found[1] = Some(hit),
        }
        path.push(target_side);
        match target_side {
            Side::First => {
                node = first;
                area = first_area;
            }
            Side::Second => {
                node = second;
                area = second_area;
            }
        }
    }
}

fn hit_test(
    mut node: &Node,
    mut area: Rectangle<i32, Logical>,
    root_area: Rectangle<i32, Logical>,
    gap: i32,
    point: Point<f64, Logical>,
    bias: SplitBias,
    mut path: Vec<Side>,
) -> Option<SplitHit> {
    loop {
        let Node::Split {
            ratio,
            first,
            second,
        } = node
        else {
            return None;
        };

        let (first_area, second_area) = split(area, *ratio, bias);
        let axis = split_axis(area, bias);
        let threshold = (gap as f64).max(4.0);
        let on_border = match axis {
            Axis::Horizontal => {
                let boundary = (first_area.loc.x + first_area.size.w) as f64;
                (point.x - boundary).abs() <= threshold
                    && point.y >= area.loc.y as f64
                    && point.y <= (area.loc.y + area.size.h) as f64
            }
            Axis::Vertical => {
                let boundary = (first_area.loc.y + first_area.size.h) as f64;
                (point.y - boundary).abs() <= threshold
                    && point.x >= area.loc.x as f64
                    && point.x <= (area.loc.x + area.size.w) as f64
            }
        };
        if on_border {
            return Some(SplitHit {
                output: String::new(),
                workspace: 0,
                path,
                axis,
                area,
                root_area,
                target_side: None,
                topology_revision: 0,
            });
        }

        if rect_contains(first_area, point) {
            path.push(Side::First);
            node = first;
            area = first_area;
        } else {
            path.push(Side::Second);
            node = second;
            area = second_area;
        }
    }
}

fn rect_contains(rect: Rectangle<i32, Logical>, point: Point<f64, Logical>) -> bool {
    point.x >= rect.loc.x as f64
        && point.x < (rect.loc.x + rect.size.w) as f64
        && point.y >= rect.loc.y as f64
        && point.y < (rect.loc.y + rect.size.h) as f64
}

fn ratio_at(node: &Node, path: &[Side]) -> Option<f32> {
    let Node::Split { ratio, .. } = node_at_path(node, path)? else {
        return None;
    };
    Some(*ratio)
}

fn set_ratio_at(node: &mut Node, path: &[Side], ratio: f32) {
    if let Some(Node::Split { ratio: current, .. }) = node_mut_at_path(node, path) {
        *current = ratio;
    }
}

fn collect(
    node: &Node,
    area: Rectangle<i32, Logical>,
    bias: SplitBias,
    out: &mut Vec<(Window, Rectangle<i32, Logical>)>,
) {
    let mut pending = vec![(node, area)];
    while let Some((node, area)) = pending.pop() {
        match node {
            Node::Leaf(window) => out.push((window.clone(), area)),
            Node::Split {
                ratio,
                first,
                second,
            } => {
                let (first_area, second_area) = split(area, *ratio, bias);
                pending.push((second, second_area));
                pending.push((first, first_area));
            }
        }
    }
}

fn collect_windows(node: &Node, out: &mut Vec<Window>) {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        match node {
            Node::Leaf(window) => out.push(window.clone()),
            Node::Split { first, second, .. } => {
                pending.push(second);
                pending.push(first);
            }
        }
    }
}

/// Which way a split at `area` runs: `bias` overrides the adaptive choice
/// when set to anything but `Auto`, otherwise wide area -> side-by-side,
/// tall area -> stacked. The single source of truth `split()`'s own axis
/// choice and every `Axis` metadata computation (`hit_test`,
/// `collect_resize_splits`) route through, so the two can't drift apart --
/// this project has been burned before by the same invariant enforced in
/// more than one place independently (see the Z-order note in AGENT.md).
fn split_axis(area: Rectangle<i32, Logical>, bias: SplitBias) -> Axis {
    match bias {
        SplitBias::Horizontal => Axis::Horizontal,
        SplitBias::Vertical => Axis::Vertical,
        SplitBias::Auto => {
            if area.size.w >= area.size.h {
                Axis::Horizontal
            } else {
                Axis::Vertical
            }
        }
    }
}

/// Wide area -> side-by-side split; tall area -> stacked split, unless
/// `bias` forces one way. Decided fresh every layout pass from the area's
/// own aspect ratio (or the override), not stored.
fn split(
    area: Rectangle<i32, Logical>,
    ratio: f32,
    bias: SplitBias,
) -> (Rectangle<i32, Logical>, Rectangle<i32, Logical>) {
    if split_axis(area, bias) == Axis::Horizontal {
        let first_w = (area.size.w as f32 * ratio).round() as i32;
        let first = Rectangle::new(area.loc, (first_w, area.size.h).into());
        let second = Rectangle::new(
            (area.loc.x + first_w, area.loc.y).into(),
            (area.size.w - first_w, area.size.h).into(),
        );
        (first, second)
    } else {
        let first_h = (area.size.h as f32 * ratio).round() as i32;
        let first = Rectangle::new(area.loc, (area.size.w, first_h).into());
        let second = Rectangle::new(
            (area.loc.x, area.loc.y + first_h).into(),
            (area.size.w, area.size.h - first_h).into(),
        );
        (first, second)
    }
}

/// Shrinks `rect` by `gap` on every edge. `pub(crate)` so callers that need
/// a single rect filling a whole tiling area (e.g. maximize, `state.rs`)
/// can apply the same inset `BspLayout::layout` applies per-leaf, without
/// duplicating the math.
pub(crate) fn inset(rect: Rectangle<i32, Logical>, gap: i32) -> Rectangle<i32, Logical> {
    let max_gap = (rect.size.w.saturating_sub(1) / 2)
        .min(rect.size.h.saturating_sub(1) / 2)
        .max(0);
    let gap = gap.clamp(0, max_gap);
    let doubled = gap.saturating_mul(2);
    Rectangle::new(
        (
            rect.loc.x.saturating_add(gap),
            rect.loc.y.saturating_add(gap),
        )
            .into(),
        (
            rect.size.w.saturating_sub(doubled).max(1),
            rect.size.h.saturating_sub(doubled).max(1),
        )
            .into(),
    )
}

/// Shrinks `rect` to `scale` of its own size on both axes, keeping it
/// centered within its original bounds -- pseudo-tiling's rect override
/// in `Smallvil::retile`, the same shape `inset` provides for gaps.
pub(crate) fn scale_centered(rect: Rectangle<i32, Logical>, scale: f64) -> Rectangle<i32, Logical> {
    let new_w = ((rect.size.w as f64 * scale).round() as i32).max(1);
    let new_h = ((rect.size.h as f64 * scale).round() as i32).max(1);
    let dx = (rect.size.w - new_w) / 2;
    let dy = (rect.size.h - new_h) / 2;
    Rectangle::new(
        (rect.loc.x + dx, rect.loc.y + dy).into(),
        (new_w, new_h).into(),
    )
}

/// Master-stack geometry (dwm/Hyprland's "master" layout): the first
/// window in `windows` (`BspLayout::windows`'s tree-traversal order --
/// this reuses the exact same tree the adaptive BSP layout already
/// maintains for membership/insertion/groups, so this is *not* necessarily
/// literal chronological insertion order once swaps have happened; a
/// deliberate, documented v1 simplification rather than tracking a
/// separate ordered list) takes `master_ratio` of the area on `orientation`'s
/// side; every other window stacks evenly in the remaining area (vertically
/// for `Left`/`Right`, horizontally for `Top`/`Bottom` -- matches Hyprland's
/// own actual behavior for those two, not a naive 90-degree rotation).
/// Always this fixed split regardless of the output's aspect ratio, unlike
/// BSP's per-split adaptive orientation -- that fixed split is the actual
/// visual point of choosing this layout over the adaptive one.
fn layout_master(
    windows: Vec<Window>,
    area: Rectangle<i32, Logical>,
    gap: i32,
    master_ratio: f32,
    orientation: MasterOrientation,
) -> Vec<(Window, Rectangle<i32, Logical>)> {
    let rects = layout_master_rects(windows.len(), area, master_ratio, orientation);
    let mut out: Vec<(Window, Rectangle<i32, Logical>)> = windows.into_iter().zip(rects).collect();
    for (_, rect) in &mut out {
        *rect = inset(*rect, gap);
    }
    out
}

/// The pure geometry behind `layout_master`, split out so it's testable
/// without needing real `Window`/Wayland fixtures (same reasoning as
/// `state.rs`'s `group_removal_outcome`): `count` windows in, one rect per
/// window out, in the same order (master first, then the stack). Each stack
/// window gets its exact share of whatever space is still left, computed
/// one at a time, rather than a single division that would leave a
/// rounding gap at the last window.
fn layout_master_rects(
    count: usize,
    area: Rectangle<i32, Logical>,
    master_ratio: f32,
    orientation: MasterOrientation,
) -> Vec<Rectangle<i32, Logical>> {
    if count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![area];
    }

    let mut out = Vec::with_capacity(count);
    match orientation {
        MasterOrientation::Left | MasterOrientation::Right => {
            let master_w = ((area.size.w as f32) * master_ratio).round() as i32;
            let stack_w = area.size.w - master_w;
            let (master_x, stack_x) = if orientation == MasterOrientation::Left {
                (area.loc.x, area.loc.x + master_w)
            } else {
                (area.loc.x + stack_w, area.loc.x)
            };
            out.push(Rectangle::new(
                (master_x, area.loc.y).into(),
                (master_w, area.size.h).into(),
            ));

            let mut y = area.loc.y;
            let mut remaining_h = area.size.h;
            let mut remaining_count = (count - 1) as i32;
            for _ in 0..count - 1 {
                let h = remaining_h / remaining_count;
                out.push(Rectangle::new((stack_x, y).into(), (stack_w, h).into()));
                y += h;
                remaining_h -= h;
                remaining_count -= 1;
            }
        }
        MasterOrientation::Top | MasterOrientation::Bottom => {
            let master_h = ((area.size.h as f32) * master_ratio).round() as i32;
            let stack_h = area.size.h - master_h;
            let (master_y, stack_y) = if orientation == MasterOrientation::Top {
                (area.loc.y, area.loc.y + master_h)
            } else {
                (area.loc.y + stack_h, area.loc.y)
            };
            out.push(Rectangle::new(
                (area.loc.x, master_y).into(),
                (area.size.w, master_h).into(),
            ));

            let mut x = area.loc.x;
            let mut remaining_w = area.size.w;
            let mut remaining_count = (count - 1) as i32;
            for _ in 0..count - 1 {
                let w = remaining_w / remaining_count;
                out.push(Rectangle::new((x, stack_y).into(), (w, stack_h).into()));
                x += w;
                remaining_w -= w;
                remaining_count -= 1;
            }
        }
    }
    out
}

/// Manual per-row/per-cell resize state for one workspace under cascade
/// layout (`Layouts::cascade_state`). `row_ratios` (one per row) and each
/// row's `cell_ratios` are absolute fractions of the tiling area's full
/// height/width, each summing to `1.0` -- not fractions of a combined pair,
/// which is why a boundary drag (`apply_paired_ratio`) redistributes only
/// the two neighbors it touches and leaves every other entry alone.
/// `row_counts` records the item count each row had when these ratios were
/// last resolved, so `resolve_cascade_state` can tell which rows survived a
/// reflow unchanged (row-scoped persistence, agreed with the maintainer
/// before this was built -- see AGENT.md's render roadmap).
#[derive(Debug, Clone, Default)]
struct CascadeState {
    row_counts: Vec<usize>,
    row_ratios: Vec<f32>,
    cell_ratios: Vec<Vec<f32>>,
}

/// A cascade grid boundary: either a row divider (`col: None`, between
/// `row` and `row + 1`, dragged vertically) or a cell divider within `row`
/// (`col: Some(j)`, between `j` and `j + 1`, dragged horizontally). Plays
/// the same role `SplitHit` does for BSP, but addresses a grid position
/// instead of a tree path since cascade has no tree, and -- unlike
/// `SplitHit`, which is fanned out into several weighted
/// `SplitResizeHandle`s for connected-vessel propagation -- carries its own
/// `start_ratio` directly: a cascade drag only ever touches the two
/// neighbors either side of the dragged boundary, so there is no second
/// type to fan out into.
#[derive(Debug, Clone)]
pub struct CascadeHit {
    pub output: String,
    pub workspace: u32,
    pub axis: Axis,
    row: usize,
    col: Option<usize>,
    /// Pre-gap tiling area at hit-test time, fixed for the whole drag so
    /// ratio math stays stable -- same reasoning as `SplitHit::area`.
    area: Rectangle<i32, Logical>,
    start_ratio: f32,
    topology_revision: u64,
}

impl CascadeHit {
    pub(crate) fn ratio_for_delta(&self, delta_pixels: f64) -> Option<f32> {
        let span = match self.axis {
            Axis::Horizontal => self.area.size.w,
            Axis::Vertical => self.area.size.h,
        };
        (span > 0).then(|| self.start_ratio + (delta_pixels as f32) / span as f32)
    }
}

/// Cascade geometry: TideWM's own "fills the basin" tiling mode. Windows
/// wrap into rows left to right, top to bottom -- `BspLayout::windows`'s own
/// traversal order, same convention `layout_master` reads -- instead of
/// BSP's recursive bisection or master's fixed master+stack split.
fn layout_cascade(
    windows: Vec<Window>,
    area: Rectangle<i32, Logical>,
    gap: i32,
    state: &CascadeState,
) -> Vec<(Window, Rectangle<i32, Logical>)> {
    let rects = cascade_rects_from_state(state, area);
    let mut out: Vec<(Window, Rectangle<i32, Logical>)> = windows.into_iter().zip(rects).collect();
    for (_, rect) in &mut out {
        *rect = inset(*rect, gap);
    }
    out
}

/// Resolves the manual-resize state for `count` windows against
/// `target_aspect`, the pure logic behind `Layouts::layout`'s cascade
/// branch (split out so it's testable without real `Window`/Wayland
/// fixtures, same reasoning as `layout_master_rects`). Picks the row count
/// whose resulting grid shape lands closest to `target_aspect`
/// (`best_cascade_row_count`), distributes `count` windows across that many
/// rows as evenly as possible (`cascade_row_item_counts`), then keeps
/// `existing`'s ratios for any row whose item count didn't change,
/// resetting only the rows that did to an equal share. Ratios are
/// renormalized afterward since a mix of kept and reset entries won't
/// generally already sum to `1.0`.
fn resolve_cascade_state(
    existing: Option<&CascadeState>,
    count: usize,
    target_aspect: f32,
) -> CascadeState {
    if count == 0 {
        return CascadeState::default();
    }

    let rows = best_cascade_row_count(count, target_aspect);
    let row_counts = cascade_row_item_counts(count, rows);

    let mut row_ratios = vec![1.0 / rows as f32; rows];
    let mut cell_ratios: Vec<Vec<f32>> = row_counts
        .iter()
        .map(|&c| vec![1.0 / c as f32; c])
        .collect();

    if let Some(existing) = existing {
        for i in 0..rows {
            if existing.row_counts.get(i) != Some(&row_counts[i]) {
                continue;
            }
            if let Some(&ratio) = existing.row_ratios.get(i) {
                row_ratios[i] = ratio;
            }
            if let Some(cells) = existing.cell_ratios.get(i) {
                if cells.len() == row_counts[i] {
                    cell_ratios[i] = cells.clone();
                }
            }
        }
    }

    normalize_ratios(&mut row_ratios);
    for cells in &mut cell_ratios {
        normalize_ratios(cells);
    }

    CascadeState {
        row_counts,
        row_ratios,
        cell_ratios,
    }
}

fn normalize_ratios(ratios: &mut [f32]) {
    let sum: f32 = ratios.iter().sum();
    if sum > f32::EPSILON {
        for r in ratios.iter_mut() {
            *r /= sum;
        }
    }
}

/// Rects (row-major, matching `BspLayout::windows`'s traversal order) for
/// `state`'s ratios against `area`. Rounds every row/cell but the last on
/// its axis and gives the last whatever space remains, generalizing
/// `layout_master_rects`'s rounding-gap-free remaining-space division from
/// equal shares to `state`'s (possibly manually resized) ratios.
fn cascade_rects_from_state(
    state: &CascadeState,
    area: Rectangle<i32, Logical>,
) -> Vec<Rectangle<i32, Logical>> {
    let rows = state.row_counts.len();
    if rows == 0 {
        return Vec::new();
    }
    if rows == 1 && state.row_counts[0] == 1 {
        return vec![area];
    }

    let mut out = Vec::new();
    let mut y = area.loc.y;
    let mut remaining_h = area.size.h;
    for i in 0..rows {
        let row_h = if i + 1 == rows {
            remaining_h.max(0)
        } else {
            let available = remaining_h.max(0);
            let minimum = i32::from(available > 0);
            ((state.row_ratios[i] * area.size.h as f32).round() as i32).clamp(minimum, available)
        };
        let cols = state.row_counts[i];
        let mut x = area.loc.x;
        let mut remaining_w = area.size.w;
        for j in 0..cols {
            let cell_w = if j + 1 == cols {
                remaining_w.max(0)
            } else {
                let available = remaining_w.max(0);
                let minimum = i32::from(available > 0);
                ((state.cell_ratios[i][j] * area.size.w as f32).round() as i32)
                    .clamp(minimum, available)
            };
            out.push(Rectangle::new((x, y).into(), (cell_w, row_h).into()));
            x += cell_w;
            remaining_w -= cell_w;
        }
        y += row_h;
        remaining_h -= row_h;
    }
    out
}

/// The two-way version of `set_ratio`'s single BSP split: redistributes
/// `ratios[i]`/`ratios[i + 1]`'s combined share so `ratios[i]` becomes
/// `new_ratio`, clamped to keep at least 5% of their *combined* share (not
/// the whole axis) on each side, since other rows/cells may hold the rest.
fn apply_paired_ratio(ratios: &mut [f32], i: usize, new_ratio: f32) {
    let combined = ratios[i] + ratios[i + 1];
    if combined <= f32::EPSILON {
        return;
    }
    let min = combined * 0.05;
    let clamped = new_ratio.clamp(min, combined - min);
    ratios[i + 1] = combined - clamped;
    ratios[i] = clamped;
}

#[cfg(test)]
fn layout_cascade_rects(
    count: usize,
    area: Rectangle<i32, Logical>,
) -> Vec<Rectangle<i32, Logical>> {
    if count == 0 {
        return Vec::new();
    }
    let target_aspect = area.size.w as f32 / (area.size.h as f32).max(1.0);
    let state = resolve_cascade_state(None, count, target_aspect);
    cascade_rects_from_state(&state, area)
}

/// Distributes `count` windows across `rows` rows as evenly as possible;
/// the first `count % rows` rows get one extra window (five windows across
/// two rows becomes `[3, 2]`, not `[2, 3]`).
fn cascade_row_item_counts(count: usize, rows: usize) -> Vec<usize> {
    let base = count / rows;
    let extra = count % rows;
    (0..rows)
        .map(|i| if i < extra { base + 1 } else { base })
        .collect()
}

/// Picks the row count (from 1 to `count`) whose resulting grid shape --
/// columns in its widest row, over row count -- lands closest to
/// `target_aspect` in log space, so a grid twice as wide as the target and
/// one twice as tall score equally against it. This compares the *grid's*
/// shape to the target, not each individual cell's aspect ratio to it: the
/// two sound equivalent but aren't -- comparing per-cell aspect against the
/// output's own aspect ratio algebraically cancels the output's real shape
/// out of the comparison and always settles on the same row count
/// regardless of monitor shape (verified numerically before writing this),
/// while comparing the grid's column/row ratio actually adapts: a wide
/// monitor prefers fewer, wider rows, a tall one prefers more, narrower ones.
fn best_cascade_row_count(count: usize, target_aspect: f32) -> usize {
    (1..=count)
        .min_by(|&a, &b| {
            cascade_row_count_score(count, a, target_aspect)
                .partial_cmp(&cascade_row_count_score(count, b, target_aspect))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(1)
}

fn cascade_row_count_score(count: usize, rows: usize, target_aspect: f32) -> f32 {
    let cols = count.div_ceil(rows);
    let grid_aspect = cols as f32 / rows as f32;
    (grid_aspect / target_aspect).ln().powi(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swapping_empty_active_workspaces_does_not_allocate_layout_trees() {
        let mut layouts = Layouts::default();
        layouts.set_active_workspace("DP-1", 4_000_000_001);
        layouts.set_active_workspace("HDMI-A-1", 4_000_000_002);

        layouts.swap_active("DP-1", "HDMI-A-1");

        assert!(layouts.trees.is_empty());
        assert_eq!(layouts.topology_revision, 0);
    }

    #[test]
    fn split_axis_auto_follows_aspect_ratio_but_bias_overrides_it() {
        let wide = area(1000, 500);
        let tall = area(500, 1000);

        assert_eq!(split_axis(wide, SplitBias::Auto), Axis::Horizontal);
        assert_eq!(split_axis(tall, SplitBias::Auto), Axis::Vertical);

        // A forced bias wins regardless of the area's own shape.
        assert_eq!(split_axis(tall, SplitBias::Horizontal), Axis::Horizontal);
        assert_eq!(split_axis(wide, SplitBias::Vertical), Axis::Vertical);
    }

    #[test]
    fn connected_resize_falloff_is_geometric_and_side_aware() {
        assert!((propagation_weight(0.5, 0) - 1.0).abs() < f32::EPSILON);
        assert!((propagation_weight(0.5, 1) - 0.5).abs() < f32::EPSILON);
        assert!((propagation_weight(0.5, 3) - 0.125).abs() < f32::EPSILON);

        assert_eq!(signed_resize_weight(0.5, None), 0.5);
        assert_eq!(signed_resize_weight(0.5, Some(Side::First)), 0.5);
        assert_eq!(signed_resize_weight(0.5, Some(Side::Second)), -0.5);
    }

    #[test]
    fn resize_handle_converts_stable_pixel_motion_into_ratio_motion() {
        let first = SplitResizeHandle {
            hit: SplitHit {
                output: "DP-1".to_string(),
                workspace: 1,
                path: Vec::new(),
                axis: Axis::Horizontal,
                area: area(1000, 800),
                root_area: area(1000, 800),
                target_side: Some(Side::First),
                topology_revision: 0,
            },
            start_ratio: 0.5,
            weight: 1.0,
        };
        assert!((first.ratio_for_delta(100.0).unwrap() - 0.6).abs() < f32::EPSILON);

        let second = SplitResizeHandle {
            weight: -0.5,
            ..first
        };
        assert!((second.ratio_for_delta(100.0).unwrap() - 0.45).abs() < f32::EPSILON);
    }

    fn area(w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((0, 0).into(), (w, h).into())
    }

    #[test]
    fn layout_master_rects_handles_zero_one_and_many_windows() {
        assert_eq!(
            layout_master_rects(0, area(1000, 1000), 0.5, MasterOrientation::Left),
            Vec::new()
        );

        // A lone window fills the whole area, same as BSP's lone leaf.
        assert_eq!(
            layout_master_rects(1, area(1000, 1000), 0.5, MasterOrientation::Left),
            vec![area(1000, 1000)]
        );

        let rects = layout_master_rects(3, area(1200, 900), 0.5, MasterOrientation::Left);
        assert_eq!(rects.len(), 3);
        // Master takes the configured fraction of the width, full height.
        assert_eq!(rects[0], Rectangle::new((0, 0).into(), (600, 900).into()));
        // The other two evenly split the remaining width's height, exactly
        // tiling it with no gap or overlap.
        assert_eq!(rects[1], Rectangle::new((600, 0).into(), (600, 450).into()));
        assert_eq!(
            rects[2],
            Rectangle::new((600, 450).into(), (600, 450).into())
        );
    }

    #[test]
    fn layout_master_rects_stack_division_has_no_rounding_gap_with_an_odd_split() {
        // 900 / 4 isn't exact -- the running-remainder approach must still
        // account for every pixel rather than leaving a gap after the last
        // stack window from a naively-rounded single division.
        let rects = layout_master_rects(5, area(1000, 901), 0.6, MasterOrientation::Left);
        let stack: Vec<_> = rects[1..].to_vec();
        let total_h: i32 = stack.iter().map(|r| r.size.h).sum();
        assert_eq!(total_h, 901);
        // Every stack rect starts exactly where the previous one ended.
        let mut y = 0;
        for rect in &stack {
            assert_eq!(rect.loc.y, y);
            y += rect.size.h;
        }
    }

    #[test]
    fn layout_master_rects_right_orientation_mirrors_left() {
        let rects = layout_master_rects(3, area(1200, 900), 0.5, MasterOrientation::Right);
        assert_eq!(rects.len(), 3);
        // Master now on the right instead of the left.
        assert_eq!(rects[0], Rectangle::new((600, 0).into(), (600, 900).into()));
        // Stack now on the left, still split evenly top/bottom.
        assert_eq!(rects[1], Rectangle::new((0, 0).into(), (600, 450).into()));
        assert_eq!(rects[2], Rectangle::new((0, 450).into(), (600, 450).into()));
    }

    #[test]
    fn layout_master_rects_top_bottom_stack_horizontally_not_vertically() {
        let top = layout_master_rects(3, area(1200, 900), 0.5, MasterOrientation::Top);
        assert_eq!(top.len(), 3);
        // Master on top, full width.
        assert_eq!(top[0], Rectangle::new((0, 0).into(), (1200, 450).into()));
        // Stack below, split evenly left/right (not top/bottom).
        assert_eq!(top[1], Rectangle::new((0, 450).into(), (600, 450).into()));
        assert_eq!(top[2], Rectangle::new((600, 450).into(), (600, 450).into()));

        let bottom = layout_master_rects(2, area(1000, 800), 0.25, MasterOrientation::Bottom);
        assert_eq!(bottom.len(), 2);
        // Master on the bottom this time, still full width.
        assert_eq!(
            bottom[0],
            Rectangle::new((0, 600).into(), (1000, 200).into())
        );
        // Lone stack window fills the remaining strip on top.
        assert_eq!(bottom[1], Rectangle::new((0, 0).into(), (1000, 600).into()));
    }

    #[test]
    fn layout_cascade_rects_handles_zero_and_one_window() {
        assert_eq!(layout_cascade_rects(0, area(1000, 1000)), Vec::new());
        assert_eq!(
            layout_cascade_rects(1, area(1000, 1000)),
            vec![area(1000, 1000)]
        );
    }

    #[test]
    fn layout_cascade_rects_puts_the_extra_window_in_the_first_row() {
        // Five windows on a 16:9-ish output should land on two rows, three
        // on top and two below -- the row with the "extra" leftover window
        // comes first, not last.
        let rects = layout_cascade_rects(5, area(1920, 1080));
        assert_eq!(rects.len(), 5);
        let top: Vec<_> = rects[..3].to_vec();
        let bottom: Vec<_> = rects[3..].to_vec();
        assert!(top.iter().all(|r| r.loc.y == 0));
        assert!(bottom.iter().all(|r| r.loc.y == top[0].size.h));
        assert_eq!(bottom.len(), 2);
    }

    #[test]
    fn layout_cascade_rects_tiles_exactly_with_no_gap_or_overlap() {
        // Every rect's area must sum to the whole output, for every window
        // count from one to a double-digit count, regardless of whether
        // divisions are exact.
        for count in 1..=13usize {
            let a = area(1917, 1073); // deliberately not evenly divisible
            let rects = layout_cascade_rects(count, a);
            assert_eq!(rects.len(), count);
            let total: i64 = rects
                .iter()
                .map(|r| (r.size.w as i64) * (r.size.h as i64))
                .sum();
            assert_eq!(total, (a.size.w as i64) * (a.size.h as i64));
        }
    }

    #[test]
    fn best_cascade_row_count_adapts_to_output_shape() {
        // A wide monitor prefers fewer, wider rows for the same five
        // windows that a narrow/portrait monitor spreads across more rows.
        // This is the property that comparing per-cell aspect to the
        // target would NOT give -- see `best_cascade_row_count`'s doc.
        let wide = best_cascade_row_count(5, 1920.0 / 1080.0);
        let ultrawide = best_cascade_row_count(5, 3440.0 / 1080.0);
        let portrait = best_cascade_row_count(5, 1080.0 / 1920.0);
        assert!(ultrawide <= wide);
        assert!(portrait >= wide);
    }

    #[test]
    fn cascade_row_item_counts_front_loads_the_remainder() {
        assert_eq!(cascade_row_item_counts(5, 2), vec![3, 2]);
        assert_eq!(cascade_row_item_counts(6, 2), vec![3, 3]);
        assert_eq!(cascade_row_item_counts(7, 3), vec![3, 2, 2]);
    }

    #[test]
    fn resolve_cascade_state_keeps_unchanged_rows_and_resets_changed_ones() {
        let a = area(1920, 1080);
        let target = a.size.w as f32 / a.size.h as f32;
        let mut existing = resolve_cascade_state(None, 5, target);
        assert_eq!(existing.row_counts, vec![3, 2]);

        // Manual resize: row 0 keeps 70% of the height, its three cells
        // split 50/30/20 instead of evenly.
        existing.row_ratios = vec![0.7, 0.3];
        existing.cell_ratios[0] = vec![0.5, 0.3, 0.2];

        // A sixth window changes the partition to two rows of three: row
        // 0's count (3) is unchanged, row 1's (2 -> 3) is not.
        let resolved = resolve_cascade_state(Some(&existing), 6, target);
        assert_eq!(resolved.row_counts, vec![3, 3]);

        // Row 0's *relative* share against row 1 still reflects the manual
        // 0.7:0.3-then-reset ratio -- normalization changes both values'
        // absolute size, but not their proportion to each other.
        let expected_ratio = 0.7 / 0.5;
        let actual_ratio = resolved.row_ratios[0] / resolved.row_ratios[1];
        assert!((actual_ratio - expected_ratio).abs() < 1e-4);

        // Row 0's cells were fully kept (already summed to 1.0, so
        // normalizing is a no-op); row 1 reset to equal thirds.
        assert_eq!(resolved.cell_ratios[0], vec![0.5, 0.3, 0.2]);
        assert_eq!(resolved.cell_ratios[1], vec![1.0 / 3.0; 3]);
    }

    #[test]
    fn apply_paired_ratio_clamps_within_the_pairs_combined_share() {
        let mut ratios = vec![0.2, 0.5, 0.3];
        // Pair (1, 2) has combined share 0.8. Push ratios[1] far past it.
        apply_paired_ratio(&mut ratios, 1, 10.0);
        assert_eq!(ratios[0], 0.2); // untouched, not part of this pair
        let combined = 0.5 + 0.3;
        assert!((ratios[1] - (combined - combined * 0.05)).abs() < 1e-5);
        // The pair's combined share is preserved, just redistributed.
        assert!((ratios[1] + ratios[2] - combined).abs() < 1e-6);
    }

    #[test]
    fn cascade_rects_from_state_honors_manual_ratios_and_still_tiles_exactly() {
        let a = area(1000, 1000);
        let mut state = resolve_cascade_state(None, 4, a.size.w as f32 / a.size.h as f32);
        // Force a known 2x2 partition regardless of what the scorer picked,
        // then apply an asymmetric manual split.
        state.row_counts = vec![2, 2];
        state.row_ratios = vec![0.75, 0.25];
        state.cell_ratios = vec![vec![0.5, 0.5], vec![0.5, 0.5]];

        let rects = cascade_rects_from_state(&state, a);
        assert_eq!(rects.len(), 4);
        assert_eq!(rects[0].size.h, 750); // top row gets 75% of height
        assert_eq!(rects[2].loc.y, 750); // bottom row starts right after
        assert_eq!(rects[2].size.h, 250); // remainder, no rounding gap

        let total: i64 = rects
            .iter()
            .map(|r| (r.size.w as i64) * (r.size.h as i64))
            .sum();
        assert_eq!(total, 1000 * 1000);
    }

    #[test]
    fn cascade_rects_exhaust_tiny_runtime_area_without_panicking() {
        let a = area(1, 1);
        let mut state = resolve_cascade_state(None, 9, 1.0);
        state.row_counts = vec![3, 3, 3];
        state.row_ratios = vec![1.0 / 3.0; 3];
        state.cell_ratios = vec![vec![1.0 / 3.0; 3]; 3];

        let rects = cascade_rects_from_state(&state, a);
        assert_eq!(rects.len(), 9);
        assert!(rects
            .iter()
            .all(|rect| rect.size.w >= 0 && rect.size.h >= 0));
        let total: i64 = rects
            .iter()
            .map(|rect| i64::from(rect.size.w) * i64::from(rect.size.h))
            .sum();
        assert_eq!(total, 1);
    }

    #[test]
    fn layouts_algorithm_and_master_ratio_default_until_overridden() {
        let mut layouts = Layouts::default();
        assert_eq!(layouts.algorithm("DP-1", 1), LayoutAlgorithm::Bsp);
        assert!((layouts.master_ratio("DP-1", 1) - 0.5).abs() < f32::EPSILON);

        layouts.set_algorithm("DP-1", 1, LayoutAlgorithm::Master);
        layouts.adjust_master_ratio("DP-1", 1, 0.1);
        assert_eq!(layouts.algorithm("DP-1", 1), LayoutAlgorithm::Master);
        assert!((layouts.master_ratio("DP-1", 1) - 0.6).abs() < f32::EPSILON);

        // A different workspace is unaffected.
        assert_eq!(layouts.algorithm("DP-1", 2), LayoutAlgorithm::Bsp);

        layouts.set_default_algorithm(LayoutAlgorithm::Master);
        assert_eq!(layouts.algorithm("DP-1", 2), LayoutAlgorithm::Master);
    }

    #[test]
    fn empty_cascade_refresh_does_not_retain_workspace_state() {
        let mut layouts = Layouts::default();
        let key = ("DP-1".to_string(), u32::MAX);
        layouts
            .cascade_state
            .insert(key.clone(), resolve_cascade_state(None, 4, 1.0));

        layouts.refresh_cascade_state("DP-1", u32::MAX, 0, 1.0);

        assert!(!layouts.cascade_state.contains_key(&key));
    }

    #[test]
    fn workspace_algorithm_override_sits_between_explicit_and_default() {
        let mut layouts = Layouts::default();
        layouts.set_default_algorithm(LayoutAlgorithm::Bsp);
        layouts.set_workspace_algorithm_overrides(HashMap::from([(5, LayoutAlgorithm::Cascade)]));

        // No explicit per-(output, workspace) override: the workspace_rule
        // default wins over the global default, on any output.
        assert_eq!(layouts.algorithm("DP-1", 5), LayoutAlgorithm::Cascade);
        assert_eq!(layouts.algorithm("HDMI-1", 5), LayoutAlgorithm::Cascade);
        // A workspace number with no workspace_rule falls through to the
        // global default, same as before this feature existed.
        assert_eq!(layouts.algorithm("DP-1", 6), LayoutAlgorithm::Bsp);

        // An explicit `layout:<algo>` action on one specific output still
        // wins over the workspace_rule default -- the user actively chose
        // something for *this* output's instance of the workspace.
        layouts.set_algorithm("DP-1", 5, LayoutAlgorithm::Master);
        assert_eq!(layouts.algorithm("DP-1", 5), LayoutAlgorithm::Master);
        assert_eq!(layouts.algorithm("HDMI-1", 5), LayoutAlgorithm::Cascade);
    }

    #[test]
    fn drain_for_migration_returns_empty_trees_and_clears_per_workspace_state() {
        let mut layouts = Layouts::default();
        // Empty trees are lazily-created stubs and must not be drained:
        // their per-workspace algorithm/ratio settings survive a Classic
        // -> Ocean -> Classic round trip this way.
        layouts
            .trees
            .insert(("DP-1".to_string(), 1), BspLayout::default());
        layouts.set_algorithm("DP-1", 1, LayoutAlgorithm::Master);
        layouts.adjust_master_ratio("DP-1", 1, 0.1);

        let drained = layouts.drain_for_migration();
        assert!(drained.is_empty());
        // The stub tree itself stays, as do its per-workspace settings.
        assert!(layouts.trees.contains_key(&("DP-1".to_string(), 1)));
        assert_eq!(layouts.algorithm("DP-1", 1), LayoutAlgorithm::Master);

        // Draining twice is a no-op.
        assert!(layouts.drain_for_migration().is_empty());
    }

    #[test]
    fn insert_migrated_tree_stores_the_tree_whole() {
        let mut layouts = Layouts::default();
        let tree = BspLayout::default();
        layouts.insert_migrated_tree("DP-1".to_string(), 3, tree);
        assert_eq!(layouts.window_count("DP-1", 3), 0);
        assert_eq!(layouts.algorithm("DP-1", 3), LayoutAlgorithm::Bsp);
        assert!(!layouts.trees.contains_key(&("DP-2".to_string(), 3)));
    }
}
