//! A dynamic BSP (binary space partitioning) tiling layout, in the spirit of
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

use std::collections::{HashMap, HashSet};

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
}

impl BspLayout {
    fn is_empty(&self) -> bool {
        self.root.is_none()
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
    }

    /// Removes the window backed by `surface`, collapsing its sibling up
    /// into its former parent's place. No-op if `surface` isn't tiled.
    pub fn remove(&mut self, surface: &WlSurface) {
        self.root = self.root.take().and_then(|root| remove_from(root, surface));
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
    pub fn replace_leaf(&mut self, old: &WlSurface, new_window: &Window) {
        if let Some(root) = &mut self.root {
            replace_leaf(root, old, new_window);
        }
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
    /// from the root down, and recursion only descends into whichever
    /// child's area contains `point`, so this always finds the *closest*
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
                collect_resize_splits(root, target, area, area, bias, Vec::new(), &mut found);
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
}

/// One independent `BspLayout` tree per (output, workspace) pair, plus which
/// workspace is currently active (visible) on each output. Workspaces are
/// lazily created, not pre-declared -- a tree springs into existence the
/// first time something is inserted into it, same as `by_output` used to
/// work with just outputs, and an output not yet in `active` defaults to
/// workspace `1` (see `active_workspace`).
///
/// A handful of operations (`contains`, `output_of`, `remove`, `swap`) search
/// or act across every workspace rather than taking one explicitly: a window
/// is still "tiled" even while its workspace is hidden, and removing a
/// destroyed window or swapping two on-screen neighbors doesn't need the
/// caller to know which workspace it's asking about. Operations that render
/// or address one specific tree (`insert`, `layout`, `hit_test_split`,
/// `ratio_at`, `set_ratio`) take `workspace` explicitly instead.
#[derive(Default)]
pub struct Layouts {
    trees: HashMap<(String, u32), BspLayout>,
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
    /// Per-(output, workspace) master/stack split fraction (master mode
    /// only; meaningless, and ignored, under BSP). Defaults to 0.5 for any
    /// key not present. Pruned the same way and for the same reason as
    /// `algorithms`.
    master_ratio: HashMap<(String, u32), f32>,
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
        self.trees.values().any(|l| l.contains(surface))
    }

    /// The name of the output whose tree currently holds `surface`, if any,
    /// regardless of whether that tree's workspace is the active one.
    pub fn output_of(&self, surface: &WlSurface) -> Option<&str> {
        self.trees
            .iter()
            .find(|(_, l)| l.contains(surface))
            .map(|((name, _), _)| name.as_str())
    }

    /// The workspace number of whichever tree currently holds `surface`, if
    /// any, regardless of whether that tree's workspace is the active one.
    /// Used to tell "already on the active workspace" apart from "tiled,
    /// but on some other hidden one" -- the two look identical if you only
    /// ever compare against `active_workspace`.
    pub fn workspace_of(&self, surface: &WlSurface) -> Option<u32> {
        self.key_of(surface).map(|(_, workspace)| workspace)
    }

    /// The `Window` handle backing `surface`, wherever it's tiled. Unlike
    /// looking it up through `Smallvil::space`, this finds it even while
    /// hidden on a non-active workspace -- a hidden tiled window is still
    /// held by its tree, just not mapped, so `space.elements()` won't have
    /// it anymore (see `Smallvil::switch_workspace`'s own note on this).
    pub fn window_of(&self, surface: &WlSurface) -> Option<Window> {
        self.trees.values().find_map(|l| l.window(surface))
    }

    fn key_of(&self, surface: &WlSurface) -> Option<(String, u32)> {
        self.trees
            .iter()
            .find(|(_, l)| l.contains(surface))
            .map(|(key, _)| key.clone())
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
        self.trees
            .entry((output.to_string(), workspace))
            .or_default()
            .insert(window, target);
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
            .filter(|(_, tree)| !tree.windows().is_empty())
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
            self.trees.insert((output_a.to_string(), ws_a), tree_b);
        }
        if !tree_a.is_empty() {
            self.trees.insert((output_b.to_string(), ws_b), tree_a);
        }
        if changed {
            self.bump_topology_revision();
        }
    }

    /// Removes the window backed by `surface` from whichever tree holds it.
    /// No-op if `surface` isn't tiled anywhere.
    pub fn remove(&mut self, surface: &WlSurface) {
        let changed = self.contains(surface);
        for layout in self.trees.values_mut() {
            layout.remove(surface);
        }
        // Workspace IDs can come from IPC, so retaining an empty tree for
        // every ID a window ever visited creates unbounded memory growth and
        // progressively slows all ownership scans.
        self.trees.retain(|_, layout| !layout.is_empty());
        // `algorithms`/`master_ratio` are keyed the same way and reachable
        // through the same IPC action surface (see their own field docs) --
        // prune them alongside `trees` so they can't outlive it.
        let live_keys: HashSet<(String, u32)> = self.trees.keys().cloned().collect();
        prune_orphaned(&mut self.algorithms, &live_keys);
        prune_orphaned(&mut self.master_ratio, &live_keys);
        if changed {
            self.bump_topology_revision();
        }
    }

    /// The tiling algorithm active for `output`'s `workspace`: an explicit
    /// override if one was ever set (`set_algorithm`), else the configured
    /// default.
    pub fn algorithm(&self, output: &str, workspace: u32) -> LayoutAlgorithm {
        self.algorithms
            .get(&(output.to_string(), workspace))
            .copied()
            .unwrap_or(self.default_algorithm)
    }

    /// Overrides `output`'s `workspace` to use `algorithm` instead of the
    /// configured default (see `algorithm`'s own doc, and this struct's
    /// `algorithms` field doc for the pruning this participates in).
    pub fn set_algorithm(&mut self, output: &str, workspace: u32, algorithm: LayoutAlgorithm) {
        self.algorithms
            .insert((output.to_string(), workspace), algorithm);
    }

    /// Sets the fallback `algorithm` uses for any (output, workspace)
    /// without its own override. Called once at startup and again on every
    /// config reload (`Smallvil::new`/`reload_config`) -- not cached inside
    /// `Config` and read fresh each `retile()`, since `Layouts` (not
    /// `Config`) is what actually needs it at layout time.
    pub fn set_default_algorithm(&mut self, algorithm: LayoutAlgorithm) {
        self.default_algorithm = algorithm;
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
        if let Some(layout) = self.trees.get_mut(&key) {
            layout.replace_leaf(old, new_window);
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
        }
    }

    /// Finds the split boundary nearest `point` within `output`'s `workspace`
    /// tree; see `BspLayout::hit_test_split`. Always `None` under master
    /// mode: master's geometry (`layout_master`) ignores the tree's own
    /// shape entirely, so a BSP split boundary found here wouldn't
    /// correspond to any actual border on screen to drag.
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
    /// master mode, same reasoning as `hit_test_split`.
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

/// Drops any entry whose (output, workspace) key isn't in `live_keys` --
/// shared by `Layouts::remove` for both `algorithms` and `master_ratio`,
/// and independently testable without needing a real tree/`Window` to
/// actually empty (see this module's tests).
fn prune_orphaned<T>(
    overrides: &mut HashMap<(String, u32), T>,
    live_keys: &HashSet<(String, u32)>,
) {
    overrides.retain(|key, _| live_keys.contains(key));
}

fn insert_into(root: Node, window: Window, target: Option<&WlSurface>) -> Node {
    match root {
        Node::Leaf(existing) => Node::Split {
            ratio: 0.5,
            first: Box::new(Node::Leaf(existing)),
            second: Box::new(Node::Leaf(window)),
        },
        Node::Split {
            ratio,
            first,
            second,
        } => {
            if target.is_some_and(|t| node_contains(&first, t)) {
                Node::Split {
                    ratio,
                    first: Box::new(insert_into(*first, window, target)),
                    second,
                }
            } else {
                // Either `second` contains the target, or there is no
                // target (nothing focused / not tiled): either way, default
                // to descending into `second` so windows stack predictably.
                Node::Split {
                    ratio,
                    first,
                    second: Box::new(insert_into(*second, window, target)),
                }
            }
        }
    }
}

fn remove_from(node: Node, target: &WlSurface) -> Option<Node> {
    match node {
        Node::Leaf(window) => {
            if is_window(&window, target) {
                None
            } else {
                Some(Node::Leaf(window))
            }
        }
        Node::Split {
            ratio,
            first,
            second,
        } => {
            let first = remove_from(*first, target);
            let second = remove_from(*second, target);
            match (first, second) {
                (Some(first), Some(second)) => Some(Node::Split {
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(surviving), None) | (None, Some(surviving)) => Some(surviving),
                (None, None) => None,
            }
        }
    }
}

fn node_contains(node: &Node, target: &WlSurface) -> bool {
    match node {
        Node::Leaf(window) => is_window(window, target),
        Node::Split { first, second, .. } => {
            node_contains(first, target) || node_contains(second, target)
        }
    }
}

fn is_window(window: &Window, target: &WlSurface) -> bool {
    window
        .toplevel()
        .map(|t| t.wl_surface() == target)
        .unwrap_or(false)
}

fn find_window(node: &Node, target: &WlSurface) -> Option<Window> {
    match node {
        Node::Leaf(window) => is_window(window, target).then(|| window.clone()),
        Node::Split { first, second, .. } => {
            find_window(first, target).or_else(|| find_window(second, target))
        }
    }
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
    match node {
        Node::Leaf(window) => {
            if is_window(window, a) {
                *window = window_b.clone();
            } else if is_window(window, b) {
                *window = window_a.clone();
            }
        }
        Node::Split { first, second, .. } => {
            swap_leaves(first, a, window_a, b, window_b);
            swap_leaves(second, a, window_a, b, window_b);
        }
    }
}

/// Single-sided counterpart to `swap_leaves`: overwrites the leaf currently
/// holding `old` with `new_window`, unconditionally (`new_window` need not
/// already be part of this tree).
fn replace_leaf(node: &mut Node, old: &WlSurface, new_window: &Window) {
    match node {
        Node::Leaf(window) if is_window(window, old) => *window = new_window.clone(),
        Node::Leaf(_) => {}
        Node::Split { first, second, .. } => {
            replace_leaf(first, old, new_window);
            replace_leaf(second, old, new_window);
        }
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
    node: &Node,
    area: Rectangle<i32, Logical>,
    bias: SplitBias,
    target_path: &[Side],
    path: Vec<Side>,
    out: &mut Vec<PathSplit>,
) {
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
    let Some(side) = target_path.first().copied() else {
        return;
    };
    let (first_area, second_area) = split(area, *ratio, bias);
    let mut child_path = path;
    child_path.push(side);
    match side {
        Side::First => {
            collect_split_path(first, first_area, bias, &target_path[1..], child_path, out)
        }
        Side::Second => collect_split_path(
            second,
            second_area,
            bias,
            &target_path[1..],
            child_path,
            out,
        ),
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
    area: Rectangle<i32, Logical>,
    root_area: Rectangle<i32, Logical>,
    bias: SplitBias,
    mut path: Vec<Side>,
    found: &mut [Option<SplitHit>; 2],
) {
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

    let target_side = if node_contains(first, target) {
        Some(Side::First)
    } else if node_contains(second, target) {
        Some(Side::Second)
    } else {
        None
    };
    let hit = SplitHit {
        output: String::new(),
        workspace: 0,
        path: path.clone(),
        axis,
        area,
        root_area,
        target_side,
        topology_revision: 0,
    };
    match axis {
        Axis::Horizontal => found[0] = Some(hit),
        Axis::Vertical => found[1] = Some(hit),
    }

    if target_side == Some(Side::First) {
        path.push(Side::First);
        collect_resize_splits(first, target, first_area, root_area, bias, path, found);
    } else if target_side == Some(Side::Second) {
        path.push(Side::Second);
        collect_resize_splits(second, target, second_area, root_area, bias, path, found);
    }
}

fn hit_test(
    node: &Node,
    area: Rectangle<i32, Logical>,
    root_area: Rectangle<i32, Logical>,
    gap: i32,
    point: Point<f64, Logical>,
    bias: SplitBias,
    mut path: Vec<Side>,
) -> Option<SplitHit> {
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
        // `output`/`workspace` are filled in by `Layouts::hit_test_split`,
        // the only caller with that context -- a bare `BspLayout` doesn't
        // know which output or workspace it belongs to.
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
        hit_test(first, first_area, root_area, gap, point, bias, path)
    } else {
        path.push(Side::Second);
        hit_test(second, second_area, root_area, gap, point, bias, path)
    }
}

fn rect_contains(rect: Rectangle<i32, Logical>, point: Point<f64, Logical>) -> bool {
    point.x >= rect.loc.x as f64
        && point.x < (rect.loc.x + rect.size.w) as f64
        && point.y >= rect.loc.y as f64
        && point.y < (rect.loc.y + rect.size.h) as f64
}

fn ratio_at(node: &Node, path: &[Side]) -> Option<f32> {
    let Node::Split {
        ratio,
        first,
        second,
    } = node
    else {
        return None;
    };
    match path.first() {
        None => Some(*ratio),
        Some(Side::First) => ratio_at(first, &path[1..]),
        Some(Side::Second) => ratio_at(second, &path[1..]),
    }
}

fn set_ratio_at(node: &mut Node, path: &[Side], ratio: f32) {
    let Node::Split {
        ratio: r,
        first,
        second,
    } = node
    else {
        return;
    };
    match path.first() {
        None => *r = ratio,
        Some(Side::First) => set_ratio_at(first, &path[1..], ratio),
        Some(Side::Second) => set_ratio_at(second, &path[1..], ratio),
    }
}

fn collect(
    node: &Node,
    area: Rectangle<i32, Logical>,
    bias: SplitBias,
    out: &mut Vec<(Window, Rectangle<i32, Logical>)>,
) {
    match node {
        Node::Leaf(window) => out.push((window.clone(), area)),
        Node::Split {
            ratio,
            first,
            second,
        } => {
            let (first_area, second_area) = split(area, *ratio, bias);
            collect(first, first_area, bias, out);
            collect(second, second_area, bias, out);
        }
    }
}

fn collect_windows(node: &Node, out: &mut Vec<Window>) {
    match node {
        Node::Leaf(window) => out.push(window.clone()),
        Node::Split { first, second, .. } => {
            collect_windows(first, out);
            collect_windows(second, out);
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
    Rectangle::new(
        (rect.loc.x + gap, rect.loc.y + gap).into(),
        (
            (rect.size.w - gap * 2).max(1),
            (rect.size.h - gap * 2).max(1),
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
    fn prune_orphaned_drops_only_keys_with_no_live_tree() {
        // Exercises the same logic `Layouts::remove` runs on `algorithms`/
        // `master_ratio` after a tree empties, without needing a real
        // `Window`/`WlSurface` (Smithay has no lightweight way to construct
        // one outside a real Wayland client connection -- see this
        // codebase's other tests, e.g. `handlers/xdg_shell.rs`, which
        // extract pure logic out for the same reason).
        let mut overrides = HashMap::new();
        overrides.insert(("DP-1".to_string(), 1), LayoutAlgorithm::Master);
        overrides.insert(("DP-1".to_string(), 2), LayoutAlgorithm::Master);
        let mut live_keys = HashSet::new();
        live_keys.insert(("DP-1".to_string(), 2));

        prune_orphaned(&mut overrides, &live_keys);

        assert_eq!(overrides.len(), 1);
        assert!(overrides.contains_key(&("DP-1".to_string(), 2)));
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
}
