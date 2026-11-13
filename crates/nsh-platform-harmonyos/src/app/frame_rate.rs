//! Single owner of the window-associated `NativeVSync` expected frame-rate
//! range. Both UI frame scheduling and shader animations submit demands here;
//! `FrameRate` arbitrates the combined `(min, max, expected)` range and updates
//! its applied cache only when the underlying setter succeeds.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

#[cfg(target_env = "ohos")]
use ohos_vsync_binding::{OH_NativeVSync_ExpectedRateRange, Vsync};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RateRange {
    pub(crate) min: i32,
    pub(crate) max: i32,
    pub(crate) expected: i32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum UiDemand {
    #[default]
    Idle,
    Uncapped,
    Capped(i32),
}

pub(crate) trait RateSink: Send + Sync {
    fn set_range(&self, range: RateRange) -> i32;
    fn read_display_hz(&self) -> u32;
}

#[cfg(target_env = "ohos")]
struct OhosSink {
    vsync: Arc<Vsync>,
}

#[cfg(target_env = "ohos")]
impl RateSink for OhosSink {
    fn set_range(&self, range: RateRange) -> i32 {
        self.vsync
            .set_expected_frame_rate_range(OH_NativeVSync_ExpectedRateRange {
                min: range.min,
                max: range.max,
                expected: range.expected,
            })
    }

    fn read_display_hz(&self) -> u32 {
        display_hz()
    }
}

struct State {
    shader_votes: usize,
    ui_demand: UiDemand,
    hz: u32,
    refreshed: Instant,
    applied: Option<RateRange>,
}

impl State {
    /// Arbitrates the shared window-associated `NativeVSync` rate range (A08).
    ///
    /// Intentional policy: while any GPU shader animation holds a vote
    /// (`shader_votes > 0`), the shared `NativeVSync` handle must run at full
    /// panel `hz` so active shaders are never down-throttled by a concurrent
    /// `UiDemand::Capped` UI animation; as soon as the last shader vote drops,
    /// any active UI cap immediately resumes. If strict per-animation UI rate
    /// limiting is ever required alongside an active shader, throttle that
    /// consumer per-deadline inside `Animator` rather than lowering the shared
    /// VSync rate range here.
    fn desired(&self) -> RateRange {
        let hz = self.hz as i32;
        if self.shader_votes > 0 {
            RateRange {
                min: hz,
                max: hz,
                expected: hz,
            }
        } else {
            match self.ui_demand {
                UiDemand::Capped(cap) => {
                    let capped = cap.clamp(1, hz);
                    RateRange {
                        min: 0,
                        max: capped,
                        expected: capped,
                    }
                }
                UiDemand::Idle | UiDemand::Uncapped => RateRange {
                    min: 0,
                    max: hz,
                    expected: hz.min(60),
                },
            }
        }
    }
}

pub(crate) struct FrameRate {
    #[cfg(target_env = "ohos")]
    vsync: Arc<Vsync>,
    sink: Box<dyn RateSink>,
    state: Mutex<State>,
}

impl FrameRate {
    #[cfg(target_env = "ohos")]
    pub(crate) fn new(vsync: Arc<Vsync>) -> Arc<Self> {
        let sink = Box::new(OhosSink {
            vsync: vsync.clone(),
        });
        let hz = sink.read_display_hz();
        Arc::new(Self {
            vsync,
            sink,
            state: Mutex::new(State {
                shader_votes: 0,
                ui_demand: UiDemand::Idle,
                hz,
                refreshed: Instant::now(),
                applied: None,
            }),
        })
    }

    #[cfg(test)]
    fn with_sink(sink: Box<dyn RateSink>) -> Arc<Self> {
        let hz = normalize_display_hz(sink.read_display_hz());
        Arc::new(Self {
            #[cfg(target_env = "ohos")]
            vsync: unreachable!(),
            sink,
            state: Mutex::new(State {
                shader_votes: 0,
                ui_demand: UiDemand::Idle,
                hz,
                refreshed: Instant::now(),
                applied: None,
            }),
        })
    }

    #[cfg(target_env = "ohos")]
    pub(crate) fn vsync(&self) -> &Vsync {
        &self.vsync
    }

    pub(crate) fn hz(&self) -> u32 {
        self.state.lock().unwrap().hz
    }

    /// Applies the initial baseline range and returns the raw status code so
    /// surface startup can fail fast if the native VSync handle rejects it.
    pub(crate) fn apply_initial(&self) -> i32 {
        let mut state = self.state.lock().unwrap();
        self.sync_locked(&mut state, false)
    }

    pub(crate) fn set_ui_demand(&self, demand: UiDemand) {
        let mut state = self.state.lock().unwrap();
        self.maybe_refresh_hz(&mut state);
        state.ui_demand = demand;
        let _ = self.sync_locked(&mut state, true);
    }

    pub(crate) fn vote(self: &Arc<Self>) -> Vote {
        Vote {
            rate: self.clone(),
            active: false,
        }
    }

    fn update_vote(&self, delta: isize) {
        let mut state = self.state.lock().unwrap();
        self.maybe_refresh_hz(&mut state);
        if delta > 0 {
            state.shader_votes = state.shader_votes.saturating_add(delta as usize);
        } else if delta < 0 {
            state.shader_votes = state.shader_votes.saturating_sub((-delta) as usize);
        }
        let _ = self.sync_locked(&mut state, true);
    }

    fn maybe_refresh_hz(&self, state: &mut State) {
        if state.refreshed.elapsed().as_secs() >= 1 {
            state.refreshed = Instant::now();
            state.hz = normalize_display_hz(self.sink.read_display_hz());
        }
    }

    fn sync_locked(&self, state: &mut State, log_failure: bool) -> i32 {
        let desired = state.desired();
        if state.applied == Some(desired) {
            return 0;
        }
        let status = self.sink.set_range(desired);
        if status == 0 {
            state.applied = Some(desired);
        } else if log_failure {
            log::warn!("OHOS VSync rate vote failed: {status}");
        }
        status
    }
}

pub(crate) struct Vote {
    rate: Arc<FrameRate>,
    active: bool,
}

impl Vote {
    pub(crate) fn update(&mut self, active: bool) {
        if self.active == active {
            // Still allow periodic display-rate refresh and failed-setter retry.
            self.rate.update_vote(0);
            return;
        }
        self.active = active;
        self.rate.update_vote(if active { 1 } else { -1 });
    }
}

impl Drop for Vote {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            self.rate.update_vote(-1);
        }
    }
}

/// Normalizes the raw OHOS display refresh rate, defaulting to a conservative
/// 60 Hz when the display query reports `0` (unavailable).
pub(crate) fn normalize_display_hz(raw: u32) -> u32 {
    match raw {
        0 => 60,
        hz => hz.clamp(30, 240),
    }
}

/// Parses an optional `NSH_VSYNC_KEEPALIVE` runtime override (`0`/`false`/`off`
/// disables, `1`/`true`/`on` enables, unset or unrecognized falls back to `default`).
pub(crate) fn parse_keepalive_switch(raw: Option<&str>, default: bool) -> bool {
    match raw.map(str::trim) {
        Some(v)
            if v.eq_ignore_ascii_case("0")
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("off") =>
        {
            false
        }
        Some(v)
            if v.eq_ignore_ascii_case("1")
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("on") =>
        {
            true
        }
        _ => default,
    }
}

#[cfg(target_env = "ohos")]
pub(crate) fn display_hz() -> u32 {
    normalize_display_hz(ohos_display_binding::default_display_refresh_rate())
}

#[cfg(all(test, not(target_env = "ohos")))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

    struct MockSink {
        hz: AtomicU32,
        next_status: AtomicI32,
        calls: Mutex<Vec<RateRange>>,
    }

    impl MockSink {
        fn new(hz: u32) -> Arc<Self> {
            Arc::new(Self {
                hz: AtomicU32::new(hz),
                next_status: AtomicI32::new(0),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn last(&self) -> Option<RateRange> {
            self.calls.lock().unwrap().last().copied()
        }

        fn len(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    struct SharedSink(Arc<MockSink>);
    impl RateSink for SharedSink {
        fn set_range(&self, range: RateRange) -> i32 {
            let status = self.0.next_status.swap(0, Ordering::SeqCst);
            self.0.calls.lock().unwrap().push(range);
            status
        }

        fn read_display_hz(&self) -> u32 {
            self.0.hz.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn unavailable_display_rate_defaults_to_60_hz() {
        assert_eq!(normalize_display_hz(0), 60);
        assert_eq!(normalize_display_hz(10), 30);
        assert_eq!(normalize_display_hz(120), 120);
        assert_eq!(normalize_display_hz(360), 240);
    }

    #[test]
    fn shader_and_capped_ui_arbitrate_without_clobbering_each_other() {
        let sink = MockSink::new(120);
        let rate = FrameRate::with_sink(Box::new(SharedSink(sink.clone())));
        assert_eq!(rate.apply_initial(), 0);
        assert_eq!(
            sink.last(),
            Some(RateRange {
                min: 0,
                max: 120,
                expected: 60,
            })
        );

        let mut shader = rate.vote();
        shader.update(true);
        assert_eq!(
            sink.last(),
            Some(RateRange {
                min: 120,
                max: 120,
                expected: 120,
            })
        );

        // A 30 Hz local UI animation must not downgrade the active 120 Hz shader.
        rate.set_ui_demand(UiDemand::Capped(30));
        assert_eq!(
            sink.last(),
            Some(RateRange {
                min: 120,
                max: 120,
                expected: 120,
            })
        );

        // When the shader stops, the still-running 30 Hz UI cap takes effect.
        shader.update(false);
        assert_eq!(
            sink.last(),
            Some(RateRange {
                min: 0,
                max: 30,
                expected: 30,
            })
        );

        // When the last UI animation stops, the normal baseline range is restored.
        rate.set_ui_demand(UiDemand::Idle);
        assert_eq!(
            sink.last(),
            Some(RateRange {
                min: 0,
                max: 120,
                expected: 60,
            })
        );
    }

    #[test]
    fn failed_setter_does_not_poison_applied_cache_and_retries() {
        let sink = MockSink::new(120);
        let rate = FrameRate::with_sink(Box::new(SharedSink(sink.clone())));
        assert_eq!(rate.apply_initial(), 0);

        sink.next_status.store(-1, Ordering::SeqCst);
        rate.set_ui_demand(UiDemand::Capped(30));
        let calls_after_failure = sink.len();

        // Retrying the same demand must invoke the setter again because the prior call failed.
        rate.set_ui_demand(UiDemand::Capped(30));
        assert_eq!(sink.len(), calls_after_failure + 1);
        assert_eq!(
            sink.last(),
            Some(RateRange {
                min: 0,
                max: 30,
                expected: 30,
            })
        );
    }

    #[test]
    fn keepalive_switch_preserves_default_and_honors_env_values() {
        assert!(parse_keepalive_switch(None, true));
        assert!(!parse_keepalive_switch(None, false));
        assert!(!parse_keepalive_switch(Some("0"), true));
        assert!(!parse_keepalive_switch(Some("false"), true));
        assert!(!parse_keepalive_switch(Some("OFF"), true));
        assert!(parse_keepalive_switch(Some("1"), false));
        assert!(parse_keepalive_switch(Some("true"), false));
        assert!(parse_keepalive_switch(Some("On"), false));
        assert!(parse_keepalive_switch(Some(""), true));
    }
}
