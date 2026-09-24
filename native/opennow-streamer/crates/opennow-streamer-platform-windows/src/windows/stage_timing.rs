//! Bounded per-frame stage timings for the active Media Foundation route.
//!
//! Four sample points per frame: receive (accepted into the decoder queue,
//! i.e. after the frame's last RTP packet), submit (MFT `ProcessInput`),
//! output (MFT `ProcessOutput`) and present (Qt records the frame on the render
//! thread, immediately before `Present`). Keys are the sender's 100 ns sample
//! times, which Media Foundation carries through its reorder buffer, so pairing
//! stays exact without assuming decode order.
//!
//! Everything here is bounded: 512 tracked frames, 240 samples per stage, one
//! report line every five seconds, payload-free.

use opennow_streamer_protocol::log;
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const SAMPLES_PER_STAGE: usize = 240;
const MAX_TRACKED_FRAMES: usize = 512;
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Inner {
    arrivals: HashMap<u64, Instant>,
    stages: [VecDeque<u64>; 3],
    report_at: Option<Instant>,
}

static STATE: LazyLock<Mutex<Inner>> = LazyLock::new(|| Mutex::new(Inner::default()));

fn lock() -> std::sync::MutexGuard<'static, Inner> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn push_bounded(values: &mut VecDeque<u64>, value: u64) {
    if values.len() == SAMPLES_PER_STAGE {
        values.pop_front();
    }
    values.push_back(value);
}

fn age_us(arrivals: &HashMap<u64, Instant>, sample_time: u64, now: Instant) -> Option<u64> {
    let received = *arrivals.get(&sample_time)?;
    let micros = now
        .duration_since(received)
        .as_micros()
        .min(u64::MAX as u128);
    Some(micros as u64)
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

/// The frame was accepted into the decoder queue (its last RTP packet arrived).
pub(crate) fn record_receive(sample_time: i64) {
    let sample_time = u64::try_from(sample_time).unwrap_or(u64::MAX);
    let mut state = lock();
    if state.arrivals.len() >= MAX_TRACKED_FRAMES {
        state.arrivals.clear();
    }
    state.arrivals.insert(sample_time, Instant::now());
}

fn record_stage(stage: usize, sample_time: i64) {
    let sample_time = u64::try_from(sample_time).unwrap_or(u64::MAX);
    let now = Instant::now();
    let mut state = lock();
    let Some(age) = age_us(&state.arrivals, sample_time, now) else {
        return;
    };
    push_bounded(&mut state.stages[stage], age);
}

/// The MFT accepted the access unit (`ProcessInput`).
pub(crate) fn record_submit(sample_time: i64) {
    record_stage(0, sample_time);
}

/// The MFT produced the decoded frame (`ProcessOutput`).
pub(crate) fn record_output(sample_time: i64) {
    record_stage(1, sample_time);
}

/// Qt took the frame for presentation; the arrival entry is consumed here.
pub(crate) fn record_present(sample_time: i64) {
    let sample_time = u64::try_from(sample_time).unwrap_or(u64::MAX);
    let now = Instant::now();
    let mut state = lock();
    let Some(age) = age_us(&state.arrivals, sample_time, now) else {
        return;
    };
    push_bounded(&mut state.stages[2], age);
    state.arrivals.remove(&sample_time);
}

fn summary(samples: &VecDeque<u64>, name: &str, report: &mut Vec<String>) {
    if samples.is_empty() {
        report.push(format!("{name} p50Us=- p95Us=- n=0"));
        return;
    }
    let mut sorted: Vec<u64> = samples.iter().copied().collect();
    sorted.sort_unstable();
    report.push(format!(
        "{name} p50Us={} p95Us={} n={}",
        percentile(&sorted, 50, 100),
        percentile(&sorted, 95, 100),
        sorted.len()
    ));
}

/// Emits one bounded report line every five seconds while frames flow.
pub(crate) fn maybe_report(now: Instant) {
    let mut state = lock();
    let due = match state.report_at {
        Some(due) if now >= due => due,
        Some(_) => return,
        None => {
            state.report_at = Some(now + REPORT_INTERVAL);
            return;
        }
    };
    let elapsed = now.saturating_duration_since(due);
    state.report_at = Some(now + REPORT_INTERVAL + elapsed - elapsed);

    let stages = std::mem::take(&mut state.stages);
    drop(state);
    if stages.iter().all(|samples| samples.is_empty()) {
        return;
    }
    let mut report = Vec::with_capacity(3);
    summary(&stages[0], "receiveToSubmit", &mut report);
    summary(&stages[1], "receiveToOutput", &mut report);
    summary(&stages[2], "receiveToPresent", &mut report);
    // Sample point for receiveToPresent is D3d11Frame::record on Qt's render
    // thread; Present follows in the same pass, so this is the last observable
    // in-process stage before the pixel reaches the screen.
    log::log_async(
        "INFO",
        "decode",
        &format!("stage-timings {}", report.join(" ")),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_bounded_and_reported_once_per_interval() {
        for sample in 0..(SAMPLES_PER_STAGE as i64 + 50) {
            record_receive(sample);
            record_submit(sample);
            record_output(sample);
            record_present(sample);
        }
        let state = lock();
        assert_eq!(state.stages[0].len(), SAMPLES_PER_STAGE);
        assert_eq!(state.stages[1].len(), SAMPLES_PER_STAGE);
        assert_eq!(state.stages[2].len(), SAMPLES_PER_STAGE);
        assert!(state.arrivals.len() <= MAX_TRACKED_FRAMES);
        drop(state);

        let now = Instant::now();
        maybe_report(now);
        assert!(lock().report_at.is_some());
    }

    #[test]
    fn percentiles_stay_inside_the_sample_set() {
        let sorted: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&sorted, 50, 100), 50);
        assert_eq!(percentile(&sorted, 95, 100), 95);
        assert_eq!(percentile(&[], 50, 100), 0);
        let single = vec![7];
        assert_eq!(percentile(&single, 95, 100), 7);
    }

    #[test]
    fn unknown_sample_times_have_no_age() {
        let empty: HashMap<u64, Instant> = HashMap::new();
        assert!(age_us(&empty, 4242, Instant::now()).is_none());
        let mut arrivals = HashMap::new();
        arrivals.insert(7, Instant::now());
        assert!(age_us(&arrivals, 4242, Instant::now()).is_none());
        assert!(age_us(&arrivals, 7, Instant::now()).is_some());
    }
}
