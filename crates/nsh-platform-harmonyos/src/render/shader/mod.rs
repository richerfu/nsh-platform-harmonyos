//! UI ownership of shader fills and effects. All GPU programs and windows run
//! on the surface's GLES compositor worker; the UI only sends changed state.

mod effects;
pub(crate) mod navigation;

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::Arc,
    time::Duration,
};

use nsh_core::{NodeId, animation::AnimationValue, shader::ShaderLayer};
use ohos_arkui_binding::{
    component::attribute::ArkUICommonAttribute, types::advanced::NodeDirtyFlag,
};
use rustc_hash::FxHashMap;

use super::{
    composition::{
        fill::{Command, Key},
        surface,
        worker::Worker,
    },
    geometry::Size,
    node::{NativeNode, Node, Result, callback},
};
use crate::app::frame_rate::FrameRate;

/// The shader views of a surface, on the UI thread.
pub(crate) struct Shaders {
    effects: effects::Effects,
    /// The surface's VSync, which paces the worker.
    rate: Arc<FrameRate>,
    worker: RefCell<Option<Rc<Worker>>>,
    generation: Cell<u32>,
    visibility_generation: Cell<u64>,
    visibility: RefCell<FxHashMap<NodeId, (u64, bool, bool)>>,
    views: RefCell<FxHashMap<NodeId, State>>,
    /// A generation distinguishes windows retired before an id is reused.
    retiring: RefCell<Vec<(Key, Retiring)>>,
}

struct State {
    key: Key,
    owner: Weak<Node>,
    parent: NativeNode,
    surface: surface::Surface,
    /// What the worker was last told: the size and scale, whether it shows.
    size: Option<(Size, f32)>,
    shown: Option<bool>,
    overflow: f32,
}

/// A view's surface kept after it went, with the view's native node it is
/// in (kept too: the view leaves disposing it to this when it is held).
struct Retiring {
    surface: surface::Surface,
    parent: NativeNode,
}

impl Shaders {
    pub(crate) fn invalidate_visibility(&self) {
        self.visibility_generation
            .set(self.visibility_generation.get().wrapping_add(1));
    }
    fn visibility(&self, node: &Rc<Node>) -> (bool, bool) {
        let generation = self.visibility_generation.get();
        if let Some(&(old, attached, shown)) = self.visibility.borrow().get(&node.id)
            && old == generation
        {
            return (attached, shown);
        }
        let mut at = Some(node.clone());
        let mut attached = false;
        while let Some(node) = at {
            if node.id == nsh_core::ROOT_ID {
                attached = true;
                break;
            }
            at = node.parent_node.borrow().upgrade();
        }
        let shown = attached && in_sight_uncached(node);
        self.visibility
            .borrow_mut()
            .insert(node.id, (generation, attached, shown));
        (attached, shown)
    }
    pub(crate) fn frame_rate(&self) -> Arc<FrameRate> {
        self.rate.clone()
    }
    #[cfg(feature = "dev")]
    pub(crate) fn animating(&self) -> bool {
        self.effects.animating()
    }

    pub(crate) fn new(rate: Arc<FrameRate>) -> Shaders {
        Shaders {
            effects: effects::Effects::default(),
            rate,
            worker: RefCell::default(),
            generation: Cell::new(0),
            visibility_generation: Cell::new(1),
            visibility: RefCell::default(),
            views: RefCell::default(),
            retiring: RefCell::default(),
        }
    }

    pub(crate) fn surface_active(&self) -> bool {
        self.effects.surface_active()
    }
    pub(crate) fn keep_alive(&self, id: NodeId) -> bool {
        self.effects.keep_alive(id)
    }
    pub(crate) fn has_navigation_layer(&self, target_id: NodeId) -> bool {
        self.effects.has_navigation_layer(target_id)
    }
    fn reconcile_hosts(&self) -> Result<()> {
        let has_fill_in_root = self.views.borrow().values().any(|state| {
            state
                .owner
                .upgrade()
                .is_some_and(|owner| effects::mounted_and_unhidden_in_root(&owner))
        });
        let fill_count = self.views.borrow().len();
        self.effects.reconcile_hosts(has_fill_in_root, fill_count)
    }

    pub(crate) fn set(&self, owner: &Rc<Node>, layer: Option<Arc<ShaderLayer>>) -> Result<()> {
        let Some(layer) = layer else {
            self.remove(owner.id);
            return Ok(());
        };
        self.effects.prewarm_root_early(owner)?;
        if layer.role != nsh_core::shader::ShaderRole::Fill {
            if self.views.borrow().contains_key(&owner.id) {
                self.remove(owner.id);
            }
            self.effects.set(owner, layer)?;
            if !owner.flushing_batch.get() {
                self.reconcile_hosts()?;
            }
            return Ok(());
        }
        self.effects.remove(owner.id)?;
        if !self.views.borrow().contains_key(&owner.id)
            && self.views.borrow().len() >= super::composition::worker::MAX_LAYERS
        {
            return Err(super::node::Error::Unsupported(
                "shader fill layer budget exceeded",
            ));
        }
        let worker = Worker::for_owner(owner)?;
        *self.worker.borrow_mut() = Some(worker.clone());
        let key = self
            .views
            .borrow()
            .get(&owner.id)
            .map(|state| state.key)
            .unwrap_or_else(|| {
                let generation = self.generation.get().wrapping_add(1).max(1);
                self.generation.set(generation);
                Key {
                    id: owner.id,
                    generation,
                }
            });
        let overflow = layer.overflow.0.max(0.0);
        worker.precompile(layer.clone());
        worker.fill(Command::Layer(key, layer));
        if !self.views.borrow().contains_key(&owner.id) {
            let surface = match surface::Surface::new(
                format!("nsh-shader-{}-{}", key.id, key.generation),
                worker.fill_window(key),
                true,
            ) {
                Ok(surface) => surface,
                Err(error) => {
                    worker.fill(Command::Remove(key));
                    return Err(error);
                }
            };
            // Under the app's children, after the other platform-owned ones.
            let index = owner.child_index(0);
            if let Err(error) = owner
                .native
                .borrow_mut()
                .insert_child(surface.native.clone(), index)
            {
                worker.fill(Command::Remove(key));
                callback(|| surface.dispose(None));
                return Err(error.into());
            }
            self.views.borrow_mut().insert(
                owner.id,
                State {
                    key,
                    owner: Rc::downgrade(owner),
                    parent: owner.native.clone(),
                    surface,
                    size: None,
                    shown: None,
                    overflow,
                },
            );
            owner.enable_layout();
        }
        if let Some(state) = self.views.borrow_mut().get_mut(&owner.id) {
            state.overflow = overflow;
            state.surface.native.borrow().set_clip(overflow <= 0.0)?;
        }
        owner.invalidate_composites();
        owner.animator.request_frame();
        if !owner.flushing_batch.get() {
            self.reconcile_hosts()?;
        }
        Ok(())
    }

    pub(crate) fn contains(&self, id: NodeId) -> bool {
        self.views.borrow().contains_key(&id)
    }
    pub(crate) fn root(&self, root: &Rc<Node>) {
        self.effects.root(root);
    }
    pub(crate) fn transition(&self, id: NodeId, duration: Duration) {
        self.effects.transition(id, duration);
    }
    pub(crate) fn begin_navigation(
        &self,
        owner: &Rc<Node>,
        lower: Option<Rc<Node>>,
        backwards: bool,
    ) -> Result<Option<Rc<navigation::Navigation>>> {
        self.effects.begin_navigation(owner, lower, backwards)
    }
    pub(crate) fn navigation_event(&self, event: navigation::Event) -> Result<()> {
        self.effects.navigation_event(event)
    }
    pub(crate) fn end_navigation(&self, id: NodeId) -> Result<()> {
        self.effects.end_navigation(id)
    }
    pub(crate) fn reconcile_navigation_capture(&self, reduce_motion: bool) -> Result<()> {
        let capture = self.effects.reconcile_navigation_capture(reduce_motion);
        let hosts = self.reconcile_hosts();
        capture.and(hosts)
    }
    pub(crate) fn cancel_navigation(&self, reason: &str) -> Result<()> {
        self.effects.cancel_navigation(reason)
    }
    pub(crate) fn expire_captures(&self) -> Result<()> {
        self.effects.expire_captures()
    }
    pub(crate) fn log_idle_configured(&self) {
        self.effects.log_idle_configured();
    }

    /// The native node of `id`'s surface, which its view lays out.
    pub(crate) fn overflow(&self, id: NodeId) -> f32 {
        self.views
            .borrow()
            .get(&id)
            .map_or(0.0, |state| state.overflow)
            .max(self.effects.max_overflow(id))
    }
    pub(crate) fn native(&self, id: NodeId) -> Option<NativeNode> {
        self.views
            .borrow()
            .get(&id)
            .map(|state| state.surface.native.clone())
    }

    pub(crate) fn remove(&self, id: NodeId) {
        self.visibility.borrow_mut().remove(&id);
        callback(|| self.effects.remove(id));
        if let Some(state) = self.views.borrow_mut().remove(&id) {
            if let Some(owner) = state.owner.upgrade() {
                owner.invalidate_composites();
            }
            if let Some(worker) = self.worker.borrow().as_ref() {
                worker.fill(Command::Remove(state.key));
                self.retiring.borrow_mut().push((
                    state.key,
                    Retiring {
                        surface: state.surface,
                        parent: state.parent,
                    },
                ));
            } else {
                callback(|| state.surface.dispose(Some(&state.parent)));
            }
        }
        if self
            .effects
            .root_node()
            .is_some_and(|root| !root.flushing_batch.get())
        {
            callback(|| self.reconcile_hosts());
        }
    }

    pub(crate) fn slot(&self, id: NodeId, slot: u16, value: AnimationValue) {
        self.effects.slot(id, slot, value);
        if let Some(state) = self.views.borrow().get(&id)
            && let Some(worker) = self.worker.borrow().as_ref()
        {
            worker.fill(Command::Slot(state.key, slot, value));
        }
    }

    /// A display frame: tells the worker what changed of its views (in
    /// sight, size) and gives each surface its view's box. The worker keeps
    /// its own pace, so this never asks for another frame.
    pub(crate) fn frame(&self) -> Result<bool> {
        let effects_due = self.effects.frame()?;
        let worker = self.worker.borrow();
        let Some(worker) = worker.as_ref() else {
            return Ok(effects_due);
        };
        let mut views = self.views.borrow_mut();
        for state in views.values_mut() {
            let Some(owner) = state.owner.upgrade() else {
                continue;
            };
            let shown = in_sight(&owner);
            if state.shown != Some(shown) {
                state.shown = Some(shown);
                worker.fill(Command::Shown(state.key, shown));
            }
            let frame = owner.frame.get();
            let size = (Size::new(frame.width, frame.height), owner.scale.get());
            if state.size != Some(size) {
                state.size = Some(size);
                worker.fill(Command::Size(state.key, size.0, size.1));
            }
            if state.surface.place(
                (
                    frame.width + 2.0 * state.overflow,
                    frame.height + 2.0 * state.overflow,
                ),
                if state.overflow > 0.0 {
                    [0.0; 4]
                } else {
                    owner.border_radii()
                },
            )? {
                owner
                    .native
                    .borrow()
                    .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
            }
        }
        Ok(effects_due)
    }

    /// The app came back to the foreground: a VSync that stopped while it was
    /// away left the worker waiting for a command.
    pub(crate) fn wake(&self) {
        self.effects.wake();
        let _ = self.reconcile_hosts();
        if let Some(root) = self.effects.root_node() {
            let _ = root.flush_arkui();
        }
        if self.effects.surface_active()
            && let Some(worker) = self.worker.borrow().as_ref()
        {
            worker.wake();
        }
    }

    /// Scroll views moved: tells the worker which views came into sight or
    /// left it.
    pub(crate) fn reveal(&self) -> Result<()> {
        self.frame().map(|_| ())
    }

    pub(crate) fn close(&self) -> Result<()> {
        self.effects.cancel_navigation("close")?;
        let ids: Vec<_> = self.views.borrow().keys().copied().collect();
        for id in ids {
            self.remove(id);
        }
        self.collect()
    }
    /// `Message::Shader`: releases the surfaces of views the worker has let
    /// go of.
    pub(crate) fn collect(&self) -> Result<()> {
        let worker = self.worker.borrow();
        let Some(worker) = worker.as_ref() else {
            return Ok(());
        };
        let retired = worker.take_retired_fills();
        let mut retiring = self.retiring.borrow_mut();
        for id in retired {
            let Some(at) = retiring.iter().position(|(r, _)| *r == id) else {
                continue;
            };
            let (_, Retiring { surface, parent }) = retiring.remove(at);
            callback(|| surface.dispose(Some(&parent)));
            // The view went meanwhile and left its native node to this.
            if parent.borrow().is_owned() && Rc::strong_count(&parent) == 1 {
                callback(|| {
                    parent.borrow().remove_from_parent()?;
                    parent.borrow_mut().dispose()?;
                    Ok(())
                });
            }
        }
        Ok(())
    }
}

/// Whether `node` can be seen: in the tree and shown, and its box (as
/// ArkUI places it, translations and scroll offsets included) within the
/// surface's root and within every scroll view around it, which clip their
/// content (a row scrolled away, a card below the fold).
fn in_sight(node: &Rc<Node>) -> bool {
    node.shaders.visibility(node).1
}
fn in_sight_uncached(node: &Rc<Node>) -> bool {
    if !node.visible_in_tree() {
        return false;
    }
    let rect = |n: &Node| -> Option<(i32, i32, i32, i32)> {
        let native = n.mounted();
        let native = native.borrow();
        let origin = native.position_with_translate_in_window().ok()?;
        let size = native.layout_size().ok()?;
        Some((
            origin.x,
            origin.y,
            origin.x + size.width,
            origin.y + size.height,
        ))
    };
    let Some(own) = rect(node) else {
        return true;
    };
    if own.2 <= own.0 || own.3 <= own.1 {
        return false;
    }
    let mut at = node.parent_node.borrow().upgrade();
    while let Some(ancestor) = at {
        let clips = ancestor.id == nsh_core::ROOT_ID || ancestor.scroll.borrow().is_some();
        if clips
            && let Some(r) = rect(&ancestor)
            && (own.0 >= r.2 || own.2 <= r.0 || own.1 >= r.3 || own.3 <= r.1)
        {
            return false;
        }
        at = ancestor.parent_node.borrow().upgrade();
    }
    true
}
