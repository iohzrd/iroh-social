use cpal::StreamConfig;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use tokio::sync::mpsc;

use super::codec::{CHANNELS, SAMPLE_RATE};

/// Audio capture handle. Reads from an input device and sends
/// f32 sample chunks to the provided channel. Supports mid-call
/// device switching via [`AudioCapture::switch_device`].
pub struct AudioCapture {
    stream: cpal::Stream,
    /// Cloneable sender -- shared with the cpal callback. When we rebuild
    /// the stream on a new device we clone this into the new callback.
    tx: mpsc::Sender<Vec<f32>>,
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

pub fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Find an input device by name, falling back to the default.
fn find_input_device(name: Option<&str>) -> Result<cpal::Device, cpal::Error> {
    let host = cpal::default_host();
    if let Some(name) = name {
        if let Some(device) = host
            .input_devices()
            .ok()
            .and_then(|mut devices| devices.find(|d| device_name(d) == name))
        {
            return Ok(device);
        }
        log::warn!("[audio-capture] device '{name}' not found, using default");
    }
    host.default_input_device()
        .ok_or_else(|| cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable))
}

fn build_capture_stream(
    tx: mpsc::Sender<Vec<f32>>,
    device_name_pref: Option<&str>,
) -> Result<cpal::Stream, cpal::Error> {
    let device = find_input_device(device_name_pref)?;
    let config = stream_config();

    log::info!("[audio-capture] device: {}", device_name(&device));

    let stream = device.build_input_stream(
        config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            let _ = tx.try_send(data.to_vec());
        },
        |err| log::error!("[audio-capture] stream error: {err}"),
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

impl AudioCapture {
    /// Start capturing audio from the specified device (or default if None).
    /// Sends chunks of f32 samples (may not be frame-aligned; the encoder handles buffering).
    pub fn start(
        tx: mpsc::Sender<Vec<f32>>,
        device_name_pref: Option<&str>,
    ) -> Result<Self, cpal::Error> {
        let stream = build_capture_stream(tx.clone(), device_name_pref)?;
        Ok(Self { stream, tx })
    }

    /// Switch to a different input device mid-stream. Builds the new stream
    /// first, then drops the old one. The mpsc channel stays the same so the
    /// encoder sees at most a brief pause in samples.
    pub fn switch_device(&mut self, device_name_pref: Option<&str>) -> Result<(), cpal::Error> {
        let new_stream = build_capture_stream(self.tx.clone(), device_name_pref)?;
        self.stream = new_stream; // old stream dropped here
        Ok(())
    }
}

// SAFETY: cpal::Stream uses platform audio APIs that are thread-safe on all
// supported backends. The only non-Send field is cpal::Stream itself (it holds
// a raw platform handle). On AAudio/ALSA/WASAPI/CoreAudio the handle is safe
// to move between threads -- cpal just doesn't blanket-impl Send because some
// niche backends (e.g. ASIO callback pointers) may not be. For our supported
// targets (Linux ALSA, Android AAudio, macOS CoreAudio, Windows WASAPI) this
// is fine.
unsafe impl Send for AudioCapture {}

/// Shared handle for mid-call device switching from the command layer.
pub type SharedCapture = Arc<std::sync::Mutex<Option<AudioCapture>>>;
