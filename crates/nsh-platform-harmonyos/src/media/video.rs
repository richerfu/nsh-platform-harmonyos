//! C XComponent playback; a media worker owns decoding, bounded input and release.
mod controls;
mod icon;
mod mailbox;
mod player;
mod source;
mod ui;

use std::{
    cell::Cell,
    rc::{Rc, Weak},
    sync::Arc,
};

use mailbox::{RawWindow, VideoMailbox};
use nsh_core::{Command, VideoProps};
use ohos_arkui_binding::component::attribute::ArkUICommonAttribute;
use ohos_native_window_binding::NativeWindow;
pub(crate) use ui::{UiState, retired, update};

use crate::render::{
    geometry::Size,
    node::{NativeNode, Node, Result},
};

impl RawWindow for NativeWindow {
    fn raw_addr(&self) -> usize {
        self.raw() as usize
    }
}

pub(super) type SharedMailbox = Arc<VideoMailbox<NativeWindow>>;

pub(crate) struct Video {
    ui: Rc<ui::Ui>,
    mailbox: SharedMailbox,
    closed: Cell<bool>,
}
impl Video {
    pub(crate) fn native(&self) -> &NativeNode {
        &self.ui.native
    }
    /// Fullscreen is a UI-thread reparenting operation on the same player.
    pub(crate) fn set_fullscreen(&self, enabled: bool) -> Result<()> {
        self.ui.set_fullscreen(enabled)
    }
    /// Whether the surface root owns the player's native Stack.
    pub(crate) fn is_fullscreen(&self) -> bool {
        self.ui.is_fullscreen()
    }
    /// Root layout supplies the fullscreen size independently of the owner.
    pub(crate) fn fullscreen_frame(&self, size: Size) -> Result<()> {
        self.ui.fullscreen_frame(size)
    }
    pub(crate) fn new(owner: Weak<Node>) -> Result<Self> {
        let owner = owner.upgrade().expect("live Video owner");
        let mailbox = Arc::new(VideoMailbox::new());
        let ui = ui::Ui::new(&owner, mailbox.clone())?;
        let worker = match player::Worker::new(&owner, ui.token, mailbox.clone()) {
            Ok(worker) => worker,
            Err(error) => {
                ui.abort();
                return Err(error);
            }
        };
        let attached = { owner.native.borrow_mut().add_child(ui.native.clone()) };
        if let Err(error) = attached {
            ui.abort();
            return Err(error.into());
        }
        if let Err(error) = std::thread::Builder::new()
            .name("nsh-video".into())
            .spawn(move || worker.run())
        {
            mailbox.close();
            ui.abort();
            return Err(napi_ohos::Error::from_reason(error.to_string()).into());
        }
        Ok(Self {
            ui,
            mailbox,
            closed: Cell::new(false),
        })
    }
    pub(crate) fn configure(&self, props: Box<VideoProps>) -> Result<()> {
        if self.closed.get() {
            return Ok(());
        }
        self.ui.configure(&props)?;
        self.mailbox
            .configure(props)
            .map(|_| ())
            .map_err(|reason| napi_ohos::Error::from_reason(reason).into())
    }
    pub(crate) fn context(&self, ready: bool) -> Result<()> {
        if self.closed.get() {
            return Ok(());
        }
        let window = if ready {
            self.ui.current_window()?
        } else {
            None
        };
        self.mailbox.set_context(ready, window);
        Ok(())
    }
    pub(crate) fn command(&self, command: Command) -> Result<()> {
        if !self.closed.get()
            && let Command::Seek(seconds) = command
        {
            self.mailbox.seek(seconds);
        }
        Ok(())
    }
    pub(crate) fn tick(&self, now: f64) -> Result<bool> {
        if self.closed.get() {
            return Ok(false);
        }
        self.ui.tick(now)
    }
    pub(crate) fn frame(&self, size: Size) -> Result<()> {
        if self.closed.get() {
            return Ok(());
        }
        self.ui.frame(size)
    }
    pub(crate) fn close(&self) -> Result<()> {
        if !self.closed.replace(true) {
            // Retain the complete UI owner before the worker may enqueue its final ack.
            self.ui.begin_retirement();
            self.mailbox.close();
        }
        Ok(())
    }
}
impl Drop for Video {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            log::error!("OHOS Video close: {error}");
        }
    }
}
