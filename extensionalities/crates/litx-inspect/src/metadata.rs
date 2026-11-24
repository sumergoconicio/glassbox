//! Module: glassbox-inspect metadata-only dispatch.
//! File: metadata.rs.
//! Responsibility: Populate v2 reports from identity, properties, and structure without body extraction or OCR.

use crate::reports::*;
use crate::{InspectReport, binary::Source};
use std::collections::BTreeMap;
use std::io::{BufRead, Read, Seek, Write};
use std::path::Path;

type ReaderResult<T> = Result<T, Box<dyn std::error::Error>>;

/// Adds one qualified native-unit count to a metadata report.
///
/// Accepts the destination metadata, unit, optional value, source, and
/// completeness. Returns unit after appending the count. No I/O or failure
/// states occur; unavailable counts use `None` rather than an invented zero.
pub(crate) fn add_count(
    metadata: &mut DocumentMetadata,
    unit: NativeUnit,
    value: Option<u64>,
    source: FactSource,
    completeness: Completeness,
) {
    metadata.counts.push(QualifiedCount {
        unit,
        value,
        source,
        completeness,
    });
}

/// Adds one schema identifier without reading or publishing its value.
///
/// Accepts metadata, identifier class, user/schema name, and source. Returns
/// unit. This helper cannot fail and never inspects document body values.
pub(crate) fn add_schema(
    metadata: &mut DocumentMetadata,
    kind: SchemaKind,
    name: impl Into<String>,
    source: FactSource,
) {
    metadata.schema_identifiers.push(SchemaIdentifier {
        kind,
        name: name.into(),
        source,
    });
}

/// Adds one allowlisted sourced embedded property.
///
/// Accepts metadata, property field, property value, and completeness. Returns
/// unit. The caller must select only metadata properties, never body text.
pub(crate) fn add_property(
    metadata: &mut DocumentMetadata,
    field: EmbeddedMetadataField,
    value: impl Into<String>,
    completeness: Completeness,
) {
    metadata.embedded_metadata.push(SourcedMetadata {
        field,
        value: value.into(),
        source: FactSource::EmbeddedProperty,
        completeness,
    });
}

/// Adds one structured content-free diagnostic.
///
/// Accepts metadata, stage, severity, stable code, message, and optional unit.
/// Returns unit. Callers must keep parser values out of error messages.
pub(crate) fn add_diagnostic(
    metadata: &mut DocumentMetadata,
    stage: DiagnosticStage,
    severity: DiagnosticSeverity,
    code: &str,
    message: impl ToString,
    unit_index: Option<u64>,
) {
    metadata.diagnostics.push(Diagnostic {
        stage,
        severity,
        code: code.to_owned(),
        message: message.to_string(),
        unit_index,
    });
}

/// Records the structural reader that supplied metadata.
///
/// Accepts metadata, reader name, stage name, and source. Returns unit after
/// appending provenance. This helper performs no I/O and cannot fail.
pub(crate) fn add_provenance(
    metadata: &mut DocumentMetadata,
    reader: &str,
    stage: &str,
    source: FactSource,
) {
    metadata.provenance.push(MetadataProvenance {
        reader: reader.to_owned(),
        stage: stage.to_owned(),
        source,
    });
}

/// Marks structural or artifact coverage partial without hiding existing facts.
///
/// Accepts mutable metadata. Returns unit after setting partial status unless
/// an operational failure already owns the outcome. No errors occur.
pub(crate) fn mark_partial(metadata: &mut DocumentMetadata) {
    metadata.completeness = Completeness::Partial;
    if metadata.status != OperationStatus::Failed {
        metadata.status = OperationStatus::Partial;
    }
}

/// Sets the concrete format identity after package or header confirmation.
///
/// Accepts metadata plus canonical confirmed extension, MIME, display name,
/// bucket, and detector. Returns unit while preserving the literal normalized
/// filename suffix in `metadata.format.extension` and recomputing extension
/// mismatch against the confirmed format family.
fn identify(
    metadata: &mut DocumentMetadata,
    canonical_extension: &str,
    mime: &str,
    name: &str,
    bucket: &str,
    detector: &str,
) {
    let (_, disagreement) =
        crate::detect::format_type(None, Some(canonical_extension), &metadata.format.extension);
    metadata.format.detected_mime = Some(mime.to_owned());
    metadata.format.detected_name = Some(name.to_owned());
    metadata.format.bucket = bucket.to_owned();
    metadata.format.basis = "bytes".to_owned();
    metadata.format.disagreement = disagreement;
    metadata.format.detector = detector.to_owned();
}

/// Copies identification produced by a header-only legacy structural reader.
///
/// Accepts destination and source format records. Returns unit after copying
/// detected labels while preserving the literal normalized filename suffix in
/// `metadata.format.extension` and recomputing extension mismatch from the
/// reader-confirmed format bucket against the literal suffix. Performs no I/O
/// and cannot fail.
fn copy_format(metadata: &mut DocumentMetadata, format: &crate::Format) {
    let literal_extension = metadata.format.extension.clone();
    let detected_type = crate::detect::bucket_to_format_type(&format.bucket);
    let literal_type = crate::detect::format_type_for_extension(&literal_extension);
    let disagreement = format.basis == "bytes"
        && !literal_extension.is_empty()
        && literal_type != "other"
        && literal_type != detected_type;
    metadata.format = FormatIdentity {
        extension: literal_extension,
        detected_mime: format.detected_mime.clone(),
        detected_name: format.detected_name.clone(),
        bucket: format.bucket.clone(),
        basis: format.basis.clone(),
        disagreement,
        detector: format.detector.clone(),
    };
}

/// Creates an empty transitional shell for readers that only list structure.
///
/// Accepts an extension hint. Returns a legacy-typed shell used solely by the
/// bounded ZIP/RAR directory readers. No body reader or OCR session is called.
fn structural_shell(extension: &str) -> InspectReport {
    let mut report = InspectReport::default();
    report.format.extension = extension.to_owned();
    report
}

/// Decodes and writes one explicitly requested thumbnail with create-new semantics.
///
/// Accepts an encoded image reader, thumbnail options, and artifact source.
/// Returns path and verified dimensions in a typed artifact. Invalid edges,
/// oversized input, decode/encode errors, existing paths, and write/sync
/// failures are returned. No OCR runs, and ordinary inspection never calls this.
pub(crate) fn write_thumbnail<R: BufRead + Seek>(
    input: R,
    options: &ThumbnailOptions,
    source: FactSource,
) -> ReaderResult<Artifact> {
    if !(1..=4096).contains(&options.max_edge) {
        return Err("thumbnail edge must be between 1 and 4096".into());
    }
    let mut bytes = Vec::new();
    input
        .take(crate::archive::MAX_ENTRY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > crate::archive::MAX_ENTRY_BYTES {
        return Err("thumbnail encoded-byte read cap exceeded".into());
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(128 << 20);
    reader.limits(limits);
    let decoded = reader.decode()?;
    let image = if decoded.width().max(decoded.height()) <= options.max_edge {
        decoded
    } else {
        decoded.thumbnail(options.max_edge, options.max_edge)
    };
    let mut encoded = std::io::Cursor::new(Vec::new());
    image.write_to(&mut encoded, image::ImageFormat::Png)?;
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let mut file = open.open(&options.path)?;
    file.write_all(encoded.get_ref())?;
    file.sync_all()?;
    Ok(Artifact {
        kind: ArtifactKind::Thumbnail,
        path: options.path.clone(),
        source,
        completeness: Completeness::Exact,
        width: Some(image.width()),
        height: Some(image.height()),
    })
}

/// Handles an explicitly requested thumbnail result without hiding failures.
///
/// Accepts metadata and the artifact result. Returns unit after appending the
/// artifact or a warning and partial status. Existing paths remain untouched.
pub(crate) fn thumbnail_result(metadata: &mut DocumentMetadata, result: ReaderResult<Artifact>) {
    match result {
        Ok(artifact) => metadata.artifacts.push(artifact),
        Err(error) => {
            add_diagnostic(
                metadata,
                DiagnosticStage::Thumbnail,
                DiagnosticSeverity::Warning,
                "thumbnail_failed",
                error,
                None,
            );
            mark_partial(metadata);
        }
    }
}

/// Withholds digest and fingerprint and marks metadata partial when the source
/// file changes or is replaced between hashing and subsequent reads.
///
/// Accepts mutable [`DocumentMetadata`]. Returns unit after clearing `digest`
/// and `fingerprint`, marking consistency `SourceChanged`, appending a single
/// `source_changed_during_read` warning diagnostic, and downgrading status to
/// `Partial` (unless already `Failed`). Performs no I/O and cannot fail.
pub(crate) fn record_source_changed_during_read(metadata: &mut DocumentMetadata) {
    metadata.digest.digest = None;
    metadata.digest.fingerprint = None;
    metadata.digest.consistency = ConsistencyStatus::SourceChanged;
    metadata.digest.source_identity = ConsistencyStatus::SourceChanged;
    metadata.identity.consistency = ConsistencyStatus::SourceChanged;
    if !metadata
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "source_changed_during_read")
    {
        add_diagnostic(
            metadata,
            DiagnosticStage::Hashing,
            DiagnosticSeverity::Warning,
            "source_changed_during_read",
            "source file changed between hashing and reading; digest and fingerprint withheld",
            None,
        );
    }
    mark_partial(metadata);
}

/// Inspects one file through the canonical content-free metadata path using the default hash policy.
///
/// Accepts a source path and metadata-only options. Returns a metadata report
/// with streamed identity (full SHA-256 up to threshold, sampled fingerprint
/// above it), structure, properties, native signals, diagnostics, and explicitly
/// requested artifacts. Missing/invalid files, parser failures, structural caps,
/// and source mutation are qualified; no body reader or OCR engine is invoked.
pub(crate) fn inspect(path: &Path, options: &InspectOptions) -> MetadataReport {
    inspect_with_policy(
        path,
        options,
        crate::hashing::HashPolicy::SampledAboveThreshold,
    )
}

/// Inspects one file through the canonical content-free metadata path with an explicit [`crate::hashing::HashPolicy`].
///
/// Accepts a source path, metadata-only options, and a hash policy controlling
/// full SHA-256 versus sampled fingerprinting. Returns a metadata report with
/// identity, structure, embedded properties, diagnostics, and optional artifacts.
/// Input, hashing, detection, and structural failures are recorded as diagnostics;
/// no body reader or OCR engine is invoked.
pub(crate) fn inspect_with_policy(
    path: &Path,
    options: &InspectOptions,
    policy: crate::hashing::HashPolicy,
) -> MetadataReport {
    inspect_with_policy_and_observation(path, options, policy).0
}

/// Inspects one file through the canonical content-free metadata path and
/// returns both the metadata report and the initial opened-handle identity
/// observation for downstream read-binding checks.
///
/// Accepts a source path, metadata-only options, and an explicit [`crate::hashing::HashPolicy`].
/// Binds detection and structural readers to the validated symlink-free path
/// returned by hashing, and re-verifies source identity after metadata dispatch
/// so concurrent replacement withholds digest/fingerprint with
/// `source_changed_during_read`. Returns `(MetadataReport, Option<Observation>)`.
pub(crate) fn inspect_with_policy_and_observation(
    path: &Path,
    options: &InspectOptions,
    policy: crate::hashing::HashPolicy,
) -> (MetadataReport, Option<crate::hashing::IdentityObservation>) {
    inspect_with_policy_and_hook(path, options, policy, || {})
}

/// Runs metadata-only inspection with a post-dispatch hook for deterministic
/// concurrent-replacement testing.
///
/// Accepts a source path, metadata options, hash policy, and a closure executed
/// immediately after structural dispatch and before the post-read identity
/// recheck. Production callers pass a no-op closure. Returns the metadata
/// report and initial identity observation.
fn inspect_with_policy_and_hook(
    path: &Path,
    options: &InspectOptions,
    policy: crate::hashing::HashPolicy,
    before_recheck: impl FnOnce(),
) -> (MetadataReport, Option<crate::hashing::IdentityObservation>) {
    let mut report = MetadataReport::default();
    let hashed = crate::hashing::hash_file_with_policy(path, policy);
    let validated_path = hashed.identity.path.clone();
    let expected_observation = hashed.observation;
    report.metadata.identity = hashed.identity;
    report.metadata.digest = hashed.digest;
    report.metadata.diagnostics.extend(hashed.diagnostics);
    if !hashed.input_valid {
        report.metadata.status = OperationStatus::Failed;
        return (report, None);
    }
    let extension = crate::detect::extension(&validated_path);
    match crate::detect::identify(&validated_path, &extension) {
        Ok(format) => report.metadata.format = format,
        Err(error) => {
            add_diagnostic(
                &mut report.metadata,
                DiagnosticStage::Detection,
                DiagnosticSeverity::Error,
                "detection_failed",
                error,
                None,
            );
            report.metadata.status = OperationStatus::Failed;
            return (report, expected_observation);
        }
    }
    if let Err(error) = dispatch(&validated_path, options, &mut report.metadata) {
        reader_error(&mut report.metadata, error);
    }
    before_recheck();
    if let Some(ref expected) = expected_observation
        && !crate::hashing::verify_identity(&validated_path, expected)
    {
        record_source_changed_during_read(&mut report.metadata);
    }
    finalize(&mut report.metadata);
    (report, expected_observation)
}

/// Reports whether the compiled raster header/pixel reader supports decoding `mime` or `extension`.
///
/// Accepts the detected MIME type (empty when absent), literal normalized
/// filename suffix, and detection basis (`"bytes"` or `"extension"`). Returns
/// `true` only when the compiled `image` crate enables reading for the resolved
/// raster format. Extension fallback runs only when `basis == "extension"` and
/// `mime` is absent or generic (`application/octet-stream` / `text/plain`), so
/// a byte-confirmed unsupported raster (such as a PSD named `.png`) never enters
/// the decoder. Formats classified as `"images"` without a compiled decoder
/// (such as PSD, HEIC, or AVIF without an AVIF decoder feature) return `false`.
/// Performs no I/O and cannot fail.
pub(crate) fn is_supported_raster_format(mime: &str, extension: &str, basis: &str) -> bool {
    let from_mime = if mime.is_empty() {
        None
    } else {
        image::ImageFormat::from_mime_type(mime).or(match mime {
            "image/x-bmp" | "image/x-ms-bmp" => Some(image::ImageFormat::Bmp),
            "image/x-icon" | "image/vnd.microsoft.icon" => Some(image::ImageFormat::Ico),
            "image/x-portable-bitmap"
            | "image/x-portable-graymap"
            | "image/x-portable-pixmap"
            | "image/x-portable-anymap" => Some(image::ImageFormat::Pnm),
            "image/x-tga" | "image/x-targa" => Some(image::ImageFormat::Tga),
            "image/vnd.ms-dds" | "image/vnd-ms.dds" => Some(image::ImageFormat::Dds),
            "image/x-exr" => Some(image::ImageFormat::OpenExr),
            "image/vnd.radiance" => Some(image::ImageFormat::Hdr),
            "image/x-farbfeld" => Some(image::ImageFormat::Farbfeld),
            "image/qoi" => Some(image::ImageFormat::Qoi),
            _ => None,
        })
    };
    if let Some(format) = from_mime {
        return format.reading_enabled();
    }
    if basis == "extension"
        && (mime.is_empty() || matches!(mime, "application/octet-stream" | "text/plain"))
        && !extension.is_empty()
        && let Some(format) = image::ImageFormat::from_extension(extension)
    {
        return format.reading_enabled();
    }
    false
}

/// Dispatches to one dedicated structural metadata reader.
///
/// Accepts path, options, and metadata with identity and format already set.
/// Returns unit after populating metadata or a parser/I/O error. Unsupported
/// formats return an explicit unsupported status rather than fabricated facts.
fn dispatch(
    path: &Path,
    options: &InspectOptions,
    metadata: &mut DocumentMetadata,
) -> ReaderResult<()> {
    let extension = metadata.format.extension.clone();
    let mime = metadata.format.detected_mime.as_deref().unwrap_or("");
    let zip = matches!(
        mime,
        "application/zip"
            | "application/epub+zip"
            | "application/vnd.ms-excel.sheet.macroEnabled.12"
    ) || mime.starts_with("application/vnd.openxmlformats-officedocument.")
        || mime.starts_with("application/vnd.oasis.opendocument.")
        || mime.starts_with("application/vnd.apple.")
        || (metadata.format.basis == "extension"
            && matches!(
                extension.as_str(),
                "zip"
                    | "cbz"
                    | "epub"
                    | "docx"
                    | "xlsx"
                    | "xlsm"
                    | "pptx"
                    | "odt"
                    | "ods"
                    | "odp"
                    | "pages"
                    | "numbers"
                    | "key"
            ));
    if zip {
        inspect_archive(path, options, metadata)
    } else if mime.contains("mobipocket")
        || mime == "application/vnd.amazon.ebook"
        || (metadata.format.basis == "extension"
            && matches!(extension.as_str(), "mobi" | "azw" | "azw3" | "prc"))
    {
        inspect_mobi(path, options, metadata)
    } else if mime.contains("djvu")
        || (metadata.format.basis == "extension" && matches!(extension.as_str(), "djvu" | "djv"))
    {
        metadata.extraction_support = SupportStatus::IdentificationOnly;
        metadata.status = OperationStatus::Unsupported;
        metadata.completeness = Completeness::Exact;
        add_diagnostic(
            metadata,
            DiagnosticStage::Metadata,
            DiagnosticSeverity::Info,
            "djvu_identification_only",
            "DjVu extraction is unsupported; identification only",
            None,
        );
        add_provenance(
            metadata,
            "djvu-identity",
            "static-boundary",
            FactSource::NativeStructure,
        );
        Ok(())
    } else if matches!(mime, "application/vnd.rar" | "application/x-rar-compressed")
        || (metadata.format.basis == "extension" && matches!(extension.as_str(), "cbr" | "rar"))
    {
        inspect_rar(path, options, metadata)
    } else if mime == "application/pdf"
        || (metadata.format.basis == "extension" && extension == "pdf")
    {
        crate::pdf::metadata(path, metadata, options.thumbnail.as_ref())
    } else if metadata.format.bucket == "Images" {
        if is_supported_raster_format(mime, &extension, &metadata.format.basis) {
            inspect_image(path, options, metadata)
        } else {
            metadata.extraction_support = SupportStatus::Unsupported;
            metadata.status = OperationStatus::Unsupported;
            metadata.completeness = Completeness::Unavailable;
            add_diagnostic(
                metadata,
                DiagnosticStage::Metadata,
                DiagnosticSeverity::Info,
                "unsupported_image_format",
                "image format identification is available; raster decoder is unsupported",
                None,
            );
            Ok(())
        }
    } else if mime == "application/vnd.apache.parquet"
        || (metadata.format.basis == "extension" && extension == "parquet")
    {
        metadata.extraction_support = SupportStatus::IdentificationOnly;
        metadata.status = OperationStatus::Unsupported;
        metadata.completeness = Completeness::Exact;
        add_diagnostic(
            metadata,
            DiagnosticStage::Metadata,
            DiagnosticSeverity::Info,
            "parquet_identification_only",
            "Parquet extraction is deferred; identification only",
            None,
        );
        add_provenance(
            metadata,
            "parquet-identity",
            "static-boundary",
            FactSource::NativeStructure,
        );
        Ok(())
    } else if (metadata.format.basis == "extension" && matches!(extension.as_str(), "csv" | "tsv"))
        || matches!(mime, "text/csv" | "text/tab-separated-values")
    {
        inspect_csv(path, metadata)
    } else if mime == "application/json"
        || (metadata.format.basis == "extension" && extension == "json")
    {
        inspect_json(path, metadata)
    } else if matches!(mime, "application/xml" | "text/xml")
        || (metadata.format.basis == "extension" && extension == "xml")
    {
        inspect_xml(path, metadata)
    } else if metadata.format.basis == "extension"
        && matches!(metadata.format.bucket.as_str(), "Text" | "Source Code")
    {
        inspect_text(path, metadata)
    } else {
        metadata.extraction_support = SupportStatus::Unsupported;
        metadata.status = OperationStatus::Unsupported;
        metadata.completeness = Completeness::Unavailable;
        add_diagnostic(
            metadata,
            DiagnosticStage::Metadata,
            DiagnosticSeverity::Info,
            "unsupported_format",
            "format identification is available; metadata reader is unsupported",
            None,
        );
        Ok(())
    }
}

/// Converts a reader error into an operational failure or qualified cap result.
///
/// Accepts metadata and the reader error. Returns unit after recording a
/// content-free diagnostic. Cap exhaustion preserves partial metadata; other
/// parser and I/O failures mark the operation failed.
fn reader_error(metadata: &mut DocumentMetadata, error: Box<dyn std::error::Error>) {
    let message = error.to_string();
    let capped = message.contains("cap exceeded") || message.contains("budget exceeded");
    add_diagnostic(
        metadata,
        DiagnosticStage::Metadata,
        if capped {
            DiagnosticSeverity::Warning
        } else {
            DiagnosticSeverity::Error
        },
        if capped {
            "metadata_cap_exceeded"
        } else {
            "metadata_reader_failed"
        },
        message,
        None,
    );
    if capped {
        mark_partial(metadata);
    } else {
        metadata.status = OperationStatus::Failed;
        metadata.completeness = Completeness::Partial;
    }
}

/// Applies aggregate hashing and reader outcomes after successful dispatch.
///
/// Accepts mutable metadata. Returns unit after assigning complete/partial/
/// unsupported/failed status. Failed hashing and thumbnail generation are
/// partial; operational reader errors remain failures.
fn finalize(metadata: &mut DocumentMetadata) {
    if metadata.digest.consistency != ConsistencyStatus::Consistent
        && metadata.status != OperationStatus::Failed
    {
        mark_partial(metadata);
    }
    if metadata.status == OperationStatus::Unknown {
        metadata.status = OperationStatus::Complete;
        if metadata.completeness == Completeness::Unavailable {
            metadata.completeness = Completeness::Exact;
        }
    }
}

/// Reads a bounded ZIP directory and dispatches package-specific structure.
///
/// Accepts path, metadata options, and destination metadata. Returns unit after
/// archive entry names, counts, package properties, and optional thumbnail.
/// Directory, XML, property, and entry-read failures are returned without body
/// previews, worksheet values, XHTML text, or OCR. Office, EPUB, and iWork
/// packages keep their own support classification; generic ZIP listings report
/// identification-only support because extraction produces no content units.
fn inspect_archive(
    path: &Path,
    options: &InspectOptions,
    metadata: &mut DocumentMetadata,
) -> ReaderResult<()> {
    let mut shell = structural_shell(&metadata.format.extension);
    let mut archive = crate::archive::Archive::open(path, &mut shell)?;
    for error in shell.errors {
        add_diagnostic(
            metadata,
            DiagnosticStage::Metadata,
            DiagnosticSeverity::Warning,
            "archive_name_warning",
            error.message,
            None,
        );
    }
    add_count(
        metadata,
        NativeUnit::ArchiveEntry,
        shell.counts.entries.map(|v| v as u64),
        FactSource::ContainerIndex,
        Completeness::Exact,
    );
    for entry in &shell.entries {
        add_schema(
            metadata,
            SchemaKind::ArchiveEntry,
            entry.name.clone(),
            FactSource::ContainerIndex,
        );
    }
    if inspect_office_package(&mut archive, options, metadata)? {
        return Ok(());
    }
    if inspect_epub(&mut archive, options, metadata)? {
        return Ok(());
    }
    if inspect_iwork(&mut archive, options, metadata)? {
        return Ok(());
    }
    if inspect_cbz(&mut archive, options, metadata)? {
        return Ok(());
    }
    // Every specialized ZIP package reader (Office/ODF, EPUB, iWork, CBZ)
    // declined this archive, so byte inspection confirms it is a generic ZIP
    // archive rather than the family implied by any misleading container suffix.
    identify(
        metadata,
        "zip",
        "application/zip",
        "ZIP",
        "Archives",
        "zip-directory",
    );
    // COND-07: a generic ZIP listing identifies entries but the reader produces
    // no native content units, so the support fact must agree with actual
    // extraction: identification only, exactly like the documented "no content
    // units" boundary. Office/EPUB packages keep their own supported paths.
    metadata.extraction_support = SupportStatus::IdentificationOnly;
    add_diagnostic(
        metadata,
        DiagnosticStage::Metadata,
        DiagnosticSeverity::Info,
        "zip_listing_identification_only",
        "generic ZIP entry listing supports identification only; no native content units are extracted",
        None,
    );
    add_provenance(
        metadata,
        "zip-directory",
        "central-directory",
        FactSource::ContainerIndex,
    );
    if let Some(thumbnail) = &options.thumbnail {
        for name in [
            "docProps/thumbnail.jpeg",
            "docProps/thumbnail.png",
            "Thumbnails/thumbnail.png",
        ] {
            if let Some(bytes) = archive.read(name)? {
                let result = write_thumbnail(
                    std::io::Cursor::new(bytes),
                    thumbnail,
                    FactSource::EmbeddedProperty,
                )
                .map_err(|error| error.to_string().into());
                thumbnail_result(metadata, result);
                break;
            }
        }
    }
    Ok(())
}

/// Maps one package-property node to the allowlisted v2 metadata field.
///
/// Accepts namespace and local property name. Returns the permitted field or
/// `None` for body, row, statistic, and unrecognised properties. No values are
/// read and no failure states occur.
fn property_field(namespace: &str, name: &str) -> Option<EmbeddedMetadataField> {
    const DC: &str = "http://purl.org/dc/elements/1.1/";
    const DCT: &str = "http://purl.org/dc/terms/";
    const META: &str = "urn:oasis:names:tc:opendocument:xmlns:meta:1.0";
    const APP: &str = "http://schemas.openxmlformats.org/officeDocument/2006/extended-properties";
    match (namespace, name) {
        (DC, "title") => Some(EmbeddedMetadataField::Title),
        (DC, "subject") => Some(EmbeddedMetadataField::Subject),
        (DC, "creator") | (META, "initial-creator") => Some(EmbeddedMetadataField::Author),
        (DC, "language") => Some(EmbeddedMetadataField::Language),
        (DCT, "created") | (META, "creation-date") => Some(EmbeddedMetadataField::CreatedAt),
        (DCT, "modified") => Some(EmbeddedMetadataField::ModifiedAt),
        (_, "keywords") => Some(EmbeddedMetadataField::Keywords),
        (_, "creator") => Some(EmbeddedMetadataField::Creator),
        (_, "producer") => Some(EmbeddedMetadataField::Producer),
        (APP, "Application") => Some(EmbeddedMetadataField::Application),
        _ => None,
    }
}

/// Reads only allowlisted Office, ODF, or EPUB embedded properties.
///
/// Accepts an open archive and destination metadata. Returns unit after adding
/// sourced properties. Missing property parts are optional; malformed present
/// XML and entry-cap errors are returned. No document body part is visited.
fn office_properties(
    archive: &mut crate::archive::Archive,
    metadata: &mut DocumentMetadata,
) -> ReaderResult<()> {
    for name in ["docProps/core.xml", "docProps/app.xml", "meta.xml"] {
        if !archive.names.iter().any(|entry| entry == name) {
            continue;
        }
        let root = crate::office::read_xml(archive, name)?;
        root.visit(&mut |node| {
            if node.children().next().is_some() {
                return;
            }
            if let Some(field) = property_field(&node.ns, &node.name) {
                let value = node.text();
                if !value.trim().is_empty() {
                    add_property(metadata, field, value, Completeness::Exact);
                }
            }
        });
    }
    Ok(())
}

/// Inspects OOXML and ODF package structure without extracting document text.
///
/// Accepts archive, options, and metadata. Returns `true` when a supported
/// package was identified, `false` for another ZIP family, or a structural
/// error. Counts come from elements and workbook/slide declarations, never
/// rendered text, table cells, formulas, or LibreOffice conversion.
fn inspect_office_package(
    archive: &mut crate::archive::Archive,
    options: &InspectOptions,
    metadata: &mut DocumentMetadata,
) -> ReaderResult<bool> {
    if archive.names.iter().any(|name| name == "mimetype") {
        let bytes = archive.read("mimetype")?.ok_or("missing ODF mimetype")?;
        let mime = std::str::from_utf8(&bytes)?.trim();
        let (extension, body_kind, name, bucket) = match mime {
            "application/vnd.oasis.opendocument.text" => {
                ("odt", "text", "OpenDocument text", "Office Docs")
            }
            "application/vnd.oasis.opendocument.presentation" => (
                "odp",
                "presentation",
                "OpenDocument presentation",
                "Presentations",
            ),
            "application/vnd.oasis.opendocument.spreadsheet" => (
                "ods",
                "spreadsheet",
                "OpenDocument spreadsheet",
                "Spreadsheets",
            ),
            _ => return Ok(false),
        };
        let root = crate::office::read_xml(archive, "content.xml")?;
        if !root.is(crate::odf::O, "document-content") {
            return Err("invalid ODF content namespace/root".into());
        }
        let body = root
            .children()
            .find(|node| node.is(crate::odf::O, "body"))
            .and_then(|node| {
                node.children()
                    .find(|node| node.is(crate::odf::O, body_kind))
            })
            .ok_or("ODF body does not match mimetype")?;
        identify(metadata, extension, mime, name, bucket, "package-mimetype");
        office_properties(archive, metadata)?;
        match extension {
            "odt" => {
                add_count(
                    metadata,
                    NativeUnit::Paragraph,
                    Some(
                        (body.descendants(crate::odf::T, "p").len()
                            + body.descendants(crate::odf::T, "h").len())
                            as u64,
                    ),
                    FactSource::NativeStructure,
                    Completeness::Exact,
                );
                add_count(
                    metadata,
                    NativeUnit::Table,
                    Some(body.descendants(crate::odf::TABLE, "table").len() as u64),
                    FactSource::NativeStructure,
                    Completeness::Exact,
                );
            }
            "odp" => add_count(
                metadata,
                NativeUnit::Slide,
                Some(
                    body.children()
                        .filter(|node| {
                            node.is("urn:oasis:names:tc:opendocument:xmlns:drawing:1.0", "page")
                        })
                        .count() as u64,
                ),
                FactSource::NativeStructure,
                Completeness::Exact,
            ),
            "ods" => {
                let sheets: Vec<_> = body
                    .children()
                    .filter(|node| node.is(crate::odf::TABLE, "table"))
                    .collect();
                add_count(
                    metadata,
                    NativeUnit::Worksheet,
                    Some(sheets.len() as u64),
                    FactSource::NativeStructure,
                    Completeness::Exact,
                );
                for sheet in sheets {
                    if let Some(name) = sheet.attr_ns(crate::odf::TABLE, "name") {
                        add_schema(
                            metadata,
                            SchemaKind::Worksheet,
                            name,
                            FactSource::NativeStructure,
                        );
                    }
                }
            }
            _ => unreachable!(),
        }
        metadata.extraction_support = SupportStatus::Supported;
        add_provenance(
            metadata,
            "odf-structure",
            "package-xml",
            FactSource::NativeStructure,
        );
        embedded_package_thumbnail(archive, options, metadata)?;
        return Ok(true);
    }
    if !archive
        .names
        .iter()
        .any(|name| name == "[Content_Types].xml")
    {
        return Ok(false);
    }
    const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
    let types = crate::office::read_xml(archive, "[Content_Types].xml")?;
    if !types.is(CT, "Types") {
        return Err("invalid OOXML content types root".into());
    }
    let mut main = None;
    for node in types.children().filter(|node| node.is(CT, "Override")) {
        let kind = match node.attr("ContentType").unwrap_or("") {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml" => {
                Some((
                    "docx",
                    crate::office::W,
                    "document",
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                    "Word document",
                    "Office Docs",
                ))
            }
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml" => {
                Some((
                    "pptx",
                    crate::office::P,
                    "presentation",
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                    "PowerPoint presentation",
                    "Presentations",
                ))
            }
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml" => Some((
                "xlsx",
                crate::office::S,
                "workbook",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "Excel workbook",
                "Spreadsheets",
            )),
            "application/vnd.ms-excel.sheet.macroEnabled.main+xml" => Some((
                "xlsm",
                crate::office::S,
                "workbook",
                "application/vnd.ms-excel.sheet.macroEnabled.12",
                "Excel workbook",
                "Spreadsheets",
            )),
            _ => None,
        };
        if let Some(kind) = kind {
            if main.is_some() {
                return Err("ambiguous OOXML main parts".into());
            }
            let part = node
                .attr("PartName")
                .ok_or("main part has no name")?
                .trim_start_matches('/')
                .to_owned();
            main = Some((kind, part));
        }
    }
    let Some(((extension, namespace, root_name, mime, name, bucket), part)) = main else {
        return Ok(false);
    };
    if part
        .split('/')
        .any(|component| matches!(component, ".." | "." | ""))
        || part.contains('\\')
    {
        return Err("unsafe OOXML main part name".into());
    }
    let root = crate::office::read_xml(archive, &part)?;
    if !root.is(namespace, root_name) {
        return Err("OOXML main part namespace/root mismatch".into());
    }
    identify(
        metadata,
        extension,
        mime,
        name,
        bucket,
        "package-content-types",
    );
    office_properties(archive, metadata)?;
    match extension {
        "docx" => {
            add_count(
                metadata,
                NativeUnit::Paragraph,
                Some(root.descendants(crate::office::W, "p").len() as u64),
                FactSource::NativeStructure,
                Completeness::Exact,
            );
            add_count(
                metadata,
                NativeUnit::Table,
                Some(root.descendants(crate::office::W, "tbl").len() as u64),
                FactSource::NativeStructure,
                Completeness::Exact,
            );
        }
        "pptx" => {
            add_count(
                metadata,
                NativeUnit::Slide,
                Some(root.descendants(crate::office::P, "sldId").len() as u64),
                FactSource::NativeStructure,
                Completeness::Exact,
            );
            add_count(
                metadata,
                NativeUnit::Table,
                Some(root.descendants(crate::office::A, "tbl").len() as u64),
                FactSource::NativeStructure,
                Completeness::Exact,
            );
        }
        "xlsx" | "xlsm" => {
            let sheets = root.descendants(crate::office::S, "sheet");
            add_count(
                metadata,
                NativeUnit::Worksheet,
                Some(sheets.len() as u64),
                FactSource::NativeStructure,
                Completeness::Exact,
            );
            for sheet in sheets {
                if let Some(name) = sheet.attr("name") {
                    add_schema(
                        metadata,
                        SchemaKind::Worksheet,
                        name,
                        FactSource::NativeStructure,
                    );
                }
            }
        }
        _ => unreachable!(),
    }
    metadata.extraction_support = SupportStatus::Supported;
    add_provenance(
        metadata,
        "ooxml-structure",
        "package-xml",
        FactSource::NativeStructure,
    );
    embedded_package_thumbnail(archive, options, metadata)?;
    Ok(true)
}

/// Writes an allowlisted embedded package thumbnail only when requested.
///
/// Accepts archive, options, and metadata. Returns unit after recording an
/// artifact or qualified failure. Entry-read and image errors are returned;
/// no thumbnail path is touched when `options.thumbnail` is absent.
fn embedded_package_thumbnail(
    archive: &mut crate::archive::Archive,
    options: &InspectOptions,
    metadata: &mut DocumentMetadata,
) -> ReaderResult<()> {
    let Some(thumbnail) = &options.thumbnail else {
        return Ok(());
    };
    for name in [
        "docProps/thumbnail.jpeg",
        "docProps/thumbnail.png",
        "Thumbnails/thumbnail.png",
    ] {
        if let Some(bytes) = archive.read(name)? {
            thumbnail_result(
                metadata,
                write_thumbnail(
                    std::io::Cursor::new(bytes),
                    thumbnail,
                    FactSource::EmbeddedProperty,
                ),
            );
            break;
        }
    }
    Ok(())
}

/// Resolves an EPUB package target without accessing external resources.
///
/// Accepts base part and target href. Returns a validated package-internal
/// path. Invalid percent encoding, external targets, and package-root escapes
/// produce errors; no network or filesystem access occurs.
fn epub_resolve(base: &str, target: &str) -> ReaderResult<String> {
    let target = target.split('#').next().unwrap_or("");
    let decoded = percent_encoding::percent_decode_str(target).decode_utf8()?;
    crate::office::resolve_part(base, &decoded)
}

/// Inspects EPUB OPF metadata and spine counts without reading spine documents.
///
/// Accepts archive, options, and metadata. Returns `true` for EPUB, `false`
/// for other ZIP files, or structural errors. XHTML, body text, and OCR are
/// never read; an optional cover is decoded only for an explicit thumbnail.
fn inspect_epub(
    archive: &mut crate::archive::Archive,
    options: &InspectOptions,
    metadata: &mut DocumentMetadata,
) -> ReaderResult<bool> {
    if !archive
        .names
        .iter()
        .any(|name| name == "META-INF/container.xml")
    {
        return Ok(false);
    }
    const CONTAINER: &str = "urn:oasis:names:tc:opendocument:xmlns:container";
    const OPF: &str = "http://www.idpf.org/2007/opf";
    const DC: &str = "http://purl.org/dc/elements/1.1/";
    let container = crate::office::read_xml(archive, "META-INF/container.xml")?;
    if !container.is(CONTAINER, "container") {
        return Err("invalid EPUB container namespace/root".into());
    }
    let package = container
        .descendants(CONTAINER, "rootfile")
        .into_iter()
        .find(|node| node.attr("media-type") == Some("application/oebps-package+xml"))
        .ok_or("EPUB has no OPF rootfile")?;
    let path = epub_resolve(
        "",
        package
            .attr("full-path")
            .ok_or("missing EPUB rootfile path")?,
    )?;
    let root = crate::office::read_xml(archive, &path)?;
    if !root.is(OPF, "package") {
        return Err("invalid OPF namespace/root".into());
    }
    identify(
        metadata,
        "epub",
        "application/epub+zip",
        "EPUB ebook",
        "Ebooks",
        "epub-opf",
    );
    let mut cover_id = None;
    if let Some(properties) = root.children().find(|node| node.is(OPF, "metadata")) {
        for node in properties.children() {
            if node.ns == DC
                && let Some(field) = property_field(&node.ns, &node.name)
            {
                let value = node.text();
                if !value.trim().is_empty() {
                    add_property(metadata, field, value, Completeness::Exact);
                }
            }
            if node.is(OPF, "meta") && node.attr("name") == Some("cover") {
                cover_id = node.attr("content");
            }
        }
    }
    let manifest = root
        .children()
        .find(|node| node.is(OPF, "manifest"))
        .ok_or("missing EPUB manifest")?;
    let mut items = BTreeMap::new();
    for item in manifest.children().filter(|node| node.is(OPF, "item")) {
        if items.len() >= 4096 {
            return Err("EPUB manifest item cap exceeded".into());
        }
        let id = item.attr("id").ok_or("manifest item has no id")?;
        if items.insert(id, item).is_some() {
            return Err("duplicate EPUB manifest id".into());
        }
    }
    let spine = root
        .children()
        .find(|node| node.is(OPF, "spine"))
        .ok_or("missing EPUB spine")?;
    let references: Vec<_> = spine
        .children()
        .filter(|node| node.is(OPF, "itemref"))
        .collect();
    if references.len() > 4096 {
        return Err("EPUB spine cap exceeded".into());
    }
    add_count(
        metadata,
        NativeUnit::SpineItem,
        Some(references.len() as u64),
        FactSource::ContainerIndex,
        Completeness::Exact,
    );
    metadata.extraction_support = SupportStatus::Supported;
    add_provenance(
        metadata,
        "epub-opf",
        "package-properties-and-spine",
        FactSource::ContainerIndex,
    );
    if let Some(thumbnail) = &options.thumbnail {
        let cover = cover_id.and_then(|id| items.get(id).copied()).or_else(|| {
            items.values().copied().find(|node| {
                node.attr("properties")
                    .unwrap_or("")
                    .split_whitespace()
                    .any(|property| property == "cover-image")
            })
        });
        if let Some(cover) = cover {
            let part = epub_resolve(&path, cover.attr("href").ok_or("missing cover href")?)?;
            let bytes = archive.read(&part)?.ok_or("missing EPUB cover part")?;
            thumbnail_result(
                metadata,
                write_thumbnail(
                    std::io::Cursor::new(bytes),
                    thumbnail,
                    FactSource::EmbeddedProperty,
                ),
            );
        }
    }
    Ok(true)
}

/// Reports iWork archive and derived-preview presence without preview extraction.
///
/// Accepts archive, options, and metadata. Returns `Ok(true)` for Pages,
/// Numbers, or Keynote packages containing a structural iWork marker, `Ok(false)`
/// for other extensions or generic ZIP archives lacking iWork markers (so
/// `inspect_archive` falls through to generic ZIP identification), or a reader
/// error if reading an explicitly requested thumbnail fails. Preview text/PDF
/// content is never parsed; only an explicitly requested image/PDF thumbnail is
/// decoded or rendered.
fn inspect_iwork(
    archive: &mut crate::archive::Archive,
    options: &InspectOptions,
    metadata: &mut DocumentMetadata,
) -> ReaderResult<bool> {
    let extension = metadata.format.extension.clone();
    if !matches!(extension.as_str(), "pages" | "numbers" | "key") {
        return Ok(false);
    }
    if !crate::iwork::has_iwork_marker(&archive.names) {
        return Ok(false);
    }
    let (name, bucket) = match extension.as_str() {
        "pages" => ("Apple Pages", "Office Docs"),
        "numbers" => ("Apple Numbers", "Spreadsheets"),
        _ => ("Apple Keynote", "Presentations"),
    };
    identify(
        metadata,
        &extension,
        &format!("application/vnd.apple.{extension}"),
        name,
        bucket,
        "iwork-package",
    );
    let preview = [
        "QuickLook/Preview.pdf",
        "QuickLook/Preview.txt",
        "QuickLook/Preview.png",
        "QuickLook/Preview.jpg",
        "preview.png",
        "preview.jpg",
        "QuickLook/Thumbnail.jpg",
    ]
    .into_iter()
    .find(|name| archive.names.iter().any(|entry| entry == name));
    if let Some(name) = preview {
        add_count(
            metadata,
            NativeUnit::DerivedPreviewUnit,
            Some(1),
            FactSource::DerivedPreview,
            Completeness::Partial,
        );
        add_schema(
            metadata,
            SchemaKind::Embedded,
            name,
            FactSource::DerivedPreview,
        );
        metadata.extraction_support = SupportStatus::Supported;
        if let Some(thumbnail) = &options.thumbnail {
            if name.ends_with(".pdf") {
                let bytes = archive.read(name)?.ok_or("missing iWork PDF preview")?;
                let result = crate::pdf::thumbnail_bytes_from(bytes).and_then(|bytes| {
                    write_thumbnail(
                        std::io::Cursor::new(bytes),
                        thumbnail,
                        FactSource::DerivedPreview,
                    )
                });
                thumbnail_result(metadata, result);
            } else if !name.ends_with(".txt") {
                let bytes = archive.read(name)?.ok_or("missing iWork image preview")?;
                thumbnail_result(
                    metadata,
                    write_thumbnail(
                        std::io::Cursor::new(bytes),
                        thumbnail,
                        FactSource::DerivedPreview,
                    ),
                );
            }
        }
    } else {
        metadata.extraction_support = SupportStatus::Unsupported;
        add_diagnostic(
            metadata,
            DiagnosticStage::Metadata,
            DiagnosticSeverity::Info,
            "iwork_preview_unavailable",
            "no supported embedded QuickLook preview; native IWA structure is unavailable",
            None,
        );
    }
    add_provenance(
        metadata,
        "iwork-archive",
        "derived-preview-index",
        FactSource::ContainerIndex,
    );
    Ok(true)
}

/// Counts CBZ image entries and optionally decodes the first cover.
///
/// Accepts archive, options, and metadata. Returns `Ok(true)` for `.cbz`
/// archives containing at least one non-macOS image entry, `Ok(false)` for
/// other extensions or generic ZIP archives lacking image entries (so
/// `inspect_archive` falls through to generic ZIP identification), or a reader
/// error if reading an explicitly requested cover thumbnail fails. Image
/// headers and pixels are untouched unless a thumbnail was explicitly
/// requested; no OCR or page text extraction occurs. The report classifies
/// extraction support as identification-only because entry names carry the page
/// facts and extraction produces no content units.
fn inspect_cbz(
    archive: &mut crate::archive::Archive,
    options: &InspectOptions,
    metadata: &mut DocumentMetadata,
) -> ReaderResult<bool> {
    if metadata.format.extension != "cbz" {
        return Ok(false);
    }
    let pages: Vec<String> = archive
        .names
        .iter()
        .filter(|name| {
            crate::comics::image_name(name)
                && !name
                    .split('/')
                    .any(|part| part == "__MACOSX" || part.starts_with("._"))
        })
        .cloned()
        .collect();
    if pages.is_empty() {
        return Ok(false);
    }
    identify(
        metadata,
        "cbz",
        "application/vnd.comicbook+zip",
        "ZIP comic book",
        "Comics",
        "zip-entry-names",
    );
    add_count(
        metadata,
        NativeUnit::PhysicalPage,
        Some(pages.len() as u64),
        FactSource::ContainerIndex,
        Completeness::Exact,
    );
    // COND-07: CBZ page facts come from image entry names, and the reader
    // produces no native content units on extraction, so the support fact
    // must agree with actual extraction: identification only.
    metadata.extraction_support = SupportStatus::IdentificationOnly;
    add_diagnostic(
        metadata,
        DiagnosticStage::Metadata,
        DiagnosticSeverity::Info,
        "cbz_listing_identification_only",
        "CBZ image entry names support identification only; no native content units are extracted",
        None,
    );
    add_provenance(
        metadata,
        "cbz-index",
        "image-entry-names",
        FactSource::ContainerIndex,
    );
    if let (Some(thumbnail), Some(first)) = (&options.thumbnail, pages.first()) {
        let bytes = archive.read(first)?.ok_or("missing CBZ cover entry")?;
        thumbnail_result(
            metadata,
            write_thumbnail(
                std::io::Cursor::new(bytes),
                thumbnail,
                FactSource::EmbeddedProperty,
            ),
        );
    }
    Ok(true)
}

/// Decodes a MOBI title/property header without decompressing text records.
///
/// Accepts bytes and the header encoding identifier. Returns UTF-8 or Windows-
/// 1252 text. Unsupported encodings and malformed UTF-8 return errors.
fn mobi_decode(bytes: &[u8], encoding: u32) -> ReaderResult<String> {
    match encoding {
        65001 => Ok(std::str::from_utf8(bytes)?.to_owned()),
        1252 => Ok(encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned()),
        _ => Err("unsupported MOBI metadata encoding".into()),
    }
}

/// Inspects MOBI/PalmDB headers, properties, encryption, and claimed records.
///
/// Accepts path, options, and metadata. Returns unit after header metadata or
/// an error. Text records are never read or decompressed; a cover record is
/// accessed only when a thumbnail was explicitly requested.
fn inspect_mobi(
    path: &Path,
    options: &InspectOptions,
    metadata: &mut DocumentMetadata,
) -> ReaderResult<()> {
    use crate::binary::{be16, be32, slice};
    let mut source = Source::open(path)?;
    let pdb = source.range(0, 78)?;
    if slice(&pdb, 60, 8)? != b"BOOKMOBI" {
        return Err("invalid MOBI PalmDB signature".into());
    }
    let count = be16(&pdb, 76)? as usize;
    if count == 0 || count > 4096 {
        return Err("MOBI record-count cap exceeded".into());
    }
    let table = source.range(78, count * 8)?;
    let mut offsets = Vec::with_capacity(count + 1);
    for index in 0..count {
        offsets.push(u64::from(be32(&table, index * 8)?));
    }
    offsets.push(source.len);
    if offsets[0] < (78 + count * 8) as u64 || offsets.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("invalid MOBI record offsets".into());
    }
    let head = source.range(offsets[0], (offsets[1] - offsets[0]) as usize)?;
    if slice(&head, 16, 4)? != b"MOBI" {
        return Err("missing MOBI record header".into());
    }
    let header_size = be32(&head, 20)? as usize;
    if header_size < 116 {
        return Err("short MOBI header".into());
    }
    slice(&head, 16, header_size)?;
    let encoding = be32(&head, 28)?;
    identify(
        metadata,
        "mobi",
        "application/x-mobipocket-ebook",
        "MOBI ebook",
        "Ebooks",
        "palmdb-mobi-header",
    );
    let title_offset = be32(&head, 84)? as usize;
    let title_length = be32(&head, 88)? as usize;
    if title_length > 0 {
        add_property(
            metadata,
            EmbeddedMetadataField::Title,
            mobi_decode(slice(&head, title_offset, title_length)?, encoding)?,
            Completeness::Exact,
        );
    }
    let encrypted = be16(&head, 12)? != 0;
    metadata.encryption = if encrypted {
        EncryptionStatus::EncryptedInaccessible
    } else {
        EncryptionStatus::NotEncrypted
    };
    let compression = be16(&head, 0)?;
    let text_records = be16(&head, 8)? as usize;
    add_count(
        metadata,
        NativeUnit::MobiTextRecord,
        Some(text_records as u64),
        FactSource::ContainerIndex,
        Completeness::Claimed,
    );
    metadata.extraction_support = if encrypted || !matches!(compression, 1 | 2) {
        SupportStatus::IdentificationOnly
    } else {
        SupportStatus::Supported
    };
    let mut cover = None;
    if be32(&head, 128)? & 0x40 != 0 {
        let exth = slice(&head, 16 + header_size, 12)?;
        if &exth[..4] != b"EXTH" {
            return Err("missing EXTH signature".into());
        }
        let exth = slice(&head, 16 + header_size, be32(exth, 4)? as usize)?;
        let records = be32(exth, 8)? as usize;
        if records > 4096 {
            return Err("EXTH record-count cap exceeded".into());
        }
        let mut cursor = 12;
        for _ in 0..records {
            let kind = be32(exth, cursor)?;
            let length = be32(exth, cursor + 4)? as usize;
            if length < 8 {
                return Err("invalid EXTH record length".into());
            }
            let data = slice(exth, cursor + 8, length - 8)?;
            let field = match kind {
                100 => Some(EmbeddedMetadataField::Author),
                503 => Some(EmbeddedMetadataField::Title),
                524 => Some(EmbeddedMetadataField::Language),
                _ => None,
            };
            if let Some(field) = field {
                add_property(
                    metadata,
                    field,
                    mobi_decode(data, encoding)?,
                    Completeness::Exact,
                );
            }
            if kind == 201 {
                cover = Some(be32(data, 0)?);
            }
            cursor += length;
        }
    }
    add_provenance(
        metadata,
        "mobi-header",
        "palmdb-and-exth",
        FactSource::ContainerIndex,
    );
    if let (Some(thumbnail), Some(relative)) = (&options.thumbnail, cover) {
        let index = be32(&head, 108)?
            .checked_add(relative)
            .ok_or("MOBI cover offset overflow")? as usize;
        if index >= count {
            return Err("MOBI cover record out of bounds".into());
        }
        let bytes = source.range(
            offsets[index],
            (offsets[index + 1] - offsets[index]) as usize,
        )?;
        thumbnail_result(
            metadata,
            write_thumbnail(
                std::io::Cursor::new(bytes),
                thumbnail,
                FactSource::EmbeddedProperty,
            ),
        );
    }
    Ok(())
}

/// Reuses the header-only RAR listing without payload decompression.
///
/// Accepts path, options, and metadata. Returns unit after entry names/counts
/// and multi-volume warnings. No compressed entry is read; CBR thumbnails are
/// explicitly unavailable because decoding would require decompression.
fn inspect_rar(
    path: &Path,
    options: &InspectOptions,
    metadata: &mut DocumentMetadata,
) -> ReaderResult<()> {
    let mut shell = structural_shell(&metadata.format.extension);
    crate::rar::inspect(path, &mut shell);
    copy_format(metadata, &shell.format);
    for error in shell.errors {
        let message = error.message;
        if message.contains("multi-volume") {
            mark_partial(metadata);
        }
        add_diagnostic(
            metadata,
            DiagnosticStage::Metadata,
            DiagnosticSeverity::Warning,
            "rar_listing_qualification",
            message,
            None,
        );
    }
    if shell.counts.entries.is_none() {
        return Err("RAR header listing failed".into());
    }
    add_count(
        metadata,
        NativeUnit::ArchiveEntry,
        shell.counts.entries.map(|value| value as u64),
        FactSource::ContainerIndex,
        Completeness::Exact,
    );
    for entry in shell.entries {
        add_schema(
            metadata,
            SchemaKind::ArchiveEntry,
            entry.name,
            FactSource::ContainerIndex,
        );
    }
    if let Some(pages) = shell.counts.pages {
        add_count(
            metadata,
            NativeUnit::PhysicalPage,
            Some(pages as u64),
            FactSource::ContainerIndex,
            Completeness::Claimed,
        );
    }
    metadata.extraction_support = SupportStatus::IdentificationOnly;
    add_provenance(
        metadata,
        "rar-headers",
        "header-listing",
        FactSource::ContainerIndex,
    );
    if options.thumbnail.is_some() {
        thumbnail_result(
            metadata,
            Err("RAR thumbnail requires entry decompression and is unavailable".into()),
        );
    }
    Ok(())
}

/// Reads raster dimensions from headers without decoding pixels or OCR.
///
/// Accepts path, options, and metadata. Returns unit after one image count,
/// the decoded header dimensions as the raster's single native page signal,
/// and an optional thumbnail. Header/cap errors are returned; pixel decode
/// occurs only inside explicit thumbnail generation.
fn inspect_image(
    path: &Path,
    options: &InspectOptions,
    metadata: &mut DocumentMetadata,
) -> ReaderResult<()> {
    let mut source = Source::open(path)?;
    let bytes = source.range(0, source.len as usize)?;
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()?
        .into_dimensions()?;
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err("image dimension cap exceeded".into());
    }
    add_count(
        metadata,
        NativeUnit::Image,
        Some(1),
        FactSource::DecoderMetadata,
        Completeness::Exact,
    );
    // COND-09: publish the decoder-reported dimensions in the dedicated
    // raster field instead of discarding them. The values are decoded pixel
    // counts, so they never enter the PDF-style width_points/height_points
    // fields of native page signals, and no DPI is inferred; the field names
    // carry the pixel disclosure themselves.
    metadata.raster = Some(crate::reports::RasterDimensions {
        width_pixels: width,
        height_pixels: height,
    });
    metadata.extraction_support = SupportStatus::Supported;
    add_provenance(
        metadata,
        "raster-header",
        "encoded-dimensions",
        FactSource::DecoderMetadata,
    );
    if let Some(thumbnail) = &options.thumbnail {
        thumbnail_result(
            metadata,
            write_thumbnail(
                std::io::Cursor::new(bytes),
                thumbnail,
                FactSource::GeneratedArtifact,
            ),
        );
    }
    Ok(())
}

/// Streams CSV/TSV headers and row counts without retaining row values.
///
/// Accepts path and metadata. Returns unit after schema names and an exact or
/// partial dataset-row count. Opens the input with no-follow semantics and
/// rejects non-regular files before reading. The established 1 MiB reader cap
/// is preserved; cap coverage is disclosed and full-file hashing remains
/// independent.
fn inspect_csv(path: &Path, metadata: &mut DocumentMetadata) -> ReaderResult<()> {
    const MAX_BYTES: u64 = 1 << 20;
    let file = crate::hashing::open_no_follow(path)?;
    if !file.metadata()?.is_file() {
        return Err("CSV input is not a regular file".into());
    }
    let delimiter = if metadata.format.extension == "tsv" {
        b'\t'
    } else {
        b','
    };
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .from_reader(file.take(MAX_BYTES + 1));
    let headers = reader.headers()?.clone();
    if headers.len() > 16_384 || reader.position().byte() > MAX_BYTES {
        return Err("CSV header byte/column cap exceeded".into());
    }
    for header in headers.iter() {
        add_schema(
            metadata,
            SchemaKind::Column,
            header,
            FactSource::NativeStructure,
        );
    }
    let mut record = csv::StringRecord::new();
    let mut rows = 0_u64;
    let mut partial = false;
    loop {
        if !reader.read_record(&mut record)? {
            break;
        }
        if reader.position().byte() > MAX_BYTES {
            partial = true;
            break;
        }
        rows = rows.checked_add(1).ok_or("CSV row count overflow")?;
    }
    add_count(
        metadata,
        NativeUnit::DatasetRow,
        Some(rows),
        FactSource::NativeStructure,
        if partial {
            Completeness::Partial
        } else {
            Completeness::Exact
        },
    );
    metadata.extraction_support = SupportStatus::Supported;
    add_provenance(
        metadata,
        "delimited-structure",
        "headers-and-record-count",
        FactSource::NativeStructure,
    );
    if partial {
        add_diagnostic(
            metadata,
            DiagnosticStage::Metadata,
            DiagnosticSeverity::Warning,
            "csv_read_cap",
            "CSV structure exceeds the 1 MiB reader cap; row count is partial",
            None,
        );
        mark_partial(metadata);
    }
    Ok(())
}

/// Publishes JSON schema keys and scalar counts without values or samples.
///
/// Accepts path and metadata. Returns unit after a full bounded profile or a
/// parser error. When the cumulative retained-path byte cap stops the stream,
/// the valid observed schema prefix is published, the scalar count is marked
/// partial, and a structured cap diagnostic marks the operation partial.
/// User-defined keys are legal identifiers; scalar strings, numbers, booleans,
/// nulls, samples, hashes, and prose are never published.
fn inspect_json(path: &Path, metadata: &mut DocumentMetadata) -> ReaderResult<()> {
    let (profile, truncated) = crate::json_profile::metadata(path)?;
    add_count(
        metadata,
        NativeUnit::JsonScalar,
        Some(profile.total_leaves as u64),
        FactSource::NativeStructure,
        if truncated {
            Completeness::Partial
        } else {
            Completeness::Exact
        },
    );
    for name in profile.leaf_stats.keys() {
        add_schema(
            metadata,
            SchemaKind::JsonPointer,
            name.clone(),
            FactSource::NativeStructure,
        );
    }
    metadata.extraction_support = SupportStatus::Supported;
    if truncated {
        add_diagnostic(
            metadata,
            DiagnosticStage::Metadata,
            DiagnosticSeverity::Warning,
            "metadata_cap_exceeded",
            "JSON retained-path byte cap exceeded",
            None,
        );
        mark_partial(metadata);
    }
    add_provenance(
        metadata,
        "json-structure",
        "value-free-stream-profile",
        FactSource::NativeStructure,
    );
    Ok(())
}

/// Publishes XML element paths and counts without text or attribute values.
///
/// Accepts path and metadata. Returns unit after a complete bounded profile or
/// a parser error. The total element count stays exact. When the cumulative
/// schema-path byte cap clips expansion, the genuine expanded paths are
/// published and a structured cap diagnostic marks the operation partial.
/// Element and attribute values never enter the report.
fn inspect_xml(path: &Path, metadata: &mut DocumentMetadata) -> ReaderResult<()> {
    let profile = crate::xml_profile::metadata(path)?;
    add_count(
        metadata,
        NativeUnit::XmlElement,
        Some(profile.total_elements),
        FactSource::NativeStructure,
        Completeness::Exact,
    );
    let (paths, truncated) = crate::xml_profile::schema_paths(&profile);
    for (index, uri) in profile.namespaces.iter().enumerate() {
        metadata
            .xml_namespaces
            .insert(format!("ns{index}"), uri.clone());
    }
    for name in paths {
        add_schema(
            metadata,
            SchemaKind::XmlPath,
            name,
            FactSource::NativeStructure,
        );
    }
    metadata.extraction_support = SupportStatus::Supported;
    if truncated {
        add_diagnostic(
            metadata,
            DiagnosticStage::Metadata,
            DiagnosticSeverity::Warning,
            "metadata_cap_exceeded",
            "XML schema-path byte cap exceeded",
            None,
        );
        mark_partial(metadata);
    }
    add_provenance(
        metadata,
        "xml-structure",
        "value-free-event-profile",
        FactSource::NativeStructure,
    );
    Ok(())
}

/// Counts UTF-8 logical lines without publishing document text.
///
/// Accepts path and metadata. Returns unit after an exact count or the existing
/// Source/UTF-8 error. Empty input has zero lines; standard `str::lines` final-
/// line semantics apply and no body string is retained in the report.
fn inspect_text(path: &Path, metadata: &mut DocumentMetadata) -> ReaderResult<()> {
    let mut source = Source::open(path)?;
    let bytes = source.range(0, source.len as usize)?;
    let text = std::str::from_utf8(&bytes)?;
    add_count(
        metadata,
        NativeUnit::LogicalLine,
        Some(text.lines().count() as u64),
        FactSource::NativeStructure,
        Completeness::Exact,
    );
    metadata.extraction_support = SupportStatus::Supported;
    add_provenance(
        metadata,
        "plain-text-structure",
        "utf8-line-count",
        FactSource::NativeStructure,
    );
    Ok(())
}

/// Renders a default (non-verbose) v3 Markdown inspection summary.
///
/// Accepts a metadata report. Returns a content-free Markdown summary reflecting
/// schema v3 fields (`Schema: 3`, `format.type`, full SHA-256 or sampled
/// fingerprint, readiness, and non-empty structure/description/diagnostic sections).
/// Performs no I/O and cannot fail.
pub fn summary(report: &MetadataReport) -> String {
    summary_markdown(report, false)
}

/// Formats an [`OperationStatus`] as its lowercase schema v3 status string.
///
/// Accepts an operational status enum value. Returns the matching lowercase
/// token. Performs no I/O and cannot fail.
fn status_label(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::Complete => "complete",
        OperationStatus::Partial => "partial",
        OperationStatus::Unsupported => "unsupported",
        OperationStatus::Failed => "failed",
        OperationStatus::Unknown => "unknown",
    }
}

/// Formats the v3 format summary line value (`type`, optional MIME, optional extension, optional mismatch).
///
/// Accepts a v3 format block. Returns a concise human-readable format string.
/// Performs no I/O and cannot fail.
fn format_summary_value(format: &crate::reports_v3::FormatBlockV3) -> String {
    let mut parts = Vec::new();
    if let Some(mime) = &format.mime {
        parts.push(mime.clone());
    }
    if let Some(ext) = &format.extension {
        parts.push(format!(".{ext}"));
    }
    let mut value = if parts.is_empty() {
        format.format_type.clone()
    } else {
        format!("{} ({})", format.format_type, parts.join(", "))
    };
    if format.extension_mismatch == Some(true) {
        value.push_str(" [extension mismatch]");
    }
    value
}

/// Output format for v3 inspection summaries rendered by [`render_summary`].
#[derive(Clone, Copy)]
enum SummaryStyle {
    /// Markdown heading (`# Glassbox inspection`), `## Section` headers, and `- ` bullets.
    Markdown,
    /// Plain-text top-level `Key: value` lines, `Section:` headers, and two-space indented items.
    Text,
}

impl SummaryStyle {
    /// Returns the document header written before top-level summary fields.
    ///
    /// Accepts `self`. Returns `"# Glassbox inspection\n\n"` for Markdown or
    /// `""` for plain text. Performs no I/O and cannot fail.
    fn header(self) -> &'static str {
        match self {
            Self::Markdown => "# Glassbox inspection\n\n",
            Self::Text => "",
        }
    }

    /// Returns the line prefix used for top-level summary entries.
    ///
    /// Accepts `self`. Returns `"- "` for Markdown or `""` for plain text.
    /// Performs no I/O and cannot fail.
    fn top_prefix(self) -> &'static str {
        match self {
            Self::Markdown => "- ",
            Self::Text => "",
        }
    }

    /// Returns the line prefix used for entries inside named sections.
    ///
    /// Accepts `self`. Returns `"- "` for Markdown or `"  "` for plain text.
    /// Performs no I/O and cannot fail.
    fn item_prefix(self) -> &'static str {
        match self {
            Self::Markdown => "- ",
            Self::Text => "  ",
        }
    }

    /// Appends a formatted section heading to `out`.
    ///
    /// Accepts `self`, destination buffer `out`, and section `title`. Returns
    /// unit after appending `\n## {title}\n\n` for Markdown or `\n{title}:\n`
    /// for plain text. Performs no I/O and cannot fail.
    fn push_section(self, out: &mut String, title: &str) {
        match self {
            Self::Markdown => out.push_str(&format!("\n## {title}\n\n")),
            Self::Text => out.push_str(&format!("\n{title}:\n")),
        }
    }
}

/// Renders a v3 inspection summary in the requested [`SummaryStyle`].
///
/// Accepts a metadata report, `verbose` flag (`true` adds a provenance section
/// when reader provenance exists), and output [`SummaryStyle`]. Returns a
/// content-free summary with empty sections omitted. Performs no I/O and cannot
/// fail.
fn render_summary(report: &MetadataReport, verbose: bool, style: SummaryStyle) -> String {
    let v3 = crate::reports_v3::inspect_to_v3(report, verbose);
    let top = style.top_prefix();
    let item = style.item_prefix();
    let mut out = String::from(style.header());
    out.push_str(&format!("{top}Schema: {}\n", v3.schema_version));
    out.push_str(&format!("{top}Status: {}\n", status_label(v3.status)));
    out.push_str(&format!("{top}Source: {}\n", v3.file.path));
    out.push_str(&format!(
        "{top}Format: {}\n",
        format_summary_value(&v3.format)
    ));
    out.push_str(&format!(
        "{top}Bytes: {}\n",
        v3.file
            .bytes
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unavailable".to_owned())
    ));
    if let Some(sha256) = &v3.file.sha256 {
        out.push_str(&format!("{top}SHA-256: {sha256}\n"));
    } else if let Some(fp) = &v3.file.fingerprint {
        out.push_str(&format!("{top}Fingerprint ({}): {}\n", fp.scheme, fp.value));
    } else {
        out.push_str(&format!("{top}SHA-256: unavailable\n"));
    }
    out.push_str(&format!(
        "{top}Readiness: extractable={}; encrypted={}; text_layer={}; ocr={}\n",
        v3.readiness.extractable, v3.readiness.encrypted, v3.readiness.text_layer, v3.readiness.ocr
    ));

    if !v3.description.is_empty() {
        style.push_section(&mut out, "Description");
        if let Some(title) = &v3.description.title {
            out.push_str(&format!("{item}Title: {title}\n"));
        }
        if !v3.description.authors.is_empty() {
            out.push_str(&format!(
                "{item}Authors: {}\n",
                v3.description.authors.join(", ")
            ));
        }
        if let Some(subject) = &v3.description.subject {
            out.push_str(&format!("{item}Subject: {subject}\n"));
        }
        if !v3.description.keywords.is_empty() {
            out.push_str(&format!(
                "{item}Keywords: {}\n",
                v3.description.keywords.join(", ")
            ));
        }
        if let Some(language) = &v3.description.language {
            out.push_str(&format!("{item}Language: {language}\n"));
        }
        if let Some(created_at) = &v3.description.created_at {
            out.push_str(&format!("{item}Created: {created_at}\n"));
        }
        if let Some(modified_at) = &v3.description.modified_at {
            out.push_str(&format!("{item}Modified: {modified_at}\n"));
        }
        if let Some(software) = &v3.description.software {
            if let Some(application) = &software.application {
                out.push_str(&format!("{item}Application: {application}\n"));
            }
            if let Some(creator_tool) = &software.creator_tool {
                out.push_str(&format!("{item}Creator tool: {creator_tool}\n"));
            }
            if let Some(producer) = &software.producer {
                out.push_str(&format!("{item}Producer: {producer}\n"));
            }
        }
    }

    if !v3.structure.counts.is_empty() {
        style.push_section(&mut out, "Counts");
        for (key, value) in &v3.structure.counts {
            out.push_str(&format!("{item}{key}: {value}\n"));
        }
    }

    if let Some(names) = &v3.structure.names {
        style.push_section(&mut out, "Structure names");
        for (label, list) in [
            ("Sheets", &names.sheets),
            ("Columns", &names.columns),
            ("Entries", &names.entries),
            ("XML paths", &names.xml_paths),
            ("JSON pointers", &names.json_pointers),
            ("Documents", &names.documents),
            ("Embedded", &names.embedded),
        ] {
            if !list.is_empty() {
                out.push_str(&format!("{item}{label}: {}\n", list.join(", ")));
            }
        }
    }

    if let Some(pdf) = &v3.structure.pdf {
        style.push_section(&mut out, "PDF geometry");
        if let Some(pages) = pdf.pages {
            out.push_str(&format!("{item}Pages: {pages}\n"));
        }
        if let (Some(w), Some(h)) = (pdf.width_points, pdf.height_points) {
            let uniformity = match pdf.uniform_page_size {
                Some(true) => " (uniform)",
                Some(false) => " (varying)",
                None => "",
            };
            out.push_str(&format!(
                "{item}Page size: {w:.2} x {h:.2} pt{uniformity}\n"
            ));
        }
        if let Some(rev) = pdf.security_handler_revision {
            out.push_str(&format!("{item}Security revision: {rev}\n"));
        }
        if let Some(perms) = pdf.permissions {
            out.push_str(&format!("{item}Permissions: 0x{perms:x}\n"));
        }
    }

    if let Some(image) = &v3.structure.image {
        style.push_section(&mut out, "Image geometry");
        out.push_str(&format!(
            "{item}{} x {} decoded pixels; no DPI is inferred\n",
            image.width_pixels, image.height_pixels
        ));
    }

    if !v3.diagnostics.is_empty() {
        style.push_section(&mut out, "Diagnostics");
        for diagnostic in &v3.diagnostics {
            out.push_str(&format!(
                "{item}{:?} {:?} {}: {}\n",
                diagnostic.stage, diagnostic.severity, diagnostic.code, diagnostic.message
            ));
        }
    }

    if !v3.artifacts.is_empty() {
        style.push_section(&mut out, "Artifacts");
        for artifact in &v3.artifacts {
            out.push_str(&format!(
                "{item}{:?}: {} ({}x{})\n",
                artifact.kind,
                artifact.path.display(),
                artifact
                    .width
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_owned()),
                artifact
                    .height
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            ));
        }
    }

    if verbose
        && let Some(provenance) = &v3.provenance
        && !provenance.is_empty()
    {
        style.push_section(&mut out, "Provenance");
        for entry in provenance {
            out.push_str(&format!(
                "{item}{}: {} ({:?})\n",
                entry.reader, entry.stage, entry.source
            ));
        }
    }

    out
}

/// Renders a v3 inspection summary in Markdown format.
///
/// Accepts a metadata report and `verbose` flag (`true` adds a `Provenance`
/// section when reader provenance exists). Returns a content-free Markdown
/// summary with empty sections omitted. Performs no I/O and cannot fail.
pub fn summary_markdown(report: &MetadataReport, verbose: bool) -> String {
    render_summary(report, verbose, SummaryStyle::Markdown)
}

/// Renders a v3 inspection summary in plain text format.
///
/// Accepts a metadata report and `verbose` flag (`true` adds a `Provenance:`
/// section when reader provenance exists). Returns a content-free plain-text
/// summary with empty sections omitted. Performs no I/O and cannot fail.
pub fn summary_text(report: &MetadataReport, verbose: bool) -> String {
    render_summary(report, verbose, SummaryStyle::Text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves metadata inspection withholds digest/fingerprint, marks status
    /// partial, and emits `source_changed_during_read` when the source file is
    /// atomically replaced between hashing and the post-read identity check (GB-AUD-15).
    ///
    /// Accepts no parameters. Returns unit after simulating atomic file
    /// replacement at the post-dispatch hook. Unexpected retained digests or
    /// missing diagnostics panic.
    #[test]
    fn metadata_inspection_withholds_digest_when_source_replaced_during_read() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tests")
            .join(format!(
                "metadata-identity-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("doc.txt");
        let replacement = dir.join("replacement.txt");
        std::fs::write(&target, b"line one\nline two\n").unwrap();
        std::fs::write(&replacement, b"replaced payload\n").unwrap();

        let (report, obs) = inspect_with_policy_and_hook(
            &target,
            &InspectOptions::default(),
            crate::hashing::HashPolicy::Full,
            || {
                std::fs::rename(&replacement, &target).unwrap();
            },
        );
        assert!(obs.is_some());
        assert_eq!(report.metadata.status, OperationStatus::Partial);
        assert!(report.metadata.digest.digest.is_none());
        assert!(report.metadata.digest.fingerprint.is_none());
        assert_eq!(
            report.metadata.digest.consistency,
            ConsistencyStatus::SourceChanged
        );
        assert!(report.metadata.diagnostics.iter().any(|d| {
            d.code == "source_changed_during_read" && d.severity == DiagnosticSeverity::Warning
        }));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Proves `summary_markdown` and `summary_text` emit exact v3 output across
    /// every section (header, description/software, counts, structure names,
    /// PDF geometry, image geometry, diagnostics, artifacts, and verbose
    /// provenance) via the shared `render_summary` helper.
    ///
    /// Accepts no parameters. Returns unit after asserting exact Markdown and
    /// plain-text outputs for both verbose and non-verbose invocations. Panics
    /// if any byte differs from the expected v3 summary contract.
    #[test]
    fn summary_markdown_and_text_render_all_sections_consistently() {
        let mut report = MetadataReport::default();
        report.metadata.status = OperationStatus::Complete;
        report.metadata.identity.path = std::path::PathBuf::from("fixtures/sample.pdf");
        report.metadata.identity.byte_size = Some(4096);
        report.metadata.format.bucket = "PDF".into();
        report.metadata.format.detected_mime = Some("application/pdf".into());
        report.metadata.format.extension = "pdf".into();
        report.metadata.format.disagreement = true;
        report.metadata.digest.digest = Some("deadbeef".into());
        report.metadata.extraction_support = SupportStatus::Supported;
        report.metadata.encryption = EncryptionStatus::NotEncrypted;
        add_property(
            &mut report.metadata,
            EmbeddedMetadataField::Title,
            "Quarterly Report",
            Completeness::Exact,
        );
        add_property(
            &mut report.metadata,
            EmbeddedMetadataField::Author,
            "Alice",
            Completeness::Exact,
        );
        add_property(
            &mut report.metadata,
            EmbeddedMetadataField::Author,
            "Bob",
            Completeness::Exact,
        );
        add_property(
            &mut report.metadata,
            EmbeddedMetadataField::Subject,
            "Finance",
            Completeness::Exact,
        );
        add_property(
            &mut report.metadata,
            EmbeddedMetadataField::Keywords,
            "q1, audit",
            Completeness::Exact,
        );
        add_property(
            &mut report.metadata,
            EmbeddedMetadataField::Language,
            "en",
            Completeness::Exact,
        );
        add_property(
            &mut report.metadata,
            EmbeddedMetadataField::CreatedAt,
            "2026-01-01T00:00:00Z",
            Completeness::Exact,
        );
        add_property(
            &mut report.metadata,
            EmbeddedMetadataField::ModifiedAt,
            "2026-01-02T00:00:00Z",
            Completeness::Exact,
        );
        add_property(
            &mut report.metadata,
            EmbeddedMetadataField::Application,
            "Glassbox Writer",
            Completeness::Exact,
        );
        add_property(
            &mut report.metadata,
            EmbeddedMetadataField::Creator,
            "Glassbox CLI",
            Completeness::Exact,
        );
        add_property(
            &mut report.metadata,
            EmbeddedMetadataField::Producer,
            "PDFium",
            Completeness::Exact,
        );
        add_count(
            &mut report.metadata,
            NativeUnit::PhysicalPage,
            Some(2),
            FactSource::NativeStructure,
            Completeness::Exact,
        );
        for (kind, name) in [
            (SchemaKind::Worksheet, "Summary"),
            (SchemaKind::Column, "Total"),
            (SchemaKind::ArchiveEntry, "doc.xml"),
            (SchemaKind::XmlPath, "/root/item"),
            (SchemaKind::JsonPointer, "/items/0"),
            (SchemaKind::Document, "main"),
            (SchemaKind::Embedded, "attach.bin"),
        ] {
            add_schema(
                &mut report.metadata,
                kind,
                name,
                FactSource::NativeStructure,
            );
        }
        report.metadata.pdf = Some(PdfDocumentSignals {
            security_handler_revision: Some(4),
            permissions: Some(0x1f),
        });
        report.metadata.native_page_signals = vec![
            NativePageSignal {
                page_index: 0,
                text_layer: TextLayerStatus::Present,
                ocr_recommended: OcrRecommendation::NotRecommended,
                object_counts: vec![],
                coverage: Completeness::Exact,
                uncertainties: vec![],
                width_points: Some(612.0),
                height_points: Some(792.0),
                rotation_quarter_turns: Some(0),
            },
            NativePageSignal {
                page_index: 1,
                text_layer: TextLayerStatus::Present,
                ocr_recommended: OcrRecommendation::NotRecommended,
                object_counts: vec![],
                coverage: Completeness::Exact,
                uncertainties: vec![],
                width_points: Some(612.0),
                height_points: Some(792.0),
                rotation_quarter_turns: Some(0),
            },
        ];
        report.metadata.raster = Some(RasterDimensions {
            width_pixels: 800,
            height_pixels: 600,
        });
        report.metadata.diagnostics.push(Diagnostic {
            stage: DiagnosticStage::Metadata,
            severity: DiagnosticSeverity::Info,
            code: "test_notice".into(),
            message: "synthetic notice".into(),
            unit_index: None,
        });
        report.metadata.artifacts.push(Artifact {
            kind: ArtifactKind::Thumbnail,
            path: std::path::PathBuf::from("thumb.png"),
            source: FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: Some(320),
            height: None,
        });
        add_provenance(
            &mut report.metadata,
            "pdfium-reader",
            "page-signals",
            FactSource::NativeStructure,
        );

        let expected_md = concat!(
            "# Glassbox inspection\n\n",
            "- Schema: 3\n",
            "- Status: complete\n",
            "- Source: fixtures/sample.pdf\n",
            "- Format: pdf (application/pdf, .pdf) [extension mismatch]\n",
            "- Bytes: 4096\n",
            "- SHA-256: deadbeef\n",
            "- Readiness: extractable=supported; encrypted=none; text_layer=unknown; ocr=unknown\n",
            "\n## Description\n\n",
            "- Title: Quarterly Report\n",
            "- Authors: Alice, Bob\n",
            "- Subject: Finance\n",
            "- Keywords: q1, audit\n",
            "- Language: en\n",
            "- Created: 2026-01-01T00:00:00Z\n",
            "- Modified: 2026-01-02T00:00:00Z\n",
            "- Application: Glassbox Writer\n",
            "- Creator tool: Glassbox CLI\n",
            "- Producer: PDFium\n",
            "\n## Counts\n\n",
            "- columns: 1\n",
            "- pages: 2\n",
            "\n## Structure names\n\n",
            "- Sheets: Summary\n",
            "- Columns: Total\n",
            "- Entries: doc.xml\n",
            "- XML paths: /root/item\n",
            "- JSON pointers: /items/0\n",
            "- Documents: main\n",
            "- Embedded: attach.bin\n",
            "\n## PDF geometry\n\n",
            "- Pages: 2\n",
            "- Page size: 612.00 x 792.00 pt (uniform)\n",
            "- Security revision: 4\n",
            "- Permissions: 0x1f\n",
            "\n## Image geometry\n\n",
            "- 800 x 600 decoded pixels; no DPI is inferred\n",
            "\n## Diagnostics\n\n",
            "- Metadata Info test_notice: synthetic notice\n",
            "\n## Artifacts\n\n",
            "- Thumbnail: thumb.png (320xunknown)\n",
            "\n## Provenance\n\n",
            "- pdfium-reader: page-signals (NativeStructure)\n",
        );
        let expected_text = concat!(
            "Schema: 3\n",
            "Status: complete\n",
            "Source: fixtures/sample.pdf\n",
            "Format: pdf (application/pdf, .pdf) [extension mismatch]\n",
            "Bytes: 4096\n",
            "SHA-256: deadbeef\n",
            "Readiness: extractable=supported; encrypted=none; text_layer=unknown; ocr=unknown\n",
            "\nDescription:\n",
            "  Title: Quarterly Report\n",
            "  Authors: Alice, Bob\n",
            "  Subject: Finance\n",
            "  Keywords: q1, audit\n",
            "  Language: en\n",
            "  Created: 2026-01-01T00:00:00Z\n",
            "  Modified: 2026-01-02T00:00:00Z\n",
            "  Application: Glassbox Writer\n",
            "  Creator tool: Glassbox CLI\n",
            "  Producer: PDFium\n",
            "\nCounts:\n",
            "  columns: 1\n",
            "  pages: 2\n",
            "\nStructure names:\n",
            "  Sheets: Summary\n",
            "  Columns: Total\n",
            "  Entries: doc.xml\n",
            "  XML paths: /root/item\n",
            "  JSON pointers: /items/0\n",
            "  Documents: main\n",
            "  Embedded: attach.bin\n",
            "\nPDF geometry:\n",
            "  Pages: 2\n",
            "  Page size: 612.00 x 792.00 pt (uniform)\n",
            "  Security revision: 4\n",
            "  Permissions: 0x1f\n",
            "\nImage geometry:\n",
            "  800 x 600 decoded pixels; no DPI is inferred\n",
            "\nDiagnostics:\n",
            "  Metadata Info test_notice: synthetic notice\n",
            "\nArtifacts:\n",
            "  Thumbnail: thumb.png (320xunknown)\n",
            "\nProvenance:\n",
            "  pdfium-reader: page-signals (NativeStructure)\n",
        );

        assert_eq!(summary_markdown(&report, true), expected_md);
        assert_eq!(summary_text(&report, true), expected_text);
        assert_eq!(summary(&report), summary_markdown(&report, false));
        assert!(!summary_markdown(&report, false).contains("## Provenance"));
        assert!(!summary_text(&report, false).contains("Provenance:"));
    }
}
