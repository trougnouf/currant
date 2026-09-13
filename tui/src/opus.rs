// ./tui/src/opus.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! .opus decoder: demuxes OGG with the `ogg` crate and decodes Opus packets
//! with libopus, exposing a `rodio::Source` of interleaved f32 samples.
//!
//! Symphonia (what rodio uses) has no Opus codec, so this fills that gap for
//! the desktop TUI. The whole file is decoded up front — Opus files are small
//! and this keeps the `Source` implementation trivial and seek-free.

use rodio::source::SeekError;
use rodio::source::Source;
use std::fs::File;
use std::io::BufReader;
use std::num::NonZero;
use std::path::Path;
use std::time::Duration;

const OPUS_SAMPLE_RATE: u32 = 48000;
const MAX_FRAME_SAMPLES: usize = 5760; // 120ms at 48kHz, per channel

/// A decoded Opus track ready to feed rodio as a `Source`.
pub struct OpusSource {
    samples: Vec<f32>,
    pos: usize,
    channels: u16,
}

impl OpusSource {
    /// Decode the Opus track at `path` into interleaved f32 samples.
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("open: {e}"))?;
        let mut reader = ogg::PacketReader::new(BufReader::new(file));

        let mut channels: u16 = 2;
        let mut pre_skip: u16 = 0;
        let mut opus_stream: Option<u32> = None;
        let mut decoder: Option<opus::Decoder> = None;
        let mut samples: Vec<f32> = Vec::new();
        let mut seen_head = false;

        while let Ok(Some(packet)) = reader.read_packet() {
            let data = &packet.data;
            // The first packet of an Opus-in-OGG stream is the OpusHead
            // identification header.  Match on the magic bytes, not on
            // first_in_stream(), so that Vorbis-in-OGG files (whose first
            // packet is the Vorbis id header "\x01vorbis") are not
            // misidentified as Opus and fed to the Opus decoder.
            if !seen_head && data.starts_with(b"OpusHead") {
                channels = data
                    .get(9)
                    .copied()
                    .map(|c| c.clamp(1, 2) as u16)
                    .unwrap_or(2);
                // Pre-skip is bytes 10-11 (u16 LE) in the OpusHead.
                pre_skip = data
                    .get(10..12)
                    .and_then(|b| u16::from_le_bytes([b[0], b[1]]).into())
                    .unwrap_or(0);
                opus_stream = Some(packet.stream_serial());
                let ch = if channels == 1 {
                    opus::Channels::Mono
                } else {
                    opus::Channels::Stereo
                };
                decoder = Some(
                    opus::Decoder::new(OPUS_SAMPLE_RATE, ch)
                        .map_err(|e| format!("opus decoder: {e}"))?,
                );
                seen_head = true;
                continue;
            }

            let Some(stream) = opus_stream else {
                continue;
            };
            if packet.stream_serial() != stream {
                continue;
            }

            // Skip the comment header and any non-audio packets.
            if data.starts_with(b"OpusTags") || !seen_head {
                continue;
            }

            let Some(dec) = decoder.as_mut() else {
                continue;
            };
            let mut pcm = vec![0i16; MAX_FRAME_SAMPLES * channels as usize];
            let per_channel = match dec.decode(data, &mut pcm, false) {
                Ok(n) => n,
                Err(_) => continue, // resync past any broken packet
            };
            let count = per_channel * channels as usize;
            for s in pcm.iter().take(count) {
                samples.push(*s as f32 / 32768.0);
            }
        }

        if samples.is_empty() {
            return Err("no opus audio decoded".to_string());
        }

        // Discard the pre-skip samples that the encoder added for decoder
        // priming (RFC 7845 section 4.2).
        let skip = (pre_skip as usize) * channels as usize;
        if skip > 0 && skip < samples.len() {
            samples.drain(..skip);
        }

        Ok(Self {
            samples,
            pos: 0,
            channels,
        })
    }
}

impl Iterator for OpusSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let s = self.samples.get(self.pos).copied();
        if s.is_some() {
            self.pos += 1;
        }
        s
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.samples.len() - self.pos;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for OpusSource {}

impl Source for OpusSource {
    fn current_span_len(&self) -> Option<usize> {
        // Return None so rodio's UniformSourceIterator treats the entire track
        // as one span and never recreates the SampleRateConverter mid-stream.
        // Returning a concrete value causes the converter to be rebuilt every
        // 32768 samples, which loses interpolation state and produces clicks
        // whenever the output device's sample rate differs from 48 kHz.
        None
    }

    fn channels(&self) -> NonZero<u16> {
        NonZero::new(self.channels).expect("opus channels >= 1")
    }

    fn sample_rate(&self) -> NonZero<u32> {
        NonZero::new(OPUS_SAMPLE_RATE).expect("sample rate > 0")
    }

    fn total_duration(&self) -> Option<Duration> {
        let total_samples = self.samples.len() / self.channels.max(1) as usize;
        Some(Duration::from_secs_f64(
            total_samples as f64 / OPUS_SAMPLE_RATE as f64,
        ))
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        let sample_offset =
            (pos.as_secs_f64() * OPUS_SAMPLE_RATE as f64 * self.channels as f64) as usize;
        self.pos = sample_offset.min(self.samples.len());
        Ok(())
    }
}
