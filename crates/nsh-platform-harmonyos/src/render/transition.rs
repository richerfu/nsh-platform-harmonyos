//! UI-thread layout/style transitions and keyframes, delegating to
//! `nsh_core::transition` and driven by the surface's native frame clock.
pub(crate) use nsh_core::transition::Extra;
use std::collections::HashSet;

use nsh_core::{
    Bounds, LayoutAnimation, LayoutTransition, MountItem, NodeId, Points, ViewStyle, transition,
};

use crate::render::geometry::Rect;

/// Pre-scanned deletions and exit-animating removals for one [`nsh_core::MountBatch`],
/// paired with step-by-step tracking of nodes created within that batch.
///
/// A single engine commit can create a subtree at its initial layout, dispatch
/// `onLayout`, and fold/remove newly created children or update surviving frames
/// under `LayoutAnimation.configureNext` inside one merged batch (for example
/// `ToolbarDemo`'s responsive formatting bar). Nodes created in the current
/// batch have never been presented on screen, so they must neither animate
/// `Frame` updates (`move_frame`) nor linger via `Remove`/`Delete` exit
/// transitions (`disappear`), matching the UIKit and Android mounters.
pub(crate) struct BatchPrepass {
    pub(crate) deletions: Vec<NodeId>,
    pub(crate) deleted: HashSet<NodeId>,
    pub(crate) removals: Vec<(NodeId, Option<LayoutAnimation>)>,
    pub(crate) removed: HashSet<NodeId>,
    pub(crate) motion: BatchMotion,
}

pub(crate) struct BatchMotion {
    layout: Option<LayoutAnimation>,
    created_any: HashSet<NodeId>,
    created_now: HashSet<NodeId>,
}

impl BatchMotion {
    pub(crate) fn scan(items: &[MountItem]) -> BatchPrepass {
        let mut created_any = HashSet::new();
        for item in items {
            if let MountItem::Create { id, .. } = item {
                created_any.insert(*id);
            }
        }
        let mut layout = None;
        let mut created_so_far = HashSet::with_capacity(created_any.len());
        let mut deletions = Vec::new();
        let mut deleted = HashSet::new();
        let mut removals = Vec::new();
        let mut removed = HashSet::new();
        for item in items {
            match item {
                MountItem::LayoutAnimation(animation) => layout = Some(*animation),
                MountItem::Create { id, .. } => {
                    created_so_far.insert(*id);
                }
                MountItem::Delete { id } => {
                    if !created_so_far.contains(id) && deleted.insert(*id) {
                        deletions.push(*id);
                    }
                    if !created_any.contains(id) && removed.insert(*id) {
                        removals.push((*id, layout));
                    }
                }
                MountItem::Remove { id } if !created_any.contains(id) && removed.insert(*id) => {
                    removals.push((*id, layout));
                }
                _ => {}
            }
        }
        let capacity = created_any.len();
        BatchPrepass {
            deletions,
            deleted,
            removals,
            removed,
            motion: Self {
                layout: None,
                created_any,
                created_now: HashSet::with_capacity(capacity),
            },
        }
    }

    pub(crate) fn set_layout(&mut self, animation: LayoutAnimation) {
        self.layout = Some(animation);
    }

    pub(crate) fn note_created(&mut self, id: NodeId) {
        self.created_now.insert(id);
    }

    pub(crate) fn is_created_in_batch(&self, id: NodeId) -> bool {
        self.created_now.contains(&id)
    }

    pub(crate) fn allows_defer(&self, id: NodeId) -> bool {
        !self.created_any.contains(&id)
    }

    pub(crate) fn insert_animation(&self) -> Option<&LayoutAnimation> {
        self.layout.as_ref()
    }

    pub(crate) fn frame_animation(&self, id: NodeId) -> Option<&LayoutAnimation> {
        self.layout
            .as_ref()
            .filter(|_| !self.created_now.contains(&id))
    }

    pub(crate) fn remove_animation(&self, id: NodeId) -> Option<&LayoutAnimation> {
        self.layout
            .as_ref()
            .filter(|_| !self.created_any.contains(&id))
    }

    pub(crate) fn note_deleted(&mut self, id: NodeId) -> bool {
        self.created_now.remove(&id) || self.created_any.contains(&id)
    }
}

#[derive(Default)]
pub(crate) struct Live {
    inner: transition::Live,
    target_style: Option<Box<ViewStyle>>,
    pub(crate) recolor: bool,
    pub(crate) text_color: Option<nsh_core::Color>,
}

pub(crate) struct Step {
    pub(crate) frame: Option<Rect>,
    pub(crate) opacity: f32,
    pub(crate) extra: Extra,
    pub(crate) active: bool,
    pub(crate) removed: bool,
    pub(crate) restyled: bool,
}

impl Live {
    #[cfg(feature = "dev")]
    pub(crate) fn test_finishing(&self, now: f64) -> bool {
        self.inner.deadline(now).is_some()
    }
    pub(crate) fn text_color(&self) -> Option<nsh_core::Color> {
        self.text_color
    }

    pub(crate) fn frame_rate(&self) -> Option<f32> {
        self.inner.frame_rate()
    }

    pub(crate) fn recolor(&mut self, change: transition::Recolor) {
        self.inner.recolor(change);
        self.text_color = self.inner.text_color();
    }

    pub(crate) fn finish(&mut self, style: &mut Box<ViewStyle>) -> Step {
        let frame = self.inner.frame_target().map(Rect::from);
        if let Some(target) = self.target_style.take() {
            *style = target;
        }
        let removed = self.inner.dying();
        self.recolor = self.text_color.take().is_some();
        self.inner = transition::Live::default();
        Step {
            frame,
            opacity: 1.0,
            extra: Extra::NONE,
            active: false,
            removed,
            restyled: true,
        }
    }

    pub(crate) fn frame_target(&self) -> Option<Rect> {
        self.inner.frame_target().map(Rect::from)
    }

    pub(crate) fn frame(
        &mut self,
        from: Rect,
        to: Rect,
        animation: Option<&LayoutAnimation>,
    ) -> bool {
        if let Some(t) = animation
            .and_then(|a| a.update)
            .filter(|_| from != Rect::default() && from != to)
        {
            self.inner
                .move_frame(Bounds::<Points>::from(from), Bounds::<Points>::from(to), t);
            true
        } else {
            self.inner.stop_frame();
            false
        }
    }

    pub(crate) fn style(
        &mut self,
        out: &mut Box<ViewStyle>,
        old: ViewStyle,
        reduce: bool,
        initialized: bool,
    ) {
        if reduce {
            let _ = self.finish(&mut Box::default());
            return;
        }
        let mut boxed = out.clone();
        let prev = if initialized {
            old
        } else {
            ViewStyle::default()
        };
        if let Some(restyle) = transition::restyle(&prev, &mut boxed, false) {
            self.target_style = Some(out.clone());
            *out = boxed;
            self.inner.restyle(restyle);
        } else {
            self.target_style = None;
            self.inner.stop_style();
        }
    }

    pub(crate) fn appear(&mut self, transition: LayoutTransition) -> (f32, Extra) {
        self.inner.appear(transition);
        (
            self.inner.opacity(),
            self.inner.extra().unwrap_or(Extra::NONE),
        )
    }

    pub(crate) fn disappear(&mut self, transition: LayoutTransition) {
        self.inner.disappear(transition);
    }

    pub(crate) fn cancel_removal(&mut self) {
        self.inner.revive();
    }

    pub(crate) fn step(
        &mut self,
        now: f64,
        current_frame: Rect,
        style: &mut Box<ViewStyle>,
    ) -> Step {
        let prev_bounds = Bounds::<Points>::from(current_frame);
        let mut bounds = prev_bounds;
        let had_frame = self.inner.frame_target().is_some();
        let s = self.inner.step(now, &mut bounds, style);
        let color = self.inner.text_color();
        self.recolor = self.text_color != color;
        self.text_color = color;
        if s.restyled && self.inner.idle() {
            self.target_style = None;
        }
        let frame_done = had_frame && self.inner.frame_target().is_none();
        Step {
            frame: (had_frame && (bounds != prev_bounds || frame_done)).then(|| Rect::from(bounds)),
            opacity: self.inner.opacity(),
            extra: self.inner.extra().unwrap_or(Extra::NONE),
            active: !s.finished && !self.inner.idle(),
            removed: s.removed,
            restyled: s.restyled,
        }
    }
}

#[cfg(test)]
mod tests {
    use nsh_core::{
        AttributedText, Color, ResolvedTextStyle, Transition, TransitionProps, ViewKind,
    };

    use super::*;

    fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<Points> {
        Bounds::from(Rect {
            x,
            y,
            width,
            height,
        })
    }

    fn text(color: Color) -> AttributedText {
        let mut text = AttributedText::default();
        text.push(
            "Color",
            ResolvedTextStyle {
                color,
                ..Default::default()
            },
        );
        text
    }

    #[test]
    fn text_color_interpolates_and_invalidates_the_terminal_frame() {
        let mut style = Box::new(ViewStyle {
            transition: Some(Transition {
                properties: TransitionProps(TransitionProps::COLORS),
                duration: 100.0,
                delay: 0.0,
                easing: [0.0, 0.0, 1.0, 1.0],
            }),
            ..Default::default()
        });
        let (red, blue) = (Color(0xff0000ff), Color(0x0000ffff));
        let mut live = Live::default();
        live.recolor(transition::recolor(&text(red), &text(blue), &style, false));
        assert_eq!(live.text_color(), Some(red));
        live.step(0.0, Rect::default(), &mut style);
        assert!(live.step(50.0, Rect::default(), &mut style).active);
        assert!(live.recolor);
        assert_eq!(live.text_color(), Some(Color(0x800080ff)));
        assert!(!live.step(100.0, Rect::default(), &mut style).active);
        assert!(live.recolor);
        assert_eq!(live.text_color(), None);

        live.recolor(transition::recolor(&text(red), &text(blue), &style, false));
        live.finish(&mut style);
        assert!(live.recolor);
        assert_eq!(live.text_color(), None);
    }

    #[test]
    fn wide_to_compact_on_layout_mount_batch_skips_slide_and_ghost_delete() {
        let ease = LayoutAnimation::EASE_IN_EASE_OUT;
        let (toolbar, trigger, paragraph, marks) = (10, 11, 12, 13);
        let wide_marks = bounds(192.0, 4.0, 134.0, 32.0);
        let compact_marks = bounds(101.0, 4.0, 66.0, 32.0);
        let items = vec![
            MountItem::LayoutAnimation(ease),
            MountItem::Create {
                id: toolbar,
                kind: ViewKind::View,
            },
            MountItem::Create {
                id: trigger,
                kind: ViewKind::View,
            },
            MountItem::Create {
                id: paragraph,
                kind: ViewKind::Text,
            },
            MountItem::Frame {
                id: paragraph,
                frame: bounds(28.0, 7.0, 56.0, 18.0),
            },
            MountItem::Insert {
                parent: trigger,
                child: paragraph,
                before: None,
            },
            MountItem::Create {
                id: marks,
                kind: ViewKind::View,
            },
            MountItem::Frame {
                id: marks,
                frame: wide_marks,
            },
            MountItem::Insert {
                parent: toolbar,
                child: marks,
                before: None,
            },
            // Round 2 merged into the same MountBatch after `onLayout`:
            MountItem::LayoutAnimation(ease),
            MountItem::Remove { id: paragraph },
            MountItem::Delete { id: paragraph },
            MountItem::Frame {
                id: marks,
                frame: compact_marks,
            },
        ];

        let prepass = BatchMotion::scan(&items);
        assert!(prepass.deletions.is_empty());
        assert!(prepass.deleted.is_empty());
        assert!(prepass.removals.is_empty());
        assert!(prepass.removed.is_empty());

        let mut motion = prepass.motion;
        let mut marks_live = Live::default();
        let mut marks_frame = Rect::default();
        for item in items {
            match item {
                MountItem::LayoutAnimation(a) => motion.set_layout(a),
                MountItem::Create { id, .. } => motion.note_created(id),
                MountItem::Frame { id, frame } if id == marks => {
                    let next = Rect::from(frame);
                    let anim = motion.frame_animation(id);
                    assert!(anim.is_none());
                    assert!(!marks_live.frame(marks_frame, next, anim));
                    marks_frame = next;
                }
                MountItem::Remove { id } if id == paragraph => {
                    assert!(motion.is_created_in_batch(id));
                    assert!(motion.remove_animation(id).is_none());
                }
                MountItem::Delete { id } if id == paragraph => {
                    assert!(motion.note_deleted(id));
                    assert!(!motion.is_created_in_batch(id));
                }
                _ => {}
            }
        }
        assert_eq!(marks_frame, Rect::from(compact_marks));
        assert!(marks_live.frame_target().is_none());
    }

    #[test]
    fn interactive_fold_after_mount_animates_existing_frames_and_removals() {
        let ease = LayoutAnimation::EASE_IN_EASE_OUT;
        let (paragraph, marks) = (12, 13);
        let wide_marks = Rect {
            x: 192.0,
            y: 4.0,
            width: 134.0,
            height: 32.0,
        };
        let compact_marks = Rect {
            x: 101.0,
            y: 4.0,
            width: 66.0,
            height: 32.0,
        };
        let items = vec![
            MountItem::LayoutAnimation(ease),
            MountItem::Remove { id: paragraph },
            MountItem::Delete { id: paragraph },
            MountItem::Frame {
                id: marks,
                frame: Bounds::from(compact_marks),
            },
        ];

        let prepass = BatchMotion::scan(&items);
        assert_eq!(prepass.deletions, vec![paragraph]);
        assert_eq!(prepass.removals, vec![(paragraph, Some(ease))]);

        let mut motion = prepass.motion;
        motion.set_layout(ease);
        assert!(motion.allows_defer(paragraph));
        assert_eq!(motion.remove_animation(paragraph), Some(&ease));
        assert!(!motion.note_deleted(paragraph));

        let mut live = Live::default();
        assert!(live.frame(wide_marks, compact_marks, motion.frame_animation(marks)));
        assert_eq!(live.frame_target(), Some(compact_marks));

        let mut style = Box::default();
        live.step(0.0, wide_marks, &mut style);
        let mid = live.step(150.0, wide_marks, &mut style);
        assert!(mid.active);
        let mid_rect = mid.frame.expect("interpolated mid-slide frame");
        assert!(mid_rect.x > compact_marks.x && mid_rect.x < wide_marks.x);
        let end = live.step(300.0, mid_rect, &mut style);
        assert!(!end.active);
        assert_eq!(end.frame, Some(compact_marks));
    }

    #[test]
    fn collapsed_reveal_and_kept_list_row_animate_across_batches() {
        let ease = LayoutAnimation::EASE_IN_EASE_OUT;
        let (reveal, kept_cell, new_cell) = (20, 21, 22);
        let collapsed = Rect {
            x: 0.0,
            y: 80.0,
            width: 300.0,
            height: 0.0,
        };
        let expanded = Rect {
            x: 0.0,
            y: 80.0,
            width: 300.0,
            height: 120.0,
        };

        // Batch 2 expands a 0-height reveal created in an earlier batch, and
        // runs list_motion (`default` reset + `ease` slide) on a kept cell
        // alongside a newly created cell.
        let old_cell = Rect {
            x: 0.0,
            y: 200.0,
            width: 300.0,
            height: 44.0,
        };
        let new_cell_rect = Rect {
            x: 0.0,
            y: 156.0,
            width: 300.0,
            height: 44.0,
        };
        let items = vec![
            MountItem::Create {
                id: new_cell,
                kind: ViewKind::View,
            },
            MountItem::LayoutAnimation(LayoutAnimation::default()),
            MountItem::Frame {
                id: kept_cell,
                frame: Bounds::from(old_cell),
            },
            MountItem::LayoutAnimation(ease),
            MountItem::Frame {
                id: reveal,
                frame: Bounds::from(expanded),
            },
            MountItem::Frame {
                id: kept_cell,
                frame: Bounds::from(new_cell_rect),
            },
            MountItem::Frame {
                id: new_cell,
                frame: Bounds::from(new_cell_rect),
            },
        ];

        let mut motion = BatchMotion::scan(&items).motion;
        motion.note_created(new_cell);

        motion.set_layout(LayoutAnimation::default());
        let mut kept_live = Live::default();
        assert!(!kept_live.frame(
            Rect {
                y: 244.0,
                ..old_cell
            },
            old_cell,
            motion.frame_animation(kept_cell),
        ));

        motion.set_layout(ease);
        let mut reveal_live = Live::default();
        assert!(reveal_live.frame(collapsed, expanded, motion.frame_animation(reveal)));
        assert_eq!(reveal_live.frame_target(), Some(expanded));

        assert!(kept_live.frame(old_cell, new_cell_rect, motion.frame_animation(kept_cell)));
        assert_eq!(kept_live.frame_target(), Some(new_cell_rect));

        assert!(motion.frame_animation(new_cell).is_none());
    }

    #[test]
    fn retagged_slot_in_same_batch_vacates_without_exit_animation_or_deferral() {
        let ease = LayoutAnimation::EASE_IN_EASE_OUT;
        let slot = 7;
        let items = vec![
            MountItem::LayoutAnimation(ease),
            MountItem::Remove { id: slot },
            MountItem::Delete { id: slot },
            MountItem::Create {
                id: slot,
                kind: ViewKind::Input,
            },
            MountItem::Frame {
                id: slot,
                frame: bounds(0.0, 0.0, 200.0, 36.0),
            },
        ];

        let prepass = BatchMotion::scan(&items);
        assert_eq!(prepass.deletions, vec![slot]);
        assert!(prepass.removals.is_empty());

        let mut motion = prepass.motion;
        motion.set_layout(ease);
        assert!(!motion.allows_defer(slot));
        assert!(motion.remove_animation(slot).is_none());
        assert!(motion.note_deleted(slot));

        motion.note_created(slot);
        assert!(motion.is_created_in_batch(slot));
        assert!(motion.frame_animation(slot).is_none());
    }
}
