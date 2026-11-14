//! Retained CSS shadow ink: its native bounds include every painted pixel.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use nsh_core::ViewStyle;
use ohos_arkui_binding::{
    api::node_custom_event::NodeCustomEvent,
    component::{
        attribute::{ArkUICommonAttribute, ArkUIEvent},
        built_in_component::Custom,
    },
    types::{
        advanced::{LengthMetricUnit, NodeDirtyFlag},
        attribute::ArkUINodeAttributeType as A,
        view::HitTestMode,
    },
};
use ohos_drawing_binding::Canvas;

use super::{
    geometry::Size,
    node::{NativeNode, Node, Result, callback},
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
    native: NativeNode,
    bounds: Cell<PaintBounds>,
    drawn_export_target: Cell<u32>,
    key: RefCell<Option<PaintKey>>,
}
impl ShadowLayer {
    pub(crate) fn new(owner: &Rc<Node>) -> Result<Rc<Self>> {
        let native = Custom::new()?.into_node();
        let layer = Rc::new(Self {
            native,
            bounds: Cell::default(),
            drawn_export_target: Cell::new(0),
            key: RefCell::default(),
        });
        let setup = || -> Result<()> {
            let native = layer.native.borrow();
            native.set_length_metric_unit(LengthMetricUnit::Px)?;
            native.set_attribute(A::HitTestBehavior, i32::from(HitTestMode::None).into())?;
            native.set_bool_attribute(A::Clip, true)?;
            native.set_i32_attribute(A::ZIndex, -1_000_000)?;
            Ok(())
        };
        if let Err(error) = setup() {
            callback(|| {
                layer.native.borrow_mut().dispose()?;
                Ok(())
            });
            return Err(error);
        }
        let weak = Rc::downgrade(&layer);
        layer.native.borrow_mut().on_custom_measure(move |event| {
            callback(|| {
                if let Some(layer) = weak.upgrade() {
                    let b = layer.bounds.get();
                    event.set_measured_size(b.width, b.height)?;
                }
                Ok(())
            });
        });
        let weak = Rc::downgrade(&layer);
        let node = Rc::downgrade(owner);
        layer.native.borrow_mut().on_custom_draw(move |event| {
            callback(|| {
                if let (Some(layer), Some(owner)) = (weak.upgrade(), node.upgrade()) {
                    let b = layer.bounds.get();
                    if b.width <= 0 || b.height <= 0 {
                        return Ok(());
                    }
                    if let Some(raw) = event.draw_context_in_draw().and_then(|cx| cx.canvas()) {
                        // SAFETY: ArkUI retains the canvas through this draw callback.
                        let canvas = unsafe { Canvas::from_raw_borrowed(raw) };
                        canvas.save();
                        canvas.translate(-b.x as f32, -b.y as f32);
                        owner.draw_outer_shadows(&canvas);
                        canvas.restore();
                        layer
                            .drawn_export_target
                            .set(owner.recordable_export_target_id());
                    }
                }
                Ok(())
            });
        });
        // Decorations are appended after all logical children; insertion/removal
        // indexes remain those of the NSH tree. zIndex keeps the ink underneath.
        let index = owner.native.borrow().children().len();
        if let Err(error) = owner
            .native
            .borrow_mut()
            .insert_child(layer.native.clone(), index)
        {
            callback(|| {
                layer.native.borrow_mut().dispose()?;
                Ok(())
            });
            return Err(error.into());
        }
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
            self.native
                .borrow()
                .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
            owner
                .native
                .borrow()
                .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
        }
        self.drawn_export_target.set(0);
        self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        *self.key.borrow_mut() = Some(next);
        Ok(())
    }
    pub(crate) fn measure(&self, event: &NodeCustomEvent) -> Result<()> {
        crate::render::layout::ChildMeasure::new(event).child(&self.native.borrow())?;
        Ok(())
    }
    pub(crate) fn layout(&self, event: &NodeCustomEvent) -> Result<()> {
        let b = self.bounds.get();
        event.layout_child(&self.native.borrow(), b.x, b.y)?;
        Ok(())
    }
    pub(crate) fn clear_drawn_export_target(&self) {
        self.drawn_export_target.set(0);
    }
    pub(crate) fn invalidate(&self) -> Result<()> {
        self.drawn_export_target.set(0);
        self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        Ok(())
    }
    pub(crate) fn invalidate_if_unrecorded(&self, target_id: u32) -> Result<bool> {
        let b = self.bounds.get();
        if target_id == 0
            || b.width <= 0
            || b.height <= 0
            || self.drawn_export_target.get() == target_id
        {
            return Ok(false);
        }
        self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        Ok(true)
    }
    pub(crate) fn outset_points(&self, scale: f32) -> f32 {
        let b = self.bounds.get();
        if b.width <= 0 || b.height <= 0 {
            return 0.0;
        }
        let s = scale.max(0.01);
        ((-b.x).max(0).max((-b.y).max(0)) as f32) / s
    }
}
