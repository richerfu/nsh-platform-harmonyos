//! Retained stacks and native headers. Back decisions use mounted props;
//! dismissed screens stay hidden until the engine removes their routes.
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use nsh_core::{
    EdgeInsets, EventKind, EventPayload, Points, ScreenAnimation, ScreenConfig, ViewKind,
};
use ohos_arkui_binding::{
    component::{
        attribute::{ArkUICommonAttribute, ArkUIEvent},
        built_in_component::{Custom, Stack, Text},
    },
    types::{
        advanced::{LengthMetricUnit, NodeDirtyFlag},
        attribute::ArkUINodeAttributeType as A,
        node_type::ArkUINodeType,
    },
};

use crate::{
    navigation::motion::Motion,
    render::node::{NativeNode, Node, Result},
    text::typography::argb,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RootBridgePhase {
    #[default]
    None,
    WaitingTextureTarget,
    RenderingExport,
}

pub(crate) struct Screen {
    owner: Weak<Node>,
    pub(crate) config: RefCell<ScreenConfig>,
    pub(crate) popped: Cell<bool>,
    header: NativeNode,
    title: NativeNode,
    back: NativeNode,
    has_back: Cell<bool>,
    top: Cell<bool>,
    revealed: Cell<bool>,
    pub(crate) bridge_phase: Cell<RootBridgePhase>,
    entered: Cell<bool>,
    progress: Cell<f32>,
    pub(crate) peer: RefCell<Weak<Node>>,
    motion: RefCell<Option<Motion>>,
    shader: RefCell<Option<Rc<crate::render::shader::navigation::Navigation>>>,
    immediate_removal: Cell<bool>,
    insets: Cell<Option<EdgeInsets>>,
}
impl Screen {
    #[cfg(feature = "dev")]
    pub(crate) fn test_animating(&self) -> bool {
        self.motion.borrow().is_some() || self.bridge_phase.get() != RootBridgePhase::None
    }
    pub(crate) fn header(&self) -> &NativeNode {
        &self.header
    }

    pub(crate) fn new(owner: Weak<Node>) -> Result<Rc<Self>> {
        let header = Stack::new()?.into_node();
        let title = Text::new()?.into_node();
        let back = Custom::new()?.into_node();
        for node in [&header, &title, &back] {
            node.borrow().set_length_metric_unit(LengthMetricUnit::Vp)?;
        }
        header.borrow().z_index(1)?;
        header.borrow().set_i32_attribute(
            A::HitTestBehavior,
            ohos_arkui_binding::types::view::HitTestMode::Transparent.into(),
        )?;
        title.borrow().set_i32_attribute(
            A::HitTestBehavior,
            ohos_arkui_binding::types::view::HitTestMode::None.into(),
        )?;
        header.borrow_mut().add_child(back.clone())?;
        header.borrow_mut().add_child(title.clone())?;
        back.borrow()
            .set_u32_attribute(A::AccessibilityRole, ArkUINodeType::Button.into())?;
        back.borrow().set_i32_attribute(
            A::AccessibilityMode,
            ohos_arkui_binding::types::view::AccessibilityMode::Enabled.into(),
        )?;
        back.borrow().id("nsh-header-back")?;
        back.borrow().width(44.0)?;
        back.borrow().height(44.0)?;
        title.borrow().set_f32_attribute(A::FontSize, 18.0)?;
        title.borrow().set_i32_attribute(A::TextAlign, 1)?;
        title.borrow().height(44.0)?;
        let screen = Rc::new(Self {
            owner,
            config: RefCell::default(),
            popped: Cell::new(false),
            header,
            title,
            back,
            has_back: Cell::new(false),
            top: Cell::new(false),
            revealed: Cell::new(false),
            bridge_phase: Cell::new(RootBridgePhase::None),
            entered: Cell::new(false),
            progress: Cell::new(0.0),
            peer: RefCell::default(),
            motion: RefCell::default(),
            shader: RefCell::default(),
            immediate_removal: Cell::new(false),
            insets: Cell::new(None),
        });
        let weak = Rc::downgrade(&screen);
        screen.back.borrow_mut().on_custom_draw(move |event| {
            crate::render::node::callback(|| {
                let Some(screen) = weak.upgrade() else {
                    return Ok(());
                };
                let Some(owner) = screen.owner.upgrade() else {
                    return Ok(());
                };
                let Some(context) = event.draw_context_in_draw() else {
                    return Ok(());
                };
                let Some(raw) = context.canvas() else {
                    return Ok(());
                };
                // SAFETY: ArkUI owns this canvas throughout the draw callback.
                let canvas = unsafe { ohos_drawing_binding::Canvas::from_raw_borrowed(raw) };
                use ohos_drawing_binding::{LineCap, LineJoin, Path, Pen};
                let size = context.size();
                let scale = owner.scale.get();
                let (x, y) = (size.width as f32 / 2.0, size.height as f32 / 2.0);
                let direction = if owner.platform.get().rtl { -1.0 } else { 1.0 };
                let mut path = Path::new();
                path.move_to(x + 4.0 * scale * direction, y - 8.0 * scale);
                path.line_to(x - 4.0 * scale * direction, y);
                path.line_to(x + 4.0 * scale * direction, y + 8.0 * scale);
                let dark = owner
                    .appearance
                    .get()
                    .resolve(owner.platform.get().color_scheme)
                    == nsh_engine::ColorScheme::Dark;
                let color = screen
                    .config
                    .borrow()
                    .tint_color
                    .map(argb)
                    .unwrap_or_else(|| owner.theme.borrow().color("foreground", dark));
                let mut pen = Pen::new();
                pen.set_anti_alias(true);
                pen.set_color(color);
                pen.set_width(2.0 * scale);
                pen.set_cap(LineCap::RoundCap);
                pen.set_join(LineJoin::RoundJoin);
                canvas.attach_pen(&pen);
                canvas.draw_path(&path);
                canvas.detach_pen();
                Ok(())
            });
        });
        let weak = Rc::downgrade(&screen);
        screen.back.borrow_mut().on_click(move || {
            if let Some(screen) = weak.upgrade() {
                crate::render::node::callback(|| screen.request_back());
            }
        });
        Ok(screen)
    }
    pub(crate) fn is_top(&self) -> bool {
        self.top.get() && !self.popped.get()
    }
    pub(crate) fn has_back(&self) -> bool {
        self.has_back.get()
    }
    pub(crate) fn insets_initialized(&self) -> bool {
        self.insets.get().is_some()
    }
    pub(crate) fn paint_visible(&self) -> bool {
        match self.bridge_phase.get() {
            RootBridgePhase::WaitingTextureTarget => return false,
            RootBridgePhase::RenderingExport => return true,
            RootBridgePhase::None => {}
        }
        crate::render::shader::navigation::screen_paint_visible(
            self.shader.borrow().as_deref(),
            self.is_top(),
            self.revealed.get(),
            self.motion.borrow().is_some(),
        )
    }
    pub(crate) fn on_shader_captured(
        &self,
        navigation: &crate::render::shader::navigation::Navigation,
    ) -> Result<()> {
        let matches = self
            .shader
            .borrow()
            .as_ref()
            .is_some_and(|current| current.key == navigation.key);
        if !matches {
            return Ok(());
        }
        if !navigation.backwards {
            self.progress.set(0.0);
        }
        self.show()?;
        self.paint()
    }
    pub(crate) fn large_title_offset(&self) -> f32 {
        let c = self.config.borrow();
        if c.header_visible && !c.header_transparent && c.large_title {
            52.0 + self
                .owner
                .upgrade()
                .map_or(0.0, |owner| owner.top_overlap())
        } else {
            0.0
        }
    }
    pub(crate) fn configure(&self, config: ScreenConfig) -> Result<()> {
        *self.config.borrow_mut() = config;
        if self.insets.get().is_some() {
            self.update(self.has_back.get())?;
        }
        Ok(())
    }
    pub(crate) fn update_environment(&self) -> Result<()> {
        if let Some(owner) = self.owner.upgrade() {
            owner.restore_style()?;
        }
        self.update(self.has_back.get())
    }
    pub(crate) fn update(&self, has_back: bool) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        self.has_back.set(has_back);
        let c = self.config.borrow();
        let dark = owner
            .appearance
            .get()
            .resolve(owner.platform.get().color_scheme)
            == nsh_engine::ColorScheme::Dark;
        let fg = owner.theme.borrow().color("foreground", dark);
        let overlap = owner.top_overlap();
        let height = if c.header_visible {
            52.0 + overlap
        } else {
            0.0
        };
        if owner.parent.get().is_some() {
            if !c.large_title && !c.header_transparent && (c.header_height.0 - height).abs() > 0.01
            {
                owner
                    .control
                    .set_screen_header(owner.id, Some(Points(height)));
            }
            let safe = owner.platform.get().safe_area;
            let insets = EdgeInsets {
                top: if c.header_visible && !c.header_transparent {
                    Points::ZERO
                } else {
                    Points(overlap)
                },
                right: safe.right,
                bottom: safe.bottom,
                left: safe.left,
            };
            if self.insets.replace(Some(insets)) != Some(insets) {
                owner.emit(
                    EventKind::Insets,
                    nsh_core::navigation::insets_payload(insets),
                );
            }
        }
        let h = self.header.borrow();
        h.width(owner.frame.get().width)?;
        h.height(height)?;
        h.set_i32_attribute(A::Visibility, i32::from(!c.header_visible))?;
        h.background_color(if c.header_transparent {
            0
        } else {
            c.background_color
                .map(argb)
                .unwrap_or_else(|| owner.theme.borrow().color("card", dark))
        })?;
        self.title
            .borrow()
            .set_string_attribute(A::TextContent, c.title.as_deref().unwrap_or(""))?;
        self.title
            .borrow()
            .set_u32_attribute(A::FontColor, c.title_color.map(argb).unwrap_or(fg))?;
        self.layout_title(&owner, &c)?;
        self.title
            .borrow()
            .set_i32_attribute(A::Visibility, i32::from(c.title_view.is_some()))?;
        self.back.borrow().set_position(vec![
            if owner.platform.get().rtl {
                (owner.frame.get().width - 48.0).max(0.0)
            } else {
                4.0
            },
            4.0 + overlap,
        ])?;
        self.back.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        let label = self.previous().and_then(|previous| {
            let screen = previous.screen.borrow();
            let config = screen.as_ref()?.config.borrow();
            config.back_title.clone().or_else(|| config.title.clone())
        });
        self.back
            .borrow()
            .set_string_attribute(A::AccessibilityText, label.as_deref().unwrap_or(""))?;
        self.back
            .borrow()
            .set_i32_attribute(A::Visibility, i32::from(!has_back || !c.back_visible))?;
        drop(c);
        for child in owner.children.borrow().iter().filter_map(Weak::upgrade) {
            if child.kind == ViewKind::HeaderItem {
                child.restore_style()?;
            } else {
                child.set_frame(child.raw_frame.get())?;
            }
        }
        owner
            .native
            .borrow()
            .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
        owner
            .native
            .borrow()
            .mark_dirty(NodeDirtyFlag::NeedLayout)?;
        Ok(())
    }
    fn title_bounds(&self, owner: &Node, config: &ScreenConfig) -> (f32, f32) {
        let mut left = 0.0;
        let mut right = 0.0;
        for child in owner.children.borrow().iter().filter_map(Weak::upgrade) {
            if child.kind != ViewKind::HeaderItem || !child.header_item_visible() {
                continue;
            }
            if config.left == Some(child.id) {
                left = child.frame.get().width;
            }
            if config.right == Some(child.id) {
                right = child.frame.get().width;
            }
        }
        super::motion::title_bounds(
            owner.frame.get().width,
            self.has_back.get() && config.back_visible,
            left,
            right,
            owner.platform.get().rtl,
        )
    }
    fn layout_title(&self, owner: &Node, config: &ScreenConfig) -> Result<()> {
        let (x, width) = self.title_bounds(owner, config);
        self.title.borrow().width(width)?;
        self.title
            .borrow()
            .set_position(vec![x, 4.0 + owner.top_overlap()])?;
        Ok(())
    }
    pub(crate) fn restore_visibility(&self) -> Result<()> {
        let c = self.config.borrow();
        self.header
            .borrow()
            .set_i32_attribute(A::Visibility, i32::from(!c.header_visible))?;
        self.title
            .borrow()
            .set_i32_attribute(A::Visibility, i32::from(c.title_view.is_some()))?;
        self.back.borrow().set_i32_attribute(
            A::Visibility,
            i32::from(!self.has_back.get() || !c.back_visible),
        )?;
        self.back.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        Ok(())
    }
    pub(crate) fn invalidate_paint(&self) -> Result<()> {
        self.back.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        Ok(())
    }
    pub(crate) fn request_back(&self) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        if self.popped.get() {
            return Ok(());
        }
        if owner.interaction.mask().contains(EventKind::BackRequest) {
            owner.emit(EventKind::BackRequest, EventPayload::None);
        }
        if self.has_back.get() && !self.config.borrow().prevent_back {
            self.popped.set(true);
            self.pop()?;
            if let Some(dispatcher) = owner.dispatcher.upgrade() {
                dispatcher.send(crate::app::runtime::Message::ReconcileNavigation);
            }
            owner.emit(EventKind::Dismiss, EventPayload::None);
        }
        Ok(())
    }
    pub(crate) fn owner(&self) -> Option<Rc<Node>> {
        self.owner.upgrade()
    }
    pub(crate) fn previous(&self) -> Option<Rc<Node>> {
        let owner = self.owner.upgrade()?;
        let parent = owner.parent_node.borrow().upgrade()?;
        let mut previous = None;
        for child in parent.children.borrow().iter().filter_map(Weak::upgrade) {
            if child.id == owner.id {
                return previous;
            }
            if child.kind == ViewKind::Screen
                && !child.is_leaving()
                && child
                    .screen
                    .borrow()
                    .as_ref()
                    .is_some_and(|screen| !screen.popped.get())
            {
                previous = Some(child);
            }
        }
        None
    }
    pub(crate) fn is_unanimated(&self) -> bool {
        self.motion.borrow().is_none() && self.shader.borrow().is_none()
    }
    pub(crate) fn set_revealed(&self, revealed: bool) {
        self.revealed.set(revealed);
    }
    fn show(&self) -> Result<()> {
        if let Some(owner) = self.owner.upgrade() {
            owner.restore_style()?;
        }
        Ok(())
    }
    fn reveal(&self) -> Result<bool> {
        let Some(previous) = self.previous() else {
            return Ok(false);
        };
        if let Some(screen) = previous.screen.borrow().as_ref() {
            // A new push can interrupt the preceding push at its settled position.
            if screen.motion.borrow().is_some() || screen.shader.borrow().is_some() {
                screen.settle()?;
            }
            screen.revealed.set(true);
            screen.show()?;
        }
        *self.peer.borrow_mut() = Rc::downgrade(&previous);
        Ok(true)
    }
    fn paint(&self) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        if self.shader.borrow().is_some() {
            owner.navigation_presentation(1.0, 0.0)?;
            if let Some(peer) = self.peer.borrow().upgrade() {
                peer.navigation_presentation(1.0, 0.0)?;
            }
            return Ok(());
        }
        let p = self.progress.get();
        let fade = self.config.borrow().animation == ScreenAnimation::Fade
            || owner.platform.get().reduce_motion;
        let sign = if owner.platform.get().rtl { -1.0 } else { 1.0 };
        let (enter_x, _) =
            nsh_core::navigation::push_offsets(nsh_core::Points(owner.frame.get().width), 1.0 - p);
        owner.navigation_presentation(
            if fade { 1.0 - p } else { 1.0 },
            if fade { 0.0 } else { enter_x.0 * sign },
        )?;
        if let Some(peer) = self.peer.borrow().upgrade() {
            let (_, below_x) = nsh_core::navigation::push_offsets(
                nsh_core::Points(peer.frame.get().width),
                1.0 - p,
            );
            peer.navigation_presentation(1.0, if fade { 0.0 } else { below_x.0 * sign })?;
        }
        Ok(())
    }
    fn start_motion(&self, owner: &Rc<Node>, to: f32) -> Result<()> {
        let motion = nsh_core::navigation::screen_motion(
            self.config.borrow().animation,
            owner.platform.get().reduce_motion,
        );
        *self.motion.borrow_mut() = Some(Motion::new(
            owner.animator.now(),
            self.progress.get(),
            to,
            motion.map(|motion| motion.0),
        ));
        self.show()?;
        self.paint()?;
        owner.animator.track(owner);
        Ok(())
    }
    fn animate(&self, to: f32) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        let start_shader =
            (to == 1.0 && self.progress.get() == 0.0) || (to == 0.0 && self.progress.get() == 1.0);
        if start_shader
            && let Some(navigation) =
                owner
                    .shaders
                    .begin_navigation(&owner, self.peer.borrow().upgrade(), to == 1.0)?
        {
            self.motion.borrow_mut().take();
            *self.shader.borrow_mut() = Some(navigation);
            self.show()?;
            self.paint()?;
            owner.animator.track(&owner);
            return Ok(());
        }
        if self.shader.borrow_mut().take().is_some() {
            owner.shaders.end_navigation(owner.id)?;
        }
        self.start_motion(&owner, to)
    }
    fn try_begin_root_bridge(&self, owner: &Rc<Node>) -> Result<bool> {
        if owner.navigation_shader.borrow().is_some() {
            return Ok(false);
        }
        let Some(stack) = owner.parent_node.borrow().upgrade() else {
            return Ok(false);
        };
        let Some(previous) = self.previous() else {
            return Ok(false);
        };
        let mut at = Some(stack.clone());
        let mut root = None;
        while let Some(node) = at {
            if !node.active.get() || node.retained.get() || node.modal.borrow().is_some() {
                return Ok(false);
            }
            if node.id == nsh_core::ROOT_ID {
                root = Some(node);
                break;
            }
            at = node.parent_node.borrow().upgrade();
        }
        let Some(root) = root else {
            return Ok(false);
        };
        if !root.can_bridge_cold_export() {
            return Ok(false);
        }
        if let Some(previous_screen) = previous.screen.borrow().as_ref() {
            if previous_screen.motion.borrow().is_some()
                || previous_screen.shader.borrow().is_some()
            {
                previous_screen.settle()?;
            }
            previous_screen.revealed.set(true);
            previous_screen.show()?;
        }
        previous.navigation_presentation(1.0, 0.0)?;
        *self.peer.borrow_mut() = Rc::downgrade(&previous);
        self.progress.set(0.0);
        self.bridge_phase.set(RootBridgePhase::WaitingTextureTarget);
        owner.navigation_presentation(1.0, 0.0)?;
        self.show()?;
        root.install_root_bridge(&stack, &previous, owner)?;
        Ok(true)
    }
    fn push(&self) -> Result<()> {
        if let Some(owner) = self.owner.upgrade()
            && self.try_begin_root_bridge(&owner)?
        {
            return Ok(());
        }
        let shader = self
            .owner
            .upgrade()
            .is_some_and(|owner| owner.navigation_shader.borrow().is_some());
        if (shader || self.config.borrow().animation != ScreenAnimation::None) && self.reveal()? {
            self.progress.set(1.0);
            self.animate(0.0)?;
        }
        Ok(())
    }
    fn pop(&self) -> Result<bool> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(false);
        };
        if self.bridge_phase.get() != RootBridgePhase::None {
            owner.release_enclosing_root_bridge()?;
            self.show()?;
            return Ok(false);
        }
        if self.immediate_removal.get() {
            return Ok(false);
        }
        if self
            .motion
            .borrow()
            .as_ref()
            .is_some_and(|motion| motion.to == 1.0)
            || self.shader.borrow().as_ref().is_some_and(|n| n.backwards)
        {
            return Ok(true);
        }
        if (self.config.borrow().animation == ScreenAnimation::None
            && owner.navigation_shader.borrow().is_none())
            || !self.reveal()?
        {
            self.show()?;
            return Ok(false);
        }
        owner.set_leaving(true);
        self.animate(1.0)?;
        Ok(true)
    }
    pub(crate) fn remove(&self) -> Result<bool> {
        if self.immediate_removal.get() {
            return Ok(false);
        }
        if !self.is_top() {
            return Ok(false);
        }
        self.popped.set(true);
        self.pop()
    }
    pub(crate) fn prepare_removal(&self, animated: bool) -> Result<()> {
        self.immediate_removal.set(!animated);
        if !animated {
            self.settle()?;
            if let Some(owner) = self.owner.upgrade() {
                owner.set_leaving(false);
            }
        }
        Ok(())
    }
    fn settle(&self) -> Result<()> {
        if self.bridge_phase.get() != RootBridgePhase::None {
            self.bridge_phase.set(RootBridgePhase::None);
            if let Some(owner) = self.owner.upgrade() {
                owner.release_enclosing_root_bridge()?;
            }
        }
        let shader = self.shader.borrow_mut().take();
        self.motion.borrow_mut().take();
        self.progress.set(0.0);
        if let Some(peer) = self.peer.replace(Weak::new()).upgrade() {
            let shared = self
                .owner
                .upgrade()
                .and_then(|owner| owner.parent_node.borrow().upgrade())
                .is_some_and(|stack| {
                    stack
                        .children
                        .borrow()
                        .iter()
                        .filter_map(Weak::upgrade)
                        .filter(|child| {
                            !self.owner.upgrade().is_some_and(|o| Rc::ptr_eq(&o, child))
                        })
                        .filter_map(|child| child.screen.borrow().clone())
                        .any(|screen| {
                            screen
                                .peer
                                .borrow()
                                .upgrade()
                                .is_some_and(|p| Rc::ptr_eq(&p, &peer))
                        })
                });
            if !shared {
                if let Some(screen) = peer.screen.borrow().as_ref() {
                    screen.revealed.set(false);
                    screen.show()?;
                }
                peer.navigation_presentation(1.0, 0.0)?;
                peer.invalidate_scene();
            }
        }
        self.show()?;
        if let Some(owner) = self.owner.upgrade() {
            owner.navigation_presentation(1.0, 0.0)?;
        }
        // Establish final native visibility before switching the root out of
        // export, so its attachment repair includes every newly hidden screen.
        if shader.is_some()
            && let Some(owner) = self.owner.upgrade()
        {
            owner.shaders.end_navigation(owner.id)?;
        }
        Ok(())
    }
    pub(crate) fn frame(&self, now: f64) -> Result<(bool, bool)> {
        let shader = self
            .shader
            .borrow()
            .as_ref()
            .map(|n| (n.running(), n.take_fallback(), n.backwards));
        if let Some((running, fallback, backwards)) = shader {
            if running {
                return Ok((true, false));
            }
            if fallback {
                self.shader.borrow_mut().take();
                if let Some(owner) = self.owner.upgrade() {
                    owner.shaders.end_navigation(owner.id)?;
                    if !backwards && self.progress.get() == 0.0 {
                        self.progress.set(1.0);
                    }
                    self.start_motion(&owner, if backwards { 1.0 } else { 0.0 })?;
                } else {
                    self.settle()?;
                    return Ok((false, backwards));
                }
            } else {
                self.settle()?;
                return Ok((false, backwards));
            }
        }
        let state = self.motion.borrow().as_ref().map(|motion| motion.step(now));
        let Some((progress, done, to)) = state else {
            return Ok((false, false));
        };
        self.progress.set(progress);
        self.paint()?;
        if done {
            self.settle()?;
        }
        Ok((!done, done && to == 1.0))
    }
    pub(crate) fn can_swipe(&self, x: f32) -> bool {
        let Some(owner) = self.owner.upgrade() else {
            return false;
        };
        let c = self.config.borrow();
        self.is_top()
            && self.has_back.get()
            && c.gesture_enabled
            && self.bridge_phase.get() == RootBridgePhase::None
            && self.motion.borrow().is_none()
            && self.shader.borrow().is_none()
            && !owner.platform.get().screen_reader
            && if owner.platform.get().rtl {
                x >= owner.frame.get().width - 24.0
            } else {
                x <= 24.0
            }
    }
    pub(crate) fn swipe(&self, dx: f32) -> Result<()> {
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        if self.peer.borrow().upgrade().is_none() {
            self.reveal()?;
        }
        let raw = (dx / owner.frame.get().width.max(1.0)).clamp(0.0, 1.0);
        let progress = if self.config.borrow().prevent_back {
            raw * 0.25
        } else {
            raw
        };
        self.progress.set(progress);
        self.paint()
    }
    pub(crate) fn finish_swipe(&self, complete: bool) -> Result<()> {
        if complete && self.is_top() && self.config.borrow().gesture_enabled {
            if self.config.borrow().prevent_back {
                self.animate(0.0)?;
            }
            self.request_back()
        } else {
            self.animate(0.0)
        }
    }
    pub(crate) fn measure(
        &self,
        event: &ohos_arkui_binding::api::node_custom_event::NodeCustomEvent,
    ) -> Result<()> {
        let mut measure = crate::render::layout::ChildMeasure::new(event);
        if let Some(owner) = self.owner.upgrade() {
            self.layout_title(&owner, &self.config.borrow())?;
            // These slots are explicitly laid out below. A newly attached
            // export wrapper needs measurement in the same parent pass, even
            // when its engine-resolved size has not changed.
            for child in owner
                .children
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .filter(|node| node.kind == ViewKind::HeaderItem)
            {
                measure.child(&child.mounted().borrow())?;
            }
        }
        measure.child(&self.header.borrow())?;
        Ok(())
    }
    pub(crate) fn item_position(&self, child: &Node) -> Option<(f32, f32)> {
        let owner = self.owner.upgrade()?;
        let c = self.config.borrow();
        if !c.header_visible {
            return None;
        }
        let width = owner.frame.get().width;
        let frame = child.frame.get();
        let x = if c.left == Some(child.id) {
            if self.has_back.get() && c.back_visible {
                56.0
            } else {
                12.0
            }
        } else if c.right == Some(child.id) {
            width - frame.width - 12.0
        } else if c.title_view == Some(child.id) {
            let (start, available) = self.title_bounds(&owner, &c);
            start + (available - frame.width).max(0.0) * 0.5
        } else {
            return None;
        };
        let x = if owner.platform.get().rtl && c.title_view != Some(child.id) {
            width - x - frame.width
        } else {
            x
        };
        let y = owner.top_overlap() + (52.0 - frame.height).max(0.0) * 0.5;
        Some((x, y))
    }
    pub(crate) fn layout(
        &self,
        event: &ohos_arkui_binding::api::node_custom_event::NodeCustomEvent,
    ) -> Result<()> {
        event.layout_child(&self.header.borrow(), 0, 0)?;
        if let Some(owner) = self.owner.upgrade() {
            for child in owner
                .children
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
                .filter(|n| n.kind == ViewKind::HeaderItem)
            {
                if let Some((x, y)) = self.item_position(&child) {
                    child.place(
                        event,
                        (x * owner.scale.get()).round() as i32,
                        (y * owner.scale.get()).round() as i32,
                    )?;
                }
            }
        }
        Ok(())
    }
    pub(crate) fn install(&self, owner: &Node) -> Result<()> {
        owner.native.borrow_mut().add_child(self.header.clone())?;
        Ok(())
    }
}
pub(crate) fn reconcile<'a>(stacks: impl Iterator<Item = &'a Rc<Node>>) -> Result<()> {
    for stack in stacks {
        let screens: Vec<_> = stack
            .children
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|node| {
                node.screen
                    .borrow()
                    .as_ref()
                    .is_some_and(|s| !s.popped.get())
            })
            .collect();
        let previous_top = screens
            .iter()
            .find(|node| node.screen.borrow().as_ref().is_some_and(|s| s.is_top()))
            .map(|node| node.id);
        for (index, screen) in screens.iter().enumerate() {
            if let Some(state) = screen.screen.borrow().as_ref() {
                let top = index + 1 == screens.len();
                let top_changed = state.top.replace(top) != top;
                let entering = state.insets.get().is_none();
                // A content/scroll batch does not change the native header.
                // Updating it would measure and lay out the entire screen again.
                if entering || state.has_back.get() != (index > 0) {
                    state.update(index > 0)?;
                }
                if top
                    && !state.entered.replace(true)
                    && index > 0
                    && previous_top.is_some_and(|id| id != screen.id)
                {
                    state.push()?;
                }
                if entering || top_changed {
                    state.show()?;
                }
            }
        }
    }
    Ok(())
}
