//! Render-only Ocean edge cues for urgent or deep windows outside the camera
//! viewport. Distance controls brightness, and cues vanish beyond the
//! configured range. The analytical shader allocates no texture; an empty cue
//! set creates no element, and the cue count is bounded.

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, Uniform, UniformName, UniformType,
        },
        utils::CommitCounter,
    },
    utils::{
        user_data::UserDataMap, Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform,
    },
};

/// Upper bound for `compass.max_cues`. Urgent cues sort first, then deep,
/// then ordinary, nearest first within a kind, so a cap drops only the least
/// important entries.
pub const MAX_CUES_LIMIT: usize = 64;

/// What text, if any, sits next to a cue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CueLabels {
    #[default]
    Off,
    /// The window's app id.
    App,
    /// The window's title.
    Title,
}

impl CueLabels {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" => Self::Off,
            "app" | "app_id" | "class" => Self::App,
            "title" | "true" => Self::Title,
            _ => return None,
        })
    }
}

const COMPASS_SHADER: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform vec3 u_color;
uniform float u_alpha;
uniform float u_angle;
uniform float u_shape;
uniform float u_glow;

float sdLine(vec2 p, vec2 a, vec2 b) {
    vec2 pa = p - a;
    vec2 ba = b - a;
    float h = clamp(dot(pa, ba) / max(dot(ba, ba), 0.0001), 0.0, 1.0);
    return length(pa - ba * h);
}

void main() {
    vec2 p = (v_coords - 0.5) * 2.0;
    float c = cos(u_angle);
    float s = sin(u_angle);
    vec2 r = vec2(p.x * c + p.y * s, -p.x * s + p.y * c);

    float d;
    if (u_shape < 0.5) {
        // circle: soft elongated blob
        d = length(vec2(r.x * 2.4, r.y * 0.85));
    } else if (u_shape < 1.5) {
        // arrow: filled triangle pointing toward the window (+x)
        // Vertices: tip (0.55, 0), top-base (-0.30, 0.50), bottom-base (-0.30, -0.50)
        float sd1 = dot(r - vec2(0.55, 0.0), vec2(-0.507, -0.862));
        float sd3 = dot(r - vec2(-0.30, -0.50), vec2(-0.507, 0.862));
        float sd2 = r.x + 0.30;
        d = max(max(sd1, sd2), sd3);
    } else if (u_shape < 2.5) {
        // chevron: ">" stroke pointing toward the window
        float upper = sdLine(r, vec2(-0.30, 0.42), vec2(0.32, 0.0)) - 0.10;
        float lower = sdLine(r, vec2(0.32, 0.0), vec2(-0.30, -0.42)) - 0.10;
        d = min(upper, lower);
    } else if (u_shape < 3.5) {
        // ring: hollow circle
        d = abs(length(r) - 0.50) - 0.10;
    } else {
        // diamond
        d = (abs(r.x) + abs(r.y)) * 1.3 - 0.50;
    }

    float glow = 1.0 - smoothstep(0.0, u_glow, max(d, 0.0));
    glow *= glow;
    float core = 1.0 - smoothstep(-0.05, 0.15, d);
    float a = clamp(glow * 0.7 + core * 0.6, 0.0, 1.0) * u_alpha * alpha;
    gl_FragColor = vec4(u_color * a, a);
}
"#;

/// Shape drawn for each compass cue. Resolved to a `u_shape` uniform in
/// the shader. `as_f32` is the shader index; keep the order in sync with
/// the `if/else` chain above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompassShape {
    Circle,
    Arrow,
    Chevron,
    Ring,
    Diamond,
}

impl CompassShape {
    pub fn as_f32(self) -> f32 {
        match self {
            CompassShape::Circle => 0.0,
            CompassShape::Arrow => 1.0,
            CompassShape::Chevron => 2.0,
            CompassShape::Ring => 3.0,
            CompassShape::Diamond => 4.0,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "circle" | "glow" | "blob" => CompassShape::Circle,
            "arrow" | "triangle" => CompassShape::Arrow,
            "chevron" | "wedge" => CompassShape::Chevron,
            "ring" => CompassShape::Ring,
            "diamond" => CompassShape::Diamond,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CueKind {
    Urgent,
    /// At least one screen below the surface line (sunk or a lower reef).
    Deep,
    /// Any other off-screen window.
    Normal,
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
    /// Shader shape index.
    pub shape: f32,
    /// Glow reach uniform.
    pub glow: f32,
    /// Index into the `windows` slice the cue came from (for labels).
    pub source: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct CompassParams {
    pub urgent_color: [f32; 3],
    pub deep_color: [f32; 3],
    pub normal_color: [f32; 3],
    pub show_urgent: bool,
    pub show_deep: bool,
    pub show_normal: bool,
    pub inset: f32,
    pub glow: f32,
    pub far_scale: f32,
    pub max_cues: usize,
    /// Urgent alpha multiplier this frame (pulse), 1 = steady.
    pub urgent_pulse: f32,
    /// World-logical distance beyond the viewport edge at which a cue
    /// fades to nothing.
    pub max_distance: f32,
    pub peak_alpha: f32,
    pub size: f32,
    pub shape: CompassShape,
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
    // Screen-space bounds for clamping the glow fully inside the output.
    let screen_w = (viewport.size.w as f32 * zoom as f32).max(1.0);
    let screen_h = (viewport.size.h as f32 * zoom as f32).max(1.0);
    // Treat configured size as a maximum. A cue larger than a small logical
    // output cannot be clamped fully inside it (`f32::clamp` would receive
    // inverted bounds), so fit it to the live viewport first.
    let max_size = params.size.min(screen_w).min(screen_h).max(1.0);

    let mut cues: Vec<((u8, f32), CompassCue)> = windows
        .iter()
        .enumerate()
        .filter_map(|(source, (world_center, kind))| {
            let shown = match kind {
                CueKind::Urgent => params.show_urgent,
                CueKind::Deep => params.show_deep,
                CueKind::Normal => params.show_normal,
            };
            if !shown {
                return None;
            }
            let (wx, wy) = (world_center.x, world_center.y);
            if wx >= left && wx <= right && wy >= top && wy <= bottom {
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
                CueKind::Normal => params.normal_color,
            };
            // Urgent sorts ahead of deep, deep ahead of ordinary, regardless
            // of distance; within a kind, nearer first.
            let rank = match kind {
                CueKind::Urgent => 0u8,
                CueKind::Deep => 1u8,
                CueKind::Normal => 2u8,
            };
            // Far cues shrink toward `far_scale` of the full size.
            let cue_size = max_size * (params.far_scale + (1.0 - params.far_scale) * factor);
            let half = cue_size / 2.0;
            let lo = |extent: f32| (half + params.inset).min(extent / 2.0);
            // Clamp the glow center fully inside the output (plus inset) so
            // the full rect is visible instead of half-clipped at the edge.
            let sx = (((edge_x - viewport.loc.x) * zoom) as f32)
                .clamp(lo(screen_w), screen_w - lo(screen_w));
            let sy = (((edge_y - viewport.loc.y) * zoom) as f32)
                .clamp(lo(screen_h), screen_h - lo(screen_h));
            let pulse = if *kind == CueKind::Urgent {
                params.urgent_pulse
            } else {
                1.0
            };
            Some((
                (rank, beyond),
                CompassCue {
                    center: [sx, sy],
                    angle: dy.atan2(dx) as f32,
                    color,
                    alpha: params.peak_alpha * factor * pulse,
                    size: cue_size,
                    shape: params.shape.as_f32(),
                    glow: params.glow,
                    source,
                },
            ))
        })
        .collect();
    cues.sort_by(|a, b| match a.0 .0.cmp(&b.0 .0) {
        std::cmp::Ordering::Equal => a.0 .1.total_cmp(&b.0 .1),
        other => other,
    });
    cues.truncate(params.max_cues.clamp(1, MAX_CUES_LIMIT));
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
    /// An urgent cue is pulsing: the output keeps drawing frames.
    pub(crate) pulsing: bool,
    /// Rendered labels by text, kept while their cue is visible.
    labels: Vec<CueLabel>,
}

/// A label buffer and its physical pixel size.
pub struct CueLabel {
    key: (String, u64, u64),
    buffer: smithay::backend::renderer::element::memory::MemoryRenderBuffer,
    physical: (i32, i32),
}

/// Look of cue labels for one frame.
pub struct LabelStyle {
    pub font: std::sync::Arc<fontdue::Font>,
    pub size: f32,
    pub color: [f32; 4],
    /// `color` and `size` change the pixels; part of the cache key.
    pub generation: u64,
}

impl Compass {
    /// Label elements for this frame's `(cue, text)` pairs, placed beside
    /// each cue toward the screen's middle. Buffers are reused by text;
    /// unused ones are dropped. `screen` is the output's logical size.
    pub fn label_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        cues: &[(CompassCue, String)],
        style: &LabelStyle,
        screen: (f32, f32),
        scale: f64,
    ) -> Vec<
        smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement<GlesRenderer>,
    > {
        let mut kept = Vec::with_capacity(cues.len());
        let mut elements = Vec::with_capacity(cues.len());
        for (cue, text) in cues {
            let key = (text.clone(), scale.to_bits(), style.generation);
            let label = match self.labels.iter().position(|label| label.key == key) {
                Some(index) => self.labels.swap_remove(index),
                None => match build_label(text, style, scale) {
                    Some((buffer, physical)) => CueLabel {
                        key,
                        buffer,
                        physical,
                    },
                    None => continue,
                },
            };
            let w = label.physical.0 as f64 / scale;
            let h = label.physical.1 as f64 / scale;
            let (cx, cy) = (f64::from(cue.center[0]), f64::from(cue.center[1]));
            let reach = f64::from(cue.size) / 2.0 + 6.0;
            let x = if cx < f64::from(screen.0) / 2.0 {
                cx + reach
            } else {
                cx - reach - w
            };
            let x = x.clamp(0.0, (f64::from(screen.0) - w).max(0.0));
            let y = (cy - h / 2.0).clamp(0.0, (f64::from(screen.1) - h).max(0.0));
            if let Ok(element) =
                smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    Point::<f64, Logical>::from((x, y)).to_physical(scale),
                    &label.buffer,
                    None,
                    Some(Rectangle::from_size(
                        (f64::from(label.physical.0), f64::from(label.physical.1)).into(),
                    )),
                    Some(Size::from((w.round() as i32, h.round() as i32))),
                    Kind::Unspecified,
                )
            {
                elements.push(element);
            }
            kept.push(label);
        }
        self.labels = kept;
        elements
    }
}

/// A rounded dark pill with `text`, rasterized at physical scale.
fn build_label(
    text: &str,
    style: &LabelStyle,
    scale: f64,
) -> Option<(
    smithay::backend::renderer::element::memory::MemoryRenderBuffer,
    (i32, i32),
)> {
    let font_px = style.size * scale as f32;
    let pad = (6.0 * scale).round() as i32;
    let max_text = (280.0 * scale) as i32;
    let text_w = crate::text::line_width(&style.font, text, font_px, max_text).max(1);
    let width = text_w + pad * 2;
    let height = (font_px * 1.7).round().max(1.0) as i32;
    let len = crate::text::checked_argb_len(width, height)?;
    let mut pixels = vec![0u8; len];
    let radius = height as f32 / 2.0;
    for y in 0..height {
        for x in 0..width {
            let coverage =
                crate::toast::rounded_rect_coverage_local(x, y, 0, 0, width, height, radius);
            if coverage > 0.0 {
                crate::text::blend_premultiplied(
                    &mut pixels,
                    width,
                    x,
                    y,
                    [0.02, 0.05, 0.08, 0.55 * coverage],
                );
            }
        }
    }
    crate::text::draw_line(
        &mut pixels,
        width,
        height,
        &style.font,
        text,
        font_px,
        (0, 0, width, height),
        pad,
        style.color,
    );
    let buffer = smithay::backend::renderer::element::memory::MemoryRenderBuffer::from_slice(
        &pixels,
        smithay::backend::allocator::Fourcc::Argb8888,
        (width, height),
        1,
        Transform::Normal,
        None,
    );
    Some((buffer, (width, height)))
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
            UniformName::new("u_shape", UniformType::_1f),
            UniformName::new("u_glow", UniformType::_1f),
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
                Uniform::new("u_shape", self.cue.shape),
                Uniform::new("u_glow", self.cue.glow),
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
            normal_color: [0.3, 0.5, 0.5],
            show_urgent: true,
            show_deep: true,
            show_normal: true,
            inset: 0.0,
            glow: 1.0,
            far_scale: 1.0,
            max_cues: 16,
            urgent_pulse: 1.0,
            max_distance: 1000.0,
            peak_alpha: 0.85,
            size: 96.0,
            shape: CompassShape::Circle,
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
    fn urgent_right_of_viewport_lands_near_right_edge() {
        let windows = [(Point::from((2500.0, 540.0)), CueKind::Urgent)];
        let cues = compute_cues(viewport(), 1.0, &windows, &params());
        assert_eq!(cues.len(), 1);
        let cue = cues[0];
        let half = params().size / 2.0;
        assert!((cue.center[0] - (1920.0 - half)).abs() < 0.5);
        assert!((cue.center[1] - 540.0).abs() < 0.5);
        assert!(cue.angle.abs() < 0.01);
        assert_eq!(cue.color, params().urgent_color);
    }

    #[test]
    fn deep_below_viewport_lands_near_bottom_edge() {
        let windows = [(Point::from((960.0, 1600.0)), CueKind::Deep)];
        let cues = compute_cues(viewport(), 1.0, &windows, &params());
        assert_eq!(cues.len(), 1);
        let cue = cues[0];
        let half = params().size / 2.0;
        assert!((cue.center[0] - 960.0).abs() < 0.5);
        assert!((cue.center[1] - (1080.0 - half)).abs() < 0.5);
        assert_eq!(cue.color, params().deep_color);
    }

    #[test]
    fn deep_window_off_to_the_side_still_glows() {
        let windows = [(Point::from((2500.0, 540.0)), CueKind::Deep)];
        let cues = compute_cues(viewport(), 1.0, &windows, &params());
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].color, params().deep_color);
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
        assert!(cues.len() <= params().max_cues);
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
        let half = params().size / 2.0;
        assert!((cues[0].center[0] - (1920.0 - half)).abs() < 0.5);
        assert!((cues[0].center[1] - 540.0).abs() < 0.5);
    }

    #[test]
    fn oversized_cue_fits_small_live_viewport() {
        let viewport = Rectangle::new(Point::from((0.0, 0.0)), Size::from((31.0, 17.0)));
        let windows = [(Point::from((100.0, 8.0)), CueKind::Urgent)];

        let cues = compute_cues(viewport, 1.0, &windows, &params());
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].size, 17.0);
        assert!(cues[0].center[0] >= cues[0].size / 2.0);
        assert!(cues[0].center[1] >= cues[0].size / 2.0);
    }

    #[test]
    fn kinds_have_their_own_color_toggle_and_order() {
        let windows = [
            (Point::from((960.0, 1600.0)), CueKind::Normal),
            (Point::from((2500.0, 540.0)), CueKind::Deep),
            (Point::from((-600.0, 540.0)), CueKind::Urgent),
        ];
        let cues = compute_cues(viewport(), 1.0, &windows, &params());
        let colors: Vec<_> = cues.iter().map(|cue| cue.color).collect();
        assert_eq!(
            colors,
            [
                params().urgent_color,
                params().deep_color,
                params().normal_color
            ]
        );
        assert_eq!(cues[0].source, 2, "a cue remembers which window it is for");
        let hidden = CompassParams {
            show_normal: false,
            show_deep: false,
            ..params()
        };
        let cues = compute_cues(viewport(), 1.0, &windows, &hidden);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].color, params().urgent_color);
    }

    #[test]
    fn inset_far_scale_cap_and_pulse_apply() {
        let near = [(Point::from((1930.0, 540.0)), CueKind::Urgent)];
        let far = [(Point::from((2820.0, 540.0)), CueKind::Urgent)];
        let tuned = CompassParams {
            inset: 20.0,
            far_scale: 0.5,
            urgent_pulse: 0.5,
            ..params()
        };
        let near_cue = compute_cues(viewport(), 1.0, &near, &tuned)[0];
        let far_cue = compute_cues(viewport(), 1.0, &far, &tuned)[0];
        assert!(far_cue.size < near_cue.size, "far cues shrink");
        assert!(far_cue.size >= params().size * 0.5 - 0.01);
        let edge = 1920.0 - near_cue.size / 2.0 - 20.0;
        assert!(
            (near_cue.center[0] - edge).abs() < 0.5,
            "inset moves the cue in"
        );
        let steady = compute_cues(viewport(), 1.0, &near, &params())[0];
        assert!((near_cue.alpha - steady.alpha * 0.5).abs() < 0.001);
        let many: Vec<_> = (0..10)
            .map(|i| {
                (
                    Point::from((2000.0 + f64::from(i) * 10.0, 540.0)),
                    CueKind::Normal,
                )
            })
            .collect();
        let capped = CompassParams {
            max_cues: 3,
            ..params()
        };
        assert_eq!(compute_cues(viewport(), 1.0, &many, &capped).len(), 3);
    }

    #[test]
    fn labels_parse() {
        assert_eq!(CueLabels::parse("title"), Some(CueLabels::Title));
        assert_eq!(CueLabels::parse("app"), Some(CueLabels::App));
        assert_eq!(CueLabels::parse("off"), Some(CueLabels::Off));
        assert_eq!(CueLabels::parse("loud"), None);
    }

    #[test]
    fn shader_is_analytical_and_allocation_free() {
        assert!(!COMPASS_SHADER.contains("sampler2D"));
        assert!(COMPASS_SHADER.contains("u_angle"));
        assert!(COMPASS_SHADER.contains("u_shape"));
    }

    #[test]
    fn shape_parses_known_names_and_rejects_unknown() {
        assert_eq!(CompassShape::parse("circle"), Some(CompassShape::Circle));
        assert_eq!(CompassShape::parse("arrow"), Some(CompassShape::Arrow));
        assert_eq!(CompassShape::parse("triangle"), Some(CompassShape::Arrow));
        assert_eq!(CompassShape::parse("chevron"), Some(CompassShape::Chevron));
        assert_eq!(CompassShape::parse("ring"), Some(CompassShape::Ring));
        assert_eq!(CompassShape::parse("diamond"), Some(CompassShape::Diamond));
        assert_eq!(CompassShape::parse("blob"), Some(CompassShape::Circle));
        assert_eq!(CompassShape::parse("wedge"), Some(CompassShape::Chevron));
        assert_eq!(CompassShape::parse("hexagon"), None);
    }
}
