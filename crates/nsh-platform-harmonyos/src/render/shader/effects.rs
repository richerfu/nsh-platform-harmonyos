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

use super::navigation::{CAPTURE_DEADLINE, CaptureDemands, Event, HeldViews, Key, Navigation};
use crate::render::node::{Node, Result};

pub(super) fn unhidden_chain(node: &Rc<Node>) -> bool {
    let mut at = Some(node.clone());
    while let Some(current) = at {
        if !current.active.get() || current.retained.get() || current.style.borrow().hidden {
            return false;
        }
        at = current.parent_node.borrow().upgrade();
    }
    true
}

pub(super) fn mounted_and_unhidden_in_root(node: &Rc<Node>) -> bool {
    let mut at = Some(node.clone());
    let mut reached_root = false;
    while let Some(current) = at {
        if !current.active.get() || current.retained.get() || current.style.borrow().hidden {
            return false;
        }
        if current.id == nsh_core::ROOT_ID {
            reached_root = true;
            break;
        }
        at = current.parent_node.borrow().upgrade();
    }
    reached_root
}

fn enclosing_screen_scope(owner: &Rc<Node>) -> Option<Rc<Node>> {
    let mut at = Some(owner.clone());
    let mut scope: Option<Rc<Node>> = None;
    let mut root: Option<Rc<Node>> = None;
    while let Some(current) = at {
        if !current.active.get() || current.retained.get() {
            return None;
        }
        if scope.is_none() && matches!(current.kind, ViewKind::Screen | ViewKind::Modal) {
            scope = Some(current.clone());
        }
        if current.id == nsh_core::ROOT_ID {
            root = Some(current);
            break;
        }
        at = current.parent_node.borrow().upgrade();
    }
    let root = root?;
    Some(scope.unwrap_or(root))
}

#[derive(Default)]
pub(super) struct Effects {
    root: RefCell<Weak<Node>>,
    views: RefCell<FxHashMap<NodeId, State>>,
    pending: RefCell<FxHashMap<NodeId, PendingNavigation>>,
    demands: RefCell<CaptureDemands>,
    held: RefCell<HeldViews>,
    capture_nodes: RefCell<FxHashMap<NodeId, Weak<Node>>>,
    capable_nodes: RefCell<FxHashMap<NodeId, (Weak<Node>, f32)>>,
    modal_hosts: RefCell<FxHashMap<NodeId, (Weak<Node>, f32)>>,
    screen_effect_owners: RefCell<FxHashMap<NodeId, Weak<Node>>>,
    screen_scopes: RefCell<FxHashMap<NodeId, Weak<Node>>>,
    root_max_overflow: Cell<f32>,
    keep_alive_active: RefCell<FxHashMap<NodeId, bool>>,
    was_surface_active: Cell<bool>,
    was_hosts_retainable: Cell<bool>,
    logged_state: Cell<Option<(usize, usize)>>,
    logged_hosts: Cell<Option<(usize, usize, usize, usize)>>,
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
    pub(super) fn surface_active(&self) -> bool {
        self.root.borrow().upgrade().is_some_and(|root| {
            root.platform.get().lifecycle != nsh_engine::Lifecycle::Background
                && root
                    .dispatcher
                    .upgrade()
                    .is_some_and(|d| d.active.load(Ordering::Acquire))
        })
    }

    pub(super) fn hosts_retainable(&self) -> bool {
        self.root.borrow().upgrade().is_some_and(|root| {
            root.active.get()
                && root.host.check().is_ok()
                && root
                    .dispatcher
                    .upgrade()
                    .is_some_and(|d| d.live.load(Ordering::Acquire))
                && root
                    .wrapper
                    .try_borrow()
                    .ok()
                    .is_none_or(|wrapper| wrapper.as_ref().is_none_or(|w| w.is_context_live()))
        })
    }

    pub(super) fn root_node(&self) -> Option<Rc<Node>> {
        self.root.borrow().upgrade()
    }

    pub(super) fn keep_alive(&self, id: NodeId) -> bool {
        self.keep_alive_active
            .borrow()
            .get(&id)
            .copied()
            .unwrap_or(false)
    }

    pub(super) fn max_overflow(&self, id: NodeId) -> f32 {
        let own = self
            .capable_nodes
            .borrow()
            .get(&id)
            .map_or(0.0, |&(_, overflow)| overflow)
            .max(
                self.modal_hosts
                    .borrow()
                    .get(&id)
                    .map_or(0.0, |&(_, overflow)| overflow),
            );
        if id == nsh_core::ROOT_ID {
            own.max(self.root_max_overflow.get())
        } else {
            own
        }
    }

    fn modal_node_keep_alive(
        &self,
        node: &Rc<Node>,
        retainable: bool,
        reduce_motion: bool,
    ) -> bool {
        let node_hidden = node.style.borrow().hidden;
        let has_navigation_shader = node.navigation_shader.borrow().is_some();
        let modal_visible = node
            .modal
            .try_borrow()
            .ok()
            .is_some_and(|modal| modal.as_ref().is_some_and(|m| m.is_visible()));
        super::navigation::modal_host_keep_alive(super::navigation::ModalHostInputs {
            retainable,
            reduce_motion,
            node_active: node.active.get(),
            node_retained: node.retained.get(),
            node_hidden,
            modal_visible,
            has_navigation_shader,
        })
    }

    fn drop_modal_host_keep_alive(&self, id: NodeId) {
        self.modal_hosts.borrow_mut().remove(&id);
        self.keep_alive_active.borrow_mut().remove(&id);
    }

    pub(super) fn finish_modal_close(&self, owner: &Rc<Node>, had_shader: bool) -> Result<()> {
        let removed_host = self.modal_hosts.borrow_mut().remove(&owner.id).is_some();
        let removed_keep = self
            .keep_alive_active
            .borrow_mut()
            .remove(&owner.id)
            .unwrap_or(false);
        let detaches_view = had_shader
            && !self.held.borrow().is_held(owner.id)
            && self
                .views
                .borrow()
                .get(&owner.id)
                .is_some_and(|state| state.navigation.is_some());
        if had_shader {
            self.end_navigation(owner.id)?;
        }
        if (removed_host || removed_keep) && !detaches_view && owner.active.get() {
            owner.invalidate_composites();
            if let Some(scene) = owner.scene.borrow().as_ref() {
                scene.require(owner);
            }
            owner.ensure_wrapper()?;
            owner.restore_style()?;
        }
        Ok(())
    }

    pub(super) fn has_navigation_layer(&self, target_id: NodeId) -> bool {
        self.views.borrow().values().any(|state| {
            state.navigation.is_some()
                && state
                    .target
                    .upgrade()
                    .is_some_and(|target| target.id == target_id)
        })
    }

    fn count_active_composites(root_node: Option<&Rc<Node>>) -> usize {
        root_node.map_or(0, |root| {
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
        })
    }

    pub(super) fn reconcile_hosts(&self, has_fill_in_root: bool, fill_count: usize) -> Result<()> {
        let root_node = self.root.borrow().upgrade();
        let active_surface = self.surface_active();
        let retainable = self.hosts_retainable();
        let surface_changed = self.was_surface_active.replace(active_surface) != active_surface;
        let retainable_changed = self.was_hosts_retainable.replace(retainable) != retainable;

        {
            let mut owners = self.screen_effect_owners.borrow_mut();
            let mut scopes = self.screen_scopes.borrow_mut();
            owners.retain(|_, weak| {
                let Some(owner) = weak.upgrade() else {
                    return false;
                };
                if !owner.active.get() || owner.retained.get() {
                    return false;
                }
                if let Some(scope) = enclosing_screen_scope(&owner) {
                    scopes.insert(scope.id, Rc::downgrade(&scope));
                }
                true
            });
            scopes.retain(|_, weak| {
                weak.upgrade()
                    .is_some_and(|scope| scope.active.get() && !scope.retained.get())
            });
        }

        let mut capable_list = Vec::new();
        {
            let mut capable = self.capable_nodes.borrow_mut();
            capable.retain(|&id, (weak, _)| {
                let Some(node) = weak.upgrade() else {
                    return false;
                };
                if !node.active.get() || node.retained.get() {
                    return false;
                }
                capable_list.push((id, node));
                true
            });
        }

        let mut modal_list = Vec::new();
        {
            let mut modals = self.modal_hosts.borrow_mut();
            modals.retain(|&id, (weak, _)| {
                let Some(node) = weak.upgrade() else {
                    return false;
                };
                if !node.active.get() || node.retained.get() {
                    return false;
                }
                modal_list.push((id, node));
                true
            });
        }

        let has_screen_views = self.views.borrow().values().any(|state| {
            state.navigation.is_none()
                && matches!(
                    state.layer.role,
                    ShaderRole::Screen | ShaderRole::ScreenTransition
                )
        });
        if !has_screen_views
            && self.screen_effect_owners.borrow().is_empty()
            && self.screen_scopes.borrow().is_empty()
        {
            self.root_max_overflow.set(0.0);
        }

        let mut desired: FxHashMap<NodeId, (Rc<Node>, bool)> = FxHashMap::default();
        for (id, node) in capable_list {
            let keep = retainable && mounted_and_unhidden_in_root(&node);
            desired.insert(id, (node, keep));
        }

        if let Some(root) = &root_node {
            let has_non_nav_effect_in_root = self.views.borrow().values().any(|state| {
                state.navigation.is_none()
                    && state
                        .owner
                        .upgrade()
                        .is_some_and(|owner| mounted_and_unhidden_in_root(&owner))
            });
            let has_screen_scope = self
                .screen_scopes
                .borrow()
                .values()
                .filter_map(Weak::upgrade)
                .any(|scope| mounted_and_unhidden_in_root(&scope));
            let root_capable = desired.get(&root.id).is_some_and(|&(_, keep)| keep);
            let root_has_active_composite = root.wrapper.borrow().as_ref().is_some_and(|wrapper| {
                wrapper.composite.borrow().is_some() && !wrapper.retiring_export()
            });
            let root_keep = retainable
                && mounted_and_unhidden_in_root(root)
                && (has_fill_in_root
                    || has_non_nav_effect_in_root
                    || has_screen_scope
                    || root_capable
                    || (root_has_active_composite && root.has_active_screen_transition()));
            desired.insert(root.id, (root.clone(), root_keep));
        }

        let reduce_motion = root_node
            .as_ref()
            .is_some_and(|root| root.platform.get().reduce_motion);
        let mut pruned_modals = Vec::new();
        for (id, node) in modal_list {
            let modal_keep = self.modal_node_keep_alive(&node, retainable, reduce_motion);
            let has_navigation_shader = node.navigation_shader.borrow().is_some();
            let modal_visible = node
                .modal
                .try_borrow()
                .ok()
                .is_some_and(|modal| modal.as_ref().is_some_and(|m| m.is_visible()));
            if !modal_visible || !has_navigation_shader {
                pruned_modals.push(id);
            }
            desired
                .entry(id)
                .and_modify(|entry| entry.1 |= modal_keep)
                .or_insert((node, modal_keep));
        }
        if !pruned_modals.is_empty() {
            let mut modals = self.modal_hosts.borrow_mut();
            for id in pruned_modals {
                modals.remove(&id);
            }
        }

        let mut to_refresh: Vec<Rc<Node>> = Vec::new();
        {
            let mut active_map = self.keep_alive_active.borrow_mut();
            let stale_keys: Vec<NodeId> = active_map
                .keys()
                .copied()
                .filter(|id| !desired.contains_key(id))
                .collect();
            for id in stale_keys {
                active_map.remove(&id);
            }
            for (&id, (node, keep)) in &desired {
                let prev = active_map.get(&id).copied().unwrap_or(false);
                if *keep {
                    active_map.insert(id, true);
                } else {
                    active_map.remove(&id);
                }
                let missing_composite = *keep
                    && !node.scene_active()
                    && node
                        .wrapper
                        .borrow()
                        .as_ref()
                        .is_none_or(|wrapper| wrapper.composite.borrow().is_none());
                if prev != *keep || missing_composite || retainable_changed || surface_changed {
                    to_refresh.push(node.clone());
                }
            }
        }

        if retainable_changed || surface_changed {
            let extra_targets: Vec<Rc<Node>> = self
                .views
                .borrow()
                .values()
                .filter_map(|state| state.target.upgrade())
                .filter(|target| !to_refresh.iter().any(|n| n.id == target.id))
                .collect();
            to_refresh.extend(extra_targets);
        }

        for node in to_refresh {
            node.invalidate_composites();
            if let Some(scene) = node.scene.borrow().as_ref() {
                scene.require(&node);
            }
            if node.active.get() {
                node.ensure_wrapper()?;
                node.restore_style()?;
            }
        }

        let active_composites = Self::count_active_composites(root_node.as_ref());
        let (keep_alive_hosts, passthrough_hosts) = {
            let active_map = self.keep_alive_active.borrow();
            let mut keep_count = 0usize;
            let mut passthrough_count = 0usize;
            for (&id, &keep) in active_map.iter() {
                if !keep {
                    continue;
                }
                keep_count += 1;
                let empty_layers = desired
                    .get(&id)
                    .is_some_and(|(node, _)| node.shader_layers.borrow().is_empty());
                if empty_layers {
                    passthrough_count += 1;
                }
            }
            (keep_count, passthrough_count)
        };
        let snap = (
            active_composites,
            keep_alive_hosts,
            passthrough_hosts,
            fill_count,
        );
        if self.logged_hosts.replace(Some(snap)) != Some(snap) {
            log::info!(
                "[NSH] shader-hosts active_composites={active_composites} keep_alive_hosts={keep_alive_hosts} passthrough_hosts={passthrough_hosts} fill_views={fill_count}"
            );
        }
        Ok(())
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
        let allow_capture = root_node
            .as_ref()
            .is_some_and(|root| !root.platform.get().reduce_motion && self.surface_active());
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
        self.held.borrow_mut().clear();
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
            if reason != "background" {
                self.drop_modal_host_keep_alive(id);
            }
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
        let mut detach_held = Vec::new();
        {
            let mut pending = self.pending.borrow_mut();
            let mut held = self.held.borrow_mut();
            for &key in &expired {
                detach_held.extend(held.release_holder(key, None));
                if pending
                    .get(&key.owner)
                    .is_some_and(|entry| entry.navigation.key == key)
                    && let Some(entry) = pending.remove(&key.owner)
                {
                    fallen_back.push(entry);
                }
            }
        }
        self.detach_navigation_views(&detach_held)?;
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

    pub(super) fn prewarm_root_early(&self, owner: &Rc<Node>) -> Result<()> {
        if !self.hosts_retainable() || !unhidden_chain(owner) {
            return Ok(());
        }
        let Some(root) = self.root.borrow().upgrade() else {
            return Ok(());
        };
        if !mounted_and_unhidden_in_root(&root) {
            return Ok(());
        }
        let already_warm = self.keep_alive(root.id)
            && root
                .wrapper
                .borrow()
                .as_ref()
                .is_some_and(|wrapper| wrapper.composite.borrow().is_some());
        if already_warm {
            return Ok(());
        }
        // Inside `Mounter::apply`, `reconcile_navigation()` -> `reconcile_hosts_now()`
        // reconciles `keep_alive_active`, `Composite`, and `RootBridge` atomically at
        // the end of the batch; returning before mutating `keep_alive_active` ensures
        // `reconcile_hosts_now()` sees `prev == false != true` and creates `Composite`.
        if root.flushing_batch.get() {
            return Ok(());
        }
        self.keep_alive_active.borrow_mut().insert(root.id, true);
        root.invalidate_composites();
        if let Some(scene) = root.scene.borrow().as_ref() {
            scene.require(&root);
        }
        root.ensure_wrapper()?;
        root.restore_style()?;
        Ok(())
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
        let is_non_nav = navigation.is_none();
        if is_non_nav {
            self.held.borrow_mut().remove_view(owner.id);
            if let Ok(worker) = crate::render::composition::worker::Worker::for_owner(target) {
                worker.precompile(layer.clone());
            }
            let overflow = layer.overflow.0.max(0.0);
            if matches!(
                layer.role,
                ShaderRole::Screen | ShaderRole::ScreenTransition
            ) {
                self.screen_effect_owners
                    .borrow_mut()
                    .insert(owner.id, Rc::downgrade(owner));
                if let Some(scope) = enclosing_screen_scope(owner) {
                    self.screen_scopes
                        .borrow_mut()
                        .insert(scope.id, Rc::downgrade(&scope));
                }
                self.root_max_overflow
                    .set(self.root_max_overflow.get().max(overflow));
            } else {
                let mut capable = self.capable_nodes.borrow_mut();
                let entry = capable
                    .entry(target.id)
                    .or_insert_with(|| (Rc::downgrade(target), 0.0));
                entry.0 = Rc::downgrade(target);
                entry.1 = entry.1.max(overflow);
            }
            if self.hosts_retainable() && unhidden_chain(owner) && unhidden_chain(target) {
                self.keep_alive_active.borrow_mut().insert(target.id, true);
            }
        } else if target.kind == ViewKind::Modal {
            let overflow = layer.overflow.0.max(0.0);
            {
                let mut modals = self.modal_hosts.borrow_mut();
                let entry = modals
                    .entry(target.id)
                    .or_insert_with(|| (Rc::downgrade(target), 0.0));
                entry.0 = Rc::downgrade(target);
                entry.1 = entry.1.max(overflow);
            }
            let retainable = self.hosts_retainable();
            let reduce_motion = target.platform.get().reduce_motion;
            if self.modal_node_keep_alive(target, retainable, reduce_motion) {
                self.keep_alive_active.borrow_mut().insert(target.id, true);
            }
        }
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
        let already_revealed = target.wrapper.borrow().as_ref().is_some_and(|wrapper| {
            wrapper
                .composite
                .borrow()
                .as_ref()
                .is_some_and(|composite| composite.is_revealed())
        });
        let (changed_layers, layer_count) = {
            let mut layers = target.shader_layers.borrow_mut();
            let changed = if let Some((_, old)) = layers.iter_mut().find(|(id, _)| *id == owner.id)
            {
                if *old != layer {
                    *old = layer;
                    true
                } else {
                    false
                }
            } else {
                layers.push((owner.id, layer));
                true
            };
            (changed, layers.len())
        };
        if is_non_nav && already_revealed && changed_layers {
            log::info!(
                "[NSH] shader-layer-swap node={} layers={} passthrough=false",
                target.id,
                layer_count,
            );
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
        self.held.borrow_mut().remove_view(id);
        let state = self.views.borrow_mut().remove(&id);
        if let Some(state) = state
            && let Some(target) = state.target.upgrade()
        {
            let is_non_nav = state.navigation.is_none();
            let (changed, next_len) = {
                let mut layers = target.shader_layers.borrow_mut();
                let prev_len = layers.len();
                layers.retain(|(owner, _)| *owner != id);
                let next_len = layers.len();
                (next_len != prev_len, next_len)
            };
            if changed
                && let Some(wrapper) = target.wrapper.borrow().as_ref()
                && let Some(composite) = wrapper.composite.borrow().as_ref()
            {
                if next_len == 0 {
                    composite.activate();
                }
                if is_non_nav && composite.is_revealed() && self.keep_alive(target.id) {
                    log::info!(
                        "[NSH] shader-layer-swap node={} layers={} passthrough={}",
                        target.id,
                        next_len,
                        next_len == 0,
                    );
                }
            }
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

    fn detach_navigation_views(&self, ids: &[NodeId]) -> Result<()> {
        for &id in ids {
            let is_nav = self
                .views
                .borrow()
                .get(&id)
                .is_some_and(|state| state.navigation.is_some());
            if is_nav {
                self.detach_view(id)?;
            } else {
                self.held.borrow_mut().remove_view(id);
            }
        }
        Ok(())
    }

    pub(super) fn end_navigation(&self, id: NodeId) -> Result<()> {
        let pending = self.pending.borrow_mut().remove(&id);
        let mut cancel_keys = Vec::new();
        let mut detach_held = Vec::new();
        if let Some(pending) = pending {
            pending.navigation.complete(pending.navigation.key);
            cancel_keys.push(pending.navigation.key);
            detach_held.extend(
                self.held
                    .borrow_mut()
                    .release_holder(pending.navigation.key, None),
            );
        }
        let released = self.demands.borrow_mut().release_node(id);
        for key in released {
            if !cancel_keys.contains(&key) {
                cancel_keys.push(key);
            }
            let matching_other = if key.owner != id {
                let mut map = self.pending.borrow_mut();
                if map.get(&key.owner).is_some_and(|p| p.navigation.key == key) {
                    map.remove(&key.owner)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(other) = matching_other {
                other.navigation.complete(other.navigation.key);
                detach_held.extend(self.held.borrow_mut().release_holder(key, None));
                if let Some(owner) = other.owner.upgrade() {
                    owner.animator.track(&owner);
                    owner.animator.request_frame();
                }
            }
        }
        self.cancel_on_worker(&cancel_keys);
        self.detach_navigation_views(&detach_held)?;
        let should_detach_self = !self.held.borrow().is_held(id)
            && self
                .views
                .borrow()
                .get(&id)
                .is_some_and(|state| state.navigation.is_some());
        if should_detach_self {
            self.detach_view(id)?;
        }
        self.sync_capture_roots("end", false)
    }

    pub(super) fn remove(&self, id: NodeId) -> Result<()> {
        self.drop_modal_host_keep_alive(id);
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
        let mut detach_held = Vec::new();
        for item in removed_pending {
            item.navigation.complete(item.navigation.key);
            detach_held.extend(
                self.held
                    .borrow_mut()
                    .release_holder(item.navigation.key, None),
            );
            if !cancel_keys.contains(&item.navigation.key) {
                cancel_keys.push(item.navigation.key);
            }
            if let Some(owner) = item.owner.upgrade() {
                owner.animator.track(&owner);
                owner.animator.request_frame();
            }
        }
        self.held.borrow_mut().remove_view(id);
        detach_held.extend(self.held.borrow_mut().remove_target(id));
        self.cancel_on_worker(&cancel_keys);
        self.detach_navigation_views(&detach_held)?;

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
        let displaced = self
            .demands
            .borrow_mut()
            .request(navigation.key, picture.id, deadline);
        if let Some(displaced) = displaced {
            self.cancel_on_worker(&[displaced]);
        }

        // Enable demand-scoped capture on `picture` while keeping any interrupted
        // shader layers on `target` intact so `Composite::update` neither wipes
        // `shaders.output()` ahead of `Task::Capture` nor retires an active Modal
        // `Composite` while `ROOT_ID` cold-captures.
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
            if let Some(displaced) = displaced {
                let stale_pending = {
                    let mut pending = self.pending.borrow_mut();
                    if pending
                        .get(&owner.id)
                        .is_some_and(|p| p.navigation.key == displaced)
                    {
                        pending.remove(&owner.id)
                    } else {
                        None
                    }
                };
                if let Some(old) = stale_pending {
                    old.navigation.complete(old.navigation.key);
                }
                let detach_held = self.held.borrow_mut().release_holder(displaced, None);
                self.detach_navigation_views(&detach_held)?;
            }
            self.sync_capture_roots("failed", false)?;
            return Ok(None);
        }

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
        let mut stale_held = Vec::new();
        for old in superseded_pending {
            old.navigation.complete(old.navigation.key);
            stale_held.extend(self.held.borrow_mut().transfer_or_release(
                old.navigation.key,
                navigation.key,
                target.id,
            ));
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
        self.detach_navigation_views(&stale_held)?;

        // Hold interrupted shader views on `target` until `navigation.key` resolves;
        // mark their Navigation completed so stale `Event::Completed(old_key)` is ignored.
        let interrupted: Vec<(NodeId, Rc<Navigation>, Option<Rc<Node>>)> = self
            .views
            .borrow()
            .iter()
            .filter(|(_, state)| {
                state
                    .target
                    .upgrade()
                    .is_some_and(|old| Rc::ptr_eq(&old, &target))
            })
            .filter_map(|(&view_id, state)| {
                state
                    .navigation
                    .as_ref()
                    .map(|nav| (view_id, nav.clone(), state.owner.upgrade()))
            })
            .collect();
        for (view_id, old_nav, old_owner) in interrupted {
            old_nav.complete(old_nav.key);
            self.held
                .borrow_mut()
                .hold(view_id, target.id, navigation.key);
            if let Some(old_owner) = old_owner
                && old_owner.id != owner.id
            {
                old_owner.animator.track(&old_owner);
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
                    let held = self.held.borrow_mut().release_holder(key, None);
                    self.detach_navigation_views(&held)?;
                    if self.demands.borrow_mut().release(key) || !held.is_empty() {
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
                    let held = self.held.borrow_mut().release_holder(key, None);
                    self.detach_navigation_views(&held)?;
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
                    let held = self.held.borrow_mut().release_holder(key, None);
                    self.detach_navigation_views(&held)?;
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

                // When `target` already owns a live Composite, queue `Task::Navigation`
                // ahead of `set_on`'s `Task::Wake` so the worker installs the new
                // snapshot before rendering with the updated shader layer list.
                let mut started = target.wrapper.borrow().as_ref().is_some_and(|wrapper| {
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

                // Attach `owner`'s transition shader layer BEFORE detaching any
                // cross-owner held layer or releasing `picture`'s demand so `target`
                // keeps `requires_texture == true` continuously across phases.
                self.set_on(
                    &owner,
                    &target,
                    pending.spec.layer,
                    Some(pending.navigation.clone()),
                )?;
                let held = self.held.borrow_mut().release_holder(key, Some(owner.id));
                self.detach_navigation_views(&held)?;

                if let Some(modal) = owner.modal.borrow().as_ref() {
                    modal.refresh_presentation()?;
                }

                self.demands.borrow_mut().release(key);
                self.sync_capture_roots("captured", false)?;

                if !started {
                    started = target.wrapper.borrow().as_ref().is_some_and(|wrapper| {
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
                }
                if !started {
                    self.drop_modal_host_keep_alive(owner.id);
                    self.detach_view(owner.id)?;
                    pending.navigation.mark_fallback(key);
                    owner.animator.track(&owner);
                    owner.animator.request_frame();
                }
                Ok(())
            }
            Event::Failed(_) => {
                let released = self.demands.borrow_mut().release(key);
                let held = self.held.borrow_mut().release_holder(key, None);
                self.detach_navigation_views(&held)?;
                if let Some(pending) = pending {
                    pending.navigation.mark_fallback(key);
                    if let Some(owner) = pending.owner.upgrade() {
                        owner.animator.track(&owner);
                        owner.animator.request_frame();
                    }
                    self.sync_capture_roots("fallback", false)?;
                } else {
                    let running_owner = {
                        let views = self.views.borrow();
                        views
                            .get(&key.owner)
                            .filter(|state| {
                                state.navigation.as_ref().is_some_and(|nav| nav.key == key)
                            })
                            .map(|state| {
                                if let Some(nav) = &state.navigation {
                                    nav.mark_fallback(key);
                                }
                                state.owner.upgrade()
                            })
                    };
                    if let Some(owner) = running_owner {
                        self.drop_modal_host_keep_alive(key.owner);
                        self.detach_view(key.owner)?;
                        if let Some(owner) = owner {
                            owner.animator.track(&owner);
                            owner.animator.request_frame();
                        }
                        self.sync_capture_roots("fallback", false)?;
                    } else if released || !held.is_empty() {
                        self.sync_capture_roots("fallback", false)?;
                    }
                }
                Ok(())
            }
            Event::Completed(_) => {
                let released = self.demands.borrow_mut().release(key);
                let held = self.held.borrow_mut().release_holder(key, None);
                self.detach_navigation_views(&held)?;
                if let Some(pending) = pending {
                    pending.navigation.complete(key);
                    if let Some(owner) = pending.owner.upgrade() {
                        owner.animator.track(&owner);
                        owner.animator.request_frame();
                    }
                }
                if released || !held.is_empty() {
                    self.sync_capture_roots("complete", false)?;
                }
                self.navigation_complete(key)?;
                Ok(())
            }
        }
    }

    fn navigation_complete(&self, key: Key) -> Result<()> {
        if self.held.borrow().is_held(key.owner) {
            return Ok(());
        }
        let owner = {
            let views = self.views.borrow();
            let Some(state) = views.get(&key.owner) else {
                return Ok(());
            };
            let Some(navigation) = state.navigation.as_ref() else {
                return Ok(());
            };
            if !navigation.complete(key) {
                return Ok(());
            }
            Some(state.owner.upgrade())
        };
        if let Some(owner) = owner {
            if let Some(owner) = owner {
                owner.animator.track(&owner);
                owner.animator.request_frame();
            } else {
                self.detach_view(key.owner)?;
            }
        }
        Ok(())
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
                attached && unhidden_chain(&owner)
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
        let keep_hosts: Vec<Rc<Node>> = {
            let root = self.root.borrow().upgrade();
            let capable = self.capable_nodes.borrow();
            let modals = self.modal_hosts.borrow();
            self.keep_alive_active
                .borrow()
                .iter()
                .filter(|(_, keep)| **keep)
                .filter_map(|(&id, _)| {
                    if id == nsh_core::ROOT_ID {
                        root.clone()
                    } else {
                        capable
                            .get(&id)
                            .or_else(|| modals.get(&id))
                            .and_then(|(weak, _)| weak.upgrade())
                    }
                })
                .filter(|node| node.active.get())
                .collect()
        };
        for host in keep_hosts {
            let shown = if host.kind == ViewKind::Modal {
                !host.style.borrow().hidden
                    && host
                        .modal
                        .try_borrow()
                        .ok()
                        .is_some_and(|modal| modal.as_ref().is_some_and(|m| m.is_visible()))
            } else {
                super::in_sight(&host)
            };
            let entry = targets.entry(host.id).or_insert((host, false));
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
