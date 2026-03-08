//! SCTE-35 integration tests — end-to-end ad break pipeline verification.
//!
//! Tests emsg extraction, SCTE-35 parsing, manifest signaling, and
//! source manifest ad marker roundtripping.

mod common;

use edgepack::manifest;
use edgepack::manifest::types::{
    AdBreakInfo, ManifestPhase, ManifestState,
};
use edgepack::media::cmaf::{self, EmsgBox};
use edgepack::media::scte35;
use edgepack::media::segment;
use edgepack::manifest::hls_input;
use edgepack::manifest::dash_input;

use common::*;

// ─── emsg + SCTE-35 Extraction ───────────────────────────────────────

/// Build a synthetic segment with emsg + moof + mdat.
fn build_segment_with_emsg(emsg: &EmsgBox) -> Vec<u8> {
    // build_emsg_box returns the full emsg box (header + payload), no need to wrap again
    let emsg_box = cmaf::build_emsg_box(emsg);

    // Build a simple clear moof+mdat
    let (clear_seg, _) = build_clear_media_segment(2, 160);

    let mut seg = Vec::with_capacity(emsg_box.len() + clear_seg.len());
    seg.extend_from_slice(&emsg_box);
    seg.extend_from_slice(&clear_seg);
    seg
}

fn make_scte35_splice_insert_emsg() -> EmsgBox {
    EmsgBox {
        version: 1,
        scheme_id_uri: "urn:scte:scte35:2013:bin".into(),
        value: String::new(),
        timescale: 90000,
        presentation_time: 540540, // 6.006 seconds
        event_duration: 2700000,   // 30 seconds
        id: 42,
        message_data: build_scte35_splice_insert(42, true, Some(2700000)),
    }
}

/// Build a minimal SCTE-35 splice_insert binary command.
fn build_scte35_splice_insert(
    splice_event_id: u32,
    out_of_network: bool,
    break_duration_90khz: Option<u64>,
) -> Vec<u8> {
    let mut data = Vec::new();

    // table_id = 0xFC
    data.push(0xFC);

    // We'll fill in section_length later
    let length_pos = data.len();
    data.push(0x00); // section_syntax_indicator(1) + private(1) + sap_type(2) + section_length(12)
    data.push(0x00);

    // protocol_version = 0
    data.push(0x00);

    // encrypted_packet(1) + encryption_algorithm(6) + pts_adjustment(33)
    data.push(0x00); // encrypted=0, enc_algo=0
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // pts_adjustment = 0

    // cw_index(8)
    data.push(0x00);

    // tier(12) + splice_command_length(12) = 24 bits
    // tier = 0xFFF (all), splice_command_length computed later
    let cmd_length_pos = data.len();
    data.push(0xFF); // tier[11:4]
    data.push(0xF0); // tier[3:0] + splice_command_length[11:8]
    data.push(0x00); // splice_command_length[7:0]

    // splice_command_type = 0x05 (splice_insert)
    data.push(0x05);

    let cmd_start = data.len();

    // splice_event_id (32)
    data.extend_from_slice(&splice_event_id.to_be_bytes());

    // splice_event_cancel_indicator(1) + reserved(7)
    data.push(0x00); // not cancelled

    // out_of_network_indicator(1) + program_splice_flag(1) + duration_flag(1) +
    // splice_immediate_flag(1) + reserved(4)
    let mut flags = 0u8;
    if out_of_network {
        flags |= 0x80;
    }
    flags |= 0x40; // program_splice_flag = 1
    if break_duration_90khz.is_some() {
        flags |= 0x20; // duration_flag = 1
    }
    flags |= 0x10; // splice_immediate_flag = 1
    data.push(flags);

    // break_duration (if present): auto_return(1) + reserved(6) + duration(33)
    if let Some(dur) = break_duration_90khz {
        data.push(0x80 | ((dur >> 32) as u8 & 0x01)); // auto_return=1
        data.extend_from_slice(&(dur as u32).to_be_bytes());
    }

    // unique_program_id (16) + avail_num (8) + avails_expected (8)
    data.extend_from_slice(&[0x00, 0x01]); // unique_program_id = 1
    data.push(0x00); // avail_num
    data.push(0x00); // avails_expected

    let cmd_end = data.len();
    let cmd_len = (cmd_end - cmd_start) as u16;

    // Patch splice_command_length
    data[cmd_length_pos + 1] = (data[cmd_length_pos + 1] & 0xF0) | ((cmd_len >> 8) as u8 & 0x0F);
    data[cmd_length_pos + 2] = cmd_len as u8;

    // descriptor_loop_length (16) = 0
    data.extend_from_slice(&[0x00, 0x00]);

    // Patch section_length (everything after section_length field to end)
    let section_length = (data.len() - 3) as u16;
    data[length_pos] = 0x30 | ((section_length >> 8) as u8 & 0x0F);
    data[length_pos + 1] = section_length as u8;

    data
}

#[test]
fn extract_emsg_from_synthetic_segment() {
    let emsg = make_scte35_splice_insert_emsg();
    let seg = build_segment_with_emsg(&emsg);

    let boxes = segment::extract_emsg_boxes(&seg);
    assert_eq!(boxes.len(), 1);
    assert_eq!(boxes[0].scheme_id_uri, "urn:scte:scte35:2013:bin");
    assert_eq!(boxes[0].id, 42);
}

#[test]
fn emsg_identified_as_scte35() {
    let emsg = make_scte35_splice_insert_emsg();
    assert!(scte35::is_scte35_emsg(&emsg));
}

#[test]
fn emsg_non_scte35_rejected() {
    let emsg = EmsgBox {
        version: 1,
        scheme_id_uri: "urn:mpeg:dash:event:2012".into(),
        value: String::new(),
        timescale: 1000,
        presentation_time: 0,
        event_duration: 0,
        id: 1,
        message_data: vec![0x01, 0x02],
    };
    assert!(!scte35::is_scte35_emsg(&emsg));
}

#[test]
fn parse_scte35_splice_insert_from_emsg() {
    let emsg = make_scte35_splice_insert_emsg();
    let splice = scte35::parse_splice_info(&emsg.message_data).unwrap();

    assert_eq!(splice.splice_command_type, 0x05);
    assert_eq!(splice.splice_event_id, 42);
    assert!(splice.out_of_network);
    assert!(splice.break_duration.is_some());
}

#[test]
fn no_emsg_in_clean_segment() {
    let (seg, _) = build_clear_media_segment(3, 160);
    let boxes = segment::extract_emsg_boxes(&seg);
    assert!(boxes.is_empty());
}

// ─── HLS Ad Break Manifest Rendering ─────────────────────────────────

#[test]
fn hls_manifest_with_ad_break_has_daterange() {
    let mut state = make_hls_manifest_state(3, ManifestPhase::Complete);
    state.ad_breaks.push(AdBreakInfo {
        id: 42,
        presentation_time: 6.006,
        duration: Some(30.0),
        scte35_cmd: Some("AQIDBA==".into()),
        segment_number: 1,
    });

    let text = manifest::render_manifest(&state).unwrap();
    assert!(text.contains("#EXT-X-DATERANGE:"));
    assert!(text.contains("ID=\"splice-42\""));
    assert!(text.contains("PLANNED-DURATION=30"));
    assert!(text.contains("SCTE35-CMD=0x"));
}

#[test]
fn hls_manifest_no_ad_breaks_no_daterange() {
    let state = make_hls_manifest_state(3, ManifestPhase::Complete);
    let text = manifest::render_manifest(&state).unwrap();
    assert!(!text.contains("#EXT-X-DATERANGE:"));
}

#[test]
fn hls_daterange_start_date_beyond_24h() {
    let mut state = make_hls_manifest_state(1, ManifestPhase::Complete);
    // 90061.5s = 25h 1m 1.5s → should produce 1970-01-02T01:01:01.500Z (not wrap)
    state.ad_breaks.push(AdBreakInfo {
        id: 1,
        presentation_time: 90061.5,
        duration: None,
        scte35_cmd: None,
        segment_number: 0,
    });
    let text = manifest::render_manifest(&state).unwrap();
    assert!(
        text.contains("START-DATE=\"1970-01-02T01:01:01.500Z\""),
        "START-DATE must not wrap at 24 hours: {text}"
    );
}

// ─── DASH Ad Break Manifest Rendering ────────────────────────────────

#[test]
fn dash_manifest_with_ad_break_has_event_stream() {
    let mut state = make_dash_manifest_state(3, ManifestPhase::Complete);
    state.ad_breaks.push(AdBreakInfo {
        id: 42,
        presentation_time: 6.0,
        duration: Some(30.0),
        scte35_cmd: Some("AQIDBA==".into()),
        segment_number: 1,
    });

    let text = manifest::render_manifest(&state).unwrap();
    assert!(text.contains("<EventStream"));
    assert!(text.contains("urn:scte:scte35:2013:bin"));
    assert!(text.contains("id=\"42\""));
    assert!(text.contains("AQIDBA=="));
}

#[test]
fn dash_manifest_no_ad_breaks_no_event_stream() {
    let state = make_dash_manifest_state(3, ManifestPhase::Complete);
    let text = manifest::render_manifest(&state).unwrap();
    assert!(!text.contains("<EventStream"));
}

// ─── Source Manifest Ad Break Parsing ────────────────────────────────

#[test]
fn hls_input_parse_daterange_roundtrip() {
    // Build an HLS manifest with DATERANGE, parse it, verify ad breaks
    let manifest = "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-TARGETDURATION:7\n\
         #EXT-X-MAP:URI=\"init.mp4\"\n\
         #EXTINF:6.006,\n\
         segment_0.cmfv\n\
         #EXT-X-DATERANGE:ID=\"splice-42\",START-DATE=\"2024-01-01T00:00:06.006Z\",PLANNED-DURATION=30.0,SCTE35-CMD=0xFC301100\n\
         #EXTINF:6.006,\n\
         segment_1.cmfv\n\
         #EXTINF:4.004,\n\
         segment_2.cmfv\n\
         #EXT-X-ENDLIST\n";

    let source = hls_input::parse_hls_manifest(
        manifest,
        "https://cdn.example.com/content/master.m3u8",
    )
    .unwrap();

    assert_eq!(source.ad_breaks.len(), 1);
    let ab = &source.ad_breaks[0];
    assert_eq!(ab.id, 42);
    assert!((ab.duration.unwrap() - 30.0).abs() < 0.001);
    assert!(ab.scte35_cmd.is_some()); // hex→base64 roundtrip
}

#[test]
fn dash_input_parse_event_stream_roundtrip() {
    let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT18S">
  <Period>
    <EventStream schemeIdUri="urn:scte:scte35:2013:bin" timescale="90000">
      <Event presentationTime="540540" duration="2700000" id="42">AQIDBA==</Event>
    </EventStream>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="1000" startNumber="0">
          <SegmentTimeline>
            <S d="6000" r="2"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let source = dash_input::parse_dash_manifest(
        mpd,
        "https://cdn.example.com/content/manifest.mpd",
    )
    .unwrap();

    assert_eq!(source.ad_breaks.len(), 1);
    let ab = &source.ad_breaks[0];
    assert_eq!(ab.id, 42);
    assert!((ab.presentation_time - 6.006).abs() < 0.001);
    assert!((ab.duration.unwrap() - 30.0).abs() < 0.001);
    assert_eq!(ab.scte35_cmd.as_deref(), Some("AQIDBA=="));
}

// ─── Ad Break Serde Roundtrip ────────────────────────────────────────

#[test]
fn ad_break_info_serde_roundtrip() {
    let ab = AdBreakInfo {
        id: 42,
        presentation_time: 6.006,
        duration: Some(30.0),
        scte35_cmd: Some("AQIDBA==".into()),
        segment_number: 1,
    };
    let json = serde_json::to_string(&ab).unwrap();
    let deserialized: AdBreakInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.id, 42);
    assert!((deserialized.presentation_time - 6.006).abs() < 0.001);
    assert!((deserialized.duration.unwrap() - 30.0).abs() < 0.001);
    assert_eq!(deserialized.scte35_cmd.as_deref(), Some("AQIDBA=="));
}

#[test]
fn manifest_state_with_ad_breaks_serde_roundtrip() {
    let mut state = make_hls_manifest_state(2, ManifestPhase::Complete);
    state.ad_breaks.push(AdBreakInfo {
        id: 1,
        presentation_time: 6.0,
        duration: Some(15.0),
        scte35_cmd: None,
        segment_number: 1,
    });

    let json = serde_json::to_string(&state).unwrap();
    let deserialized: ManifestState = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.ad_breaks.len(), 1);
    assert_eq!(deserialized.ad_breaks[0].id, 1);
}
