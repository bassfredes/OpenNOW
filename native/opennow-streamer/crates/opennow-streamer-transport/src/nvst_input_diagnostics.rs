//! Bounded input-path diagnostics. Never record key values or control payloads.
//!
//! The timing stages are always recorded: payload-free microsecond windows
//! over the client-side input chain, reported once per five seconds like the
//! video `stage-timings` line. The control-header records below are also
//! payload-free; `enabled()` only mirrors them to stderr.
use std::collections::VecDeque;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

static ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var("OPENNOW_INPUT_DIAGNOSTICS").as_deref() == Ok("1"));

pub(crate) fn enabled() -> bool {
    *ENABLED
}

const STAGE_SAMPLE_CAPACITY: usize = 512;
const PENDING_FLUSH_CAPACITY: usize = 256;
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct StageWindow {
    samples: VecDeque<u64>,
    count: u64,
    max_us: u64,
}

impl StageWindow {
    fn record(&mut self, micros: u64) {
        if self.samples.len() == STAGE_SAMPLE_CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(micros);
        self.count += 1;
        self.max_us = self.max_us.max(micros);
    }

    fn summarize(&self, name: &str) -> String {
        if self.samples.is_empty() {
            return format!("{name}{{n=0}}");
        }
        let mut sorted: Vec<u64> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        format!(
            "{name}{{p50Us={} p95Us={} maxUs={} n={}}}",
            sorted[(sorted.len() - 1) / 2],
            sorted[(sorted.len() * 95).div_ceil(100) - 1],
            self.max_us,
            self.count
        )
    }

    fn reset(&mut self) {
        self.samples.clear();
        self.count = 0;
        self.max_us = 0;
    }
}

#[derive(Default)]
pub(crate) struct InputDiagnostics {
    signatures: Vec<(&'static str, u16, usize, bool)>,
    /// FFI submit (queue push) -> transport enqueue.
    submit_to_enqueue: StageWindow,
    /// Transport enqueue -> receive-worker dequeue.
    enqueue_to_worker: StageWindow,
    /// Receive-worker dequeue -> str0m packetize + UDP send flush.
    dequeue_to_sctp: StageWindow,
    /// Dequeue stamps awaiting the end-of-iteration flush stamp. Bounded:
    /// the oldest pending stamp is dropped when a single drain overshoots.
    pending_flush: VecDeque<Instant>,
    report_at: Option<Instant>,
}

/// One payload-free control-header record: command, sizes and completeness
/// only. The same text goes to stderr (flagged runs) and the durable sink.
fn control_header_line(
    channel: &str,
    command: u16,
    length: usize,
    message_bytes: usize,
    offset: usize,
    complete: bool,
) -> String {
    format!(
        "NVST control header: channel={channel} command=0x{command:04x} payloadBytes={length} messageBytes={message_bytes} offset={offset} complete={complete}"
    )
}

impl InputDiagnostics {
    pub(crate) fn control(&mut self, channel: &'static str, data: &[u8]) {
        // At most 48 distinct header records per stream. Payloads are skipped.
        let stderr = enabled();
        let mut offset = 0;
        while data.len().saturating_sub(offset) >= 4 && self.signatures.len() < 48 {
            let command = u16::from_le_bytes([data[offset], data[offset + 1]]);
            let length = usize::from(u16::from_le_bytes([data[offset + 2], data[offset + 3]]));
            let end = offset + 4 + length;
            let complete = end <= data.len();
            let signature = (channel, command, length, complete);
            if !self.signatures.contains(&signature) {
                self.signatures.push(signature);
                let line =
                    control_header_line(channel, command, length, data.len(), offset, complete);
                // Always keep the payload-free record in the durable file sink:
                // a session launched without OPENNOW_INPUT_DIAGNOSTICS must still
                // show which control commands the seat sent (a resolution or
                // display-mode notification is otherwise invisible after exit).
                opennow_streamer_protocol::log::log_line("INFO", "diagnostics", &line);
                if stderr {
                    eprintln!("{line}");
                }
            }
            if !complete {
                break;
            }
            offset = end;
        }
    }

    /// Records a dequeued input: submit -> enqueue (when the origin is
    /// carried), enqueue -> dequeue, and queues the dequeue stamp for the
    /// end-of-iteration flush pairing.
    pub(crate) fn input(
        &mut self,
        origin: Option<Instant>,
        queued_at: Instant,
        dequeued_at: Instant,
    ) {
        if let Some(origin) = origin {
            let micros = queued_at
                .saturating_duration_since(origin)
                .as_micros()
                .min(u64::MAX as u128) as u64;
            self.submit_to_enqueue.record(micros);
        }
        let micros = dequeued_at
            .saturating_duration_since(queued_at)
            .as_micros()
            .min(u64::MAX as u128) as u64;
        self.enqueue_to_worker.record(micros);
        if self.pending_flush.len() == PENDING_FLUSH_CAPACITY {
            self.pending_flush.pop_front();
        }
        self.pending_flush.push_back(dequeued_at);
    }

    /// Closes every input dequeued since the previous flush: the worker has
    /// now packetized them through str0m and handed the datagrams to the UDP
    /// socket, so `now - dequeue` is the worker -> SCTP-send stage. Also
    /// drives the five-second report.
    pub(crate) fn flush(&mut self, now: Instant) {
        if self.pending_flush.is_empty() {
            return;
        }
        while let Some(dequeued_at) = self.pending_flush.pop_front() {
            let micros = now
                .saturating_duration_since(dequeued_at)
                .as_micros()
                .min(u64::MAX as u128) as u64;
            self.dequeue_to_sctp.record(micros);
        }
        let report_at = self.report_at.get_or_insert(now + REPORT_INTERVAL);
        if now < *report_at {
            return;
        }
        let line = format!(
            "NVST input stage-timings {} {} {}",
            self.submit_to_enqueue.summarize("submitToEnqueue"),
            self.enqueue_to_worker.summarize("enqueueToWorker"),
            self.dequeue_to_sctp.summarize("dequeueToSctp"),
        );
        opennow_streamer_protocol::log::log_line("INFO", "input-stage-timings", &line);
        eprintln!("{line}");
        self.submit_to_enqueue.reset();
        self.enqueue_to_worker.reset();
        self.dequeue_to_sctp.reset();
        self.report_at = Some(now + REPORT_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_headers_skip_payload_and_bound_unique_records() {
        let mut diagnostics = InputDiagnostics::default();
        diagnostics.control(
            "control",
            &[0x10, 1, 4, 0, 0xff, 0xff, 0xff, 0xff, 0x0f, 1, 0, 0],
        );
        assert_eq!(
            diagnostics.signatures,
            [("control", 0x110, 4, true), ("control", 0x10f, 0, true)]
        );
        diagnostics.control("control", &[0x10, 1, 4, 0, 0, 0, 0, 0]);
        assert_eq!(diagnostics.signatures.len(), 2);
        diagnostics.control("control", &[0x10, 1, 0xff, 0xff, 0]);
        assert_eq!(
            diagnostics.signatures.last(),
            Some(&("control", 0x110, 65535, false))
        );
        for id in 0..100 {
            diagnostics.control("control", &[id, 0, 0, 0]);
        }
        assert_eq!(diagnostics.signatures.len(), 48);
    }

    #[test]
    fn stage_windows_are_bounded_and_reset_after_reporting() {
        let mut diagnostics = InputDiagnostics::default();
        let start = Instant::now();
        for i in 0..1000_u64 {
            let origin = start;
            let queued = start + Duration::from_micros(i);
            let dequeued = queued + Duration::from_micros(10);
            diagnostics.input(Some(origin), queued, dequeued);
            diagnostics.flush(dequeued + Duration::from_micros(5));
        }
        assert_eq!(diagnostics.submit_to_enqueue.samples.len(), 512);
        assert_eq!(diagnostics.enqueue_to_worker.samples.len(), 512);
        assert_eq!(diagnostics.dequeue_to_sctp.samples.len(), 512);
        assert_eq!(diagnostics.submit_to_enqueue.count, 1000);
        assert_eq!(diagnostics.enqueue_to_worker.count, 1000);
        assert_eq!(diagnostics.dequeue_to_sctp.count, 1000);
        // Every recorded dequeue-to-sctp span is exactly the flush offset.
        assert!(
            diagnostics
                .dequeue_to_sctp
                .samples
                .iter()
                .all(|micros| *micros == 5)
        );
        assert!(diagnostics.pending_flush.is_empty());

        let later = start + Duration::from_secs(6);
        diagnostics.input(Some(later), later, later);
        diagnostics.flush(later);
        assert!(diagnostics.submit_to_enqueue.samples.is_empty());
        assert_eq!(diagnostics.submit_to_enqueue.count, 0);
        assert_eq!(diagnostics.submit_to_enqueue.max_us, 0);
        assert!(diagnostics.enqueue_to_worker.samples.is_empty());
        assert!(diagnostics.dequeue_to_sctp.samples.is_empty());
    }

    #[test]
    fn sync_path_without_origin_skips_only_the_first_stage() {
        let mut diagnostics = InputDiagnostics::default();
        let start = Instant::now();
        diagnostics.input(None, start, start + Duration::from_micros(7));
        diagnostics.flush(start + Duration::from_micros(9));
        assert_eq!(diagnostics.submit_to_enqueue.count, 0);
        assert_eq!(diagnostics.enqueue_to_worker.count, 1);
        assert_eq!(diagnostics.enqueue_to_worker.max_us, 7);
        assert_eq!(diagnostics.dequeue_to_sctp.count, 1);
        assert_eq!(diagnostics.dequeue_to_sctp.max_us, 2);
    }

    /// The record must reach the durable sink without the opt-in flag, carry
    /// only discriminators, and never carry the payload it walked over.
    #[test]
    fn control_header_reaches_durable_sink_payload_free() {
        let expected = control_header_line("control_channel_reliable", 0x0111, 16, 20, 0, true);
        assert_eq!(
            expected,
            "NVST control header: channel=control_channel_reliable command=0x0111 \
             payloadBytes=16 messageBytes=20 offset=0 complete=true"
        );

        let path = std::env::temp_dir().join(format!(
            "opennow-control-header-test-{}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        opennow_streamer_protocol::log::set_log_file(path.to_str().expect("utf-8 temp path"))
            .expect("sink set");
        let mut diagnostics = InputDiagnostics::default();
        diagnostics.control(
            "control_channel_reliable",
            &[0x11, 0x01, 0x10, 0x00, 0xaa, 0xbb, 0xcc, 0xdd],
        );
        // Repeats stay deduplicated: one record per distinct signature.
        diagnostics.control(
            "control_channel_reliable",
            &[0x11, 0x01, 0x10, 0x00, 0xaa, 0xbb, 0xcc, 0xdd],
        );

        let text = std::fs::read_to_string(&path).expect("durable record written");
        let records: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("command=0x0111"))
            .collect();
        assert_eq!(records.len(), 1, "expected one record, got: {text}");
        assert!(
            records[0].contains("payloadBytes=16 messageBytes=8"),
            "{text}"
        );
        assert!(
            !records[0].contains("aabbccdd") && !records[0].contains("aabb"),
            "payload leaked into the durable record: {}",
            records[0]
        );
        let _ = std::fs::remove_file(&path);
    }
}
