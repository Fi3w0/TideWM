//! Coalesced damage scope. Output identity prevents a stale request from
//! dirtying a reconnected output that happens to reuse the same name.

use smithay::{output::Output, reexports::wayland_server::protocol::wl_surface::WlSurface};
use std::collections::HashSet;

use crate::placement::PlacedWindow;

#[derive(Default)]
pub(crate) struct RedrawRequests {
    all: bool,
    outputs: Vec<Output>,
}

impl RedrawRequests {
    pub(crate) fn global() -> Self {
        Self {
            all: true,
            outputs: Vec::new(),
        }
    }

    pub(crate) fn request_all(&mut self) {
        self.all = true;
        self.outputs.clear();
    }

    pub(crate) fn request_output(&mut self, output: &Output) {
        if !self.all && !self.outputs.contains(output) {
            self.outputs.push(output.clone());
        }
    }

    pub(crate) fn is_global(&self) -> bool {
        self.all
    }

    /// Render-only window changes follow the scene, not the window's owner:
    /// a floating window or a shared Ocean view may appear on several outputs.
    #[allow(clippy::mutable_key_type)] // WlSurface hashes its stable protocol object identity.
    pub(crate) fn request_surface_changes(
        &mut self,
        output: &Output,
        placements: &[PlacedWindow],
        surfaces: &HashSet<WlSurface>,
    ) -> bool {
        if self.all
            || !placements
                .iter()
                .filter_map(PlacedWindow::surface)
                .any(|surface| surfaces.contains(surface))
        {
            return false;
        }
        self.request_output(output);
        true
    }

    pub(crate) fn includes(&self, output: &Output) -> bool {
        self.all || self.outputs.contains(output)
    }

    pub(crate) fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

#[cfg(test)]
#[allow(clippy::mutable_key_type)] // Real Wayland surface identities, as in the production queue.
mod tests {
    use super::*;
    use crate::{
        config::{OceanConfig, SplitBias},
        ocean::{test_protocol::ProtocolFixture, OceanSpace},
        placement::PlacementKind,
    };
    use smithay::output::{PhysicalProperties, Subpixel};
    use smithay::utils::Rectangle;

    fn output(name: &str) -> Output {
        Output::new(
            name.into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        )
    }

    #[test]
    fn output_requests_coalesce_without_dirtying_other_outputs() {
        let first = output("first");
        let second = output("second");
        let mut requests = RedrawRequests::default();
        for _ in 0..64 {
            requests.request_output(&first);
        }
        assert_eq!(requests.outputs.len(), 1);
        let batch = requests.take();
        assert!(batch.includes(&first));
        assert!(!batch.includes(&second));
        assert!(!requests.includes(&first));
        requests.request_output(&second);
        assert!(!requests.includes(&first));
        assert!(requests.includes(&second));
    }

    #[test]
    fn global_damage_supersedes_scoped_requests_in_any_order() {
        let first = output("first");
        let second = output("second");
        let mut requests = RedrawRequests::default();
        requests.request_output(&first);
        requests.request_all();
        requests.request_output(&second);
        assert!(requests.outputs.is_empty());
        let batch = requests.take();
        assert!(batch.includes(&first) && batch.includes(&second));
        assert!(!requests.includes(&first) && !requests.includes(&second));
    }

    #[test]
    fn reconnected_output_with_the_same_name_does_not_inherit_old_damage() {
        let old = output("DP-test");
        let replacement = output("DP-test");
        let mut requests = RedrawRequests::default();
        requests.request_output(&old);
        assert!(requests.includes(&old.clone()));
        assert!(!requests.includes(&replacement));
        assert!(RedrawRequests::global().includes(&replacement));
    }

    #[test]
    fn ocean_surface_redraws_follow_independent_and_shared_views() {
        let mut protocol = ProtocolFixture::new();
        let window = protocol.window();
        let surface = window.toplevel().unwrap().wl_surface().clone();
        let changed = HashSet::from([surface.clone()]);
        let left = output("left");
        let right = output("right");
        let geometry = Rectangle::new((0, 0).into(), (1000, 800).into());
        let mut ocean = OceanSpace::from_config(&OceanConfig::default());
        ocean.push_migrated_floating(
            surface,
            window,
            Rectangle::new((100, 100).into(), (600, 400).into()),
            "right",
        );
        for shared in [false, true] {
            ocean.set_shared_canvas(shared);
            let mut requests = RedrawRequests::default();
            for target in [&left, &right] {
                let placements = ocean.placements(&target.name(), geometry, 8, SplitBias::Auto);
                requests.request_surface_changes(target, &placements, &changed);
            }
            assert_eq!(requests.includes(&left), shared);
            assert!(requests.includes(&right));
            assert!(!requests.is_global());
        }
    }

    #[test]
    fn screen_pins_and_offscreen_world_windows_do_not_dirty_foreign_views() {
        let mut protocol = ProtocolFixture::new();
        let window = protocol.window();
        let surface = window.toplevel().unwrap().wl_surface().clone();
        let changed = HashSet::from([surface.clone()]);
        let left = output("left");
        let right = output("right");
        let geometry = Rectangle::new((0, 0).into(), (1000, 800).into());
        let mut ocean = OceanSpace::from_config(&OceanConfig {
            shared_canvas: true,
            ..Default::default()
        });
        let rectangle = Rectangle::new((4000, 100).into(), (600, 400).into());
        ocean.push_migrated_floating(surface.clone(), window, rectangle, "right");
        let mut requests = RedrawRequests::default();
        for target in [&left, &right] {
            let placements = ocean.placements(&target.name(), geometry, 8, SplitBias::Auto);
            assert!(!requests.request_surface_changes(target, &placements, &changed));
        }
        assert!(!requests.includes(&left) && !requests.includes(&right));
        ocean.center_on_rect(
            "left",
            geometry.size,
            rectangle,
            std::time::Duration::ZERO,
            0.0,
        );
        assert!(requests.request_surface_changes(
            &left,
            &ocean.placements("left", geometry, 8, SplitBias::Auto),
            &changed,
        ));
        assert!(!requests.includes(&right));
        assert!(ocean.pin_to_screen(&surface, "left"));
        requests.take();
        for target in [&left, &right] {
            let placements = ocean.placements(&target.name(), geometry, 8, SplitBias::Auto);
            requests.request_surface_changes(target, &placements, &changed);
        }
        assert!(requests.includes(&left) && !requests.includes(&right));
    }

    #[test]
    fn overlapping_floating_and_preview_scenes_receive_the_same_surface_change() {
        let mut protocol = ProtocolFixture::new();
        let window = protocol.window();
        let changed = HashSet::from([window.toplevel().unwrap().wl_surface().clone()]);
        let left = output("left");
        let right = output("right");
        let rectangle = Rectangle::new((900, 50).into(), (200, 200).into());
        let floating = PlacedWindow::authoritative(window.clone(), rectangle)
            .with_kind(PlacementKind::Floating);
        let preview = PlacedWindow::preview(window, rectangle);
        let mut requests = RedrawRequests::default();
        for _ in 0..64 {
            assert!(requests.request_surface_changes(
                &left,
                std::slice::from_ref(&floating),
                &changed
            ));
            assert!(requests.request_surface_changes(
                &right,
                std::slice::from_ref(&preview),
                &changed
            ));
        }
        assert!(requests.includes(&left) && requests.includes(&right));
        assert_eq!(requests.outputs.len(), 2);
    }

    #[test]
    fn hidden_surface_changes_preserve_capture_requests_and_global_precedence() {
        let mut protocol = ProtocolFixture::new();
        let hidden = protocol.window();
        let visible = protocol.window();
        let changed = HashSet::from([hidden.toplevel().unwrap().wl_surface().clone()]);
        let left = output("left");
        let right = output("right");
        let placements = vec![PlacedWindow::authoritative(
            visible,
            Rectangle::new((0, 0).into(), (100, 100).into()),
        )];
        let mut requests = RedrawRequests::default();
        requests.request_output(&right); // a capture already queued on right
        assert!(!requests.request_surface_changes(&left, &placements, &changed));
        assert!(!requests.includes(&left) && requests.includes(&right));
        requests.request_all();
        assert!(requests.is_global());
        assert!(!requests.request_surface_changes(&left, &placements, &changed));
        assert!(requests.includes(&left) && requests.includes(&right));
        assert!(requests.outputs.is_empty());
    }

    #[test]
    fn changing_surface_extents_keep_old_and_new_output_damage() {
        let mut protocol = ProtocolFixture::new();
        let window = protocol.window();
        let changed = HashSet::from([window.toplevel().unwrap().wl_surface().clone()]);
        let old_output = output("old");
        let new_output = output("new");
        let untouched = output("untouched");
        let placement =
            PlacedWindow::authoritative(window, Rectangle::new((10, 10).into(), (200, 200).into()))
                .with_kind(PlacementKind::Floating);
        let mut requests = RedrawRequests::default();
        // Pre-commit geometry intersects old; after geometry processing it
        // intersects new. The second walk must preserve the first's damage.
        requests.request_surface_changes(&old_output, std::slice::from_ref(&placement), &changed);
        requests.request_surface_changes(&new_output, &[], &changed);
        requests.request_surface_changes(&old_output, &[], &changed);
        requests.request_surface_changes(&new_output, std::slice::from_ref(&placement), &changed);
        assert!(requests.includes(&old_output) && requests.includes(&new_output));
        assert!(!requests.includes(&untouched));
        assert!(!requests.is_global());
    }
}
