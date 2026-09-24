//! Bounded render-thread stage timings for the D3D11 present path.
//!
//! One summary line every five seconds while frames flow, matching the
//! `decode stage-timings` style: presented and superseded frame rates, the
//! SharedContextLock acquire wait, the Y410 `record()` CPU cost, the
//! `ExecuteCommandList` CPU cost, and the GPU time of the Y410 conversion
//! from D3D11 timestamp queries read back several frames late with
//! `D3D11_ASYNC_GETDATA_DONOTWAIT` (never a spin, never a blocking GetData).
//!
//! The line keeps firing through a render stall (presentedPerS=0.0) so a
//! starved consumer is visible in the log instead of going silent. Everything
//! is bounded: 240 samples per metric, counters reset each report window,
//! payload-free log line.

use opennow_streamer_protocol::log;
use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const SAMPLES_PER_METRIC: usize = 240;
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Inner {
    lock_wait: VecDeque<u64>,
    record: VecDeque<u64>,
    execute: VecDeque<u64>,
    gpu_convert: VecDeque<u64>,
    presented: u64,
    superseded: u64,
    window_started: Option<Instant>,
    report_at: Option<Instant>,
}

static STATE: LazyLock<Mutex<Inner>> = LazyLock::new(|| Mutex::new(Inner::default()));

fn lock() -> std::sync::MutexGuard<'static, Inner> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn push_bounded(values: &mut VecDeque<u64>, value: u64) {
    if values.len() == SAMPLES_PER_METRIC {
        values.pop_front();
    }
    values.push_back(value);
}

fn percentile(sorted: &[u64], fraction: usize, denominator: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = sorted
        .len()
        .saturating_mul(fraction)
        .div_ceil(denominator)
        .saturating_sub(1);
    sorted[index.min(sorted.len() - 1)]
}

fn note_activity() {
    let mut state = lock();
    if state.window_started.is_none() {
        state.window_started = Some(Instant::now());
    }
}

/// Render thread consumed one decoded frame for presentation.
pub(crate) fn note_presented() {
    let mut state = lock();
    state.presented += 1;
    if state.window_started.is_none() {
        state.window_started = Some(Instant::now());
    }
}

/// The renderer superseded (skipped) this many decoded frames; their leases
/// are released immediately.
pub(crate) fn note_superseded(count: usize) {
    if count == 0 {
        return;
    }
    let mut state = lock();
    state.superseded += count as u64;
    if state.window_started.is_none() {
        state.window_started = Some(Instant::now());
    }
}

/// Time spent waiting for the shared immediate-context lock before record.
pub(crate) fn record_lock_wait_us(micros: u64) {
    note_activity();
    push_bounded(&mut lock().lock_wait, micros);
}

/// CPU time of the whole Y410 record (lock held).
pub(crate) fn record_record_us(micros: u64) {
    note_activity();
    push_bounded(&mut lock().record, micros);
}

/// CPU time of the immediate-context `ExecuteCommandList` call itself.
pub(crate) fn record_execute_us(micros: u64) {
    note_activity();
    push_bounded(&mut lock().execute, micros);
}

/// GPU time of the Y410 conversion commands, from timestamp queries.
pub(crate) fn record_gpu_convert_us(micros: u64) {
    note_activity();
    push_bounded(&mut lock().gpu_convert, micros);
}

/// Elapsed microseconds since `started`, saturating instead of wrapping.
pub(crate) fn micros_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn summary(samples: &VecDeque<u64>, name: &str, report: &mut Vec<String>) {
    if samples.is_empty() {
        report.push(format!("{name} p50Us=- p95Us=- maxUs=- n=0"));
        return;
    }
    let mut sorted: Vec<u64> = samples.iter().copied().collect();
    sorted.sort_unstable();
    report.push(format!(
        "{name} p50Us={} p95Us={} maxUs={} n={}",
        percentile(&sorted, 50, 100),
        percentile(&sorted, 95, 100),
        sorted.last().copied().unwrap_or(0),
        sorted.len()
    ));
}

/// Emits one bounded report line every five seconds once the render path has
/// presented at least one frame. A fully stalled consumer reports zero rates
/// instead of going silent.
pub(crate) fn maybe_report(now: Instant) {
    let mut state = lock();
    if state.window_started.is_none() {
        return;
    }
    match state.report_at {
        Some(due) if now < due => return,
        _ => {}
    }
    state.report_at = Some(now + REPORT_INTERVAL);
    let window = state
        .window_started
        .replace(now)
        .map(|started| now.saturating_duration_since(started))
        .unwrap_or_default();
    let presented = std::mem::take(&mut state.presented);
    let superseded = std::mem::take(&mut state.superseded);
    let metrics = (
        std::mem::take(&mut state.lock_wait),
        std::mem::take(&mut state.record),
        std::mem::take(&mut state.execute),
        std::mem::take(&mut state.gpu_convert),
    );
    drop(state);

    let seconds = window.as_secs_f64().max(1.0e-9);
    let mut report = Vec::with_capacity(7);
    report.push(format!(
        "presentedPerS={:.1} supersededPerS={:.1} presented={} superseded={}",
        presented as f64 / seconds,
        superseded as f64 / seconds,
        presented,
        superseded
    ));
    summary(&metrics.0, "lockWait", &mut report);
    summary(&metrics.1, "record", &mut report);
    summary(&metrics.2, "execute", &mut report);
    summary(&metrics.3, "gpuConvert", &mut report);
    log::log_async(
        "INFO",
        "render",
        &format!("stage-timings {}", report.join(" ")),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_bounded_and_percentiles_stay_inside_the_set() {
        for sample in 0..(SAMPLES_PER_METRIC as u64 + 50) {
            record_lock_wait_us(sample);
            record_record_us(sample);
            record_execute_us(sample);
            record_gpu_convert_us(sample);
        }
        let state = lock();
        assert_eq!(state.lock_wait.len(), SAMPLES_PER_METRIC);
        assert_eq!(state.record.len(), SAMPLES_PER_METRIC);
        assert_eq!(state.execute.len(), SAMPLES_PER_METRIC);
        assert_eq!(state.gpu_convert.len(), SAMPLES_PER_METRIC);
        drop(state);

        let sorted: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&sorted, 50, 100), 50);
        assert_eq!(percentile(&sorted, 95, 100), 95);
        assert_eq!(percentile(&[], 50, 100), 0);
    }

    #[test]
    fn reports_start_after_first_present_and_reset_each_window() {
        {
            let mut state = lock();
            state.window_started = None;
            state.report_at = None;
            state.presented = 0;
            state.superseded = 0;
        }
        // Before any render activity there is nothing to report.
        maybe_report(Instant::now());
        assert!(lock().report_at.is_none());

        note_presented();
        note_superseded(3);
        note_superseded(0);
        {
            let mut state = lock();
            assert_eq!(state.presented, 1);
            assert_eq!(state.superseded, 3);
            state.report_at = Some(Instant::now());
            state.window_started = Some(
                Instant::now()
                    .checked_sub(Duration::from_secs(5))
                    .unwrap_or_else(Instant::now),
            );
        }
        maybe_report(Instant::now() + Duration::from_secs(6));
        let mut state = lock();
        assert_eq!(state.presented, 0, "counters reset after the report");
        assert_eq!(state.superseded, 0);
        assert!(state.report_at.is_some());
        assert!(state.window_started.is_some(), "window re-anchors");

        // A stalled consumer (zero activity) still re-arms the next report
        // so starvation shows up as presentedPerS=0.0 instead of silence.
        state.presented = 0;
        state.superseded = 0;
        state.report_at = Some(Instant::now());
        drop(state);
        maybe_report(Instant::now() + Duration::from_secs(6));
        let state = lock();
        assert!(state.report_at.is_some(), "stall windows keep reporting");
    }
}
