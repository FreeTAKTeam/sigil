//! Load one recognizer, then time the full Kodiak sequence on 16 kHz mono WAVs.
//!
//! ```text
//! cargo run --release -p kodiak-asr --example bench -- whisper models/ggml-base.en.bin a.wav b.wav
//! cargo run --release -p kodiak-asr --features parakeet --example bench -- parakeet models/<dir> a.wav
//! ```

use std::path::Path;
use std::time::Instant;

use kodiak_core::{AudioInput, Engine};
use kodiak_intent::DeterministicClassifier;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [backend, model, wavs @ ..] = arguments.as_slice() else {
        return Err("usage: bench <whisper|parakeet> <model> <file.wav>...".into());
    };
    let backend = match backend.as_str() {
        #[cfg(feature = "whisper")]
        "whisper" => kodiak_asr::Backend::Whisper,
        #[cfg(feature = "parakeet")]
        "parakeet" => kodiak_asr::Backend::Parakeet,
        other => return Err(format!("backend {other} is not enabled").into()),
    };
    let started = Instant::now();
    let transcriber = kodiak_asr::load(backend, Path::new(model))?;
    let engine = Engine::new(DeterministicClassifier::new()).with_transcriber(transcriber);
    println!("model load: {} ms", started.elapsed().as_millis());
    for wav in wavs {
        let mut reader = hound::WavReader::open(wav)?;
        let spec = reader.spec();
        if spec.channels != 1
            || spec.sample_rate != 16_000
            || spec.sample_format != hound::SampleFormat::Int
        {
            println!("{wav}: skipped, not 16 kHz mono PCM");
            continue;
        }
        let scale = f32::from(i16::MAX);
        let samples: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.map(|s| f32::from(s) / scale))
            .collect::<Result<_, _>>()?;
        #[expect(clippy::cast_precision_loss)]
        let seconds = samples.len() as f64 / 16_000.0;
        let started = Instant::now();
        let result = engine.process_audio(&AudioInput::mono_16k(samples))?;
        let elapsed = started.elapsed().as_secs_f64();
        println!(
            "{wav}: audio {seconds:.2} s, processed in {:.0} ms (real-time factor {:.2}) -> {}",
            elapsed * 1000.0,
            elapsed / seconds,
            serde_json::to_string(&result)?
        );
    }
    Ok(())
}
