//! Reconcile hidden, retained children when a native subtree changes texture export.
//!
//! `RSNode::SetTextureExport` reuses its RenderThread node; `SyncTextureExport`
//! re-adds only current children, and `RemoveChild` targets only the current side.
//! ArkUI's `ReCreateRsNodeTreeByTargetList` omits invisible FrameNodes. Briefly
//! including the outermost hidden nodes, transparent and inert, lets the export
//! reach them before normal visibility removes them on the destination side too.
//! This applies both when entering export and when returning to RenderService.
//! Two frame callbacks leave one complete rebuild between attachment and restore,
//! regardless of whether the export switch precedes that rebuild. Routes own the
//! repair; callbacks and native nodes are weak so retirement cancels it safely.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use ohos_arkui_binding::{
    component::attribute::ArkUICommonAttribute,
    types::{
        advanced::NodeDirtyFlag,
        attribute::ArkUINodeAttributeType as A,
        view::{AccessibilityMode, HitTestMode},
    },
};
use rustc_hash::FxHashMap;

use crate::render::node::{NativeNode, Node, Result, callback};

#[derive(Default)]
pub(crate) struct Export {
    pub(crate) enabled: Cell<bool>,
    epoch: Cell<u64>,
    hidden: RefCell<Vec<Hidden>>,
}

struct Hidden {
    native: Weak<RefCell<ohos_arkui_binding::common::node::ArkUINode>>,
    owner: Option<Weak<Node>>,
    controls: Option<Weak<Node>>,
    visibility: i32,
    opacity: f32,
    hit_test: i32,
    accessibility: i32,
    focusable: i32,
}

impl Export {
    pub(crate) fn prepare(&self, owner: &Rc<Node>, inner: &NativeNode) -> Result<()> {
        if !self.enabled.get() {
            return Ok(());
        }
        self.restore();
        self.epoch.set(self.epoch.get().wrapping_add(1));
        let mut owners = FxHashMap::default();
        Self::owners(owner, &mut owners);
        let result = self.walk(inner, &owners, None);
        if result.is_err() {
            self.restore();
        }
        result
    }

    fn owners(node: &Rc<Node>, owners: &mut FxHashMap<usize, Weak<Node>>) {
        owners.insert(Rc::as_ptr(&node.mounted()) as usize, Rc::downgrade(node));
        owners.insert(Rc::as_ptr(&node.native) as usize, Rc::downgrade(node));
        for child in node
            .retained_children
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
        {
            if child.retained.get() && child.retained_owner.borrow().ptr_eq(&Rc::downgrade(node)) {
                Self::owners(&child, owners);
            }
        }
        for child in node.children.borrow().iter().filter_map(Weak::upgrade) {
            // Native dialogs have their own export boundary.
            if child.modal.borrow().is_none() {
                Self::owners(&child, owners);
            }
        }
    }

    fn walk(
        &self,
        native: &NativeNode,
        owners: &FxHashMap<usize, Weak<Node>>,
        controls: Option<&Rc<Node>>,
    ) -> Result<()> {
        let owner = owners
            .get(&(Rc::as_ptr(native) as usize))
            .and_then(Weak::upgrade);
        if let Some(owner) = &owner {
            if owner.export_hidden() {
                return self.hide(&owner.visual_node(), Some(owner), None);
            }
            if !Rc::ptr_eq(native, &owner.native) {
                return self.walk(&owner.native, owners, Some(owner));
            }
        }
        // Includes platform-only children such as native header and video controls.
        if native.borrow().i32_attribute(A::Visibility, 0)? != 0 {
            return self.hide(native, None, owner.as_ref().or(controls));
        }
        for child in native.borrow().children() {
            self.walk(child, owners, owner.as_ref().or(controls))?;
        }
        Ok(())
    }

    fn hide(
        &self,
        native: &NativeNode,
        owner: Option<&Rc<Node>>,
        controls: Option<&Rc<Node>>,
    ) -> Result<()> {
        let node = native.borrow();
        let hidden = Hidden {
            native: Rc::downgrade(native),
            owner: owner.map(Rc::downgrade),
            controls: controls.map(Rc::downgrade),
            visibility: node.i32_attribute(A::Visibility, 0)?,
            opacity: node.f32_attribute(A::Opacity, 0)?,
            hit_test: node.i32_attribute(A::HitTestBehavior, 0)?,
            accessibility: node.i32_attribute(A::AccessibilityMode, 0)?,
            focusable: node.i32_attribute(A::Focusable, 0)?,
        };
        if let Some(owner) = owner {
            owner.export_repair.set(owner.export_repair.get() + 1);
        }
        self.hidden.borrow_mut().push(hidden);
        node.opacity(0.0)?;
        node.set_i32_attribute(A::HitTestBehavior, HitTestMode::None.into())?;
        node.set_i32_attribute(
            A::AccessibilityMode,
            AccessibilityMode::DisabledForDescendants.into(),
        )?;
        node.set_i32_attribute(A::Focusable, 0)?;
        node.set_i32_attribute(A::Visibility, 0)?;
        Ok(())
    }

    pub(crate) fn after_attach(self: &Rc<Self>, inner: &NativeNode) -> Result<()> {
        if self.hidden.borrow().is_empty() {
            return Ok(());
        }
        let epoch = self.epoch.get();
        let result = Self::defer_restore(Rc::downgrade(self), Rc::downgrade(inner), epoch, 3);
        if result.is_err() {
            self.restore();
        }
        result
    }

    fn defer_restore(
        repair: Weak<Self>,
        native: Weak<RefCell<ohos_arkui_binding::common::node::ArkUINode>>,
        epoch: u64,
        remaining: u8,
    ) -> Result<()> {
        let Some(node) = native.upgrade() else {
            if let Some(repair) = repair.upgrade()
                && repair.epoch.get() == epoch
            {
                repair.restore();
            }
            return Ok(());
        };
        node.borrow()
            .post_frame_callback(move |_, _| {
                let Some(current) = repair.upgrade() else {
                    return;
                };
                if current.epoch.get() != epoch {
                    return;
                }
                if remaining == 0 {
                    current.restore();
                    return;
                }
                // Property updates or nested textureController attachment before
                // RSRootNode::AttachRSSurfaceNode must not shorten the rebuild
                // in which these nodes join the exported tree.
                let mut released = Vec::new();
                current.hidden.borrow_mut().retain(|hidden| {
                    let keep = hidden
                        .owner
                        .as_ref()
                        .and_then(Weak::upgrade)
                        .is_none_or(|owner| owner.desired_visibility() != 0);
                    if !keep {
                        released.push(Hidden {
                            native: hidden.native.clone(),
                            owner: hidden.owner.clone(),
                            controls: hidden.controls.clone(),
                            visibility: hidden.visibility,
                            opacity: hidden.opacity,
                            hit_test: hidden.hit_test,
                            accessibility: hidden.accessibility,
                            focusable: hidden.focusable,
                        });
                    }
                    keep
                });
                for hidden in released {
                    Self::restore_one(hidden);
                }
                for hidden in current.hidden.borrow().iter() {
                    if let Some(native) = hidden.native.upgrade()
                        && native.borrow().is_owned()
                    {
                        callback(|| {
                            let node = native.borrow();
                            node.opacity(0.0)?;
                            node.set_i32_attribute(A::Visibility, 0)?;
                            node.set_i32_attribute(A::HitTestBehavior, HitTestMode::None.into())?;
                            node.set_i32_attribute(
                                A::AccessibilityMode,
                                AccessibilityMode::DisabledForDescendants.into(),
                            )?;
                            node.set_i32_attribute(A::Focusable, 0)?;
                            Ok(())
                        });
                    }
                }
                if Self::defer_restore(repair, native, epoch, remaining - 1).is_err() {
                    current.restore();
                }
            })
            .map_err(Into::into)
    }

    pub(crate) fn restore(&self) {
        self.epoch.set(self.epoch.get().wrapping_add(1));
        for hidden in self.hidden.take() {
            Self::restore_one(hidden);
        }
    }

    fn restore_one(hidden: Hidden) {
        callback(|| {
            let owner = hidden.owner.and_then(|owner| owner.upgrade());
            if let Some(owner) = &owner {
                owner
                    .export_repair
                    .set(owner.export_repair.get().saturating_sub(1));
            }
            let Some(native) = hidden.native.upgrade() else {
                return Ok(());
            };
            let keep_transparent = owner.as_ref().is_some_and(|owner| {
                owner.export_repair.get() != 0 || owner.native_visibility_hidden()
            });
            {
                let node = native.borrow();
                if !node.is_owned() {
                    return Ok(());
                }
                if owner
                    .as_ref()
                    .is_none_or(|owner| Rc::ptr_eq(&native, &owner.visual_node()))
                {
                    node.set_i32_attribute(A::Visibility, hidden.visibility)?;
                    node.opacity(if keep_transparent {
                        0.0
                    } else {
                        hidden.opacity
                    })?;
                }
                node.set_i32_attribute(A::HitTestBehavior, hidden.hit_test)?;
                node.set_i32_attribute(A::AccessibilityMode, hidden.accessibility)?;
                node.set_i32_attribute(A::Focusable, hidden.focusable)?;
            }
            if let Some(owner) = owner {
                owner.restore_after_export()?;
                owner.invalidate_subtree_paint()?;
                if owner.native_visibility_hidden()
                    && let Some(parent) = owner.mount_parent.borrow().upgrade()
                    && let Ok(parent_node) = parent.try_borrow()
                    && parent_node.is_owned()
                {
                    parent_node.mark_dirty(NodeDirtyFlag::NeedLayout)?;
                    parent_node.mark_dirty(NodeDirtyFlag::NeedRender)?;
                }
            }
            if let Some(controls) = hidden.controls.and_then(|node| node.upgrade()) {
                controls.restore_export_controls()?;
                if hidden.visibility != 0
                    && let Ok(parent_node) = controls.native.try_borrow()
                    && parent_node.is_owned()
                {
                    parent_node.mark_dirty(NodeDirtyFlag::NeedLayout)?;
                    parent_node.mark_dirty(NodeDirtyFlag::NeedRender)?;
                }
            }
            Ok(())
        });
    }
}
