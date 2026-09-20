// ./tui/src/opus.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! .opus decoder: demuxes OGG with the `ogg` crate and decodes Opus packets
//! with libopus, exposing a `rodio::Source` of interleaved f32 samples.
//!
//! Symphonia (what rodio uses) has no Opus codec, so this fills that gap for
//! the desktop TUI. Audio is decoded packet-by-packet on demand, so memory
//! stays flat no matter how long the track is.

use currant_core::net::stream::Seekable;
use rodio::source::SeekError;
use rodio::source::Source;
use std::collections::VecDeque;
use std::io::{BufReader, SeekFrom};
use std::num::NonZero;
use std::time::Duration;

const OPUS_SAMPLE_RATE: u32 = 48000;
const MAX_FRAME_SAMPLES: usize = 5760; // 120ms at 48kHz, per channel

/// A decoded Opus track ready to feed rodio as a `Source`.
pub struct OpusSource {
    reader: Option<ogg::PacketReader<BufReader<Box<dyn Seekable>>>>,
    decoder: opus::Decoder,
    channels: u16,
    opus_stream: u32,
    buffer: VecDeque<f32>,
    /// Number of *frames* (per-channel samples) decoded and played/buffered.
    pos_samples: usize,
    /// Encoder pre-skip (OpusHead, RFC 7845 section 4.2). A recreated
    /// decoder re-emits these priming samples, so backward seeks must
    /// discard them before counting toward the target.
    pre_skip: usize,
}

impl OpusSource {
    /// Decode an Opus track from any seekable source (local file or remote
    /// HTTP stream) into interleaved f32 samples.
    pub fn open(reader: Box<dyn Seekable>) -> Result<Self, String> {
        let mut reader = ogg::PacketReader::new(BufReader::new(reader));

        let mut channels: u16 = 2;
        let mut pre_skip: u16 = 0;
        let mut opus_stream: Option<u32> = None;
        let mut decoder: Option<opus::Decoder> = None;
        let mut seen_head = false;

        // Extract metadata from the headers
        while let Ok(Some(packet)) = reader.read_packet() {
            let data = &packet.data;
            if !seen_head && data.starts_with(b"OpusHead") {
                channels = data
                    .get(9)
                    .copied()
                    .map(|c| c.clamp(1, 2) as u16)
                    .unwrap_or(2);
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

            if data.starts_with(b"OpusTags") {
                break; // Header parse complete
            }
        }

        let decoder = decoder.ok_or_else(|| "no opus stream found".to_string())?;
        let opus_stream = opus_stream.unwrap();

        let mut source = Self {
            reader: Some(reader),
            decoder,
            channels,
            opus_stream,
            buffer: VecDeque::new(),
            pos_samples: 0,
            pre_skip: pre_skip as usize,
        };

        // Pre-roll: decode and discard the encoder pre-skip
        let mut skip_remaining = pre_skip as usize;
        let mut pcm = vec![0i16; MAX_FRAME_SAMPLES * channels as usize];
        while skip_remaining > 0 {
            if let Some(r) = source.reader.as_mut() {
                match r.read_packet() {
                    Ok(Some(packet)) => {
                        if packet.stream_serial() != source.opus_stream {
                            continue;
                        }
                        if let Ok(per_channel) =
                            source.decoder.decode(&packet.data, &mut pcm, false)
                        {
                            if per_channel > skip_remaining {
                                let keep = per_channel - skip_remaining;
                                let skip_total = skip_remaining * channels as usize;
                                let total = per_channel * channels as usize;
                                for s in pcm.iter().take(total).skip(skip_total) {
                                    source.buffer.push_back(*s as f32 / 32768.0);
                                }
                                source.pos_samples += keep;
                                skip_remaining = 0;
                            } else {
                                skip_remaining = skip_remaining.saturating_sub(per_channel);
                            }
                        }
                    }
                    _ => break,
                }
            } else {
                break;
            }
        }

        Ok(source)
    }

    fn fill_buffer(&mut self) -> bool {
        if let Some(r) = self.reader.as_mut() {
            let mut pcm = vec![0i16; MAX_FRAME_SAMPLES * self.channels as usize];
            while let Ok(Some(packet)) = r.read_packet() {
                if packet.stream_serial() != self.opus_stream {
                    continue;
                }
                if let Ok(per_channel) = self.decoder.decode(&packet.data, &mut pcm, false) {
                    let total = per_channel * self.channels as usize;
                    for s in pcm.iter().take(total) {
                        self.buffer.push_back(*s as f32 / 32768.0);
                    }
                    self.pos_samples += per_channel;
                    return true;
                }
            }
        }
        false
    }
}

impl Iterator for OpusSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.buffer.is_empty() && !self.fill_buffer() {
            return None;
        }
        self.buffer.pop_front()
    }
}

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
        None // We stream on the fly, so we don't know the exact length a priori
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        let target_samples = (pos.as_secs_f64() * OPUS_SAMPLE_RATE as f64).round() as usize;

        // Audio frames decoded so far, plus any encoder pre-skip still to
        // discard (a fresh decoder re-emits it, RFC 7845 section 4.2).
        let mut audio = self.pos_samples;
        let mut priming = 0;

        // Seeking backwards: reset the HTTP stream/file cursor to 0, recreate the
        // packet reader and decoder.
        if target_samples < self.pos_samples
            && let Some(r) = self.reader.take()
        {
            let mut inner = r.into_inner().into_inner();
            if inner.seek(SeekFrom::Start(0)).is_err() {
                return Err(SeekError::NotSupported {
                    underlying_source: "seek failed",
                });
            }
            let mut new_reader = ogg::PacketReader::new(BufReader::new(inner));

            // Hunt for the start of the stream
            let mut seen_head = false;
            while let Ok(Some(packet)) = new_reader.read_packet() {
                if packet.data.starts_with(b"OpusHead") {
                    seen_head = true;
                } else if seen_head && packet.data.starts_with(b"OpusTags") {
                    break;
                }
            }
            self.reader = Some(new_reader);

            let ch = if self.channels == 1 {
                opus::Channels::Mono
            } else {
                opus::Channels::Stereo
            };
            self.decoder =
                opus::Decoder::new(OPUS_SAMPLE_RATE, ch).map_err(|_| SeekError::NotSupported {
                    underlying_source: "decoder init failed",
                })?;
            audio = 0;
            priming = self.pre_skip;
            self.buffer.clear();
        }

        // Fast-forward to the target. Decoding is so fast that we just read, decode,
        // and drop the samples until we reach the timestamp. This leverages HTTP ranges
        // safely without breaking the Opus state machine.
        let mut pcm = vec![0i16; MAX_FRAME_SAMPLES * self.channels as usize];
        while audio < target_samples {
            if let Some(r) = self.reader.as_mut() {
                match r.read_packet() {
                    Ok(Some(packet)) => {
                        if packet.stream_serial() != self.opus_stream {
                            continue;
                        }
                        if let Ok(per_channel) = self.decoder.decode(&packet.data, &mut pcm, false)
                        {
                            // Drop any remaining pre-skip priming samples.
                            let (skip, keep) = if priming > 0 {
                                let skip = priming.min(per_channel);
                                priming -= skip;
                                (skip, per_channel - skip)
                            } else {
                                (0, per_channel)
                            };
                            audio += keep;
                            if keep > 0 && audio > target_samples {
                                // We've overshot the target frame inside this packet.
                                // Buffer the remainder so playback picks up seamlessly.
                                let start = target_samples - (audio - keep);
                                let start_total = (skip + start) * self.channels as usize;
                                let keep_total = (keep - start) * self.channels as usize;
                                self.buffer.clear();
                                for s in pcm.iter().skip(start_total).take(keep_total) {
                                    self.buffer.push_back(*s as f32 / 32768.0);
                                }
                            }
                            // Frames before the target are dropped, not buffered.
                        }
                    }
                    _ => break,
                }
            } else {
                break;
            }
        }
        self.pos_samples = audio;

        Ok(())
    }
}
