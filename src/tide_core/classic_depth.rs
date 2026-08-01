//! Classic workspace depth: explicit parking below the active layout.
//!
//! This is deliberately separate from `visual::depth`, whose attention
//! tiers only change how a still-visible window is rendered. A deck entry is
//! structural state: its client remains mapped, but its `Window` is absent
//! from both `Layouts` and `Space` until recalled.

use smithay::{desktop::Window, reexports::wayland_server::protocol::wl_surface::WlSurface};

pub struct DepthDeckEntry {
    pub surface: WlSurface,
    pub window: Window,
    pub output: String,
    pub workspace: u32,
}

#[derive(Clone)]
pub struct DepthDeckView {
    pub output: String,
    pub workspace: u32,
    pub selected: usize,
}

#[derive(Default)]
pub struct ClassicDepthDeck {
    entries: Vec<DepthDeckEntry>,
    view: Option<DepthDeckView>,
}

impl ClassicDepthDeck {
    pub fn entry(&self, surface: &WlSurface) -> Option<&DepthDeckEntry> {
        self.entries.iter().find(|entry| &entry.surface == surface)
    }

    pub fn view(&self) -> Option<&DepthDeckView> {
        self.view.as_ref()
    }

    pub fn entries_for(&self, output: &str, workspace: u32) -> Vec<&DepthDeckEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.output == output && entry.workspace == workspace)
            .collect()
    }

    /// Newest parked window appears first, matching a working-set deck rather
    /// than an archival list. A surface can only occupy one deck slot.
    pub fn park(&mut self, entry: DepthDeckEntry) {
        self.remove(&entry.surface);
        self.entries.insert(0, entry);
    }

    pub fn remove(&mut self, surface: &WlSurface) -> Option<DepthDeckEntry> {
        let index = self
            .entries
            .iter()
            .position(|entry| &entry.surface == surface)?;
        let entry = self.entries.remove(index);
        self.clamp_view();
        Some(entry)
    }

    pub fn open(&mut self, output: String, workspace: u32) -> bool {
        if self.count(&output, workspace) == 0 {
            return false;
        }
        self.view = Some(DepthDeckView {
            output,
            workspace,
            selected: 0,
        });
        true
    }

    pub fn close(&mut self) -> bool {
        self.view.take().is_some()
    }

    pub fn drain(&mut self) -> Vec<DepthDeckEntry> {
        self.view = None;
        std::mem::take(&mut self.entries)
    }

    pub fn cycle(&mut self, delta: isize) -> bool {
        let Some(view) = self.view.as_ref() else {
            return false;
        };
        let count = self.count(&view.output, view.workspace);
        if count == 0 {
            self.view = None;
            return false;
        }
        let view = self.view.as_mut().unwrap();
        view.selected = wrap_index(view.selected, count, delta);
        true
    }

    /// Replaces the selected slot and returns the window that occupied it.
    /// This is the exact operation Classic's default swap-recall needs.
    pub fn swap_selected(&mut self, replacement: DepthDeckEntry) -> Option<DepthDeckEntry> {
        let index = self.selected_entry_index()?;
        Some(std::mem::replace(&mut self.entries[index], replacement))
    }

    pub fn take_selected(&mut self) -> Option<DepthDeckEntry> {
        let index = self.selected_entry_index()?;
        let entry = self.entries.remove(index);
        self.clamp_view();
        Some(entry)
    }

    /// Rotates one surface tile through this workspace's deck without opening
    /// the picker. Down takes the newest deep entry and sends the current
    /// surface to the back; up takes the oldest and sends the current surface
    /// to the front, making the two directions exact inverses.
    pub fn rotate(
        &mut self,
        output: &str,
        workspace: u32,
        replacement: DepthDeckEntry,
        down: bool,
    ) -> Option<DepthDeckEntry> {
        let matching: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.output == output && entry.workspace == workspace)
            .map(|(index, _)| index)
            .collect();
        let chosen = if down {
            matching.first().copied()
        } else {
            matching.last().copied()
        }?;
        let selected = self.entries.remove(chosen);

        let remaining: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.output == output && entry.workspace == workspace)
            .map(|(index, _)| index)
            .collect();
        let insertion = if down {
            remaining
                .last()
                .map_or(self.entries.len(), |index| index + 1)
        } else {
            remaining.first().copied().unwrap_or(0)
        };
        self.entries.insert(insertion, replacement);
        self.clamp_view();
        Some(selected)
    }

    fn count(&self, output: &str, workspace: u32) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.output == output && entry.workspace == workspace)
            .count()
    }

    fn selected_entry_index(&self) -> Option<usize> {
        let view = self.view.as_ref()?;
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.output == view.output && entry.workspace == view.workspace)
            .nth(view.selected)
            .map(|(index, _)| index)
    }

    fn clamp_view(&mut self) {
        let Some(view) = self.view.as_ref() else {
            return;
        };
        let count = self.count(&view.output, view.workspace);
        if count == 0 {
            self.view = None;
        } else if self.view.as_ref().unwrap().selected >= count {
            self.view.as_mut().unwrap().selected = count - 1;
        }
    }
}

fn wrap_index(current: usize, count: usize, delta: isize) -> usize {
    (current as isize + delta).rem_euclid(count as isize) as usize
}

#[cfg(test)]
mod tests {
    use super::wrap_index;

    #[test]
    fn selection_wraps_in_both_directions() {
        assert_eq!(wrap_index(0, 3, 1), 1);
        assert_eq!(wrap_index(2, 3, 1), 0);
        assert_eq!(wrap_index(0, 3, -1), 2);
    }
}
