//! UI ownership of shader effects; native texture exports retain live input.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use nsh_core::{
    NavShader, NodeId, ViewKind,
    animation::AnimationValue,
    shader::{ShaderLayer, ShaderRole},
};
use rustc_hash::FxHashMap;

use super::navigation::{CAPTURE_DEADLINE, CaptureDemands, Event, Key, Navigation};
use crate::render::node::{Node, Result};

#[derive(Default)]
pub(super) struct Effects {
    root: RefCell<Weak<Node>>,
    views: RefCell<FxHashMap<NodeId, State>>,
    pending: RefCell<FxHashMap<NodeId, PendingNavigation>>,
    demands: RefCell<CaptureDemands>,
    capture_nodes: RefCell<FxHashMap<NodeId, Weak<Node>>>,
    logged_state: Cell<Option<(usize, usize)>>,
}

struct State {
    owner: Weak<Node>,
    target: Weak<Node>,
    layer: Arc<ShaderLayer>,
    active: Cell<bool>,
    navigation: Option<Rc<Navigation>>,
    #[cfg(feature = "dev")]
    transition_until: Cell<Option<Instant>>,
}

struct PendingNavigation {
    owner: Weak<Node>,
    target: Weak<Node>,
    picture_id: NodeId,
    spec: NavShader,
    navigation: Rc<Navigation>,
    page: bool,
}

impl Effects {
    fn surface_active(&self) -> bool {
        self.root.borrow().upgrade().is_some_and(|root| {
            root.dispatcher
                .upgrade()
                .is_some_and(|d| d.active.load(Ordering::Acquire))
        })
    }

    fn cancel_on_worker(&self, keys: &[Key]) {
        if keys.is_empty() {
            return;
        }
        if let Some(root) = self.root.borrow().upgrade()
            && let Some(worker) = root.compositor.borrow().as_ref()
        {
            for &key in keys {
                worker.cancel_capture(key);
            }
        }
    }

    fn sync_capture_roots(&self, reason: &str, force_log: bool) -> Result<()> {
        let root_node = self.root.borrow().upgrade();
        let allow_capture = root_node.as_ref().is_some_and(|root| {
            !root.platform.get().reduce_motion
                && root
                    .dispatcher
                    .upgrade()
                    .is_some_and(|d| d.active.load(Ordering::Acquire))
        });
        let active_demands = self.demands.borrow().len();

        let mut candidates: Vec<(NodeId, Rc<Node>, bool)> = Vec::new();
        {
            let demands = self.demands.borrow();
            let mut nodes = self.capture_nodes.borrow_mut();
            let mut stale = Vec::new();
            for (&id, weak) in nodes.iter() {
                if let Some(node) = weak.upgrade() {
                    let wanted = allow_capture && demands.contains_picture(id);
                    candidates.push((id, node, wanted));
                    if !wanted {
                        stale.push(id);
                    }
                } else {
                    stale.push(id);
                }
            }
            for id in stale {
                nodes.remove(&id);
            }
            if let Some(root) = &root_node
                && !candidates.iter().any(|(id, _, _)| *id == root.id)
            {
                let wanted = allow_capture && demands.contains_picture(root.id);
                candidates.push((root.id, root.clone(), wanted));
            }
        }

        for (_, node, wanted) in &candidates {
            let capture = *wanted && node.active.get();
            if node.navigation_capture.replace(capture) != capture && node.active.get() {
                if let Some(scene) = node.scene.borrow().as_ref() {
                    scene.require(node);
                }
                node.invalidate_composites();
                node.ensure_wrapper()?;
                node.restore_style()?;
            }
        }

        let nav_exported = {
            let mut count = self
                .capture_nodes
                .borrow()
                .values()
                .filter_map(Weak::upgrade)
                .filter(|node| node.active.get() && node.navigation_capture.get())
                .count();
            if let Some(root) = &root_node
                && !self.capture_nodes.borrow().contains_key(&root.id)
                && root.active.get()
                && root.navigation_capture.get()
            {
                count += 1;
            }
            count
        };
        let total_composites = root_node.as_ref().map_or(0, |root| {
            root.composite_nodes
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .filter(|node| {
                    node.active.get()
                        && node.wrapper.borrow().as_ref().is_some_and(|wrapper| {
                            wrapper.composite.borrow().is_some() && !wrapper.retiring_export()
                        })
                })
                .count()
        });

        let snapshot_state = (active_demands, nav_exported);
        if force_log || self.logged_state.replace(Some(snapshot_state)) != Some(snapshot_state) {
            log::info!(
                "[NSH] nav-capture demand={reason} active={active_demands} exported={nav_exported} composites={total_composites}"
            );
        }
        Ok(())
    }

    pub(super) fn log_idle_configured(&self) {
        if self.demands.borrow().is_empty() {
            let _ = self.sync_capture_roots("idle-configured", true);
        }
    }

    pub(super) fn reconcile_navigation_capture(&self, reduce_motion: bool) -> Result<()> {
        if reduce_motion {
            return self.cancel_navigation("reduce-motion");
        }
        self.sync_capture_roots("reconcile", false)
    }

    pub(super) fn cancel_navigation(&self, reason: &str) -> Result<()> {
        let fallback = reason == "reduce-motion";
        let pending: Vec<PendingNavigation> =
            self.pending.borrow_mut().drain().map(|(_, p)| p).collect();
        let cleared = self.demands.borrow_mut().clear();
        self.cancel_on_worker(&cleared);
        for item in pending {
            if fallback {
                item.navigation.mark_fallback(item.navigation.key);
            } else {
                item.navigation.complete(item.navigation.key);
            }
            if let Some(owner) = item.owner.upgrade() {
                owner.animator.track(&owner);
                owner.animator.request_frame();
            }
        }

        let nav_owners: Vec<(NodeId, Option<Rc<Node>>)> = self
            .views
            .borrow()
            .iter()
            .filter_map(|(&id, state)| {
                let nav = state.navigation.as_ref()?;
                nav.complete(nav.key);
                Some((id, state.owner.upgrade()))
            })
            .collect();
        for (id, owner) in nav_owners {
            self.detach_view(id)?;
            if let Some(owner) = owner {
                owner.animator.track(&owner);
                owner.animator.request_frame();
            }
        }
        self.sync_capture_roots(reason, false)
    }

    pub(super) fn expire_captures(&self) -> Result<()> {
        let expired = self.demands.borrow_mut().expire(Instant::now());
        if expired.is_empty() {
            return Ok(());
        }
        self.cancel_on_worker(&expired);
        let mut fallen_back = Vec::new();
        {
            let mut pending = self.pending.borrow_mut();
            for &key in &expired {
                if pending
                    .get(&key.owner)
                    .is_some_and(|entry| entry.navigation.key == key)
                    && let Some(entry) = pending.remove(&key.owner)
                {
                    fallen_back.push(entry);
                }
            }
        }
        for entry in fallen_back {
            entry.navigation.mark_fallback(entry.navigation.key);
            if let Some(owner) = entry.owner.upgrade() {
                owner.animator.track(&owner);
                owner.animator.request_frame();
            }
        }
        self.sync_capture_roots("deadline", false)
    }

    pub(super) fn wake(&self) {
        if !self.surface_active() {
            let _ = self.cancel_navigation("background");
            return;
        }
        if let Some(root) = self.root.borrow().upgrade()
            && let Some(worker) = root.compositor.borrow().as_ref()
        {
            worker.wake();
        }
    }

    pub(super) fn root(&self, root: &Rc<Node>) {
        *self.root.borrow_mut() = Rc::downgrade(root);
    }

    pub(super) fn set(&self, owner: &Rc<Node>, layer: Arc<ShaderLayer>) -> Result<()> {
        let target = if matches!(
            layer.role,
            ShaderRole::Screen | ShaderRole::ScreenTransition
        ) {
            self.root.borrow().upgrade()
        } else {
            Some(owner.clone())
        };
        let Some(target) = target else {
            return Ok(());
        };
        self.set_on(owner, &target, layer, None)
    }

    fn set_on(
        &self,
        owner: &Rc<Node>,
        target: &Rc<Node>,
        layer: Arc<ShaderLayer>,
        navigation: Option<Rc<Navigation>>,
    ) -> Result<()> {
        let moved = self.views.borrow().get(&owner.id).is_some_and(|state| {
            state
                .target
                .upgrade()
                .is_none_or(|old| !Rc::ptr_eq(&old, target))
        });
        if moved {
            self.detach_view(owner.id)?;
        }
        self.views.borrow_mut().insert(
            owner.id,
            State {
                owner: Rc::downgrade(owner),
                target: Rc::downgrade(target),
                layer: layer.clone(),
                active: Cell::new(true),
                navigation,
                #[cfg(feature = "dev")]
                transition_until: Cell::new(None),
            },
        );
        {
            let mut layers = target.shader_layers.borrow_mut();
            if let Some((_, old)) = layers.iter_mut().find(|(id, _)| *id == owner.id) {
                *old = layer;
            } else {
                layers.push((owner.id, layer));
            }
        }
        target.invalidate_composites();
        if let Some(scene) = target.scene.borrow().as_ref() {
            scene.require(target);
        }
        target.ensure_wrapper()?;
        target.restore_style()?;
        target.animator.request_frame();
        Ok(())
    }

    fn detach_view(&self, id: NodeId) -> Result<()> {
        let state = self.views.borrow_mut().remove(&id);
        if let Some(state) = state
            && let Some(target) = state.target.upgrade()
        {
            target
                .shader_layers
                .borrow_mut()
                .retain(|(owner, _)| *owner != id);
            target.invalidate_composites();
            if let Some(scene) = target.scene.borrow().as_ref() {
                scene.require(&target);
            }
            if target.active.get() {
                target.ensure_wrapper()?;
                target.restore_style()?;
            }
        }
        Ok(())
    }

    pub(super) fn end_navigation(&self, id: NodeId) -> Result<()> {
        let pending = self.pending.borrow_mut().remove(&id);
        let mut cancel_keys = Vec::new();
        if let Some(pending) = pending {
            pending.navigation.complete(pending.navigation.key);
            cancel_keys.push(pending.navigation.key);
        }
        let released = self.demands.borrow_mut().release_node(id);
        for key in released {
            if !cancel_keys.contains(&key) {
                cancel_keys.push(key);
            }
        }
        self.cancel_on_worker(&cancel_keys);
        let is_nav = self
            .views
            .borrow()
            .get(&id)
            .is_some_and(|state| state.navigation.is_some());
        if is_nav {
            self.detach_view(id)?;
        }
        self.sync_capture_roots("end", false)
    }

    pub(super) fn remove(&self, id: NodeId) -> Result<()> {
        let mut removed_pending = Vec::new();
        {
            let mut pending = self.pending.borrow_mut();
            let matching: Vec<NodeId> = pending
                .iter()
                .filter_map(|(&owner_id, item)| {
                    let target_matches = item.target.upgrade().is_some_and(|t| t.id == id);
                    (owner_id == id || item.picture_id == id || target_matches).then_some(owner_id)
                })
                .collect();
            for owner_id in matching {
                if let Some(item) = pending.remove(&owner_id) {
                    removed_pending.push(item);
                }
            }
        }
        let mut cancel_keys = self.demands.borrow_mut().release_node(id);
        for item in removed_pending {
            item.navigation.complete(item.navigation.key);
            if !cancel_keys.contains(&item.navigation.key) {
                cancel_keys.push(item.navigation.key);
            }
            if let Some(owner) = item.owner.upgrade() {
                owner.animator.track(&owner);
                owner.animator.request_frame();
            }
        }
        self.cancel_on_worker(&cancel_keys);

        let target_nav_owners: Vec<NodeId> = self
            .views
            .borrow()
            .iter()
            .filter_map(|(&owner_id, state)| {
                let target_matches = state.target.upgrade().is_some_and(|t| t.id == id);
                (owner_id != id && target_matches && state.navigation.is_some()).then(|| {
                    if let Some(nav) = &state.navigation {
                        nav.complete(nav.key);
                    }
                    owner_id
                })
            })
            .collect();
        self.detach_view(id)?;
        for owner_id in target_nav_owners {
            self.detach_view(owner_id)?;
        }
        self.sync_capture_roots("remove", false)
    }

    pub(super) fn begin_navigation(
        &self,
        owner: &Rc<Node>,
        lower: Option<Rc<Node>>,
        backwards: bool,
    ) -> Result<Option<Rc<Navigation>>> {
        let Some(spec) = owner
            .navigation_shader
            .borrow()
            .clone()
            .filter(|_| !owner.platform.get().reduce_motion && self.surface_active())
        else {
            return Ok(None);
        };
        let Some(mut root) = self.root.borrow().upgrade() else {
            return Ok(None);
        };
        let mut ancestor = owner.parent_node.borrow().upgrade();
        while let Some(node) = ancestor {
            if node.kind == ViewKind::Modal {
                root = node;
                break;
            }
            ancestor = node.parent_node.borrow().upgrade();
        }
        let page = owner.kind == ViewKind::Screen;
        // Pages compose on the root, where the native tree supplies the live
        // destination after push/pop. Dialogs own a separate native surface.
        let target = if page { root.clone() } else { owner.clone() };
        let picture = if page {
            root.clone()
        } else {
            lower.unwrap_or(root)
        };
        let picture_frame = picture.frame.get();
        let target_frame = target.frame.get();
        if !picture.active.get()
            || !target.active.get()
            || picture_frame.width <= 0.0
            || picture_frame.height <= 0.0
            || target_frame.width <= 0.0
            || target_frame.height <= 0.0
        {
            return Ok(None);
        }

        let navigation = Rc::new(Navigation::new(owner.id, backwards).ok_or(
            crate::render::node::Error::Unsupported("navigation generation exhausted"),
        )?);
        let deadline = Instant::now() + CAPTURE_DEADLINE;
        self.capture_nodes
            .borrow_mut()
            .insert(picture.id, Rc::downgrade(&picture));
        if let Some(displaced) =
            self.demands
                .borrow_mut()
                .request(navigation.key, picture.id, deadline)
        {
            self.cancel_on_worker(&[displaced]);
        }

        // Enable demand-scoped capture on `picture` first so an existing Composite
        // (e.g. ScreenEffect or mid-transition interruption) stays live and a cold
        // root creates its Composite before Task::Capture is queued.
        self.sync_capture_roots("begin", false)?;

        let captured_queued = picture.wrapper.borrow().as_ref().is_some_and(|wrapper| {
            wrapper
                .composite
                .borrow()
                .as_ref()
                .filter(|composite| !composite.is_failed())
                .is_some_and(|composite| {
                    composite.capture(navigation.key, deadline);
                    true
                })
        });
        if !captured_queued {
            self.demands.borrow_mut().release(navigation.key);
            self.sync_capture_roots("failed", false)?;
            return Ok(None);
        }

        // Interrupt superseded pending captures or running navigations on this
        // owner or target after queueing Task::Capture so mid-flight GPU output
        // is snapshotted before old shader layers are detached.
        let superseded_pending: Vec<PendingNavigation> = {
            let mut pending = self.pending.borrow_mut();
            let ids: Vec<NodeId> = pending
                .iter()
                .filter_map(|(&id, item)| {
                    let same_target = item
                        .target
                        .upgrade()
                        .is_some_and(|old| Rc::ptr_eq(&old, &target));
                    (id == owner.id || same_target).then_some(id)
                })
                .collect();
            ids.into_iter()
                .filter_map(|id| pending.remove(&id))
                .collect()
        };
        let mut cancel_keys = Vec::new();
        for old in superseded_pending {
            old.navigation.complete(old.navigation.key);
            if self.demands.borrow_mut().release(old.navigation.key) {
                cancel_keys.push(old.navigation.key);
            }
            if let Some(old_owner) = old.owner.upgrade()
                && old_owner.id != owner.id
            {
                old_owner.animator.track(&old_owner);
                old_owner.animator.request_frame();
            }
        }
        self.cancel_on_worker(&cancel_keys);

        let interrupted: Vec<(Key, Option<Rc<Node>>)> = self
            .views
            .borrow()
            .values()
            .filter(|state| {
                state
                    .target
                    .upgrade()
                    .is_some_and(|old| Rc::ptr_eq(&old, &target))
            })
            .filter_map(|state| {
                state
                    .navigation
                    .as_ref()
                    .map(|n| (n.key, state.owner.upgrade()))
            })
            .collect();
        for (key, old_owner) in interrupted {
            self.navigation_complete(key);
            self.detach_view(key.owner)?;
            if let Some(old_owner) = old_owner
                && old_owner.id != owner.id
            {
                old_owner.animator.request_frame();
            }
        }

        self.pending.borrow_mut().insert(
            owner.id,
            PendingNavigation {
                owner: Rc::downgrade(owner),
                target: Rc::downgrade(&target),
                picture_id: picture.id,
                spec,
                navigation: navigation.clone(),
                page,
            },
        );
        self.sync_capture_roots("begin", false)?;
        picture.animator.request_frame();
        owner.animator.request_frame();
        Ok(Some(navigation))
    }

    pub(super) fn navigation_event(&self, event: Event) -> Result<()> {
        let key = event.key();
        let pending = {
            let mut map = self.pending.borrow_mut();
            if map
                .get(&key.owner)
                .is_some_and(|entry| entry.navigation.key == key)
            {
                map.remove(&key.owner)
            } else {
                None
            }
        };
        match event {
            Event::Captured(_) => {
                let Some(pending) = pending else {
                    if self.demands.borrow_mut().release(key) {
                        self.sync_capture_roots("stale-capture", false)?;
                    }
                    return Ok(());
                };
                let owner = pending.owner.upgrade();
                let target = pending.target.upgrade();
                let valid = owner.as_ref().is_some_and(|o| {
                    o.active.get() && !o.platform.get().reduce_motion && self.surface_active()
                }) && target.as_ref().is_some_and(|t| t.active.get());
                if !valid {
                    if owner
                        .as_ref()
                        .is_some_and(|o| o.platform.get().reduce_motion)
                    {
                        pending.navigation.mark_fallback(key);
                    } else {
                        pending.navigation.complete(key);
                    }
                    self.demands.borrow_mut().release(key);
                    self.sync_capture_roots("cancelled", false)?;
                    if let Some(owner) = owner {
                        owner.animator.track(&owner);
                        owner.animator.request_frame();
                    }
                    return Ok(());
                }
                let owner = owner.unwrap();
                let target = target.unwrap();
                if !pending.navigation.mark_running(key) {
                    self.demands.borrow_mut().release(key);
                    self.sync_capture_roots("cancelled", false)?;
                    return Ok(());
                }

                // Commit the live destination side now that the leaving snapshot
                // is safely copied into its standalone GPU target on the worker.
                if let Some(screen) = owner.screen.borrow().as_ref() {
                    screen.on_shader_captured(&pending.navigation)?;
                }
                if let Some(modal) = owner.modal.borrow().as_ref() {
                    modal.on_shader_captured(&pending.navigation)?;
                }

                // Attach the transition shader layer to `target` BEFORE releasing
                // `picture`'s capture demand so `target == picture` keeps its
                // Composite continuously without retiring between phases.
                self.set_on(
                    &owner,
                    &target,
                    pending.spec.layer,
                    Some(pending.navigation.clone()),
                )?;
                if let Some(modal) = owner.modal.borrow().as_ref() {
                    modal.refresh_presentation()?;
                }

                self.demands.borrow_mut().release(key);
                self.sync_capture_roots("captured", false)?;

                let started = target.wrapper.borrow().as_ref().is_some_and(|wrapper| {
                    wrapper
                        .composite
                        .borrow()
                        .as_ref()
                        .filter(|composite| !composite.is_failed())
                        .is_some_and(|composite| {
                            composite.shader_navigation(
                                key,
                                pending.spec.duration,
                                pending.navigation.backwards,
                                pending.page && pending.navigation.backwards,
                            );
                            true
                        })
                });
                if !started {
                    self.detach_view(owner.id)?;
                    pending.navigation.complete(key);
                    owner.animator.track(&owner);
                    owner.animator.request_frame();
                }
                Ok(())
            }
            Event::Failed(_) => {
                let released = self.demands.borrow_mut().release(key);
                if let Some(pending) = pending {
                    pending.navigation.mark_fallback(key);
                    if let Some(owner) = pending.owner.upgrade() {
                        owner.animator.track(&owner);
                        owner.animator.request_frame();
                    }
                    self.sync_capture_roots("fallback", false)?;
                } else if released {
                    self.sync_capture_roots("fallback", false)?;
                }
                Ok(())
            }
            Event::Completed(_) => {
                let released = self.demands.borrow_mut().release(key);
                if let Some(pending) = pending {
                    pending.navigation.complete(key);
                    if let Some(owner) = pending.owner.upgrade() {
                        owner.animator.track(&owner);
                        owner.animator.request_frame();
                    }
                }
                if released {
                    self.sync_capture_roots("complete", false)?;
                }
                self.navigation_complete(key);
                Ok(())
            }
        }
    }

    fn navigation_complete(&self, key: Key) {
        let owner = {
            let views = self.views.borrow();
            let Some(state) = views.get(&key.owner) else {
                return;
            };
            let Some(navigation) = state.navigation.as_ref() else {
                return;
            };
            if !navigation.complete(key) {
                return;
            }
            state.owner.upgrade()
        };
        if let Some(owner) = owner {
            owner.animator.request_frame();
        }
    }

    pub(super) fn slot(&self, id: NodeId, slot: u16, value: AnimationValue) {
        let target = self
            .views
            .borrow()
            .get(&id)
            .and_then(|state| state.target.upgrade());
        if let Some(target) = target
            && let Some(wrapper) = target.wrapper.borrow().as_ref()
            && let Some(composite) = wrapper.composite.borrow().as_ref()
        {
            composite.shader_slot(id, slot, value);
        }
    }

    pub(super) fn transition(&self, id: NodeId, duration: Duration) {
        let views = self.views.borrow();
        let Some(state) = views.get(&id) else {
            return;
        };
        let Some(target) = state.target.upgrade() else {
            return;
        };
        if !target.platform.get().reduce_motion
            && let Some(wrapper) = target.wrapper.borrow().as_ref()
            && let Some(composite) = wrapper.composite.borrow().as_ref()
        {
            #[cfg(feature = "dev")]
            if !duration.is_zero() {
                state.transition_until.set(Some(Instant::now() + duration));
            }
            composite.shader_transition(id, duration);
            drop(views);
            target.animator.request_frame();
        }
    }

    #[cfg(feature = "dev")]
    pub(super) fn animating(&self) -> bool {
        let now = Instant::now();
        !self.pending.borrow().is_empty()
            || self.views.borrow().values().any(|state| {
                state.navigation.as_ref().is_some_and(|nav| nav.running())
                    || state
                        .transition_until
                        .get()
                        .is_some_and(|until| now < until)
            })
    }

    pub(super) fn frame(&self) -> Result<bool> {
        self.expire_captures()?;
        let mut due = !self.demands.borrow().is_empty();
        let mut targets: FxHashMap<NodeId, (Rc<Node>, bool)> = FxHashMap::default();
        // Restoring a native style reapplies animated slots, which reads this
        // table again. Keep membership immutable throughout the frame.
        for (&id, state) in self.views.borrow().iter() {
            let Some(owner) = state.owner.upgrade() else {
                continue;
            };
            let Some(target) = state.target.upgrade() else {
                continue;
            };
            let attached = owner.shaders.visibility(&owner).0;
            let active = if state.navigation.is_some() {
                // Closing dialogs can already be deactivated; their retained
                // native picture must keep ticking until navigation ends.
                true
            } else {
                // A screen shader belongs to the surface until unmounted,
                // even when its declaring page is behind another page. This
                // matches UIKit/AppKit and keeps the root's render ownership
                // stable throughout a navigation.
                attached
            };
            if active != state.active.replace(active) {
                target
                    .shader_layers
                    .borrow_mut()
                    .retain(|(owner, _)| *owner != id);
                if active {
                    target
                        .shader_layers
                        .borrow_mut()
                        .push((id, state.layer.clone()));
                }
                target.invalidate_composites();
                if let Some(scene) = target.scene.borrow().as_ref() {
                    scene.require(&target);
                }
                target.ensure_wrapper()?;
                target.restore_style()?;
            }
            let shown = active && (state.navigation.is_some() || super::in_sight(&target));
            let entry = targets.entry(target.id).or_insert((target, false));
            entry.1 |= shown;
        }
        // Keep demanded picture roots active on the compositor while awaiting
        // their initial export frame.
        let capture_targets: Vec<Rc<Node>> = self
            .capture_nodes
            .borrow()
            .values()
            .filter_map(Weak::upgrade)
            .filter(|node| node.active.get() && node.navigation_capture.get())
            .collect();
        for picture in capture_targets {
            let entry = targets.entry(picture.id).or_insert((picture, false));
            entry.1 = true;
        }
        for (_, (target, shown)) in targets {
            due |= target.wrapper.borrow().as_ref().is_some_and(|wrapper| {
                wrapper
                    .composite
                    .borrow()
                    .as_ref()
                    .is_some_and(|composite| composite.shader_frame(shown))
            });
        }
        Ok(due)
    }
}
