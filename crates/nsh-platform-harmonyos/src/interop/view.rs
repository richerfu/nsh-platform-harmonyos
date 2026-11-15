//! On-demand ArkTS wrappers retain visual and accessibility composition around C subtrees.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use nsh_core::{AccessibilityRole as R, Checked, EventKind};
use ohos_arkui_binding::{
    component::{
        attribute::{ArkUICommonAttribute, ArkUIEvent},
        built_in_component::Custom,
    },
    types::advanced::{LengthMetricUnit, NodeDirtyFlag},
};

use crate::{
    accessibility::value::owns_semantics,
    interop::{
        attachment::NativeAttachment,
        sdk::{MenuItem, ViewOptions, ViewRoute},
    },
    render::{
        composition::worker::Composite,
        node::{NativeNode, Node, Result, callback},
    },
};

// A stable layout host owns the native subtree and its optional GPU composition.
pub(crate) struct Wrapper {
    closed: Cell<bool>,
    pub(crate) outer: NativeNode,
    content: NativeNode,
    target: RefCell<Option<NativeAttachment>>,
    route: RefCell<Option<ViewRoute>>,
    context_live: Cell<bool>,
    owner: std::rc::Weak<Node>,
    settings: RefCell<Option<ViewOptions>>,
    pub(crate) composite: RefCell<Option<Composite>>,
    semantics: Cell<Option<(i32, bool)>>,
    measured: Cell<Option<(f32, f32)>>,
}
impl Wrapper {
    pub(crate) fn is_context_live(&self) -> bool {
        !self.closed.get() && self.context_live.get()
    }
    pub(crate) fn invalidate_context(&self) {
        self.context_live.set(false);
        self.settings.borrow_mut().take();
        self.route.borrow_mut().take();
    }
    fn replace_target(&self, owner: &Node) -> Result<u32> {
        let target = NativeAttachment::for_subtree(&self.content, &owner.native)?;
        let id = target.id();
        if let Some(old) = self.target.replace(Some(target)) {
            owner.view_remove(old);
        }
        // The native target is the unique, nonreused callback generation.
        let weak = self.owner.clone();
        *self.route.borrow_mut() = Some(ViewRoute::new(id, weak));
        Ok(id)
    }
    fn ensure_context(&self, owner: &Node) -> Result<()> {
        let target_retired = self
            .target
            .borrow()
            .as_ref()
            .is_none_or(NativeAttachment::is_retired);
        if !self.context_live.get() || target_retired {
            self.settings.borrow_mut().take();
            self.replace_target(owner)?;
            if !self.context_live.get() && self.composite.borrow_mut().take().is_some() {
                let composite = crate::render::composition::worker::Composite::new(
                    &self
                        .owner
                        .upgrade()
                        .ok_or(crate::render::node::Error::InvalidNode(owner.id))?,
                )?;
                *self.composite.borrow_mut() = Some(composite);
            }
            self.context_live.set(true);
        }
        Ok(())
    }
    fn with_child<T>(
        &self,
        f: impl FnOnce(&ohos_arkui_binding::common::node::ArkUINode) -> T,
    ) -> Result<Option<T>> {
        if self.closed.get()
            || !self.context_live.get()
            || !self
                .owner
                .upgrade()
                .is_some_and(|owner| owner.host.check().is_ok())
        {
            return Ok(None);
        }
        self.target
            .borrow()
            .as_ref()
            .map_or(Ok(None), |target| Ok(target.with_child(f)?))
    }
    fn abort(self) {
        let cleanup = || -> Result<()> {
            if let Some(target) = self.target.borrow().as_ref() {
                target.invalidate()?;
            }
            self.detach_content()?;
            self.outer.borrow().remove_from_parent()?;
            self.outer.borrow_mut().dispose()?;
            Ok(())
        };
        if let Err(error) = cleanup() {
            log::error!("Failed to release an unmounted OHOS wrapper: {error}");
            // A failed native cleanup must retain the complete owner, not just its target ID.
            std::mem::forget(self);
        }
    }
    fn new(owner: &Rc<Node>) -> Result<Self> {
        let outer = Custom::new()?.into_node();
        let content = Custom::new()?.into_node();
        outer
            .borrow()
            .set_length_metric_unit(LengthMetricUnit::Vp)?;
        content
            .borrow()
            .set_length_metric_unit(LengthMetricUnit::Vp)?;
        outer.borrow_mut().add_child(content.clone())?;
        let target = NativeAttachment::for_subtree(&content, &owner.native)?;
        let route = ViewRoute::new(target.id(), Rc::downgrade(owner));
        let weak = Rc::downgrade(owner);
        outer.borrow_mut().on_custom_draw(move |event| {
            callback(|| {
                let Some(owner) = weak.upgrade() else {
                    return Ok(());
                };
                if owner.scene_root.get()
                    && let Some(raw) = event.draw_context_in_draw().and_then(|c| c.canvas())
                    && let Some(scene) = owner.scene.borrow().as_ref()
                {
                    // SAFETY: the canvas is borrowed for this ArkUI draw callback.
                    let canvas = unsafe { ohos_drawing_binding::Canvas::from_raw_borrowed(raw) };
                    scene.draw(owner.id, &canvas, owner.scale.get());
                }
                Ok(())
            });
        });
        let weak = Rc::downgrade(owner);
        outer.borrow_mut().on_custom_measure(move |event| {
            callback(|| {
                let Some(owner) = weak.upgrade() else {
                    return Ok(());
                };
                if let Some(modal) = owner.modal.borrow().as_ref() {
                    let size = modal.measure(event)?;
                    let mut frame = owner.raw_frame.get();
                    frame.width = size.width;
                    frame.height = size.height;
                    owner.set_frame(frame)?;
                }
                let frame = owner.frame.get();
                event.set_measured_size(
                    (frame.width * owner.scale.get()).round().max(1.0) as i32,
                    (frame.height * owner.scale.get()).round().max(1.0) as i32,
                )?;
                if let Some(wrapper) = owner.wrapper.borrow().as_ref() {
                    crate::render::layout::ChildMeasure::new(event)
                        .child(&wrapper.content.borrow())?;
                    if let Some(composite) = wrapper.composite.borrow().as_ref() {
                        composite.measure(event)?;
                    }
                }
                Ok(())
            });
        });
        let weak = Rc::downgrade(owner);
        content.borrow_mut().on_custom_measure(move |event| {
            callback(|| {
                let Some(owner) = weak.upgrade() else {
                    return Ok(());
                };
                if let Some(wrapper) = owner.wrapper.borrow().as_ref() {
                    let frame = owner.frame.get();
                    let insets = wrapper.insets();
                    event.set_measured_size(
                        ((frame.width as f64 + insets.left + insets.right)
                            * owner.scale.get() as f64)
                            .ceil()
                            .max(1.0) as i32,
                        ((frame.height as f64 + insets.top + insets.bottom)
                            * owner.scale.get() as f64)
                            .ceil()
                            .max(1.0) as i32,
                    )?;
                    if let Some(result) = wrapper.with_child(|node| {
                        crate::render::layout::ChildMeasure::new(event).child(node)
                    })? {
                        result?;
                    }
                }
                Ok(())
            });
        });
        let weak = Rc::downgrade(owner);
        outer.borrow_mut().on_custom_layout(move |event| {
            callback(|| {
                let position = event.position_in_layout();
                event.set_layout_position(position.x, position.y)?;
                if let Some(owner) = weak.upgrade()
                    && let Some(wrapper) = owner.wrapper.borrow().as_ref()
                {
                    let insets = wrapper.insets();
                    event.layout_child(
                        &wrapper.content.borrow(),
                        (-insets.left * owner.scale.get() as f64).round() as i32,
                        (-insets.top * owner.scale.get() as f64).round() as i32,
                    )?;
                    if let Some(composite) = wrapper.composite.borrow().as_ref() {
                        composite.layout(event)?;
                    }
                }
                Ok(())
            });
        });
        let weak = Rc::downgrade(owner);
        content.borrow_mut().on_custom_layout(move |event| {
            callback(|| {
                let position = event.position_in_layout();
                event.set_layout_position(position.x, position.y)?;
                if let Some(owner) = weak.upgrade()
                    && let Some(wrapper) = owner.wrapper.borrow().as_ref()
                    && let Some(result) =
                        wrapper.with_child(|node| event.layout_child(node, 0, 0))?
                {
                    result?;
                }
                Ok(())
            });
        });
        Ok(Self {
            closed: Cell::new(false),
            outer,
            content,
            target: RefCell::new(Some(target)),
            route: RefCell::new(Some(route)),
            context_live: Cell::new(true),
            owner: Rc::downgrade(owner),
            settings: RefCell::default(),
            composite: RefCell::default(),
            semantics: Cell::new(None),
            measured: Cell::new(None),
        })
    }
    fn insets(&self) -> crate::interop::sdk::TextureInsets {
        self.composite
            .borrow()
            .as_ref()
            .map_or_else(Default::default, Composite::insets)
    }
    fn update(&self, owner: &Node) -> Result<bool> {
        if self.closed.get() || owner.host.check().is_err() {
            return Ok(false);
        }
        self.ensure_context(owner)?;
        let Some(_) = self.target.borrow().as_ref().map(NativeAttachment::id) else {
            return Ok(false);
        };
        let texture_surface = self
            .composite
            .borrow()
            .as_ref()
            .and_then(Composite::surface_id);
        if self.composite.borrow().is_some() && texture_surface.is_none() {
            return Ok(false);
        }
        if self
            .settings
            .borrow()
            .as_ref()
            .is_some_and(|s| s.texture_surface != texture_surface)
        {
            self.replace_target(owner)?;
            self.settings.borrow_mut().take();
        }
        let target_id = self.target.borrow().as_ref().unwrap().id();
        let spec = owner.accessibility_state.spec.borrow();
        let frame = owner.frame.get();
        let value = crate::accessibility::value::spoken_value(&spec).unwrap_or_default();
        let actions: Vec<MenuItem> = if cfg!(feature = "api-26") {
            spec.actions
                .iter()
                .take(16)
                .map(|action| MenuItem {
                    kind: "action".into(),
                    name: action.name.to_string(),
                    label: action.label.as_deref().unwrap_or(&action.name).to_owned(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let settings = ViewOptions {
            texture_surface,
            texture_insets: Some(self.insets()),
            target_id,
            test_id: owner.interaction.test_id(),
            width: frame.width as f64,
            height: frame.height as f64,
            hue: 0.0,
            semantic: owns_semantics(&spec),
            clickable: owner.interaction.mask().contains(EventKind::Press)
                || spec
                    .actions
                    .iter()
                    .any(|action| action.name.as_ref() == "activate"),
            disabled: spec.state.disabled,
            accessible: spec.accessible,
            hidden: spec.hidden,
            label: owner.accessibility_state.label(owner),
            hint: spec.hint.as_deref().unwrap_or("").to_owned(),
            api23: cfg!(feature = "api-23") && !value.is_empty(),
            api26: !actions.is_empty(),
            value,
            role: role(spec.role).to_owned(),
            checked: spec.state.checked.map(|value| value == Checked::True),
            selected: spec.state.selected,
            actions,
            menu: owner.selection_menu(),
        };
        drop(spec);
        let mut previous = self.settings.borrow_mut();
        let changed = previous.as_ref() != Some(&settings);
        if changed {
            *previous = Some(settings.clone());
            owner.view_update(settings);
        }
        let replaced = if owner.scene_active() {
            !owner.scene_root.get()
                || owner
                    .scene
                    .borrow()
                    .as_ref()
                    .is_some_and(|s| s.has_output_for(owner.id))
        } else {
            self.composite
                .borrow()
                .as_ref()
                .is_some_and(Composite::is_revealed)
        };
        self.content
            .borrow()
            .opacity(if replaced { 0.0 } else { 1.0 })?;
        Ok(changed)
    }
    pub(crate) fn close(&self, owner: &Node) -> Result<()> {
        if self.closed.replace(true) {
            return Ok(());
        }
        self.detach_content()?;
        self.route.borrow_mut().take();
        if let Some(old) = self.target.borrow_mut().take() {
            owner.view_remove(old);
        }
        self.composite.borrow_mut().take();
        Ok(())
    }
    fn detach_content(&self) -> Result<()> {
        let index = self
            .outer
            .borrow()
            .children()
            .iter()
            .position(|child| Rc::ptr_eq(child, &self.content));
        if let Some(index) = index {
            self.outer.borrow_mut().remove_child(index)?;
        }
        Ok(())
    }
}
impl Drop for Wrapper {
    fn drop(&mut self) {
        // An attached ArkTS view retains content until its detach acknowledgement.
        // Otherwise this wrapper is its final owner and releases it here.
        if self.content.borrow().is_owned() && Rc::strong_count(&self.content) == 1 {
            callback(|| {
                self.content.borrow().remove_from_parent()?;
                self.content.borrow_mut().dispose()?;
                Ok(())
            });
        }
    }
}
impl Node {
    pub(crate) fn wrapper_owns_semantics(&self) -> bool {
        self.wrapper.borrow().is_some() && owns_semantics(&self.accessibility_state.spec.borrow())
    }
    pub(crate) fn accessibility_id(&self) -> Result<i32> {
        use ohos_arkui_binding::types::attribute::ArkUINodeAttributeType as A;
        let spec = self.accessibility_state.spec.borrow();
        if owns_semantics(&spec)
            && let Some(wrapper) = self.wrapper.borrow().as_ref()
            && let Some(id) =
                wrapper.with_child(|node| node.i32_attribute(A::AccessibilityId, 0))?
        {
            return Ok(id?);
        }
        if let Some(input) = self.input.borrow().as_ref()
            && let Some(id) = input.accessibility_id()?
        {
            return Ok(id);
        }
        Ok(self.native.borrow().i32_attribute(A::AccessibilityId, 0)?)
    }
    pub(crate) fn mounted(&self) -> NativeNode {
        self.wrapper
            .borrow()
            .as_ref()
            .map_or_else(|| self.native.clone(), |wrapper| wrapper.outer.clone())
    }
    pub(crate) fn ensure_wrapper(self: &Rc<Self>) -> Result<()> {
        if self
            .wrapper
            .borrow()
            .as_ref()
            .is_some_and(|wrapper| wrapper.closed.get())
        {
            return Ok(());
        }
        let spec = self.accessibility_state.spec.borrow();
        if cfg!(feature = "api-26") && spec.actions.len() > 16 {
            log::warn!("OHOS accessibility: showing the first 16 custom actions");
        }
        let has_semantics = owns_semantics(&spec);
        drop(spec);
        // Surface-wide effects and navigation shaders can start while their
        // native content is already visible. ArkUI texture export removes its
        // source from the display tree before producing the first buffer, so
        // changing a live surface between display and texture targets loses a
        // frame. Keep the surface's producer and presentation for its lifetime;
        // without effects ArkUI exports directly into the output queue.
        let requires_texture = self.id == nsh_core::ROOT_ID
            || self.kind == nsh_core::ViewKind::Modal
            || self.effects_plan().requires_texture;
        // Establish the root's layout host before it has scroll descendants.
        // Adding it around an already laid-out tree resets native scroll offsets.
        let needed = self.id == nsh_core::ROOT_ID
            || self.kind == nsh_core::ViewKind::Modal
            || self.selectable()
            || has_semantics
            || requires_texture;
        let mut created_wrapper = false;
        if needed && self.wrapper.borrow().is_none() {
            self.enable_layout();
            self.reset_visual_baseline(&self.native)?;
            let parent = self.mount_parent.borrow().upgrade();
            let index = parent
                .as_ref()
                .map(|parent| {
                    parent
                        .borrow()
                        .children()
                        .iter()
                        .position(|child| Rc::ptr_eq(child, &self.native))
                        .ok_or(crate::render::node::Error::InvalidNode(self.id))
                })
                .transpose()?;
            let wrapper = Wrapper::new(self)?;
            if let Some((parent, index)) = parent.zip(index) {
                let mut parent = parent.borrow_mut();
                if let Err(error) = parent.remove_child(index) {
                    wrapper.abort();
                    return Err(error.into());
                }
                if let Err(error) = parent.insert_child(wrapper.outer.clone(), index) {
                    let restored = parent.insert_child(self.native.clone(), index);
                    wrapper.abort();
                    restored?;
                    return Err(error.into());
                }
            }
            *self.wrapper.borrow_mut() = Some(wrapper);
            created_wrapper = true;
            self.accessibility_wrappers
                .set(self.accessibility_wrappers.get() + 1);
            self.invalidate_mounted_geometry();
            self.update_layout_bounds()?;
            // The wrapper owns this node's semantics; native descendants keep their own entries.
            #[cfg(feature = "api-23")]
            self.accessibility_provider.clear();
        }
        let mut created_composite = false;
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            if requires_texture && wrapper.composite.borrow().is_none() {
                *wrapper.composite.borrow_mut() = Some(Composite::new(self)?);
                created_composite = true;
            } else if !requires_texture {
                wrapper.composite.borrow_mut().take();
            }
        }
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            let spec = self.accessibility_state.spec.borrow();
            let semantic = owns_semantics(&spec);
            use ohos_arkui_binding::types::view::AccessibilityMode as Mode;
            let mode = if semantic && spec.accessible == Some(true) {
                // The ArkTS group is the single semantic entry for this subtree.
                Mode::DisabledForDescendants
            } else if semantic {
                Mode::Disabled
            } else if spec.hidden {
                Mode::DisabledForDescendants
            } else {
                match spec.accessible {
                    Some(true) => Mode::Enabled,
                    Some(false) => Mode::Disabled,
                    None => Mode::Auto,
                }
            };
            let next = (i32::from(mode), !semantic && spec.accessible == Some(true));
            // Style and text updates come here too: give the wrapped node
            // its semantics only when they change or its own props reset them.
            if wrapper.semantics.replace(Some(next)) != Some(next) {
                use ohos_arkui_binding::types::attribute::ArkUINodeAttributeType as A;
                let native = self.native.borrow();
                // The wrapper carries the test ID.
                native.reset_attribute(A::Id)?;
                native.set_i32_attribute(A::AccessibilityMode, next.0)?;
                native.set_bool_attribute(A::AccessibilityGroup, next.1)?;
            }
        }
        if created_wrapper || created_composite {
            self.apply_visual_style(&self.style.borrow())?;
        }
        self.update_wrapper()
    }
    pub(crate) fn update_wrapper(&self) -> Result<()> {
        if let Some(scroll) = self.scroll.borrow().as_ref()
            && let Some(plane) = scroll.overlay.borrow().as_ref()
        {
            plane.update(self)?;
        }
        if let Some(wrapper) = self
            .wrapper
            .borrow()
            .as_ref()
            .filter(|wrapper| !wrapper.closed.get())
        {
            let composite = if let Some(composite) = wrapper.composite.borrow().as_ref() {
                composite.update(self)?
            } else {
                false
            };
            let changed = wrapper.update(self)?;
            // The outer node measures at the frame's size (a modal at its
            // content's) and places the ArkTS view at the composite's origin:
            // measure again only when one of those, or what the view shows,
            // may have changed.
            let frame = self.frame.get();
            let size = Some((frame.width, frame.height));
            let resized = wrapper.measured.replace(size) != size;
            let modal = self.modal.borrow().is_some();
            if changed || composite || modal || resized {
                wrapper
                    .outer
                    .borrow()
                    .mark_dirty(NodeDirtyFlag::NeedMeasure)?;
            }
        }
        self.composite_dirty.set(false);
        Ok(())
    }
    pub(crate) fn wrapper_semantics_reset(&self) {
        if let Some(wrapper) = self.wrapper.borrow().as_ref() {
            wrapper.semantics.set(None);
        }
    }
    pub(crate) fn visual_node(&self) -> NativeNode {
        self.mounted()
    }
    pub(crate) fn composite_ready(&self, generation: u32) -> Result<()> {
        if let Some(scroll) = self.scroll.borrow().as_ref()
            && let Some(plane) = scroll.overlay.borrow().as_ref()
        {
            plane.ready(self, generation)?;
        }
        if let Some(wrapper) = self.wrapper.borrow().as_ref()
            && let Some(composite) = wrapper
                .composite
                .borrow()
                .as_ref()
                .filter(|c| c.generation == generation)
        {
            composite.ready();
            wrapper
                .outer
                .borrow()
                .mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        self.update_wrapper()?;
        // The frame is new input for enclosing effects, never for its own producer.
        if let Some(parent) = self.parent_node.borrow().upgrade() {
            parent.invalidate_composites();
        }
        self.invalidate_scene();
        Ok(())
    }
}
fn role(role: R) -> &'static str {
    match role {
        R::None => "",
        R::Button | R::ImageButton | R::KeyboardKey => "button",
        R::Checkbox => "checkbox",
        R::Switch => "switch",
        R::Radio => "radio",
        R::Adjustable | R::Scrollbar => "slider",
        R::Image => "image",
        R::Link => "link",
        R::List => "list",
        R::Grid => "grid",
        R::Progressbar => "progress",
        R::Search => "search",
        R::Menuitem => "menuitem",
        _ => "text",
    }
}
