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
            self.gap_kind = GapKind::Mount;
            self.gap_started_at.get_or_insert(now);
            self.gate = CommitGate::Committed;
            self.callback_in_flight = None;
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

/// Frame callbacks between detaching a bridged `Screen` from `wrapper.outer`
/// (`RenderService` pipeline) and re-inserting it into `BuilderNode` (`TextureExport`
/// pipeline). Matches `texture_export.rs`: two frame callbacks leave one complete
/// `RenderService` tree rebuild so `wrapper.outer`'s `RSNode::RemoveChild` executes
/// before `RSNode::SetTextureExport(true)` switches the child's target side.
pub(crate) const BRIDGE_REATTACH_SETTLE_FRAMES: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BridgeReattachPlan {
    Immediate,
    Defer {
        flush_alpha_on_outer: bool,
        settle_frames: u8,
    },
}

/// Places a bridged `Screen` after the active `NativeAttachment` child (if mounted on
/// `wrapper.outer`) and strictly before any `Presentation` (`XComponent`) overlays.
pub(crate) fn bridge_insert_index(outer_len: usize, target_child_index: Option<usize>) -> usize {
    target_child_index.map_or(0, |idx| (idx + 1).min(outer_len))
}

pub(crate) fn plan_bridge_reattach(
    closed: bool,
    outer_owned: bool,
    destination_is_texture_export: bool,
) -> BridgeReattachPlan {
    if !closed && outer_owned && destination_is_texture_export {
        BridgeReattachPlan::Defer {
            flush_alpha_on_outer: true,
            settle_frames: BRIDGE_REATTACH_SETTLE_FRAMES,
        }
    } else {
        BridgeReattachPlan::Immediate
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReattachPhase {
    HoldOnOuterUntilDirect { remaining_frames: u8 },
    FlushAlphaOnOuter { settle_frames: u8 },
    SettleDetached { remaining_frames: u8 },
}

#[derive(Debug)]
pub(crate) struct PendingReattachEntry<T> {
    pub id: u64,
    pub stack_id: u32,
    pub peer_id: u32,
    phase: ReattachPhase,
    pub payload: T,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReattachTick<T> {
    Stale,
    DetachFromOuter(T),
    WaitMore,
    Complete(T),
}

#[derive(Debug)]
pub(crate) struct PendingReattachQueue<T> {
    next_id: u64,
    entries: Vec<PendingReattachEntry<T>>,
}

impl<T> Default for PendingReattachQueue<T> {
    fn default() -> Self {
        Self {
            next_id: 1,
            entries: Vec::new(),
        }
    }
}

impl<T> PendingReattachQueue<T> {
    pub(crate) fn enqueue(
        &mut self,
        stack_id: u32,
        peer_id: u32,
        flush_alpha_on_outer: bool,
        settle_frames: u8,
        payload: T,
    ) -> (u64, Option<T>) {
        let displaced = self.take_peer(peer_id);
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let phase = if flush_alpha_on_outer {
            ReattachPhase::FlushAlphaOnOuter {
                settle_frames: settle_frames.max(1),
            }
        } else {
            ReattachPhase::SettleDetached {
                remaining_frames: settle_frames.max(1),
            }
        };
        self.entries.push(PendingReattachEntry {
            id,
            stack_id,
            peer_id,
            phase,
            payload,
        });
        (id, displaced)
    }

    pub(crate) fn enqueue_hold_until_direct(
        &mut self,
        stack_id: u32,
        peer_id: u32,
        max_frames: u8,
        payload: T,
    ) -> (u64, Option<T>) {
        let displaced = self.take_peer(peer_id);
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.entries.push(PendingReattachEntry {
            id,
            stack_id,
            peer_id,
            phase: ReattachPhase::HoldOnOuterUntilDirect {
                remaining_frames: max_frames.max(1),
            },
            payload,
        });
        (id, displaced)
    }

    pub(crate) fn promote_holds_for_export(
        &mut self,
        settle_frames: u8,
        mut on_promote: impl FnMut(&mut T),
    ) {
        let settle = settle_frames.max(1);
        for entry in &mut self.entries {
            if matches!(entry.phase, ReattachPhase::HoldOnOuterUntilDirect { .. }) {
                entry.phase = ReattachPhase::FlushAlphaOnOuter {
                    settle_frames: settle,
                };
                on_promote(&mut entry.payload);
            }
        }
    }

    pub(crate) fn on_frame_callback(&mut self, id: u64) -> ReattachTick<T>
    where
        T: Clone,
    {
        let Some(pos) = self.entries.iter().position(|entry| entry.id == id) else {
            return ReattachTick::Stale;
        };
        match self.entries[pos].phase {
            ReattachPhase::HoldOnOuterUntilDirect { remaining_frames } => {
                if remaining_frames > 1 {
                    self.entries[pos].phase = ReattachPhase::HoldOnOuterUntilDirect {
                        remaining_frames: remaining_frames - 1,
                    };
                    ReattachTick::WaitMore
                } else {
                    ReattachTick::Complete(self.entries.remove(pos).payload)
                }
            }
            ReattachPhase::FlushAlphaOnOuter { settle_frames } => {
                self.entries[pos].phase = ReattachPhase::SettleDetached {
                    remaining_frames: settle_frames.max(1),
                };
                ReattachTick::DetachFromOuter(self.entries[pos].payload.clone())
            }
            ReattachPhase::SettleDetached { remaining_frames } => {
                if remaining_frames > 1 {
                    self.entries[pos].phase = ReattachPhase::SettleDetached {
                        remaining_frames: remaining_frames - 1,
                    };
                    ReattachTick::WaitMore
                } else {
                    ReattachTick::Complete(self.entries.remove(pos).payload)
                }
            }
        }
    }

    pub(crate) fn iter_on_outer(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().filter_map(|entry| match entry.phase {
            ReattachPhase::HoldOnOuterUntilDirect { .. }
            | ReattachPhase::FlushAlphaOnOuter { .. } => Some(&entry.payload),
            ReattachPhase::SettleDetached { .. } => None,
        })
    }

    pub(crate) fn take_by_id(&mut self, id: u64) -> Option<T> {
        let pos = self.entries.iter().position(|entry| entry.id == id)?;
        Some(self.entries.remove(pos).payload)
    }

    pub(crate) fn take_peer(&mut self, peer_id: u32) -> Option<T> {
        let pos = self
            .entries
            .iter()
            .position(|entry| entry.peer_id == peer_id)?;
        Some(self.entries.remove(pos).payload)
    }

    pub(crate) fn take_referencing(&mut self, node_id: u32) -> Vec<T> {
        let mut taken = Vec::new();
        let mut i = 0;
        while i < self.entries.len() {
            if self.entries[i].peer_id == node_id || self.entries[i].stack_id == node_id {
                taken.push(self.entries.remove(i).payload);
            } else {
                i += 1;
            }
        }
        taken
    }

    pub(crate) fn drain_all(&mut self) -> Vec<T> {
        self.entries.drain(..).map(|entry| entry.payload).collect()
    }
}

/// Whether a retiring `Composite` pushed to `Wrapper::lingering` must hide its
/// `Presentation` surface as soon as `DirectRoot` mounts.
///
/// Opaque surfaces (`has_opaque_placeholder_fill == true`, e.g. `ROOT_ID`) keep
/// `Presentation` visible across the linger window so `DirectRoot` has time to
/// record custom-drawn children (`<Text>`, `<Canvas>`) without flashing blank.
/// Translucent subtrees (`has_opaque_placeholder_fill == false`, e.g. a
/// `FullScreen` `Modal` with `rgba(0,0,0,0.45)`) cannot stack `Presentation`
/// over `DirectRoot` without double-blending alpha (`0.45 + 0.45 * 0.55`), so
/// `Presentation` hides (`opacity = 0.0`) when `DirectRoot` commits while the
/// `Composite` stays retained in `lingering` for safe native surface release.
pub(crate) fn should_hide_lingering_presentation(has_opaque_placeholder_fill: bool) -> bool {
    !has_opaque_placeholder_fill
}

/// Whether a `Composite` whose owner no longer requires texture export should
/// drop immediately instead of entering the multi-frame `retire_composite`
/// `post_frame_callback` pipeline.
///
/// An unmounted/closed `Modal` (`unmounted_modal == true`) has already detached
/// `wrapper.outer` via `dialog.remove_content()`, so `post_frame_callback` will
/// never fire on `outer`; dropping synchronously avoids stranding the `Composite`.
pub(crate) fn should_immediately_retire_composite(
    hosts_retainable: bool,
    unmounted_modal: bool,
    composite_unrevealed: bool,
) -> bool {
    !hosts_retainable || unmounted_modal || composite_unrevealed
}

/// Whether a child node of a retiring export wrapper must enter `clearing_export`
/// (`Visibility::None` = `2`) before switching back to `DirectRoot`.
///
/// Both active screens AND popped/`pending_delete` screens are included so they stay
/// at `Visibility = 2` across `DirectRoot` attachment and never undergo `Export::hide`
/// (`Visibility = 0`), which would re-attach hardware `XComponent` `RSSurfaceRenderNode`s
/// that ignore ancestor canvas opacity.
pub(crate) fn should_clear_screen_on_export_retire(
    active: bool,
    retained: bool,
    has_modal: bool,
    is_screen: bool,
) -> bool {
    active && !retained && !has_modal && is_screen
}

/// Whether `Export::walk` may visit or call `Export::hide` (`Visibility = 0`) on an owner.
///
/// Screens held in `clearing_export`, queued in `pending_delete`, or already settled in
/// a popped state must never be flipped to `Visibility = 0` during export repair; popped
/// screens are instead removed on `DirectRoot` via `delete_now` + `Reclaim` while the
/// retiring `Presentation` remains opaque.
pub(crate) fn should_export_repair_visit_owner(
    clearing_export: bool,
    pending_delete: bool,
    popped_unanimated_screen: bool,
) -> bool {
    !clearing_export && !pending_delete && !popped_unanimated_screen
}

/// Committed VSyncs after `DirectRoot` attaches before hiding an opaque lingering
/// `Presentation` (`> 3` `Export::after_attach` repair frames + custom draw rebuilds).
pub(crate) const LINGERING_OPAQUE_COMMIT_FRAMES: u8 = 6;
/// Minimum committed VSyncs *after* `Export` repair and pending deletions/reclaims finish
/// before hiding an opaque lingering `Presentation`, ensuring `Visibility::None`
/// restoration and `~RSNode` destruction on `DirectRoot` have flushed to `render_service`.
pub(crate) const LINGERING_POST_REPAIR_COMMIT_FRAMES: u8 = 2;
/// Committed VSyncs after `DirectRoot` attaches before hiding a translucent lingering
/// `Presentation` (avoids stacking semi-transparent scrims while ensuring `DirectRoot` drew).
pub(crate) const LINGERING_TRANSLUCENT_COMMIT_FRAMES: u8 = 1;
/// VSyncs between setting `Presentation` `opacity = 0.0` and disposing its `XComponent`.
pub(crate) const LINGERING_DISPOSE_SETTLE_FRAMES: u8 = 3;
/// Safety fallback VSyncs if `DirectRoot` never attaches (e.g. host torn down).
pub(crate) const LINGERING_ATTACH_FALLBACK_FRAMES: u8 = 90;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LingeringPhase {
    WaitingDirectAttach {
        remaining_fallback: u8,
        commit_frames: u8,
        await_export_repair: bool,
    },
    WaitingDirectCommit {
        remaining_frames: u8,
        await_export_repair: bool,
        remaining_repair_fallback: u8,
    },
    HiddenWaitingDispose {
        remaining_frames: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LingeringGateEntry {
    generation: u32,
    phase: LingeringPhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LingeringTick {
    Stale,
    WaitMore,
    HideAndWaitMore,
    ReleaseFallbackHoldsHideAndWaitMore,
    Dispose,
}

#[derive(Default, Debug)]
pub(crate) struct LingeringQueue {
    entries: Vec<LingeringGateEntry>,
}

impl LingeringQueue {
    pub(crate) fn enqueue(&mut self, generation: u32, has_opaque_fill: bool, already_direct: bool) {
        self.remove(generation);
        let hide_on_commit = should_hide_lingering_presentation(has_opaque_fill);
        let commit_frames = if hide_on_commit {
            LINGERING_TRANSLUCENT_COMMIT_FRAMES
        } else {
            LINGERING_OPAQUE_COMMIT_FRAMES
        };
        let await_export_repair = !hide_on_commit;
        let phase = if already_direct {
            LingeringPhase::WaitingDirectCommit {
                remaining_frames: commit_frames,
                await_export_repair,
                remaining_repair_fallback: LINGERING_ATTACH_FALLBACK_FRAMES,
            }
        } else {
            LingeringPhase::WaitingDirectAttach {
                remaining_fallback: LINGERING_ATTACH_FALLBACK_FRAMES,
                commit_frames,
                await_export_repair,
            }
        };
        self.entries.push(LingeringGateEntry { generation, phase });
    }

    pub(crate) fn on_direct_attached(&mut self) -> Vec<u32> {
        let mut promoted = Vec::new();
        for entry in &mut self.entries {
            if let LingeringPhase::WaitingDirectAttach {
                commit_frames,
                await_export_repair,
                ..
            } = entry.phase
            {
                entry.phase = LingeringPhase::WaitingDirectCommit {
                    remaining_frames: commit_frames,
                    await_export_repair,
                    remaining_repair_fallback: LINGERING_ATTACH_FALLBACK_FRAMES,
                };
                promoted.push(entry.generation);
            }
        }
        promoted
    }

    pub(crate) fn mark_all_hidden(&mut self) -> Vec<u32> {
        let mut hidden = Vec::new();
        for entry in &mut self.entries {
            if !matches!(entry.phase, LingeringPhase::HiddenWaitingDispose { .. }) {
                entry.phase = LingeringPhase::HiddenWaitingDispose {
                    remaining_frames: LINGERING_DISPOSE_SETTLE_FRAMES,
                };
                hidden.push(entry.generation);
            }
        }
        hidden
    }

    pub(crate) fn is_waiting_direct_attach(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.phase, LingeringPhase::WaitingDirectAttach { .. }))
    }

    pub(crate) fn remove(&mut self, generation: u32) {
        self.entries.retain(|entry| entry.generation != generation);
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(crate) fn on_frame_callback(
        &mut self,
        generation: u32,
        export_repairing: bool,
        pending_deletions: bool,
    ) -> LingeringTick {
        let Some(pos) = self
            .entries
            .iter()
            .position(|entry| entry.generation == generation)
        else {
            return LingeringTick::Stale;
        };
        match self.entries[pos].phase {
            LingeringPhase::WaitingDirectAttach {
                remaining_fallback,
                commit_frames,
                await_export_repair,
            } => {
                if remaining_fallback > 1 {
                    self.entries[pos].phase = LingeringPhase::WaitingDirectAttach {
                        remaining_fallback: remaining_fallback - 1,
                        commit_frames,
                        await_export_repair,
                    };
                    LingeringTick::WaitMore
                } else {
                    self.entries[pos].phase = LingeringPhase::HiddenWaitingDispose {
                        remaining_frames: LINGERING_DISPOSE_SETTLE_FRAMES,
                    };
                    LingeringTick::ReleaseFallbackHoldsHideAndWaitMore
                }
            }
            LingeringPhase::WaitingDirectCommit {
                remaining_frames,
                await_export_repair,
                remaining_repair_fallback,
            } => {
                if await_export_repair
                    && (export_repairing || pending_deletions)
                    && remaining_repair_fallback > 1
                {
                    self.entries[pos].phase = LingeringPhase::WaitingDirectCommit {
                        remaining_frames: remaining_frames
                            .saturating_sub(1)
                            .max(LINGERING_POST_REPAIR_COMMIT_FRAMES),
                        await_export_repair,
                        remaining_repair_fallback: remaining_repair_fallback - 1,
                    };
                    LingeringTick::WaitMore
                } else if remaining_frames > 1 {
                    self.entries[pos].phase = LingeringPhase::WaitingDirectCommit {
                        remaining_frames: remaining_frames - 1,
                        await_export_repair,
                        remaining_repair_fallback,
                    };
                    LingeringTick::WaitMore
                } else {
                    self.entries[pos].phase = LingeringPhase::HiddenWaitingDispose {
                        remaining_frames: LINGERING_DISPOSE_SETTLE_FRAMES,
                    };
                    LingeringTick::HideAndWaitMore
                }
            }
            LingeringPhase::HiddenWaitingDispose { remaining_frames } => {
                if remaining_frames > 1 {
                    self.entries[pos].phase = LingeringPhase::HiddenWaitingDispose {
                        remaining_frames: remaining_frames - 1,
                    };
                    LingeringTick::WaitMore
                } else {
                    self.entries.remove(pos);
                    LingeringTick::Dispose
                }
            }
        }
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
    fn root_live_bridge_clears_placeholder_without_holding_direct_and_reports_mount_gap() {
        let mut state = HandoffState::default();
        let t0 = Instant::now();
        let mut inputs = base_inputs();
        inputs.is_root = true;
        inputs.is_mount_or_prewarm = false;
        inputs.has_mounted_direct = true;
        inputs.export_requested = true;

        // Initial host reconciliation in the same batch before `stack::reconcile` installs bridge.
        let (out0, _) = state.prepare_target(inputs, t0);
        let generation = out0.arm_frame_callback.expect("initially armed");
        assert_eq!(state.active_fill(), Some(DARK_BG));

        // `stack::reconcile` installs live `peer` bridge on `ROOT_ID` in the same UI turn.
        // Live `peer` on `outer` covers the viewport directly, so placeholder fill is cleared
        // with zero color assumptions and `DirectRoot` is not held on a placeholder draw gate.
        inputs.covered_by_live_bridge = true;
        inputs.desired_surface_ready = true;
        let (out1, gap) = state.prepare_target(inputs, t0);
        assert!(gap.is_none());
        assert!(!out1.hold_direct_target);
        assert!(out1.repaint_outer);
        assert!(out1.arm_frame_callback.is_none());
        assert_eq!(state.active_fill(), None);
        assert_eq!(
            state.on_frame_callback(generation),
            FrameCallbackStep::Stale
        );

        state.note_target_updated(true, false, t0 + Duration::from_millis(12));
        let (stopped, gap) = state.note_revealed(t0 + Duration::from_millis(118));
        assert!(!stopped);
        assert_eq!(gap, Some((Duration::from_millis(118), GapKind::Mount)));
    }

    #[test]
    fn hold_until_direct_keeps_peer_on_outer_and_promotes_if_export_resumes() {
        let mut queue = PendingReattachQueue::default();
        let (id, _) = queue.enqueue_hold_until_direct(10, 20, 3, "home");
        assert_eq!(queue.iter_on_outer().copied().collect::<Vec<_>>(), ["home"]);
        assert_eq!(queue.on_frame_callback(id), ReattachTick::WaitMore);
        assert_eq!(queue.iter_on_outer().copied().collect::<Vec<_>>(), ["home"]);

        // If a new Composite reveals before DirectRoot attaches, promote to FlushAlphaOnOuter.
        let mut promoted = Vec::new();
        queue.promote_holds_for_export(BRIDGE_REATTACH_SETTLE_FRAMES, |p| promoted.push(*p));
        assert_eq!(promoted, vec!["home"]);
        assert_eq!(
            queue.on_frame_callback(id),
            ReattachTick::DetachFromOuter("home")
        );
        assert_eq!(queue.on_frame_callback(id), ReattachTick::WaitMore);
        assert_eq!(queue.on_frame_callback(id), ReattachTick::Complete("home"));
    }

    #[test]
    fn lingering_queue_waits_for_direct_attach_then_commits_and_disposes() {
        let mut q = LingeringQueue::default();
        q.enqueue(7, true, false);
        assert!(q.is_waiting_direct_attach());

        // Before DirectRoot attaches, ticking stays in WaitingDirectAttach without hiding early.
        for _ in 0..15 {
            assert_eq!(
                q.on_frame_callback(7, false, false),
                LingeringTick::WaitMore
            );
        }

        // DirectRoot attaches -> promotes to 6-frame commit countdown.
        assert_eq!(q.on_direct_attached(), vec![7]);
        assert!(!q.is_waiting_direct_attach());
        for _ in 0..(LINGERING_OPAQUE_COMMIT_FRAMES - 1) {
            assert_eq!(
                q.on_frame_callback(7, false, false),
                LingeringTick::WaitMore
            );
        }
        assert_eq!(
            q.on_frame_callback(7, false, false),
            LingeringTick::HideAndWaitMore
        );

        // Settles 3 frames at opacity 0 before disposing XComponent.
        for _ in 0..(LINGERING_DISPOSE_SETTLE_FRAMES - 1) {
            assert_eq!(
                q.on_frame_callback(7, false, false),
                LingeringTick::WaitMore
            );
        }
        assert_eq!(q.on_frame_callback(7, false, false), LingeringTick::Dispose);
        assert_eq!(q.on_frame_callback(7, false, false), LingeringTick::Stale);

        // If `Export` repair takes longer than `LINGERING_OPAQUE_COMMIT_FRAMES`, opaque lingering
        // `Presentation` stays visible until `!export_repairing` + `LINGERING_POST_REPAIR_COMMIT_FRAMES`.
        q.enqueue(11, true, true);
        for _ in 0..12 {
            assert_eq!(
                q.on_frame_callback(11, true, false),
                LingeringTick::WaitMore
            );
        }
        for _ in 0..(LINGERING_POST_REPAIR_COMMIT_FRAMES - 1) {
            assert_eq!(
                q.on_frame_callback(11, false, false),
                LingeringTick::WaitMore
            );
        }
        assert_eq!(
            q.on_frame_callback(11, false, false),
            LingeringTick::HideAndWaitMore
        );

        // Successor Composite reveal hides immediately and still waits settle frames before dispose.
        q.enqueue(8, true, false);
        assert_eq!(q.mark_all_hidden(), vec![8]);
        assert!(q.mark_all_hidden().is_empty());
        for _ in 0..(LINGERING_DISPOSE_SETTLE_FRAMES - 1) {
            assert_eq!(
                q.on_frame_callback(8, false, false),
                LingeringTick::WaitMore
            );
        }
        assert_eq!(q.on_frame_callback(8, false, false), LingeringTick::Dispose);

        // Translucent already-direct commits in 1 frame even during export repair, fallback releases holds, and clear resets.
        q.enqueue(9, false, true);
        assert_eq!(
            q.on_frame_callback(9, true, true),
            LingeringTick::HideAndWaitMore
        );
        q.clear();
        assert_eq!(q.on_frame_callback(9, false, false), LingeringTick::Stale);

        q.enqueue(10, true, false);
        for _ in 0..(LINGERING_ATTACH_FALLBACK_FRAMES - 1) {
            assert_eq!(
                q.on_frame_callback(10, false, false),
                LingeringTick::WaitMore
            );
        }
        assert_eq!(
            q.on_frame_callback(10, false, false),
            LingeringTick::ReleaseFallbackHoldsHideAndWaitMore
        );
    }

    #[test]
    fn popped_and_pending_delete_screens_stay_in_clearing_export_and_never_export_hide() {
        // Active screens AND popped/pending-delete screens (active == true, retained == false)
        // both enter `clearing_export` (`Visibility::None` = 2) when retiring `ExportRoot`.
        assert!(should_clear_screen_on_export_retire(
            true, false, false, true
        ));
        // Inactive, retained, modal, or non-screen nodes do not enter screen clearing_export.
        assert!(!should_clear_screen_on_export_retire(
            false, false, false, true
        ));
        assert!(!should_clear_screen_on_export_retire(
            true, true, false, true
        ));
        assert!(!should_clear_screen_on_export_retire(
            true, false, true, true
        ));
        assert!(!should_clear_screen_on_export_retire(
            true, false, false, false
        ));

        // Popped, `pending_delete`, and `clearing_export` screens are NEVER visited by
        // `Export::walk` / `Export::hide` (`Visibility = 0`), preventing dead descendant
        // `XComponent` `RSSurfaceRenderNode`s from re-attaching on `render_service`.
        assert!(!should_export_repair_visit_owner(true, false, false));
        assert!(!should_export_repair_visit_owner(false, true, false));
        assert!(!should_export_repair_visit_owner(false, false, true));
        assert!(!should_export_repair_visit_owner(true, true, true));
        // Normal retained/hidden non-popped views still undergo `Export` reconciliation.
        assert!(should_export_repair_visit_owner(false, false, false));
    }

    #[test]
    fn lingering_queue_holds_opaque_presentation_until_pending_deletions_and_reclaim_drain() {
        let mut q = LingeringQueue::default();
        q.enqueue(42, true, false);
        assert!(q.is_waiting_direct_attach());

        // `DirectRoot` attaches while `<Shader>` is still in `pending_delete` / `retired` (`Reclaim`).
        assert_eq!(q.on_direct_attached(), vec![42]);
        assert!(!q.is_waiting_direct_attach());

        // Even after `LINGERING_OPAQUE_COMMIT_FRAMES` have elapsed and `export_repairing` is false,
        // `Presentation` MUST stay opaque (`WaitMore`) while `pending_deletions` is true, including
        // when `pending_deletions` becomes/remains true partway through `WaitingDirectCommit`.
        assert_eq!(
            q.on_frame_callback(42, false, false),
            LingeringTick::WaitMore
        );
        for _ in 0..20 {
            assert_eq!(
                q.on_frame_callback(42, false, true),
                LingeringTick::WaitMore
            );
        }

        // Once `delete_now` + `Message::Reclaim` drain (`pending_deletions == false`), `LingeringQueue`
        // still commits `LINGERING_POST_REPAIR_COMMIT_FRAMES` VSyncs so `~RSNode` (`RSNodeDestroy`)
        // flushes to `render_service` before hiding `Presentation`.
        for _ in 0..(LINGERING_POST_REPAIR_COMMIT_FRAMES - 1) {
            assert_eq!(
                q.on_frame_callback(42, false, false),
                LingeringTick::WaitMore
            );
        }
        assert_eq!(
            q.on_frame_callback(42, false, false),
            LingeringTick::HideAndWaitMore
        );
    }

    #[test]
    fn bridge_insert_index_places_peer_after_target_and_before_presentation() {
        // `[0: DirectRoot, 1: Presentation]` -> insert `peer` at `1` (`[DirectRoot, peer, Presentation]`).
        assert_eq!(bridge_insert_index(2, Some(0)), 1);
        // No target child attached yet (`[0: Presentation]`) -> insert `peer` at `0` (`[peer, Presentation]`).
        assert_eq!(bridge_insert_index(1, None), 0);
        // Empty `outer` -> `0`.
        assert_eq!(bridge_insert_index(0, None), 0);
        // Bounds clamp if target was last child.
        assert_eq!(bridge_insert_index(1, Some(0)), 1);
    }

    #[test]
    fn plan_bridge_reattach_defers_only_for_live_cross_pipeline_export() {
        assert_eq!(
            plan_bridge_reattach(false, true, true),
            BridgeReattachPlan::Defer {
                flush_alpha_on_outer: true,
                settle_frames: BRIDGE_REATTACH_SETTLE_FRAMES,
            }
        );
        assert_eq!(
            plan_bridge_reattach(false, true, false),
            BridgeReattachPlan::Immediate
        );
        assert_eq!(
            plan_bridge_reattach(true, true, true),
            BridgeReattachPlan::Immediate
        );
        assert_eq!(
            plan_bridge_reattach(false, false, true),
            BridgeReattachPlan::Immediate
        );
    }

    #[test]
    fn pending_reattach_queue_flushes_alpha_on_outer_then_settles_and_handles_cancel() {
        let mut queue = PendingReattachQueue::default();
        let (id1, displaced) = queue.enqueue(10, 20, true, BRIDGE_REATTACH_SETTLE_FRAMES, "home");
        assert_eq!(displaced, None);
        assert_eq!(queue.iter_on_outer().copied().collect::<Vec<_>>(), ["home"]);

        // Callback 1: `outer` flushed `RSNode::SetAlpha(0.0)` while `home` was still on main tree;
        // now detach `home` from `outer` and begin the 2-frame RS tree settle.
        assert_eq!(
            queue.on_frame_callback(id1),
            ReattachTick::DetachFromOuter("home")
        );
        assert!(queue.iter_on_outer().next().is_none());
        // Callback 2: `outer` rebuilt RS child list without `home`; wait 1 more settle frame.
        assert_eq!(queue.on_frame_callback(id1), ReattachTick::WaitMore);
        // Callback 3: RS settle complete; finish reattach into BuilderNode.
        assert_eq!(queue.on_frame_callback(id1), ReattachTick::Complete("home"));
        // Subsequent stale callbacks are ignored.
        assert_eq!(queue.on_frame_callback(id1), ReattachTick::Stale);

        // Enqueuing a duplicate `peer_id` returns the displaced payload so callers never leak state.
        let (old_id, None) = queue.enqueue(10, 20, true, BRIDGE_REATTACH_SETTLE_FRAMES, "old")
        else {
            panic!("expected no prior entry");
        };
        let (new_id, displaced) = queue.enqueue(10, 20, false, 1, "new");
        assert_eq!(displaced, Some("old"));
        assert_eq!(queue.on_frame_callback(old_id), ReattachTick::Stale);
        assert_eq!(
            queue.on_frame_callback(new_id),
            ReattachTick::Complete("new")
        );

        // Unlinking either `peer` or `stack` cancels without re-inserting.
        let (id2, _) = queue.enqueue(10, 21, true, BRIDGE_REATTACH_SETTLE_FRAMES, "peer_a");
        let (id3, _) = queue.enqueue(11, 22, true, BRIDGE_REATTACH_SETTLE_FRAMES, "peer_b");
        assert_eq!(queue.take_referencing(21), vec!["peer_a"]);
        assert_eq!(queue.on_frame_callback(id2), ReattachTick::Stale);
        // Unlinking an unrelated screen (`screen_owner` id = 99) leaves pending reattach intact.
        assert!(queue.take_referencing(99).is_empty());
        assert_eq!(queue.take_referencing(11), vec!["peer_b"]);
        assert_eq!(queue.on_frame_callback(id3), ReattachTick::Stale);

        // Re-bridging the same `peer`, failed callback fallback (`take_by_id`), or returning to
        // `DirectRoot` drains cleanly.
        let (id4, _) = queue.enqueue(10, 30, true, BRIDGE_REATTACH_SETTLE_FRAMES, "rebridged");
        assert_eq!(queue.take_peer(30), Some("rebridged"));
        assert_eq!(queue.on_frame_callback(id4), ReattachTick::Stale);

        let (id5, _) = queue.enqueue(10, 33, true, BRIDGE_REATTACH_SETTLE_FRAMES, "fallback");
        assert_eq!(queue.take_by_id(id5), Some("fallback"));
        assert_eq!(queue.take_by_id(id5), None);
        assert_eq!(queue.on_frame_callback(id5), ReattachTick::Stale);

        let _ = queue.enqueue(10, 31, true, BRIDGE_REATTACH_SETTLE_FRAMES, "d1");
        let _ = queue.enqueue(10, 32, false, BRIDGE_REATTACH_SETTLE_FRAMES, "d2");
        assert_eq!(queue.drain_all(), vec!["d1", "d2"]);
        assert!(queue.drain_all().is_empty());
    }

    #[test]
    fn translucent_lingering_composite_hides_presentation_to_prevent_double_scrim() {
        assert!(should_hide_lingering_presentation(false));
        assert!(!should_hide_lingering_presentation(true));
    }

    #[test]
    fn closed_modal_and_unrevealed_composites_retire_immediately() {
        assert!(!should_immediately_retire_composite(true, false, false));
        assert!(should_immediately_retire_composite(true, true, false));
        assert!(should_immediately_retire_composite(true, false, true));
        assert!(should_immediately_retire_composite(false, false, false));
    }
}
