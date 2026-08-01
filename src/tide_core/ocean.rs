//! Continuous-world ownership for TideWM's Ocean spatial engine.
//!
//! Ocean is intentionally not implemented as a tall stack of Classic
//! workspaces. Windows belong to local [`OceanReef`] tiling zones in stable
//! world coordinates, while each physical output owns only a camera position
//! into that shared world. Rendering converts the resulting world rectangles
//! into the shared [`PlacedWindow`](crate::placement::PlacedWindow) boundary.

use std::collections::{HashMap, HashSet};

use smithay::{
    desktop::Window,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Size},
};

use crate::{
    config::{OceanConfig, SplitBias},
    layout::BspLayout,
    placement::{PlacedWindow, PlacementKind},
};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OceanPoint {
    pub x: f64,
    pub y: f64,
}

impl OceanPoint {
    fn translated(self, dx: f64, dy: f64) -> Self {
        Self {
            x: self.x + dx,
            y: self.y + dy,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OceanCamera {
    /// World point shown at the output viewport's top-left corner.
    pub origin: OceanPoint,
}

pub struct OceanReef {
    _name: String,
    pub rect: Rectangle<i32, Logical>,
    auto_width: bool,
    auto_height: bool,
    layout: BspLayout,
}

struct OceanScreenPin {
    output: String,
    viewport_loc: Point<f64, Logical>,
    size: Size<i32, Logical>,
}

impl OceanReef {
    fn new(
        name: String,
        rect: Rectangle<i32, Logical>,
        auto_width: bool,
        auto_height: bool,
    ) -> Self {
        Self {
            _name: name,
            rect,
            auto_width,
            auto_height,
            layout: BspLayout::default(),
        }
    }
}

/// Ocean's complete spatial state. It has no workspace number, active page,
/// or output-owned window tree: those concepts belong exclusively to Classic.
#[derive(Default)]
pub struct OceanSpace {
    reefs: Vec<OceanReef>,
    cameras: HashMap<String, OceanCamera>,
    bookmarks: HashMap<String, OceanPoint>,
    runtime_bookmarks: HashSet<String>,
    /// Output where a window entered the world. This is an input/focus hint,
    /// not spatial ownership; every output can render the same window.
    entry_outputs: HashMap<WlSurface, String>,
    floating: HashMap<WlSurface, (Window, Rectangle<i32, Logical>)>,
    screen_pins: HashMap<WlSurface, OceanScreenPin>,
}

impl OceanSpace {
    pub fn from_config(config: &OceanConfig) -> Self {
        let reefs = config
            .reefs
            .iter()
            .map(|reef| {
                OceanReef::new(
                    reef.name.clone(),
                    Rectangle::new(
                        Point::from((reef.x, reef.y)),
                        Size::from((reef.width.unwrap_or(0), reef.height.unwrap_or(0))),
                    ),
                    reef.width.is_none(),
                    reef.height.is_none(),
                )
            })
            .collect();
        let mut bookmarks: HashMap<String, OceanPoint> = config
            .bookmarks
            .iter()
            .map(|bookmark| {
                (
                    bookmark.name.clone(),
                    OceanPoint {
                        x: bookmark.x,
                        y: bookmark.y,
                    },
                )
            })
            .collect();
        for (index, reef) in config.reefs.iter().enumerate() {
            let origin = OceanPoint {
                x: reef.x as f64,
                y: reef.y as f64,
            };
            bookmarks.entry((index + 1).to_string()).or_insert(origin);
            bookmarks.entry(reef.name.clone()).or_insert(origin);
        }
        let default_home = config
            .reefs
            .first()
            .map(|reef| OceanPoint {
                x: reef.x as f64,
                y: reef.y as f64,
            })
            .unwrap_or_default();
        bookmarks.entry("home".to_string()).or_insert(default_home);
        Self {
            reefs,
            cameras: HashMap::new(),
            bookmarks,
            runtime_bookmarks: HashSet::new(),
            entry_outputs: HashMap::new(),
            floating: HashMap::new(),
            screen_pins: HashMap::new(),
        }
    }

    /// Materializes Ocean's implicit first reef using the real viewport size.
    /// This is called only when Ocean is active and its config has no reefs.
    pub fn ensure_default_reef(&mut self, viewport: Size<i32, Logical>) -> bool {
        let mut changed = false;
        if self.reefs.is_empty() {
            self.reefs.push(OceanReef::new(
                "main".to_string(),
                Rectangle::new(Point::from((0, 0)), viewport),
                true,
                true,
            ));
            changed = true;
        }
        self.bookmarks.entry("1".to_string()).or_default();
        for reef in &mut self.reefs {
            if reef.auto_width {
                let width = reef.rect.size.w.max(viewport.w);
                changed |= width != reef.rect.size.w;
                reef.rect.size.w = width;
            }
            if reef.auto_height {
                let height = reef.rect.size.h.max(viewport.h);
                changed |= height != reef.rect.size.h;
                reef.rect.size.h = height;
            }
        }
        changed
    }

    pub fn ensure_camera(&mut self, output: &str) -> OceanCamera {
        let initial = self
            .bookmarks
            .get("home")
            .copied()
            .or_else(|| {
                self.reefs.first().map(|reef| OceanPoint {
                    x: reef.rect.loc.x as f64,
                    y: reef.rect.loc.y as f64,
                })
            })
            .unwrap_or_default();
        *self
            .cameras
            .entry(output.to_string())
            .or_insert(OceanCamera { origin: initial })
    }

    pub fn camera(&self, output: &str) -> OceanCamera {
        self.cameras.get(output).copied().unwrap_or_else(|| {
            let origin = self.bookmarks.get("home").copied().unwrap_or_default();
            OceanCamera { origin }
        })
    }

    pub fn pan(&mut self, output: &str, dx: f64, dy: f64) {
        let current = self.ensure_camera(output);
        self.cameras.insert(
            output.to_string(),
            OceanCamera {
                origin: current.origin.translated(dx, dy),
            },
        );
    }

    pub fn jump_to_bookmark(&mut self, output: &str, name: &str) -> bool {
        let Some(origin) = self.bookmarks.get(name).copied() else {
            return false;
        };
        self.cameras
            .insert(output.to_string(), OceanCamera { origin });
        true
    }

    pub fn save_bookmark(&mut self, output: &str, name: String) -> bool {
        const MAX_RUNTIME_BOOKMARKS: usize = 64;
        if !self.bookmarks.contains_key(&name) {
            if self.runtime_bookmarks.len() >= MAX_RUNTIME_BOOKMARKS {
                return false;
            }
            self.runtime_bookmarks.insert(name.clone());
        }
        self.bookmarks.insert(name, self.camera(output).origin);
        true
    }

    pub fn remove_output(&mut self, output: &str, fallback: Option<&str>) {
        self.cameras.remove(output);
        match fallback {
            Some(fallback) => {
                for entry_output in self.entry_outputs.values_mut() {
                    if entry_output == output {
                        *entry_output = fallback.to_string();
                    }
                }
                for pin in self.screen_pins.values_mut() {
                    if pin.output == output {
                        pin.output = fallback.to_string();
                    }
                }
            }
            None => {
                self.entry_outputs.retain(|_, name| name != output);
                self.screen_pins.retain(|_, pin| pin.output != output);
            }
        }
    }

    pub fn insert(
        &mut self,
        output: &str,
        viewport: Size<i32, Logical>,
        window: Window,
        target: Option<&WlSurface>,
    ) {
        self.ensure_default_reef(viewport);
        let camera = self.ensure_camera(output);
        let center = OceanPoint {
            x: camera.origin.x + viewport.w as f64 / 2.0,
            y: camera.origin.y + viewport.h as f64 / 2.0,
        };
        let reef_index = nearest_reef(&self.reefs, center).unwrap_or(0);
        if let Some(surface) = window.toplevel().map(|toplevel| toplevel.wl_surface()) {
            self.entry_outputs
                .insert(surface.clone(), output.to_string());
        }
        self.reefs[reef_index].layout.insert(window, target);
    }

    pub fn remove(&mut self, surface: &WlSurface) {
        for reef in &mut self.reefs {
            reef.layout.remove(surface);
        }
        self.entry_outputs.remove(surface);
        self.floating.remove(surface);
        self.screen_pins.remove(surface);
    }

    pub fn contains(&self, surface: &WlSurface) -> bool {
        self.is_tiled(surface) || self.floating.contains_key(surface)
    }

    pub fn is_tiled(&self, surface: &WlSurface) -> bool {
        self.reefs.iter().any(|reef| reef.layout.contains(surface))
    }

    pub fn window(&self, surface: &WlSurface) -> Option<Window> {
        self.reefs
            .iter()
            .find_map(|reef| reef.layout.window(surface))
            .or_else(|| self.floating.get(surface).map(|(window, _)| window.clone()))
    }

    pub fn entry_output(&self, surface: &WlSurface) -> Option<&str> {
        self.entry_outputs.get(surface).map(String::as_str)
    }

    pub fn set_entry_output(&mut self, surface: &WlSurface, output: String) {
        if self.contains(surface) {
            self.entry_outputs.insert(surface.clone(), output);
        }
    }

    /// Stable world geometry for every Ocean tile. This is also the sizing
    /// authority used to configure clients; camera motion never resizes one.
    pub fn tiled_layouts(
        &self,
        gap: i32,
        split_bias: SplitBias,
    ) -> Vec<(Window, Rectangle<i32, Logical>)> {
        self.reefs
            .iter()
            .flat_map(|reef| reef.layout.layout(reef.rect, gap, split_bias))
            .collect()
    }

    pub fn make_floating(&mut self, surface: &WlSurface, gap: i32, split_bias: SplitBias) -> bool {
        let Some((window, rect)) =
            self.tiled_layouts(gap, split_bias)
                .into_iter()
                .find(|(window, _)| {
                    window
                        .toplevel()
                        .is_some_and(|toplevel| toplevel.wl_surface() == surface)
                })
        else {
            return false;
        };
        for reef in &mut self.reefs {
            reef.layout.remove(surface);
        }
        self.floating.insert(surface.clone(), (window, rect));
        true
    }

    pub fn make_tiled(
        &mut self,
        surface: &WlSurface,
        output: &str,
        viewport: Size<i32, Logical>,
        target: Option<&WlSurface>,
    ) -> bool {
        let Some((window, _)) = self.floating.remove(surface) else {
            return false;
        };
        self.screen_pins.remove(surface);
        self.insert(output, viewport, window, target);
        true
    }

    pub fn set_floating_rect(
        &mut self,
        surface: &WlSurface,
        rect: Rectangle<i32, Logical>,
    ) -> bool {
        let Some((_, current)) = self.floating.get_mut(surface) else {
            return false;
        };
        *current = rect;
        true
    }

    pub fn floating_rect(&self, surface: &WlSurface) -> Option<Rectangle<i32, Logical>> {
        self.floating.get(surface).map(|(_, rect)| *rect)
    }

    pub fn pin_to_screen(&mut self, surface: &WlSurface, output: &str) -> bool {
        if self.screen_pins.contains_key(surface) {
            return true;
        }
        let Some((_, rect)) = self.floating.get(surface) else {
            return false;
        };
        let camera = self.camera(output);
        self.screen_pins.insert(
            surface.clone(),
            OceanScreenPin {
                output: output.to_string(),
                viewport_loc: Point::from((
                    rect.loc.x as f64 - camera.origin.x,
                    rect.loc.y as f64 - camera.origin.y,
                )),
                size: rect.size,
            },
        );
        true
    }

    pub fn unpin_from_screen(&mut self, surface: &WlSurface) -> bool {
        self.screen_pins.remove(surface).is_some()
    }

    pub(crate) fn world_layouts(
        &self,
        gap: i32,
        split_bias: SplitBias,
    ) -> Vec<(Window, Rectangle<i32, Logical>, PlacementKind)> {
        self.floating
            .values()
            .map(|(window, rect)| (window.clone(), *rect, PlacementKind::Floating))
            .chain(
                self.tiled_layouts(gap, split_bias)
                    .into_iter()
                    .map(|(window, rect)| (window, rect, PlacementKind::Tiled)),
            )
            .collect()
    }

    /// Produces one output camera's view of the shared world. World geometry
    /// stays unchanged; only `view_offset` translates it into this viewport.
    pub(crate) fn placements(
        &self,
        output: &str,
        output_geo: Rectangle<i32, Logical>,
        gap: i32,
        split_bias: SplitBias,
    ) -> Vec<PlacedWindow> {
        let camera = self.camera(output);
        let view_offset = Point::from((
            output_geo.loc.x as f64 - camera.origin.x,
            output_geo.loc.y as f64 - camera.origin.y,
        ));
        let mut layouts = self.world_layouts(gap, split_bias);
        // Floating entries were collected first and are already frontmost.
        // Reverse only the tiled suffix so cascade/BSP tree order mirrors
        // Classic's front-to-back renderer contract.
        let floating_count = self.floating.len();
        layouts[floating_count..].reverse();
        layouts
            .into_iter()
            .filter_map(|(window, rect, kind)| {
                let pin = window
                    .toplevel()
                    .and_then(|toplevel| self.screen_pins.get(toplevel.wl_surface()));
                if let Some(pin) = pin {
                    if pin.output != output {
                        return None;
                    }
                    let rect = Rectangle::new(
                        output_geo.loc
                            + Point::from((
                                pin.viewport_loc.x.round() as i32,
                                pin.viewport_loc.y.round() as i32,
                            )),
                        pin.size,
                    );
                    return Some(PlacedWindow::authoritative(window, rect).with_kind(kind));
                }
                visible_through_camera(rect, camera, output_geo.size).then(|| {
                    PlacedWindow::authoritative(window, rect)
                        .with_view_offset(view_offset)
                        .with_kind(kind)
                })
            })
            .collect()
    }
}

fn nearest_reef(reefs: &[OceanReef], point: OceanPoint) -> Option<usize> {
    reefs
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            distance_to_rect(a.rect, point).total_cmp(&distance_to_rect(b.rect, point))
        })
        .map(|(index, _)| index)
}

fn distance_to_rect(rect: Rectangle<i32, Logical>, point: OceanPoint) -> f64 {
    let left = rect.loc.x as f64;
    let top = rect.loc.y as f64;
    let right = left + rect.size.w as f64;
    let bottom = top + rect.size.h as f64;
    let dx = if point.x < left {
        left - point.x
    } else if point.x > right {
        point.x - right
    } else {
        0.0
    };
    let dy = if point.y < top {
        top - point.y
    } else if point.y > bottom {
        point.y - bottom
    } else {
        0.0
    };
    dx * dx + dy * dy
}

fn visible_through_camera(
    rect: Rectangle<i32, Logical>,
    camera: OceanCamera,
    viewport: Size<i32, Logical>,
) -> bool {
    let left = rect.loc.x as f64 - camera.origin.x;
    let top = rect.loc.y as f64 - camera.origin.y;
    let right = left + rect.size.w as f64;
    let bottom = top + rect.size.h as f64;
    right > 0.0 && bottom > 0.0 && left < viewport.w as f64 && top < viewport.h as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OceanBookmarkConfig, OceanReefConfig};

    #[test]
    fn cameras_are_independent_views_of_one_world() {
        let mut ocean = OceanSpace::from_config(&OceanConfig::default());
        ocean.ensure_default_reef(Size::from((1200, 800)));
        ocean.pan("left", 300.0, 40.0);
        ocean.pan("right", -75.0, 900.0);

        assert_eq!(
            ocean.camera("left").origin,
            OceanPoint { x: 300.0, y: 40.0 }
        );
        assert_eq!(
            ocean.camera("right").origin,
            OceanPoint { x: -75.0, y: 900.0 }
        );
    }

    #[test]
    fn named_bookmarks_return_a_camera_without_moving_another() {
        let config = OceanConfig {
            camera_step: 480,
            reefs: Vec::new(),
            bookmarks: vec![OceanBookmarkConfig {
                name: "code".to_string(),
                x: 2200.5,
                y: -40.0,
            }],
        };
        let mut ocean = OceanSpace::from_config(&config);
        ocean.pan("other", 8.0, 9.0);

        assert!(ocean.jump_to_bookmark("main", "code"));
        assert_eq!(
            ocean.camera("main").origin,
            OceanPoint {
                x: 2200.5,
                y: -40.0
            }
        );
        assert_eq!(ocean.camera("other").origin, OceanPoint { x: 8.0, y: 9.0 });
        assert!(!ocean.jump_to_bookmark("main", "missing"));
    }

    #[test]
    fn nearest_reef_uses_world_distance_not_list_order() {
        let ocean = OceanSpace::from_config(&OceanConfig {
            camera_step: 480,
            reefs: vec![
                OceanReefConfig {
                    name: "home".to_string(),
                    x: 0,
                    y: 0,
                    width: Some(1000),
                    height: Some(800),
                },
                OceanReefConfig {
                    name: "deep".to_string(),
                    x: 0,
                    y: 2000,
                    width: Some(1000),
                    height: Some(800),
                },
            ],
            bookmarks: Vec::new(),
        });

        assert_eq!(
            nearest_reef(
                &ocean.reefs,
                OceanPoint {
                    x: 500.0,
                    y: 2300.0
                }
            ),
            Some(1)
        );
    }

    #[test]
    fn auto_dimensions_follow_larger_real_viewports_while_fixed_ones_stay_fixed() {
        let mut ocean = OceanSpace::from_config(&OceanConfig {
            camera_step: 480,
            reefs: vec![OceanReefConfig {
                name: "wide".to_string(),
                x: 0,
                y: 0,
                width: None,
                height: Some(1200),
            }],
            bookmarks: Vec::new(),
        });

        assert!(ocean.ensure_default_reef(Size::from((1920, 1080))));
        assert_eq!(ocean.reefs[0].rect.size, Size::from((1920, 1200)));
        assert!(ocean.ensure_default_reef(Size::from((3440, 1440))));
        assert_eq!(ocean.reefs[0].rect.size, Size::from((3440, 1200)));
        assert!(!ocean.ensure_default_reef(Size::from((2560, 1080))));
    }

    #[test]
    fn visibility_is_camera_relative_in_both_axes() {
        let rect = Rectangle::new(Point::from((2000, 2000)), Size::from((400, 300)));
        let viewport = Size::from((1000, 700));
        assert!(!visible_through_camera(
            rect,
            OceanCamera::default(),
            viewport
        ));
        assert!(visible_through_camera(
            rect,
            OceanCamera {
                origin: OceanPoint {
                    x: 1700.0,
                    y: 1800.0
                }
            },
            viewport
        ));
    }
}
