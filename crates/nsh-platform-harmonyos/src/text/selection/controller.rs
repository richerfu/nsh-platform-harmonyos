//! Surface-local text selection. UI callbacks consume owned geometry, never typography.
use std::{ops::Range, rc::Rc};

use ohos_arkui_binding::types::advanced::NodeDirtyFlag;
use ohos_drawing_binding::{Brush, Canvas, Point, Rect};

use super::area::{Area, project};
use crate::{
    interop::sdk::{Menu as ViewMenu, MenuAction as NativeViewAction, MenuItem as ViewAction},
    render::node::{Node, Result},
    text::{
        selection::layout::Layout,
        typography::{NativeText, argb},
    },
};

pub(crate) enum Command {
    Copy,
    SelectAll,
    Clear,
}
impl Node {
    fn selection_layout<T>(&self, f: impl FnOnce(&Layout) -> T) -> Option<T> {
        let raster = self.raster.borrow();
        let layout = raster
            .as_ref()?
            .0
            .downcast_ref::<NativeText>()?
            .selection
            .as_ref()?;
        Some(f(layout))
    }
    pub(crate) fn selectable(&self) -> bool {
        let raster = self.raster.borrow();
        let Some(text) = raster
            .as_ref()
            .and_then(|r| r.0.downcast_ref::<NativeText>())
        else {
            return false;
        };
        if text.selection.is_none() {
            return false;
        }
        if text.selectable || self.interaction.selectable_view() {
            return true;
        }
        let mut parent = self.parent_node.borrow().upgrade();
        while let Some(node) = parent {
            if node.interaction.selectable_view() {
                return true;
            }
            parent = node.parent_node.borrow().upgrade();
        }
        false
    }
    pub(crate) fn selection_configure(self: &Rc<Self>) -> Result<()> {
        if self.raster.borrow().is_some() {
            if self.selectable() {
                self.ensure_wrapper()?;
            }
            self.set_events(self.interaction.mask())?;
        }
        for child in self
            .children
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
        {
            child.selection_configure()?;
        }
        Ok(())
    }
    pub(crate) fn has_selection(&self) -> bool {
        self.selection_area
            .borrow()
            .as_ref()
            .is_some_and(|a| !a.model.is_empty())
    }
    pub(crate) fn selection_refresh(&self) -> Result<()> {
        if !self.selectable() {
            return self.selection_clear();
        }
        if let Some(area) = self.selection_area.borrow_mut().as_mut() {
            area.update(self, |_, _, _| {})?;
        }
        if !self.has_selection() {
            return self.selection_clear();
        }
        self.update_wrapper()
    }
    fn selection_range(&self) -> Option<Range<usize>> {
        self.selection.range.get().map(|(start, end)| start..end)
    }
    pub(crate) fn clear_text_selection(&self) -> Result<()> {
        let old = self
            .selected_text
            .borrow_mut()
            .take()
            .and_then(|n| n.upgrade());
        if let Some(old) = old {
            old.selection_clear()?;
        }
        Ok(())
    }
    pub(crate) fn selection_clear(&self) -> Result<()> {
        if self.selection.range.get().is_none() && !self.has_selection() {
            return Ok(());
        }
        let owner = self
            .selected_text
            .borrow_mut()
            .take()
            .and_then(|n| n.upgrade());
        let owner = owner.as_deref().unwrap_or(self);
        if let Some(mut area) = owner.selection_area.borrow_mut().take() {
            area.clear(owner)?;
        }
        if let Some(route) = owner.dispatcher.upgrade() {
            route.keyboard.lock().unwrap().selection = None;
        }
        owner.selection.menu.set(false);
        owner.selection_clear_local()?;
        owner.update_wrapper()
    }
    pub(crate) fn selection_clear_local(&self) -> Result<()> {
        if self.selection.range.take().is_some() {
            self.selection.invalidate_menu();
            self.selection.handles.set([false; 2]);
            *self.selection.geometry.borrow_mut() = Default::default();
            self.clear_drawn_export_target();
            self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        }
        Ok(())
    }
    pub(crate) fn selection_begin(self: &Rc<Self>, x: f32, y: f32) -> Result<bool> {
        if !self.selectable()
            || !self.visible_in_tree()
            || self.accessibility_state.spec.borrow().state.disabled
        {
            return Ok(false);
        }
        self.clear_text_selection()?;
        let mut area = Area::new(&Area::root(self));
        area.update(self, |model, texts, matrix| {
            model.select_word_at(texts, project(matrix, (x, y)));
        })?;
        if area.model.is_empty() {
            return Ok(false);
        }
        *self.selection_area.borrow_mut() = Some(area);
        *self.selected_text.borrow_mut() = Some(Rc::downgrade(self));
        if let Some(route) = self.dispatcher.upgrade() {
            route.keyboard.lock().unwrap().selection = Some(self.id);
        }
        self.selection_menu_show(true)?;
        self.touches.borrow_mut().selection_started(self, (x, y))?;
        Ok(true)
    }
    pub(crate) fn selection_set(&self, range: Range<usize>) -> Result<()> {
        if self.selection.range.get() == Some((range.start, range.end)) {
            return Ok(());
        }
        {
            let mut geometry = self.selection.geometry.borrow_mut();
            if self
                .selection_layout(|l| l.geometry(range.clone(), &mut geometry))
                .is_none()
            {
                return Ok(());
            }
        }
        self.selection.invalidate_menu();
        self.selection.range.set(Some((range.start, range.end)));
        self.clear_drawn_export_target();
        self.native.borrow().mark_dirty(NodeDirtyFlag::NeedRender)?;
        Ok(())
    }
    pub(crate) fn selection_extend(&self, point: (f32, f32)) -> Result<()> {
        if let Some(area) = self.selection_area.borrow_mut().as_mut() {
            area.update(self, |model, texts, matrix| {
                model.drag(texts, project(matrix, point));
            })?;
        }
        self.update_wrapper()
    }
    pub(crate) fn selection_handle(&self, point: (f32, f32)) -> Result<bool> {
        let mut area = self.selection_area.borrow_mut();
        let Some(area) = area.as_mut() else {
            return Ok(false);
        };
        let handle = area
            .handles(self)?
            .into_iter()
            .filter_map(|(handle, (x, top, bottom))| {
                let dx = point.0 - x;
                let dy = point.1 - (bottom + 5.0);
                (dx.abs() <= 22.0 && point.1 >= top - 12.0 && dy.abs() <= 28.0).then_some((
                    dx * dx + dy * dy,
                    handle,
                    (x, bottom),
                ))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0));
        let Some((_, handle, at)) = handle else {
            return Ok(false);
        };
        area.update(self, |model, texts, matrix| {
            model.move_handle(texts, handle, project(matrix, at));
        })?;
        Ok(true)
    }
    pub(crate) fn selection_menu_show(&self, visible: bool) -> Result<()> {
        if visible && self.has_selection() {
            if !self.selection.open_menu(true) {
                return Ok(());
            }
        } else {
            self.selection.menu.set(false);
        }
        self.update_wrapper()
    }
    pub(crate) fn selection_menu(&self) -> Option<ViewMenu> {
        // A selectable block keeps its hidden menu; disabling selection removes its actions.
        if !self.selectable() && !self.has_selection() {
            return None;
        }
        let anchor = self
            .selection_area
            .borrow()
            .as_ref()
            .and_then(|a| a.handles(self).ok())
            .and_then(|h| h.first().map(|(_, p)| (p.0, p.1)))
            .unwrap_or((0.0, 0.0));
        Some(ViewMenu {
            generation: self.selection.generation.get().max(1),
            visible: self.selection.menu.get()
                && self.has_selection()
                && !self.accessibility_state.spec.borrow().state.disabled
                && self.visible_in_tree(),
            x: anchor.0.clamp(-16384.0, 16384.0) as f64,
            y: anchor.1.clamp(-16384.0, 16384.0) as f64,
            items: {
                let mut items = vec![ViewAction {
                    kind: "action".into(),
                    name: "copy".into(),
                    label: "Copy".into(),
                }];
                items.push(ViewAction {
                    kind: "action".into(),
                    name: "select-all".into(),
                    label: "Select All".into(),
                });
                items
            },
        })
    }
    pub(crate) fn selection_action(&self, event: &NativeViewAction) -> Result<bool> {
        if !self.visible_in_tree()
            || self.accessibility_state.spec.borrow().state.disabled
            || event.generation != Some(self.selection.generation.get())
        {
            return Ok(false);
        }
        if !self.has_selection() {
            return Ok(false);
        }
        match event.kind.as_deref() {
            Some("menu-dismiss") => {
                self.selection.menu.set(false);
                self.update_wrapper()?;
            }
            Some("menu") if event.action == "copy" => {
                self.selection_command(Command::Copy)?;
            }
            Some("menu") if event.action == "select-all" => {
                self.selection_command(Command::SelectAll)?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    pub(crate) fn selection_command(&self, command: Command) -> Result<()> {
        if !self.visible_in_tree() || self.accessibility_state.spec.borrow().state.disabled {
            return Ok(());
        }
        if !self.has_selection() {
            return Ok(());
        }
        match command {
            Command::Copy => {
                let text = self
                    .selection_area
                    .borrow()
                    .as_ref()
                    .map(|a| a.text())
                    .transpose()?
                    .unwrap_or_default();
                self.host.system.command(
                    &self.host,
                    self.dispatcher.clone(),
                    nsh_core::Command::SetClipboard(text),
                )?;
                self.selection_clear()?;
            }
            Command::SelectAll => {
                if let Some(area) = self.selection_area.borrow_mut().as_mut() {
                    area.update(self, |model, texts, _| {
                        model.select_all(texts);
                    })?;
                }
                self.selection_menu_show(false)?;
            }
            Command::Clear => self.selection_clear()?,
        }
        Ok(())
    }
    pub(crate) fn draw_selection(&self, canvas: &Canvas, handles: bool) {
        if self.selection_range().is_none() {
            return;
        }
        let content = self.content.get();
        let color = self
            .raster
            .borrow()
            .as_ref()
            .and_then(|r| r.0.downcast_ref::<NativeText>())
            .and_then(|r| r.selection_color)
            .unwrap_or(nsh_core::Color(0x007aff55));
        let mut brush = Brush::new();
        brush.set_color(argb(if handles {
            nsh_core::Color(color.0 | 0xff)
        } else {
            color
        }));
        brush.set_anti_alias(true);
        canvas.attach_brush(&brush);
        let geometry = self.selection.geometry.borrow();
        if handles {
            if let Some(edges) = geometry.handles {
                for (index, (x, top, bottom)) in edges.into_iter().enumerate() {
                    if !self.selection.handles.get()[index] {
                        continue;
                    }
                    let x = (content.x + x) * self.scale.get();
                    let top = (content.y + top) * self.scale.get();
                    let bottom = (content.y + bottom) * self.scale.get();
                    canvas.draw_rect(&Rect::new(
                        x - self.scale.get(),
                        top,
                        x + self.scale.get(),
                        bottom,
                    ));
                    canvas.draw_circle(
                        &Point::new(x, bottom + 5.0 * self.scale.get()),
                        5.0 * self.scale.get(),
                    );
                }
            }
        } else {
            for r in &geometry.rectangles {
                canvas.draw_rect(&Rect::new(
                    (content.x + r[0]) * self.scale.get(),
                    (content.y + r[1]) * self.scale.get(),
                    (content.x + r[2]) * self.scale.get(),
                    (content.y + r[3]) * self.scale.get(),
                ));
            }
        }
        canvas.detach_brush();
    }
}
