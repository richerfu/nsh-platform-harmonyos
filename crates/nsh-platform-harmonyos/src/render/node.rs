//! Main-thread ArkUI nodes. Callbacks hold weak references; the mounter owns
//! the nodes through detachment and frame-delayed retirement.

mod layout;
mod lifecycle;
mod motion;
mod paint;
mod pointer;
mod style;
mod transform;

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use nsh_core::{NodeId, ViewKind, ViewStyle};
use ohos_arkui_binding::common::node::ArkUINode;

use crate::{
    input::{editor::Input, events::Interaction},
    render::geometry::{Rect, Size},
    scroll::controller::Scroller,
};

pub(crate) type NativeNode = Rc<RefCell<ArkUINode>>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("OHOS mount tree: {0}")]
    Tree(Box<Error>),
    #[error("invalid OHOS mount node {0}")]
    InvalidNode(NodeId),
    #[error("{0} at {1}")]
    ArkUi(
        ohos_arkui_binding::common::error::ArkUIError,
        &'static std::panic::Location<'static>,
    ),
    #[cfg(feature = "api-23")]
    #[error(transparent)]
    Accessibility(#[from] ohos_accessibility_binding::AccessibilityError),
    #[error(transparent)]
    Host(#[from] napi_ohos::Error),
    #[error("OHOS native VSync request failed: {0}")]
    Vsync(i32),
    #[error("OHOS backend does not support {0}")]
    Unsupported(&'static str),
}
impl From<ohos_arkui_binding::common::error::ArkUIError> for Error {
    #[track_caller]
    fn from(err: ohos_arkui_binding::common::error::ArkUIError) -> Self {
        Self::ArkUi(err, std::panic::Location::caller())
    }
}
pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy)]
struct AppliedStyle {
    visual_ptr: usize,
    background_color: u32,
    border_radius: [f32; 4],
    border_width: [f32; 4],
    border_color: [u32; 4],
    border_style: i32,
    shadow: Option<(f32, f32, f32, u32)>,
    ink_outset: f32,
    clip: bool,
    clip_shape: Option<(Size, [f32; 4])>,
    backdrop_blur: f32,
    visibility: i32,
    transform: Option<[f32; 16]>,
    transform_center: [f32; 3],
    blur: f32,
    blend: nsh_core::BlendMode,
    z_index: i32,
}
impl Default for AppliedStyle {
    fn default() -> Self {
        Self {
            visual_ptr: 0,
            background_color: 0,
            border_radius: [0.0; 4],
            border_width: [0.0; 4],
            border_color: [0xff000000; 4],
            border_style: 0,
            shadow: None,
            ink_outset: 0.0,
            clip: false,
            clip_shape: None,
            backdrop_blur: 0.0,
            visibility: 0,
            transform: None,
            transform_center: [f32::NAN; 3],
            blur: 0.0,
            blend: nsh_core::BlendMode::Normal,
            z_index: 0,
        }
    }
}

pub(crate) struct Node {
    weak: Weak<Node>,
    pub(crate) accessibility_state: crate::accessibility::state::State,
    #[cfg(feature = "api-23")]
    pub(crate) accessibility_provider: crate::accessibility::provider::State,
    pub(crate) native: NativeNode,
    pub(crate) wrapper: RefCell<Option<crate::interop::view::Wrapper>>,
    pub(crate) mount_parent: RefCell<Weak<RefCell<ArkUINode>>>,
    pub(crate) id: NodeId,
    pub(crate) kind: ViewKind,
    pub(crate) pending_delete: Cell<bool>,
    pub(crate) scene_prepared: Cell<bool>,
    pub(crate) scene_root: Cell<bool>,
    /// Effects applied to this node's exported native subtree, in mount order.
    pub(crate) shader_layers: RefCell<Vec<(NodeId, std::sync::Arc<nsh_core::shader::ShaderLayer>)>>,
    pub(crate) navigation_capture: Cell<bool>,
    pub(crate) composite_dirty: Cell<bool>,
    capture_cache: Cell<Option<super::composition::geometry::Bounds>>,
    pub(crate) navigation_shader: RefCell<Option<nsh_core::NavShader>>,
    pub(crate) modal: RefCell<Option<Rc<crate::navigation::modal::Modal>>>,
    pub(crate) screen: RefCell<Option<Rc<crate::navigation::stack::Screen>>>,
    pub(crate) raw_frame: Cell<Rect>,
    pub(crate) frame: Cell<Rect>,
    pub(crate) parent: Cell<Option<NodeId>>,
    pub(crate) parent_node: RefCell<Weak<Node>>,
    pub(crate) children: RefCell<Vec<Weak<Node>>>,
    context: Rc<crate::render::mount::UiContext>,
    pub(crate) input: RefCell<Option<Rc<Input>>>,
    pub(crate) scroll: RefCell<Option<Rc<Scroller>>>,
    pub(crate) canvas: RefCell<Option<crate::render::canvas::CanvasView>>,
    pub(crate) image: RefCell<Option<Rc<crate::media::image::ImageView>>>,
    pub(crate) component: RefCell<Option<crate::interop::component::Instance>>,
    pub(crate) component_slots: RefCell<Vec<std::sync::Arc<str>>>,
    pub(crate) component_animated: RefCell<Vec<Option<nsh_core::animation::AnimatedPropValue>>>,
    pub(crate) video: RefCell<Option<crate::media::video::Video>>,
    pub(crate) web: RefCell<Option<crate::media::webview::WebView>>,
    pub(crate) active: Cell<bool>,
    pub(crate) export_repair: Cell<u32>,
    pub(crate) clearing_export: Cell<bool>,
    pub(crate) bridged_on_root_outer: Cell<bool>,
    pub(crate) retained: Cell<bool>,
    pub(crate) retained_owner: RefCell<Weak<Node>>,
    pub(crate) retained_children: RefCell<Vec<Weak<Node>>>,
    backface_away: Cell<bool>,
    pub(crate) interaction: Interaction,
    pub(crate) style: RefCell<Box<ViewStyle>>,
    pub(crate) animated_opacity: Cell<Option<f32>>,
    applied: Cell<AppliedStyle>,
    styled: Cell<bool>,
    paint_state: Cell<super::subtree_paint::SubtreePaint>,
    pub(crate) live: RefCell<Option<Box<crate::render::transition::Live>>>,
    /// Leaving (an exit transition, a popped screen): changed only through
    /// [`Node::set_leaving`], which keeps the surface's count of them.
    leaving: Cell<bool>,
    extra: Cell<(f32, crate::render::transition::Extra)>,
    navigation_presentation: Cell<(f32, f32)>,
    decoration: crate::render::decor::Decoration,
    layout_callbacks: Cell<bool>,
    drawing: Cell<bool>,
    outlined: Cell<bool>,
    shadow_layer: RefCell<Option<Rc<super::shadow::ShadowLayer>>>,
    pub(crate) text_drawing: RefCell<Option<crate::text::typography::TextDrawing>>,
    pub(crate) selection_area: RefCell<Option<crate::text::selection::area::Area>>,
    pub(crate) raster: RefCell<Option<nsh_core::TextLayout>>,
    pub(crate) selection: crate::text::selection::state::State,
    tint: Cell<Option<nsh_core::Color>>,
    pub(crate) content: Cell<Rect>,
    pub(crate) overflow: Cell<Option<Rect>>,
    raster_origin_y: Cell<f32>,
    presentation: Cell<(f32, f32)>,
    pinned: Cell<(f32, f32)>,
    input_transform: Cell<nsh_core::Matrix>,
    applied_layout_size: Cell<(usize, i32, i32)>,
    applied_position: Cell<(usize, i32, i32)>,
    has_text_links: Cell<bool>,
}

impl std::ops::Deref for Node {
    type Target = crate::render::mount::UiContext;
    fn deref(&self) -> &Self::Target {
        &self.context
    }
}

pub(crate) fn callback(f: impl FnOnce() -> Result<()>) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => log::error!("OHOS callback: {error}"),
        Err(_) => log::error!("panic in an OHOS native callback"),
    }
}
