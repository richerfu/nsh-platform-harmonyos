//! Native attachment ownership, creation, retirement and disposal.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use nsh_core::{NodeId, ViewKind};
use ohos_arkui_binding::{
    component::{attribute::ArkUICommonAttribute, built_in_component::Custom},
    types::advanced::{LengthMetricUnit, NodeDirtyFlag},
};

use super::{AppliedStyle, NativeNode, Node, Result, callback};
use crate::{
    input::{editor::Input, events::Interaction},
    render::geometry::Rect,
    scroll::controller::Scroller,
};

impl Node {
    pub(crate) fn new(
        id: NodeId,
        kind: ViewKind,
        context: Rc<crate::render::mount::UiContext>,
    ) -> Result<Rc<Self>> {
        let native = Custom::new()?.into_node();
        native
            .borrow()
            .set_length_metric_unit(LengthMetricUnit::Vp)?;
        // Every engine node is a native measure boundary, including the
        // zero-sized containers that position overflowing virtual children.
        native.borrow().set_layout_rect([0, 0, 0, 0])?;
        let visual_ptr = Rc::as_ptr(&native) as usize;
        let node = Rc::new_cyclic(|weak| Self {
            weak: weak.clone(),
            accessibility_state: crate::accessibility::state::State::default(),
            #[cfg(feature = "api-23")]
            accessibility_provider: crate::accessibility::provider::State::default(),
            native,
            wrapper: RefCell::default(),
            mount_parent: RefCell::default(),
            id,
            kind,
            pending_delete: Cell::new(false),
            scene_prepared: Cell::new(false),
            scene_root: Cell::new(false),
            shader_layers: RefCell::default(),
            navigation_capture: Cell::new(false),
            composite_dirty: Cell::new(true),
            capture_cache: Cell::new(None),
            navigation_shader: RefCell::default(),
            modal: RefCell::new(None),
            screen: RefCell::new(None),
            raw_frame: Cell::new(Rect::default()),
            frame: Cell::new(Rect::default()),
            parent: Cell::new(None),
            parent_node: RefCell::default(),
            children: RefCell::new(Vec::new()),
            context,
            input: RefCell::new(None),
            scroll: RefCell::new(None),
            image: RefCell::new(None),
            canvas: RefCell::new(None),
            component: RefCell::new(None),
            component_slots: RefCell::default(),
            component_animated: RefCell::default(),
            web: RefCell::default(),
            video: RefCell::default(),
            active: Cell::new(true),
            export_repair: Cell::new(0),
            clearing_export: Cell::new(false),
            bridged_on_root_outer: Cell::new(false),
            retained: Cell::new(false),
            retained_owner: RefCell::default(),
            retained_children: RefCell::default(),
            backface_away: Cell::new(false),
            interaction: Interaction::default(),
            style: RefCell::new(Box::default()),
            animated_opacity: Cell::new(None),
            applied: Cell::new(AppliedStyle {
                visual_ptr,
                ..AppliedStyle::default()
            }),
            styled: Cell::new(false),
            paint_state: Cell::new(crate::render::subtree_paint::SubtreePaint::default()),
            live: RefCell::default(),
            leaving: Cell::new(false),
            extra: Cell::new((1.0, crate::render::transition::Extra::NONE)),
            navigation_presentation: Cell::new((1.0, 0.0)),
            decoration: crate::render::decor::Decoration::default(),
            layout_callbacks: Cell::new(false),
            drawing: Cell::new(false),
            outlined: Cell::new(false),
            shadow_layer: RefCell::default(),
            text_drawing: RefCell::new(None),
            selection_area: RefCell::new(None),
            raster: RefCell::new(None),
            selection: Default::default(),
            tint: Cell::new(None),
            content: Cell::new(Rect::default()),
            overflow: Cell::new(None),
            raster_origin_y: Cell::new(0.0),
            presentation: Cell::new((1.0, 0.0)),
            pinned: Cell::new((0.0, 0.0)),
            input_transform: Cell::new(nsh_core::Matrix::IDENTITY),
            applied_layout_size: Cell::new((visual_ptr, 0, 0)),
            applied_position: Cell::new((0, 0, 0)),
            has_text_links: Cell::new(false),
        });
        if id == nsh_core::ROOT_ID || kind == ViewKind::Modal {
            crate::input::keyboard::install(&node)?;
        }
        match kind {
            ViewKind::Modal => {
                *node.modal.borrow_mut() =
                    Some(crate::navigation::modal::Modal::new(Rc::downgrade(&node))?)
            }
            ViewKind::Image => {
                *node.image.borrow_mut() = Some(crate::media::image::ImageView::new(
                    Rc::downgrade(&node),
                    node.images.clone(),
                )?)
            }
            ViewKind::Input => *node.input.borrow_mut() = Some(Input::new(Rc::downgrade(&node))?),
            ViewKind::Scroll => {
                *node.scroll.borrow_mut() = Some(Scroller::new(Rc::downgrade(&node))?)
            }
            ViewKind::Canvas => {
                *node.canvas.borrow_mut() = Some(crate::render::canvas::CanvasView::new(
                    Rc::downgrade(&node),
                    node.canvas_bytes.clone(),
                ));
            }
            ViewKind::Video => {
                *node.video.borrow_mut() =
                    Some(crate::media::video::Video::new(Rc::downgrade(&node))?);
            }
            ViewKind::WebView => {
                *node.web.borrow_mut() =
                    Some(crate::media::webview::WebView::new(Rc::downgrade(&node))?);
            }
            ViewKind::Screen => {
                let screen = crate::navigation::stack::Screen::new(Rc::downgrade(&node))?;
                screen.install(&node)?;
                *node.screen.borrow_mut() = Some(screen);
            }
            ViewKind::View
            | ViewKind::Text
            | ViewKind::Native
            | ViewKind::Stack
            | ViewKind::HeaderItem
            | ViewKind::InputAccessory => {}
        }
        if !matches!(
            kind,
            ViewKind::View
                | ViewKind::Text
                | ViewKind::Stack
                | ViewKind::HeaderItem
                | ViewKind::InputAccessory
        ) {
            node.enable_layout();
        }
        if node.canvas.borrow().is_some() {
            node.enable_drawing();
        }
        if kind == ViewKind::HeaderItem {
            node.apply_z_index()?;
        }
        if kind == ViewKind::Modal {
            node.ensure_wrapper()?;
        }
        Ok(node)
    }
    /// Whether it is leaving (an exit transition, a popped screen).
    pub(crate) fn is_leaving(&self) -> bool {
        self.leaving.get()
    }
    /// Marks this node leaving or not, keeping the count of leaving nodes
    /// the mounter skips its ancestor walks by. Returns whether it was.
    pub(crate) fn set_leaving(&self, leaving: bool) -> bool {
        let was = self.leaving.replace(leaving);
        if was != leaving {
            let count = self.leaving_nodes.get();
            debug_assert!(leaving || count > 0, "leaving count underflow");
            self.leaving_nodes.set(if leaving {
                count + 1
            } else {
                count.saturating_sub(1)
            });
        }
        was
    }
    pub(crate) fn self_ref(&self) -> Option<Rc<Self>> {
        self.weak.upgrade()
    }
    pub(crate) fn deactivate(&self) {
        if !self.active.replace(false) {
            return;
        }
        self.bridged_on_root_outer.set(false);
        self.paint_state.set(self.paint_state.get().reset());
        self.animated_opacity.set(None);
        callback(|| self.cursor.update(self, false));
        callback(|| self.selection_clear());
        self.invalidate_scene();
        self.accessibility_state.deactivate(self);
        #[cfg(feature = "api-23")]
        self.accessibility_provider.clear();
        if let Some(input) = self.input.borrow().as_ref() {
            callback(|| input.close());
        }
        if let Some(scroll) = self.scroll.borrow().as_ref() {
            self.scroll_watchers.remove(self.id);
            if let Some(plane) = scroll.overlay.borrow().as_ref() {
                callback(|| plane.close(self));
            }
        }
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            self.accessibility_wrappers
                .set(self.accessibility_wrappers.get().saturating_sub(1));
            callback(|| wrapper.close(self));
        }
        if let Some(web) = self.web.borrow().as_ref() {
            callback(|| web.close());
        }
        if let Some(video) = self.video.borrow().as_ref() {
            callback(|| video.close());
        }
        if let Some(route) = self.dispatcher.upgrade() {
            let mut keyboard = route.keyboard.lock().unwrap();
            if keyboard.focused == Some(self.id) {
                keyboard.focused = None;
            }
            if keyboard.input == Some(self.id) {
                keyboard.input = None;
            }
        }
        if let Some(component) = self.component.borrow().as_ref() {
            component.deactivate();
        }
    }
    pub(crate) fn child_container(&self) -> NativeNode {
        self.scroll
            .borrow()
            .as_ref()
            .map_or_else(|| self.native.clone(), |scroll| scroll.content.clone())
    }
    pub(crate) fn child_index(&self, index: usize) -> usize {
        let children = self.children.borrow();
        let index = index.min(children.len());
        let container = self.child_container();
        let native = container.borrow();
        // Retained removed nodes and platform overlays are not logical siblings.
        // Locate a real sibling instead of counting assumed native child slots.
        for child in children[index..].iter().filter_map(Weak::upgrade) {
            if let Some(at) = native
                .children()
                .iter()
                .position(|entry| Rc::ptr_eq(entry, &child.mounted()))
            {
                return at;
            }
        }
        for child in children[..index].iter().rev().filter_map(Weak::upgrade) {
            if let Some(at) = native
                .children()
                .iter()
                .position(|entry| Rc::ptr_eq(entry, &child.mounted()))
            {
                return at + 1;
            }
        }
        let anchors = [
            self.component
                .borrow()
                .as_ref()
                .map(|component| component.native.clone()),
            self.screen
                .borrow()
                .as_ref()
                .map(|screen| screen.header().clone()),
            self.web.borrow().as_ref().map(|web| web.native().clone()),
            self.video
                .borrow()
                .as_ref()
                .filter(|video| !video.is_fullscreen())
                .map(|video| video.native().clone()),
            self.shaders.native(self.id),
        ];
        anchors
            .iter()
            .flatten()
            .filter_map(|anchor| {
                native
                    .children()
                    .iter()
                    .position(|child| Rc::ptr_eq(child, anchor))
            })
            .max()
            .map_or(0, |index| index + 1)
    }
    fn detach_component(&self, component: &crate::interop::component::Instance) -> Result<()> {
        let index = self
            .native
            .borrow()
            .children()
            .iter()
            .position(|child| Rc::ptr_eq(child, &component.native));
        if let Some(index) = index {
            self.native.borrow_mut().remove_child(index)?;
        }
        Ok(())
    }
    pub(crate) fn attach_component(
        &self,
        component: crate::interop::component::Instance,
    ) -> Result<()> {
        if let Some(old) = self.component.borrow_mut().take() {
            self.detach_component(&old)?;
            old.dispose()?;
        }
        let frame = self.frame.get();
        component.native.borrow().width(frame.width)?;
        component.native.borrow().height(frame.height)?;
        self.native
            .borrow_mut()
            .insert_child(component.native.clone(), 0)?;
        *self.component.borrow_mut() = Some(component);
        self.native
            .borrow()
            .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
        Ok(())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.set_leaving(false);
        // Removed children still belong to their engine slots. Release their
        // native edges before disposing this parent's platform-owned children.
        for child in self
            .retained_children
            .take()
            .into_iter()
            .filter_map(|child| child.upgrade())
        {
            if std::ptr::eq(child.retained_owner.borrow().as_ptr(), self) {
                callback(|| {
                    let mounted = child.mounted();
                    if let Some(parent) = child.mount_parent.borrow().upgrade() {
                        let index = parent
                            .borrow()
                            .children()
                            .iter()
                            .position(|node| Rc::ptr_eq(node, &mounted));
                        if let Some(index) = index {
                            parent.borrow_mut().remove_child(index)?;
                        }
                    }
                    *child.mount_parent.borrow_mut() = Weak::new();
                    *child.retained_owner.borrow_mut() = Weak::new();
                    Ok(())
                });
            }
        }
        self.shadow_layer.borrow_mut().take();
        // Struct fields otherwise outlive this Drop body. Release controller
        // handles before recursively disposing their mounted native descendants.
        #[cfg(feature = "api-23")]
        self.accessibility_provider.clear();
        if let Some(input) = self.input.borrow_mut().take() {
            callback(|| input.close());
        }
        if let Some(web) = self.web.borrow_mut().take() {
            callback(|| web.close());
            let native = web.native().clone();
            drop(web);
            if native.borrow().is_owned() {
                callback(|| {
                    let index = self
                        .native
                        .borrow()
                        .children()
                        .iter()
                        .position(|child| Rc::ptr_eq(child, &native));
                    if let Some(index) = index {
                        if native.borrow().parent()?.is_some() {
                            self.native.borrow_mut().remove_child(index)?;
                        } else {
                            self.native.borrow_mut().children_mut().remove(index);
                        }
                    } else {
                        native.borrow().remove_from_parent()?;
                    }
                    if Rc::strong_count(&native) == 1 {
                        native.borrow_mut().dispose()?;
                    }
                    Ok(())
                });
            }
        }
        if let Some(video) = self.video.borrow_mut().take() {
            callback(|| video.close());
        }
        callback(|| self.interaction.dispose(self));
        if let Some(scroll) = self.scroll.borrow_mut().take() {
            callback(|| scroll.dispose());
            if let Some(plane) = scroll.overlay.borrow_mut().take() {
                callback(|| plane.close(self));
                // Its outer node remains in self.native; dropping Plane
                // releases its duplicate handle and its inner wrapper handle.
            }
            let wrapper = scroll.wrapper.clone();
            drop(scroll);
            // Texture export replaces the wrapper in the mounted tree. Once
            // the attachment releases it, this detached tree needs its own disposal.
            if wrapper.borrow().is_owned() && Rc::strong_count(&wrapper) == 1 {
                callback(|| {
                    wrapper.borrow().remove_from_parent()?;
                    wrapper.borrow_mut().dispose()?;
                    Ok(())
                });
            }
        }
        self.screen.borrow_mut().take();
        self.image.borrow_mut().take();
        if let Some(modal) = self.modal.borrow_mut().take() {
            callback(|| modal.close(false));
        }
        if let Some(component) = self.component.borrow_mut().take() {
            callback(|| {
                self.detach_component(&component)?;
                component.dispose()
            });
        }
        if let Some(wrapper) = self.wrapper.borrow_mut().take() {
            callback(|| wrapper.close(self));
            if wrapper.outer.borrow().is_owned() && Rc::strong_count(&wrapper.outer) == 1 {
                callback(|| {
                    wrapper.outer.borrow().remove_from_parent()?;
                    wrapper.outer.borrow_mut().dispose()?;
                    Ok(())
                });
            }
        }
        // A retiring Video retains this parent until its worker acknowledges
        // release. That UI owner disposes the parent after detaching its child.
        if Rc::strong_count(&self.native) == 1
            && let Err(error) = self.native.borrow_mut().dispose()
        {
            log::error!("OHOS dispose: {error}");
        }
    }
}
