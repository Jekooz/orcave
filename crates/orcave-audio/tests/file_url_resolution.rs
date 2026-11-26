//! End-to-end `file://` URL -> path -> `File::open` -> decode resolution tests.
//!
//! Exercises `orcave_audio::file_url_to_path` (the exact conversion used by
//! `AudioEngine::decode_track`) against real files created in the OS temp
//! directory, including spaces, Unicode names, and a real MP3 copied from the
//! test fixtures. User source audio files are never modified.

use orcave_audio::{AudioEngine, LocalFileSource};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

fn rt_block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Runtime::new()
        .expect("failed to create Tokio runtime")
        .block_on(f)
}

/// Minimal RIFF/PCM WAV writer (mirrors `make_wav` in lib.rs tests).
fn make_wav(freq: f32, duration_ms: u32, sample_rate: u32) -> Vec<u8> {
    let channels: u16 = 2;
    let bits: u16 = 16;
    let byte_rate = sample_rate * channels as u32 * (bits / 8) as u32;
    let block_align = channels * (bits / 8);
    let num_samples = (sample_rate as f64 * duration_ms as f64 / 1000.0) as u32;
    let data_size = num_samples as usize * block_align as usize;
    let chunk_size = 4 + 24 + 8 + data_size;
    let mut buf = Vec::with_capacity(8 + chunk_size);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(chunk_size as u32).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&(16u32).to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&(sample_rate as u32).to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&bits.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(data_size as u32).to_le_bytes());
    for i in 0..num_samples {
        let t = i as f32 / sample_rate as f32;
        let s = (freq * 2.0 * std::f32::consts::PI * t).sin();
        let sample = (s * 15000.0) as i16;
        for _ in 0..channels {
            buf.extend_from_slice(&sample.to_le_bytes());
        }
    }
    buf
}

/// Root for all resolution test fixtures (OS temp dir, never the user's library).
fn resolution_root() -> PathBuf {
    std::env::temp_dir().join("orcave_file_url_resolution")
}

/// Mimic `LibraryService::path_to_file_url`: backslashes -> forward slashes,
/// `file:///` prefix on Windows. Does NOT percent-encode.
fn url_from_path(path: &Path) -> String {
    let display = path.display().to_string();
    let normalized = display.replace('\\', "/");
    format!("file:///{normalized}")
}

fn setup_dir(name: &str) -> PathBuf {
    let dir = resolution_root().join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn teardown_dir(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

fn decode_path(path: &Path) -> (Vec<f32>, u64, u32, u16) {
    let file = File::open(path).expect("failed to open test file");
    let source = Box::new(LocalFileSource::new(file));
    rt_block_on(AudioEngine::decode_from_source(
        source,
        Arc::new(AtomicBool::new(false)),
    ))
    .expect("decode failed")
}

/// `file:///C:/.../song.wav` (literal space in dir name, as emitted by
/// `path_to_file_url`) -> `File::open` -> decode succeeds.
#[test]
fn test_file_url_windows_local_spaced_path_decodes() {
    let dir = setup_dir("spaced dir");
    let wav = make_wav(440.0, 500, 48000);
    let path = dir.join("song file.wav");
    fs::write(&path, &wav).unwrap();

    let url = url_from_path(&path);
    let resolved = AudioEngine::file_url_to_path(&url).expect("URL must resolve");
    assert_eq!(resolved, path, "resolved path must match on-disk path");

    let (frames, total_samples, sample_rate, channels) = decode_path(&resolved);
    assert!(!frames.is_empty());
    assert!(total_samples > 0);
    assert_eq!(sample_rate, 48000);
    assert_eq!(channels, 2);

    teardown_dir(&dir);
}

/// Explicitly percent-encoded URL (`%20` -> space) opens the real file.
#[test]
fn test_file_url_percent_encoded_space_decodes() {
    let dir = setup_dir("encoded dir");
    let wav = make_wav(440.0, 500, 48000);
    let path = dir.join("song file.wav");
    fs::write(&path, &wav).unwrap();

    // Drive letter may differ from this machine's temp drive; rebuild URL with
    // the real drive but encoded segments.
    let real = url_from_path(&path); // file:///<drive>:/.../encoded dir/song file.wav
    let encoded = real
        .replace("encoded dir", "encoded%20dir")
        .replace("song file.wav", "song%20file.wav");
    let resolved = AudioEngine::file_url_to_path(&encoded).expect("encoded URL must resolve");
    assert_eq!(resolved, path, "%20 must decode to literal space");

    let (frames, _, _, _) = decode_path(&resolved);
    assert!(!frames.is_empty());

    teardown_dir(&dir);
}

/// Unicode filename: URL with literal `é` and URL with `%C3%A9` both open it.
#[test]
fn test_file_url_unicode_decodes() {
    let dir = setup_dir("unicode dir");
    let wav = make_wav(880.0, 300, 48000);
    let path = dir.join("café.wav");
    fs::write(&path, &wav).unwrap();

    let url_literal = url_from_path(&path);
    let resolved =
        AudioEngine::file_url_to_path(&url_literal).expect("literal Unicode URL must resolve");
    assert_eq!(resolved, path);

    let url_encoded = url_literal.replace("café.wav", "caf%C3%A9.wav");
    let resolved_enc = AudioEngine::file_url_to_path(&url_encoded)
        .expect("percent-encoded Unicode URL must resolve");
    assert_eq!(resolved_enc, path, "%C3%A9 must decode to é");

    let (frames, _, _, _) = decode_path(&resolved);
    assert!(!frames.is_empty());

    teardown_dir(&dir);
}

/// Real MP3 (copied from fixture, never moved/modified at source) with a space
/// in its file name resolves and decodes through `file_url_to_path`.
#[test]
fn test_file_url_real_mp3_with_space_decodes() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/audio/test_440hz_5s.mp3");
    assert!(fixture.exists(), "MP3 fixture missing at {:?}", fixture);

    let dir = setup_dir("My Music");
    let path = dir.join("song file.mp3");
    fs::copy(&fixture, &path).expect("failed to copy MP3 fixture");

    let url = url_from_path(&path);
    let resolved = AudioEngine::file_url_to_path(&url).expect("MP3 URL must resolve");
    assert_eq!(resolved, path);

    let (frames, total_samples, sample_rate, channels) = decode_path(&resolved);
    assert!(!frames.is_empty());
    assert!(total_samples > 0);
    assert_eq!(sample_rate, 44100);
    assert_eq!(channels, 2);

    teardown_dir(&dir);
}

/// Missing file: `File::open` fails (the error `decode_track` maps to
/// `AudioError::Stream`).
#[test]
fn test_file_url_missing_file_fails_to_open() {
    let dir = setup_dir("missing dir");
    let path = dir.join("does not exist.wav");
    assert!(!path.exists());

    let url = url_from_path(&path);
    let resolved =
        AudioEngine::file_url_to_path(&url).expect("URL must still resolve syntactically");
    let result = File::open(&resolved);
    assert!(
        result.is_err(),
        "missing file must fail to open, got {:?}",
        result
    );

    teardown_dir(&dir);
}

/// Synthetic WAV (no fixture dependency) decodes through the full conversion.
#[test]
fn test_file_url_synthetic_wav_decodes() {
    let dir = setup_dir("synthetic");
    let wav = make_wav(1000.0, 250, 44100);
    let path = dir.join("tone.wav");
    fs::write(&path, &wav).unwrap();

    let url = url_from_path(&path);
    let resolved = AudioEngine::file_url_to_path(&url).expect("synthetic WAV URL must resolve");
    assert_eq!(resolved, path);

    let (frames, total_samples, sample_rate, channels) = decode_path(&resolved);
    assert!(!frames.is_empty());
    assert!(total_samples > 0);
    assert_eq!(sample_rate, 44100);
    assert_eq!(channels, 2);

    let start = Instant::now();
    let _ = rt_block_on(AudioEngine::decode_from_source(
        Box::new(LocalFileSource::new(File::open(&path).unwrap())),
        Arc::new(AtomicBool::new(false)),
    ));
    eprintln!(
        "file_url_resolution synthetic WAV decode: {}ms, {} frames",
        start.elapsed().as_millis(),
        frames.len()
    );

    teardown_dir(&dir);
}
