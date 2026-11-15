//! OHOS host session binding. Each window owns its own `Services` session,
//! bound explicitly from its `UIContext` without global single-window state.

use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};

use napi_derive_ohos::napi;
use napi_ohos::{
    Env, Error, JsValue, Result, Status,
    bindgen_prelude::{FromNapiValue, Function, FunctionRef, Object, Unknown},
};
use ohos_resource_manager_binding::ResourceManager;

use crate::app::{
    environment::NshEnvironment,
    runtime::Message,
    session::{AvoidArea, HostCommand, HostInit, Services},
};

/// Capabilities available to the embedding host on this runtime.
#[napi(object)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NshCapabilities {
    /// Actual system API level, independent of surface environment overrides.
    pub api_level: u32,
    /// Native Shell's minimum supported system API level.
    pub minimum_api_level: u32,
    /// API-23 support was included in this library.
    pub compiled_api23: bool,
    /// API-26 support was included in this library.
    pub compiled_api26: bool,
    /// API-23 enhancements may be used on this runtime.
    pub api23: bool,
    /// API-26 enhancements may be used on this runtime.
    pub api26: bool,
}

static RUNTIME_API_VERSION: AtomicI32 = AtomicI32::new(0);

fn set_runtime_api_version(version: i32) {
    RUNTIME_API_VERSION.store(version.max(0), Ordering::Relaxed);
}

/// Returns compiled enhancements intersected with the running system API level.
#[napi]
pub fn get_capabilities() -> NshCapabilities {
    let mut version = RUNTIME_API_VERSION.load(Ordering::Relaxed);
    if version <= 0 {
        version = ohos_device_binding::sdk_api_version() as i32;
        if version > 0 {
            RUNTIME_API_VERSION.store(version, Ordering::Relaxed);
        }
    }
    let capabilities = crate::app::features::Capabilities::for_api(version.max(0) as u32);
    NshCapabilities {
        api_level: capabilities.api_level,
        minimum_api_level: capabilities.minimum_api_level,
        compiled_api23: capabilities.compiled_api23,
        compiled_api26: capabilities.compiled_api26,
        api23: capabilities.api23,
        api26: capabilities.api26,
    }
}

thread_local! {
    static SESSION_BINDER: RefCell<Option<FunctionRef<(), ()>>> = RefCell::default();
}

#[napi]
pub(crate) fn nsh_register_session_binder(
    sdk_api_version: i32,
    #[napi(ts_arg_type = "(uiContext: object) => number")] binder: Function<'_, (), ()>,
) -> Result<()> {
    set_runtime_api_version(sdk_api_version);
    crate::media::webview::register_custom_scheme();
    let reference = binder.create_ref()?;
    SESSION_BINDER.with(|slot| {
        *slot.borrow_mut() = Some(reference);
    });
    Ok(())
}

pub(crate) fn bind_session(env: &Env, ui_context: &Object<'_>) -> Result<Arc<Services>> {
    let has_binder = SESSION_BINDER.with(|slot| slot.borrow().is_some());
    if !has_binder {
        let _ = env.load("@nsh/harmonyos");
    }
    let binder = SESSION_BINDER
        .with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|reference| reference.borrow_back(env))
                .transpose()
        })?
        .ok_or_else(|| Error::from_reason("@nsh/harmonyos session binder is not registered"))?;
    let arg = ui_context.raw();
    let mut raw_result = std::ptr::null_mut();
    // SAFETY: all handles belong to this UI-thread environment and remain live
    // for the synchronous call; raw_result receives its owned N-API value.
    let status = unsafe {
        napi_ohos::sys::napi_call_function(
            env.raw(),
            binder.raw(),
            binder.raw(),
            1,
            &arg,
            &mut raw_result,
        )
    };
    if status != napi_ohos::sys::Status::napi_ok {
        return Err(Error::from_status(Status::from(status)));
    }
    // SAFETY: a successful call initialized raw_result in this environment;
    // FromNapiValue checks the binder's session ID before converting it.
    let session_id = unsafe { u32::from_napi_value(env.raw(), raw_result) }?;
    crate::app::session::get(session_id)
}

#[napi]
pub(crate) fn nsh_create_session(
    init: HostInit,
    platform_window_id: i32,
    #[napi(ts_arg_type = "object")] resource_manager: Unknown<'_>,
    #[napi(ts_arg_type = "(batch: Batch) => void")] apply_batch: Function<
        '_,
        crate::interop::sdk::Batch,
        (),
    >,
    #[napi(ts_arg_type = "(command: HostCommand) => void")] on_command: Function<
        '_,
        HostCommand,
        (),
    >,
    #[napi(ts_arg_type = "(context: TabBarContext) => object")] tab_bar: Function<
        '_,
        crate::interop::component::TabBarContext,
        napi_ohos::bindgen_prelude::ObjectRef,
    >,
) -> Result<u32> {
    if init.sdk_api_version < crate::app::features::MIN_API as i32 {
        return Err(Error::from_reason(format!(
            "Native Shell requires OHOS API {} or later",
            crate::app::features::MIN_API
        )));
    }
    set_runtime_api_version(init.sdk_api_version);
    crate::media::webview::register_custom_scheme();
    let resource_manager = ResourceManager::from_napi_value(resource_manager)?;
    let applier = apply_batch.create_ref()?;
    let command_tsfn = on_command
        .build_threadsafe_function::<HostCommand>()
        .callee_handled::<false>()
        .build()?;
    Services::create(
        init,
        platform_window_id,
        resource_manager,
        applier,
        command_tsfn,
        tab_bar.create_ref()?,
    )
}

#[napi]
pub(crate) fn nsh_close_session(session_id: u32) {
    crate::app::session::close_ui_session(session_id);
}

#[napi]
pub(crate) fn nsh_set_lifecycle(session_id: u32, lifecycle: String) -> Result<()> {
    if let Ok(host) = crate::app::session::get(session_id) {
        host.update(NshEnvironment {
            lifecycle: Some(lifecycle),
            ..Default::default()
        })?;
    }
    Ok(())
}

#[napi]
pub(crate) fn nsh_update_keyboard(session_id: u32, duration: f64) -> Result<()> {
    crate::app::session::get(session_id)?.update(NshEnvironment {
        keyboard_duration: Some(duration),
        ..Default::default()
    })
}

#[napi]
#[allow(clippy::too_many_arguments)]
pub(crate) fn nsh_update_config(
    session_id: u32,
    color_mode: i32,
    font_size_scale: f64,
    language: String,
    rtl: bool,
    device_type: Option<String>,
    pixel_ratio: Option<f64>,
    languages: Option<Vec<String>>,
    time_zone: Option<String>,
    regional_locale: Option<String>,
    is_24_hour_clock: Option<bool>,
) -> Result<()> {
    if let Ok(host) = crate::app::session::get(session_id) {
        let mut environment = crate::app::session::environment_from_config(
            color_mode,
            font_size_scale,
            language,
            device_type.as_deref(),
            rtl,
            languages,
            time_zone,
            regional_locale,
            is_24_hour_clock,
        );
        environment.pixel_ratio = pixel_ratio;
        host.update(environment)?;
    }
    Ok(())
}

#[napi]
pub(crate) fn nsh_update_accessibility(
    session_id: u32,
    screen_reader: Option<bool>,
    reduce_motion: Option<bool>,
) -> Result<()> {
    if let Ok(host) = crate::app::session::get(session_id) {
        host.update(NshEnvironment {
            screen_reader,
            reduce_motion,
            ..Default::default()
        })?;
    }
    Ok(())
}

#[napi]
pub(crate) fn nsh_update_window(
    session_id: u32,
    platform_window_id: i32,
    visual: Vec<AvoidArea>,
    keyboard: AvoidArea,
    max_refresh_rate: Option<u32>,
) -> Result<()> {
    if let Ok(host) = crate::app::session::get(session_id) {
        if let Some(hz) = max_refresh_rate {
            host.set_max_refresh_rate(hz);
        }
        let visual = [
            visual.first().copied().unwrap_or_default(),
            visual.get(1).copied().unwrap_or_default(),
            visual.get(2).copied().unwrap_or_default(),
        ];
        host.update_window(platform_window_id, visual, keyboard)?;
    }
    Ok(())
}

#[napi]
pub(crate) fn nsh_open_url_result(session_id: u32, open_id: u32, opened: bool) {
    if let Ok(host) = crate::app::session::get(session_id) {
        host.complete_open_url(open_id, opened);
    }
}

#[napi]
pub(crate) fn nsh_pick_images_result(
    session_id: u32,
    pick_id: u32,
    files: Vec<crate::services::system::PickedDescriptor>,
) {
    let files = files
        .into_iter()
        .take(nsh_core::picker::MAX_PICKED as usize)
        .filter_map(|file| {
            file.retain()
                .map_err(|error| log::warn!("OHOS picked file: {error}"))
                .ok()
        })
        .collect();
    if let Ok(host) = crate::app::session::get(session_id) {
        host.complete_pick(pick_id, files);
    }
}

#[napi]
pub(crate) fn nsh_report_host_error(session_id: u32, message: String) {
    if let Ok(host) = crate::app::session::get(session_id) {
        host.broadcast(|| Message::Error(message.clone()));
    }
}

#[napi]
pub(crate) fn on_back_press_intercept(
    env: Env,
    #[napi(ts_arg_type = "import('@ohos.arkui.UIContext').UIContext")] ui_context: Object<'_>,
) -> bool {
    bind_session(&env, &ui_context).is_ok_and(|host| host.handle_back())
}

#[napi]
pub(crate) fn nsh_service_event(
    session_id: u32,
    key: u32,
    value: crate::services::system::SystemEvent,
) {
    if let Ok(host) = crate::app::session::get(session_id) {
        host.system.complete(key, value);
    }
}
