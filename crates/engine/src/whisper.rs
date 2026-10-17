//! The speech model (cargo feature "speech"): whisper.cpp's small.en through whisper-rs.
//! The ggml model is read from settings' models_dir. The app downloads it with `models::download`.

use crate::transcribe::{Segment, SpeechModel, SpeechOptions};
use std::path::Path;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// small.en, as transcribe.MODEL. The 8-bit build comes first: on CPUs without AVX (the
/// Celeron J4125 appliance) it runs much faster.
pub const MODEL_FILES: [&str; 2] = ["ggml-small.en-q8_0.bin", "ggml-small.en.bin"];
pub const MODEL_FILE: &str = MODEL_FILES[1];

pub struct Whisper {
    ctx: WhisperContext,
    /// Silero VAD for whisper.cpp (ggml-silero-*.bin in models_dir), for vad_filter.
    vad: Option<String>,
}

pub fn load(models_dir: &Path) -> Result<Box<dyn SpeechModel>, String> {
    let path = MODEL_FILES.iter().map(|f| models_dir.join(f)).find(|p| p.exists())
        .unwrap_or_else(|| models_dir.join(MODEL_FILE));
    if !path.exists() {
        return Err(format!("speech model {} not found (download {MODEL_FILE} from huggingface.co/ggerganov/whisper.cpp)",
                           path.display()));
    }
    let ctx = WhisperContext::new_with_params(&path, WhisperContextParameters::default()).map_err(|e| e.to_string())?;
    let vad = std::fs::read_dir(models_dir).into_iter().flatten().flatten().map(|e| e.path())
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("ggml-silero")))
        .map(|p| p.to_string_lossy().into_owned());
    Ok(Box::new(Whisper { ctx, vad }))
}

impl SpeechModel for Whisper {
    fn transcribe(&mut self, audio: &[f32], opts: &SpeechOptions) -> Result<(Vec<Segment>, Option<String>), String> {
        let mut state = self.ctx.create_state().map_err(|e| e.to_string())?;
        let strategy = if opts.beam_size > 1 {
            SamplingStrategy::BeamSearch { beam_size: opts.beam_size as i32, patience: -1.0 }
        } else {
            SamplingStrategy::Greedy { best_of: 1 }
        };
        let mut p = FullParams::new(strategy);
        // speech runs rarely (detector calls that don't match by sound), so it may use the cores
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).min(4);
        p.set_n_threads(threads as i32);
        // Whisper pads every clip to 30 s; a milepost number is about 2 s. Shrinking the
        // encoder's window to the clip (plus a margin) cuts the work about tenfold.
        let ctx = ctx_for(audio.len());
        if ctx < 1500 {
            p.set_audio_ctx(ctx);
        }
        p.set_language(Some("en"));
        p.set_initial_prompt(&opts.initial_prompt);
        p.set_no_context(!opts.condition_on_previous_text);
        if let Some(t) = opts.temperature {
            // one pass at this temperature, no fallback retries
            p.set_temperature(t as f32);
            p.set_temperature_inc(0.0);
        }
        if let Some(n) = opts.max_new_tokens {
            p.set_max_tokens(n as i32);
        }
        p.set_no_timestamps(opts.without_timestamps);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_special(false);
        p.set_print_timestamps(false);
        if opts.vad_filter {
            if let Some(v) = &self.vad {
                p.enable_vad(true);
                p.set_vad_model_path(Some(v));
            }
        }
        state.full(p, audio).map_err(|e| e.to_string())?;
        let mut out = vec![];
        for seg in state.as_iter() {
            let text = seg.to_str_lossy().map_err(|e| e.to_string())?.into_owned();
            // avg_logprob: the mean log probability of the text tokens
            let mut lp = vec![];
            for i in 0..seg.n_tokens() {
                if let Some(tok) = seg.get_token(i) {
                    let s = tok.to_str_lossy().map(|c| c.into_owned()).unwrap_or_default();
                    if !(s.starts_with("[_") || s.starts_with("<|")) {
                        lp.push((tok.token_probability().max(1e-10) as f64).ln());
                    }
                }
            }
            let avg = if lp.is_empty() { f64::NEG_INFINITY } else { lp.iter().sum::<f64>() / lp.len() as f64 };
            out.push(Segment {
                text,
                start: seg.start_timestamp() as f64 / 100.0,
                end: seg.end_timestamp() as f64 / 100.0,
                avg_logprob: avg,
                no_speech_prob: seg.no_speech_probability() as f64,
            });
        }
        Ok((out, Some("en".into())))
    }
}

/// Encoder frames for this many 16 kHz samples: 50 per second, plus 1 s of margin, at least 3 s.
fn ctx_for(samples: usize) -> i32 {
    let frames = (samples as f64 / 16_000.0 * 50.0).ceil() as i32 + 50;
    frames.clamp(150, 1500)
}
