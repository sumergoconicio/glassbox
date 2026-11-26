//! Module: glassbox-cli pure-Rust voice activity detection for `--asr short`.
//! File: media/vad.rs.
//! Responsibility: Detect speech onset on streaming 16 kHz mono PCM using 20 ms frame RMS energy, speech-band zero-crossing rate, adaptive noise-floor tracking, a 500 ms pre-roll ring buffer, a 60 s capture limit, and a 600 s lead-in scan cap.

use super::MediaError;
use super::asr::SAMPLE_RATE_HZ;
use std::collections::VecDeque;
use std::io::Read;

/// Samples per 20 ms VAD analysis frame at 16 kHz (`0.020 * 16_000`).
pub const VAD_FRAME_SAMPLES: usize = 320;

/// Number of 20 ms frames retained in the 500 ms pre-roll ring buffer.
pub const PRE_ROLL_FRAMES: usize = 25;

/// Total samples retained in the 500 ms pre-roll ring buffer (`25 * 320`).
pub const PRE_ROLL_SAMPLES: usize = PRE_ROLL_FRAMES * VAD_FRAME_SAMPLES;

/// Minimum active speech frames required to confirm onset (`15 * 20 ms = 300 ms`).
pub const ONSET_ACTIVE_FRAMES: usize = 15;

/// Maximum consecutive inactive frames tolerated inside an onset candidate run.
const MAX_ONSET_DIP_FRAMES: usize = 1;

/// Maximum PCM samples captured once speech onset triggers (`60 s * 16_000`).
pub const MAX_CAPTURE_SAMPLES: usize = 60 * SAMPLE_RATE_HZ;

/// Hard lead-in scan cap in samples before declaring no speech (`600 s * 16_000`).
pub const MAX_SCAN_SAMPLES: usize = 600 * SAMPLE_RATE_HZ;

/// Minimum AC RMS energy required to consider a frame above digital silence.
const MIN_RMS_FLOOR: f32 = 0.008;

/// Initial adaptive noise-floor RMS estimate before quiet frames are observed.
const INITIAL_NOISE_FLOOR_RMS: f32 = 0.004;

/// Lower clamp for the adaptive noise-floor RMS estimate.
const MIN_NOISE_FLOOR_RMS: f32 = 0.002;

/// Upper clamp for the adaptive noise-floor RMS estimate so loud hum cannot mask normal speech.
const MAX_NOISE_FLOOR_RMS: f32 = 0.06;

/// Multiplier above the adaptive noise floor required to mark a frame as speech-active.
const SPEECH_SNR_RATIO: f32 = 2.5;

/// Lower speech-band zero-crossing rate bound (~160 Hz equivalent at 16 kHz; rejects 50/60/120 Hz hum).
const MIN_SPEECH_ZCR: f32 = 0.020;

/// Upper speech-band zero-crossing rate bound (~3,600 Hz equivalent at 16 kHz; rejects harsh broadband static).
const MAX_SPEECH_ZCR: f32 = 0.45;

/// One-pole high-pass filter coefficient (~120 Hz cutoff at 16 kHz) used only for VAD analysis.
const HPF_ALPHA: f32 = 0.955;

/// Fast decay rate when quiet frames pull the noise floor downward.
const NOISE_FLOOR_DOWN_ALPHA: f32 = 0.15;

/// Slow adaptation rate when ambient non-speech frames pull the noise floor upward.
const NOISE_FLOOR_UP_ALPHA: f32 = 0.03;

/// Streaming read buffer size in bytes for `extract_short_speech` (16 KiB = 4,096 `f32` samples).
const READ_CHUNK_BYTES: usize = 16 * 1024;

/// Converts a 16 kHz sample count into milliseconds without integer overflow.
///
/// Accepts a count of 16 kHz PCM samples. Returns the corresponding duration in
/// milliseconds (floored). Performs no I/O and cannot fail.
pub fn samples_to_ms(samples: usize) -> u64 {
    ((samples as u128 * 1_000) / SAMPLE_RATE_HZ as u128) as u64
}

/// Incremental state returned after feeding a PCM chunk into [`ShortSpeechDetector`].
#[derive(Debug, Clone, PartialEq)]
pub enum VadStep {
    /// Still scanning lead-in audio or accumulating captured speech below the 60 s cap.
    Scanning,
    /// Speech onset occurred and the full 60 s capture buffer has been filled.
    Completed {
        /// Zero-based sample offset in the source stream where the emitted PCM begins (including pre-roll).
        start_sample: usize,
        /// Captured mono 16 kHz `f32` PCM samples (pre-roll + speech, up to 60 s).
        pcm: Vec<f32>,
    },
    /// The 600 s lead-in scan cap was reached without detecting speech onset.
    NoSpeech {
        /// Total number of source samples scanned before stopping.
        scanned_samples: usize,
    },
}

/// Final outcome produced when a short-mode audio stream finishes or hits a cap.
#[derive(Debug, Clone, PartialEq)]
pub enum VadOutcome {
    /// Speech onset was detected and up to 60 s of audio (with 500 ms pre-roll) was captured.
    Speech {
        /// Zero-based sample offset in the source stream where `pcm` begins.
        start_sample: usize,
        /// Captured mono 16 kHz `f32` PCM samples.
        pcm: Vec<f32>,
    },
    /// No speech onset was detected before EOF or the 600 s lead-in scan cap.
    NoSpeechDetected {
        /// Duration of audio scanned in milliseconds.
        scanned_ms: u64,
    },
}

/// Result returned by [`extract_short_speech`] when reading a raw `f32le` PCM stream.
#[derive(Debug, Clone, PartialEq)]
pub struct ShortSpeechResult {
    /// Whether speech was captured or no speech was detected within the scan cap.
    pub outcome: VadOutcome,
    /// Total number of 16 kHz PCM samples consumed from the underlying reader.
    pub total_samples_read: usize,
    /// Whether the underlying reader reached EOF naturally (false if stopped early at 60 s or 600 s cap).
    pub reached_eof: bool,
}

/// Internal capture phase of [`ShortSpeechDetector`].
enum DetectorState {
    /// Scanning 20 ms frames for sustained speech onset while maintaining a 500 ms pre-roll ring buffer.
    Scanning,
    /// Capturing raw PCM starting at `start_sample` until `MAX_CAPTURE_SAMPLES` or EOF.
    Capturing {
        /// Zero-based start sample offset including pre-roll.
        start_sample: usize,
        /// Accumulated mono 16 kHz PCM samples.
        pcm: Vec<f32>,
    },
    /// Terminal state after `push_samples` has already yielded `Completed` or `NoSpeech`.
    Finished {
        /// Scanned sample count at termination.
        scanned_samples: usize,
    },
}

/// Streaming pure-Rust 20 ms frame voice activity detector for `--asr short`.
///
/// Tracks per-frame high-pass filtered RMS energy and speech-band zero-crossing
/// rate against an adaptive noise floor, buffers a 500 ms pre-roll ring, emits
/// up to 60 s of unmodified PCM once 300 ms of speech is detected, and enforces
/// a hard 600 s lead-in scan cap.
pub struct ShortSpeechDetector {
    /// Current detector phase.
    state: DetectorState,
    /// Partial frame samples (< 320) buffered across `push_samples` calls during scanning.
    pending_frame: Vec<f32>,
    /// Ring buffer holding up to 25 quiet frames (500 ms) immediately preceding speech onset.
    pre_roll: VecDeque<[f32; VAD_FRAME_SAMPLES]>,
    /// Candidate frames accumulated during a potential speech onset run.
    candidate: Vec<[f32; VAD_FRAME_SAMPLES]>,
    /// Number of speech-active frames in `candidate`.
    active_count: usize,
    /// Consecutive inactive frames seen at the tail of `candidate`.
    consecutive_inactive: usize,
    /// Total samples processed while scanning (used to compute onset offset and 600 s cap).
    scanned_samples: usize,
    /// Adaptive noise-floor RMS estimate updated on non-speech frames.
    noise_floor_rms: f32,
    /// Previous raw sample held by the 1-pole analysis high-pass filter.
    hpf_prev_x: f32,
    /// Previous filtered sample held by the 1-pole analysis high-pass filter.
    hpf_prev_y: f32,
}

impl Default for ShortSpeechDetector {
    /// Constructs a new [`ShortSpeechDetector`] initialized with calibrated speech defaults.
    ///
    /// Accepts no arguments. Returns a ready detector in the `Scanning` state.
    /// Performs no I/O and cannot fail.
    fn default() -> Self {
        Self::new()
    }
}

impl ShortSpeechDetector {
    /// Creates a new [`ShortSpeechDetector`] ready to scan streaming 16 kHz mono PCM.
    ///
    /// Accepts no arguments. Returns an initialized detector with an empty
    /// 500 ms pre-roll ring buffer and calibrated noise-floor baseline. Cannot fail.
    pub fn new() -> Self {
        Self {
            state: DetectorState::Scanning,
            pending_frame: Vec::with_capacity(VAD_FRAME_SAMPLES),
            pre_roll: VecDeque::with_capacity(PRE_ROLL_FRAMES),
            candidate: Vec::with_capacity(ONSET_ACTIVE_FRAMES + MAX_ONSET_DIP_FRAMES + 1),
            active_count: 0,
            consecutive_inactive: 0,
            scanned_samples: 0,
            noise_floor_rms: INITIAL_NOISE_FLOOR_RMS,
            hpf_prev_x: 0.0,
            hpf_prev_y: 0.0,
        }
    }

    /// Feeds the next slice of 16 kHz mono `f32` PCM samples into the detector.
    ///
    /// Accepts a slice of floating-point PCM samples of any length. Returns
    /// [`VadStep::Scanning`] while lead-in scanning or speech capture is still
    /// in progress, [`VadStep::Completed`] immediately when the 60 s capture
    /// buffer fills, or [`VadStep::NoSpeech`] immediately when the 600 s
    /// lead-in scan cap is reached without speech onset. Never panics.
    pub fn push_samples(&mut self, samples: &[f32]) -> VadStep {
        let mut cursor = 0;

        while cursor < samples.len() {
            match &mut self.state {
                DetectorState::Scanning => {
                    let needed = VAD_FRAME_SAMPLES - self.pending_frame.len();
                    let take = needed.min(samples.len() - cursor);
                    self.pending_frame
                        .extend_from_slice(&samples[cursor..cursor + take]);
                    cursor += take;

                    if self.pending_frame.len() < VAD_FRAME_SAMPLES {
                        break;
                    }

                    let mut frame = [0.0_f32; VAD_FRAME_SAMPLES];
                    frame.copy_from_slice(&self.pending_frame);
                    self.pending_frame.clear();

                    if let Some(step) = self.process_scan_frame(frame) {
                        return step;
                    }
                }
                DetectorState::Capturing { start_sample, pcm } => {
                    let remaining_cap = MAX_CAPTURE_SAMPLES.saturating_sub(pcm.len());
                    let take = remaining_cap.min(samples.len() - cursor);
                    pcm.extend_from_slice(&samples[cursor..cursor + take]);
                    cursor += take;

                    if pcm.len() >= MAX_CAPTURE_SAMPLES {
                        let start = *start_sample;
                        let finished_pcm = std::mem::take(pcm);
                        self.state = DetectorState::Finished {
                            scanned_samples: self.scanned_samples,
                        };
                        return VadStep::Completed {
                            start_sample: start,
                            pcm: finished_pcm,
                        };
                    }
                }
                DetectorState::Finished { scanned_samples } => {
                    return VadStep::NoSpeech {
                        scanned_samples: *scanned_samples,
                    };
                }
            }
        }

        VadStep::Scanning
    }

    /// Finalizes detection at end-of-stream and returns the resulting [`VadOutcome`].
    ///
    /// Accepts ownership of `self`. Returns [`VadOutcome::Speech`] when speech
    /// onset already triggered before EOF (emitting whatever pre-roll + speech
    /// samples were captured up to 60 s), or [`VadOutcome::NoSpeechDetected`]
    /// with the total scanned duration in milliseconds when no speech occurred.
    /// Cannot fail.
    pub fn finish(self) -> VadOutcome {
        match self.state {
            DetectorState::Capturing { start_sample, pcm } => {
                VadOutcome::Speech { start_sample, pcm }
            }
            DetectorState::Scanning => {
                let total = (self.scanned_samples + self.pending_frame.len()).min(MAX_SCAN_SAMPLES);
                VadOutcome::NoSpeechDetected {
                    scanned_ms: samples_to_ms(total),
                }
            }
            DetectorState::Finished { scanned_samples } => VadOutcome::NoSpeechDetected {
                scanned_ms: samples_to_ms(scanned_samples),
            },
        }
    }

    /// Analyzes one complete 20 ms frame during the `Scanning` phase.
    ///
    /// Accepts a 320-sample PCM frame. Updates the adaptive noise floor,
    /// pre-roll ring buffer, and candidate onset run; transitions into
    /// `Capturing` when 15 active frames (300 ms) are accumulated. Returns
    /// `Some(VadStep::Completed)` or `Some(VadStep::NoSpeech)` when a hard cap
    /// is hit on this frame, or `None` to keep processing.
    fn process_scan_frame(&mut self, frame: [f32; VAD_FRAME_SAMPLES]) -> Option<VadStep> {
        let (rms, zcr) = self.analyze_frame(&frame);
        let energy_threshold = (self.noise_floor_rms * SPEECH_SNR_RATIO).max(MIN_RMS_FLOOR);
        let zcr_in_speech_band = (MIN_SPEECH_ZCR..=MAX_SPEECH_ZCR).contains(&zcr);
        let is_speech = rms >= energy_threshold && zcr_in_speech_band;

        if !is_speech {
            self.update_noise_floor(rms, zcr_in_speech_band);
        }

        self.scanned_samples += VAD_FRAME_SAMPLES;

        if is_speech {
            self.consecutive_inactive = 0;
            self.active_count += 1;
            self.candidate.push(frame);

            if self.active_count >= ONSET_ACTIVE_FRAMES {
                let buffered_samples =
                    (self.pre_roll.len() + self.candidate.len()) * VAD_FRAME_SAMPLES;
                let start_sample = self.scanned_samples.saturating_sub(buffered_samples);
                let mut pcm = Vec::with_capacity(buffered_samples.min(MAX_CAPTURE_SAMPLES));

                for pre_frame in self.pre_roll.drain(..) {
                    append_capped(&mut pcm, &pre_frame, MAX_CAPTURE_SAMPLES);
                }
                for cand_frame in self.candidate.drain(..) {
                    append_capped(&mut pcm, &cand_frame, MAX_CAPTURE_SAMPLES);
                }
                self.active_count = 0;
                self.consecutive_inactive = 0;

                if pcm.len() >= MAX_CAPTURE_SAMPLES {
                    self.state = DetectorState::Finished {
                        scanned_samples: self.scanned_samples,
                    };
                    return Some(VadStep::Completed { start_sample, pcm });
                }

                self.state = DetectorState::Capturing { start_sample, pcm };
                return None;
            }
        } else if self.candidate.is_empty() {
            self.push_pre_roll(frame);
        } else {
            self.consecutive_inactive += 1;
            if self.consecutive_inactive <= MAX_ONSET_DIP_FRAMES {
                self.candidate.push(frame);
            } else {
                for failed in self.candidate.drain(..) {
                    push_pre_roll_frame(&mut self.pre_roll, failed);
                }
                push_pre_roll_frame(&mut self.pre_roll, frame);
                self.active_count = 0;
                self.consecutive_inactive = 0;
            }
        }

        if self.scanned_samples >= MAX_SCAN_SAMPLES {
            let scanned = self.scanned_samples;
            self.state = DetectorState::Finished {
                scanned_samples: scanned,
            };
            return Some(VadStep::NoSpeech {
                scanned_samples: scanned,
            });
        }

        None
    }

    /// Computes high-pass filtered AC RMS energy and zero-crossing rate for one 20 ms frame.
    ///
    /// Accepts a 320-sample PCM frame. Applies a 1-pole high-pass filter and
    /// per-frame mean subtraction on a stack-local analysis buffer (leaving the
    /// caller's PCM untouched) to reject DC bias and mains hum. Returns
    /// `(rms, zcr)`. Non-finite inputs are sanitized to `0.0` so internal filter
    /// state never becomes `NaN`.
    fn analyze_frame(&mut self, frame: &[f32; VAD_FRAME_SAMPLES]) -> (f32, f32) {
        let mut filtered = [0.0_f32; VAD_FRAME_SAMPLES];
        let mut sum = 0.0_f32;

        for (dst, &raw) in filtered.iter_mut().zip(frame.iter()) {
            let x = if raw.is_finite() { raw } else { 0.0 };
            let y = HPF_ALPHA * (self.hpf_prev_y + x - self.hpf_prev_x);
            self.hpf_prev_x = x;
            self.hpf_prev_y = y;
            *dst = y;
            sum += y;
        }

        let mean = sum / VAD_FRAME_SAMPLES as f32;
        let mut energy_sum = 0.0_f64;
        let mut crossings = 0_usize;
        let mut prev_ac = filtered[0] - mean;
        energy_sum += f64::from(prev_ac) * f64::from(prev_ac);

        for &sample in &filtered[1..] {
            let ac = sample - mean;
            energy_sum += f64::from(ac) * f64::from(ac);
            if (prev_ac < 0.0 && ac >= 0.0) || (prev_ac > 0.0 && ac <= 0.0) {
                crossings += 1;
            }
            prev_ac = ac;
        }

        let rms = (energy_sum / VAD_FRAME_SAMPLES as f64).sqrt() as f32;
        let zcr = crossings as f32 / (VAD_FRAME_SAMPLES - 1) as f32;
        (rms, zcr)
    }

    /// Updates the adaptive noise-floor RMS estimate from a non-speech frame.
    ///
    /// Accepts the frame's high-pass RMS energy and whether its ZCR fell inside
    /// the speech band. Pulls the noise floor down quickly during silence and
    /// nudges it upward gently during steady in-band ambient noise while
    /// clamping to `[MIN_NOISE_FLOOR_RMS, MAX_NOISE_FLOOR_RMS]`. Cannot fail.
    fn update_noise_floor(&mut self, rms: f32, zcr_in_speech_band: bool) {
        if rms < self.noise_floor_rms {
            self.noise_floor_rms = (1.0 - NOISE_FLOOR_DOWN_ALPHA) * self.noise_floor_rms
                + NOISE_FLOOR_DOWN_ALPHA * rms;
        } else if zcr_in_speech_band {
            self.noise_floor_rms =
                (1.0 - NOISE_FLOOR_UP_ALPHA) * self.noise_floor_rms + NOISE_FLOOR_UP_ALPHA * rms;
        }
        self.noise_floor_rms = self
            .noise_floor_rms
            .clamp(MIN_NOISE_FLOOR_RMS, MAX_NOISE_FLOOR_RMS);
    }

    /// Appends one frame to the bounded 500 ms pre-roll ring buffer.
    ///
    /// Accepts a 320-sample PCM frame. Evicts the oldest frame once
    /// `PRE_ROLL_FRAMES` frames are retained. Cannot fail.
    fn push_pre_roll(&mut self, frame: [f32; VAD_FRAME_SAMPLES]) {
        push_pre_roll_frame(&mut self.pre_roll, frame);
    }
}

/// Appends one 20 ms frame into a bounded 500 ms pre-roll ring buffer.
///
/// Accepts the pre-roll deque and one 320-sample PCM frame. Evicts the oldest
/// frame when the deque already holds `PRE_ROLL_FRAMES` entries. Cannot fail.
fn push_pre_roll_frame(
    pre_roll: &mut VecDeque<[f32; VAD_FRAME_SAMPLES]>,
    frame: [f32; VAD_FRAME_SAMPLES],
) {
    if pre_roll.len() == PRE_ROLL_FRAMES {
        pre_roll.pop_front();
    }
    pre_roll.push_back(frame);
}

/// Appends samples from `src` into `dst` without exceeding `max_len`.
///
/// Accepts a destination vector, a source sample slice, and a maximum sample
/// count. Copies at most `max_len - dst.len()` samples. Cannot panic.
fn append_capped(dst: &mut Vec<f32>, src: &[f32], max_len: usize) {
    let room = max_len.saturating_sub(dst.len());
    if room > 0 {
        let take = room.min(src.len());
        dst.extend_from_slice(&src[..take]);
    }
}

/// Streams raw little-endian 16 kHz mono `f32` PCM from `reader` through [`ShortSpeechDetector`].
///
/// Accepts any byte stream emitting `f32le` 16 kHz mono PCM (such as FFmpeg's
/// `pipe:1`). Reads in 16 KiB chunks, validates finite samples and 4-byte
/// alignment, and returns a [`ShortSpeechResult`] immediately upon capturing
/// 60 s of speech, reaching the 600 s lead-in scan cap, or hitting EOF.
/// Returns `MediaError::Io` on read errors or `MediaError::DecodeFailed` on
/// non-finite PCM values or truncated trailing sample bytes.
pub fn extract_short_speech(reader: &mut dyn Read) -> Result<ShortSpeechResult, MediaError> {
    let mut detector = ShortSpeechDetector::new();
    let mut read_buf = [0_u8; READ_CHUNK_BYTES];
    let mut rem = [0_u8; 4];
    let mut rem_len = 0_usize;
    let mut samples_buf = Vec::with_capacity(READ_CHUNK_BYTES / 4 + 1);
    let mut total_samples_read = 0_usize;

    loop {
        let n = match reader.read(&mut read_buf) {
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(MediaError::Io(err)),
        };
        if n == 0 {
            if rem_len != 0 {
                return Err(MediaError::DecodeFailed(
                    "audio stream ended mid-sample (truncated f32le PCM)".to_string(),
                ));
            }
            return Ok(ShortSpeechResult {
                outcome: detector.finish(),
                total_samples_read,
                reached_eof: true,
            });
        }

        samples_buf.clear();
        let mut offset = 0_usize;

        if rem_len > 0 {
            let need = 4 - rem_len;
            let take = need.min(n);
            rem[rem_len..rem_len + take].copy_from_slice(&read_buf[..take]);
            rem_len += take;
            offset += take;
            if rem_len == 4 {
                let sample = f32::from_le_bytes(rem);
                if !sample.is_finite() {
                    return Err(MediaError::DecodeFailed(
                        "audio stream contains non-finite f32 PCM sample".to_string(),
                    ));
                }
                samples_buf.push(sample);
                rem_len = 0;
            }
        }

        let aligned_len = ((n - offset) / 4) * 4;
        for &chunk in read_buf[offset..offset + aligned_len].as_chunks::<4>().0 {
            let sample = f32::from_le_bytes(chunk);
            if !sample.is_finite() {
                return Err(MediaError::DecodeFailed(
                    "audio stream contains non-finite f32 PCM sample".to_string(),
                ));
            }
            samples_buf.push(sample);
        }

        let leftover = n - (offset + aligned_len);
        if leftover > 0 {
            rem[..leftover].copy_from_slice(&read_buf[offset + aligned_len..n]);
            rem_len = leftover;
        }

        total_samples_read += samples_buf.len();

        match detector.push_samples(&samples_buf) {
            VadStep::Scanning => {}
            VadStep::Completed { start_sample, pcm } => {
                return Ok(ShortSpeechResult {
                    outcome: VadOutcome::Speech { start_sample, pcm },
                    total_samples_read,
                    reached_eof: false,
                });
            }
            VadStep::NoSpeech { scanned_samples } => {
                return Ok(ShortSpeechResult {
                    outcome: VadOutcome::NoSpeechDetected {
                        scanned_ms: samples_to_ms(scanned_samples),
                    },
                    total_samples_read,
                    reached_eof: false,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;
    use std::io::Cursor;

    fn synth_speech_samples(count: usize, start_index: usize, amplitude: f32) -> Vec<f32> {
        // Composite speech-band harmonic signal (320 Hz + 780 Hz + 1450 Hz) with syllable envelope.
        (0..count)
            .map(|i| {
                let t = (start_index + i) as f32 / SAMPLE_RATE_HZ as f32;
                let envelope = 0.65 + 0.35 * (TAU * 4.0 * t).sin();
                amplitude
                    * envelope
                    * (0.50 * (TAU * 320.0 * t).sin()
                        + 0.30 * (TAU * 780.0 * t).sin()
                        + 0.20 * (TAU * 1450.0 * t).sin())
            })
            .collect()
    }

    fn synth_hum_dc_samples(count: usize, dc_offset: f32, hum_amp: f32, hum_hz: f32) -> Vec<f32> {
        (0..count)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE_HZ as f32;
                dc_offset + hum_amp * (TAU * hum_hz * t).sin()
            })
            .collect()
    }

    fn f32_to_le_bytes(samples: &[f32]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(samples.len() * 4);
        for &s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn immediate_speech_at_zero_triggers_at_sample_zero() {
        let pcm = synth_speech_samples(5 * SAMPLE_RATE_HZ, 0, 0.25);
        let mut detector = ShortSpeechDetector::new();
        assert_eq!(detector.push_samples(&pcm), VadStep::Scanning);
        match detector.finish() {
            VadOutcome::Speech {
                start_sample,
                pcm: captured,
            } => {
                assert_eq!(start_sample, 0);
                assert_eq!(captured.len(), 5 * SAMPLE_RATE_HZ);
            }
            other => panic!("expected immediate speech, got {other:?}"),
        }
    }

    #[test]
    fn three_second_silence_intro_preserves_preroll_and_caps_at_sixty_seconds() {
        let intro_samples = 3 * SAMPLE_RATE_HZ;
        let speech_samples = 70 * SAMPLE_RATE_HZ;
        let mut signal = vec![0.0_f32; intro_samples];
        signal.extend(synth_speech_samples(speech_samples, intro_samples, 0.25));

        let mut detector = ShortSpeechDetector::new();
        let mut completed = None;
        for chunk in signal.chunks(4_096) {
            match detector.push_samples(chunk) {
                VadStep::Scanning => {}
                VadStep::Completed { start_sample, pcm } => {
                    completed = Some((start_sample, pcm));
                    break;
                }
                VadStep::NoSpeech { .. } => panic!("unexpected NoSpeech"),
            }
        }

        let (start_sample, captured) = completed.expect("should hit 60 s cap");
        assert!(
            start_sample < intro_samples,
            "pre-roll onset ({start_sample}) must precede speech start ({intro_samples})"
        );
        assert_eq!(start_sample, intro_samples - PRE_ROLL_SAMPLES);
        assert_eq!(captured.len(), MAX_CAPTURE_SAMPLES);
    }

    #[test]
    fn low_level_hum_and_dc_intro_rejected_until_speech_begins() {
        let intro_samples = 3 * SAMPLE_RATE_HZ;
        let speech_samples = 4 * SAMPLE_RATE_HZ;
        let mut signal = synth_hum_dc_samples(intro_samples, 0.08, 0.04, 60.0);
        // Add speech on top of the ongoing hum + DC to test real-world noisy rooms.
        let mut tail = synth_hum_dc_samples(speech_samples, 0.08, 0.04, 60.0);
        let speech = synth_speech_samples(speech_samples, intro_samples, 0.22);
        for (dst, s) in tail.iter_mut().zip(speech.iter()) {
            *dst += *s;
        }
        signal.extend(tail);

        let mut detector = ShortSpeechDetector::new();
        assert_eq!(detector.push_samples(&signal), VadStep::Scanning);
        match detector.finish() {
            VadOutcome::Speech { start_sample, pcm } => {
                // Onset must NOT false-trigger during the 3 s hum/DC intro; pre-roll lands around 2.5 s.
                assert!(
                    start_sample >= 2 * SAMPLE_RATE_HZ && start_sample < intro_samples,
                    "expected onset pre-roll in [2.0s, 3.0s), got {} ms",
                    samples_to_ms(start_sample)
                );
                assert!(pcm.len() >= speech_samples);
            }
            other => panic!("expected speech after hum/DC intro, got {other:?}"),
        }
    }

    #[test]
    fn all_silence_terminates_cleanly_at_eof_and_at_600s_scan_cap() {
        // Short silent clip finishes at EOF.
        let short_silence = vec![0.0_f32; 5 * SAMPLE_RATE_HZ];
        let mut cursor = Cursor::new(f32_to_le_bytes(&short_silence));
        let res = extract_short_speech(&mut cursor).expect("short silence should succeed");
        assert!(res.reached_eof);
        assert_eq!(
            res.outcome,
            VadOutcome::NoSpeechDetected { scanned_ms: 5_000 }
        );

        // Multi-hour silent stream hits the hard 600 s cap immediately without reading beyond it.
        let mut detector = ShortSpeechDetector::new();
        let one_sec_silence = vec![0.0_f32; SAMPLE_RATE_HZ];
        let mut stopped_at = None;
        for sec in 1..=650 {
            match detector.push_samples(&one_sec_silence) {
                VadStep::Scanning => {}
                VadStep::NoSpeech { scanned_samples } => {
                    stopped_at = Some((sec, scanned_samples));
                    break;
                }
                VadStep::Completed { .. } => panic!("silence must never trigger speech"),
            }
        }
        assert_eq!(stopped_at, Some((600, MAX_SCAN_SAMPLES)));
    }

    #[test]
    fn allows_brief_single_frame_dip_during_onset_and_rejects_short_clicks() {
        // 1 s silence + 4 frames click + 2 s silence + 8 speech + 1 silence dip + 8 speech + 1 s speech.
        let mut signal = vec![0.0_f32; SAMPLE_RATE_HZ];
        signal.extend(synth_speech_samples(4 * VAD_FRAME_SAMPLES, 0, 0.30));
        signal.extend(vec![0.0_f32; 2 * SAMPLE_RATE_HZ]);

        let real_speech_start = signal.len();
        signal.extend(synth_speech_samples(8 * VAD_FRAME_SAMPLES, 0, 0.25));
        signal.extend(vec![0.0_f32; VAD_FRAME_SAMPLES]); // 1-frame dip allowed
        signal.extend(synth_speech_samples(8 * VAD_FRAME_SAMPLES, 9 * 320, 0.25));
        signal.extend(synth_speech_samples(SAMPLE_RATE_HZ, 17 * 320, 0.25));

        let mut detector = ShortSpeechDetector::new();
        assert_eq!(detector.push_samples(&signal), VadStep::Scanning);
        match detector.finish() {
            VadOutcome::Speech { start_sample, .. } => {
                assert_eq!(start_sample, real_speech_start - PRE_ROLL_SAMPLES);
            }
            other => panic!("expected speech after 1-frame dip, got {other:?}"),
        }
    }

    #[test]
    fn extract_short_speech_handles_unaligned_reads_and_rejects_corrupt_pcm() {
        struct TinyChunkReader<'a> {
            data: &'a [u8],
            pos: usize,
        }
        impl Read for TinyChunkReader<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.pos >= self.data.len() {
                    return Ok(0);
                }
                let take = 7.min(buf.len()).min(self.data.len() - self.pos);
                buf[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
                self.pos += take;
                Ok(take)
            }
        }

        let speech = synth_speech_samples(2 * SAMPLE_RATE_HZ, 0, 0.25);
        let raw = f32_to_le_bytes(&speech);
        let mut reader = TinyChunkReader { data: &raw, pos: 0 };
        let result = extract_short_speech(&mut reader).expect("unaligned reads should work");
        assert!(result.reached_eof);
        assert_eq!(result.total_samples_read, speech.len());
        assert!(matches!(
            result.outcome,
            VadOutcome::Speech {
                start_sample: 0,
                ..
            }
        ));

        // Truncated trailing bytes fail with DecodeFailed.
        let mut truncated = Cursor::new(vec![0_u8; 6]);
        let err = extract_short_speech(&mut truncated).expect_err("truncated PCM must fail");
        assert!(matches!(err, MediaError::DecodeFailed(_)));

        // NaN sample fails with DecodeFailed.
        let mut nan_reader = Cursor::new(f32::NAN.to_le_bytes());
        let err = extract_short_speech(&mut nan_reader).expect_err("NaN PCM must fail");
        assert!(matches!(err, MediaError::DecodeFailed(_)));
    }
}
