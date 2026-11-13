//! On-demand ArkTS wrappers retain visual and accessibility composition around C subtrees.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use nsh_core::{AccessibilityRole as R, Checked, EventKind};
use ohos_arkui_binding::{
    component::{
        attribute::{ArkUICommonAttribute, ArkUIEvent},
        built_in_component::Custom,
    },
    types::advanced::{LengthMetricUnit, NodeDirtyFlag},
};

use crate::{
    accessibility::value::owns_semantics,
    interop::{
        attachment::NativeAttachment,
        sdk::{MenuItem, ViewOptions, ViewRoute},
    },
    render::node::{NativeNode, Node, Result, callback},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetirePhase {
    Exporting,
    Settling,
}

struct RootBridge {
    stack: std::rc::Weak<Node>,
    peer: std::rc::Weak<Node>,
    screen_owner: std::rc::Weak<Node>,
    hosted_on_outer: Cell<bool>,
}

#[derive(Clone)]
struct PendingBridgeNodes {
    stack: std::rc::Weak<Node>,
    peer: std::rc::Weak<Node>,
    holds_export_repair: bool,
    outer_offset: (i32, i32),
}

fn bridge_peer_px_offset(stack: &Rc<Node>, peer: &Rc<Node>, scale: f32) -> (i32, i32) {
    let mut x = peer.frame.get().x;
    let mut y = peer.frame.get().y;
    let mut at = Some(stack.clone());
    while let Some(node) = at {
        if node.id == nsh_core::ROOT_ID {
            break;
        }
        let frame = node.frame.get();
        x += frame.x;
        y += frame.y;
        at = node.parent_node.borrow().upgrade();
    }
    ((x * scale).round() as i32, (y * scale).round() as i32)
}

pub(crate) struct Wrapper {
    closed: std::cell::Cell<bool>,
    pub(crate) outer: NativeNode,
    target: RefCell<Option<NativeAttachment>>,
    route: RefCell<Option<ViewRoute>>,
    handoff: RefCell<crate::interop::handoff::HandoffState>,
    bridge: RefCell<Option<Rc<RootBridge>>>,
    pending_reattach: RefCell<crate::interop::handoff::PendingReattachQueue<PendingBridgeNodes>>,
    reattach_export: RefCell<Option<Rc<crate::interop::texture_export::Export>>>,
    context_live: Cell<bool>,
    owner: std::rc::Weak<Node>,
    settings: std::cell::RefCell<Option<ViewOptions>>,
    pub(crate) composite: std::cell::RefCell<Option<crate::render::composition::worker::Composite>>,
    retiring: Cell<Option<(u32, RetirePhase)>>,
    lingering: std::cell::RefCell<Vec<crate::render::composition::worker::Composite>>,
    /// The accessibility mode and grouping last given the wrapped node
    /// (`None`: to give again, after the node's own props reset them).
    semantics: Cell<Option<(i32, bool)>>,
    /// The frame size the outer node was last asked to measure at.
    measured: Cell<Option<(f32, f32)>>,
    /// Set when backgrounding retired an active composite so foreground resume
    /// reports `shader-mount-gap` rather than `handoff-gap`.
    retired_for_background: Cell<bool>,
}
fn draw_placeholder_fill(
    canvas: &ohos_drawing_binding::Canvas,
    width: f32,
    height: f32,
    scale: f32,
    fill: crate::interop::handoff::PlaceholderFill,
) {
    let w = width * scale;
    let h = height * scale;
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let rect = ohos_drawing_binding::Rect::new(0.0, 0.0, w, h);
    let mut brush = ohos_drawing_binding::Brush::new();
    brush.set_anti_alias(true);
    brush.set_color(fill.argb);
    canvas.attach_brush(&brush);
    if fill.radii.iter().any(|&r| r > 0.0) {
        let mut round_rect = ohos_drawing_binding::RoundRect::new(&rect, 0.0, 0.0);
        let corners = [
            (ohos_drawing_binding::CornerPosition::TopLeft, fill.radii[0]),
            (
                ohos_drawing_binding::CornerPosition::TopRight,
                fill.radii[1],
            ),
            (
                ohos_drawing_binding::CornerPosition::BottomLeft,
                fill.radii[2],
            ),
            (
                ohos_drawing_binding::CornerPosition::BottomRight,
                fill.radii[3],
            ),
        ];
        for (pos, radius) in corners {
            let px = (radius * scale).max(0.0);
            round_rect.set_corner(pos, px, px);
        }
        let mut path = ohos_drawing_binding::Path::new();
        path.add_round_rect(&round_rect, ohos_drawing_binding::PathDirection::Cw);
        canvas.draw_path(&path);
    } else {
        canvas.draw_rect(&rect);
    }
    canvas.detach_brush();
}

impl Wrapper {
    pub(crate) fn is_context_live(&self) -> bool {
        !self.closed.get() && self.context_live.get()
    }
    pub(crate) fn retiring_export(&self) -> bool {
        matches!(self.retiring.get(), Some((_, RetirePhase::Exporting)))
    }
    fn bridge_nodes_valid(bridge: &RootBridge) -> Option<(Rc<Node>, Rc<Node>, Rc<Node>)> {
        let stack = bridge.stack.upgrade()?;
        let peer = bridge.peer.upgrade()?;
        let screen_owner = bridge.screen_owner.upgrade()?;
        if !stack.active.get()
            || stack.retained.get()
            || !peer.active.get()
            || peer.retained.get()
            || peer.parent.get() != Some(stack.id)
            || !screen_owner.active.get()
            || screen_owner.retained.get()
            || screen_owner.parent.get() != Some(stack.id)
        {
            return None;
        }
        let valid_screen = screen_owner.screen.try_borrow().ok().is_some_and(|s| {
            s.as_ref()
                .is_some_and(|sc| sc.is_top() && sc.is_unanimated())
        });
        if !valid_screen {
            return None;
        }
        Some((stack, peer, screen_owner))
    }
    fn has_live_bridge(&self, export_requested: bool, revealed: bool) -> bool {
        if self.closed.get() || !self.context_live.get() || !export_requested || revealed {
            return false;
        }
        let bridge = self.bridge.try_borrow().ok().and_then(|b| b.clone());
        bridge
            .as_ref()
            .is_some_and(|b| Self::bridge_nodes_valid(b).is_some())
    }
    fn hosted_bridge_peer(&self) -> Option<Rc<Node>> {
        let bridge = self.bridge.try_borrow().ok()?.clone()?;
        if !bridge.hosted_on_outer.get() {
            return None;
        }
        bridge.peer.upgrade().filter(|peer| peer.active.get())
    }
    fn hosted_bridge_layout(&self, owner: &Node) -> Option<(Rc<Node>, i32, i32)> {
        let bridge = self.bridge.try_borrow().ok()?.clone()?;
        if !bridge.hosted_on_outer.get() {
            return None;
        }
        let stack = bridge.stack.upgrade().filter(|s| s.active.get())?;
        let peer = bridge.peer.upgrade().filter(|p| p.active.get())?;
        let (x, y) = bridge_peer_px_offset(&stack, &peer, owner.scale.get());
        Some((peer, x, y))
    }
    fn pending_outer_bridge_peers(&self) -> Vec<Rc<Node>> {
        self.pending_reattach
            .try_borrow()
            .ok()
            .map(|queue| {
                queue
                    .iter_on_outer()
                    .filter_map(|entry| entry.peer.upgrade().filter(|peer| peer.active.get()))
                    .collect()
            })
            .unwrap_or_default()
    }
    fn pending_outer_bridge_layouts(&self) -> Vec<(Rc<Node>, i32, i32)> {
        self.pending_reattach
            .try_borrow()
            .ok()
            .map(|queue| {
                queue
                    .iter_on_outer()
                    .filter_map(|entry| {
                        let peer = entry.peer.upgrade().filter(|peer| peer.active.get())?;
                        Some((peer, entry.outer_offset.0, entry.outer_offset.1))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    fn detach_peer_from_outer(outer: &NativeNode, peer: &Rc<Node>) -> Result<()> {
        let mounted = peer.mounted();
        let outer_owned = outer.borrow().is_owned();
        let outer_idx = if outer_owned {
            outer
                .borrow()
                .children()
                .iter()
                .position(|c| Rc::ptr_eq(c, &mounted))
        } else {
            None
        };
        if let Some(idx) = outer_idx {
            outer.borrow_mut().remove_child(idx)?;
            if peer
                .mount_parent
                .borrow()
                .upgrade()
                .is_some_and(|p| Rc::ptr_eq(&p, outer))
            {
                *peer.mount_parent.borrow_mut() = std::rc::Weak::new();
            }
            outer.borrow().mark_dirty(NodeDirtyFlag::NeedMeasure)?;
            outer.borrow().mark_dirty(NodeDirtyFlag::NeedLayout)?;
            outer.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        } else if peer
            .mount_parent
            .borrow()
            .upgrade()
            .is_some_and(|p| Rc::ptr_eq(&p, outer))
        {
            *peer.mount_parent.borrow_mut() = std::rc::Weak::new();
        }
        Ok(())
    }
    fn release_entry_export_repair(entry: &mut PendingBridgeNodes) {
        if std::mem::replace(&mut entry.holds_export_repair, false)
            && let Some(peer) = entry.peer.upgrade()
        {
            peer.export_repair
                .set(peer.export_repair.get().saturating_sub(1));
        }
    }
    fn sync_bridge(
        &self,
        owner: &Node,
        export_requested: bool,
        active_target_is_texture: bool,
        revealed: bool,
    ) -> Result<()> {
        let Some(bridge) = self.bridge.borrow().clone() else {
            return Ok(());
        };
        if self.closed.get() || !self.context_live.get() || !export_requested || revealed {
            return self.release_bridge();
        }
        let Some((stack, peer, screen_owner)) = Self::bridge_nodes_valid(&bridge) else {
            return self.release_bridge();
        };
        if !bridge.hosted_on_outer.get() {
            let cancelled = self.pending_reattach.borrow_mut().take_peer(peer.id);
            let had_pending = cancelled.is_some();
            if let Some(mut entry) = cancelled {
                Self::detach_peer_from_outer(&self.outer, &peer)?;
                Self::release_entry_export_repair(&mut entry);
                peer.restore_after_export()?;
            }
            if let Some(old_export) = self.reattach_export.borrow_mut().take() {
                old_export.restore();
            }
            if !had_pending {
                peer.snapshot_subtree_scroll_offsets();
            }
            let mounted = peer.mounted();
            let detach = peer.mount_parent.borrow().upgrade().and_then(|parent| {
                let idx = parent
                    .borrow()
                    .children()
                    .iter()
                    .position(|child| Rc::ptr_eq(child, &mounted))?;
                Some((parent, idx))
            });
            if let Some((parent, idx)) = detach {
                parent.borrow_mut().remove_child(idx)?;
                parent.borrow().mark_dirty(NodeDirtyFlag::NeedMeasure)?;
                parent.borrow().mark_dirty(NodeDirtyFlag::NeedLayout)?;
            }
            let target_child_idx = self
                .with_child(|tc| {
                    let raw = tc.raw_handle();
                    self.outer
                        .borrow()
                        .children()
                        .iter()
                        .position(|c| c.borrow().raw_handle() == raw)
                })?
                .flatten();
            let insert_idx = crate::interop::handoff::bridge_insert_index(
                self.outer.borrow().children().len(),
                target_child_idx,
            );
            self.outer
                .borrow_mut()
                .insert_child(mounted.clone(), insert_idx)?;
            *peer.mount_parent.borrow_mut() = Rc::downgrade(&self.outer);
            peer.bridged_on_root_outer.set(true);
            bridge.hosted_on_outer.set(true);
            mounted.borrow().set_i32_attribute(
                ohos_arkui_binding::types::attribute::ArkUINodeAttributeType::HitTestBehavior,
                ohos_arkui_binding::types::view::HitTestMode::None.into(),
            )?;
            peer.interaction.pointer(&peer, true)?;
            let (x, y) = bridge_peer_px_offset(&stack, &peer, owner.scale.get());
            peer.invalidate_mounted_geometry();
            peer.place_bounds(x, y)?;
            peer.restore_subtree_scroll_offsets(true)?;
            self.outer.borrow().mark_dirty(NodeDirtyFlag::NeedMeasure)?;
            self.outer.borrow().mark_dirty(NodeDirtyFlag::NeedLayout)?;
            self.outer.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        let current_phase = screen_owner
            .screen
            .borrow()
            .as_ref()
            .map(|screen| screen.bridge_phase.get());
        if active_target_is_texture {
            if current_phase != Some(crate::navigation::stack::RootBridgePhase::RenderingExport) {
                if let Some(screen) = screen_owner.screen.borrow().as_ref() {
                    screen
                        .bridge_phase
                        .set(crate::navigation::stack::RootBridgePhase::RenderingExport);
                }
                screen_owner.navigation_presentation(1.0, 0.0)?;
                screen_owner.restore_style()?;
                screen_owner.restore_export_controls()?;
                screen_owner.schedule_subtree_paint(4)?;
                screen_owner.shaders.invalidate_visibility();
                screen_owner.animator.request_frame();
            }
        } else if current_phase != Some(crate::navigation::stack::RootBridgePhase::HoldingDirect) {
            if let Some(screen) = screen_owner.screen.borrow().as_ref() {
                screen
                    .bridge_phase
                    .set(crate::navigation::stack::RootBridgePhase::HoldingDirect);
            }
            screen_owner.navigation_presentation(0.0, 0.0)?;
        }
        Ok(())
    }
    fn cleanup_peer_bridge_state(stack_weak: &std::rc::Weak<Node>, peer: &Rc<Node>) -> Result<()> {
        if !peer.active.get() {
            return Ok(());
        }
        let still_shared = stack_weak.upgrade().is_some_and(|stack| {
            stack
                .children
                .borrow()
                .iter()
                .filter_map(std::rc::Weak::upgrade)
                .filter_map(|child| child.screen.borrow().clone())
                .any(|screen| {
                    screen
                        .peer
                        .borrow()
                        .upgrade()
                        .is_some_and(|p| Rc::ptr_eq(&p, peer))
                })
        });
        if !still_shared {
            if let Some(peer_screen) = peer.screen.borrow().as_ref() {
                peer_screen.set_revealed(false);
            }
            peer.navigation_presentation(1.0, 0.0)?;
            peer.restore_style()?;
            peer.restore_export_controls()?;
            peer.invalidate_scene();
        }
        Ok(())
    }
    fn finish_bridge_reattach(
        root_owner: Option<&Node>,
        mut entry: PendingBridgeNodes,
    ) -> Result<()> {
        let root_handles = root_owner.and_then(|root| {
            let borrow = root.wrapper.try_borrow().ok()?;
            let wrapper = borrow.as_ref()?;
            Some((
                wrapper.reattach_export.borrow_mut().take(),
                wrapper.outer.clone(),
            ))
        });
        if let Some((old_export, outer)) = root_handles {
            if let Some(old_export) = old_export {
                old_export.restore();
            }
            if let Some(peer) = entry.peer.upgrade() {
                Self::detach_peer_from_outer(&outer, &peer)?;
            }
        }
        let Some(peer) = entry.peer.upgrade() else {
            Self::release_entry_export_repair(&mut entry);
            return Ok(());
        };
        peer.bridged_on_root_outer.set(false);
        let mounted = peer.mounted();
        if peer.active.get() {
            let reset_hit_test = peer.wrapper.borrow().is_some() && mounted.borrow().is_owned();
            if reset_hit_test {
                mounted.borrow().set_i32_attribute(
                    ohos_arkui_binding::types::attribute::ArkUINodeAttributeType::HitTestBehavior,
                    ohos_arkui_binding::types::view::HitTestMode::Default.into(),
                )?;
            }
        }
        Self::cleanup_peer_bridge_state(&entry.stack, &peer)?;
        let reattach = if peer.active.get() && !peer.retained.get() {
            entry
                .stack
                .upgrade()
                .filter(|stack| stack.active.get() && peer.parent.get() == Some(stack.id))
                .and_then(|stack| {
                    let logical_idx = stack
                        .children
                        .borrow()
                        .iter()
                        .position(|child| child.upgrade().is_some_and(|c| c.id == peer.id))?;
                    Some((stack, logical_idx))
                })
        } else {
            None
        };
        if let Some((stack, logical_idx)) = reattach {
            let native_idx = stack.child_index(logical_idx);
            let container = stack.child_container();
            let already_mounted = container
                .borrow()
                .children()
                .iter()
                .any(|c| Rc::ptr_eq(c, &mounted));
            let in_export =
                !already_mounted && stack.inside_texture_export() && root_owner.is_some();
            Self::release_entry_export_repair(&mut entry);
            peer.restore_after_export()?;
            let repair = if in_export {
                let export = Rc::new(crate::interop::texture_export::Export::default());
                export.enabled.set(true);
                export.prepare(&peer, &mounted)?;
                Some(export)
            } else {
                None
            };
            if !already_mounted
                && let Err(error) = container
                    .borrow_mut()
                    .insert_child(mounted.clone(), native_idx)
            {
                if let Some(export) = repair {
                    export.restore();
                } else {
                    peer.restore_after_export()?;
                }
                return Err(error.into());
            }
            if repair.is_some() && peer.export_repair.get() != 0 && mounted.borrow().is_owned() {
                mounted.borrow().opacity(f32::MIN_POSITIVE)?;
                mounted.borrow().opacity(0.0)?;
            }
            *peer.mount_parent.borrow_mut() = Rc::downgrade(&container);
            container.borrow().mark_dirty(NodeDirtyFlag::NeedMeasure)?;
            container.borrow().mark_dirty(NodeDirtyFlag::NeedLayout)?;
            container.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
            if peer.active.get() {
                peer.invalidate_mounted_geometry();
                peer.update_layout_bounds()?;
            }
            if let Some(export) = repair {
                if let Some(root) = root_owner
                    && let Ok(borrow) = root.wrapper.try_borrow()
                    && let Some(wrapper) = borrow.as_ref()
                {
                    *wrapper.reattach_export.borrow_mut() = Some(export.clone());
                }
                let _ = export.after_attach(&container);
            }
        } else {
            Self::release_entry_export_repair(&mut entry);
            peer.restore_after_export()?;
            *peer.mount_parent.borrow_mut() = std::rc::Weak::new();
            if peer.active.get() {
                peer.invalidate_mounted_geometry();
                peer.update_layout_bounds()?;
            }
        }
        if peer.active.get() {
            peer.restore_subtree_scroll_offsets(true)?;
        } else {
            peer.clear_subtree_scroll_snapshots();
        }
        Ok(())
    }
    fn cancel_bridge_reattach(
        outer: Option<&NativeNode>,
        mut entry: PendingBridgeNodes,
    ) -> Result<()> {
        if let Some(outer) = outer
            && let Some(peer) = entry.peer.upgrade()
        {
            Self::detach_peer_from_outer(outer, &peer)?;
        }
        Self::release_entry_export_repair(&mut entry);
        let Some(peer) = entry.peer.upgrade() else {
            return Ok(());
        };
        peer.bridged_on_root_outer.set(false);
        *peer.mount_parent.borrow_mut() = std::rc::Weak::new();
        peer.clear_subtree_scroll_snapshots();
        if peer.active.get() {
            let mounted = peer.mounted();
            let reset_hit_test = peer.wrapper.borrow().is_some() && mounted.borrow().is_owned();
            if reset_hit_test {
                mounted.borrow().set_i32_attribute(
                    ohos_arkui_binding::types::attribute::ArkUINodeAttributeType::HitTestBehavior,
                    ohos_arkui_binding::types::view::HitTestMode::Default.into(),
                )?;
            }
            peer.restore_after_export()?;
        }
        Ok(())
    }
    fn advance_pending_reattach(owner: std::rc::Weak<Node>, id: u64) {
        let Some(node) = owner.upgrade() else {
            return;
        };
        callback(|| {
            let step = {
                let borrow = node.wrapper.borrow();
                let Some(wrapper) = borrow.as_ref().filter(|w| !w.closed.get()) else {
                    return Ok(());
                };
                (
                    wrapper.outer.clone(),
                    wrapper.pending_reattach.borrow_mut().on_frame_callback(id),
                )
            };
            let (outer, tick) = step;
            match tick {
                crate::interop::handoff::ReattachTick::Stale => {}
                crate::interop::handoff::ReattachTick::DetachFromOuter(entry) => {
                    if let Some(peer) = entry.peer.upgrade() {
                        Self::detach_peer_from_outer(&outer, &peer)?;
                        node.flush_arkui()?;
                    }
                    let next = owner.clone();
                    if outer
                        .borrow()
                        .post_frame_callback(move |_, _| {
                            Self::advance_pending_reattach(next, id);
                        })
                        .is_err()
                    {
                        let fallback = node
                            .wrapper
                            .borrow()
                            .as_ref()
                            .and_then(|w| w.pending_reattach.borrow_mut().take_by_id(id));
                        if let Some(entry) = fallback {
                            Self::finish_bridge_reattach(Some(&node), entry)?;
                            node.flush_arkui()?;
                        }
                    }
                }
                crate::interop::handoff::ReattachTick::WaitMore => {
                    let next = owner.clone();
                    if outer
                        .borrow()
                        .post_frame_callback(move |_, _| {
                            Self::advance_pending_reattach(next, id);
                        })
                        .is_err()
                    {
                        let fallback = node
                            .wrapper
                            .borrow()
                            .as_ref()
                            .and_then(|w| w.pending_reattach.borrow_mut().take_by_id(id));
                        if let Some(entry) = fallback {
                            Self::finish_bridge_reattach(Some(&node), entry)?;
                            node.flush_arkui()?;
                        }
                    }
                }
                crate::interop::handoff::ReattachTick::Complete(entry) => {
                    Self::finish_bridge_reattach(Some(&node), entry)?;
                    node.flush_arkui()?;
                }
            }
            Ok(())
        });
    }
    fn release_bridge(&self) -> Result<()> {
        let Some(bridge) = self.bridge.borrow_mut().take() else {
            return Ok(());
        };
        if let Some(screen_owner) = bridge.screen_owner.upgrade() {
            let mut reset_nav = false;
            if let Some(screen) = screen_owner.screen.borrow().as_ref() {
                screen
                    .bridge_phase
                    .set(crate::navigation::stack::RootBridgePhase::None);
                if screen.is_unanimated() {
                    *screen.peer.borrow_mut() = std::rc::Weak::new();
                    reset_nav = true;
                }
            }
            if screen_owner.active.get() {
                if reset_nav {
                    screen_owner.navigation_presentation(1.0, 0.0)?;
                }
                screen_owner.restore_style()?;
                screen_owner.shaders.invalidate_visibility();
            }
        }
        if bridge.hosted_on_outer.replace(false) {
            let outer_owned = self.outer.borrow().is_owned();
            if let Some(peer) = bridge.peer.upgrade() {
                peer.snapshot_subtree_scroll_offsets();
                let reattach_stack = if peer.active.get() && !peer.retained.get() {
                    bridge
                        .stack
                        .upgrade()
                        .filter(|stack| stack.active.get() && peer.parent.get() == Some(stack.id))
                } else {
                    None
                };
                let destination_in_export = reattach_stack
                    .as_ref()
                    .is_some_and(|stack| stack.inside_texture_export());
                let plan = crate::interop::handoff::plan_bridge_reattach(
                    self.closed.get() || !self.context_live.get(),
                    outer_owned,
                    destination_in_export,
                );
                match (plan, reattach_stack) {
                    (
                        crate::interop::handoff::BridgeReattachPlan::Defer {
                            flush_alpha_on_outer,
                            settle_frames,
                        },
                        Some(stack),
                    ) => {
                        let scale = self.owner.upgrade().map_or(1.0, |owner| owner.scale.get());
                        let outer_offset = bridge_peer_px_offset(&stack, &peer, scale);
                        if flush_alpha_on_outer {
                            peer.export_repair.set(peer.export_repair.get() + 1);
                            peer.interaction.invalidate_opacity();
                            peer.restore_style()?;
                            peer.mounted().borrow().opacity(0.0)?;
                            self.outer.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
                        } else {
                            Self::detach_peer_from_outer(&self.outer, &peer)?;
                        }
                        let (id, displaced) = self.pending_reattach.borrow_mut().enqueue(
                            stack.id,
                            peer.id,
                            flush_alpha_on_outer,
                            settle_frames,
                            PendingBridgeNodes {
                                stack: Rc::downgrade(&stack),
                                peer: Rc::downgrade(&peer),
                                holds_export_repair: flush_alpha_on_outer,
                                outer_offset,
                            },
                        );
                        if let Some(mut old) = displaced {
                            Self::release_entry_export_repair(&mut old);
                        }
                        let next = self.owner.clone();
                        if self
                            .outer
                            .borrow()
                            .post_frame_callback(move |_, _| {
                                Self::advance_pending_reattach(next, id);
                            })
                            .is_err()
                        {
                            let fallback = self.pending_reattach.borrow_mut().take_by_id(id);
                            if let Some(entry) = fallback {
                                let root = self.owner.upgrade();
                                Self::finish_bridge_reattach(root.as_deref(), entry)?;
                            }
                        }
                    }
                    _ => {
                        Self::detach_peer_from_outer(&self.outer, &peer)?;
                        let root = self.owner.upgrade();
                        Self::finish_bridge_reattach(
                            root.as_deref(),
                            PendingBridgeNodes {
                                stack: bridge.stack.clone(),
                                peer: bridge.peer.clone(),
                                holds_export_repair: false,
                                outer_offset: (0, 0),
                            },
                        )?;
                    }
                }
            } else if outer_owned {
                self.outer.borrow().mark_dirty(NodeDirtyFlag::NeedMeasure)?;
                self.outer.borrow().mark_dirty(NodeDirtyFlag::NeedLayout)?;
                self.outer.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
            }
        } else if let Some(peer) = bridge.peer.upgrade() {
            Self::cleanup_peer_bridge_state(&bridge.stack, &peer)?;
        }
        Ok(())
    }
    fn release_bridge_referencing(&self, id: nsh_core::NodeId) -> Result<()> {
        let matches = self.bridge.borrow().as_ref().is_some_and(|b| {
            b.stack.upgrade().is_some_and(|n| n.id == id)
                || b.peer.upgrade().is_some_and(|n| n.id == id)
                || b.screen_owner.upgrade().is_some_and(|n| n.id == id)
        });
        if matches {
            self.release_bridge()?;
        }
        let cancelled = self.pending_reattach.borrow_mut().take_referencing(id);
        for entry in cancelled {
            Self::cancel_bridge_reattach(Some(&self.outer), entry)?;
        }
        Ok(())
    }
    fn retire_composite(owner: std::rc::Weak<Node>, generation: u32, remaining: u8) {
        let Some(node) = owner.upgrade() else {
            return;
        };
        let Some((outer, active, closed)) = node.wrapper.borrow().as_ref().map(|w| {
            (
                w.outer.clone(),
                w.retiring.get().map(|(g, _)| g) == Some(generation)
                    && w.composite
                        .borrow()
                        .as_ref()
                        .is_some_and(|c| c.generation == generation),
                w.closed.get(),
            )
        }) else {
            return;
        };
        if closed || !active {
            return;
        }
        if node.needs_texture() {
            if let Some(wrapper) = node.wrapper.borrow().as_ref()
                && wrapper.retiring.get().map(|(g, _)| g) == Some(generation)
            {
                wrapper.retiring.set(None);
            }
            return;
        }
        if remaining == 2 {
            callback(|| {
                node.composite_dirty.set(true);
                if let Some(wrapper) = node.wrapper.borrow().as_ref()
                    && wrapper.retiring.get().map(|(g, _)| g) == Some(generation)
                {
                    wrapper
                        .retiring
                        .set(Some((generation, RetirePhase::Settling)));
                    if let Some(composite) = wrapper
                        .composite
                        .borrow()
                        .as_ref()
                        .filter(|c| c.generation == generation)
                    {
                        let _ = composite.update(&node);
                    }
                }
                Ok(())
            });
        }
        if remaining > 0 {
            let next = owner.clone();
            if outer
                .borrow()
                .post_frame_callback(move |_, _| {
                    Self::retire_composite(next, generation, remaining - 1);
                })
                .is_ok()
            {
                return;
            }
        }
        callback(|| {
            let retired = node.wrapper.borrow().as_ref().and_then(|wrapper| {
                if wrapper.retiring.get().map(|(g, _)| g) != Some(generation) {
                    return None;
                }
                wrapper.retiring.set(None);
                if wrapper
                    .composite
                    .borrow()
                    .as_ref()
                    .is_some_and(|c| c.generation == generation)
                {
                    wrapper.composite.borrow_mut().take()
                } else {
                    None
                }
            });
            if let Some(old) = retired {
                let old_generation = old.generation;
                node.composite_dirty.set(true);
                let _ = old.update(&node);
                if let Some(wrapper) = node.wrapper.borrow().as_ref() {
                    wrapper.lingering.borrow_mut().push(old);
                    Self::retire_lingering(Rc::downgrade(&node), old_generation, 12);
                    wrapper
                        .outer
                        .borrow()
                        .mark_dirty(NodeDirtyFlag::NeedRender)?;
                }
                node.schedule_subtree_paint(6)?;
                node.update_wrapper()?;
                node.restore_style()?;
                node.flush_arkui()?;
            }
            Ok(())
        });
    }
    fn advance_handoff_gate(owner: std::rc::Weak<Node>, generation: u32) {
        let Some(node) = owner.upgrade() else {
            return;
        };
        callback(|| {
            let step = {
                let borrow = node.wrapper.borrow();
                let Some(wrapper) = borrow.as_ref().filter(|w| !w.closed.get()) else {
                    return Ok(());
                };
                (
                    wrapper.outer.clone(),
                    wrapper.handoff.borrow_mut().on_frame_callback(generation),
                )
            };
            let (outer, step) = step;
            match step {
                crate::interop::handoff::FrameCallbackStep::Stale => {}
                crate::interop::handoff::FrameCallbackStep::ReArm => {
                    let next = owner.clone();
                    if outer
                        .borrow()
                        .post_frame_callback(move |_, _| {
                            Self::advance_handoff_gate(next, generation);
                        })
                        .is_err()
                    {
                        let forced = node
                            .wrapper
                            .borrow()
                            .as_ref()
                            .is_some_and(|w| w.handoff.borrow_mut().force_commit(generation));
                        if forced {
                            node.update_wrapper()?;
                            if node
                                .wrapper
                                .borrow()
                                .as_ref()
                                .is_some_and(Self::active_target_is_texture)
                            {
                                node.schedule_subtree_paint(4)?;
                            }
                            node.flush_arkui()?;
                        }
                    }
                }
                crate::interop::handoff::FrameCallbackStep::ReadyToDetach => {
                    node.update_wrapper()?;
                    if node
                        .wrapper
                        .borrow()
                        .as_ref()
                        .is_some_and(Self::active_target_is_texture)
                    {
                        node.schedule_subtree_paint(4)?;
                    }
                    node.flush_arkui()?;
                }
            }
            Ok(())
        });
    }
    fn retire_lingering(owner: std::rc::Weak<Node>, generation: u32, remaining: u8) {
        let Some(node) = owner.upgrade() else {
            return;
        };
        let Some((outer, present)) = node.wrapper.borrow().as_ref().map(|w| {
            (
                w.outer.clone(),
                !w.closed.get()
                    && w.lingering
                        .borrow()
                        .iter()
                        .any(|c| c.generation == generation),
            )
        }) else {
            return;
        };
        if !present {
            return;
        }
        if remaining > 0 {
            let next = owner.clone();
            if outer
                .borrow()
                .post_frame_callback(move |_, _| {
                    Self::retire_lingering(next, generation, remaining - 1);
                })
                .is_ok()
            {
                return;
            }
        }
        if let Some(wrapper) = node.wrapper.borrow().as_ref() {
            wrapper
                .lingering
                .borrow_mut()
                .retain(|c| c.generation != generation);
            let _ = wrapper.outer.borrow().mark_dirty(NodeDirtyFlag::NeedRender);
        }
    }
    pub(crate) fn invalidate_context(&self) {
        self.context_live.set(false);
        if let Some(old_export) = self.reattach_export.borrow_mut().take() {
            old_export.restore();
        }
        let _ = self.release_bridge();
        let drained = self.pending_reattach.borrow_mut().drain_all();
        let root = self.owner.upgrade();
        for entry in drained {
            let _ = Self::finish_bridge_reattach(root.as_deref(), entry);
        }
        self.settings.borrow_mut().take();
        self.route.borrow_mut().take();
        self.handoff.borrow_mut().reset();
        self.retired_for_background.set(false);
        // Keep the old target through SDK lifecycle cleanup; replacement queues its final owner.
    }
    pub(crate) fn active_target_id(&self) -> Option<u32> {
        self.settings
            .try_borrow()
            .ok()?
            .as_ref()
            .filter(|settings| settings.texture_surface.is_some())
            .map(|settings| settings.target_id)
    }
    pub(crate) fn active_target_is_texture(&self) -> bool {
        self.active_target_id().is_some()
    }
    fn replace_target(&self, owner: &Node) -> Result<u32> {
        if let Some(old_export) = self.reattach_export.borrow_mut().take() {
            old_export.restore();
        }
        let target = NativeAttachment::for_subtree(&self.outer, &owner.native)?;
        let id = target.id();
        if let Some(old) = self.target.replace(Some(target)) {
            owner.view_remove(old);
        }
        // The native target is the unique, nonreused callback generation.
        let weak = self.owner.clone();
        *self.route.borrow_mut() = Some(ViewRoute::new(id, weak, &owner.native));
        Ok(id)
    }
    fn ensure_context(&self, owner: &Node) -> Result<()> {
        let target_retired = self
            .target
            .borrow()
            .as_ref()
            .is_none_or(NativeAttachment::is_retired);
        if !self.context_live.get() || target_retired {
            self.handoff.borrow_mut().reset();
            self.settings.borrow_mut().take();
            self.replace_target(owner)?;
            if !self.context_live.get() && self.composite.borrow_mut().take().is_some() {
                let composite = crate::render::composition::worker::Composite::new(
                    &self
                        .owner
                        .upgrade()
                        .ok_or(crate::render::node::Error::InvalidNode(owner.id))?,
                )?;
                *self.composite.borrow_mut() = Some(composite);
            }
            self.context_live.set(true);
        }
        Ok(())
    }
    fn with_child<T>(
        &self,
        f: impl FnOnce(&ohos_arkui_binding::common::node::ArkUINode) -> T,
    ) -> Result<Option<T>> {
        if self.closed.get()
            || !self.context_live.get()
            || !self
                .owner
                .upgrade()
                .is_some_and(|owner| owner.host.check().is_ok())
        {
            return Ok(None);
        }
        self.target
            .borrow()
            .as_ref()
            .map_or(Ok(None), |target| Ok(target.with_child(f)?))
    }
    fn abort(self) {
        let cleanup = || -> Result<()> {
            if let Some(target) = self.target.borrow().as_ref() {
                target.invalidate()?;
            }
            self.outer.borrow().remove_from_parent()?;
            self.outer.borrow_mut().dispose()?;
            Ok(())
        };
        if let Err(error) = cleanup() {
            log::error!("Failed to release an unmounted OHOS wrapper: {error}");
            // A failed native cleanup must retain the complete owner, not just its target ID.
            std::mem::forget(self);
        }
    }
    fn new(owner: &Rc<Node>) -> Result<Self> {
        let outer = Custom::new()?.into_node();
        let setup = || -> Result<NativeAttachment> {
            outer
                .borrow()
                .set_length_metric_unit(LengthMetricUnit::Vp)?;
            Ok(NativeAttachment::for_subtree(&outer, &owner.native)?)
        };
        let target = match setup() {
            Ok(target) => target,
            Err(error) => {
                if let Err(cleanup) = outer.borrow_mut().dispose() {
                    log::error!("Failed to release a new OHOS wrapper: {cleanup}");
                    std::mem::forget(outer.clone());
                }
                return Err(error);
            }
        };
        let route = ViewRoute::new(target.id(), Rc::downgrade(owner), &owner.native);
        let weak = Rc::downgrade(owner);
        outer.borrow_mut().on_custom_draw(move |event| {
            callback(|| {
                let Some(owner) = weak.upgrade() else {
                    return Ok(());
                };
                let draw_scene = owner.scene_root.get()
                    && owner
                        .scene
                        .try_borrow()
                        .ok()
                        .is_some_and(|scene| scene.as_ref().is_some_and(|s| s.has_output()));
                let placeholder_fill = if draw_scene {
                    None
                } else {
                    owner
                        .wrapper
                        .try_borrow()
                        .ok()
                        .and_then(|borrow| {
                            let wrapper = borrow.as_ref()?;
                            if wrapper.closed.get() || owner.is_handoff_revealed(wrapper) {
                                return None;
                            }
                            wrapper.handoff.try_borrow().ok()?.active_fill()
                        })
                        .and_then(|_| owner.placeholder_fill())
                };
                if !(draw_scene || placeholder_fill.is_some()) {
                    return Ok(());
                }
                if let Some(raw) = event
                    .draw_context_in_draw()
                    .and_then(|context| context.canvas())
                {
                    // SAFETY: ArkUI owns this canvas until this draw callback returns.
                    let canvas = unsafe { ohos_drawing_binding::Canvas::from_raw_borrowed(raw) };
                    if draw_scene {
                        if let Some(scene) = owner.scene.borrow().as_ref() {
                            scene.draw(owner.id, &canvas, owner.scale.get());
                        }
                    } else if let Some(fill) = placeholder_fill {
                        let frame = owner.frame.get();
                        draw_placeholder_fill(
                            &canvas,
                            frame.width,
                            frame.height,
                            owner.scale.get(),
                            fill,
                        );
                        let arm = owner.wrapper.try_borrow().ok().and_then(|borrow| {
                            let wrapper = borrow.as_ref()?;
                            let generation = wrapper
                                .handoff
                                .try_borrow_mut()
                                .ok()?
                                .note_placeholder_drawn()?;
                            Some((wrapper.outer.clone(), generation))
                        });
                        if let Some((outer, generation)) = arm {
                            let next = Rc::downgrade(&owner);
                            if outer
                                .borrow()
                                .post_frame_callback(move |_, _| {
                                    Wrapper::advance_handoff_gate(next, generation);
                                })
                                .is_err()
                                && let Ok(borrow) = owner.wrapper.try_borrow()
                                && let Some(wrapper) = borrow.as_ref()
                                && let Ok(mut handoff) = wrapper.handoff.try_borrow_mut()
                            {
                                handoff.force_commit(generation);
                            }
                        }
                    }
                }
                Ok(())
            });
        });
        let weak = Rc::downgrade(owner);
        outer.borrow_mut().on_custom_measure(move |event| {
            callback(|| {
                if let Some(owner) = weak.upgrade() {
                    if let Some(modal) = owner.modal.borrow().as_ref() {
                        let size = modal.measure(event)?;
                        let mut raw = owner.raw_frame.get();
                        raw.width = size.width;
                        raw.height = size.height;
                        owner.set_frame(raw)?;
                    }
                    let frame = owner.frame.get();
                    let mut width = (frame.width * owner.scale.get()).round() as i32;
                    let mut height = (frame.height * owner.scale.get()).round() as i32;
                    if !owner.clips_children() && !owner.children.borrow().is_empty() {
                        width = width.max(1);
                        height = height.max(1);
                    }
                    event.set_measured_size(width, height)?;
                    if let Some(wrapper) = owner.wrapper.borrow().as_ref() {
                        if let Some(result) = wrapper.with_child(|node| {
                            crate::render::layout::ChildMeasure::new(event).child(node)
                        })? {
                            result?;
                        }
                        if let Some(composite) = wrapper.composite.borrow().as_ref() {
                            composite.measure(event)?;
                        }
                        for composite in wrapper.lingering.borrow().iter() {
                            composite.measure(event)?;
                        }
                        if let Some(peer) = wrapper.hosted_bridge_peer() {
                            crate::render::layout::ChildMeasure::new(event)
                                .child(&peer.mounted().borrow())?;
                        }
                        for peer in wrapper.pending_outer_bridge_peers() {
                            crate::render::layout::ChildMeasure::new(event)
                                .child(&peer.mounted().borrow())?;
                        }
                    }
                }
                Ok(())
            })
        });
        let weak = Rc::downgrade(owner);
        outer.borrow_mut().on_custom_layout(move |event| {
            callback(|| {
                let position = event.position_in_layout();
                event.set_layout_position(position.x, position.y)?;
                if let Some(owner) = weak.upgrade()
                    && let Some(wrapper) = owner.wrapper.borrow().as_ref()
                {
                    let texture_origin = wrapper
                        .composite
                        .borrow()
                        .as_ref()
                        .filter(|composite| composite.surface().is_some())
                        .map_or((0.0, 0.0), |composite| composite.origin());
                    if let Some(result) = wrapper.with_child(|node| {
                        let (x, y) = if wrapper.active_target_is_texture() {
                            texture_origin
                        } else {
                            (0.0, 0.0)
                        };
                        event.layout_child(
                            node,
                            (x * owner.scale.get()).round() as i32,
                            (y * owner.scale.get()).round() as i32,
                        )
                    })? {
                        result?;
                    }
                    if let Some(composite) = wrapper.composite.borrow().as_ref() {
                        composite.layout(event)?;
                    }
                    for composite in wrapper.lingering.borrow().iter() {
                        composite.layout(event)?;
                    }
                    if let Some((peer, x, y)) = wrapper.hosted_bridge_layout(&owner) {
                        peer.place(event, x, y)?;
                    }
                    for (peer, x, y) in wrapper.pending_outer_bridge_layouts() {
                        peer.place(event, x, y)?;
                    }
                }
                Ok(())
            })
        });
        Ok(Self {
            closed: std::cell::Cell::new(false),
            outer,
            target: RefCell::new(Some(target)),
            route: RefCell::new(Some(route)),
            handoff: RefCell::default(),
            bridge: RefCell::default(),
            pending_reattach: RefCell::default(),
            reattach_export: RefCell::default(),
            context_live: Cell::new(true),
            owner: Rc::downgrade(owner),
            settings: std::cell::RefCell::default(),
            composite: std::cell::RefCell::default(),
            retiring: Cell::new(None),
            lingering: std::cell::RefCell::default(),
            semantics: Cell::new(None),
            measured: Cell::new(None),
            retired_for_background: Cell::new(false),
        })
    }
    /// Sends the ArkTS view its settings; whether they changed.
    fn update(&self, owner: &Node) -> Result<bool> {
        if self.closed.get() || owner.host.check().is_err() {
            return Ok(false);
        }
        self.ensure_context(owner)?;
        let now = std::time::Instant::now();
        let desired_surface = self
            .composite
            .borrow()
            .as_ref()
            .and_then(|composite| composite.surface());
        let previous_texture = self
            .settings
            .borrow()
            .as_ref()
            .map(|previous| previous.texture_surface.is_some());
        let is_root = owner.id == nsh_core::ROOT_ID;
        let has_mounted_child = matches!(self.with_child(|_| ()), Ok(Some(())));
        let has_mounted_direct = previous_texture == Some(false) && has_mounted_child;
        let revealed = owner.is_handoff_revealed(self);
        let export_requested = if owner.scene_active() {
            owner.scene_root.get() && self.composite.borrow().is_some()
        } else {
            self.composite.borrow().is_some()
        };
        if previous_texture.is_none() && !is_root && export_requested && desired_surface.is_none() {
            self.handoff.borrow_mut().begin_mount_export(now);
            return Ok(false);
        }
        if (!export_requested && owner.shaders.surface_active()) || revealed {
            self.retired_for_background.set(false);
        }
        let is_mount_or_prewarm = !has_mounted_direct
            || self.retired_for_background.get()
            || (owner.shaders.keep_alive(owner.id) && owner.shader_layers.borrow().is_empty());
        let covered_by_live_bridge = is_root && self.has_live_bridge(export_requested, revealed);
        let inputs = crate::interop::handoff::HandoffInputs {
            closed: self.closed.get(),
            fill: owner.placeholder_fill(),
            is_root,
            is_mount_or_prewarm,
            covered_by_live_bridge,
            has_mounted_direct,
            active_target_is_texture: previous_texture == Some(true),
            export_requested,
            desired_surface_ready: desired_surface.is_some(),
            revealed,
        };
        let (mut outcome, finished_gap) = self.handoff.borrow_mut().prepare_target(inputs, now);
        if let Some((gap, gap_outcome, gap_kind)) = finished_gap {
            log::info!(
                "[NSH] {} ms={:.1} node={} outcome={}",
                gap_kind.log_tag(),
                gap.as_secs_f64() * 1000.0,
                owner.id,
                gap_outcome.as_str(),
            );
        }
        if let Some(generation) = outcome.arm_frame_callback {
            let weak = self.owner.clone();
            if self
                .outer
                .borrow()
                .post_frame_callback(move |_, _| {
                    Self::advance_handoff_gate(weak, generation);
                })
                .is_err()
            {
                self.handoff.borrow_mut().force_commit(generation);
                outcome.hold_direct_target = false;
            }
        }
        let active_surface = if outcome.hold_direct_target {
            None
        } else {
            desired_surface
        };
        if self
            .settings
            .borrow()
            .as_ref()
            .is_some_and(|previous| previous.texture_surface != active_surface)
        {
            self.replace_target(owner)?;
        }
        if let Some(route) = self.route.borrow().as_ref() {
            route.texture(
                active_surface.is_some()
                    || self
                        .settings
                        .borrow()
                        .as_ref()
                        .is_some_and(|previous| previous.texture_surface.is_some()),
            );
        }
        let Some(target_id) = self.target.borrow().as_ref().map(NativeAttachment::id) else {
            return Ok(false);
        };
        let spec = owner.accessibility_state.spec.borrow();
        let frame = owner.frame.get();
        let value = crate::accessibility::value::spoken_value(&spec).unwrap_or_default();
        let actions: Vec<MenuItem> = if cfg!(feature = "api-26") {
            spec.actions
                .iter()
                .take(16)
                .map(|action| MenuItem {
                    kind: "action".into(),
                    name: action.name.to_string(),
                    label: action.label.as_deref().unwrap_or(&action.name).to_owned(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let texture_insets = self
            .composite
            .borrow()
            .as_ref()
            .map(|composite| composite.insets());
        let settings = ViewOptions {
            texture_insets: if active_surface.is_some() {
                texture_insets
            } else {
                None
            },
            texture_surface: active_surface,
            target_id,
            test_id: owner.interaction.test_id(),
            width: frame.width as f64,
            height: frame.height as f64,
            hue: 0.0,
            semantic: owns_semantics(&spec),
            clickable: owner.interaction.mask().contains(EventKind::Press)
                || spec
                    .actions
                    .iter()
                    .any(|action| action.name.as_ref() == "activate"),
            disabled: spec.state.disabled,
            accessible: spec.accessible,
            hidden: spec.hidden,
            label: owner.accessibility_state.label(owner),
            hint: spec.hint.as_deref().unwrap_or("").to_owned(),
            api23: cfg!(feature = "api-23") && !value.is_empty(),
            api26: !actions.is_empty(),
            value,
            role: role(spec.role).to_owned(),
            checked: spec.state.checked.map(|value| value == Checked::True),
            selected: spec.state.selected,
            actions,
            menu: owner.selection_menu(),
        };
        drop(spec);
        let mut changed = false;
        {
            let mut previous = self.settings.borrow_mut();
            if previous.as_ref() != Some(&settings) {
                *previous = Some(settings.clone());
                owner.view_update(settings);
                changed = true;
            }
        }
        self.handoff.borrow_mut().note_target_updated(
            self.active_target_is_texture(),
            revealed,
            now,
        );
        if is_root {
            self.sync_bridge(
                owner,
                export_requested,
                self.active_target_is_texture(),
                revealed,
            )?;
        }
        if outcome.repaint_outer {
            self.outer.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        Ok(changed)
    }
    pub(crate) fn close(&self, owner: &Node) -> Result<()> {
        self.closed.set(true);
        if let Some(old_export) = self.reattach_export.borrow_mut().take() {
            old_export.restore();
        }
        let _ = self.release_bridge();
        let drained = self.pending_reattach.borrow_mut().drain_all();
        for entry in drained {
            let _ = Self::cancel_bridge_reattach(Some(&self.outer), entry);
        }
        self.retiring.set(None);
        self.retired_for_background.set(false);
        self.handoff.borrow_mut().reset();
        self.route.borrow_mut().take();
        if let Some(old) = self.target.borrow_mut().take() {
            owner.view_remove(old);
        }
        self.composite.borrow_mut().take();
        self.lingering.borrow_mut().clear();
        Ok(())
    }
}
impl Node {
    fn is_handoff_revealed(&self, wrapper: &Wrapper) -> bool {
        if self.scene_active() {
            self.scene_root.get()
                && self
                    .scene
                    .try_borrow()
                    .ok()
                    .is_some_and(|scene| scene.as_ref().is_some_and(|s| s.has_output()))
        } else {
            wrapper
                .composite
                .try_borrow()
                .ok()
                .is_some_and(|composite| composite.as_ref().is_some_and(|c| c.is_revealed()))
        }
    }
    fn placeholder_fill(&self) -> Option<crate::interop::handoff::PlaceholderFill> {
        if !self.paint_visible() {
            return None;
        }
        if self.scene_active() && !self.scene_root.get() {
            return None;
        }
        let frame = self.frame.get();
        if frame.width <= 0.0 || frame.height <= 0.0 {
            return None;
        }
        let style = self.style.try_borrow().ok()?;
        if self.visual_opacity_for(style.opacity) < 0.999 {
            return None;
        }
        let style_color = style.background_color;
        let style_bg = crate::text::typography::argb(style_color);
        drop(style);
        let default_surface_bg = self.id == nsh_core::ROOT_ID
            || self.kind == nsh_core::ViewKind::Screen
            || self.modal.try_borrow().ok().is_some_and(|modal| {
                modal
                    .as_ref()
                    .is_some_and(|m| m.presentation() != nsh_core::ModalPresentation::FullScreen)
            });
        let argb = if (style_bg >> 24) == 0xff {
            Some(style_bg)
        } else if default_surface_bg && style_color.is_transparent() {
            let dark = self
                .appearance
                .get()
                .resolve(self.platform.get().color_scheme)
                == nsh_engine::ColorScheme::Dark;
            self.theme
                .try_borrow()
                .ok()
                .map(|theme| theme.color("background", dark))
                .filter(|&color| (color >> 24) == 0xff)
        } else {
            None
        }?;
        Some(crate::interop::handoff::PlaceholderFill {
            argb,
            radii: self.border_radii(),
        })
    }
    pub(crate) fn needs_texture(&self) -> bool {
        self.effects_plan().requires_texture
    }
    pub(crate) fn wrapper_owns_semantics(&self) -> bool {
        self.wrapper.borrow().is_some() && owns_semantics(&self.accessibility_state.spec.borrow())
    }
    pub(crate) fn accessibility_id(&self) -> Result<i32> {
        use ohos_arkui_binding::types::attribute::ArkUINodeAttributeType as A;
        let spec = self.accessibility_state.spec.borrow();
        if owns_semantics(&spec)
            && let Some(wrapper) = self.wrapper.borrow().as_ref()
            && let Some(id) =
                wrapper.with_child(|node| node.i32_attribute(A::AccessibilityId, 0))?
        {
            return Ok(id?);
        }
        if let Some(input) = self.input.borrow().as_ref()
            && let Some(id) = input.accessibility_id()?
        {
            return Ok(id);
        }
        Ok(self.native.borrow().i32_attribute(A::AccessibilityId, 0)?)
    }
    pub(crate) fn mounted(&self) -> NativeNode {
        self.wrapper
            .borrow()
            .as_ref()
            .map_or_else(|| self.native.clone(), |wrapper| wrapper.outer.clone())
    }
    pub(crate) fn ensure_wrapper(self: &Rc<Self>) -> Result<()> {
        if self
            .wrapper
            .borrow()
            .as_ref()
            .is_some_and(|wrapper| wrapper.closed.get())
        {
            return Ok(());
        }
        let spec = self.accessibility_state.spec.borrow();
        if cfg!(feature = "api-26") && spec.actions.len() > 16 {
            log::warn!("OHOS accessibility: showing the first 16 custom actions");
        }
        let has_semantics = owns_semantics(&spec);
        drop(spec);
        let requires_texture = self.effects_plan().requires_texture;
        // Establish the root's layout host before it has scroll descendants.
        // Adding it around an already laid-out tree resets native scroll offsets.
        let needed = self.id == nsh_core::ROOT_ID
            || self.kind == nsh_core::ViewKind::Modal
            || self.selectable()
            || has_semantics
            || requires_texture;
        let mut created_wrapper = false;
        if needed && self.wrapper.borrow().is_none() {
            self.enable_layout();
            self.reset_visual_baseline(&self.native)?;
            let parent = self.mount_parent.borrow().upgrade();
            let index = parent
                .as_ref()
                .map(|parent| {
                    parent
                        .borrow()
                        .children()
                        .iter()
                        .position(|child| Rc::ptr_eq(child, &self.native))
                        .ok_or(crate::render::node::Error::InvalidNode(self.id))
                })
                .transpose()?;
            let wrapper = Wrapper::new(self)?;
            if let Some((parent, index)) = parent.zip(index) {
                let mut parent = parent.borrow_mut();
                if let Err(error) = parent.remove_child(index) {
                    wrapper.abort();
                    return Err(error.into());
                }
                if let Err(error) = parent.insert_child(wrapper.outer.clone(), index) {
                    let restored = parent.insert_child(self.native.clone(), index);
                    wrapper.abort();
                    restored?;
                    return Err(error.into());
                }
            }
            *self.wrapper.borrow_mut() = Some(wrapper);
            created_wrapper = true;
            self.accessibility_wrappers
                .set(self.accessibility_wrappers.get() + 1);
            self.invalidate_mounted_geometry();
            self.update_layout_bounds()?;
            // The wrapper owns this node's semantics; native descendants keep their own entries.
            #[cfg(feature = "api-23")]
            self.accessibility_provider.clear();
        }
        let mut created_composite = false;
        if !requires_texture {
            let immediate_retire = !self.shaders.hosts_retainable()
                || self.wrapper.borrow().as_ref().is_some_and(|wrapper| {
                    wrapper
                        .composite
                        .borrow()
                        .as_ref()
                        .is_some_and(|composite| !composite.is_revealed())
                });
            if immediate_retire {
                if let Some(wrapper) = self.wrapper.borrow().as_ref() {
                    wrapper.retiring.set(None);
                    if wrapper.composite.borrow_mut().take().is_some() {
                        if !self.shaders.hosts_retainable() {
                            wrapper.retired_for_background.set(true);
                        }
                        wrapper
                            .outer
                            .borrow()
                            .mark_dirty(NodeDirtyFlag::NeedRender)?;
                        self.composite_dirty.set(true);
                        self.restore_style()?;
                    }
                }
            } else {
                let start_generation = self.wrapper.borrow().as_ref().and_then(|wrapper| {
                    let generation = wrapper.composite.borrow().as_ref()?.generation;
                    if wrapper.retiring.get().is_none() {
                        wrapper
                            .retiring
                            .set(Some((generation, RetirePhase::Exporting)));
                        Some(generation)
                    } else {
                        None
                    }
                });
                if let Some(generation) = start_generation {
                    self.composite_dirty.set(true);
                    self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
                    if let Some(wrapper) = self.wrapper.borrow().as_ref() {
                        wrapper
                            .outer
                            .borrow()
                            .mark_dirty(NodeDirtyFlag::NeedRender)?;
                    }
                    Wrapper::retire_composite(Rc::downgrade(self), generation, 3);
                }
            }
        } else if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            wrapper.retiring.set(None);
            if wrapper.composite.borrow().is_none() {
                if wrapper.target.borrow().is_none() {
                    return Err(crate::render::node::Error::Unsupported(
                        "wrapper target retired",
                    ));
                }
                let composite = crate::render::composition::worker::Composite::new(self)?;
                *wrapper.composite.borrow_mut() = Some(composite);
                created_composite = true;
            }
        }
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            let spec = self.accessibility_state.spec.borrow();
            let semantic = owns_semantics(&spec);
            use ohos_arkui_binding::types::view::AccessibilityMode as Mode;
            let mode = if semantic && spec.accessible == Some(true) {
                // The ArkTS group is the single semantic entry for this subtree.
                Mode::DisabledForDescendants
            } else if semantic {
                Mode::Disabled
            } else if spec.hidden {
                Mode::DisabledForDescendants
            } else {
                match spec.accessible {
                    Some(true) => Mode::Enabled,
                    Some(false) => Mode::Disabled,
                    None => Mode::Auto,
                }
            };
            let next = (i32::from(mode), !semantic && spec.accessible == Some(true));
            // Style and text updates come here too: give the wrapped node
            // its semantics only when they change or its own props reset them.
            if wrapper.semantics.replace(Some(next)) != Some(next) {
                use ohos_arkui_binding::types::attribute::ArkUINodeAttributeType as A;
                let native = self.native.borrow();
                // The wrapper carries the test ID.
                native.reset_attribute(A::Id)?;
                native.set_i32_attribute(A::AccessibilityMode, next.0)?;
                native.set_bool_attribute(A::AccessibilityGroup, next.1)?;
            }
        }
        if created_wrapper || created_composite {
            self.apply_visual_style(&self.style.borrow())?;
        }
        self.update_wrapper()
    }
    pub(crate) fn update_wrapper(&self) -> Result<()> {
        if let Some(scroll) = self.scroll.borrow().as_ref()
            && let Some(plane) = scroll.overlay.borrow().as_ref()
        {
            plane.update(self)?;
        }
        if let Some(wrapper) = self
            .wrapper
            .borrow()
            .as_ref()
            .filter(|wrapper| !wrapper.closed.get())
        {
            let composite = if let Some(composite) = wrapper.composite.borrow().as_ref() {
                composite.update(self)?
            } else {
                false
            };
            let changed = wrapper.update(self)?;
            // The outer node measures at the frame's size (a modal at its
            // content's) and places the ArkTS view at the composite's origin:
            // measure again only when one of those, or what the view shows,
            // may have changed.
            let frame = self.frame.get();
            let size = Some((frame.width, frame.height));
            let resized = wrapper.measured.replace(size) != size;
            let modal = self.modal.borrow().is_some();
            if changed || composite || modal || resized {
                wrapper
                    .outer
                    .borrow()
                    .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
                if resized && wrapper.handoff.borrow().active_fill().is_some() {
                    wrapper
                        .outer
                        .borrow()
                        .mark_dirty(NodeDirtyFlag::NeedRender)?;
                }
            }
        }
        self.composite_dirty.set(false);
        Ok(())
    }
    /// The wrapped node's own props rewrote its test ID and accessibility
    /// mode: the wrapper gives them again at the next update.
    pub(crate) fn wrapper_semantics_reset(&self) {
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            wrapper.semantics.set(None);
        }
    }
    pub(crate) fn visual_node(&self) -> NativeNode {
        self.mounted()
    }
    pub(crate) fn can_bridge_cold_export(&self) -> bool {
        if self.id != nsh_core::ROOT_ID || !self.needs_texture() || self.navigation_capture.get() {
            return false;
        }
        self.wrapper.borrow().as_ref().is_some_and(|wrapper| {
            wrapper.is_context_live()
                && wrapper.composite.borrow().is_some()
                && !self.is_handoff_revealed(wrapper)
        })
    }
    pub(crate) fn install_root_bridge(
        &self,
        stack: &Rc<Self>,
        peer: &Rc<Self>,
        screen_owner: &Rc<Self>,
    ) -> Result<()> {
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            wrapper.release_bridge()?;
            *wrapper.bridge.borrow_mut() = Some(Rc::new(RootBridge {
                stack: Rc::downgrade(stack),
                peer: Rc::downgrade(peer),
                screen_owner: Rc::downgrade(screen_owner),
                hosted_on_outer: Cell::new(false),
            }));
        }
        self.update_wrapper()
    }
    pub(crate) fn release_root_bridge_for(&self, id: nsh_core::NodeId) -> Result<()> {
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            wrapper.release_bridge_referencing(id)?;
        }
        Ok(())
    }
    pub(crate) fn release_enclosing_root_bridge(&self) -> Result<()> {
        let mut at = self.parent_node.borrow().upgrade();
        while let Some(node) = at {
            if node.id == nsh_core::ROOT_ID {
                return node.release_root_bridge_for(self.id);
            }
            at = node.parent_node.borrow().upgrade();
        }
        Ok(())
    }
    pub(crate) fn flush_pending_bridge_reattach_if_direct(&self) -> Result<()> {
        if self.inside_texture_export() {
            return Ok(());
        }
        let drained = self
            .wrapper
            .try_borrow()
            .ok()
            .and_then(|borrow| {
                let wrapper = borrow.as_ref()?;
                Some(wrapper.pending_reattach.borrow_mut().drain_all())
            })
            .unwrap_or_default();
        for entry in drained {
            Wrapper::finish_bridge_reattach(Some(self), entry)?;
        }
        Ok(())
    }
    pub(crate) fn active_export_target_id(&self) -> Option<u32> {
        if self.bridged_on_root_outer.get() {
            return None;
        }
        let own_target = |node: &Self| -> Option<u32> {
            if let Some(id) = node.scroll.try_borrow().ok().and_then(|scroll| {
                scroll
                    .as_ref()?
                    .overlay
                    .try_borrow()
                    .ok()?
                    .as_ref()?
                    .active_target_id()
            }) {
                return Some(id);
            }
            node.wrapper
                .try_borrow()
                .ok()
                .and_then(|wrapper| wrapper.as_ref()?.active_target_id())
        };
        if let Some(id) = own_target(self) {
            return Some(id);
        }
        let next_parent = |node: &Self| -> Option<Rc<Self>> {
            node.parent_node
                .try_borrow()
                .ok()
                .and_then(|parent| parent.upgrade())
                .or_else(|| {
                    node.retained
                        .get()
                        .then(|| node.retained_owner.try_borrow().ok()?.upgrade())
                        .flatten()
                })
        };
        let mut at = next_parent(self);
        while let Some(node) = at {
            if node.bridged_on_root_outer.get() {
                return None;
            }
            if let Some(id) = own_target(&node) {
                return Some(id);
            }
            at = next_parent(&node);
        }
        None
    }
    pub(crate) fn inside_texture_export(&self) -> bool {
        self.active_export_target_id().is_some()
    }
    pub(crate) fn composite_ready(&self, generation: u32) -> Result<()> {
        let mut initialized = false;
        let mut revealed = false;
        let mut drop_failed = false;
        if let Some(scroll) = self.scroll.borrow().as_ref()
            && let Some(plane) = scroll.overlay.borrow().as_ref()
        {
            plane.ready(self, generation)?;
        }
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            for composite in wrapper
                .lingering
                .borrow()
                .iter()
                .filter(|composite| composite.generation == generation)
            {
                composite.ready();
            }
            if let Some(composite) = wrapper
                .composite
                .borrow()
                .as_ref()
                .filter(|composite| composite.generation == generation)
            {
                (initialized, revealed) = composite.ready();
                if revealed {
                    wrapper.retired_for_background.set(false);
                    let (_, gap) = wrapper
                        .handoff
                        .borrow_mut()
                        .note_revealed(std::time::Instant::now());
                    if let Some((gap, gap_kind)) = gap {
                        log::info!(
                            "[NSH] {} ms={:.1} node={} outcome=revealed",
                            gap_kind.log_tag(),
                            gap.as_secs_f64() * 1000.0,
                            self.id,
                        );
                    }
                }
                drop_failed = composite.is_failed();
                if composite.needs_shader_frame() {
                    self.animator.request_frame();
                }
                wrapper
                    .outer
                    .borrow()
                    .mark_dirty(NodeDirtyFlag::NeedRender)?;
            }
            if drop_failed {
                wrapper.composite.borrow_mut().take();
            }
        }
        if initialized || revealed || drop_failed {
            self.restore_style()?;
            self.update_wrapper()?;
            self.invalidate_scene();
            if !drop_failed {
                self.schedule_subtree_paint(4)?;
            }
        }
        Ok(())
    }
}
fn role(role: R) -> &'static str {
    match role {
        R::None => "",
        R::Button | R::ImageButton | R::KeyboardKey => "button",
        R::Checkbox => "checkbox",
        R::Switch => "switch",
        R::Radio => "radio",
        R::Adjustable | R::Scrollbar => "slider",
        R::Image => "image",
        R::Link => "link",
        R::List => "list",
        R::Grid => "grid",
        R::Progressbar => "progress",
        R::Search => "search",
        R::Menuitem => "menuitem",
        _ => "text",
    }
}
