//! Text and custom painting, with CSS scene geometry.

use std::rc::{Rc, Weak};

use nsh_core::ViewKind;
use ohos_arkui_binding::{
    api::node_custom_event::NodeCustomEvent, component::attribute::ArkUIEvent,
    types::advanced::NodeDirtyFlag,
};
use ohos_drawing_binding::Canvas;

use super::{Error, Node, Result, callback};
use crate::{
    render::{
        geometry::{Rect, Size},
        subtree_paint::{
            SubtreePaint, SubtreePaintNode, commit_schedule, plan_schedule, step_frame,
        },
    },
    text::typography::{NativeText, argb},
};

impl SubtreePaintNode for Rc<Node> {
    fn is_live(&self) -> bool {
        // Nodes marked `pending_delete` are frozen for an active exit animation
        // (leaving Screen or closing Modal) and still render until `delete_now`
        // runs `deactivate()` (`active = false`).
        self.active.get()
    }

    fn paint_state(&self) -> SubtreePaint {
        self.paint_state.get()
    }

    fn set_paint_state(&self, state: SubtreePaint) {
        self.paint_state.set(state);
    }

    fn parent_repaint_owner(&self) -> Option<Self> {
        self.parent_node.borrow().upgrade().or_else(|| {
            self.retained
                .get()
                .then(|| self.retained_owner.borrow().upgrade())
                .flatten()
        })
    }

    fn for_each_repaint_child(&self, f: &mut dyn FnMut(Self)) {
        for child in self.children.borrow().iter().filter_map(Weak::upgrade) {
            f(child);
        }
        for child in self
            .retained_children
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
        {
            if child.retained.get() {
                f(child);
            }
        }
    }
}

impl Node {
    /// Plain native containers need no recording canvas. Install custom paint
    /// only when content needs it, as text/canvas views do on UIKit. Keep the
    /// callback thereafter so removing that content clears the old recording.
    pub(crate) fn enable_drawing(&self) {
        if self.drawing.replace(true) {
            return;
        }
        let weak = self.weak.clone();
        self.native.borrow_mut().on_custom_draw(move |event| {
            callback(|| {
                if let Some(node) = weak.upgrade() {
                    node.draw(event);
                }
                Ok(())
            });
        });
    }
    pub(crate) fn scene_active(&self) -> bool {
        self.scene_prepared.get()
    }
    pub(crate) fn invalidate_composites(&self) {
        self.shaders.invalidate_visibility();
        self.composite_dirty.set(true);
        self.capture_cache.set(None);
        let mut at = self.parent_node.borrow().upgrade();
        while let Some(node) = at {
            node.composite_dirty.set(true);
            node.capture_cache.set(None);
            at = node.parent_node.borrow().upgrade();
        }
    }
    pub(crate) fn invalidate_scene(&self) {
        self.shaders.invalidate_visibility();
        if let Some(scene) = self.scene.borrow().as_ref() {
            scene.invalidate();
        }
    }
    pub(crate) fn scene_opacity(&self) -> f32 {
        self.visual_opacity_for(self.style.borrow().opacity)
    }
    pub(crate) fn has_native_paint(&self, style: &nsh_core::ViewStyle) -> bool {
        self.id == nsh_core::ROOT_ID
            || self.scene_root.get()
            || self.kind != ViewKind::View
            || self.shaders.contains(self.id)
            || !style.background_color.is_transparent()
            || style.borders.widths.any(|v| v.0 > 0.0)
            || style.outline.is_visible()
            || style.effects.as_ref().is_some_and(|effects| {
                !effects.background_images.is_empty() || !effects.box_shadows.is_empty()
            })
    }
    pub(crate) fn capture_bounds(
        &self,
        depth: usize,
    ) -> Result<crate::render::composition::geometry::Bounds> {
        use crate::render::composition::geometry::Bounds;
        if depth > 256 {
            return Err(Error::Unsupported(
                "compositor hierarchy exceeds 256 levels",
            ));
        }
        if let Some(bounds) = self.capture_cache.get() {
            return Ok(bounds);
        }
        let frame = self.frame.get();
        let mut bounds = Bounds::new(frame.width, frame.height)
            .shaders(self.shaders.overflow(self.id), &self.shader_layers.borrow())
            .own(&self.style.borrow());
        if !self.clips_children() {
            for child in self.children.borrow().iter().filter_map(Weak::upgrade) {
                if child.kind == ViewKind::Modal || child.style.borrow().hidden {
                    continue;
                }
                let child_frame = child.frame.get();
                let filters = child
                    .style
                    .borrow()
                    .effects
                    .as_ref()
                    .map(|effects| effects.filters.clone())
                    .unwrap_or_default();
                let matrix = child
                    .input_transform
                    .get()
                    .mul(&nsh_core::Matrix::of(
                        nsh_core::TransformKind::TranslateX,
                        child_frame.x as f64,
                    ))
                    .mul(&nsh_core::Matrix::of(
                        nsh_core::TransformKind::TranslateY,
                        child_frame.y as f64,
                    ));
                let child = child
                    .capture_bounds(depth + 1)?
                    .filters(&filters)
                    .transformed(matrix)
                    .ok_or(Error::Unsupported("unbounded projected compositor capture"))?;
                bounds.union(child);
            }
        }
        self.capture_cache.set(Some(bounds));
        Ok(bounds)
    }
    pub(crate) fn paint_visible(&self) -> bool {
        self.active.get()
            && !self.retained.get()
            && !self.backface_away.get()
            && !self.style.borrow().hidden
            && self.header_item_visible()
            && self
                .screen
                .borrow()
                .as_ref()
                .is_none_or(|screen| screen.paint_visible())
            && self
                .parent_node
                .borrow()
                .upgrade()
                .is_none_or(|parent| parent.paint_visible())
    }
    pub(crate) fn text(
        self: &Rc<Self>,
        raster: &NativeText,
        content: Rect,
        layout: nsh_core::TextLayout,
    ) -> Result<bool> {
        let same_layout = self
            .raster
            .borrow()
            .as_ref()
            .is_some_and(|old| std::sync::Arc::ptr_eq(&old.0, &layout.0));
        let same_placement =
            self.content.get() == content && self.raster_origin_y.get() == raster.origin_y;
        if !same_layout {
            let change = self
                .raster
                .borrow()
                .as_ref()
                .and_then(|old| old.0.downcast_ref::<NativeText>())
                .map(|old| {
                    nsh_core::transition::recolor(
                        &old.source,
                        &raster.source,
                        &self.style.borrow(),
                        self.platform.get().reduce_motion,
                    )
                });
            if let Some(change) = change {
                if change.starts() {
                    self.live
                        .borrow_mut()
                        .get_or_insert_with(Default::default)
                        .recolor(change);
                    self.animator.track(self);
                } else if let Some(live) = self.live.borrow_mut().as_mut() {
                    live.recolor(change);
                }
            }
            self.selection.range.take();
            *self.text_drawing.borrow_mut() = raster.drawing.lock().expect("text drawing").take();
        }
        if self.text_drawing.borrow().is_some() {
            self.enable_drawing();
        }
        let has_links = !raster.links.is_empty();
        let links_changed = self.has_text_links.replace(has_links) != has_links || has_links;
        *self.raster.borrow_mut() = Some(layout);
        self.content.set(content);
        self.raster_origin_y.set(raster.origin_y);
        if self.selectable() || self.wrapper.borrow().is_some() {
            self.ensure_wrapper()?;
        }
        if !same_layout || !same_placement {
            self.clear_drawn_export_target();
            self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        self.set_events(self.interaction.mask())?;
        Ok(links_changed)
    }
    /// Re-record custom paint after a native visibility or render-target change.
    pub(crate) fn invalidate_subtree_paint(&self) -> Result<()> {
        if !self.active.get()
            || self.retained.get()
            || self.pending_delete.get()
            || self.is_settled_popped_screen()
        {
            return Ok(());
        }
        self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        if let Some(image) = self.image.borrow().as_ref() {
            image
                .native
                .borrow()
                .mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            wrapper
                .outer
                .borrow()
                .mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
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
        if let Some(screen) = self.screen.borrow().as_ref() {
            screen.invalidate_paint()?;
        }
        if let Some(layer) = self.shadow_layer.borrow().as_ref() {
            layer.invalidate()?;
        }
        self.clear_drawn_export_target();
        for child in self.children.borrow().iter().filter_map(Weak::upgrade) {
            child.invalidate_subtree_paint()?;
        }
        Ok(())
    }
    pub(crate) fn schedule_subtree_paint(&self, frames: u8) -> Result<()> {
        let Some(owner) = self.weak.upgrade() else {
            return Ok(());
        };
        let Some(plan) = plan_schedule(&owner, frames) else {
            return Ok(());
        };
        if !plan.was_armed {
            self.invalidate_subtree_paint()?;
        }
        if commit_schedule(&owner, plan) {
            Self::arm_subtree_paint(self.weak.clone());
        }
        Ok(())
    }
    pub(super) fn post_subtree_frame_callback(
        owner: &Rc<Self>,
        callback_fn: impl Fn(u64, u32) + 'static,
    ) -> bool {
        let mut candidates: Vec<super::NativeNode> = Vec::new();
        let mut at = Some(owner.clone());
        while let Some(node) = at {
            let mounted = node.mounted();
            if !candidates
                .iter()
                .any(|existing| Rc::ptr_eq(existing, &mounted))
            {
                candidates.push(mounted);
            }
            if !candidates
                .iter()
                .any(|existing| Rc::ptr_eq(existing, &node.native))
            {
                candidates.push(node.native.clone());
            }
            at = node.parent_node.borrow().upgrade().or_else(|| {
                node.retained
                    .get()
                    .then(|| node.retained_owner.borrow().upgrade())
                    .flatten()
            });
        }
        if let Some(viewport) = owner.viewport.upgrade()
            && !candidates
                .iter()
                .any(|existing| Rc::ptr_eq(existing, &viewport))
        {
            candidates.push(viewport);
        }
        let callback_fn = Rc::new(callback_fn);
        for candidate in candidates {
            let cb = callback_fn.clone();
            if super::FrameCallbackExt::post_frame_callback_dirty(
                &*candidate.borrow(),
                move |timestamp, frame_count| cb(timestamp, frame_count),
            )
            .is_ok()
            {
                owner.pulse_frame_wake();
                return true;
            }
        }
        false
    }
    fn arm_subtree_paint(weak: Weak<Self>) {
        let Some(owner) = weak.upgrade() else {
            return;
        };
        if !owner.is_live() || owner.paint_state.get().frames == 0 {
            owner.paint_state.set(owner.paint_state.get().reset());
            return;
        }
        let next = weak.clone();
        if !Self::post_subtree_frame_callback(&owner, move |timestamp, frame_count| {
            let Some(owner) = next.upgrade() else {
                return;
            };
            let tick = step_frame(&owner, (timestamp, u64::from(frame_count)));
            if tick.invalidate {
                callback(|| owner.invalidate_subtree_paint());
            }
            if tick.rearm {
                Self::arm_subtree_paint(next.clone());
            }
        }) {
            owner.paint_state.set(owner.paint_state.get().reset());
        }
    }

    pub(crate) fn draw_outer_shadows(&self, canvas: &Canvas) {
        let frame = self.frame.get();
        self.decoration.shadows(
            canvas,
            &self.style.borrow(),
            Size::new(frame.width, frame.height),
            self.scale.get(),
            false,
        );
    }
    pub(super) fn draw(&self, event: &NodeCustomEvent) {
        let frame = self.frame.get();
        let has_area = frame.width > 0.0 && frame.height > 0.0;
        let has_effects = has_area
            && self
                .style
                .borrow()
                .effects
                .as_ref()
                .is_some_and(|e| !e.background_images.is_empty() || !e.box_shadows.is_empty());
        if !has_effects {
            self.decoration.clear_paint_cache();
            if !has_area
                || (self.canvas.borrow().is_none()
                    && self.text_drawing.borrow().is_none()
                    && self.selection.range.get().is_none())
            {
                return;
            }
        }
        if let Some(raw) = event.draw_context_in_draw().and_then(|ctx| ctx.canvas()) {
            // SAFETY: ArkUI owns this canvas for the duration of this draw callback.
            let canvas = unsafe { Canvas::from_raw_borrowed(raw) };
            if has_effects {
                self.decoration.background(
                    &canvas,
                    &self.style.borrow(),
                    Size::new(frame.width, frame.height),
                    self.scale.get(),
                );
            }
            if let Some(drawing) = self.canvas.borrow().as_ref() {
                callback(|| drawing.draw(&canvas));
            }
            self.draw_selection(&canvas, false);
            if let Some(drawing) = self.text_drawing.borrow_mut().as_mut() {
                let content = self.content.get();
                let brush = self
                    .tint
                    .get()
                    .or_else(|| {
                        self.live
                            .borrow()
                            .as_ref()
                            .and_then(|live| live.text_color())
                    })
                    .and_then(|color| {
                        ohos_drawing_binding::ColorFilter::blend(
                            argb(color),
                            ohos_drawing_binding::BlendMode::SrcIn,
                        )
                    })
                    .map(|color| {
                        let mut filter = ohos_drawing_binding::Filter::new();
                        filter.set_color_filter(Some(&color));
                        let mut brush = ohos_drawing_binding::Brush::new();
                        brush.set_filter(Some(&filter));
                        brush
                    });
                let raster_scale = self
                    .raster
                    .borrow()
                    .as_ref()
                    .and_then(|layout| layout.0.downcast_ref::<NativeText>())
                    .map_or(self.scale.get(), |raster| raster.scale);
                let tx = content.x * self.scale.get();
                let ty = (content.y + self.raster_origin_y.get()) * self.scale.get();
                let scale_mismatch = (self.scale.get() - raster_scale).abs() > f32::EPSILON;
                let needs_save = tx != 0.0 || ty != 0.0 || scale_mismatch;
                if needs_save {
                    canvas.save();
                    if tx != 0.0 || ty != 0.0 {
                        canvas.translate(tx, ty);
                    }
                    if scale_mismatch {
                        canvas.scale(
                            self.scale.get() / raster_scale,
                            self.scale.get() / raster_scale,
                        );
                    }
                }
                if let Some(brush) = &brush {
                    canvas.save_layer(None, Some(brush));
                }
                if let Err(error) = drawing.draw(&canvas) {
                    log::warn!("OHOS text draw: {error}");
                }
                if brush.is_some() {
                    canvas.restore();
                }
                if needs_save {
                    canvas.restore();
                }
            }
            self.draw_selection(&canvas, true);
            self.drawn_export_target
                .set(self.recordable_export_target_id());
        }
    }
}
