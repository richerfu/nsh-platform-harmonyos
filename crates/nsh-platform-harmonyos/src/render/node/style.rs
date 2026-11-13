//! Applying style attributes and restoring temporary native presentation.

use std::rc::Rc;

use nsh_core::{Points, ViewKind, ViewStyle};
use ohos_arkui_binding::{
    common::attribute::ArkUINodeAttributeNumber as Number,
    component::attribute::{ArkUICommonAttribute, ArkUIEvent},
    types::{advanced::NodeDirtyFlag, attribute::ArkUINodeAttributeType as Attribute},
};
use ohos_drawing_binding::Canvas;

use super::{NativeNode, Node, Result, callback};
use crate::{render::geometry::Size, text::typography::argb};

impl Node {
    pub(crate) fn density_changed(&self) -> Result<()> {
        self.decoration.clear_paint_cache();
        self.invalidate_composites();
        self.update_layout_bounds()?;
        self.restore_style()?;
        self.animator.apply_node(self)?;
        self.native
            .borrow()
            .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
        self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        Ok(())
    }

    /// The corner radii its native node was given (top-left first), which
    /// platform-owned children that fill it clip to.
    pub(crate) fn border_radii(&self) -> [f32; 4] {
        self.applied.get().border_radius
    }
    pub(crate) fn clips_children(&self) -> bool {
        self.style.borrow().overflow_hidden
            || matches!(
                self.kind,
                ViewKind::Scroll | ViewKind::Image | ViewKind::Stack | ViewKind::Input
            )
            || self.modal.borrow().as_ref().is_some_and(|modal| {
                modal.presentation() != nsh_core::ModalPresentation::FullScreen
            })
    }
    pub(crate) fn header_item_visible(&self) -> bool {
        if self.kind != ViewKind::HeaderItem {
            return true;
        }
        let Some(parent) = self.parent_node.borrow().upgrade() else {
            return false;
        };
        let Some(screen) = parent.screen.borrow().as_ref().cloned() else {
            return false;
        };
        let c = screen.config.borrow();
        c.header_visible
            && (c.left == Some(self.id)
                || c.right == Some(self.id)
                || c.title_view == Some(self.id))
    }
    pub(crate) fn style(self: &Rc<Self>, style: &ViewStyle) -> Result<()> {
        self.invalidate_composites();
        if style.outline.width.0 > 0.0 && !self.outlined.replace(true) {
            let weak = Rc::downgrade(self);
            self.native
                .borrow_mut()
                .on_custom_foreground_draw(move |event| {
                    callback(|| {
                        if let Some(node) = weak.upgrade()
                            && !node.scene_active()
                            && let Some(raw) =
                                event.draw_context_in_draw().and_then(|ctx| ctx.canvas())
                        {
                            // SAFETY: ArkUI retains this drawing context for this callback.
                            let canvas = unsafe { Canvas::from_raw_borrowed(raw) };
                            let frame = node.frame.get();
                            node.decoration.outline(
                                &canvas,
                                &node.style.borrow(),
                                Size::new(frame.width, frame.height),
                                node.scale.get(),
                            );
                        }
                        Ok(())
                    })
                });
        }
        if self.style.borrow().effects != style.effects
            || self.style.borrow().outline != style.outline
        {
            self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        let cursor_changed = self.style.borrow().cursor != style.cursor;
        let mut target = Box::new(style.clone());
        if self.live.borrow().is_some() || target.transition.is_some() || target.animation.is_some()
        {
            let old = self.style.borrow().clone();
            self.live
                .borrow_mut()
                .get_or_insert_with(Default::default)
                .style(
                    &mut target,
                    *old,
                    self.platform.get().reduce_motion,
                    self.styled.get(),
                );
            self.animator.track(self);
        }
        self.styled.set(true);
        *self.style.borrow_mut() = target;
        if cursor_changed {
            self.set_events(self.interaction.mask())?;
            self.cursor.update(self, self.interaction.hovered())?;
        }
        if let Some(scene) = self.scene.borrow().as_ref() {
            scene.require(self);
        }
        self.ensure_wrapper()?;
        self.restore_style()
    }
    pub(super) fn sync_visual_node(&self, visual: &NativeNode) {
        let ptr = Rc::as_ptr(visual) as usize;
        let mut applied = self.applied.get();
        if applied.visual_ptr != ptr {
            applied.visual_ptr = ptr;
            applied.visibility = 0;
            applied.transform = None;
            applied.transform_center = [f32::NAN; 3];
            applied.blur = 0.0;
            applied.blend = nsh_core::BlendMode::Normal;
            applied.z_index = 0;
            self.applied.set(applied);
            self.interaction.invalidate_opacity();
        }
    }
    fn unhidden_for_effects(&self, style: &ViewStyle) -> bool {
        if !self.active.get() || self.retained.get() || style.hidden {
            return false;
        }
        let mut at = self.parent_node.borrow().upgrade();
        while let Some(current) = at {
            if !current.active.get() || current.retained.get() || current.style.borrow().hidden {
                return false;
            }
            at = current.parent_node.borrow().upgrade();
        }
        true
    }
    pub(crate) fn effects_plan_for(
        &self,
        style: &ViewStyle,
    ) -> crate::render::effects::EffectsPlan {
        let surface_active = self.shaders.surface_active();
        let hosts_retainable = self.shaders.hosts_retainable();
        let unhidden = hosts_retainable && self.unhidden_for_effects(style);
        let shaders = self.shader_layers.borrow();
        let has_shaders = !shaders.is_empty()
            && hosts_retainable
            && (unhidden || (surface_active && self.shaders.has_navigation_layer(self.id)));
        let shader_samples_backdrop = has_shaders
            && self.id != nsh_core::ROOT_ID
            && shaders.iter().any(|(_, layer)| {
                layer
                    .shader
                    .inputs()
                    .contains(nsh_core::shader::ShaderInput::Backdrop)
            });
        drop(shaders);
        let shader_keep_alive = unhidden && self.shaders.keep_alive(self.id);
        let composite_active = self.wrapper.borrow().as_ref().is_some_and(|wrapper| {
            wrapper
                .composite
                .borrow()
                .as_ref()
                .is_some_and(|composite| composite.surface().is_some())
        });
        crate::render::effects::EffectsPlan::compute(
            style,
            crate::render::effects::EffectsContext {
                has_shaders,
                shader_keep_alive,
                shader_samples_backdrop,
                navigation_capture: surface_active && self.navigation_capture.get(),
                scene_active: self.scene_active(),
                composite_active,
                has_native_paint: self.has_native_paint(style),
            },
        )
    }
    pub(crate) fn effects_plan(&self) -> crate::render::effects::EffectsPlan {
        self.effects_plan_for(&self.style.borrow())
    }
    pub(crate) fn desired_z_index(&self) -> i32 {
        if self.pinned.get() != (0.0, 0.0) {
            1_000_000
        } else if self.kind == ViewKind::HeaderItem {
            2
        } else {
            0
        }
    }
    pub(crate) fn apply_z_index(&self) -> Result<()> {
        let visual = self.visual_node();
        self.sync_visual_node(&visual);
        let z_index = self.desired_z_index();
        let mut applied = self.applied.get();
        if applied.z_index != z_index {
            visual
                .borrow()
                .set_i32_attribute(Attribute::ZIndex, z_index)?;
            applied.z_index = z_index;
            self.applied.set(applied);
        }
        Ok(())
    }
    pub(super) fn apply_effects(&self, style: &ViewStyle) -> Result<()> {
        let visual = self.visual_node();
        self.sync_visual_node(&visual);
        let plan = self.effects_plan_for(style);
        let mut applied = self.applied.get();
        let scale = self.scale.get();
        let blur = crate::render::composition::geometry::native_blur_value(plan.native_blur, scale);
        if applied.blur != blur {
            visual.borrow().set_f32_attribute(Attribute::Blur, blur)?;
            applied.blur = blur;
        }
        let backdrop_blur = crate::render::composition::geometry::native_blur_value(
            plan.native_backdrop_blur,
            scale,
        );
        if applied.backdrop_blur != backdrop_blur {
            self.native
                .borrow()
                .set_f32_attribute(Attribute::BackdropBlur, backdrop_blur)?;
            applied.backdrop_blur = backdrop_blur;
        }
        if applied.blend != plan.native_blend {
            let mode = crate::render::decor::arkui_blend_mode(plan.native_blend);
            visual
                .borrow()
                .set_attribute(Attribute::BlendMode, vec![i32::from(mode), 1].into())?;
            applied.blend = plan.native_blend;
        }
        self.applied.set(applied);
        Ok(())
    }
    pub(crate) fn reset_visual_baseline(&self, node: &NativeNode) -> Result<()> {
        let native = node.borrow();
        native.reset_attribute(Attribute::LayoutRect)?;
        native.reset_attribute(Attribute::Position)?;
        native.reset_attribute(Attribute::Transform)?;
        native.reset_attribute(Attribute::TransformCenter)?;
        native.reset_attribute(Attribute::Blur)?;
        native.reset_attribute(Attribute::BlendMode)?;
        native.reset_attribute(Attribute::ZIndex)?;
        native.opacity(1.0)?;
        native.set_i32_attribute(Attribute::Visibility, 0)?;
        Ok(())
    }
    pub(crate) fn apply_visual_style(&self, style: &ViewStyle) -> Result<()> {
        let visual = self.visual_node();
        self.sync_visual_node(&visual);
        self.apply_effects(style)?;
        self.apply_z_index()?;
        self.apply_opacity(style.opacity)?;
        self.transform(style, |_| None)?;
        self.animator.apply_node(self)?;
        Ok(())
    }
    pub(crate) fn visual_opacity_for(&self, style_opacity: f32) -> f32 {
        crate::render::effects::resolve_visual_opacity(
            style_opacity,
            self.animated_opacity.get(),
            self.presentation.get().0,
            self.extra.get().0,
            self.navigation_presentation.get().0,
        )
    }
    pub(super) fn apply_opacity(&self, style_opacity: f32) -> Result<()> {
        let visual = self.visual_node();
        self.sync_visual_node(&visual);
        let scene_active = self.scene_active();
        if scene_active {
            self.invalidate_scene();
        }
        self.interaction.opacity(
            self,
            if self.export_repair.get() != 0 {
                0.0
            } else if scene_active {
                1.0
            } else {
                self.visual_opacity_for(style_opacity)
            },
        )
    }
    pub(super) fn apply_background_color(&self, color: nsh_core::Color) -> Result<()> {
        let default_surface_bg = self.kind == ViewKind::Screen
            || self.modal.borrow().as_ref().is_some_and(|modal| {
                modal.presentation() != nsh_core::ModalPresentation::FullScreen
            });
        let bg = if default_surface_bg && color.is_transparent() {
            let dark = self
                .appearance
                .get()
                .resolve(self.platform.get().color_scheme)
                == nsh_engine::ColorScheme::Dark;
            self.theme.borrow().color("background", dark)
        } else {
            argb(color)
        };
        let mut applied = self.applied.get();
        if applied.background_color != bg {
            self.native.borrow().background_color(bg)?;
            applied.background_color = bg;
            self.applied.set(applied);
        }
        Ok(())
    }
    pub(super) fn apply_borders(&self, style: &ViewStyle) -> Result<()> {
        let corners = style
            .corner_radius
            .map(|r| r.unwrap_or(style.border_radius).0);
        let mut radius = [
            corners.top_left,
            corners.top_right,
            corners.bottom_left,
            corners.bottom_right,
        ];
        if let Some(modal) = self.modal.borrow().as_ref()
            && radius == [0.0; 4]
        {
            radius = match modal.presentation() {
                nsh_core::ModalPresentation::FullScreen => [0.0; 4],
                nsh_core::ModalPresentation::PageSheet => [16.0, 16.0, 0.0, 0.0],
                nsh_core::ModalPresentation::FormSheet => [16.0; 4],
            };
        }
        let frame = self.frame.get();
        if frame.width > 0.0 && frame.height > 0.0 {
            let short = frame.width.min(frame.height);
            let half = short * 0.5;
            let snapped_half = ((short * self.scale.get()).round() * 0.5) / self.scale.get();
            for r in &mut radius {
                if *r >= half - 1e-4 && *r < snapped_half {
                    *r = snapped_half;
                }
            }
        }
        let widths = style.borders.widths.to_array().map(|v| v.0);
        let colors = style.borders.colors.map(argb);
        let border_style = match style.borders.style {
            nsh_core::BorderStyle::Solid => 0,
            nsh_core::BorderStyle::Dashed => 1,
            nsh_core::BorderStyle::Dotted => 2,
        };
        let mut applied = self.applied.get();
        let node = self.native.borrow();
        if applied.border_radius != radius {
            node.set_number_attribute(
                Attribute::BorderRadius,
                radius.into_iter().map(Number::Float).collect(),
            )?;
            applied.border_radius = radius;
        }
        if applied.border_width != widths {
            node.set_number_attribute(
                Attribute::BorderWidth,
                widths.into_iter().map(Number::Float).collect(),
            )?;
            applied.border_width = widths;
        }
        if applied.border_color != colors {
            node.set_number_attribute(
                Attribute::BorderColor,
                colors.into_iter().map(Number::Uint).collect(),
            )?;
            applied.border_color = colors;
        }
        if applied.border_style != border_style {
            node.set_i32_attribute(Attribute::BorderStyle, border_style)?;
            applied.border_style = border_style;
        }
        self.applied.set(applied);
        Ok(())
    }
    /// Outlines still use the foreground callback; box shadows own bounded nodes.
    pub(super) fn apply_ink_outset(&self, style: &ViewStyle) -> Result<()> {
        let mut outset = 0.0f32;
        if style.outline.is_visible() {
            outset = outset.max(style.outline.width.0 + style.outline.offset.0);
        }
        let outset = outset.max(0.0).ceil();
        let mut applied = self.applied.get();
        if applied.ink_outset != outset {
            applied.ink_outset = outset;
            self.applied.set(applied);
            self.invalidate_ink(outset.max(1.0))?;
        }
        Ok(())
    }
    /// A view whose paint reaches outside its bounds moved: ArkUI repaints
    /// only the view's own region, so its parent repaints the area the paint
    /// left (inside the parent's bounds).
    pub(super) fn invalidate_ink(&self, outset: f32) -> Result<()> {
        if outset > 0.0
            && let Some(parent) = self.parent_node.borrow().upgrade()
        {
            parent
                .child_container()
                .borrow()
                .mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        Ok(())
    }
    pub(super) fn apply_shadow(&self, style: &ViewStyle) -> Result<()> {
        use crate::render::shadow_geometry::{native_box_shadow, native_radius};
        let next = native_box_shadow(style).map(|shadow| {
            (
                native_radius(shadow.blur.0 * self.scale.get() * 0.5),
                shadow.offset_x.0 * self.scale.get(),
                shadow.offset_y.0 * self.scale.get(),
                argb(shadow.color),
            )
        });
        let mut applied = self.applied.get();
        if applied.shadow != next {
            let node = self.native.borrow();
            if let Some((radius, ox, oy, color)) = next {
                node.set_number_slice_attribute(
                    Attribute::CustomShadow,
                    &[
                        Number::Float(radius),
                        Number::Int(0),
                        Number::Float(ox),
                        Number::Float(oy),
                        Number::Int(0),
                        Number::Uint(color),
                        Number::Uint(0),
                    ],
                )?;
            } else {
                node.reset_attribute(Attribute::CustomShadow)?;
            }
            applied.shadow = next;
            self.applied.set(applied);
        }
        Ok(())
    }
    pub(crate) fn apply_clip(&self) -> Result<()> {
        let clip = self.clips_children();
        let mut applied = self.applied.get();
        let style = self.style.borrow();
        let frame = self.frame.get();
        let size = Size::new(frame.width, frame.height);
        let shape = (clip && style.continuous_corners && size.width > 0.0 && size.height > 0.0)
            .then(|| {
                (
                    size,
                    nsh_core::canvas::fit_radii(
                        size.into(),
                        style
                            .corner_radius
                            .map(|r| r.unwrap_or(style.border_radius)),
                    )
                    .to_array()
                    .map(|r| r.0),
                )
            });
        if applied.clip_shape != shape {
            if let Some((size, radii)) = shape {
                use std::fmt::Write;

                use ohos_arkui_binding::common::attribute::ArkUINodeCompositeAttributeItem;
                let mut path = String::new();
                nsh_core::shape::rounded_rect(
                    nsh_core::Bounds::new(
                        nsh_core::Point::default(),
                        nsh_core::Size::new(
                            Points(size.width * self.scale.get()),
                            Points(size.height * self.scale.get()),
                        ),
                    ),
                    nsh_core::Corners::from(radii.map(|r| {
                        nsh_core::Size::new(
                            Points(r * self.scale.get()),
                            Points(r * self.scale.get()),
                        )
                    })),
                    true,
                    false,
                    &mut |segment| {
                        match segment {
                            nsh_core::shape::Segment::Move(p) => {
                                write!(path, "M {} {} ", p.x.0, p.y.0)
                            }
                            nsh_core::shape::Segment::Line(p) => {
                                write!(path, "L {} {} ", p.x.0, p.y.0)
                            }
                            nsh_core::shape::Segment::Cubic([a, b, c]) => write!(
                                path,
                                "C {} {} {} {} {} {} ",
                                a.x.0, a.y.0, b.x.0, b.y.0, c.x.0, c.y.0
                            ),
                            nsh_core::shape::Segment::Close => write!(path, "Z"),
                        }
                        .expect("write path string");
                    },
                );
                self.native.borrow().set_attribute(
                    Attribute::ClipShape,
                    ArkUINodeCompositeAttributeItem::new()
                        .with_number_values(vec![
                            Number::Int(3),
                            Number::Float(size.width),
                            Number::Float(size.height),
                        ])
                        .with_string(path)
                        .into(),
                )?;
            } else {
                self.native.borrow().reset_attribute(Attribute::ClipShape)?;
            }
            applied.clip_shape = shape;
            self.applied.set(applied);
        }
        if applied.clip != clip {
            self.native
                .borrow()
                .set_bool_attribute(Attribute::Clip, clip)?;
            applied.clip = clip;
            self.applied.set(applied);
            let frame = self.frame.get();
            if frame.width <= 0.0 || frame.height <= 0.0 {
                self.update_layout_bounds()?;
                self.native
                    .borrow()
                    .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
            }
        }
        Ok(())
    }
    pub(super) fn ensure_shadow_layer(&self, style: &ViewStyle) -> Result<()> {
        let needs_shadow_layer = self.shadow_layer.borrow().is_none()
            && crate::render::shadow_geometry::native_box_shadow(style).is_none()
            && style
                .effects
                .as_ref()
                .is_some_and(|e| e.box_shadows.iter().any(|s| !s.inset && s.is_visible()));
        if needs_shadow_layer && let Some(owner) = self.weak.upgrade() {
            self.enable_layout();
            *self.shadow_layer.borrow_mut() =
                Some(crate::render::shadow::ShadowLayer::new(&owner)?);
        }
        Ok(())
    }
    pub(super) fn apply_style(&self, style: &ViewStyle) -> Result<()> {
        if style.effects.as_ref().is_some_and(|effects| {
            !effects.background_images.is_empty()
                || effects
                    .box_shadows
                    .iter()
                    .any(|shadow| shadow.inset && shadow.is_visible())
        }) {
            self.enable_drawing();
        }
        crate::render::decor::warn_repeating_conic(self, style);
        self.ensure_shadow_layer(style)?;
        if let Some(wrapper) = self.wrapper.borrow().as_ref()
            && let Some(composite) = wrapper.composite.borrow().as_ref()
        {
            composite.update(self)?;
        }
        if let Some(image) = self.image.borrow().as_ref() {
            image.style(style)?;
        }
        self.apply_background_color(style.background_color)?;
        self.apply_borders(style)?;
        self.apply_shadow(style)?;
        if let Some(layer) = self.shadow_layer.borrow().as_ref() {
            layer.update(self, style)?;
        }
        self.apply_ink_outset(style)?;
        self.apply_clip()?;
        self.apply_visual_style(style)
    }
    pub(crate) fn reset_text_tint(&self) -> Result<()> {
        if self.tint.take().is_some() {
            self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        Ok(())
    }
    pub(crate) fn export_hidden(&self) -> bool {
        self.applied.get().visibility != 0 || self.export_repair.get() != 0
    }
    pub(crate) fn restore_after_export(self: &Rc<Self>) -> Result<()> {
        if !self.active.get() {
            return Ok(());
        }
        let mut applied = self.applied.get();
        applied.visibility = -1;
        self.applied.set(applied);
        self.interaction.invalidate_opacity();
        self.restore_style()?;
        let inherited = self
            .parent_node
            .borrow()
            .upgrade()
            .is_some_and(|parent| parent.interaction.blocks_children());
        self.interaction.pointer(self, inherited)?;
        self.accessibility_state.changed(self);
        Ok(())
    }
    pub(crate) fn restore_export_controls(&self) -> Result<()> {
        if !self.active.get() {
            return Ok(());
        }
        if let Some(screen) = self.screen.borrow().as_ref() {
            screen.restore_visibility()?;
        }
        if let Some(video) = self.video.borrow().as_ref() {
            video.restore_visibility()?;
        }
        Ok(())
    }
    pub(crate) fn restore_style(&self) -> Result<()> {
        self.apply_style(&self.style.borrow())
    }
}
