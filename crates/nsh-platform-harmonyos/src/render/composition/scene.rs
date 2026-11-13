//! Surface-local composition policy. ArkUI retains every native input/scroll node.
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

use nsh_core::{Matrix, NodeId, ViewKind};
use ohos_arkui_binding::types::advanced::NodeDirtyFlag;
use ohos_drawing_binding::{Bitmap, BitmapFormat, Canvas};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    app::runtime::{Dispatcher, Message},
    render::{
        composition::{
            draw::{PaintNode, Plan, Root},
            geometry::{Bounds, target_size},
            raster::Shared as SceneShared,
            scene_policy::{
                MAX_PREPARE_RETRIES, SceneRetry, TransientOutcome, bounds_overflow_box,
                is_opaque_color, opaque_background_covers_bounds,
            },
        },
        effects::has_active_filters,
        geometry::Size,
        node::{Error, Node, Result},
    },
};

struct RootBitmap {
    bitmap: Bitmap,
    x: f32,
    y: f32,
    scale: f32,
}

struct PreparingGuard<'a>(&'a Scene);

impl<'a> PreparingGuard<'a> {
    fn enter(scene: &'a Scene) -> Self {
        scene.preparing.set(true);
        Self(scene)
    }
}

impl Drop for PreparingGuard<'_> {
    fn drop(&mut self) {
        self.0.preparing.set(false);
        if self.0.requeued.replace(false) {
            self.0.invalidate();
        }
    }
}

struct CandidateRoot {
    root: Rc<Node>,
    req_ids: Vec<NodeId>,
}

struct ReconcileFailure {
    error: Error,
    culprit_ids: Vec<NodeId>,
}

fn is_unsupported_error(error: &Error) -> bool {
    match error {
        Error::Unsupported(_) => true,
        Error::Tree(inner) => is_unsupported_error(inner),
        _ => false,
    }
}

pub(crate) struct Scene {
    root: Weak<Node>,
    active: Cell<bool>,
    dirty: Cell<bool>,
    preparing: Cell<bool>,
    requeued: Cell<bool>,
    shared: Arc<SceneShared>,
    bitmaps: RefCell<FxHashMap<u32, RootBitmap>>,
    roots: RefCell<Vec<Weak<Node>>>,
    pending_restore: RefCell<Vec<Weak<Node>>>,
    requirements: RefCell<Vec<Weak<Node>>>,
    retry: RefCell<SceneRetry>,
}
impl Scene {
    pub(crate) fn new(root: &Rc<Node>) -> Self {
        Self {
            root: Rc::downgrade(root),
            active: Cell::new(false),
            dirty: Cell::new(false),
            preparing: Cell::new(false),
            requeued: Cell::new(false),
            shared: Arc::default(),
            bitmaps: RefCell::default(),
            roots: RefCell::default(),
            pending_restore: RefCell::default(),
            requirements: RefCell::default(),
            retry: RefCell::default(),
        }
    }
    pub(crate) fn has_output(&self) -> bool {
        !self.bitmaps.borrow().is_empty()
    }
    pub(crate) fn require(&self, node: &Rc<Node>) {
        let required = node.effects_plan().requires_scene;
        let mut nodes = self.requirements.borrow_mut();
        nodes.retain(|weak| weak.upgrade().is_some_and(|owner| owner.id != node.id));
        if required {
            nodes.push(Rc::downgrade(node));
        }
        drop(nodes);
        self.retry
            .borrow_mut()
            .on_requirement_updated(node.id, required);
        if self.preparing.get() {
            self.requeued.set(true);
        } else {
            self.invalidate();
        }
    }
    pub(crate) fn invalidate(&self) {
        if !self.active.get()
            && self.pending_restore.borrow().is_empty()
            && self.retry.borrow().is_exhausted()
        {
            return;
        }
        if (self.active.get()
            || !self.pending_restore.borrow().is_empty()
            || !self.requirements.borrow().is_empty())
            && !self.preparing.get()
            && !self.dirty.replace(true)
            && let Some(root) = self.root.upgrade()
            && let Some(route) = root.dispatcher.upgrade()
        {
            route.send(Message::Arkui);
        }
    }
    pub(crate) fn prepare(&self) {
        if !self.dirty.get() {
            return;
        }
        let now = Instant::now();
        if !self.retry.borrow().due(now) {
            return;
        }
        let _guard = PreparingGuard::enter(self);
        let max_passes = self.requirements.borrow().len().saturating_add(1);
        for _ in 0..max_passes {
            match self.reconcile() {
                Ok(()) => {
                    self.dirty.set(false);
                    self.retry.borrow_mut().on_success();
                    return;
                }
                Err(failure) if is_unsupported_error(&failure.error) => {
                    let emit = self
                        .retry
                        .borrow_mut()
                        .mark_unsupported(&failure.culprit_ids);
                    if emit {
                        log::warn!(
                            "OHOS scene unsupported for {:?}; falling back to native: {}",
                            failure.culprit_ids,
                            failure.error
                        );
                    }
                    if failure.culprit_ids.is_empty() {
                        self.dirty.set(false);
                        return;
                    }
                }
                Err(failure) => {
                    match self.retry.borrow_mut().on_transient_failure(Instant::now()) {
                        TransientOutcome::Retry { delay, .. } => {
                            self.dirty.set(true);
                            self.schedule_retry(delay);
                        }
                        TransientOutcome::Exhausted { emit_diagnostic } => {
                            self.dirty.set(false);
                            if emit_diagnostic {
                                log::warn!(
                                    "OHOS scene prepare failed after {MAX_PREPARE_RETRIES} retries; falling back to native: {}",
                                    failure.error
                                );
                            }
                        }
                    }
                    return;
                }
            }
        }
    }
    fn schedule_retry(&self, delay: std::time::Duration) {
        let Some(root) = self.root.upgrade() else {
            return;
        };
        if delay.is_zero() {
            if let Some(route) = root.dispatcher.upgrade() {
                route.send(Message::Arkui);
            }
        } else {
            let dispatcher = root.dispatcher.clone();
            Dispatcher::after(
                &root.dispatcher,
                delay,
                Box::new(move || {
                    if let Some(route) = dispatcher.upgrade() {
                        route.send(Message::Arkui);
                    }
                }),
            );
        }
    }
    fn reconcile(&self) -> std::result::Result<(), ReconcileFailure> {
        let Some(root) = self.root.upgrade() else {
            return Ok(());
        };
        let mut live_ids = FxHashSet::default();
        let mut candidates: Vec<CandidateRoot> = Vec::new();
        {
            let mut requirements = self.requirements.borrow_mut();
            requirements.retain(|node| node.strong_count() > 0);
            let retry = self.retry.borrow();
            // Most surfaces have no scene effects. Check only current trigger nodes;
            // ordinary mount/animation turns do not scan the whole native tree.
            for node in requirements.iter().filter_map(Weak::upgrade) {
                live_ids.insert(node.id);
                if !retry.is_eligible(node.id) || !node.paint_visible() {
                    continue;
                }
                let mut connected = false;
                let mut ancestor = Some(node.clone());
                while let Some(current) = ancestor {
                    if Rc::ptr_eq(&current, &root) {
                        connected = true;
                        break;
                    }
                    ancestor = current.parent_node.borrow().upgrade();
                }
                if connected {
                    let candidate = scene_root_for(&node, &root);
                    if let Some(existing) = candidates
                        .iter_mut()
                        .find(|existing| Rc::ptr_eq(&existing.root, &candidate))
                    {
                        if !existing.req_ids.contains(&node.id) {
                            existing.req_ids.push(node.id);
                        }
                    } else {
                        candidates.push(CandidateRoot {
                            root: candidate,
                            req_ids: vec![node.id],
                        });
                    }
                }
            }
        }
        self.retry
            .borrow_mut()
            .retain_live(|id| live_ids.contains(&id));

        let mut initial_roots: Vec<CandidateRoot> = Vec::new();
        for candidate in &candidates {
            if let Some(ancestor) = candidates.iter().find(|other| {
                !Rc::ptr_eq(&candidate.root, &other.root)
                    && is_scene_ancestor(&other.root, &candidate.root)
            }) {
                let _ = ancestor;
                continue;
            }
            let mut req_ids = candidate.req_ids.clone();
            for covered in &candidates {
                if !Rc::ptr_eq(&candidate.root, &covered.root)
                    && is_scene_ancestor(&candidate.root, &covered.root)
                {
                    for &id in &covered.req_ids {
                        if !req_ids.contains(&id) {
                            req_ids.push(id);
                        }
                    }
                }
            }
            initial_roots.push(CandidateRoot {
                root: candidate.root.clone(),
                req_ids,
            });
        }

        let active = !initial_roots.is_empty();
        let previous = self.active.replace(active);
        if active {
            return self.build(&root, initial_roots);
        }
        let prior_failed = std::mem::take(&mut *self.pending_restore.borrow_mut());
        if previous || !prior_failed.is_empty() {
            if previous {
                if let Some(worker) = root.compositor.borrow().as_ref() {
                    worker.clear_plan();
                }
                self.bitmaps.borrow_mut().clear();
                self.shared.output.lock().unwrap().take();
            }
            let old_roots = std::mem::take(&mut *self.roots.borrow_mut());
            let mut first_error = None;
            let mut restore_failed = Vec::new();
            for node in prior_failed.into_iter().filter_map(|w| w.upgrade()) {
                restore_native_node(&node, &mut first_error, &mut restore_failed);
            }
            for old in old_roots.into_iter().filter_map(|w| w.upgrade()) {
                restore_subtree(&old, &mut first_error, &mut restore_failed);
            }
            if previous {
                restore_subtree(&root, &mut first_error, &mut restore_failed);
            }
            *self.pending_restore.borrow_mut() = restore_failed;
            if let Some(error) = first_error {
                return Err(ReconcileFailure {
                    error,
                    culprit_ids: Vec::new(),
                });
            }
        }
        Ok(())
    }
    fn build(
        &self,
        root: &Rc<Node>,
        initial_roots: Vec<CandidateRoot>,
    ) -> std::result::Result<(), ReconcileFailure> {
        let mut first_old_restore_err = None;
        let mut restore_failed = Vec::new();
        let prior_failed = std::mem::take(&mut *self.pending_restore.borrow_mut());
        for node in prior_failed.into_iter().filter_map(|w| w.upgrade()) {
            restore_native_node(&node, &mut first_old_restore_err, &mut restore_failed);
        }
        for old in self.roots.borrow().iter().filter_map(Weak::upgrade) {
            if !initial_roots
                .iter()
                .any(|next| Rc::ptr_eq(&old, &next.root) || is_scene_ancestor(&next.root, &old))
            {
                restore_subtree(&old, &mut first_old_restore_err, &mut restore_failed);
            }
        }
        *self.pending_restore.borrow_mut() = restore_failed;
        if let Some(error) = first_old_restore_err {
            let all_ids: Vec<NodeId> = initial_roots
                .iter()
                .flat_map(|c| c.req_ids.iter().copied())
                .collect();
            self.rollback(root, &[], &[]);
            return Err(ReconcileFailure {
                error,
                culprit_ids: all_ids,
            });
        }

        fn prepare_subtree(
            node: &Rc<Node>,
            roots: &mut Vec<(Rc<Node>, Vec<NodeId>)>,
            touched: &mut Vec<Rc<Node>>,
            req_ids: &[NodeId],
            depth: usize,
        ) -> Result<()> {
            if depth > 256 {
                return Err(Error::Unsupported("scene hierarchy exceeds 256 levels"));
            }
            if !node.paint_visible() {
                return Ok(());
            }
            touched.push(node.clone());
            let was_prepared = node.scene_prepared.replace(true);
            node.ensure_wrapper()?;
            if let Some(scroll) = node.scroll.borrow().as_ref() {
                scroll.prepare_scene(node)?;
            }
            if !was_prepared {
                node.restore_style()?;
            }
            if node.kind == ViewKind::Modal
                && !roots.iter().any(|(existing, _)| Rc::ptr_eq(existing, node))
            {
                node.scene_root.set(true);
                roots.push((node.clone(), req_ids.to_vec()));
            }
            for child in node.children.borrow().iter().filter_map(Weak::upgrade) {
                prepare_subtree(&child, roots, touched, req_ids, depth + 1)?;
            }
            Ok(())
        }

        let mut roots: Vec<(Rc<Node>, Vec<NodeId>)> = initial_roots
            .iter()
            .map(|c| (c.root.clone(), c.req_ids.clone()))
            .collect();
        let mut touched: Vec<Rc<Node>> = Vec::new();
        let build_result = (|| -> std::result::Result<Plan, ReconcileFailure> {
            for candidate in &initial_roots {
                touched.push(candidate.root.clone());
                candidate.root.scene_root.set(true);
                if let Err(error) = prepare_subtree(
                    &candidate.root,
                    &mut roots,
                    &mut touched,
                    &candidate.req_ids,
                    0,
                ) {
                    return Err(ReconcileFailure {
                        error,
                        culprit_ids: candidate.req_ids.clone(),
                    });
                }
            }
            paint_plan(&roots)
        })();
        let plan = match build_result {
            Ok(plan) => plan,
            Err(failure) => {
                let root_nodes: Vec<Rc<Node>> = roots.into_iter().map(|(n, _)| n).collect();
                self.rollback(root, &root_nodes, &touched);
                return Err(failure);
            }
        };
        *self.roots.borrow_mut() = roots.iter().map(|(n, _)| Rc::downgrade(n)).collect();
        if let Some(worker) = root.compositor.borrow().as_ref() {
            worker.plan(Arc::new(plan), self.shared.clone());
        }
        Ok(())
    }
    fn rollback(&self, root: &Rc<Node>, candidate_roots: &[Rc<Node>], touched: &[Rc<Node>]) {
        // Unconditionally clear `scene_prepared` and `scene_root` across every
        // previously-active, candidate, and touched subtree without aborting on
        // intermediate restore errors, guaranteeing zero orphan `scene_prepared` nodes.
        self.active.set(false);
        if let Some(worker) = root.compositor.borrow().as_ref() {
            worker.clear_plan();
        }
        self.bitmaps.borrow_mut().clear();
        self.shared.output.lock().unwrap().take();
        let old_roots = std::mem::take(&mut *self.roots.borrow_mut());
        let mut ignored = None;
        let mut restore_failed = std::mem::take(&mut *self.pending_restore.borrow_mut());
        for node in touched {
            restore_node_only(node, &mut ignored, &mut restore_failed);
        }
        for old in old_roots.into_iter().filter_map(|w| w.upgrade()) {
            restore_subtree(&old, &mut ignored, &mut restore_failed);
        }
        for candidate in candidate_roots {
            restore_subtree(candidate, &mut ignored, &mut restore_failed);
        }
        restore_subtree(root, &mut ignored, &mut restore_failed);
        *self.pending_restore.borrow_mut() = restore_failed;
    }
    pub(crate) fn ready(&self) -> Result<()> {
        self.shared.posted.store(false, Ordering::Release);
        let frames = self.shared.output.lock().unwrap().take();
        if !self.active.get() {
            return Ok(());
        }
        if let Some(frames) = frames {
            let was_empty = !self.has_output();
            let mut bitmaps = self.bitmaps.borrow_mut();
            let mut next_ids = FxHashSet::default();
            for frame in frames {
                next_ids.insert(frame.id);
                if let Some(existing) = bitmaps.get_mut(&frame.id)
                    && existing.bitmap.width() == frame.width
                    && existing.bitmap.height() == frame.height
                {
                    existing.bitmap.pixels_mut().copy_from_slice(&frame.pixels);
                    existing.x = frame.x;
                    existing.y = frame.y;
                    existing.scale = frame.scale;
                } else {
                    let mut bitmap =
                        Bitmap::new(frame.width, frame.height, BitmapFormat::default());
                    bitmap.pixels_mut().copy_from_slice(&frame.pixels);
                    bitmaps.insert(
                        frame.id,
                        RootBitmap {
                            bitmap,
                            x: frame.x,
                            y: frame.y,
                            scale: frame.scale,
                        },
                    );
                }
            }
            bitmaps.retain(|id, _| next_ids.contains(id));
            let has_output = !bitmaps.is_empty();
            drop(bitmaps);
            for root in self.roots.borrow().iter().filter_map(Weak::upgrade) {
                root.mounted()
                    .borrow()
                    .mark_dirty(NodeDirtyFlag::NeedRender)?;
            }
            if was_empty
                && has_output
                && let Some(root) = self.root.upgrade()
            {
                let composites = root.composite_nodes.borrow().clone();
                for node in composites.into_iter().filter_map(|w| w.upgrade()) {
                    node.update_wrapper()?;
                }
            }
        }
        Ok(())
    }
    pub(crate) fn draw(&self, id: u32, canvas: &Canvas, display_scale: f32) {
        if let Some(entry) = self.bitmaps.borrow().get(&id) {
            let scale = entry.scale.max(0.01);
            if (display_scale - scale).abs() <= 1e-4 {
                canvas.draw_bitmap(&entry.bitmap, entry.x, entry.y);
            } else {
                let ratio = display_scale / scale;
                canvas.save();
                canvas.scale(ratio, ratio);
                canvas.draw_bitmap(&entry.bitmap, entry.x, entry.y);
                canvas.restore();
            }
        }
    }
}

#[track_caller]
fn record_err<E: Into<Error>>(
    slot: &mut Option<Error>,
    node_failed: &mut bool,
    result: std::result::Result<(), E>,
) {
    if let Err(error) = result {
        *node_failed = true;
        if slot.is_none() {
            *slot = Some(error.into());
        }
    }
}

fn restore_native_node(
    node: &Rc<Node>,
    first_error: &mut Option<Error>,
    failed_nodes: &mut Vec<Weak<Node>>,
) {
    if !node.active.get() || node.pending_delete.get() {
        return;
    }
    let mut node_failed = false;
    if let Some(scroll) = node.scroll.borrow().as_ref() {
        record_err(first_error, &mut node_failed, scroll.restore_scene(node));
    }
    record_err(first_error, &mut node_failed, node.ensure_wrapper());
    record_err(first_error, &mut node_failed, node.restore_style());
    record_err(
        first_error,
        &mut node_failed,
        node.native.borrow().mark_dirty(NodeDirtyFlag::NeedMeasure),
    );
    record_err(
        first_error,
        &mut node_failed,
        node.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender),
    );
    record_err(
        first_error,
        &mut node_failed,
        node.mounted()
            .borrow()
            .mark_dirty(NodeDirtyFlag::NeedRender),
    );
    if node_failed
        && !failed_nodes
            .iter()
            .any(|existing| existing.upgrade().is_some_and(|n| Rc::ptr_eq(&n, node)))
    {
        failed_nodes.push(Rc::downgrade(node));
    }
}

fn restore_node_only(
    node: &Rc<Node>,
    first_error: &mut Option<Error>,
    failed_nodes: &mut Vec<Weak<Node>>,
) {
    let was_prepared = node.scene_prepared.replace(false);
    let was_root = node.scene_root.replace(false);
    if was_prepared || was_root {
        restore_native_node(node, first_error, failed_nodes);
    }
}

fn restore_subtree(
    node: &Rc<Node>,
    first_error: &mut Option<Error>,
    failed_nodes: &mut Vec<Weak<Node>>,
) {
    restore_node_only(node, first_error, failed_nodes);
    for child in node.children.borrow().iter().filter_map(Weak::upgrade) {
        restore_subtree(&child, first_error, failed_nodes);
    }
    for child in node
        .retained_children
        .borrow()
        .iter()
        .filter_map(Weak::upgrade)
    {
        if child.retained.get() {
            restore_subtree(&child, first_error, failed_nodes);
        }
    }
}

fn is_scene_ancestor(ancestor: &Rc<Node>, node: &Rc<Node>) -> bool {
    let mut current = node.parent_node.borrow().upgrade();
    while let Some(parent) = current {
        if parent.kind == ViewKind::Modal {
            return false;
        }
        if Rc::ptr_eq(&parent, ancestor) {
            return true;
        }
        current = parent.parent_node.borrow().upgrade();
    }
    false
}

fn local_effect_bounds(node: &Rc<Node>, include_backdrop: bool) -> Bounds {
    let frame = node.frame.get();
    let style = node.style.borrow();
    let filters = style
        .effects
        .as_ref()
        .map_or(&[][..], |effects| effects.filters.as_slice());
    let mut bounds = Bounds::new(frame.width, frame.height)
        .shaders(node.shaders.overflow(node.id), &node.shader_layers.borrow())
        .own(&style)
        .filters(filters);
    if include_backdrop && let Some(effects) = &style.effects {
        bounds.union(Bounds::new(frame.width, frame.height).filters(&effects.backdrop_filters));
    }
    bounds
}

/// Selects the minimal stable ancestor root for a `Scene` trigger `node` (A14):
/// - Non-backdrop triggers (e.g. `mask-image`) stay on `node`'s own subtree unless
///   its computed visual bounds genuinely overflow `[0, 0, width, height]`.
/// - Backdrop triggers (`backdrop-filter`, non-normal `blend-mode`, shader backdrop)
///   stop at the nearest ancestor whose opaque background covers `node`'s projected
///   backdrop sample bounds (even without `overflow: hidden`), or at clip+opaque /
///   backdrop-root / `Screen` / `Modal` / surface `root` boundaries.
fn scene_root_for(node: &Rc<Node>, root: &Rc<Node>) -> Rc<Node> {
    let samples_backdrop = {
        let style = node.style.borrow();
        style.blend_mode != nsh_core::BlendMode::Normal
            || style
                .effects
                .as_ref()
                .is_some_and(|e| has_active_filters(&e.backdrop_filters))
            || node.shader_layers.borrow().iter().any(|(_, layer)| {
                layer
                    .shader
                    .inputs()
                    .contains(nsh_core::shader::ShaderInput::Backdrop)
            })
    };
    if !samples_backdrop {
        if node.clips_children() {
            return node.clone();
        }
        let frame = node.frame.get();
        let bounds = local_effect_bounds(node, false);
        if !bounds_overflow_box(bounds.0, frame.width, frame.height) {
            return node.clone();
        }
        if let Some(parent) = node.parent_node.borrow().upgrade() {
            return parent;
        }
        return node.clone();
    }

    let mut sample_bounds = Some(local_effect_bounds(node, true));
    let mut child = node.clone();
    let mut current = node.parent_node.borrow().upgrade();
    while let Some(ancestor) = current {
        sample_bounds = sample_bounds.and_then(|mut bounds| {
            if child.clips_children() {
                let cf = child.frame.get();
                bounds.0 = [
                    bounds.0[0].max(0.0),
                    bounds.0[1].max(0.0),
                    bounds.0[2].min(cf.width.max(0.0)),
                    bounds.0[3].min(cf.height.max(0.0)),
                ];
            }
            let to_parent = child.input_to_parent(&ancestor).ok()?;
            bounds.transformed(to_parent)
        });

        let backdrop_root = {
            let style = ancestor.style.borrow();
            let effects = style.effects.as_ref();
            ancestor.scene_opacity() < 1.0
                || style.blend_mode != nsh_core::BlendMode::Normal
                || effects.is_some_and(|e| {
                    !e.mask_images.is_empty()
                        || has_active_filters(&e.filters)
                        || has_active_filters(&e.backdrop_filters)
                })
        };
        if Rc::ptr_eq(&ancestor, root)
            || ancestor.kind == ViewKind::Modal
            || ancestor.kind == ViewKind::Screen
            || backdrop_root
            || !ancestor.shader_layers.borrow().is_empty()
            || ancestor.navigation_capture.get()
        {
            return ancestor;
        }
        let ancestor_frame = ancestor.frame.get();
        if let Some(bounds) = sample_bounds
            && opaque_background_covers_bounds(
                &ancestor.style.borrow(),
                ancestor.scene_opacity(),
                ancestor_frame.width,
                ancestor_frame.height,
                bounds.0,
            )
        {
            return ancestor;
        }
        if ancestor.clips_children() {
            let opaque_bg = is_opaque_color(ancestor.style.borrow().background_color)
                || ancestor
                    .children
                    .borrow()
                    .iter()
                    .filter_map(Weak::upgrade)
                    .find(|first| first.paint_visible())
                    .is_some_and(|first| {
                        let ff = first.frame.get();
                        opaque_background_covers_bounds(
                            &first.style.borrow(),
                            first.scene_opacity(),
                            ff.width,
                            ff.height,
                            [0.0, 0.0, ancestor_frame.width, ancestor_frame.height],
                        ) && ff.x <= 0.0
                            && ff.y <= 0.0
                            && ff.x + ff.width >= ancestor_frame.width
                            && ff.y + ff.height >= ancestor_frame.height
                    });
            if opaque_bg {
                return ancestor;
            }
        }
        child = ancestor.clone();
        current = ancestor.parent_node.borrow().upgrade();
    }
    root.clone()
}

fn paint_plan(roots: &[(Rc<Node>, Vec<NodeId>)]) -> std::result::Result<Plan, ReconcileFailure> {
    let mut plan = Plan { roots: Vec::new() };
    for (scene_root, req_ids) in roots {
        if !scene_root.paint_visible() {
            continue;
        }
        let node = build_paint_tree(scene_root, None, 0).map_err(|error| ReconcileFailure {
            error,
            culprit_ids: req_ids.clone(),
        })?;
        let own_filters = node
            .style
            .effects
            .as_ref()
            .map_or(&[][..], |effects| effects.filters.as_slice());
        let root_bounds = node.bounds.filters(own_filters);
        let logical_w = (root_bounds.0[2] - root_bounds.0[0]).max(1.0);
        let logical_h = (root_bounds.0[3] - root_bounds.0[1]).max(1.0);
        let (_, _, root_scale) = target_size(logical_w, logical_h, scene_root.scale.get());
        if root_scale <= 0.0 {
            return Err(ReconcileFailure {
                error: Error::Unsupported("invalid scene root dimensions"),
                culprit_ids: req_ids.clone(),
            });
        }
        let origin_x = (root_bounds.0[0] * root_scale).floor();
        let origin_y = (root_bounds.0[1] * root_scale).floor();
        let width = ((root_bounds.0[2] * root_scale).ceil() - origin_x).clamp(1.0, 4096.0) as u32;
        let height = ((root_bounds.0[3] * root_scale).ceil() - origin_y).clamp(1.0, 4096.0) as u32;
        plan.roots.push(Root {
            id: scene_root.id,
            width,
            height,
            origin: (origin_x, origin_y),
            scale: root_scale,
            node,
        });
    }
    Ok(plan)
}

fn build_paint_tree(owner: &Rc<Node>, parent: Option<&Node>, depth: usize) -> Result<PaintNode> {
    if depth > 256 {
        return Err(Error::Unsupported("scene hierarchy exceeds 256 levels"));
    }
    let frame = owner.frame.get();
    let size = Size::new(frame.width, frame.height);
    let style = owner.style.borrow().clone();
    let source = owner.wrapper.borrow().as_ref().and_then(|wrapper| {
        wrapper
            .composite
            .borrow()
            .as_ref()
            .map(|composite| composite.key())
    });
    let mut children: Vec<_> = owner
        .children
        .borrow()
        .iter()
        .filter_map(Weak::upgrade)
        .filter(|child| child.kind != ViewKind::Modal && child.paint_visible())
        .collect();
    children.sort_by_key(|child| child.input_priority());
    let mut bounds = Bounds::new(size.width, size.height)
        .shaders(
            owner.shaders.overflow(owner.id),
            &owner.shader_layers.borrow(),
        )
        .own(&style);
    let children = children
        .iter()
        .map(|child| build_paint_tree(child, Some(owner), depth + 1))
        .collect::<Result<Vec<_>>>()?;
    if !owner.clips_children() {
        for child in &children {
            let effects = child
                .style
                .effects
                .as_ref()
                .map_or(&[][..], |effects| effects.filters.as_slice());
            if let Some(child) = child.bounds.filters(effects).transformed(child.transform) {
                bounds.union(child);
            } else {
                return Err(Error::Unsupported("unbounded scene projection"));
            }
        }
    }
    if let Some(effects) = &style.effects {
        bounds.union(Bounds::new(size.width, size.height).filters(&effects.backdrop_filters));
    }
    let transform = parent.map_or(Ok(Matrix::IDENTITY), |parent| owner.input_to_parent(parent))?;
    Ok(PaintNode {
        source,
        shaders: !owner.shader_layers.borrow().is_empty(),
        shader_backdrop: owner.id != nsh_core::ROOT_ID
            && owner.shader_layers.borrow().iter().any(|(_, layer)| {
                layer
                    .shader
                    .inputs()
                    .contains(nsh_core::shader::ShaderInput::Backdrop)
            }),
        capture: owner.navigation_capture.get(),
        overlay: owner.scroll.borrow().as_ref().and_then(|scroll| {
            scroll
                .overlay
                .borrow()
                .as_ref()
                .and_then(|plane| plane.key())
        }),
        transform,
        size,
        opacity: owner.scene_opacity(),
        clip: owner.clips_children(),
        bounds,
        style: Arc::from(style),
        children,
    })
}
