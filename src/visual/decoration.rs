//! Rounded client clipping and analytical gradient borders for TideWM.
//!
//! The surface wrapper follows niri's proven shape: temporarily override
//! Smithay's default texture program while drawing each toplevel surface
//! element, mapping texture coordinates back into the window geometry.
//! Borders are a separate fixed-cost signed shape and allocate no texture.

use std::hash::{Hash, Hasher};

use cgmath::{Matrix3, Vector2};
use smithay::{
    backend::renderer::{
        buffer_y_inverted,
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, Uniform,
            UniformName, UniformType, UniformValue,
        },
        utils::CommitCounter,
    },
    utils::{user_data::UserDataMap, Buffer, Physical, Rectangle, Scale, Transform},
};

use crate::config::{BorderConfig, BorderPlacement, RoundingConfig};

const ROUNDED_SURFACE_FRAGMENT_SHADER: &str = r#"
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

uniform vec2 u_geometry_size;
uniform vec4 u_corner_radii;
uniform float u_rounding_power;
uniform float u_antialias;
uniform mat3 u_input_to_geometry;

float corner_coverage(vec2 point, vec2 size, vec4 radii) {
    vec2 center;
    float radius;
    if (point.x < radii.x && point.y < radii.x) {
        radius = radii.x;
        center = vec2(radius);
    } else if (point.x > size.x - radii.y && point.y < radii.y) {
        radius = radii.y;
        center = vec2(size.x - radius, radius);
    } else if (point.x > size.x - radii.z && point.y > size.y - radii.z) {
        radius = radii.z;
        center = vec2(size.x - radius, size.y - radius);
    } else if (point.x < radii.w && point.y > size.y - radii.w) {
        radius = radii.w;
        center = vec2(radius, size.y - radius);
    } else {
        return 1.0;
    }
    if (radius < 0.01)
        return 1.0;
    vec2 q = abs(point - center) / radius;
    float super_distance = pow(
        pow(q.x, u_rounding_power) + pow(q.y, u_rounding_power),
        1.0 / u_rounding_power
    );
    float aa = u_antialias / radius;
    return 1.0 - smoothstep(1.0 - aa, 1.0 + aa, super_distance);
}

void main() {
    vec3 mapped = u_input_to_geometry * vec3(v_coords, 1.0);
    vec2 point = mapped.xy * u_geometry_size;
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif
    if (mapped.x < 0.0 || mapped.x > 1.0 || mapped.y < 0.0 || mapped.y > 1.0)
        color = vec4(0.0);
    else
        color *= corner_coverage(point, u_geometry_size, u_corner_radii);
    color *= alpha;
#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif
    gl_FragColor = color;
}
"#;

const BORDER_FRAGMENT_SHADER: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform vec2 u_outer_origin;
uniform vec2 u_outer_size;
uniform vec2 u_inner_origin;
uniform vec2 u_inner_size;
uniform vec4 u_outer_radii;
uniform vec4 u_inner_radii;
uniform float u_rounding_power;
uniform float u_antialias;
uniform vec4 u_color_from;
uniform vec4 u_color_to;
uniform float u_angle;
uniform float u_opacity;

float rounded_coverage(vec2 point, vec2 origin, vec2 box_size, vec4 radii) {
    vec2 p = point - origin;
    if (p.x < 0.0 || p.y < 0.0 || p.x > box_size.x || p.y > box_size.y)
        return 0.0;
    vec2 center;
    float radius;
    if (p.x < radii.x && p.y < radii.x) {
        radius = radii.x;
        center = vec2(radius);
    } else if (p.x > box_size.x - radii.y && p.y < radii.y) {
        radius = radii.y;
        center = vec2(box_size.x - radius, radius);
    } else if (p.x > box_size.x - radii.z && p.y > box_size.y - radii.z) {
        radius = radii.z;
        center = vec2(box_size.x - radius, box_size.y - radius);
    } else if (p.x < radii.w && p.y > box_size.y - radii.w) {
        radius = radii.w;
        center = vec2(radius, box_size.y - radius);
    } else {
        return 1.0;
    }
    if (radius < 0.01)
        return 1.0;
    vec2 q = abs(p - center) / radius;
    float d = pow(
        pow(q.x, u_rounding_power) + pow(q.y, u_rounding_power),
        1.0 / u_rounding_power
    );
    float aa = u_antialias / radius;
    return 1.0 - smoothstep(1.0 - aa, 1.0 + aa, d);
}

void main() {
    vec2 point = v_coords * size;
    float outer = rounded_coverage(
        point, u_outer_origin, u_outer_size, u_outer_radii
    );
    float inner = rounded_coverage(
        point, u_inner_origin, u_inner_size, u_inner_radii
    );
    float coverage = outer * (1.0 - inner);
    vec2 direction = vec2(cos(u_angle), sin(u_angle));
    vec2 centered = point - (u_outer_origin + u_outer_size * 0.5);
    float reach = max(
        abs(direction.x) * u_outer_size.x + abs(direction.y) * u_outer_size.y,
        1.0
    );
    float gradient = clamp(dot(centered, direction) / reach + 0.5, 0.0, 1.0);
    vec4 color = mix(u_color_from, u_color_to, gradient);
    float out_alpha = coverage * color.a * u_opacity * alpha;
    gl_FragColor = vec4(color.rgb * out_alpha, out_alpha);
}
"#;

pub fn rounded_surface_program(
    cache: &mut Option<GlesTexProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesTexProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    let uniforms = [
        UniformName::new("u_geometry_size", UniformType::_2f),
        UniformName::new("u_corner_radii", UniformType::_4f),
        UniformName::new("u_rounding_power", UniformType::_1f),
        UniformName::new("u_antialias", UniformType::_1f),
        UniformName::new("u_input_to_geometry", UniformType::Matrix3x3),
    ];
    match renderer.compile_custom_texture_shader(ROUNDED_SURFACE_FRAGMENT_SHADER, &uniforms) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile rounded-surface shader");
            None
        }
    }
}

pub fn border_program(
    cache: &mut Option<GlesPixelProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesPixelProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    let uniforms = [
        UniformName::new("u_outer_origin", UniformType::_2f),
        UniformName::new("u_outer_size", UniformType::_2f),
        UniformName::new("u_inner_origin", UniformType::_2f),
        UniformName::new("u_inner_size", UniformType::_2f),
        UniformName::new("u_outer_radii", UniformType::_4f),
        UniformName::new("u_inner_radii", UniformType::_4f),
        UniformName::new("u_rounding_power", UniformType::_1f),
        UniformName::new("u_antialias", UniformType::_1f),
        UniformName::new("u_color_from", UniformType::_4f),
        UniformName::new("u_color_to", UniformType::_4f),
        UniformName::new("u_angle", UniformType::_1f),
        UniformName::new("u_opacity", UniformType::_1f),
    ];
    match renderer.compile_custom_pixel_shader(BORDER_FRAGMENT_SHADER, &uniforms) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile window-border shader");
            None
        }
    }
}

fn matrix_uniform(name: &'static str, matrix: Matrix3<f32>) -> Uniform<'static> {
    let columns = [
        matrix.x.x, matrix.x.y, matrix.x.z, matrix.y.x, matrix.y.y, matrix.y.z, matrix.z.x,
        matrix.z.y, matrix.z.z,
    ];
    Uniform::new(
        name,
        UniformValue::Matrix3x3 {
            matrices: vec![columns],
            transpose: false,
        },
    )
}

/// A Wayland surface element clipped to its parent toplevel's visual
/// geometry. Popups are intentionally rendered normally by the caller.
pub struct RoundedSurfaceElement {
    id: Id,
    inner: smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<GlesRenderer>,
    program: GlesTexProgram,
    geometry: Rectangle<i32, Physical>,
    radii: [f32; 4],
    power: f32,
    antialias: f32,
    scale: Scale<f64>,
}

impl RoundedSurfaceElement {
    pub fn new(
        inner: smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<
            GlesRenderer,
        >,
        program: GlesTexProgram,
        geometry: Rectangle<i32, Physical>,
        radii: [f32; 4],
        power: f32,
        antialias: f32,
        scale: Scale<f64>,
    ) -> Self {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for value in radii {
            value.to_bits().hash(&mut hash);
        }
        power.to_bits().hash(&mut hash);
        antialias.to_bits().hash(&mut hash);
        let id = inner.id().clone().namespaced(hash.finish() as usize);
        Self {
            id,
            inner,
            program,
            geometry,
            radii,
            power,
            antialias,
            scale,
        }
    }

    fn uniforms(&self) -> Vec<Uniform<'static>> {
        let elem = self.inner.geometry(self.scale);
        let elem_size = [elem.size.w.max(1) as f32, elem.size.h.max(1) as f32];
        let geo_size = [
            self.geometry.size.w.max(1) as f32,
            self.geometry.size.h.max(1) as f32,
        ];
        let buffer_size = self.inner.buffer_size();
        let view = self.inner.view();
        let src_size = [
            view.src.size.w.max(f64::EPSILON) as f32,
            view.src.size.h.max(f64::EPSILON) as f32,
        ];
        let mut transform = self.inner.transform();
        if transform == Transform::_90 {
            transform = Transform::_270;
        } else if transform == Transform::_270 {
            transform = Transform::_90;
        }
        let transform_matrix = Matrix3::from_translation(Vector2::new(0.5, 0.5))
            * transform.matrix()
            * Matrix3::from_translation(Vector2::new(-0.5, -0.5));
        let y_invert = if buffer_y_inverted(self.inner.buffer()).unwrap_or(false) {
            Matrix3::from_nonuniform_scale(1.0, -1.0)
        } else {
            Matrix3::from_scale(1.0)
        };
        let matrix = transform_matrix
            * Matrix3::from_nonuniform_scale(
                elem_size[0] / geo_size[0],
                elem_size[1] / geo_size[1],
            )
            * Matrix3::from_translation(Vector2::new(
                (elem.loc.x - self.geometry.loc.x) as f32 / elem_size[0],
                (elem.loc.y - self.geometry.loc.y) as f32 / elem_size[1],
            ))
            * Matrix3::from_nonuniform_scale(
                buffer_size.w.max(1) as f32 / src_size[0],
                buffer_size.h.max(1) as f32 / src_size[1],
            )
            * Matrix3::from_translation(Vector2::new(
                -(view.src.loc.x as f32) / buffer_size.w.max(1) as f32,
                -(view.src.loc.y as f32) / buffer_size.h.max(1) as f32,
            ))
            * y_invert;
        vec![
            Uniform::new("u_geometry_size", geo_size),
            Uniform::new("u_corner_radii", self.radii),
            Uniform::new("u_rounding_power", self.power),
            Uniform::new("u_antialias", self.antialias),
            matrix_uniform("u_input_to_geometry", matrix),
        ]
    }
}

impl Element for RoundedSurfaceElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for RoundedSurfaceElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        frame.override_default_tex_program(self.program.clone(), self.uniforms());
        let result = self
            .inner
            .draw(frame, src, dst, damage, opaque_regions, cache);
        frame.clear_tex_program_override();
        result
    }
}

pub struct BorderElement {
    id: Id,
    commit: CommitCounter,
    area: Rectangle<i32, Physical>,
    program: GlesPixelProgram,
    outer_origin: [f32; 2],
    outer_size: [f32; 2],
    inner_origin: [f32; 2],
    inner_size: [f32; 2],
    outer_radii: [f32; 4],
    inner_radii: [f32; 4],
    power: f32,
    antialias: f32,
    from: [f32; 4],
    to: [f32; 4],
    angle: f32,
    opacity: f32,
}

impl BorderElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_id: &Id,
        window: Rectangle<i32, Physical>,
        scale: f64,
        program: GlesPixelProgram,
        rounding: &RoundingConfig,
        border: &BorderConfig,
        focused: bool,
        urgent: bool,
        elapsed_secs: f32,
    ) -> Self {
        let px = scale as f32;
        let width = border.width * px;
        let (outside, inside) = match border.placement {
            BorderPlacement::Outside => (width, 0.0),
            BorderPlacement::Center => (width * 0.5, width * 0.5),
            BorderPlacement::Inside => (0.0, width),
        };
        let padding = (outside + border.antialias * px + 2.0).ceil();
        let padding_i = padding as i32;
        let area = Rectangle::new(
            (window.loc.x - padding_i, window.loc.y - padding_i).into(),
            (window.size.w + padding_i * 2, window.size.h + padding_i * 2).into(),
        );
        let outer_origin = [padding - outside, padding - outside];
        let outer_size = [
            window.size.w as f32 + outside * 2.0,
            window.size.h as f32 + outside * 2.0,
        ];
        let inner_origin = [padding + inside, padding + inside];
        let inner_size = [
            (window.size.w as f32 - inside * 2.0).max(0.0),
            (window.size.h as f32 - inside * 2.0).max(0.0),
        ];
        let radius_offset = border.radius_offset * px;
        let mut outer_radii = [0.0; 4];
        let mut inner_radii = [0.0; 4];
        for index in 0..4 {
            let base = rounding.radii[index] * px + radius_offset;
            outer_radii[index] = (base + outside).max(0.0);
            inner_radii[index] = (base - inside).max(0.0);
        }
        let (from, to, opacity) = if urgent {
            (border.urgent_from, border.urgent_to, border.urgent_opacity)
        } else if focused {
            (border.active_from, border.active_to, border.opacity)
        } else {
            (
                border.inactive_from,
                border.inactive_to,
                border.inactive_opacity,
            )
        };
        let state_animate = border.animate
            && if urgent {
                border.animate_urgent
            } else if focused {
                border.animate_focused
            } else {
                border.animate_inactive
            };
        let angle = (border.angle
            + if state_animate {
                elapsed_secs * border.animation_speed
            } else {
                0.0
            })
        .to_radians();
        let pulse = if state_animate && border.pulse_amount > 0.0 {
            1.0 - border.pulse_amount
                + border.pulse_amount
                    * (elapsed_secs * border.pulse_speed * std::f32::consts::TAU)
                        .sin()
                        .mul_add(0.5, 0.5)
        } else {
            1.0
        };
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for value in [
            width,
            border.angle,
            border.radius_offset,
            border.antialias,
            rounding.power,
            opacity,
        ] {
            value.to_bits().hash(&mut hash);
        }
        for radius in rounding.radii {
            radius.to_bits().hash(&mut hash);
        }
        for color in [from, to] {
            for channel in color {
                channel.to_bits().hash(&mut hash);
            }
        }
        focused.hash(&mut hash);
        urgent.hash(&mut hash);
        state_animate.hash(&mut hash);
        border.placement.hash(&mut hash);
        let id = base_id.clone().namespaced(hash.finish() as usize);
        let commit = if state_animate {
            CommitCounter::from((elapsed_secs * 120.0) as usize)
        } else {
            CommitCounter::default()
        };
        Self {
            id,
            commit,
            area,
            program,
            outer_origin,
            outer_size,
            inner_origin,
            inner_size,
            outer_radii,
            inner_radii,
            power: rounding.power,
            antialias: border.antialias * px,
            from,
            to,
            angle,
            opacity: opacity * pulse,
        }
    }
}

impl Element for BorderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size((self.area.size.w as f64, self.area.size.h as f64).into())
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.area
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for BorderElement {
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
            (self.area.size.w, self.area.size.h).into(),
            Some(damage),
            1.0,
            &[
                Uniform::new("u_outer_origin", self.outer_origin),
                Uniform::new("u_outer_size", self.outer_size),
                Uniform::new("u_inner_origin", self.inner_origin),
                Uniform::new("u_inner_size", self.inner_size),
                Uniform::new("u_outer_radii", self.outer_radii),
                Uniform::new("u_inner_radii", self.inner_radii),
                Uniform::new("u_rounding_power", self.power),
                Uniform::new("u_antialias", self.antialias),
                Uniform::new("u_color_from", self.from),
                Uniform::new("u_color_to", self.to),
                Uniform::new("u_angle", self.angle),
                Uniform::new("u_opacity", self.opacity),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shaders_keep_fixed_cost_contracts() {
        assert!(ROUNDED_SURFACE_FRAGMENT_SHADER.contains("//_DEFINES_"));
        assert!(ROUNDED_SURFACE_FRAGMENT_SHADER.contains("u_input_to_geometry"));
        assert!(ROUNDED_SURFACE_FRAGMENT_SHADER.contains("u_rounding_power"));
        assert!(!ROUNDED_SURFACE_FRAGMENT_SHADER.contains("for ("));
        assert!(BORDER_FRAGMENT_SHADER.contains("u_color_from"));
        assert!(BORDER_FRAGMENT_SHADER.contains("u_color_to"));
        assert!(!BORDER_FRAGMENT_SHADER.contains("sampler2D"));
    }
}
