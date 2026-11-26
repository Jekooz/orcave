//! Proof test: ORCAVE-ALPHA-R3Q — does nonzero PCM reach Windows output?
//!
//! Mirrors AudioEngine's output thread exactly:
//!   cpal default_output_device -> rodio::OutputStream::try_default
//!   -> rodio::Sink::try_new -> SamplesBuffer append -> sink.play()
//!   -> poll sink.get_pos() to confirm the playback clock advances from
//!   actual audio processing.
//!
//! Fixture: controlled low-volume synthetic WAV decoded through the engine's
//! real symphonia decode path (decode_from_source), so we verify nonzero
//! frames AND nonzero-but-quiet PCM, not just device enumeration.
//!
//! Run: cargo test -p orcave-audio --test audio_output_proof -- --nocapture --test-threads=1

use cpal::traits::{DeviceTrait, HostTrait};
use hound;
use orcave_audio::{AudioEngine, LocalFileSource};
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use symphonia::core::io::MediaSource;

/// Write a controlled low-volume synthetic WAV fixture via hound (valid WAV,
/// symphonia-decodable). Returns the WAV byte count and the temp path.
fn write_low_volume_wav(
    path: &PathBuf,
    freq: f32,
    duration_ms: u32,
    sample_rate: u32,
    volume: f32,
) -> usize {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let file = File::create(path).expect("hound: create output file failed");
    let mut writer = hound::WavWriter::new(file, spec).expect("hound WavWriter::new failed");
    let amp = (volume * 30000.0) as i32;
    let num_samples = (sample_rate as f64 * duration_ms as f64 / 1000.0) as u32;
    for i in 0..num_samples {
        let t = i as f32 / sample_rate as f32;
        let s = (freq * 2.0 * std::f32::consts::PI * t).sin();
        let sample = (s * amp as f32) as i16;
        for _ in 0..2 {
            writer
                .write_sample(sample)
                .expect("hound write_sample failed");
        }
    }
    writer.finalize().expect("hound finalize failed");
    std::fs::metadata(path).unwrap().len() as usize
}

fn rms_of(frames: &[f32]) -> f64 {
    if frames.is_empty() {
        return 0.0;
    }
    let sum: f64 = frames.iter().map(|&s| (s as f64).powi(2)).sum();
    (sum / frames.len() as f64).sqrt()
}

#[test]
fn proof_pcm_reaches_windows_output() {
    // ---- Stage 2: actual output-device identity (cpal) ----
    let host = cpal::default_host();
    let devices: Vec<_> = host
        .output_devices()
        .expect("cpal output_devices() failed")
        .collect();
    let default = host
        .default_output_device()
        .expect("cpal default_output_device() returned None");
    let default_name = default.name().unwrap_or_default();
    println!(
        "PROOF stage 2: cpal output devices enumerated = {}",
        devices.len()
    );
    for d in &devices {
        println!(
            "PROOF stage 2:   output device = {:?}",
            d.name().unwrap_or_default()
        );
    }
    println!("PROOF stage 2: default_output_device = {:?}", default_name);
    assert!(!devices.is_empty(), "no output devices enumerated");
    assert!(
        !default_name.is_empty(),
        "default output device has no name"
    );
    // Stage 12 guard: enumeration != rendering; we continue past this point.

    // ---- Stage 11: controlled low-volume synthetic WAV fixture ----
    let sample_rate: u32 = 48000;
    let path: PathBuf =
        std::env::temp_dir().join(format!("orcave_proof_lowvol_{}.wav", std::process::id()));
    let wav_len = write_low_volume_wav(&path, 440.0, 400, sample_rate, 0.08); // quiet 440 Hz tone
    println!("PROOF stage 11: wrote low-volume WAV fixture, {wav_len} bytes");

    // ---- Stage 4 (decode side): real engine decode path, nonzero frames ----
    let file = File::open(&path).unwrap();
    let source: Box<dyn MediaSource + Send> = Box::new(LocalFileSource::new(file));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (frames, total_samples, decoded_rate, decoded_ch) = rt
        .block_on(AudioEngine::decode_from_source(
            source,
            Arc::new(AtomicBool::new(false)),
        ))
        .expect("engine decode_from_source failed on low-volume WAV fixture");
    let rms = rms_of(&frames);
    println!(
        "PROOF stage 4/11: decoded frames={} total_samples={} rate={} ch={} rms={:.6}",
        frames.len(),
        total_samples,
        decoded_rate,
        decoded_ch,
        rms
    );
    assert!(!frames.is_empty(), "decoded PCM frame buffer is empty");
    assert!(total_samples > 0, "decode reported zero samples");
    assert!(rms > 0.0, "decoded PCM is silent (all zeros)");
    assert!(
        rms < 0.5,
        "fixture not low-volume: rms={rms} (expected quiet tone)"
    );

    // ---- Stages 3, 5, 6: rodio stream + sink creation succeeds ----
    let (stream, handle) = rodio::OutputStream::try_default()
        .expect("rodio OutputStream::try_default failed — no output stream opened");
    let sink =
        rodio::Sink::try_new(&handle).expect("rodio Sink::try_new failed — sink not created");
    println!("PROOF stage 3/5: rodio OutputStream + Sink created on default device");
    assert!(sink.empty(), "fresh sink should be empty before append");

    // ---- Stage 5: sink.append() called with nonzero frames ----
    let frame_count = frames.len();
    sink.append(rodio::buffer::SamplesBuffer::new(
        decoded_ch,
        decoded_rate,
        frames,
    ));
    println!("PROOF stage 5: sink.append() called with {frame_count} nonzero frames");
    assert!(!sink.empty(), "sink empty immediately after append");

    // ---- Stage 5: sink.play() actually called ----
    sink.play();
    println!("PROOF stage 5: sink.play() called");

    // ---- Stages 6, 7: sink stays alive; playback clock advances ----
    let mut prev_pos = sink.get_pos();
    let mut max_pos = prev_pos;
    let mut advanced = false;
    let mut non_empty_seen = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
        let pos = sink.get_pos();
        if !sink.empty() {
            non_empty_seen = true;
        }
        if pos > prev_pos {
            advanced = true;
        }
        if pos > max_pos {
            max_pos = pos;
        }
        prev_pos = pos;
        // Once the short fixture has fully rendered, sink becomes empty; stop polling.
        if sink.empty() && non_empty_seen {
            break;
        }
    }
    // sink must not be inadvertently paused/stopped mid-render: it must have
    // been non-empty at some point and the clock must have advanced.
    assert!(
        non_empty_seen,
        "sink was empty the whole time — PCM not rendered (paused/dropped/empty)"
    );
    assert!(
        advanced,
        "playback clock did not advance (get_pos stuck) — no actual audio processing"
    );
    println!(
        "PROOF stage 6/7: sink alive during render; get_pos advanced {:?} -> {:?} (max {:?}); non_empty_seen={}",
        Duration::ZERO,
        prev_pos,
        max_pos,
        non_empty_seen
    );

    // ---- Stage 9: sink not inadvertently dropped ----
    // sink is still in scope here; stream/handle keep the device open.
    // After the fixture renders, empty() becomes true (expected end of track),
    // which is NOT a failure — the sink simply finished its one buffer.
    println!(
        "PROOF stage 9: sink dropped intentionally at test end; final empty()={}",
        sink.empty()
    );

    let _ = stream; // keep stream alive until end of scope
    let _ = std::fs::remove_file(&path);

    println!(
        "PROOF CONCLUSION: nonzero PCM reached Windows output device {:?}",
        default_name
    );
}
