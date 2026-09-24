use super::*;
use opennow_streamer_platform::{MediaColorQuality, MediaVideoCodec};

fn stream(color_quality: MediaColorQuality, hdr: bool) -> MediaStreamConfig {
    MediaStreamConfig {
        codec: MediaVideoCodec::H265,
        color_quality,
        hdr,
        width: 2560,
        height: 1440,
        fps: 120,
        ..MediaStreamConfig::default()
    }
}

#[test]
fn chroma_line_is_officials_observed_wire_value_for_every_quality() {
    // The official client sends chromaFormat:1 in both observed announces:
    // the captured 10-bit 4:2:0 session and the live session whose decode was
    // verified Y410. OpenNOW's previous 3 for 4:4:4 was accepted by the seat
    // and delivered as 4:2:0 every time.
    for quality in [
        MediaColorQuality::EightBit420,
        MediaColorQuality::EightBit444,
        MediaColorQuality::TenBit420,
        MediaColorQuality::TenBit444,
    ] {
        let lines = announce_color_lines(stream(quality, false));
        assert!(
            lines.contains(&"a=x-nv-video[0].chromaFormat:1".to_owned()),
            "{quality:?}: {lines:?}"
        );
        let depth = quality.bit_depth();
        assert!(
            lines.contains(&format!("a=x-nv-video[0].bitDepth:{depth}")),
            "{quality:?}"
        );
    }
}

#[test]
fn four_four_four_sessions_carry_the_official_live_profile_block() {
    let lines = announce_color_lines(stream(MediaColorQuality::TenBit444, false));
    for expected in [
        "a=x-nv-video[0].surfaceFormat:0",
        "a=x-nv-vqos[0].H265BitStreamProfile:1",
    ] {
        assert!(
            lines.contains(&expected.to_owned()),
            "{expected}: {lines:?}"
        );
    }

    let two_two_zero = announce_color_lines(stream(MediaColorQuality::TenBit420, false));
    for absent in [
        "a=x-nv-video[0].surfaceFormat:0",
        "a=x-nv-vqos[0].H265BitStreamProfile:1",
    ] {
        assert!(
            !two_two_zero.contains(&absent.to_owned()),
            "4:2:0 must keep the captured baseline: {two_two_zero:?}"
        );
    }

    // The bitstream profile follows the codec: non-H.265 never announces it.
    let av1 = stream(MediaColorQuality::TenBit444, false);
    let av1_codec = MediaStreamConfig {
        codec: MediaVideoCodec::Av1,
        ..av1
    };
    // The session-level gating keeps AV1 on 4:2:0 in production; this only
    // proves the announce helper itself keys the profile line off H.265.
    let lines = announce_color_lines(av1_codec);
    assert!(!lines.contains(&"a=x-nv-vqos[0].H265BitStreamProfile:1".to_owned()));
    assert!(lines.contains(&"a=x-nv-video[0].surfaceFormat:0".to_owned()));
}

#[test]
fn dynamic_range_mode_is_hdr_only() {
    assert_eq!(
        announce_color_lines(stream(MediaColorQuality::TenBit420, true)),
        vec![
            "a=x-nv-video[0].bitDepth:10",
            "a=x-nv-video[0].chromaFormat:1",
            "a=x-nv-video[0].dynamicRangeMode:1",
        ]
    );
    assert_eq!(
        announce_color_lines(stream(MediaColorQuality::TenBit444, true)),
        vec![
            "a=x-nv-video[0].bitDepth:10",
            "a=x-nv-video[0].chromaFormat:1",
            "a=x-nv-video[0].dynamicRangeMode:1",
            "a=x-nv-video[0].surfaceFormat:0",
            "a=x-nv-vqos[0].H265BitStreamProfile:1",
        ]
    );
    for quality in [
        MediaColorQuality::EightBit420,
        MediaColorQuality::EightBit444,
        MediaColorQuality::TenBit420,
        MediaColorQuality::TenBit444,
    ] {
        assert!(
            announce_color_lines(stream(quality, false))
                .iter()
                .all(|line| !line.contains("dynamicRangeMode")),
            "{quality:?}"
        );
    }
}

#[test]
fn ten_bit_420_matches_the_vendor_capture() {
    // The only vendor-observed 10-bit 4:2:0 value on the wire.
    assert_eq!(
        announce_color_lines(stream(MediaColorQuality::TenBit420, false)),
        vec![
            "a=x-nv-video[0].bitDepth:10",
            "a=x-nv-video[0].chromaFormat:1",
        ]
    );
}
