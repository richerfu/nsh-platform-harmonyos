//! Native transforms shared by presentation and input coordinates.

use nsh_core::{Points, ViewStyle};
use ohos_arkui_binding::{
    common::attribute::ArkUINodeAttributeNumber as Number,
    component::attribute::ArkUICommonAttribute,
    types::{advanced::NodeDirtyFlag, attribute::ArkUINodeAttributeType as Attribute},
};

use super::{Node, Result};

impl Node {
    pub(crate) fn desired_visibility(&self) -> i32 {
        if self.retained.get() || !self.header_item_visible() {
            2
        } else if self
            .screen
            .borrow()
            .as_ref()
            .is_some_and(|screen| !screen.paint_visible())
        {
            1
        } else {
            i32::from(self.style.borrow().hidden)
        }
    }
    pub(crate) fn native_visibility_hidden(&self) -> bool {
        self.desired_visibility() != 0 || self.backface_away.get()
    }
    fn notify_mount_parent_visibility_hidden(&self) -> Result<()> {
        if let Some(parent) = self.mount_parent.borrow().upgrade()
            && let Ok(parent_node) = parent.try_borrow()
            && parent_node.is_owned()
        {
            parent_node.mark_dirty(NodeDirtyFlag::NeedLayout)?;
            parent_node.mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        Ok(())
    }
    /// Native child-plane transform, calculated once for a hit-test descent.
    pub(crate) fn input_children_transform(&self) -> Result<nsh_core::Matrix> {
        use nsh_core::{Matrix, TransformKind as K};
        let Some(scroll) = self.scroll.borrow().as_ref().cloned() else {
            return Ok(Matrix::IDENTITY);
        };
        let content = scroll.content.borrow().layout_position_in_window()?;
        let origin = self.native.borrow().layout_position_in_window()?;
        let offset = (
            (content.x - origin.x) as f64 / self.scale.get() as f64 + scroll.centering().0 as f64,
            (content.y - origin.y) as f64 / self.scale.get() as f64 + scroll.centering().1 as f64,
        );
        let matrix = Matrix::of(K::Scale, scroll.scale() as f64)
            .mul(&Matrix::of(K::TranslateX, offset.0))
            .mul(&Matrix::of(K::TranslateY, offset.1));
        Ok(if scroll.mirrored() {
            matrix
                .mul(&Matrix::of(K::ScaleX, -1.0))
                .mul(&Matrix::of(K::TranslateX, self.frame.get().width as f64))
        } else {
            matrix
        })
    }
    pub(crate) fn input_in_plane(&self, plane: &nsh_core::Matrix) -> Result<nsh_core::Matrix> {
        use nsh_core::{Matrix, TransformKind as K};
        // Mounted nodes are direct children of the native child plane, including
        // wrappers and screen header offsets. This local query is O(1).
        let position = self.mounted().borrow().layout_position()?;
        Ok(self
            .input_transform
            .get()
            .mul(&Matrix::of(
                K::TranslateX,
                position.x as f64 / self.scale.get() as f64,
            ))
            .mul(&Matrix::of(
                K::TranslateY,
                position.y as f64 / self.scale.get() as f64,
            ))
            .mul(plane))
    }
    pub(crate) fn input_to_parent(&self, parent: &Node) -> Result<nsh_core::Matrix> {
        self.input_in_plane(&parent.input_children_transform()?)
    }
    pub(crate) fn input_in_root(&self, root: &Node) -> Result<Option<nsh_core::Matrix>> {
        if self.id == root.id {
            return Ok(Some(nsh_core::Matrix::IDENTITY));
        }
        let Some(parent) = self.parent_node.borrow().upgrade() else {
            return Ok(None);
        };
        parent
            .input_in_root(root)?
            .map(|world| self.input_to_parent(&parent).map(|m| m.mul(&world)))
            .transpose()
    }
    pub(crate) fn pin(&self, x: f32, y: f32) -> Result<()> {
        if self.pinned.replace((x, y)) != (x, y) {
            self.apply_z_index()?;
            self.transform(&self.style.borrow(), |_| None)?;
            self.animator.apply_node(self)?;
        }
        Ok(())
    }
    pub(crate) fn presentation(&self, alpha: f32, y: f32) -> Result<()> {
        let old = self.presentation.replace((alpha, y));
        if old != (alpha, y) {
            let style = self.style.borrow();
            if old.0 != alpha {
                self.apply_opacity(style.opacity)?;
            }
            if old.1 != y {
                self.transform(&style, |_| None)?;
            }
            drop(style);
            self.animator.apply_node(self)?;
        }
        Ok(())
    }
    pub(crate) fn transform(
        &self,
        style: &ViewStyle,
        value: impl Fn(usize) -> Option<f32>,
    ) -> Result<()> {
        use nsh_core::{Matrix, TransformKind as K};
        let visual = self.visual_node();
        self.sync_visual_node(&visual);
        let extra = self.extra.get().1;
        let pinned = self.pinned.get();
        let nav_x = self.navigation_presentation.get().1;
        let pres_y = self.presentation.get().1;
        let parent_mirrored = self.parent_node.borrow().upgrade().is_some_and(|parent| {
            parent
                .scroll
                .borrow()
                .as_ref()
                .is_some_and(|scroll| scroll.mirrored())
        });
        let is_identity = style.transform.ops().is_empty()
            && !parent_mirrored
            && extra == crate::render::transition::Extra::NONE
            && pinned == (0.0, 0.0)
            && nav_x == 0.0
            && pres_y == 0.0;
        let mut applied = self.applied.get();
        let native = visual.borrow();
        if is_identity {
            self.backface_away.set(false);
            let visibility = if self.export_repair.get() != 0 {
                0
            } else {
                self.desired_visibility()
            };
            let visibility_changed = applied.visibility != visibility;
            let revealed = visibility_changed && applied.visibility != 0 && visibility == 0;
            if visibility_changed {
                native.set_i32_attribute(Attribute::Visibility, visibility)?;
                applied.visibility = visibility;
            }
            if applied.transform.is_some() {
                native.reset_attribute(Attribute::Transform)?;
                applied.transform = None;
                self.invalidate_ink(applied.ink_outset)?;
            }
            self.applied.set(applied);
            drop(native);
            if visibility_changed {
                self.apply_opacity(style.opacity)?;
                if visibility != 0 {
                    self.notify_mount_parent_visibility_hidden()?;
                }
            }
            if revealed {
                self.schedule_subtree_paint(2)?;
            }
            if self.input_transform.replace(Matrix::IDENTITY) != Matrix::IDENTITY {
                self.invalidate_composites();
            }
            if self.scene_active() {
                self.invalidate_scene();
            }
            return Ok(());
        }
        let mut m = style.transform.matrix(value);
        if parent_mirrored {
            m = Matrix::of(K::ScaleX, -1.0).mul(&m);
        }
        let m = if extra == crate::render::transition::Extra::NONE {
            m
        } else {
            Matrix::of(K::Scale, extra.scale as f64)
                .mul(&Matrix::of(K::Rotate, extra.rotate.to_degrees() as f64))
                .mul(&Matrix::of(
                    K::TranslateY,
                    (extra.shift_y.0 * self.frame.get().height) as f64,
                ))
                .mul(&m)
        };
        let mut matrix = m.0;
        matrix[12] += (pinned.0 + nav_x) as f64;
        matrix[13] += (pres_y + pinned.1) as f64;
        // ArkUI matrices translate in pixels; core matrices use surface vp.
        let mut raw_values = [0.0f32; 16];
        for (i, v) in matrix.into_iter().enumerate() {
            raw_values[i] = (v * if matches!(i, 12..=14) {
                self.scale.get() as f64
            } else if matches!(i, 3 | 7 | 11) {
                1.0 / self.scale.get() as f64
            } else {
                1.0
            }) as f32;
        }
        if applied.transform != Some(raw_values) {
            native.set_number_attribute(
                Attribute::Transform,
                raw_values.into_iter().map(Number::Float).collect(),
            )?;
            applied.transform = Some(raw_values);
            self.invalidate_ink(applied.ink_outset)?;
        }
        self.backface_away
            .set(style.backface_hidden && m.0[10] < 0.0);
        let visibility = if self.export_repair.get() != 0 {
            0
        } else if self.retained.get() || !self.header_item_visible() {
            2
        } else if self
            .screen
            .borrow()
            .as_ref()
            .is_some_and(|screen| !screen.paint_visible())
        {
            1
        } else {
            i32::from(style.hidden || self.backface_away.get())
        };
        let visibility_changed = applied.visibility != visibility;
        let revealed = visibility_changed && applied.visibility != 0 && visibility == 0;
        if visibility_changed {
            native.set_i32_attribute(Attribute::Visibility, visibility)?;
            applied.visibility = visibility;
        }
        let frame = self.frame.get();
        let ox = style
            .transform_origin
            .x
            .to_points(
                nsh_core::geometry::AbsoluteLength::Points(Points(frame.width)),
                nsh_core::geometry::REM_SIZE,
            )
            .0;
        let oy = style
            .transform_origin
            .y
            .to_points(
                nsh_core::geometry::AbsoluteLength::Points(Points(frame.height)),
                nsh_core::geometry::REM_SIZE,
            )
            .0;
        let oz = style.transform_origin.z.0;
        let center = [ox, oy, oz];
        if applied.transform_center != center {
            native.set_attribute(Attribute::TransformCenter, center.into())?;
            applied.transform_center = center;
        }
        self.applied.set(applied);
        drop(native);
        if visibility_changed {
            self.apply_opacity(style.opacity)?;
            if visibility != 0 {
                self.notify_mount_parent_visibility_hidden()?;
            }
        }
        if revealed {
            self.schedule_subtree_paint(2)?;
        }
        let origin = (ox as f64, oy as f64, oz as f64);
        let mut before = Matrix::IDENTITY;
        before.0[12] = -origin.0;
        before.0[13] = -origin.1;
        before.0[14] = -origin.2;
        let mut after = Matrix::IDENTITY;
        after.0[12] = origin.0;
        after.0[13] = origin.1;
        after.0[14] = origin.2;
        let transform = before.mul(&Matrix(matrix)).mul(&after);
        if self.input_transform.replace(transform) != transform {
            self.invalidate_composites();
        }
        if self.scene_active() {
            self.invalidate_scene();
        }
        Ok(())
    }
    pub(crate) fn navigation_presentation(&self, alpha: f32, x: f32) -> Result<()> {
        let old = self.navigation_presentation.replace((alpha, x));
        if old != (alpha, x) {
            let style = self.style.borrow();
            if old.0 != alpha {
                self.apply_opacity(style.opacity)?;
            }
            if old.1 != x {
                self.transform(&style, |_| None)?;
            }
            drop(style);
            self.animator.apply_node(self)?;
        }
        Ok(())
    }
}
