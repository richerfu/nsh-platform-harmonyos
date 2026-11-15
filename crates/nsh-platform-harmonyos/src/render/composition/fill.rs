//! Fill layers on the surface's compositor context and VSync.
use std::sync::Arc;

use nsh_core::{NodeId, animation::AnimationValue, shader::ShaderLayer};
use ohos_native_window_binding::NativeWindow;
use rustc_hash::FxHashMap;

use super::{
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
    failed: bool,
}
impl View {
    /// Only live output work requests a compositor frame.
    fn needs_wake(&self) -> bool {
        !self.failed
            && self.shown
            && self.window.is_some()
            && self.size.0 > 0
            && self.size.1 > 0
            && (self.dirty || self.window_dirty || self.effects.wants_frame())
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
        Ok(())
    }
    fn handle_render_error(&mut self, key: Key, error: &gles::Error, route: Option<&Dispatcher>) {
        log::warn!("shader fill: {error}");
        self.failed = true;
        self.dirty = false;
        self.window_dirty = false;
        if let Some(route) = route {
            route.send(Message::Error(format!(
                "shader fill node {}: {error}",
                key.id
            )));
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
                    failed: false,
                });
                view.overflow = layer.overflow.0.max(0.0);
                view.failed = false;
                match view.effects.layers(&[(key.id, layer)]) {
                    Ok(()) => {
                        view.dirty = true;
                    }
                    Err(error) => {
                        view.handle_render_error(key, &error, route);
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
                    if view.failed {
                        view.shown = false;
                        view.dirty = false;
                        return;
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
                        view.failed = false;
                        return;
                    }
                    view.window = Some(gpu.window(window));
                    view.window_dirty = true;
                    view.dirty = true;
                    view.failed = false;
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
    pub(super) fn render(&mut self, route: Option<&Dispatcher>) {
        let _trace = super::trace::Scope::new(c"NSH fills");
        for (&key, view) in &mut self.views {
            if !view.needs_wake() {
                continue;
            }
            if let Err(error) = view.render() {
                view.handle_render_error(key, &error, route);
            }
        }
    }
    pub(super) fn animating(&self) -> bool {
        self.views.values().any(View::needs_wake)
    }
}
