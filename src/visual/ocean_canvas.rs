//! Camera-anchored reference field for Ocean's continuous world.
//!
//! Windows alone do not provide motion cues once the camera crosses empty
//! space. This analytical grid sits between the wallpaper and windows, moves
//! with the world, and changes density with zoom. A center point appears only
//! after camera movement and fades with inactivity. It owns no texture or
//! framebuffer and becomes no render element at all when both cues are idle.

use std::time::{Duration, Instant};

use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement},
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, Uniform, UniformName, UniformType,
        },
        utils::CommitCounter,
    },
    utils::{user_data::UserDataMap, Buffer, Logical, Physical, Rectangle, Scale, Transform},
};

const OCEAN_CANVAS_SHADER: &str = r#"
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform vec2 u_camera_origin;
uniform float u_zoom;
uniform float u_grid_size;
uniform vec3 u_color;
uniform float u_opacity;
uniform float u_marker_alpha;

float grid_line(float coordinate, float spacing, float width) {
    float position = mod(coordinate, spacing);
    float distance = min(position, spacing - position);
    return 1.0 - smoothstep(width, width + 1.0, distance);
}

void main() {
    float zoom = max(u_zoom, 0.05);
    float spacing = u_grid_size * zoom;
    if (spacing < 32.0) spacing *= 4.0;
    if (spacing < 32.0) spacing *= 4.0;
    if (spacing < 32.0) spacing *= 4.0;

    vec2 screen = v_coords * size;
    vec2 scaled_world = screen + u_camera_origin * zoom;
    float minor = max(
        grid_line(scaled_world.x, spacing, 0.55),
        grid_line(scaled_world.y, spacing, 0.55)
    );
    float major_spacing = spacing * 4.0;
    float major = max(
        grid_line(scaled_world.x, major_spacing, 0.9),
        grid_line(scaled_world.y, major_spacing, 0.9)
    );
    float origin_axis = max(
        1.0 - smoothstep(1.0, 2.4, abs(scaled_world.x)),
        1.0 - smoothstep(1.0, 2.4, abs(scaled_world.y))
    );
    float strength = clamp(minor * 0.32 + major * 0.62 + origin_axis, 0.0, 1.0);
    float grid_alpha = strength * u_opacity * alpha;

    float center_distance = length(screen - size * 0.5);
    float point = 1.0 - smoothstep(2.0, 5.5, center_distance);
    float halo = (1.0 - smoothstep(5.0, 18.0, center_distance)) * 0.24;
    float marker_alpha = clamp(point + halo, 0.0, 1.0) * u_marker_alpha * alpha;
    vec3 marker_color = vec3(0.78, 0.97, 1.0);
    float out_alpha = grid_alpha + marker_alpha * (1.0 - grid_alpha);
    vec3 premultiplied = u_color * grid_alpha
        + marker_color * marker_alpha * (1.0 - grid_alpha);
    gl_FragColor = vec4(premultiplied, out_alpha);
}
"#;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OceanCanvasSample {
    pub origin: [f32; 2],
    pub zoom: f32,
    pub grid_size: f32,
    pub color: [f32; 3],
    pub opacity: f32,
    pub marker_alpha: f32,
}

pub struct OceanCanvas {
    id: Id,
    commit: CommitCounter,
    last_sample: Option<OceanCanvasSample>,
    last_camera: Option<([f32; 2], f32)>,
    last_movement: Option<Instant>,
}

impl Default for OceanCanvas {
    fn default() -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
            last_sample: None,
            last_camera: None,
            last_movement: None,
        }
    }
}

impl OceanCanvas {
    pub fn note_camera(
        &mut self,
        origin: [f32; 2],
        zoom: f32,
        marker_enabled: bool,
        fade: Duration,
    ) -> f32 {
        let camera = (origin, zoom);
        if self.last_camera.is_some_and(|previous| previous != camera) {
            self.last_movement = marker_enabled.then(Instant::now);
        }
        self.last_camera = Some(camera);
        if !marker_enabled || fade.is_zero() {
            self.last_movement = None;
            return 0.0;
        }
        self.last_movement
            .map(|last| 1.0 - last.elapsed().as_secs_f32() / fade.as_secs_f32())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    }

    pub fn marker_active(&self, fade: Duration) -> bool {
        !fade.is_zero() && self.last_movement.is_some_and(|last| last.elapsed() < fade)
    }

    pub fn frame_element(
        &mut self,
        program: GlesPixelProgram,
        area: Rectangle<i32, Logical>,
        sample: OceanCanvasSample,
    ) -> OceanCanvasElement {
        if self.last_sample != Some(sample) {
            self.last_sample = Some(sample);
            self.commit.increment();
        }
        OceanCanvasElement {
            id: self.id.clone(),
            commit: self.commit,
            area,
            program,
            sample,
        }
    }
}

pub fn ocean_canvas_program(
    cache: &mut Option<GlesPixelProgram>,
    renderer: &mut GlesRenderer,
) -> Option<GlesPixelProgram> {
    if let Some(program) = cache {
        return Some(program.clone());
    }
    match renderer.compile_custom_pixel_shader(
        OCEAN_CANVAS_SHADER,
        &[
            UniformName::new("u_camera_origin", UniformType::_2f),
            UniformName::new("u_zoom", UniformType::_1f),
            UniformName::new("u_grid_size", UniformType::_1f),
            UniformName::new("u_color", UniformType::_3f),
            UniformName::new("u_opacity", UniformType::_1f),
            UniformName::new("u_marker_alpha", UniformType::_1f),
        ],
    ) {
        Ok(program) => {
            *cache = Some(program.clone());
            Some(program)
        }
        Err(err) => {
            tracing::warn!(%err, "Failed to compile Ocean canvas shader");
            None
        }
    }
}

pub struct OceanCanvasElement {
    id: Id,
    commit: CommitCounter,
    area: Rectangle<i32, Logical>,
    program: GlesPixelProgram,
    sample: OceanCanvasSample,
}

impl Element for OceanCanvasElement {
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

impl RenderElement<GlesRenderer> for OceanCanvasElement {
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
                Uniform::new("u_camera_origin", self.sample.origin),
                Uniform::new("u_zoom", self.sample.zoom),
                Uniform::new("u_grid_size", self.sample.grid_size),
                Uniform::new("u_color", self.sample.color),
                Uniform::new("u_opacity", self.sample.opacity),
                Uniform::new("u_marker_alpha", self.sample.marker_alpha),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_is_camera_anchored_and_allocation_free() {
        assert!(OCEAN_CANVAS_SHADER.contains("u_camera_origin * zoom"));
        assert!(OCEAN_CANVAS_SHADER.contains("u_grid_size * zoom"));
        assert!(OCEAN_CANVAS_SHADER.contains("screen - size * 0.5"));
        assert!(!OCEAN_CANVAS_SHADER.contains("sampler2D"));
    }

    #[test]
    fn marker_starts_only_after_motion_and_expires() {
        let mut canvas = OceanCanvas::default();
        let fade = Duration::from_millis(4200);
        assert_eq!(canvas.note_camera([0.0, 0.0], 1.0, true, fade), 0.0);
        assert!(canvas.note_camera([10.0, 0.0], 1.0, true, fade) > 0.99);
        canvas.last_movement = Some(Instant::now() - fade);
        assert!(!canvas.marker_active(fade));
        assert_eq!(canvas.note_camera([10.0, 0.0], 1.0, true, fade), 0.0);
    }
}
