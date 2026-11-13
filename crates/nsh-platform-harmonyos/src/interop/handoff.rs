//! Placeholder-fill and commit-gate state for `DirectRoot -> TextureContainer` handoffs.
//!
//! On OpenHarmony, moving a single-parent `ArkUINode` out of `DirectRoot` into
//! `TextureContainer` (`RENDER_TYPE_TEXTURE`) detaches it from `/system/bin/render_service`
//! immediately while cold Skia texture export + GLES `eglSwapBuffers` produces frame 0
//! asynchronously on `Presentation` (`XComponent`).
//!
//! To prevent exposing the underlying `#FFFFFFFF` `UIAbility` window during that gap
//! without taking screenshots or painting node content:
//! 1. `wrapper.outer` (`ARKUI_NODE_CUSTOM`) paints ONLY a solid theme-resolved background
//!    (the owner's opaque background color, else the theme `"background"` color for
//!    `ROOT_ID`/`Screen`, and nothing for non-opaque owners).
//! 2. `DirectRoot` detach is gated until `wrapper.outer`'s placeholder draw has recorded in
//!    `on_custom_draw` (`FlushRenderTask` of VSync `K`) and reached the next UI frame
//!    callback (Phase 1 of VSync `K+1`, strictly after VSync `K`'s `FlushMessages()`
//!    committed to `render_service`), with an offscreen-cull escape if ArkUI skips
//!    `on_custom_draw` across a full render pass.
//! 3. `Presentation` reveals only after its first swap, and `wrapper.outer` stops painting
//!    the placeholder in the exact same transaction as reveal.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PlaceholderFill {
    pub argb: u32,
    pub radii: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HandoffInputs {
    pub closed: bool,
    pub fill: Option<PlaceholderFill>,
    pub is_root: bool,
    pub is_mount_or_prewarm: bool,
    pub covered_by_live_bridge: bool,
    pub has_mounted_direct: bool,
    pub active_target_is_texture: bool,
    pub export_requested: bool,
    pub desired_surface_ready: bool,
    pub revealed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PrepareOutcome {
    pub hold_direct_target: bool,
    pub repaint_outer: bool,
    pub arm_frame_callback: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameCallbackStep {
    Stale,
    ReArm,
    ReadyToDetach,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GapOutcome {
    Revealed,
    Cancelled,
}

impl GapOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Revealed => "revealed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum GapKind {
    Mount,
    #[default]
    FallbackCutover,
}

impl GapKind {
    pub(crate) fn log_tag(self) -> &'static str {
        match self {
            Self::Mount => "shader-mount-gap",
            Self::FallbackCutover => "handoff-gap",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CommitGate {
    #[default]
    Idle,
    WaitingDraw {
        generation: u32,
        vsync_polls: u8,
    },
    DrawnWaitingCommit {
        generation: u32,
    },
    Committed,
}

#[derive(Default, Debug)]
pub(crate) struct HandoffState {
    gate: CommitGate,
    generation: u32,
    callback_in_flight: Option<u32>,
    placeholder_active: bool,
    last_fill: Option<PlaceholderFill>,
    was_texture: bool,
    in_handoff: bool,
    mount_hidden: bool,
    gap_kind: GapKind,
    gap_started_at: Option<Instant>,
}

impl HandoffState {
    pub(crate) fn active_fill(&self) -> Option<PlaceholderFill> {
        if self.placeholder_active {
            self.last_fill
        } else {
            None
        }
    }

    pub(crate) fn begin_mount_export(&mut self, now: Instant) {
        if !self.in_handoff {
            self.in_handoff = true;
            self.mount_hidden = true;
            self.gap_kind = GapKind::Mount;
        }
        self.gap_started_at.get_or_insert(now);
    }

    pub(crate) fn prepare_target(
        &mut self,
        inputs: HandoffInputs,
        now: Instant,
    ) -> (PrepareOutcome, Option<(Duration, GapOutcome, GapKind)>) {
        if inputs.closed || !inputs.export_requested {
            let kind = self.gap_kind;
            let gap = self.gap_started_at.take().map(|started| {
                (
                    now.saturating_duration_since(started),
                    GapOutcome::Cancelled,
                    kind,
                )
            });
            self.gate = CommitGate::Idle;
            self.callback_in_flight = None;
            self.in_handoff = false;
            self.mount_hidden = false;
            self.gap_kind = GapKind::FallbackCutover;
            self.was_texture = inputs.active_target_is_texture;
            let repaint_outer = std::mem::replace(&mut self.placeholder_active, false);
            self.last_fill = None;
            return (
                PrepareOutcome {
                    hold_direct_target: false,
                    repaint_outer,
                    arm_frame_callback: None,
                },
                gap,
            );
        }

        if inputs.revealed {
            let kind = self.gap_kind;
            let gap = self.gap_started_at.take().map(|started| {
                (
                    now.saturating_duration_since(started),
                    GapOutcome::Revealed,
                    kind,
                )
            });
            self.gate = CommitGate::Idle;
            self.callback_in_flight = None;
            self.in_handoff = false;
            self.mount_hidden = false;
            self.gap_kind = GapKind::FallbackCutover;
            self.was_texture = inputs.active_target_is_texture;
            let repaint_outer = std::mem::replace(&mut self.placeholder_active, false);
            self.last_fill = None;
            return (
                PrepareOutcome {
                    hold_direct_target: false,
                    repaint_outer,
                    arm_frame_callback: None,
                },
                gap,
            );
        }

        if !self.in_handoff {
            self.in_handoff = true;
            self.mount_hidden =
                !inputs.has_mounted_direct && !inputs.is_root && !inputs.active_target_is_texture;
            self.gap_kind = if inputs.is_mount_or_prewarm {
                GapKind::Mount
            } else {
                GapKind::FallbackCutover
            };
        }

        if inputs.covered_by_live_bridge {
            self.gate = CommitGate::Committed;
            self.callback_in_flight = None;
            self.gap_kind = GapKind::Mount;
            self.gap_started_at.get_or_insert(now);
            let repaint_outer = std::mem::replace(&mut self.placeholder_active, false);
            self.last_fill = None;
            return (
                PrepareOutcome {
                    hold_direct_target: false,
                    repaint_outer,
                    arm_frame_callback: None,
                },
                None,
            );
        }

        if self.mount_hidden {
            self.gate = CommitGate::Committed;
            self.callback_in_flight = None;
            self.gap_started_at.get_or_insert(now);
            let repaint_outer = std::mem::replace(&mut self.placeholder_active, false);
            self.last_fill = None;
            return (
                PrepareOutcome {
                    hold_direct_target: false,
                    repaint_outer,
                    arm_frame_callback: None,
                },
                None,
            );
        }

        let Some(fill) = inputs.fill else {
            self.gate = CommitGate::Idle;
            self.callback_in_flight = None;
            self.was_texture = inputs.active_target_is_texture;
            let repaint_outer = std::mem::replace(&mut self.placeholder_active, false);
            self.last_fill = None;
            return (
                PrepareOutcome {
                    hold_direct_target: false,
                    repaint_outer,
                    arm_frame_callback: None,
                },
                None,
            );
        };

        let mut repaint_outer = !self.placeholder_active || self.last_fill != Some(fill);
        self.placeholder_active = true;
        self.last_fill = Some(fill);

        if inputs.active_target_is_texture || !inputs.has_mounted_direct {
            self.gate = CommitGate::Committed;
            self.callback_in_flight = None;
            return (
                PrepareOutcome {
                    hold_direct_target: false,
                    repaint_outer,
                    arm_frame_callback: None,
                },
                None,
            );
        }

        let mut arm_frame_callback = None;
        match self.gate {
            CommitGate::Idle => {
                self.generation = self.generation.wrapping_add(1);
                let generation = self.generation;
                self.gate = CommitGate::WaitingDraw {
                    generation,
                    vsync_polls: 0,
                };
                self.callback_in_flight = Some(generation);
                repaint_outer = true;
                arm_frame_callback = Some(generation);
            }
            CommitGate::WaitingDraw { generation, .. }
            | CommitGate::DrawnWaitingCommit { generation } => {
                if self.callback_in_flight != Some(generation) {
                    self.callback_in_flight = Some(generation);
                    arm_frame_callback = Some(generation);
                }
            }
            CommitGate::Committed => {}
        }

        let hold_direct_target =
            inputs.desired_surface_ready && !matches!(self.gate, CommitGate::Committed);

        (
            PrepareOutcome {
                hold_direct_target,
                repaint_outer,
                arm_frame_callback,
            },
            None,
        )
    }

    pub(crate) fn note_placeholder_drawn(&mut self) -> Option<u32> {
        if let CommitGate::WaitingDraw { generation, .. } = self.gate {
            self.gate = CommitGate::DrawnWaitingCommit { generation };
            if self.callback_in_flight != Some(generation) {
                self.callback_in_flight = Some(generation);
                return Some(generation);
            }
        }
        None
    }

    pub(crate) fn on_frame_callback(&mut self, generation: u32) -> FrameCallbackStep {
        if self.callback_in_flight == Some(generation) {
            self.callback_in_flight = None;
        }
        match self.gate {
            CommitGate::WaitingDraw {
                generation: active,
                vsync_polls,
            } if active == generation => {
                if vsync_polls == 0 {
                    self.gate = CommitGate::WaitingDraw {
                        generation,
                        vsync_polls: 1,
                    };
                    self.callback_in_flight = Some(generation);
                    FrameCallbackStep::ReArm
                } else {
                    // A full VSync render pass elapsed without ArkUI invoking
                    // `on_custom_draw` (offscreen/culled node); release the gate.
                    self.gate = CommitGate::Committed;
                    FrameCallbackStep::ReadyToDetach
                }
            }
            CommitGate::DrawnWaitingCommit { generation: active } if active == generation => {
                self.gate = CommitGate::Committed;
                FrameCallbackStep::ReadyToDetach
            }
            _ => FrameCallbackStep::Stale,
        }
    }

    pub(crate) fn force_commit(&mut self, generation: u32) -> bool {
        let active = match self.gate {
            CommitGate::WaitingDraw { generation: g, .. }
            | CommitGate::DrawnWaitingCommit { generation: g } => g == generation,
            _ => false,
        };
        if active {
            self.callback_in_flight = None;
            self.gate = CommitGate::Committed;
            true
        } else {
            false
        }
    }

    pub(crate) fn note_target_updated(
        &mut self,
        active_target_is_texture: bool,
        revealed: bool,
        now: Instant,
    ) {
        if !self.was_texture && active_target_is_texture && !revealed {
            self.gap_started_at.get_or_insert(now);
        }
        self.was_texture = active_target_is_texture;
    }

    pub(crate) fn note_revealed(&mut self, now: Instant) -> (bool, Option<(Duration, GapKind)>) {
        let kind = self.gap_kind;
        self.gate = CommitGate::Idle;
        self.callback_in_flight = None;
        self.in_handoff = false;
        self.mount_hidden = false;
        self.gap_kind = GapKind::FallbackCutover;
        let stopped_placeholder = std::mem::replace(&mut self.placeholder_active, false);
        self.last_fill = None;
        let gap = self
            .gap_started_at
            .take()
            .map(|started| (now.saturating_duration_since(started), kind));
        (stopped_placeholder, gap)
    }

    pub(crate) fn reset(&mut self) {
        self.gate = CommitGate::Idle;
        self.callback_in_flight = None;
        self.placeholder_active = false;
        self.last_fill = None;
        self.was_texture = false;
        self.in_handoff = false;
        self.mount_hidden = false;
        self.gap_kind = GapKind::FallbackCutover;
        self.gap_started_at = None;
        self.generation = self.generation.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARK_BG: PlaceholderFill = PlaceholderFill {
        argb: 0xff09090b,
        radii: [0.0; 4],
    };

    fn base_inputs() -> HandoffInputs {
        HandoffInputs {
            closed: false,
            fill: Some(DARK_BG),
            is_root: false,
            is_mount_or_prewarm: false,
            covered_by_live_bridge: false,
            has_mounted_direct: true,
            active_target_is_texture: false,
            export_requested: false,
            desired_surface_ready: false,
            revealed: false,
        }
    }

    #[test]
    fn fast_surface_arrival_holds_direct_root_until_placeholder_commits() {
        let mut state = HandoffState::default();
        let t0 = Instant::now();
        let mut inputs = base_inputs();
        inputs.export_requested = true;

        // Turn 0: Composite created (`surface_id` not ready yet). Placeholder arms immediately.
        let (out, gap) = state.prepare_target(inputs, t0);
        assert!(gap.is_none());
        assert!(!out.hold_direct_target);
        assert!(out.repaint_outer);
        let generation = out.arm_frame_callback.expect("armed callback");
        assert_eq!(state.active_fill(), Some(DARK_BG));
        state.note_target_updated(false, false, t0);

        // +11ms (`T2`/`T4` race): `surface_id` arrives BEFORE VSync 1 committed placeholder!
        inputs.desired_surface_ready = true;
        let (out, gap) = state.prepare_target(inputs, t0 + Duration::from_millis(11));
        assert!(gap.is_none());
        assert!(out.hold_direct_target);
        assert!(out.arm_frame_callback.is_none());

        // Start of VSync 1 (Phase 1, before FlushRenderTask): callback #1 sees WaitingDraw -> ReArm.
        assert_eq!(
            state.on_frame_callback(generation),
            FrameCallbackStep::ReArm
        );

        // VSync 1 FlushRenderTask (Phase 3): `on_custom_draw` records solid fill; callback #2 already armed.
        assert_eq!(state.note_placeholder_drawn(), None);

        // Start of VSync 2 (Phase 1, after VSync 1 FlushMessages committed to render_service):
        assert_eq!(
            state.on_frame_callback(generation),
            FrameCallbackStep::ReadyToDetach
        );

        // Cutover runs in VSync 2: DirectRoot detaches, gap timer starts.
        let detach_at = t0 + Duration::from_millis(19);
        let (out, gap) = state.prepare_target(inputs, detach_at);
        assert!(gap.is_none());
        assert!(!out.hold_direct_target);
        state.note_target_updated(true, false, detach_at);
        inputs.active_target_is_texture = true;
        inputs.has_mounted_direct = false;

        // Placeholder stays active across the entire SetTextureExport gap.
        let (out, gap) = state.prepare_target(inputs, detach_at + Duration::from_millis(100));
        assert!(gap.is_none());
        assert!(!out.repaint_outer);
        assert_eq!(state.active_fill(), Some(DARK_BG));

        // First GLES swap reveals Presentation -> stops placeholder in same turn and reports gap.
        let reveal_at = detach_at + Duration::from_millis(180);
        let (stopped, gap) = state.note_revealed(reveal_at);
        assert!(stopped);
        assert_eq!(
            gap,
            Some((Duration::from_millis(180), GapKind::FallbackCutover))
        );
        assert_eq!(GapKind::FallbackCutover.log_tag(), "handoff-gap");
        assert_eq!(GapOutcome::Revealed.as_str(), "revealed");
        assert_eq!(state.active_fill(), None);

        // Follow-up `update_wrapper()` in `composite_ready` does not double-report or repaint.
        inputs.revealed = true;
        let (out, gap) = state.prepare_target(inputs, reveal_at);
        assert!(gap.is_none());
        assert!(!out.repaint_outer);
        assert_eq!(state.active_fill(), None);
    }

    #[test]
    fn non_root_mount_stays_hidden_without_placeholder_and_reports_mount_gap() {
        let mut state = HandoffState::default();
        let t0 = Instant::now();
        state.begin_mount_export(t0);

        let mut inputs = base_inputs();
        inputs.is_root = false;
        inputs.is_mount_or_prewarm = true;
        inputs.has_mounted_direct = false;
        inputs.export_requested = true;
        inputs.desired_surface_ready = true;

        // Turn 1 (`surface_id` ready, attaching TextureContainer directly): no placeholder box!
        let t1 = t0 + Duration::from_millis(4);
        let (out, gap) = state.prepare_target(inputs, t1);
        assert!(gap.is_none());
        assert!(!out.hold_direct_target);
        assert!(!out.repaint_outer);
        assert!(out.arm_frame_callback.is_none());
        assert_eq!(state.active_fill(), None);
        state.note_target_updated(true, false, t1);

        // Turn 1.5 (style/frame update while waiting for frame 0 swap): still no placeholder!
        inputs.active_target_is_texture = true;
        let (out, gap) = state.prepare_target(inputs, t0 + Duration::from_millis(25));
        assert!(gap.is_none());
        assert!(!out.repaint_outer);
        assert_eq!(state.active_fill(), None);

        // Turn 2 (frame 0 swapped): reports total mount gap from t0 with `GapKind::Mount`.
        let (stopped, gap) = state.note_revealed(t0 + Duration::from_millis(62));
        assert!(!stopped);
        assert_eq!(gap, Some((Duration::from_millis(62), GapKind::Mount)));
        assert_eq!(GapKind::Mount.log_tag(), "shader-mount-gap");
    }

    #[test]
    fn root_prewarm_uses_placeholder_protection_and_reports_mount_gap() {
        let mut state = HandoffState::default();
        let t0 = Instant::now();
        let mut inputs = base_inputs();
        inputs.is_root = true;
        inputs.is_mount_or_prewarm = true;
        inputs.has_mounted_direct = false;
        inputs.export_requested = true;
        inputs.desired_surface_ready = true;

        let (out, _) = state.prepare_target(inputs, t0);
        assert!(out.repaint_outer);
        assert_eq!(state.active_fill(), Some(DARK_BG));
        state.note_target_updated(true, false, t0);
        inputs.active_target_is_texture = true;
        // Even if a shader toggles on before pre-warm reveal finishes, GapKind::Mount stays latched.
        inputs.is_mount_or_prewarm = false;
        let _ = state.prepare_target(inputs, t0 + Duration::from_millis(30));

        let (stopped, gap) = state.note_revealed(t0 + Duration::from_millis(110));
        assert!(stopped);
        assert_eq!(gap, Some((Duration::from_millis(110), GapKind::Mount)));
    }

    #[test]
    fn non_opaque_owner_paints_nothing_and_never_gates_detach() {
        let mut state = HandoffState::default();
        let t0 = Instant::now();
        let mut inputs = base_inputs();
        inputs.fill = None;
        inputs.export_requested = true;
        inputs.desired_surface_ready = true;

        let (out, gap) = state.prepare_target(inputs, t0);
        assert!(gap.is_none());
        assert!(!out.hold_direct_target);
        assert!(!out.repaint_outer);
        assert!(out.arm_frame_callback.is_none());
        assert_eq!(state.active_fill(), None);

        state.note_target_updated(true, false, t0);
        let (stopped, gap) = state.note_revealed(t0 + Duration::from_millis(95));
        assert!(!stopped);
        assert_eq!(
            gap,
            Some((Duration::from_millis(95), GapKind::FallbackCutover))
        );
    }

    #[test]
    fn offscreen_culled_node_releases_commit_gate_after_one_skipped_render_pass() {
        let mut state = HandoffState::default();
        let t0 = Instant::now();
        let mut inputs = base_inputs();
        inputs.export_requested = true;
        inputs.desired_surface_ready = true;

        let (out, _) = state.prepare_target(inputs, t0);
        assert!(out.hold_direct_target);
        let generation = out.arm_frame_callback.expect("armed");

        // VSync 1 Phase 1 -> ReArm; VSync 1 FlushRenderTask culls offscreen `on_custom_draw`.
        assert_eq!(
            state.on_frame_callback(generation),
            FrameCallbackStep::ReArm
        );
        // VSync 2 Phase 1 -> releases gate so offscreen node never stalls.
        assert_eq!(
            state.on_frame_callback(generation),
            FrameCallbackStep::ReadyToDetach
        );
        let (out, _) = state.prepare_target(inputs, t0 + Duration::from_millis(16));
        assert!(!out.hold_direct_target);
    }

    #[test]
    fn unrevealed_cancel_clears_placeholder_and_reports_cancelled_gap() {
        let mut state = HandoffState::default();
        let t0 = Instant::now();
        let mut inputs = base_inputs();
        inputs.is_root = true;
        inputs.is_mount_or_prewarm = true;
        inputs.has_mounted_direct = false;
        inputs.export_requested = true;
        inputs.desired_surface_ready = true;

        let (out, _) = state.prepare_target(inputs, t0);
        assert!(!out.hold_direct_target);
        state.note_target_updated(true, false, t0);
        inputs.active_target_is_texture = true;
        assert_eq!(state.active_fill(), Some(DARK_BG));

        // Cancelled before Presentation reveals -> back to DirectRoot.
        inputs.export_requested = false;
        inputs.desired_surface_ready = false;
        inputs.active_target_is_texture = false;
        let (out, gap) = state.prepare_target(inputs, t0 + Duration::from_millis(64));
        assert!(out.repaint_outer);
        assert_eq!(state.active_fill(), None);
        let (dur, outcome, kind) = gap.expect("cancelled gap");
        assert_eq!(dur, Duration::from_millis(64));
        assert_eq!(outcome, GapOutcome::Cancelled);
        assert_eq!(kind, GapKind::Mount);
        assert_eq!(outcome.as_str(), "cancelled");
    }

    #[test]
    fn draw_without_in_flight_callback_requests_commit_callback_and_force_commit_resets() {
        let mut state = HandoffState::default();
        let t0 = Instant::now();
        let mut inputs = base_inputs();
        inputs.export_requested = true;

        let (out, _) = state.prepare_target(inputs, t0);
        let generation = out.arm_frame_callback.expect("armed");
        // Simulate failed callback registration recovered by on_custom_draw, then force_commit.
        state.callback_in_flight = None;
        assert_eq!(state.note_placeholder_drawn(), Some(generation));
        assert!(!state.force_commit(generation.wrapping_add(1)));
        assert!(state.force_commit(generation));

        state.reset();
        assert_eq!(
            state.on_frame_callback(generation),
            FrameCallbackStep::Stale
        );
        assert_eq!(state.active_fill(), None);
    }

    #[test]
    fn scene_root_reveal_via_prepare_target_stops_placeholder_and_reports_gap() {
        let mut state = HandoffState::default();
        let t0 = Instant::now();
        let mut inputs = base_inputs();
        inputs.is_root = true;
        inputs.has_mounted_direct = false;
        inputs.export_requested = true;
        inputs.desired_surface_ready = true;

        let _ = state.prepare_target(inputs, t0);
        state.note_target_updated(true, false, t0);
        inputs.active_target_is_texture = true;

        inputs.revealed = true;
        let (out, gap) = state.prepare_target(inputs, t0 + Duration::from_millis(140));
        assert!(out.repaint_outer);
        assert_eq!(state.active_fill(), None);
        assert_eq!(
            gap,
            Some((
                Duration::from_millis(140),
                GapOutcome::Revealed,
                GapKind::FallbackCutover
            ))
        );
    }

    #[test]
    fn foreground_rewarm_from_direct_root_logs_shader_mount_gap() {
        let mut state = HandoffState::default();
        let t0 = Instant::now();
        let mut inputs = base_inputs();
        inputs.has_mounted_direct = true;
        inputs.is_mount_or_prewarm = true;
        inputs.export_requested = true;
        inputs.desired_surface_ready = true;

        let (out, _) = state.prepare_target(inputs, t0);
        let generation = out.arm_frame_callback.expect("armed");
        assert!(out.hold_direct_target);
        assert_eq!(state.note_placeholder_drawn(), None);
        assert_eq!(
            state.on_frame_callback(generation),
            FrameCallbackStep::ReadyToDetach
        );

        let (out2, _) = state.prepare_target(inputs, t0 + Duration::from_millis(8));
        assert!(!out2.hold_direct_target);
        state.note_target_updated(true, false, t0 + Duration::from_millis(8));

        let (stopped, gap) = state.note_revealed(t0 + Duration::from_millis(120));
        assert!(stopped);
        assert_eq!(gap, Some((Duration::from_millis(112), GapKind::Mount)));
        assert_eq!(GapKind::Mount.log_tag(), "shader-mount-gap");
    }

    #[test]
    fn root_live_bridge_skips_placeholder_and_commit_gate_and_reports_mount_gap() {
        let mut state = HandoffState::default();
        let t0 = Instant::now();
        let mut inputs = base_inputs();
        inputs.is_root = true;
        inputs.is_mount_or_prewarm = true;
        inputs.has_mounted_direct = true;
        inputs.export_requested = true;

        // Initial host reconciliation in the same batch before `stack::reconcile` installs bridge.
        let (out0, _) = state.prepare_target(inputs, t0);
        let stale_gen = out0.arm_frame_callback.expect("initially armed");
        assert_eq!(state.active_fill(), Some(DARK_BG));

        // `stack::reconcile` installs live `peer` bridge on `ROOT_ID` in the same UI turn.
        inputs.covered_by_live_bridge = true;
        inputs.desired_surface_ready = true;
        let (out1, gap) = state.prepare_target(inputs, t0);
        assert!(gap.is_none());
        assert!(!out1.hold_direct_target);
        assert!(out1.repaint_outer);
        assert!(out1.arm_frame_callback.is_none());
        assert_eq!(state.active_fill(), None);
        assert_eq!(state.on_frame_callback(stale_gen), FrameCallbackStep::Stale);

        state.note_target_updated(true, false, t0 + Duration::from_millis(18));
        let (stopped, gap) = state.note_revealed(t0 + Duration::from_millis(118));
        assert!(!stopped);
        assert_eq!(gap, Some((Duration::from_millis(118), GapKind::Mount)));
    }
}
