//! Native measurement and placement of engine-resolved bounds.

use std::rc::Rc;

use nsh_core::ViewKind;
use ohos_arkui_binding::{
    api::node_custom_event::NodeCustomEvent,
    component::attribute::{ArkUICommonAttribute, ArkUIEvent},
    types::{advanced::NodeDirtyFlag, attribute::ArkUINodeAttributeType as Attribute},
};

use super::{Node, Result, callback};
use crate::render::geometry::{Rect, Size};

impl Node {
    /// LayoutRect gives leaf nodes their native size and placement. Custom
    /// layout is needed only for a node's children or platform-owned content.
    pub(crate) fn enable_layout(&self) {
        if self.layout_callbacks.replace(true) {
            return;
        }
        let weak = self.weak.clone();
        self.native.borrow_mut().on_custom_measure(move |event| {
            callback(|| {
                if let Some(node) = weak.upgrade() {
                    node.measure(event)?;
                }
                Ok(())
            })
        });
        let weak = self.weak.clone();
        self.native.borrow_mut().on_custom_layout(move |event| {
            callback(|| {
                if let Some(node) = weak.upgrade() {
                    node.layout(event)?;
                    node.invalidate_scene();
                }
                Ok(())
            })
        });
    }
    pub(crate) fn top_overlap(&self) -> f32 {
        let mut y = self.frame.get().y;
        let mut parent = self.parent_node.borrow().upgrade();
        while let Some(node) = parent {
            if node.kind == ViewKind::Modal {
                return node
                    .modal
                    .borrow()
                    .as_ref()
                    .map_or(0.0, |modal| (modal.safe_top() - y).max(0.0));
            }
            y += node.frame.get().y;
            parent = node.parent_node.borrow().upgrade();
        }
        (self.platform.get().safe_area.top.0 - y).max(0.0)
    }
    pub(super) fn adjusted_frame(&self, mut frame: Rect) -> Rect {
        if let Some(modal) = self.modal.borrow().as_ref()
            && let Some(size) = modal.sync_metrics()
        {
            frame.width = size.width;
            frame.height = size.height;
        }
        if self.kind != ViewKind::HeaderItem
            && let Some(parent) = self.parent_node.borrow().upgrade()
            && let Some(screen) = parent.screen.borrow().as_ref()
        {
            let offset = screen.large_title_offset();
            if offset > 0.0 && frame.y < offset {
                let shift = offset - frame.y;
                frame.y += shift;
                frame.height = (frame.height - shift).max(0.0);
            }
        }
        frame
    }
    pub(crate) fn set_frame(&self, raw: Rect) -> Result<()> {
        self.raw_frame.set(raw);
        let frame = self.adjusted_frame(raw);
        let old = self.frame.replace(frame);
        if old == frame {
            return Ok(());
        }
        self.invalidate_composites();
        self.invalidate_scene();
        self.update_layout_bounds()?;
        self.invalidate_ink(self.applied.get().ink_outset)?;
        let size_changed = old.width != frame.width || old.height != frame.height;
        if size_changed {
            if self.shaders.contains(self.id) {
                self.animator.request_frame();
            }
            #[cfg(feature = "api-23")]
            self.accessibility_provider.bounds(self)?;
            if let Some(canvas) = self.canvas.borrow().as_ref() {
                canvas.invalidate()?;
            }
            self.update_wrapper()?;
            self.interaction.hit_region(self)?;
            if let Some(component) = self.component.borrow().as_ref() {
                component.native.borrow().width(frame.width)?;
                component.native.borrow().height(frame.height)?;
            }
            if let Some(image) = self.image.borrow().as_ref() {
                image.frame(Size::new(frame.width, frame.height))?;
            }
            if let Some(web) = self.web.borrow().as_ref() {
                web.frame(Size::new(frame.width, frame.height))?;
            }
            if let Some(video) = self.video.borrow().as_ref() {
                video.frame(Size::new(frame.width, frame.height))?;
            }
            let style = self.style.borrow();
            self.apply_borders(&style)?;
            self.apply_clip()?;
            if let Some(layer) = self.shadow_layer.borrow().as_ref() {
                layer.update(self, &style)?;
            }
            if !style.transform.is_identity()
                || self.extra.get().1 != crate::render::transition::Extra::NONE
            {
                self.transform(&style, |_| None)?;
            }
            drop(style);
            self.animator.apply_node(self)?;
            if let Some(scroll) = self.scroll.borrow().as_ref() {
                scroll.viewport(Size::new(frame.width, frame.height))?;
            }
            if let Some(screen) = self.screen.borrow().as_ref()
                && screen.insets_initialized()
            {
                screen.update(screen.has_back())?;
            }
            if self.text_drawing.borrow().is_some()
                || self.canvas.borrow().is_some()
                || self.style.borrow().effects.is_some()
                || self.outlined.get()
            {
                self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
            }
            if self.wrapper.borrow().is_some() {
                // The outer node owns LayoutRect. Its native child still needs
                // to resize when the content area changes.
                self.native
                    .borrow()
                    .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
            }
        } else if self.inside_texture_export() {
            self.schedule_subtree_paint(2)?;
        }
        // Size changes request measure/layout through LayoutRect. Position is a
        // native render property, so moving a node does not measure its subtree.
        Ok(())
    }
    pub(crate) fn invalidate_mounted_geometry(&self) {
        self.applied_layout_size.set((0, 0, 0));
        self.applied_position.set((0, 0, 0));
    }
    pub(crate) fn update_layout_bounds(&self) -> Result<()> {
        let frame = self.frame.get();
        // Header slots have core-measured sizes, but the native bar owns their
        // positions. Rewrapping for scene export must preserve that placement.
        let header_position = (self.kind == ViewKind::HeaderItem)
            .then(|| self.parent_node.borrow().upgrade())
            .flatten()
            .and_then(|parent| parent.screen.borrow().as_ref()?.item_position(self));
        let (x, y) = header_position.unwrap_or((frame.x, frame.y));
        self.place_bounds(
            (x * self.scale.get()).round() as i32,
            (y * self.scale.get()).round() as i32,
        )
    }
    pub(super) fn place_bounds(&self, x: i32, y: i32) -> Result<()> {
        let frame = self.frame.get();
        let mut width = (frame.width * self.scale.get()).round() as i32;
        let mut height = (frame.height * self.scale.get()).round() as i32;
        if !self.clips_children() && !self.children.borrow().is_empty() {
            width = width.max(1);
            height = height.max(1);
        }
        // The engine owns this geometry. Keep the measure boundary at a local
        // origin: changing LayoutRect's origin also requests measurement, and
        // ArkUI's SetLayoutRect marks NeedMeasure even when given the same box.
        // Native Position places it relative to the parent without reshaping
        // its children, like moving UIView/Android bounds at an unchanged size.
        let mounted = self.mounted();
        let ptr = Rc::as_ptr(&mounted) as usize;
        let native = mounted.borrow();
        if self.kind == ViewKind::Modal {
            // Dialog lays out its content in a native Column. An absolute
            // Position removes that content from the Column's measured size,
            // leaving hit testing centered around a zero-sized parent.
            native.reset_attribute(Attribute::Position)?;
            native.width(frame.width)?;
            native.height(frame.height)?;
            return Ok(());
        }
        if self.applied_layout_size.get() != (ptr, width, height) {
            native.set_layout_rect([0, 0, width, height])?;
            self.applied_layout_size.set((ptr, width, height));
        }
        if self.applied_position.get() != (ptr, x, y) {
            native.set_position([x as f32 / self.scale.get(), y as f32 / self.scale.get()])?;
            self.applied_position.set((ptr, x, y));
        }
        Ok(())
    }
    pub(crate) fn place(&self, event: &NodeCustomEvent, x: i32, y: i32) -> Result<()> {
        let frame = self.frame.get();
        if x != (frame.x * self.scale.get()).round() as i32
            || y != (frame.y * self.scale.get()).round() as i32
        {
            self.place_bounds(x, y)?;
        }
        event.layout_child(&self.mounted().borrow(), x, y)?;
        Ok(())
    }
    pub(super) fn measure(&self, event: &NodeCustomEvent) -> Result<()> {
        let mut frame = self.frame.get();
        if self.wrapper.borrow().is_none()
            && let Some(modal) = self.modal.borrow().as_ref()
        {
            let size = modal.measure(event)?;
            frame.width = size.width;
            frame.height = size.height;
            if self.frame.get() != frame
                && let Some(canvas) = self.canvas.borrow().as_ref()
            {
                canvas.invalidate()?;
            }
            if self.frame.replace(frame) != frame {
                self.invalidate_composites();
            }
            self.raw_frame.set(frame);
        }
        // Rosen's `RSObjAbsGeometry::UpdateMatrix` returns early when both
        // width and height are `<= 0`, discarding the node's `(x, y)` layout
        // offset and transforms for all descendants, and `A::ResponseRegion`
        // percentages multiply by the measured size. Keep unclipped containers
        // at least 1px in each axis so their geometry and hit regions propagate.
        let mut width = (frame.width * self.scale.get()).round() as i32;
        let mut height = (frame.height * self.scale.get()).round() as i32;
        if !self.clips_children() && !self.children.borrow().is_empty() {
            width = width.max(1);
            height = height.max(1);
        }
        event.set_measured_size(width, height)?;
        if let Some(layer) = self.shadow_layer.borrow().as_ref() {
            layer.measure(event)?;
        }
        if let Some(input) = self.input.borrow().as_ref() {
            input.measure(event)?;
        } else if let Some(image) = self.image.borrow().as_ref() {
            crate::render::layout::ChildMeasure::new(event).child(&image.native.borrow())?;
        } else if let Some(scroll) = self.scroll.borrow().as_ref() {
            crate::render::layout::ChildMeasure::new(event).child(&scroll.mounted().borrow())?;
        } else {
            self.measure_children(event)?;
        }
        Ok(())
    }
    pub(crate) fn measure_children(&self, event: &NodeCustomEvent) -> Result<()> {
        let mut measure = crate::render::layout::ChildMeasure::new(event);
        if let Some(web) = self
            .web
            .borrow()
            .as_ref()
            .filter(|web| web.native().borrow().is_owned())
        {
            measure.child(&web.native().borrow())?;
        }
        if let Some(video) = self
            .video
            .borrow()
            .as_ref()
            .filter(|video| !video.is_fullscreen() && video.native().borrow().is_owned())
        {
            measure.child(&video.native().borrow())?;
        }
        if let Some(screen) = self.screen.borrow().as_ref() {
            screen.measure(event)?;
        }
        if let Some(component) = self.component.borrow().as_ref() {
            measure.child(&component.native.borrow())?;
        }
        if let Some(surface) = self.shaders.native(self.id) {
            measure.child(&surface.borrow())?;
        }
        if self.id == nsh_core::ROOT_ID
            && let Some(owner) = self.fullscreen_video.borrow().upgrade()
            && let Some(video) = owner.video.borrow().as_ref()
        {
            let frame = self.frame.get();
            video.fullscreen_frame(Size::new(frame.width, frame.height))?;
            measure.child(&video.native().borrow())?;
        }
        // Engine children have independent LayoutRect boundaries and native
        // Position. Their own dirty layout tasks apply engine-resolved geometry;
        // walking them here repeats native measurement of unaffected subtrees.
        Ok(())
    }
    pub(super) fn layout(&self, event: &NodeCustomEvent) -> Result<()> {
        if let Some(layer) = self.shadow_layer.borrow().as_ref() {
            layer.layout(event)?;
        }
        let position = event.position_in_layout();
        event.set_layout_position(position.x, position.y)?;
        #[cfg(feature = "api-23")]
        self.accessibility_provider.bounds(self)?;
        if let Some(input) = self.input.borrow().as_ref() {
            input.layout(event, self.scale.get())?;
        } else if let Some(image) = self.image.borrow().as_ref() {
            event.layout_child(&image.native.borrow(), 0, 0)?;
        } else if let Some(scroll) = self.scroll.borrow().as_ref() {
            event.layout_child(&scroll.mounted().borrow(), 0, 0)?;
        } else {
            self.layout_children(event)?;
        }
        Ok(())
    }
    pub(crate) fn layout_children(&self, event: &NodeCustomEvent) -> Result<()> {
        if let Some(web) = self
            .web
            .borrow()
            .as_ref()
            .filter(|web| web.native().borrow().is_owned())
        {
            event.layout_child(&web.native().borrow(), 0, 0)?;
        }
        if let Some(video) = self
            .video
            .borrow()
            .as_ref()
            .filter(|video| !video.is_fullscreen() && video.native().borrow().is_owned())
        {
            event.layout_child(&video.native().borrow(), 0, 0)?;
        }
        if let Some(component) = self.component.borrow().as_ref() {
            event.layout_child(&component.native.borrow(), 0, 0)?;
        }
        if let Some(surface) = self.shaders.native(self.id) {
            let overflow = (self.shaders.overflow(self.id) * self.scale.get()).round() as i32;
            event.layout_child(&surface.borrow(), -overflow, -overflow)?;
        }
        // Engine children place themselves through native Position; only
        // platform-owned children participate in this parent's layout pass.
        if let Some(screen) = self.screen.borrow().as_ref() {
            screen.layout(event)?;
        }
        if self.id == nsh_core::ROOT_ID
            && let Some(owner) = self.fullscreen_video.borrow().upgrade()
            && let Some(video) = owner.video.borrow().as_ref()
        {
            event.layout_child(&video.native().borrow(), 0, 0)?;
        }
        Ok(())
    }
}
