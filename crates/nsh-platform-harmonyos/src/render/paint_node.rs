//! Retained custom ink. Unlike C node draw events, a ContentModifier keeps its
//! draw callback when Rosen moves the node between the window and texture trees.
use std::{cell::Cell, ffi::c_void, ptr::NonNull, rc::Rc};

use ohos_arkui_binding::{
    component::{attribute::ArkUICommonAttribute, built_in_component::Custom},
    types::{
        advanced::LengthMetricUnit, attribute::ArkUINodeAttributeType as A, view::HitTestMode,
    },
};
use ohos_drawing_binding::Canvas;

use super::node::{Error, NativeNode, Result, callback};

type Handle = *mut c_void;
type Draw = unsafe extern "C" fn(Handle, Handle);

// Public API 20 native_render.h APIs, not yet exposed by ohos-arkui-binding.
#[link(name = "ace_ndk.z")]
unsafe extern "C" {
    fn OH_ArkUI_RenderNodeUtils_CreateNode() -> Handle;
    fn OH_ArkUI_RenderNodeUtils_DisposeNode(node: Handle) -> i32;
    fn OH_ArkUI_RenderNodeUtils_AddRenderNode(node: Handle, child: Handle) -> i32;
    fn OH_ArkUI_RenderNodeUtils_RemoveRenderNode(node: Handle, child: Handle) -> i32;
    fn OH_ArkUI_RenderNodeUtils_SetSize(node: Handle, width: i32, height: i32) -> i32;
    fn OH_ArkUI_RenderNodeUtils_CreateContentModifier() -> Handle;
    fn OH_ArkUI_RenderNodeUtils_DisposeContentModifier(modifier: Handle);
    fn OH_ArkUI_RenderNodeUtils_AttachContentModifier(node: Handle, modifier: Handle) -> i32;
    fn OH_ArkUI_RenderNodeUtils_SetContentModifierOnDraw(
        modifier: Handle,
        data: Handle,
        draw: Draw,
    ) -> i32;
    fn OH_ArkUI_RenderNodeUtils_CreateFloatProperty(value: f32) -> Handle;
    fn OH_ArkUI_RenderNodeUtils_DisposeFloatProperty(property: Handle);
    fn OH_ArkUI_RenderNodeUtils_AttachFloatProperty(modifier: Handle, property: Handle) -> i32;
    fn OH_ArkUI_RenderNodeUtils_SetFloatPropertyValue(property: Handle, value: f32) -> i32;
    fn OH_ArkUI_DrawContext_GetCanvas(context: Handle) -> Handle;
}

fn check(status: i32) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(ohos_arkui_binding::common::error::ArkUIError::new(
            (status as u32).into(),
            "retained paint",
        )
        .into())
    }
}

struct Drawing(Box<dyn Fn(&Canvas)>);

unsafe extern "C" fn draw(context: Handle, data: Handle) {
    callback(|| {
        // SAFETY: the modifier runs on the UI thread; PaintNode keeps Drawing
        // alive until its callback has been replaced during teardown.
        let drawing = unsafe { &*data.cast::<Drawing>() };
        let raw = unsafe { OH_ArkUI_DrawContext_GetCanvas(context) };
        if let Some(raw) = NonNull::new(raw) {
            let canvas = unsafe { Canvas::from_raw_borrowed(raw) };
            (drawing.0)(&canvas);
        }
        Ok(())
    });
}
unsafe extern "C" fn detached(_: Handle, _: Handle) {}

pub(crate) struct PaintNode {
    pub(crate) native: NativeNode,
    render: NonNull<c_void>,
    modifier: NonNull<c_void>,
    property: NonNull<c_void>,
    _drawing: Box<Drawing>,
    changed: Cell<bool>,
    bounds: Cell<[i32; 4]>,
}

impl PaintNode {
    pub(crate) fn new(paint: impl Fn(&Canvas) + 'static) -> Result<Self> {
        let native = Custom::new()?.into_node();
        native
            .borrow()
            .set_length_metric_unit(LengthMetricUnit::Px)?;
        native
            .borrow()
            .set_attribute(A::HitTestBehavior, i32::from(HitTestMode::None).into())?;
        native.borrow().set_bool_attribute(A::Clip, false)?;
        let drawing = Box::new(Drawing(Box::new(paint)));
        // SAFETY: these UI-thread constructors return separately owned handles.
        unsafe {
            let render = NonNull::new(OH_ArkUI_RenderNodeUtils_CreateNode())
                .ok_or(Error::Unsupported("creating a paint render node"))?;
            let Some(modifier) = NonNull::new(OH_ArkUI_RenderNodeUtils_CreateContentModifier())
            else {
                OH_ArkUI_RenderNodeUtils_DisposeNode(render.as_ptr());
                return Err(Error::Unsupported("creating a paint modifier"));
            };
            let Some(property) = NonNull::new(OH_ArkUI_RenderNodeUtils_CreateFloatProperty(0.0))
            else {
                OH_ArkUI_RenderNodeUtils_DisposeContentModifier(modifier.as_ptr());
                OH_ArkUI_RenderNodeUtils_DisposeNode(render.as_ptr());
                return Err(Error::Unsupported("creating a paint property"));
            };
            let node = Self {
                native,
                render,
                modifier,
                property,
                changed: Cell::new(false),
                bounds: Cell::new([i32::MIN; 4]),
                _drawing: drawing,
            };
            check(OH_ArkUI_RenderNodeUtils_SetContentModifierOnDraw(
                modifier.as_ptr(),
                (&*node._drawing as *const Drawing).cast_mut().cast(),
                draw,
            ))?;
            check(OH_ArkUI_RenderNodeUtils_AttachContentModifier(
                render.as_ptr(),
                modifier.as_ptr(),
            ))?;
            // Custom properties acquire their target from the modifier's node
            // when attached. Attaching before the modifier has a node leaves
            // property updates unable to invalidate its recorded drawing.
            check(OH_ArkUI_RenderNodeUtils_AttachFloatProperty(
                modifier.as_ptr(),
                property.as_ptr(),
            ))?;
            check(OH_ArkUI_RenderNodeUtils_AddRenderNode(
                node.native.borrow().raw_handle().cast(),
                render.as_ptr(),
            ))?;
            Ok(node)
        }
    }

    pub(crate) fn bounds(&self, bounds: [i32; 4]) -> Result<()> {
        if self.bounds.get() == bounds {
            return Ok(());
        }
        let [x, y, w, h] = bounds;
        self.native
            .borrow()
            .set_layout_rect([0, 0, w.max(0), h.max(0)])?;
        self.native.borrow().set_position([x as f32, y as f32])?;
        // SAFETY: the render node belongs to this UI-thread owner.
        check(unsafe {
            OH_ArkUI_RenderNodeUtils_SetSize(self.render.as_ptr(), w.max(0), h.max(0))
        })?;
        self.bounds.set(bounds);
        self.invalidate()
    }

    pub(crate) fn invalidate(&self) -> Result<()> {
        let changed = !self.changed.get();
        // A modifier property is ArkUI's native invalidation mechanism. Only
        // content changes flip it; scrolling and VSync do not re-record ink.
        check(unsafe {
            OH_ArkUI_RenderNodeUtils_SetFloatPropertyValue(
                self.property.as_ptr(),
                u8::from(changed) as f32,
            )
        })?;
        self.changed.set(changed);
        Ok(())
    }
}

impl Drop for PaintNode {
    fn drop(&mut self) {
        // SAFETY: replace the callback before releasing its captured Rust state.
        // The native parent may retain the modifier until its next transaction.
        unsafe {
            let _ = OH_ArkUI_RenderNodeUtils_SetContentModifierOnDraw(
                self.modifier.as_ptr(),
                std::ptr::null_mut(),
                detached,
            );
            if self.native.borrow().is_owned() {
                let _ = OH_ArkUI_RenderNodeUtils_RemoveRenderNode(
                    self.native.borrow().raw_handle().cast(),
                    self.render.as_ptr(),
                );
            }
            OH_ArkUI_RenderNodeUtils_DisposeNode(self.render.as_ptr());
            OH_ArkUI_RenderNodeUtils_DisposeContentModifier(self.modifier.as_ptr());
            OH_ArkUI_RenderNodeUtils_DisposeFloatProperty(self.property.as_ptr());
        }
        if Rc::strong_count(&self.native) == 1 {
            callback(|| {
                self.native.borrow_mut().dispose()?;
                Ok(())
            });
        }
    }
}
