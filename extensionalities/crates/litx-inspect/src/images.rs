//! Module: glassbox-inspect raster content extraction.
//! File: images.rs.
//! Responsibility: Decode bounded raster inputs and run session-owned local OCR within the file lease.
use crate::{InspectOptions, InspectReport, Preflight, binary::Source, xml};
use image::{ImageReader, Limits};
use serde::Serialize;
use std::{io::Cursor, path::Path};

#[derive(Debug, Serialize)]
pub struct ImageAnalysis {
    pub width: u32,
    pub height: u32,
    pub aspect_ratio: f64,
    pub dpi: Option<[f64; 2]>,
    pub basis: &'static str,
    pub sampled_pixels: usize,
    pub white_fraction: f64,
    pub histogram: [usize; 16],
    pub classification: &'static str,
    pub probe: Option<OcrProbe>,
    pub ocr_status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct OcrProbe {
    pub basis: &'static str,
    pub engine: &'static str,
    pub language: String,
    pub word_count: usize,
    pub mean_confidence: f64,
}

/// Lazy worker-owned OCR model and the one active shared file lease.
pub(crate) struct OcrSession {
    pub(crate) api: Option<tesseract_rs::TesseractAPI>,
    language: Option<String>,
    budget: crate::SharedWorkBudget,
    active: Option<crate::WorkPermit>,
    staging: Vec<crate::OcrStagingPermit>,
}

impl Default for OcrSession {
    /// Creates a sequential OCR session without loading a model.
    ///
    /// Accepts no parameters. Returns a session with one file/OCR slot and no
    /// active lease. The infallible sequential budget allocates only synchronization state.
    fn default() -> Self {
        Self::with_budget(
            crate::SharedWorkBudget::new(1).expect("one is a valid shared work budget"),
        )
    }
}

impl OcrSession {
    /// Attaches lazy OCR state to the batch's shared file/OCR budget.
    ///
    /// Accepts an owned budget clone. Returns an empty session with no active
    /// file lease or model. No native library or source file is touched.
    pub(crate) fn with_budget(budget: crate::SharedWorkBudget) -> Self {
        Self {
            api: None,
            language: None,
            budget,
            active: None,
            staging: Vec::new(),
        }
    }

    /// Acquires the one lease covering a whole file and all its OCR operations.
    ///
    /// Accepts no parameters. Returns true after admission and false after
    /// cancellation. The lease remains owned until `end_file`.
    pub(crate) fn begin_file(&mut self) -> bool {
        debug_assert!(self.active.is_none());
        self.active = self.budget.acquire();
        self.active.is_some()
    }

    /// Releases the active file lease after ordinary native/OCR completion.
    ///
    /// Accepts no parameters. Returns unit after dropping the owned permit and
    /// waking waiting workers. Calling without an active lease is a no-op.
    pub(crate) fn end_file(&mut self) {
        self.staging.clear();
        self.active.take();
    }

    /// Enters recognition inside the current file lease without another acquire.
    ///
    /// Accepts no parameters. Returns a recognition marker, or None when no file
    /// lease exists or cancellation precedes OCR. Active OCR finishes normally.
    pub(crate) fn recognition(&mut self) -> Option<crate::OcrPermit> {
        if self.active.is_none() {
            self.active = self.budget.acquire();
        }
        self.active.as_ref()?.recognition()
    }

    /// Charges one retained PDF raster against the session-wide staging capacity.
    ///
    /// Accepts actual raster Vec capacity. Returns true after retaining a shared
    /// permit and false when the aggregate cap would be exceeded. Existing permits
    /// stay charged until recognition/merge completes or the file session ends.
    pub(crate) fn reserve_staging(&mut self, capacity: usize) -> bool {
        let Some(permit) = self.budget.reserve_staging(capacity) else {
            return false;
        };
        self.staging.push(permit);
        true
    }

    /// Releases all retained raster charges after recognition and native/OCR merge.
    ///
    /// Accepts no parameters. Returns unit after dropping permits in reverse order.
    /// The file lease remains active for projection and final report assembly.
    pub(crate) fn release_staging(&mut self) {
        self.staging.clear();
    }
}

pub(crate) fn inspect(
    path: &Path,
    report: &mut InspectReport,
    options: &InspectOptions,
    ocr: &mut OcrSession,
) {
    let result = (|| -> xml::Result<()> {
        let mut source = Source::open(path)?;
        let bytes = source.range(0, source.len as usize)?;
        inner(bytes, report, options, ocr)
    })();
    if let Err(error) = result {
        report.error("image", error);
    }
}

pub(crate) fn inspect_bytes(
    bytes: Vec<u8>,
    report: &mut InspectReport,
    options: &InspectOptions,
    ocr: &mut OcrSession,
) {
    if let Err(error) = inner(bytes, report, options, ocr) {
        report.error("image", error);
    }
}

/// Decodes one bounded image and optionally recognizes it within the file lease.
///
/// Accepts owned encoded bytes, a destination legacy report, extraction controls,
/// and the worker-owned OCR session. Returns unit after native/OCR assembly.
/// Decoder, selection, model, cancellation, and recognition failures are returned.
fn inner(
    bytes: Vec<u8>,
    report: &mut InspectReport,
    options: &InspectOptions,
    ocr: &mut OcrSession,
) -> xml::Result<()> {
    let mut reader = ImageReader::new(Cursor::new(&bytes)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(128 << 20);
    reader.limits(limits);
    let decoded = reader.decode()?;
    let (width, height) = (decoded.width(), decoded.height());
    let sample = if width.max(height) > 1024 {
        decoded.thumbnail(1024, 1024).to_rgba8()
    } else {
        decoded.to_rgba8()
    };
    let sampled_pixels = sample.width() as usize * sample.height() as usize;
    let mut histogram = [0; 16];
    let mut white = 0;
    for pixel in sample.pixels() {
        let alpha = pixel[3] as usize;
        let rgb = [pixel[0], pixel[1], pixel[2]]
            .map(|v| (v as usize * alpha + 255 * (255 - alpha)) / 255);
        let luma = (299 * rgb[0] + 587 * rgb[1] + 114 * rgb[2]) / 1000;
        histogram[luma / 16] += 1;
        white += usize::from(rgb.iter().all(|&v| v >= 245));
    }
    let white_fraction = white as f64 / sampled_pixels.max(1) as f64;
    let aspect_ratio = width as f64 / height.max(1) as f64;
    let dark_fraction = histogram[0] as f64 / sampled_pixels.max(1) as f64;
    let classification = if white_fraction > 0.995 {
        "blank_or_near_blank"
    } else if white_fraction > 0.55 && dark_fraction > 0.005 && (0.45..=0.9).contains(&aspect_ratio)
    {
        "document_scan_like"
    } else {
        "photo_or_graphic"
    };
    report.counts.pages = Some(1);
    if options.max_units > 0 {
        report.preflight.push(Preflight {
            page_number: 1,
            unit_kind: "image",
            needs_ocr: classification == "document_scan_like",
            reasons: vec![classification.into()],
            confidence: "heuristic",
            coverage_available: false,
        });
    }
    report.image = Some(ImageAnalysis {
        width,
        height,
        aspect_ratio,
        dpi: png_dpi(&bytes).or_else(|| jpeg_dpi(&bytes)),
        basis: "heuristic",
        sampled_pixels,
        white_fraction,
        histogram,
        classification,
        probe: None,
        ocr_status: "not_probed",
    });
    let selection_plan = match &options.native_selection {
        Some(selection) => Some(crate::selection::begin(
            report,
            selection,
            crate::reports::NativeUnit::Image,
            1,
        )?),
        None => None,
    };
    if options.thumbnail.is_some() {
        crate::thumbnail::write(Cursor::new(bytes), "downscaled", options, report);
    }
    let mut ocr_facts: Option<crate::reports::OcrProvenance> = None;
    if (options.probe_ocr || options.identify) && options.max_units > 0 {
        match probe(&decoded, ocr, options) {
            Ok((probe, text)) => {
                let word_count = probe.word_count;
                // COND-10: OCR provenance facts are captured only when
                // recognition actually ran, so a native-only image unit never
                // claims OCR; a failed probe keeps the unit unclaimed too.
                ocr_facts = Some(crate::reports::OcrProvenance {
                    engine: probe.engine.into(),
                    language: probe.language.clone(),
                    mean_confidence: Some(probe.mean_confidence),
                    raster_long_edge: Some(options.ocr_raster_long_edge),
                });
                let image = report.image.as_mut().unwrap();
                image.ocr_status = if word_count > 0 {
                    "text_detected"
                } else {
                    "no_text_detected"
                };
                image.probe = Some(probe);
                report.text.origin = "ocr".into();
                report.append_text(&text, options.max_chars);
                // LITX-AUD-009: OCR-recovered image text joins the Markdown
                // output with OCR provenance, not a plain text fallback.
                if word_count > 0 {
                    crate::markdown::append(
                        report,
                        &crate::markdown::escape(&text),
                        options,
                        "ocr_text_fallback",
                    );
                }
            }
            Err(error) => {
                report.image.as_mut().unwrap().ocr_status = "failed";
                report.error("probe_ocr", error);
            }
        }
    }
    if selection_plan.is_some() {
        let origin = if report.text.origin == "ocr" {
            crate::reports::ContentOrigin::Ocr
        } else {
            crate::reports::ContentOrigin::Native
        };
        let mut unit = crate::selection::text_unit(
            1,
            crate::reports::NativeUnit::Image,
            None,
            report.text.content.clone(),
            origin,
        );
        unit.provenance[0].ocr = ocr_facts;
        report.native_units.push(unit);
    }
    Ok(())
}

/// Resolves existing local Tesseract models without network fallback.
///
/// Accepts a Tesseract language expression. Returns its model directory after
/// checking every canonical model for presence, nonzero size, and the 128 MiB
/// per-model cap. Missing, invalid, or oversized models return an error.
pub(crate) fn local_tessdata(language: &str) -> xml::Result<std::path::PathBuf> {
    let directory = if let Some(path) = std::env::var_os("TESSDATA_PREFIX") {
        std::path::PathBuf::from(path)
    } else {
        let mut candidates = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(std::path::PathBuf::from(&home).join(".tesseract-rs/tessdata"));
            candidates.push(
                std::path::PathBuf::from(home)
                    .join("Library/Application Support/tesseract-rs/tessdata"),
            );
        }
        candidates.extend(
            [
                "/usr/share/tesseract-ocr/5/tessdata",
                "/usr/share/tessdata",
                "/opt/homebrew/share/tessdata",
            ]
            .map(std::path::PathBuf::from),
        );
        candidates
            .into_iter()
            .find(|path| path.join("eng.traineddata").is_file())
            .ok_or("local eng.traineddata unavailable; set TESSDATA_PREFIX (no downloads)")?
    };
    for code in canonical_language(language)?.split('+') {
        let metadata =
            std::fs::metadata(directory.join(format!("{code}.traineddata"))).map_err(|_| {
                format!("local {code}.traineddata unavailable; set TESSDATA_PREFIX (no downloads)")
            })?;
        if metadata.len() == 0 || metadata.len() > 128 << 20 {
            return Err("local OCR model byte cap exceeded or empty model".into());
        }
    }
    directory
        .to_str()
        .ok_or("local tessdata directory must be UTF-8")?;
    Ok(directory)
}

/// Maps one OCR language code to its canonical Tesseract model name.
///
/// Accepts a single code from a '+'-joined expression. Returns the canonical
/// model name, keeping unknown codes unchanged. Empty codes, or codes with
/// bytes beyond ASCII letters, digits, and '_', return an error.
fn canonical_model(code: &str) -> xml::Result<String> {
    let code = match code.to_ascii_lowercase().as_str() {
        "en" => "eng".to_string(),
        "fr" => "fra".to_string(),
        "de" => "deu".to_string(),
        "es" => "spa".to_string(),
        "it" => "ita".to_string(),
        "pt" => "por".to_string(),
        "ru" => "rus".to_string(),
        "zh" | "zh-cn" => "chi_sim".to_string(),
        "zh-tw" => "chi_tra".to_string(),
        "ja" => "jpn".to_string(),
        "ko" => "kor".to_string(),
        "ar" => "ara".to_string(),
        "hi" => "hin".to_string(),
        "th" => "tha".to_string(),
        "vi" => "vie".to_string(),
        other => other.to_string(),
    };
    if code.is_empty()
        || !code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err("invalid local OCR language model name".into());
    }
    Ok(code)
}

/// Normalizes a '+'-joined OCR language expression to canonical model names.
///
/// Accepts the requested OCR language expression. Returns the same codes after
/// alias mapping, such as `en` to `eng` and `zh` to `chi_sim`. Empty or invalid
/// codes return an error before any model file is checked.
pub(crate) fn canonical_language(language: &str) -> xml::Result<String> {
    language
        .split('+')
        .map(canonical_model)
        .collect::<xml::Result<Vec<_>>>()
        .map(|codes| codes.join("+"))
}

/// Runs local OCR with request-isolated language and raster controls.
///
/// Accepts a decoded image, worker-owned session, and current extraction options.
/// Returns the OCR probe and recognized text. Missing models, initialization,
/// cancellation, raster, recognition, and text-extraction failures are returned;
/// changing language replaces this worker's model instead of contaminating requests.
pub(crate) fn probe(
    image: &image::DynamicImage,
    session: &mut OcrSession,
    options: &InspectOptions,
) -> xml::Result<(OcrProbe, String)> {
    use tesseract_rs::{TessPageSegMode, TesseractAPI};
    let _recognition = session
        .recognition()
        .ok_or("OCR cancelled before recognition; native content retained")?;
    // COND-10: one shared normalization now covers model checks, init, the
    // session cache key, and the reported observation, so every language
    // alias resolves to one canonical Tesseract model expression.
    let language = canonical_language(&options.ocr_language)?;
    if session.api.is_none() || session.language.as_deref() != Some(language.as_str()) {
        let directory = local_tessdata(&options.ocr_language)?;
        let api = TesseractAPI::new();
        api.init(&directory, &language)?;
        api.set_page_seg_mode(TessPageSegMode::PSM_AUTO)?;
        session.api = Some(api);
        session.language = Some(language.clone());
    }
    let edge = options.ocr_raster_long_edge.max(1);
    let grayscale = if image.width().max(image.height()) > edge {
        image.thumbnail(edge, edge).to_luma8()
    } else {
        image.to_luma8()
    };
    let api = session.api.as_ref().unwrap();
    api.clear()?;
    api.clear_adaptive_classifier()?;
    api.set_image(
        grayscale.as_raw(),
        grayscale.width() as i32,
        grayscale.height() as i32,
        1,
        grayscale.width() as i32,
    )?;
    api.set_source_resolution(150)?;
    api.recognize()?;
    let text = api.get_utf8_text()?;
    let result = OcrProbe {
        basis: "probed",
        engine: "tesseract",
        language,
        word_count: text.split_whitespace().count(),
        mean_confidence: (api.mean_text_conf()? as f64 / 100.0).clamp(0.0, 1.0),
    };
    api.clear()?;
    Ok((result, text))
}

fn png_dpi(bytes: &[u8]) -> Option<[f64; 2]> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut offset: usize = 8;
    while offset.checked_add(12)? <= bytes.len() {
        let size = crate::binary::be32(bytes, offset).ok()? as usize;
        let end = offset.checked_add(12)?.checked_add(size)?;
        let chunk = bytes.get(offset..end)?;
        if &chunk[4..8] == b"pHYs" && size == 9 && chunk[16] == 1 {
            let x = crate::binary::be32(chunk, 8).ok()?;
            let y = crate::binary::be32(chunk, 12).ok()?;
            if x > 0 && y > 0 {
                return Some([x as f64 * 0.0254, y as f64 * 0.0254]);
            }
        }
        if &chunk[4..8] == b"IDAT" {
            break;
        }
        offset = end;
    }
    None
}

fn jpeg_dpi(bytes: &[u8]) -> Option<[f64; 2]> {
    if !bytes.starts_with(b"\xff\xd8") {
        return None;
    }
    let mut offset = 2;
    while bytes.get(offset) == Some(&0xff) {
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *bytes.get(offset)?;
        offset += 1;
        if matches!(marker, 0xda | 0xd9) {
            break;
        }
        if marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            continue;
        }
        let size = crate::binary::be16(bytes, offset).ok()? as usize;
        if size < 2 {
            return None;
        }
        let body = bytes.get(offset + 2..offset + size)?;
        if marker == 0xe0 && body.len() >= 14 && body.starts_with(b"JFIF\0") {
            let scale = match body[7] {
                1 => 1.0,
                2 => 2.54,
                _ => return None,
            };
            let x = crate::binary::be16(body, 8).ok()?;
            let y = crate::binary::be16(body, 10).ok()?;
            return (x > 0 && y > 0).then_some([x as f64 * scale, y as f64 * scale]);
        }
        offset += size;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::canonical_language;

    /// Verifies alias codes normalize to canonical Tesseract model names.
    ///
    /// Accepts no parameters. Returns unit after asserting alias and canonical
    /// spellings map to one canonical model expression. No filesystem, model,
    /// or network access occurs.
    #[test]
    fn language_aliases_normalize_to_canonical_model_names() {
        assert_eq!(canonical_language("en").unwrap(), "eng");
        assert_eq!(canonical_language("EN").unwrap(), "eng");
        assert_eq!(
            canonical_language("zh-CN+zh-TW").unwrap(),
            "chi_sim+chi_tra"
        );
        assert_eq!(canonical_language("deu+fra+spa").unwrap(), "deu+fra+spa");
    }

    /// Verifies empty or unsafe language expressions are rejected.
    ///
    /// Accepts no parameters. Returns unit after asserting each malformed
    /// expression returns an error. No model file is looked up.
    #[test]
    fn invalid_language_expressions_are_rejected() {
        assert!(canonical_language("").is_err());
        assert!(canonical_language("eng+").is_err());
        assert!(canonical_language("eñ").is_err());
        assert!(canonical_language("../etc/passwd").is_err());
    }
}
