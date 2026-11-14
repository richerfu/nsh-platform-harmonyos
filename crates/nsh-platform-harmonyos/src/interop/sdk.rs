//! NSH-owned SDK fragments that have no equivalent C attribute or constructor.
//! Rust owns policy and native attachment targets retain SDK owners on the UI thread.

use std::{cell::RefCell, rc::Weak};

use napi_derive_ohos::napi;
use napi_ohos::{Env, Error, Result, bindgen_prelude::Object};
use rustc_hash::FxHashMap;

use crate::{
    interop::attachment::NativeAttachment,
    render::node::{NativeNode, Node},
};

#[derive(Clone, Debug, PartialEq)]
#[napi(object)]
pub(crate) struct EditorSettings {
    pub target_id: u32,
    pub multiline: bool,
    pub secure: bool,
    pub text: String,
    pub count: u32,
    pub width: f64,
    pub height: f64,
    pub placeholder: String,
    pub font_size: f64,
    pub font_weight: u32,
    pub font_family: Option<String>,
    pub italic: bool,
    pub color: u32,
    pub placeholder_color: u32,
    pub caret_color: u32,
    pub selection_color: u32,
    pub editable: bool,
    pub auto_correct: bool,
    pub report_paste: bool,
    pub scrollbar_visible: bool,
    pub hit_test: u32,
    pub align: u32,
    pub rtl: bool,
    pub capitalize: u32,
    pub keyboard: String,
    pub enter_key: String,
    pub blur_on_submit: bool,
    pub max_length: Option<u32>,
    pub auto_fill: bool,
    pub content_type: Option<String>,
    pub auto_focus: bool,
    pub accessibility: Option<EditorAccessibility>,
}

#[derive(Clone, Debug, PartialEq)]
#[napi(object)]
pub(crate) struct EditorAccessibility {
    pub label: String,
    pub hint: String,
    pub hidden: bool,
    pub accessible: Option<bool>,
    pub disabled: bool,
}

/// Focus, selection and scrolling operations ordered after this batch's state updates.
#[derive(Clone, Debug, Default, PartialEq)]
#[napi(object)]
pub(crate) struct EditorCommand {
    pub target_id: u32,
    pub kind: String,
    pub start: u32,
    pub end: u32,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub animated: bool,
    pub text: Option<String>,
    pub actions: Option<Vec<String>>,
}

#[derive(Default)]
#[napi(object)]
pub(crate) struct EditorEvent {
    pub kind: String,
    pub x: f64,
    pub y: f64,
    pub text: String,
    pub count: u32,
    pub start: u32,
    pub end: u32,
    pub width: f64,
    pub height: f64,
    pub html: Option<String>,
    pub files: Option<String>,
    pub image: Option<String>,
}

#[napi(object)]
pub(crate) struct EditorReceipt {
    pub accepted: bool,
    pub count: u32,
}

#[derive(Clone, Debug, PartialEq)]
#[napi(object)]
pub(crate) struct MenuItem {
    pub name: String,
    pub label: String,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq)]
#[napi(object)]
pub(crate) struct Menu {
    pub generation: u32,
    pub visible: bool,
    pub x: f64,
    pub y: f64,
    pub items: Vec<MenuItem>,
}

#[napi(object)]
pub(crate) struct MenuAction {
    pub target_id: u32,
    pub action: String,
    pub kind: Option<String>,
    pub generation: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
#[napi(object)]
pub(crate) struct TextureInsets {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
#[napi(object)]
pub(crate) struct ViewOptions {
    pub target_id: u32,
    pub width: f64,
    pub height: f64,
    pub hue: f64,
    pub texture_surface: Option<String>,
    pub texture_insets: Option<TextureInsets>,
    pub test_id: Option<String>,
    pub semantic: bool,
    pub clickable: bool,
    pub disabled: bool,
    pub accessible: Option<bool>,
    pub hidden: bool,
    pub label: String,
    pub hint: String,
    pub value: String,
    pub role: String,
    pub checked: Option<bool>,
    pub selected: bool,
    pub api23: bool,
    pub api26: bool,
    pub actions: Vec<MenuItem>,
    pub menu: Option<Menu>,
}

#[derive(Clone, Debug, PartialEq)]
#[napi(object)]
pub(crate) struct WebSettings {
    pub shared_cookies: bool,
    pub clear_cookies: bool,
    pub target_id: u32,
    pub url: Option<String>,
    pub html: Option<String>,
    pub html_base_url: Option<String>,
    pub load_generation: u32,
    pub java_script_enabled: bool,
    pub scroll_enabled: bool,
    pub user_agent: Option<String>,
    pub injected_javascript: Option<String>,
    pub document_start_script: String,
    pub download_directory: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
#[napi(object)]
pub(crate) struct WebHeader {
    pub header_key: String,
    pub header_value: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
#[napi(object)]
pub(crate) struct WebCommand {
    pub target_id: u32,
    pub kind: String,
    pub url: Option<String>,
    pub headers: Option<Vec<WebHeader>>,
    pub script: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
}

#[napi(object)]
pub(crate) struct WebEvent {
    pub target_id: u32,
    pub phase: String,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub message: Option<String>,
    pub code: Option<i32>,
    pub download: Option<WebDownload>,
}

#[napi(object)]
pub(crate) struct WebDownload {
    pub state: String,
    pub id: String,
    pub filename: String,
    pub path: String,
    pub bytes: f64,
}

// Only weak routes live here. Nodes, native targets and engines retain their own lifetime.
struct ViewRegistration {
    owner: Weak<Node>,
    inner: std::rc::Weak<std::cell::RefCell<ohos_arkui_binding::common::node::ArkUINode>>,
    export: std::rc::Rc<super::texture_export::Export>,
}
thread_local! {
    static VIEWS: RefCell<FxHashMap<u32, ViewRegistration>> = RefCell::default();
    static EDITORS: RefCell<FxHashMap<u32, Weak<crate::input::editor::Input>>> = RefCell::default();
}

pub(crate) struct ViewRoute {
    id: u32,
    owner: Weak<Node>,
    export: std::rc::Rc<super::texture_export::Export>,
}
impl ViewRoute {
    pub(crate) fn new(id: u32, owner: Weak<Node>, inner: &NativeNode) -> Self {
        let export = std::rc::Rc::new(super::texture_export::Export::default());
        VIEWS.with(|views| {
            views.borrow_mut().insert(
                id,
                ViewRegistration {
                    owner: owner.clone(),
                    inner: std::rc::Rc::downgrade(inner),
                    export: export.clone(),
                },
            )
        });
        Self { id, owner, export }
    }
    pub(crate) fn texture(&self, enabled: bool) {
        // A route is replaced at each export boundary. Subsequent style updates
        // may precede its deferred ArkTS attachment; they must not disarm repair.
        if enabled {
            self.export.enabled.set(true);
        }
    }
}
impl Drop for ViewRoute {
    fn drop(&mut self) {
        self.export.restore();
        VIEWS.with(|views| {
            let mut views = views.borrow_mut();
            if views
                .get(&self.id)
                .is_some_and(|route| route.owner.ptr_eq(&self.owner))
            {
                views.remove(&self.id);
            }
        });
    }
}

pub(crate) struct EditorRoute {
    id: u32,
}
impl EditorRoute {
    pub(crate) fn new(id: u32, input: Weak<crate::input::editor::Input>) -> Self {
        EDITORS.with(|routes| routes.borrow_mut().insert(id, input));
        Self { id }
    }
}
impl Drop for EditorRoute {
    fn drop(&mut self) {
        EDITORS.with(|routes| routes.borrow_mut().remove(&self.id));
    }
}

#[derive(Default, Clone)]
#[napi(object)]
pub(crate) struct Batch {
    pub editors: Vec<EditorSettings>,
    pub commands: Vec<EditorCommand>,
    pub remove_editors: Vec<u32>,
    pub views: Vec<ViewOptions>,
    pub remove_views: Vec<u32>,
    pub webs: Vec<WebSettings>,
    pub web_commands: Vec<WebCommand>,
    pub remove_webs: Vec<u32>,
}

impl Batch {
    pub(crate) fn is_empty(&self) -> bool {
        self.editors.is_empty()
            && self.commands.is_empty()
            && self.remove_editors.is_empty()
            && self.views.is_empty()
            && self.remove_views.is_empty()
            && self.webs.is_empty()
            && self.web_commands.is_empty()
            && self.remove_webs.is_empty()
    }
}

pub(crate) fn apply(
    context: &crate::render::mount::UiContext,
    batch: Batch,
) -> crate::render::node::Result<()> {
    context.host.apply_batch(&context.env, batch)?;
    Ok(())
}

#[napi]
pub(crate) fn nsh_attach_native_node(
    env: Env,
    target_id: u32,
    #[napi(ts_arg_type = "FrameNode")] content: Object<'_>,
) -> Result<()> {
    NativeAttachment::attach(&env, target_id, &content)
}

#[napi]
pub(crate) fn nsh_attach_native_subtree(
    env: Env,
    target_id: u32,
    #[napi(ts_arg_type = "FrameNode")] content: Object<'_>,
    #[napi(ts_arg_type = "NodeContent")] slot: Object<'_>,
) -> Result<()> {
    let (owner, inner, export) = VIEWS
        .with(|views| {
            views.borrow().get(&target_id).and_then(|route| {
                route
                    .owner
                    .upgrade()
                    .zip(route.inner.upgrade())
                    .map(|(owner, inner)| (owner, inner, route.export.clone()))
            })
        })
        .ok_or_else(|| Error::from_reason("Native subtree route is no longer live"))?;
    let is_direct = owner.is_current_direct_target(target_id);
    if is_direct && let Err(error) = owner.prepare_direct_target_attach() {
        log::warn!("OHOS pre-attach direct hold release failed for target {target_id}: {error}");
    }
    export
        .prepare(&owner, &inner)
        .map_err(|error| Error::from_reason(error.to_string()))?;
    if let Err(error) = NativeAttachment::attach_subtree(&env, target_id, &content, &slot) {
        export.restore();
        return Err(error);
    }
    if let Err(error) = owner.flush_pending_bridge_reattach_if_direct() {
        log::warn!("OHOS post-attach bridge reattach failed for target {target_id}: {error}");
    }
    if is_direct && let Err(error) = owner.restore_subtree_scroll_offsets(true) {
        log::warn!("OHOS post-attach scroll restore failed for target {target_id}: {error}");
    }
    if let Err(error) = owner.sync_subtree_scroll_offsets() {
        log::warn!("OHOS post-attach scroll sync failed for target {target_id}: {error}");
    }
    if let Err(error) = export.after_attach(&inner) {
        log::warn!("OHOS post-attach export repair failed for target {target_id}: {error}");
    }
    // Reparenting into or out of a texture export changes the render target
    // across the attachment and surface-export frames. ArkUI's retained custom
    // draw recordings must be rebuilt once the destination pipeline is live.
    if let Err(error) = owner.invalidate_subtree_paint() {
        log::warn!("OHOS post-attach subtree invalidate failed for target {target_id}: {error}");
    }
    if let Err(error) = owner.schedule_subtree_paint(6) {
        log::warn!("OHOS post-attach subtree paint failed for target {target_id}: {error}");
    }
    if is_direct {
        owner.notify_direct_target_attached();
    }
    Ok(())
}

#[napi]
pub(crate) fn nsh_retire_native_node(target_id: u32) -> Result<()> {
    NativeAttachment::retire(target_id)
}

#[napi]
pub(crate) fn nsh_editor_event(target_id: u32, event: EditorEvent) -> Result<EditorReceipt> {
    let count = NativeAttachment::dispatch(target_id, || {
        let input = EDITORS.with(|routes| routes.borrow().get(&target_id).and_then(Weak::upgrade));
        input
            .map(|input| {
                input
                    .native_event(event)
                    .map_err(|error| Error::from_reason(error.to_string()))
            })
            .transpose()
    })??;
    Ok(EditorReceipt {
        accepted: count.is_some(),
        count: count.unwrap_or(0),
    })
}

#[napi]
pub(crate) fn nsh_view_action(action: MenuAction) -> Result<bool> {
    NativeAttachment::dispatch(action.target_id, || {
        let owner = VIEWS.with(|views| {
            views
                .borrow()
                .get(&action.target_id)
                .and_then(|route| route.owner.upgrade())
        });
        owner.is_some_and(|owner| {
            let mut accepted = false;
            crate::render::node::callback(|| {
                if action.kind.is_some() {
                    accepted = owner.selection_action(&action)?;
                } else {
                    let spec = owner.accessibility_state.spec.borrow();
                    if !spec.hidden && !spec.state.disabled && owner.visible_in_tree() {
                        let registered = spec
                            .actions
                            .iter()
                            .any(|item| item.name.as_ref() == action.action);
                        drop(spec);
                        if registered {
                            owner.emit(
                                nsh_core::EventKind::AccessibilityAction,
                                nsh_core::EventPayload::Map(vec![(
                                    "actionName",
                                    nsh_core::PayloadValue::Str(action.action.clone()),
                                )]),
                            );
                            accepted = true;
                        } else if action.action == "activate"
                            && owner
                                .interaction
                                .mask()
                                .contains(nsh_core::EventKind::Press)
                        {
                            owner.emit(nsh_core::EventKind::Press, nsh_core::EventPayload::None);
                            accepted = true;
                        }
                    }
                }
                Ok(())
            });
            accepted
        })
    })
}

#[napi]
pub(crate) fn nsh_web_controller_attached(target_id: u32, native_tag: String) -> Result<bool> {
    crate::media::webview::on_controller_attached(target_id, &native_tag)
}

#[napi]
pub(crate) fn nsh_web_navigate(target_id: u32, url: String) -> Result<bool> {
    crate::media::webview::on_navigate(target_id, &url)
}

#[napi]
pub(crate) fn nsh_web_event(event: WebEvent) -> Result<bool> {
    crate::media::webview::on_event(event)
}

#[napi]
pub(crate) fn nsh_web_download_allowed(target_id: u32, url: String) -> napi_ohos::Result<bool> {
    crate::media::webview::download_allowed(target_id, &url)
}
