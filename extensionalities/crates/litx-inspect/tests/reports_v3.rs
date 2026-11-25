//! Module: glassbox-inspect schema v3 projection and format classification tests.
//! File: reports_v3.rs.
//! Responsibility: Verify the 23-family `format.type` table, literal filename suffix preservation, mismatch rules, RFC 3339 date normalization, envelope omission rules, readiness mappings, ExtractReportV3 projection, and v3 text/Markdown summaries.

use file_format::{FileFormat, Kind};
use glassbox_inspect::hashing::{HashPolicy, SAMPLED_FINGERPRINT_SCHEME};
use glassbox_inspect::metadata::{summary, summary_markdown, summary_text};
use glassbox_inspect::reports::{
    Completeness, Diagnostic, DiagnosticSeverity, DiagnosticStage, DocumentMetadata,
    EmbeddedMetadataField, EncryptionStatus, ExtractOptions, FactSource, InspectOptions,
    MetadataProvenance, MetadataReport, NativePageSignal, NativeUnit, OcrRecommendation,
    OperationStatus, PdfDocumentSignals, QualifiedCount, RasterDimensions, SampledFingerprint,
    SchemaIdentifier, SchemaKind, SourcedMetadata, SupportStatus, TextLayerStatus,
};
use glassbox_inspect::reports_v3::{
    ALL_FORMAT_TYPES, ExtractReportV3, InspectReportV3, REPORT_SCHEMA_VERSION_V3, extract_to_v3,
    format_type_for_extension, format_type_for_file_format, inspect_to_v3, normalize_extension,
    normalize_metadata_date,
};
use glassbox_inspect::{Inspector, extract, inspect};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Returns the checked-in repository fixture directory.
///
/// Accepts no parameters. Returns the absolute path to `extensionalities/fixtures`.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// Creates a unique isolated scratch directory inside `extensionalities/target/settling-in-tests`.
///
/// Accepts a short label. Returns the created directory path. Creation failures panic.
fn scratch(label: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/settling-in-tests")
        .join(format!(
            "reports-v3-{label}-{}-{unique}",
            std::process::id()
        ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Writes one synthetic fixture file under a test-owned scratch directory.
///
/// Accepts directory, filename, and bytes. Returns the written file path. Panics on I/O failure.
fn write_fixture(directory: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Verifies all 23 canonical `format.type` identifiers and every documented suffix mapping.
///
/// Accepts no parameters. Panics if any suffix from `design.md §3.3` or any `FileFormat`
/// variant maps outside the exact 23 lowercase identifiers.
#[test]
fn all_23_format_types_and_every_file_format_variant_map_cleanly() {
    let expected_types: BTreeSet<&str> = [
        "pdf",
        "office_docs",
        "presentations",
        "spreadsheets",
        "text",
        "tabular_data",
        "structured_data",
        "images",
        "vector_graphics",
        "ebooks",
        "comics",
        "audio",
        "video",
        "archives",
        "source_code",
        "web",
        "databases",
        "personal_info",
        "subtitles",
        "fonts",
        "3d_cad",
        "applications",
        "other",
    ]
    .into_iter()
    .collect();
    assert_eq!(ALL_FORMAT_TYPES.len(), 23);
    let actual_types: BTreeSet<&str> = ALL_FORMAT_TYPES.iter().copied().collect();
    assert_eq!(actual_types, expected_types);

    let extension_cases = [
        ("pdf", "pdf"),
        ("doc", "office_docs"),
        ("docx", "office_docs"),
        ("odt", "office_docs"),
        ("rtf", "office_docs"),
        ("pages", "office_docs"),
        ("ppt", "presentations"),
        ("pptx", "presentations"),
        ("odp", "presentations"),
        ("key", "presentations"),
        ("xls", "spreadsheets"),
        ("xlsx", "spreadsheets"),
        ("xlsm", "spreadsheets"),
        ("ods", "spreadsheets"),
        ("numbers", "spreadsheets"),
        ("txt", "text"),
        ("md", "text"),
        ("rst", "text"),
        ("tex", "text"),
        ("csv", "tabular_data"),
        ("tsv", "tabular_data"),
        ("parquet", "tabular_data"),
        ("arrow", "tabular_data"),
        ("json", "structured_data"),
        ("jsonl", "structured_data"),
        ("xml", "structured_data"),
        ("yaml", "structured_data"),
        ("yml", "structured_data"),
        ("toml", "structured_data"),
        ("jpg", "images"),
        ("jpeg", "images"),
        ("png", "images"),
        ("gif", "images"),
        ("webp", "images"),
        ("heic", "images"),
        ("avif", "images"),
        ("tif", "images"),
        ("tiff", "images"),
        ("bmp", "images"),
        ("svg", "vector_graphics"),
        ("eps", "vector_graphics"),
        ("epub", "ebooks"),
        ("mobi", "ebooks"),
        ("azw", "ebooks"),
        ("azw3", "ebooks"),
        ("prc", "ebooks"),
        ("djvu", "ebooks"),
        ("djv", "ebooks"),
        ("cbz", "comics"),
        ("cbr", "comics"),
        ("mp3", "audio"),
        ("m4a", "audio"),
        ("aac", "audio"),
        ("flac", "audio"),
        ("wav", "audio"),
        ("ogg", "audio"),
        ("opus", "audio"),
        ("aiff", "audio"),
        ("mp4", "video"),
        ("mkv", "video"),
        ("mov", "video"),
        ("avi", "video"),
        ("webm", "video"),
        ("m4v", "video"),
        ("mpeg", "video"),
        ("tar.gz", "archives"),
        ("tar.bz2", "archives"),
        ("tar.xz", "archives"),
        ("tar.zst", "archives"),
        ("zip", "archives"),
        ("rar", "archives"),
        ("7z", "archives"),
        ("tar", "archives"),
        ("gz", "archives"),
        ("bz2", "archives"),
        ("xz", "archives"),
        ("zst", "archives"),
        ("py", "source_code"),
        ("go", "source_code"),
        ("js", "source_code"),
        ("ts", "source_code"),
        ("jsx", "source_code"),
        ("tsx", "source_code"),
        ("java", "source_code"),
        ("c", "source_code"),
        ("cpp", "source_code"),
        ("rs", "source_code"),
        ("sh", "source_code"),
        ("sql", "source_code"),
        ("html", "web"),
        ("htm", "web"),
        ("css", "web"),
        ("mhtml", "web"),
        ("sqlite", "databases"),
        ("sqlite3", "databases"),
        ("db", "databases"),
        ("mdb", "databases"),
        ("accdb", "databases"),
        ("eml", "personal_info"),
        ("msg", "personal_info"),
        ("mbox", "personal_info"),
        ("ics", "personal_info"),
        ("vcf", "personal_info"),
        ("srt", "subtitles"),
        ("vtt", "subtitles"),
        ("ass", "subtitles"),
        ("ttf", "fonts"),
        ("otf", "fonts"),
        ("woff", "fonts"),
        ("woff2", "fonts"),
        ("stl", "3d_cad"),
        ("obj", "3d_cad"),
        ("gltf", "3d_cad"),
        ("glb", "3d_cad"),
        ("step", "3d_cad"),
        ("stp", "3d_cad"),
        ("dwg", "3d_cad"),
        ("dxf", "3d_cad"),
        ("exe", "applications"),
        ("msi", "applications"),
        ("apk", "applications"),
        ("deb", "applications"),
        ("rpm", "applications"),
        ("appimage", "applications"),
        ("unknownext", "other"),
        ("", "other"),
    ];
    let mut covered_types = BTreeSet::new();
    for (ext, expected) in extension_cases {
        let actual = format_type_for_extension(ext);
        assert_eq!(actual, expected, "extension `{ext}` mapped to `{actual}`");
        covered_types.insert(actual);
    }
    assert_eq!(covered_types, expected_types);

    // Exhaustively check every FileFormat variant across every Kind.
    let all_kinds = [
        Kind::Archive,
        Kind::Audio,
        Kind::Compressed,
        Kind::Database,
        Kind::Diagram,
        Kind::Disk,
        Kind::Document,
        Kind::Ebook,
        Kind::Executable,
        Kind::Font,
        Kind::Formula,
        Kind::Geospatial,
        Kind::Image,
        Kind::Metadata,
        Kind::Model,
        Kind::Other,
        Kind::Package,
        Kind::Playlist,
        Kind::Presentation,
        Kind::Rom,
        Kind::Spreadsheet,
        Kind::Subtitle,
        Kind::Video,
    ];
    let mut variant_count = 0usize;
    for kind in all_kinds {
        for format in FileFormat::from_kind(kind) {
            variant_count += 1;
            let mapped = format_type_for_file_format(*format);
            assert!(
                expected_types.contains(mapped),
                "FileFormat::{format:?} mapped to invalid format.type `{mapped}`"
            );
        }
    }
    assert!(variant_count > 100, "expected full FileFormat enum sweep");

    // Verify collision disambiguations.
    assert_eq!(format_type_for_file_format(FileFormat::Djvu), "ebooks");
    assert_eq!(
        format_type_for_file_format(FileFormat::Mpeg2TransportStream),
        "video"
    );
    assert_eq!(
        format_type_for_file_format(FileFormat::MegaDriveRom),
        "other"
    );
    assert_eq!(
        format_type_for_file_format(FileFormat::PemPrivateKey),
        "other"
    );
    assert_eq!(
        format_type_for_file_format(FileFormat::ScalableVectorGraphics),
        "vector_graphics"
    );
    assert_eq!(
        format_type_for_file_format(FileFormat::EncapsulatedPostscript),
        "vector_graphics"
    );
    assert_eq!(
        format_type_for_file_format(FileFormat::HypertextMarkupLanguage),
        "web"
    );
    assert_eq!(
        format_type_for_file_format(FileFormat::PortableDocumentFormat),
        "pdf"
    );
}

/// Verifies longest compound suffix normalization and extensionless filename handling.
///
/// Accepts no parameters. Panics if compound archive suffixes, dotfiles, or
/// extensionless files emit wrong suffixes in v3 JSON.
#[test]
fn compound_and_extensionless_suffix_normalization() {
    assert_eq!(
        normalize_extension(Path::new("/in/backup.tar.gz")).as_deref(),
        Some("tar.gz")
    );
    assert_eq!(
        normalize_extension(Path::new("/in/backup.TAR.BZ2")).as_deref(),
        Some("tar.bz2")
    );
    assert_eq!(
        normalize_extension(Path::new("/in/backup.tar.xz")).as_deref(),
        Some("tar.xz")
    );
    assert_eq!(
        normalize_extension(Path::new("/in/backup.tar.zst")).as_deref(),
        Some("tar.zst")
    );
    assert_eq!(normalize_extension(Path::new("/in/README")), None);
    assert_eq!(normalize_extension(Path::new("/in/.bashrc")), None);
    assert_eq!(
        normalize_extension(Path::new("/in/.tar.gz")).as_deref(),
        Some("gz")
    );
    assert_eq!(normalize_extension(Path::new("/in/trailing.")), None);
    assert_eq!(
        normalize_extension(Path::new("/in/.config.JSON")).as_deref(),
        Some("json")
    );

    let dir = scratch("compound");
    // Minimal gzip header bytes.
    let tar_gz = write_fixture(
        &dir,
        "archive.tar.gz",
        &[0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03],
    );
    let report = inspect(&tar_gz, &InspectOptions::default());
    let v3 = report.to_v3(false);
    assert_eq!(v3.format.format_type, "archives");
    assert_eq!(v3.format.extension.as_deref(), Some("tar.gz"));
    assert_eq!(v3.format.extension_mismatch, None);
    let json = serde_json::to_value(&v3).unwrap();
    assert_eq!(json["format"]["type"], "archives");
    assert_eq!(json["format"]["extension"], "tar.gz");
    assert!(json["format"].get("extension_mismatch").is_none());

    // Extensionless file omits `format.extension`.
    let readme = write_fixture(&dir, "README", b"plain extensionless text\n");
    let v3_readme = inspect(&readme, &InspectOptions::default()).to_v3(false);
    assert_eq!(v3_readme.format.format_type, "other");
    assert_eq!(v3_readme.format.extension, None);
    assert_eq!(v3_readme.format.extension_mismatch, None);
    let json_readme = serde_json::to_value(&v3_readme).unwrap();
    assert!(json_readme["format"].get("extension").is_none());
    assert!(json_readme["format"].get("extension_mismatch").is_none());

    std::fs::remove_dir_all(&dir).ok();
}

/// Verifies misnamed files preserve literal filename suffixes and flag `extension_mismatch: true` while aliases do not false-flag.
///
/// Accepts no parameters. Panics if misnamed HTML/DOCX files named `report.pdf`
/// lose their `"pdf"` suffix or fail to flag mismatch, or if valid family aliases false-flag.
#[test]
fn misnamed_files_flag_mismatch_and_aliases_never_false_flag() {
    let dir = scratch("mismatch");

    // 1. HTML bytes named `report.pdf` -> type: "web", extension: "pdf", extension_mismatch: true.
    let html_as_pdf = write_fixture(
        &dir,
        "report.pdf",
        b"<!DOCTYPE html><html><head><title>Login</title></head><body>Hi</body></html>",
    );
    let html_report = inspect(&html_as_pdf, &InspectOptions::default());
    assert_ne!(html_report.metadata.status, OperationStatus::Failed);
    let html_v3 = html_report.to_v3(false);
    assert_eq!(html_v3.format.format_type, "web");
    assert_eq!(html_v3.format.mime.as_deref(), Some("text/html"));
    assert_eq!(html_v3.format.extension.as_deref(), Some("pdf"));
    assert_eq!(html_v3.format.extension_mismatch, Some(true));
    let html_json = serde_json::to_value(&html_v3).unwrap();
    assert_eq!(html_json["format"]["type"], "web");
    assert_eq!(html_json["format"]["extension"], "pdf");
    assert_eq!(html_json["format"]["extension_mismatch"], true);

    // 2. DOCX bytes named `contract.pdf` -> type: "office_docs", extension: "pdf", extension_mismatch: true.
    let docx_bytes = std::fs::read(fixtures().join("notes.docx")).unwrap();
    let docx_as_pdf = write_fixture(&dir, "contract.pdf", &docx_bytes);
    let docx_report = inspect(&docx_as_pdf, &InspectOptions::default());
    assert_eq!(docx_report.metadata.status, OperationStatus::Complete);
    let docx_v3 = docx_report.to_v3(false);
    assert_eq!(docx_v3.format.format_type, "office_docs");
    assert_eq!(docx_v3.format.extension.as_deref(), Some("pdf"));
    assert_eq!(docx_v3.format.extension_mismatch, Some(true));

    // 2b. RAR magic bytes and full RAR archive bytes named `report.pdf` must preserve
    //     `type: "archives"`, `extension: "pdf"`, and `extension_mismatch: true`.
    let rar_magic_dir = dir.join("rar-magic");
    std::fs::create_dir_all(&rar_magic_dir).unwrap();
    let rar_magic_as_pdf = write_fixture(&rar_magic_dir, "report.pdf", b"Rar!\x1a\x07\x00");
    let rar_magic_v3 = inspect(&rar_magic_as_pdf, &InspectOptions::default()).to_v3(false);
    assert_eq!(rar_magic_v3.format.format_type, "archives");
    assert_eq!(rar_magic_v3.format.extension.as_deref(), Some("pdf"));
    assert_eq!(rar_magic_v3.format.extension_mismatch, Some(true));
    let rar_magic_json = serde_json::to_value(&rar_magic_v3).unwrap();
    assert_eq!(rar_magic_json["format"]["type"], "archives");
    assert_eq!(rar_magic_json["format"]["extension"], "pdf");
    assert_eq!(rar_magic_json["format"]["extension_mismatch"], true);

    let rar_archive_dir = dir.join("rar-archive");
    std::fs::create_dir_all(&rar_archive_dir).unwrap();
    let rar_bytes = std::fs::read(fixtures().join("pages5.cbr")).unwrap();
    let rar_archive_as_pdf = write_fixture(&rar_archive_dir, "report.pdf", &rar_bytes);
    let rar_archive_report = inspect(&rar_archive_as_pdf, &InspectOptions::default());
    assert_eq!(
        rar_archive_report.metadata.status,
        OperationStatus::Complete
    );
    let rar_archive_v3 = rar_archive_report.to_v3(false);
    assert_eq!(rar_archive_v3.format.format_type, "archives");
    assert_eq!(rar_archive_v3.format.extension.as_deref(), Some("pdf"));
    assert_eq!(rar_archive_v3.format.extension_mismatch, Some(true));

    // Genuine `.cbr` RAR comic book must NOT false-flag `extension_mismatch`.
    let comic_cbr = write_fixture(&dir, "comic.cbr", &rar_bytes);
    let comic_v3 = inspect(&comic_cbr, &InspectOptions::default()).to_v3(false);
    assert_eq!(comic_v3.format.format_type, "comics");
    assert_eq!(comic_v3.format.extension.as_deref(), Some("cbr"));
    assert_eq!(comic_v3.format.extension_mismatch, None);

    // 3. Family aliases must NEVER false-flag `extension_mismatch`.
    let jpeg_bytes = [
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00,
    ];
    let photo_jpeg = write_fixture(&dir, "photo.jpeg", &jpeg_bytes);
    let photo_v3 = inspect(&photo_jpeg, &InspectOptions::default()).to_v3(false);
    assert_eq!(photo_v3.format.format_type, "images");
    assert_eq!(photo_v3.format.extension.as_deref(), Some("jpeg"));
    assert_eq!(photo_v3.format.extension_mismatch, None);

    let mobi_bytes = std::fs::read(fixtures().join("native.mobi")).unwrap();
    for ext in ["azw3", "prc"] {
        let alias = write_fixture(&dir, &format!("ebook.{ext}"), &mobi_bytes);
        let alias_v3 = inspect(&alias, &InspectOptions::default()).to_v3(false);
        assert_eq!(alias_v3.format.format_type, "ebooks");
        assert_eq!(alias_v3.format.extension.as_deref(), Some(ext));
        assert_eq!(alias_v3.format.extension_mismatch, None);
    }

    let djvu_bytes = std::fs::read(fixtures().join("single.djvu")).unwrap();
    let djv = write_fixture(&dir, "scan.djv", &djvu_bytes);
    let djv_v3 = inspect(&djv, &InspectOptions::default()).to_v3(false);
    assert_eq!(djv_v3.format.format_type, "ebooks");
    assert_eq!(djv_v3.format.extension.as_deref(), Some("djv"));
    assert_eq!(djv_v3.format.extension_mismatch, None);

    let xlsx_bytes = std::fs::read(fixtures().join("grid.xlsx")).unwrap();
    let xlsm = write_fixture(&dir, "workbook.xlsm", &xlsx_bytes);
    let xlsm_v3 = inspect(&xlsm, &InspectOptions::default()).to_v3(false);
    assert_eq!(xlsm_v3.format.format_type, "spreadsheets");
    assert_eq!(xlsm_v3.format.extension.as_deref(), Some("xlsm"));
    assert_eq!(xlsm_v3.format.extension_mismatch, None);

    std::fs::remove_dir_all(&dir).ok();
}

/// Verifies zero-dependency RFC 3339 date normalization, date-only retention, and `date_unnormalized` diagnostics.
///
/// Accepts no parameters. Panics if valid PDF/ISO timestamps fail to normalize,
/// valid date-only strings emit diagnostics, or invalid dates fail to retain raw text + diagnostic.
#[test]
fn date_normalization_handles_pdf_iso_date_only_and_malformed_values() {
    // Valid PDF dates.
    assert_eq!(
        normalize_metadata_date("D:20240229143000+02'00'").as_deref(),
        Some("2024-02-29T14:30:00+02:00")
    );
    assert_eq!(
        normalize_metadata_date("D:20240229143000-05'00").as_deref(),
        Some("2024-02-29T14:30:00-05:00")
    );
    assert_eq!(
        normalize_metadata_date("D:20240229143000Z").as_deref(),
        Some("2024-02-29T14:30:00Z")
    );
    assert_eq!(
        normalize_metadata_date("D:20240229143000+02").as_deref(),
        Some("2024-02-29T14:30:00+02:00")
    );
    assert_eq!(
        normalize_metadata_date("D:20240229").as_deref(),
        Some("2024-02-29")
    );
    assert_eq!(
        normalize_metadata_date("20240229143000Z").as_deref(),
        Some("2024-02-29T14:30:00Z")
    );

    // Valid ISO-8601 / W3CDTF timestamps.
    assert_eq!(
        normalize_metadata_date("2023-11-05T09:15:00Z").as_deref(),
        Some("2023-11-05T09:15:00Z")
    );
    assert_eq!(
        normalize_metadata_date("2023-11-05 09:15:00+0100").as_deref(),
        Some("2023-11-05T09:15:00+01:00")
    );
    assert_eq!(
        normalize_metadata_date("2023-11-05T09:15:00.250-04:00").as_deref(),
        Some("2023-11-05T09:15:00.250-04:00")
    );
    assert_eq!(
        normalize_metadata_date("2023-11-05T09:15:00").as_deref(),
        Some("2023-11-05T09:15:00Z")
    );

    // Valid date-only strings kept verbatim.
    assert_eq!(
        normalize_metadata_date("2023-11-05").as_deref(),
        Some("2023-11-05")
    );
    assert_eq!(
        normalize_metadata_date("2023-11").as_deref(),
        Some("2023-11")
    );
    assert_eq!(normalize_metadata_date("2023").as_deref(), Some("2023"));
    assert_eq!(
        normalize_metadata_date("2024-02-29").as_deref(),
        Some("2024-02-29")
    );

    // Malformed / invalid calendar bounds / non-ASCII UTF-8 / fractional minutes without seconds.
    for bad in [
        "2023-02-30",
        "2023-02-29",
        "2023-13",
        "not-a-date",
        "D:20231399",
        "2023-11-05T25:00:00Z",
        "2023-11-05T09:15:00+25:00",
        "2024-01-01T12:34.5Z",
        "2023-11-05T09:€",
        "2023-11-05T09:15:€",
        "D:20240229143000+😀",
        "2024-01-01T12:00+😀",
    ] {
        assert_eq!(
            normalize_metadata_date(bad),
            None,
            "expected invalid date `{bad}` to fail"
        );
    }

    // End-to-end projection: valid date-only emits 0 diagnostics; malformed retains raw + info diagnostic.
    let mut valid_report = MetadataReport::default();
    valid_report.metadata.status = OperationStatus::Complete;
    valid_report
        .metadata
        .embedded_metadata
        .push(SourcedMetadata {
            field: EmbeddedMetadataField::CreatedAt,
            value: "2023-11-05".into(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Exact,
        });
    valid_report
        .metadata
        .embedded_metadata
        .push(SourcedMetadata {
            field: EmbeddedMetadataField::ModifiedAt,
            value: "D:20240229143000+02'00'".into(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Exact,
        });
    let valid_v3 = inspect_to_v3(&valid_report, false);
    assert_eq!(
        valid_v3.description.created_at.as_deref(),
        Some("2023-11-05")
    );
    assert_eq!(
        valid_v3.description.modified_at.as_deref(),
        Some("2024-02-29T14:30:00+02:00")
    );
    assert!(valid_v3.diagnostics.is_empty());

    let mut invalid_report = MetadataReport::default();
    invalid_report.metadata.status = OperationStatus::Complete;
    invalid_report
        .metadata
        .embedded_metadata
        .push(SourcedMetadata {
            field: EmbeddedMetadataField::CreatedAt,
            value: "2023-02-30".into(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Exact,
        });
    invalid_report
        .metadata
        .embedded_metadata
        .push(SourcedMetadata {
            field: EmbeddedMetadataField::ModifiedAt,
            value: "not-a-date".into(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Exact,
        });
    let invalid_v3 = inspect_to_v3(&invalid_report, false);
    assert_eq!(
        invalid_v3.description.created_at.as_deref(),
        Some("2023-02-30")
    );
    assert_eq!(
        invalid_v3.description.modified_at.as_deref(),
        Some("not-a-date")
    );
    assert_eq!(invalid_v3.diagnostics.len(), 2);
    assert!(invalid_v3.diagnostics.iter().all(|d| {
        d.stage == DiagnosticStage::Metadata
            && d.severity == DiagnosticSeverity::Info
            && d.code == "date_unnormalized"
    }));

    // Non-ASCII embedded dates retain verbatim raw strings and emit info diagnostics without panicking.
    for (created_raw, modified_raw) in [
        ("2023-11-05T09:€", "2023-11-05T09:15:€"),
        ("D:20240229143000+😀", "2024-01-01T12:00+😀"),
    ] {
        let mut utf8_report = MetadataReport::default();
        utf8_report.metadata.status = OperationStatus::Complete;
        utf8_report
            .metadata
            .embedded_metadata
            .push(SourcedMetadata {
                field: EmbeddedMetadataField::CreatedAt,
                value: created_raw.into(),
                source: FactSource::EmbeddedProperty,
                completeness: Completeness::Exact,
            });
        utf8_report
            .metadata
            .embedded_metadata
            .push(SourcedMetadata {
                field: EmbeddedMetadataField::ModifiedAt,
                value: modified_raw.into(),
                source: FactSource::EmbeddedProperty,
                completeness: Completeness::Exact,
            });
        let utf8_v3 = inspect_to_v3(&utf8_report, false);
        assert_eq!(utf8_v3.description.created_at.as_deref(), Some(created_raw));
        assert_eq!(
            utf8_v3.description.modified_at.as_deref(),
            Some(modified_raw)
        );
        assert_eq!(utf8_v3.diagnostics.len(), 2);
        assert!(utf8_v3.diagnostics.iter().all(|d| {
            d.stage == DiagnosticStage::Metadata
                && d.severity == DiagnosticSeverity::Info
                && d.code == "date_unnormalized"
        }));
    }
}

/// Verifies v3 envelope omission rules, software/author separation, keyed structure, and serde round-trip.
///
/// Accepts no parameters. Panics if forbidden legacy fields appear, empty blocks
/// fail to omit, or verbose provenance toggles fail.
#[test]
fn envelope_omission_rules_description_separation_and_round_trip() {
    let pdf_path = fixtures().join("native.pdf");
    let report = inspect(&pdf_path, &InspectOptions::default());

    let quiet = report.to_v3(false);
    let quiet_json = serde_json::to_value(&quiet).unwrap();
    assert_eq!(quiet_json["schema_version"], REPORT_SCHEMA_VERSION_V3);
    assert_eq!(quiet_json["kind"], "inspect");
    assert_eq!(quiet_json["status"], "complete");
    assert!(quiet_json.get("next_step").is_none());
    assert!(quiet_json.get("advice").is_none());
    assert!(quiet_json["file"].get("consistent").is_none());
    assert!(quiet_json["file"].get("sha256").is_some());
    assert!(quiet_json["file"].get("fingerprint").is_none());
    assert!(quiet_json.get("diagnostics").is_none());
    assert!(quiet_json.get("artifacts").is_none());
    assert!(quiet_json.get("provenance").is_none());
    assert_eq!(quiet_json["format"]["type"], "pdf");
    assert_eq!(quiet_json["format"]["extension"], "pdf");
    assert!(quiet_json["format"].get("extension_mismatch").is_none());
    assert!(quiet_json["structure"]["counts"]["pages"].as_u64().unwrap() >= 1);
    assert!(quiet_json["structure"]["pdf"].is_object());
    assert!(quiet_json["structure"].get("image").is_none());

    // Round-trip deserialize InspectReportV3.
    let decoded: InspectReportV3 = serde_json::from_value(quiet_json).unwrap();
    assert_eq!(decoded, quiet);

    // Verbose projection includes reader provenance.
    let verbose = report.to_v3(true);
    let verbose_json = serde_json::to_value(&verbose).unwrap();
    assert!(
        verbose_json["provenance"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );

    // Description keeps software separate from authors and omits software when absent.
    let mut synthetic = MetadataReport::default();
    synthetic.metadata.status = OperationStatus::Complete;
    synthetic.metadata.identity.path = PathBuf::from("/in/report.pdf");
    synthetic.metadata.identity.byte_size = Some(482_113);
    synthetic.metadata.format.bucket = "PDF".into();
    synthetic.metadata.format.extension = "pdf".into();
    synthetic.metadata.embedded_metadata = vec![
        SourcedMetadata {
            field: EmbeddedMetadataField::Title,
            value: "Annual Report".into(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Exact,
        },
        SourcedMetadata {
            field: EmbeddedMetadataField::Author,
            value: "J. Doe".into(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Exact,
        },
        SourcedMetadata {
            field: EmbeddedMetadataField::Author,
            value: "A. Smith".into(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Exact,
        },
        SourcedMetadata {
            field: EmbeddedMetadataField::Author,
            value: "J. Doe".into(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Exact,
        },
        SourcedMetadata {
            field: EmbeddedMetadataField::Creator,
            value: "LibreOffice Writer".into(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Exact,
        },
        SourcedMetadata {
            field: EmbeddedMetadataField::Producer,
            value: "Acrobat".into(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Exact,
        },
        SourcedMetadata {
            field: EmbeddedMetadataField::Application,
            value: "Writer 24".into(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Exact,
        },
        SourcedMetadata {
            field: EmbeddedMetadataField::Keywords,
            value: "climate, finance; risk, climate; finance".into(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Exact,
        },
    ];
    let projected = synthetic.to_v3(false);
    assert_eq!(
        projected.description.title.as_deref(),
        Some("Annual Report")
    );
    assert_eq!(projected.description.authors, vec!["J. Doe", "A. Smith"]);
    assert_eq!(
        projected.description.keywords,
        vec!["climate", "finance", "risk"]
    );
    let software = projected
        .description
        .software
        .as_ref()
        .expect("software block expected");
    assert_eq!(software.creator_tool.as_deref(), Some("LibreOffice Writer"));
    assert_eq!(software.producer.as_deref(), Some("Acrobat"));
    assert_eq!(software.application.as_deref(), Some("Writer 24"));
    let projected_json = serde_json::to_value(&projected).unwrap();
    assert_eq!(
        projected_json["description"]["software"]["creator_tool"],
        "LibreOffice Writer"
    );
    assert!(
        projected_json["description"]["software"]
            .get("creator")
            .is_none()
    );

    // Empty description serializes as `{}`.
    let empty_desc_v3 = MetadataReport::default().to_v3(false);
    let empty_desc_json = serde_json::to_value(&empty_desc_v3).unwrap();
    assert_eq!(
        empty_desc_json["description"],
        Value::Object(Default::default())
    );

    // Image geometry appears under `structure.image` for raster images.
    let img_v3 = inspect(&fixtures().join("image.png"), &InspectOptions::default()).to_v3(false);
    let img_json = serde_json::to_value(&img_v3).unwrap();
    assert_eq!(img_json["format"]["type"], "images");
    assert!(
        img_json["structure"]["image"]["width_pixels"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        img_json["structure"]["image"]["height_pixels"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(img_json["structure"].get("pdf").is_none());
}

/// Verifies every readiness branch (`extractable`, `encrypted`, `text_layer`, `ocr`) and keyed `structure`.
///
/// Accepts no parameters. Panics if any readiness mapping diverges from `spec.md`.
#[test]
fn readiness_mappings_and_structure_counts_match_spec() {
    let make = |bucket: &str,
                support: SupportStatus,
                encryption: EncryptionStatus,
                text_layer: TextLayerStatus,
                ocr: OcrRecommendation| {
        let mut report = MetadataReport::default();
        report.metadata.format.bucket = bucket.into();
        report.metadata.extraction_support = support;
        report.metadata.encryption = encryption;
        report.metadata.text_layer = text_layer;
        report.metadata.ocr_recommended = ocr;
        report.to_v3(false).readiness
    };

    // Archive: text_layer -> not_applicable, ocr Unknown -> not_needed.
    let archive = make(
        "Archives",
        SupportStatus::IdentificationOnly,
        EncryptionStatus::NotEncrypted,
        TextLayerStatus::Unknown,
        OcrRecommendation::Unknown,
    );
    assert_eq!(archive.extractable, "identification_only");
    assert_eq!(archive.encrypted, "none");
    assert_eq!(archive.text_layer, "not_applicable");
    assert_eq!(archive.ocr, "not_needed");

    // Scanned PDF: Absent text layer + Recommended OCR -> ocr: "required".
    let scanned = make(
        "PDF",
        SupportStatus::Supported,
        EncryptionStatus::EncryptedAccessible,
        TextLayerStatus::Absent,
        OcrRecommendation::Recommended,
    );
    assert_eq!(scanned.extractable, "supported");
    assert_eq!(scanned.encrypted, "accessible");
    assert_eq!(scanned.text_layer, "absent");
    assert_eq!(scanned.ocr, "required");

    // Mixed PDF: Mixed text layer + Recommended OCR -> ocr: "recommended".
    let mixed = make(
        "PDF",
        SupportStatus::Supported,
        EncryptionStatus::EncryptedInaccessible,
        TextLayerStatus::Mixed,
        OcrRecommendation::Recommended,
    );
    assert_eq!(mixed.encrypted, "locked");
    assert_eq!(mixed.text_layer, "mixed");
    assert_eq!(mixed.ocr, "recommended");

    // Clean native PDF: Present + NotRecommended -> ocr: "not_needed".
    let native = make(
        "PDF",
        SupportStatus::Supported,
        EncryptionStatus::NotEncrypted,
        TextLayerStatus::Present,
        OcrRecommendation::NotRecommended,
    );
    assert_eq!(native.text_layer, "present");
    assert_eq!(native.ocr, "not_needed");

    // Unknown other binary with zero signals -> not_applicable + not_needed.
    let other = make(
        "Other",
        SupportStatus::Unsupported,
        EncryptionStatus::Unknown,
        TextLayerStatus::Unknown,
        OcrRecommendation::Unknown,
    );
    assert_eq!(other.extractable, "unsupported");
    assert_eq!(other.encrypted, "unknown");
    assert_eq!(other.text_layer, "not_applicable");
    assert_eq!(other.ocr, "not_needed");

    // Verify all NativeUnit count keys & column count derivation.
    let mut structure_report = MetadataReport::default();
    structure_report.metadata.counts = vec![
        QualifiedCount {
            unit: NativeUnit::PhysicalPage,
            value: Some(12),
            source: FactSource::NativeStructure,
            completeness: Completeness::Exact,
        },
        QualifiedCount {
            unit: NativeUnit::Worksheet,
            value: Some(3),
            source: FactSource::NativeStructure,
            completeness: Completeness::Exact,
        },
        QualifiedCount {
            unit: NativeUnit::Slide,
            value: Some(5),
            source: FactSource::NativeStructure,
            completeness: Completeness::Exact,
        },
        QualifiedCount {
            unit: NativeUnit::Table,
            value: Some(2),
            source: FactSource::NativeStructure,
            completeness: Completeness::Exact,
        },
        QualifiedCount {
            unit: NativeUnit::SpineItem,
            value: Some(8),
            source: FactSource::ContainerIndex,
            completeness: Completeness::Exact,
        },
        QualifiedCount {
            unit: NativeUnit::DatasetRow,
            value: Some(42),
            source: FactSource::NativeStructure,
            completeness: Completeness::Exact,
        },
    ];
    structure_report.metadata.schema_identifiers = vec![
        SchemaIdentifier {
            kind: SchemaKind::Column,
            name: "id".into(),
            source: FactSource::NativeStructure,
        },
        SchemaIdentifier {
            kind: SchemaKind::Column,
            name: "amount".into(),
            source: FactSource::NativeStructure,
        },
        SchemaIdentifier {
            kind: SchemaKind::Worksheet,
            name: "Summary".into(),
            source: FactSource::NativeStructure,
        },
    ];
    let st = structure_report.to_v3(false).structure;
    assert_eq!(st.counts.get("pages"), Some(&12));
    assert_eq!(st.counts.get("sheets"), Some(&3));
    assert_eq!(st.counts.get("slides"), Some(&5));
    assert_eq!(st.counts.get("tables"), Some(&2));
    assert_eq!(st.counts.get("chapters"), Some(&8));
    assert_eq!(st.counts.get("rows"), Some(&42));
    assert_eq!(st.counts.get("columns"), Some(&2));
    let names = st.names.expect("structure names expected");
    assert_eq!(names.columns, vec!["id", "amount"]);
    assert_eq!(names.sheets, vec!["Summary"]);
}

/// Verifies `ExtractReportV3` projection, `Inspector::with_full_hash`, and v3 Markdown/text summaries.
///
/// Accepts no parameters. Panics if extract v3 schema, inspector hash policy plumbing,
/// or sampled/verbose summary rendering diverges from contract.
#[test]
fn extract_report_v3_inspector_hash_policy_and_v3_summaries() {
    // Inspector hash policy builder.
    let default_inspector = Inspector::default();
    assert_eq!(
        default_inspector.hash_policy(),
        HashPolicy::SampledAboveThreshold
    );
    let full_inspector = Inspector::default().with_full_hash(true);
    assert_eq!(full_inspector.hash_policy(), HashPolicy::Full);
    let back_to_sampled = full_inspector.with_full_hash(false);
    assert_eq!(
        back_to_sampled.hash_policy(),
        HashPolicy::SampledAboveThreshold
    );

    // ExtractReportV3 projection + round-trip.
    let extract_report = extract(&fixtures().join("hello.txt"), &ExtractOptions::default());
    let extract_v3 = extract_to_v3(&extract_report, false);
    assert_eq!(extract_v3.schema_version, REPORT_SCHEMA_VERSION_V3);
    assert_eq!(extract_v3.kind, "extract");
    assert_eq!(extract_v3.metadata.schema_version, REPORT_SCHEMA_VERSION_V3);
    assert_eq!(extract_v3.metadata.kind, "inspect");
    assert_eq!(extract_v3.metadata.format.format_type, "text");
    assert!(extract_v3.provenance.is_none());
    let extract_json = serde_json::to_value(&extract_v3).unwrap();
    assert!(extract_json.get("provenance").is_none());
    let decoded_extract: ExtractReportV3 = serde_json::from_value(extract_json).unwrap();
    assert_eq!(decoded_extract, extract_v3);

    let extract_v3_verbose = extract_report.to_v3(true);
    assert!(extract_v3_verbose.provenance.is_some());
    assert!(extract_v3_verbose.metadata.provenance.is_some());

    // v3 Markdown and plain-text summaries: sampled fingerprint, empty-section omission, verbose provenance.
    let mut sampled_report = MetadataReport {
        metadata: DocumentMetadata {
            status: OperationStatus::Complete,
            extraction_support: SupportStatus::Unsupported,
            encryption: EncryptionStatus::NotEncrypted,
            ..DocumentMetadata::default()
        },
        ..MetadataReport::default()
    };
    sampled_report.metadata.identity.path = PathBuf::from("/in/clip.mp4");
    sampled_report.metadata.identity.byte_size = Some(4_000_000_000);
    sampled_report.metadata.format.bucket = "Video".into();
    sampled_report.metadata.format.extension = "mp4".into();
    sampled_report.metadata.format.detected_mime = Some("video/mp4".into());
    sampled_report.metadata.digest.fingerprint = Some(SampledFingerprint {
        scheme: SAMPLED_FINGERPRINT_SCHEME.to_owned(),
        value: "a".repeat(64),
    });
    sampled_report.metadata.diagnostics.push(Diagnostic {
        stage: DiagnosticStage::Hashing,
        severity: DiagnosticSeverity::Info,
        code: "fingerprint_sampled".into(),
        message: "change detection used size and modification time only.".into(),
        unit_index: None,
    });
    sampled_report.metadata.provenance.push(MetadataProvenance {
        reader: "file-format".into(),
        stage: "prefix-signature".into(),
        source: FactSource::ByteScan,
    });

    let md_default = summary(&sampled_report);
    assert!(md_default.contains("- Schema: 3"));
    assert!(md_default.contains("- Format: video (video/mp4, .mp4)"));
    assert!(md_default.contains(&format!("- Fingerprint ({SAMPLED_FINGERPRINT_SCHEME}):")));
    assert!(!md_default.contains("SHA-256:"));
    assert!(!md_default.contains("## Description"));
    assert!(!md_default.contains("## Counts"));
    assert!(!md_default.contains("## Artifacts"));
    assert!(!md_default.contains("## Provenance"));
    assert!(md_default.contains("## Diagnostics"));

    let md_verbose = summary_markdown(&sampled_report, true);
    assert!(md_verbose.contains("## Provenance"));
    assert!(md_verbose.contains("file-format"));

    let txt_default = summary_text(&sampled_report, false);
    assert!(txt_default.contains("Schema: 3\n"));
    assert!(txt_default.contains("Format: video (video/mp4, .mp4)\n"));
    assert!(txt_default.contains(&format!("Fingerprint ({SAMPLED_FINGERPRINT_SCHEME}):")));
    assert!(!txt_default.contains("# Glassbox"));
    assert!(!txt_default.contains("Provenance:"));

    let txt_verbose = summary_text(&sampled_report, true);
    assert!(txt_verbose.contains("Provenance:\n"));
    assert!(txt_verbose.contains("file-format"));

    // Verify PDF geometry + image geometry summaries render cleanly without leaking empty sections.
    let mut geo_report = MetadataReport::default();
    geo_report.metadata.status = OperationStatus::Complete;
    geo_report.metadata.format.bucket = "PDF".into();
    geo_report.metadata.pdf = Some(PdfDocumentSignals {
        security_handler_revision: None,
        permissions: Some(0xffff),
    });
    geo_report.metadata.native_page_signals = vec![NativePageSignal {
        page_index: 0,
        text_layer: TextLayerStatus::Present,
        ocr_recommended: OcrRecommendation::NotRecommended,
        object_counts: vec![],
        coverage: Completeness::Exact,
        uncertainties: vec![],
        width_points: Some(612.0),
        height_points: Some(792.0),
        rotation_quarter_turns: Some(0),
    }];
    geo_report.metadata.raster = Some(RasterDimensions {
        width_pixels: 800,
        height_pixels: 600,
    });
    let geo_md = summary_markdown(&geo_report, false);
    assert!(geo_md.contains("## PDF geometry"));
    assert!(geo_md.contains("612.00 x 792.00 pt (uniform)"));
    assert!(geo_md.contains("## Image geometry"));
    assert!(geo_md.contains("800 x 600 decoded pixels"));
}

/// Verifies legacy Rust report structs serialize under schema version 2 while
/// canonical v3 projections exclusively stamp schema version 3 with the five
/// top-level v3 blocks (GB-AUD-03).
#[test]
fn legacy_rust_reports_and_v3_projections_use_distinct_schema_versions() {
    use glassbox_inspect::reports::{LEGACY_REPORT_SCHEMA_VERSION, REPORT_SCHEMA_VERSION};

    assert_eq!(LEGACY_REPORT_SCHEMA_VERSION, 2);
    assert_eq!(REPORT_SCHEMA_VERSION, 3);
    assert_eq!(REPORT_SCHEMA_VERSION_V3, REPORT_SCHEMA_VERSION);

    let inspect_report = inspect(&fixtures().join("hello.txt"), &InspectOptions::default());
    let legacy_inspect_json = serde_json::to_value(&inspect_report).unwrap();
    assert_eq!(legacy_inspect_json["schema_version"], 2);
    assert!(legacy_inspect_json.get("metadata").is_some());
    assert!(legacy_inspect_json.get("file").is_none());

    let v3_inspect_json = serde_json::to_value(inspect_report.to_v3(false)).unwrap();
    assert_eq!(v3_inspect_json["schema_version"], 3);
    for key in ["file", "format", "description", "structure", "readiness"] {
        assert!(
            v3_inspect_json.get(key).is_some(),
            "missing v3 inspect block `{key}`: {v3_inspect_json}"
        );
    }

    let extract_report = extract(&fixtures().join("hello.txt"), &ExtractOptions::default());
    let legacy_extract_json = serde_json::to_value(&extract_report).unwrap();
    assert_eq!(legacy_extract_json["schema_version"], 2);

    let v3_extract_json = serde_json::to_value(extract_report.to_v3(false)).unwrap();
    assert_eq!(v3_extract_json["schema_version"], 3);
    assert_eq!(v3_extract_json["metadata"]["schema_version"], 3);
    for key in ["file", "format", "description", "structure", "readiness"] {
        assert!(
            v3_extract_json["metadata"].get(key).is_some(),
            "missing v3 extract metadata block `{key}`: {v3_extract_json}"
        );
    }
}

/// Verifies `structure.pdf.uniform_page_size` is true only on complete uniform
/// scans, false when observed dimensions differ, and omitted when scan coverage
/// is incomplete (GB-AUD-05).
#[test]
fn pdf_uniform_page_size_respects_scan_coverage_and_observed_differences() {
    let make_signal = |page_index: u64, w: f32, h: f32| NativePageSignal {
        page_index,
        text_layer: TextLayerStatus::Present,
        ocr_recommended: OcrRecommendation::NotRecommended,
        object_counts: vec![],
        coverage: Completeness::Exact,
        uncertainties: vec![],
        width_points: Some(w),
        height_points: Some(h),
        rotation_quarter_turns: Some(0),
    };

    // (a) Complete uniform 2-page scan -> Some(true).
    let mut complete = MetadataReport::default();
    complete.metadata.format.bucket = "PDF".into();
    complete.metadata.counts.push(QualifiedCount {
        unit: NativeUnit::PhysicalPage,
        value: Some(2),
        source: FactSource::NativeStructure,
        completeness: Completeness::Exact,
    });
    complete.metadata.native_page_signals =
        vec![make_signal(1, 612.0, 792.0), make_signal(2, 612.0, 792.0)];
    let complete_v3 = complete.to_v3(false);
    assert_eq!(
        complete_v3
            .structure
            .pdf
            .as_ref()
            .unwrap()
            .uniform_page_size,
        Some(true)
    );

    // (b) Partial capped scan (4,097 declared pages, uniform scanned prefix) -> None (omitted).
    let mut partial_uniform = MetadataReport::default();
    partial_uniform.metadata.format.bucket = "PDF".into();
    partial_uniform.metadata.counts.push(QualifiedCount {
        unit: NativeUnit::PhysicalPage,
        value: Some(4097),
        source: FactSource::NativeStructure,
        completeness: Completeness::Exact,
    });
    partial_uniform.metadata.native_page_signals =
        vec![make_signal(1, 612.0, 792.0), make_signal(2, 612.0, 792.0)];
    partial_uniform.metadata.diagnostics.push(Diagnostic {
        stage: DiagnosticStage::NativeSignals,
        severity: DiagnosticSeverity::Warning,
        code: "pdf_page_signals_capped".into(),
        message: "page signals capped at 4096 pages; remaining pages are unscanned".into(),
        unit_index: None,
    });
    let partial_v3 = partial_uniform.to_v3(false);
    assert_eq!(
        partial_v3.structure.pdf.as_ref().unwrap().uniform_page_size,
        None
    );
    let partial_json = serde_json::to_value(&partial_v3).unwrap();
    assert!(
        partial_json["structure"]["pdf"]
            .get("uniform_page_size")
            .is_none(),
        "incomplete scan must omit uniform_page_size: {partial_json}"
    );

    // (c) Partial capped scan with differing observed pages -> Some(false).
    let mut partial_varying = partial_uniform;
    partial_varying.metadata.native_page_signals[1].width_points = Some(842.0);
    let varying_v3 = partial_varying.to_v3(false);
    assert_eq!(
        varying_v3.structure.pdf.as_ref().unwrap().uniform_page_size,
        Some(false)
    );
}

/// Verifies format classification, detection confidence, and reader eligibility
/// are decoupled for generic ZIPs with Office suffixes, unsupported raster
/// formats (PSD), and conclusive signatures mapped to `other` or `applications`
/// wearing misleading suffixes (GB-AUD-06).
#[test]
fn format_classification_confidence_and_reader_eligibility_are_decoupled() {
    use std::io::Write;

    let dir = scratch("format-decoupling");

    // 1. Generic ZIP named `listing.docx` -> type: "archives", extension: "docx", extension_mismatch: true.
    let listing_docx = dir.join("listing.docx");
    {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&listing_docx).unwrap());
        zip.start_file(
            "notes.txt",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(b"plain zip payload").unwrap();
        zip.finish().unwrap();
    }
    let zip_report = inspect(&listing_docx, &InspectOptions::default());
    assert_eq!(zip_report.metadata.status, OperationStatus::Complete);
    let zip_v3 = zip_report.to_v3(false);
    assert_eq!(zip_v3.format.format_type, "archives");
    assert_eq!(zip_v3.format.mime.as_deref(), Some("application/zip"));
    assert_eq!(zip_v3.format.extension.as_deref(), Some("docx"));
    assert_eq!(zip_v3.format.extension_mismatch, Some(true));

    // 2. Valid minimal PSD (`8BPS` header) named `regular.psd` -> type: "images",
    //    not failed, no `metadata_reader_failed`, unsupported decoder diagnostic.
    let mut psd_bytes = vec![0u8; 26];
    psd_bytes[0..4].copy_from_slice(b"8BPS");
    psd_bytes[4..6].copy_from_slice(&1u16.to_be_bytes()); // version 1
    psd_bytes[12..14].copy_from_slice(&3u16.to_be_bytes()); // 3 channels (RGB)
    psd_bytes[14..18].copy_from_slice(&16u32.to_be_bytes()); // height
    psd_bytes[18..22].copy_from_slice(&16u32.to_be_bytes()); // width
    psd_bytes[22..24].copy_from_slice(&8u16.to_be_bytes()); // 8-bit depth
    psd_bytes[24..26].copy_from_slice(&3u16.to_be_bytes()); // RGB mode
    let regular_psd = write_fixture(&dir, "regular.psd", &psd_bytes);
    let psd_report = inspect(&regular_psd, &InspectOptions::default());
    assert_ne!(psd_report.metadata.status, OperationStatus::Failed);
    assert_eq!(psd_report.metadata.status, OperationStatus::Unsupported);
    assert!(
        !psd_report
            .metadata
            .diagnostics
            .iter()
            .any(|d| d.code == "metadata_reader_failed"),
        "PSD must not fail raster reader: {:?}",
        psd_report.metadata.diagnostics
    );
    assert!(
        psd_report
            .metadata
            .diagnostics
            .iter()
            .any(|d| d.code == "unsupported_image_format")
    );
    let psd_v3 = psd_report.to_v3(false);
    assert_eq!(psd_v3.format.format_type, "images");
    assert_eq!(
        psd_v3.format.mime.as_deref(),
        Some("image/vnd.adobe.photoshop")
    );
    assert_eq!(psd_v3.format.extension.as_deref(), Some("psd"));
    assert_eq!(psd_v3.format.extension_mismatch, None);

    // Extracting PSD must also report unsupported cleanly rather than failed.
    let psd_extract = extract(&regular_psd, &ExtractOptions::default());
    assert_eq!(psd_extract.metadata.status, OperationStatus::Unsupported);

    // 3a. Conclusive `"other"` signature (PEM private key) named `fake.pdf` ->
    //     type: "other", extension: "pdf", extension_mismatch: true.
    let pem_as_pdf = write_fixture(
        &dir,
        "fake.pdf",
        b"-----BEGIN PRIVATE KEY-----\nMIIBVQIBADANBgkqhkiG9w0BAQEFAASCAT8wggE7AgEAAkEA\n-----END PRIVATE KEY-----\n",
    );
    let pem_v3 = inspect(&pem_as_pdf, &InspectOptions::default()).to_v3(false);
    assert_eq!(pem_v3.format.format_type, "other");
    assert_eq!(pem_v3.format.extension.as_deref(), Some("pdf"));
    assert_eq!(pem_v3.format.extension_mismatch, Some(true));

    // 3b. Conclusive ELF binary named `binary.pdf` ->
    //     type: "applications", extension: "pdf", extension_mismatch: true.
    let mut elf_bytes = vec![0u8; 64];
    elf_bytes[0..4].copy_from_slice(b"\x7fELF");
    elf_bytes[4] = 2; // 64-bit
    elf_bytes[5] = 1; // little-endian
    elf_bytes[6] = 1; // ELF version 1
    let elf_as_pdf = write_fixture(&dir, "binary.pdf", &elf_bytes);
    let elf_v3 = inspect(&elf_as_pdf, &InspectOptions::default()).to_v3(false);
    assert_eq!(elf_v3.format.format_type, "applications");
    assert_eq!(elf_v3.format.extension.as_deref(), Some("pdf"));
    assert_eq!(elf_v3.format.extension_mismatch, Some(true));

    std::fs::remove_dir_all(&dir).ok();
}

/// Verifies a byte-confirmed PSD (`8BPS` header) disguised with a `.png`
/// extension reports `format.type == "images"`, preserves `extension == Some("png")`,
/// never fails inspection or extraction, and produces zero raster decode errors.
#[test]
fn misnamed_psd_with_png_extension_never_triggers_raster_decode_failure() {
    let dir = scratch("misnamed-psd-png");
    let mut psd_bytes = vec![0u8; 26];
    psd_bytes[0..4].copy_from_slice(b"8BPS");
    psd_bytes[4..6].copy_from_slice(&1u16.to_be_bytes());
    psd_bytes[12..14].copy_from_slice(&3u16.to_be_bytes());
    psd_bytes[14..18].copy_from_slice(&16u32.to_be_bytes());
    psd_bytes[18..22].copy_from_slice(&16u32.to_be_bytes());
    psd_bytes[22..24].copy_from_slice(&8u16.to_be_bytes());
    psd_bytes[24..26].copy_from_slice(&3u16.to_be_bytes());
    let misnamed_psd = write_fixture(&dir, "misnamed_psd.png", &psd_bytes);

    let inspected = inspect(&misnamed_psd, &InspectOptions::default());
    assert_ne!(inspected.metadata.status, OperationStatus::Failed);
    assert_eq!(inspected.metadata.status, OperationStatus::Unsupported);
    assert!(
        !inspected
            .metadata
            .diagnostics
            .iter()
            .any(|d| d.severity == DiagnosticSeverity::Error),
        "inspect must produce zero error diagnostics: {:?}",
        inspected.metadata.diagnostics
    );
    let inspect_v3 = inspected.to_v3(false);
    assert_eq!(inspect_v3.format.format_type, "images");
    assert_eq!(inspect_v3.format.extension.as_deref(), Some("png"));
    assert_eq!(
        inspect_v3.format.mime.as_deref(),
        Some("image/vnd.adobe.photoshop")
    );

    let mut inspector = Inspector::default();
    let extracted = inspector.extract(&misnamed_psd, &ExtractOptions::default());
    assert_ne!(extracted.metadata.status, OperationStatus::Failed);
    assert_eq!(extracted.metadata.status, OperationStatus::Unsupported);
    assert!(
        !extracted
            .metadata
            .diagnostics
            .iter()
            .any(|d| d.severity == DiagnosticSeverity::Error || d.code == "image"),
        "extract must produce zero images::inspect decode errors: {:?}",
        extracted.metadata.diagnostics
    );
    let extract_v3 = extracted.to_v3(false);
    assert_eq!(extract_v3.metadata.format.format_type, "images");
    assert_eq!(extract_v3.metadata.format.extension.as_deref(), Some("png"));
    assert_eq!(
        extract_v3.metadata.format.mime.as_deref(),
        Some("image/vnd.adobe.photoshop")
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Verifies out-of-range extraction selection fails the top-level extraction
/// operation while preserving the nested standalone inspection status as
/// `complete` with zero metadata diagnostics (GB-AUD-17).
#[test]
fn out_of_range_extract_keeps_nested_inspection_status_complete() {
    use glassbox_inspect::reports::{NativeRange, NativeSelection, OcrOptions};

    let pdf = fixtures().join("native.pdf");
    let standalone = inspect(&pdf, &InspectOptions::default()).to_v3(false);
    assert_eq!(standalone.status, OperationStatus::Complete);
    assert!(standalone.diagnostics.is_empty());

    let report = extract(
        &pdf,
        &ExtractOptions {
            selection: NativeSelection::Ranges {
                ranges: vec![NativeRange { start: 99, end: 99 }],
            },
            ocr: OcrOptions {
                enabled: false,
                ..OcrOptions::default()
            },
            ..ExtractOptions::default()
        },
    );
    assert_eq!(report.status, OperationStatus::Failed);
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    assert!(report.metadata.diagnostics.is_empty());
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.stage == DiagnosticStage::Selection)
    );

    let v3 = report.to_v3(false);
    assert_eq!(v3.status, OperationStatus::Failed);
    assert_eq!(v3.metadata.status, OperationStatus::Complete);
    assert!(v3.metadata.diagnostics.is_empty());
    assert!(
        v3.diagnostics
            .iter()
            .any(|d| d.stage == DiagnosticStage::Selection)
    );
    let json = serde_json::to_value(&v3).unwrap();
    assert_eq!(json["status"], "failed");
    assert_eq!(json["metadata"]["status"], "complete");
    assert!(json["metadata"].get("diagnostics").is_none());
    assert!(
        json["diagnostics"]
            .as_array()
            .is_some_and(|d| !d.is_empty())
    );
}

/// Verifies `structure.names.json_pointers` emits unambiguous RFC 6901-escaped
/// schema pointers with `*` array wildcards and `structure.names.xml_namespaces`
/// publishes the prefix-to-URI map for generated `nsN` XML prefixes (GB-AUD-18).
#[test]
fn structured_json_pointers_and_xml_namespaces_are_unambiguous() {
    let dir = scratch("structured-paths");

    // JSON with dotted key vs nested object, array wildcard, and RFC 6901 escapes (`/` and `~`).
    let json_path = write_fixture(
        &dir,
        "schema.json",
        br#"{"a.b": 1, "a": {"b": 2}, "items": [{"id": 3}], "k/s~t": 4}"#,
    );
    let json_v3 = inspect(&json_path, &InspectOptions::default()).to_v3(false);
    assert_eq!(json_v3.status, OperationStatus::Complete);
    let json_names = json_v3
        .structure
        .names
        .as_ref()
        .expect("structure.names expected for JSON");
    assert!(json_names.xml_namespaces.is_empty());
    let mut pointers = json_names.json_pointers.clone();
    pointers.sort();
    assert_eq!(
        pointers,
        vec![
            "/a.b".to_owned(),
            "/a/b".to_owned(),
            "/items/*/id".to_owned(),
            "/k~1s~0t".to_owned(),
        ]
    );

    // Namespaced XML must publish the exact `nsN` -> URI map alongside `xml_paths`.
    let xml_path = write_fixture(
        &dir,
        "namespaced.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?>
<root xmlns:a="urn:example:alpha" xmlns:b="urn:example:beta">
  <a:item><b:code>42</b:code></a:item>
</root>"#,
    );
    let xml_v3 = inspect(&xml_path, &InspectOptions::default()).to_v3(false);
    assert_eq!(xml_v3.status, OperationStatus::Complete);
    let xml_names = xml_v3
        .structure
        .names
        .as_ref()
        .expect("structure.names expected for XML");
    assert!(
        xml_names
            .xml_paths
            .iter()
            .any(|path| path.contains("ns0:item") && path.contains("ns1:code")),
        "expected generated ns0/ns1 prefixes in xml_paths: {:?}",
        xml_names.xml_paths
    );
    assert_eq!(
        xml_names.xml_namespaces.get("ns0").map(String::as_str),
        Some("urn:example:alpha")
    );
    assert_eq!(
        xml_names.xml_namespaces.get("ns1").map(String::as_str),
        Some("urn:example:beta")
    );
    let xml_json = serde_json::to_value(&xml_v3).unwrap();
    assert_eq!(
        xml_json["structure"]["names"]["xml_namespaces"],
        serde_json::json!({
            "ns0": "urn:example:alpha",
            "ns1": "urn:example:beta",
        })
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Writes a synthetic ZIP archive under a test-owned scratch directory.
///
/// Accepts destination directory, filename, and `(entry_name, bytes)` pairs.
/// Returns the written archive path. ZIP creation or I/O failures panic.
fn write_zip_fixture(directory: &Path, name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
    use std::io::Write;
    let path = directory.join(name);
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    for (entry_name, bytes) in entries {
        writer
            .start_file(*entry_name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap();
    path
}

/// Builds a minimal 64-byte little-endian ELF64 executable header for spoofing tests.
///
/// Accepts no parameters. Returns a byte vector matching the ELF magic signature
/// without invoking any external toolchain. Cannot fail.
fn minimal_elf64_bytes() -> Vec<u8> {
    let mut bytes = vec![0u8; 64];
    bytes[0..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2; // 64-bit
    bytes[5] = 1; // little-endian
    bytes[6] = 1; // ELF version 1
    bytes[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    bytes[18..20].copy_from_slice(&0x3eu16.to_le_bytes()); // EM_X86_64
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes()); // EV_CURRENT
    bytes
}

/// Verifies byte-confirmed HTML and ELF payloads disguised as `.parquet` report
/// their true format with `extension_mismatch: true` and never route to Parquet.
///
/// Accepts no parameters. Panics if spoofed HTML/ELF `.parquet` files lose
/// byte-confirmed identity, fail to mark `extension_mismatch`, report Parquet
/// diagnostics or provenance, or misroute into `parquet-identity`.
#[test]
fn spoofed_html_and_elf_named_parquet_do_not_misroute_to_parquet() {
    let dir = scratch("spoofed-parquet");

    let html_path = write_fixture(
        &dir,
        "spoof.parquet",
        b"<!doctype html><html><body>hi</body></html>",
    );
    let html_report = inspect(&html_path, &InspectOptions::default());
    let html_v3 = html_report.to_v3(true);
    assert_eq!(html_v3.format.format_type, "web");
    assert_eq!(html_v3.format.extension.as_deref(), Some("parquet"));
    assert_eq!(html_v3.format.extension_mismatch, Some(true));
    assert_eq!(html_v3.readiness.extractable, "unsupported");
    assert!(
        !html_v3
            .diagnostics
            .iter()
            .any(|d| d.code == "parquet_identification_only"),
        "spoofed HTML must not emit parquet_identification_only: {:?}",
        html_v3.diagnostics
    );
    assert!(
        !html_report
            .metadata
            .provenance
            .iter()
            .any(|p| p.reader == "parquet-identity"),
        "spoofed HTML must not record parquet-identity provenance: {:?}",
        html_report.metadata.provenance
    );

    let elf_path = write_fixture(&dir, "spoof_elf.parquet", &minimal_elf64_bytes());
    let elf_report = inspect(&elf_path, &InspectOptions::default());
    let elf_v3 = elf_report.to_v3(true);
    assert_eq!(elf_v3.format.format_type, "applications");
    assert_eq!(elf_v3.format.extension.as_deref(), Some("parquet"));
    assert_eq!(elf_v3.format.extension_mismatch, Some(true));
    assert_eq!(elf_v3.readiness.extractable, "unsupported");
    assert!(
        !elf_v3
            .diagnostics
            .iter()
            .any(|d| d.code == "parquet_identification_only"),
        "spoofed ELF must not emit parquet_identification_only: {:?}",
        elf_v3.diagnostics
    );
    assert!(
        !elf_report
            .metadata
            .provenance
            .iter()
            .any(|p| p.reader == "parquet-identity"),
        "spoofed ELF must not record parquet-identity provenance: {:?}",
        elf_report.metadata.provenance
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Verifies generic ZIP archives named `.pages`, `.numbers`, `.key`, or `.cbz`
/// fall through to generic ZIP identification with `extension_mismatch: true`,
/// while legitimate iWork and CBZ archives retain their specialized identities.
///
/// Accepts no parameters. Panics if generic ZIPs are misclassified as iWork or
/// CBZ, fail to flag `extension_mismatch`, emit `iwork_preview` errors on
/// extraction, or if valid iWork/CBZ archives regress.
#[test]
fn generic_zips_named_iwork_or_cbz_fall_through_while_legitimate_archives_match() {
    let dir = scratch("zip-container-validation");
    let mut inspector = Inspector::default();

    for ext in ["pages", "numbers", "key", "cbz"] {
        let generic = write_zip_fixture(
            &dir,
            &format!("generic.{ext}"),
            &[("readme.txt", b"just a generic zip archive")],
        );
        let inspected = inspect(&generic, &InspectOptions::default());
        let v3 = inspected.to_v3(true);
        assert_eq!(
            v3.format.format_type, "archives",
            "generic.{ext} must resolve to archives"
        );
        assert_eq!(v3.format.extension.as_deref(), Some(ext));
        assert_eq!(v3.format.extension_mismatch, Some(true));
        assert_eq!(v3.readiness.extractable, "identification_only");
        assert!(
            inspected
                .metadata
                .provenance
                .iter()
                .any(|p| p.reader == "zip-directory"),
            "generic.{ext} should record zip-directory provenance: {:?}",
            inspected.metadata.provenance
        );
        assert!(
            !inspected
                .metadata
                .provenance
                .iter()
                .any(|p| matches!(p.reader.as_str(), "iwork-archive" | "cbz-index")),
            "generic.{ext} must not record iwork-archive or cbz-index provenance: {:?}",
            inspected.metadata.provenance
        );

        let extracted = inspector.extract(&generic, &ExtractOptions::default());
        assert!(
            !extracted
                .diagnostics
                .iter()
                .any(|d| d.code == "iwork_preview"),
            "generic.{ext} extraction must not emit iwork_preview error: {:?}",
            extracted.diagnostics
        );
    }

    // A `.cbz` containing only macOS resource-fork image entries must also fall through to generic ZIP.
    let macos_only_cbz = write_zip_fixture(
        &dir,
        "macos_only.cbz",
        &[("__MACOSX/._01.png", b"resource fork")],
    );
    let macos_v3 = inspect(&macos_only_cbz, &InspectOptions::default()).to_v3(false);
    assert_eq!(macos_v3.format.format_type, "archives");
    assert_eq!(macos_v3.format.extension.as_deref(), Some("cbz"));
    assert_eq!(macos_v3.format.extension_mismatch, Some(true));
    assert_eq!(macos_v3.readiness.extractable, "identification_only");

    // Positive iWork case: Index/Document.iwa without QuickLook preview is still recognized as office_docs.
    let valid_pages = write_zip_fixture(&dir, "doc.pages", &[("Index/Document.iwa", b"iwa-bytes")]);
    let pages_report = inspect(&valid_pages, &InspectOptions::default());
    let pages_v3 = pages_report.to_v3(true);
    assert_eq!(pages_v3.format.format_type, "office_docs");
    assert_eq!(pages_v3.format.extension.as_deref(), Some("pages"));
    assert_eq!(pages_v3.format.extension_mismatch, None);
    assert!(
        pages_report
            .metadata
            .provenance
            .iter()
            .any(|p| p.reader == "iwork-archive")
    );

    // Positive CBZ case: archive with a real image entry is recognized as comics.
    let png_bytes = std::fs::read(fixtures().join("image.png")).unwrap();
    let valid_cbz = write_zip_fixture(&dir, "comic.cbz", &[("01.png", &png_bytes)]);
    let cbz_report = inspect(&valid_cbz, &InspectOptions::default());
    let cbz_v3 = cbz_report.to_v3(true);
    assert_eq!(cbz_v3.format.format_type, "comics");
    assert_eq!(cbz_v3.format.extension.as_deref(), Some("cbz"));
    assert_eq!(cbz_v3.format.extension_mismatch, None);
    assert!(
        cbz_report
            .metadata
            .provenance
            .iter()
            .any(|p| p.reader == "cbz-index")
    );

    std::fs::remove_dir_all(&dir).ok();
}
