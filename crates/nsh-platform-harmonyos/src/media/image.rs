//! Image IO stays off the UI thread. Native ArkUI decodes and presents validated
//! encoded data; all HTTP redirects pass the shared Rust capability policy.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak as UiWeak},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use nsh_core::{
    EventKind, EventPayload, ImageData, ImageOptions, ImageSource, PayloadValue, ResizeMode,
    image::{
        DiskCache, Lease, MemoryCache, budget,
        loader::{self, Context, Location, Priority, Ticket},
    },
};
use nsh_engine::{Access, Assets};
use ohos_arkui_binding::{
    component::{
        attribute::{ArkUICommonAttribute, ArkUIEvent},
        built_in_component::Image,
    },
    types::{
        advanced::LengthMetricUnit, attribute::ArkUINodeAttributeType as A,
        event::NodeEventType as E,
    },
};
use rustc_hash::FxHashMap;

use crate::{
    app::runtime::Dispatcher,
    render::{
        geometry::Size,
        node::{NativeNode, Node, Result, callback},
    },
};

const MAX_DATA: usize = 16 << 20;

#[derive(Clone, Copy)]
pub(crate) enum Target {
    View {
        id: u32,
        generation: u64,
        placeholder: bool,
    },
    Info(u32),
    Canvas {
        id: u32,
        generation: u64,
        index: usize,
    },
}
pub(crate) struct Ready {
    pub(crate) target: Target,
    pub(crate) data: std::result::Result<Arc<Data>, String>,
}
pub(crate) struct Progress {
    pub(crate) target: Target,
    pub(crate) loaded: u64,
    pub(crate) total: u64,
}
pub(crate) struct Decoded {
    bytes: Arc<[u8]>,
    svg: Option<nsh_core::svg::Raster>,
    width: u32,
    height: u32,
}
pub(crate) struct Data {
    decoded: Arc<Decoded>,
    _lease: Lease,
}
impl std::ops::Deref for Data {
    type Target = Decoded;
    fn deref(&self) -> &Decoded {
        &self.decoded
    }
}
struct Loading {
    request: crate::media::request::Request<Target>,
    ticket: Mutex<Option<Ticket>>,
}
impl std::ops::Deref for Loading {
    type Target = crate::media::request::Request<Target>;
    fn deref(&self) -> &Self::Target {
        &self.request
    }
}
pub(crate) struct Loader {
    context: Mutex<Context>,
    dispatcher: Weak<Dispatcher>,
    cache: MemoryCache<String, Arc<Decoded>>,
    pending: Mutex<FxHashMap<String, Arc<Loading>>>,
}
impl Data {
    pub(crate) fn decoded_bytes(&self) -> usize {
        if let Some(svg) = &self.svg {
            svg.pixels.len()
        } else {
            self.width as usize * self.height as usize * 4
        }
    }
    pub(crate) fn bitmap(&self) -> std::result::Result<ohos_drawing_binding::Bitmap, String> {
        if let Some(svg) = &self.svg {
            let mut bitmap = ohos_drawing_binding::Bitmap::new(
                svg.width,
                svg.height,
                ohos_drawing_binding::BitmapFormat::default(),
            );
            let output = bitmap.pixels_mut();
            if output.len() != svg.pixels.len() {
                return Err("image bitmap allocation failed".into());
            }
            output.copy_from_slice(&svg.pixels);
            return Ok(bitmap);
        }
        use ohos_image_native_binding::{DecodingOptions, ImageSource, types::PixelFormat};
        let source = ImageSource::create_from_data(&self.bytes).map_err(|e| e.to_string())?;
        let mut options = DecodingOptions::new().map_err(|e| e.to_string())?;
        options
            .set_pixel_format(PixelFormat::Rgba8888)
            .map_err(|e| e.to_string())?;
        let pixels = source
            .create_pixelmap(&mut options)
            .map_err(|e| e.to_string())?;
        let mut bitmap = ohos_drawing_binding::Bitmap::new(
            self.width,
            self.height,
            ohos_drawing_binding::BitmapFormat::default(),
        );
        let output = bitmap.pixels_mut();
        if output.len() != self.width as usize * self.height as usize * 4 {
            return Err("image bitmap allocation failed".into());
        }
        let read = pixels.read_pixels(output).map_err(|e| e.to_string())?;
        if read != output.len() {
            return Err("image decoder returned incomplete pixels".into());
        }
        Ok(bitmap)
    }
}
impl Loader {
    pub(crate) fn new(
        assets: Arc<dyn Assets>,
        access: Access,
        dispatcher: Weak<Dispatcher>,
    ) -> Arc<Self> {
        let access = Arc::new(access);
        Arc::new(Self {
            context: Mutex::new(Context {
                read_asset: Some(Arc::new(move |uri| assets.read(uri))),
                access,
                transport: Some(transport()),
                ..Default::default()
            }),
            dispatcher,
            cache: MemoryCache::new(budget()),
            pending: Mutex::default(),
        })
    }
    pub(crate) fn set_disk_cache(&self, cache: Option<DiskCache>) {
        self.context.lock().expect("image context").disk = cache;
    }
    pub(crate) fn load(
        self: &Arc<Self>,
        target: Target,
        uri: String,
        inline: Option<ImageData>,
    ) -> Arc<AtomicBool> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = {
            let mut pending = self.pending.lock().expect("pending images");
            if let Some(request) = pending.get(&uri) {
                match request.join(target, &cancelled) {
                    Ok(true) => return cancelled,
                    Ok(false) => {}
                    Err(error) => {
                        if let Some(dispatcher) = self.dispatcher.upgrade() {
                            dispatcher.image(Ready {
                                target,
                                data: Err(error.into()),
                            });
                        }
                        return cancelled;
                    }
                }
            }
            let request = Arc::new(Loading {
                request: crate::media::request::Request::new(target, cancelled.clone()),
                ticket: Mutex::default(),
            });
            pending.insert(uri.clone(), request.clone());
            request
        };
        let context = self.context.lock().expect("image context").clone();
        if inline.is_none() && !context.access.allows_media(&uri) {
            self.finish(
                &uri,
                &request,
                Err("image URL is not granted by capabilities.network".into()),
            );
            return cancelled;
        }
        if let Some((decoded, lease)) = self.cache.get(&uri) {
            self.finish(
                &uri,
                &request,
                Ok(Arc::new(Data {
                    decoded,
                    _lease: lease,
                })),
            );
            return cancelled;
        }
        let location = inline
            .map(|bytes| Location::Memory(uri.clone().into(), bytes))
            .or_else(|| context.resolve(&uri));
        let Some(location) = location else {
            self.finish(&uri, &request, Err("unsupported image URI".into()));
            return cancelled;
        };
        let decode_uri = uri.clone();
        let progress_loader = self.clone();
        let progress_request = request.clone();
        let progress = Arc::new(move |loaded, total| {
            if progress_request.cancelled() {
                progress_request.ticket.lock().expect("image ticket").take();
                // Also retire the platform's waiters: a cancelled shared read has no completion.
                progress_loader.finish(&uri, &progress_request, Err("cancelled".into()));
                return;
            }
            if let Some(dispatcher) = progress_loader.dispatcher.upgrade() {
                for waiter in progress_request
                    .waiters
                    .lock()
                    .expect("image waiters")
                    .iter()
                {
                    if !waiter.cancelled.load(Ordering::Relaxed) {
                        dispatcher.image_progress(Progress {
                            target: waiter.target,
                            loaded,
                            total,
                        });
                    }
                }
            }
        });
        let decode_loader = self.clone();
        let decode_request = request.clone();
        let uri = decode_uri;
        let ticket = loader::loader().read(
            location,
            &context,
            Priority::Normal,
            Some(progress),
            Box::new(move |bytes| {
                loader::loader()
                    .decode(Priority::Normal, move || {
                        let result = bytes
                            .and_then(|bytes| decode_loader.decode(&uri, bytes, &decode_request));
                        decode_loader.finish(&uri, &decode_request, result);
                    })
                    .detach();
            }),
        );
        *request.ticket.lock().expect("image ticket") = Some(ticket);
        cancelled
    }
    fn finish(
        &self,
        uri: &str,
        request: &Arc<Loading>,
        data: std::result::Result<Arc<Data>, String>,
    ) {
        let mut pending = self.pending.lock().expect("pending images");
        if pending
            .get(uri)
            .is_some_and(|current| Arc::ptr_eq(current, request))
        {
            pending.remove(uri);
        }
        let waiters = std::mem::take(&mut *request.waiters.lock().expect("image waiters"));
        drop(pending);
        if let Some(dispatcher) = self.dispatcher.upgrade() {
            for waiter in waiters {
                if !waiter.cancelled.load(Ordering::Relaxed) {
                    dispatcher.image(Ready {
                        target: waiter.target,
                        data: data.clone(),
                    });
                }
            }
        }
    }
    fn decode(
        &self,
        uri: &str,
        bytes: ImageData,
        request: &Loading,
    ) -> std::result::Result<Arc<Data>, String> {
        if request.cancelled() {
            return Err("cancelled".into());
        }
        if bytes.len() > MAX_DATA {
            return Err("image exceeds 16 MiB".into());
        }
        let (svg, width, height) = if nsh_core::svg::sniff(&bytes) {
            let raster = nsh_core::svg::rasterize_oversampled(&bytes).map_err(|e| e.to_string())?;
            let reported = nsh_core::svg::reported_size(raster.size);
            (
                Some(raster),
                reported.width.0 as u32,
                reported.height.0 as u32,
            )
        } else {
            let source = ohos_image_native_binding::ImageSource::create_from_data(&bytes)
                .map_err(|e| e.to_string())?;
            let info = source.image_info(0).map_err(|e| e.to_string())?;
            let (width, height) = (
                info.width().map_err(|e| e.to_string())?,
                info.height().map_err(|e| e.to_string())?,
            );
            let frames = u64::from(source.frame_count().unwrap_or(1).max(1));
            let frame_pixels = u64::from(width) * u64::from(height);
            if width == 0
                || height == 0
                || frame_pixels > 16_777_216
                || frame_pixels.saturating_mul(frames) > 67_108_864
            {
                return Err("image dimensions exceed 16 megapixels".into());
            }
            (None, width, height)
        };
        let decoded = Arc::new(Decoded {
            bytes: bytes.to_vec().into(),
            svg,
            width,
            height,
        });
        let cost = decoded.bytes.len().saturating_add(
            decoded
                .svg
                .as_ref()
                .map_or(width as usize * height as usize * 4, |svg| svg.pixels.len()),
        );
        let lease = self.cache.insert(uri.to_owned(), decoded.clone(), cost);
        Ok(Arc::new(Data {
            decoded,
            _lease: lease,
        }))
    }
}

struct NativeImage {
    descriptor: ohos_arkui_binding::api::attribute_option::DrawableDescriptor,
    _pixels: Vec<ohos_image_native_binding::PixelMap>,
    _bytes: Arc<[u8]>,
}
impl NativeImage {
    fn new(data: &Data) -> std::result::Result<Self, String> {
        use ohos_image_native_binding::{
            DecodingOptions, ImageSource, PixelMap, PixelMapInitializationOptions,
            types::{PixelFormat, PixelMapAlphaType},
        };
        if let Some(svg) = &data.svg {
            let mut options = PixelMapInitializationOptions::new().map_err(|e| e.to_string())?;
            options.set_width(svg.width).map_err(|e| e.to_string())?;
            options.set_height(svg.height).map_err(|e| e.to_string())?;
            options
                .set_src_pixel_format(PixelFormat::Rgba8888)
                .map_err(|e| e.to_string())?;
            options
                .set_pixel_format(PixelFormat::Rgba8888)
                .map_err(|e| e.to_string())?;
            options
                .set_alpha_type(PixelMapAlphaType::Premultiplied)
                .map_err(|e| e.to_string())?;
            let mut raw_pixels = svg.pixels.clone();
            let pixels =
                PixelMap::create(&mut raw_pixels, &mut options).map_err(|e| e.to_string())?;
            let descriptor =
                ohos_arkui_binding::api::attribute_option::DrawableDescriptor::from_pixel_map(
                    pixels.handle(),
                )
                .map_err(|e| e.to_string())?;
            return Ok(Self {
                descriptor,
                _pixels: vec![pixels],
                _bytes: data.bytes.clone(),
            });
        }
        let source = ImageSource::create_from_data(&data.bytes).map_err(|e| e.to_string())?;
        let mut options = DecodingOptions::new().map_err(|e| e.to_string())?;
        let frame_count = source.frame_count().unwrap_or(1);
        if frame_count > 1 {
            let frames = source
                .create_pixelmap_list(&mut options)
                .map_err(|e| e.to_string())?;
            if !frames.is_empty() {
                let handles: Vec<_> = frames.iter().map(PixelMap::handle).collect();
                let mut descriptor =
                    ohos_arkui_binding::api::attribute_option::DrawableDescriptor::from_animated_pixel_map(
                        &handles,
                    )
                    .map_err(|e| e.to_string())?;
                let delays = source.delay_time_list().unwrap_or_default();
                let mut frame_durations: Vec<u32> = (0..frames.len())
                    .map(|idx| {
                        let d = delays.get(idx).copied().unwrap_or(100);
                        if d <= 10 { 100 } else { d as u32 }
                    })
                    .collect();
                let total_ms: i32 = frame_durations
                    .iter()
                    .map(|&d| d as i32)
                    .sum::<i32>()
                    .max(100);
                descriptor.set_animation_duration(total_ms);
                descriptor.set_animation_iteration(-1);
                #[cfg(feature = "api-23")]
                {
                    let _ = descriptor.set_animation_frame_durations(&mut frame_durations);
                    let _ = descriptor.set_animation_auto_play(1);
                }
                #[cfg(not(feature = "api-23"))]
                let _ = &mut frame_durations;
                return Ok(Self {
                    descriptor,
                    _pixels: frames,
                    _bytes: data.bytes.clone(),
                });
            }
        }
        let pixels = source
            .create_pixelmap(&mut options)
            .map_err(|e| e.to_string())?;
        let descriptor =
            ohos_arkui_binding::api::attribute_option::DrawableDescriptor::from_pixel_map(
                pixels.handle(),
            )
            .map_err(|e| e.to_string())?;
        Ok(Self {
            descriptor,
            _pixels: vec![pixels],
            _bytes: data.bytes.clone(),
        })
    }
}
pub(crate) struct ImageView {
    decoded: RefCell<[Option<NativeImage>; 2]>,
    pub(crate) native: NativeNode,
    owner: UiWeak<Node>,
    loader: Arc<Loader>,
    generation: Cell<u64>,
    jobs: RefCell<Vec<Arc<AtomicBool>>>,
    primary: Cell<bool>,
    loaded: Cell<bool>,
    data: RefCell<Option<Arc<Data>>>,
    tint: Cell<Option<nsh_core::Color>>,
    radii: Cell<[f32; 4]>,
    blur_radius: Cell<f32>,
    applied_blur: Cell<f32>,
    fade_ms: Cell<f32>,
    fade_start: Cell<Option<f64>>,
}
impl ImageView {
    pub(crate) fn new(owner: UiWeak<Node>, loader: Arc<Loader>) -> Result<Rc<Self>> {
        let native = Image::new()?.into_node();
        native
            .borrow()
            .set_length_metric_unit(LengthMetricUnit::Vp)?;
        native
            .borrow()
            .set_bool_attribute(A::ImageDraggable, false)?;
        native.borrow().set_bool_attribute(A::Clip, true)?;
        let image = Rc::new(Self {
            decoded: RefCell::new([None, None]),
            native,
            owner,
            loader,
            generation: Cell::new(0),
            jobs: RefCell::default(),
            primary: Cell::new(false),
            loaded: Cell::new(false),
            data: RefCell::default(),
            tint: Cell::new(None),
            radii: Cell::new([0.0; 4]),
            blur_radius: Cell::new(0.0),
            applied_blur: Cell::new(0.0),
            fade_ms: Cell::new(0.0),
            fade_start: Cell::new(None),
        });
        let weak = Rc::downgrade(&image);
        image
            .native
            .borrow_mut()
            .on_event(E::ImageOnComplete, move |event| {
                callback(|| {
                    if let Some(image) = weak.upgrade()
                        && image.primary.get()
                        && event.component_i32(0) == Some(1)
                        && !image.loaded.replace(true)
                        && let Some(owner) = image.owner.upgrade()
                    {
                        if image.fade_ms.get() > 0.0 && !owner.platform.get().reduce_motion {
                            image.native.borrow().set_f32_attribute(A::Opacity, 0.0)?;
                            image.fade_start.set(Some(owner.animator.now()));
                            owner.animator.track(&owner);
                        }
                        if let Some(data) = image.data.borrow().as_ref() {
                            owner.emit(
                                EventKind::Load,
                                EventPayload::Image(nsh_core::Size::new(
                                    nsh_core::geometry::DevicePixels(data.width as i32),
                                    nsh_core::geometry::DevicePixels(data.height as i32),
                                )),
                            );
                        }
                        owner.emit(EventKind::LoadEnd, EventPayload::None);
                    }
                    Ok(())
                })
            });
        let weak = Rc::downgrade(&image);
        image
            .native
            .borrow_mut()
            .on_event(E::ImageOnError, move |event| {
                if let Some(image) = weak.upgrade()
                    && image.primary.get()
                {
                    image.failed(format!(
                        "native image error {}",
                        event.component_i32(0).unwrap_or_default()
                    ));
                }
            });
        if let Some(owner) = image.owner.upgrade() {
            owner.native.borrow_mut().add_child(image.native.clone())?;
        }
        Ok(image)
    }
    pub(crate) fn frame(&self, size: Size) -> Result<()> {
        self.native.borrow().width(size.width)?;
        self.native.borrow().height(size.height)?;
        Ok(())
    }
    pub(crate) fn style(&self, style: &nsh_core::ViewStyle) -> Result<()> {
        if self.tint.replace(style.tint_color) != style.tint_color {
            let native = self.native.borrow();
            if let Some(color) = style.tint_color {
                let (r, g, b, a) = color.to_f64();
                native.set_attribute(
                    A::ImageColorFilter,
                    vec![
                        0.0_f32, 0.0, 0.0, 0.0, r as f32, 0.0, 0.0, 0.0, 0.0, g as f32, 0.0, 0.0,
                        0.0, 0.0, b as f32, 0.0, 0.0, 0.0, a as f32, 0.0,
                    ]
                    .into(),
                )?;
            } else {
                native.reset_attribute(A::ImageColorFilter)?;
            }
        }
        let c = style.corner_radius;
        let r = style.border_radius.0;
        let radii =
            [c.top_left, c.top_right, c.bottom_left, c.bottom_right].map(|v| v.map_or(r, |p| p.0));
        if self.radii.replace(radii) != radii {
            self.native
                .borrow()
                .set_attribute(A::BorderRadius, radii.to_vec().into())?;
        }
        self.sync_blur()?;
        Ok(())
    }
    fn sync_blur(&self) -> Result<()> {
        let scale = self.owner.upgrade().map_or(1.0, |owner| owner.scale.get());
        let blur =
            crate::render::composition::geometry::native_blur_value(self.blur_radius.get(), scale);
        if self.applied_blur.replace(blur) != blur {
            self.native.borrow().set_f32_attribute(A::Blur, blur)?;
        }
        Ok(())
    }
    pub(crate) fn tick(&self, now: f64) -> Result<bool> {
        let Some(start) = self.fade_start.get() else {
            return Ok(false);
        };
        let reduced = self
            .owner
            .upgrade()
            .is_none_or(|n| n.platform.get().reduce_motion);
        let progress = if reduced {
            1.0
        } else {
            ((now - start) / self.fade_ms.get() as f64).clamp(0.0, 1.0) as f32
        };
        self.native
            .borrow()
            .set_f32_attribute(A::Opacity, progress)?;
        if progress >= 1.0 {
            self.fade_start.set(None);
        }
        Ok(progress < 1.0)
    }
    pub(crate) fn configure(
        &self,
        source: Option<ImageSource>,
        mode: ResizeMode,
        options: ImageOptions,
    ) -> Result<()> {
        self.cancel();
        self.fade_start.set(None);
        self.fade_ms.set(options.fade_ms.unwrap_or(0.0).max(0.0));
        self.native.borrow().set_f32_attribute(A::Opacity, 1.0)?;
        self.blur_radius.set(options.blur_radius.0);
        self.sync_blur()?;
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.primary.set(false);
        self.loaded.set(false);
        self.data.borrow_mut().take();
        self.native.borrow().reset_attribute(A::ImageSrc)?;
        self.native.borrow().reset_attribute(A::ImageAlt)?;
        *self.decoded.borrow_mut() = [None, None];
        self.native.borrow().set_i32_attribute(
            A::ImageObjectFit,
            match mode {
                ResizeMode::Contain => 0,   // ARKUI_OBJECT_FIT_CONTAIN
                ResizeMode::Cover => 1,     // ARKUI_OBJECT_FIT_COVER
                ResizeMode::Stretch => 3,   // ARKUI_OBJECT_FIT_FILL
                ResizeMode::ScaleDown => 4, // ARKUI_OBJECT_FIT_SCALE_DOWN
                ResizeMode::Center => 10,   // ARKUI_OBJECT_FIT_NONE_AND_ALIGN_CENTER
            },
        )?;
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        if let Some(source) = source {
            owner.emit(EventKind::LoadStart, EventPayload::None);
            for (source, placeholder) in [(Some(source), false), (options.placeholder, true)] {
                if let Some(source) = source {
                    self.jobs.borrow_mut().push(self.loader.load(
                        Target::View {
                            id: owner.id,
                            generation,
                            placeholder,
                        },
                        source.uri.to_string(),
                        source.data,
                    ));
                }
            }
        }
        Ok(())
    }
    pub(crate) fn ready(
        &self,
        generation: u64,
        placeholder: bool,
        result: std::result::Result<Arc<Data>, String>,
    ) -> Result<()> {
        if generation != self.generation.get() {
            return Ok(());
        }
        match result {
            Ok(data) => {
                let decoded = match NativeImage::new(&data) {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        if !placeholder {
                            self.failed(error);
                        }
                        return Ok(());
                    }
                };
                if placeholder {
                    self.native
                        .borrow()
                        .set_attribute(A::ImageAlt, (&decoded.descriptor).into())?;
                } else {
                    self.primary.set(true);
                    *self.data.borrow_mut() = Some(data.clone());
                    self.native
                        .borrow()
                        .set_attribute(A::ImageSrc, (&decoded.descriptor).into())?;
                }
                self.decoded.borrow_mut()[usize::from(placeholder)] = Some(decoded);
            }
            Err(message) if !placeholder => self.failed(message),
            Err(_) => {}
        }
        Ok(())
    }
    pub(crate) fn progress(&self, generation: u64, loaded: u64, total: u64) {
        if generation == self.generation.get()
            && let Some(owner) = self.owner.upgrade()
        {
            owner.emit(
                EventKind::Progress,
                EventPayload::Map(vec![
                    ("loaded", PayloadValue::Number(loaded as f64)),
                    ("total", PayloadValue::Number(total as f64)),
                ]),
            );
        }
    }
    fn failed(&self, message: String) {
        if !self.loaded.replace(true)
            && let Some(owner) = self.owner.upgrade()
        {
            owner.emit(EventKind::Error, EventPayload::Error { message });
            owner.emit(EventKind::LoadEnd, EventPayload::None);
        }
    }
    fn cancel(&self) {
        for job in self.jobs.borrow_mut().drain(..) {
            job.store(true, Ordering::Relaxed);
        }
    }
}
impl Drop for ImageView {
    fn drop(&mut self) {
        self.cancel();
        let result = (|| -> Result<()> {
            if self.native.borrow().is_owned() {
                self.native.borrow().reset_attribute(A::ImageSrc)?;
                self.native.borrow().reset_attribute(A::ImageAlt)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            log::error!("OHOS image retirement: {error}; retaining descriptors");
            std::mem::forget(self.decoded.replace([None, None]));
        }
    }
}
pub(crate) fn info(
    control: &nsh_engine::EngineControl,
    id: u32,
    result: std::result::Result<Arc<Data>, String>,
) {
    let mut values = vec![("id", PayloadValue::Number(id as f64))];
    match result {
        Ok(data) => {
            values.push(("width", PayloadValue::Number(data.width as f64)));
            values.push(("height", PayloadValue::Number(data.height as f64)));
        }
        Err(error) => values.push(("error", PayloadValue::Str(error))),
    }
    control.dispatch_event(
        nsh_core::ROOT_ID,
        EventKind::ImageInfo,
        EventPayload::Map(values),
    );
}

struct OhosTransport(ureq::Agent);

pub(crate) fn transport() -> Arc<dyn loader::Transport> {
    static TRANSPORT: std::sync::OnceLock<Arc<OhosTransport>> = std::sync::OnceLock::new();
    TRANSPORT
        .get_or_init(|| {
            Arc::new(OhosTransport(
                ureq::Agent::config_builder()
                    .http_status_as_error(false)
                    .max_redirects(0)
                    .max_redirects_will_error(false)
                    .timeout_global(Some(loader::TIMEOUT))
                    .build()
                    .into(),
            ))
        })
        .clone()
}

impl loader::Transport for OhosTransport {
    fn get(&self, get: loader::Get<'_>) -> std::result::Result<nsh_core::image::Fetched, String> {
        use std::io::Read as _;

        use nsh_core::image::{
            CacheHeaders, Fetched,
            loader::{MAX_BYTES, MAX_REDIRECTS, TIMEOUT, redirect},
        };
        let proxy = crate::services::net::SystemProxy::read().map_err(|error| error.to_string())?;
        let deadline = std::time::Instant::now() + TIMEOUT;
        let mut url = get.url.to_owned();
        for hop in 0..=MAX_REDIRECTS {
            if get.cancelled.load(Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            let mut request = self.0.get(&url);
            for (name, value) in get.headers {
                request = request.header(*name, value);
            }
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or("image request timed out")?;
            let mut response = request
                .config()
                .proxy(proxy.value())
                .timeout_global(Some(remaining))
                .build()
                .call()
                .map_err(|error| error.to_string())?;
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if hop == MAX_REDIRECTS {
                    return Err("too many image redirects".into());
                }
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or("image redirect without a Location")?;
                url = redirect(get.access, &url, location)?;
                continue;
            }
            let headers = CacheHeaders::from_lookup(|name| {
                let values: Vec<_> = response
                    .headers()
                    .get_all(name)
                    .iter()
                    .filter_map(|v| v.to_str().ok())
                    .collect();
                (!values.is_empty()).then(|| values.join(", "))
            });
            if status >= 300 {
                return Ok(Fetched::new(status, headers, Vec::new()));
            }
            let total = response.body().content_length().unwrap_or(0);
            if total > MAX_BYTES {
                return Err("image exceeds loader byte limit".into());
            }
            let mut reader = response
                .body_mut()
                .with_config()
                .limit(MAX_BYTES + 1)
                .reader();
            let mut bytes = Vec::new();
            let mut chunk = [0; 64 << 10];
            loop {
                if get.cancelled.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
                let count = reader.read(&mut chunk).map_err(|error| error.to_string())?;
                if count == 0 {
                    break;
                }
                if bytes.len() as u64 + count as u64 > MAX_BYTES {
                    return Err("image exceeds loader byte limit".into());
                }
                bytes.extend_from_slice(&chunk[..count]);
                (get.progress)(bytes.len() as u64, total);
            }
            return Ok(Fetched::new(status, headers, bytes));
        }
        Err("no image response".into())
    }
}
