//! Integration tests for LL-HLS (Low-Latency HLS) and LL-DASH support.
//!
//! Tests cover chunk boundary detection, LL-HLS tag parsing and rendering roundtrips,
//! LL-DASH attribute parsing and rendering roundtrips, progressive output with parts,
//! and backward compatibility with non-LL content.

mod common;

use edgepack::manifest::hls;
use edgepack::manifest::hls_input;
use edgepack::manifest::dash;
use edgepack::manifest::dash_input;
use edgepack::manifest::types::*;
use edgepack::media::chunk;
use edgepack::media::cmaf;
use edgepack::media::container::ContainerFormat;
use edgepack::repackager::progressive::ProgressiveOutput;

// ─── Chunk Boundary Detection ────────────────────────────────────────

/// Build a minimal moof+mdat chunk for testing.
fn build_test_chunk(seq: u32, data_size: usize, independent: bool) -> Vec<u8> {
    // Build mfhd
    let mut mfhd = Vec::new();
    cmaf::write_full_box_header(&mut mfhd, 16, b"mfhd", 0, 0);
    mfhd.extend_from_slice(&seq.to_be_bytes());

    // Build tfhd
    let mut tfhd = Vec::new();
    cmaf::write_full_box_header(&mut tfhd, 16, b"tfhd", 0, 0x020000);
    tfhd.extend_from_slice(&1u32.to_be_bytes());

    // Build trun with first_sample_flags
    let first_flags: u32 = if independent { 0x02000000 } else { 0x01000000 };
    let trun_flags: u32 = 0x000004 | 0x000200;
    let trun_size = 8u32 + 4 + 4 + 4 + 4; // header + ver_flags + count + first_flags + sample_size
    let mut trun = Vec::new();
    cmaf::write_box_header(&mut trun, trun_size, b"trun");
    trun.push(0);
    trun.extend_from_slice(&trun_flags.to_be_bytes()[1..4]);
    trun.extend_from_slice(&1u32.to_be_bytes()); // sample_count
    trun.extend_from_slice(&first_flags.to_be_bytes());
    trun.extend_from_slice(&(data_size as u32).to_be_bytes());

    // Build traf
    let mut traf_children = Vec::new();
    traf_children.extend_from_slice(&tfhd);
    traf_children.extend_from_slice(&trun);
    let traf = common::wrap_box(b"traf", &traf_children);

    // Build moof
    let mut moof_children = Vec::new();
    moof_children.extend_from_slice(&mfhd);
    moof_children.extend_from_slice(&traf);
    let moof = common::wrap_box(b"moof", &moof_children);

    // Build mdat
    let mdat = common::wrap_box(b"mdat", &vec![0xAA; data_size]);

    let mut result = Vec::new();
    result.extend_from_slice(&moof);
    result.extend_from_slice(&mdat);
    result
}

#[test]
fn chunk_detection_on_multi_moof_segment() {
    let mut segment = Vec::new();
    let chunk1 = build_test_chunk(1, 200, true);
    let chunk2 = build_test_chunk(2, 150, false);
    let chunk3 = build_test_chunk(3, 100, false);
    segment.extend_from_slice(&chunk1);
    segment.extend_from_slice(&chunk2);
    segment.extend_from_slice(&chunk3);

    let boundaries = chunk::detect_chunk_boundaries(&segment);
    assert_eq!(boundaries.len(), 3);

    // First chunk is independent
    assert!(boundaries[0].independent);
    assert_eq!(boundaries[0].offset, 0);
    assert_eq!(boundaries[0].size, chunk1.len());

    // Second and third are not independent
    assert!(!boundaries[1].independent);
    assert!(!boundaries[2].independent);
}

#[test]
fn chunk_extraction_preserves_data() {
    let chunk = build_test_chunk(1, 100, true);
    let boundaries = chunk::detect_chunk_boundaries(&chunk);
    assert_eq!(boundaries.len(), 1);

    let extracted = chunk::extract_chunk(&chunk, &boundaries[0]).unwrap();
    assert_eq!(extracted, chunk);
}

#[test]
fn chunk_detection_on_single_moof_segment() {
    let segment = build_test_chunk(1, 500, true);
    let boundaries = chunk::detect_chunk_boundaries(&segment);
    assert_eq!(boundaries.len(), 1);
    assert!(boundaries[0].independent);
}

// ─── HLS LL-HLS Parsing and Rendering Roundtrip ─────────────────────

const BASE_URL: &str = "https://cdn.example.com/content/master.m3u8";

#[test]
fn hls_ll_parsing_roundtrip() {
    let manifest = "#EXTM3U\n\
         #EXT-X-VERSION:9\n\
         #EXT-X-TARGETDURATION:4\n\
         #EXT-X-PART-INF:PART-TARGET=0.33334\n\
         #EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.0\n\
         #EXT-X-MAP:URI=\"init.mp4\"\n\
         #EXT-X-PART:DURATION=0.33334,URI=\"part0.0.cmfv\",INDEPENDENT=YES\n\
         #EXT-X-PART:DURATION=0.33334,URI=\"part0.1.cmfv\"\n\
         #EXT-X-PART:DURATION=0.33334,URI=\"part0.2.cmfv\"\n\
         #EXTINF:1.0,\n\
         segment_0.cmfv\n\
         #EXT-X-ENDLIST\n";

    let source = hls_input::parse_hls_manifest(manifest, BASE_URL).unwrap();
    assert_eq!(source.parts.len(), 3);
    assert_eq!(source.part_target_duration, Some(0.33334));
    assert!(source.server_control.is_some());
    let sc = source.server_control.unwrap();
    assert!(sc.can_block_reload);
    assert_eq!(sc.part_hold_back, Some(1.0));
}

#[test]
fn hls_ll_rendering_with_parts() {
    let mut state = ManifestState::new(
        "ll-test".into(),
        OutputFormat::Hls,
        "/base/".into(),
        ContainerFormat::default(),
    );
    state.phase = ManifestPhase::Live;
    state.init_segment = Some(InitSegmentInfo {
        uri: "/base/init.mp4".into(),
        byte_size: 256,
    });
    state.segments.push(SegmentInfo {
        number: 0,
        duration: 1.0,
        uri: "/base/segment_0.cmfv".into(),
        byte_size: 10000,
        key_period: None,
    });
    state.part_target_duration = Some(0.33334);
    state.server_control = Some(ServerControl {
        can_skip_until: None,
        hold_back: None,
        part_hold_back: Some(1.0),
        can_block_reload: true,
    });
    state.parts.push(PartInfo {
        segment_number: 0,
        part_index: 0,
        duration: 0.33334,
        independent: true,
        uri: "/base/part_0.0.cmfv".into(),
        byte_size: 3000,
    });
    state.parts.push(PartInfo {
        segment_number: 0,
        part_index: 1,
        duration: 0.33334,
        independent: false,
        uri: "/base/part_0.1.cmfv".into(),
        byte_size: 3500,
    });

    let m3u8 = hls::render(&state).unwrap();
    assert!(m3u8.contains("#EXT-X-VERSION:9"));
    assert!(m3u8.contains("#EXT-X-PART-INF:PART-TARGET=0.33334"));
    assert!(m3u8.contains("#EXT-X-SERVER-CONTROL:"));
    assert!(m3u8.contains("CAN-BLOCK-RELOAD=YES"));
    assert!(m3u8.contains("PART-HOLD-BACK="));
    assert!(m3u8.contains("#EXT-X-PART:DURATION="));
    assert!(m3u8.contains("INDEPENDENT=YES"));
    assert_eq!(m3u8.matches("#EXT-X-PART:").count(), 2);
    // RFC 8216bis 4.4.4.9: EXT-X-PART tags MUST appear before the EXTINF of the parent segment
    let part_pos = m3u8.find("#EXT-X-PART:DURATION=").unwrap();
    let extinf_pos = m3u8.find("#EXTINF:").unwrap();
    assert!(
        part_pos < extinf_pos,
        "EXT-X-PART must appear before EXTINF per RFC 8216bis"
    );
}

// ─── DASH LL-DASH Parsing and Rendering Roundtrip ────────────────────

const DASH_BASE_URL: &str = "https://cdn.example.com/content/manifest.mpd";

#[test]
fn dash_ll_parsing_roundtrip() {
    let mpd = r#"<?xml version="1.0"?>
<MPD type="dynamic">
  <Period>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="1000" availabilityTimeOffset="5.0" availabilityTimeComplete="false">
          <SegmentTimeline>
            <S d="6000"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let source = dash_input::parse_dash_manifest(mpd, DASH_BASE_URL).unwrap();
    let ll = source.ll_dash_info.unwrap();
    assert!((ll.availability_time_offset - 5.0).abs() < 0.001);
    assert!(!ll.availability_time_complete);
}

#[test]
fn dash_ll_rendering_with_ato() {
    let mut state = ManifestState::new(
        "ll-test".into(),
        OutputFormat::Dash,
        "/base/".into(),
        ContainerFormat::default(),
    );
    state.phase = ManifestPhase::Live;
    state.init_segment = Some(InitSegmentInfo {
        uri: "/base/init.mp4".into(),
        byte_size: 256,
    });
    state.segments.push(SegmentInfo {
        number: 0,
        duration: 6.0,
        uri: "/base/segment_0.cmfv".into(),
        byte_size: 50000,
        key_period: None,
    });
    state.ll_dash_info = Some(LowLatencyDashInfo {
        availability_time_offset: 5.0,
        availability_time_complete: false,
    });

    let mpd = dash::render(&state).unwrap();
    assert!(mpd.contains("availabilityTimeOffset=\"5.000\""));
    assert!(mpd.contains("availabilityTimeComplete=\"false\""));
}

// ─── Progressive Output with Parts ───────────────────────────────────

#[test]
fn progressive_output_with_parts() {
    let mut po = ProgressiveOutput::new(
        "ll-test".into(),
        OutputFormat::Hls,
        "/base/".into(),
        None,
        ContainerFormat::default(),
    );
    po.set_init_segment(vec![0x00; 32]);
    po.set_part_target_duration(0.33334);
    po.set_server_control(ServerControl {
        can_skip_until: None,
        hold_back: None,
        part_hold_back: Some(1.0),
        can_block_reload: true,
    });

    po.add_part(0, 0, vec![0xAA; 100], 0.33334, true);
    po.add_part(0, 1, vec![0xBB; 80], 0.33334, false);
    let manifest = po.add_segment(0, vec![0xCC; 300], 1.0);
    assert!(manifest.is_some());

    let m3u8 = manifest.unwrap();
    assert!(m3u8.contains("#EXT-X-VERSION:9"));
    assert!(m3u8.contains("#EXT-X-PART-INF"));
    assert!(m3u8.contains("#EXT-X-SERVER-CONTROL"));
    assert_eq!(m3u8.matches("#EXT-X-PART:").count(), 2);

    // Verify part data lookup
    assert_eq!(po.part_data(0, 0).unwrap().len(), 100);
    assert_eq!(po.part_data(0, 1).unwrap().len(), 80);
    assert!(po.part_data(0, 2).is_none());
}

#[test]
fn progressive_output_dash_with_ll_info() {
    let mut po = ProgressiveOutput::new(
        "ll-test".into(),
        OutputFormat::Dash,
        "/base/".into(),
        None,
        ContainerFormat::default(),
    );
    po.set_init_segment(vec![0x00; 32]);
    po.set_ll_dash_info(LowLatencyDashInfo {
        availability_time_offset: 3.0,
        availability_time_complete: false,
    });

    po.add_segment(0, vec![0xAA; 100], 6.0);
    let manifest = po.current_manifest().unwrap();
    assert!(manifest.contains("availabilityTimeOffset=\"3.000\""));
    assert!(manifest.contains("availabilityTimeComplete=\"false\""));
}

// ─── Serde Roundtrips ────────────────────────────────────────────────

#[test]
fn server_control_serde_roundtrip() {
    let sc = ServerControl {
        can_skip_until: Some(12.0),
        hold_back: Some(9.0),
        part_hold_back: Some(1.0),
        can_block_reload: true,
    };
    let json = serde_json::to_string(&sc).unwrap();
    let parsed: ServerControl = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.can_skip_until, Some(12.0));
    assert_eq!(parsed.hold_back, Some(9.0));
    assert_eq!(parsed.part_hold_back, Some(1.0));
    assert!(parsed.can_block_reload);
}

#[test]
fn part_info_serde_roundtrip() {
    let part = PartInfo {
        segment_number: 2,
        part_index: 1,
        duration: 0.33334,
        independent: true,
        uri: "/base/part_2.1.cmfv".to_string(),
        byte_size: 5000,
    };
    let json = serde_json::to_string(&part).unwrap();
    let parsed: PartInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.segment_number, 2);
    assert_eq!(parsed.part_index, 1);
    assert!(parsed.independent);
}

#[test]
fn low_latency_dash_info_serde_roundtrip() {
    let info = LowLatencyDashInfo {
        availability_time_offset: 5.5,
        availability_time_complete: false,
    };
    let json = serde_json::to_string(&info).unwrap();
    let parsed: LowLatencyDashInfo = serde_json::from_str(&json).unwrap();
    assert!((parsed.availability_time_offset - 5.5).abs() < f64::EPSILON);
    assert!(!parsed.availability_time_complete);
}

#[test]
fn source_part_info_serde_roundtrip() {
    let sp = SourcePartInfo {
        segment_number: 1,
        part_index: 0,
        duration: 0.33334,
        independent: true,
        uri: "https://cdn.example.com/part.cmfv".to_string(),
    };
    let json = serde_json::to_string(&sp).unwrap();
    let parsed: SourcePartInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.segment_number, 1);
    assert!(parsed.independent);
}

// ─── Backward Compatibility ──────────────────────────────────────────

#[test]
fn non_ll_content_unchanged_hls() {
    let state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    let m3u8 = hls::render(&state).unwrap();
    assert!(m3u8.contains("#EXT-X-VERSION:7"));
    assert!(!m3u8.contains("#EXT-X-VERSION:9"));
    assert!(!m3u8.contains("#EXT-X-PART-INF"));
    assert!(!m3u8.contains("#EXT-X-SERVER-CONTROL"));
    assert!(!m3u8.contains("#EXT-X-PART:"));
}

#[test]
fn non_ll_content_unchanged_dash() {
    let state = common::make_dash_manifest_state(3, ManifestPhase::Complete);
    let mpd = dash::render(&state).unwrap();
    assert!(!mpd.contains("availabilityTimeOffset"));
    assert!(!mpd.contains("availabilityTimeComplete"));
}

#[test]
fn manifest_state_backward_compat_deserialization() {
    // Simulate old JSON without LL-HLS/LL-DASH fields
    let json = r#"{
        "content_id": "old-content",
        "format": "Hls",
        "phase": "Complete",
        "init_segment": null,
        "segments": [],
        "target_duration": 6.0,
        "variants": [],
        "drm_info": null,
        "media_sequence": 0,
        "base_url": "/"
    }"#;
    let state: ManifestState = serde_json::from_str(json).unwrap();
    assert!(state.parts.is_empty());
    assert!(state.part_target_duration.is_none());
    assert!(state.server_control.is_none());
    assert!(state.ll_dash_info.is_none());
}
