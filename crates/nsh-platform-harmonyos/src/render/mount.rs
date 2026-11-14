//! Applies each engine commit on the ArkUI thread. Deleted boxes stay alive
//! until a native frame and a subsequent event-loop turn have completed.

#[cfg(feature = "dev")]
mod testing;

use std::{collections::VecDeque, rc::Rc, sync::Arc};

use nsh_core::{Command, MountBatch, MountItem, NodeId, ViewKind};
use nsh_engine::EngineControl;
use ohos_arkui_binding::component::attribute::ArkUICommonAttribute;

use crate::{
    render::{
        geometry::{Rect, Size},
        node::{Error, Node, Result},
    },
    text::typography::NativeText,
};

struct MountTarget {
    parent: std::rc::Weak<std::cell::RefCell<ohos_arkui_binding::common::node::ArkUINode>>,
    attached: bool,
}
impl MountTarget {
    fn mount(&mut self, root: crate::render::node::NativeNode) -> Result<()> {
        if let Some(parent) = self.parent.upgrade() {
            parent.borrow_mut().insert_child(root, 0)?;
            self.attached = true;
        }
        Ok(())
    }
    fn unmount(&mut self) -> Result<()> {
        if self.attached {
            if let Some(parent) = self.parent.upgrade() {
                parent.borrow_mut().remove_child(0)?;
            }
            self.attached = false;
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct Theme {
    pub(crate) tokens: Option<Arc<nsh_core::element::Tokens>>,
}
impl Theme {
    pub(crate) fn color(&self, name: &str, dark: bool) -> u32 {
        let names: &[&str] = match name {
            "popover" => &["popover", "card", "background"],
            "popover-foreground" => &["popover-foreground", "card-foreground", "foreground"],
            "card" => &["card", "background"],
            _ => &[name],
        };
        if let Some(value) = self
            .tokens
            .as_ref()
            .and_then(|tokens| names.iter().find_map(|name| tokens.color(name)))
        {
            return crate::text::typography::argb(
                if dark { value.dark } else { value.light }.into(),
            );
        }
        match (name, dark) {
            ("foreground" | "popover-foreground" | "primary", true) => 0xfffafafa,
            ("foreground" | "popover-foreground" | "primary", false) => 0xff171717,
            ("border", true) => 0xff404040,
            ("border", false) => 0xffe5e5e5,
            ("destructive", true) => 0xfff87171,
            ("destructive", false) => 0xffdc2626,
            ("card" | "popover", true) => 0xff171717,
            (_, true) => 0xff0a0a0a,
            (_, false) => 0xffffffff,
        }
    }
}

pub(crate) struct UiContext {
    pub(crate) viewport:
        std::rc::Weak<std::cell::RefCell<ohos_arkui_binding::common::node::ArkUINode>>,
    pub(crate) theme: std::cell::RefCell<Theme>,
    pub(crate) cursor: crate::input::cursor::State,
    pub(crate) shaders: super::shader::Shaders,
    /// The video whose native Stack is temporarily the root's last child.
    pub(crate) fullscreen_video: std::cell::RefCell<std::rc::Weak<Node>>,
    pub(crate) scroll_reports: std::cell::RefCell<Vec<std::rc::Weak<Node>>>,
    pub(crate) scroll_watchers: crate::scroll::watch::ScrollWatchers,
    pub(crate) flushing_batch: std::cell::Cell<bool>,
    pub(crate) scene: std::cell::RefCell<Option<Rc<crate::render::composition::scene::Scene>>>,
    pub(crate) compositor:
        std::cell::RefCell<Option<Rc<crate::render::composition::worker::Worker>>>,
    pub(crate) composite_nodes: std::cell::RefCell<Vec<std::rc::Weak<Node>>>,
    pub(crate) selected_text: std::cell::RefCell<Option<std::rc::Weak<Node>>>,
    pub(crate) accessibility_dirty: std::cell::RefCell<Vec<std::rc::Weak<Node>>>,
    pub(crate) accessibility_focus: std::cell::RefCell<Vec<std::rc::Weak<Node>>>,
    pub(crate) accessibility_live_regions: std::cell::Cell<usize>,
    pub(crate) accessibility_wrappers: std::cell::Cell<usize>,
    /// How many nodes are leaving (exit transitions, popped screens): while
    /// none is, no mount item needs the walk up its ancestors that looks
    /// for one ([`Node::set_leaving`]).
    pub(crate) leaving_nodes: std::cell::Cell<usize>,
    pub(crate) arkui: std::cell::RefCell<crate::interop::sdk::Batch>,
    pub(crate) arkui_views: std::cell::RefCell<Vec<crate::interop::attachment::NativeAttachment>>,
    pub(crate) host: Arc<crate::app::session::Services>,
    pub(crate) dispatcher: std::sync::Weak<crate::app::runtime::Dispatcher>,
    pub(crate) env: napi_ohos::Env,
    pub(crate) scale: std::cell::Cell<f32>,
    pub(crate) text_scale: Arc<std::sync::atomic::AtomicU32>,
    pub(crate) control: EngineControl,
    pub(crate) animator: Rc<crate::render::animation::Animator>,
    pub(crate) images: Arc<crate::media::image::Loader>,
    pub(crate) access: Arc<nsh_engine::Access>,
    pub(crate) assets: Arc<dyn crate::services::assets::StreamAssets>,
    pub(crate) components: crate::interop::component::Components,
    pub(crate) touches: crate::input::touch::State,
    pub(crate) fonts: Arc<std::sync::Mutex<crate::text::fonts::Fonts>>,
    pub(crate) canvas_bytes: Rc<std::cell::Cell<usize>>,
    pub(crate) platform: std::cell::Cell<nsh_engine::Environment>,
    pub(crate) appearance: std::cell::Cell<nsh_core::platform::Appearance>,
}

pub(crate) struct Mounter {
    context: Rc<UiContext>,
    mount: MountTarget,

    slots: Vec<Option<Rc<Node>>>,
    // Sorted like slots: back priority must not depend on recycled IDs' age.
    stacks: Vec<NodeId>,
    removed: Vec<Rc<Node>>,
    retired: VecDeque<(u64, Vec<Rc<Node>>)>,
    epoch: u64,
    /// Batches applied, from 1: what the mount log counts by.
    batches: u64,
    modals: Vec<std::rc::Weak<crate::navigation::modal::Modal>>,
    pending_delete: Vec<NodeId>,
    deferred: Vec<MountItem>,
    dialogs: crate::services::dialogs::Dialogs,
}

impl std::ops::Deref for Mounter {
    type Target = UiContext;
    fn deref(&self) -> &UiContext {
        &self.context
    }
}

impl Mounter {
    pub(crate) fn scene_ready(&self) -> Result<()> {
        if let Some(scene) = self.scene.borrow().as_ref() {
            scene.ready()?;
        }
        self.flush_arkui()
    }
    pub(crate) fn composite_ready(&self, id: NodeId, generation: u32) -> Result<()> {
        if let Some(worker) = self.compositor.borrow().as_ref() {
            worker.collect();
        }
        if let Some(Some(node)) = self.slots.get(id as usize) {
            node.composite_ready(generation)?;
            if id == nsh_core::ROOT_ID {
                self.shaders.reveal()?;
            } else if let Some(Some(root)) = self.slots.get(nsh_core::ROOT_ID as usize)
                && root.composite_dirty.get()
            {
                Self::attribute(root.update_wrapper());
            }
            if let Some(modal) = node.modal.borrow().as_ref() {
                modal.refresh_presentation()?;
            }
        }
        self.flush_arkui()
    }
    pub(crate) fn navigation_event(
        &self,
        event: crate::render::shader::navigation::Event,
    ) -> Result<()> {
        self.shaders.navigation_event(event)?;
        self.reconcile_navigation_capture()?;
        self.flush_arkui()?;
        self.animator.request_frame();
        Ok(())
    }
    pub(crate) fn text_selection(
        &self,
        id: NodeId,
        command: crate::text::selection::controller::Command,
    ) -> Result<()> {
        let selected = self
            .selected_text
            .borrow()
            .as_ref()
            .and_then(std::rc::Weak::upgrade);
        if let Some(node) = selected.filter(|node| node.id == id) {
            node.selection_command(command)?;
        }
        Ok(())
    }
    #[cfg(feature = "api-23")]
    pub(crate) fn accessibility_event(
        &self,
        id: NodeId,
        event: ohos_accessibility_binding::EventType,
    ) -> Result<()> {
        // A callback queued before removal cannot reach a retired node.
        if let Some(Some(node)) = self.slots.get(id as usize) {
            node.accessibility_provider.send_event(event)?;
        }
        Ok(())
    }

    pub(crate) fn environment(&self, environment: nsh_engine::Environment) -> Result<()> {
        let density_changed =
            self.scale.replace(environment.pixel_ratio) != environment.pixel_ratio;
        let reduce_motion_changed = self.platform.get().reduce_motion != environment.reduce_motion;
        let was_background = self.platform.get().lifecycle == nsh_engine::Lifecycle::Background;
        self.platform.set(environment);
        for node in self.slots.iter().flatten() {
            if density_changed {
                node.density_changed()?;
            }
            if let Some(screen) = node.screen.borrow().as_ref() {
                screen.update_environment()?;
            }
            if node.modal.borrow().is_some() {
                node.native
                    .borrow()
                    .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedMeasure)?;
            }
        }
        let background = environment.lifecycle == nsh_engine::Lifecycle::Background;
        if background {
            self.shaders.cancel_navigation("background")?;
        }
        if reduce_motion_changed || background || was_background {
            self.reconcile_navigation_capture()?;
            self.flush_arkui()?;
        }
        if density_changed {
            if let Some(scene) = self.scene.borrow().as_ref() {
                scene.invalidate();
            }
            self.flush_arkui()?;
            self.animator.request_frame();
        }
        self.system_appearance()
    }
    fn system_appearance(&self) -> Result<()> {
        let dark = self
            .appearance
            .get()
            .resolve(self.platform.get().color_scheme)
            == nsh_engine::ColorScheme::Dark;
        let config = self.dispatcher.upgrade().and_then(|route| {
            let status = *route.status_bar.lock().unwrap();
            status
                .filter(|(_, previous)| *previous != dark)
                .map(|(config, _)| config)
        });
        if let Some(config) = config {
            self.host
                .status_bar(self.dispatcher.clone(), config, dark)?;
        }
        Ok(())
    }
    pub(crate) fn native_context(&self, ready: bool) -> Result<()> {
        if !ready {
            self.shaders.cancel_navigation("context-lost")?;
            let mut pending = self.arkui.borrow_mut();
            pending.editors.clear();
            pending.commands.clear();
            pending.views.clear();
            pending.webs.clear();
            pending.web_commands.clear();
            // Retirements keep their targets until SDK cleanup or the next ready batch.
        }
        for node in self.slots.iter().flatten() {
            if !ready {
                if let Some(wrapper) = node.wrapper.borrow().as_ref() {
                    wrapper.invalidate_context();
                }
                if let Some(scroll) = node.scroll.borrow().as_ref()
                    && let Some(plane) = scroll.overlay.borrow().as_ref()
                {
                    plane.invalidate_context();
                }
            }
            if ready {
                node.update_wrapper()?;
            }
            if let Some(input) = node.input.borrow().as_ref() {
                input.context(ready);
            }
            if let Some(web) = node.web.borrow().as_ref() {
                web.context(ready)?;
            }
            if let Some(video) = node.video.borrow().as_ref() {
                video.context(ready)?;
            }
        }
        self.shaders.reconcile_hosts_now()?;
        if ready {
            self.flush_arkui()?;
        }
        Ok(())
    }
    pub(crate) fn cancel_input(&self) -> Result<()> {
        if let Ok(root) = self.node(nsh_core::ROOT_ID) {
            self.touches.borrow_mut().cancel(root)?;
        }
        for node in self.slots.iter().flatten() {
            if let Some(input) = node.input.borrow().as_ref() {
                input.suspend()?;
            }
            if let Some(scroll) = node.scroll.borrow().as_ref() {
                scroll.block(false)?;
            }
        }
        Ok(())
    }
    pub(crate) fn flush_scroll_layout(&self, id: NodeId) -> Result<()> {
        if let Ok(node) = self.node(id)
            && let Some(scroll) = node.scroll.borrow().as_ref()
        {
            scroll.flush_layout()?;
        }
        Ok(())
    }
    pub(crate) fn wants_back(&self) -> bool {
        self.fullscreen_video.borrow().strong_count() > 0
            || self.dialogs.visible()
            || self
                .modals
                .iter()
                .filter_map(std::rc::Weak::upgrade)
                .any(|modal| modal.is_visible())
            || self.back_stack(None).is_some()
            || self
                .slots
                .get(1)
                .and_then(Option::as_ref)
                .is_some_and(|root| {
                    root.interaction
                        .mask()
                        .contains(nsh_core::EventKind::BackPress)
                })
    }
    pub(crate) fn back(&mut self) -> Result<()> {
        let fullscreen = self.fullscreen_video.borrow().upgrade();
        if let Some(owner) = fullscreen
            && let Some(video) = owner.video.borrow().as_ref()
        {
            return video.set_fullscreen(false);
        }
        if self.dialogs.visible() {
            return self.dialogs.request_close();
        }
        self.route_back(true)
    }
    pub(crate) fn dismiss_keyboard(&self) -> Result<()> {
        self.node(nsh_core::ROOT_ID)?.dismiss_keyboard()
    }
    fn back_fallback(&self) -> Result<()> {
        self.route_back(false)
    }
    fn route_back(&self, app_listener: bool) -> Result<()> {
        use nsh_core::navigation::{Back, back_target};
        let modal = self
            .modals
            .iter()
            .rev()
            .filter_map(std::rc::Weak::upgrade)
            .find(|modal| modal.is_visible());
        let target = back_target(
            modal.as_ref().and_then(|modal| modal.node_id()),
            app_listener
                && self
                    .node(nsh_core::ROOT_ID)?
                    .interaction
                    .mask()
                    .contains(nsh_core::EventKind::BackPress),
            |scope| self.back_stack(scope).map(|(id, _)| id),
        );
        match target {
            Some(Back::Stack(id)) => {
                if let Some((_, screen)) = self.stack_candidate(id) {
                    screen.request_back()?;
                }
            }
            Some(Back::Modal(_)) => {
                if let Some(modal) = modal {
                    modal.request_close()?;
                }
            }
            Some(Back::App) => self.control.dispatch_event(
                nsh_core::ROOT_ID,
                nsh_core::EventKind::BackPress,
                nsh_core::EventPayload::None,
            ),
            None => {}
        }
        Ok(())
    }
    fn contains(&self, ancestor: NodeId, mut child: NodeId) -> bool {
        while let Ok(node) = self.node(child) {
            if child == ancestor {
                return true;
            }
            let Some(parent) = node.parent.get() else {
                break;
            };
            child = parent;
        }
        false
    }
    fn stack_candidate(
        &self,
        id: NodeId,
    ) -> Option<(NodeId, Rc<crate::navigation::stack::Screen>)> {
        let stack = self.node(id).ok().filter(|node| node.visible_in_tree())?;
        let screens: Vec<_> = stack
            .children
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .filter_map(|node| node.screen.borrow().clone())
            .filter(|screen| !screen.popped.get())
            .collect();
        let top = screens.last()?;
        (screens.len() > 1 || top.config.borrow().prevent_back).then(|| (id, top.clone()))
    }
    fn back_stack(
        &self,
        scope: Option<NodeId>,
    ) -> Option<(NodeId, Rc<crate::navigation::stack::Screen>)> {
        let candidates: Vec<_> = self
            .stacks
            .iter()
            .copied()
            .filter(|id| scope.is_none_or(|scope| self.contains(scope, *id)))
            .filter_map(|id| self.stack_candidate(id))
            .collect();
        nsh_core::navigation::innermost(&candidates, |(outer, _), (inner, _)| {
            self.contains(*outer, *inner)
        })
        .cloned()
    }
    pub(crate) fn reconcile_navigation(&mut self) -> Result<()> {
        self.shaders.reconcile_hosts_now()?;
        crate::navigation::stack::reconcile(
            self.stacks
                .iter()
                .filter_map(|id| self.slots[*id as usize].as_ref()),
        )?;
        for modal in self.modals.iter().filter_map(std::rc::Weak::upgrade) {
            modal.present_if_pending()?;
        }
        self.reconcile_navigation_capture()
    }
    fn reconcile_navigation_capture(&self) -> Result<()> {
        self.shaders
            .reconcile_navigation_capture(self.platform.get().reduce_motion)
    }
    pub(crate) fn dialog_result(&mut self, id: u32, index: i32) -> Result<()> {
        self.dialogs.finish(id, index)
    }
    pub(crate) fn window_environment(
        &self,
        visual: [crate::app::session::AvoidArea; 3],
        keyboard: crate::app::session::AvoidArea,
    ) -> Result<crate::app::environment::NshEnvironment> {
        let root = self.node(1)?;
        let Some(viewport) = self.mount.parent.upgrade() else {
            return Ok(crate::app::environment::NshEnvironment::default());
        };
        // The external viewport has already been laid out by the host. A newly
        // attached texture wrapper may not have propagated its window origin yet.
        let origin = viewport.borrow().layout_position_in_window()?;
        let frame = root.frame.get();
        let scale = self.scale.get() as f64;
        let viewport = super::geometry::WindowRect {
            x: origin.x as f64,
            y: origin.y as f64,
            width: frame.width as f64 * scale,
            height: frame.height as f64 * scale,
        };
        let rect = |rect: crate::app::session::AvoidRect| super::geometry::WindowRect {
            x: rect.left as f64,
            y: rect.top as f64,
            width: rect.width as f64,
            height: rect.height as f64,
        };
        let [top, right, bottom, left] = super::geometry::visual_insets(
            viewport,
            visual.into_iter().filter(|area| area.visible).map(|area| {
                [
                    rect(area.top_rect),
                    rect(area.right_rect),
                    rect(area.bottom_rect),
                    rect(area.left_rect),
                ]
            }),
        );
        Ok(crate::app::environment::NshEnvironment {
            safe_top: Some(top / scale),
            safe_right: Some(right / scale),
            safe_bottom: Some(bottom / scale),
            safe_left: Some(left / scale),
            keyboard_height: Some(if keyboard.visible {
                viewport
                    .intersection(rect(keyboard.bottom_rect))
                    .map_or(0.0, |area| area.height / scale)
            } else {
                0.0
            }),
            ..crate::app::environment::NshEnvironment::default()
        })
    }
    pub(crate) fn new(
        parent: crate::render::node::NativeNode,
        size: Size,
        context: Rc<UiContext>,
    ) -> Result<Self> {
        let mut mount = MountTarget {
            parent: Rc::downgrade(&parent),
            attached: false,
        };
        let root = Node::new(1, ViewKind::View, context.clone())?;
        root.set_frame(Rect {
            x: 0.0,
            y: 0.0,
            width: size.width,
            height: size.height,
        })?;
        root.ensure_wrapper()?;
        mount.mount(root.mounted())?;
        *root.mount_parent.borrow_mut() = mount.parent.clone();
        root.set_events(nsh_core::EventMask::default())?;
        context.shaders.root(&root);
        *context.scene.borrow_mut() = Some(Rc::new(crate::render::composition::scene::Scene::new(
            &root,
        )));
        Ok(Self {
            dialogs: crate::services::dialogs::Dialogs::new(context.clone()),
            context,
            mount,
            slots: vec![None, Some(root)],
            stacks: Vec::new(),
            removed: Vec::new(),
            retired: VecDeque::new(),
            epoch: 0,
            batches: 0,
            modals: Vec::new(),
            pending_delete: Vec::new(),
            deferred: Vec::new(),
        })
    }
    fn node(&self, id: NodeId) -> Result<&Rc<Node>> {
        self.slots
            .get(id as usize)
            .and_then(Option::as_ref)
            .ok_or(Error::InvalidNode(id))
    }

    fn detach(&mut self, id: NodeId) -> Result<()> {
        self.unlink(id, false)
    }

    fn unlink(&mut self, id: NodeId, retain: bool) -> Result<()> {
        if let Some(root) = self
            .slots
            .get(nsh_core::ROOT_ID as usize)
            .and_then(Option::as_ref)
        {
            Self::attribute(root.release_root_bridge_for(id));
        }
        let node = self.node(id)?;
        let parent = node.parent.get().map(|id| self.node(id)).transpose()?;
        let retain = retain && node.modal.borrow().is_none() && parent.is_some();
        if let Some(modal) = node.modal.borrow().as_ref() {
            Self::attribute(modal.close(true));
        } else if !retain && let Some(container) = node.mount_parent.borrow().upgrade() {
            let mounted = node.mounted();
            let index = container
                .borrow()
                .children()
                .iter()
                .position(|entry| Rc::ptr_eq(entry, &mounted))
                .ok_or(Error::InvalidNode(id))?;
            container
                .borrow_mut()
                .remove_child(index)
                .map_err(|error| Error::Tree(Box::new(error.into())))?;
        }
        if let Some(parent) = parent {
            let index = parent
                .children
                .borrow()
                .iter()
                .position(|entry| entry.upgrade().is_some_and(|child| child.id == id))
                .ok_or(Error::InvalidNode(id))?;
            parent.children.borrow_mut().remove(index);
            parent.invalidate_composites();
            if retain {
                // Keep a removed-but-live FrameNode reachable for the next export
                // reconciliation. Visibility None removes it from layout and RS.
                parent.retained_children.borrow_mut().retain(|entry| {
                    entry.upgrade().is_some_and(|child| {
                        child.retained.get()
                            && child.retained_owner.borrow().ptr_eq(&Rc::downgrade(parent))
                    })
                });
                parent
                    .retained_children
                    .borrow_mut()
                    .push(Rc::downgrade(node));
                *node.retained_owner.borrow_mut() = Rc::downgrade(parent);
                node.retained.set(true);
                Self::attribute(node.restore_style());
            }
            if parent.children.borrow().is_empty() && !parent.clips_children() {
                Self::attribute(parent.update_layout_bounds());
            }
            parent.accessibility_state.changed(parent);
        }
        if !retain {
            *node.mount_parent.borrow_mut() = std::rc::Weak::new();
            *node.retained_owner.borrow_mut() = std::rc::Weak::new();
        }
        node.parent.set(None);
        *node.parent_node.borrow_mut() = std::rc::Weak::new();
        node.invalidate_scene();
        Ok(())
    }

    /// Whether `id` or an ancestor is leaving or a closing modal: its
    /// content keeps what it showed until it is gone. One walk up for both,
    /// none while nothing leaves and no modal is open.
    fn frozen(&self, mut id: NodeId) -> bool {
        let leaving = self.leaving_nodes.get() > 0;
        let modals = self.modals.iter().any(|modal| modal.strong_count() > 0);
        if !leaving && !modals {
            return false;
        }
        loop {
            let Ok(node) = self.node(id) else {
                return false;
            };
            if (leaving && node.is_leaving())
                || (modals
                    && node
                        .modal
                        .borrow()
                        .as_ref()
                        .is_some_and(|modal| modal.closing()))
            {
                return true;
            }
            let Some(parent) = node.parent.get() else {
                return false;
            };
            id = parent;
        }
    }
    fn delete(&mut self, id: NodeId) -> Result<()> {
        if let Some(modal) = self.node(id)?.modal.borrow().as_ref() {
            Self::attribute(modal.close(true));
        }
        if self.frozen(id) {
            if !self.node(id)?.pending_delete.replace(true) {
                // Both exits still render their retained subtree. Deactivation
                // closes texture wrappers, so it belongs to delete_now after
                // the page/dialog transition has finished.
                self.pending_delete.push(id);
            }
            return Ok(());
        }
        self.delete_now(id)
    }
    fn orphan_subtree(&mut self, id: NodeId) -> Result<()> {
        self.detach(id)?;
        // Children can be deleted later in the batch (or when QuickJS collects a
        // reference cycle). Sever every native edge and release native attachments
        // across the orphaned subtree before any endpoint is eligible for disposal.
        loop {
            let child = self
                .node(id)?
                .children
                .borrow()
                .last()
                .and_then(|n| n.upgrade());
            let Some(child) = child else { break };
            self.orphan_subtree(child.id)?;
        }
        if let Ok(node) = self.node(id) {
            self.animator.remove(id);
            self.shaders.remove(id);
            *node.navigation_shader.borrow_mut() = None;
            node.deactivate();
            *node.text_drawing.borrow_mut() = None;
            *node.raster.borrow_mut() = None;
        }
        Ok(())
    }
    fn delete_now(&mut self, id: NodeId) -> Result<()> {
        self.orphan_subtree(id)?;
        self.shaders.remove(id);
        if let Some(node) = self.slots[id as usize].take() {
            if node.kind == ViewKind::Stack
                && let Ok(index) = self.stacks.binary_search(&id)
            {
                self.stacks.remove(index);
            }
            self.removed.push(node);
        }
        while self.slots.last().is_some_and(Option::is_none) {
            self.slots.pop();
        }
        Ok(())
    }

    fn deferred_node(item: &MountItem) -> Option<NodeId> {
        match item {
            MountItem::ViewStyle { id, .. }
            | MountItem::Frame { id, .. }
            | MountItem::Text { id, .. }
            | MountItem::ScreenConfig { id, .. }
            | MountItem::ModalConfig { id, .. }
            | MountItem::Shader { id, .. }
            | MountItem::ShaderTransition { id, .. }
            | MountItem::NavShader { id, .. }
            | MountItem::Remove { id } => Some(*id),
            _ => None,
        }
    }
    fn finish_deferred(&mut self) -> Result<()> {
        let mut ready = Vec::new();
        for item in std::mem::take(&mut self.deferred) {
            let id = Self::deferred_node(&item).expect("deferred view update");
            if let Ok(node) = self.node(id) {
                if node.pending_delete.get() {
                    continue;
                }
                if self.frozen(id) {
                    self.deferred.push(item);
                } else {
                    ready.push(item);
                }
            }
        }
        if !ready.is_empty() {
            self.apply(MountBatch { items: ready })?;
        }
        Ok(())
    }

    fn attribute(result: Result<()>) {
        if let Err(error) = result {
            log::warn!(target: "nsh::mount", "native attribute skipped: {error}");
        }
    }
    pub(crate) fn apply(&mut self, batch: MountBatch) -> Result<()> {
        struct BatchGuard(Rc<UiContext>, bool);
        impl Drop for BatchGuard {
            fn drop(&mut self) {
                self.0.flushing_batch.set(self.1);
            }
        }
        let flushing = self.flushing_batch.replace(true);
        let _batch_guard = BatchGuard(self.context.clone(), flushing);
        self.finish_deferred()?;
        if let Some(scene) = self.scene.borrow().as_ref() {
            scene.invalidate();
        }
        let crate::render::transition::BatchPrepass {
            deletions,
            deleted,
            removals,
            removed,
            mut motion,
        } = crate::render::transition::BatchMotion::scan(&batch.items);
        // Start a screen's exit before its descendants' delete operations. An
        // enclosing tree deletion retires immediately instead of animating orphans.
        for id in &deletions {
            let Ok(node) = self.node(*id) else {
                continue;
            };
            let Some(screen) = node.screen.borrow().as_ref().cloned() else {
                continue;
            };
            let mut ancestor = node.parent_node.borrow().upgrade();
            let mut enclosing_delete = false;
            while let Some(parent) = ancestor {
                if deleted.contains(&parent.id) {
                    enclosing_delete = true;
                    break;
                }
                ancestor = parent.parent_node.borrow().upgrade();
            }
            let animated = !enclosing_delete
                && screen
                    .previous()
                    .is_some_and(|previous| !deleted.contains(&previous.id));
            Self::attribute(screen.prepare_removal(animated));
            if animated {
                Self::attribute(screen.remove().map(|_| ()));
            }
        }
        for id in &deletions {
            if let Ok(node) = self.node(*id)
                && let Some(modal) = node.modal.borrow().as_ref()
            {
                Self::attribute(modal.close(true));
            }
        }
        for (id, animation) in removals {
            if !animation.is_some_and(|animation| animation.delete.is_some()) {
                continue;
            }
            let Ok(node) = self.node(id) else { continue };
            let mut parent = node.parent_node.borrow().upgrade();
            let mut enclosed = false;
            while let Some(ancestor) = parent {
                if removed.contains(&ancestor.id) {
                    enclosed = true;
                    break;
                }
                parent = ancestor.parent_node.borrow().upgrade();
            }
            if !enclosed {
                Self::attribute(node.removing(animation.as_ref()).map(|_| ()));
            }
        }
        // Each frame uses the most recent marker. Batch-local state also keeps
        // an unclosed scope from leaking into a later commit or deferred replay.
        let mut nav_shader_configured = false;
        self.batches += 1;
        let items = batch.items.len();
        for (at, item) in batch.items.into_iter().enumerate() {
            if Self::deferred_node(&item)
                .is_some_and(|id| motion.allows_defer(id) && self.frozen(id))
            {
                self.deferred.push(item);
                continue;
            }

            let creating_root = matches!(
                item,
                MountItem::Create {
                    id: nsh_core::ROOT_ID,
                    ..
                }
            );
            let result = (|| -> Result<()> {
                match item {
                    MountItem::Component { id, name } => {
                        if let Some(component) = self.components.make(
                            self.env,
                            &name,
                            id,
                            self.control.clone(),
                            &self.host,
                        )? {
                            let node = self.node(id)?;
                            node.component_animated.borrow_mut().clear();
                            node.attach_component(component)?;
                            self.animator.mark_dirty();
                        }
                    }
                    MountItem::ComponentPropSlots { id, names } => {
                        let node = self.node(id)?;
                        *node.component_slots.borrow_mut() = names;
                        node.component_animated.borrow_mut().clear();
                        self.animator.mark_dirty();
                    }
                    MountItem::Canvas { id, drawing } => {
                        if let Some(canvas) = self.node(id)?.canvas.borrow().as_ref() {
                            canvas.set(drawing)?;
                        }
                    }
                    MountItem::ScreenConfig { id, config } => {
                        if !self.frozen(id)
                            && let Some(screen) = self.node(id)?.screen.borrow().as_ref()
                        {
                            screen.configure(*config)?;
                        }
                    }
                    MountItem::NativeProps { id, props } => match props {
                        nsh_core::NativeProps::WebView(props) => {
                            if let Some(web) = self.node(id)?.web.borrow().as_ref() {
                                web.configure(*props)?;
                            }
                        }
                        nsh_core::NativeProps::Video(props) => {
                            if let Some(video) = self.node(id)?.video.borrow().as_ref() {
                                video.configure(props)?;
                            }
                        }
                    },
                    MountItem::ComponentProps { id, props } => {
                        if let Some(component) = self.node(id)?.component.borrow().as_ref() {
                            component.props(props)?;
                        }
                    }
                    MountItem::Create { id, kind } => {
                        self.slots
                            .resize_with(self.slots.len().max(id as usize + 1), || None);
                        if self.slots[id as usize].is_some() {
                            return Err(Error::Tree(Box::new(Error::InvalidNode(id))));
                        }
                        self.slots[id as usize] = Some(
                            Node::new(id, kind, self.context.clone())
                                .map_err(|error| Error::Tree(Box::new(error)))?,
                        );
                        motion.note_created(id);
                        if kind == ViewKind::Stack {
                            let index = self.stacks.partition_point(|stack| *stack < id);
                            self.stacks.insert(index, id);
                        }
                        let modal = self.node(id)?.modal.borrow().as_ref().map(Rc::downgrade);
                        if let Some(modal) = modal {
                            self.modals.push(modal);
                        }
                    }
                    MountItem::Insert {
                        parent,
                        child,
                        before,
                    } => {
                        self.detach(child).map_err(|error| match error {
                            Error::InvalidNode(_) => Error::Tree(Box::new(error)),
                            error => error,
                        })?;
                        let parent = self
                            .node(parent)
                            .map_err(|error| Error::Tree(Box::new(error)))?;
                        parent.enable_layout();
                        let node = self.slots[child as usize].as_ref().unwrap();
                        // A sibling not in the parent yet (a host retagged in this
                        // batch, inserted by its own later item) places the child
                        // last, as GTK's mounter does, rather than fail the batch.
                        let index = {
                            let children = parent.children.borrow();
                            before
                                .and_then(|before| {
                                    children
                                        .iter()
                                        .position(|n| n.upgrade().is_some_and(|n| n.id == before))
                                })
                                .unwrap_or(children.len())
                        };
                        if let Some(modal) = node.modal.borrow().as_ref() {
                            modal.schedule_present();
                        } else {
                            let container = parent.child_container();
                            let native_index = parent.child_index(index);
                            container
                                .borrow_mut()
                                .insert_child(node.mounted(), native_index)
                                .map_err(|error| Error::Tree(Box::new(error.into())))?;
                            // ArkUI insert/remove request measurement themselves.
                            // NeedMeasure adds SELF_AND_PARENT and would escape
                            // the engine-owned parent's measure boundary.
                            *node.mount_parent.borrow_mut() = Rc::downgrade(&container);
                        }
                        let was_empty = parent.children.borrow().is_empty();
                        parent
                            .children
                            .borrow_mut()
                            .insert(index, Rc::downgrade(node));
                        if was_empty && !parent.clips_children() {
                            Self::attribute(parent.update_layout_bounds());
                        }
                        parent.accessibility_state.changed(parent);
                        node.parent.set(Some(parent.id));
                        *node.parent_node.borrow_mut() = Rc::downgrade(parent);
                        node.invalidate_composites();
                        node.accessibility_state.changed(node);
                        Self::attribute(
                            node.interaction
                                .pointer(node, parent.interaction.blocks_children()),
                        );
                        if node.kind == ViewKind::HeaderItem
                            && let Some(screen) = parent.screen.borrow().as_ref()
                        {
                            Self::attribute(screen.update(screen.has_back()));
                        }
                        Self::attribute(node.inserted(motion.insert_animation()));
                        // This Insert supersedes a removal deferred while the node
                        // was leaving. Replay its other updates before newer ones.
                        self.deferred.retain(
                            |item| !matches!(item, MountItem::Remove { id } if *id == child),
                        );
                        self.finish_deferred()?;
                    }
                    MountItem::Remove { id } => {
                        if motion.is_created_in_batch(id) {
                            let node = self.node(id)?;
                            if node.live.borrow_mut().take().is_some() {
                                self.animator.remove(id);
                                Self::attribute(node.finish_removal());
                            }
                            self.unlink(id, true)?;
                        } else if !self.frozen(id)
                            && !self
                                .node(id)?
                                .removing(motion.remove_animation(id))
                                .unwrap_or_else(|error| {
                                    Self::attribute(Err(error));
                                    false
                                })
                        {
                            self.unlink(id, true)?;
                        }
                    }
                    MountItem::Delete { id } => {
                        if motion.note_deleted(id) {
                            if let Some(modal) = self.node(id)?.modal.borrow().as_ref() {
                                Self::attribute(modal.close(true));
                            }
                            self.delete_now(id)?;
                        } else {
                            if !self.frozen(id) {
                                Self::attribute(
                                    self.node(id)?
                                        .removing(motion.remove_animation(id))
                                        .map(|_| ()),
                                );
                            }
                            self.delete(id)?;
                        }
                    }
                    MountItem::Frame { id, frame } => {
                        if motion.is_created_in_batch(id) || !self.frozen(id) {
                            self.node(id)?
                                .animated_frame(frame.into(), motion.frame_animation(id))?;
                        }
                    }
                    MountItem::LayoutAnimation(animation) => motion.set_layout(animation),
                    MountItem::ViewStyle { id, style } => {
                        if motion.is_created_in_batch(id) || !self.frozen(id) {
                            self.node(id)?.style(&style)?;
                        }
                    }
                    MountItem::Text {
                        id,
                        layout,
                        content,
                        ..
                    } => {
                        if motion.is_created_in_batch(id) || !self.frozen(id) {
                            let node = self.node(id)?;
                            let raster = layout
                                .0
                                .downcast_ref::<NativeText>()
                                .ok_or(Error::Unsupported("foreign text layout"))?;
                            let shown: Arc<str> = Arc::from(raster.source.visible_text());
                            node.accessibility_state.text(node, &shown);
                            let links_changed =
                                node.text(raster, content.into(), layout.clone())?;
                            if links_changed {
                                node.set_events(node.interaction.mask())?;
                            }
                            node.interaction.text(node, &shown)?;
                        }
                    }
                    MountItem::Gestures { id, values } => self
                        .node(id)?
                        .interaction
                        .gestures(self.node(id)?, values)?,
                    MountItem::Animation(op) => self.animator.apply(op),
                    MountItem::AnimationBindings { id, bindings } => {
                        self.animator.bind(self.node(id)?, bindings)?
                    }
                    MountItem::Events { id, events } => self.node(id)?.set_events(events)?,
                    MountItem::Keymap(map) => {
                        if let Some(dispatcher) = self.dispatcher.upgrade() {
                            dispatcher.keyboard.lock().unwrap().map = map;
                        }
                    }
                    MountItem::ViewProps { id, props } => {
                        self.node(id)?.interaction.props(self.node(id)?, *props)?
                    }
                    MountItem::AccessibilityLabel { id, label } => {
                        self.node(id)?.interaction.label(self.node(id)?, label)?
                    }
                    // A host retagged in this batch (a component whose root is an
                    // input) can have its tracking ahead of its new `Create`, which
                    // sends it again: skip a view not made yet, as UIKit's and GTK's
                    // mounters do, rather than fail the batch.
                    MountItem::TrackStates { id, states } => {
                        if let Ok(node) = self.node(id) {
                            node.interaction.track(node, states)?;
                        }
                    }
                    MountItem::ModalConfig { id, config } => {
                        if !self.frozen(id)
                            && let Some(modal) = self.node(id)?.modal.borrow().as_ref()
                        {
                            modal.configure(config)?;
                        }
                    }
                    MountItem::Image {
                        id,
                        source,
                        resize_mode,
                        options,
                    } => {
                        if let Some(image) = self.node(id)?.image.borrow().as_ref() {
                            image.configure(source, resize_mode, options)?;
                        }
                    }
                    MountItem::Input { id, config } => {
                        if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                            input.configure(*config)?;
                        }
                    }
                    MountItem::InputValue { id, text, count } => {
                        if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                            input.value(text, count)?;
                        }
                    }
                    MountItem::InputContent { id, content } => {
                        if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                            input.content(content.into())?;
                        }
                    }
                    MountItem::Shader { id, layer } => {
                        if motion.is_created_in_batch(id) || !self.frozen(id) {
                            self.shaders.set(self.node(id)?, layer)?;
                        }
                    }
                    MountItem::ShaderTransition { id, duration } => {
                        if motion.is_created_in_batch(id) || !self.frozen(id) {
                            self.shaders.transition(id, duration);
                        }
                    }
                    // ArkUI's texture producer reports native content changes,
                    // including native input/scroll updates between core batches.
                    MountItem::ShaderInput { .. } => {}
                    MountItem::NavShader { id, shader } => {
                        if !self.frozen(id) {
                            let configured = shader.is_some();
                            let node = self.node(id)?;
                            if let Some(spec) = &shader
                                && let Ok(worker) =
                                    crate::render::composition::worker::Worker::for_owner(node)
                            {
                                worker.precompile(spec.layer.clone());
                            }
                            *node.navigation_shader.borrow_mut() = shader;
                            if !configured {
                                self.shaders.end_navigation(id)?;
                            } else {
                                nav_shader_configured = true;
                            }
                            node.ensure_wrapper()?;
                            node.restore_style()?;
                        }
                    }
                    // These projections are opt-in for the terminal backend.
                    MountItem::TreeOrder(_) | MountItem::ProjectStates { .. } => {}
                    MountItem::ScrollConfig {
                        id,
                        horizontal,
                        scrollbar_visible,
                        replaced,
                        scroll_value,
                        pull_value,
                        keyboard_dismiss,
                        mirrored,
                    } => {
                        if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                            scroll.config(
                                horizontal,
                                scrollbar_visible && !replaced,
                                scroll_value,
                                pull_value,
                                keyboard_dismiss,
                                mirrored,
                            )?;
                        }
                    }
                    MountItem::ScrollContent { id, size } => {
                        if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                            scroll.size(size.into())?;
                        }
                    }
                    MountItem::StickyCells { id, cells } => {
                        // A cell without a view yet is not pinned, as on UIKit and GTK.
                        let cells = cells
                            .into_iter()
                            .filter_map(|cell| {
                                let node = self.node(cell.cell).ok()?;
                                Some((cell, Rc::downgrade(node)))
                            })
                            .collect::<Vec<_>>();
                        if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                            scroll.sticky(cells)?;
                        }
                    }
                    MountItem::Refreshing { id, refreshing } => {
                        if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                            scroll.refreshing(refreshing)?;
                        }
                    }
                    MountItem::ScrollBehavior { id, behavior } => {
                        if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                            scroll.behavior(&behavior)?;
                        }
                    }
                    MountItem::ScrollBy { id, dx, dy } => {
                        if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                            scroll.by(dx.0, dy.0)?;
                        }
                    }
                    MountItem::Command { id, command } => match command {
                        Command::StatusBar(config) => self.host.status_bar(
                            self.dispatcher.clone(),
                            config,
                            self.appearance
                                .get()
                                .resolve(self.platform.get().color_scheme)
                                == nsh_engine::ColorScheme::Dark,
                        )?,
                        Command::Desktop(_) => {}
                        Command::Tokens(tokens) => {
                            self.theme.borrow_mut().tokens = Some(tokens);
                            for node in self.slots.iter().flatten() {
                                if let Some(screen) = node.screen.borrow().as_ref() {
                                    screen.update_environment()?;
                                } else if node.modal.borrow().is_some() {
                                    node.restore_style()?;
                                }
                            }
                        }
                        Command::Announce(text) => self.host.accessibility_event(
                            self.dispatcher.clone(),
                            Some(text),
                            None,
                        )?,
                        Command::AccessibilityFocus => {
                            self.accessibility_focus
                                .borrow_mut()
                                .push(Rc::downgrade(self.node(id)?));
                        }
                        Command::Dialog(request) => {
                            let id = request.id;
                            if let Err(error) = self.dialogs.show(*request) {
                                self.control.dispatch_event(
                                    nsh_core::ROOT_ID,
                                    nsh_core::EventKind::Dialog,
                                    nsh_core::EventPayload::Map(vec![
                                        ("id", nsh_core::PayloadValue::Number(id as f64)),
                                        ("index", nsh_core::PayloadValue::Number(-1.0)),
                                    ]),
                                );
                                log::error!("OHOS dialog: {error}");
                            }
                        }
                        Command::Back => self.back_fallback()?,
                        Command::ExitApp => {
                            log::warn!("OHOS ExitApp: the embedding host owns Ability lifetime")
                        }
                        Command::ImageInfo { id, uri } => {
                            self.images
                                .load(crate::media::image::Target::Info(id), uri, None);
                        }
                        Command::OpenUrl { id, url } => {
                            if self
                                .host
                                .open_url(self.dispatcher.clone(), id, url)
                                .is_err()
                            {
                                self.control.dispatch_event(
                                    nsh_core::ROOT_ID,
                                    nsh_core::EventKind::OpenUrl,
                                    nsh_core::EventPayload::Map(vec![
                                        ("id", nsh_core::PayloadValue::Number(id as f64)),
                                        ("opened", nsh_core::PayloadValue::Bool(false)),
                                    ]),
                                );
                            }
                        }
                        command @ (Command::SetClipboardImage(_)
                        | Command::ReadClipboardImage { .. }
                        | Command::ReadClipboardFiles { .. }
                        | Command::SetClipboard(_)
                        | Command::ReadClipboard { .. }
                        | Command::Speech(_)
                        | Command::Notification(_)) => {
                            self.host.system.command(
                                &self.host,
                                self.dispatcher.clone(),
                                command,
                            )?;
                        }
                        Command::PickImages { id, request } => {
                            if let Err(error) = self.host.pick(
                                self.dispatcher.clone(),
                                id,
                                nsh_core::picker::Pick::Images(request),
                            ) {
                                log::warn!("OHOS pickImages failed: {error}");
                            }
                        }
                        Command::PickFiles { id, request } => {
                            if let Err(error) = self.host.pick(
                                self.dispatcher.clone(),
                                id,
                                nsh_core::picker::Pick::Files(request),
                            ) {
                                log::warn!("OHOS pickFiles failed: {error}");
                            }
                        }
                        Command::Native { name, args } => {
                            if let Some(component) = self.node(id)?.component.borrow().as_ref() {
                                component.command(&name, &args)?;
                            }
                        }
                        Command::Blur if id == nsh_core::ROOT_ID => {
                            for node in self.slots.iter().flatten() {
                                if let Some(input) = node.input.borrow().as_ref() {
                                    input.focus(false)?;
                                }
                            }
                        }
                        Command::Focus | Command::Blur => {
                            if let Some(web) = self.node(id)?.web.borrow().as_ref() {
                                web.command(command)?;
                            } else if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                                input.focus(matches!(command, Command::Focus))?;
                            }
                        }
                        Command::SetSelection { start, end } => {
                            if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                                input.selection(start, end)?;
                            }
                        }
                        Command::InsertText(text) => {
                            if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                                input.insert_text(text)?;
                            }
                        }
                        Command::OpenEditMenu { bounds, actions } => {
                            if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                                input.open_edit_menu(bounds, actions)?;
                            }
                        }
                        Command::CloseEditMenu => {
                            if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                                input.close_edit_menu()?;
                            }
                        }
                        Command::SetJsResponder { block } => {
                            let mut node = Some(self.node(id)?.clone());
                            while let Some(current) = node {
                                if let Some(scroll) = current.scroll.borrow().as_ref() {
                                    scroll.block(block)?;
                                }
                                node = current.parent_node.borrow().upgrade();
                            }
                        }
                        Command::ClearJsResponder => {
                            for node in self.slots.iter().flatten() {
                                if let Some(scroll) = node.scroll.borrow().as_ref() {
                                    scroll.block(false)?;
                                }
                            }
                        }
                        Command::ZoomTo {
                            scale,
                            center,
                            animated,
                        } => {
                            if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                                scroll.zoom_to(scale, center.map(|p| (p.x.0, p.y.0)), animated)?;
                            }
                        }
                        Command::ScrollTo { x, y, animated } => {
                            if let Some(scroll) = self.node(id)?.scroll.borrow().as_ref() {
                                scroll.command_to(x.0, y.0, animated)?;
                            } else if let Some(input) = self.node(id)?.input.borrow().as_ref() {
                                input.scroll_to(x.0, y.0, animated);
                            }
                        }
                        command @ Command::Seek(_) => {
                            if let Some(video) = self.node(id)?.video.borrow().as_ref() {
                                video.command(command)?;
                            }
                        }
                        command @ (Command::Reload
                        | Command::GoBack
                        | Command::GoForward
                        | Command::StopLoading
                        | Command::InjectJavaScript(_)
                        | Command::PostMessage(_)
                        | Command::Load(_)
                        | Command::SendInput(_)
                        | Command::Evaluate { .. }
                        | Command::CancelEvaluate { .. }) => {
                            let node = self.node(id)?;
                            if let Some(web) = node.web.borrow().as_ref() {
                                web.command(command)?;
                            } else if let Command::Evaluate { request, .. } = command {
                                crate::media::webview::abort_evaluation(&self.control, id, request);
                            }
                        }
                        Command::LayoutAnimation(_)
                        | Command::SetAppUrl(_)
                        | Command::PushAppUrl(_)
                        | Command::CancelPress => {}
                        Command::Haptic(haptic) => {
                            self.host.haptic(haptic)?;
                        }
                        Command::PerfMonitor(enabled) => {
                            self.control
                                .set_perf_monitor(enabled.then(Default::default));
                        }
                        Command::ScrollToIndex { .. }
                        | Command::ScrollToEnd { .. }
                        | Command::Reveal(_)
                        | Command::MeasureScrollPosition { .. }
                        | Command::SetStatusBar(_)
                        | Command::Measure { .. }
                        | Command::HostCall { .. }
                        | Command::HostPost { .. } => {
                            return Err(Error::Unsupported("unresolved core command"));
                        }
                        Command::PickScreenColor { id } => self.control.dispatch_event(
                            nsh_core::ROOT_ID,
                            nsh_core::EventKind::ScreenColor,
                            nsh_core::EventPayload::Map(vec![(
                                "id",
                                nsh_core::PayloadValue::Number(f64::from(id)),
                            )]),
                        ),
                        Command::Audio(nsh_core::audio::AudioCommand::Load {
                            id,
                            bytes,
                            hint,
                            max_duration,
                        }) => {
                            // Sound.load waits for Loaded before Play or Unload, so
                            // staging on a worker preserves the command order.
                            let host = self.host.clone();
                            let route = self.dispatcher.clone();
                            std::thread::Builder::new()
                                .name("nsh-audio-load".into())
                                .spawn(move || {
                                    if let Err(error) = host.system.load_sound(
                                        &host,
                                        route,
                                        id,
                                        bytes,
                                        hint,
                                        max_duration,
                                    ) {
                                        log::warn!("OHOS sound load failed: {error}");
                                    }
                                })
                                .map_err(|error| {
                                    napi_ohos::Error::from_reason(error.to_string())
                                })?;
                        }
                        Command::Audio(audio) => {
                            self.host.system.audio(
                                &self.host,
                                self.dispatcher.clone(),
                                &self.assets,
                                audio,
                            )?;
                        }
                        Command::FileRequest { id, request } => {
                            self.host.system.file_request(
                                &self.host,
                                self.dispatcher.clone(),
                                id,
                                *request,
                            )?;
                        }
                    },
                    // Appearance and direction also reach native controls and SDK subtrees.
                    MountItem::Appearance { appearance } => {
                        self.appearance.set(appearance);
                        self.system_appearance()?;
                    }
                    MountItem::LayoutDirection { rtl } => {
                        use ohos_arkui_binding::types::{
                            attribute::ArkUINodeAttributeType as A, direction::Direction,
                        };
                        self.node(nsh_core::ROOT_ID)?
                            .native
                            .borrow()
                            .set_i32_attribute(
                                A::Direction,
                                if rtl { Direction::Rtl } else { Direction::Ltr }.into(),
                            )?;
                    }
                    MountItem::Overflow { id, extent } => {
                        let node = self.node(id)?;
                        node.overflow.set(extent.map(Rect::from));
                        node.invalidate_composites();
                        node.apply_clip()?;
                        node.interaction.hit_region(node)?;
                        node.invalidate_scene();
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                if creating_root {
                    return Err(error);
                }
                // Attribute and resource failures affect this item only. Later
                // items (including updates after a stale node) still apply.
                log::warn!(
                    target: "nsh::mount",
                    "skipped mount item: batch={} item={at}/{items}: {error}",
                    self.batches
                );
            }
        }
        Self::attribute(self.reconcile_navigation());
        if nav_shader_configured {
            self.shaders.log_idle_configured();
        }
        Self::attribute(self.animator.flush());
        Self::attribute(self.shaders.frame().map(|_| ()));
        let selected = self
            .selected_text
            .borrow()
            .as_ref()
            .and_then(std::rc::Weak::upgrade);
        if let Some(owner) = selected {
            Self::attribute(owner.selection_refresh());
        }
        self.flush_arkui()
    }

    pub(crate) fn image_ready(&self, ready: crate::media::image::Ready) -> Result<()> {
        match ready.target {
            crate::media::image::Target::Canvas {
                id,
                generation,
                index,
            } => {
                if let Ok(node) = self.node(id)
                    && let Some(canvas) = node.canvas.borrow().as_ref()
                {
                    canvas.ready(generation, index, ready.data)?;
                }
            }
            crate::media::image::Target::Info(id) => {
                crate::media::image::info(&self.control, id, ready.data)
            }
            crate::media::image::Target::View {
                id,
                generation,
                placeholder,
            } => {
                if let Ok(node) = self.node(id)
                    && let Some(image) = node.image.borrow().as_ref()
                {
                    image.ready(generation, placeholder, ready.data)?;
                }
            }
        }
        Ok(())
    }
    pub(crate) fn image_progress(&self, progress: crate::media::image::Progress) {
        if let crate::media::image::Target::View {
            id,
            generation,
            placeholder: false,
        } = progress.target
            && let Ok(node) = self.node(id)
            && let Some(image) = node.image.borrow().as_ref()
        {
            image.progress(generation, progress.loaded, progress.total);
        }
    }
    pub(crate) fn flush_scroll_reports(&self) -> Result<()> {
        let pending = std::mem::take(&mut *self.scroll_reports.borrow_mut());
        for node in pending.into_iter().filter_map(|node| node.upgrade()) {
            if let Some(scroll) = node.scroll.borrow().as_ref() {
                scroll.flush_report()?;
            }
        }
        // A shader view scrolled into or out of sight starts or stops.
        self.shaders.reveal()
    }
    pub(crate) fn frame(&mut self, timestamp: Option<u64>) -> Result<bool> {
        self.shaders.expire_captures()?;
        let mut animating = self.animator.tick(timestamp)?;
        animating |= self.shaders.frame()?;
        let mut settled_leaving = false;
        for id in self.animator.take_removals() {
            if let Ok(node) = self.node(id) {
                settled_leaving = true;
                if node.pending_delete.get() {
                    node.set_leaving(false);
                } else {
                    node.finish_removal()?;
                    self.unlink(id, true)?;
                }
            }
        }
        self.modals.retain(|modal| modal.strong_count() > 0);
        for modal in self.modals.iter().filter_map(std::rc::Weak::upgrade) {
            animating |= modal.frame(self.animator.frame_time())?;
        }
        if settled_leaving && !self.pending_delete.is_empty() {
            animating = true;
        } else {
            let pending = std::mem::take(&mut self.pending_delete);
            let mut deleted_any = false;
            for id in pending {
                if self.frozen(id) {
                    self.pending_delete.push(id);
                } else {
                    self.delete_now(id)?;
                    deleted_any = true;
                }
            }
            if settled_leaving || deleted_any {
                self.reconcile_navigation_capture()?;
            }
        }
        self.finish_deferred()?;
        self.flush_arkui()?;
        Ok(animating)
    }

    pub(crate) fn resize(&self, size: Size) -> Result<()> {
        let mut platform = self.platform.get();
        platform.window = size.into();
        self.platform.set(platform);
        for modal in self.modals.iter().filter_map(std::rc::Weak::upgrade) {
            modal.sync_metrics();
            if let Some(id) = modal.node_id() {
                self.node(id)?
                    .native
                    .borrow()
                    .mark_dirty(ohos_arkui_binding::types::advanced::NodeDirtyFlag::NeedMeasure)?;
            }
        }
        self.node(1)?.set_frame(Rect {
            x: 0.0,
            y: 0.0,
            width: size.width,
            height: size.height,
        })?;
        if let Some(owner) = self.fullscreen_video.borrow().upgrade()
            && let Some(video) = owner.video.borrow().as_ref()
        {
            video.fullscreen_frame(size)?;
        }
        self.control.resize(size.into());
        Ok(())
    }

    pub(crate) fn input_paste(
        &self,
        id: NodeId,
        text: String,
        html: Option<String>,
        files: Option<String>,
        image: Option<String>,
        report_only: bool,
    ) -> Result<()> {
        if let Some(node) = self.slots.get(id as usize).and_then(Option::as_ref)
            && let Some(input) = node.input.borrow().as_ref()
        {
            input.complete_paste(text, html, files, image, report_only)?;
        }
        Ok(())
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        self.scroll_watchers.clear_nodes();
        self.dialogs.clear();
        self.mount.unmount()?;
        if let Ok(root) = self.node(nsh_core::ROOT_ID) {
            // MountTarget already removed the engine root from the viewport.
            // Its normal deletion must not remove that native edge a second time.
            *root.mount_parent.borrow_mut() = std::rc::Weak::new();
        }
        for modal in self.modals.iter().filter_map(std::rc::Weak::upgrade) {
            modal.close(false)?;
        }
        self.pending_delete.clear();
        self.deferred.clear();
        for id in (0..self.slots.len()).rev() {
            if self.slots.get(id).is_some_and(Option::is_some) {
                self.delete_now(id as NodeId)?;
            }
        }
        self.shaders.close()?;
        if let Some(worker) = self.compositor.borrow().as_ref() {
            worker.collect();
            worker.drain();
        }
        self.flush_arkui()
    }

    pub(crate) fn freeze_retired(&mut self) -> Option<u64> {
        if self.removed.is_empty() {
            return None;
        }
        self.epoch += 1;
        self.retired
            .push_back((self.epoch, std::mem::take(&mut self.removed)));
        Some(self.epoch)
    }

    pub(crate) fn reclaim(&mut self, epoch: u64) {
        while self.retired.front().is_some_and(|(id, _)| *id <= epoch) {
            self.retired.pop_front();
        }
    }
}

impl Drop for Mounter {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            log::error!("OHOS detach failed: {error}");
        }
    }
}
