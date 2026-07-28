//! Water-glass: the render roadmap's first actually-visible effect (Phase
//! R1, see AGENT.md's "Render and visual identity roadmap"). Renders a
//! captured backdrop (`backdrop.rs`, Phase R0.5) through a custom GLES
//! fragment shader that perturbs the sample coordinate, giving a wavy,
//! refracted look instead of the plain flat content that would otherwise
//! sit behind a semi-transparent window.
//!
//! Applies specifically to a floating window with a `window_opacity` rule
//! below 1.0 -- reusing that existing rule rather than inventing a new
//! config key, since "what shows through a semi-transparent window" is
//! exactly what this already means. The water-glass layer draws fully
//! opaque at the window's own rect, *behind* the window's own (semi-
//! transparent) surface element: without that, the real undistorted
//! backdrop would still show through underneath the distorted copy,
//! reading as a ghost rather than glass.

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture},
        utils::CommitCounter,
        Texture,
    },
    utils::{user_data::UserDataMap, Buffer, Physical, Rectangle, Scale, Transform},
};

/// Adapted directly from Smithay's own default texture fragment shader
/// (`backend/renderer/gles/shaders/implicit/texture.frag` in the pinned
/// source) -- same `//_DEFINES_` placeholder and `#if defined(...)`
/// blocks, since `compile_custom_texture_shader` substitutes the same set
/// of defines regardless of which custom shader is compiled. The only
/// change from the original is the sampling coordinate: a small,
/// position-based sine/cosine offset instead of a straight `v_coords`
/// lookup. Deliberately static (no time uniform) for this first cut --
/// animating the ripple is a later polish pass, once something is driving
/// it through the R0 `Animation` clock.
const WATER_GLASS_FRAGMENT_SHADER: &str = r#"
#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    vec2 distorted = v_coords + vec2(
        sin(v_coords.y * 24.0) * 0.012,
        cos(v_coords.x * 20.0) * 0.012
    );
    distorted = clamp(distorted, 0.0, 1.0);
    vec4 color = texture2D(tex, distorted);

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0) * alpha;
#else
    color = color * alpha;
#endif

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
"#;

/// Compiles the shader against `renderer` the first time it's needed and
/// caches it -- a `GlesTexProgram` is cheap to clone (`Arc`-backed) once
/// compiled, so every frame just clones the cached handle rather than
/// recompiling. Per-renderer, not a process-global `OnceLock` like
/// `toast::font()`, since compiling needs a live `&mut GlesRenderer`.
pub fn water_glass_program(
    cache: &mut Option<GlesTexProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesTexProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    match renderer.compile_custom_texture_shader(WATER_GLASS_FRAGMENT_SHADER, &[]) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile water-glass shader");
            None
        }
    }
}

/// One window's water-glass layer for one frame: the captured backdrop
/// texture run through the refraction shader, drawn at the window's own
/// rect. `id`/`commit` are the *stable* per-window identity kept in
/// `backdrop::BackdropCapture` (not a fresh `Id` each frame) -- the damage
/// tracker keys its own per-element bookkeeping off `Id`, and a fresh one
/// every frame would leak an orphaned entry in that tracker's internal
/// map for every frame this window is water-glass-eligible, never pruned.
pub struct WaterGlassElement {
    id: Id,
    commit: CommitCounter,
    texture: GlesTexture,
    geometry: Rectangle<i32, Physical>,
    program: GlesTexProgram,
}

impl WaterGlassElement {
    pub fn new(
        id: Id,
        commit: CommitCounter,
        texture: GlesTexture,
        geometry: Rectangle<i32, Physical>,
        program: GlesTexProgram,
    ) -> Self {
        Self {
            id,
            commit,
            texture,
            geometry,
            program,
        }
    }
}

impl Element for WaterGlassElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.texture.size().to_f64())
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }

    // `opaque_regions` deliberately left at its default (empty): this
    // element's own alpha is always 1.0 (see the module doc comment for
    // why), but claiming opacity here would tell the damage tracker it
    // can skip drawing whatever's behind it -- and the whole point of
    // this element existing is that it visually replaces that content,
    // not that nothing needs to be there. Getting this wrong is exactly
    // the kind of "looks plausible, quietly skips a draw" bug this
    // project has already been burned by once (session-lock's element
    // ordering).
}

impl RenderElement<GlesRenderer> for WaterGlassElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            self.alpha(),
            Some(&self.program),
            &[],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_source_keeps_the_defines_placeholder_and_required_uniforms() {
        // Not a GL compile check (no EGL context in unit tests, see every
        // other backend/gles-adjacent module in this project) -- just a
        // guard against accidentally dropping the substitution point or a
        // uniform `compile_custom_texture_shader`'s contract requires.
        assert!(WATER_GLASS_FRAGMENT_SHADER.contains("//_DEFINES_"));
        assert!(WATER_GLASS_FRAGMENT_SHADER.contains("uniform sampler2D tex"));
        assert!(WATER_GLASS_FRAGMENT_SHADER.contains("uniform float alpha"));
        assert!(WATER_GLASS_FRAGMENT_SHADER.contains("varying vec2 v_coords"));
    }
}
