//! Targeted custom-paint invalidation for `RENDER_TYPE_TEXTURE` subtrees.
//!
//! Under `RenderType.RENDER_TYPE_TEXTURE`, ArkUI culls `on_custom_draw` for
//! nodes outside a `Scroll` viewport (clearing `NeedRender`) and discards a
//! node's `RSCanvasNode` display list once that node scrolls completely outside
//! the `Scroll` viewport. Conversely, while an unclipped custom node remains
//! inside the `Scroll` viewport, its recorded `RSCanvasNode` survives scroll
//! translations without re-recording.
//!
//! By marking `drawn_export_target` only when `on_custom_draw` runs while
//! unclipped inside ancestor `Scroll` viewports, evicting `drawn_export_target`
//! in Rust when a node leaves the strict `Scroll` viewport, and marking only
//! newly entering / unrecorded custom nodes, scroll flings avoid re-recording
//! the entire exported subtree on every frame while keeping all custom paint
//! intact across arbitrary scroll directions.

use std::rc::Weak;

use nsh_core::ViewKind;
use ohos_arkui_binding::types::advanced::NodeDirtyFlag;

use super::{Node, Result, callback};
pub(crate) use crate::render::subtree_paint::{
    SCROLL_EXPORT_LOOKAHEAD_PT, is_rect_unclipped_in_scroll, needs_custom_export_record,
    rect_intersects_viewport, scroll_viewport_rect,
};

fn native_window_rect(node: &Node) -> Option<(i32, i32, i32, i32)> {
    let native = node.mounted();
    let native = native.borrow();
    let origin = native.position_with_translate_in_window().ok()?;
    let size = native.layout_size().ok()?;
    Some((
        origin.x,
        origin.y,
        origin.x + size.width,
        origin.y + size.height,
    ))
}

impl Node {
    #[inline]
    pub(crate) fn clear_drawn_export_target(&self) {
        self.drawn_export_target.set(0);
        self.drawn_outline_export_target.set(0);
    }

    pub(crate) fn own_active_export_target_id(&self) -> Option<u32> {
        if let Some(id) = self.scroll.try_borrow().ok().and_then(|scroll| {
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
        self.wrapper
            .try_borrow()
            .ok()
            .and_then(|wrapper| wrapper.as_ref()?.active_target_id())
    }

    /// Returns `target_id` when `self` is inside an exported subtree and not
    /// currently outside or clipped by an ancestor `Scroll` within that same
    /// export subtree; otherwise returns `0` so `self` stays eligible for
    /// re-recording once it enters the `Scroll` viewport unclipped.
    pub(crate) fn recordable_export_target_id(&self) -> u32 {
        let Some(target_id) = self.active_export_target_id() else {
            return 0;
        };
        if self.own_active_export_target_id() == Some(target_id) {
            return target_id;
        }
        let mut own_rect: Option<(i32, i32, i32, i32)> = None;
        let mut at = self.parent_node.borrow().upgrade();
        while let Some(ancestor) = at {
            let is_export_host = ancestor.own_active_export_target_id() == Some(target_id);
            if ancestor.scroll.borrow().is_some() {
                let Some(own) = own_rect.or_else(|| {
                    own_rect = native_window_rect(self);
                    own_rect
                }) else {
                    return 0;
                };
                let Some(scr) = native_window_rect(&ancestor) else {
                    return 0;
                };
                if !is_rect_unclipped_in_scroll(own, scr, 2) {
                    return 0;
                }
            }
            if is_export_host {
                break;
            }
            at = ancestor.parent_node.borrow().upgrade();
        }
        target_id
    }

    fn has_active_custom_draw(&self) -> bool {
        if !self.drawing.get() {
            return false;
        }
        let frame = self.frame.get();
        if frame.width <= 0.0 || frame.height <= 0.0 {
            return false;
        }
        self.text_drawing.borrow().is_some()
            || self.canvas.borrow().is_some()
            || self.selection.range.get().is_some()
            || self
                .style
                .borrow()
                .effects
                .as_ref()
                .is_some_and(|e| !e.background_images.is_empty() || !e.box_shadows.is_empty())
    }

    fn has_active_outline_draw(&self) -> bool {
        if !self.outlined.get() {
            return false;
        }
        let frame = self.frame.get();
        frame.width > 0.0 && frame.height > 0.0 && self.style.borrow().outline.width.0 > 0.0
    }

    fn clear_own_and_shadow_export_target(&self) {
        self.clear_drawn_export_target();
        if let Some(layer) = self.shadow_layer.borrow().as_ref() {
            layer.clear_drawn_export_target();
        }
    }

    fn evict_subtree_drawn_export_target(&self, target_id: u32) {
        if !self.active.get() || self.bridged_on_root_outer.get() {
            return;
        }
        if self
            .own_active_export_target_id()
            .is_some_and(|own| own != target_id)
        {
            return;
        }
        self.clear_own_and_shadow_export_target();
        for child in self.children.borrow().iter().filter_map(Weak::upgrade) {
            child.evict_subtree_drawn_export_target(target_id);
        }
        for child in self
            .retained_children
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
        {
            if child.retained.get() {
                child.evict_subtree_drawn_export_target(target_id);
            }
        }
    }

    fn mark_unrecorded_own_paint(&self, target_id: u32) -> Result<usize> {
        let mut marked = 0usize;
        let need_draw = needs_custom_export_record(
            self.has_active_custom_draw(),
            self.drawn_export_target.get(),
            target_id,
        );
        let need_outline = needs_custom_export_record(
            self.has_active_outline_draw(),
            self.drawn_outline_export_target.get(),
            target_id,
        );
        if need_draw || need_outline {
            self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
            marked += 1;
        }
        if let Some(layer) = self.shadow_layer.borrow().as_ref()
            && layer.invalidate_if_unrecorded(target_id)?
        {
            marked += 1;
        }
        Ok(marked)
    }

    fn has_nontrivial_visual_offset(&self) -> bool {
        self.input_transform.get() != nsh_core::Matrix::IDENTITY
            || self.extra.get().1 != crate::render::transition::Extra::NONE
            || self.presentation.get().1 != 0.0
            || self.navigation_presentation.get().1 != 0.0
    }

    fn invalidate_unrecorded_walk(
        &self,
        target_id: u32,
        is_root: bool,
        strict_vp: Option<[f32; 4]>,
        mark_vp: Option<[f32; 4]>,
        origin: (f32, f32),
    ) -> Result<usize> {
        if !self.active.get()
            || self.bridged_on_root_outer.get()
            || (!is_root && (self.kind == ViewKind::Modal || self.style.borrow().hidden))
        {
            return Ok(0);
        }
        if !is_root
            && self
                .own_active_export_target_id()
                .is_some_and(|own| own != target_id)
        {
            return Ok(0);
        }

        let frame = self.frame.get();
        let pinned = self.pinned.get();
        let (nx, ny) = if is_root {
            (0.0, 0.0)
        } else {
            (origin.0 + frame.x + pinned.0, origin.1 + frame.y + pinned.1)
        };
        let rect = [
            nx,
            ny,
            nx + frame.width.max(0.0),
            ny + frame.height.max(0.0),
        ];
        let (eff_strict_vp, eff_mark_vp) = if self.has_nontrivial_visual_offset() {
            (None, None)
        } else {
            (strict_vp, mark_vp)
        };

        let outset = self
            .shadow_layer
            .borrow()
            .as_ref()
            .map_or(0.0, |layer| layer.outset_points(self.scale.get()));
        let own_rect = [
            rect[0] - outset,
            rect[1] - outset,
            rect[2] + outset,
            rect[3] + outset,
        ];

        // Evict `drawn_export_target` once a node scrolls completely outside the
        // strict `Scroll` viewport (where ArkUI discards its `RSCanvasNode`).
        if !is_root
            && let Some(svp) = eff_strict_vp
            && !rect_intersects_viewport(own_rect, svp)
        {
            self.clear_own_and_shadow_export_target();
        }

        let mut marked = 0usize;
        if eff_mark_vp.is_none_or(|mvp| rect_intersects_viewport(own_rect, mvp)) {
            marked += self.mark_unrecorded_own_paint(target_id)?;
        }

        if let Some(mvp) = eff_mark_vp
            && !is_root
            && self.clips_children()
            && !rect_intersects_viewport(rect, mvp)
        {
            self.evict_subtree_drawn_export_target(target_id);
            return Ok(marked);
        }

        let child_origin = if !is_root && let Some(scroll) = self.scroll.borrow().as_ref() {
            let off = scroll.current_offset();
            (nx - off.0, ny - off.1)
        } else {
            (nx, ny)
        };

        for child in self.children.borrow().iter().filter_map(Weak::upgrade) {
            marked += child.invalidate_unrecorded_walk(
                target_id,
                false,
                eff_strict_vp,
                eff_mark_vp,
                child_origin,
            )?;
        }
        for child in self
            .retained_children
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
        {
            if child.retained.get() {
                marked += child.invalidate_unrecorded_walk(
                    target_id,
                    false,
                    eff_strict_vp,
                    eff_mark_vp,
                    child_origin,
                )?;
            }
        }
        Ok(marked)
    }

    fn flush_export_host_for_target(&self, target_id: u32) -> Result<()> {
        if let Some(scroll) = self.scroll.borrow().as_ref() {
            scroll
                .native
                .borrow()
                .mark_dirty(NodeDirtyFlag::NeedRender)?;
            scroll
                .content
                .borrow()
                .mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        let mut at = self.weak.upgrade();
        while let Some(node) = at {
            if node.own_active_export_target_id() == Some(target_id) {
                node.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
                if let Some(wrapper) = node.wrapper.borrow().as_ref() {
                    wrapper
                        .outer
                        .borrow()
                        .mark_dirty(NodeDirtyFlag::NeedRender)?;
                }
                break;
            }
            at = node.parent_node.borrow().upgrade().or_else(|| {
                node.retained
                    .get()
                    .then(|| node.retained_owner.borrow().upgrade())
                    .flatten()
            });
        }
        Ok(())
    }

    /// Marks custom-draw descendants in `self` that have not yet executed
    /// `on_custom_draw` unclipped for `target_id`.
    pub(crate) fn invalidate_unrecorded_export_paint(&self, target_id: u32) -> Result<usize> {
        let marked = self.invalidate_unrecorded_walk(target_id, true, None, None, (0.0, 0.0))?;
        if marked > 0 {
            self.flush_export_host_for_target(target_id)?;
        }
        Ok(marked)
    }

    /// Marks custom-draw descendants inside this scroller intersecting `mark_vp`
    /// that have not yet recorded unclipped into `target_id`, while evicting
    /// `drawn_export_target` for nodes outside `strict_vp`.
    pub(crate) fn invalidate_unrecorded_scroller_export_paint(
        &self,
        target_id: u32,
        strict_vp: Option<[f32; 4]>,
        mark_vp: Option<[f32; 4]>,
    ) -> Result<usize> {
        let marked =
            self.invalidate_unrecorded_walk(target_id, true, strict_vp, mark_vp, (0.0, 0.0))?;
        if marked > 0 {
            self.flush_export_host_for_target(target_id)?;
        }
        Ok(marked)
    }

    /// Schedules up to `frames` next-VSync passes via `post_subtree_frame_callback`
    /// that re-check only unrecorded custom-draw nodes in the viewport once
    /// ArkUI's `Scroll` clip settles.
    pub(crate) fn schedule_unrecorded_export_check(&self, target_id: u32, frames: u8) {
        if target_id == 0 || frames == 0 {
            return;
        }
        let Some(owner) = self.weak.upgrade() else {
            return;
        };
        let (prev_target, prev_frames) = self.unrecorded_check_state.get();
        let was_armed = prev_target != 0 && prev_frames > 0;
        let next_frames = if prev_target == target_id {
            prev_frames.max(frames)
        } else {
            frames
        };
        self.unrecorded_check_state.set((target_id, next_frames));
        if !was_armed {
            Self::arm_unrecorded_export_check(owner);
        }
    }

    fn arm_unrecorded_export_check(owner: std::rc::Rc<Self>) {
        let (target_id, frames) = owner.unrecorded_check_state.get();
        if !owner.active.get() || target_id == 0 || frames == 0 {
            owner.unrecorded_check_state.set((0, 0));
            return;
        }
        let weak = owner.weak.clone();
        if !Self::post_subtree_frame_callback(&owner, move |_, _| {
            let Some(node) = weak.upgrade() else {
                return;
            };
            let (cur_target, cur_frames) = node.unrecorded_check_state.get();
            if !node.active.get()
                || cur_target == 0
                || cur_frames == 0
                || node.active_export_target_id() != Some(cur_target)
            {
                node.unrecorded_check_state.set((0, 0));
                return;
            }
            let remaining = cur_frames.saturating_sub(1);
            node.unrecorded_check_state
                .set((if remaining > 0 { cur_target } else { 0 }, remaining));
            callback(|| {
                if let Some(scroll) = node.scroll.borrow().as_ref() {
                    let (strict_vp, mark_vp) = scroll.export_content_viewports();
                    node.invalidate_unrecorded_scroller_export_paint(
                        cur_target, strict_vp, mark_vp,
                    )?;
                } else {
                    node.invalidate_unrecorded_export_paint(cur_target)?;
                }
                Ok(())
            });
            if remaining > 0 {
                Self::arm_unrecorded_export_check(node);
            }
        }) {
            owner.unrecorded_check_state.set((0, 0));
        }
    }
}
