//! GPU-resident effect inputs and targets on the NativeImage consumer's context.
//! Coordinates in pictures start at the top left; only presentation flips rows.

mod filters;
use std::{
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) use filters::Filters;
use glow::HasContext;
use nsh_core::{
    NodeId,
    animation::AnimationValue,
    geometry::Points,
    shader::{ShaderInput, ShaderLayer, ShaderRole, SlotKind, builtins},
};

use super::{
    geometry::{Coverage, target_size},
    readback::{RgbaFrame, TextureContext, Window},
};

pub(super) type Picture = (glow::Texture, u32, u32);
type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("{0}")]
    Configuration(&'static str),
    #[error(transparent)]
    Native(#[from] super::readback::Error),
    #[error("GLES: {0}")]
    Driver(String),
    #[error("shader compilation: {0}")]
    Shader(String),
    #[error("shader draw GL error {0:#x}")]
    Draw(u32),
}
impl Error {
    pub(super) fn is_retryable(&self) -> bool {
        !matches!(self, Self::Shader(_) | Self::Configuration(_))
    }
}
impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::Driver(message)
    }
}
impl From<&'static str> for Error {
    fn from(message: &'static str) -> Self {
        Self::Configuration(message)
    }
}
impl From<Error> for String {
    fn from(error: Error) -> Self {
        error.to_string()
    }
}

pub(super) struct Target {
    gpu: TextureContext,
    texture: glow::Texture,
    framebuffer: glow::Framebuffer,
    width: u32,
    height: u32,
}

impl Target {
    pub(super) fn new(gpu: &TextureContext, width: u32, height: u32) -> Result<Self> {
        let (width, height, _) = target_size(width as f32, height as f32, 1.0);
        if width == 0 || height == 0 {
            return Err("shader target dimensions exceed the pixel budget".into());
        }
        let _current = gpu.context.enter()?;
        let gl = &gpu.gl;
        // SAFETY: all objects and calls are confined to this current worker context.
        unsafe {
            let texture = gl.create_texture()?;
            let framebuffer = match gl.create_framebuffer() {
                Ok(framebuffer) => framebuffer,
                Err(error) => {
                    gl.delete_texture(texture);
                    return Err(error.into());
                }
            };
            let target = Self {
                gpu: gpu.clone(),
                texture,
                framebuffer,
                width,
                height,
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::LINEAR as i32);
            }
            for parameter in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::CLAMP_TO_EDGE as i32);
            }
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                return Err("incomplete shader framebuffer".into());
            }
            Ok(target)
        }
    }
    pub(super) fn picture(&self) -> Picture {
        (self.texture, self.width, self.height)
    }
    pub(super) fn upload(gpu: &TextureContext, frame: &RgbaFrame) -> Result<Self> {
        if frame.pixels.len() != frame.width as usize * frame.height as usize * 4 {
            return Err("invalid shader picture storage".into());
        }
        if frame.width > 4096 || frame.height > 4096 {
            return Err("shader upload must be resampled before allocation".into());
        }
        let target = Self::new(gpu, frame.width, frame.height)?;
        let _current = gpu.context.enter()?;
        // SAFETY: the live texture's exact-size RGBA source stays borrowed during upload.
        unsafe {
            gpu.gl.bind_texture(glow::TEXTURE_2D, Some(target.texture));
            gpu.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gpu.gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                frame.width as i32,
                frame.height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&frame.pixels)),
            );
        }
        Ok(target)
    }
    pub(super) fn copy(gpu: &TextureContext, picture: Picture) -> Result<Self> {
        let target = Self::new(gpu, picture.1, picture.2)?;
        target.blit(picture, false)?;
        Ok(target)
    }
    fn blit(&self, picture: Picture, flip: bool) -> Result<()> {
        self.blit_rect(picture, flip, [0, 0, self.width as i32, self.height as i32])
    }
    pub(super) fn reframe(
        &self,
        picture: Picture,
        source: Coverage,
        destination: Coverage,
    ) -> Result<()> {
        self.blit_rect(
            picture,
            false,
            destination.destination(source, (self.width, self.height)),
        )
    }
    fn blit_rect(&self, picture: Picture, flip: bool, rect: [i32; 4]) -> Result<()> {
        let _current = self.gpu.context.enter()?;
        let gl = &self.gpu.gl;
        // SAFETY: source and destination textures are owned on this context and
        // never alias; the temporary read framebuffer is detached before deletion.
        unsafe {
            let read = gl.create_framebuffer()?;
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(read));
            gl.framebuffer_texture_2d(
                glow::READ_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(picture.0),
                0,
            );
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.framebuffer));
            gl.disable(glow::SCISSOR_TEST);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            let (y0, y1) = if flip {
                (picture.2 as i32, 0)
            } else {
                (0, picture.2 as i32)
            };
            gl.blit_framebuffer(
                0,
                y0,
                picture.1 as i32,
                y1,
                rect[0],
                rect[1],
                rect[2],
                rect[3],
                glow::COLOR_BUFFER_BIT,
                glow::LINEAR,
            );
            gl.delete_framebuffer(read);
        }
        Ok(())
    }
    pub(super) fn present(&self, window: &mut Window) -> Result<bool> {
        let result = (|| -> Result<()> {
            let _current = window.enter(self.width, self.height)?;
            // SAFETY: this native window and the source target use the same EGL context.
            unsafe {
                let gl = &self.gpu.gl;
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(self.framebuffer));
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
                gl.disable(glow::SCISSOR_TEST);
                gl.blit_framebuffer(
                    0,
                    self.height as i32,
                    self.width as i32,
                    0,
                    0,
                    0,
                    self.width as i32,
                    self.height as i32,
                    glow::COLOR_BUFFER_BIT,
                    glow::NEAREST,
                );
            }
            window.present().map_err(Error::Native)
        })();
        match result {
            Ok(()) => {
                if window.waiting.replace(false) {
                    log::debug!("OHOS shader output resumed");
                }
                Ok(true)
            }
            Err(Error::Native(error)) if error.output_unavailable() => {
                // ArkUI can detach a Surface's consumer while rebuilding an export
                // tree. Retry on the next VSync/created window; the logical shader
                // remains visible. Clear only the failed window operation's GL
                // errors so another layer does not inherit its OUT_OF_MEMORY flag.
                let _current = self.gpu.context.enter()?;
                // SAFETY: the surface's shared GLES context is current here.
                unsafe { while self.gpu.gl.get_error() != glow::NO_ERROR {} }
                if !window.waiting.replace(true) {
                    log::debug!("OHOS shader output deferred: {error}");
                }
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
}
impl Drop for Target {
    fn drop(&mut self) {
        if let Ok(_current) = self.gpu.context.enter() {
            // SAFETY: these objects belong to the current, owned worker context.
            unsafe {
                self.gpu.gl.delete_framebuffer(self.framebuffer);
                self.gpu.gl.delete_texture(self.texture);
            }
        }
    }
}

pub(super) struct Compiled {
    context: Rc<super::readback::Context>,
    gl: Rc<glow::Context>,
    native: glow::Program,
    pictures: Vec<(glow::UniformLocation, ShaderInput)>,
}
impl Drop for Compiled {
    fn drop(&mut self) {
        if let Ok(_current) = self.context.enter() {
            // SAFETY: the cache retains its creating context through deletion.
            unsafe {
                self.gl.delete_program(self.native);
            }
        }
    }
}
struct Program {
    gpu: TextureContext,
    compiled: Rc<Compiled>,
    buffers: [glow::Buffer; 2],
    empty: Target,
}
pub(super) fn precompile(gpu: &TextureContext, layer: &ShaderLayer) -> Result<()> {
    Program::compile(gpu, layer).map(|_| ())
}

impl Program {
    fn compile(gpu: &TextureContext, layer: &ShaderLayer) -> Result<Rc<Compiled>> {
        let _current = gpu.context.enter()?;
        let gl = &gpu.gl;
        // SAFETY: validated core-generated GLSL and all program objects are
        // compiled and used exclusively on this worker's current context.
        unsafe {
            if let Some(cached) = gpu.programs.borrow().get(&layer.shader.hash()).cloned() {
                return Ok(cached);
            }
            let source = layer
                .shader
                .glsl_es()
                .map_err(|e| Error::Shader(e.to_string()))?;
            let native = gl.create_program()?;
            let compile = || -> Result<()> {
                for (kind, source) in [
                    (glow::VERTEX_SHADER, &source.vertex),
                    (glow::FRAGMENT_SHADER, &source.fragment),
                ] {
                    let shader = gl.create_shader(kind)?;
                    gl.shader_source(shader, source);
                    gl.compile_shader(shader);
                    if !gl.get_shader_compile_status(shader) {
                        let error = gl.get_shader_info_log(shader);
                        gl.delete_shader(shader);
                        return Err(Error::Shader(error));
                    }
                    gl.attach_shader(native, shader);
                    gl.delete_shader(shader);
                }
                gl.link_program(native);
                if !gl.get_program_link_status(native) {
                    return Err(Error::Shader(gl.get_program_info_log(native)));
                }
                Ok(())
            };
            if let Err(error) = compile() {
                gl.delete_program(native);
                return Err(error);
            }
            for (binding, name) in [
                source.builtins_block.as_deref(),
                source.uniforms_block.as_deref(),
            ]
            .into_iter()
            .enumerate()
            {
                if let Some(name) = name
                    && let Some(index) = gl.get_uniform_block_index(native, name)
                {
                    gl.uniform_block_binding(native, index, binding as u32);
                }
            }
            let pictures = source
                .pictures
                .iter()
                .filter_map(|(name, input)| {
                    gl.get_uniform_location(native, name)
                        .map(|location| (location, *input))
                })
                .collect();
            let compiled = Rc::new(Compiled {
                context: gpu.context.clone(),
                gl: gpu.gl.clone(),
                native,
                pictures,
            });
            gpu.programs
                .borrow_mut()
                .insert(layer.shader.hash(), compiled.clone());
            Ok(compiled)
        }
    }

    fn new(gpu: &TextureContext, layer: &ShaderLayer) -> Result<Self> {
        let empty = Target::upload(
            gpu,
            &RgbaFrame {
                width: 1,
                height: 1,
                pixels: vec![0; 4],
            },
        )?;
        let compiled = Self::compile(gpu, layer)?;
        let _current = gpu.context.enter()?;
        let gl = &gpu.gl;
        // SAFETY: buffer objects belong exclusively to this worker's current context.
        unsafe {
            let first = match gl.create_buffer() {
                Ok(b) => b,
                Err(e) => {
                    return Err(e.into());
                }
            };
            let second = match gl.create_buffer() {
                Ok(b) => b,
                Err(e) => {
                    gl.delete_buffer(first);
                    return Err(e.into());
                }
            };
            Ok(Self {
                gpu: gpu.clone(),
                compiled,
                buffers: [first, second],
                empty,
            })
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn draw(
        &self,
        target: &Target,
        words: &[u32],
        builtins: &[u32; 8],
        content: Picture,
        from: Option<Picture>,
        to: Option<Picture>,
        backdrop: Option<Picture>,
    ) -> Result<()> {
        let _current = self.gpu.context.enter()?;
        let gl = &self.gpu.gl;
        // SAFETY: both UBO slices stay borrowed for upload; all target, sampler
        // and program objects belong to this current context. Draw uses no VBO.
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target.framebuffer));
            gl.viewport(0, 0, target.width as i32, target.height as i32);
            gl.disable(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::DEPTH_TEST);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.use_program(Some(self.compiled.native));
            for (binding, data) in [builtins.as_slice(), words].into_iter().enumerate() {
                let bytes =
                    std::slice::from_raw_parts(data.as_ptr().cast(), std::mem::size_of_val(data));
                gl.bind_buffer(glow::UNIFORM_BUFFER, Some(self.buffers[binding]));
                gl.buffer_data_u8_slice(glow::UNIFORM_BUFFER, bytes, glow::DYNAMIC_DRAW);
                gl.bind_buffer_base(
                    glow::UNIFORM_BUFFER,
                    binding as u32,
                    Some(self.buffers[binding]),
                );
            }
            for (index, (location, slot)) in self.compiled.pictures.iter().enumerate() {
                let picture = match *slot {
                    ShaderInput::Content => content,
                    ShaderInput::Backdrop => backdrop.unwrap_or_else(|| self.empty.picture()),
                    ShaderInput::From => from.unwrap_or_else(|| self.empty.picture()),
                    ShaderInput::To => to.unwrap_or_else(|| self.empty.picture()),
                    _ => self.empty.picture(),
                };
                gl.active_texture(glow::TEXTURE0 + index as u32);
                gl.bind_texture(glow::TEXTURE_2D, Some(picture.0));
                gl.uniform_1_i32(Some(location), index as i32);
            }
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            let error = gl.get_error();
            if error != glow::NO_ERROR {
                return Err(Error::Draw(error));
            }
        }
        Ok(())
    }
}
impl Drop for Program {
    fn drop(&mut self) {
        if let Ok(_current) = self.gpu.context.enter() {
            // SAFETY: deletion is confined to the owning context and worker.
            unsafe {
                for buffer in self.buffers {
                    self.gpu.gl.delete_buffer(buffer);
                }
            }
        }
    }
}

struct Effect {
    id: NodeId,
    layer: Arc<ShaderLayer>,
    program: Program,
    words: Vec<u32>,
    progress: f32,
    clock: Instant,
    elapsed: Duration,
    dirty: bool,
    transition: Option<(Instant, Duration, bool)>,
    pending: Option<(Duration, bool)>,
    navigation: bool,
    reverse_inputs: bool,
    from: Option<Target>,
    from_coverage: Option<Coverage>,
    coverage: Option<Coverage>,
    aligned: Option<Target>,
    backdrop: Option<Target>,
    input: Option<Target>,
    scratch: Option<Target>,
    output: Option<Target>,
}

pub(super) struct Effects {
    gpu: TextureContext,
    views: Vec<Effect>,
    output: Option<Target>,
    shown: Option<usize>,
    direct_output: bool,
}
impl Effects {
    pub(super) fn new(gpu: &TextureContext) -> Self {
        Self {
            gpu: gpu.clone(),
            views: Vec::new(),
            output: None,
            shown: None,
            direct_output: false,
        }
    }
    pub(super) fn layers(&mut self, layers: &[(NodeId, Arc<ShaderLayer>)]) -> Result<()> {
        self.views
            .retain(|view| layers.iter().any(|(id, _)| *id == view.id));
        if self.shown.is_some_and(|index| index >= self.views.len()) {
            self.direct_output = false;
        }
        for (index, (id, layer)) in layers.iter().enumerate() {
            if let Some(at) = self.views.iter().position(|view| view.id == *id) {
                self.views.swap(index, at);
                let view = &mut self.views[index];
                if view.layer != *layer {
                    let same = view.layer.shader == layer.shader && view.layer.slots == layer.slots;
                    if !same {
                        view.program = Program::new(&self.gpu, layer)?;
                    }
                    let mut words = layer.uniforms.to_vec();
                    if same {
                        for slot in &layer.slots {
                            let count = match slot.kind {
                                SlotKind::Rgb => 3,
                                SlotKind::Rgba => 4,
                                _ => 1,
                            };
                            let at = slot.word as usize;
                            if let (Some(dst), Some(src)) = (
                                words.get_mut(at..at + count),
                                view.words.get(at..at + count),
                            ) {
                                dst.copy_from_slice(src);
                            }
                        }
                    }
                    if !view.layer.paused {
                        view.elapsed += view.clock.elapsed();
                    }
                    view.clock = Instant::now();
                    view.layer = layer.clone();
                    view.words = words;
                    if !same || layer.progress_slot().is_none() {
                        view.progress = layer.progress.0;
                    }
                    view.dirty = true;
                }
            } else {
                self.views.insert(
                    index,
                    Effect {
                        id: *id,
                        layer: layer.clone(),
                        program: Program::new(&self.gpu, layer)?,
                        words: layer.uniforms.to_vec(),
                        progress: layer.progress.0,
                        clock: Instant::now(),
                        elapsed: Duration::ZERO,
                        dirty: true,
                        transition: None,
                        pending: None,
                        navigation: false,
                        reverse_inputs: false,
                        from: None,
                        from_coverage: None,
                        coverage: None,
                        aligned: None,
                        backdrop: None,
                        input: None,
                        scratch: None,
                        output: None,
                    },
                );
            }
        }
        Ok(())
    }
    pub(super) fn slot(&mut self, id: NodeId, slot: u16, value: AnimationValue) {
        if let Some(view) = self.views.iter_mut().find(|v| v.id == id) {
            if view.layer.progress_slot() == Some(slot) {
                if let AnimationValue::Number(value) = value {
                    view.progress = value;
                }
            } else {
                view.layer.write_slot(&mut view.words, slot, value);
            }
            view.dirty = true;
        }
    }
    pub(super) fn transition(&mut self, id: NodeId, duration: Duration) -> Result<()> {
        if let Some(view) = self.views.iter_mut().find(|v| v.id == id) {
            let source = if view.transition.is_some() {
                view.output.as_ref()
            } else {
                view.input.as_ref()
            };
            if let Some(source) = source {
                view.from = Some(Target::copy(&self.gpu, source.picture())?);
                view.from_coverage = view.coverage;
            }
            view.pending = Some((duration, false));
            view.dirty = true;
        }
        Ok(())
    }
    pub(super) fn navigation(
        &mut self,
        id: NodeId,
        frame: Target,
        coverage: Coverage,
        duration: Duration,
        backwards: bool,
        reverse_inputs: bool,
    ) -> Result<()> {
        if let Some(view) = self.views.iter_mut().find(|v| v.id == id) {
            view.from = Some(frame);
            view.from_coverage = Some(coverage);
            view.navigation = true;
            view.reverse_inputs = reverse_inputs;
            view.pending = Some((duration, backwards));
            view.dirty = true;
        }
        Ok(())
    }
    pub(super) fn navigation_running(&self, id: NodeId) -> bool {
        self.views
            .iter()
            .find(|view| view.id == id)
            .is_some_and(|view| view.pending.is_some() || view.transition.is_some())
    }
    pub(super) fn wants_frame(&self) -> bool {
        self.views.iter().any(|v| {
            v.dirty
                || v.pending.is_some()
                || v.transition.is_some()
                || v.layer.shader.animated() && !v.layer.paused
        })
    }
    pub(super) fn render(
        &mut self,
        source: Picture,
        changed: bool,
        scale: f32,
        coverage: Coverage,
        backdrop: Option<Picture>,
    ) -> Result<bool> {
        self.render_size(
            source,
            (source.1, source.2),
            changed,
            scale,
            coverage,
            backdrop,
        )
    }
    pub(super) fn render_fill(
        &mut self,
        size: (u32, u32),
        scale: f32,
        changed: bool,
        box_size: nsh_core::Size<Points>,
    ) -> Result<bool> {
        let Some(view) = self.views.first() else {
            return Ok(false);
        };
        let coverage = Coverage::shader(box_size, view.layer.overflow.0);
        self.render_size(
            view.program.empty.picture(),
            size,
            changed,
            scale,
            coverage,
            None,
        )
    }
    fn render_size(
        &mut self,
        source: Picture,
        size: (u32, u32),
        changed: bool,
        scale: f32,
        coverage: Coverage,
        backdrop: Option<Picture>,
    ) -> Result<bool> {
        let mut picture = source;
        let mut current_coverage = coverage;
        let mut changed = changed;
        let mut shown = None;
        for (index, view) in self.views.iter_mut().enumerate() {
            let layer_coverage = Coverage::shader(coverage.box_size, view.layer.overflow.0);
            let (width, height, effective_scale) = layer_coverage.pixels(scale);
            let layer_size = (width.max(1), height.max(1));
            let align = view.layer.role != ShaderRole::Fill
                && (current_coverage != layer_coverage || (picture.1, picture.2) != layer_size);
            let current = if align {
                if view
                    .aligned
                    .as_ref()
                    .is_none_or(|target| (target.width, target.height) != layer_size)
                {
                    view.aligned = Some(Target::new(&self.gpu, layer_size.0, layer_size.1)?);
                }
                let target = view.aligned.as_ref().unwrap();
                target.reframe(picture, current_coverage, layer_coverage)?;
                target.picture()
            } else {
                picture
            };
            if view.from.is_none() && (view.pending.is_some() || view.transition.is_some()) {
                view.from = Some(Target::copy(&self.gpu, current)?);
                view.from_coverage = Some(layer_coverage);
            }
            if let (Some(from), Some(from_coverage)) = (&view.from, view.from_coverage)
                && from_coverage != layer_coverage
            {
                let target = Target::new(&self.gpu, layer_size.0, layer_size.1)?;
                target.reframe(from.picture(), from_coverage, layer_coverage)?;
                view.from = Some(target);
                view.from_coverage = Some(layer_coverage);
            }
            let now = Instant::now();
            if let Some((duration, backwards)) = view.pending.take() {
                view.transition = Some((now, duration, backwards));
            }
            let progress = view
                .transition
                .map_or(view.progress, |(start, duration, backwards)| {
                    let p = (now.saturating_duration_since(start).as_secs_f32()
                        / duration.as_secs_f32().max(f32::MIN_POSITIVE))
                    .clamp(0.0, 1.0);
                    if backwards { 1.0 - p } else { p }
                });
            let transitioning = view.transition.is_some();
            let replaces = matches!(
                view.layer.role,
                ShaderRole::Fill | ShaderRole::Effect | ShaderRole::Screen
            ) || transitioning
                || view.navigation;
            if changed
                && !view.navigation
                && matches!(
                    view.layer.role,
                    ShaderRole::Transition | ShaderRole::ScreenTransition
                )
            {
                if view
                    .input
                    .as_ref()
                    .is_none_or(|target| (target.width, target.height) != layer_size)
                {
                    view.input = Some(Target::new(&self.gpu, layer_size.0, layer_size.1)?);
                }
                view.input.as_ref().unwrap().blit(current, false)?;
            }
            if replaces
                && (changed
                    || view.dirty
                    || transitioning
                    || view.layer.shader.animated() && !view.layer.paused)
            {
                for target in [&mut view.scratch, &mut view.output] {
                    if target
                        .as_ref()
                        .is_none_or(|t| (t.width, t.height) != layer_size)
                    {
                        *target = Some(Target::new(&self.gpu, layer_size.0, layer_size.1)?);
                    }
                }
                let time = view.elapsed
                    + if view.layer.paused {
                        Duration::ZERO
                    } else {
                        view.clock.elapsed()
                    };
                let scratch = view.scratch.as_ref().unwrap();
                if let Some(backdrop) = backdrop {
                    if view
                        .backdrop
                        .as_ref()
                        .is_none_or(|target| (target.width, target.height) != layer_size)
                    {
                        view.backdrop = Some(Target::new(&self.gpu, layer_size.0, layer_size.1)?);
                    }
                    view.backdrop
                        .as_ref()
                        .unwrap()
                        .reframe(backdrop, coverage, layer_coverage)?;
                }
                let in_transition = transitioning || view.navigation;
                let (from_pic, to_pic) = if !in_transition {
                    (None, None)
                } else if view.reverse_inputs {
                    (Some(current), view.from.as_ref().map(Target::picture))
                } else {
                    (view.from.as_ref().map(Target::picture), Some(current))
                };
                view.program.draw(
                    scratch,
                    &view.words,
                    &builtins(
                        coverage.box_size,
                        time.as_secs_f32(),
                        effective_scale,
                        nsh_core::geometry::Percentage(progress),
                        Points(view.layer.overflow.0.max(0.0)),
                    ),
                    current,
                    from_pic,
                    to_pic,
                    backdrop.and_then(|_| view.backdrop.as_ref().map(Target::picture)),
                )?;
                view.output
                    .as_ref()
                    .unwrap()
                    .blit(scratch.picture(), true)?;
                changed = true;
            }
            if replaces && let Some(output) = &view.output {
                picture = output.picture();
                current_coverage = layer_coverage;
                shown = Some(index);
            }
            if view.transition.is_some_and(|(start, duration, _)| {
                now.saturating_duration_since(start) >= duration
            }) {
                view.progress = progress;
                view.transition = None;
                if !view.navigation {
                    view.from = None;
                }
            }
            view.coverage = Some(layer_coverage);
            view.dirty = false;
        }
        changed |= self.shown != shown;
        self.shown = shown;
        self.direct_output =
            shown.is_some() && current_coverage == coverage && (picture.1, picture.2) == size;
        if self.direct_output {
            return Ok(changed);
        }
        if changed || self.output.is_none() {
            if self
                .output
                .as_ref()
                .is_none_or(|t| (t.width, t.height) != size)
            {
                self.output = Some(Target::new(&self.gpu, size.0, size.1)?);
            }
            self.output
                .as_ref()
                .unwrap()
                .reframe(picture, current_coverage, coverage)?;
            return Ok(true);
        }
        Ok(false)
    }
    pub(super) fn output(&self) -> Option<&Target> {
        if self.direct_output {
            self.shown
                .and_then(|index| self.views.get(index))
                .and_then(|view| view.output.as_ref())
        } else {
            self.output.as_ref()
        }
    }
}
