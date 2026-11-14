//! Coalescing state machine for multi-frame native subtree paint invalidation.
//!
//! Ensures an ancestor arming [`schedule_subtree_paint`] cancels already-armed
//! descendant chains covered by its frame window, and tracks the last painted
//! VSync `(timestamp, frame_count)` so a shorter-lived ancestor finishing earlier
//! in the same VSync never causes a longer-lived descendant callback to run a
//! duplicate subtree traversal on that frame.

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct SubtreePaint {
    pub(crate) frames: u8,
    pub(crate) armed: bool,
    pub(crate) last_frame: Option<(u64, u64)>,
}

impl SubtreePaint {
    pub(crate) fn reset(mut self) -> Self {
        self.frames = 0;
        self.armed = false;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SchedulePlan {
    pub(crate) target_frames: u8,
    pub(crate) was_armed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameTick {
    pub(crate) invalidate: bool,
    pub(crate) rearm: bool,
}

pub(crate) trait SubtreePaintNode: Clone {
    fn is_live(&self) -> bool;
    fn paint_state(&self) -> SubtreePaint;
    fn set_paint_state(&self, state: SubtreePaint);
    fn parent_repaint_owner(&self) -> Option<Self>;
    fn for_each_repaint_child(&self, f: &mut dyn FnMut(Self));
}

pub(crate) fn has_ancestor_repaint<N: SubtreePaintNode>(
    node: &N,
    min_frames: u8,
    current_frame: Option<(u64, u64)>,
) -> bool {
    let mut cur = node.parent_repaint_owner();
    while let Some(ancestor) = cur {
        if ancestor.is_live() {
            let state = ancestor.paint_state();
            if state.frames >= min_frames
                || (current_frame.is_some() && state.last_frame == current_frame)
            {
                return true;
            }
        }
        cur = ancestor.parent_repaint_owner();
    }
    false
}

pub(crate) fn cancel_descendant_repaints<N: SubtreePaintNode>(node: &N, covered_frames: u8) {
    node.for_each_repaint_child(&mut |child| {
        let mut state = child.paint_state();
        if state.frames <= covered_frames && (state.frames != 0 || state.armed) {
            state.frames = 0;
            state.armed = false;
            child.set_paint_state(state);
        }
        cancel_descendant_repaints(&child, covered_frames);
    });
}

pub(crate) fn plan_schedule<N: SubtreePaintNode>(node: &N, frames: u8) -> Option<SchedulePlan> {
    if !node.is_live() {
        return None;
    }
    let state = node.paint_state();
    let target_frames = state.frames.max(frames);
    if has_ancestor_repaint(node, target_frames.max(1), None) {
        return None;
    }
    Some(SchedulePlan {
        target_frames,
        was_armed: state.armed,
    })
}

/// Commits [`SchedulePlan`], cancelling armed descendant chains whose remaining
/// frame window is covered by `plan.target_frames`. Returns `true` if the node
/// needs to register a new `post_frame_callback`.
pub(crate) fn commit_schedule<N: SubtreePaintNode>(node: &N, plan: SchedulePlan) -> bool {
    cancel_descendant_repaints(node, plan.target_frames);
    let mut state = node.paint_state();
    let target = state.frames.max(plan.target_frames);
    let should_arm = target > 0 && !state.armed;
    state.frames = target;
    if target > 0 {
        state.armed = true;
    }
    node.set_paint_state(state);
    should_arm
}

pub(crate) fn step_frame<N: SubtreePaintNode>(node: &N, frame_id: (u64, u64)) -> FrameTick {
    if !node.is_live() {
        node.set_paint_state(node.paint_state().reset());
        return FrameTick {
            invalidate: false,
            rearm: false,
        };
    }
    let mut state = node.paint_state();
    if state.frames == 0 {
        state.armed = false;
        node.set_paint_state(state);
        return FrameTick {
            invalidate: false,
            rearm: false,
        };
    }
    state.frames -= 1;
    let covered = has_ancestor_repaint(node, 1, Some(frame_id));
    if !covered {
        state.last_frame = Some(frame_id);
    }
    let rearm = state.frames > 0;
    if !rearm {
        state.armed = false;
    }
    node.set_paint_state(state);
    FrameTick {
        invalidate: !covered,
        rearm,
    }
}

/// Lookahead margin (in logical points) around a scroll viewport when checking
/// for incoming unrecorded custom-draw nodes during flings.
pub(crate) const SCROLL_EXPORT_LOOKAHEAD_PT: f32 = 96.0;

#[inline]
pub(crate) fn needs_custom_export_record(
    has_custom_paint: bool,
    drawn_target_id: u32,
    active_target_id: u32,
) -> bool {
    active_target_id != 0 && has_custom_paint && drawn_target_id != active_target_id
}

#[inline]
pub(crate) fn rect_intersects_viewport(rect: [f32; 4], viewport: [f32; 4]) -> bool {
    rect[2] > viewport[0] && rect[0] < viewport[2] && rect[3] > viewport[1] && rect[1] < viewport[3]
}

#[inline]
pub(crate) fn scroll_viewport_rect(
    offset: (f32, f32),
    viewport_size: (f32, f32),
    margin: f32,
) -> Option<[f32; 4]> {
    if viewport_size.0 <= 0.0 || viewport_size.1 <= 0.0 {
        return None;
    }
    let m = margin.max(0.0);
    Some([
        offset.0 - m,
        offset.1 - m,
        offset.0 + viewport_size.0 + m,
        offset.1 + viewport_size.1 + m,
    ])
}

#[inline]
pub(crate) fn is_rect_unclipped_in_scroll(
    own: (i32, i32, i32, i32),
    scroll: (i32, i32, i32, i32),
    tolerance_px: i32,
) -> bool {
    let own_w = own.2 - own.0;
    let own_h = own.3 - own.1;
    let scr_w = scroll.2 - scroll.0;
    let scr_h = scroll.3 - scroll.1;
    if own_w <= 0 || own_h <= 0 || scr_w <= 0 || scr_h <= 0 {
        return false;
    }
    if own.0 >= scroll.2 || own.2 <= scroll.0 || own.1 >= scroll.3 || own.3 <= scroll.1 {
        return false;
    }
    let tol = tolerance_px.max(0);
    own_w <= scr_w
        && own_h <= scr_h
        && own.0 >= scroll.0 - tol
        && own.2 <= scroll.2 + tol
        && own.1 >= scroll.1 - tol
        && own.3 <= scroll.3 + tol
}

#[inline]
pub(crate) fn should_publish_gpu_output(
    ui_revealed: bool,
    previous_geom: Option<(u32, u32, u32, u32)>,
    next_geom: (u32, u32, u32, u32),
) -> bool {
    !ui_revealed || previous_geom != Some(next_geom)
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        rc::{Rc, Weak},
    };

    use super::*;

    struct TestNode {
        live: Cell<bool>,
        retained: Cell<bool>,
        state: Cell<SubtreePaint>,
        parent: RefCell<Weak<TestNode>>,
        retained_owner: RefCell<Weak<TestNode>>,
        children: RefCell<Vec<Rc<TestNode>>>,
        retained_children: RefCell<Vec<Rc<TestNode>>>,
        dirty_marks: Cell<usize>,
    }

    impl TestNode {
        fn new() -> Rc<Self> {
            Rc::new(Self {
                live: Cell::new(true),
                retained: Cell::new(false),
                state: Cell::new(SubtreePaint::default()),
                parent: RefCell::new(Weak::new()),
                retained_owner: RefCell::new(Weak::new()),
                children: RefCell::new(Vec::new()),
                retained_children: RefCell::new(Vec::new()),
                dirty_marks: Cell::new(0),
            })
        }

        fn add_child(parent: &Rc<Self>, child: &Rc<Self>) {
            *child.parent.borrow_mut() = Rc::downgrade(parent);
            parent.children.borrow_mut().push(child.clone());
        }

        fn add_retained_child(owner: &Rc<Self>, child: &Rc<Self>) {
            child.retained.set(true);
            *child.retained_owner.borrow_mut() = Rc::downgrade(owner);
            owner.retained_children.borrow_mut().push(child.clone());
        }

        fn invalidate_subtree(&self, traversals: &Cell<usize>) {
            traversals.set(traversals.get() + 1);
            self.walk_invalidate();
        }

        fn walk_invalidate(&self) {
            if !self.live.get() {
                return;
            }
            self.dirty_marks.set(self.dirty_marks.get() + 1);
            for child in self.children.borrow().iter() {
                child.walk_invalidate();
            }
            for child in self.retained_children.borrow().iter() {
                if child.retained.get() {
                    child.walk_invalidate();
                }
            }
        }
    }

    impl SubtreePaintNode for Rc<TestNode> {
        fn is_live(&self) -> bool {
            self.live.get()
        }

        fn paint_state(&self) -> SubtreePaint {
            self.state.get()
        }

        fn set_paint_state(&self, state: SubtreePaint) {
            self.state.set(state);
        }

        fn parent_repaint_owner(&self) -> Option<Self> {
            self.parent.borrow().upgrade().or_else(|| {
                self.retained
                    .get()
                    .then(|| self.retained_owner.borrow().upgrade())
                    .flatten()
            })
        }

        fn for_each_repaint_child(&self, f: &mut dyn FnMut(Self)) {
            for child in self.children.borrow().iter() {
                f(child.clone());
            }
            for child in self.retained_children.borrow().iter() {
                if child.retained.get() {
                    f(child.clone());
                }
            }
        }
    }

    struct Harness {
        queue: VecDeque<Rc<TestNode>>,
        frame_index: u64,
        traversals_this_frame: Cell<usize>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                queue: VecDeque::new(),
                frame_index: 0,
                traversals_this_frame: Cell::new(0),
            }
        }

        fn schedule(&mut self, node: &Rc<TestNode>, frames: u8) {
            let Some(plan) = plan_schedule(node, frames) else {
                return;
            };
            if !plan.was_armed {
                node.invalidate_subtree(&self.traversals_this_frame);
            }
            if commit_schedule(node, plan) {
                self.queue.push_back(node.clone());
            }
        }

        /// Runs one VSync frame across all queued callbacks and returns
        /// `(subtree_traversals, callbacks_executed)`.
        fn run_frame(&mut self, reverse_callback_order: bool) -> (usize, usize) {
            self.frame_index += 1;
            self.traversals_this_frame.set(0);
            let frame_id = (self.frame_index * 16_666_667, self.frame_index);
            let mut batch: Vec<_> = self.queue.drain(..).collect();
            if reverse_callback_order {
                batch.reverse();
            }
            let callbacks = batch.len();
            for node in batch {
                let tick = step_frame(&node, frame_id);
                if tick.invalidate {
                    node.invalidate_subtree(&self.traversals_this_frame);
                }
                if tick.rearm {
                    self.queue.push_back(node);
                }
            }
            (self.traversals_this_frame.get(), callbacks)
        }
    }

    #[test]
    fn child_first_then_ancestor_cancels_descendant_and_traverses_once_per_frame() {
        for reverse in [false, true] {
            let root = TestNode::new();
            let ancestor = TestNode::new();
            let child = TestNode::new();
            let leaf = TestNode::new();
            let sibling = TestNode::new();
            TestNode::add_child(&root, &ancestor);
            TestNode::add_child(&ancestor, &child);
            TestNode::add_child(&child, &leaf);
            TestNode::add_child(&ancestor, &sibling);

            let mut harness = Harness::new();
            harness.schedule(&child, 4);
            assert!(child.paint_state().armed);
            assert_eq!(child.paint_state().frames, 4);

            // Ancestor arms after child: child's chain must be disarmed immediately.
            harness.schedule(&ancestor, 4);
            assert!(!child.paint_state().armed);
            assert_eq!(child.paint_state().frames, 0);
            assert!(ancestor.paint_state().armed);
            assert_eq!(ancestor.paint_state().frames, 4);

            // Reset synchronous setup marks so we measure per-frame VSync work cleanly.
            for n in [&root, &ancestor, &child, &leaf, &sibling] {
                n.dirty_marks.set(0);
            }

            // Frame 1 drains the stale queued child callback as a no-op and runs ancestor once.
            let (traversals_f1, callbacks_f1) = harness.run_frame(reverse);
            assert_eq!(traversals_f1, 1);
            assert_eq!(callbacks_f1, 2);

            // Frames 2..=4 have ONLY ancestor armed (1 callback, 1 traversal per frame).
            for frame in 2..=4 {
                let (traversals, callbacks) = harness.run_frame(reverse);
                assert_eq!(traversals, 1, "frame {frame} reverse={reverse}");
                assert_eq!(callbacks, 1, "frame {frame} reverse={reverse}");
            }

            // Every node in ancestor's subtree was dirtied exactly once per frame (4 total).
            for n in [&ancestor, &child, &leaf, &sibling] {
                assert_eq!(n.dirty_marks.get(), 4);
            }
            assert_eq!(root.dirty_marks.get(), 0);
            assert_eq!(harness.run_frame(reverse), (0, 0));
        }
    }

    #[test]
    fn ancestor_first_then_child_skips_descendant_completely() {
        let ancestor = TestNode::new();
        let child = TestNode::new();
        TestNode::add_child(&ancestor, &child);

        let mut harness = Harness::new();
        harness.schedule(&ancestor, 4);
        let marks_after_ancestor = child.dirty_marks.get();
        harness.schedule(&child, 4);
        assert_eq!(child.dirty_marks.get(), marks_after_ancestor);
        assert!(!child.paint_state().armed);

        for _ in 1..=4 {
            assert_eq!(harness.run_frame(false), (1, 1));
        }
        assert_eq!(harness.run_frame(false), (0, 0));
    }

    #[test]
    fn shorter_ancestor_after_longer_child_still_runs_at_most_once_per_frame() {
        for reverse in [false, true] {
            let ancestor = TestNode::new();
            let child = TestNode::new();
            let sibling = TestNode::new();
            TestNode::add_child(&ancestor, &child);
            TestNode::add_child(&ancestor, &sibling);

            let mut harness = Harness::new();
            harness.schedule(&child, 4);
            harness.schedule(&ancestor, 2);

            for n in [&ancestor, &child, &sibling] {
                n.dirty_marks.set(0);
            }

            // Frames 1..=2: ancestor covers both child and sibling; child skips even on frame 2
            // when ancestor decrements 1 -> 0 before child's callback runs (`reverse = true`).
            for frame in 1..=2 {
                let (traversals, _) = harness.run_frame(reverse);
                assert_eq!(traversals, 1, "frame {frame} reverse={reverse}");
            }
            // Frames 3..=4: ancestor finished; child finishes its remaining 2 frames alone.
            for frame in 3..=4 {
                let (traversals, callbacks) = harness.run_frame(reverse);
                assert_eq!(traversals, 1, "frame {frame} reverse={reverse}");
                assert_eq!(callbacks, 1, "frame {frame} reverse={reverse}");
            }
            assert_eq!(ancestor.dirty_marks.get(), 2);
            assert_eq!(sibling.dirty_marks.get(), 2);
            assert_eq!(child.dirty_marks.get(), 4);
            assert_eq!(harness.run_frame(reverse), (0, 0));
        }
    }

    #[test]
    fn retained_descendant_is_cancelled_when_retained_owner_arms() {
        let owner = TestNode::new();
        let leaving_child = TestNode::new();
        TestNode::add_retained_child(&owner, &leaving_child);

        let mut harness = Harness::new();
        harness.schedule(&leaving_child, 4);
        assert!(leaving_child.paint_state().armed);

        harness.schedule(&owner, 6);
        assert!(!leaving_child.paint_state().armed);
        assert_eq!(leaving_child.paint_state().frames, 0);

        for _ in 1..=6 {
            assert_eq!(harness.run_frame(false).0, 1);
        }
        assert_eq!(harness.run_frame(false), (0, 0));
    }

    #[test]
    fn custom_export_record_only_marks_unrecorded_or_changed_targets() {
        assert!(!needs_custom_export_record(false, 0, 7));
        assert!(!needs_custom_export_record(true, 0, 0));
        assert!(needs_custom_export_record(true, 0, 7));
        assert!(!needs_custom_export_record(true, 7, 7));
        // Replacing the export attachment (target 7 -> 8) automatically re-arms recording.
        assert!(needs_custom_export_record(true, 7, 8));
    }

    #[test]
    fn scroll_viewport_rect_expands_by_lookahead_and_rejects_empty_viewport() {
        assert_eq!(scroll_viewport_rect((0.0, 120.0), (0.0, 800.0), 96.0), None);
        assert_eq!(scroll_viewport_rect((0.0, 120.0), (420.0, 0.0), 96.0), None);
        let vp = scroll_viewport_rect((0.0, 300.0), (420.0, 892.0), SCROLL_EXPORT_LOOKAHEAD_PT)
            .expect("non-empty viewport");
        assert_eq!(vp, [-96.0, 204.0, 516.0, 1288.0]);
        assert!(!rect_intersects_viewport([16.0, 1400.0, 404.0, 1600.0], vp));
        assert!(rect_intersects_viewport([16.0, 1260.0, 404.0, 1460.0], vp));
    }

    #[test]
    fn gpu_output_notifies_ui_only_until_revealed_or_when_geometry_changes() {
        let g1 = (1260, 2769, 0.0_f32.to_bits(), 0.0_f32.to_bits());
        let g2 = (1260, 2769, (-12.0_f32).to_bits(), 0.0_f32.to_bits());
        assert!(should_publish_gpu_output(false, None, g1));
        assert!(should_publish_gpu_output(false, Some(g1), g1));
        assert!(!should_publish_gpu_output(true, Some(g1), g1));
        assert!(should_publish_gpu_output(true, Some(g1), g2));
    }

    #[test]
    fn unclipped_scroll_check_rejects_offscreen_and_partially_clipped_nodes() {
        let scroll = (0, 240, 1260, 2640);
        // Completely inside scroll viewport:
        assert!(is_rect_unclipped_in_scroll((104, 560, 420, 620), scroll, 2));
        // Off-screen below the fold:
        assert!(!is_rect_unclipped_in_scroll(
            (104, 2700, 420, 2760),
            scroll,
            2
        ));
        // Partially peeking across the bottom fold:
        assert!(!is_rect_unclipped_in_scroll(
            (104, 2610, 420, 2680),
            scroll,
            2
        ));
        // Rounding tolerance within 2px at edge:
        assert!(is_rect_unclipped_in_scroll(
            (104, 2580, 420, 2642),
            scroll,
            2
        ));
    }
}
