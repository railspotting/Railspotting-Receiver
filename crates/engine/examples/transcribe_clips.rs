//! Runs the real speech model on saved voice clips: the full transcript (forced, as for a call
//! that matched no template) and the milepost reader. For checking a model or build by ear.
//!
//!     cargo run --release --features speech --example transcribe_clips -- <models_dir> <clip.wav>...
//!
//! With CLEAN=1 in the environment, each clip goes through the voice filter first. With
//! WATCH=<MHz>,<railroad> each clip is read as a call on that watched channel, the way the receiver
//! reads one (voice filter, the railroad's detector prompt, milepost matching), and
//! KNOWN=<json list> sets the headend's detector list for it.

use std::path::Path;
use std::time::Instant;

use railspotting_engine::{transcribe::Transcriber, wav};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: transcribe_clips <models_dir> <clip.wav>...");
        std::process::exit(2);
    }
    // a watched-channel run needs other calls off, as on a receiver
    let tr = Transcriber::new(Path::new(&args[0]), None, 0, None, None, std::env::var_os("WATCH").is_none());
    if !tr.speech_available() {
        eprintln!("speech unavailable: build with --features speech");
        std::process::exit(1);
    }
    for clip in &args[1..] {
        let audio = match wav::read(Path::new(clip)) {
            Ok(w) if std::env::var_os("CLEAN").is_some() => railspotting_detect::detectors::clean_voice(&w.samples()),
            Ok(w) => w.samples(),
            Err(e) => {
                println!("{clip}: {e}");
                continue;
            }
        };
        let t = Instant::now();
        let watch = std::env::var("WATCH").ok().and_then(|w| {
            let (f, r) = w.split_once(',')?;
            Some((f.parse::<f64>().ok()?, r.to_string()))
        });
        if let Some((f, _)) = &watch {
            tr.set_watch_freqs(vec![*f]);
            let known = std::env::var("KNOWN").ok().and_then(|k| serde_json::from_str::<Vec<serde_json::Value>>(&k).ok());
            tr.set_known_detectors(known);
        }
        let event = watch.as_ref().map(|(f, r)| serde_json::json!({"freq": f, "railroad": r}));
        let result = tr.transcribe(&audio, event.as_ref(), watch.is_none());
        if let Ok(Some(m)) = &result {
            println!("parsed: {}", m.get("parsed").map_or("-".into(), |p| p.to_string()));
        }
        let text = match result {
            Ok(Some(m)) => m.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            Ok(None) => format!("(none: {:?})", tr.last_reject()),
            Err(e) => format!("(error: {e})"),
        };
        let full_s = t.elapsed().as_secs_f64();
        let name = Path::new(clip).file_name().and_then(|n| n.to_str()).unwrap_or(clip);
        println!("{name} {full_s:.1}s {text:?}");
    }
}
