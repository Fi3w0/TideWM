use super::*;
use crate::scroll::{self, Orientation, WorkspaceMode};

type ScrollSlot = (u32, Option<Window>, f64, f64);

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
            let windows = self.windows_in(output, number);
            if windows.is_empty() {
                result.push((number, None, start, viewport));
                start += viewport + f64::from(gap.max(0));
            } else {
                for window in windows {
                    let proportion = window
                        .toplevel()
                        .map(|t| self.scroll.proportion(t.wl_surface()))
                        .unwrap_or(cfg.tile_proportion);
                    let extent = (viewport * proportion).round().max(1.0);
                    result.push((number, Some(window), start, extent));
                    start += extent + f64::from(gap.max(0));
                }
            }
        }
        result
    }

    fn scroll_viewport(&self, area: Rectangle<i32, Logical>, gap: i32) -> f64 {
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
        let maximum = slots
            .last()
            .map(|(_, _, start, extent)| start + extent - viewport)
            .unwrap_or(0.0)
            .max(0.0);
        let key = self.scroll_key(workspace);
        let mut offset = self.scroll.offset(output, key).min(maximum);
        if self.scroll_section_requests.remove(output).is_some() && key == 0 {
            if let Some((_, _, start, _)) =
                slots.iter().find(|(number, _, _, _)| *number == workspace)
            {
                offset = start.min(maximum);
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
        self.scroll_slots(output, workspace, area, gap)
            .into_iter()
            .filter_map(|(_, window, start, extent)| {
                window.map(|window| {
                    (
                        window,
                        scroll::tile_rect(area, start, extent, offset, gap, cfg.orientation),
                    )
                })
            })
            .collect()
    }

    pub(crate) fn scroll_reveal(
        &mut self,
        surface: &WlSurface,
        area: Rectangle<i32, Logical>,
        gap: i32,
    ) -> bool {
        let Some(cfg) = self.scroll.config else {
            return false;
        };
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
        let Some((_, _, start, extent)) = slots.iter().find(|(_, window, _, _)| {
            window
                .as_ref()
                .is_some_and(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
        }) else {
            return false;
        };
        let key = self.scroll_key(workspace);
        let previous = self.scroll.offset(&output, key);
        let viewport = self.scroll_viewport(area, gap);
        let maximum = slots
            .last()
            .map(|(_, _, start, extent)| start + extent - viewport)
            .unwrap_or(0.0)
            .max(0.0);
        let offset =
            scroll::reveal(previous, *start, *extent, viewport, cfg.center_focused).min(maximum);
        self.scroll.set_offset(&output, key, offset);
        (previous - offset).abs() > f64::EPSILON
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
        let maximum = slots
            .last()
            .map(|(_, _, start, extent)| start + extent - viewport)
            .unwrap_or(0.0)
            .max(0.0);
        let key = self.scroll_key(workspace);
        let offset = (self.scroll.offset(output, key) + delta * viewport).clamp(0.0, maximum);
        self.scroll.set_offset(output, key, offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ocean::test_protocol::ProtocolFixture, scroll::ScrollConfig};

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
