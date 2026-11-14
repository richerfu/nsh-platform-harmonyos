//! NativeImage texture consumption through statically linked community EGL/GLES bindings.
use std::{marker::PhantomData, rc::Rc};

use glow::HasContext;

// GL_OES_EGL_image_external is accepted by glow but has no exported enum.
const TEXTURE_EXTERNAL_OES: u32 = 0x8d65;
use khronos_egl as egl;
use ohos_native_image_binding::{BufferUsage, TextureImage};

use self::layout::{Layout, validate_size};
use super::frame_wake::FrameWake;

fn failed(operation: &'static str) -> Error {
    Error {
        operation,
        code: -1,
    }
}
fn egl_error(error: egl::Error) -> Error {
    Error {
        operation: "EGL",
        code: error.into(),
    }
}

pub(super) struct Context {
    egl: egl::Instance<egl::Static>,
    display: egl::Display,
    config: egl::Config,
    context: egl::Context,
    surface: egl::Surface,
    // EGL contexts must remain on their creating thread, even if glow is Send.
    _thread: PhantomData<Rc<()>>,
}
struct ForeignEglBinding {
    display: Option<egl::Display>,
    context: egl::Context,
    draw: Option<egl::Surface>,
    read: Option<egl::Surface>,
}
pub(super) struct Current<'a> {
    context: &'a Context,
    restore_foreign: Option<ForeignEglBinding>,
    previous_api: Option<egl::Enum>,
}
struct ClientApi<'a> {
    egl: &'a egl::Instance<egl::Static>,
    previous: egl::Enum,
    active: bool,
}
impl ClientApi<'_> {
    fn restore(&mut self) -> Result<(), Error> {
        self.egl.bind_api(self.previous).map_err(egl_error)?;
        self.active = false;
        Ok(())
    }
}
impl Drop for ClientApi<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.egl.bind_api(self.previous);
        }
    }
}
impl Drop for Current<'_> {
    fn drop(&mut self) {
        if let Some(foreign) = self.restore_foreign.take() {
            let _ = self.context.egl.make_current(
                foreign.display.unwrap_or(self.context.display),
                foreign.draw,
                foreign.read,
                Some(foreign.context),
            );
        }
        if let Some(previous_api) = self.previous_api {
            let _ = self.context.egl.bind_api(previous_api);
        }
    }
}
impl Context {
    fn new() -> Result<Self, Error> {
        let egl = egl::Instance::new(egl::Static);
        let previous = egl.query_api();
        egl.bind_api(egl::OPENGL_ES_API).map_err(egl_error)?;
        let mut client_api = ClientApi {
            egl: &egl,
            previous,
            active: true,
        };
        // SAFETY: EGL_DEFAULT_DISPLAY is the platform's null display sentinel.
        let display =
            unsafe { egl.get_display(egl::DEFAULT_DISPLAY) }.ok_or(failed("EGL display"))?;
        egl.initialize(display).map_err(egl_error)?;
        let choose = |renderable| {
            egl.choose_first_config(
                display,
                &[
                    egl::SURFACE_TYPE,
                    egl::PBUFFER_BIT | egl::WINDOW_BIT,
                    egl::RENDERABLE_TYPE,
                    renderable,
                    egl::RED_SIZE,
                    8,
                    egl::GREEN_SIZE,
                    8,
                    egl::BLUE_SIZE,
                    8,
                    egl::ALPHA_SIZE,
                    8,
                    egl::NONE,
                ],
            )
            .map_err(egl_error)
        };
        // EGL_KHR_create_context's ES3 bit; some older drivers advertise ES2 only.
        let config = match choose(0x0040)? {
            Some(config) => config,
            None => choose(egl::OPENGL_ES2_BIT)?.ok_or(failed("EGL RGBA config"))?,
        };
        let context = egl
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
            )
            .map_err(egl_error)?;
        let surface = match egl.create_pbuffer_surface(
            display,
            config,
            &[egl::WIDTH, 1, egl::HEIGHT, 1, egl::NONE],
        ) {
            Ok(surface) => surface,
            Err(error) => {
                let _ = egl.destroy_context(display, context);
                return Err(egl_error(error));
            }
        };
        if let Err(error) = client_api.restore() {
            let _ = egl.destroy_surface(display, surface);
            let _ = egl.destroy_context(display, context);
            return Err(error);
        }
        drop(client_api);
        Ok(Self {
            egl,
            display,
            config,
            context,
            surface,
            _thread: PhantomData,
        })
    }
    pub(super) fn enter(&self) -> Result<Current<'_>, Error> {
        let previous_api = self.egl.query_api();
        let switched_api = if previous_api != egl::OPENGL_ES_API {
            self.egl.bind_api(egl::OPENGL_ES_API).map_err(egl_error)?;
            Some(previous_api)
        } else {
            None
        };
        let mut client_api = ClientApi {
            egl: &self.egl,
            previous: previous_api,
            active: switched_api.is_some(),
        };
        let current_context = self.egl.get_current_context();
        if current_context == Some(self.context) {
            client_api.active = false;
            return Ok(Current {
                context: self,
                restore_foreign: None,
                previous_api: switched_api,
            });
        }
        // EGL current state is per client API. Preserve any foreign ES context
        // before replacing it, while keeping this worker's own context resident.
        let restore_foreign = current_context.map(|context| ForeignEglBinding {
            display: self.egl.get_current_display(),
            context,
            draw: self.egl.get_current_surface(egl::DRAW),
            read: self.egl.get_current_surface(egl::READ),
        });
        self.egl
            .make_current(
                self.display,
                Some(self.surface),
                Some(self.surface),
                Some(self.context),
            )
            .map_err(egl_error)?;
        client_api.active = false;
        Ok(Current {
            context: self,
            restore_foreign,
            previous_api: switched_api,
        })
    }
    pub(super) fn bind_pbuffer(&self) {
        if self.egl.query_api() == egl::OPENGL_ES_API
            && self.egl.get_current_context() == Some(self.context)
            && (self.egl.get_current_surface(egl::DRAW) != Some(self.surface)
                || self.egl.get_current_surface(egl::READ) != Some(self.surface))
        {
            let _ = self.egl.make_current(
                self.display,
                Some(self.surface),
                Some(self.surface),
                Some(self.context),
            );
        }
    }
}
impl Drop for Context {
    fn drop(&mut self) {
        if self.egl.query_api() == egl::OPENGL_ES_API
            && self.egl.get_current_context() == Some(self.context)
        {
            let _ = self.egl.make_current(self.display, None, None, None);
        }
        let _ = self.egl.destroy_surface(self.display, self.surface);
        let _ = self.egl.destroy_context(self.display, self.context);
        // The default EGL display is shared with the embedding app. Terminating
        // it here would invalidate that app's independent rendering contexts.
    }
}

pub(super) struct BlitFramebuffer {
    context: Rc<Context>,
    gl: Rc<glow::Context>,
    raw: glow::Framebuffer,
}
impl Drop for BlitFramebuffer {
    fn drop(&mut self) {
        if let Ok(_current) = self.context.enter() {
            // SAFETY: framebuffer belongs to this worker's owned context.
            unsafe {
                self.gl.delete_framebuffer(self.raw);
            }
        }
    }
}

/// A thread-bound offscreen context shared by NativeImage readers on one worker.
#[derive(Clone)]
pub(crate) struct TextureContext {
    pub(super) context: Rc<Context>,
    pub(super) gl: Rc<glow::Context>,
    blit_fbo: Rc<BlitFramebuffer>,
    max_texture_size: u32,
    pub(super) programs:
        Rc<std::cell::RefCell<rustc_hash::FxHashMap<u64, Rc<super::gles::Compiled>>>>,
}
impl TextureContext {
    pub(super) fn blit_framebuffer(&self) -> glow::Framebuffer {
        self.blit_fbo.raw
    }
    pub(super) fn window(
        &self,
        native: ohos_native_window_binding::NativeWindow,
    ) -> Result<Window, Error> {
        let current = self.context.enter()?;
        // SAFETY: both handles belong to OHOS's default display; Window owns
        // the retained native window until after its EGL surface is destroyed.
        let surface = unsafe {
            self.context.egl.create_window_surface(
                self.context.display,
                self.context.config,
                native.raw().cast(),
                None,
            )
        }
        .map_err(|error| Error {
            operation: "eglCreateWindowSurface",
            code: error.into(),
        })?;
        drop(current);
        Ok(Window {
            context: self.context.clone(),
            surface,
            native,
            size: std::cell::Cell::new((0, 0)),
            swap_interval_zero: std::cell::Cell::new(false),
            waiting: std::cell::Cell::new(false),
        })
    }
    /// Creates an offscreen GLES context on the calling rendering worker.
    pub(super) fn new() -> Result<Self, Error> {
        let context = Rc::new(Context::new()?);
        let current = context.enter()?;
        // SAFETY: the owned context is current. EGL supplies standard driver
        // entry points for glow; native API availability remains feature-gated.
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                context
                    .egl
                    .get_proc_address(name)
                    .map_or(std::ptr::null(), |function| function as *const _)
            })
        };
        // SAFETY: the query runs in the current, owned GLES context.
        let max_texture_size = unsafe { gl.get_parameter_i32(glow::MAX_TEXTURE_SIZE) };
        let max_texture_size = u32::try_from(max_texture_size)
            .ok()
            .filter(|limit| *limit > 0)
            .ok_or(failed("GLES texture limit"))?;
        // SAFETY: persistent read-only blit FBO created in the owned GLES context.
        let blit_raw = unsafe { gl.create_framebuffer() }.map_err(|_| failed("blit FBO"))?;
        let gl = Rc::new(gl);
        let blit_fbo = Rc::new(BlitFramebuffer {
            context: context.clone(),
            gl: gl.clone(),
            raw: blit_raw,
        });
        drop(current);
        Ok(Self {
            context,
            gl,
            blit_fbo,
            max_texture_size,
            programs: Rc::default(),
        })
    }
}

/// Worker-owned output queue. Entering it preserves the caller's EGL binding.
pub(super) struct Window {
    context: Rc<Context>,
    surface: egl::Surface,
    native: ohos_native_window_binding::NativeWindow,
    size: std::cell::Cell<(u32, u32)>,
    swap_interval_zero: std::cell::Cell<bool>,
    pub(super) waiting: std::cell::Cell<bool>,
}
impl Window {
    pub(super) fn raw(&self) -> usize {
        self.native.raw() as usize
    }
    pub(super) fn invalidate_geometry(&self) {
        self.size.set((0, 0));
        self.swap_interval_zero.set(false);
    }
    pub(super) fn on_present_unavailable(&self) {
        self.swap_interval_zero.set(false);
        self.context.bind_pbuffer();
    }
    pub(super) fn enter(&self, width: u32, height: u32) -> Result<Current<'_>, Error> {
        if self.size.get() != (width, height) {
            self.native
                .set_buffer_geometry(width as i32, height as i32)
                .map_err(|_| failed("window geometry"))?;
            self.size.set((width, height));
        }
        let current = self.context.enter()?;
        if self.context.egl.get_current_surface(egl::DRAW) != Some(self.surface)
            || self.context.egl.get_current_surface(egl::READ) != Some(self.surface)
        {
            self.context
                .egl
                .make_current(
                    self.context.display,
                    Some(self.surface),
                    Some(self.surface),
                    Some(self.context.context),
                )
                .map_err(|error| {
                    self.swap_interval_zero.set(false);
                    Error {
                        operation: "eglMakeCurrent(window)",
                        code: error.into(),
                    }
                })?;
        }
        if !self.swap_interval_zero.get() {
            self.context
                .egl
                .swap_interval(self.context.display, 0)
                .map_err(|error| Error {
                    operation: "eglSwapInterval",
                    code: error.into(),
                })?;
            self.swap_interval_zero.set(true);
        }
        Ok(current)
    }
    pub(super) fn present(&self) -> Result<(), Error> {
        self.context
            .egl
            .swap_buffers(self.context.display, self.surface)
            .map_err(|error| Error {
                operation: "eglSwapBuffers",
                code: error.into(),
            })
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        self.context.bind_pbuffer();
        let _ = self
            .context
            .egl
            .destroy_surface(self.context.display, self.surface);
    }
}
struct Objects {
    context: Rc<Context>,
    gl: Rc<glow::Context>,
    texture: Option<glow::Texture>,
    target: Option<glow::Texture>,
    framebuffer: Option<glow::Framebuffer>,
    program: Option<glow::Program>,
    vertices: Option<glow::Buffer>,
}
impl Drop for Objects {
    fn drop(&mut self) {
        if self.texture.is_none() {
            return;
        }
        if let Ok(_current) = self.context.enter() {
            // SAFETY: partial construction retains each allocated object in its creating context.
            unsafe {
                if let Some(object) = self.vertices {
                    self.gl.delete_buffer(object);
                }
                if let Some(object) = self.program {
                    self.gl.delete_program(object);
                }
                if let Some(object) = self.framebuffer {
                    self.gl.delete_framebuffer(object);
                }
                if let Some(object) = self.target {
                    self.gl.delete_texture(object);
                }
                if let Some(object) = self.texture {
                    self.gl.delete_texture(object);
                }
            }
        }
    }
}
pub(super) enum PassthroughOutcome {
    Idle,
    Deferred,
    Presented { width: u32, height: u32 },
}

/// A thread-bound NativeImage reader with an offscreen GLES context.
///
/// GPU sampling resolves the producer's texture layout and transform before
/// copying tightly packed RGBA pixels. This supports GPU-only buffer layouts
/// for which a CPU NativeBuffer mapping does not expose linear image pixels.
/// Create and use this owner on a rendering worker, never a UI callback.
pub(crate) struct TextureReader {
    surface: Option<TextureImage>,
    wake: Option<FrameWake>,
    gl: Rc<glow::Context>,
    context: Rc<Context>,
    texture: glow::Texture,
    target: glow::Texture,
    framebuffer: glow::Framebuffer,
    program: glow::Program,
    transform_loc: Option<glow::UniformLocation>,
    y_scale_loc: Option<glow::UniformLocation>,
    image_loc: Option<glow::UniformLocation>,
    vertices: glow::Buffer,
    size: (u32, u32),
    allocated: (u32, u32),
    last_transform: [f32; 16],
    has_oes_frame: bool,
    target_dirty: bool,
    max_texture_size: u32,
}
impl TextureReader {
    /// Creates another consumer on the same rendering worker and GLES context.
    pub(super) fn with_context(
        gpu: &TextureContext,
        callback: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        let context = gpu.context.clone();
        let current = context.enter()?;
        let gl = gpu.gl.clone();
        let mut objects = Objects {
            context: context.clone(),
            gl: gl.clone(),
            texture: None,
            target: None,
            framebuffer: None,
            program: None,
            vertices: None,
        };
        // SAFETY: all objects belong to the current owned context. The construction
        // guard deletes every allocated object if any later step fails.
        let (
            texture,
            target,
            framebuffer,
            program,
            transform_loc,
            y_scale_loc,
            image_loc,
            vertices,
        ) = unsafe {
            let texture = gl
                .create_texture()
                .map_err(|_| failed("external texture"))?;
            objects.texture = Some(texture);
            gl.bind_texture(TEXTURE_EXTERNAL_OES, Some(texture));
            for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                gl.tex_parameter_i32(TEXTURE_EXTERNAL_OES, parameter, glow::LINEAR as i32);
            }
            for parameter in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                gl.tex_parameter_i32(TEXTURE_EXTERNAL_OES, parameter, glow::CLAMP_TO_EDGE as i32);
            }
            let target = gl
                .create_texture()
                .map_err(|_| failed("readback texture"))?;
            objects.target = Some(target);
            let framebuffer = gl
                .create_framebuffer()
                .map_err(|_| failed("readback framebuffer"))?;
            objects.framebuffer = Some(framebuffer);
            let program = gl
                .create_program()
                .map_err(|_| failed("readback program"))?;
            objects.program = Some(program);
            for (kind, source) in [
                (
                    glow::VERTEX_SHADER,
                    "attribute vec2 position; uniform mat4 transform; uniform float y_scale; varying vec2 uv; void main() { gl_Position = vec4(position, 0.0, 1.0); uv = (transform * vec4(position.x * 0.5 + 0.5, 0.5 + position.y * y_scale, 0.0, 1.0)).xy; }",
                ),
                (
                    glow::FRAGMENT_SHADER,
                    "#extension GL_OES_EGL_image_external : require\nprecision mediump float; varying vec2 uv; uniform samplerExternalOES image; void main() { gl_FragColor = texture2D(image, uv); }",
                ),
            ] {
                let shader = gl
                    .create_shader(kind)
                    .map_err(|_| failed("readback shader"))?;
                gl.shader_source(shader, source);
                gl.compile_shader(shader);
                let compiled = gl.get_shader_compile_status(shader);
                if compiled {
                    gl.attach_shader(program, shader);
                }
                gl.delete_shader(shader);
                if !compiled {
                    return Err(failed("compile readback shader"));
                }
            }
            gl.bind_attrib_location(program, 0, "position");
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(failed("link readback program"));
            }
            let transform_loc = gl.get_uniform_location(program, "transform");
            let y_scale_loc = gl.get_uniform_location(program, "y_scale");
            let image_loc = gl.get_uniform_location(program, "image");
            let vertices = gl
                .create_buffer()
                .map_err(|_| failed("readback vertices"))?;
            objects.vertices = Some(vertices);
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vertices));
            let points: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
            let bytes =
                std::slice::from_raw_parts(points.as_ptr().cast(), std::mem::size_of_val(&points));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
            (
                texture,
                target,
                framebuffer,
                program,
                transform_loc,
                y_scale_loc,
                image_loc,
                vertices,
            )
        };
        // SAFETY: this worker restores this same context for updates and Drop;
        // the image is always destroyed before its external texture.
        let mut surface = unsafe { TextureImage::new(texture.0.get(), TEXTURE_EXTERNAL_OES) }
            .map_err(native_error)?;
        surface
            .set_usage(BufferUsage::HW_RENDER | BufferUsage::HW_TEXTURE)
            .map_err(native_error)?;
        let wake =
            FrameWake::new(callback).ok_or(failed("frame notification identity exhausted"))?;
        // SAFETY: context is a never-reused integer token, never dereferenced.
        // Late native calls perform a weak lookup and return after token retirement.
        unsafe { surface.set_frame_available_listener(wake.listener()) }.map_err(native_error)?;
        objects.texture = None;
        objects.target = None;
        objects.framebuffer = None;
        objects.program = None;
        objects.vertices = None;
        drop(current);
        Ok(Self {
            surface: Some(surface),
            wake: Some(wake),
            gl,
            context,
            texture,
            target,
            framebuffer,
            program,
            transform_loc,
            y_scale_loc,
            image_loc,
            vertices,
            size: (0, 0),
            allocated: (0, 0),
            last_transform: [
                1.0, 0.0, 0.0, 0.0, //
                0.0, 1.0, 0.0, 0.0, //
                0.0, 0.0, 1.0, 0.0, //
                0.0, 0.0, 0.0, 1.0,
            ],
            has_oes_frame: false,
            target_dirty: false,
            max_texture_size: gpu.max_texture_size,
        })
    }
    /// Identity accepted by ArkUI's texture rendering options.
    pub(super) fn id(&mut self) -> Result<u64, Error> {
        self.surface
            .as_mut()
            .unwrap()
            .surface_id()
            .map_err(native_error)
    }
    /// Updates future dimensions within native signed and GLES texture limits.
    pub(super) fn resize(&mut self, width: u32, height: u32) -> Result<(), Error> {
        let (width, height, _) = super::geometry::target_size(width as f32, height as f32, 1.0);
        self.layout(width, height)?;
        self.surface
            .as_mut()
            .unwrap()
            .resize(width, height)
            .map_err(native_error)?;
        if self.size != (width, height) {
            self.size = (width, height);
            self.target_dirty = true;
        }
        Ok(())
    }
    fn layout(&self, width: u32, height: u32) -> Result<Layout, Error> {
        let layout = validate_size(width, height)?;
        if width > self.max_texture_size || height > self.max_texture_size {
            return Err(failed("GLES texture dimensions"));
        }
        Ok(layout)
    }
    fn poll_oes(&mut self) -> Result<bool, Error> {
        let _current = self.context.enter()?;
        let (width, height) = self.size;
        self.layout(width, height)?;
        let surface = self.surface.as_mut().unwrap();
        if !surface.update().map_err(native_error)? {
            return Ok(false);
        }
        self.last_transform = surface.transform().map_err(native_error)?;
        self.has_oes_frame = true;
        self.target_dirty = true;
        Ok(true)
    }
    /// Drains up to `max_frames` queued producer buffers into the OES texture
    /// without resolving an intermediate RGBA FBO until needed.
    pub(super) fn drain_oes(&mut self, max_frames: usize) -> Result<bool, Error> {
        let mut updated = false;
        for _ in 0..max_frames {
            if !self.poll_oes()? {
                break;
            }
            updated = true;
        }
        Ok(updated)
    }
    /// Draws the currently bound `TEXTURE_EXTERNAL_OES` frame into the active
    /// draw framebuffer (`y_scale = -0.5` for top-left offscreen FBOs, `+0.5`
    /// for bottom-left native windows).
    ///
    /// # Safety
    /// Caller must hold `self.context` (or a `Window` sharing `self.context`)
    /// current on this thread and have bound the destination framebuffer.
    unsafe fn draw_oes(&self, width: u32, height: u32, y_scale: f32) -> Result<(), Error> {
        // SAFETY: upheld by caller; GL objects belong to `self.context`.
        unsafe {
            let gl = &self.gl;
            gl.viewport(0, 0, width as i32, height as i32);
            gl.disable(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::DEPTH_TEST);
            gl.use_program(Some(self.program));
            gl.uniform_matrix_4_f32_slice(self.transform_loc.as_ref(), false, &self.last_transform);
            gl.uniform_1_f32(self.y_scale_loc.as_ref(), y_scale);
            gl.uniform_1_i32(self.image_loc.as_ref(), 0);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(TEXTURE_EXTERNAL_OES, Some(self.texture));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vertices));
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 0, 0);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            let code = gl.get_error();
            if code != glow::NO_ERROR {
                return Err(Error {
                    operation: "GLES texture resolve",
                    code: code as i32,
                });
            }
        }
        Ok(())
    }
    /// Ensures `self.target` holds the latest acquired OES frame as a top-left
    /// RGBA texture, resolving on demand if passthrough skipped FBO updates.
    pub(super) fn ensure_texture(&mut self) -> Result<Option<(glow::Texture, u32, u32)>, Error> {
        if !self.has_oes_frame {
            return Ok(None);
        }
        let (width, height) = self.size;
        self.layout(width, height)?;
        if !self.target_dirty && self.allocated == (width, height) {
            return Ok(Some((self.target, width, height)));
        }
        let _current = self.context.enter()?;
        // SAFETY: the context and every GL object are live on this thread.
        unsafe {
            let gl = &self.gl;
            if self.allocated != (width, height) {
                gl.bind_texture(glow::TEXTURE_2D, Some(self.target));
                for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::LINEAR as i32);
                }
                for parameter in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::CLAMP_TO_EDGE as i32);
                }
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA as i32,
                    width as i32,
                    height as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(self.target),
                0,
            );
            if self.allocated != (width, height)
                && gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE
            {
                return Err(failed("incomplete readback framebuffer"));
            }
            self.draw_oes(width, height, -0.5)?;
        }
        self.allocated = (width, height);
        self.target_dirty = false;
        Ok(Some((self.target, width, height)))
    }
    /// Presents the latest producer OES buffer directly into `window` in a
    /// single pass, bypassing intermediate RGBA FBO allocations and blits.
    pub(super) fn present_passthrough(
        &mut self,
        window: &mut Window,
        force: bool,
    ) -> Result<PassthroughOutcome, Error> {
        let updated = self.drain_oes(4)?;
        if !self.has_oes_frame || (!updated && !force) {
            return Ok(PassthroughOutcome::Idle);
        }
        let (width, height) = self.size;
        self.layout(width, height)?;
        let result = (|| -> Result<(), Error> {
            let _current = window.enter(width, height)?;
            // SAFETY: `window.enter` made the shared context current on `window`.
            unsafe {
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                self.draw_oes(width, height, 0.5)?;
            }
            window.present()
        })();
        match result {
            Ok(()) => {
                if window.waiting.replace(false) {
                    log::debug!("OHOS passthrough output resumed");
                }
                Ok(PassthroughOutcome::Presented { width, height })
            }
            Err(error) if error.output_unavailable() => {
                window.on_present_unavailable();
                let _current = self.context.enter()?;
                // SAFETY: shared GLES context is current on pbuffer.
                unsafe { while self.gl.get_error() != glow::NO_ERROR {} }
                if !window.waiting.replace(true) {
                    log::debug!("OHOS passthrough output deferred: {error}");
                }
                Ok(PassthroughOutcome::Deferred)
            }
            Err(error) => Err(error),
        }
    }
    /// Resolve a producer buffer to a top-left RGBA texture, entirely on GPU.
    pub(super) fn update_texture(&mut self) -> Result<bool, Error> {
        if !self.poll_oes()? {
            return Ok(false);
        }
        self.ensure_texture()?;
        Ok(true)
    }
    pub(super) fn texture(&self) -> Option<(glow::Texture, u32, u32)> {
        (!self.target_dirty && self.allocated != (0, 0)).then_some((
            self.target,
            self.allocated.0,
            self.allocated.1,
        ))
    }
    /// CPU consumers (CSS composition) read only after the GPU resolve.
    pub(super) fn read(&mut self) -> Result<Option<RgbaFrame>, Error> {
        if !self.update_texture()? {
            return Ok(None);
        }
        self.read_texture(self.texture().unwrap()).map(Some)
    }
    pub(super) fn read_texture(&self, picture: super::gles::Picture) -> Result<RgbaFrame, Error> {
        let _current = self.context.enter()?;
        let (_, width, height) = picture;
        let mut pixels = self.layout(width, height)?.allocate()?;
        // SAFETY: the framebuffer and exact-size RGBA storage belong to this
        // current context; the resolved texture is attached by update_texture.
        unsafe {
            let gl = &self.gl;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(picture.0),
                0,
            );
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.read_pixels(
                0,
                0,
                width as i32,
                height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
            let code = gl.get_error();
            if code != glow::NO_ERROR {
                return Err(Error {
                    operation: "GLES readback",
                    code: code as i32,
                });
            }
        }
        Ok(RgbaFrame {
            width,
            height,
            pixels,
        })
    }
}
impl Drop for TextureReader {
    fn drop(&mut self) {
        // Retire delivery before native teardown; late native callbacks only miss lookup.
        self.wake.take();
        if let Ok(_current) = self.context.enter() {
            self.surface.take();
            // SAFETY: native image has retired before its texture; all objects
            // are deleted on their creating thread in their current context.
            unsafe {
                self.gl.delete_buffer(self.vertices);
                self.gl.delete_program(self.program);
                self.gl.delete_framebuffer(self.framebuffer);
                self.gl.delete_texture(self.target);
                self.gl.delete_texture(self.texture);
            }
        } else if let Some(surface) = self.surface.take() {
            log::warn!(
                "OHOS readback cleanup could not enter EGL context; retaining native image and GL owners"
            );
            // The SDK image belongs to its creating EGL context. If entering
            // fails, retain that complete owner rather than destroy it in an
            // unrelated context. Retire Rust delivery before quarantining it.
            std::mem::forget((surface, self.context.clone(), self.gl.clone()));
        }
    }
}

#[derive(Debug)]
pub(crate) struct Error {
    operation: &'static str,
    code: i32,
}
impl Error {
    pub(super) fn output_unavailable(&self) -> bool {
        matches!(self.operation, "eglMakeCurrent(window)" | "eglSwapBuffers")
            && matches!(
                self.code,
                egl::BAD_ALLOC | egl::BAD_SURFACE | egl::BAD_NATIVE_WINDOW
            )
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed ({})", self.operation, self.code)
    }
}
impl std::error::Error for Error {}
fn native_error(error: ohos_native_image_binding::Error) -> Error {
    Error {
        operation: "NativeImage",
        code: error.code(),
    }
}
pub(crate) struct RgbaFrame {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}
mod layout {
    use super::{Error, failed};
    pub(super) struct Layout {
        byte_len: usize,
    }
    pub(super) fn validate_size(width: u32, height: u32) -> Result<Layout, Error> {
        if width == 0
            || height == 0
            || u64::from(width) * u64::from(height) > 16_777_216
            || width > i32::MAX as u32
            || height > i32::MAX as u32
        {
            return Err(failed("NSH layer pixel budget"));
        }
        let row_bytes = (width as usize).checked_mul(4).ok_or(failed("RGBA row"))?;
        let byte_len = row_bytes
            .checked_mul(height as usize)
            .filter(|n| *n <= isize::MAX as usize)
            .ok_or(failed("RGBA bytes"))?;
        Ok(Layout { byte_len })
    }
    impl Layout {
        pub(super) fn allocate(&self) -> Result<Vec<u8>, Error> {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(self.byte_len)
                .map_err(|_| failed("RGBA allocation"))?;
            bytes.resize(self.byte_len, 0);
            Ok(bytes)
        }
    }
}
