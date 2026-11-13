//! One navigation generation shared by the UI owner, its capture demand, and worker replies.
use std::{
    cell::Cell,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use nsh_core::NodeId;

/// Maximum wait for a demand-driven leaving-side snapshot before falling back
/// to the platform's standard non-shader transition.
///
/// Cold `RENDER_TYPE_TEXTURE` export requires `Task::Create` (`OH_NativeImage`
/// allocation), `Message::Composite` + `BuilderNode(RENDER_TYPE_TEXTURE)`
/// subtree attachment, 1–2 RenderService frames (~16.7–33.3 ms at 60 Hz), and
/// `FrameWake` -> `Entry::snapshot()`. 100 ms (~6 frames at 60 Hz) absorbs cold
/// GPU/RS scheduling jitter on API 20 devices without false fallbacks while
/// keeping worst-case fallback latency below perceptual stall thresholds.
pub(crate) const CAPTURE_DEADLINE: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    pub(crate) owner: NodeId,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Captured(Key),
    Failed(Key),
    Completed(Key),
}

impl Event {
    pub(crate) fn key(self) -> Key {
        match self {
            Self::Captured(key) | Self::Failed(key) | Self::Completed(key) => key,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Capturing,
    Running,
    Fallback,
    Complete,
}

pub(crate) struct Navigation {
    pub(crate) key: Key,
    pub(crate) backwards: bool,
    phase: Cell<Phase>,
    fallback_pending: Cell<bool>,
}

impl Navigation {
    pub(crate) fn new(owner: NodeId, backwards: bool) -> Option<Self> {
        // Queued replies can outlive an app reload on the same host dispatcher.
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let generation = NEXT
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .ok()?;
        Some(Self {
            key: Key { owner, generation },
            backwards,
            phase: Cell::new(Phase::Capturing),
            fallback_pending: Cell::new(false),
        })
    }

    pub(crate) fn capturing(&self) -> bool {
        self.phase.get() == Phase::Capturing
    }

    pub(crate) fn running(&self) -> bool {
        matches!(self.phase.get(), Phase::Capturing | Phase::Running)
    }

    pub(crate) fn mark_running(&self, key: Key) -> bool {
        if self.key != key || self.phase.get() != Phase::Capturing {
            return false;
        }
        self.phase.set(Phase::Running);
        true
    }

    pub(crate) fn mark_fallback(&self, key: Key) -> bool {
        if self.key != key || self.phase.get() != Phase::Capturing {
            return false;
        }
        self.fallback_pending.set(true);
        self.phase.set(Phase::Fallback);
        true
    }

    pub(crate) fn take_fallback(&self) -> bool {
        self.fallback_pending.replace(false)
    }

    pub(crate) fn complete(&self, key: Key) -> bool {
        if self.key != key {
            return false;
        }
        self.fallback_pending.set(false);
        self.phase.set(Phase::Complete);
        true
    }
}

/// Whether a `Screen` paints natively before/during/after a transition.
///
/// While `Capturing`, the displayed side is held so the compositor snapshots the
/// leaving screen rather than the destination:
/// - Push (`!backwards`): entering screen stays hidden (`false`) until capture lands,
///   then flips visible (`true`) when `Running`.
/// - Pop (`backwards`): leaving screen stays visible (`true`) until capture lands,
///   then hides (`false`) when `Running` so the revealed peer renders underneath.
pub(crate) fn screen_paint_visible(
    shader: Option<&Navigation>,
    is_top: bool,
    revealed: bool,
    has_motion: bool,
) -> bool {
    if let Some(nav) = shader {
        if nav.capturing() {
            return nav.backwards;
        }
        if nav.backwards {
            return false;
        }
        return true;
    }
    is_top || revealed || has_motion
}

/// Whether an opening `Modal` must stay transparent (`presentation = 0.0`) so its
/// un-shaded `DirectRoot` never flashes while the underlying surface captures or
/// before the modal's own `RENDER_TYPE_TEXTURE` surface is ready.
pub(crate) fn modal_open_waiting(shader: &Navigation, has_texture_surface: bool) -> bool {
    !shader.backwards && (shader.capturing() || !has_texture_surface)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Demand {
    pub(crate) key: Key,
    pub(crate) picture: NodeId,
    pub(crate) deadline: Instant,
}

/// Tracks active leaving-picture capture demands so `navigation_capture` is
/// enabled strictly while at least one transition is waiting on its snapshot.
#[derive(Default)]
pub(crate) struct CaptureDemands {
    entries: Vec<Demand>,
}

impl CaptureDemands {
    pub(crate) fn request(&mut self, key: Key, picture: NodeId, deadline: Instant) -> Option<Key> {
        let mut displaced = None;
        if let Some(pos) = self
            .entries
            .iter()
            .position(|entry| entry.key.owner == key.owner)
        {
            let old = self.entries.remove(pos).key;
            if old != key {
                displaced = Some(old);
            }
        }
        self.entries.push(Demand {
            key,
            picture,
            deadline,
        });
        displaced
    }

    pub(crate) fn release(&mut self, key: Key) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.key != key);
        self.entries.len() != before
    }

    pub(crate) fn release_node(&mut self, id: NodeId) -> Vec<Key> {
        let mut removed = Vec::new();
        self.entries.retain(|entry| {
            if entry.key.owner == id || entry.picture == id {
                removed.push(entry.key);
                false
            } else {
                true
            }
        });
        removed
    }

    pub(crate) fn clear(&mut self) -> Vec<Key> {
        self.entries.drain(..).map(|entry| entry.key).collect()
    }

    pub(crate) fn expire(&mut self, now: Instant) -> Vec<Key> {
        let mut expired = Vec::new();
        self.entries.retain(|entry| {
            if now >= entry.deadline {
                expired.push(entry.key);
                false
            } else {
                true
            }
        });
        expired
    }

    pub(crate) fn contains_picture(&self, id: NodeId) -> bool {
        self.entries.iter().any(|entry| entry.picture == id)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_completion_cannot_finish_a_reused_owner() {
        let previous = Navigation::new(7, false).unwrap();
        let current = Navigation::new(7, true).unwrap();
        assert!(current.backwards);
        assert!(!current.complete(previous.key));
        assert!(current.running());
        assert!(current.complete(current.key));
        assert!(!current.running());
    }

    #[test]
    fn idle_shader_configuration_creates_zero_demands() {
        let demands = CaptureDemands::default();
        assert!(demands.is_empty());
        assert_eq!(demands.len(), 0);
        assert!(!demands.contains_picture(1));
    }

    #[test]
    fn capture_lifecycle_transitions_capturing_to_running_to_complete() {
        let now = Instant::now();
        let mut demands = CaptureDemands::default();
        let nav = Navigation::new(12, false).unwrap();
        assert!(nav.capturing());
        assert!(nav.running());

        assert_eq!(demands.request(nav.key, 1, now + CAPTURE_DEADLINE), None);
        assert!(demands.contains_picture(1));

        // Stale key cannot promote or complete.
        let stale = Key {
            owner: 12,
            generation: nav.key.generation - 1,
        };
        assert!(!nav.mark_running(stale));
        assert!(nav.capturing());

        assert!(nav.mark_running(nav.key));
        assert!(!nav.capturing());
        assert!(nav.running());
        assert!(demands.release(nav.key));
        assert!(!demands.contains_picture(1));

        // Fallback cannot fire once running.
        assert!(!nav.mark_fallback(nav.key));
        assert!(!nav.take_fallback());

        assert_eq!(Event::Captured(nav.key).key(), nav.key);
        assert_eq!(Event::Failed(nav.key).key(), nav.key);
        assert_eq!(Event::Completed(nav.key).key(), nav.key);
        assert!(nav.complete(nav.key));
        assert!(!nav.running());
    }

    #[test]
    fn deadline_expiry_marks_fallback_and_releases_demand() {
        let t0 = Instant::now();
        let mut demands = CaptureDemands::default();
        let nav = Navigation::new(21, true).unwrap();
        demands.request(nav.key, 1, t0 + CAPTURE_DEADLINE);

        assert!(demands.expire(t0 + Duration::from_millis(50)).is_empty());
        assert_eq!(demands.len(), 1);

        let expired = demands.expire(t0 + CAPTURE_DEADLINE);
        assert_eq!(expired, vec![nav.key]);
        assert!(demands.is_empty());
        assert!(!demands.contains_picture(1));

        assert!(nav.mark_fallback(nav.key));
        assert!(!nav.capturing());
        assert!(!nav.running());
        assert!(nav.take_fallback());
        assert!(!nav.take_fallback());
    }

    #[test]
    fn shared_picture_root_stays_exported_until_last_demand_releases() {
        let t0 = Instant::now();
        let mut demands = CaptureDemands::default();
        let first = Navigation::new(10, false).unwrap();
        let second = Navigation::new(11, false).unwrap();
        demands.request(first.key, 1, t0 + CAPTURE_DEADLINE);
        demands.request(second.key, 1, t0 + CAPTURE_DEADLINE);
        assert_eq!(demands.len(), 2);
        assert!(demands.contains_picture(1));

        // Replacing owner 10's demand returns the displaced key without dropping root 1.
        let replacement = Navigation::new(10, true).unwrap();
        assert_eq!(
            demands.request(replacement.key, 1, t0 + CAPTURE_DEADLINE),
            Some(first.key)
        );
        assert!(demands.contains_picture(1));

        assert!(demands.release(replacement.key));
        assert!(demands.contains_picture(1));
        assert!(demands.release(second.key));
        assert!(!demands.contains_picture(1));
    }

    #[test]
    fn node_removal_and_clear_release_matching_and_all_demands() {
        let t0 = Instant::now();
        let mut demands = CaptureDemands::default();
        let page = Navigation::new(30, false).unwrap();
        let modal = Navigation::new(31, false).unwrap();
        let nested = Navigation::new(32, false).unwrap();
        demands.request(page.key, 1, t0 + CAPTURE_DEADLINE);
        demands.request(modal.key, 1, t0 + CAPTURE_DEADLINE);
        demands.request(nested.key, 99, t0 + CAPTURE_DEADLINE);

        // Removing owner 30 releases only owner 30; root 1 remains demanded by modal 31.
        assert_eq!(demands.release_node(30), vec![page.key]);
        assert!(demands.contains_picture(1));
        assert!(demands.contains_picture(99));

        // Removing picture root 99 releases nested navigation 32.
        assert_eq!(demands.release_node(99), vec![nested.key]);
        assert!(!demands.contains_picture(99));

        // Background / reduce-motion clears remaining demands.
        assert_eq!(demands.clear(), vec![modal.key]);
        assert!(demands.is_empty());
        assert!(!demands.contains_picture(1));
    }

    #[test]
    fn screen_and_modal_hold_visibility_until_capture_lands() {
        // Screen push: hidden while capturing, visible once running.
        let push = Navigation::new(40, false).unwrap();
        assert!(!screen_paint_visible(Some(&push), true, false, false));
        assert!(push.mark_running(push.key));
        assert!(screen_paint_visible(Some(&push), true, false, false));

        // Screen pop: visible while capturing, hidden once running so peer shows.
        let pop = Navigation::new(41, true).unwrap();
        assert!(screen_paint_visible(Some(&pop), false, false, false));
        assert!(pop.mark_running(pop.key));
        assert!(!screen_paint_visible(Some(&pop), false, false, false));

        // Modal open: transparent while capturing or before texture surface is ready;
        // modal close: stays visible throughout capture and shader close.
        let modal_open = Navigation::new(42, false).unwrap();
        assert!(modal_open_waiting(&modal_open, false));
        assert!(modal_open_waiting(&modal_open, true));
        assert!(modal_open.mark_running(modal_open.key));
        assert!(modal_open_waiting(&modal_open, false));
        assert!(!modal_open_waiting(&modal_open, true));

        let modal_close = Navigation::new(43, true).unwrap();
        assert!(!modal_open_waiting(&modal_close, false));
        assert!(modal_close.mark_running(modal_close.key));
        assert!(!modal_open_waiting(&modal_close, false));
    }
}
