//! Pure push/pop interpolation and edge-swipe completion decisions.

use std::cell::Cell;

/// One 120 Hz VSync interval (`~8.33 ms`) of initial lead when an unanchored
/// `Motion` samples its first VSync. Keeps cold-mount delays out of the curve
/// while ensuring VSync 1 always produces a non-zero transform/alpha delta so
/// ArkUI's `FlushRenderTask` commits frame 1 and schedules VSync 2.
const INITIAL_VSYNC_LEAD_MS: f64 = 1000.0 / 120.0;

pub(crate) struct Motion {
    start: Cell<Option<f64>>,
    duration: f64,
    from: f32,
    pub(crate) to: f32,
}
impl Motion {
    pub(crate) fn new(
        start: Option<f64>,
        from: f32,
        to: f32,
        duration: Option<std::time::Duration>,
    ) -> Self {
        Self {
            start: Cell::new(start),
            from,
            to,
            duration: duration.map_or(0.0, |duration| {
                duration.as_secs_f64() * 1000.0 * (from - to).abs() as f64
            }),
        }
    }
    pub(crate) fn step(&self, now: f64) -> (f32, bool, f32) {
        let start = match self.start.get() {
            Some(start) => start,
            None => {
                let lead = if self.duration > 0.0 {
                    INITIAL_VSYNC_LEAD_MS.min(self.duration * 0.25)
                } else {
                    0.0
                };
                let anchored = now - lead;
                self.start.set(Some(anchored));
                anchored
            }
        };
        let t = if self.duration <= 0.0 {
            1.0
        } else {
            ((now - start) / self.duration).clamp(0.0, 1.0) as f32
        };
        (
            self.from + (self.to - self.from) * nsh_core::navigation::ease(t as f64),
            t >= 1.0,
            self.to,
        )
    }
}
pub(crate) fn complete_swipe(distance: f32, width: f32, velocity: f32) -> bool {
    distance.is_finite()
        && width.is_finite()
        && velocity.is_finite()
        && width > 0.0
        && distance > 8.0
        && velocity > -600.0
        && (distance >= width * 0.35 || velocity > 600.0)
}

pub(crate) fn title_bounds(width: f32, back: bool, left: f32, right: f32, rtl: bool) -> (f32, f32) {
    let leading =
        if back { 56.0 } else { 12.0 } + left.max(0.0) + if left > 0.0 { 8.0 } else { 0.0 };
    let trailing = 12.0 + right.max(0.0) + if right > 0.0 { 8.0 } else { 0.0 };
    let (start, end) = if rtl {
        (trailing, leading)
    } else {
        (leading, trailing)
    };
    let x = start.min(width.max(0.0));
    (x, (width - x - end).max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wide_items_bound_the_title_and_mirror_in_rtl() {
        assert_eq!(
            title_bounds(400.0, true, 100.0, 80.0, false),
            (164.0, 136.0)
        );
        assert_eq!(title_bounds(400.0, true, 100.0, 80.0, true), (100.0, 136.0));
        assert_eq!(title_bounds(100.0, true, 200.0, 80.0, false), (100.0, 0.0));
        assert_eq!(title_bounds(400.0, false, 0.0, 0.0, false), (12.0, 376.0));
    }
    #[test]
    fn cancelling_a_partial_swipe_preserves_position_and_finishes_at_rest() {
        let cancel = Motion::new(Some(100.0), 0.4, 0.0, Some(nsh_core::navigation::SLIDE));
        assert_eq!(cancel.step(100.0), (0.4, false, 0.0));
        let middle = cancel.step(160.0);
        assert!(middle.0 > 0.0 && middle.0 < 0.4 && !middle.1);
        assert_eq!(cancel.step(220.0), (0.0, true, 0.0));
    }
    #[test]
    fn fade_and_reduced_motion_use_core_durations() {
        for reduced in [false, true] {
            let motion =
                nsh_core::navigation::screen_motion(nsh_core::ScreenAnimation::Fade, reduced)
                    .map(|m| m.0);
            let fade = Motion::new(Some(0.0), 0.0, 1.0, motion);
            assert!(!fade.step(100.0).1);
            assert_eq!(fade.step(200.0), (1.0, true, 1.0));
        }
        assert_eq!(
            Motion::new(Some(0.0), 1.0, 0.0, None).step(0.0),
            (0.0, true, 0.0)
        );
    }
    #[test]
    fn unanchored_motion_starts_on_first_vsync_sample() {
        // Push (1.0 -> 0.0): advances off 1.0 on VSync 1 even after a 540ms cold stall,
        // while preserving >97% of the 300ms curve.
        let slide = Motion::new(None, 1.0, 0.0, Some(nsh_core::navigation::SLIDE));
        let first = slide.step(540.0);
        assert!(first.0 < 1.0 && first.0 > 0.9 && !first.1);
        let mid = slide.step(680.0);
        assert!(mid.0 > 0.0 && mid.0 < first.0 && !mid.1);
        assert_eq!(slide.step(840.0), (0.0, true, 0.0));

        // Pop (0.0 -> 1.0, e.g. hardware back): steps > 0.0 on VSync 1 so Transform
        // immediately mutates and keeps ArkUI's VSync loop running.
        let pop = Motion::new(None, 0.0, 1.0, Some(nsh_core::navigation::SLIDE));
        let pop_first = pop.step(1000.0);
        assert!(pop_first.0 > 0.0 && pop_first.0 < 0.1 && !pop_first.1);
        assert_eq!(pop.step(1300.0), (1.0, true, 1.0));
    }
    #[test]
    fn completion_accounts_for_fling_direction_and_invalid_geometry() {
        assert!(complete_swipe(150.0, 400.0, 0.0));
        assert!(complete_swipe(20.0, 400.0, 700.0));
        assert!(!complete_swipe(200.0, 400.0, -700.0));
        assert!(!complete_swipe(4.0, 400.0, 2000.0));
        assert!(!complete_swipe(200.0, 0.0, 0.0));
        assert!(!complete_swipe(f32::NAN, 400.0, 0.0));
    }
}
