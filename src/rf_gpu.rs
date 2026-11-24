//! Feature-gated GPU spectrum path for Radioman.
//!
//! `rf-gpu` receives raw IQ over UDP and runs a windowed 1024-point FFT on
//! `wgpu` (Metal on macOS, Vulkan on Linux). It emits the same bounded
//! `SpectrumFrame` JSON that the CPU spectrum path emits, so waterfall and
//! detection consumers do not need to know which backend produced the FFT.
//!
//! This is deliberately a *front-end* work offload: the SDR remains owned by
//! the Pi and audio demodulation remains a CPU/real-time concern. The GPU
//! path is opt-in with `--features rf-gpu` and does not affect the default
//! Radioman binary.
//!
//! Wire input is raw interleaved IQ datagrams in `cs8`, `cu8`, or `cs16le`.
//! The stream is treated as best-effort: late IQ is dropped rather than
//! allowed to grow an unbounded queue, because this path feeds visualization
//! and detection, not a sample-exact recording.

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{CONTRACT_VERSION, SampleFormat, SpectrumFrame};
use bytemuck::cast_slice;

const FFT_SIZE: usize = 1024;
const FFT_BINS: usize = 512;
const MAX_DATAGRAM: usize = 65_536;
const DEFAULT_FPS: u32 = 20;

pub const FFT_SHADER: &str = r#"
const PI: f32 = 3.141592653589793;
const N: u32 = 1024u;
const BINS: u32 = 512u;

@group(0) @binding(0) var<storage, read> input: array<vec2<f32>>;
@group(0) @binding(1) var<storage, read_write> output: array<f32>;

var<workgroup> data: array<vec2<f32>, 1024>;

fn cmul(a: vec2<f32>, b: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(a.x * b.x - a.y * b.y, a.x * b.y + a.y * b.x);
}

fn bit_reverse(value: u32) -> u32 {
    var x = value;
    var result = 0u;
    for (var i = 0u; i < 10u; i = i + 1u) {
        result = (result << 1u) | (x & 1u);
        x = x >> 1u;
    }
    return result;
}

@compute @workgroup_size(256)
fn main(@builtin(local_invocation_id) local: vec3<u32>) {
    let tid = local.x;

    for (var k = tid; k < N; k = k + 256u) {
        let window = 0.5 - 0.5 * cos(2.0 * PI * f32(k) / f32(N - 1u));
        data[bit_reverse(k)] = input[k] * window;
    }
    workgroupBarrier();

    var size = 2u;
    loop {
        if (size > N) { break; }
        let half = size / 2u;
        for (var b = tid; b < N / 2u; b = b + 256u) {
            let group = b / half;
            let j = b % half;
            let i = group * size + j;
            let angle = -2.0 * PI * f32(j) / f32(size);
            let twiddle = vec2<f32>(cos(angle), sin(angle));
            let even = data[i];
            let odd = cmul(data[i + half], twiddle);
            data[i] = even + odd;
            data[i + half] = even - odd;
        }
        workgroupBarrier();
        size = size * 2u;
    }

    // FFT-shifted display order: DC at bin 0, negative frequencies first.
    // Two FFT bins per output bin keeps the bounded 512-bin contract.
    for (var b = tid; b < BINS; b = b + 256u) {
        let index = (b * 2u + 512u) % 1024u;
        let power = dot(data[index], data[index]) / 262144.0;
        output[b] = clamp(10.0 * log(power + 1.0e-12) - 30.0, -130.0, -20.0);
    }
}
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfGpuConfig {
    pub bind: SocketAddr,
    pub destination: SocketAddr,
    pub center_frequency_hz: u64,
    pub sample_rate_hz: u32,
    pub format: SampleFormat,
    pub fps: u32,
}

impl RfGpuConfig {
    pub fn parse(
        bind: &str,
        destination: &str,
        center_frequency_hz: u64,
        sample_rate_hz: u32,
        format: &str,
        fps: Option<u32>,
    ) -> Result<Self, String> {
        let bind = bind
            .parse()
            .map_err(|error| format!("invalid IQ bind address: {error}"))?;
        let destination = destination
            .parse()
            .map_err(|error| format!("invalid spectrum destination: {error}"))?;
        if center_frequency_hz == 0 || sample_rate_hz == 0 {
            return Err("rf-gpu center and sample rate must be non-zero".into());
        }
        let format = match format.to_ascii_lowercase().as_str() {
            "cs8" => SampleFormat::Cs8,
            "cu8" => SampleFormat::Cu8,
            "cs16le" | "cs16" => SampleFormat::Cs16Le,
            other => return Err(format!("unsupported rf-gpu IQ format: {other}")),
        };
        let fps = fps.unwrap_or(DEFAULT_FPS).clamp(1, 120);
        Ok(Self {
            bind,
            destination,
            center_frequency_hz,
            sample_rate_hz,
            format,
            fps,
        })
    }

    fn window_bytes(&self) -> usize {
        FFT_SIZE * self.format.bytes_per_complex_sample() as usize
    }
}

struct GpuFft {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_group: wgpu::BindGroup,
    input: wgpu::Buffer,
    output: wgpu::Buffer,
    staging: wgpu::Buffer,
}

impl GpuFft {
    fn new() -> Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok_or("rf-gpu: no wgpu adapter (Metal/Vulkan) is available")?;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("radioman-rf-gpu"),
                ..Default::default()
            },
            None,
        ))
        .map_err(|error| format!("rf-gpu request device: {error}"))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("radioman-fft"),
            source: wgpu::ShaderSource::Wgsl(FFT_SHADER.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("radioman-fft"),
            layout: None,
            module: &shader,
            entry_point: "main",
            compilation_options: Default::default(),
            cache: None,
        });
        let input_size = (FFT_SIZE * 2 * std::mem::size_of::<f32>()) as u64;
        let output_size = (FFT_BINS * std::mem::size_of::<f32>()) as u64;
        let input = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("radioman-iq"),
            size: input_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let output = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("radioman-spectrum"),
            size: output_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("radioman-spectrum-readback"),
            size: output_size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("radioman-fft"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: input.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: output.as_entire_binding(),
                },
            ],
        });
        Ok(Self {
            device,
            queue,
            pipeline,
            bind_group,
            input,
            output,
            staging,
        })
    }

    fn spectrum(&self, bytes: &[u8], format: SampleFormat) -> Result<Vec<f32>, String> {
        let samples = decode_iq(bytes, format)?;
        let packed: Vec<[f32; 2]> = samples;
        self.queue.write_buffer(&self.input, 0, cast_slice(&packed));

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("radioman-fft"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("radioman-fft"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.dispatch_workgroups(1, 1, 1);
        }
        encoder.copy_buffer_to_buffer(
            &self.output,
            0,
            &self.staging,
            0,
            (FFT_BINS * std::mem::size_of::<f32>()) as u64,
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = self.staging.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device.poll(wgpu::Maintain::Wait);
        receiver
            .recv()
            .map_err(|_| "rf-gpu readback channel closed".to_string())?
            .map_err(|error| format!("rf-gpu readback: {error}"))?;
        let data = slice.get_mapped_range();
        let bins = cast_slice::<u8, f32>(&data)
            .iter()
            .copied()
            .take(FFT_BINS)
            .collect::<Vec<_>>();
        drop(data);
        self.staging.unmap();
        Ok(bins)
    }
}

fn decode_iq(bytes: &[u8], format: SampleFormat) -> Result<Vec<[f32; 2]>, String> {
    let needed = FFT_SIZE * format.bytes_per_complex_sample() as usize;
    if bytes.len() < needed {
        return Err("rf-gpu received a partial FFT window".into());
    }
    let mut samples = Vec::with_capacity(FFT_SIZE);
    match format {
        SampleFormat::Cs8 => {
            for pair in bytes[..needed].chunks_exact(2) {
                samples.push([pair[0] as i8 as f32 / 128.0, pair[1] as i8 as f32 / 128.0]);
            }
        }
        SampleFormat::Cu8 => {
            for pair in bytes[..needed].chunks_exact(2) {
                samples.push([
                    (pair[0] as f32 - 127.5) / 128.0,
                    (pair[1] as f32 - 127.5) / 128.0,
                ]);
            }
        }
        SampleFormat::Cs16Le => {
            for pair in bytes[..needed].chunks_exact(4) {
                samples.push([
                    i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0,
                    i16::from_le_bytes([pair[2], pair[3]]) as f32 / 32768.0,
                ]);
            }
        }
        SampleFormat::Cf32Le => {
            for pair in bytes[..needed].chunks_exact(8) {
                samples.push([
                    f32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]]),
                    f32::from_le_bytes([pair[4], pair[5], pair[6], pair[7]]),
                ]);
            }
        }
    }
    Ok(samples)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Run the feature-gated GPU spectrum plugin.
pub fn run(config: RfGpuConfig) -> Result<(), String> {
    let gpu = GpuFft::new()?;
    let socket = UdpSocket::bind(config.bind)
        .map_err(|error| format!("rf-gpu bind {}: {error}", config.bind))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|error| format!("rf-gpu read timeout: {error}"))?;
    let window_bytes = config.window_bytes();
    let interval = Duration::from_secs_f64(1.0 / f64::from(config.fps));
    let mut pending = VecDeque::<u8>::with_capacity(window_bytes * 4);
    let mut datagram = [0_u8; MAX_DATAGRAM];
    let mut sequence = 0_u64;
    let mut next_frame = Instant::now();

    eprintln!(
        "radioman: rf-gpu bind={} destination={} center={}Hz rate={}Hz format={:?} fps={}",
        config.bind,
        config.destination,
        config.center_frequency_hz,
        config.sample_rate_hz,
        config.format,
        config.fps
    );

    loop {
        match socket.recv(&mut datagram) {
            Ok(size) => {
                pending.extend(&datagram[..size]);
                // Spectrum is a live projection. If we fell behind, keep only
                // the newest window instead of replaying stale spectrum.
                if pending.len() > window_bytes * 8 {
                    let discard = pending.len() - window_bytes * 2;
                    pending.drain(..discard);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => return Err(format!("rf-gpu receive IQ: {error}")),
        }

        while pending.len() >= window_bytes {
            let window: Vec<u8> = pending.drain(..window_bytes).collect();
            if Instant::now() < next_frame {
                continue;
            }
            let bins = gpu.spectrum(&window, config.format.clone())?;
            sequence = sequence.saturating_add(1);
            let frame = SpectrumFrame {
                schema_version: CONTRACT_VERSION,
                session_id: "rf-gpu".into(),
                generation: 1,
                sequence,
                observed_at_ms: now_ms(),
                center_frequency_hz: config.center_frequency_hz,
                span_hz: config.sample_rate_hz,
                floor_dbfs: -130.0,
                ceiling_dbfs: -20.0,
                bins_dbfs: bins,
                tuning: None,
            };
            frame.validate()?;
            socket
                .send_to(
                    &serde_json::to_vec(&frame)
                        .map_err(|error| format!("rf-gpu encode spectrum: {error}"))?,
                    config.destination,
                )
                .map_err(|error| format!("rf-gpu send spectrum: {error}"))?;
            next_frame = Instant::now() + interval;
        }
    }
}

/// Marker for a future GPU waterfall/ML stage. Kept in the plugin so callers
/// can wire a consumer without changing the IQ/FFT boundary.
pub trait SpectrumConsumer {
    fn observe(&mut self, frame: &SpectrumFrame) -> Result<(), String>;
}

#[cfg(test)]
mod tests {
    use super::{FFT_SIZE, RfGpuConfig, decode_iq};
    use crate::SampleFormat;

    #[test]
    fn parses_bounded_gpu_config() {
        let config = RfGpuConfig::parse(
            "0.0.0.0:51001",
            "127.0.0.1:50070",
            94_900_000,
            2_000_000,
            "cs8",
            Some(30),
        )
        .unwrap();
        assert_eq!(config.fps, 30);
        assert_eq!(config.window_bytes(), FFT_SIZE * 2);
        assert!(RfGpuConfig::parse("bad", "127.0.0.1:1", 1, 1, "cs8", None).is_err());
    }

    #[test]
    fn decodes_signed_and_unsigned_iq_without_unbounded_input() {
        let cs8 = [0u8, 0u8, 128u8, 127u8];
        let samples = decode_iq(&cs8.repeat(FFT_SIZE / 2), SampleFormat::Cs8).unwrap();
        assert_eq!(samples.len(), FFT_SIZE);
        assert!(samples.iter().all(|sample| sample[0].is_finite()));
        assert!(decode_iq(&cs8, SampleFormat::Cs8).is_err());
    }
}
