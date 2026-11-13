//! Bounded retry, backoff, and error classification state machine for static and
//! animated shader fills (B06).

use std::time::{Duration, Instant};

pub(crate) const MAX_FILL_FAILURES: u8 = 3;

const FILL_BACKOFFS: [Duration; (MAX_FILL_FAILURES - 1) as usize] =
    [Duration::from_millis(32), Duration::from_millis(128)];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FillFailureAction {
    /// Keep `dirty = true` and `shown = true`, retrying once `now >= retry_at`.
    RetryLater,
    /// Stop retrying (`shown = false`, `dirty = false`); emit one error if `report == true`.
    GiveUp { report: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowFailureOutcome {
    /// Emit `Message::Error` to the host runtime once per failure streak when either
    /// no previous fallback window exists or consecutive window bind failures reach
    /// [`MAX_FILL_FAILURES`].
    pub(crate) report_error: bool,
}

#[derive(Default, Debug)]
pub(crate) struct FillRetry {
    attempts: u8,
    retry_at: Option<Instant>,
    reported: bool,
    invalid_layer: bool,
    window_failures: u8,
    window_reported: bool,
}

impl FillRetry {
    #[cfg(test)]
    pub(crate) fn attempts(&self) -> u8 {
        self.attempts
    }

    pub(crate) fn has_invalid_layer(&self) -> bool {
        self.invalid_layer
    }

    pub(crate) fn due(&self, now: Instant) -> bool {
        !self.invalid_layer && self.retry_at.is_none_or(|at| now >= at)
    }

    pub(crate) fn on_render_success(&mut self) {
        self.attempts = 0;
        self.retry_at = None;
        self.reported = false;
    }

    /// Resets render retry state when a fresh `Command::Layer` arrives.
    pub(crate) fn on_new_layer(&mut self) {
        self.attempts = 0;
        self.retry_at = None;
        self.reported = false;
        self.invalid_layer = false;
    }

    /// Resets transient render retry state on visibility/window updates without
    /// clearing a non-retryable shader compilation failure from `Command::Layer`.
    pub(crate) fn reset_transient(&mut self) {
        if !self.invalid_layer {
            self.attempts = 0;
            self.retry_at = None;
            self.reported = false;
        }
    }

    pub(crate) fn on_render_failure(&mut self, now: Instant, retryable: bool) -> FillFailureAction {
        if !retryable {
            self.attempts = MAX_FILL_FAILURES;
            self.retry_at = None;
            self.invalid_layer = true;
            let report = !self.reported;
            self.reported = true;
            return FillFailureAction::GiveUp { report };
        }
        let idx = usize::from(self.attempts);
        self.attempts = self.attempts.saturating_add(1);
        if let Some(&backoff) = FILL_BACKOFFS.get(idx) {
            self.retry_at = Some(now + backoff);
            FillFailureAction::RetryLater
        } else {
            self.retry_at = None;
            let report = !self.reported;
            self.reported = true;
            FillFailureAction::GiveUp { report }
        }
    }

    pub(crate) fn on_window_attached(&mut self) {
        self.window_failures = 0;
        self.window_reported = false;
        self.reset_transient();
    }

    pub(crate) fn on_window_failure(&mut self, has_previous_window: bool) -> WindowFailureOutcome {
        self.window_failures = self.window_failures.saturating_add(1);
        let should_report = (!has_previous_window || self.window_failures >= MAX_FILL_FAILURES)
            && !self.window_reported;
        if should_report {
            self.window_reported = true;
        }
        WindowFailureOutcome {
            report_error: should_report,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_fill_failure_retries_with_backoff_then_gives_up_once() {
        let t0 = Instant::now();
        let mut retry = FillRetry::default();

        // 1st transient failure -> 32ms backoff, stays dirty, no error report yet.
        assert_eq!(
            retry.on_render_failure(t0, true),
            FillFailureAction::RetryLater
        );
        assert_eq!(retry.attempts(), 1);
        assert!(!retry.due(t0 + Duration::from_millis(31)));
        assert!(retry.due(t0 + Duration::from_millis(32)));

        // 2nd transient failure -> 128ms backoff.
        let t1 = t0 + Duration::from_millis(32);
        assert_eq!(
            retry.on_render_failure(t1, true),
            FillFailureAction::RetryLater
        );
        assert_eq!(retry.attempts(), 2);
        assert!(!retry.due(t1 + Duration::from_millis(127)));
        assert!(retry.due(t1 + Duration::from_millis(128)));

        // 3rd consecutive failure -> GiveUp with single error report.
        let t2 = t1 + Duration::from_millis(128);
        assert_eq!(
            retry.on_render_failure(t2, true),
            FillFailureAction::GiveUp { report: true }
        );
        // Any stray duplicate failure before reset does not emit a second report.
        assert_eq!(
            retry.on_render_failure(t2, true),
            FillFailureAction::GiveUp { report: false }
        );
    }

    #[test]
    fn transient_fill_recovery_clears_attempts_and_backoff() {
        let t0 = Instant::now();
        let mut retry = FillRetry::default();
        assert_eq!(
            retry.on_render_failure(t0, true),
            FillFailureAction::RetryLater
        );
        let t1 = t0 + Duration::from_millis(32);
        assert!(retry.due(t1));
        retry.on_render_success();
        assert_eq!(retry.attempts(), 0);
        assert!(retry.due(t1));
        // Next transient streak gets a fresh 3-attempt budget and error report.
        assert_eq!(
            retry.on_render_failure(t1, true),
            FillFailureAction::RetryLater
        );
        assert_eq!(
            retry.on_render_failure(t1 + Duration::from_millis(32), true),
            FillFailureAction::RetryLater
        );
        assert_eq!(
            retry.on_render_failure(t1 + Duration::from_millis(160), true),
            FillFailureAction::GiveUp { report: true }
        );
    }

    #[test]
    fn shader_compile_failure_is_non_retryable_and_survives_shown_and_window_until_new_layer() {
        let t0 = Instant::now();
        let mut retry = FillRetry::default();
        assert_eq!(
            retry.on_render_failure(t0, false),
            FillFailureAction::GiveUp { report: true }
        );
        assert!(retry.has_invalid_layer());
        assert!(!retry.due(t0));

        // Subsequent Shown(true) / Window attach for the same broken layer must NOT
        // clear `invalid_layer` or allow a second error report.
        retry.reset_transient();
        retry.on_window_attached();
        assert!(retry.has_invalid_layer());
        assert!(!retry.due(t0));
        assert_eq!(
            retry.on_render_failure(t0, false),
            FillFailureAction::GiveUp { report: false }
        );

        // Replacing the shader via a new `Command::Layer` clears `invalid_layer`.
        retry.on_new_layer();
        assert!(!retry.has_invalid_layer());
        assert!(retry.due(t0));
        assert_eq!(retry.attempts(), 0);
    }

    #[test]
    fn window_failure_reports_immediately_without_fallback_or_after_repeated_failures_with_fallback()
     {
        let mut no_prev = FillRetry::default();
        assert!(no_prev.on_window_failure(false).report_error);
        assert!(!no_prev.on_window_failure(false).report_error);

        let mut with_prev = FillRetry::default();
        assert!(!with_prev.on_window_failure(true).report_error);
        assert!(!with_prev.on_window_failure(true).report_error);
        assert!(with_prev.on_window_failure(true).report_error);
        assert!(!with_prev.on_window_failure(true).report_error);
        with_prev.on_window_attached();
        assert!(!with_prev.on_window_failure(true).report_error);
    }
}
