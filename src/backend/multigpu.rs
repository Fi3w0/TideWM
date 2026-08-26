//! Import bridge between Smithay's multi-GPU texture cache and TideWM's
//! deliberately concrete GLES visual pipeline.

use std::fmt;

use smithay::{
    backend::{
        allocator::{dmabuf::Dmabuf, Format, Fourcc},
        drm::DrmDeviceFd,
        egl::{display::EGLBufferReader, Error as EglError},
        renderer::{
            gles::{GlesError, GlesRenderer, GlesTexture},
            multigpu::{gbm::GbmGlesBackend, MultiRenderer},
            sync::SyncPoint,
            ContextId, DebugFlags, ImportDma, ImportDmaWl, ImportEgl, ImportMem, ImportMemWl,
            Renderer, RendererSuper, TextureFilter,
        },
    },
    reexports::wayland_server::{
        protocol::{wl_buffer::WlBuffer, wl_shm},
        DisplayHandle,
    },
    utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform},
    wayland::compositor::SurfaceData,
};

pub(crate) type GbmGlesApi = GbmGlesBackend<GlesRenderer, DrmDeviceFd>;
pub(crate) type TideMultiRenderer<'a> = MultiRenderer<'a, 'a, GbmGlesApi, GbmGlesApi>;

/// Presents the selected node's concrete GLES identity while using
/// `MultiRenderer` for DMA-BUF imports. This causes Smithay to perform any
/// required cross-device copy, then stores the resulting primary-node
/// `GlesTexture` in the ordinary surface cache consumed by TideWM's existing
/// `WaylandSurfaceRenderElement<GlesRenderer>` elements.
pub(crate) struct ImportBridge<'a> {
    renderer: TideMultiRenderer<'a>,
}

impl fmt::Debug for ImportBridge<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImportBridge").finish_non_exhaustive()
    }
}

impl<'a> ImportBridge<'a> {
    pub(crate) fn new(renderer: TideMultiRenderer<'a>) -> Self {
        Self { renderer }
    }

    fn gles(&mut self) -> &mut GlesRenderer {
        self.renderer.as_mut()
    }

    fn imported_texture(
        &mut self,
        texture: smithay::backend::renderer::multigpu::MultiTexture,
    ) -> Option<GlesTexture> {
        let context = self.gles().context_id();
        texture.get::<GbmGlesApi>(&context)
    }
}

impl RendererSuper for ImportBridge<'_> {
    type Error = GlesError;
    type TextureId = GlesTexture;
    type Framebuffer<'buffer> = <GlesRenderer as RendererSuper>::Framebuffer<'buffer>;
    type Frame<'frame, 'buffer>
        = <GlesRenderer as RendererSuper>::Frame<'frame, 'buffer>
    where
        'buffer: 'frame,
        Self: 'frame;
}

impl Renderer for ImportBridge<'_> {
    fn context_id(&self) -> ContextId<GlesTexture> {
        self.renderer.as_ref().context_id()
    }

    fn downscale_filter(&mut self, filter: TextureFilter) -> Result<(), GlesError> {
        self.gles().downscale_filter(filter)
    }

    fn upscale_filter(&mut self, filter: TextureFilter) -> Result<(), GlesError> {
        self.gles().upscale_filter(filter)
    }

    fn set_debug_flags(&mut self, flags: DebugFlags) {
        self.gles().set_debug_flags(flags);
    }

    fn debug_flags(&self) -> DebugFlags {
        self.renderer.as_ref().debug_flags()
    }

    fn render<'frame, 'buffer>(
        &'frame mut self,
        framebuffer: &'frame mut Self::Framebuffer<'buffer>,
        output_size: Size<i32, Physical>,
        transform: Transform,
    ) -> Result<Self::Frame<'frame, 'buffer>, GlesError>
    where
        'buffer: 'frame,
    {
        self.gles().render(framebuffer, output_size, transform)
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), GlesError> {
        self.gles().wait(sync)
    }

    fn cleanup_texture_cache(&mut self) -> Result<(), GlesError> {
        self.gles().cleanup_texture_cache()
    }
}

impl ImportMem for ImportBridge<'_> {
    fn import_memory(
        &mut self,
        data: &[u8],
        format: Fourcc,
        size: Size<i32, BufferCoord>,
        flipped: bool,
    ) -> Result<GlesTexture, GlesError> {
        self.gles().import_memory(data, format, size, flipped)
    }

    fn update_memory(
        &mut self,
        texture: &GlesTexture,
        data: &[u8],
        region: Rectangle<i32, BufferCoord>,
    ) -> Result<(), GlesError> {
        self.gles().update_memory(texture, data, region)
    }

    fn mem_formats(&self) -> Box<dyn Iterator<Item = Fourcc>> {
        self.renderer.as_ref().mem_formats()
    }
}

impl ImportMemWl for ImportBridge<'_> {
    fn import_shm_buffer(
        &mut self,
        buffer: &WlBuffer,
        surface: Option<&SurfaceData>,
        damage: &[Rectangle<i32, BufferCoord>],
    ) -> Result<GlesTexture, GlesError> {
        self.gles().import_shm_buffer(buffer, surface, damage)
    }

    fn shm_formats(&self) -> Box<dyn Iterator<Item = wl_shm::Format>> {
        self.renderer.as_ref().shm_formats()
    }
}

impl ImportEgl for ImportBridge<'_> {
    fn bind_wl_display(&mut self, display: &DisplayHandle) -> Result<(), EglError> {
        self.gles().bind_wl_display(display)
    }

    fn unbind_wl_display(&mut self) {
        self.gles().unbind_wl_display();
    }

    fn egl_reader(&self) -> Option<&EGLBufferReader> {
        self.renderer.as_ref().egl_reader()
    }

    fn import_egl_buffer(
        &mut self,
        buffer: &WlBuffer,
        surface: Option<&SurfaceData>,
        damage: &[Rectangle<i32, BufferCoord>],
    ) -> Result<GlesTexture, GlesError> {
        self.gles().import_egl_buffer(buffer, surface, damage)
    }
}

impl ImportDma for ImportBridge<'_> {
    fn dmabuf_formats(&self) -> smithay::backend::allocator::format::FormatSet {
        self.renderer.dmabuf_formats()
    }

    fn has_dmabuf_format(&self, format: Format) -> bool {
        self.renderer.has_dmabuf_format(format)
    }

    fn import_dmabuf(
        &mut self,
        dmabuf: &Dmabuf,
        damage: Option<&[Rectangle<i32, BufferCoord>]>,
    ) -> Result<GlesTexture, GlesError> {
        if let Ok(texture) = self.renderer.import_dmabuf(dmabuf, damage) {
            if let Some(texture) = self.imported_texture(texture) {
                return Ok(texture);
            }
        }
        self.gles().import_dmabuf(dmabuf, damage)
    }
}

impl ImportDmaWl for ImportBridge<'_> {}
