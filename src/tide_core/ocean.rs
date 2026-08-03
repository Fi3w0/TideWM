//! Continuous-world ownership for TideWM's Ocean spatial engine.
//!
//! Ocean is intentionally not implemented as a tall stack of Classic
//! workspaces. Windows belong to local [`OceanReef`] tiling zones in stable
//! world coordinates, while each physical output owns only a camera position
//! into that shared world. Rendering converts the resulting world rectangles
//! into the shared [`PlacedWindow`](crate::placement::PlacedWindow) boundary.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OceanCamera {
    /// World point shown at the output viewport's top-left corner.
    pub origin: OceanPoint,
    /// View pixels per world logical pixel.
    pub zoom: f64,
}

impl Default for OceanCamera {
    fn default() -> Self {
        Self {
            origin: OceanPoint::default(),
            zoom: 1.0,
        }
    }
}

struct OceanCameraMotion {
    from: OceanCamera,
    to: OceanCamera,
    started: Instant,
    duration: Duration,
    sway_screen: f64,
}

impl OceanCameraMotion {
    fn sample(&self) -> OceanCamera {
        if self.duration.is_zero() {
            return self.to;
        }
        let linear =
            (self.started.elapsed().as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0);
        let eased = linear * linear * (3.0 - 2.0 * linear);
        let zoom = if self.from.zoom > 0.0 && self.to.zoom > 0.0 {
            self.from.zoom * (self.to.zoom / self.from.zoom).powf(eased)
        } else {
            self.to.zoom
        };
        let dx = self.to.origin.x - self.from.origin.x;
        let dy = self.to.origin.y - self.from.origin.y;
        let distance = dx.hypot(dy);
        let arc = if distance > f64::EPSILON {
            (std::f64::consts::PI * linear).sin() * self.sway_screen / zoom.max(0.05)
        } else {
            0.0
        };
        let normal_x = if distance > f64::EPSILON {
            -dy / distance
        } else {
            0.0
        };
        let normal_y = if distance > f64::EPSILON {
            dx / distance
        } else {
            0.0
        };
        OceanCamera {
            origin: OceanPoint {
                x: self.from.origin.x + dx * eased + normal_x * arc,
                y: self.from.origin.y + dy * eased + normal_y * arc,
            },
            zoom,
        }
    }

    fn active(&self) -> bool {
        self.started.elapsed() < self.duration
    }
}

pub struct OceanReef {
    _name: String,
    pub rect: Rectangle<i32, Logical>,
    auto_width: bool,
    auto_height: bool,
    /// The implicit starting reef follows the camera until its first window
    /// is inserted. Configured reefs remain fixed world landmarks.
    anchor_empty_layout_to_camera: bool,
    layout: BspLayout,
}

struct OceanScreenPin {
    output: String,
    viewport_loc: Point<f64, Logical>,
    size: Size<i32, Logical>,
    view_scale: f64,
}

impl OceanReef {
    fn new(
        name: String,
        rect: Rectangle<i32, Logical>,
        auto_width: bool,
        auto_height: bool,
        anchor_empty_layout_to_camera: bool,
    ) -> Self {
        Self {
            _name: name,
            rect,
            auto_width,
            auto_height,
            anchor_empty_layout_to_camera,
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
    camera_motions: HashMap<String, OceanCameraMotion>,
    bookmarks: HashMap<String, OceanPoint>,
    runtime_bookmarks: HashSet<String>,
    /// Output where a window entered the world. This is an input/focus hint,
    /// not spatial ownership; every output can render the same window.
    entry_outputs: HashMap<WlSurface, String>,
    floating: HashMap<WlSurface, (Window, Rectangle<i32, Logical>)>,
    /// Optional client-sized rectangles for windows that remain in a reef
    /// tree after smart reattachment. The tree still owns their slot; the
    /// override only changes the rendered/configured size around that slot.
    attached_sizes: HashMap<WlSurface, Size<i32, Logical>>,
    /// Front-to-back order for freely placed windows. A HashMap's iteration
    /// order is deliberately unstable and must never become visible stacking
    /// policy once Ocean windows are allowed to overlap arbitrarily.
    floating_stack: Vec<WlSurface>,
    screen_pins: HashMap<WlSurface, OceanScreenPin>,
    /// Live world-space rectangle for a tiled window mid `OceanTileMoveGrab`
    /// drag, plus the surface it would swap into on release. The reef tree
    /// itself stays frozen during the gesture (see that grab's own doc
    /// comment); without this override `placements()` would keep rendering
    /// the window at its frozen slot; `Space` reflects the drag position
    /// too, but only rendering fed from `Space` reads it, and Ocean's does
    /// not. Both fields are `None` outside an active drag.
    drag_override: Option<(WlSurface, Rectangle<i32, Logical>)>,
    drag_hint: Option<WlSurface>,
    /// Insertion-order record of every window Ocean has ever mapped, for
    /// numbered app-slot switching (`Action::SwitchWorkspace`'s Ocean
    /// branch: `$mod+1` jumps to slot 1, etc.) -- "slot N" is the Nth
    /// surface here, 1-based. Appended to only by `record_app_opened`
    /// (called once per window from `map_toplevel`, not from `insert`
    /// itself, so a floating<->tiled reattach never reorders or re-adds an
    /// entry) and pruned by `remove`, which shifts every later slot down
    /// to fill the gap rather than leaving a hole.
    app_order: Vec<WlSurface>,
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
                    false,
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
            camera_motions: HashMap::new(),
            bookmarks,
            runtime_bookmarks: HashSet::new(),
            entry_outputs: HashMap::new(),
            floating: HashMap::new(),
            attached_sizes: HashMap::new(),
            floating_stack: Vec::new(),
            screen_pins: HashMap::new(),
            drag_override: None,
            drag_hint: None,
            app_order: Vec::new(),
        }
    }

    /// Materializes Ocean's implicit starting reef using the real viewport
    /// size. A configured `home` bookmark is also a valid starting location:
    /// when it does not fall inside any configured reef, create an implicit
    /// reef there so the first mapped window is visible instead of being
    /// inserted into a distant configured reef while the camera remains at
    /// the empty bookmark location.
    pub fn ensure_default_reef(&mut self, viewport: Size<i32, Logical>) -> bool {
        let mut changed = false;
        if self.reefs.is_empty() {
            self.reefs.push(OceanReef::new(
                "main".to_string(),
                Rectangle::new(Point::from((0, 0)), viewport),
                true,
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
        if let Some(home) = self.bookmarks.get("home").copied() {
            let home_is_covered = self
                .reefs
                .iter()
                .any(|reef| rectangle_contains_point(reef.rect, home));
            let implicit_start_exists = self
                .reefs
                .iter()
                .any(|reef| reef.anchor_empty_layout_to_camera);
            if !home_is_covered && !implicit_start_exists {
                self.reefs.push(OceanReef::new(
                    "main".to_string(),
                    Rectangle::new(
                        Point::from((home.x.round() as i32, home.y.round() as i32)),
                        viewport,
                    ),
                    true,
                    true,
                    true,
                ));
                changed = true;
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
        let fallback = OceanCamera {
            origin: initial,
            zoom: 1.0,
        };
        self.cameras.entry(output.to_string()).or_insert(fallback);
        self.camera(output)
    }

    pub fn camera(&self, output: &str) -> OceanCamera {
        if let Some(motion) = self.camera_motions.get(output) {
            return motion.sample();
        }
        self.cameras.get(output).copied().unwrap_or_else(|| {
            let origin = self.bookmarks.get("home").copied().unwrap_or_default();
            OceanCamera { origin, zoom: 1.0 }
        })
    }

    /// Configured reef rectangles, for the minimap's world extent -- an
    /// empty reef is still a landmark worth framing even with nothing
    /// inside it yet.
    pub(crate) fn reefs(&self) -> &[OceanReef] {
        &self.reefs
    }

    fn set_camera(
        &mut self,
        output: &str,
        target: OceanCamera,
        duration: Duration,
        sway_screen: f64,
    ) {
        let current = self.ensure_camera(output);
        self.cameras.insert(output.to_string(), target);
        if duration.is_zero() || current == target {
            self.camera_motions.remove(output);
        } else {
            self.camera_motions.insert(
                output.to_string(),
                OceanCameraMotion {
                    from: current,
                    to: target,
                    started: Instant::now(),
                    duration,
                    sway_screen,
                },
            );
        }
    }

    pub fn has_active_camera_motion(&self) -> bool {
        self.camera_motions.values().any(OceanCameraMotion::active)
    }

    pub fn clamp_zooms(&mut self, min_zoom: f64, max_zoom: f64) {
        for camera in self.cameras.values_mut() {
            camera.zoom = camera.zoom.clamp(min_zoom, max_zoom);
        }
        self.camera_motions.clear();
    }

    pub fn pan(&mut self, output: &str, dx: f64, dy: f64) {
        let current = self.ensure_camera(output);
        self.set_camera(
            output,
            OceanCamera {
                origin: current.origin.translated(dx, dy),
                zoom: current.zoom,
            },
            Duration::ZERO,
            0.0,
        );
    }

    pub fn animate_pan(
        &mut self,
        output: &str,
        dx: f64,
        dy: f64,
        duration: Duration,
        sway_screen: f64,
    ) {
        let current = self.ensure_camera(output);
        self.set_camera(
            output,
            OceanCamera {
                origin: current.origin.translated(dx, dy),
                zoom: current.zoom,
            },
            duration,
            sway_screen,
        );
    }

    pub fn jump_to_bookmark(&mut self, output: &str, name: &str) -> bool {
        let Some(origin) = self.bookmarks.get(name).copied() else {
            return false;
        };
        self.cameras.insert(
            output.to_string(),
            OceanCamera {
                origin,
                zoom: self.camera(output).zoom,
            },
        );
        self.camera_motions.remove(output);
        true
    }

    pub fn animate_to_bookmark(
        &mut self,
        output: &str,
        name: &str,
        duration: Duration,
        sway_screen: f64,
    ) -> bool {
        let Some(origin) = self.bookmarks.get(name).copied() else {
            return false;
        };
        let current = self.ensure_camera(output);
        self.set_camera(
            output,
            OceanCamera {
                origin,
                zoom: current.zoom,
            },
            duration,
            sway_screen,
        );
        true
    }

    /// Same as `animate_to_bookmark`, but for a literal world point instead
    /// of a named one -- used by app-slot switching's "no apps open" case,
    /// which means the world origin specifically, not whatever a user's
    /// `home` bookmark happens to be configured to.
    pub fn animate_to_point(
        &mut self,
        output: &str,
        origin: OceanPoint,
        duration: Duration,
        sway_screen: f64,
    ) {
        let current = self.ensure_camera(output);
        self.set_camera(
            output,
            OceanCamera {
                origin,
                zoom: current.zoom,
            },
            duration,
            sway_screen,
        );
    }

    pub fn zoom_at(
        &mut self,
        output: &str,
        viewport_anchor: Point<f64, Logical>,
        target_zoom: f64,
        duration: Duration,
    ) {
        let current = self.ensure_camera(output);
        let target_zoom = target_zoom.max(0.05);
        let world_anchor = OceanPoint {
            x: current.origin.x + viewport_anchor.x / current.zoom,
            y: current.origin.y + viewport_anchor.y / current.zoom,
        };
        self.set_camera(
            output,
            OceanCamera {
                origin: OceanPoint {
                    x: world_anchor.x - viewport_anchor.x / target_zoom,
                    y: world_anchor.y - viewport_anchor.y / target_zoom,
                },
                zoom: target_zoom,
            },
            duration,
            0.0,
        );
    }

    pub fn center_on_rect(
        &mut self,
        output: &str,
        viewport: Size<i32, Logical>,
        rect: Rectangle<i32, Logical>,
        duration: Duration,
        sway_screen: f64,
    ) {
        let current = self.ensure_camera(output);
        let visible_world_w = viewport.w as f64 / current.zoom;
        let visible_world_h = viewport.h as f64 / current.zoom;
        let center: Point<f64, Logical> = Point::from((
            rect.loc.x as f64 + rect.size.w as f64 / 2.0,
            rect.loc.y as f64 + rect.size.h as f64 / 2.0,
        ));
        self.set_camera(
            output,
            OceanCamera {
                origin: OceanPoint {
                    x: center.x - visible_world_w / 2.0,
                    y: center.y - visible_world_h / 2.0,
                },
                zoom: current.zoom,
            },
            duration,
            sway_screen,
        );
    }

    pub fn navigate_depth(
        &mut self,
        output: &str,
        viewport: Size<i32, Logical>,
        down: bool,
        motion: (Duration, f64),
    ) -> bool {
        let current = self.ensure_camera(output);
        let half_view = viewport.h as f64 / current.zoom.max(0.05) * 0.5;
        let mut anchors: Vec<f64> = self
            .reefs
            .iter()
            .map(|reef| reef.rect.loc.y as f64)
            // A tiled window's Y position is only its local slot inside a
            // reef. Treating every tile row as a navigation stop makes
            // depth feel like vertically stacked workspaces. Floating
            // rectangles are explicit world placements, so sunk/manual
            // windows remain meaningful content-driven destinations.
            .chain(self.floating.values().map(|(_, rect)| rect.loc.y as f64))
            .collect();
        anchors.sort_by(f64::total_cmp);
        anchors.dedup_by(|a, b| (*a - *b).abs() < 1.0);
        let target_y = if down {
            anchors
                .into_iter()
                .find(|anchor| *anchor > current.origin.y + half_view)
        } else {
            anchors
                .into_iter()
                .rev()
                .find(|anchor| *anchor < current.origin.y - half_view)
        };
        let Some(target_y) = target_y else {
            return false;
        };
        self.set_camera(
            output,
            OceanCamera {
                origin: OceanPoint {
                    x: current.origin.x,
                    y: target_y,
                },
                zoom: current.zoom,
            },
            motion.0,
            motion.1,
        );
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
        self.camera_motions.remove(output);
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
        let reef_index = target
            .and_then(|target| {
                self.reefs
                    .iter()
                    .position(|reef| reef.layout.contains(target))
            })
            .or_else(|| nearest_reef(&self.reefs, center))
            .unwrap_or(0);
        self.anchor_empty_reef_to_camera(reef_index, camera);
        if let Some(surface) = window.toplevel().map(|toplevel| toplevel.wl_surface()) {
            self.entry_outputs
                .insert(surface.clone(), output.to_string());
        }
        self.reefs[reef_index].layout.insert(window, target);
    }

    fn anchor_empty_reef_to_camera(&mut self, reef_index: usize, camera: OceanCamera) -> bool {
        let reef = &mut self.reefs[reef_index];
        if !reef.anchor_empty_layout_to_camera || !reef.layout.windows().is_empty() {
            return false;
        }
        reef.rect.loc = Point::from((
            camera.origin.x.round() as i32,
            camera.origin.y.round() as i32,
        ));
        true
    }

    pub fn remove(&mut self, surface: &WlSurface) {
        for reef in &mut self.reefs {
            reef.layout.remove(surface);
        }
        self.entry_outputs.remove(surface);
        self.floating.remove(surface);
        self.attached_sizes.remove(surface);
        self.floating_stack.retain(|candidate| candidate != surface);
        self.screen_pins.remove(surface);
        self.app_order.retain(|candidate| candidate != surface);
        if self
            .drag_override
            .as_ref()
            .is_some_and(|(dragged, _)| dragged == surface)
        {
            self.drag_override = None;
        }
        if self.drag_hint.as_ref() == Some(surface) {
            self.drag_hint = None;
        }
    }

    /// Records a newly mapped window at the end of the app-slot order. A
    /// no-op if `surface` is already recorded (defensive; `map_toplevel`
    /// only calls this once per window's lifetime, but a duplicate here
    /// would otherwise silently reorder every later slot).
    pub fn record_app_opened(&mut self, surface: WlSurface) {
        if !self.app_order.contains(&surface) {
            self.app_order.push(surface);
        }
    }

    /// The `index`-th (1-based) window in app-opened order, if that slot
    /// exists -- `app_slot(1)` is the first window still open, not
    /// necessarily the first ever opened, since `remove` shifts later
    /// slots down when an earlier one closes.
    pub fn app_slot(&self, index: usize) -> Option<&WlSurface> {
        index.checked_sub(1).and_then(|i| self.app_order.get(i))
    }

    pub fn has_open_apps(&self) -> bool {
        !self.app_order.is_empty()
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
            .map(|(window, rect)| {
                let surface = window.toplevel().map(|toplevel| toplevel.wl_surface());
                let rect = surface
                    .and_then(|surface| self.attached_sizes.get(surface))
                    .map(|size| centered_attached_rect(rect, *size))
                    .unwrap_or(rect);
                (window, rect)
            })
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
        self.attached_sizes.remove(surface);
        self.floating.insert(surface.clone(), (window, rect));
        self.raise_floating(surface);
        true
    }

    pub fn make_tiled(
        &mut self,
        surface: &WlSurface,
        output: &str,
        viewport: Size<i32, Logical>,
        target: Option<&WlSurface>,
        gap: i32,
        split_bias: SplitBias,
    ) -> bool {
        self.make_tiled_with_size(surface, output, viewport, target, false, gap, split_bias)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn make_tiled_with_size(
        &mut self,
        surface: &WlSurface,
        output: &str,
        viewport: Size<i32, Logical>,
        target: Option<&WlSurface>,
        preserve_size: bool,
        gap: i32,
        split_bias: SplitBias,
    ) -> bool {
        let Some((window, rect)) = self.floating.remove(surface) else {
            return false;
        };
        let attached_size = rect.size;
        self.floating_stack.retain(|candidate| candidate != surface);
        self.screen_pins.remove(surface);
        self.insert(output, viewport, window, target);
        if preserve_size {
            self.attached_sizes.insert(surface.clone(), attached_size);
            // Grow the reef to fit the preserved size instead of leaving
            // `centered_attached_rect` to clamp it down -- Ocean's whole
            // premise is a world that isn't bound to a fixed screen
            // rectangle, so a reattached window keeping its size should
            // push the reef outward, not get squeezed into whatever slot
            // a viewport-sized tree happened to leave. Classic's BspLayout
            // always divides a fixed output rect and never reaches this.
            self.grow_reef_to_fit(surface, attached_size, gap, split_bias);
        }
        true
    }

    /// Grows the reef containing `surface` -- within its own `auto_width`/
    /// `auto_height` flags -- until `surface`'s own slot is at least
    /// `needed`. Capped at the nearest other reef's edge so one reef's
    /// growth can never reach into another and reintroduce the exact
    /// overlap this path exists to avoid. If growth is disabled or capped
    /// before the slot is big enough, `centered_attached_rect`'s own clamp
    /// is still the backstop that keeps the render rect inside its slot.
    fn grow_reef_to_fit(
        &mut self,
        surface: &WlSurface,
        needed: Size<i32, Logical>,
        gap: i32,
        split_bias: SplitBias,
    ) {
        let Some(reef_index) = self.reefs.iter().position(|reef| reef.layout.contains(surface))
        else {
            return;
        };
        let (max_w, max_h) = self.growth_ceiling(reef_index);
        // ponytail: fixed 8-round measure-and-grow rather than solving the
        // dwindle tree's split math for an exact minimum rect -- each round
        // measures the real slot through the same `layout()` the renderer
        // itself calls, so it can never drift from what's actually on
        // screen. Upgrade to a closed-form solve if 8 rounds ever visibly
        // fails to converge for a real tree depth.
        for _ in 0..8 {
            let reef = &self.reefs[reef_index];
            if !reef.auto_width && !reef.auto_height {
                return;
            }
            let Some(slot_size) = reef
                .layout
                .layout(reef.rect, gap, split_bias)
                .into_iter()
                .find(|(window, _)| {
                    window
                        .toplevel()
                        .is_some_and(|toplevel| toplevel.wl_surface() == surface)
                })
                .map(|(_, rect)| rect.size)
            else {
                return;
            };
            let deficit_w = if reef.auto_width {
                (needed.w - slot_size.w).max(0)
            } else {
                0
            };
            let deficit_h = if reef.auto_height {
                (needed.h - slot_size.h).max(0)
            } else {
                0
            };
            if deficit_w == 0 && deficit_h == 0 {
                return;
            }
            let reef = &mut self.reefs[reef_index];
            let new_w = (reef.rect.size.w + deficit_w).min(max_w);
            let new_h = (reef.rect.size.h + deficit_h).min(max_h);
            if new_w == reef.rect.size.w && new_h == reef.rect.size.h {
                return; // Capped by a neighboring reef; can't grow further.
            }
            reef.rect.size.w = new_w;
            reef.rect.size.h = new_h;
        }
    }

    /// The largest (width, height) `reefs[index]` could grow to, keeping its
    /// `rect.loc` fixed, without reaching another reef that currently spans
    /// its row/column. Deliberately axis-aligned rather than full 2D
    /// expansion math: reefs are user-configured landmarks meant to be laid
    /// out in a simple row or column (this project's own config included),
    /// and solving exact diagonal-neighbor clearance isn't worth it before
    /// that layout is something anyone actually uses.
    fn growth_ceiling(&self, index: usize) -> (i32, i32) {
        let rect = self.reefs[index].rect;
        let mut max_w = i32::MAX;
        let mut max_h = i32::MAX;
        for (other_index, other) in self.reefs.iter().enumerate() {
            if other_index == index {
                continue;
            }
            let y_overlap = rect.loc.y < other.rect.loc.y + other.rect.size.h
                && other.rect.loc.y < rect.loc.y + rect.size.h;
            if y_overlap && other.rect.loc.x >= rect.loc.x + rect.size.w {
                max_w = max_w.min(other.rect.loc.x - rect.loc.x);
            }
            let x_overlap = rect.loc.x < other.rect.loc.x + other.rect.size.w
                && other.rect.loc.x < rect.loc.x + rect.size.w;
            if x_overlap && other.rect.loc.y >= rect.loc.y + rect.size.h {
                max_h = max_h.min(other.rect.loc.y - rect.loc.y);
            }
        }
        (max_w.max(rect.size.w), max_h.max(rect.size.h))
    }

    pub fn smart_tiling_target(
        &self,
        surface: &WlSurface,
        output: &str,
        pointer_view: Point<f64, Logical>,
        snap_distance: i32,
        gap: i32,
        split_bias: SplitBias,
    ) -> Option<WlSurface> {
        let moving = self.floating_rect(surface)?;
        let camera = self.camera(output);
        let zoom = camera.zoom.max(0.05);
        let pointer_world = OceanPoint {
            x: camera.origin.x + pointer_view.x / zoom,
            y: camera.origin.y + pointer_view.y / zoom,
        };
        let max_gap = snap_distance.max(0) as f64 / zoom;
        self.tiled_layouts(gap, split_bias)
            .into_iter()
            .filter_map(|(window, rect)| {
                let target = window.toplevel()?.wl_surface().clone();
                (target != *surface).then_some((target, rect))
            })
            .filter_map(|(target, rect)| {
                let gap = rectangle_gap_distance(moving, rect);
                // `distance_to_rect` returns a squared distance (fine for
                // its other callers, which only ever rank candidates
                // against each other); this compares it against a real
                // screen-pixel threshold, so it needs the actual distance.
                let pointer_distance = distance_to_rect(rect, pointer_world).sqrt();
                (gap <= max_gap
                    && pointer_distance <= max_gap + moving.size.w.max(moving.size.h) as f64)
                    .then_some((target, gap))
            })
            .min_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(target, _)| target)
    }

    pub fn tiled_target_at_view(
        &self,
        surface: &WlSurface,
        output: &str,
        pointer_view: Point<f64, Logical>,
        gap: i32,
        split_bias: SplitBias,
    ) -> Option<WlSurface> {
        let camera = self.camera(output);
        let zoom = camera.zoom.max(0.05);
        let world = OceanPoint {
            x: camera.origin.x + pointer_view.x / zoom,
            y: camera.origin.y + pointer_view.y / zoom,
        };
        self.tiled_layouts(gap, split_bias)
            .into_iter()
            .filter_map(|(window, rect)| {
                let target = window.toplevel()?.wl_surface().clone();
                (target != *surface && rectangle_contains_point(rect, world)).then_some(target)
            })
            .next()
    }

    pub fn swap_tiled(&mut self, first: &WlSurface, second: &WlSurface) -> bool {
        if first == second {
            return false;
        }
        let Some(first_reef) = self
            .reefs
            .iter()
            .position(|reef| reef.layout.contains(first))
        else {
            return false;
        };
        let Some(second_reef) = self
            .reefs
            .iter()
            .position(|reef| reef.layout.contains(second))
        else {
            return false;
        };
        if first_reef != second_reef {
            return false;
        }
        self.reefs[first_reef].layout.swap(first, second);
        true
    }

    /// Records `OceanTileMoveGrab`'s live drag position and current swap
    /// target for `placements()`/the border highlight to pick up, without
    /// touching the frozen reef tree. `hint` is the surface a release would
    /// swap into right now, or `None` when the pointer isn't over a valid
    /// target.
    pub fn set_tile_drag(
        &mut self,
        surface: WlSurface,
        rect: Rectangle<i32, Logical>,
        hint: Option<WlSurface>,
    ) {
        self.drag_override = Some((surface, rect));
        self.drag_hint = hint;
    }

    /// Ends a smart-tiling drag. Must run whenever `OceanTileMoveGrab` ends,
    /// however it ends -- a stale override otherwise pins a window at a
    /// phantom rectangle forever.
    pub fn clear_tile_drag(&mut self) {
        self.drag_override = None;
        self.drag_hint = None;
    }

    /// The surface a smart-tiling drag would swap into on release, for the
    /// magnet-highlight border. `None` outside an active drag or when the
    /// pointer isn't over a valid target.
    pub fn drag_hint(&self) -> Option<&WlSurface> {
        self.drag_hint.as_ref()
    }

    pub fn raise_floating(&mut self, surface: &WlSurface) -> bool {
        if !self.floating.contains_key(surface) {
            return false;
        }
        self.floating_stack.retain(|candidate| candidate != surface);
        self.floating_stack.insert(0, surface.clone());
        true
    }

    fn ensure_floating(&mut self, surface: &WlSurface, gap: i32, split_bias: SplitBias) -> bool {
        self.floating.contains_key(surface) || self.make_floating(surface, gap, split_bias)
    }

    pub fn sink_window(
        &mut self,
        surface: &WlSurface,
        output: &str,
        viewport: Size<i32, Logical>,
        gap: i32,
        split_bias: SplitBias,
    ) -> bool {
        if !self.ensure_floating(surface, gap, split_bias) {
            return false;
        }
        let camera = self.camera(output);
        let Some((_, rect)) = self.floating.get_mut(surface) else {
            return false;
        };
        rect.loc.y =
            (camera.origin.y + viewport.h as f64 / camera.zoom.max(0.05) + gap.max(24) as f64)
                .round() as i32;
        self.screen_pins.remove(surface);
        true
    }

    pub fn surface_window(
        &mut self,
        surface: &WlSurface,
        gap: i32,
        split_bias: SplitBias,
    ) -> Option<Rectangle<i32, Logical>> {
        if !self.ensure_floating(surface, gap, split_bias) {
            return None;
        }
        let (_, rect) = self.floating.get_mut(surface)?;
        rect.loc.y = 0;
        self.screen_pins.remove(surface);
        Some(*rect)
    }

    pub fn dredge_nearest(
        &mut self,
        output: &str,
        viewport: Size<i32, Logical>,
        gap: i32,
        split_bias: SplitBias,
    ) -> Option<WlSurface> {
        let camera = self.camera(output);
        let visible_bottom = camera.origin.y + viewport.h as f64 / camera.zoom.max(0.05);
        let view_center_x = camera.origin.x + viewport.w as f64 / camera.zoom.max(0.05) / 2.0;
        let candidate = self
            .world_layouts(gap, split_bias)
            .into_iter()
            .filter_map(|(window, rect, _)| {
                let surface = window.toplevel()?.wl_surface().clone();
                let dy = rect.loc.y as f64 - visible_bottom;
                (dy >= 0.0).then_some((
                    surface,
                    rect,
                    dy,
                    (rect.loc.x as f64 - view_center_x).abs(),
                ))
            })
            .min_by(|a, b| a.2.total_cmp(&b.2).then_with(|| a.3.total_cmp(&b.3)))?;
        if !self.ensure_floating(&candidate.0, gap, split_bias) {
            return None;
        }
        let (_, rect) = self.floating.get_mut(&candidate.0)?;
        let world_view_w = viewport.w as f64 / camera.zoom.max(0.05);
        let world_view_h = viewport.h as f64 / camera.zoom.max(0.05);
        rect.loc.x = (camera.origin.x + (world_view_w - rect.size.w as f64) / 2.0).round() as i32;
        rect.loc.y = (camera.origin.y + (world_view_h - rect.size.h as f64) / 2.0).round() as i32;
        self.screen_pins.remove(&candidate.0);
        Some(candidate.0)
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

    pub fn world_rect(
        &self,
        surface: &WlSurface,
        gap: i32,
        split_bias: SplitBias,
    ) -> Option<Rectangle<i32, Logical>> {
        self.floating_rect(surface).or_else(|| {
            self.tiled_layouts(gap, split_bias)
                .into_iter()
                .find_map(|(window, rect)| {
                    window
                        .toplevel()
                        .is_some_and(|toplevel| toplevel.wl_surface() == surface)
                        .then_some(rect)
                })
        })
    }

    pub fn view_delta_to_world(
        &self,
        output: &str,
        delta: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        let zoom = self.camera(output).zoom.max(0.05);
        Point::from((delta.x / zoom, delta.y / zoom))
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
                    (rect.loc.x as f64 - camera.origin.x) * camera.zoom,
                    (rect.loc.y as f64 - camera.origin.y) * camera.zoom,
                )),
                size: Size::from((
                    (rect.size.w as f64 * camera.zoom).round().max(1.0) as i32,
                    (rect.size.h as f64 * camera.zoom).round().max(1.0) as i32,
                )),
                view_scale: camera.zoom,
            },
        );
        true
    }

    pub fn unpin_from_screen(&mut self, surface: &WlSurface) -> bool {
        let Some(pin) = self.screen_pins.remove(surface) else {
            return false;
        };
        let camera = self.camera(&pin.output);
        if let Some((_, rect)) = self.floating.get_mut(surface) {
            rect.loc = Point::from((
                (camera.origin.x + pin.viewport_loc.x / camera.zoom.max(0.05)).round() as i32,
                (camera.origin.y + pin.viewport_loc.y / camera.zoom.max(0.05)).round() as i32,
            ));
        }
        true
    }

    pub(crate) fn world_layouts(
        &self,
        gap: i32,
        split_bias: SplitBias,
    ) -> Vec<(Window, Rectangle<i32, Logical>, PlacementKind)> {
        self.floating_stack
            .iter()
            .filter_map(|surface| self.floating.get(surface))
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
        let mut layouts = self.world_layouts(gap, split_bias);
        // Floating entries were collected first and are already frontmost.
        // Reverse only the tiled suffix so cascade/BSP tree order mirrors
        // Classic's front-to-back renderer contract.
        let floating_count = self.floating.len();
        layouts[floating_count..].reverse();
        // A tiled window mid `OceanTileMoveGrab` drag renders at its live
        // dragged rectangle instead of its frozen reef slot, lifted to the
        // front like a floating window -- the tree itself stays untouched
        // until the gesture ends, see `set_tile_drag`'s doc comment.
        if let Some((surface, rect)) = &self.drag_override {
            if let Some(pos) = layouts.iter().position(|(window, _, _)| {
                window
                    .toplevel()
                    .is_some_and(|toplevel| toplevel.wl_surface() == surface)
            }) {
                let (window, _, _) = layouts.remove(pos);
                layouts.insert(0, (window, *rect, PlacementKind::Floating));
            }
        }
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
                    return Some(
                        PlacedWindow::authoritative(window, rect)
                            .with_view_scale(pin.view_scale)
                            .fit_content_to_placement()
                            .with_kind(kind),
                    );
                }
                visible_through_camera(rect, camera, output_geo.size).then(|| {
                    let view_rect = world_to_view_rect(rect, camera, output_geo.loc);
                    PlacedWindow::authoritative(window, view_rect)
                        .with_view_scale(camera.zoom)
                        .fit_content_to_placement()
                        .with_kind(kind)
                })
            })
            .collect()
    }
}

fn world_to_view_rect(
    rect: Rectangle<i32, Logical>,
    camera: OceanCamera,
    output_loc: Point<i32, Logical>,
) -> Rectangle<i32, Logical> {
    Rectangle::new(
        Point::from((
            output_loc.x + ((rect.loc.x as f64 - camera.origin.x) * camera.zoom).round() as i32,
            output_loc.y + ((rect.loc.y as f64 - camera.origin.y) * camera.zoom).round() as i32,
        )),
        Size::from((
            (rect.size.w as f64 * camera.zoom).round().max(1.0) as i32,
            (rect.size.h as f64 * camera.zoom).round().max(1.0) as i32,
        )),
    )
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

/// Clamped to `slot`'s own bounds: a preserved floating size larger than the
/// slot it reattached into must never bleed into a neighboring tile's area,
/// even though centering alone would happily do that. Smaller than the slot
/// still centers at its natural size instead of stretching to fill it.
fn centered_attached_rect(
    slot: Rectangle<i32, Logical>,
    size: Size<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let size = Size::from((size.w.min(slot.size.w), size.h.min(slot.size.h)));
    Rectangle::new(
        Point::from((
            slot.loc.x + (slot.size.w - size.w) / 2,
            slot.loc.y + (slot.size.h - size.h) / 2,
        )),
        size,
    )
}

fn rectangle_gap_distance(first: Rectangle<i32, Logical>, second: Rectangle<i32, Logical>) -> f64 {
    let first_right = first.loc.x + first.size.w;
    let first_bottom = first.loc.y + first.size.h;
    let second_right = second.loc.x + second.size.w;
    let second_bottom = second.loc.y + second.size.h;
    let dx = if first_right < second.loc.x {
        (second.loc.x - first_right) as f64
    } else if second_right < first.loc.x {
        (first.loc.x - second_right) as f64
    } else {
        0.0
    };
    let dy = if first_bottom < second.loc.y {
        (second.loc.y - first_bottom) as f64
    } else if second_bottom < first.loc.y {
        (first.loc.y - second_bottom) as f64
    } else {
        0.0
    };
    dx.hypot(dy)
}

fn rectangle_contains_point(rect: Rectangle<i32, Logical>, point: OceanPoint) -> bool {
    point.x >= rect.loc.x as f64
        && point.y >= rect.loc.y as f64
        && point.x < (rect.loc.x + rect.size.w) as f64
        && point.y < (rect.loc.y + rect.size.h) as f64
}

fn visible_through_camera(
    rect: Rectangle<i32, Logical>,
    camera: OceanCamera,
    viewport: Size<i32, Logical>,
) -> bool {
    let left = (rect.loc.x as f64 - camera.origin.x) * camera.zoom;
    let top = (rect.loc.y as f64 - camera.origin.y) * camera.zoom;
    let right = left + rect.size.w as f64 * camera.zoom;
    let bottom = top + rect.size.h as f64 * camera.zoom;
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
            reefs: Vec::new(),
            bookmarks: vec![OceanBookmarkConfig {
                name: "code".to_string(),
                x: 2200.5,
                y: -40.0,
            }],
            ..OceanConfig::default()
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
            ..OceanConfig::default()
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
    fn growth_ceiling_stops_at_a_neighboring_reef_sharing_its_row() {
        let ocean = OceanSpace::from_config(&OceanConfig {
            reefs: vec![
                OceanReefConfig {
                    name: "home".to_string(),
                    x: 0,
                    y: 0,
                    width: Some(1000),
                    height: Some(800),
                },
                // Same row (y-ranges overlap): caps rightward growth.
                OceanReefConfig {
                    name: "code".to_string(),
                    x: 4000,
                    y: 0,
                    width: Some(1000),
                    height: Some(800),
                },
                // Neither row nor column shared with "home": must not
                // constrain its growth in either axis.
                OceanReefConfig {
                    name: "deep".to_string(),
                    x: 2000,
                    y: 5000,
                    width: Some(1000),
                    height: Some(800),
                },
            ],
            bookmarks: Vec::new(),
            ..OceanConfig::default()
        });

        let (max_w, max_h) = ocean.growth_ceiling(0);
        assert_eq!(max_w, 4000); // stops right at "code"'s left edge
        assert_eq!(max_h, i32::MAX); // "deep" doesn't share home's row
    }

    #[test]
    fn growth_ceiling_is_unbounded_with_no_neighbors() {
        let ocean = OceanSpace::from_config(&OceanConfig {
            reefs: vec![OceanReefConfig {
                name: "home".to_string(),
                x: 0,
                y: 0,
                width: Some(1000),
                height: Some(800),
            }],
            bookmarks: Vec::new(),
            ..OceanConfig::default()
        });

        assert_eq!(ocean.growth_ceiling(0), (i32::MAX, i32::MAX));
    }

    #[test]
    fn auto_dimensions_follow_larger_real_viewports_while_fixed_ones_stay_fixed() {
        let mut ocean = OceanSpace::from_config(&OceanConfig {
            reefs: vec![OceanReefConfig {
                name: "wide".to_string(),
                x: 0,
                y: 0,
                width: None,
                height: Some(1200),
            }],
            bookmarks: Vec::new(),
            ..OceanConfig::default()
        });

        assert!(ocean.ensure_default_reef(Size::from((1920, 1080))));
        assert_eq!(ocean.reefs[0].rect.size, Size::from((1920, 1200)));
        assert!(ocean.ensure_default_reef(Size::from((3440, 1440))));
        assert_eq!(ocean.reefs[0].rect.size, Size::from((3440, 1200)));
        assert!(!ocean.ensure_default_reef(Size::from((2560, 1080))));
    }

    #[test]
    fn home_bookmark_gets_an_implicit_reef_when_configured_reefs_start_elsewhere() {
        let mut ocean = OceanSpace::from_config(&OceanConfig {
            reefs: vec![OceanReefConfig {
                name: "code".to_string(),
                x: 4000,
                y: 0,
                width: None,
                height: None,
            }],
            bookmarks: vec![OceanBookmarkConfig {
                name: "home".to_string(),
                x: 0.0,
                y: 0.0,
            }],
            ..OceanConfig::default()
        });

        assert!(ocean.ensure_default_reef(Size::from((1000, 800))));
        assert_eq!(ocean.reefs.len(), 2);
        assert_eq!(ocean.reefs[1].rect.loc, Point::from((0, 0)));
        assert_eq!(ocean.ensure_camera("winit-0").origin, OceanPoint::default());

        ocean.pan("winit-0", 640.0, 320.0);
        let camera = ocean.camera("winit-0");
        assert!(ocean.anchor_empty_reef_to_camera(1, camera));
        assert!(!ocean.ensure_default_reef(Size::from((1000, 800))));
        assert_eq!(ocean.reefs.len(), 2);
    }

    #[test]
    fn empty_implicit_reef_follows_the_camera_for_its_first_window() {
        let mut ocean = OceanSpace::from_config(&OceanConfig::default());
        ocean.ensure_default_reef(Size::from((1000, 800)));
        ocean.pan("main", 640.0, 320.0);

        let camera = ocean.camera("main");
        assert!(ocean.anchor_empty_reef_to_camera(0, camera));
        assert_eq!(ocean.reefs[0].rect.loc, Point::from((640, 320)));
    }

    #[test]
    fn attached_size_clamps_to_the_slot_instead_of_overlapping_neighbors() {
        let slot = Rectangle::new(Point::from((100, 80)), Size::from((400, 300)));
        let attached = centered_attached_rect(slot, Size::from((600, 500)));

        // Clamped to the slot's own size, not the larger preserved size --
        // a bigger rect here would overlap whatever tile sits next door.
        assert_eq!(attached.size, Size::from((400, 300)));
        assert_eq!(attached.loc, slot.loc);
        assert_eq!(rectangle_gap_distance(attached, slot), 0.0);
    }

    #[test]
    fn attached_size_stays_centered_when_smaller_than_the_slot() {
        let slot = Rectangle::new(Point::from((100, 80)), Size::from((400, 300)));
        let attached = centered_attached_rect(slot, Size::from((200, 100)));

        assert_eq!(attached.loc, Point::from((200, 180)));
        assert_eq!(attached.size, Size::from((200, 100)));
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
                },
                zoom: 1.0,
            },
            viewport
        ));
    }

    #[test]
    fn zoom_keeps_the_world_point_under_the_anchor_stable() {
        let mut ocean = OceanSpace::from_config(&OceanConfig::default());
        ocean.pan("main", 100.0, 200.0);
        let anchor = Point::from((400.0, 300.0));
        ocean.zoom_at("main", anchor, 2.0, Duration::ZERO);

        let camera = ocean.camera("main");
        assert_eq!(camera.zoom, 2.0);
        assert_eq!(camera.origin, OceanPoint { x: 300.0, y: 350.0 });
    }

    #[test]
    fn world_rects_scale_around_the_camera_origin() {
        let rect = Rectangle::new(Point::from((1200, 900)), Size::from((600, 400)));
        let view = world_to_view_rect(
            rect,
            OceanCamera {
                origin: OceanPoint {
                    x: 1000.0,
                    y: 800.0,
                },
                zoom: 0.5,
            },
            Point::from((40, 20)),
        );
        assert_eq!(
            view,
            Rectangle::new(Point::from((140, 70)), Size::from((300, 200)))
        );
    }

    #[test]
    fn depth_navigation_uses_real_reef_coordinates() {
        let mut ocean = OceanSpace::from_config(&OceanConfig {
            reefs: vec![
                OceanReefConfig {
                    name: "surface".to_string(),
                    x: 0,
                    y: 0,
                    width: Some(1000),
                    height: Some(800),
                },
                OceanReefConfig {
                    name: "deep".to_string(),
                    x: 300,
                    y: 2400,
                    width: Some(1000),
                    height: Some(800),
                },
            ],
            ..OceanConfig::default()
        });
        assert!(ocean.navigate_depth("main", Size::from((1000, 800)), true, (Duration::ZERO, 0.0),));
        assert_eq!(ocean.camera("main").origin.y, 2400.0);
        assert!(ocean.navigate_depth(
            "main",
            Size::from((1000, 800)),
            false,
            (Duration::ZERO, 0.0),
        ));
        assert_eq!(ocean.camera("main").origin.y, 0.0);
    }
}
