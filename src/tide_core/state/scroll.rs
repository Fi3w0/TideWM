use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle},
};

use crate::config::Direction;

use super::{is_scratchpad_workspace, Smallvil};

impl Smallvil {
    /// The tile under `point` on a Scroll strip and where a drop there
    /// lands, for a tiled drag of `moving` on `output`'s `workspace`.
    pub(crate) fn scroll_drop_target(
        &self,
        moving: &WlSurface,
        output: &str,
        workspace: u32,
        point: Point<f64, Logical>,
    ) -> Option<(WlSurface, crate::scroll::DropZone, Rectangle<i32, Logical>)> {
        if self.spatial.engine.projection() != Some(crate::config::WorkspaceProjection::Scroll) {
            return None;
        }
        let area = self.output_tiling_area(&self.output_by_name(output)?)?;
        let (window, rect) = self
            .spatial
            .workspaces
            .layout
            .layout(output, workspace, area, self.gaps_for(output, workspace))
            .into_iter()
            .find(|(_, rect)| rect.to_f64().contains(point))?;
        let target = window.toplevel()?.wl_surface().clone();
        if &target == moving
            || self.spatial.workspaces.layout.workspace_of(&target)
                != self.spatial.workspaces.layout.workspace_of(moving)
        {
            return None;
        }
        let zone = crate::scroll::drop_zone(rect, point, self.config.scroll.orientation);
        Some((target, zone, rect))
    }

    pub(super) fn queue_scroll_pointer_refresh(&mut self) {
        if self.spatial.engine.projection() != Some(crate::config::WorkspaceProjection::Scroll)
            || self.input.scroll_pointer_refresh_pending
        {
            return;
        }
        self.input.scroll_pointer_refresh_pending = true;
        // Retiling may run inside a pointer grab while its handle is locked.
        // Enter/leave must run after that callback releases the handle.
        self.loop_handle.insert_idle(|state| {
            state.input.scroll_pointer_refresh_pending = false;
            state.refresh_scroll_pointer_focus();
        });
    }

    /// Moving a strip or an Ocean camera changes the pointer target even with
    /// a stationary mouse. Keep keyboard focus selected by navigation; only
    /// refresh pointer routing.
    pub(crate) fn refresh_scroll_pointer_focus(&mut self) {
        if self.spatial.engine.projection() == Some(crate::config::WorkspaceProjection::Classic)
            || self.activation_blocked()
            || self
                .seat
                .get_pointer()
                .is_none_or(|pointer| pointer.is_grabbed())
        {
            return;
        }
        self.refresh_pointer_focus();
    }

    pub(crate) fn reveal_scroll_window(&mut self, surface: &WlSurface) {
        if self.spatial.engine.projection() != Some(crate::config::WorkspaceProjection::Scroll) {
            return;
        }
        if self.scroll_strip_floater(surface) {
            self.reveal_strip_floater(surface);
            return;
        }
        let Some(owner) = self
            .spatial
            .workspaces
            .layout
            .output_of(surface)
            .map(str::to_owned)
        else {
            return;
        };
        let Some(output) = self.output_by_name(&owner) else {
            return;
        };
        let Some(area) = self.output_tiling_area(&output) else {
            return;
        };
        let workspace = self.spatial.workspaces.layout.active_workspace(&owner);
        let gap = self.gaps_for(&owner, workspace);
        if self
            .spatial
            .workspaces
            .layout
            .scroll_reveal(surface, area, gap)
        {
            self.retile_filtered(false, None, Some(&output.name()));
        }
    }

    /// Scrolls the strip just far enough to show a strip-anchored floater
    /// that sits off-screen along it; the floater rides along through
    /// `anchor_scroll_floaters`. Bounded like any other pan.
    fn reveal_strip_floater(&mut self, surface: &WlSurface) {
        let Some(tag) = self.spatial.workspaces.floating_workspace.get(surface) else {
            return;
        };
        let (owner, workspace, window) = (tag.output.clone(), tag.workspace, tag.window.clone());
        if self.spatial.workspaces.layout.active_workspace(&owner) != workspace
            || self.fullscreen.contains_key(surface)
            || self.maximized.contains_key(surface)
        {
            return;
        }
        let (Some(output), Some(rect)) = (
            self.output_by_name(&owner),
            self.space.element_geometry(&window),
        ) else {
            return;
        };
        let Some(area) = self.output_tiling_area(&output) else {
            return;
        };
        let gap = self.gaps_for(&owner, workspace);
        let horizontal = self.config.scroll.orientation == crate::scroll::Orientation::Horizontal;
        let (start, size, origin, extent) = if horizontal {
            (rect.loc.x, rect.size.w, area.loc.x, area.size.w)
        } else {
            (rect.loc.y, rect.size.h, area.loc.y, area.size.h)
        };
        let shift = if start < origin {
            start - origin
        } else if start + size > origin + extent {
            (start + size - (origin + extent)).min(start - origin)
        } else {
            return;
        };
        let (offset, first, last) = self
            .spatial
            .workspaces
            .layout
            .scroll_reach(&owner, workspace, area, gap);
        let viewport = self.spatial.workspaces.layout.scroll_viewport(area, gap);
        let maximum = (last - viewport).max(first);
        let target = (offset + f64::from(shift)).clamp(first, maximum);
        if (target - offset).abs() < 0.5 {
            return;
        }
        let key = self.spatial.workspaces.layout.scroll_key(workspace);
        self.spatial
            .workspaces
            .layout
            .scroll
            .set_margins(&owner, key, false);
        self.spatial
            .workspaces
            .layout
            .scroll
            .set_offset(&owner, key, target);
        self.retile_filtered(false, None, Some(&owner));
        self.request_output_redraw(&output);
    }

    /// Whether `direction` runs along the Scroll strip, and its step (-1/1).
    pub(super) fn scroll_axis_step(&self, direction: Direction) -> (bool, isize) {
        let along = matches!(
            (self.config.scroll.orientation, direction),
            (
                crate::scroll::Orientation::Horizontal,
                Direction::Left | Direction::Right
            ) | (
                crate::scroll::Orientation::Vertical,
                Direction::Up | Direction::Down
            )
        );
        let step = if matches!(direction, Direction::Left | Direction::Up) {
            -1
        } else {
            1
        };
        (along, step)
    }

    /// The window Scroll focus moves to from `focused`. Along the strip it
    /// steps one column, across workspace sections too, and lands on the
    /// window most recently focused in that column; across the strip it
    /// steps one row inside the column.
    pub(super) fn scroll_focus_target(
        &mut self,
        focused: &WlSurface,
        direction: Direction,
    ) -> Option<WlSurface> {
        let owner = self
            .spatial
            .workspaces
            .layout
            .output_of(focused)?
            .to_owned();
        let output = self.output_by_name(&owner)?;
        let area = self.output_tiling_area(&output)?;
        let workspace = self.spatial.workspaces.layout.active_workspace(&owner);
        let gap = self.gaps_for(&owner, workspace);
        let strip: Vec<WlSurface> = self
            .spatial
            .workspaces
            .layout
            .layout(&owner, workspace, area, gap)
            .iter()
            .filter_map(|(w, _)| w.toplevel().map(|t| t.wl_surface().clone()))
            .collect();
        // Consecutive strip windows form one column while they are stacked
        // and share a workspace.
        let mut columns: Vec<Vec<WlSurface>> = Vec::new();
        for surface in strip {
            let joins = self.spatial.workspaces.layout.scroll.is_stacked(&surface)
                && columns.last().and_then(|c| c.last()).is_some_and(|prev| {
                    self.spatial.workspaces.layout.workspace_of(prev)
                        == self.spatial.workspaces.layout.workspace_of(&surface)
                });
            match columns.last_mut() {
                Some(column) if joins => column.push(surface),
                _ => columns.push(vec![surface]),
            }
        }
        let (column, row) = columns
            .iter()
            .enumerate()
            .find_map(|(c, col)| col.iter().position(|s| s == focused).map(|r| (c, r)))?;
        let (along, step) = self.scroll_axis_step(direction);
        if !along {
            return columns[column].get(row.checked_add_signed(step)?).cloned();
        }
        let target = columns.get(column.checked_add_signed(step)?)?;
        self.input
            .focus_history
            .iter()
            .find(|s| target.contains(s))
            .cloned()
            .or_else(|| target.get(row.min(target.len() - 1)).cloned())
    }

    /// Starting state for a modifier+right-drag on a Scroll tile, or `None`
    /// outside Scroll, for floating/fullscreen windows or without geometry.
    pub(crate) fn scroll_resize_start(
        &self,
        surface: &WlSurface,
    ) -> Option<crate::grabs::ScrollResizeStart> {
        if self.spatial.engine.projection() != Some(crate::config::WorkspaceProjection::Scroll)
            || !self.spatial.workspaces.layout.contains(surface)
            || self.fullscreen.contains_key(surface)
        {
            return None;
        }
        let owner = self
            .spatial
            .workspaces
            .layout
            .output_of(surface)?
            .to_owned();
        let workspace = self.spatial.workspaces.layout.workspace_of(surface)?;
        let output = self.output_by_name(&owner)?;
        let area = self.output_tiling_area(&output)?;
        let gap = self.gaps_for(&owner, workspace).max(0);
        let (column, _) = self.spatial.workspaces.layout.scroll_position(surface)?;
        let columns = self
            .spatial
            .workspaces
            .layout
            .scroll_columns(&owner, workspace);
        let column = columns.get(column)?;
        let horizontal = self.config.scroll.orientation == crate::scroll::Orientation::Horizontal;
        let viewport = f64::from(if horizontal { area.size.w } else { area.size.h } - 2 * gap);
        let row = crate::scroll::row_resize_enabled(column.windows.len(), self.config.scroll.stack)
            .then(|| {
                let size = self
                    .mapped_toplevel_window(surface)
                    .map(|w| w.geometry().size)
                    .unwrap_or_default();
                let cross = if horizontal { size.h } else { size.w };
                (
                    self.spatial.workspaces.layout.scroll.weight(surface),
                    f64::from(cross),
                )
            });
        Some(crate::grabs::ScrollResizeStart {
            head: column.windows.first()?.clone(),
            width: column.width,
            viewport,
            row,
            horizontal,
        })
    }

    /// Moves floating windows by however far the strip scrolled since the
    /// last retile when `scroll.float_anchor = strip`, so they stay on the
    /// strip instead of following the camera. Pinned windows and
    /// scratchpads keep their screen position.
    pub(super) fn anchor_scroll_floaters(&mut self, output: &str, workspace: u32) {
        if self.spatial.workspaces.layout.scroll.config.is_none() {
            return;
        }
        let key = self.spatial.workspaces.layout.scroll_key(workspace);
        let offset = self.spatial.workspaces.layout.scroll.offset(output, key);
        let Some(shift) = self
            .spatial
            .workspaces
            .layout
            .scroll
            .float_shift(output, key, offset)
        else {
            return;
        };
        if self.config.scroll.float_anchor != crate::scroll::FloatAnchor::Strip {
            return;
        }
        let delta: Point<i32, Logical> = match self.config.scroll.orientation {
            crate::scroll::Orientation::Horizontal => (shift, 0).into(),
            crate::scroll::Orientation::Vertical => (0, shift).into(),
        };
        let surfaces: Vec<WlSurface> = self
            .spatial
            .workspaces
            .floating_workspace
            .iter()
            .filter(|(surface, tag)| {
                tag.output == output
                    && self.spatial.workspaces.layout.scroll_key(tag.workspace) == key
                    && self.scroll_strip_floater(surface)
            })
            .map(|(surface, _)| surface.clone())
            .collect();
        for surface in surfaces {
            let old = self
                .spatial
                .workspaces
                .floating_workspace
                .get(&surface)
                .filter(|_| self.window_is_visible(&surface))
                .and_then(|tag| self.space.element_geometry(&tag.window));
            self.translate_floating_window(&surface, delta);
            let new = self
                .spatial
                .workspaces
                .floating_workspace
                .get(&surface)
                .and_then(|tag| self.space.element_geometry(&tag.window));
            if let (Some(old), Some(new)) = (old, new) {
                self.start_window_move_animation(&surface, old, new);
            }
        }
    }

    /// `scroll.float_rescue`: a visible strip floater lying wholly outside
    /// the part of the strip scrolling can still show (the columns around
    /// it closed) is moved onto the screen, nearest edge first, so it never
    /// becomes unreachable.
    pub(super) fn rescue_scroll_floaters(
        &mut self,
        output: &str,
        workspace: u32,
        area: Rectangle<i32, Logical>,
        gap: i32,
    ) {
        if !self.config.scroll.float_rescue
            || self.config.scroll.float_anchor != crate::scroll::FloatAnchor::Strip
            || self.spatial.workspaces.layout.scroll.config.is_none()
        {
            return;
        }
        let key = self.spatial.workspaces.layout.scroll_key(workspace);
        let (offset, first, last) = self
            .spatial
            .workspaces
            .layout
            .scroll_reach(output, workspace, area, gap);
        let horizontal = self.config.scroll.orientation == crate::scroll::Orientation::Horizontal;
        let (origin, extent) = if horizontal {
            (area.loc.x, area.size.w)
        } else {
            (area.loc.y, area.size.h)
        };
        let lead = f64::from(origin) + f64::from(gap.max(0));
        let surfaces: Vec<WlSurface> = self
            .spatial
            .workspaces
            .floating_workspace
            .iter()
            .filter(|(surface, tag)| {
                tag.output == output
                    && self.spatial.workspaces.layout.scroll_key(tag.workspace) == key
                    && self.scroll_strip_floater(surface)
                    && !self.fullscreen.contains_key(*surface)
                    && !self.maximized.contains_key(*surface)
                    && self.window_is_visible(surface)
            })
            .map(|(surface, _)| surface.clone())
            .collect();
        for surface in surfaces {
            let Some(window) = self
                .spatial
                .workspaces
                .floating_workspace
                .get(&surface)
                .map(|t| t.window.clone())
            else {
                continue;
            };
            let Some(rect) = self.space.element_geometry(&window) else {
                continue;
            };
            let (start, size) = if horizontal {
                (rect.loc.x, rect.size.w)
            } else {
                (rect.loc.y, rect.size.h)
            };
            let strip_start = f64::from(start) - lead + offset;
            if strip_start + f64::from(size) > first && strip_start < last {
                continue;
            }
            let low = origin;
            let high = (origin + extent - size).max(origin);
            let target = start.clamp(low, high);
            let delta: Point<i32, Logical> = if horizontal {
                (target - start, 0).into()
            } else {
                (0, target - start).into()
            };
            self.translate_floating_window(&surface, delta);
        }
    }

    /// A floating window that `scroll.float_anchor = strip` keeps on the
    /// strip: it may sit off-screen and belongs only to its own output.
    pub(crate) fn scroll_strip_floater(&self, surface: &WlSurface) -> bool {
        self.spatial.engine.projection() == Some(crate::config::WorkspaceProjection::Scroll)
            && self.config.scroll.float_anchor == crate::scroll::FloatAnchor::Strip
            && !self.spatial.workspaces.pinned.contains(surface)
            && self
                .spatial
                .workspaces
                .floating_workspace
                .get(surface)
                .is_some_and(|tag| !is_scratchpad_workspace(tag.workspace))
    }

    /// The bounds a floating window is kept inside: the output, unbounded
    /// along the strip for a strip-anchored Scroll floater.
    pub(super) fn floating_bounds(
        &self,
        surface: &WlSurface,
        full: Rectangle<i32, Logical>,
    ) -> Rectangle<i32, Logical> {
        if !self.scroll_strip_floater(surface) {
            return full;
        }
        const HALF: i32 = i32::MAX / 4;
        match self.config.scroll.orientation {
            crate::scroll::Orientation::Horizontal => {
                Rectangle::new((-HALF, full.loc.y).into(), (2 * HALF, full.size.h).into())
            }
            crate::scroll::Orientation::Vertical => {
                Rectangle::new((full.loc.x, -HALF).into(), (full.size.w, 2 * HALF).into())
            }
        }
    }

    /// Runtime counterpart of `scroll.float_anchor`; a reload restores the
    /// configured value.
    pub(crate) fn toggle_float_anchor(&mut self) {
        use crate::scroll::FloatAnchor;
        self.config.scroll.float_anchor = match self.config.scroll.float_anchor {
            FloatAnchor::Screen => FloatAnchor::Strip,
            FloatAnchor::Strip => FloatAnchor::Screen,
        };
        tracing::info!(anchor = ?self.config.scroll.float_anchor, "scroll.float_anchor toggled");
    }

    /// Retiles after a column edit and keeps the edited window in view.
    pub(super) fn scroll_columns_changed(&mut self, surface: &WlSurface) {
        self.retile();
        self.reveal_scroll_window(surface);
        self.request_redraw();
    }

    /// Runs one Scroll column action on the focused tiled window.
    pub(crate) fn column_action(&mut self, action: crate::config::ColumnAction) {
        if self.spatial.engine.projection() != Some(crate::config::WorkspaceProjection::Scroll) {
            return;
        }
        let Some(focused) = self.focused_window_surface() else {
            return;
        };
        if !self.spatial.workspaces.layout.contains(&focused) {
            return;
        }
        use crate::config::ColumnAction;
        let (owner, workspace) = match (
            self.spatial
                .workspaces
                .layout
                .output_of(&focused)
                .map(str::to_owned),
            self.spatial.workspaces.layout.workspace_of(&focused),
        ) {
            (Some(owner), Some(workspace)) => (owner, workspace),
            _ => return,
        };
        let area = self
            .output_by_name(&owner)
            .and_then(|output| self.output_tiling_area(&output));
        let gap = self.gaps_for(&owner, workspace);
        let columns = self
            .spatial
            .workspaces
            .layout
            .scroll_columns(&owner, workspace);
        let Some((column, _)) = self.spatial.workspaces.layout.scroll_position(&focused) else {
            return;
        };
        let head = columns[column].windows[0].clone();
        match action {
            ColumnAction::SwitchPresetWidth | ColumnAction::SwitchPresetWidthBack => {
                let step = if action == ColumnAction::SwitchPresetWidth {
                    crate::scroll::next_preset
                } else {
                    crate::scroll::previous_preset
                };
                if let Some(width) = step(
                    self.config.scroll.preset_widths.as_slice(),
                    columns[column].width,
                ) {
                    self.spatial.workspaces.layout.scroll.resize(head, width);
                    self.scroll_columns_changed(&focused);
                }
                return;
            }
            ColumnAction::SetWidth(change) => {
                if let Some(area) = area {
                    let viewport = self.spatial.workspaces.layout.scroll_viewport(area, gap);
                    let width = change.apply(columns[column].width, viewport);
                    self.spatial.workspaces.layout.scroll.resize(head, width);
                    self.scroll_columns_changed(&focused);
                }
                return;
            }
            ColumnAction::Maximize => {
                self.spatial
                    .workspaces
                    .layout
                    .scroll
                    .toggle_maximized(&head);
                self.scroll_columns_changed(&focused);
                return;
            }
            ColumnAction::Expand => {
                if let Some(width) = area.and_then(|area| {
                    self.spatial
                        .workspaces
                        .layout
                        .scroll_available_width(&focused, area, gap)
                }) {
                    self.spatial.workspaces.layout.scroll.resize(head, width);
                    self.scroll_columns_changed(&focused);
                }
                return;
            }
            ColumnAction::Center => {
                if let Some(area) = area {
                    if self.spatial.workspaces.layout.scroll_reveal_with(
                        &focused,
                        area,
                        gap,
                        Some(true),
                    ) {
                        self.retile();
                        self.request_redraw();
                    }
                }
                return;
            }
            ColumnAction::ToggleTabbed => {
                self.toggle_column_tabbed(&focused);
                return;
            }
            ColumnAction::FocusRowEnd { last } => {
                let windows = &columns[column].windows;
                let target = if last {
                    windows.last()
                } else {
                    windows.first()
                };
                if let Some(target) = target.filter(|target| **target != focused).cloned() {
                    self.activate_toplevel(&target);
                }
                return;
            }
            ColumnAction::FocusEnd { .. } | ColumnAction::FocusWrap { .. } => {
                let target = match action {
                    ColumnAction::FocusEnd { last: true } => columns.last(),
                    ColumnAction::FocusWrap { forward } if columns.len() > 1 => {
                        let count = columns.len();
                        columns.get(if forward {
                            (column + 1) % count
                        } else {
                            (column + count - 1) % count
                        })
                    }
                    ColumnAction::FocusWrap { .. } => None,
                    _ => columns.first(),
                };
                if let Some(target) = target {
                    let next = self
                        .input
                        .focus_history
                        .iter()
                        .find(|s| target.windows.contains(s))
                        .cloned()
                        .unwrap_or_else(|| target.windows[0].clone());
                    self.activate_toplevel(&next);
                }
                return;
            }
            _ => {}
        }
        // Column edits leave grouped and fullscreen windows where they are.
        let editable = !self.fullscreen.contains_key(&focused) && self.group_of(&focused).is_none();
        let changed = match action {
            _ if !editable
                && !matches!(
                    action,
                    ColumnAction::MoveToEnd { .. } | ColumnAction::MoveToIndex(_)
                ) =>
            {
                false
            }
            ColumnAction::MoveToIndex(index) => self
                .spatial
                .workspaces
                .layout
                .scroll_move_column_to(&focused, index - 1),
            ColumnAction::MoveToEnd { last } => self.spatial.workspaces.layout.scroll_move_column(
                &focused,
                if last { 1 } else { -1 },
                true,
            ),
            crate::config::ColumnAction::ConsumeOrExpel { forward } => self
                .spatial
                .workspaces
                .layout
                .scroll_consume_or_expel(&focused, forward),
            crate::config::ColumnAction::ConsumeInto => self
                .spatial
                .workspaces
                .layout
                .scroll_consume_into_column(&focused),
            crate::config::ColumnAction::Expel => self
                .spatial
                .workspaces
                .layout
                .scroll_expel_from_column(&focused),
            _ => false,
        };
        if changed {
            self.scroll_columns_changed(&focused);
        }
    }

    pub(crate) fn scroll_view(&mut self, direction: Direction) {
        if self.spatial.engine.projection() != Some(crate::config::WorkspaceProjection::Scroll)
            || self.activation_blocked()
        {
            return;
        }
        // Horizontal Scroll with a stack camera: Up/Down pans the focused
        // column vertically instead of doing nothing. Left/Right keep the
        // ordinary horizontal half-viewport pan below.
        if self.config.scroll.orientation == crate::scroll::Orientation::Horizontal
            && self.config.scroll.stack != crate::scroll::StackMode::Fit
            && matches!(direction, Direction::Up | Direction::Down)
        {
            let Some(focused) = self.focused_window_surface() else {
                return;
            };
            let Some(owner) = self
                .spatial
                .workspaces
                .layout
                .output_of(&focused)
                .map(str::to_owned)
            else {
                return;
            };
            let Some(output) = self.output_by_name(&owner) else {
                return;
            };
            let Some(area) = self.output_tiling_area(&output) else {
                return;
            };
            let gap = self.gaps_for(
                &owner,
                self.spatial.workspaces.layout.active_workspace(&owner),
            );
            let viewport = f64::from(area.size.h - 2 * gap).max(1.0);
            let delta = if direction == Direction::Down {
                viewport * 0.5
            } else {
                -viewport * 0.5
            };
            if self
                .spatial
                .workspaces
                .layout
                .scroll_stack_pan(&focused, area, gap, delta)
            {
                self.retile();
                self.request_redraw();
            }
            return;
        }
        let delta = match (self.config.scroll.orientation, direction) {
            (crate::scroll::Orientation::Horizontal, Direction::Left)
            | (crate::scroll::Orientation::Vertical, Direction::Up) => -0.5,
            (crate::scroll::Orientation::Horizontal, Direction::Right)
            | (crate::scroll::Orientation::Vertical, Direction::Down) => 0.5,
            _ => return,
        };
        let Some(output) = self.primary_output() else {
            return;
        };
        let Some(area) = self.output_tiling_area(&output) else {
            return;
        };
        let workspace = self
            .spatial
            .workspaces
            .layout
            .active_workspace(&output.name());
        let gap = self.gaps_for(&output.name(), workspace);
        self.spatial
            .workspaces
            .layout
            .scroll_pan(&output.name(), workspace, area, gap, delta);
        self.retile_filtered(false, None, Some(&output.name()));
        self.request_output_redraw(&output);
    }

    /// Pans the vertical stack camera of the column containing `surface`.
    /// Returns whether the offset changed.
    pub(crate) fn scroll_stack_wheel(&mut self, surface: &WlSurface, delta: f64) -> bool {
        let Some(owner) = self
            .spatial
            .workspaces
            .layout
            .output_of(surface)
            .map(str::to_owned)
        else {
            return false;
        };
        let Some(output) = self.output_by_name(&owner) else {
            return false;
        };
        let Some(area) = self.output_tiling_area(&output) else {
            return false;
        };
        let workspace = self.spatial.workspaces.layout.active_workspace(&owner);
        let gap = self.gaps_for(&owner, workspace);
        if self
            .spatial
            .workspaces
            .layout
            .scroll_stack_pan(surface, area, gap, delta)
        {
            self.retile_filtered(false, None, Some(&owner));
            self.request_output_redraw(&output);
            return true;
        }
        false
    }

    pub(crate) fn update_scroll_gesture(
        &mut self,
        output_name: &str,
        workspace: u32,
        delta_x: f64,
        delta_y: f64,
    ) {
        if self.spatial.engine.projection() != Some(crate::config::WorkspaceProjection::Scroll)
            || self.activation_blocked()
        {
            return;
        }
        let Some(output) = self.output_by_name(output_name) else {
            return;
        };
        if self.spatial.workspaces.layout.active_workspace(output_name) != workspace {
            return;
        }
        let Some(area) = self.output_tiling_area(&output) else {
            return;
        };
        let gap = self.gaps_for(output_name, workspace);
        let delta = match self.config.scroll.orientation {
            crate::scroll::Orientation::Horizontal => delta_x,
            crate::scroll::Orientation::Vertical => delta_y,
        } / self.config.scroll.gesture_distance;
        let delta = if self.config.scroll.gesture_invert {
            -delta
        } else {
            delta
        };
        self.spatial
            .workspaces
            .layout
            .scroll_pan(output_name, workspace, area, gap, delta);
        self.retile_filtered(false, None, Some(&output.name()));
        self.request_output_redraw(&output);
    }

    pub(crate) fn finish_scroll_gesture(
        &mut self,
        output_name: &str,
        workspace: u32,
        initial_offset: f64,
        cancelled: bool,
    ) {
        if self.spatial.engine.projection() != Some(crate::config::WorkspaceProjection::Scroll) {
            return;
        }
        let Some(output) = self.output_by_name(output_name) else {
            return;
        };
        if self.spatial.workspaces.layout.active_workspace(output_name) != workspace {
            return;
        }
        if cancelled || self.activation_blocked() {
            self.spatial.workspaces.layout.scroll.set_offset(
                output_name,
                self.spatial.workspaces.layout.scroll_key(workspace),
                initial_offset,
            );
            self.retile_filtered(false, None, Some(&output.name()));
            self.request_output_redraw(&output);
            return;
        }
        let Some(area) = self.output_tiling_area(&output) else {
            return;
        };
        let gap = self.gaps_for(output_name, workspace);
        let target = self
            .spatial
            .workspaces
            .layout
            .layout(output_name, workspace, area, gap)
            .into_iter()
            .filter_map(|(window, rect)| {
                let intersection = rect.intersection(area)?;
                let visible = i64::from(intersection.size.w) * i64::from(intersection.size.h);
                Some((window, visible))
            })
            .max_by_key(|(_, visible)| *visible)
            .map(|(window, _)| window);
        if let Some(window) = target {
            if let Some(toplevel) = window.toplevel() {
                self.activate_toplevel(toplevel.wl_surface());
            }
        }
    }
}
