//! Layout transitions and animation graph outputs.

use std::rc::Rc;

use nsh_core::ViewKind;
use ohos_arkui_binding::{
    common::attribute::ArkUINodeAttributeNumber as Number,
    component::attribute::ArkUICommonAttribute,
    types::{advanced::NodeDirtyFlag, attribute::ArkUINodeAttributeType as Attribute},
};

use super::{Node, Result};
use crate::{render::geometry::Rect, text::typography::argb};

impl Node {
    pub(super) fn navigated(&self) -> bool {
        self.id == nsh_core::ROOT_ID
            || matches!(
                self.kind,
                ViewKind::Stack
                    | ViewKind::Screen
                    | ViewKind::Modal
                    | ViewKind::HeaderItem
                    | ViewKind::InputAccessory
            )
            || self
                .parent_node
                .borrow()
                .upgrade()
                .is_some_and(|p| p.kind == ViewKind::Stack)
    }
    pub(crate) fn animated_frame(
        self: &Rc<Self>,
        raw: Rect,
        animation: Option<&nsh_core::LayoutAnimation>,
    ) -> Result<()> {
        self.raw_frame.set(raw);
        let frame = self.adjusted_frame(raw);
        let target = self.live.borrow().as_ref().and_then(|l| l.frame_target());
        if target.unwrap_or(self.frame.get()) == frame {
            return Ok(());
        }
        let animate = animation.filter(|_| !self.navigated() && !self.platform.get().reduce_motion);
        if animate.is_some() || self.live.borrow().is_some() {
            let moving = self
                .live
                .borrow_mut()
                .get_or_insert_with(Default::default)
                .frame(self.frame.get(), frame, animate);
            if moving {
                self.animator.track(self);
                return Ok(());
            }
        }
        self.set_frame(raw)
    }
    pub(crate) fn inserted(
        self: &Rc<Self>,
        animation: Option<&nsh_core::LayoutAnimation>,
    ) -> Result<()> {
        let returned = self.retained.replace(false);
        if self.set_leaving(false) || returned {
            if let Some(live) = self.live.borrow_mut().as_mut() {
                live.cancel_removal();
            }
            self.extra
                .set((1.0, crate::render::transition::Extra::NONE));
            self.interaction.pointer(
                self,
                self.parent_node
                    .borrow()
                    .upgrade()
                    .is_some_and(|p| p.interaction.blocks_children()),
            )?;
            self.restore_style()?;
        }
        if self.frame.get() != Rect::default()
            || self.navigated()
            || self.platform.get().reduce_motion
        {
            return Ok(());
        }
        if let Some(transition) = animation.and_then(|a| a.create) {
            let initial = self
                .live
                .borrow_mut()
                .get_or_insert_with(Default::default)
                .appear(transition);
            self.extra.set(initial);
            self.restore_style()?;
            self.animator.track(self);
        }
        Ok(())
    }
    pub(crate) fn removing(
        self: &Rc<Self>,
        animation: Option<&nsh_core::LayoutAnimation>,
    ) -> Result<bool> {
        if self.leaving.get() {
            return Ok(true);
        }
        // No ancestor can be leaving while none is.
        if self.leaving_nodes.get() > 0 {
            let mut parent = self.parent_node.borrow().upgrade();
            while let Some(ancestor) = parent {
                if ancestor.leaving.get() {
                    return Ok(true);
                }
                parent = ancestor.parent_node.borrow().upgrade();
            }
        }
        if let Some(screen) = self.screen.borrow().as_ref()
            && screen.remove()?
        {
            return Ok(true);
        }
        if self.parent.get().is_none() || self.navigated() || self.platform.get().reduce_motion {
            return Ok(false);
        }
        let Some(transition) = animation.and_then(|a| a.delete) else {
            return Ok(false);
        };
        self.set_leaving(true);
        self.native.borrow().set_i32_attribute(
            Attribute::HitTestBehavior,
            ohos_arkui_binding::types::view::HitTestMode::None.into(),
        )?;
        self.live
            .borrow_mut()
            .get_or_insert_with(Default::default)
            .disappear(transition);
        self.animator.track(self);
        Ok(true)
    }
    pub(crate) fn tick_transition(&self, now: f64) -> Result<(bool, bool)> {
        let screen = self.screen.borrow().clone();
        let (navigation_active, navigation_removed) = screen
            .as_ref()
            .map(|screen| screen.frame(now))
            .transpose()?
            .unwrap_or_default();
        let image_active = self
            .image
            .borrow()
            .as_ref()
            .map(|image| image.tick(now))
            .transpose()?
            .unwrap_or(false);
        let video_active = self
            .video
            .borrow()
            .as_ref()
            .map(|video| video.tick(now))
            .transpose()?
            .unwrap_or(false);
        let Some(mut live) = self.live.borrow_mut().take() else {
            return Ok((
                image_active || navigation_active || video_active,
                navigation_removed,
            ));
        };
        let step = if self.platform.get().reduce_motion {
            live.finish(&mut self.style.borrow_mut())
        } else {
            live.step(now, self.frame.get(), &mut self.style.borrow_mut())
        };
        if live.recolor {
            self.clear_drawn_export_target();
            self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
            self.invalidate_scene();
        }
        if step.active {
            *self.live.borrow_mut() = Some(live);
        }
        if let Some(frame) = step.frame {
            self.set_frame(frame)?;
        }
        let previous = self.extra.replace((step.opacity, step.extra));
        if step.restyled {
            self.invalidate_composites();
            if let Some(owner) = self.weak.upgrade() {
                if let Some(scene) = self.scene.borrow().as_ref() {
                    scene.require(&owner);
                }
                owner.ensure_wrapper()?;
            }
            self.restore_style()?;
            self.clear_drawn_export_target();
            self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        } else {
            let style = self.style.borrow();
            if previous.0 != step.opacity {
                self.apply_opacity(style.opacity)?;
            }
            if previous.1 != step.extra {
                self.transform(&style, |_| None)?;
            }
            drop(style);
            self.animator.apply_node(self)?;
        }
        Ok((
            step.active || image_active || navigation_active || video_active,
            step.removed || navigation_removed,
        ))
    }
    pub(crate) fn finish_removal(&self) -> Result<()> {
        self.set_leaving(false);
        self.extra
            .set((1.0, crate::render::transition::Extra::NONE));
        self.restore_style()
    }
    pub(crate) fn animate(
        &self,
        values: &[(
            nsh_core::animation::AnimationProp,
            nsh_core::animation::AnimationValue,
        )],
    ) -> Result<()> {
        use nsh_core::animation::AnimationProp as P;
        let style = self.style.borrow();
        let mut opacity = None;
        let mut bg = None;
        let mut border_color = None;
        let mut has_transform = false;
        let mut component_props = Vec::new();
        for (prop, value) in values {
            match prop {
                P::CanvasArg(slot) => {
                    self.shaders.slot(self.id, *slot, *value);
                    if let Some(canvas) = self.canvas.borrow().as_ref() {
                        canvas.slot(*slot, *value)?;
                    }
                }
                P::ComponentSlot(slot) => {
                    let idx = *slot as usize;
                    if let Some(name) = self.component_slots.borrow().get(idx).cloned() {
                        let next = nsh_core::animation::AnimatedPropValue::from(*value);
                        let mut cache = self.component_animated.borrow_mut();
                        if cache.len() <= idx {
                            cache.resize(idx + 1, None);
                        }
                        if cache[idx] != Some(next) {
                            cache[idx] = Some(next);
                            component_props.push((
                                name,
                                Some(match next {
                                    nsh_core::animation::AnimatedPropValue::Number(n) => {
                                        serde_json::Number::from_f64(n).map_or(
                                            nsh_core::JsonValue::Null,
                                            nsh_core::JsonValue::Number,
                                        )
                                    }
                                    nsh_core::animation::AnimatedPropValue::Color(argb) => {
                                        nsh_core::JsonValue::from(argb)
                                    }
                                    _ => nsh_core::JsonValue::Null,
                                }),
                            ));
                        }
                    }
                }
                P::Color => {
                    let color = Some(crate::render::animation::color(*value));
                    if self.tint.replace(color) != color {
                        self.clear_drawn_export_target();
                        self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
                    }
                }
                P::Opacity => opacity = Some(value.number().clamp(0.0, 1.0)),
                P::BackgroundColor => bg = Some(crate::render::animation::color(*value)),
                P::BorderColor => border_color = Some(crate::render::animation::color(*value)),
                P::Transform(_) => has_transform = true,
                _ => {}
            }
        }
        if self.animated_opacity.replace(opacity) != opacity {
            self.apply_opacity(style.opacity)?;
        }
        if let Some(bg) = bg {
            self.apply_background_color(bg)?;
        }
        if let Some(color) = border_color {
            let colors = [argb(color); 4];
            let mut applied = self.applied.get();
            if applied.border_color != colors {
                self.native.borrow().set_number_attribute(
                    Attribute::BorderColor,
                    colors.into_iter().map(Number::Uint).collect(),
                )?;
                applied.border_color = colors;
                self.applied.set(applied);
            }
        }
        if bg.is_some() {
            let mut presented = style.clone();
            if let Some(bg) = bg {
                presented.background_color = bg;
            }
            self.ensure_shadow_layer(&presented)?;
            self.apply_shadow(&presented)?;
            if let Some(layer) = self.shadow_layer.borrow().as_ref() {
                layer.update(self, &presented)?;
            }
        }
        if has_transform {
            self.transform(&style, |i| {
                values
                    .iter()
                    .find_map(|(p, v)| (*p == P::Transform(i as u8)).then(|| v.number()))
            })?;
        }
        if let Some(wrapper) = self.wrapper.borrow().as_ref()
            && let Some(composite) = wrapper.composite.borrow().as_ref()
            && (opacity.is_some() || bg.is_some() || border_color.is_some() || has_transform)
        {
            composite.update(self)?;
        }
        if let Some(image) = self.image.borrow().as_ref()
            && (bg.is_some() || border_color.is_some())
        {
            let mut animated_style = style.clone();
            if let Some(bg) = bg {
                animated_style.background_color = bg;
            }
            if let Some(color) = border_color {
                animated_style.borders.colors.fill(color);
            }
            image.style(&animated_style)?;
        }
        drop(style);
        if !component_props.is_empty() {
            if let Some(component) = self.component.borrow().as_ref() {
                component.props(component_props)?;
            } else {
                self.component_animated.borrow_mut().clear();
            }
        }
        Ok(())
    }
}
