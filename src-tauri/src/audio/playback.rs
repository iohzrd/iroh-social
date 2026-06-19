use cpal::StreamConfig;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer as _, Producer as _, Split as _};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use super::capture::device_name;
use super::codec::{CHANNELS, FRAME_SIZE, SAMPLE_RATE};

/// Ring buffer capacity in frames. Callers cannot exceed this depth.
pub const PLAYBACK_CAPACITY_FRAMES: usize = 10;

/// Producer handle returned by [`AudioPlayback::start`].
///
/// Push decoded PCM samples with [`PlaybackProducer::push`]. If the buffer is
/// full the excess is silently dropped (listener is behind; dropping is
/// preferable to unbounded lag).
///
/// Read underrun feedback with [`PlaybackProducer::drain_underruns`] to drive
/// adaptive pre-fill depth in the caller.
pub struct PlaybackProducer {
    inner: Arc<Mutex<ringbuf::HeapProd<f32>>>,
    underruns: Arc<AtomicUsize>,
}

impl PlaybackProducer {
    /// Push samples into the jitter buffer. Returns the number actually pushed;
    /// any remainder was dropped because the buffer was full.
    pub fn push(&mut self, samples: &[f32]) -> usize {
        self.inner.lock().unwrap().push_slice(samples)
    }

    /// Atomically read and reset the accumulated underrun count since the last
    /// call. Safe to call from any thread.
    pub fn drain_underruns(&self) -> usize {
        self.underruns.swap(0, Ordering::Relaxed)
    }
}

/// Audio playback handle. Owns the cpal output stream and the consumer half of
/// the ring buffer. Drop to stop playback. Supports mid-call device switching.
pub struct AudioPlayback {
    stream: cpal::Stream,
    /// Shared producer handle -- allows swapping the ring buffer on device switch.
    producer: Arc<Mutex<ringbuf::HeapProd<f32>>>,
    underruns: Arc<AtomicUsize>,
}

fn stream_config() -> StreamConfig {
    let channel_count = match CHANNELS {
        opus::Channels::Mono => 1,
        opus::Channels::Stereo => 2,
    };
    StreamConfig {
        channels: channel_count,
        sample_rate: SAMPLE_RATE,
        buffer_size: cpal::BufferSize::Default,
    }
}

/// Find an output device by name, falling back to the default.
fn find_output_device(name: Option<&str>) -> Result<cpal::Device, cpal::Error> {
    let host = cpal::default_host();
    if let Some(name) = name {
        if let Some(device) = host
            .output_devices()
            .ok()
            .and_then(|mut devices| devices.find(|d| device_name(d) == name))
        {
            return Ok(device);
        }
        log::warn!("[audio-playback] device '{name}' not found, using default");
    }
    host.default_output_device()
        .ok_or_else(|| cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable))
}

/// Build a new output stream + ring buffer pair.
fn build_playback_stream(
    device_name_pref: Option<&str>,
    underruns: &Arc<AtomicUsize>,
) -> Result<(ringbuf::HeapProd<f32>, cpal::Stream), cpal::Error> {
    let device = find_output_device(device_name_pref)?;
    let config = stream_config();

    log::info!("[audio-playback] device: {}", device_name(&device));

    let ring = ringbuf::HeapRb::<f32>::new(PLAYBACK_CAPACITY_FRAMES * FRAME_SIZE);
    let (prod, mut cons) = ring.split();

    let underruns_cb = underruns.clone();

    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let available = cons.pop_slice(data);
            if available < data.len() {
                underruns_cb.fetch_add(1, Ordering::Relaxed);
                data[available..].fill(0.0);
            }
        },
        |err| log::error!("[audio-playback] stream error: {err}"),
        None,
    )?;
    stream.play()?;

    Ok((prod, stream))
}

impl AudioPlayback {
    /// Start playing audio on the specified device (or default if None).
    pub fn start(device_name_pref: Option<&str>) -> Result<(PlaybackProducer, Self), cpal::Error> {
        let underruns = Arc::new(AtomicUsize::new(0));
        let (prod, stream) = build_playback_stream(device_name_pref, &underruns)?;

        let producer = Arc::new(Mutex::new(prod));

        let playback_producer = PlaybackProducer {
            inner: producer.clone(),
            underruns: underruns.clone(),
        };
        Ok((
            playback_producer,
            Self {
                stream,
                producer,
                underruns,
            },
        ))
    }

    /// Switch to a different output device mid-stream. Builds a new stream
    /// and ring buffer, swaps the producer so the decode thread feeds the new
    /// buffer, then drops the old stream.
    pub fn switch_device(&mut self, device_name_pref: Option<&str>) -> Result<(), cpal::Error> {
        let (new_prod, new_stream) = build_playback_stream(device_name_pref, &self.underruns)?;
        // Swap the producer under the lock so the decode thread starts
        // pushing into the new ring buffer immediately.
        *self.producer.lock().unwrap() = new_prod;
        self.stream = new_stream; // old stream dropped here
        Ok(())
    }
}

unsafe impl Send for AudioPlayback {}

/// Shared handle for mid-call device switching from the command layer.
pub type SharedPlayback = Arc<std::sync::Mutex<Option<AudioPlayback>>>;
