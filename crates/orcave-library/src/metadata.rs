//! Orcave Library - Metadata extraction from audio files using symphonia.
//!
//! Reads title, artist, album, duration, and embedded artwork bytes from a
//! file. Read-only: never modifies the source file.

use std::fs::File;
use std::io::Cursor;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use image::ImageFormat;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::default::get_probe;

use crate::models::TrackMetadata;

/// Audio extensions treated as scannable music files.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "wav", "ogg", "oga", "aac", "m4a", "mp4", "wma",
];

/// Whether a file path looks like an audio file we should scan.
pub fn is_audio_file(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => AUDIO_EXTENSIONS.contains(&ext.to_lowercase().as_str()),
        None => false,
    }
}

/// Extract metadata from an audio file using symphonia.
///
/// Returns title, artist, album, duration, and any embedded artwork bytes.
/// The source file is only opened for reading.
pub fn extract_metadata(path: &Path) -> Result<TrackMetadata> {
    let file = File::open(path)
        .with_context(|| format!("failed to open audio file: {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let hint = Hint::new();

    let probed = get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .with_context(|| format!("symphonia probe failed for: {}", path.display()))?;

    let mut format = probed.format;

    // Metadata: tags + visuals (embedded pictures).
    let mut title: Option<String> = None;
    let mut artist: Option<String> = None;
    let mut album: Option<String> = None;
    let mut artwork_bytes: Option<Vec<u8>> = None;

    if let Some(revision) = format.metadata().current() {
        for tag in revision.tags() {
            match tag.std_key {
                Some(symphonia::core::meta::StandardTagKey::TrackTitle) => {
                    if title.is_none() {
                        title = string_value(&tag.value);
                    }
                }
                Some(symphonia::core::meta::StandardTagKey::Artist)
                | Some(symphonia::core::meta::StandardTagKey::AlbumArtist) => {
                    if artist.is_none() {
                        artist = string_value(&tag.value);
                    }
                }
                Some(symphonia::core::meta::StandardTagKey::Album) => {
                    if album.is_none() {
                        album = string_value(&tag.value);
                    }
                }
                _ => {}
            }
        }
        // Embedded artwork: first visual whose data decodes as an image.
        if artwork_bytes.is_none() {
            for visual in revision.visuals() {
                if looks_like_image(&visual.data) {
                    artwork_bytes = Some(visual.data.to_vec());
                    break;
                }
            }
        }
    }

    // Duration from codec params: n_frames / sample_rate.
    let duration = format.tracks().first().and_then(|track| {
        track
            .codec_params
            .n_frames
            .zip(track.codec_params.sample_rate)
            .map(|(n_frames, sr)| Duration::from_secs_f64(n_frames as f64 / sr as f64))
    });

    Ok(TrackMetadata {
        title,
        artist,
        album,
        duration,
        artwork_bytes,
    })
}

/// Interpret a tag `Value` as a string.
fn string_value(value: &symphonia::core::meta::Value) -> Option<String> {
    match value {
        symphonia::core::meta::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Heuristic: does this byte buffer look like a decodable image?
fn looks_like_image(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > 5_000_000 {
        // 5 MB cap on embedded art candidates.
        return false;
    }
    image::load_from_memory(bytes).is_ok()
}

/// Extract embedded artwork and encode to PNG for stable caching.
/// Returns PNG-encoded bytes, or None if no artwork present.
pub fn extract_artwork_png(path: &Path) -> Result<Option<Vec<u8>>> {
    let meta = extract_metadata(path)?;
    let bytes = match meta.artwork_bytes {
        Some(b) => b,
        None => return Ok(None),
    };
    let img = image::load_from_memory(&bytes)
        .with_context(|| format!("embedded artwork is not a valid image: {}", path.display()))?;
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .with_context(|| "failed to encode artwork to PNG")?;
    Ok(Some(out))
}

/// Build a minimal valid WAV file in memory (no metadata, just audio).
/// `pub(crate)` so sibling modules (e.g. artwork tests) can reuse it.
pub(crate) fn make_wav(freq: f32, duration_ms: u32, sample_rate: u32) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn is_audio_file_recognizes_extensions() {
        assert!(is_audio_file(Path::new("song.mp3")));
        assert!(is_audio_file(Path::new("song.FLAC")));
        assert!(is_audio_file(Path::new("song.Wav")));
        assert!(!is_audio_file(Path::new("song.txt")));
        assert!(!is_audio_file(Path::new("song")));
        assert!(!is_audio_file(Path::new("image.png")));
    }

    #[test]
    fn extract_metadata_on_wav_returns_duration() {
        let wav = make_wav(440.0, 200, 48000);
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&wav).unwrap();
        let path = file.path();

        let meta = extract_metadata(path).expect("wav metadata extraction should succeed");
        assert!(meta.duration.is_some(), "wav should report a duration");
        assert!(meta.title.is_none(), "wav without tags has no title");
        assert!(meta.artist.is_none());
        assert!(meta.album.is_none());
        assert!(meta.artwork_bytes.is_none());
    }

    #[test]
    fn looks_like_image_rejects_non_images() {
        assert!(!looks_like_image(b"not an image at all"));
        assert!(!looks_like_image(b""));
        assert!(looks_like_image_accepts_valid_png());
    }

    fn looks_like_image_accepts_valid_png() -> bool {
        let img = image::RgbImage::new(2, 2);
        let dyn_img = image::DynamicImage::ImageRgb8(img);
        let mut buf = std::io::Cursor::new(Vec::new());
        let _ = dyn_img.write_to(&mut buf, image::ImageFormat::Png);
        looks_like_image(buf.get_ref())
    }

    #[test]
    fn string_value_extracts_string() {
        use symphonia::core::meta::Value;
        let v = Value::String("hello".into());
        assert_eq!(string_value(&v), Some("hello".to_string()));
        let v = Value::Binary((*b"bytes").into());
        assert_eq!(string_value(&v), None);
    }
}
