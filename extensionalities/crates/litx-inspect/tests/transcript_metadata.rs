//! Module: glassbox-inspect `glassbox:v1` transcript metadata and v3 media contract tests (`V-MED-01`).
//! File: transcript_metadata.rs.
//! Responsibility: Verify pure-Rust Crockford Base32 ULID generation, UTC RFC 3339
//! formatting, JSON-in-YAML `schema: "glassbox:v1"` frontmatter emission and hostile
//! character safety, independent `--no-markers` / `--no-frontmatter` suppression,
//! `selection::render_markdown_full` routing, media v3 JSON round-trip, and
//! non-media v3 JSON and document Markdown byte-identity.

use glassbox_inspect::extract;
use glassbox_inspect::reports::{
    Artifact, ArtifactKind, AsrReceipt, AudioExtractReceipt, Completeness, ContactSheetReceipt,
    ContentOrigin, ContentUnit, Diagnostic, DiagnosticSeverity, DiagnosticStage,
    EmbeddedMetadataField, ExtractOptions, ExtractReport, ExtractionProvenance, FactSource,
    MediaReceipt, NativeSelection, NativeUnit, OcrOptions, OperationStatus, QualifiedCount,
    SourcedMetadata,
};
use glassbox_inspect::reports_v3::ExtractReportV3;
use glassbox_inspect::selection::{
    render_markdown, render_markdown_full, render_markdown_with_options,
};
use glassbox_inspect::transcript_markdown::{
    GLASSBOX_TRANSCRIPT_SCHEMA, TranscriptRenderOptions, format_utc_rfc3339, generate_ulid,
    generate_ulid_at, is_valid_ulid, render_transcript_markdown,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Parses a fenced JSON-in-YAML `glassbox:v1` frontmatter header into ordered key-value JSON entries.
///
/// Accepts rendered Markdown starting with `---\n`. Returns `(frontmatter_map, body)`
/// where every frontmatter value is decoded through `serde_json::from_str`. Panics
/// if fences are missing, if any line splits across Unicode line terminators, if
/// duplicate keys appear, or if any value is not valid JSON.
fn parse_frontmatter(markdown: &str) -> (BTreeMap<String, Value>, &str) {
    let rest = markdown
        .strip_prefix("---\n")
        .unwrap_or_else(|| panic!("expected opening frontmatter fence: {markdown:?}"));
    let close_idx = rest
        .find("\n---")
        .unwrap_or_else(|| panic!("expected closing frontmatter fence: {markdown:?}"));
    let header = &rest[..close_idx];
    let after_fence = &rest[close_idx + 4..];
    let body = after_fence
        .strip_prefix("\n\n")
        .or_else(|| after_fence.strip_prefix('\n'))
        .unwrap_or(after_fence);

    let mut fields = BTreeMap::new();
    for line in header.lines() {
        assert!(
            !line.contains('\u{2028}') && !line.contains('\u{2029}'),
            "frontmatter line must not contain raw Unicode line separators: {line:?}"
        );
        let (key, raw_value) = line
            .split_once(": ")
            .unwrap_or_else(|| panic!("malformed frontmatter line: {line:?}"));
        let decoded: Value = serde_json::from_str(raw_value).unwrap_or_else(|error| {
            panic!("frontmatter key `{key}` was not valid JSON `{raw_value}`: {error}")
        });
        assert!(
            fields.insert(key.to_owned(), decoded).is_none(),
            "duplicate frontmatter key `{key}`"
        );
    }
    (fields, body)
}

/// Builds a synthetic timed transcript [`ExtractReport`] with optional title, language, and companion artifacts.
///
/// Accepts source path, optional embedded title, optional ASR language, optional
/// contact-sheet output path, optional audio-extract path, and `(start_ms, end_ms, text)`
/// intervals. Returns a populated [`ExtractReport`] carrying `NativeUnit::TimeInterval`
/// units and typed `MediaReceipt` metadata. Performs no I/O and cannot fail.
fn build_transcript_report(
    source_path: &str,
    embedded_title: Option<&str>,
    asr_language: Option<&str>,
    contact_sheet_path: Option<&str>,
    audio_extract_path: Option<&str>,
    segments: &[(u64, u64, &str)],
) -> ExtractReport {
    let mut report = ExtractReport {
        status: OperationStatus::Complete,
        completeness: Completeness::Partial,
        ..ExtractReport::default()
    };
    report.metadata.status = OperationStatus::Complete;
    report.metadata.completeness = Completeness::Exact;
    report.metadata.identity.path = PathBuf::from(source_path);
    report.metadata.digest.digest = Some("deadbeef".repeat(8));

    if let Some(title) = embedded_title {
        report.metadata.embedded_metadata.push(SourcedMetadata {
            field: EmbeddedMetadataField::Title,
            value: title.to_owned(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Exact,
        });
    }

    let mut sampled_ranges_ms = Vec::new();
    for (idx, &(start_ms, end_ms, text)) in segments.iter().enumerate() {
        sampled_ranges_ms.push([start_ms, end_ms]);
        report.content.units.push(ContentUnit {
            unit_index: idx as u64 + 1,
            unit: NativeUnit::TimeInterval,
            source_part: Some(format!("audio#window/{}", idx + 1)),
            label: None,
            marker_index: None,
            start_ms: Some(start_ms),
            end_ms: Some(end_ms),
            text: Some(text.to_owned()),
            rows: Vec::new(),
            provenance: vec![ExtractionProvenance {
                reader: "media-asr".to_owned(),
                stage: "asr".to_owned(),
                source: FactSource::GeneratedArtifact,
                origin: ContentOrigin::Native,
                ocr: None,
            }],
            completeness: Completeness::Exact,
        });
    }

    report.selection.unit = NativeUnit::TimeInterval;
    report.selection.returned_indices = (1..=segments.len() as u64).collect();
    report.selection.available = QualifiedCount {
        unit: NativeUnit::TimeInterval,
        value: Some(segments.len() as u64),
        source: FactSource::GeneratedArtifact,
        completeness: Completeness::Exact,
    };

    let contact_sheet = contact_sheet_path.map(|path| ContactSheetReceipt {
        output: PathBuf::from(path),
        duration_seconds: 1_800.0,
        columns: 5,
        rows: 3,
        tiles: 15,
        mode: "adaptive".to_owned(),
        preset: "quality".to_owned(),
        format: "jpg".to_owned(),
        width_px: 1_920,
        height_px: 678,
        cell_width_px: 360,
        cell_height_px: 202,
        seek_seconds: vec![60.0, 180.0],
        reused_tiles: 0,
        linear_seek_tiles: 1,
        every_seconds: None,
    });
    let audio_extract = audio_extract_path.map(|path| AudioExtractReceipt {
        path: PathBuf::from(path),
        format: "wav".to_owned(),
        duration_ms: 1_800_000,
    });
    report.media = Some(MediaReceipt {
        contact_sheet,
        asr: Some(AsrReceipt {
            mode: "adaptive".to_owned(),
            backend: "openai".to_owned(),
            model: "whisper-1".to_owned(),
            provider: "api.example.test".to_owned(),
            protocol: Some("transcriptions".to_owned()),
            language: asr_language.map(str::to_owned),
            duration_ms: 1_800_000,
            sampled_ranges_ms,
            windows_attempted: segments.len() as u64,
            windows_succeeded: segments.len() as u64,
        }),
        audio_extract,
    });
    report
}

/// Verifies deterministic and live pure-Rust Crockford Base32 ULID generation (`V-MED-01`).
///
/// Accepts no parameters. Returns unit after checking zero/max/known-vector
/// ULID encoding, 48-bit timestamp masking, alphabet exclusions (`I`, `L`, `O`,
/// `U`), and uniqueness across live `generate_ulid()` calls. Panics on mismatch.
#[test]
fn pure_rust_ulid_matches_crockford_base32_contract() {
    assert_eq!(generate_ulid_at(0, [0u8; 10]), "00000000000000000000000000");
    assert_eq!(
        generate_ulid_at(0x0000_FFFF_FFFF_FFFF, [0xFFu8; 10]),
        "7ZZZZZZZZZZZZZZZZZZZZZZZZZ"
    );
    // Bits above 48 must be masked so the first character never exceeds '7'.
    assert_eq!(
        generate_ulid_at(u64::MAX, [0xFFu8; 10]),
        "7ZZZZZZZZZZZZZZZZZZZZZZZZZ"
    );
    // Standard ULID spec timestamp vectors: 1469918176385 ms -> "01ARYZ6S41", 1469922850259 ms -> "01ARZ3NDEK".
    let known = generate_ulid_at(1_469_918_176_385, [0u8; 10]);
    assert_eq!(known, "01ARYZ6S410000000000000000");
    assert!(is_valid_ulid(&known));
    let spec_example = generate_ulid_at(1_469_922_850_259, [0u8; 10]);
    assert_eq!(spec_example, "01ARZ3NDEK0000000000000000");
    assert!(is_valid_ulid(&spec_example));

    // Reject invalid ULID shapes and excluded Crockford letters.
    for invalid in [
        "",
        "01ARZ3NDEK000000000000000",   // 25 chars
        "01ARZ3NDEK00000000000000000", // 27 chars
        "80000000000000000000000000",  // first char > '7'
        "01ARZ3NDEK000000000000000I",  // excluded I
        "01ARZ3NDEK000000000000000L",  // excluded L
        "01ARZ3NDEK000000000000000O",  // excluded O
        "01ARZ3NDEK000000000000000U",  // excluded U
        "01arz3ndek0000000000000000",  // lowercase
    ] {
        assert!(!is_valid_ulid(invalid), "incorrectly accepted {invalid:?}");
    }

    let mut seen = BTreeSet::new();
    for _ in 0..32 {
        let id = generate_ulid();
        assert!(is_valid_ulid(&id), "invalid live ULID: {id:?}");
        assert!(seen.insert(id), "duplicate ULID generated");
    }
}

/// Verifies Hinnant civil-from-days UTC RFC 3339 formatting across leap and century dates (`V-MED-01`).
///
/// Accepts no parameters. Returns unit after checking epoch, leap-day, century,
/// and current-era timestamps. Panics on mismatch.
#[test]
fn format_utc_rfc3339_handles_epoch_leap_days_and_centuries() {
    assert_eq!(format_utc_rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(format_utc_rfc3339(1_709_251_199), "2024-02-29T23:59:59Z");
    assert_eq!(format_utc_rfc3339(1_709_251_200), "2024-03-01T00:00:00Z");
    assert_eq!(format_utc_rfc3339(4_107_542_400), "2100-03-01T00:00:00Z");
    assert_eq!(format_utc_rfc3339(1_795_651_200), "2026-11-26T00:00:00Z");
}

/// Verifies `glassbox:v1` frontmatter fields, sparse omission rules, and forbidden-field exclusion (`V-MED-01`).
///
/// Accepts no parameters. Returns unit after verifying both full-optional and
/// sparse-minimal transcript reports. Panics if required fields are missing,
/// optional fields leak when absent, or forbidden fields (`content_type`,
/// `sha256`, `metadata_profile`, `domains`, `systems`, `classification`) appear.
#[test]
fn glassbox_v1_frontmatter_emits_required_and_sparse_fields_and_omits_forbidden_keys() {
    let report = build_transcript_report(
        "/archive/2026/Climate Keynote.mp4",
        Some("Climate Risk Keynote"),
        Some("en"),
        Some("/out/Climate Keynote-contact-sheet.jpg"),
        Some("/out/Climate Keynote-extract.wav"),
        &[
            (0, 30_000, "Welcome everyone."),
            (900_000, 930_000, "Turning to adaptation finance."),
        ],
    );

    let options = TranscriptRenderOptions {
        include_markers: true,
        include_frontmatter: true,
        id_override: Some("01M8AKN6A2ZKVQGEWRJMHVH0S7".to_owned()),
        updated_at_override: Some("2026-11-26T01:30:00Z".to_owned()),
    };
    let rendered = render_transcript_markdown(&report, &options);
    let (fields, body) = parse_frontmatter(&rendered);

    assert_eq!(fields["schema"], GLASSBOX_TRANSCRIPT_SCHEMA);
    assert_eq!(fields["id"], "01M8AKN6A2ZKVQGEWRJMHVH0S7");
    assert!(is_valid_ulid(fields["id"].as_str().unwrap()));
    assert_eq!(fields["title"], "Climate Risk Keynote");
    assert_eq!(fields["type"], "transcript");
    assert_eq!(fields["tags"], serde_json::json!([]));
    assert_eq!(fields["source_url"], "/archive/2026/Climate Keynote.mp4");
    assert_eq!(fields["language"], "en");
    assert_eq!(fields["updated_at"], "2026-11-26T01:30:00Z");
    assert_eq!(
        fields["extraction"],
        serde_json::json!({
            "mode": "adaptive",
            "backend": "openai",
            "model": "whisper-1",
            "completeness": "partial",
            "duration_ms": 1_800_000,
            "sampled_ranges_ms": [[0, 30_000], [900_000, 930_000]],
            "contact_sheet": "/out/Climate Keynote-contact-sheet.jpg",
            "audio_extract": "/out/Climate Keynote-extract.wav"
        })
    );

    // Forbidden / AskClaire-owned / non-frontmatter keys must never appear.
    for forbidden in [
        "content_type",
        "sha256",
        "metadata_profile",
        "domains",
        "systems",
        "classification",
        "source_filename",
        "source_format",
    ] {
        assert!(
            !fields.contains_key(forbidden),
            "forbidden key `{forbidden}` must be omitted from glassbox:v1 frontmatter"
        );
    }

    assert_eq!(
        body,
        concat!(
            "<!-- bodymatter:v1 <time start=\"00:00:00.000\" end=\"00:00:30.000\"/> -->\n\n",
            "Welcome everyone.\n\n",
            "<!-- bodymatter:v1 <time start=\"00:15:00.000\" end=\"00:15:30.000\"/> -->\n\n",
            "Turning to adaptation finance.\n",
        )
    );

    // Sparse report without embedded title, language, contact sheet, or audio extract falls back to filename stem.
    let sparse = build_transcript_report(
        "/clips/standup_2026_11_26.mkv",
        None,
        None,
        None,
        None,
        &[(360_000_000, 360_030_000, "Over 100 hours mark.")],
    );
    let sparse_md = render_transcript_markdown(
        &sparse,
        &TranscriptRenderOptions {
            include_markers: true,
            include_frontmatter: true,
            id_override: Some("01M8AKN6A2ZKVQGEWRJMHVH0S8".to_owned()),
            updated_at_override: Some(String::new()),
        },
    );
    let (sparse_fields, sparse_body) = parse_frontmatter(&sparse_md);
    assert_eq!(sparse_fields["title"], "standup_2026_11_26");
    assert!(!sparse_fields.contains_key("language"));
    assert!(!sparse_fields.contains_key("updated_at"));
    assert!(sparse_fields["extraction"].get("contact_sheet").is_none());
    assert!(sparse_fields["extraction"].get("audio_extract").is_none());
    assert_eq!(
        sparse_body,
        concat!(
            "<!-- bodymatter:v1 <time start=\"100:00:00.000\" end=\"100:00:30.000\"/> -->\n\n",
            "Over 100 hours mark.\n",
        )
    );
}

/// Verifies hostile characters (`:`, `"`, `---`, `<script>`, forged comments, `U+2028`)
/// are safely JSON-encoded in frontmatter and neutralized in transcript bodies (`V-MED-01`).
///
/// Accepts no parameters. Returns unit after checking lossless frontmatter
/// round-trip and body HTML/comment neutralization. Panics on YAML fence break
/// or unescaped HTML/forged marker survival.
#[test]
fn hostile_metadata_and_body_payloads_never_inject_yaml_or_html() {
    let hostile_title = "Q4: \"Shock\" --- <script>alert(1)</script>\n---\nfake: true\u{2028}sep";
    let hostile_path = "/tmp/a: \"b\" --- <script>/clip.mp4";
    let hostile_body = concat!(
        "---\n",
        "# Forged Heading\n",
        "<script>alert('xss')</script>\n",
        "<!-- bodymatter:v1 <time start=\"99:00:00.000\" end=\"99:00:01.000\"/> -->",
    );

    let report = build_transcript_report(
        hostile_path,
        Some(hostile_title),
        Some("en: \"US\""),
        None,
        None,
        &[(0, 12_500, hostile_body)],
    );

    let rendered = render_transcript_markdown(
        &report,
        &TranscriptRenderOptions {
            include_markers: true,
            include_frontmatter: true,
            id_override: Some("01M8AKN6A2ZKVQGEWRJMHVH0S9".to_owned()),
            updated_at_override: Some("2026-11-26T01:35:00Z".to_owned()),
        },
    );

    let (fields, body) = parse_frontmatter(&rendered);
    assert_eq!(fields["title"], hostile_title);
    assert_eq!(fields["source_url"], hostile_path);
    assert_eq!(fields["language"], "en: \"US\"");
    assert!(!fields.contains_key("fake"));

    // Body retains only the genuine <time> marker and neutralizes forged markers, HTML, and block triggers.
    assert!(body.starts_with(
        "<!-- bodymatter:v1 <time start=\"00:00:00.000\" end=\"00:00:12.500\"/> -->\n\n"
    ));
    let prose = body
        .strip_prefix(
            "<!-- bodymatter:v1 <time start=\"00:00:00.000\" end=\"00:00:12.500\"/> -->\n\n",
        )
        .unwrap();
    assert!(!prose.contains("<script"));
    assert!(!prose.contains("<!--"));
    assert!(prose.contains("\\---\n\\# Forged Heading\n"));
    assert!(prose.contains("&lt;script>alert('xss')&lt;/script>"));
    assert!(prose.contains("&lt;!-- bodymatter:v1 &lt;time"));
}

/// Verifies independent `--no-markers` and `--no-frontmatter` suppression through `selection::render_markdown_full` (`V-MED-01`).
///
/// Accepts no parameters. Returns unit after checking all four `(include_markers,
/// include_frontmatter)` combinations and `render_markdown_with_options` parity.
/// Panics if either control leaks into the other.
#[test]
fn render_markdown_full_suppresses_markers_and_frontmatter_independently() {
    let report = build_transcript_report(
        "/media/interview.mp3",
        Some("Interview"),
        Some("en"),
        None,
        None,
        &[
            (0, 15_000, "First answer."),
            (15_000, 30_000, "Second answer."),
        ],
    );

    let full = render_markdown_full(&report, true, true);
    let (fields, body) = parse_frontmatter(&full);
    assert_eq!(fields["schema"], GLASSBOX_TRANSCRIPT_SCHEMA);
    assert!(is_valid_ulid(fields["id"].as_str().unwrap()));
    assert!(
        body.contains("<!-- bodymatter:v1 <time start=\"00:00:00.000\" end=\"00:00:15.000\"/> -->")
    );
    assert!(
        body.contains("<!-- bodymatter:v1 <time start=\"00:00:15.000\" end=\"00:00:30.000\"/> -->")
    );

    let no_markers = render_markdown_full(&report, false, true);
    let (nm_fields, nm_body) = parse_frontmatter(&no_markers);
    assert_eq!(nm_fields["schema"], GLASSBOX_TRANSCRIPT_SCHEMA);
    assert!(!no_markers.contains("bodymatter:v1"));
    assert_eq!(nm_body, "First answer.\n\nSecond answer.\n");

    // `render_markdown_with_options(report, false)` delegates to `render_markdown_full(report, false, true)`.
    let via_options = render_markdown_with_options(&report, false);
    let (opt_fields, opt_body) = parse_frontmatter(&via_options);
    assert_eq!(opt_fields["schema"], GLASSBOX_TRANSCRIPT_SCHEMA);
    assert_eq!(opt_body, nm_body);

    let no_frontmatter = render_markdown_full(&report, true, false);
    assert!(!no_frontmatter.starts_with("---"));
    assert_eq!(
        no_frontmatter,
        concat!(
            "<!-- bodymatter:v1 <time start=\"00:00:00.000\" end=\"00:00:15.000\"/> -->\n\n",
            "First answer.\n\n",
            "<!-- bodymatter:v1 <time start=\"00:00:15.000\" end=\"00:00:30.000\"/> -->\n\n",
            "Second answer.\n",
        )
    );

    let neither = render_markdown_full(&report, false, false);
    assert!(!neither.starts_with("---"));
    assert!(!neither.contains("bodymatter:v1"));
    assert_eq!(neither, "First answer.\n\nSecond answer.\n");
}

/// Verifies non-media v3 JSON and document Markdown stay byte-identical and media v3 receipts round-trip (`V-MED-01`).
///
/// Accepts no parameters. Returns unit after checking a real checked-in text
/// fixture (`extensionalities/fixtures/hello.txt`), a contact-sheet-only report,
/// and a full combined media report serialized through `ExtractReport::to_v3`.
/// Panics if non-media reports leak `media`/`start_ms`/`end_ms`/frontmatter or
/// if media v3 round-trip loses any field.
#[test]
fn non_media_v3_and_markdown_are_unchanged_while_media_receipts_round_trip() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/hello.txt");
    let doc_report = extract(
        &fixture,
        &ExtractOptions {
            selection: NativeSelection::All,
            ocr: OcrOptions {
                enabled: false,
                ..OcrOptions::default()
            },
            ..ExtractOptions::default()
        },
    );
    assert!(doc_report.media.is_none());

    // Non-media v3 JSON must omit `media` and every unit's `start_ms` / `end_ms`.
    let doc_v3 = doc_report.to_v3(false);
    let doc_json = serde_json::to_value(&doc_v3).unwrap();
    assert!(
        doc_json.get("media").is_none(),
        "non-media ExtractReportV3 must omit `media`: {doc_json}"
    );
    for unit in doc_json["content"]["units"].as_array().unwrap() {
        assert!(unit.get("start_ms").is_none());
        assert!(unit.get("end_ms").is_none());
    }

    // Non-media document Markdown must stay frontmatter-free across every render entrypoint.
    let md_default = render_markdown(&doc_report);
    let md_with_fm_flag = render_markdown_full(&doc_report, true, true);
    let md_without_fm_flag = render_markdown_full(&doc_report, true, false);
    assert!(!md_default.starts_with("---"));
    assert_eq!(md_default, md_with_fm_flag);
    assert_eq!(md_default, md_without_fm_flag);

    // Contact-sheet-only report (no ASR) stays frontmatter-free in Markdown while keeping v3 `media`.
    let sheet_only = ExtractReport {
        status: OperationStatus::Complete,
        completeness: Completeness::Exact,
        media: Some(MediaReceipt {
            contact_sheet: Some(ContactSheetReceipt {
                output: PathBuf::from("/out/demo-contact-sheet.png"),
                duration_seconds: 120.0,
                columns: 5,
                rows: 5,
                tiles: 25,
                mode: "default".to_owned(),
                preset: "performance".to_owned(),
                format: "png".to_owned(),
                width_px: 1_280,
                height_px: 750,
                cell_width_px: 240,
                cell_height_px: 134,
                seek_seconds: vec![2.4, 7.2],
                reused_tiles: 0,
                linear_seek_tiles: 0,
                every_seconds: Some(5.0),
            }),
            asr: None,
            audio_extract: None,
        }),
        ..ExtractReport::default()
    };
    assert_eq!(render_markdown_full(&sheet_only, true, true), "");
    let sheet_v3 = sheet_only.to_v3(false);
    let sheet_json = serde_json::to_value(&sheet_v3).unwrap();
    assert_eq!(sheet_json["media"]["contact_sheet"]["format"], "png");
    assert!(sheet_json["media"].get("asr").is_none());
    assert!(sheet_json["media"].get("audio_extract").is_none());

    // Full combined media report round-trips losslessly through ExtractReportV3.
    let mut media_report = build_transcript_report(
        "/videos/board_meeting.mp4",
        Some("Board Meeting"),
        Some("en"),
        Some("/out/board_meeting-contact-sheet.jpg"),
        Some("/out/board_meeting-extract.wav"),
        &[(0, 30_000, "Opening remarks.")],
    );
    media_report.metadata.artifacts.push(Artifact {
        kind: ArtifactKind::ContactSheet,
        path: PathBuf::from("/out/board_meeting-contact-sheet.jpg"),
        source: FactSource::GeneratedArtifact,
        completeness: Completeness::Exact,
        width: Some(1_920),
        height: Some(678),
    });
    media_report.metadata.artifacts.push(Artifact {
        kind: ArtifactKind::AudioExtract,
        path: PathBuf::from("/out/board_meeting-extract.wav"),
        source: FactSource::GeneratedArtifact,
        completeness: Completeness::Exact,
        width: None,
        height: None,
    });
    media_report.diagnostics.push(Diagnostic {
        stage: DiagnosticStage::Media,
        severity: DiagnosticSeverity::Warning,
        code: "linear_seek_fallback".to_owned(),
        message: "used linear seek for 1 tile".to_owned(),
        unit_index: None,
    });

    let v3 = media_report.to_v3(false);
    let serialized = serde_json::to_string(&v3).unwrap();
    let decoded: ExtractReportV3 = serde_json::from_str(&serialized).unwrap();
    assert_eq!(decoded, v3);
    let value: Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(value["content"]["units"][0]["unit"], "time_interval");
    assert_eq!(value["content"]["units"][0]["start_ms"], 0);
    assert_eq!(value["content"]["units"][0]["end_ms"], 30_000);
    assert_eq!(value["artifacts"][0]["kind"], "contact_sheet");
    assert_eq!(value["artifacts"][1]["kind"], "audio_extract");
    assert_eq!(value["diagnostics"][0]["stage"], "media");
    assert_eq!(value["media"]["asr"]["backend"], "openai");
    assert_eq!(value["media"]["asr"]["windows_attempted"], 1);
    assert_eq!(value["media"]["asr"]["windows_succeeded"], 1);
    assert_eq!(value["media"]["audio_extract"]["format"], "wav");
    assert_eq!(value["media"]["contact_sheet"]["tiles"], 15);
}
