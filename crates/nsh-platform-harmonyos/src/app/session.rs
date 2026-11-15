//! Host session and system services. Each render attachment owns its
//! engine and reply queue; services retain only weak reply routes.

use std::{
    cell::RefCell,
    path::PathBuf,
    sync::{
        Arc, LazyLock, Mutex, Weak,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use napi_derive_ohos::napi;
use napi_ohos::{
    Env, Error, Result, Status,
    bindgen_prelude::FunctionRef,
    threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode},
};
use nsh_core::{EventKind, EventPayload, PayloadValue};
#[cfg(feature = "dev")]
use ohos_pasteboard_binding::Pasteboard;
use ohos_resource_manager_binding::{RawFile, ResourceManager};
#[cfg(feature = "dev")]
use ohos_udmf_binding::{UdmfData, UdmfRecord, UdsPlainText};
use rustc_hash::FxHashMap;

use crate::app::{
    environment::NshEnvironment,
    runtime::{Dispatcher, Message},
};

const MAX_RESOURCE: usize = 64 << 20;

static NEXT_SESSION_ID: AtomicU32 = AtomicU32::new(1);
static SESSIONS: LazyLock<Mutex<FxHashMap<u32, Weak<Services>>>> = LazyLock::new(Mutex::default);

struct UiSession {
    // The window owns the last UI lifetime, independent of resource workers.
    services: Arc<Services>,
    tab_bar: std::rc::Rc<crate::interop::component::TabFactory>,
    apply_batch: std::rc::Rc<FunctionRef<crate::interop::sdk::Batch, ()>>,
    surfaces: Vec<crate::app::surface::LifecycleRoute>,
}

thread_local! {
    static UI_SESSIONS: RefCell<FxHashMap<u32, UiSession>> = RefCell::default();
}

#[napi(object)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AvoidRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

#[napi(object)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AvoidArea {
    pub visible: bool,
    pub left_rect: AvoidRect,
    pub top_rect: AvoidRect,
    pub right_rect: AvoidRect,
    pub bottom_rect: AvoidRect,
}

#[napi(object)]
#[derive(Clone, Debug, Default)]
pub(crate) struct HostInit {
    pub speech_available: bool,
    pub sdk_api_version: i32,
    pub device_type: String,
    pub cache_dir: String,
    pub files_dir: String,
    pub pixel_ratio: f64,
    pub max_refresh_rate: Option<u32>,
    pub rtl: bool,
    pub lifecycle: String,
    pub color_mode: i32,
    pub font_size_scale: f64,
    pub language: String,
    pub languages: Option<Vec<String>>,
    pub time_zone: Option<String>,
    pub regional_locale: Option<String>,
    pub is_24_hour_clock: Option<bool>,
    pub screen_reader: bool,
    pub reduce_motion: Option<bool>,
}

#[napi(object)]
#[derive(Clone, Debug, Default)]
pub(crate) struct HostCommand {
    pub system: Option<crate::services::system::SystemCommand>,
    pub kind: String,
    pub enabled: Option<bool>,
    pub light_content: Option<bool>,
    pub animated: Option<bool>,
    pub text: Option<String>,
    pub assertive: Option<bool>,
    pub announcement: Option<String>,
    pub element_id: Option<i64>,
    pub open_id: Option<u32>,
    pub limit: Option<u32>,
    pub filters: Option<Vec<String>>,
    pub url: Option<String>,
}

pub(crate) type HostCommandCallback =
    ThreadsafeFunction<HostCommand, (), HostCommand, Status, false>;

pub(crate) fn get(session_id: u32) -> Result<Arc<Services>> {
    let services = {
        let mut sessions = SESSIONS.lock().unwrap();
        sessions.retain(|_, weak| weak.strong_count() > 0);
        sessions
            .get(&session_id)
            .and_then(Weak::upgrade)
            .ok_or_else(|| Error::from_reason("OHOS host session is closed"))?
    };
    services.check()?;
    Ok(services)
}

/// Resolves an explicit window or the sole active UI session. Multiple active
/// windows require a context so a package never reads another Ability's resources.
pub(crate) fn resolve_context(
    env: &Env,
    context: Option<&napi_ohos::bindgen_prelude::Object<'_>>,
) -> Result<Arc<Services>> {
    if let Some(context) = context {
        return crate::app::entry::bind_session(env, context);
    }
    UI_SESSIONS.with(|sessions| {
        let sessions = sessions.borrow();
        let mut active = sessions
            .values()
            .filter(|session| session.services.is_foreground());
        let host = active.next().ok_or_else(|| {
            Error::from_reason("openResource requires a UIContext or an active NSH session")
        })?;
        if active.next().is_some() {
            return Err(Error::from_reason(
                "openResource requires UIContext when several windows are active",
            ));
        }
        host.services.check()?;
        Ok(host.services.clone())
    })
}

pub(crate) fn close_ui_session(session_id: u32) {
    let ui_session = UI_SESSIONS.with(|sessions| sessions.borrow_mut().remove(&session_id));
    SESSIONS.lock().unwrap().remove(&session_id);
    if let Some(ui_session) = ui_session {
        // All UI retirement runs here, on the ArkTS callback thread, even when
        // chunks, trust verification or image IO still own Services references.
        ui_session.services.close();
        for surface in ui_session.surfaces {
            if let Err(error) = surface.dispose() {
                log::error!("OHOS session surface teardown: {error}");
            }
        }
    }
}

enum Operation {
    #[cfg(feature = "dev")]
    DevCopy {
        route: Weak<crate::app::surface::Queue>,
        revision: u64,
        text: String,
    },
    Pick {
        route: Weak<Dispatcher>,
        id: u32,
        request: nsh_core::picker::Pick,
        files: Vec<crate::services::system::PickedHandle>,
    },
    Stop,
}

type PendingPick = (u32, nsh_core::picker::Pick, Weak<Dispatcher>);

pub(crate) struct Services {
    pub(crate) speech_available: bool,
    id: u32,
    active: AtomicBool,
    pixel_ratio: AtomicU32,
    max_refresh_rate: AtomicU32,
    loads: AtomicUsize,
    resource_lock: Mutex<()>,
    resource_manager: Arc<ResourceManager>,
    cache_dir: PathBuf,
    files_dir: PathBuf,
    environment: Mutex<NshEnvironment>,
    routes: Mutex<Vec<Weak<Dispatcher>>>,
    focused: Mutex<Weak<Dispatcher>>,
    selected: Mutex<Weak<crate::app::surface::Queue>>,
    overlays: Mutex<Vec<(u64, Weak<Dispatcher>)>>,
    next_overlay: AtomicU64,
    keyboard: Mutex<Option<crate::input::keyboard::KeyboardWindow>>,
    window_id: AtomicU32,
    window_avoid: Mutex<Option<([AvoidArea; 3], AvoidArea)>>,
    pub(crate) system: crate::services::system::System,
    command_tsfn: HostCommandCallback,
    next_open_id: AtomicU32,
    open_urls: Mutex<FxHashMap<u32, (u32, Weak<Dispatcher>)>>,
    picks: Mutex<FxHashMap<u32, PendingPick>>,
    queue: mpsc::SyncSender<Operation>,
}

pub(crate) struct Overlay {
    host: Weak<Services>,
    id: u64,
}
impl Drop for Overlay {
    fn drop(&mut self) {
        if let Some(host) = self.host.upgrade() {
            host.overlays
                .lock()
                .unwrap()
                .retain(|(id, _)| *id != self.id);
        }
    }
}
pub(crate) struct LoadPermit(Arc<Services>);
impl Drop for LoadPermit {
    fn drop(&mut self) {
        self.0.loads.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Services {
    pub(crate) fn max_refresh_rate(&self) -> u32 {
        crate::app::frame_rate::normalize_display_hz(self.max_refresh_rate.load(Ordering::Relaxed))
    }

    pub(crate) fn set_max_refresh_rate(&self, hz: u32) {
        self.max_refresh_rate.store(hz, Ordering::Relaxed);
    }

    pub(crate) fn window_id(&self) -> Option<u64> {
        let id = self.window_id.load(Ordering::Acquire);
        (id != 0).then_some(u64::from(id))
    }

    pub(crate) fn data_directory(&self) -> PathBuf {
        self.files_dir.join("nsh")
    }
    pub(crate) fn cache_directory(&self) -> PathBuf {
        self.cache_dir.join("nsh")
    }
    pub(crate) fn video_cache_directory(&self) -> Result<PathBuf> {
        self.check()?;
        if self.cache_dir.as_os_str().is_empty() {
            return Err(Error::from_reason(
                "OHOS host cache directory is not initialized",
            ));
        }
        Ok(self.cache_dir.clone())
    }

    pub(crate) fn apply_batch(
        &self,
        env: &Env,
        batch: crate::interop::sdk::Batch,
    ) -> crate::render::node::Result<()> {
        self.check()?;
        let applier = UI_SESSIONS
            .with(|sessions| {
                sessions
                    .borrow()
                    .get(&self.id)
                    .map(|slot| slot.apply_batch.clone())
            })
            .ok_or_else(|| Error::from_reason("ArkUI batch applier is not attached"))?;
        applier.borrow_back(env)?.call(batch)?;
        Ok(())
    }

    pub(crate) fn register_surface(&self, route: crate::app::surface::LifecycleRoute) {
        UI_SESSIONS.with(|sessions| {
            if let Some(slot) = sessions.borrow_mut().get_mut(&self.id) {
                slot.surfaces.retain(|surface| surface.is_alive());
                slot.surfaces.push(route);
            }
        });
    }

    fn ensure_keyboard(self: &Arc<Self>, id: i32) {
        if id <= 0 {
            return;
        }
        let mut filter = self.keyboard.lock().unwrap();
        if filter.as_ref().is_none_or(|filter| filter.id() != id) {
            filter.take();
            *filter = Some(crate::input::keyboard::KeyboardWindow::new(id));
        }
    }

    pub(crate) fn update_window(
        self: &Arc<Self>,
        id: i32,
        visual: [AvoidArea; 3],
        keyboard: AvoidArea,
    ) -> Result<()> {
        self.check()?;
        if id <= 0 {
            return Err(Error::from_reason("platformWindowId must be positive"));
        }
        let current = self.window_id.load(Ordering::Acquire);
        if current > 0 && current != id as u32 {
            return Err(Error::from_reason(
                "resolved window differs from the session window",
            ));
        }
        self.window_id.store(id as u32, Ordering::Release);
        self.ensure_keyboard(id);
        *self.window_avoid.lock().unwrap() = Some((visual, keyboard));
        self.broadcast(|| Message::HostWindow(visual, keyboard));
        Ok(())
    }

    pub(crate) fn window_areas(&self) -> Option<([AvoidArea; 3], AvoidArea)> {
        *self.window_avoid.lock().unwrap()
    }

    pub(crate) fn is_foreground(&self) -> bool {
        self.environment.lock().unwrap().lifecycle.as_deref() != Some("background")
    }

    pub(crate) fn activate(&self, route: Weak<Dispatcher>) {
        if let Some(route) = route.upgrade() {
            *self.selected.lock().unwrap() = route.owner.clone();
        }
        *self.focused.lock().unwrap() = route;
        self.apply_selected_status();
    }

    pub(crate) fn select_surface(&self, owner: Weak<crate::app::surface::Queue>) {
        *self.selected.lock().unwrap() = owner;
        *self.focused.lock().unwrap() = Weak::new();
        self.apply_selected_status();
    }

    fn selected_route(&self) -> Option<Arc<Dispatcher>> {
        let selected = self.selected.lock().unwrap().clone();
        self.routes
            .lock()
            .unwrap()
            .iter()
            .rev()
            .filter_map(Weak::upgrade)
            .find(|r| r.live.load(Ordering::Acquire) && Weak::ptr_eq(&r.owner, &selected))
    }

    pub(crate) fn key_event(
        &self,
        event: ohos_arkui_binding::event::KeyEvent<'_>,
        route: &Dispatcher,
    ) -> bool {
        self.is_foreground()
            && self
                .keyboard
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|keyboard| keyboard.event(event, route))
    }

    pub(crate) fn modifiers(&self) -> nsh_core::keymap::Modifiers {
        self.keyboard
            .lock()
            .unwrap()
            .as_ref()
            .map_or_else(Default::default, |keyboard| keyboard.modifiers())
    }

    pub(crate) fn apply_selected_status(&self) {
        let Some(selected) = self.selected_route().filter(|route| {
            route.manages_status_bar
                && (route.status_bar_configured.load(Ordering::Acquire)
                    || route.video_fullscreen.load(Ordering::Acquire))
        }) else {
            if let Err(error) = self.send_host_command(HostCommand {
                kind: "restore-bars".into(),
                ..Default::default()
            }) {
                log::warn!("OHOS system bar restore: {error}");
            }
            return;
        };
        let settings = *selected.status_bar.lock().unwrap();
        let Some((config, dark)) = settings else {
            return;
        };
        let light_content = match config.style {
            nsh_core::StatusBarStyle::Default => dark,
            nsh_core::StatusBarStyle::Light => true,
            nsh_core::StatusBarStyle::Dark => false,
        };
        if let Err(error) = self.send_host_command(HostCommand {
            kind: "status-bar".into(),
            enabled: Some(!config.hidden && !selected.video_fullscreen.load(Ordering::Acquire)),
            light_content: Some(light_content),
            animated: Some(config.animated),
            ..Default::default()
        }) {
            selected.send(Message::Error(error.to_string()));
        }
    }

    pub(crate) fn status_bar(
        &self,
        route: Weak<Dispatcher>,
        config: nsh_core::StatusBarConfig,
        dark: bool,
    ) -> Result<()> {
        if let Some(route) = route.upgrade().filter(|route| route.manages_status_bar) {
            route.status_bar_configured.store(true, Ordering::Release);
            *route.status_bar.lock().unwrap() = Some((config, dark));
            self.apply_selected_status();
        }
        Ok(())
    }

    /// Native video controls own the window bars until fullscreen ends.
    pub(crate) fn video_fullscreen(&self, route: Weak<Dispatcher>, enabled: bool) -> Result<()> {
        if route
            .upgrade()
            .is_none_or(|route| !route.manages_status_bar)
        {
            return Ok(());
        }
        if let Some(dispatcher) = route.upgrade() {
            dispatcher
                .status_bar_configured
                .store(true, Ordering::Release);
            dispatcher
                .video_fullscreen
                .store(enabled, Ordering::Release);
        }
        if enabled {
            self.activate(route);
        }
        let result = self.send_host_command(HostCommand {
            kind: "video-fullscreen".into(),
            enabled: Some(enabled),
            ..Default::default()
        });
        if !enabled {
            self.apply_selected_status();
        }
        result
    }

    pub(crate) fn accessibility_event(
        &self,
        route: Weak<Dispatcher>,
        announcement: Option<String>,
        element_id: Option<i64>,
    ) -> Result<()> {
        self.check()?;
        if route
            .upgrade()
            .is_none_or(|r| !r.live.load(Ordering::Acquire))
        {
            return Ok(());
        }
        self.send_host_command(HostCommand {
            kind: "accessibility-event".into(),
            announcement,
            element_id,
            ..Default::default()
        })
    }

    pub(crate) fn accessibility_live(
        &self,
        route: Weak<Dispatcher>,
        text: String,
        assertive: bool,
    ) -> Result<()> {
        self.check()?;
        if route
            .upgrade()
            .is_none_or(|r| !r.live.load(Ordering::Acquire))
        {
            return Ok(());
        }
        self.send_host_command(HostCommand {
            kind: "accessibility-live".into(),
            text: Some(text),
            assertive: Some(assertive),
            ..Default::default()
        })
    }

    pub(crate) fn cursor(&self, cursor: nsh_core::Cursor) -> Result<()> {
        self.send_host_command(HostCommand {
            kind: "cursor".into(),
            text: Some(cursor.name().into()),
            ..Default::default()
        })
    }

    pub(crate) fn haptic(&self, haptic: nsh_core::Haptic) -> Result<()> {
        self.check()?;
        let (duration, usage) = match haptic {
            nsh_core::Haptic::Selection => (10, ohos_vibrator_binding::VibratorUsage::Touch),
            nsh_core::Haptic::Impact(nsh_core::HapticImpact::Soft) => {
                (12, ohos_vibrator_binding::VibratorUsage::Touch)
            }
            nsh_core::Haptic::Impact(nsh_core::HapticImpact::Light) => {
                (15, ohos_vibrator_binding::VibratorUsage::Touch)
            }
            nsh_core::Haptic::Impact(nsh_core::HapticImpact::Rigid) => {
                (20, ohos_vibrator_binding::VibratorUsage::PhysicalFeedback)
            }
            nsh_core::Haptic::Impact(nsh_core::HapticImpact::Medium) => {
                (25, ohos_vibrator_binding::VibratorUsage::PhysicalFeedback)
            }
            nsh_core::Haptic::Impact(nsh_core::HapticImpact::Heavy) => {
                (40, ohos_vibrator_binding::VibratorUsage::PhysicalFeedback)
            }
            nsh_core::Haptic::Notification(nsh_core::HapticNotification::Success) => {
                (30, ohos_vibrator_binding::VibratorUsage::Notification)
            }
            nsh_core::Haptic::Notification(nsh_core::HapticNotification::Warning) => {
                (45, ohos_vibrator_binding::VibratorUsage::Notification)
            }
            nsh_core::Haptic::Notification(nsh_core::HapticNotification::Error) => {
                (60, ohos_vibrator_binding::VibratorUsage::Notification)
            }
            _ => return Ok(()),
        };
        let _ = ohos_vibrator_binding::start(
            duration,
            ohos_vibrator_binding::Attribute {
                vibrator_id: 0,
                usage,
            },
        );
        Ok(())
    }
    pub(crate) fn send_host_command(&self, command: HostCommand) -> Result<()> {
        self.check()?;
        let status = self
            .command_tsfn
            .call(command, ThreadsafeFunctionCallMode::NonBlocking);
        if status != Status::Ok {
            return Err(Error::from_reason(format!(
                "OHOS host command dispatch failed: {status:?}"
            )));
        }
        Ok(())
    }

    pub(crate) fn overlay(self: &Arc<Self>, route: Weak<Dispatcher>) -> Result<Overlay> {
        let id = self
            .next_overlay
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| Error::from_reason("overlay generation exhausted"))?;
        self.overlays.lock().unwrap().push((id, route));
        Ok(Overlay {
            host: Arc::downgrade(self),
            id,
        })
    }

    pub(crate) fn handle_back(&self) -> bool {
        if !self.is_foreground() {
            return false;
        }
        let overlay = {
            let mut overlays = self.overlays.lock().unwrap();
            overlays.retain(|(_, route)| {
                route
                    .upgrade()
                    .is_some_and(|r| r.live.load(Ordering::Acquire))
            });
            overlays.last().and_then(|(_, route)| route.upgrade())
        };
        let mut route = overlay.or_else(|| {
            self.focused
                .lock()
                .unwrap()
                .upgrade()
                .filter(|r| r.live.load(Ordering::Acquire))
        });
        if route.is_none() {
            let selected = self.selected.lock().unwrap().clone();
            let has_selection = selected.strong_count() > 0;
            route = self
                .routes
                .lock()
                .unwrap()
                .iter()
                .rev()
                .filter_map(Weak::upgrade)
                .find(|r| {
                    r.live.load(Ordering::Acquire)
                        && (!has_selection || Weak::ptr_eq(&r.owner, &selected))
                });
        }
        if let Some(route) = route
            && route.wants_back.load(Ordering::Acquire)
        {
            route.send(Message::Back);
            return true;
        }
        false
    }

    pub(crate) fn reserve_load(self: &Arc<Self>) -> Result<LoadPermit> {
        self.check()?;
        if self.loads.fetch_add(1, Ordering::AcqRel) >= 32 {
            self.loads.fetch_sub(1, Ordering::AcqRel);
            return Err(Error::from_reason(
                "at most 32 resource loads may be pending per Ability",
            ));
        }
        Ok(LoadPermit(self.clone()))
    }

    pub(crate) fn set_scale(&self, scale: f32) {
        self.pixel_ratio.store(scale.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn scale(&self) -> Result<f32> {
        let scale = f32::from_bits(self.pixel_ratio.load(Ordering::Relaxed));
        if !scale.is_finite() || !(0.25..=8.0).contains(&scale) {
            return Err(Error::from_reason(
                "host pixel ratio must be between 0.25 and 8",
            ));
        }
        Ok(scale)
    }

    pub(crate) fn tab_bar_factory(
        &self,
    ) -> Result<std::rc::Rc<crate::interop::component::TabFactory>> {
        UI_SESSIONS
            .with(|sessions| {
                sessions
                    .borrow()
                    .get(&self.id)
                    .map(|session| session.tab_bar.clone())
            })
            .ok_or_else(|| Error::from_reason("OHOS UI session closed"))
    }

    pub(crate) fn create(
        init: HostInit,
        platform_window_id: i32,
        resource_manager: ResourceManager,
        apply_batch: FunctionRef<crate::interop::sdk::Batch, ()>,
        command_tsfn: HostCommandCallback,
        tab_bar: crate::interop::component::TabFactory,
    ) -> Result<u32> {
        if platform_window_id < 0 {
            return Err(Error::from_reason("platformWindowId must be nonnegative"));
        }
        let id = NEXT_SESSION_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::from_reason("OHOS host session IDs exhausted"))?;
        let environment = environment_from_init(&init);
        environment
            .clone()
            .apply(nsh_engine::Environment::default())?;
        let (queue, receiver) = mpsc::sync_channel(256);
        let services = Arc::new(Self {
            speech_available: init.speech_available,
            id,
            active: AtomicBool::new(true),
            pixel_ratio: AtomicU32::new((init.pixel_ratio as f32).to_bits()),
            max_refresh_rate: AtomicU32::new(
                init.max_refresh_rate
                    .unwrap_or_else(ohos_display_binding::default_display_refresh_rate),
            ),
            loads: AtomicUsize::new(0),
            resource_lock: Mutex::new(()),
            resource_manager: Arc::new(resource_manager),
            cache_dir: PathBuf::from(init.cache_dir),
            files_dir: PathBuf::from(init.files_dir),
            environment: Mutex::new(environment),
            routes: Mutex::new(Vec::new()),
            focused: Mutex::new(Weak::new()),
            selected: Mutex::new(Weak::new()),
            overlays: Mutex::default(),
            next_overlay: AtomicU64::new(1),
            keyboard: Mutex::default(),
            window_id: AtomicU32::new(platform_window_id as u32),
            window_avoid: Mutex::default(),
            command_tsfn,
            system: Default::default(),
            next_open_id: AtomicU32::new(1),
            open_urls: Mutex::default(),
            picks: Mutex::default(),
            queue,
        });
        let weak = Arc::downgrade(&services);
        thread::Builder::new()
            .name(format!("nsh-ohos-host-{id}"))
            .spawn(move || {
                while let Ok(operation) = receiver.recv() {
                    if matches!(operation, Operation::Stop) {
                        break;
                    }
                    let Some(services) = weak.upgrade() else {
                        break;
                    };
                    if services.check().is_err() {
                        break;
                    }
                    match operation {
                        #[cfg(feature = "dev")]
                        Operation::DevCopy {
                            route,
                            revision,
                            text,
                        } => services.copy_development(route, revision, text),
                        Operation::Pick {
                            route,
                            id,
                            request,
                            files,
                        } => {
                            let files = crate::services::system::read_picked(files, &request);
                            if services.check().is_ok()
                                && let Some(route) = route.upgrade()
                            {
                                route.send(Message::HostEvent(
                                    EventKind::Pick,
                                    EventPayload::Picked { id, files },
                                ));
                            }
                        }
                        Operation::Stop => unreachable!(),
                    }
                }
            })
            .map_err(|error| Error::from_reason(error.to_string()))?;
        if platform_window_id > 0 {
            services.ensure_keyboard(platform_window_id);
        }
        UI_SESSIONS.with(|sessions| {
            sessions.borrow_mut().insert(
                id,
                UiSession {
                    tab_bar: std::rc::Rc::new(tab_bar),
                    services: services.clone(),
                    apply_batch: std::rc::Rc::new(apply_batch),
                    surfaces: Vec::new(),
                },
            );
        });
        {
            let mut sessions = SESSIONS.lock().unwrap();
            sessions.retain(|_, weak| weak.strong_count() > 0);
            sessions.insert(id, Arc::downgrade(&services));
        }
        Ok(id)
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.active.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(Error::from_reason("Ability session is closed"))
        }
    }

    pub(crate) fn environment(&self) -> NshEnvironment {
        self.environment.lock().unwrap().clone()
    }

    pub(crate) fn attach(&self, route: Weak<Dispatcher>) -> Result<()> {
        self.check()?;
        let mut routes = self.routes.lock().unwrap();
        routes.retain(|route| route.strong_count() > 0);
        routes.push(route.clone());
        drop(routes);
        let selected = self.selected.lock().unwrap().clone();
        if selected.strong_count() == 0
            || route
                .upgrade()
                .is_some_and(|r| Weak::ptr_eq(&r.owner, &selected))
        {
            self.activate(route.clone());
        }
        if let (Some(route), Some((visual, keyboard))) =
            (route.upgrade(), *self.window_avoid.lock().unwrap())
        {
            route.send(Message::HostWindow(visual, keyboard));
        }
        Ok(())
    }

    pub(crate) fn broadcast(&self, message: impl Fn() -> Message) {
        let routes: Vec<_> = {
            let mut routes = self.routes.lock().unwrap();
            routes.retain(|route| route.strong_count() > 0);
            routes.iter().filter_map(Weak::upgrade).collect()
        };
        for route in routes {
            route.send(message());
        }
    }

    pub(crate) fn update(&self, changes: NshEnvironment) -> Result<()> {
        self.check()?;
        if changes
            .lifecycle
            .as_deref()
            .is_some_and(|state| state != "active")
            && let Some(keyboard) = self.keyboard.lock().unwrap().as_ref()
        {
            keyboard.clear();
        }
        let mut environment = self.environment.lock().unwrap();
        let snapshot = environment
            .clone()
            .apply(nsh_engine::Environment::default())?;
        let next = changes.clone().apply(snapshot)?;
        self.set_scale(next.pixel_ratio);
        *environment = NshEnvironment::snapshot(next);
        drop(environment);
        self.broadcast(|| Message::HostEnvironment(changes.clone()));
        Ok(())
    }

    fn close(&self) {
        if self.active.swap(false, Ordering::AcqRel) {
            self.keyboard.lock().unwrap().take();
            self.open_urls.lock().unwrap().clear();
            self.picks.lock().unwrap().clear();
            self.broadcast(|| Message::HostClosed);
            let _ = self.queue.try_send(Operation::Stop);
        }
    }

    pub(crate) fn resource_reader(self: &Arc<Self>, name: &str) -> Result<Box<dyn std::io::Read>> {
        resource_name(name)?;
        self.check()?;
        let _lock = self.resource_lock.lock().unwrap();
        let file = self
            .resource_manager
            .open_raw_file(name)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(Box::new(ResourceReader {
            file: Some(file),
            _manager: self.resource_manager.clone(),
            host: self.clone(),
        }))
    }

    pub(crate) fn resource(&self, name: &str) -> Result<Vec<u8>> {
        self.resource_with_limit(name, MAX_RESOURCE)
    }

    pub(crate) fn resource_with_limit(&self, name: &str, limit: usize) -> Result<Vec<u8>> {
        resource_name(name)?;
        self.check()?;
        // One manager is shared by every engine and image worker in this host.
        let _lock = self.resource_lock.lock().unwrap();
        self.check()?;
        let file = self
            .resource_manager
            .open_raw_file(name)
            .map_err(|error| Error::from_reason(error.to_string()))?;
        let bytes = file
            .read_bounded(limit)
            .map_err(|error| Error::from_reason(error.to_string()))?;
        self.check()?;
        Ok(bytes)
    }

    #[cfg(feature = "dev")]
    pub(crate) fn copy_development_error(
        &self,
        route: Weak<crate::app::surface::Queue>,
        revision: u64,
        text: String,
    ) -> Result<()> {
        self.enqueue(Operation::DevCopy {
            route,
            revision,
            text,
        })
    }

    #[cfg(feature = "dev")]
    fn copy_development(
        &self,
        route: Weak<crate::app::surface::Queue>,
        revision: u64,
        text: String,
    ) {
        if route.strong_count() == 0 {
            return;
        }
        let result = write_clipboard_text(&text).map_err(|error| error.to_string());
        if self.check().is_ok()
            && let Some(route) = route.upgrade()
        {
            route.call(
                crate::app::surface::Message::DevCopied { revision, result },
                ThreadsafeFunctionCallMode::NonBlocking,
            );
        }
    }

    pub(crate) fn open_url(&self, route: Weak<Dispatcher>, id: u32, url: String) -> Result<()> {
        self.check()?;
        let open_id = self
            .next_open_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::from_reason("openUrl request IDs exhausted"))?;
        self.open_urls
            .lock()
            .unwrap()
            .insert(open_id, (id, route.clone()));
        if let Err(error) = self.send_host_command(HostCommand {
            kind: "open-url".into(),
            open_id: Some(open_id),
            url: Some(url),
            ..Default::default()
        }) {
            self.open_urls.lock().unwrap().remove(&open_id);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn complete_open_url(&self, open_id: u32, opened: bool) {
        if self.check().is_err() {
            return;
        }
        if let Some((id, route)) = self.open_urls.lock().unwrap().remove(&open_id)
            && let Some(route) = route.upgrade()
        {
            route.send(Message::HostEvent(
                EventKind::OpenUrl,
                EventPayload::Map(vec![
                    ("id", PayloadValue::Number(id as f64)),
                    ("opened", PayloadValue::Bool(opened)),
                ]),
            ));
        }
    }

    pub(crate) fn pick(
        &self,
        route: Weak<Dispatcher>,
        id: u32,
        request: nsh_core::picker::Pick,
    ) -> Result<()> {
        self.check()?;
        let pick_id = self
            .next_open_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::from_reason("picker request IDs exhausted"))?;
        let command = match &request {
            nsh_core::picker::Pick::Images(r) => HostCommand {
                kind: "pick-images".into(),
                open_id: Some(pick_id),
                limit: Some(r.limit),
                ..Default::default()
            },
            nsh_core::picker::Pick::Files(r) => {
                let filters: Vec<String> = r
                    .filters
                    .iter()
                    .filter(|f| !f.extensions.is_empty())
                    .map(|f| {
                        let suffixes = f
                            .extensions
                            .iter()
                            .map(|ext| format!(".{ext}"))
                            .collect::<Vec<_>>()
                            .join(",");
                        let label = f.name.replace('|', " ").trim().to_owned();
                        if label.is_empty() {
                            suffixes
                        } else {
                            format!("{label}|{suffixes}")
                        }
                    })
                    .collect();
                HostCommand {
                    kind: "pick-files".into(),
                    open_id: Some(pick_id),
                    limit: Some(r.limit),
                    filters: (!filters.is_empty()).then_some(filters),
                    ..Default::default()
                }
            }
        };
        self.picks
            .lock()
            .unwrap()
            .insert(pick_id, (id, request, route.clone()));
        if let Err(error) = self.send_host_command(command) {
            self.picks.lock().unwrap().remove(&pick_id);
            if let Some(route) = route.upgrade() {
                route.send(Message::HostEvent(
                    EventKind::Pick,
                    EventPayload::Picked {
                        id,
                        files: Vec::new(),
                    },
                ));
            }
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn complete_pick(
        &self,
        pick_id: u32,
        files: Vec<crate::services::system::PickedHandle>,
    ) {
        if self.check().is_err() {
            return;
        }
        let Some((id, request, route)) = self.picks.lock().unwrap().remove(&pick_id) else {
            return;
        };
        if files.is_empty() {
            if let Some(route) = route.upgrade() {
                route.send(Message::HostEvent(
                    EventKind::Pick,
                    EventPayload::Picked {
                        id,
                        files: Vec::new(),
                    },
                ));
            }
            return;
        }
        if self
            .enqueue(Operation::Pick {
                route: route.clone(),
                id,
                request,
                files,
            })
            .is_err()
            && let Some(route) = route.upgrade()
        {
            route.send(Message::HostEvent(
                EventKind::Pick,
                EventPayload::Picked {
                    id,
                    files: Vec::new(),
                },
            ));
        }
    }

    fn enqueue(&self, operation: Operation) -> Result<()> {
        self.check()?;
        self.queue
            .try_send(operation)
            .map_err(|_| Error::from_reason("Ability operation queue is full or closed"))
    }
}

#[cfg(feature = "dev")]
fn write_clipboard_text(text: &str) -> Result<()> {
    if text.contains('\0') {
        return Err(Error::from_reason("clipboard text must not contain NUL"));
    }
    let board = Pasteboard::new();
    if text.is_empty() {
        return board
            .clear()
            .map_err(|error| Error::from_reason(error.to_string()));
    }
    let mut data = UdmfData::try_new().map_err(|error| Error::from_reason(error.to_string()))?;
    let record = UdmfRecord::try_new().map_err(|error| Error::from_reason(error.to_string()))?;
    let uds = UdsPlainText::new();
    uds.set_content(text)
        .map_err(|error| Error::from_reason(error.to_string()))?;
    record
        .add(uds.into())
        .map_err(|error| Error::from_reason(error.to_string()))?;
    data.add_record(&record)
        .map_err(|error| Error::from_reason(error.to_string()))?;
    board
        .set_data(&data)
        .map_err(|error| Error::from_reason(error.to_string()))
}

pub(crate) fn idiom(device_type: &str) -> &'static str {
    match device_type.trim() {
        "tablet" | "pad" => "pad",
        "2in1" | "pc" | "desktop" => "desktop",
        "tv" => "tv",
        _ => "phone",
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn environment_from_config(
    color_mode: i32,
    font_size_scale: f64,
    language: String,
    device_type: Option<&str>,
    rtl: bool,
    languages: Option<Vec<String>>,
    time_zone: Option<String>,
    regional_locale: Option<String>,
    is_24_hour_clock: Option<bool>,
) -> NshEnvironment {
    let regional_source = regional_locale
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&language);
    let preferences = nsh_core::platform::LocaleSettings::parse(regional_source);
    let hour_cycle = is_24_hour_clock.map(|twenty_four| {
        let twelve = !twenty_four;
        let cycle = if preferences.hour_cycle.twelve_hour() == twelve {
            preferences.hour_cycle
        } else if twelve {
            nsh_core::platform::HourCycle::H12
        } else {
            nsh_core::platform::HourCycle::H23
        };
        cycle.name().to_owned()
    });
    NshEnvironment {
        font_scale: (font_size_scale.is_finite() && font_size_scale > 0.0)
            .then_some(font_size_scale),
        color_scheme: match color_mode {
            0 => Some("dark".into()),
            1 => Some("light".into()),
            _ => None,
        },
        rtl: Some(rtl),
        idiom: device_type
            .filter(|v| !v.is_empty())
            .map(idiom)
            .or_else(|| ohos_device_binding::device_type().map(idiom))
            .map(str::to_owned),
        locale: (!language.is_empty()).then_some(language),
        languages: languages.filter(|list| !list.is_empty()),
        // Drop identifiers this NSH does not know so the rest of the update applies.
        // NshEnvironment::apply stays strict for explicit JS setEnvironment overrides.
        time_zone: time_zone
            .filter(|tz| !tz.is_empty() && nsh_core::platform::TimeZone::parse(tz).is_some()),
        regional_locale: regional_locale.filter(|tag| !tag.is_empty()),
        hour_cycle,
        ..NshEnvironment::default()
    }
}

fn environment_from_init(init: &HostInit) -> NshEnvironment {
    let mut env = environment_from_config(
        init.color_mode,
        init.font_size_scale,
        init.language.clone(),
        Some(&init.device_type),
        init.rtl,
        init.languages.clone(),
        init.time_zone.clone(),
        init.regional_locale.clone(),
        init.is_24_hour_clock,
    );
    env.pixel_ratio = Some(init.pixel_ratio);
    env.lifecycle = Some(init.lifecycle.clone());
    env.api_level = u32::try_from(init.sdk_api_version).ok();
    env.screen_reader = Some(init.screen_reader);
    env.reduce_motion = init.reduce_motion;
    env
}

pub(crate) fn resource_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains(['\0', '\\'])
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(Error::from_reason(
            "resource path must contain relative normal components",
        ));
    }
    Ok(())
}

// RawFile stays on the caller's IO worker. Every manager operation, including
// closing the file, shares the host's serialization lock.
struct ResourceReader {
    file: Option<RawFile>,
    _manager: Arc<ResourceManager>,
    host: Arc<Services>,
}
impl std::io::Read for ResourceReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.host.check().map_err(std::io::Error::other)?;
        let _lock = self.host.resource_lock.lock().unwrap();
        let file = self.file.as_ref().unwrap();
        let remaining = file.remain();
        if remaining < 0 {
            return Err(std::io::Error::other("invalid raw resource length"));
        }
        let count = buffer.len().min(remaining as usize).min(64 << 10);
        if count == 0 {
            return Ok(0);
        }
        let read = file
            .read_into(&mut buffer[..count])
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        if read == 0 {
            return Err(std::io::Error::other("raw resource read failed"));
        }
        Ok(read)
    }
}
impl Drop for ResourceReader {
    fn drop(&mut self) {
        let _lock = self.host.resource_lock.lock().unwrap();
        self.file.take();
    }
}
