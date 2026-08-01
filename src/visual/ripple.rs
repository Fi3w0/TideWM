//! Impulse ripple, the second piece of Phase R1's identity slice (see
//! AGENT.md's "Render and visual identity roadmap"). One shared primitive
//! for a radial disturbance from a point, decaying over time -- intended
//! to be triggered by different window-manager events (a window mapping
//! is a droplet impact, a focus change ripples from the old focus to the
//! new one, an urgent hint pulses until acknowledged) rather than three
//! separate effects. The wave/aqua workspace transition (Phase R1, later
//! sub-slice) will be this same primitive's directional variant, not a
//! separate effect.
//!
//! Fully configurable through the global `ripple { }` block and per-app
//! `rule { ripple { } }` overrides -- see `config::RippleConfig` for the
//! knob surface (shape, color, radius, thickness, duration, peak alpha,
//! ease, anchor, offset, layer, triggers). Defaults come from
//! `RippleConfig::system_default` and match the original hardcoded
//! behavior, so an existing config that never touches the `ripple` block
//! gets the same visuals it had before this module was made tunable.
//!
//! Gated on the `water_effects` config toggle the same way `water_glass`
//! is, since neither is meaningful with the identity off.

use std::time::{Duration, Instant};

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

use crate::config::{
    RippleAnchor, RippleConfig, RippleEase, RippleLayer, RipplePreset, RippleShape,
};

/// Procedural fragment shader for polished expanding impulse presets and the
/// original geometric-shape compatibility mode.
/// `v_coords` is normalized UV in `[0, 1]` from Smithay's vertex shader
/// (see `build_texture_mat` in `backend/renderer/gles/mod.rs`, which
/// divides by texture size at the end of building the tex matrix). The
/// element's bounding square is sized to `(2r, 2r)` where `r` is the
/// current ring radius, so a "ring" shape sits at UV radius `0.5` --
/// the element grows over time, the ring follows its edge. Other shapes
/// (square, droplet, cross) reuse the same bounding-square convention,
/// drawn as different distance-field falloffs within it.
///
/// The Smithay pixel-shader framework supplies `size` (the element's
/// buffer size) and `alpha` (the element's own alpha, see
/// `RippleElement::alpha`) automatically. Additional uniforms:
/// Presets remain one analytical element each. Their inner rings, glow,
/// highlights, lobes, and wobble are math inside this shader, not extra
/// textures or render elements. `Legacy` alone may emit multiple elements
/// for the old `shapes` list.
///
/// RGB is pre-multiplied by alpha before writing `gl_FragColor`, matching
/// `water_glass::WATER_GLASS_FRAGMENT_SHADER`'s own alpha handling --
/// Smithay's GL blend setup expects pre-multiplied on this path.
///
/// The shader must not contain a `#version` directive; Smithay prepends
/// `#version 100` itself (see `GlesRenderer::compile_custom_pixel_shader`'s
/// own contract). Mirrors `water_glass::WATER_GLASS_FRAGMENT_SHADER`'s
/// approach to that contract.
const RIPPLE_FRAGMENT_SHADER: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform vec3 u_tint;
uniform vec3 u_secondary_tint;
uniform float u_thickness_uv;
uniform float u_shape;
uniform float u_preset;
uniform float u_progress;
uniform float u_glow;
uniform float u_wobble;
uniform float u_detail;

float band(float value, float center, float width) {
    return 1.0 - smoothstep(0.0, max(width, 0.001), abs(value - center));
}

void main() {
    vec2 p = v_coords - vec2(0.5);
    float r = length(p);
    float angle = atan(p.y, p.x);
    float progress = clamp(u_progress, 0.0, 1.0);
    float width = max(u_thickness_uv, 0.006);
    float a = 0.0;
    float color_mix = smoothstep(0.08, 0.52, r);

    if (u_preset < 0.5) {
        // Original independently-stackable geometry.
        if (u_shape < 0.5) {
            a = band(r, 0.5, width);
        } else if (u_shape < 1.5) {
            a = band(max(abs(p.x), abs(p.y)), 0.5, width);
        } else if (u_shape < 2.5) {
            float ring = band(r, 0.5, width);
            float crater = 1.0 - smoothstep(0.0, 0.18, r);
            a = max(ring, crater * 0.55);
        } else {
            float arm = 0.07;
            float bars = max(
                1.0 - smoothstep(0.0, arm, abs(p.x)),
                1.0 - smoothstep(0.0, arm, abs(p.y))
            );
            a = bars * band(max(abs(p.x), abs(p.y)), 0.5, width);
        }
    } else if (u_preset < 1.5) {
        // Water drop: three concentric wavefronts and a healing impact
        // crater. Inner rings soften as the impulse travels outward.
        float outer = band(r, 0.47, width * 0.85);
        float middle = band(r, 0.33 - progress * 0.025, width * 0.58)
            * (0.56 + 0.24 * u_detail);
        float inner = band(r, 0.19 - progress * 0.018, width * 0.45)
            * 0.42 * u_detail;
        float crater = (1.0 - smoothstep(0.0, 0.105, r))
            * (1.0 - progress) * 0.9;
        float halo = (1.0 - smoothstep(0.18, 0.52, r))
            * 0.18 * u_glow;
        a = max(max(outer, middle), max(inner, crater)) + halo;
        color_mix = smoothstep(0.12, 0.48, r);
    } else if (u_preset < 2.5) {
        // Jelly: an organic membrane that oscillates several times before
        // settling. This is the lightweight "jiggle/giggle" appearance.
        float settle = 1.0 - progress * 0.68;
        float wobble =
            sin(angle * 5.0 + progress * 20.0) * 0.030 +
            sin(angle * 3.0 - progress * 14.0) * 0.019 +
            sin(angle * 8.0 + progress * 9.0) * 0.010 * u_detail;
        float membrane = 0.43 + wobble * u_wobble * settle;
        float rim = band(r, membrane, width * 0.95);
        float gel = (1.0 - smoothstep(membrane - 0.16, membrane, r))
            * 0.13 * u_glow;
        float echo = band(r, membrane * 0.70, width * 0.42)
            * 0.26 * u_detail;
        a = max(rim, echo) + gel;
        color_mix = 0.5 + 0.5 * sin(angle * 2.0 + progress * 8.0);
    } else if (u_preset < 3.5) {
        // Bubble: two thin membranes with an upper-left moving specular
        // glint. The body remains mostly transparent.
        float outer = band(r, 0.455, width * 0.62);
        float inner = band(r, 0.405, width * 0.34) * (0.42 + 0.2 * u_detail);
        vec2 glint_center = vec2(-0.17, -0.18)
            + vec2(progress * 0.055, progress * 0.025);
        float glint = (1.0 - smoothstep(0.025, 0.085, length(p - glint_center)))
            * (0.52 + 0.32 * u_detail);
        float sheen = (1.0 - smoothstep(0.28, 0.46, r))
            * max(0.0, dot(normalize(p + vec2(0.001)), normalize(vec2(-1.0, -1.0))))
            * 0.18 * u_glow;
        a = max(max(outer, inner), glint) + sheen;
        color_mix = max(glint, smoothstep(0.20, 0.47, r));
    } else if (u_preset < 4.5) {
        // Splash: a seven-lobed crown and sharper outer spray peaks.
        float lobe = pow(0.5 + 0.5 * sin(angle * 7.0 - progress * 8.0), 3.0);
        float crown_radius = 0.34 + lobe * 0.085 * u_detail;
        float crown = band(r, crown_radius, width * 0.72);
        float spray_peaks = pow(
            max(0.0, sin(angle * 11.0 + progress * 13.0)),
            10.0
        );
        float spray = band(r, 0.475, width * 0.48)
            * spray_peaks * u_detail;
        float core = (1.0 - smoothstep(0.04, 0.22, r))
            * (1.0 - progress) * 0.45;
        float halo = (1.0 - smoothstep(0.20, 0.48, r))
            * 0.11 * u_glow;
        a = max(max(crown, spray), core) + halo;
        color_mix = max(lobe, smoothstep(0.22, 0.48, r));
    } else {
        // Tide: several sinusoidal bands travelling in opposite directions,
        // softly clipped to a circular impulse envelope.
        float phase = progress * 10.0;
        float wave1 = 0.12 * sin(p.x * 10.0 + phase);
        float wave2 = 0.10 * sin(p.x * 8.0 - phase * 0.72);
        float wave3 = 0.07 * sin(p.x * 13.0 + phase * 0.46);
        float bands = max(
            band(p.y, wave1, width * 0.72),
            max(
                band(p.y, wave2 + 0.13, width * 0.50),
                band(p.y, wave3 - 0.14, width * 0.42) * u_detail
            )
        );
        float envelope = 1.0 - smoothstep(0.34, 0.50, r);
        float rim = band(r, 0.47, width * 0.50) * 0.45;
        a = max(bands * envelope, rim) + envelope * 0.08 * u_glow;
        color_mix = 0.5 + 0.5 * sin(p.x * 9.0 + phase);
    }

    a = clamp(a, 0.0, 1.0) * alpha;
    vec3 color = mix(u_tint, u_secondary_tint, clamp(color_mix, 0.0, 1.0));
    gl_FragColor = vec4(color * a, a);
}
"#;

/// Compiles the ripple shader against `renderer` the first time it's
/// needed and caches it -- a `GlesPixelProgram` is cheap to clone
/// (`Arc`-backed) once compiled, so every frame just clones the cached
/// handle rather than recompiling. Per-renderer, not a process-global
/// `OnceLock`, since compiling needs a live `&mut GlesRenderer`. Same
/// pattern as `water_glass::water_glass_program`.
pub fn ripple_program(
    cache: &mut Option<GlesPixelProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesPixelProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    match renderer.compile_custom_pixel_shader(
        RIPPLE_FRAGMENT_SHADER,
        &[
            UniformName::new("u_tint", UniformType::_3f),
            UniformName::new("u_secondary_tint", UniformType::_3f),
            UniformName::new("u_thickness_uv", UniformType::_1f),
            UniformName::new("u_shape", UniformType::_1f),
            UniformName::new("u_preset", UniformType::_1f),
            UniformName::new("u_progress", UniformType::_1f),
            UniformName::new("u_glow", UniformType::_1f),
            UniformName::new("u_wobble", UniformType::_1f),
            UniformName::new("u_detail", UniformType::_1f),
        ],
    ) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile ripple shader");
            None
        }
    }
}

/// Applies the configured easing to a progress value in `[0.0, 1.0]`.
/// Kept as a free function (not a method on `RippleEase`) so unit tests
/// can exercise each shape directly without going through the whole
/// `Ripple` lifetime.
pub fn ease_value(ease: RippleEase, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    match ease {
        RippleEase::Linear => t,
        RippleEase::CubicOut => 1.0 - (1.0 - t).powi(3),
        RippleEase::CubicInOut => {
            if t < 0.5 {
                4.0 * t * t * t
            } else {
                1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
            }
        }
        RippleEase::QuadOut => 1.0 - (1.0 - t).powi(2),
        // A simple exp-out: fast initial jump, smooth asymptotic approach
        // to 1. The `1 - exp(-5t)` shape reaches ~0.99 by t = 1.
        RippleEase::ExpOut => 1.0 - (-5.0 * t).exp(),
    }
}

/// Resolves an anchor against a window rect. Side anchors use
/// `edge_position`; `NearestEdge` projects the current pointer onto the
/// closest side. Positive `edge_offset` moves outward from the window.
pub fn anchor_point(
    window: Rectangle<i32, Logical>,
    anchor: RippleAnchor,
    pointer: Option<Point<f64, Logical>>,
    edge_position: f32,
    edge_offset: f32,
) -> Point<f64, Logical> {
    let edge_position = edge_position.clamp(0.0, 1.0) as f64;
    let edge_offset = edge_offset as f64;
    let left = window.loc.x as f64;
    let top = window.loc.y as f64;
    let right = left + window.size.w as f64;
    let bottom = top + window.size.h as f64;
    let center = Point::from(((left + right) / 2.0, (top + bottom) / 2.0));

    match anchor {
        RippleAnchor::Center => center,
        RippleAnchor::Cursor => pointer.unwrap_or(center),
        RippleAnchor::Top => Point::from((
            left + window.size.w as f64 * edge_position,
            top - edge_offset,
        )),
        RippleAnchor::Bottom => Point::from((
            left + window.size.w as f64 * edge_position,
            bottom + edge_offset,
        )),
        RippleAnchor::Left => Point::from((
            left - edge_offset,
            top + window.size.h as f64 * edge_position,
        )),
        RippleAnchor::Right => Point::from((
            right + edge_offset,
            top + window.size.h as f64 * edge_position,
        )),
        RippleAnchor::NearestEdge => {
            let pointer = pointer.unwrap_or(center);
            let distances = [
                ((pointer.y - top).abs(), RippleAnchor::Top),
                ((pointer.y - bottom).abs(), RippleAnchor::Bottom),
                ((pointer.x - left).abs(), RippleAnchor::Left),
                ((pointer.x - right).abs(), RippleAnchor::Right),
            ];
            match distances
                .into_iter()
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, edge)| edge)
                .unwrap_or(RippleAnchor::Top)
            {
                RippleAnchor::Top => Point::from((pointer.x.clamp(left, right), top - edge_offset)),
                RippleAnchor::Bottom => {
                    Point::from((pointer.x.clamp(left, right), bottom + edge_offset))
                }
                RippleAnchor::Left => {
                    Point::from((left - edge_offset, pointer.y.clamp(top, bottom)))
                }
                RippleAnchor::Right => {
                    Point::from((right + edge_offset, pointer.y.clamp(top, bottom)))
                }
                _ => unreachable!(),
            }
        }
        RippleAnchor::TopLeft => Point::from((left, top)),
        RippleAnchor::TopRight => Point::from((right, top)),
        RippleAnchor::BottomLeft => Point::from((left, bottom)),
        RippleAnchor::BottomRight => Point::from((right, bottom)),
    }
}

/// One impulse: a center point on a specific output, a start time, and a
/// resolved `RippleConfig` carrying every visual knob the user set (or
/// didn't -- unset fields come from `system_default`). Purely analytical
/// -- no per-frame simulation, just closed-form radius/alpha given
/// elapsed time, sampled by `RippleElement::from_ripple` building each
/// frame's render element.
pub struct Ripple {
    /// Which output this ripple lives on. Ripples are output-local (the
    /// center is in `output`'s own logical space), and each output's
    /// render loop only draws the ripples it owns; a ripple spawned on
    /// output A never appears on output B even if its radius reaches
    /// into B's pixels. A multi-output ripple that visibly crosses the
    /// boundary in both outputs is a later scope item.
    pub output: String,
    /// Center in `output`-local logical space.
    pub center: Point<f64, Logical>,
    pub start: Instant,
    pub cfg: RippleConfig,
    /// Stable identity carried into each frame's `RippleElement` so the
    /// damage tracker keys its bookkeeping off one entry for the lifetime
    /// of this impulse, not a fresh entry every frame -- see
    /// `water_glass::WaterGlassElement`'s own `id` field doc for the
    /// shape of the leak a fresh `Id` every frame would create in the
    /// tracker's per-element map.
    pub id: Id,
    /// Incremented every frame this ripple is rendered, so the damage
    /// tracker sees a fresh commit and redraws rather than assuming
    /// unchanged content and skipping the draw.
    pub commit: CommitCounter,
}

impl Ripple {
    /// Spawns a ripple with `cfg` as its resolved visual config. Fields
    /// of `cfg` the user left unset were already filled in by the caller
    /// merging `RippleConfig::system_default` underneath, so this
    /// constructor never needs to fall back to hardcoded constants.
    pub fn new(output: String, center: Point<f64, Logical>, cfg: RippleConfig) -> Self {
        Self {
            output,
            center,
            start: Instant::now(),
            cfg,
            id: Id::new(),
            commit: CommitCounter::default(),
        }
    }

    fn progress(&self) -> f32 {
        let duration_ms = self.cfg.duration_ms.unwrap_or(650) as f32;
        (self.start.elapsed().as_secs_f32() * 1000.0 / duration_ms).clamp(0.0, 1.0)
    }

    /// Current ring radius in logical pixels. The configured easing is
    /// applied to progress before scaling by `peak_radius`, so a linear
    /// ease produces constant-velocity growth and a cubic-out produces
    /// the water-impact deceleration that's the default look.
    pub fn radius(&self) -> f32 {
        let peak = self.cfg.peak_radius.unwrap_or(220.0);
        let ease = self.cfg.ease.unwrap_or(RippleEase::CubicOut);
        ease_value(ease, self.progress()) * peak
    }

    /// Current alpha. Quadratic fade from `peak_alpha` to 0 over the
    /// lifetime -- deliberately not eased by `cfg.ease` (the radius
    /// progression is), since a fading look on top of a nonlinear radius
    /// curve tends to read as an "impact + bounce" rather than a clean
    /// water ripple.
    pub fn alpha(&self) -> f32 {
        let peak_alpha = self.cfg.peak_alpha.unwrap_or(1.0);
        let p = self.progress();
        (1.0 - p).powi(2) * peak_alpha
    }

    pub fn finished(&self) -> bool {
        let duration_ms = self.cfg.duration_ms.unwrap_or(650) as u64;
        self.start.elapsed() >= Duration::from_millis(duration_ms)
    }

    /// Where in the render-element z-order this ripple's elements go.
    /// Used by the backend element-chain builders to decide which
    /// position in the front-to-back list to splice the ripples into.
    pub fn layer(&self) -> RippleLayer {
        self.cfg.layer.unwrap_or(RippleLayer::AboveWindows)
    }

    fn preset(&self) -> RipplePreset {
        self.cfg.built_in_preset()
    }

    /// Polished presets render as one fixed-cost element. `Legacy` preserves
    /// the old one-element-per-shape behavior.
    fn render_shapes(&self) -> Vec<RippleShape> {
        if self.preset() != RipplePreset::Legacy || self.cfg.shapes.is_empty() {
            vec![RippleShape::Ring]
        } else {
            self.cfg.shapes.clone()
        }
    }

    /// Bounding square for the per-frame render element: side `2*radius`,
    /// centered on the impulse. Returned in the same output-local
    /// logical space `center` itself is given in.
    fn element_rect(&self) -> Rectangle<i32, Logical> {
        let r = self.radius() as i32;
        Rectangle::new(
            Point::from(((self.center.x as i32) - r, (self.center.y as i32) - r)),
            Size::from((2 * r, 2 * r)),
        )
    }
}

/// One shape of one ripple's render element for one frame: a procedural
/// pixel-shader quad sized to the ripple's current bounding square,
/// drawing the configured shape with the configured tint/alpha. A
/// ripple with multiple shapes (`shapes = ring square` in config)
/// produces one `RippleElement` per shape per frame, all sharing the
/// same bounding square but each asking the shader to draw a different
/// `u_shape`.
pub struct RippleElement {
    id: Id,
    commit: CommitCounter,
    area: Rectangle<i32, Logical>,
    program: GlesPixelProgram,
    alpha: f32,
    tint: [f32; 3],
    secondary_tint: [f32; 3],
    thickness_uv: f32,
    shape: RippleShape,
    preset: RipplePreset,
    progress: f32,
    glow: f32,
    wobble: f32,
    detail: f32,
}

impl RippleElement {
    /// Builds the element(s) `ripple` produces for this frame,
    /// incrementing `ripple`'s commit counter so the next frame is
    /// visibly distinct to the damage tracker. Returns an empty Vec if
    /// `ripple` is already `finished()` -- callers should `retain`
    /// finished ripples out of their list rather than relying on this,
    /// but the guard is here as a belt-and-braces check.
    ///
    /// One element per configured shape: a ripple with `shapes = ring
    /// square` returns two elements that stack on top of each other at
    /// the same bounding square. The shader's `u_shape` uniform tells it
    /// which shape to draw; the order within the returned Vec is the
    /// order shapes are listed in config (front-to-back).
    pub fn from_ripple(ripple: &mut Ripple, program: GlesPixelProgram) -> Vec<Self> {
        if ripple.finished() {
            return Vec::new();
        }
        ripple.commit.increment();
        let commit = ripple.commit;
        let id = ripple.id.clone();
        let area = ripple.element_rect();
        let alpha = ripple.alpha();
        let tint = ripple.cfg.color.unwrap_or([0.55, 0.85, 1.0]);
        let secondary_tint = ripple.cfg.secondary_color.unwrap_or([0.91, 0.98, 1.0]);
        let thickness = ripple.cfg.thickness.unwrap_or(8.0);
        let max_side = area.size.w.max(area.size.h).max(1) as f32;
        let thickness_uv = thickness / max_side;
        let preset = ripple.preset();
        let progress = ripple.progress();
        let glow = ripple.cfg.glow.unwrap_or(0.55);
        let wobble = ripple.cfg.wobble.unwrap_or(0.7);
        let detail = ripple.cfg.detail.unwrap_or(0.8);
        ripple
            .render_shapes()
            .into_iter()
            .map(|shape| RippleElement {
                id: id.clone(),
                commit,
                area,
                program: program.clone(),
                alpha,
                tint,
                secondary_tint,
                thickness_uv,
                shape,
                preset,
                progress,
                glow,
                wobble,
                detail,
            })
            .collect()
    }
}

impl Element for RippleElement {
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
        self.alpha
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }

    // `opaque_regions` deliberately left at its default (empty): the
    // shapes are fully translucent outside their thin outlines, and
    // claiming any opaque region would let the damage tracker skip
    // drawing whatever's behind them -- the same load-bearing reasoning
    // as `water_glass::WaterGlassElement`'s own omitted override.
}

impl RenderElement<GlesRenderer> for RippleElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let u_shape = match self.shape {
            RippleShape::Ring => 0.0,
            RippleShape::Square => 1.0,
            RippleShape::Droplet => 2.0,
            RippleShape::Cross => 3.0,
        };
        let u_preset = match self.preset {
            RipplePreset::Legacy => 0.0,
            RipplePreset::WaterDrop => 1.0,
            RipplePreset::Jelly => 2.0,
            RipplePreset::Bubble => 3.0,
            RipplePreset::Splash => 4.0,
            RipplePreset::Tide => 5.0,
        };
        frame.render_pixel_shader_to(
            &self.program,
            src,
            dst,
            self.area.size.to_buffer(1, Transform::Normal),
            Some(damage),
            self.alpha,
            &[
                Uniform::new("u_tint", self.tint),
                Uniform::new("u_secondary_tint", self.secondary_tint),
                Uniform::new("u_thickness_uv", self.thickness_uv),
                Uniform::new("u_shape", u_shape),
                Uniform::new("u_preset", u_preset),
                Uniform::new("u_progress", self.progress),
                Uniform::new("u_glow", self.glow),
                Uniform::new("u_wobble", self.wobble),
                Uniform::new("u_detail", self.detail),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_source_keeps_required_uniforms_and_omits_version_directive() {
        // Not a GL compile check (no EGL context in unit tests, see every
        // other backend/gles-adjacent module in this project) -- a guard
        // against accidentally dropping a uniform `compile_custom_pixel_shader`'s
        // contract requires, or accidentally adding a `#version` directive
        // the same function rejects (it prepends its own).
        assert!(!RIPPLE_FRAGMENT_SHADER.contains("#version"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform float alpha"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform vec2 size"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("varying vec2 v_coords"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform vec3 u_tint"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform vec3 u_secondary_tint"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform float u_thickness_uv"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform float u_shape"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform float u_preset"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform float u_progress"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform float u_glow"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform float u_wobble"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform float u_detail"));
    }

    #[test]
    fn ripple_starts_at_configured_alpha_radius_zero_and_is_not_finished() {
        let ripple = Ripple::new(
            "eDP-1".to_string(),
            Point::from((100.0, 100.0)),
            RippleConfig::system_default(),
        );
        assert!(ripple.radius() < 1.0);
        assert!((ripple.alpha() - 0.88).abs() < 0.05);
        assert!(!ripple.finished());
    }

    #[test]
    fn ripple_reaches_peak_radius_and_finishes_after_duration() {
        let mut cfg = RippleConfig::system_default();
        cfg.duration_ms = Some(10);
        let ripple = Ripple::new("eDP-1".to_string(), Point::from((0.0, 0.0)), cfg);
        std::thread::sleep(Duration::from_millis(25));
        assert!((ripple.radius() - 220.0).abs() < 0.5);
        assert!(ripple.alpha() < 0.01);
        assert!(ripple.finished());
    }

    #[test]
    fn element_rect_is_a_centered_square_of_side_two_radius() {
        let ripple = Ripple::new(
            "eDP-1".to_string(),
            Point::from((300.0, 200.0)),
            RippleConfig::system_default(),
        );
        let rect = ripple.element_rect();
        assert_eq!(rect.size.w, rect.size.h);
        let cx = rect.loc.x + rect.size.w / 2;
        let cy = rect.loc.y + rect.size.h / 2;
        assert!((cx - 300).abs() <= 1);
        assert!((cy - 200).abs() <= 1);
    }

    #[test]
    fn side_and_nearest_edge_anchors_resolve_with_offsets() {
        let window = Rectangle::new(Point::from((100, 100)), Size::from((200, 100)));
        assert_eq!(
            anchor_point(window, RippleAnchor::Top, None, 0.25, 10.0),
            Point::from((150.0, 90.0))
        );
        assert_eq!(
            anchor_point(window, RippleAnchor::Bottom, None, 0.75, -5.0),
            Point::from((250.0, 195.0))
        );
        assert_eq!(
            anchor_point(
                window,
                RippleAnchor::NearestEdge,
                Some(Point::from((292.0, 130.0))),
                0.5,
                6.0,
            ),
            Point::from((306.0, 130.0))
        );
    }

    #[test]
    fn ease_value_shape_is_monotonic_and_bounded() {
        for ease in [
            RippleEase::Linear,
            RippleEase::CubicOut,
            RippleEase::CubicInOut,
            RippleEase::QuadOut,
            RippleEase::ExpOut,
        ] {
            let mut prev = -1.0_f32;
            for i in 0..=10 {
                let t = i as f32 / 10.0;
                let v = ease_value(ease, t);
                assert!(
                    (0.0..=1.01).contains(&v),
                    "ease {:?} at t={} gave {}",
                    ease,
                    t,
                    v
                );
                assert!(v >= prev - 1e-6, "ease {:?} not monotonic at t={}", ease, t);
                prev = v;
            }
            assert_eq!(ease_value(ease, 0.0), 0.0);
            assert!((ease_value(ease, 1.0) - 1.0).abs() < 0.05);
        }
    }

    #[test]
    fn polished_presets_are_one_element_and_legacy_keeps_shape_multiplicity() {
        let mut cfg = RippleConfig::system_default();
        cfg.shapes = vec![RippleShape::Ring, RippleShape::Square];
        let mut ripple = Ripple::new("eDP-1".to_string(), Point::from((100.0, 100.0)), cfg);
        assert_eq!(ripple.render_shapes(), vec![RippleShape::Ring]);

        ripple.cfg.preset = Some(crate::config::RipplePresetSelection::BuiltIn(
            RipplePreset::Legacy,
        ));
        assert_eq!(
            ripple.render_shapes(),
            vec![RippleShape::Ring, RippleShape::Square]
        );
        assert!(!ripple.finished());
    }
}
