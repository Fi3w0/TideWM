use super::*;
use crate::scroll::{self, Orientation, WorkspaceMode};

/// One window (or an empty workspace section) placed on the strip. Every
/// row of a column shares the column's `start`/`extent`.
#[derive(Clone)]
struct ScrollSlot {
    workspace: u32,
    window: Option<Window>,
    start: f64,
    extent: f64,
    row: usize,
    weights: Vec<f64>,
    /// First surface of the column (the head), used to key its vertical
    /// stack camera. `None` for an empty workspace section.
    head: Option<WlSurface>,
}

/// A column as an ordered list of its windows plus its width (the head's
/// proportion of the viewport).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScrollColumn {
    pub(crate) windows: Vec<WlSurface>,
    pub(crate) width: f64,
}

fn surface_of(window: &Window) -> Option<WlSurface> {
    window.toplevel().map(|t| t.wl_surface().clone())
}

impl Layouts {
    pub(crate) fn scroll_key(&self, workspace: u32) -> u32 {
        if self
            .scroll
            .config
            .is_some_and(|cfg| cfg.workspaces == WorkspaceMode::Sections)
            && !crate::state::is_scratchpad_workspace(workspace)
        {
            0
        } else {
            workspace
        }
    }

    /// The windows of one workspace grouped into columns, in strip order.
    fn window_columns(&self, output: &str, workspace: u32) -> Vec<Vec<Window>> {
        let mut columns: Vec<Vec<Window>> = Vec::new();
        for window in self.windows_in(output, workspace) {
            let stacked = surface_of(&window).is_some_and(|s| self.scroll.is_stacked(&s));
            match columns.last_mut() {
                Some(column) if stacked => column.push(window),
                _ => columns.push(vec![window]),
            }
        }
        columns
    }

    /// Columns of `output`/`workspace` as surfaces plus widths.
    pub(crate) fn scroll_columns(&self, output: &str, workspace: u32) -> Vec<ScrollColumn> {
        self.window_columns(output, workspace)
            .into_iter()
            .map(|column| {
                let windows: Vec<WlSurface> = column.iter().filter_map(surface_of).collect();
                let width = windows
                    .first()
                    .map(|head| self.scroll.proportion(head))
                    .unwrap_or_else(|| self.scroll.config.unwrap_or_default().tile_proportion);
                ScrollColumn { windows, width }
            })
            .filter(|column| !column.windows.is_empty())
            .collect()
    }

    /// Rewrites one workspace's window order and stacking to `columns`. The
    /// tree keeps its leaves; only their contents move, through swaps.
    pub(crate) fn set_scroll_columns(
        &mut self,
        output: &str,
        workspace: u32,
        columns: Vec<ScrollColumn>,
    ) {
        let key = (output.to_string(), workspace);
        let target: Vec<WlSurface> = columns
            .iter()
            .flat_map(|column| column.windows.iter().cloned())
            .collect();
        let mut current: Vec<WlSurface> = self
            .windows_in(output, workspace)
            .iter()
            .filter_map(surface_of)
            .collect();
        if target.len() != current.len() || target.iter().any(|s| !current.contains(s)) {
            return;
        }
        if let Some(tree) = self.trees.get_mut(&key) {
            for index in 0..target.len() {
                if current[index] != target[index] {
                    let Some(from) = current.iter().position(|s| s == &target[index]) else {
                        continue;
                    };
                    tree.swap(&current[index], &current[from]);
                    current.swap(index, from);
                }
            }
        }
        for column in &columns {
            for (row, surface) in column.windows.iter().enumerate() {
                self.scroll.set_stacked(surface, row > 0);
                if row > 0 {
                    self.scroll.clear_proportion(surface);
                }
            }
            if let Some(head) = column.windows.first() {
                self.scroll.resize(head.clone(), column.width);
            }
        }
        self.bump_topology_revision();
    }

    /// `(column, row)` of `surface` within its workspace.
    pub(crate) fn scroll_position(&self, surface: &WlSurface) -> Option<(usize, usize)> {
        let (output, workspace) = self.key_of(surface)?;
        self.scroll_columns(&output, workspace)
            .iter()
            .enumerate()
            .find_map(|(column, c)| {
                c.windows
                    .iter()
                    .position(|s| s == surface)
                    .map(|row| (column, row))
            })
    }

    /// Applies `edit` to the columns of `surface`'s workspace and writes the
    /// result back when it returns true.
    fn edit_scroll_columns(
        &mut self,
        surface: &WlSurface,
        edit: impl FnOnce(&mut Vec<ScrollColumn>, usize, usize, f64) -> bool,
    ) -> bool {
        let Some((output, workspace)) = self.key_of(surface) else {
            return false;
        };
        let mut columns = self.scroll_columns(&output, workspace);
        let Some((column, row)) = columns.iter().enumerate().find_map(|(c, col)| {
            col.windows
                .iter()
                .position(|s| s == surface)
                .map(|r| (c, r))
        }) else {
            return false;
        };
        let default_width = self.scroll.config.unwrap_or_default().tile_proportion;
        if !edit(&mut columns, column, row, default_width) {
            return false;
        }
        columns.retain(|c| !c.windows.is_empty());
        self.set_scroll_columns(&output, workspace, columns);
        true
    }

    /// niri's consume-or-expel: a window sharing a column leaves it for a new
    /// column on that side; a window alone joins the neighbor column.
    pub(crate) fn scroll_consume_or_expel(&mut self, surface: &WlSurface, forward: bool) -> bool {
        self.edit_scroll_columns(surface, |columns, column, row, default_width| {
            if columns[column].windows.len() > 1 {
                let window = columns[column].windows.remove(row);
                let at = if forward { column + 1 } else { column };
                columns.insert(
                    at,
                    ScrollColumn {
                        windows: vec![window],
                        width: default_width,
                    },
                );
                return true;
            }
            let Some(target) = (if forward {
                column.checked_add(1)
            } else {
                column.checked_sub(1)
            })
            .filter(|target| *target < columns.len()) else {
                return false;
            };
            let window = columns[column].windows.remove(0);
            columns[target].windows.push(window);
            true
        })
    }

    /// Pulls the first window of the next column into this one.
    pub(crate) fn scroll_consume_into_column(&mut self, surface: &WlSurface) -> bool {
        self.edit_scroll_columns(surface, |columns, column, _, _| {
            if column + 1 >= columns.len() {
                return false;
            }
            let window = columns[column + 1].windows.remove(0);
            columns[column].windows.push(window);
            true
        })
    }

    /// Moves a window out of a shared column into a new column right after.
    pub(crate) fn scroll_expel_from_column(&mut self, surface: &WlSurface) -> bool {
        self.edit_scroll_columns(surface, |columns, column, row, default_width| {
            if columns[column].windows.len() < 2 {
                return false;
            }
            let window = columns[column].windows.remove(row);
            columns.insert(
                column + 1,
                ScrollColumn {
                    windows: vec![window],
                    width: default_width,
                },
            );
            true
        })
    }

    /// Moves the whole column by `delta` places along the strip, or to the
    /// first/last place when `to_end` is set.
    pub(crate) fn scroll_move_column(
        &mut self,
        surface: &WlSurface,
        delta: isize,
        to_end: bool,
    ) -> bool {
        self.edit_scroll_columns(surface, |columns, column, _, _| {
            let last = columns.len() - 1;
            let target = if to_end {
                if delta < 0 {
                    0
                } else {
                    last
                }
            } else {
                match column.checked_add_signed(delta) {
                    Some(target) if target <= last => target,
                    _ => return false,
                }
            };
            if target == column {
                return false;
            }
            let moved = columns.remove(column);
            columns.insert(target, moved);
            true
        })
    }

    /// Moves a window up or down inside its column.
    pub(crate) fn scroll_move_in_column(&mut self, surface: &WlSurface, delta: isize) -> bool {
        self.edit_scroll_columns(surface, |columns, column, row, _| {
            let rows = columns[column].windows.len();
            match row.checked_add_signed(delta) {
                Some(target) if target < rows => {
                    columns[column].windows.swap(row, target);
                    true
                }
                _ => false,
            }
        })
    }

    /// Drops `moving` relative to `target` (same workspace) per `zone`.
    /// `Swap` is left to `Layouts::swap`.
    pub(crate) fn scroll_drop(
        &mut self,
        moving: &WlSurface,
        target: &WlSurface,
        zone: scroll::DropZone,
    ) -> bool {
        if moving == target || self.key_of(moving) != self.key_of(target) {
            return false;
        }
        let target = target.clone();
        self.edit_scroll_columns(moving, |columns, column, row, default_width| {
            let alone = columns[column].windows.len() == 1;
            let width = if alone {
                columns[column].width
            } else {
                default_width
            };
            let window = columns[column].windows.remove(row);
            columns.retain(|c| !c.windows.is_empty());
            let Some((at, at_row)) = columns.iter().enumerate().find_map(|(c, col)| {
                col.windows
                    .iter()
                    .position(|s| s == &target)
                    .map(|r| (c, r))
            }) else {
                return false;
            };
            let single = |window| ScrollColumn {
                windows: vec![window],
                width,
            };
            match zone {
                scroll::DropZone::ColumnBefore => columns.insert(at, single(window)),
                scroll::DropZone::ColumnAfter => columns.insert(at + 1, single(window)),
                scroll::DropZone::Before => columns[at].windows.insert(at_row, window),
                scroll::DropZone::After => columns[at].windows.insert(at_row + 1, window),
                scroll::DropZone::Swap => return false,
            }
            true
        })
    }

    /// Where a new window goes when `target` is focused: after the last
    /// window of `target`'s column, so a new window never splits a column.
    pub(crate) fn scroll_insert_target(&self, target: &WlSurface) -> WlSurface {
        let Some((output, workspace)) = self.key_of(target) else {
            return target.clone();
        };
        self.scroll_columns(&output, workspace)
            .into_iter()
            .find(|column| column.windows.contains(target))
            .and_then(|column| column.windows.last().cloned())
            .unwrap_or_else(|| target.clone())
    }

    /// Keeps a column intact when its head leaves: the next window becomes
    /// the head and inherits the column width. Call before removing.
    pub(crate) fn scroll_detach(&mut self, surface: &WlSurface) {
        let Some((output, workspace)) = self.key_of(surface) else {
            return;
        };
        let order: Vec<WlSurface> = self
            .windows_in(&output, workspace)
            .iter()
            .filter_map(surface_of)
            .collect();
        let Some(index) = order.iter().position(|s| s == surface) else {
            return;
        };
        if !self.scroll.is_stacked(surface) {
            if let Some(next) = order.get(index + 1).filter(|n| self.scroll.is_stacked(n)) {
                let next = next.clone();
                self.scroll.set_stacked(&next, false);
                if self.scroll.has_proportion(surface) {
                    let width = self.scroll.proportion(surface);
                    self.scroll.resize(next.clone(), width);
                }
                self.scroll.transfer_stack_offset(surface, &next);
            } else {
                self.scroll.clear_stack_offset(surface);
            }
        }
    }

    /// Heights, viewport and overflow bound for `surface`'s column, plus the
    /// column head and row. `None` outside an overflowing horizontal column.
    fn scroll_stack_metrics(
        &self,
        surface: &WlSurface,
        area: Rectangle<i32, Logical>,
        gap: i32,
    ) -> Option<(WlSurface, usize, Vec<f64>, f64, f64, i32)> {
        let cfg = self.scroll.config?;
        if cfg.orientation != crate::scroll::Orientation::Horizontal
            || cfg.stack == crate::scroll::StackMode::Fit
        {
            return None;
        }
        let (output, workspace) = self.key_of(surface)?;
        let active = self.active_workspace(&output);
        let workspace = if self.scroll_key(workspace) == 0 {
            active
        } else {
            workspace
        };
        let columns = self.scroll_columns(&output, workspace);
        let (column, row) = self.scroll_position(surface)?;
        let column = columns.get(column)?;
        let head = column.windows.first()?.clone();
        let gap = gap
            .max(0)
            .min(area.size.w.min(area.size.h).saturating_sub(1) / 2);
        let n = column.windows.len().max(1);
        let available =
            (f64::from(area.size.h - 2 * gap) - f64::from(gap) * (n - 1) as f64).max(1.0);
        let weights: Vec<f64> = column
            .windows
            .iter()
            .map(|s| self.scroll.weight(s))
            .collect();
        let heights = crate::scroll::column_row_heights(
            available,
            &weights,
            cfg.stack,
            cfg.stack_height,
            cfg.stack_min_height,
        );
        let viewport = f64::from(area.size.h - 2 * gap).max(1.0);
        let content = heights
            .iter()
            .map(|height| height.max(1.0) + f64::from(gap))
            .sum::<f64>()
            - f64::from(gap);
        let maximum = (content - viewport).max(0.0);
        Some((head, row, heights, viewport, maximum, gap))
    }

    /// Reveals `surface`'s row in its column's vertical stack camera.
    pub(crate) fn scroll_reveal_stack(
        &mut self,
        surface: &WlSurface,
        area: Rectangle<i32, Logical>,
        gap: i32,
    ) -> bool {
        let Some((head, row, heights, viewport, maximum, gap)) =
            self.scroll_stack_metrics(surface, area, gap)
        else {
            return false;
        };
        let previous = self.scroll.stack_offset(&head).clamp(0.0, maximum);
        let top = heights
            .iter()
            .take(row)
            .map(|height| height.max(1.0) + f64::from(gap))
            .sum::<f64>();
        let bottom = top + heights.get(row).copied().unwrap_or(1.0).max(1.0);
        let mut offset = previous;
        if top < offset {
            offset = top;
        }
        if bottom > offset + viewport {
            offset = bottom - viewport;
        }
        let offset = offset.clamp(0.0, maximum);
        self.scroll.set_stack_offset(&head, offset);
        (offset - previous).abs() > 0.5
    }

    /// Pans the vertical stack camera of `surface`'s column by `delta`
    /// logical pixels (positive scrolls toward lower windows).
    pub(crate) fn scroll_stack_pan(
        &mut self,
        surface: &WlSurface,
        area: Rectangle<i32, Logical>,
        gap: i32,
        delta: f64,
    ) -> bool {
        let Some((head, _row, _heights, _viewport, maximum, _gap)) =
            self.scroll_stack_metrics(surface, area, gap)
        else {
            return false;
        };
        let previous = self.scroll.stack_offset(&head).clamp(0.0, maximum);
        let offset = (previous + delta).clamp(0.0, maximum);
        self.scroll.set_stack_offset(&head, offset);
        (offset - previous).abs() > 0.5
    }

    fn scroll_slots(
        &self,
        output: &str,
        workspace: u32,
        area: Rectangle<i32, Logical>,
        gap: i32,
    ) -> Vec<ScrollSlot> {
        let Some(cfg) = self.scroll.config else {
            return Vec::new();
        };
        let viewport = self.scroll_viewport(area, gap);
        let mut owners: Vec<_> = if self.scroll_key(workspace) == 0 {
            self.trees
                .keys()
                .filter(|(name, number)| {
                    name == output && !crate::state::is_scratchpad_workspace(*number)
                })
                .map(|(_, number)| *number)
                .chain(std::iter::once(workspace))
                .collect()
        } else {
            vec![workspace]
        };
        owners.sort_unstable();
        owners.dedup();
        let mut start = 0.0;
        let mut result = Vec::new();
        for number in owners {
            let columns = self.window_columns(output, number);
            if columns.is_empty() {
                result.push(ScrollSlot {
                    workspace: number,
                    window: None,
                    start,
                    extent: viewport,
                    row: 0,
                    weights: vec![1.0],
                    head: None,
                });
                start += viewport + f64::from(gap.max(0));
                continue;
            }
            for column in columns {
                let surfaces: Vec<Option<WlSurface>> = column.iter().map(surface_of).collect();
                let proportion = surfaces[0]
                    .as_ref()
                    .map(|s| self.scroll.proportion(s))
                    .unwrap_or(cfg.tile_proportion);
                let extent = surfaces
                    .iter()
                    .flatten()
                    .filter_map(|s| self.scroll.fullscreen_extents.get(s).copied())
                    .reduce(f64::max)
                    .unwrap_or_else(|| (viewport * proportion).round().max(1.0));
                let weights: Vec<f64> = surfaces
                    .iter()
                    .map(|s| s.as_ref().map(|s| self.scroll.weight(s)).unwrap_or(1.0))
                    .collect();
                for (row, window) in column.into_iter().enumerate() {
                    result.push(ScrollSlot {
                        workspace: number,
                        window: Some(window),
                        start,
                        extent,
                        row,
                        weights: weights.clone(),
                        head: surfaces.first().cloned().flatten(),
                    });
                }
                start += extent + f64::from(gap.max(0));
            }
        }
        result
    }

    /// Whether revealing `slot` from `offset` should center it.
    fn scroll_centers(
        &self,
        slots: &[ScrollSlot],
        slot: &ScrollSlot,
        offset: f64,
        viewport: f64,
    ) -> bool {
        let Some(cfg) = self.scroll.config else {
            return false;
        };
        let columns = slots
            .iter()
            .filter(|s| s.window.is_some() && s.row == 0)
            .count();
        if cfg.always_center_single && columns == 1 {
            return true;
        }
        match cfg.center_focused {
            scroll::CenterMode::Never => false,
            scroll::CenterMode::Always => true,
            scroll::CenterMode::OnOverflow => {
                slot.start < offset || slot.start + slot.extent > offset + viewport
            }
        }
    }

    pub(crate) fn scroll_viewport(&self, area: Rectangle<i32, Logical>, gap: i32) -> f64 {
        let gap = gap
            .max(0)
            .min(area.size.w.min(area.size.h).saturating_sub(1) / 2);
        f64::from(
            match self.scroll.config.unwrap_or_default().orientation {
                Orientation::Horizontal => area.size.w,
                Orientation::Vertical => area.size.h,
            }
            .saturating_sub(2 * gap)
            .max(1),
        )
    }

    /// Centering permits empty margins at either end of the strip, so the
    /// first, last and sole tile can align like every interior tile.
    fn scroll_bounds(&self, slots: &[ScrollSlot], viewport: f64, key: (&str, u32)) -> (f64, f64) {
        let centered = self.scroll.config.is_some_and(|cfg| {
            cfg.center_focused != scroll::CenterMode::Never || cfg.always_center_single
        }) || self.scroll.has_margins(key.0, key.1);
        let minimum = slots
            .first()
            .map(|slot| {
                if centered {
                    slot.start - ((viewport - slot.extent) / 2.0).max(0.0)
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0);
        let maximum = slots
            .last()
            .map(|slot| {
                let trailing = if centered {
                    ((viewport - slot.extent) / 2.0).max(0.0)
                } else {
                    0.0
                };
                slot.start + slot.extent - viewport + trailing
            })
            .unwrap_or(0.0)
            .max(minimum);
        (minimum, maximum)
    }

    /// The part of the strip scrolling can ever show, in strip
    /// coordinates (`offset` 0 at the first column's leading edge), with
    /// the current offset: `(offset, first, last)`.
    pub(crate) fn scroll_reach(
        &self,
        output: &str,
        workspace: u32,
        area: Rectangle<i32, Logical>,
        gap: i32,
    ) -> (f64, f64, f64) {
        let slots = self.scroll_slots(output, workspace, area, gap);
        let viewport = self.scroll_viewport(area, gap);
        let key = self.scroll_key(workspace);
        let (minimum, maximum) = self.scroll_bounds(&slots, viewport, (output, key));
        (self.scroll.offset(output, key), minimum, maximum + viewport)
    }

    pub(crate) fn prepare_scroll(
        &mut self,
        output: &str,
        workspace: u32,
        area: Rectangle<i32, Logical>,
        gap: i32,
    ) {
        if self.scroll.config.is_none() {
            return;
        }
        let slots = self.scroll_slots(output, workspace, area, gap);
        let viewport = self.scroll_viewport(area, gap);
        let key = self.scroll_key(workspace);
        let (minimum, maximum) = self.scroll_bounds(&slots, viewport, (output, key));
        let mut offset = self.scroll.offset(output, key).clamp(minimum, maximum);
        if self.scroll_section_requests.remove(output).is_some() && key == 0 {
            if let Some(slot) = slots.iter().find(|slot| slot.workspace == workspace) {
                let center = self.scroll_centers(&slots, slot, 0.0, viewport);
                offset = scroll::reveal(0.0, slot.start, slot.extent, viewport, center)
                    .clamp(minimum, maximum);
            }
        }
        self.scroll.set_offset(output, key, offset);
    }

    pub(crate) fn scroll_layout(
        &self,
        output: &str,
        workspace: u32,
        area: Rectangle<i32, Logical>,
        gap: i32,
    ) -> Vec<(Window, Rectangle<i32, Logical>)> {
        let cfg = self.scroll.config.unwrap_or_default();
        let offset = self.scroll.offset(output, self.scroll_key(workspace));
        let horizontal = cfg.orientation == crate::scroll::Orientation::Horizontal;
        self.scroll_slots(output, workspace, area, gap)
            .into_iter()
            .filter_map(|slot| {
                let window = slot.window?;
                // A fullscreen window takes its whole column.
                let fullscreen = surface_of(&window)
                    .is_some_and(|s| self.scroll.fullscreen_extents.contains_key(&s));
                let rect = if fullscreen {
                    scroll::tile_rect(area, slot.start, slot.extent, offset, gap, cfg.orientation)
                } else if horizontal && cfg.stack != crate::scroll::StackMode::Fit {
                    let gap = gap
                        .max(0)
                        .min(area.size.w.min(area.size.h).saturating_sub(1) / 2);
                    let rows = slot.weights.len().max(1);
                    let available = (f64::from(area.size.h - 2 * gap)
                        - f64::from(gap) * (rows - 1) as f64)
                        .max(1.0);
                    let heights = scroll::column_row_heights(
                        available,
                        &slot.weights,
                        cfg.stack,
                        cfg.stack_height,
                        cfg.stack_min_height,
                    );
                    let viewport = f64::from(area.size.h - 2 * gap).max(1.0);
                    let content = heights
                        .iter()
                        .map(|height| height.max(1.0) + f64::from(gap))
                        .sum::<f64>()
                        - f64::from(gap);
                    let maximum = (content - viewport).max(0.0);
                    let y_offset = slot
                        .head
                        .as_ref()
                        .map(|head| self.scroll.stack_offset(head))
                        .unwrap_or(0.0)
                        .min(maximum);
                    scroll::column_tile_rect_stacked(
                        area,
                        slot.start,
                        slot.extent,
                        offset,
                        gap,
                        &heights,
                        slot.row,
                        y_offset,
                    )
                } else {
                    scroll::column_tile_rect(
                        area,
                        slot.start,
                        slot.extent,
                        offset,
                        gap,
                        cfg.orientation,
                        (slot.row, &slot.weights),
                    )
                };
                Some((window, rect))
            })
            .collect()
    }

    pub(crate) fn scroll_reveal(
        &mut self,
        surface: &WlSurface,
        area: Rectangle<i32, Logical>,
        gap: i32,
    ) -> bool {
        self.scroll_reveal_with(surface, area, gap, None)
    }

    /// Reveals `surface`, forcing centering on or off with `center`, or
    /// following the configured centering mode with `None`.
    pub(crate) fn scroll_reveal_with(
        &mut self,
        surface: &WlSurface,
        area: Rectangle<i32, Logical>,
        gap: i32,
        center: Option<bool>,
    ) -> bool {
        if self.scroll.config.is_none() {
            return false;
        }
        let Some((output, workspace)) = self.key_of(surface) else {
            return false;
        };
        let active = self.active_workspace(&output);
        let slots = self.scroll_slots(
            &output,
            if self.scroll_key(workspace) == 0 {
                active
            } else {
                workspace
            },
            area,
            gap,
        );
        let Some(slot) = slots.iter().find(|slot| {
            slot.window
                .as_ref()
                .is_some_and(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
        }) else {
            return false;
        };
        let key = self.scroll_key(workspace);
        let previous = self.scroll.offset(&output, key);
        let viewport = self.scroll_viewport(area, gap);
        // An explicit center may leave empty space past the strip's ends
        // until the next ordinary reveal or pan.
        self.scroll.set_margins(&output, key, center == Some(true));
        let (minimum, maximum) = self.scroll_bounds(&slots, viewport, (&output, key));
        let center =
            center.unwrap_or_else(|| self.scroll_centers(&slots, slot, previous, viewport));
        let offset = scroll::reveal(previous, slot.start, slot.extent, viewport, center)
            .clamp(minimum, maximum);
        self.scroll.set_offset(&output, key, offset);
        let x_changed = (previous - offset).abs() > f64::EPSILON;
        let y_changed = self.scroll_reveal_stack(surface, area, gap);
        x_changed || y_changed
    }

    /// The column width that fills the viewport space other fully visible
    /// columns leave around `surface`'s column (niri's
    /// expand-column-to-available-width).
    pub(crate) fn scroll_available_width(
        &self,
        surface: &WlSurface,
        area: Rectangle<i32, Logical>,
        gap: i32,
    ) -> Option<f64> {
        let (output, workspace) = self.key_of(surface)?;
        let shown = if self.scroll_key(workspace) == 0 {
            self.active_workspace(&output)
        } else {
            workspace
        };
        let slots = self.scroll_slots(&output, shown, area, gap);
        let viewport = self.scroll_viewport(area, gap);
        let offset = self.scroll.offset(&output, self.scroll_key(workspace));
        let own = slots.iter().find(|slot| {
            slot.window
                .as_ref()
                .and_then(surface_of)
                .is_some_and(|s| &s == surface)
        })?;
        let gap = f64::from(gap.max(0));
        let taken: f64 = slots
            .iter()
            .filter(|slot| slot.row == 0 && slot.window.is_some())
            .filter(|slot| (slot.start - own.start).abs() > f64::EPSILON)
            .filter(|slot| {
                slot.start >= offset - 0.5 && slot.start + slot.extent <= offset + viewport + 0.5
            })
            .map(|slot| slot.extent + gap)
            .sum();
        Some(((viewport - taken) / viewport).clamp(0.1, 1.0))
    }

    pub(crate) fn scroll_pan(
        &mut self,
        output: &str,
        workspace: u32,
        area: Rectangle<i32, Logical>,
        gap: i32,
        delta: f64,
    ) {
        let slots = self.scroll_slots(output, workspace, area, gap);
        let viewport = self.scroll_viewport(area, gap);
        let key = self.scroll_key(workspace);
        self.scroll.set_margins(output, key, false);
        let (minimum, maximum) = self.scroll_bounds(&slots, viewport, (output, key));
        let offset = (self.scroll.offset(output, key) + delta * viewport).clamp(minimum, maximum);
        self.scroll.set_offset(output, key, offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ocean::test_protocol::ProtocolFixture, scroll::ScrollConfig};

    fn surface(window: &Window) -> WlSurface {
        window.toplevel().unwrap().wl_surface().clone()
    }

    fn shape(layouts: &Layouts) -> Vec<Vec<WlSurface>> {
        layouts
            .scroll_columns("screen", 1)
            .into_iter()
            .map(|c| c.windows)
            .collect()
    }

    #[test]
    fn consume_expel_and_moves_keep_columns_contiguous() {
        let mut protocol = ProtocolFixture::new();
        let mut layouts = Layouts::default();
        layouts.scroll.config = Some(ScrollConfig::default());
        let w: Vec<_> = (0..4).map(|_| protocol.window()).collect();
        let s: Vec<_> = w.iter().map(surface).collect();
        for window in &w {
            layouts.insert("screen", 1, window.clone(), None);
        }
        assert_eq!(shape(&layouts).len(), 4);

        // Alone: consume-or-expel joins the neighbor column on that side.
        assert!(layouts.scroll_consume_or_expel(&s[1], false));
        assert_eq!(
            shape(&layouts),
            vec![
                vec![s[0].clone(), s[1].clone()],
                vec![s[2].clone()],
                vec![s[3].clone()]
            ]
        );
        // Shared: it leaves for a new column on that side.
        assert!(layouts.scroll_consume_or_expel(&s[1], true));
        assert_eq!(shape(&layouts).len(), 4);
        assert_eq!(shape(&layouts)[1], vec![s[1].clone()]);
        // Consume the next column's first window, then expel it again.
        assert!(layouts.scroll_consume_into_column(&s[2]));
        assert_eq!(shape(&layouts)[2], vec![s[2].clone(), s[3].clone()]);
        assert!(!layouts.scroll_consume_into_column(&s[3]));
        assert!(layouts.scroll_move_in_column(&s[3], -1));
        assert_eq!(shape(&layouts)[2], vec![s[3].clone(), s[2].clone()]);
        // Moving a column moves all of its windows, to the front too.
        assert!(layouts.scroll_move_column(&s[2], -1, true));
        assert!(!layouts.scroll_move_column(&s[2], -1, true));
        assert_eq!(shape(&layouts)[0], vec![s[3].clone(), s[2].clone()]);
        assert!(layouts.scroll_expel_from_column(&s[3]));
        assert_eq!(shape(&layouts).len(), 4);
        assert!(!layouts.scroll_move_column(&s[0], 5, false));
    }

    #[test]
    fn new_windows_follow_the_column_and_a_closed_head_keeps_it() {
        let mut protocol = ProtocolFixture::new();
        let mut layouts = Layouts::default();
        layouts.scroll.config = Some(ScrollConfig::default());
        let first = protocol.window();
        let second = protocol.window();
        layouts.insert("screen", 1, first.clone(), None);
        layouts.insert("screen", 1, second.clone(), None);
        assert!(layouts.scroll_consume_or_expel(&surface(&second), false));
        layouts.scroll.resize(surface(&first), 0.8);
        // Opening next to the head lands after the whole column.
        let third = protocol.window();
        layouts.insert("screen", 1, third.clone(), Some(&surface(&first)));
        assert_eq!(
            shape(&layouts),
            vec![
                vec![surface(&first), surface(&second)],
                vec![surface(&third)]
            ]
        );
        // Closing the head promotes the next row with the column width.
        layouts.remove(&surface(&first));
        let columns = layouts.scroll_columns("screen", 1);
        assert_eq!(columns[0].windows, vec![surface(&second)]);
        assert_eq!(columns[0].width, 0.8);
        assert_eq!(columns.len(), 2);
    }

    #[test]
    fn stacked_rows_share_the_column_and_split_its_height() {
        let mut protocol = ProtocolFixture::new();
        let mut layouts = Layouts::default();
        layouts.scroll.config = Some(ScrollConfig::default());
        let top = protocol.window();
        let bottom = protocol.window();
        let next = protocol.window();
        for window in [&top, &bottom, &next] {
            layouts.insert("screen", 1, window.clone(), None);
        }
        assert!(layouts.scroll_consume_or_expel(&surface(&bottom), false));
        layouts.scroll.set_weight(&surface(&bottom), 3.0);
        let area = Rectangle::new((0, 0).into(), (1216, 816).into());
        let rects = layouts.layout("screen", 1, area, 8);
        let (a, b, c) = (rects[0].1, rects[1].1, rects[2].1);
        assert_eq!(a.loc.x, b.loc.x);
        assert_eq!(a.size.w, b.size.w);
        assert_eq!(a.loc.y + a.size.h + 8, b.loc.y);
        assert_eq!(b.size.h, 3 * a.size.h);
        assert_eq!(c.loc.x, a.loc.x + a.size.w + 8);
        assert_eq!(c.size.h, 800);
    }

    #[test]
    fn on_overflow_centers_only_hidden_columns_and_single_columns_can_center() {
        let mut protocol = ProtocolFixture::new();
        let mut layouts = Layouts::default();
        layouts.scroll.config = Some(ScrollConfig {
            center_focused: scroll::CenterMode::OnOverflow,
            tile_proportion: 0.4,
            ..Default::default()
        });
        let w: Vec<_> = (0..4).map(|_| protocol.window()).collect();
        for window in &w {
            layouts.insert("screen", 1, window.clone(), None);
        }
        let area = Rectangle::new((0, 0).into(), (1016, 600).into());
        // Column 2 is already fully visible: nothing moves.
        assert!(!layouts.scroll_reveal(&surface(&w[1]), area, 8));
        // Column 4 is off screen: it is centered, not edge-aligned.
        assert!(layouts.scroll_reveal(&surface(&w[3]), area, 8));
        let rect = layouts.layout("screen", 1, area, 8)[3].1;
        assert!(
            (2 * rect.loc.x + rect.size.w - area.size.w).abs() <= 1,
            "{rect:?}"
        );

        let mut single = Layouts::default();
        single.scroll.config = Some(ScrollConfig {
            always_center_single: true,
            tile_proportion: 0.4,
            ..Default::default()
        });
        let only = protocol.window();
        single.insert("screen", 1, only.clone(), None);
        single.scroll_reveal(&surface(&only), area, 8);
        single.prepare_scroll("screen", 1, area, 8);
        let rect = single.layout("screen", 1, area, 8)[0].1;
        assert!(
            (2 * rect.loc.x + rect.size.w - area.size.w).abs() <= 1,
            "{rect:?}"
        );
    }

    #[test]
    fn available_width_fills_the_space_other_visible_columns_leave() {
        let mut protocol = ProtocolFixture::new();
        let mut layouts = Layouts::default();
        layouts.scroll.config = Some(ScrollConfig {
            tile_proportion: 0.25,
            ..Default::default()
        });
        let w: Vec<_> = (0..2).map(|_| protocol.window()).collect();
        for window in &w {
            layouts.insert("screen", 1, window.clone(), None);
        }
        // Viewport 1000 (1016 - 2*8); the other column takes 250 + 8.
        let area = Rectangle::new((0, 0).into(), (1016, 600).into());
        let width = layouts
            .scroll_available_width(&surface(&w[1]), area, 8)
            .unwrap();
        assert!((width - 0.742).abs() < 1e-9, "{width}");
    }

    #[test]
    fn drops_insert_new_columns_or_join_a_column_at_either_end() {
        let mut protocol = ProtocolFixture::new();
        let mut layouts = Layouts::default();
        layouts.scroll.config = Some(ScrollConfig::default());
        let w: Vec<_> = (0..3).map(|_| protocol.window()).collect();
        for window in &w {
            layouts.insert("screen", 1, window.clone(), None);
        }
        let s: Vec<_> = w.iter().map(surface).collect();
        use scroll::DropZone::*;
        // Into the middle column, above its window.
        assert!(layouts.scroll_drop(&s[0], &s[1], Before));
        assert_eq!(
            shape(&layouts),
            vec![vec![s[0].clone(), s[1].clone()], vec![s[2].clone()]]
        );
        // Out again as a new column after the last column.
        assert!(layouts.scroll_drop(&s[0], &s[2], ColumnAfter));
        assert_eq!(
            shape(&layouts),
            vec![vec![s[1].clone()], vec![s[2].clone()], vec![s[0].clone()]]
        );
        // Below a window, then a new column before the first.
        assert!(layouts.scroll_drop(&s[2], &s[0], After));
        assert_eq!(
            shape(&layouts),
            vec![vec![s[1].clone()], vec![s[0].clone(), s[2].clone()]]
        );
        assert!(layouts.scroll_drop(&s[2], &s[1], ColumnBefore));
        assert_eq!(
            shape(&layouts),
            vec![vec![s[2].clone()], vec![s[1].clone()], vec![s[0].clone()]]
        );
        assert!(!layouts.scroll_drop(&s[2], &s[2], After));
        assert!(!layouts.scroll_drop(&s[2], &s[1], Swap));
    }

    #[test]
    fn swapped_windows_trade_column_slots() {
        let mut protocol = ProtocolFixture::new();
        let mut layouts = Layouts::default();
        layouts.scroll.config = Some(ScrollConfig::default());
        let w: Vec<_> = (0..3).map(|_| protocol.window()).collect();
        for window in &w {
            layouts.insert("screen", 1, window.clone(), None);
        }
        let s: Vec<_> = w.iter().map(surface).collect();
        assert!(layouts.scroll_consume_or_expel(&s[1], false));
        layouts.swap(&s[1], &s[2]);
        assert_eq!(
            shape(&layouts),
            vec![vec![s[0].clone(), s[2].clone()], vec![s[1].clone()]]
        );
    }

    #[test]
    fn strips_preserve_sizes_and_scroll_only_the_owning_output() {
        let mut protocol = ProtocolFixture::new();
        let windows: Vec<_> = (0..5).map(|_| protocol.window()).collect();
        let mut layouts = Layouts::default();
        layouts.scroll.config = Some(ScrollConfig::default());
        for window in &windows[..4] {
            layouts.insert("left", 1, window.clone(), None);
        }
        layouts.insert("right", 1, windows[4].clone(), None);
        let area = Rectangle::new((-1307, 31).into(), (1307, 801).into());
        let before = layouts.layout("left", 1, area, 9);
        let other = layouts.layout("right", 1, area, 9);
        let last = before
            .last()
            .unwrap()
            .0
            .toplevel()
            .unwrap()
            .wl_surface()
            .clone();
        assert!(layouts.scroll_reveal(&last, area, 9));
        let after = layouts.layout("left", 1, area, 9);
        assert!(after.last().unwrap().1.loc.x < before.last().unwrap().1.loc.x);
        assert_eq!(after[0].1.size, before[0].1.size);
        assert_eq!(layouts.layout("right", 1, area, 9)[0].1, other[0].1);
        layouts.insert("left", 1, protocol.window(), None);
        assert_eq!(
            layouts.layout("left", 1, area, 9)[0].1.size,
            before[0].1.size
        );
        // A separate workspace retains its own position.
        layouts.insert("left", 3, protocol.window(), None);
        assert_eq!(layouts.scroll.offset("left", 3), 0.0);
        assert!(layouts.scroll.offset("left", 1) > 0.0);
    }

    #[test]
    fn centered_edges_single_tiles_and_pan_use_symmetric_margins() {
        let mut protocol = ProtocolFixture::new();
        for orientation in [Orientation::Horizontal, Orientation::Vertical] {
            let mut layouts = Layouts::default();
            layouts.scroll.config = Some(ScrollConfig {
                center_focused: scroll::CenterMode::Always,
                tile_proportion: 0.94,
                orientation,
                ..Default::default()
            });
            let first = protocol.window();
            layouts.insert("screen", 1, first.clone(), None);
            let area = Rectangle::new((-100, 25).into(), (1200, 800).into());
            let centered = |rect: Rectangle<i32, Logical>| {
                let error = match orientation {
                    Orientation::Horizontal => {
                        2 * rect.loc.x + rect.size.w - (2 * area.loc.x + area.size.w)
                    }
                    Orientation::Vertical => {
                        2 * rect.loc.y + rect.size.h - (2 * area.loc.y + area.size.h)
                    }
                };
                assert!(error.abs() <= 1, "not centered: {rect:?}");
            };
            layouts.prepare_scroll("screen", 1, area, 8);
            centered(layouts.layout("screen", 1, area, 8)[0].1);
            assert!(layouts.scroll.offset("screen", 1) < 0.0);
            let last = protocol.window();
            layouts.insert("screen", 1, last.clone(), None);
            layouts.scroll_reveal(last.toplevel().unwrap().wl_surface(), area, 8);
            layouts.prepare_scroll("screen", 1, area, 8);
            centered(layouts.layout("screen", 1, area, 8)[1].1);
            layouts.scroll_pan("screen", 1, area, 8, -100.0);
            centered(layouts.layout("screen", 1, area, 8)[0].1);
            layouts.scroll_pan("screen", 1, area, 8, 100.0);
            centered(layouts.layout("screen", 1, area, 8)[1].1);
            layouts.scroll.config.as_mut().unwrap().center_focused = scroll::CenterMode::Never;
            layouts.scroll_reveal(first.toplevel().unwrap().wl_surface(), area, 8);
            layouts.prepare_scroll("screen", 1, area, 8);
            assert_eq!(layouts.scroll.offset("screen", 1), 0.0);
        }
    }

    #[test]
    fn fullscreen_columns_reserve_output_extent_without_covering_the_next_tile() {
        let mut protocol = ProtocolFixture::new();
        let mut layouts = Layouts::default();
        layouts.scroll.config = Some(ScrollConfig::default());
        let fullscreen = protocol.window();
        let neighbor = protocol.window();
        layouts.insert("screen", 1, fullscreen.clone(), None);
        layouts.insert("screen", 1, neighbor.clone(), None);
        let area = Rectangle::new((0, 0).into(), (1301, 701).into());
        let surface = fullscreen.toplevel().unwrap().wl_surface().clone();
        layouts.scroll.fullscreen_extents.insert(surface, 1301.0);
        let before = layouts.layout("screen", 1, area, 7);
        assert_eq!(before[0].1.size.w, 1301);
        assert!(layouts.scroll_reveal(neighbor.toplevel().unwrap().wl_surface(), area, 7));
        let after = layouts.layout("screen", 1, area, 7);
        assert!(after[0].1.loc.x < 0);
        assert!(after[1].1.loc.x >= area.loc.x);
        assert!(after[1].1.loc.x + after[1].1.size.w <= area.size.w);
    }

    #[test]
    fn sections_share_a_view_but_keep_workspace_ownership_and_hide_scratchpads() {
        let mut protocol = ProtocolFixture::new();
        let mut layouts = Layouts::default();
        layouts.scroll.config = Some(ScrollConfig {
            workspaces: WorkspaceMode::Sections,
            ..Default::default()
        });
        let first = protocol.window();
        let second = protocol.window();
        layouts.insert("screen", 1, first.clone(), None);
        layouts.insert("screen", 9, second.clone(), None);
        layouts.insert("screen", u32::MAX, protocol.window(), None);
        let area = Rectangle::new((120, -55).into(), (1101, 701).into());
        assert_eq!(layouts.layout("screen", 1, area, 7).len(), 2);
        layouts.set_active_workspace("screen", 9);
        layouts.prepare_scroll("screen", 9, area, 7);
        assert!(layouts.scroll.offset("screen", 0) > 0.0);
        assert_eq!(
            layouts.workspace_of(second.toplevel().unwrap().wl_surface()),
            Some(9)
        );
        assert_eq!(layouts.layout("screen", u32::MAX, area, 7).len(), 1);
        layouts.set_active_workspace("screen", 4); // empty section is reachable
        layouts.prepare_scroll("screen", 4, area, 7);
        assert!(layouts.scroll.offset("screen", 0) > 0.0);
        layouts.scroll.config = None;
        assert_eq!(layouts.layout("screen", 1, area, 7).len(), 1);
        assert_eq!(layouts.layout("screen", 9, area, 7).len(), 1);
    }
}
