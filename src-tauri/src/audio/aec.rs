use aec3::api::control::EchoControl;
use aec3::audio_processing::aec3::echo_canceller3::EchoCanceller3;
use aec3::audio_processing::audio_buffer::AudioBuffer;
use aec3::audio_processing::stream_config::StreamConfig;

/// Number of samples per 10 ms frame at 48 kHz (mono).
const FRAME_SAMPLES: usize = 480;

/// Acoustic echo canceller wrapping `aec3::EchoCanceller3`.
///
/// Operates at 48 kHz mono. The inner `AudioBuffer` requires exactly
/// 480-sample frames, so incoming arbitrary-size chunks are accumulated
/// in `render_buf`/`capture_buf` and drained in frame-sized increments.
///
/// Usage:
/// 1. Before processing each mic chunk, call `render()` with any pending
///    playback samples to keep the far-end reference up to date.
/// 2. Call `process_capture()` with the raw mic samples; returns cleaned samples.
pub struct EchoCanceller {
    inner: EchoCanceller3,
    /// Accumulates far-end samples until a full frame is available.
    render_buf: Vec<f32>,
    /// Accumulates near-end samples until a full frame is available.
    capture_buf: Vec<f32>,
    /// Reusable 480-sample buffer for feeding render data to AEC3.
    render_audio: AudioBuffer,
    /// Reusable 480-sample buffer for feeding capture data to AEC3.
    capture_audio: AudioBuffer,
    stream_config: StreamConfig,
}

impl EchoCanceller {
    pub fn new() -> Self {
        let config = EchoCanceller3::create_default_config(1, 1);
        let inner = EchoCanceller3::new(config, 48_000, 1, 1);
        let stream_config = StreamConfig::new(48_000, 1, false);
        let render_audio = AudioBuffer::new(FRAME_SAMPLES, 1, FRAME_SAMPLES, 1, FRAME_SAMPLES);
        let capture_audio = AudioBuffer::new(FRAME_SAMPLES, 1, FRAME_SAMPLES, 1, FRAME_SAMPLES);
        Self {
            inner,
            render_buf: Vec::new(),
            capture_buf: Vec::new(),
            render_audio,
            capture_audio,
            stream_config,
        }
    }

    /// Feed playback (far-end reference) samples into the AEC render path.
    /// Call this before `process_capture` for the same time slice.
    pub fn render(&mut self, samples: &[f32]) {
        self.render_buf.extend_from_slice(samples);
        while self.render_buf.len() >= FRAME_SAMPLES {
            let frame: Vec<f32> = self.render_buf.drain(..FRAME_SAMPLES).collect();
            let data = [frame.as_slice()];
            self.render_audio.copy_from(&data, &self.stream_config);
            self.inner.analyze_render(&mut self.render_audio);
        }
    }

    /// Process mic samples through AEC. Returns echo-cancelled samples.
    /// Drain all pending render samples first by calling `render()` before this.
    pub fn process_capture(&mut self, samples: &[f32]) -> Vec<f32> {
        self.capture_buf.extend_from_slice(samples);
        let mut out = Vec::with_capacity(samples.len());
        while self.capture_buf.len() >= FRAME_SAMPLES {
            let frame: Vec<f32> = self.capture_buf.drain(..FRAME_SAMPLES).collect();
            let data = [frame.as_slice()];
            self.capture_audio.copy_from(&data, &self.stream_config);
            self.inner.process_capture(&mut self.capture_audio, false);
            let mut cleaned = vec![0.0f32; FRAME_SAMPLES];
            let mut out_slice = [cleaned.as_mut_slice()];
            self.capture_audio
                .copy_to_stream(&self.stream_config, &mut out_slice);
            out.extend_from_slice(&cleaned);
        }
        out
    }
}
