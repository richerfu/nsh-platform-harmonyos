//! Retained CSS shadow ink: its native bounds include every painted pixel.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use nsh_core::ViewStyle;
use ohos_arkui_binding::{
    api::node_custom_event::NodeCustomEvent,
    component::attribute::ArkUICommonAttribute,
    types::{advanced::NodeDirtyFlag, attribute::ArkUINodeAttributeType as A},
};

use super::{
    geometry::Size,
    node::{Node, Result},
    shadow_geometry::{PaintBounds, native_box_shadow},
};

#[derive(PartialEq)]
struct PaintKey {
    size: Size,
    radii: [f32; 4],
    continuous: bool,
    effects: Option<Arc<nsh_core::Effects>>,
    bounds: PaintBounds,
}

pub(crate) struct ShadowLayer {
    paint: super::paint_node::PaintNode,
    bounds: Rc<Cell<PaintBounds>>,
    key: RefCell<Option<PaintKey>>,
}
impl ShadowLayer {
    pub(crate) fn new(owner: &Rc<Node>) -> Result<Rc<Self>> {
        let bounds = Rc::new(Cell::new(PaintBounds::default()));
        let draw_bounds = bounds.clone();
        let weak = Rc::downgrade(owner);
        let paint = super::paint_node::PaintNode::new(move |canvas| {
            if let Some(owner) = weak.upgrade() {
                let b = draw_bounds.get();
                canvas.save();
                canvas.translate(-b.x as f32, -b.y as f32);
                owner.draw_outer_shadows(canvas);
                canvas.restore();
            }
        })?;
        paint
            .native
            .borrow()
            .set_i32_attribute(A::ZIndex, -1_000_000)?;
        let layer = Rc::new(Self {
            paint,
            bounds,
            key: RefCell::default(),
        });
        // Decorations are appended after all logical children; insertion/removal
        // indexes remain those of the NSH tree. zIndex keeps the ink underneath.
        let index = owner.native.borrow().children().len();
        owner
            .native
            .borrow_mut()
            .insert_child(layer.paint.native.clone(), index)?;
        owner
            .native
            .borrow()
            .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
        Ok(layer)
    }
    pub(crate) fn update(&self, owner: &Node, style: &ViewStyle) -> Result<()> {
        let frame = owner.frame.get();
        let bounds = if native_box_shadow(style).is_some() {
            PaintBounds::default()
        } else {
            PaintBounds::shadows(style, frame.width, frame.height, owner.scale.get())
        };
        let next = PaintKey {
            size: Size::new(frame.width, frame.height),
            radii: style
                .corner_radius
                .map(|r| r.unwrap_or(style.border_radius).0)
                .to_array(),
            continuous: style.continuous_corners,
            effects: style.effects.clone(),
            bounds,
        };
        if self.key.borrow().as_ref() == Some(&next) {
            return Ok(());
        }
        if self.bounds.replace(bounds) != bounds {
            self.paint.bounds([0, 0, bounds.width, bounds.height])?;
            self.paint
                .native
                .borrow()
                .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
            owner
                .native
                .borrow()
                .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
        }

        self.paint.invalidate()?;
        *self.key.borrow_mut() = Some(next);
        Ok(())
    }
    pub(crate) fn measure(&self, event: &NodeCustomEvent) -> Result<()> {
        crate::render::layout::ChildMeasure::new(event).child(&self.paint.native.borrow())?;
        Ok(())
    }
    pub(crate) fn layout(&self, event: &NodeCustomEvent) -> Result<()> {
        let b = self.bounds.get();
        event.layout_child(&self.paint.native.borrow(), b.x, b.y)?;
        Ok(())
    }
}
