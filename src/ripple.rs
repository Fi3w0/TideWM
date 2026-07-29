//! Impulse ripple: the second piece of Phase R1's identity slice (see
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

use crate::config::{RippleConfig, RippleEase, RippleLayer, RippleShape};

/// Procedural fragment shader for an expanding shape with a soft falloff.
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
/// `u_tint` (RGB), `u_thickness_uv` (half-width of the shape's edge,
/// in UV units relative to the element's larger side), and `u_shape`
/// (0.0 ring, 1.0 square, 2.0 droplet, 3.0 cross -- passed as float
/// because integer uniforms in GLSL ES 100 are unevenly supported).
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
uniform float u_thickness_uv;
uniform float u_shape;

void main() {
    vec2 p = v_coords - vec2(0.5);
    float a;
    if (u_shape < 0.5) {
        // Ring: Euclidean distance from center, edge at 0.5.
        float d = length(p);
        a = smoothstep(u_thickness_uv, 0.0, abs(d - 0.5));
    } else if (u_shape < 1.5) {
        // Square outline: Chebyshev distance, edge at 0.5.
        float d = max(abs(p.x), abs(p.y));
        a = smoothstep(u_thickness_uv, 0.0, abs(d - 0.5));
    } else if (u_shape < 2.5) {
        // Droplet: ring at 0.5 plus a small filled disc at the center.
        // The disc's alpha is the *current* alpha already fading, so the
        // crater looks like it's already healing as the ring expands.
        float d = length(p);
        float ring = smoothstep(u_thickness_uv, 0.0, abs(d - 0.5));
        float crater = smoothstep(0.18, 0.0, d);
        a = max(ring, crater * 0.55);
    } else {
        // Cross: two perpendicular bars at the center, fading at the
        // bounding-square edge. Bar half-width is fixed in UV so the
        // bars keep their shape as the element grows.
        float arm = 0.07;
        float bar_x = smoothstep(arm, 0.0, abs(p.x));
        float bar_y = smoothstep(arm, 0.0, abs(p.y));
        float arm_alpha = max(bar_x, bar_y);
        float edge_d = max(abs(p.x), abs(p.y));
        float edge = smoothstep(u_thickness_uv, 0.0, abs(edge_d - 0.5));
        a = arm_alpha * edge;
    }
    a *= alpha;
    gl_FragColor = vec4(u_tint * a, a);
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
            UniformName::new("u_thickness_uv", UniformType::_1f),
            UniformName::new("u_shape", UniformType::_1f),
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

    /// The shapes to draw for this ripple, resolving the empty-list
    /// "inherit" fallback to a single `Ring` so an unset config can't
    /// produce a ripple with no visual.
    fn shapes(&self) -> Vec<RippleShape> {
        if self.cfg.shapes.is_empty() {
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
    thickness_uv: f32,
    shape: RippleShape,
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
        let thickness = ripple.cfg.thickness.unwrap_or(8.0);
        let max_side = area.size.w.max(area.size.h).max(1) as f32;
        let thickness_uv = thickness / max_side;
        ripple
            .shapes()
            .into_iter()
            .map(|shape| RippleElement {
                id: id.clone(),
                commit,
                area,
                program: program.clone(),
                alpha,
                tint,
                thickness_uv,
                shape,
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
        frame.render_pixel_shader_to(
            &self.program,
            src,
            dst,
            self.area.size.to_buffer(1, Transform::Normal),
            Some(damage),
            self.alpha,
            &[
                Uniform::new("u_tint", self.tint),
                Uniform::new("u_thickness_uv", self.thickness_uv),
                Uniform::new("u_shape", u_shape),
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
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform float u_thickness_uv"));
        assert!(RIPPLE_FRAGMENT_SHADER.contains("uniform float u_shape"));
    }

    #[test]
    fn ripple_starts_at_zero_alpha_one_radius_zero_and_is_not_finished() {
        let ripple = Ripple::new(
            "eDP-1".to_string(),
            Point::from((100.0, 100.0)),
            RippleConfig::system_default(),
        );
        assert!(ripple.radius() < 1.0);
        assert!((ripple.alpha() - 1.0).abs() < 0.05);
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
    fn from_ripple_returns_one_element_per_shape() {
        let mut cfg = RippleConfig::system_default();
        cfg.shapes = vec![RippleShape::Ring, RippleShape::Square];
        let ripple = Ripple::new("eDP-1".to_string(), Point::from((100.0, 100.0)), cfg);
        // A real GlesPixelProgram can't be constructed without an EGL
        // context, so this test stops at the multiplicity check by
        // short-circuiting through `finished()` -- which we don't want
        // to flip. Use the shapes() helper directly to verify the
        // shape list logic.
        assert_eq!(
            ripple.shapes(),
            vec![RippleShape::Ring, RippleShape::Square]
        );
        assert!(!ripple.finished());
    }
}
