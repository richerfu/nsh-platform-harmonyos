//! Shader frames presented by an XComponent Surface, outside the exported subtree.

use ohos_arkui_binding::{
    api::node_custom_event::NodeCustomEvent,
    common::attribute::ArkUINodeAttributeNumber as Number,
    component::attribute::ArkUICommonAttribute,
    types::{advanced::NodeDirtyFlag, attribute::ArkUINodeAttributeType as Attribute},
};

use crate::render::node::{NativeNode, Node, Result, callback};

pub(super) struct Presentation {
    parent: NativeNode,
    surface: super::surface::Surface,
    scale: f32,
    origin: (i32, i32),
    size: (u32, u32),
    revealed: bool,
    bridge: Option<(f32, f32)>,
    applied_dx_px: f32,
}

impl Presentation {
    pub(super) fn new(
        owner: &Node,
        key: super::worker::Key,
        events: super::worker::WindowEvents,
    ) -> Result<Self> {
        let surface = super::surface::Surface::new(
            format!("nsh-effect-{}-{}", key.id, key.generation),
            events,
            false,
        )?;
        surface.native.borrow().z_index(1)?;
        surface.native.borrow().opacity(0.0)?;
        let native = surface.native.clone();
        let parent = owner.mounted();
        parent.borrow_mut().add_child(native.clone())?;
        let mut layer = Self {
            parent,
            surface,
            scale: owner.scale.get(),
            origin: (0, 0),
            size: (0, 0),
            revealed: false,
            bridge: None,
            applied_dx_px: 0.0,
        };
        let frame = owner.frame.get();
        let scale = owner.scale.get();
        layer.update(
            (frame.width * scale).ceil().max(1.0) as u32,
            (frame.height * scale).ceil().max(1.0) as u32,
            0.0,
            0.0,
            scale,
        )?;
        Ok(layer)
    }

    pub(super) fn is_revealed(&self) -> bool {
        self.revealed
    }

    fn apply_bridge_transform(
        node: &ohos_arkui_binding::common::node::ArkUINode,
        bridge: Option<(f32, f32)>,
        scale: f32,
        applied_dx_px: &mut f32,
    ) -> Result<bool> {
        let dx_px = bridge.map_or(0.0, |(_, dx_vp)| dx_vp * scale.max(0.01));
        if (*applied_dx_px - dx_px).abs() <= 0.01 {
            return Ok(false);
        }
        if dx_px.abs() <= 0.01 {
            node.reset_attribute(Attribute::Transform)?;
            *applied_dx_px = 0.0;
        } else {
            let raw_values = [
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, dx_px, 0.0, 0.0, 1.0,
            ];
            node.set_number_attribute(
                Attribute::Transform,
                raw_values.into_iter().map(Number::Float).collect(),
            )?;
            *applied_dx_px = dx_px;
        }
        Ok(true)
    }

    pub(super) fn set_bridge(&mut self, alpha: f32, dx_vp: f32) -> Result<()> {
        let clamped = alpha.clamp(0.0, 1.0);
        let prev_alpha = self.bridge.map(|(a, _)| a);
        self.bridge = Some((clamped, dx_vp));
        let node = self.surface.native.borrow();
        let moved =
            Self::apply_bridge_transform(&node, self.bridge, self.scale, &mut self.applied_dx_px)?;
        let alpha_changed = self.revealed && prev_alpha != Some(clamped);
        if alpha_changed {
            node.opacity(clamped)?;
        }
        if moved || alpha_changed {
            node.mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        Ok(())
    }

    pub(super) fn clear_bridge(&mut self) -> Result<()> {
        let had_bridge = self.bridge.take();
        if had_bridge.is_none() && self.applied_dx_px == 0.0 {
            return Ok(());
        }
        let node = self.surface.native.borrow();
        let moved =
            Self::apply_bridge_transform(&node, self.bridge, self.scale, &mut self.applied_dx_px)?;
        if self.revealed {
            node.opacity(1.0)?;
        }
        if moved || self.revealed {
            node.mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        Ok(())
    }

    pub(super) fn reveal(&mut self) -> Result<bool> {
        if !self.revealed {
            let node = self.surface.native.borrow();
            let _ = Self::apply_bridge_transform(
                &node,
                self.bridge,
                self.scale,
                &mut self.applied_dx_px,
            )?;
            let alpha = self.bridge.map_or(1.0, |(a, _)| a);
            node.opacity(alpha)?;
            node.mark_dirty(NodeDirtyFlag::NeedRender)?;
            self.revealed = true;
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) fn hide(&mut self) {
        let node = self.surface.native.borrow();
        let _ = node.opacity(f32::MIN_POSITIVE);
        let _ = node.opacity(0.0);
        let _ = node.mark_dirty(NodeDirtyFlag::NeedRender);
        self.revealed = false;
    }

    pub(super) fn update(
        &mut self,
        width: u32,
        height: u32,
        x: f32,
        y: f32,
        scale: f32,
    ) -> Result<()> {
        let scale = scale.max(0.01);
        let scale_changed = (self.scale - scale).abs() > f32::EPSILON;
        if scale_changed {
            self.scale = scale;
        }
        let node = self.surface.native.borrow();
        if scale_changed && self.bridge.is_some() {
            let _ = Self::apply_bridge_transform(
                &node,
                self.bridge,
                self.scale,
                &mut self.applied_dx_px,
            )?;
        }
        let origin = (x.round() as i32, y.round() as i32);
        let origin_changed = self.origin != origin;
        if origin_changed || scale_changed {
            node.set_position([x / self.scale, y / self.scale])?;
            self.origin = origin;
        }
        if self.size != (width, height) || origin_changed || scale_changed {
            if self.size != (width, height) || scale_changed {
                node.width(width as f32 / self.scale)?;
                node.height(height as f32 / self.scale)?;
                self.size = (width, height);
            }
            node.set_layout_rect([self.origin.0, self.origin.1, width as i32, height as i32])?;
        }
        Ok(())
    }

    pub(super) fn measure(&self, event: &NodeCustomEvent) -> Result<()> {
        crate::render::layout::ChildMeasure::new(event).child(&self.surface.native.borrow())
    }

    pub(super) fn layout(&self, event: &NodeCustomEvent) -> Result<()> {
        event.layout_child(&self.surface.native.borrow(), self.origin.0, self.origin.1)?;
        Ok(())
    }
}

impl Drop for Presentation {
    fn drop(&mut self) {
        callback(|| self.surface.dispose(Some(&self.parent)));
    }
}
