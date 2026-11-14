//! One compositor worker per surface; exports and replies are generation scoped.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use nsh_core::{Filter, NodeId, animation::AnimationValue, shader::ShaderLayer};
use ohos_native_window_binding::NativeWindow;

use super::{
    mailbox::{Coalesce, CommandSender, PlanOp, SceneMailbox, command_channel},
    raster::{Scene as ScenePlan, Shared as SceneShared},
    readback::{RgbaFrame, TextureContext, TextureReader},
};
use crate::{
    app::{
        frame_rate::FrameRate,
        runtime::{Dispatcher, Message},
    },
    render::{
        composition::filter::Image,
        node::{Node, Result},
    },
};

pub(crate) const MAX_LAYERS: usize = 128;
pub(super) const MAX_SURFACE_PIXELS: u64 = 67_108_864;

#[derive(Clone, PartialEq)]
struct Settings {
    gpu: bool,
    box_size: nsh_core::Size<nsh_core::Points>,
    width: u32,
    height: u32,
    filters: Vec<Filter>,
    scale: f32,
    density: f32,
    insets: [u32; 4],
    scene: bool,
    shaders: Vec<(NodeId, Arc<ShaderLayer>)>,
}
impl Settings {
    fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}
struct Output {
    frame: RgbaFrame,
    x: f32,
    y: f32,
}
struct State {
    settings: Settings,
    screen_origin: Option<(i32, i32)>,
    output: Option<Output>,
    published_geom: Option<(u32, u32, u32, u32)>,
    ui_revealed: bool,
    presented: bool,
    surface: Option<std::result::Result<u64, String>>,
    closed: bool,
}
struct Shared {
    released: AtomicBool,
    active: AtomicBool,
    frozen: AtomicBool,
    present_gate: Mutex<bool>,
    awaiting_origin: AtomicBool,
    state: Mutex<State>,
    queued: AtomicBool,
    posted: AtomicBool,
    animating: AtomicBool,
}
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Key {
    pub(crate) id: NodeId,
    pub(crate) generation: u32,
    overlay: bool,
}
impl Key {
    fn slot(self) -> usize {
        self.id as usize * 2 + usize::from(self.overlay)
    }
}
enum Task {
    Create {
        key: Key,
        shared: Arc<Shared>,
    },
    Precompile(Vec<Arc<ShaderLayer>>),
    Fill(super::fill::Command),
    Frame,
    Resume,
    Wake(Key, Weak<Shared>),
    Slot(Key, NodeId, u16, AnimationValue),
    Transition(Key, NodeId, std::time::Duration),
    Capture {
        key: crate::render::shader::navigation::Key,
        source: Key,
        deadline: std::time::Instant,
    },
    CancelCapture(crate::render::shader::navigation::Key),
    Navigation(Navigation<()>),
    Window(Key, NativeWindow),
    WindowGone(Key, usize),
    RetireWindow(Key, Arc<AtomicBool>),
    Plan,
    Drain,
    Close,
}
impl Coalesce for Task {
    fn is_barrier(&self) -> bool {
        use super::fill::Command as FillCmd;
        match self {
            Self::Frame | Self::Resume | Self::Plan | Self::Precompile(_) | Self::Slot(..) => false,
            Self::Fill(FillCmd::Slot(..) | FillCmd::Size(..) | FillCmd::Shown(..)) => false,
            Self::Create { .. }
            | Self::Fill(
                FillCmd::Layer(..)
                | FillCmd::Window(..)
                | FillCmd::WindowGone(..)
                | FillCmd::Remove(..),
            )
            | Self::Wake(..)
            | Self::Transition(..)
            | Self::Capture { .. }
            | Self::CancelCapture(..)
            | Self::Navigation(..)
            | Self::Window(..)
            | Self::WindowGone(..)
            | Self::RetireWindow(..)
            | Self::Drain
            | Self::Close => true,
        }
    }

    fn try_merge(&mut self, incoming: &mut Option<Self>) -> bool {
        use super::fill::Command as FillCmd;
        match (self, incoming.as_ref()) {
            (Self::Frame, Some(Self::Frame))
            | (Self::Resume, Some(Self::Resume))
            | (Self::Plan, Some(Self::Plan)) => {
                incoming.take();
                true
            }
            (Self::Precompile(layers), Some(Self::Precompile(_))) => {
                if let Some(Self::Precompile(more)) = incoming.take() {
                    layers.extend(more);
                }
                true
            }
            (Self::Slot(k1, id1, s1, v1), Some(Self::Slot(k2, id2, s2, _)))
                if k1 == k2 && id1 == id2 && s1 == s2 =>
            {
                if let Some(Self::Slot(_, _, _, next)) = incoming.take() {
                    *v1 = next;
                }
                true
            }
            (Self::Fill(FillCmd::Slot(k1, s1, v1)), Some(Self::Fill(FillCmd::Slot(k2, s2, _))))
                if k1 == k2 && s1 == s2 =>
            {
                if let Some(Self::Fill(FillCmd::Slot(_, _, next))) = incoming.take() {
                    *v1 = next;
                }
                true
            }
            (
                Self::Fill(FillCmd::Size(k1, sz1, sc1)),
                Some(Self::Fill(FillCmd::Size(k2, _, _))),
            ) if k1 == k2 => {
                if let Some(Self::Fill(FillCmd::Size(_, next_sz, next_sc))) = incoming.take() {
                    *sz1 = next_sz;
                    *sc1 = next_sc;
                }
                true
            }
            (Self::Fill(FillCmd::Shown(k1, sh1)), Some(Self::Fill(FillCmd::Shown(k2, _))))
                if k1 == k2 =>
            {
                if let Some(Self::Fill(FillCmd::Shown(_, next_sh))) = incoming.take() {
                    *sh1 = next_sh;
                }
                true
            }
            _ => false,
        }
    }
}
struct Navigation<S> {
    target: Key,
    key: crate::render::shader::navigation::Key,
    snapshot: S,
    duration: std::time::Duration,
    queued_at: std::time::Instant,
    backwards: bool,
    reverse_inputs: bool,
    complete: bool,
}
struct Snapshot {
    target: super::gles::Target,
    coverage: super::geometry::Coverage,
    origin: (f32, f32),
}
struct PendingCapture {
    key: crate::render::shader::navigation::Key,
    source: Key,
    deadline: std::time::Instant,
}
pub(crate) struct Worker {
    send: CommandSender<Task>,
    plan: Arc<SceneMailbox<ScenePlan>>,
    thread: Option<std::thread::JoinHandle<()>>,
    retired: RefCell<Vec<(Arc<Shared>, super::presentation::Presentation)>>,
    retired_windows: RefCell<Vec<(Arc<AtomicBool>, super::presentation::Presentation)>>,
    retired_fills: Arc<Mutex<Vec<super::fill::Key>>>,
    drained: Arc<AtomicBool>,
}
#[derive(Clone, Copy)]
enum WindowTarget {
    Composite(Key),
    Fill(super::fill::Key),
}
#[derive(Clone)]
pub(crate) struct WindowEvents {
    target: WindowTarget,
    send: CommandSender<Task>,
}
impl WindowEvents {
    pub(super) fn created(&self, window: NativeWindow) {
        let task = match self.target {
            WindowTarget::Composite(key) => Task::Window(key, window),
            WindowTarget::Fill(key) => Task::Fill(super::fill::Command::Window(key, window)),
        };
        let _ = self.send.send(task);
    }
    pub(super) fn destroyed(&self, raw: usize) {
        let task = match self.target {
            WindowTarget::Composite(key) => Task::WindowGone(key, raw),
            WindowTarget::Fill(key) => Task::Fill(super::fill::Command::WindowGone(key, raw)),
        };
        let _ = self.send.send(task);
    }
}
impl Worker {
    pub(crate) fn for_owner(owner: &Node) -> Result<Rc<Self>> {
        let mut slot = owner.compositor.borrow_mut();
        if slot.is_none() {
            *slot = Some(Rc::new(
                Self::new(owner.dispatcher.clone(), owner.shaders.frame_rate()).map_err(
                    |error| {
                        napi_ohos::Error::from_reason(format!(
                            "Could not start OHOS compositor: {error}"
                        ))
                    },
                )?,
            ));
        }
        Ok(slot.as_ref().unwrap().clone())
    }
    pub(crate) fn precompile(&self, layer: Arc<ShaderLayer>) {
        let _ = self.send.send(Task::Precompile(vec![layer]));
    }
    pub(crate) fn fill(&self, command: super::fill::Command) {
        let _ = self.send.send(Task::Fill(command));
    }
    pub(crate) fn fill_window(&self, key: super::fill::Key) -> WindowEvents {
        WindowEvents {
            target: WindowTarget::Fill(key),
            send: self.send.clone(),
        }
    }
    pub(crate) fn take_retired_fills(&self) -> Vec<super::fill::Key> {
        std::mem::take(&mut *self.retired_fills.lock().unwrap())
    }
    pub(crate) fn new(route: Weak<Dispatcher>, rate: Arc<FrameRate>) -> std::io::Result<Self> {
        let (send, receive) = command_channel();
        let callback = send.clone();
        let plan = Arc::new(SceneMailbox::new());
        let pending_plan = plan.clone();
        let retired_fills = Arc::new(Mutex::new(Vec::new()));
        let completed_fills = retired_fills.clone();
        let drained = Arc::new(AtomicBool::new(false));
        let worker_drained = drained.clone();
        let thread = std::thread::Builder::new()
            .name("nsh-compositor".into())
            .spawn(move || {
                let vsync = rate.vsync();
                let mut vote = rate.vote();
                let mut entries: Vec<Option<Entry>> = Vec::new();
                let mut gpu: Option<TextureContext> = None;
                let mut gpu_error_reported = false;
                let mut scene: Option<ScenePlan> = None;
                let mut scene_stats = super::scene_policy::SceneStats::default();
                let mut fills = super::fill::Fills::default();
                let mut frame_pending = false;
                let mut scene_dirty = false;
                let mut pending_precompiles: std::collections::VecDeque<Arc<ShaderLayer>> =
                    std::collections::VecDeque::new();
                let mut pending_captures: Vec<PendingCapture> = Vec::new();
                let mut captured: Vec<(
                    crate::render::shader::navigation::Key,
                    Snapshot,
                    std::time::Instant,
                )> = Vec::new();
                loop {
                    let task = if pending_precompiles.is_empty() {
                        match receive.recv() {
                            Ok(task) => task,
                            Err(()) => break,
                        }
                    } else if let Some(task) = receive.try_recv() {
                        task
                    } else {
                        if gpu.is_none() {
                            match TextureContext::new() {
                                Ok(ctx) => {
                                    gpu = Some(ctx);
                                    gpu_error_reported = false;
                                }
                                Err(error) => {
                                    log::warn!("shader precompile context: {error}");
                                    pending_precompiles.clear();
                                    continue;
                                }
                            }
                        }
                        if let Some(gpu) = gpu.as_ref() {
                            while let Some(layer) = pending_precompiles.pop_front() {
                                if super::gles::is_compiled(gpu, &layer) {
                                    continue;
                                }
                                if let Err(error) = super::gles::precompile(gpu, &layer) {
                                    log::warn!("shader precompile: {error}");
                                }
                                break;
                            }
                        }
                        continue;
                    };
                    let frame = matches!(&task, Task::Frame);
                    let mut repaint = false;
                    let mut fill_frame = false;
                    match task {
                        Task::Close => break,
                        Task::Drain => {
                            scene = None;
                            scene_stats.reset_idle();
                            pending_precompiles.clear();
                            captured.clear();
                            if let Some(route) = route.upgrade() {
                                for pending in pending_captures.drain(..) {
                                    route.send(Message::Navigation(
                                        crate::render::shader::navigation::Event::Failed(
                                            pending.key,
                                        ),
                                    ));
                                }
                            } else {
                                pending_captures.clear();
                            }
                            for slot in &mut entries {
                                if let Some(entry) = slot.take() {
                                    retire_entry(entry, &route);
                                }
                            }
                            completed_fills.lock().unwrap().extend(fills.clear());
                            worker_drained.store(true, Ordering::Release);
                            if let Some(route) = route.upgrade() {
                                route.send(Message::Composite {
                                    id: nsh_core::ROOT_ID,
                                    generation: 0,
                                });
                                route.send(Message::Shader);
                            }
                        }
                        Task::Precompile(layers) => {
                            pending_precompiles.extend(layers);
                        }
                        Task::Fill(super::fill::Command::Remove(key)) => {
                            fills.remove(key);
                            completed_fills.lock().unwrap().push(key);
                            if let Some(route) = route.upgrade() {
                                route.send(Message::Shader);
                            }
                        }
                        Task::Fill(command) => {
                            let upgraded = route.upgrade();
                            if gpu.is_none() {
                                match TextureContext::new() {
                                    Ok(ctx) => {
                                        gpu = Some(ctx);
                                        gpu_error_reported = false;
                                    }
                                    Err(error) => {
                                        log::warn!("shader fill context: {error}");
                                        if !gpu_error_reported
                                            && let Some(route) = upgraded.as_deref()
                                        {
                                            gpu_error_reported = true;
                                            route.send(Message::Error(format!(
                                                "shader fill context: {error}"
                                            )));
                                        }
                                    }
                                }
                            }
                            if let Some(gpu) = gpu.as_ref() {
                                fills.apply(command, gpu, upgraded.as_deref());
                            }
                            // Apply every update, but present only on VSync. A
                            // batch can change several animated uniforms; drawing
                            // all fills after each command starves later exports.
                        }
                        Task::Resume => {
                            frame_pending = false;
                            repaint = scene.is_some();
                            fill_frame = true;
                            fills.mark_windows_dirty();
                            if route
                                .upgrade()
                                .is_some_and(|r| r.active.load(Ordering::Acquire))
                            {
                                for entry in entries.iter_mut().flatten() {
                                    if entry.shared.frozen.load(Ordering::Acquire) {
                                        continue;
                                    }
                                    if entry.window.is_some() {
                                        entry.window_dirty = true;
                                    }
                                    wake(&callback, entry.key, &entry.shared);
                                }
                            }
                        }
                        Task::Frame => {
                            repaint = scene.is_some();
                            fill_frame = true;
                            frame_pending = false;
                            for entry in entries.iter().flatten() {
                                if route
                                    .upgrade()
                                    .is_some_and(|r| r.active.load(Ordering::Acquire))
                                    && (!entry.settings.scene
                                        || entry
                                            .navigation
                                            .as_ref()
                                            .is_some_and(|nav| !nav.complete))
                                    && !entry.shared.frozen.load(Ordering::Acquire)
                                    && entry.shared.active.load(Ordering::Acquire)
                                    && entry.shared.animating.load(Ordering::Acquire)
                                {
                                    wake(&callback, entry.key, &entry.shared);
                                }
                            }
                        }
                        Task::Plan => match pending_plan.take() {
                            PlanOp::Set(next) => {
                                scene = Some(next);
                                repaint = true;
                            }
                            PlanOp::Clear => {
                                scene = None;
                                scene_stats.reset_idle();
                            }
                            PlanOp::None => {}
                        },
                        Task::Create { key, shared } => {
                            let result = (|| {
                                if shared.state.lock().unwrap().closed {
                                    return Err("composite retired".into());
                                }
                                let occupied_others = entries
                                    .iter()
                                    .enumerate()
                                    .filter(|(idx, slot)| *idx != key.slot() && slot.is_some())
                                    .count();
                                if occupied_others >= MAX_LAYERS {
                                    return Err("compositor layer budget exceeded".into());
                                }
                                let settings = shared.state.lock().unwrap().settings.clone();
                                check_pixels(&entries, key, settings.pixels())?;
                                let callback = callback.clone();
                                let weak = Arc::downgrade(&shared);
                                if gpu.is_none() {
                                    gpu = Some(
                                        TextureContext::new().map_err(|error| error.to_string())?,
                                    );
                                }
                                pending_precompiles
                                    .extend(settings.shaders.iter().map(|(_, layer)| layer.clone()));
                                let mut reader =
                                    TextureReader::with_context(gpu.as_ref().unwrap(), move || {
                                        if let Some(shared) = weak.upgrade()
                                            && !shared.frozen.load(Ordering::Acquire)
                                        {
                                            wake(&callback, key, &shared);
                                        }
                                    })
                                    .map_err(|error| error.to_string())?;
                                reader
                                    .resize(settings.width, settings.height)
                                    .map_err(|error| error.to_string())?;
                                let id = reader.id().map_err(|error| error.to_string())?;
                                entries.resize_with(entries.len().max(key.slot() + 1), || None);
                                if let Some(old) = entries[key.slot()].take() {
                                    retire_entry(old, &route);
                                }
                                entries[key.slot()] = Some(Entry {
                                    key,
                                    shared: shared.clone(),
                                    reader,
                                    gpu: gpu.as_ref().unwrap().clone(),
                                    settings,
                                    scene_source: None,
                                    scene_output: None,
                                    shaders: super::gles::Effects::new(gpu.as_ref().unwrap()),
                                    filters: super::gles::Filters::new(gpu.as_ref().unwrap()),
                                    filtered: None,
                                    window: None,
                                    window_dirty: false,
                                    window_retry: super::fill_retry::FillRetry::default(),
                                    pending_readback_bytes: 0,
                                    navigation: None,
                                    completed: None,
                                });
                                Ok(id)
                            })();
                            if result.is_err() {
                                shared.released.store(true, Ordering::Release);
                            }
                            shared.state.lock().unwrap().surface = Some(result);
                            if let Some(route) = route.upgrade() {
                                route.send(Message::Composite {
                                    id: key.id,
                                    generation: key.generation,
                                });
                            }
                        }
                        Task::Wake(key, weak_shared) => {
                            let Some(entry) = entries
                                .get(key.slot())
                                .and_then(Option::as_ref)
                                .filter(|entry| entry.key == key)
                            else {
                                if let Some(shared) = weak_shared.upgrade() {
                                    shared.queued.store(false, Ordering::Release);
                                }
                                continue;
                            };
                            entry.shared.queued.store(false, Ordering::Release);
                            if entry.shared.state.lock().unwrap().closed {
                                if let Some(old) = entries[key.slot()].take() {
                                    retire_entry(old, &route);
                                }
                                if let Some(route) = route.upgrade() {
                                    route.send(Message::Composite {
                                        id: key.id,
                                        generation: key.generation,
                                    });
                                }
                                continue;
                            }
                            if entry.shared.frozen.load(Ordering::Acquire) {
                                continue;
                            }
                            if !route
                                .upgrade()
                                .is_some_and(|r| r.active.load(Ordering::Acquire))
                            {
                                vote.update(false);
                                continue;
                            }
                            let was_animating = entry.shared.animating.load(Ordering::Acquire);
                            let settings = entry.shared.state.lock().unwrap().settings.clone();
                            let result =
                                check_pixels(&entries, key, settings.pixels()).and_then(|()| {
                                    entries[key.slot()].as_mut().unwrap().render(settings)
                                });
                            let upgraded = route.upgrade();
                            match result {
                                Ok(changed) => repaint = changed,
                                Err(error) => {
                                    let entry = entries[key.slot()].as_mut().unwrap();
                                    entry.shared.animating.store(false, Ordering::Release);
                                    entry.shared.awaiting_origin.store(false, Ordering::Release);
                                    if let Some(navigation) = entry.navigation.take()
                                        && !navigation.complete
                                        && let Some(route) = &upgraded
                                    {
                                        route.send(Message::Navigation(
                                            crate::render::shader::navigation::Event::Completed(
                                                navigation.key,
                                            ),
                                        ));
                                    }
                                    fail_captures_for_source(
                                        &mut pending_captures,
                                        key,
                                        upgraded.as_deref(),
                                    );
                                    if let Some(route) = &upgraded {
                                        route.send(Message::Error(format!(
                                            "compositor node {} generation {}: {error}",
                                            key.id, key.generation
                                        )));
                                    }
                                    continue;
                                }
                            }
                            fulfill_captures(
                                &mut entries,
                                &mut pending_captures,
                                &mut captured,
                                Some(key),
                                upgraded.as_deref(),
                            );
                            let completed_nav =
                                entries[key.slot()].as_mut().unwrap().completed.take();
                            if let Some(route) = &upgraded {
                                let shared = &entries[key.slot()].as_ref().unwrap().shared;
                                let started_animating =
                                    !was_animating && shared.animating.load(Ordering::Acquire);
                                let publish = {
                                    let state = shared.state.lock().unwrap();
                                    !state.settings.scene
                                        && (state.output.is_some() || started_animating)
                                };
                                if publish && !shared.posted.swap(true, Ordering::AcqRel) {
                                    route.send(Message::Composite {
                                        id: key.id,
                                        generation: key.generation,
                                    });
                                }
                                if let Some(key) = completed_nav {
                                    route.send(Message::Navigation(
                                        crate::render::shader::navigation::Event::Completed(key),
                                    ));
                                }
                            }
                        }
                        Task::Slot(key, id, slot, value) => {
                            repaint = scene.is_some();
                            if let Some(entry) = entries
                                .get_mut(key.slot())
                                .and_then(Option::as_mut)
                                .filter(|entry| entry.key == key)
                            {
                                if let Err(error) = entry
                                    .shaders
                                    .layers(&entry.shared.state.lock().unwrap().settings.shaders)
                                {
                                    log::warn!("shader layers: {error}");
                                }
                                entry.shaders.slot(id, slot, value);
                                wake(&callback, key, &entry.shared);
                            }
                        }
                        Task::Transition(key, id, duration) => {
                            repaint = scene.is_some();
                            if let Some(entry) = entries
                                .get_mut(key.slot())
                                .and_then(Option::as_mut)
                                .filter(|entry| entry.key == key)
                            {
                                if let Err(error) = entry
                                    .shaders
                                    .layers(&entry.shared.state.lock().unwrap().settings.shaders)
                                    .and_then(|()| entry.shaders.transition(id, duration))
                                {
                                    log::warn!("shader transition: {error}");
                                }
                                wake(&callback, key, &entry.shared);
                            }
                        }
                        Task::Capture {
                            key,
                            source,
                            deadline,
                        } => {
                            let now = std::time::Instant::now();
                            captured.retain(|(k, _, at)| {
                                k.owner != key.owner
                                    && now.saturating_duration_since(*at)
                                        < std::time::Duration::from_secs(2)
                            });
                            pending_captures.retain(|p| p.key.owner != key.owner);
                            let upgraded = route.upgrade();
                            let entry = entries
                                .get_mut(source.slot())
                                .and_then(Option::as_mut)
                                .filter(|entry| {
                                    entry.key == source
                                        && !entry.shared.state.lock().unwrap().closed
                                });
                            if let Some(entry) = entry {
                                match entry.snapshot() {
                                    Ok(Some(snapshot)) => {
                                        captured.push((key, snapshot, now));
                                        if let Some(route) = &upgraded {
                                            route.send(Message::Navigation(
                                                crate::render::shader::navigation::Event::Captured(
                                                    key,
                                                ),
                                            ));
                                        }
                                    }
                                    Ok(None) => {
                                        if entry.settings.scene {
                                            repaint = true;
                                        }
                                        wake(&callback, source, &entry.shared);
                                        pending_captures.push(PendingCapture {
                                            key,
                                            source,
                                            deadline,
                                        });
                                    }
                                    Err(error) => {
                                        log::warn!("shader navigation capture: {error}");
                                        if let Some(route) = &upgraded {
                                            route.send(Message::Navigation(
                                                crate::render::shader::navigation::Event::Failed(
                                                    key,
                                                ),
                                            ));
                                        }
                                    }
                                }
                            } else if let Some(route) = &upgraded {
                                route.send(Message::Navigation(
                                    crate::render::shader::navigation::Event::Failed(key),
                                ));
                            }
                        }
                        Task::CancelCapture(key) => {
                            pending_captures.retain(|p| p.key != key);
                            captured.retain(|(k, _, _)| *k != key);
                        }
                        Task::Navigation(request) => {
                            repaint = scene.is_some();
                            let snapshot = captured
                                .iter()
                                .position(|(key, _, _)| *key == request.key)
                                .map(|idx| captured.swap_remove(idx).1);
                            let request = Navigation {
                                snapshot,
                                target: request.target,
                                key: request.key,
                                duration: request.duration,
                                queued_at: request.queued_at,
                                backwards: request.backwards,
                                reverse_inputs: request.reverse_inputs,
                                complete: false,
                            };
                            if let Some(entry) = entries
                                .get_mut(request.target.slot())
                                .and_then(Option::as_mut)
                                .filter(|e| e.key == request.target)
                            {
                                if request.snapshot.is_none() {
                                    if let Some(route) = route.upgrade() {
                                        route.send(Message::Navigation(
                                            crate::render::shader::navigation::Event::Failed(
                                                request.key,
                                            ),
                                        ));
                                    }
                                } else {
                                    let need_origin = entry
                                        .shared
                                        .state
                                        .lock()
                                        .unwrap()
                                        .screen_origin
                                        .is_none();
                                    entry
                                        .shared
                                        .awaiting_origin
                                        .store(need_origin, Ordering::Release);
                                    if let Some(old) = entry.navigation.replace(request)
                                        && !old.complete
                                        && let Some(route) = route.upgrade()
                                    {
                                        route.send(Message::Navigation(
                                            crate::render::shader::navigation::Event::Completed(
                                                old.key,
                                            ),
                                        ));
                                    }
                                    entry.shared.animating.store(true, Ordering::Release);
                                }
                                wake(&callback, entry.key, &entry.shared);
                            } else if let Some(route) = route.upgrade() {
                                route.send(Message::Navigation(
                                    crate::render::shader::navigation::Event::Completed(
                                        request.key,
                                    ),
                                ));
                            }
                        }
                        Task::Window(key, window) => {
                            if let Some(entry) = entries
                                .get_mut(key.slot())
                                .and_then(Option::as_mut)
                                .filter(|e| e.key == key)
                            {
                                let raw = window.raw() as usize;
                                if let Some(existing) = entry.window.as_ref()
                                    && existing.raw() == raw
                                {
                                    existing.invalidate_geometry();
                                    entry.window_dirty = true;
                                    entry.window_retry.on_window_attached();
                                } else {
                                    match gpu.as_ref().unwrap().window(window) {
                                        Ok(next) => {
                                            entry.window = Some(next);
                                            entry.window_dirty = true;
                                            entry.window_retry.on_window_attached();
                                            let mut state = entry.shared.state.lock().unwrap();
                                            state.ui_revealed = false;
                                            state.published_geom = None;
                                        }
                                        Err(error) => {
                                            entry.window_dirty = true;
                                            log::warn!("shader window: {error}");
                                            if entry
                                                .window_retry
                                                .on_window_failure(entry.window.is_some())
                                                .report_error
                                                && let Some(route) = route.upgrade()
                                            {
                                                route.send(Message::Error(format!(
                                                    "compositor node {} generation {} window: {error}",
                                                    key.id, key.generation
                                                )));
                                            }
                                        }
                                    }
                                }
                                wake(&callback, key, &entry.shared);
                            }
                        }
                        Task::RetireWindow(key, released) => {
                            if let Some(entry) = entries
                                .get_mut(key.slot())
                                .and_then(Option::as_mut)
                                .filter(|entry| entry.key == key)
                            {
                                entry.window = None;
                                let mut state = entry.shared.state.lock().unwrap();
                                state.ui_revealed = false;
                                state.published_geom = None;
                            }
                            released.store(true, Ordering::Release);
                            if let Some(route) = route.upgrade() {
                                route.send(Message::Composite {
                                    id: key.id,
                                    generation: key.generation,
                                });
                            }
                        }
                        Task::WindowGone(key, raw) => {
                            if let Some(entry) = entries
                                .get_mut(key.slot())
                                .and_then(Option::as_mut)
                                .filter(|e| e.key == key)
                                && entry.window.as_ref().is_some_and(|w| w.raw() == raw)
                            {
                                entry.window = None;
                                let mut state = entry.shared.state.lock().unwrap();
                                state.ui_revealed = false;
                                state.published_geom = None;
                            }
                        }
                    }
                    let upgraded_for_frame = route.upgrade();
                    if fill_frame
                        && upgraded_for_frame
                            .as_ref()
                            .is_some_and(|r| r.active.load(Ordering::Acquire))
                    {
                        fills.render(std::time::Instant::now(), upgraded_for_frame.as_deref());
                    }
                    // Many exports wake together during navigation. Compose their latest
                    // pictures once on VSync, so command delivery never waits for a full
                    // scene raster after each individual texture notification.
                    scene_dirty = scene.is_some() && (scene_dirty || repaint);
                    if frame
                        && std::mem::take(&mut scene_dirty)
                        && let Some(scene) = &scene
                    {
                        let upgraded = upgraded_for_frame;
                        if let Some(gpu) = gpu.as_ref() {
                            let mut active_planes = 0usize;
                            let mut pre_readback_bytes = 0u64;
                            for entry in entries.iter_mut().flatten() {
                                if entry.settings.scene
                                    && entry.shared.active.load(Ordering::Acquire)
                                {
                                    active_planes += 1;
                                }
                                pre_readback_bytes = pre_readback_bytes
                                    .saturating_add(std::mem::take(&mut entry.pending_readback_bytes));
                            }
                            let mut scene_sources = SceneEntries {
                                entries: &mut entries,
                                gpu,
                                readback_bytes: pre_readback_bytes,
                                upload_bytes: 0,
                            };
                            let failed = scene.render(&mut scene_sources, upgraded.as_deref());
                            let frame_readback = scene_sources.readback_bytes;
                            let frame_upload = scene_sources.upload_bytes;
                            if let Some(snap) = scene_stats.record(
                                std::time::Instant::now(),
                                active_planes,
                                frame_readback,
                                frame_upload,
                            ) {
                                log::info!(
                                    "[NSH] scene-stats active_planes={} readback_bytes={} upload_bytes={} frames={} window_readback_bytes={} window_upload_bytes={}",
                                    snap.active_planes,
                                    snap.readback_bytes,
                                    snap.upload_bytes,
                                    snap.window_frames,
                                    snap.window_readback_bytes,
                                    snap.window_upload_bytes,
                                );
                            }
                            for entry in entries.iter_mut().flatten() {
                                if failed
                                    && entry.settings.scene
                                    && let Some(nav) = entry.navigation.as_mut()
                                    && !nav.complete
                                {
                                    nav.complete = true;
                                    entry.completed = Some(nav.key);
                                }
                                if let Some(key) = entry.completed.take()
                                    && let Some(route) = &upgraded
                                {
                                    route.send(Message::Navigation(
                                        crate::render::shader::navigation::Event::Completed(key),
                                    ));
                                }
                            }
                            if !failed {
                                fulfill_captures(
                                    &mut entries,
                                    &mut pending_captures,
                                    &mut captured,
                                    None,
                                    upgraded.as_deref(),
                                );
                            }
                        }
                    }
                    if !pending_captures.is_empty() {
                        let upgraded = route.upgrade();
                        if frame {
                            fulfill_captures(
                                &mut entries,
                                &mut pending_captures,
                                &mut captured,
                                None,
                                upgraded.as_deref(),
                            );
                        }
                        let now = std::time::Instant::now();
                        pending_captures.retain(|pending| {
                            let live = entries
                                .get(pending.source.slot())
                                .and_then(Option::as_ref)
                                .is_some_and(|entry| {
                                    entry.key == pending.source
                                        && !entry.shared.state.lock().unwrap().closed
                                });
                            if live && now < pending.deadline {
                                true
                            } else {
                                if let Some(route) = &upgraded {
                                    route.send(Message::Navigation(
                                        crate::render::shader::navigation::Event::Failed(
                                            pending.key,
                                        ),
                                    ));
                                }
                                false
                            }
                        });
                    }
                    // Shader clocks advance on the window's native VSync, just
                    // like fill shaders. UI callbacks only update ownership and
                    // uniforms; they must not divide the GPU presentation rate.
                    let animating = route
                        .upgrade()
                        .is_some_and(|r| r.active.load(Ordering::Acquire))
                        && (scene_dirty
                            || !pending_captures.is_empty()
                            || fills.animating()
                            || entries.iter().flatten().any(|entry| {
                                let pending_nav =
                                    entry.navigation.as_ref().is_some_and(|n| !n.complete);
                                (entry.window.is_some() || entry.settings.scene || pending_nav)
                                    && !entry.shared.frozen.load(Ordering::Acquire)
                                    && entry.shared.active.load(Ordering::Acquire)
                                    && (entry.window_dirty
                                        || entry.shaders.wants_frame()
                                        || pending_nav)
                            }));
                    vote.update(animating);
                    if !frame_pending && animating {
                        let send = callback.clone();
                        let status = vsync.request_frame_once(move |_| {
                            let _ = send.send(Task::Frame);
                        });
                        if status == 0 {
                            frame_pending = true;
                        } else {
                            log::warn!("OHOS compositor VSync request failed: {status}");
                            if let Some(route) = route.upgrade() {
                                route.request_frame();
                            }
                        }
                    }
                }
                // Deterministic exit handshake: release every remaining entry,
                // retired window, and pending navigation even on early shutdown.
                vote.update(false);
                drop(scene);
                drop(captured);
                if let Some(route) = route.upgrade() {
                    for pending in pending_captures.drain(..) {
                        route.send(Message::Navigation(
                            crate::render::shader::navigation::Event::Failed(pending.key),
                        ));
                    }
                }
                while let Some(task) = receive.try_recv() {
                    match task {
                        Task::Create { shared, .. } => {
                            shared.released.store(true, Ordering::Release);
                        }
                        Task::RetireWindow(_, released) => {
                            released.store(true, Ordering::Release);
                        }
                        Task::Capture { key, .. } => {
                            if let Some(route) = route.upgrade() {
                                route.send(Message::Navigation(
                                    crate::render::shader::navigation::Event::Failed(key),
                                ));
                            }
                        }
                        Task::Navigation(request) => {
                            if let Some(route) = route.upgrade() {
                                route.send(Message::Navigation(
                                    crate::render::shader::navigation::Event::Completed(
                                        request.key,
                                    ),
                                ));
                            }
                        }
                        Task::Wake(_, weak_shared) => {
                            if let Some(shared) = weak_shared.upgrade() {
                                shared.queued.store(false, Ordering::Release);
                            }
                        }
                        Task::Fill(super::fill::Command::Remove(key)) => {
                            fills.remove(key);
                            completed_fills.lock().unwrap().push(key);
                        }
                        _ => {}
                    }
                }
                for slot in &mut entries {
                    if let Some(entry) = slot.take() {
                        retire_entry(entry, &route);
                    }
                }
                completed_fills.lock().unwrap().extend(fills.clear());
                drop(gpu);
                worker_drained.store(true, Ordering::Release);
                if let Some(route) = route.upgrade() {
                    route.send(Message::Composite {
                        id: nsh_core::ROOT_ID,
                        generation: 0,
                    });
                    route.send(Message::Shader);
                }
            })?;
        Ok(Self {
            send,
            plan,
            thread: Some(thread),
            retired: RefCell::default(),
            retired_windows: RefCell::default(),
            retired_fills,
            drained,
        })
    }
    pub(crate) fn drain(&self) {
        let _ = self.send.send(Task::Drain);
    }
    pub(crate) fn drained(&self) -> bool {
        self.drained.load(Ordering::Acquire)
    }
    pub(crate) fn wake(&self) {
        let _ = self.send.send(Task::Resume);
    }
    pub(crate) fn cancel_capture(&self, key: crate::render::shader::navigation::Key) {
        let _ = self.send.send(Task::CancelCapture(key));
    }
    pub(crate) fn collect(&self) {
        self.retired_windows
            .borrow_mut()
            .retain(|(released, _)| !released.load(Ordering::Acquire));
        self.retired
            .borrow_mut()
            .retain(|(shared, _)| !shared.released.load(Ordering::Acquire));
    }
    pub(crate) fn clear_plan(&self) {
        if self.plan.clear() {
            let _ = self.send.send(Task::Plan);
        }
    }
    pub(crate) fn plan(
        &self,
        plan: Arc<crate::render::composition::draw::Plan>,
        shared: Arc<SceneShared>,
    ) {
        if self.plan.set(ScenePlan { plan, shared }) {
            let _ = self.send.send(Task::Plan);
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.send.send(Task::Close);
        // The worker owns its EGL resources and releases them on its own lane.
        self.thread.take();
    }
}
fn wake(send: &CommandSender<Task>, key: Key, shared: &Arc<Shared>) {
    if !shared.queued.swap(true, Ordering::AcqRel)
        && send.send(Task::Wake(key, Arc::downgrade(shared))).is_err()
    {
        shared.queued.store(false, Ordering::Release);
    }
}
fn retire_entry(mut entry: Entry, route: &Weak<Dispatcher>) {
    if let Some(nav) = entry.navigation.take()
        && !nav.complete
        && let Some(route) = route.upgrade()
    {
        route.send(Message::Navigation(
            crate::render::shader::navigation::Event::Completed(nav.key),
        ));
    }
    let shared = entry.shared.clone();
    drop(entry);
    shared.released.store(true, Ordering::Release);
}
fn fail_captures_for_source(
    pending_captures: &mut Vec<PendingCapture>,
    source: Key,
    route: Option<&Dispatcher>,
) {
    pending_captures.retain(|pending| {
        if pending.source == source {
            if let Some(route) = route {
                route.send(Message::Navigation(
                    crate::render::shader::navigation::Event::Failed(pending.key),
                ));
            }
            false
        } else {
            true
        }
    });
}
fn fulfill_captures(
    entries: &mut [Option<Entry>],
    pending_captures: &mut Vec<PendingCapture>,
    captured: &mut Vec<(
        crate::render::shader::navigation::Key,
        Snapshot,
        std::time::Instant,
    )>,
    only_source: Option<Key>,
    route: Option<&Dispatcher>,
) {
    if pending_captures.is_empty() {
        return;
    }
    let now = std::time::Instant::now();
    pending_captures.retain(|pending| {
        if only_source.is_some_and(|source| pending.source != source) {
            return true;
        }
        let Some(entry) = entries
            .get_mut(pending.source.slot())
            .and_then(Option::as_mut)
            .filter(|entry| {
                entry.key == pending.source && !entry.shared.state.lock().unwrap().closed
            })
        else {
            if let Some(route) = route {
                route.send(Message::Navigation(
                    crate::render::shader::navigation::Event::Failed(pending.key),
                ));
            }
            return false;
        };
        match entry.snapshot() {
            Ok(Some(snapshot)) => {
                captured.retain(|(k, _, _)| k.owner != pending.key.owner);
                captured.push((pending.key, snapshot, now));
                if let Some(route) = route {
                    route.send(Message::Navigation(
                        crate::render::shader::navigation::Event::Captured(pending.key),
                    ));
                }
                false
            }
            Ok(None) => true,
            Err(error) => {
                log::warn!("shader navigation capture: {error}");
                if let Some(route) = route {
                    route.send(Message::Navigation(
                        crate::render::shader::navigation::Event::Failed(pending.key),
                    ));
                }
                false
            }
        }
    });
}
fn check_pixels(
    entries: &[Option<Entry>],
    key: Key,
    pixels: u64,
) -> std::result::Result<(), String> {
    if pixels > 16_777_216
        || entries
            .iter()
            .flatten()
            .filter(|entry| entry.key.slot() != key.slot())
            .map(|entry| entry.settings.pixels())
            .sum::<u64>()
            + pixels
            > MAX_SURFACE_PIXELS
    {
        Err("compositor pixel budget exceeded".into())
    } else {
        Ok(())
    }
}
struct Entry {
    key: Key,
    shared: Arc<Shared>,
    reader: TextureReader,
    gpu: TextureContext,
    settings: Settings,
    scene_source: Option<Image>,
    scene_output: Option<(super::geometry::Coverage, (f32, f32))>,
    shaders: super::gles::Effects,
    filters: super::gles::Filters,
    filtered: Option<super::gles::Target>,
    window: Option<super::readback::Window>,
    window_dirty: bool,
    window_retry: super::fill_retry::FillRetry,
    pending_readback_bytes: u64,
    navigation: Option<Navigation<Option<Snapshot>>>,
    completed: Option<crate::render::shader::navigation::Key>,
}
impl Entry {
    fn render(&mut self, settings: Settings) -> std::result::Result<bool, String> {
        if let Some(navigation) = &self.navigation {
            if !settings
                .shaders
                .iter()
                .any(|(id, _)| *id == navigation.key.owner)
            {
                if let Some(navigation) = self.navigation.take()
                    && !navigation.complete
                {
                    self.completed = Some(navigation.key);
                }
            } else if navigation.complete {
                // Keep the terminal picture until its generation is retired by the UI.
                return Ok(false);
            }
        }
        if let Some(navigation) = &mut self.navigation
            && !navigation.complete
            && navigation.queued_at.elapsed()
                > navigation.duration + std::time::Duration::from_secs(1)
        {
            navigation.complete = true;
            self.completed = Some(navigation.key);
            self.shared
                .animating
                .store(self.shaders.wants_frame(), Ordering::Release);
            return Ok(false);
        }
        let mut changed = self.settings != settings || std::mem::take(&mut self.window_dirty);
        self.shaders.layers(&settings.shaders)?;
        if (settings.width, settings.height) != (self.settings.width, self.settings.height) {
            self.reader
                .resize(settings.width, settings.height)
                .map_err(|e| e.to_string())?;
        }
        self.settings = settings;
        let direct = !self.settings.scene;
        let x = -(self.settings.insets[0] as f32);
        let y = -(self.settings.insets[1] as f32);
        let passthrough = direct
            && self.settings.filters.is_empty()
            && self.settings.shaders.is_empty()
            && self.navigation.is_none();
        if passthrough {
            self.scene_source = None;
            self.scene_output = None;
            self.shaders.clear_output();
            self.shared.awaiting_origin.store(false, Ordering::Release);
            if let Some(window) = &mut self.window {
                let outcome = {
                    let gate = self.shared.present_gate.lock().unwrap();
                    if !*gate || self.shared.frozen.load(Ordering::Acquire) {
                        self.shared.animating.store(false, Ordering::Release);
                        return Ok(false);
                    }
                    self.reader
                        .present_passthrough(window, changed)
                        .map_err(|e| e.to_string())?
                };
                match outcome {
                    super::readback::PassthroughOutcome::Presented { width, height } => {
                        self.shared.animating.store(false, Ordering::Release);
                        self.record_gpu_output(width, height, x, y);
                        return Ok(true);
                    }
                    super::readback::PassthroughOutcome::Deferred => {
                        self.window_dirty = true;
                        self.shared.animating.store(true, Ordering::Release);
                        return Ok(false);
                    }
                    super::readback::PassthroughOutcome::Idle => {
                        self.shared.animating.store(false, Ordering::Release);
                        return Ok(false);
                    }
                }
            }
            let updated = self.reader.drain_oes(4).map_err(|e| e.to_string())?;
            self.shared.animating.store(false, Ordering::Release);
            return Ok(updated);
        }
        if direct {
            if self.reader.drain_oes(4).map_err(|e| e.to_string())? {
                changed = true;
            }
        } else {
            for _ in 0..4 {
                let Some(frame) = self.reader.read().map_err(|e| e.to_string())? else {
                    break;
                };
                self.pending_readback_bytes = self
                    .pending_readback_bytes
                    .saturating_add(frame.pixels.len() as u64);
                // Keep bounded captures at their own resolution. The scene canvas scales
                // them while painting, after reserving its intermediate pixel budget.
                let mut image = Image::new(frame);
                image.x = -(self.settings.insets[0] as f32);
                image.y = -(self.settings.insets[1] as f32);
                self.scene_source = Some(image);
                changed = true;
            }
        }
        changed |= self.start_navigation()?;
        let pending_nav = self.navigation.as_ref().is_some_and(|nav| !nav.complete);
        if !changed && !self.shaders.wants_frame() && !pending_nav {
            self.shared.animating.store(false, Ordering::Release);
            return Ok(false);
        }
        if direct {
            let Some(source) = self.reader.ensure_texture().map_err(|e| e.to_string())? else {
                self.shared
                    .animating
                    .store(self.shaders.wants_frame() || pending_nav, Ordering::Release);
                return Ok(false);
            };
            self.scene_source = None;
            self.scene_output = None;
            let (source, dx, dy, scale) =
                self.filters
                    .render(source, &self.settings.filters, self.settings.scale)?;
            let coverage = self.coverage();
            let mut filtered = coverage;
            filtered.insets[0] -= dx / scale;
            filtered.insets[1] -= dy / scale;
            filtered.insets[2] =
                source.1 as f32 / scale - filtered.box_size.width.0 - filtered.insets[0];
            filtered.insets[3] =
                source.2 as f32 / scale - filtered.box_size.height.0 - filtered.insets[1];
            let picture = if filtered != coverage
                || (source.1, source.2) != (self.settings.width, self.settings.height)
            {
                if self.filtered.as_ref().is_none_or(|target| {
                    (target.picture().1, target.picture().2)
                        != (self.settings.width, self.settings.height)
                }) {
                    self.filtered = Some(super::gles::Target::new(
                        &self.gpu,
                        self.settings.width,
                        self.settings.height,
                    )?);
                }
                let target = self.filtered.as_ref().unwrap();
                target.reframe(source, filtered, coverage)?;
                target.picture()
            } else {
                source
            };
            let repaint =
                self.shaders
                    .render(picture, changed, self.settings.scale, coverage, None)?;
            let published = self.publish_gpu(repaint, x, y)?;
            if published
                && self.window.is_some()
                && let Some(navigation) = &mut self.navigation
                && navigation.snapshot.is_none()
                && !self.shaders.navigation_running(navigation.key.owner)
            {
                navigation.complete = true;
                self.completed = Some(navigation.key);
                self.shared
                    .animating
                    .store(self.shaders.wants_frame(), Ordering::Release);
            }
            return Ok(repaint);
        }
        self.shared
            .animating
            .store(self.shaders.wants_frame() || pending_nav, Ordering::Release);
        Ok(changed || self.shaders.wants_frame())
    }
    fn coverage(&self) -> super::geometry::Coverage {
        super::geometry::Coverage {
            box_size: self.settings.box_size,
            insets: self.settings.insets.map(|v| v as f32 / self.settings.scale),
        }
    }
    fn snapshot(&mut self) -> std::result::Result<Option<Snapshot>, String> {
        if let Some((coverage, origin)) = self.scene_output
            && let Some(output) = self.shaders.output()
        {
            return Ok(Some(Snapshot {
                target: super::gles::Target::copy(&self.gpu, output.picture())?,
                coverage,
                origin,
            }));
        }
        if self.settings.scene {
            return Ok(None);
        }
        self.reader
            .drain_oes(4)
            .map_err(|error| error.to_string())?;
        let picture = match self.shaders.output().map(super::gles::Target::picture) {
            Some(output) => Some(output),
            None => self
                .reader
                .ensure_texture()
                .map_err(|error| error.to_string())?,
        };
        let origin = self
            .shared
            .state
            .lock()
            .unwrap()
            .screen_origin
            .unwrap_or_default();
        picture
            .map(|picture| {
                Ok(Snapshot {
                    target: super::gles::Target::copy(&self.gpu, picture)?,
                    coverage: self.coverage(),
                    origin: (origin.0 as f32, origin.1 as f32),
                })
            })
            .transpose()
    }
    fn start_navigation(&mut self) -> std::result::Result<bool, String> {
        let coverage = self.coverage();
        let origin = self.shared.state.lock().unwrap().screen_origin;
        let Some(navigation) = self.navigation.as_mut() else {
            self.shared.awaiting_origin.store(false, Ordering::Release);
            return Ok(false);
        };
        if navigation.snapshot.is_none() {
            self.shared.awaiting_origin.store(false, Ordering::Release);
            return Ok(false);
        }
        let Some(origin) = origin else {
            self.shared.awaiting_origin.store(true, Ordering::Release);
            return Ok(false);
        };
        self.shared.awaiting_origin.store(false, Ordering::Release);
        let snapshot = navigation.snapshot.take().unwrap();
        let overflow = self
            .settings
            .shaders
            .iter()
            .find(|(id, _)| *id == navigation.key.owner)
            .map_or(0.0, |(_, layer)| layer.overflow.0);
        let destination = super::geometry::Coverage::shader(coverage.box_size, overflow);
        let (w, h, _) = destination.pixels(self.settings.scale);
        let mut source = snapshot.coverage;
        // Screen origins are physical pixels; convert their delta with the display density.
        let density = self.shared.state.lock().unwrap().settings.density;
        let dx = (snapshot.origin.0 - origin.0 as f32) / density;
        let dy = (snapshot.origin.1 - origin.1 as f32) / density;
        source.insets[0] -= dx;
        source.insets[1] -= dy;
        source.insets[2] += dx;
        source.insets[3] += dy;
        let target = super::gles::Target::new(&self.gpu, w, h)?;
        target.reframe(snapshot.target.picture(), source, destination)?;
        self.shaders.navigation(
            navigation.key.owner,
            target,
            destination,
            navigation.duration,
            navigation.backwards,
            navigation.reverse_inputs,
        )?;
        Ok(true)
    }
    fn record_gpu_output(&self, width: u32, height: u32, x: f32, y: f32) {
        let geom = (width, height, x.to_bits(), y.to_bits());
        let mut state = self.shared.state.lock().unwrap();
        state.presented = true;
        if crate::render::subtree_paint::should_publish_gpu_output(
            state.ui_revealed,
            state.published_geom,
            geom,
        ) {
            state.published_geom = Some(geom);
            state.output = Some(Output {
                frame: RgbaFrame {
                    width,
                    height,
                    pixels: Vec::new(),
                },
                x,
                y,
            });
        }
    }
    fn publish_gpu(&mut self, repaint: bool, x: f32, y: f32) -> std::result::Result<bool, String> {
        let pending_nav = self.navigation.as_ref().is_some_and(|nav| !nav.complete);
        self.shared
            .animating
            .store(self.shaders.wants_frame() || pending_nav, Ordering::Release);
        let Some(output) = self.shaders.output() else {
            return Ok(false);
        };
        let Some(window) = &mut self.window else {
            return Ok(false);
        };
        if !repaint {
            return Ok(false);
        }
        let presented = {
            let gate = self.shared.present_gate.lock().unwrap();
            if !*gate || self.shared.frozen.load(Ordering::Acquire) {
                self.shared.animating.store(false, Ordering::Release);
                return Ok(false);
            }
            output.present(window)?
        };
        if !presented {
            self.window_dirty = true;
            self.shared.animating.store(true, Ordering::Release);
            return Ok(false);
        }
        let (_, width, height) = output.picture();
        self.record_gpu_output(width, height, x, y);
        Ok(true)
    }
}

struct SceneEntries<'a> {
    entries: &'a mut [Option<Entry>],
    gpu: &'a TextureContext,
    readback_bytes: u64,
    upload_bytes: u64,
}
impl super::draw::Sources for SceneEntries<'_> {
    fn image(&self, key: Key) -> Option<(&Image, f32)> {
        self.entries
            .get(key.slot())
            .and_then(Option::as_ref)
            .filter(|entry| entry.key == key)
            .and_then(|entry| {
                entry
                    .scene_source
                    .as_ref()
                    .map(|image| (image, entry.settings.scale))
            })
    }
    fn shade(
        &mut self,
        key: Key,
        image: Image,
        backdrop: Option<Image>,
        scale: f32,
    ) -> std::result::Result<Image, String> {
        let Some(entry) = self
            .entries
            .get_mut(key.slot())
            .and_then(Option::as_mut)
            .filter(|entry| entry.key == key)
        else {
            return Ok(image);
        };
        entry
            .shaders
            .layers(&entry.shared.state.lock().unwrap().settings.shaders)?;
        entry.start_navigation()?;
        let box_size = entry.settings.box_size;
        let coverage = super::geometry::Coverage {
            box_size,
            insets: [
                -image.x / scale,
                -image.y / scale,
                (image.x + image.bitmap.width() as f32) / scale - box_size.width.0,
                (image.y + image.bitmap.height() as f32) / scale - box_size.height.0,
            ],
        };
        let (width, height, effective_scale) = coverage.pixels(scale);
        let gpu = self.gpu;
        let upload_bytes = &mut self.upload_bytes;
        let mut upload = |image: &Image| -> std::result::Result<super::gles::Target, String> {
            let resized = image.resized(width, height);
            let pixels = resized.bitmap.pixels().to_vec();
            *upload_bytes = upload_bytes.saturating_add(pixels.len() as u64);
            Ok(super::gles::Target::upload(
                gpu,
                &RgbaFrame {
                    width,
                    height,
                    pixels,
                },
            )?)
        };
        let input = upload(&image)?;
        let backdrop = backdrop
            .as_ref()
            .map(|backdrop| {
                // CSS filters may have enlarged the content beyond the backdrop capture.
                let aligned = backdrop.aligned(
                    image.bitmap.width(),
                    image.bitmap.height(),
                    (
                        (backdrop.x - image.x).round() as i32,
                        (backdrop.y - image.y).round() as i32,
                    ),
                );
                upload(&aligned)
            })
            .transpose()?;
        entry.shaders.render(
            input.picture(),
            true,
            effective_scale,
            coverage,
            backdrop.as_ref().map(super::gles::Target::picture),
        )?;
        let Some(output) = entry.shaders.output() else {
            return Ok(image);
        };
        let frame = entry
            .reader
            .read_texture(output.picture())
            .map_err(|error| error.to_string())?;
        self.readback_bytes = self
            .readback_bytes
            .saturating_add(frame.pixels.len() as u64);
        let origin = entry
            .shared
            .state
            .lock()
            .unwrap()
            .screen_origin
            .unwrap_or_default();
        entry.scene_output = Some((coverage, (origin.0 as f32, origin.1 as f32)));
        let mut result = Image::new(frame).resized(image.bitmap.width(), image.bitmap.height());
        result.x = image.x;
        result.y = image.y;
        if let Some(navigation) = &mut entry.navigation
            && navigation.snapshot.is_none()
            && !entry.shaders.navigation_running(navigation.key.owner)
            && !navigation.complete
        {
            navigation.complete = true;
            entry.completed = Some(navigation.key);
        }
        let pending_nav = entry.navigation.as_ref().is_some_and(|nav| !nav.complete);
        entry.shared.animating.store(
            entry.shaders.wants_frame() || pending_nav,
            Ordering::Release,
        );
        Ok(result)
    }
}

fn next_composite_generation() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    loop {
        let generation = NEXT.fetch_add(1, Ordering::Relaxed);
        if generation != 0 {
            return generation;
        }
    }
}

pub(crate) struct Composite {
    owner: std::rc::Weak<Node>,
    presentation: RefCell<Option<super::presentation::Presentation>>,
    bridge: Cell<Option<(f32, f32)>>,
    surface: RefCell<Option<String>>,
    failed: Cell<bool>,
    pub(crate) generation: u32,
    key: Key,
    shared: Arc<Shared>,
    worker: Rc<Worker>,
}
impl Composite {
    pub(crate) fn key(&self) -> Key {
        self.key
    }
    pub(crate) fn is_failed(&self) -> bool {
        self.failed.get()
    }
    pub(crate) fn has_presented(&self, owner: &Node) -> bool {
        if owner.scene_active() {
            owner
                .scene
                .borrow()
                .as_ref()
                .is_some_and(|scene| scene.has_output_for(owner.id))
        } else {
            self.shared.state.lock().unwrap().presented
        }
    }
    pub(crate) fn new(owner: &Rc<Node>) -> Result<Self> {
        Self::create(owner, false)
    }
    pub(crate) fn overlay(owner: &Rc<Node>) -> Result<Self> {
        Self::create(owner, true)
    }
    fn create(owner: &Rc<Node>, overlay: bool) -> Result<Self> {
        let generation = next_composite_generation();
        let worker = Worker::for_owner(owner)?;
        let initial_settings = settings(owner, overlay)?;
        let initial_origin = owner
            .mounted()
            .borrow()
            .layout_position_in_screen()
            .ok()
            .map(|origin| (origin.x, origin.y));
        let shared = Arc::new(Shared {
            released: AtomicBool::new(false),
            active: AtomicBool::new(true),
            frozen: AtomicBool::new(false),
            present_gate: Mutex::new(true),
            awaiting_origin: AtomicBool::new(false),
            state: Mutex::new(State {
                settings: initial_settings.clone(),
                screen_origin: initial_origin,
                output: None,
                published_geom: None,
                ui_revealed: false,
                presented: false,
                surface: None,
                closed: false,
            }),
            queued: AtomicBool::new(false),
            posted: AtomicBool::new(false),
            animating: AtomicBool::new(false),
        });
        let key = Key {
            id: owner.id,
            generation,
            overlay,
        };
        worker
            .send
            .send(Task::Create {
                key,
                shared: shared.clone(),
            })
            .map_err(|_| napi_ohos::Error::from_reason("OHOS compositor stopped"))?;
        if !overlay {
            let mut nodes = owner.composite_nodes.borrow_mut();
            nodes.retain(|node| node.strong_count() != 0);
            let weak = Rc::downgrade(owner);
            if !nodes
                .iter()
                .any(|existing| std::rc::Weak::ptr_eq(existing, &weak))
            {
                nodes.push(weak);
            }
        }
        let presentation = if initial_settings.gpu {
            let mut layer = super::presentation::Presentation::new(
                owner,
                key,
                WindowEvents {
                    target: WindowTarget::Composite(key),
                    send: worker.send.clone(),
                },
            )?;
            let ratio = initial_settings.density / initial_settings.scale;
            layer.update(
                (initial_settings.width as f32 * ratio).round().max(1.0) as u32,
                (initial_settings.height as f32 * ratio).round().max(1.0) as u32,
                -(initial_settings.insets[0] as f32 * ratio),
                -(initial_settings.insets[1] as f32 * ratio),
                initial_settings.density,
            )?;
            Some(layer)
        } else {
            None
        };
        Ok(Self {
            owner: Rc::downgrade(owner),
            presentation: RefCell::new(presentation),
            bridge: Cell::new(None),
            surface: RefCell::new(None),
            failed: Cell::new(false),
            generation,
            key,
            shared,
            worker,
        })
    }
    pub(crate) fn update(&self, owner: &Node) -> Result<bool> {
        if self.failed.get() {
            return Ok(false);
        }
        if !self.key.overlay
            && (self.shared.frozen.load(Ordering::Acquire)
                || owner
                    .wrapper
                    .borrow()
                    .as_ref()
                    .is_some_and(crate::interop::view::Wrapper::blocks_composite_update))
        {
            return Ok(false);
        }
        if !owner.composite_dirty.get()
            && self.shared.state.lock().unwrap().settings.scene == owner.scene_active()
        {
            return Ok(false);
        }
        let settings = settings(owner, self.key.overlay)?;
        if settings.gpu && self.presentation.borrow().is_none() {
            let mut layer = super::presentation::Presentation::new(
                owner,
                self.key,
                WindowEvents {
                    target: WindowTarget::Composite(self.key),
                    send: self.worker.send.clone(),
                },
            )?;
            if let Some((alpha, dx_vp)) = self.bridge.get() {
                layer.set_bridge(alpha, dx_vp)?;
            }
            *self.presentation.borrow_mut() = Some(layer);
        } else if !settings.gpu
            && let Some(mut layer) = self.presentation.borrow_mut().take()
        {
            layer.hide();
            let released = Arc::new(AtomicBool::new(false));
            self.worker
                .retired_windows
                .borrow_mut()
                .push((released.clone(), layer));
            let _ = self
                .worker
                .send
                .send(Task::RetireWindow(self.key, released));
        }
        if let Some(layer) = self.presentation.borrow_mut().as_mut() {
            let ratio = settings.density / settings.scale;
            layer.update(
                (settings.width as f32 * ratio).round().max(1.0) as u32,
                (settings.height as f32 * ratio).round().max(1.0) as u32,
                -(settings.insets[0] as f32 * ratio),
                -(settings.insets[1] as f32 * ratio),
                settings.density,
            )?;
        }
        let mut state = self.shared.state.lock().unwrap();
        if state.screen_origin.is_none() {
            state.screen_origin = owner
                .mounted()
                .borrow()
                .layout_position_in_screen()
                .ok()
                .map(|origin| (origin.x, origin.y));
        }
        let changed = state.settings != settings;
        if changed {
            state.settings = settings;
            drop(state);
            wake(&self.worker.send, self.key, &self.shared);
            owner
                .visual_node()
                .borrow()
                .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedMeasure)?;
        }
        Ok(changed)
    }
    pub(crate) fn shader_slot(&self, id: NodeId, slot: u16, value: AnimationValue) {
        let _ = self.worker.send.send(Task::Slot(self.key, id, slot, value));
    }
    pub(crate) fn shader_transition(&self, id: NodeId, duration: std::time::Duration) {
        let _ = self
            .worker
            .send
            .send(Task::Transition(self.key, id, duration));
    }
    pub(crate) fn capture(
        &self,
        key: crate::render::shader::navigation::Key,
        deadline: std::time::Instant,
    ) {
        let _ = self.worker.send.send(Task::Capture {
            key,
            source: self.key,
            deadline,
        });
    }
    pub(crate) fn shader_navigation(
        &self,
        key: crate::render::shader::navigation::Key,
        duration: std::time::Duration,
        backwards: bool,
        reverse_inputs: bool,
    ) {
        let _ = self.worker.send.send(Task::Navigation(Navigation {
            target: self.key,
            key,
            snapshot: (),
            duration,
            queued_at: std::time::Instant::now(),
            backwards,
            reverse_inputs,
            complete: false,
        }));
    }
    pub(crate) fn set_bridge_presentation(&self, alpha: f32, dx_vp: f32) -> Result<()> {
        self.bridge.set(Some((alpha, dx_vp)));
        if let Some(layer) = self.presentation.borrow_mut().as_mut() {
            layer.set_bridge(alpha, dx_vp)?;
        }
        Ok(())
    }
    pub(crate) fn clear_bridge_presentation(&self) -> Result<()> {
        self.bridge.set(None);
        if let Some(layer) = self.presentation.borrow_mut().as_mut() {
            layer.clear_bridge()?;
        }
        Ok(())
    }
    pub(crate) fn freeze(&self) {
        let mut gate = self.shared.present_gate.lock().unwrap();
        *gate = false;
        self.shared.frozen.store(true, Ordering::Release);
        self.shared.active.store(false, Ordering::Release);
    }
    pub(crate) fn unfreeze(&self) {
        let was_frozen = {
            let mut gate = self.shared.present_gate.lock().unwrap();
            *gate = true;
            self.shared.frozen.swap(false, Ordering::AcqRel)
        };
        if was_frozen {
            self.shared.active.store(true, Ordering::Release);
            wake(&self.worker.send, self.key, &self.shared);
        }
    }
    pub(crate) fn activate(&self) {
        if !self.shared.frozen.load(Ordering::Acquire) {
            self.shared.active.store(true, Ordering::Release);
        }
    }
    pub(crate) fn shader_frame(&self, active: bool) -> bool {
        if self.shared.frozen.load(Ordering::Acquire) {
            return false;
        }
        let was_active = self.shared.active.swap(active, Ordering::AcqRel);
        let due = active && self.needs_shader_frame();
        if due && !was_active {
            wake(&self.worker.send, self.key, &self.shared);
        }
        due
    }
    pub(crate) fn needs_shader_frame(&self) -> bool {
        self.shared.animating.load(Ordering::Acquire)
    }
    pub(crate) fn insets(&self) -> crate::interop::sdk::TextureInsets {
        let state = self.shared.state.lock().unwrap();
        let [left, top, right, bottom] = state
            .settings
            .insets
            .map(|v| v as f64 / state.settings.scale as f64);
        crate::interop::sdk::TextureInsets {
            left,
            top,
            right,
            bottom,
        }
    }
    pub(crate) fn origin(&self) -> (f32, f32) {
        let insets = self.insets();
        (-(insets.left as f32), -(insets.top as f32))
    }
    pub(crate) fn surface(&self) -> Option<String> {
        self.surface.borrow().clone()
    }
    pub(crate) fn is_revealed(&self) -> bool {
        self.presentation
            .borrow()
            .as_ref()
            .is_some_and(super::presentation::Presentation::is_revealed)
    }
    pub(crate) fn hide_presentation(&self) {
        if let Some(layer) = self.presentation.borrow_mut().as_mut() {
            layer.hide();
        }
        self.shared.state.lock().unwrap().ui_revealed = false;
    }
    pub(crate) fn ready(&self) -> (bool, bool) {
        let surface = self.shared.state.lock().unwrap().surface.take();
        let mut initialized = false;
        if let Some(result) = surface {
            match result {
                Ok(id) => {
                    *self.surface.borrow_mut() = Some(id.to_string());
                    initialized = true;
                }
                Err(error) => {
                    log::warn!("OHOS compositor initialization: {error}");
                    self.failed.set(true);
                    if let Some(mut layer) = self.presentation.borrow_mut().take() {
                        layer.hide();
                    }
                }
            }
        }
        self.shared.posted.store(false, Ordering::Release);
        let output = self.shared.state.lock().unwrap().output.take();
        if self.shared.frozen.load(Ordering::Acquire) {
            return (initialized, false);
        }
        if let Some(output) = output {
            let mut presentation = self.presentation.borrow_mut();
            if let Some(layer) = presentation.as_mut() {
                let result = {
                    let state = self.shared.state.lock().unwrap();
                    let ratio = state.settings.density / state.settings.scale;
                    layer
                        .update(
                            (output.frame.width as f32 * ratio).round() as u32,
                            (output.frame.height as f32 * ratio).round() as u32,
                            output.x * ratio,
                            output.y * ratio,
                            state.settings.density,
                        )
                        .and_then(|()| layer.reveal())
                };
                match result {
                    Ok(revealed) => {
                        let is_revealed = layer.is_revealed();
                        drop(presentation);
                        self.shared.state.lock().unwrap().ui_revealed = is_revealed;
                        return (initialized, revealed);
                    }
                    Err(error) => log::warn!("shader frame presentation: {error}"),
                }
                return (initialized, false);
            }
        }
        (initialized, false)
    }
    pub(crate) fn measure(
        &self,
        event: &ohos_arkui_binding::api::node_custom_event::NodeCustomEvent,
    ) -> Result<()> {
        if let Some(layer) = self.presentation.borrow().as_ref() {
            layer.measure(event)?;
        }
        Ok(())
    }
    pub(crate) fn layout(
        &self,
        event: &ohos_arkui_binding::api::node_custom_event::NodeCustomEvent,
    ) -> Result<()> {
        if let Some(owner) = self.owner.upgrade() {
            let origin = owner.mounted().borrow().layout_position_in_screen()?;
            let mut state = self.shared.state.lock().unwrap();
            let origin = Some((origin.x, origin.y));
            if state.screen_origin != origin {
                state.screen_origin = origin;
                drop(state);
                if self.shared.awaiting_origin.load(Ordering::Acquire) {
                    wake(&self.worker.send, self.key, &self.shared);
                }
            }
        }
        if let Some(layer) = self.presentation.borrow().as_ref() {
            layer.layout(event)?;
        }
        Ok(())
    }
}
impl Drop for Composite {
    fn drop(&mut self) {
        if let Some(mut layer) = self.presentation.get_mut().take() {
            layer.hide();
            self.worker
                .retired
                .borrow_mut()
                .push((self.shared.clone(), layer));
        }
        self.shared.state.lock().unwrap().closed = true;
        wake(&self.worker.send, self.key, &self.shared);
    }
}
fn settings(owner: &Node, overlay: bool) -> Result<Settings> {
    let frame = owner.frame.get();
    let scene = owner.scene_active();
    let bounds = if overlay {
        crate::render::composition::geometry::Bounds::new(frame.width, frame.height)
    } else if scene {
        crate::render::composition::geometry::Bounds::new(frame.width, frame.height)
            .shaders(
                owner.shaders.overflow(owner.id),
                &owner.shader_layers.borrow(),
            )
            .own(&owner.style.borrow())
    } else {
        let bounds = owner.capture_bounds(0)?;
        let style = owner.style.borrow();
        bounds.filters(
            style
                .effects
                .as_ref()
                .map_or(&[], |effects| effects.filters.as_slice()),
        )
    };
    let (logical_w, logical_h) = (bounds.0[2] - bounds.0[0], bounds.0[3] - bounds.0[1]);
    let (_, _, mut scale) =
        super::geometry::target_size(logical_w.max(1.0), logical_h.max(1.0), owner.scale.get());
    if scale <= 0.0 {
        return Err(crate::render::node::Error::Unsupported(
            "invalid compositor coverage",
        ));
    }
    let dimensions = |scale| {
        let insets = bounds.insets(frame.width, frame.height, scale);
        let width = (frame.width * scale).ceil().max(1.0) as u32 + insets[0] + insets[2];
        let height = (frame.height * scale).ceil().max(1.0) as u32 + insets[1] + insets[3];
        (insets, width, height)
    };
    let (_, width, height) = dimensions(scale);
    if width.max(height) > 4096 {
        scale *= 4093.0 / width.max(height) as f32;
    }
    let (insets, width, height) = dimensions(scale);
    Ok(Settings {
        gpu: !scene && !overlay,
        box_size: nsh_core::Size::new(
            nsh_core::Points(frame.width),
            nsh_core::Points(frame.height),
        ),
        density: owner.scale.get(),
        width,
        height,
        insets,
        scene,
        shaders: if overlay {
            Vec::new()
        } else {
            owner.shader_layers.borrow().clone()
        },
        filters: if overlay {
            Vec::new()
        } else {
            owner
                .style
                .borrow()
                .effects
                .as_ref()
                .map_or_else(Vec::new, |effects| effects.filters.clone())
        },
        scale,
    })
}
