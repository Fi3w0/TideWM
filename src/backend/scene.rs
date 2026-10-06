//! Prepare Smithay's output membership before consuming a frame's scene.
//! Mapping changes positions immediately, but `elements_for_output` reads a
//! cache updated by `Space::refresh`. A maintenance deadline must not control
//! the first visible frame of a drag, map, resize, or output transition.

use smithay::desktop::{space::SpaceElement, Space};

pub(super) fn prepare_render_scene<E: SpaceElement + PartialEq>(space: &mut Space<E>) {
    space.refresh();
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::{
        output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
        utils::{IsAlive, Logical, Point, Rectangle, Size, Transform},
    };
    use std::{cell::Cell, rc::Rc};

    #[derive(Clone)]
    struct TestWindow {
        size: Rc<Cell<Size<i32, Logical>>>,
        alive: Rc<Cell<bool>>,
    }

    impl PartialEq for TestWindow {
        fn eq(&self, other: &Self) -> bool {
            Rc::ptr_eq(&self.size, &other.size)
        }
    }

    impl IsAlive for TestWindow {
        fn alive(&self) -> bool {
            self.alive.get()
        }
    }

    impl SpaceElement for TestWindow {
        fn bbox(&self) -> Rectangle<i32, Logical> {
            Rectangle::from_size(self.size.get())
        }
        fn is_in_input_region(&self, point: &Point<f64, Logical>) -> bool {
            self.bbox().to_f64().contains(*point)
        }
        fn set_activate(&self, _: bool) {}
        fn output_enter(&self, _: &Output, _: Rectangle<i32, Logical>) {}
        fn output_leave(&self, _: &Output) {}
    }

    fn output(name: &str, size: (i32, i32), scale: f64, transform: Transform) -> Output {
        let output = Output::new(
            name.into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "test".into(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: size.into(),
                refresh: 114_013,
            }),
            Some(transform),
            Some(Scale::Fractional(scale)),
            None,
        );
        output
    }

    fn scene() -> (Space<TestWindow>, TestWindow, Output, Output) {
        let left = output("left", (1200, 800), 1.0, Transform::Normal);
        let right = output("right", (2000, 1000), 2.0, Transform::_90);
        let mut space = Space::default();
        space.map_output(&left, (-1200, 0));
        space.map_output(&right, (0, 0));
        let window = TestWindow {
            size: Rc::new(Cell::new((400, 300).into())),
            alive: Rc::new(Cell::new(true)),
        };
        space.map_element(window.clone(), (-600, 200), false);
        prepare_render_scene(&mut space);
        (space, window, left, right)
    }

    #[test]
    fn first_drag_frame_updates_both_outputs_without_a_maintenance_tick() {
        let (mut space, window, left, right) = scene();
        assert_eq!(space.elements_for_output(&right).count(), 0);
        space.map_element(window.clone(), (-100, 200), false);
        // The actual Smithay cache reproduces the old missed first frame.
        assert_eq!(space.elements_for_output(&right).count(), 0);
        prepare_render_scene(&mut space);
        assert_eq!(space.elements_for_output(&left).count(), 1);
        assert_eq!(space.elements_for_output(&right).count(), 1);

        space.map_element(window.clone(), (100, 200), false);
        prepare_render_scene(&mut space);
        assert_eq!(space.elements_for_output(&left).count(), 0);
        assert_eq!(space.elements_for_output(&right).count(), 1);
        // Starting another drag in the reverse direction also refreshes now.
        space.map_element(window, (-600, 200), false);
        prepare_render_scene(&mut space);
        assert_eq!(space.elements_for_output(&left).count(), 1);
        assert_eq!(space.elements_for_output(&right).count(), 0);
    }

    #[test]
    fn committed_resize_and_surface_death_are_reflected_before_render() {
        let (mut space, window, left, right) = scene();
        window.size.set((700, 300).into());
        assert_eq!(space.elements_for_output(&right).count(), 0);
        prepare_render_scene(&mut space);
        assert_eq!(space.elements_for_output(&right).count(), 1);
        window.alive.set(false);
        prepare_render_scene(&mut space);
        assert_eq!(space.elements_for_output(&left).count(), 0);
        assert_eq!(space.elements_for_output(&right).count(), 0);
        assert_eq!(space.elements().count(), 0);
    }
}
