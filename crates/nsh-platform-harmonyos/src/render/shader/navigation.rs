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
/// Calibrated against OpenHarmony 6.0 (`1260x2720`) device measurements:
/// - Warm `Composite` reuse: ~8–10 ms.
/// - Cold `ROOT_ID` `BuilderNode(RENDER_TYPE_TEXTURE)` export + in-process Skia
///   rasterization + `OH_NativeImage` + GLES `Target::copy`: ~73–119 ms steady
///   state (~110–200 ms under scroll load).
/// - First-time in-process Skia GPU pipeline warm-up in an Ability lifetime
///   (since idle `NavShader` exports 0 textures): up to ~357 ms (`Submit task`).
///
/// 500 ms (~30 frames at 60 Hz) gives ~4x headroom over steady-state cold
/// `ROOT_ID` export and absorbs first-use Skia shader compilation without false
/// fallbacks, while bounding worst-case recovery if a surface stalls. Notice
/// healthy captures resolve immediately on `Event::Captured` (8–120 ms) and
/// never wait out the deadline.
pub(crate) const CAPTURE_DEADLINE: Duration = Duration::from_millis(500);

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
        if self.key != key || !matches!(self.phase.get(), Phase::Capturing | Phase::Running) {
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
        if self.key != key || matches!(self.phase.get(), Phase::Complete | Phase::Fallback) {
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
/// before the modal's own `Composite` has swapped its first shaded frame.
pub(crate) fn modal_open_waiting(shader: &Navigation, has_presented_frame: bool) -> bool {
    !shader.backwards && (shader.capturing() || !has_presented_frame)
}

/// Inputs governing whether an open `Modal` (`Dialog`) with a `transitionShader`
/// retains its `Composite` in identity-passthrough mode until dismissal completes.
///
/// Retaining `Composite(modal_id)` strictly from first `Event::Captured` until
/// `Modal::finish(0.0)` (or shader removal / reduce-motion / context loss) avoids:
/// 1. Open-completion `Composite -> DirectRoot` retirement (preventing translucent
///    modal scrims from double-darkening or flickering on settle), and
/// 2. Close-start `DirectRoot -> TextureContainer` cold re-export gaps (`30-50ms`
///    scrim/dialog vanish + custom-draw rebuild) when reverse `transitionShader` runs.
///
/// Closed dialogs (`!modal_visible`) keep `0` idle texture exports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModalHostInputs {
    pub(crate) retainable: bool,
    pub(crate) reduce_motion: bool,
    pub(crate) node_active: bool,
    pub(crate) node_retained: bool,
    pub(crate) node_hidden: bool,
    pub(crate) modal_visible: bool,
    pub(crate) has_navigation_shader: bool,
}

pub(crate) fn modal_host_keep_alive(inputs: ModalHostInputs) -> bool {
    inputs.retainable
        && !inputs.reduce_motion
        && inputs.node_active
        && !inputs.node_retained
        && !inputs.node_hidden
        && inputs.modal_visible
        && inputs.has_navigation_shader
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HeldView {
    view_owner: NodeId,
    target: NodeId,
    holder: Key,
}

/// Keeps interrupted navigation shader layers attached on `target` while a
/// replacement capture is pending so `Composite::update` neither wipes live
/// mid-transition shader output ahead of `Task::Capture` nor retires an active
/// modal `Composite` while `ROOT_ID` cold-captures.
#[derive(Default)]
pub(crate) struct HeldViews {
    entries: Vec<HeldView>,
}

impl HeldViews {
    pub(crate) fn hold(&mut self, view_owner: NodeId, target: NodeId, holder: Key) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|entry| entry.view_owner == view_owner)
        {
            existing.target = target;
            existing.holder = holder;
        } else {
            self.entries.push(HeldView {
                view_owner,
                target,
                holder,
            });
        }
    }

    pub(crate) fn transfer_or_release(
        &mut self,
        old_holder: Key,
        new_holder: Key,
        new_target: NodeId,
    ) -> Vec<NodeId> {
        let mut detach = Vec::new();
        self.entries.retain_mut(|entry| {
            if entry.holder != old_holder {
                return true;
            }
            if entry.target == new_target {
                entry.holder = new_holder;
                true
            } else {
                detach.push(entry.view_owner);
                false
            }
        });
        detach
    }

    pub(crate) fn release_holder(
        &mut self,
        holder: Key,
        replaced_in_place: Option<NodeId>,
    ) -> Vec<NodeId> {
        let mut detach = Vec::new();
        self.entries.retain(|entry| {
            if entry.holder == holder {
                if replaced_in_place != Some(entry.view_owner) {
                    detach.push(entry.view_owner);
                }
                false
            } else {
                true
            }
        });
        detach
    }

    pub(crate) fn remove_view(&mut self, view_owner: NodeId) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.view_owner != view_owner);
        self.entries.len() != before
    }

    pub(crate) fn remove_target(&mut self, target: NodeId) -> Vec<NodeId> {
        let mut detach = Vec::new();
        self.entries.retain(|entry| {
            if entry.target == target {
                detach.push(entry.view_owner);
                false
            } else {
                true
            }
        });
        detach
    }

    pub(crate) fn is_held(&self, view_owner: NodeId) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.view_owner == view_owner)
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
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
        // Duplicate completion from a superseded worker task is ignored.
        assert!(!current.complete(current.key));
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

        // Stale key cannot promote, fail, or complete.
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

        assert!(!nav.mark_fallback(stale));
        assert!(nav.running());

        assert_eq!(Event::Captured(nav.key).key(), nav.key);
        assert_eq!(Event::Failed(nav.key).key(), nav.key);
        assert_eq!(Event::Completed(nav.key).key(), nav.key);
        assert!(nav.complete(nav.key));
        assert!(!nav.running());
        // Once completed, late fallback cannot revive or set fallback_pending.
        assert!(!nav.mark_fallback(nav.key));
        assert!(!nav.take_fallback());
    }

    #[test]
    fn running_failure_after_capture_marks_fallback_and_stops_running() {
        // Reproduces Event::Failed arriving from Task::Navigation (missing/expired
        // snapshot) after Event::Captured already moved Navigation into Running.
        let nav = Navigation::new(50, false).unwrap();
        assert!(nav.mark_running(nav.key));
        assert!(nav.running());

        assert!(nav.mark_fallback(nav.key));
        assert!(!nav.capturing());
        assert!(!nav.running());
        assert!(nav.take_fallback());
        assert!(!nav.take_fallback());
        assert!(!nav.complete(nav.key));
        assert!(!nav.mark_fallback(nav.key));
    }

    #[test]
    fn held_views_preserve_interrupted_layers_until_replacement_resolves() {
        let mut held = HeldViews::default();

        // 1. Same-owner interruption (e.g. Modal close interrupting mid-IRIS Modal open):
        //    modal 20's layer on target 20 stays held during ROOT_ID capture, then is
        //    replaced in place by set_on without detaching modal 20's Composite.
        let modal_close = Navigation::new(20, true).unwrap();
        held.hold(20, 20, modal_close.key);
        assert!(held.is_held(20));
        assert_eq!(
            held.release_holder(modal_close.key, Some(20)),
            Vec::<NodeId>::new()
        );
        assert!(!held.is_held(20));

        // 2. Cross-owner interruption (Screen 11 push interrupting Screen 10 push on root 1),
        //    followed by a third Screen 12 superseding Screen 11 before capture lands:
        //    Screen 10's layer stays held across the transfer, then detaches after Screen 12's
        //    layer is attached via set_on.
        let screen_11 = Navigation::new(11, false).unwrap();
        let screen_12 = Navigation::new(12, false).unwrap();
        held.hold(10, 1, screen_11.key);
        assert!(held.is_held(10));
        assert_eq!(
            held.transfer_or_release(screen_11.key, screen_12.key, 1),
            Vec::<NodeId>::new()
        );
        assert!(held.is_held(10));
        assert_eq!(held.release_holder(screen_12.key, Some(12)), vec![10]);
        assert!(!held.is_held(10));

        // 3. Failure / deadline / cancellation before set_on detaches even same-owner held views.
        let retry_close = Navigation::new(20, true).unwrap();
        held.hold(20, 20, retry_close.key);
        assert_eq!(held.release_holder(retry_close.key, None), vec![20]);
        assert!(!held.is_held(20));

        // 4. Node/target removal and clear release matching entries.
        let nav_a = Navigation::new(30, false).unwrap();
        let nav_b = Navigation::new(31, false).unwrap();
        held.hold(30, 1, nav_a.key);
        held.hold(31, 99, nav_b.key);
        assert!(held.remove_view(30));
        assert!(!held.is_held(30));
        assert_eq!(held.remove_target(99), vec![31]);
        assert!(!held.is_held(31));
        held.hold(32, 1, nav_a.key);
        held.clear();
        assert!(!held.is_held(32));
    }

    #[test]
    fn deadline_expiry_marks_fallback_and_releases_demand() {
        let t0 = Instant::now();
        let mut demands = CaptureDemands::default();
        let nav = Navigation::new(21, true).unwrap();
        demands.request(nav.key, 1, t0 + CAPTURE_DEADLINE);

        // Measured OHOS cold ROOT_ID export (102–119 ms) and first-use Skia
        // pipeline init (~357 ms) stay well within CAPTURE_DEADLINE.
        assert!(demands.expire(t0 + Duration::from_millis(120)).is_empty());
        assert!(demands.expire(t0 + Duration::from_millis(360)).is_empty());
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

    #[test]
    fn modal_host_keep_alive_holds_only_visible_shader_modals_and_releases_on_close() {
        let open_shader_modal = ModalHostInputs {
            retainable: true,
            reduce_motion: false,
            node_active: true,
            node_retained: false,
            node_hidden: false,
            modal_visible: true,
            has_navigation_shader: true,
        };
        assert!(modal_host_keep_alive(open_shader_modal));

        // Closed / idle modal configured with NavShader exports 0 textures.
        assert!(!modal_host_keep_alive(ModalHostInputs {
            modal_visible: false,
            ..open_shader_modal
        }));

        // Open modal without transitionShader stays on DirectRoot.
        assert!(!modal_host_keep_alive(ModalHostInputs {
            has_navigation_shader: false,
            ..open_shader_modal
        }));

        // Reduce-motion, hidden, retained, deactivated, or lost context release keep-alive.
        assert!(!modal_host_keep_alive(ModalHostInputs {
            reduce_motion: true,
            ..open_shader_modal
        }));
        assert!(!modal_host_keep_alive(ModalHostInputs {
            node_hidden: true,
            ..open_shader_modal
        }));
        assert!(!modal_host_keep_alive(ModalHostInputs {
            node_retained: true,
            ..open_shader_modal
        }));
        assert!(!modal_host_keep_alive(ModalHostInputs {
            node_active: false,
            ..open_shader_modal
        }));
        assert!(!modal_host_keep_alive(ModalHostInputs {
            retainable: false,
            ..open_shader_modal
        }));
    }
}
