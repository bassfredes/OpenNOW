use opennow_streamer_platform::{MediaStreamConfig, MediaVideoCodec};

/// NVST ANNOUNCE color encoding, matching the official wire format.
///
/// `bitDepth` is literal (8 or 10) and both base lines are always sent
/// explicitly; the seat never sees a lone `bitDepth` line. `dynamicRangeMode`
/// is sent only for HDR (`1`); SDR omits it, matching the captured baseline.
///
/// `chromaFormat` carries `1` for every session. That is the only value ever
/// observed from the official client on the wire — both its captured 4:2:0
/// announce (vendor capture, `NvstCapturedAnnounce`) and its live 4:4:4
/// session whose decode was verified Y410 (`geronimo-20260924-0018.log`
/// `video[0].chromaFormat: 1`, line 12148). OpenNOW previously sent `3`
/// (`chroma_format_idc` for 4:4:4) while the seat accepted the ANNOUNCE and
/// then delivered 4:2:0 in every session; 4:4:4 discrimination rides on
/// CloudMatch `requestedStreamingFeatures.chromaFormat` (app enum, `1` for
/// 4:4:4), exactly as the official request does.
///
/// The extra lines below exist only in the official client's live 4:4:4
/// session config (surfaceFormat, H265BitStreamProfile — neither appears in
/// its 4:2:0 capture) and were absent from every OpenNOW announce that ended
/// up decoding 4:2:0. The prefilter Mode 1 counterpart lives with the rest of
/// the announce's prefilter block (nvst_rtsp.rs), which is also emitted for
/// every session. Values are copied verbatim; no value is invented.
pub(super) fn announce_color_lines(stream: MediaStreamConfig) -> Vec<String> {
    let mut lines = vec![
        format!(
            "a=x-nv-video[0].bitDepth:{}",
            stream.color_quality.bit_depth()
        ),
        "a=x-nv-video[0].chromaFormat:1".to_owned(),
    ];
    if stream.hdr {
        lines.push("a=x-nv-video[0].dynamicRangeMode:1".to_owned());
    }
    if stream.color_quality.is_444() {
        lines.push("a=x-nv-video[0].surfaceFormat:0".to_owned());
        if matches!(stream.codec, MediaVideoCodec::H265) {
            lines.push("a=x-nv-vqos[0].H265BitStreamProfile:1".to_owned());
        }
    }
    lines
}

#[cfg(test)]
#[path = "nvst_rtsp_color_tests.rs"]
mod tests;
