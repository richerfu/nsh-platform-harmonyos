//! Fill layers on the surface's compositor context and VSync.
use std::{sync::Arc, time::Instant};

use nsh_core::{NodeId, animation::AnimationValue, shader::ShaderLayer};
use ohos_native_window_binding::NativeWindow;
use rustc_hash::FxHashMap;

use super::{
    fill_retry::{FillFailureAction, FillRetry},
    gles::{self, Effects},
    readback::{TextureContext, Window},
};
use crate::{
    app::runtime::{Dispatcher, Message},
    render::geometry::Size,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    pub(crate) id: NodeId,
    pub(crate) generation: u32,
}

pub(crate) enum Command {
    Layer(Key, Arc<ShaderLayer>),
    Slot(Key, u16, AnimationValue),
    Size(Key, Size, f32),
    Shown(Key, bool),
    Window(Key, NativeWindow),
    WindowGone(Key, usize),
    Remove(Key),
}

struct View {
    effects: Effects,
    window: Option<Window>,
    size: (u32, u32),
    scale: f32,
    box_size: nsh_core::Size<nsh_core::Points>,
    overflow: f32,
    shown: bool,
    dirty: bool,
    window_dirty: bool,
    retry: FillRetry,
}
impl View {
    /// True while the view needs VSync wakeups (including during backoff before
    /// `retry_at` expires, so static fills wake once their backoff elapses).
    fn needs_wake(&self) -> bool {
        !self.retry.has_invalid_layer()
            && self.shown
            && self.window.is_some()
            && self.size.0 > 0
            && self.size.1 > 0
            && (self.dirty || self.window_dirty || self.effects.wants_frame())
    }
    fn due(&self, now: Instant) -> bool {
        self.needs_wake() && self.retry.due(now)
    }
    fn render(&mut self) -> Result<(), gles::Error> {
        let (width, height, scale) =
            super::geometry::Coverage::shader(self.box_size, self.overflow).pixels(self.scale);
        let needs_redraw = self.dirty || self.window_dirty;
        let rendered =
            self.effects
                .render_fill((width, height), scale, needs_redraw, self.box_size)?;
        if (rendered || self.window_dirty)
            && let Some(output) = self.effects.output()
            && let Some(window) = &mut self.window
            && !output.present(window)?
        {
            self.dirty = true;
            self.window_dirty = true;
            return Ok(());
        }
        self.dirty = false;
        self.window_dirty = false;
        self.retry.on_render_success();
        Ok(())
    }
    fn handle_render_error(
        &mut self,
        key: Key,
        error: &gles::Error,
        now: Instant,
        route: Option<&Dispatcher>,
    ) {
        log::warn!("shader fill: {error}");
        match self.retry.on_render_failure(now, error.is_retryable()) {
            FillFailureAction::RetryLater => {
                self.dirty = true;
            }
            FillFailureAction::GiveUp { report } => {
                self.shown = false;
                self.dirty = false;
                self.window_dirty = false;
                if report && let Some(route) = route {
                    route.send(Message::Error(format!(
                        "shader fill node {}: {error}",
                        key.id
                    )));
                }
            }
        }
    }
}

#[derive(Default)]
pub(super) struct Fills {
    views: FxHashMap<Key, View>,
}
impl Fills {
    pub(super) fn clear(&mut self) -> Vec<Key> {
        self.views.drain().map(|(key, _)| key).collect()
    }
    pub(super) fn remove(&mut self, key: Key) {
        self.views.remove(&key);
    }
    pub(super) fn mark_windows_dirty(&mut self) {
        for view in self.views.values_mut() {
            if view.window.is_some() {
                view.window_dirty = true;
            }
        }
    }
    pub(super) fn apply(
        &mut self,
        command: Command,
        gpu: &TextureContext,
        route: Option<&Dispatcher>,
    ) {
        match command {
            Command::Layer(key, layer) => {
                if !self.views.contains_key(&key) && self.views.len() >= super::worker::MAX_LAYERS {
                    let message = format!(
                        "shader fill node {}: shader fill layer budget exceeded",
                        key.id
                    );
                    log::warn!("{message}");
                    if let Some(route) = route {
                        route.send(Message::Error(message));
                    }
                    return;
                }
                if let Some(view) = self.views.get(&key) {
                    let (w, h, _) =
                        super::geometry::Coverage::shader(view.box_size, layer.overflow.0)
                            .pixels(view.scale);
                    let others = self
                        .views
                        .iter()
                        .filter(|(other, _)| **other != key)
                        .map(|(_, view)| {
                            let (w, h, _) =
                                super::geometry::Coverage::shader(view.box_size, view.overflow)
                                    .pixels(view.scale);
                            u64::from(w) * u64::from(h)
                        })
                        .sum::<u64>();
                    if others + u64::from(w) * u64::from(h) > super::worker::MAX_SURFACE_PIXELS {
                        let message = format!(
                            "shader fill node {}: shader fill pixel budget exceeded",
                            key.id
                        );
                        log::warn!("{message}");
                        if let Some(route) = route {
                            route.send(Message::Error(message));
                        }
                        return;
                    }
                }
                let view = self.views.entry(key).or_insert_with(|| View {
                    effects: Effects::new(gpu),
                    window: None,
                    size: (0, 0),
                    scale: 1.0,
                    box_size: nsh_core::Size::default(),
                    overflow: 0.0,
                    shown: false,
                    dirty: true,
                    window_dirty: false,
                    retry: FillRetry::default(),
                });
                view.overflow = layer.overflow.0.max(0.0);
                view.retry.on_new_layer();
                match view.effects.layers(&[(key.id, layer)]) {
                    Ok(()) => {
                        view.dirty = true;
                    }
                    Err(error) => {
                        view.handle_render_error(key, &error, Instant::now(), route);
                    }
                }
            }
            Command::Slot(key, slot, value) => {
                if let Some(view) = self.views.get_mut(&key) {
                    view.effects.slot(key.id, slot, value);
                }
            }
            Command::Size(key, size, scale) => {
                let box_size = nsh_core::Size::new(
                    nsh_core::Points(size.width),
                    nsh_core::Points(size.height),
                );
                let overflow = self.views.get(&key).map_or(0.0, |view| view.overflow);
                let (width, height, _) =
                    super::geometry::Coverage::shader(box_size, overflow).pixels(scale);
                let pixels = self
                    .views
                    .iter()
                    .filter(|(other, _)| **other != key)
                    .map(|(_, view)| {
                        let (w, h, _) =
                            super::geometry::Coverage::shader(view.box_size, view.overflow)
                                .pixels(view.scale);
                        u64::from(w) * u64::from(h)
                    })
                    .sum::<u64>()
                    + u64::from(width) * u64::from(height);
                if pixels > super::worker::MAX_SURFACE_PIXELS {
                    let message = format!(
                        "shader fill node {}: shader fill pixel budget exceeded",
                        key.id
                    );
                    log::warn!("{message}");
                    if let Some(route) = route {
                        route.send(Message::Error(message));
                    }
                    return;
                }
                if let Some(view) = self.views.get_mut(&key) {
                    view.box_size = box_size;
                    view.size = (width, height);
                    view.scale = scale;
                    view.dirty = true;
                }
            }
            Command::Shown(key, shown) => {
                if let Some(view) = self.views.get_mut(&key) {
                    if view.retry.has_invalid_layer() {
                        view.shown = false;
                        view.dirty = false;
                        return;
                    }
                    if shown && !view.shown {
                        view.retry.reset_transient();
                    }
                    view.shown = shown;
                    view.dirty |= shown;
                }
            }
            Command::Window(key, window) => {
                if let Some(view) = self.views.get_mut(&key) {
                    let raw = window.raw() as usize;
                    if let Some(existing) = view.window.as_ref()
                        && existing.raw() == raw
                    {
                        existing.invalidate_geometry();
                        view.window_dirty = true;
                        view.dirty = true;
                        view.retry.on_window_attached();
                        return;
                    }
                    match gpu.window(window) {
                        Ok(next) => {
                            view.window = Some(next);
                            view.window_dirty = true;
                            view.dirty = true;
                            view.retry.on_window_attached();
                        }
                        Err(error) => {
                            // Keep any previously bound window alive and mark window_dirty
                            // so presentation retries rather than silently dropping output.
                            view.window_dirty = true;
                            view.dirty = true;
                            log::warn!("shader fill window: {error}");
                            if view
                                .retry
                                .on_window_failure(view.window.is_some())
                                .report_error
                                && let Some(route) = route
                            {
                                route.send(Message::Error(format!(
                                    "shader fill node {} window: {error}",
                                    key.id
                                )));
                            }
                        }
                    }
                }
            }
            Command::WindowGone(key, raw) => {
                if let Some(view) = self.views.get_mut(&key)
                    && view.window.as_ref().is_some_and(|w| w.raw() == raw)
                {
                    view.window = None;
                }
            }
            Command::Remove(key) => self.remove(key),
        }
    }
    pub(super) fn render(&mut self, now: Instant, route: Option<&Dispatcher>) {
        for (&key, view) in &mut self.views {
            if !view.due(now) {
                continue;
            }
            if let Err(error) = view.render() {
                view.handle_render_error(key, &error, now, route);
            }
        }
    }
    pub(super) fn animating(&self) -> bool {
        self.views.values().any(View::needs_wake)
    }
}
