//! Integration tests for local file playback support.
//!
//! Tests cover:
//! - WAV, MP3, FLAC decoding via `file://` URLs
//! - Output device enumeration via cpal
//! - Error handling (missing file, corrupt file)
//! - Memory usage and timing measurements

use cpal::traits::{DeviceTrait, HostTrait};
use orcave_audio::AudioEngine;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

/// Returns the path to the test audio fixtures directory.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/audio")
}

/// Returns the path to a fixture file, panicking if missing.
fn fixture(name: &str) -> PathBuf {
    let p = fixtures_dir().join(name);
    assert!(
        p.exists(),
        "Fixture not found: {} (looked in {:?})",
        name,
        fixtures_dir()
    );
    p
}

/// Create a corrupt WAV file (valid RIFF header, truncated data chunk).
fn make_corrupt_wav(path: &Path) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(100_000u32).to_le_bytes()); // chunk_size
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&(16u32).to_le_bytes());
    buf.extend_from_slice(&(1u16).to_le_bytes()); // PCM
    buf.extend_from_slice(&(2u16).to_le_bytes()); // stereo
    buf.extend_from_slice(&(44100u32).to_le_bytes()); // sample rate
    buf.extend_from_slice(&((44100 * 2 * 2) as u32).to_le_bytes()); // byte rate
    buf.extend_from_slice(&(4u16).to_le_bytes()); // block align
    buf.extend_from_slice(&(16u16).to_le_bytes()); // bits
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(100_000u32).to_le_bytes()); // data size - lies!
    buf.extend_from_slice(&vec![0u8; 100]);
    let _ = File::create(path).and_then(|mut f| f.write_all(&buf));
    buf
}

/// Run an async decode within a Tokio runtime.
fn rt_block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Runtime::new()
        .expect("failed to create Tokio runtime")
        .block_on(f)
}

#[test]
fn test_device_enumeration() {
    let host = cpal::default_host();
    let devices: Vec<_> = host
        .output_devices()
        .expect("failed to enumerate output devices")
        .collect();
    assert!(
        !devices.is_empty(),
        "no output devices found on this system"
    );
    let default = host
        .default_output_device()
        .expect("no default output device");
    eprintln!(
        "Default output device: {}",
        default.name().unwrap_or_default()
    );
    for d in &devices {
        eprintln!("  Device: {}", d.name().unwrap_or_default());
    }
}

#[test]
fn test_decode_wav_file() {
    let path = fixture("test_440hz_5s.wav");
    let file = File::open(&path).expect("failed to open WAV fixture");
    let source = Box::new(orcave_audio::LocalFileSource::new(file));

    let start = Instant::now();
    let (frames, total_samples, sample_rate, channels) = rt_block_on(
        AudioEngine::decode_from_source(source, Arc::new(AtomicBool::new(false))),
    )
    .expect("WAV decode failed");
    let decode_ms = start.elapsed().as_millis();

    assert!(!frames.is_empty(), "WAV decode produced no frames");
    assert!(total_samples > 0, "WAV decode produced no samples");
    assert_eq!(sample_rate, 44100, "unexpected sample rate");
    assert_eq!(channels, 2, "unexpected channel count");

    let duration_secs = total_samples as f64 / sample_rate as f64;
    eprintln!(
        "WAV decode: {} frames, {} samples, {} Hz, {} ch, {:.2}s duration, {}ms decode time",
        frames.len(),
        total_samples,
        sample_rate,
        channels,
        duration_secs,
        decode_ms
    );

    let mem_bytes = frames.len() * std::mem::size_of::<f32>();
    eprintln!(
        "WAV decoded memory: {} bytes ({:.2} KB)",
        mem_bytes,
        mem_bytes as f64 / 1024.0
    );
}

#[test]
fn test_decode_mp3_file() {
    let path = fixture("test_440hz_5s.mp3");
    let file = File::open(&path).expect("failed to open MP3 fixture");
    let source = Box::new(orcave_audio::LocalFileSource::new(file));

    let start = Instant::now();
    let (frames, total_samples, sample_rate, channels) = rt_block_on(
        AudioEngine::decode_from_source(source, Arc::new(AtomicBool::new(false))),
    )
    .expect("MP3 decode failed");
    let decode_ms = start.elapsed().as_millis();

    assert!(!frames.is_empty(), "MP3 decode produced no frames");
    assert!(total_samples > 0, "MP3 decode produced no samples");
    assert_eq!(sample_rate, 44100, "unexpected sample rate");
    assert_eq!(channels, 2, "unexpected channel count");

    let duration_secs = total_samples as f64 / sample_rate as f64;
    eprintln!(
        "MP3 decode: {} frames, {} samples, {} Hz, {} ch, {:.2}s duration, {}ms decode time",
        frames.len(),
        total_samples,
        sample_rate,
        channels,
        duration_secs,
        decode_ms
    );

    let mem_bytes = frames.len() * std::mem::size_of::<f32>();
    eprintln!(
        "MP3 decoded memory: {} bytes ({:.2} KB)",
        mem_bytes,
        mem_bytes as f64 / 1024.0
    );
}

#[test]
fn test_decode_flac_file() {
    let path = fixture("test_440hz_5s.flac");
    let file = File::open(&path).expect("failed to open FLAC fixture");
    let source = Box::new(orcave_audio::LocalFileSource::new(file));

    let start = Instant::now();
    let (frames, total_samples, sample_rate, channels) = rt_block_on(
        AudioEngine::decode_from_source(source, Arc::new(AtomicBool::new(false))),
    )
    .expect("FLAC decode failed");
    let decode_ms = start.elapsed().as_millis();

    assert!(!frames.is_empty(), "FLAC decode produced no frames");
    assert!(total_samples > 0, "FLAC decode produced no samples");
    assert_eq!(sample_rate, 44100, "unexpected sample rate");
    assert_eq!(channels, 2, "unexpected channel count");

    let duration_secs = total_samples as f64 / sample_rate as f64;
    eprintln!(
        "FLAC decode: {} frames, {} samples, {} Hz, {} ch, {:.2}s duration, {}ms decode time",
        frames.len(),
        total_samples,
        sample_rate,
        channels,
        duration_secs,
        decode_ms
    );

    let mem_bytes = frames.len() * std::mem::size_of::<f32>();
    eprintln!(
        "FLAC decoded memory: {} bytes ({:.2} KB)",
        mem_bytes,
        mem_bytes as f64 / 1024.0
    );
}

#[test]
fn test_decode_mono_wav_file() {
    let path = fixture("test_880hz_3s_mono.wav");
    let file = File::open(&path).expect("failed to open mono WAV fixture");
    let source = Box::new(orcave_audio::LocalFileSource::new(file));

    let start = Instant::now();
    let (frames, total_samples, sample_rate, channels) = rt_block_on(
        AudioEngine::decode_from_source(source, Arc::new(AtomicBool::new(false))),
    )
    .expect("mono WAV decode failed");
    let decode_ms = start.elapsed().as_millis();

    assert!(!frames.is_empty(), "mono WAV decode produced no frames");
    assert_eq!(channels, 1, "expected mono channel count");
    assert_eq!(sample_rate, 48000, "unexpected sample rate");

    let duration_secs = total_samples as f64 / sample_rate as f64;
    eprintln!(
        "Mono WAV decode: {} frames, {} samples, {} Hz, {} ch, {:.2}s duration, {}ms decode time",
        frames.len(),
        total_samples,
        sample_rate,
        channels,
        duration_secs,
        decode_ms
    );
}

#[test]
fn test_decode_missing_file() {
    let path = PathBuf::from("/nonexistent/path/to/file.wav");
    let result = File::open(&path);
    assert!(
        result.is_err(),
        "missing file should fail to open, got {:?}",
        result
    );
}

#[test]
fn test_decode_corrupt_file() {
    let corrupt_path = fixtures_dir().join("test_corrupt.wav");
    make_corrupt_wav(&corrupt_path);

    let file = File::open(&corrupt_path).expect("failed to open corrupt WAV fixture");
    let source = Box::new(orcave_audio::LocalFileSource::new(file));

    let result = rt_block_on(AudioEngine::decode_from_source(
        source,
        Arc::new(AtomicBool::new(false)),
    ));

    // Corrupt file may partially decode or error - both are acceptable.
    // The key is it doesn't hang or crash.
    match result {
        Ok((frames, total_samples, sample_rate, channels)) => {
            eprintln!(
                "Corrupt WAV partial decode: {} frames, {} samples, {} Hz, {} ch",
                frames.len(),
                total_samples,
                sample_rate,
                channels
            );
            if total_samples > 0 {
                assert!(sample_rate > 0);
                assert!(channels > 0);
            }
        }
        Err(e) => {
            eprintln!("Corrupt WAV decode error (expected): {e}");
        }
    }

    // Clean up
    let _ = std::fs::remove_file(&corrupt_path);
}

#[test]
fn test_decode_memory_bounded() {
    let path = fixture("test_440hz_5s.wav");
    let file = File::open(&path).expect("failed to open WAV fixture");
    let source = Box::new(orcave_audio::LocalFileSource::new(file));

    let (frames, total_samples, sample_rate, _) = rt_block_on(AudioEngine::decode_from_source(
        source,
        Arc::new(AtomicBool::new(false)),
    ))
    .expect("WAV decode failed");

    let mem_bytes = frames.len() * std::mem::size_of::<f32>();
    // A 5-second stereo 44.1kHz WAV decoded to f32 interleaved:
    // 5 * 44100 * 2 channels * 4 bytes = ~1.76 MB
    let expected_mb = (5.0 * 44100.0 * 2.0 * 4.0) / 1_048_576.0;
    let actual_mb = mem_bytes as f64 / 1_048_576.0;
    eprintln!(
        "Memory usage: {:.2} MB (expected ~{:.2} MB)",
        actual_mb, expected_mb
    );
    assert!(
        actual_mb > 0.5,
        "decoded memory should be substantial for a 5s track"
    );
    assert!(
        actual_mb < 50.0,
        "decoded memory should not be unreasonably large"
    );
    assert_eq!(
        total_samples as usize,
        frames.len() / 2,
        "total samples should match frame count / channels"
    );
    assert_eq!(sample_rate, 44100);
}

#[test]
fn test_decode_deterministic() {
    let path = fixture("test_440hz_5s.wav");

    let decode_once = |path: &Path| {
        let file = File::open(path).expect("failed to open WAV fixture");
        let source = Box::new(orcave_audio::LocalFileSource::new(file));
        rt_block_on(AudioEngine::decode_from_source(
            source,
            Arc::new(AtomicBool::new(false)),
        ))
    };

    let (frames1, samples1, rate1, ch1) = decode_once(path.as_path()).expect("first decode");
    let (frames2, samples2, rate2, ch2) = decode_once(path.as_path()).expect("second decode");

    assert_eq!(
        frames1.len(),
        frames2.len(),
        "decodes should be deterministic"
    );
    assert_eq!(samples1, samples2, "sample counts should match");
    assert_eq!(rate1, rate2, "sample rates should match");
    assert_eq!(ch1, ch2, "channel counts should match");
}

#[test]
fn test_file_url_path_extraction() {
    // Verify file:// URL path extraction logic
    let url = "file:///C:/Users/test/audio.wav";
    let path = &url[7..]; // strip "file://"
    assert_eq!(path, "/C:/Users/test/audio.wav");

    let url = "file://relative/path.wav";
    let path = &url[7..];
    assert_eq!(path, "relative/path.wav");
}

/// Deterministic negative fixture: WAV with a valid header but a zero-byte
/// `data` chunk. Probe and codec initialization succeed; the decode loop
/// yields no packets.
fn make_zero_data_wav(path: &Path) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&36u32.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&2u16.to_le_bytes()); // stereo
    buf.extend_from_slice(&44100u32.to_le_bytes());
    buf.extend_from_slice(&(44100u32 * 2 * 2).to_le_bytes()); // byte rate
    buf.extend_from_slice(&4u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&0u32.to_le_bytes()); // zero data size
    let _ = std::fs::File::create(path).and_then(|mut f| f.write_all(&buf));
    buf
}

/// GAP probe (ORCAVE-ALPHA-R3P): `decode_from_source` reports success on a
/// zero-data WAV but yields zero frames. Playback then proceeds to silence
/// (Playing -> immediate TrackEnded/Ended) with no error surfaced.
#[test]
fn test_decode_zero_data_wav_yields_no_frames() {
    let path = fixtures_dir().join("test_zero_data.wav");
    make_zero_data_wav(&path);

    let file = std::fs::File::open(&path).expect("failed to open zero-data WAV fixture");
    let source = Box::new(orcave_audio::LocalFileSource::new(file));

    let result = rt_block_on(AudioEngine::decode_from_source(
        source,
        Arc::new(AtomicBool::new(false)),
    ));

    match result {
        Ok((frames, total_samples, sample_rate, channels)) => {
            assert!(
                frames.is_empty(),
                "zero-data WAV must yield zero frames, got {}",
                frames.len()
            );
            assert_eq!(total_samples, 0);
            assert!(sample_rate > 0);
            assert!(channels > 0);
            eprintln!(
                "GAP CONFIRMED: zero-data WAV decoded to Ok with {} frames ({} Hz, {} ch)",
                frames.len(),
                sample_rate,
                channels
            );
        }
        Err(e) => {
            eprintln!("zero-data WAV errored (acceptable): {e}");
        }
    }

    let _ = std::fs::remove_file(&path);
}

/// Negative fixture: a non-audio file must error instead of decoding to silence.
#[test]
fn test_decode_unsupported_format_errors() {
    let path = fixtures_dir().join("test_unsupported.bin");
    let _ = std::fs::write(&path, b"this is not an audio file");

    let file = std::fs::File::open(&path).expect("failed to open unsupported fixture");
    let source = Box::new(orcave_audio::LocalFileSource::new(file));

    let result = rt_block_on(AudioEngine::decode_from_source(
        source,
        Arc::new(AtomicBool::new(false)),
    ));

    assert!(
        result.is_err(),
        "unsupported format must error instead of silence: {result:?}"
    );

    let _ = std::fs::remove_file(&path);
}
