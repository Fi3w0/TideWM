//! Bioluminescent edge-glow compass for the Ocean engine (spatial roadmap
//! S5). A window outside the output camera's viewport leaves a soft glow
//! at the viewport edge in its direction: urgent windows glow in any
//! direction, physically deep (sunk or lower-reef) windows glow below.
//! Nearer windows glow brighter; the cue fades to nothing at the
//! configured maximum distance. Cues are ambient and render-only --
//! camera travel stays on the existing pan/zoom/bookmark/depth actions.
//!
//! One analytical pixel shader, no texture or framebuffer, and no render
//! element at all when nothing is off-screen (the common case), so an
//! idle desktop stays damage-driven. Cue slots are capped so a crowded
//! world cannot grow the element list unboundedly.

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, Uniform, UniformName, UniformType,
        },
        utils::CommitCounter,
    },
    utils::{user_data::UserDataMap, Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};

/// Hard cap on simultaneously rendered cues. Urgent cues sort first, then
/// nearest, so the cap drops only the least important entries.
pub const MAX_CUES: usize = 16;

const COMPASS_SHADER: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform vec3 u_color;
uniform float u_alpha;
uniform float u_angle;

void main() {
    vec2 p = (v_coords - 0.5) * 2.0;
    float c = cos(u_angle);
    float s = sin(u_angle);
    // Rotate so x runs along the cue's outward direction: the glow is
    // narrow across the edge and elongated along it.
    vec2 r = vec2(p.x * c + p.y * s, -p.x * s + p.y * c);
    float d = length(vec2(r.x * 2.4, r.y * 0.85));
    float glow = 1.0 - smoothstep(0.0, 1.0, d);
    glow *= glow;
    float core = 1.0 - smoothstep(0.0, 0.4, d);
    float a = clamp(glow * 0.7 + core * 0.6, 0.0, 1.0) * u_alpha * alpha;
    gl_FragColor = vec4(u_color * a, a);
}
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CueKind {
    Urgent,
    Deep,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompassCue {
    /// Glow center in output-local logical coordinates, on the viewport
    /// edge toward the window.
    pub center: [f32; 2],
    /// Outward direction (radians), orienting the glow's squash.
    pub angle: f32,
    pub color: [f32; 3],
    pub alpha: f32,
    /// Element rect side, logical pixels.
    pub size: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct CompassParams {
    pub urgent_color: [f32; 3],
    pub deep_color: [f32; 3],
    /// World-logical distance beyond the viewport edge at which a cue
    /// fades to nothing.
    pub max_distance: f32,
    pub peak_alpha: f32,
    pub size: f32,
}

/// Maps off-viewport world windows to edge cues. `viewport` is the
/// camera's world-space view rect (origin + output size / zoom). Pure and
/// allocation-bounded so unit tests can pin the geometry exactly.
pub fn compute_cues(
    viewport: Rectangle<f64, Logical>,
    zoom: f64,
    windows: &[(Point<f64, Logical>, CueKind)],
    params: &CompassParams,
) -> Vec<CompassCue> {
    let zoom = zoom.max(0.05);
    let center: Point<f64, Logical> = Point::from((
        viewport.loc.x + viewport.size.w / 2.0,
        viewport.loc.y + viewport.size.h / 2.0,
    ));
    let left = viewport.loc.x;
    let right = viewport.loc.x + viewport.size.w;
    let top = viewport.loc.y;
    let bottom = viewport.loc.y + viewport.size.h;

    let mut cues: Vec<((u8, f32), CompassCue)> = windows
        .iter()
        .filter_map(|(world_center, kind)| {
            let (wx, wy) = (world_center.x, world_center.y);
            if wx >= left && wx <= right && wy >= top && wy <= bottom {
                return None;
            }
            // Deep cues only ever point downward: lateral off-screen
            // travel is ordinary panning, not depth.
            if *kind == CueKind::Deep && wy <= bottom {
                return None;
            }
            let (dx, dy) = (wx - center.x, wy - center.y);
            if dx == 0.0 && dy == 0.0 {
                return None;
            }
            let tx = if dx > 0.0 {
                (right - center.x) / dx
            } else if dx < 0.0 {
                (left - center.x) / dx
            } else {
                f64::INFINITY
            };
            let ty = if dy > 0.0 {
                (bottom - center.y) / dy
            } else if dy < 0.0 {
                (top - center.y) / dy
            } else {
                f64::INFINITY
            };
            let t = tx.min(ty);
            let total = dx.hypot(dy);
            let beyond = (total - t * total).max(0.0) as f32;
            let factor = 1.0 - beyond / params.max_distance.max(1.0);
            if factor <= 0.0 {
                return None;
            }
            let edge_x = center.x + t * dx;
            let edge_y = center.y + t * dy;
            let color = match kind {
                CueKind::Urgent => params.urgent_color,
                CueKind::Deep => params.deep_color,
            };
            // Urgent (rank 0) sorts ahead of deep (rank 1) regardless of
            // distance; within a kind, nearer first.
            let rank = match kind {
                CueKind::Urgent => 0u8,
                CueKind::Deep => 1u8,
            };
            Some((
                (rank, beyond),
                CompassCue {
                    center: [
                        ((edge_x - viewport.loc.x) * zoom) as f32,
                        ((edge_y - viewport.loc.y) * zoom) as f32,
                    ],
                    angle: dy.atan2(dx) as f32,
                    color,
                    alpha: params.peak_alpha * factor,
                    size: params.size,
                },
            ))
        })
        .collect();
    cues.sort_by(|a, b| match a.0 .0.cmp(&b.0 .0) {
        std::cmp::Ordering::Equal => a.0 .1.total_cmp(&b.0 .1),
        other => other,
    });
    cues.truncate(MAX_CUES);
    cues.into_iter().map(|(_, cue)| cue).collect()
}

/// Per-output compass state: stable slot ids plus change detection so the
/// damage tracker repaints exactly when cues move, appear, or vanish, and
/// never while the world is static.
#[derive(Default)]
pub struct Compass {
    slots: Vec<Id>,
    commit: CommitCounter,
    last_cues: Vec<CompassCue>,
}

impl Compass {
    pub fn frame_elements(
        &mut self,
        program: GlesPixelProgram,
        cues: Vec<CompassCue>,
    ) -> Vec<CompassElement> {
        if self.last_cues != cues {
            self.last_cues = cues.clone();
            self.commit.increment();
        }
        while self.slots.len() < cues.len() {
            self.slots.push(Id::new());
        }
        cues.into_iter()
            .enumerate()
            .map(|(index, cue)| {
                let side = cue.size.ceil() as i32;
                let area = Rectangle::new(
                    Point::from((
                        (cue.center[0] - cue.size / 2.0).round() as i32,
                        (cue.center[1] - cue.size / 2.0).round() as i32,
                    )),
                    Size::from((side, side)),
                );
                CompassElement {
                    id: self.slots[index].clone(),
                    commit: self.commit,
                    area,
                    program: program.clone(),
                    cue,
                }
            })
            .collect()
    }
}

pub fn compass_program(
    cache: &mut Option<GlesPixelProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesPixelProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    match renderer.compile_custom_pixel_shader(
        COMPASS_SHADER,
        &[
            UniformName::new("u_color", UniformType::_3f),
            UniformName::new("u_alpha", UniformType::_1f),
            UniformName::new("u_angle", UniformType::_1f),
        ],
    ) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile compass shader");
            None
        }
    }
}

pub struct CompassElement {
    id: Id,
    commit: CommitCounter,
    area: Rectangle<i32, Logical>,
    program: GlesPixelProgram,
    cue: CompassCue,
}

impl Element for CompassElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.area.size.to_f64().to_buffer(1.0, Transform::Normal))
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.area.to_physical_precise_round(scale)
    }

    fn alpha(&self) -> f32 {
        1.0
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for CompassElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        frame.render_pixel_shader_to(
            &self.program,
            src,
            dst,
            self.area.size.to_buffer(1, Transform::Normal),
            Some(damage),
            1.0,
            &[
                Uniform::new("u_color", self.cue.color),
                Uniform::new("u_alpha", self.cue.alpha),
                Uniform::new("u_angle", self.cue.angle),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> CompassParams {
        CompassParams {
            urgent_color: [0.4, 0.9, 1.0],
            deep_color: [0.2, 0.4, 0.6],
            max_distance: 1000.0,
            peak_alpha: 0.85,
            size: 96.0,
        }
    }

    fn viewport() -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((0.0, 0.0)), Size::from((1920.0, 1080.0)))
    }

    #[test]
    fn window_inside_viewport_gets_no_cue() {
        let windows = [(Point::from((500.0, 500.0)), CueKind::Urgent)];
        assert!(compute_cues(viewport(), 1.0, &windows, &params()).is_empty());
    }

    #[test]
    fn urgent_right_of_viewport_lands_on_right_edge() {
        let windows = [(Point::from((2500.0, 540.0)), CueKind::Urgent)];
        let cues = compute_cues(viewport(), 1.0, &windows, &params());
        assert_eq!(cues.len(), 1);
        let cue = cues[0];
        assert!((cue.center[0] - 1920.0).abs() < 0.5);
        assert!((cue.center[1] - 540.0).abs() < 0.5);
        assert!(cue.angle.abs() < 0.01);
        assert_eq!(cue.color, params().urgent_color);
    }

    #[test]
    fn deep_below_viewport_lands_on_bottom_edge() {
        let windows = [(Point::from((960.0, 1600.0)), CueKind::Deep)];
        let cues = compute_cues(viewport(), 1.0, &windows, &params());
        assert_eq!(cues.len(), 1);
        let cue = cues[0];
        assert!((cue.center[0] - 960.0).abs() < 0.5);
        assert!((cue.center[1] - 1080.0).abs() < 0.5);
        assert_eq!(cue.color, params().deep_color);
    }

    #[test]
    fn deep_window_off_to_the_side_gets_no_cue() {
        let windows = [(Point::from((2500.0, 540.0)), CueKind::Deep)];
        assert!(compute_cues(viewport(), 1.0, &windows, &params()).is_empty());
    }

    #[test]
    fn distance_fades_and_beyond_max_distance_vanishes() {
        let near = [(Point::from((2020.0, 540.0)), CueKind::Urgent)];
        let far = [(Point::from((2820.0, 540.0)), CueKind::Urgent)];
        let gone = [(Point::from((3000.0, 540.0)), CueKind::Urgent)];
        let near_alpha = compute_cues(viewport(), 1.0, &near, &params())[0].alpha;
        let far_alpha = compute_cues(viewport(), 1.0, &far, &params())[0].alpha;
        assert!(near_alpha > far_alpha);
        assert!(compute_cues(viewport(), 1.0, &gone, &params()).is_empty());
    }

    #[test]
    fn urgent_cues_sort_ahead_of_deep_and_cap_holds() {
        let mut windows: Vec<(Point<f64, Logical>, CueKind)> = (0..20)
            .map(|i| {
                (
                    Point::from((960.0, 1200.0 + f64::from(i) * 40.0)),
                    CueKind::Deep,
                )
            })
            .collect();
        windows.push((Point::from((960.0, 1500.0)), CueKind::Urgent));
        let cues = compute_cues(viewport(), 1.0, &windows, &params());
        assert!(cues.len() <= MAX_CUES);
        assert_eq!(cues[0].color, params().urgent_color);
    }

    #[test]
    fn camera_zoom_and_origin_translate_to_screen_edge() {
        // Camera at origin (960, 0), zoom 2: the viewport covers world
        // x 960..1920 at half the world width, and screen coordinates are
        // doubled relative to the world offset.
        let viewport = Rectangle::new(Point::from((960.0, 0.0)), Size::from((960.0, 540.0)));
        let windows = [(Point::from((2400.0, 270.0)), CueKind::Urgent)];
        let cues = compute_cues(viewport, 2.0, &windows, &params());
        assert_eq!(cues.len(), 1);
        assert!((cues[0].center[0] - 1920.0).abs() < 0.5);
        assert!((cues[0].center[1] - 540.0).abs() < 0.5);
    }

    #[test]
    fn shader_is_analytical_and_allocation_free() {
        assert!(!COMPASS_SHADER.contains("sampler2D"));
        assert!(COMPASS_SHADER.contains("u_angle"));
    }
}
