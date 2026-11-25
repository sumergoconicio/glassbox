//! Module: glassbox-inspect bounded format detection.
//! File: detect.rs.
//! Responsibility: Identify file families from bounded byte prefixes and normalized suffixes, mapping every format into the 23 canonical v3 `format.type` identifiers.

use crate::{Format, InspectReport};
use file_format::{FileFormat, Kind};
use std::{io::Read, path::Path};

/// All 23 canonical lowercase `format.type` identifiers in schema v3.
pub const ALL_FORMAT_TYPES: [&str; 23] = [
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
];

/// Compound archive suffixes matched before single-component extensions.
const COMPOUND_SUFFIXES: [&str; 4] = ["tar.gz", "tar.bz2", "tar.xz", "tar.zst"];

/// Extracts the normalized lowercase filename suffix without a leading dot.
///
/// Accepts any file path. Returns the longest known compound archive suffix
/// (`tar.gz`, `tar.bz2`, `tar.xz`, `tar.zst`) before falling back to the final
/// extension component, or `None` when the filename has no suffix (including
/// single-dot dotfiles such as `.bashrc` and trailing dots).
/// Performs no filesystem I/O and cannot fail.
pub fn normalize_extension(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    for suffix in COMPOUND_SUFFIXES {
        if name.len() > suffix.len() + 1
            && name.ends_with(suffix)
            && name.as_bytes()[name.len() - suffix.len() - 1] == b'.'
        {
            return Some(suffix.to_owned());
        }
    }
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    (!ext.is_empty()).then_some(ext)
}

/// Maps one normalized filename suffix to its canonical v3 `format.type` identifier.
///
/// Accepts a suffix with or without a leading dot in any ASCII letter case.
/// Returns one of the 23 lowercase identifiers in [`ALL_FORMAT_TYPES`], defaulting
/// to `"other"` for empty or unmapped suffixes. Performs no I/O and cannot fail.
pub fn format_type_for_extension(ext: &str) -> &'static str {
    let normalized = ext.trim_start_matches('.').to_ascii_lowercase();
    match normalized.as_str() {
        "pdf" => "pdf",
        "doc" | "docx" | "odt" | "rtf" | "pages" => "office_docs",
        "ppt" | "pptx" | "odp" | "key" => "presentations",
        "xls" | "xlsx" | "xlsm" | "ods" | "numbers" => "spreadsheets",
        "txt" | "md" | "rst" | "tex" => "text",
        "csv" | "tsv" | "parquet" | "arrow" => "tabular_data",
        "json" | "jsonl" | "xml" | "yaml" | "yml" | "toml" => "structured_data",
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "avif" | "tif" | "tiff" | "bmp" => {
            "images"
        }
        "svg" | "eps" => "vector_graphics",
        "epub" | "mobi" | "azw" | "azw3" | "prc" | "djvu" | "djv" => "ebooks",
        "cbz" | "cbr" => "comics",
        "mp3" | "m4a" | "aac" | "flac" | "wav" | "ogg" | "opus" | "aiff" => "audio",
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" | "mpeg" => "video",
        "tar.gz" | "tar.bz2" | "tar.xz" | "tar.zst" | "zip" | "rar" | "7z" | "tar" | "gz"
        | "bz2" | "xz" | "zst" => "archives",
        "py" | "go" | "js" | "ts" | "jsx" | "tsx" | "java" | "c" | "cpp" | "rs" | "sh" | "sql" => {
            "source_code"
        }
        "html" | "htm" | "css" | "mhtml" => "web",
        "sqlite" | "sqlite3" | "db" | "mdb" | "accdb" => "databases",
        "eml" | "msg" | "mbox" | "ics" | "vcf" => "personal_info",
        "srt" | "vtt" | "ass" => "subtitles",
        "ttf" | "otf" | "woff" | "woff2" => "fonts",
        "stl" | "obj" | "gltf" | "glb" | "step" | "stp" | "dwg" | "dxf" => "3d_cad",
        "exe" | "msi" | "apk" | "deb" | "rpm" | "appimage" => "applications",
        _ => "other",
    }
}

/// Maps any byte-detected [`FileFormat`] variant to one of the 23 canonical v3 `format.type` identifiers.
///
/// Accepts a `file_format::FileFormat` detected from source bytes. Returns an
/// identifier from [`ALL_FORMAT_TYPES`], explicitly disambiguating colliding
/// extensions (such as MPEG-2 transport streams `.ts`, Mega Drive ROMs `.md`,
/// and PEM private keys `.key`) before falling back to [`Kind`] and suffix lookup.
/// Performs no I/O and cannot fail.
pub fn format_type_for_file_format(format: FileFormat) -> &'static str {
    match format {
        FileFormat::ArbitraryBinaryData | FileFormat::Empty => "other",
        FileFormat::PlainText => "text",
        FileFormat::PortableDocumentFormat => "pdf",
        FileFormat::Djvu => "ebooks",
        FileFormat::ScalableVectorGraphics | FileFormat::EncapsulatedPostscript => {
            "vector_graphics"
        }
        FileFormat::MegaDriveRom | FileFormat::PemPrivateKey => "other",
        FileFormat::Mpeg2TransportStream => "video",
        _ => match format.kind() {
            Kind::Audio => "audio",
            Kind::Video => "video",
            Kind::Archive => match format_type_for_extension(format.extension()) {
                "comics" => "comics",
                _ => "archives",
            },
            Kind::Compressed => "archives",
            Kind::Database => "databases",
            Kind::Ebook => "ebooks",
            Kind::Executable | Kind::Package => "applications",
            Kind::Font => "fonts",
            Kind::Image => "images",
            Kind::Model => "3d_cad",
            Kind::Presentation => "presentations",
            Kind::Spreadsheet => "spreadsheets",
            Kind::Subtitle => "subtitles",
            Kind::Document => match format_type_for_extension(format.extension()) {
                "other" => "office_docs",
                mapped => mapped,
            },
            _ => format_type_for_extension(format.extension()),
        },
    }
}

/// Converts an internal display bucket label into its canonical lowercase v3 `format.type` identifier.
///
/// Accepts one of the legacy display bucket names stored on [`crate::reports::FormatIdentity`].
/// Returns the matching identifier from [`ALL_FORMAT_TYPES`], defaulting to `"other"`
/// for unrecognized or empty strings. Performs no I/O and cannot fail.
pub(crate) fn bucket_to_format_type(bucket: &str) -> &'static str {
    match bucket {
        "PDF" | "pdf" => "pdf",
        "Office Docs" | "office_docs" => "office_docs",
        "Presentations" | "presentations" => "presentations",
        "Spreadsheets" | "spreadsheets" => "spreadsheets",
        "Text" | "text" => "text",
        "Tabular Data" | "tabular_data" => "tabular_data",
        "Structured Data" | "structured_data" => "structured_data",
        "Images" | "images" => "images",
        "Vector Graphics" | "vector_graphics" => "vector_graphics",
        "Ebooks" | "ebooks" => "ebooks",
        "Comics" | "comics" => "comics",
        "Audio" | "audio" => "audio",
        "Video" | "video" => "video",
        "Archives" | "archives" => "archives",
        "Source Code" | "source_code" => "source_code",
        "Web" | "web" => "web",
        "Databases" | "databases" => "databases",
        "Personal Info" | "personal_info" => "personal_info",
        "Subtitles" | "subtitles" => "subtitles",
        "Fonts" | "fonts" => "fonts",
        "3D CAD" | "3d_cad" => "3d_cad",
        "Applications" | "applications" => "applications",
        _ => "other",
    }
}

/// Converts a canonical lowercase v3 `format.type` identifier back to the legacy display bucket name.
///
/// Accepts one of the 23 identifiers in [`ALL_FORMAT_TYPES`]. Returns the display
/// label consumed by internal reader dispatch branches, defaulting to `"Other"`.
/// Performs no I/O and cannot fail.
pub(crate) fn format_type_to_bucket(format_type: &str) -> &'static str {
    match format_type {
        "pdf" => "PDF",
        "office_docs" => "Office Docs",
        "presentations" => "Presentations",
        "spreadsheets" => "Spreadsheets",
        "text" => "Text",
        "tabular_data" => "Tabular Data",
        "structured_data" => "Structured Data",
        "images" => "Images",
        "vector_graphics" => "Vector Graphics",
        "ebooks" => "Ebooks",
        "comics" => "Comics",
        "audio" => "Audio",
        "video" => "Video",
        "archives" => "Archives",
        "source_code" => "Source Code",
        "web" => "Web",
        "databases" => "Databases",
        "personal_info" => "Personal Info",
        "subtitles" => "Subtitles",
        "fonts" => "Fonts",
        "3d_cad" => "3D CAD",
        "applications" => "Applications",
        _ => "Other",
    }
}

/// Checks whether a normalized extension identifies a ZIP-backed container family.
///
/// Accepts a lowercase suffix without leading dot. Returns `true` for container
/// extensions whose 64 KiB prefix can legitimately fall back to generic `FileFormat::Zip`.
/// Performs no I/O and cannot fail.
fn is_zip_container_extension(ext: &str) -> bool {
    matches!(
        ext,
        "docx"
            | "xlsx"
            | "xlsm"
            | "pptx"
            | "odt"
            | "ods"
            | "odp"
            | "epub"
            | "cbz"
            | "pages"
            | "numbers"
            | "key"
            | "apk"
    )
}

/// Checks whether a normalized extension identifies a CFB-backed container family.
///
/// Accepts a lowercase suffix without leading dot. Returns `true` for formats
/// backed by Compound File Binary headers. Performs no I/O and cannot fail.
fn is_cfb_container_extension(ext: &str) -> bool {
    matches!(ext, "doc" | "xls" | "ppt" | "msg" | "msi")
}

/// Reports whether a byte-detected [`FileFormat`] is conclusive against `literal_ext`.
///
/// Accepts a `FileFormat` and the literal lowercase filename suffix (empty when absent).
/// Returns `false` for generic text/binary fallbacks (`PlainText`, `ArbitraryBinaryData`,
/// `Empty`) and generic containers paired with their known container suffixes (`Zip` with
/// OOXML/ODF/EPUB/CBZ/iWork/APK, `CompoundFileBinary` with DOC/XLS/PPT/MSG/MSI, RAR
/// with CBR). Conclusive signatures remain conclusive even when their canonical
/// `format.type` is `"other"` (for example PEM keys or ROM images), so a misleading
/// suffix cannot override verified byte identity. Performs no I/O and cannot fail.
pub(crate) fn is_conclusive_file_format(format: FileFormat, literal_ext: &str) -> bool {
    let ext = literal_ext.trim_start_matches('.').to_ascii_lowercase();
    match format {
        FileFormat::PlainText | FileFormat::ArbitraryBinaryData | FileFormat::Empty => false,
        FileFormat::Zip if is_zip_container_extension(&ext) => false,
        FileFormat::CompoundFileBinary if is_cfb_container_extension(&ext) => false,
        fmt if fmt.extension() == "rar" && ext == "cbr" => false,
        _ => true,
    }
}

/// Resolves canonical v3 `format.type` and `extension_mismatch` using bytes-first precedence.
///
/// Accepts an optional byte-detected [`FileFormat`], an optional reader-confirmed
/// canonical extension (used after package/header confirmation), and the literal
/// normalized filename suffix (`""` when extensionless). Returns `(format_type, mismatch)`:
/// conclusive byte detection wins over suffix fallback, and `mismatch` is `true` only
/// when byte detection is conclusive AND the filename has a known mapped suffix whose
/// family disagrees with the detected family. Performs no I/O and cannot fail.
pub(crate) fn format_type(
    detected: Option<FileFormat>,
    detected_ext: Option<&str>,
    literal_ext: &str,
) -> (&'static str, bool) {
    let literal_norm = literal_ext.trim_start_matches('.').to_ascii_lowercase();
    let literal_type = format_type_for_extension(&literal_norm);
    let conclusive_type = if let Some(fmt) = detected {
        is_conclusive_file_format(fmt, &literal_norm).then(|| format_type_for_file_format(fmt))
    } else if let Some(ext) = detected_ext {
        let mapped = format_type_for_extension(ext);
        (mapped != "other").then_some(mapped)
    } else {
        None
    };

    if let Some(detected_type) = conclusive_type {
        let mismatch =
            !literal_norm.is_empty() && literal_type != "other" && literal_type != detected_type;
        (detected_type, mismatch)
    } else {
        (literal_type, false)
    }
}

/// Maps one normalized filename suffix to its legacy display bucket name.
///
/// Accepts a lowercase suffix without leading dot. Returns one of the 23 display
/// labels preserved for internal reader dispatch. Performs no I/O and cannot fail.
pub(crate) fn bucket(ext: &str) -> &'static str {
    format_type_to_bucket(format_type_for_extension(ext))
}

/// Extracts the normalized lowercase filename suffix or an empty string when absent.
///
/// Accepts a source file path. Returns the longest known compound archive suffix
/// or final extension component, or `""` for extensionless names. Performs no I/O.
pub(crate) fn extension(path: &Path) -> String {
    normalize_extension(path).unwrap_or_default()
}

/// Identifies a source file from a bounded 65,536-byte prefix for the
/// transitional extraction path using bytes-first precedence.
///
/// Accepts the source path and mutable [`InspectReport`] whose
/// `format.extension` already holds the normalized literal filename suffix.
/// Opens the source with [`crate::hashing::open_no_follow`] and returns
/// `Ok(())` on success or an `std::io::Error` if opening or reading the prefix
/// fails. Conclusive byte signatures always set `basis = "bytes"` (including
/// formats whose suffix is unmapped such as PSD) so downstream readers never
/// misroute byte-confirmed payloads via extension fallback.
pub(crate) fn detect(path: &Path, report: &mut InspectReport) -> std::io::Result<()> {
    // Do not let detector readers recursively inspect unbounded container payloads.
    let mut prefix = Vec::new();
    crate::hashing::open_no_follow(path)?
        .take(65_536)
        .read_to_end(&mut prefix)?;
    let format = FileFormat::from_bytes(&prefix);
    let ext = report.format.extension.clone();
    let conclusive = is_conclusive_file_format(format, &ext);
    let (chosen_type, family_disagreement) = format_type(Some(format), None, &ext);
    let chosen_bucket = format_type_to_bucket(chosen_type);
    let detected_ext = format.extension();
    let disagreement = family_disagreement
        || (conclusive
            && !ext.is_empty()
            && format_type_for_extension(&ext) == "other"
            && ext != detected_ext
            && !ext.starts_with(&format!("tar.{detected_ext}")));
    report.format = Format {
        extension: ext,
        detected_mime: Some(format.media_type().into()),
        detected_name: Some(format.name().into()),
        bucket: chosen_bucket.into(),
        basis: if conclusive { "bytes" } else { "extension" }.into(),
        disagreement,
        detector: "file-format".into(),
    };
    #[cfg(feature = "magika")]
    if !conclusive && let Err(error) = magika_fallback(&prefix, report) {
        report.error("magika", error);
    }
    Ok(())
}

/// Identifies one file for the content-free metadata path.
///
/// Accepts a path and literal normalized lowercase extension (`""` when absent).
/// Opens the source with [`crate::hashing::open_no_follow`] and returns format
/// labels from a bounded 65,536-byte prefix using bytes-first precedence and
/// preserving the literal filename suffix. File open/read failures are
/// returned; no body parser, OCR engine, or statistical fallback runs.
pub(crate) fn identify(
    path: &Path,
    extension: &str,
) -> std::io::Result<crate::reports::FormatIdentity> {
    let mut prefix = Vec::new();
    crate::hashing::open_no_follow(path)?
        .take(65_536)
        .read_to_end(&mut prefix)?;
    let format = FileFormat::from_bytes(&prefix);
    let conclusive = is_conclusive_file_format(format, extension);
    let (chosen_type, mismatch) = format_type(Some(format), None, extension);
    Ok(crate::reports::FormatIdentity {
        extension: extension.to_owned(),
        detected_mime: Some(format.media_type().into()),
        detected_name: Some(format.name().into()),
        bucket: format_type_to_bucket(chosen_type).into(),
        basis: if conclusive { "bytes" } else { "extension" }.into(),
        disagreement: mismatch,
        detector: "file-format".into(),
    })
}
#[cfg(feature = "magika")]
fn magika_fallback(prefix: &[u8], report: &mut InspectReport) -> crate::xml::Result<()> {
    // Share prepared CPU plans while each request owns its mutable session.
    // Magika 2 uses tract and leaves ONNX Runtime exclusively to upstream OCRia.
    static RUNTIME: std::sync::OnceLock<Result<magika::Runtime, String>> =
        std::sync::OnceLock::new();
    let runtime = RUNTIME
        .get_or_init(|| {
            magika::Runtime::builder()
                .with_backend(magika::Backend::Cpu)
                .with_max_batch(1)
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| format!("optional Magika initialization failed: {error}"))?;
    let mut session = runtime.session()?;
    let found = session.identify_content(prefix)?;
    let info = found.info();
    report.claims.push(crate::Claim {
        key: "magika_detection".into(),
        value: format!(
            "{}; sampled_bytes={}; engine=magika-2.0.0-rc.0; backend={}",
            info.label,
            prefix.len(),
            runtime.backend_info().implementation()
        ),
        source: "bytes",
    });
    // A statistical alternative remains a claim, not a byte-verified identity.
    Ok(())
}
