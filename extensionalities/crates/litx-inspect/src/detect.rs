//! Module: glassbox-inspect bounded format detection.
//! File: detect.rs.
//! Responsibility: Identify file families from extensions and bounded byte prefixes.

use crate::{Format, InspectReport};
use file_format::FileFormat;
use std::{fs::File, io::Read, path::Path};

pub(crate) fn bucket(ext: &str) -> &'static str {
    for (name, extensions) in [
        ("PDF", "pdf"),
        ("Office Docs", "doc docx odt rtf pages"),
        ("Presentations", "ppt pptx odp key"),
        ("Spreadsheets", "xls xlsx xlsm ods numbers"),
        ("Text", "txt md rst tex"),
        ("Tabular Data", "csv tsv parquet arrow"),
        ("Structured Data", "json jsonl xml yaml yml toml"),
        ("Images", "jpg jpeg png gif webp heic avif tif tiff bmp"),
        ("Vector Graphics", "svg eps"),
        ("Ebooks", "epub mobi azw azw3 prc djvu djv"),
        ("Comics", "cbz cbr"),
        ("Audio", "mp3 m4a aac flac wav ogg opus aiff"),
        ("Video", "mp4 mkv mov avi webm m4v mpeg"),
        (
            "Archives",
            "zip rar 7z tar gz bz2 xz zst tar.gz tar.bz2 tar.xz tar.zst",
        ),
        ("Source Code", "py go js ts jsx tsx java c cpp rs sh sql"),
        ("Web", "html htm css mhtml"),
        ("Databases", "sqlite sqlite3 db mdb accdb"),
        ("Personal Info", "eml msg mbox ics vcf"),
        ("Subtitles", "srt vtt ass"),
        ("Fonts", "ttf otf woff woff2"),
        ("3D CAD", "stl obj gltf glb step stp dwg dxf"),
        ("Applications", "exe msi apk deb rpm appimage"),
    ] {
        if extensions.split_whitespace().any(|e| e == ext) {
            return name;
        }
    }
    "Other"
}

pub(crate) fn extension(path: &Path) -> String {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    for suffix in ["tar.gz", "tar.bz2", "tar.xz", "tar.zst"] {
        if name.ends_with(&format!(".{suffix}")) {
            return suffix.into();
        }
    }
    path.extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase()
}

pub(crate) fn detect(path: &Path, report: &mut InspectReport) -> std::io::Result<()> {
    // Do not let detector readers recursively inspect unbounded container payloads.
    let mut prefix = Vec::new();
    File::open(path)?.take(65_536).read_to_end(&mut prefix)?;
    let format = FileFormat::from_bytes(&prefix);
    let ext = report.format.extension.clone();
    let detected_ext = format.extension();
    let detected_bucket = bucket(detected_ext);
    let ambiguous = detected_bucket == "Other" || matches!(format, FileFormat::PlainText);
    let chosen_bucket = if ambiguous {
        bucket(&ext)
    } else {
        detected_bucket
    };
    let alias = |s: &str| {
        match s {
            "jpeg" => "jpg",
            "tiff" => "tif",
            "sqlite3" | "db" => "sqlite",
            "htm" => "html",
            "cbz" => "zip",
            "cbr" => "rar",
            other => other,
        }
        .to_string()
    };
    report.format = Format {
        extension: ext.clone(),
        detected_mime: Some(format.media_type().into()),
        detected_name: Some(format.name().into()),
        bucket: chosen_bucket.into(),
        basis: if ambiguous { "extension" } else { "bytes" }.into(),
        disagreement: !ambiguous
            && !ext.is_empty()
            && alias(&ext) != alias(detected_ext)
            && !ext.starts_with(&format!("tar.{detected_ext}")),
        detector: "file-format".into(),
    };
    #[cfg(feature = "magika")]
    if ambiguous && let Err(error) = magika_fallback(&prefix, report) {
        report.error("magika", error);
    }
    Ok(())
}

/// Identifies one file for the content-free v2 metadata path.
///
/// Accepts a path and lowercase extension hint. Returns format labels from a
/// bounded 65,536-byte prefix. File open/read failures are returned; no body
/// parser, OCR engine, or optional statistical fallback is invoked.
pub(crate) fn identify(
    path: &Path,
    extension: &str,
) -> std::io::Result<crate::reports::FormatIdentity> {
    let mut prefix = Vec::new();
    File::open(path)?.take(65_536).read_to_end(&mut prefix)?;
    let format = FileFormat::from_bytes(&prefix);
    let detected_ext = format.extension();
    let detected_bucket = bucket(detected_ext);
    let ambiguous = detected_bucket == "Other" || matches!(format, FileFormat::PlainText);
    let chosen_bucket = if ambiguous {
        bucket(extension)
    } else {
        detected_bucket
    };
    let alias = |value: &str| {
        match value {
            "jpeg" => "jpg",
            "tiff" => "tif",
            "sqlite3" | "db" => "sqlite",
            "htm" => "html",
            "cbz" => "zip",
            "cbr" => "rar",
            other => other,
        }
        .to_string()
    };
    Ok(crate::reports::FormatIdentity {
        extension: extension.to_owned(),
        detected_mime: Some(format.media_type().into()),
        detected_name: Some(format.name().into()),
        bucket: chosen_bucket.into(),
        basis: if ambiguous { "extension" } else { "bytes" }.into(),
        disagreement: !ambiguous
            && !extension.is_empty()
            && alias(extension) != alias(detected_ext)
            && !extension.starts_with(&format!("tar.{detected_ext}")),
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
