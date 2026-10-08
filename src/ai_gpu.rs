use crate::ai::{self, Detection, Settings, Worker};
use eframe::{egui, wgpu};
use std::sync::{
    mpsc::{self, Receiver},
    Arc,
};

const PARAM_FLOATS: usize = 20 + 16 * 8;
type Parameters = [f32; PARAM_FLOATS];

pub struct Processor {
    pub settings: Settings,
    pub status: String,
    state: Option<State>,
    epoch: u64,
    pub pts: f64,
}
impl Default for Processor {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            status: String::new(),
            state: None,
            epoch: 0,
            pts: 0.0,
        }
    }
}
struct Timed {
    pts: f64,
    detection: Box<Detection>,
}
struct Capture {
    epoch: u64,
    pts: f64,
    rx: Receiver<Result<Vec<u8>, String>>,
}
struct State {
    flags: u32,
    landmarks: Option<Worker>,
    segment: Option<Worker>,
    points: Option<Timed>,
    skin: Option<Timed>,
    capture: Option<Capture>,
    output: wgpu::Texture,
    thumbnail: wgpu::Texture,
    buffer: wgpu::Buffer,
    mask: wgpu::Texture,
    parameters: wgpu::Buffer,
    sampler: wgpu::Sampler,
    layout: wgpu::BindGroupLayout,
    capture_pipeline: wgpu::RenderPipeline,
    beauty_pipeline: wgpu::RenderPipeline,
    params: Parameters,
    elapsed: [f32; 2],
    error: String,
}
fn texture(
    device: &wgpu::Device,
    size: [u32; 2],
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("AI video resource"),
        size: wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}
impl State {
    fn new(device: &wgpu::Device, size: [u32; 2], flags: u32, ctx: &egui::Context) -> Self {
        let ratio = (640.0 / size[0].max(size[1]) as f32).min(1.0);
        let small = [
            (size[0] as f32 * ratio).round().max(1.0) as u32,
            (size[1] as f32 * ratio).round().max(1.0) as u32,
        ];
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC;
        let output = texture(device, size, wgpu::TextureFormat::Rgba8UnormSrgb, usage);
        let thumbnail = texture(device, small, wgpu::TextureFormat::Rgba8UnormSrgb, usage);
        let mask = texture(
            device,
            [256, 256],
            wgpu::TextureFormat::Rg8Unorm,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        );
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("AI thumbnail readback"),
            size: (small[0] * 4).div_ceil(256) as u64 * 256 * small[1] as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let parameters = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("AI beauty controls"),
            size: (PARAM_FLOATS * 4) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let tex = wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: tex,
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: tex,
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("AI warp and skin beauty"),
            source: wgpu::ShaderSource::Wgsl(include_str!("ai.wgsl").into()),
        });
        let pipeline = |entry| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8UnormSrgb,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview: None,
                cache: None,
            })
        };
        Self {
            flags,
            landmarks: (flags & 3 != 0).then(|| Worker::new(flags & 3, ctx.clone())),
            segment: (flags & 4 != 0).then(|| Worker::new(4, ctx.clone())),
            points: None,
            skin: None,
            capture: None,
            capture_pipeline: pipeline("capture"),
            beauty_pipeline: pipeline("beautify"),
            output,
            thumbnail,
            buffer,
            mask,
            parameters,
            sampler,
            layout,
            params: [0.0; PARAM_FLOATS],
            elapsed: [0.0; 2],
            error: String::new(),
        }
    }
    fn bind(&self, device: &wgpu::Device, source: &wgpu::Texture) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &source.create_view(&Default::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.parameters.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(
                        &self.mask.create_view(&Default::default()),
                    ),
                },
            ],
        })
    }
    fn draw(&self, encoder: &mut wgpu::CommandEncoder, bind: &wgpu::BindGroup, capture: bool) {
        let view = if capture {
            &self.thumbnail
        } else {
            &self.output
        }
        .create_view(&Default::default());
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(if capture {
            &self.capture_pipeline
        } else {
            &self.beauty_pipeline
        });
        pass.set_bind_group(0, bind, &[]);
        pass.draw(0..3, 0..1);
    }
}
impl Processor {
    pub fn clear(&mut self) {
        self.state = None;
        self.status.clear();
        self.epoch = self.epoch.wrapping_add(1);
    }
    pub fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if let Some(s) = &mut self.state {
            s.points = None;
            s.skin = None;
            for worker in [&mut s.landmarks, &mut s.segment].into_iter().flatten() {
                worker.reset();
            }
        }
    }
    pub fn poll(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> bool {
        let Some(s) = &mut self.state else {
            return false;
        };
        device.poll(wgpu::Maintain::Poll);
        let mut changed = false;
        for (index, worker) in [&mut s.landmarks, &mut s.segment].into_iter().enumerate() {
            if let Some(out) = worker.as_mut().and_then(Worker::poll) {
                if out.epoch != self.epoch {
                    continue;
                }
                s.elapsed[index] = out.elapsed_ms;
                match out.data {
                    Ok(detection) => {
                        if index == 0 {
                            s.points = Some(Timed {
                                pts: out.pts,
                                detection,
                            });
                        } else {
                            queue.write_texture(
                                s.mask.as_image_copy(),
                                &detection.mask,
                                wgpu::TexelCopyBufferLayout {
                                    offset: 0,
                                    bytes_per_row: Some(512),
                                    rows_per_image: None,
                                },
                                s.mask.size(),
                            );
                            s.skin = Some(Timed {
                                pts: out.pts,
                                detection,
                            });
                        }
                    }
                    Err(error) => {
                        s.error = error;
                        if index == 0 {
                            s.points = None;
                        } else {
                            s.skin = None;
                        }
                    }
                }
                changed = true;
            }
        }
        if let Some((epoch, pts, result)) = s
            .capture
            .as_ref()
            .and_then(|c| c.rx.try_recv().ok().map(|r| (c.epoch, c.pts, r)))
        {
            s.capture = None;
            if epoch == self.epoch {
                match result {
                    Ok(rgba) => {
                        let rgba = Arc::new(rgba);
                        for worker in [&mut s.landmarks, &mut s.segment].into_iter().flatten() {
                            worker.submit(ai::Input {
                                rgba: rgba.clone(),
                                size: [s.thumbnail.width(), s.thumbnail.height()],
                                epoch,
                                pts,
                            });
                        }
                    }
                    Err(error) => s.error = error,
                }
            }
        }
        self.status = if !s.error.is_empty() {
            format!("AI 不可用：{}", s.error)
        } else if s.points.is_none() && s.skin.is_none() {
            "AI 正在识别…".into()
        } else {
            format!(
                "人脸 {} · 人体 {} · 定位 {:.0} ms / 皮肤 {:.0} ms",
                s.points
                    .as_ref()
                    .map_or(0, |p| u32::from(p.detection.face_count > 0)),
                s.points
                    .as_ref()
                    .map_or(0, |p| u32::from(p.detection.pose_count > 0)),
                s.elapsed[0],
                s.elapsed[1]
            )
        };
        changed
            || (s.capture.is_none()
                && s.error.is_empty()
                && [&s.landmarks, &s.segment]
                    .into_iter()
                    .flatten()
                    .any(|w| w.wants(self.pts)))
    }
    pub fn pending(&self) -> bool {
        self.state.as_ref().is_some_and(|s| {
            s.capture.is_some()
                || s.landmarks.as_ref().is_some_and(|w| w.busy)
                || s.segment.as_ref().is_some_and(|w| w.busy)
        })
    }
    pub fn apply(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        source: &wgpu::Texture,
        ctx: &egui::Context,
    ) -> wgpu::Texture {
        let flags = self.settings.flags();
        if flags == 0 {
            self.clear();
            return source.clone();
        }
        if self
            .state
            .as_ref()
            .is_none_or(|s| s.flags != flags || s.output.size() != source.size())
        {
            self.clear();
            self.state = Some(State::new(
                device,
                [source.width(), source.height()],
                flags,
                ctx,
            ));
        }
        self.poll(device, queue);
        let s = self.state.as_mut().unwrap();
        let params = parameters(
            self.settings,
            s.points.as_ref(),
            s.skin.as_ref(),
            self.pts,
            [source.width(), source.height()],
        );
        if params != s.params {
            queue.write_buffer(&s.parameters, 0, bytemuck::cast_slice(&params));
            s.params = params;
        }
        let bind = s.bind(device, source);
        let mut encoder = device.create_command_encoder(&Default::default());
        let capture = s.capture.is_none()
            && s.error.is_empty()
            && [&s.landmarks, &s.segment]
                .into_iter()
                .flatten()
                .any(|w| w.wants(self.pts));
        if capture {
            s.draw(&mut encoder, &bind, true);
            encoder.copy_texture_to_buffer(
                s.thumbnail.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &s.buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some((s.thumbnail.width() * 4).div_ceil(256) * 256),
                        rows_per_image: None,
                    },
                },
                s.thumbnail.size(),
            );
        }
        s.draw(&mut encoder, &bind, false);
        queue.submit([encoder.finish()]);
        if capture {
            let buffer = s.buffer.clone();
            let width = s.thumbnail.width();
            let (tx, rx) = mpsc::sync_channel(1);
            let ctx = ctx.clone();
            s.buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    let data = result.map_err(|e| e.to_string()).map(|_| {
                        let map = buffer.slice(..).get_mapped_range();
                        let bytes = map
                            .chunks_exact((width * 4).div_ceil(256) as usize * 256)
                            .flat_map(|row| row[..width as usize * 4].iter().copied())
                            .collect();
                        drop(map);
                        buffer.unmap();
                        bytes
                    });
                    let _ = tx.send(data);
                    ctx.request_repaint();
                });
            s.capture = Some(Capture {
                epoch: self.epoch,
                pts: self.pts,
                rx,
            });
        }
        s.output.clone()
    }
}

fn fresh(t: &Timed, pts: f64, max_age: f64) -> bool {
    pts >= t.pts && pts - t.pts <= max_age
}
fn parameters(
    settings: Settings,
    points: Option<&Timed>,
    skin: Option<&Timed>,
    pts: f64,
    size: [u32; 2],
) -> Parameters {
    let mut p = [0.0; PARAM_FLOATS];
    let mask = skin.is_some_and(|s| fresh(s, pts, 0.25));
    p[0] = settings.smooth;
    p[1] = settings.white;
    p[2] = u32::from(mask) as f32;
    let Some(d) = points.filter(|t| fresh(t, pts, 0.15)).map(|t| &t.detection) else {
        return p;
    };
    let mut count = 0;
    let mut add = |area: [f32; 4], effect: [f32; 4]| {
        if count < 16 && area[2] > 0.0001 && area[3] > 0.0001 {
            let i = 20 + count * 8;
            p[i..i + 4].copy_from_slice(&area);
            p[i + 4..i + 8].copy_from_slice(&effect);
            count += 1;
        }
    };
    if d.face_count == 478 {
        let f = &d.face;
        let width = (f[454][0] - f[234][0]).abs();
        let height = (f[152][1] - f[10][1]).abs();
        for index in [172, 397] {
            add(
                [f[index][0], f[index][1], width * 0.45, height * 0.4],
                [
                    (f[index][0] - f[1][0]) * settings.slim_face * 0.12,
                    0.,
                    0.,
                    0.,
                ],
            );
        }
        for (left, right) in [(33, 133), (362, 263)] {
            let center = [
                (f[left][0] + f[right][0]) * 0.5,
                (f[left][1] + f[right][1]) * 0.5,
            ];
            let r = (f[left][0] - f[right][0]).abs() * 0.85;
            add(
                [center[0], center[1], r, r * size[0] as f32 / size[1] as f32],
                [0., 0., settings.eyes * 0.25, 1.],
            );
        }
        add(
            [f[152][0], f[152][1], width * 0.4, height * 0.35],
            [0., -height * settings.chin * 0.06, 0., 0.],
        );
    }
    if mask && d.pose_count == 33 {
        let b = &d.pose;
        let visible = |ids: &[usize]| {
            ids.iter().all(|&i| {
                b[i][3] > 0.65 && b[i][0] > 0. && b[i][0] < 1. && b[i][1] > 0. && b[i][1] < 1.
            })
        };
        if visible(&[11, 12, 23, 24]) {
            let sx = (b[11][0] + b[12][0]) * 0.5;
            let sy = (b[11][1] + b[12][1]) * 0.5;
            let hx = (b[23][0] + b[24][0]) * 0.5;
            let hy = (b[23][1] + b[24][1]) * 0.5;
            let w = (b[11][0] - b[12][0]).abs().max((b[23][0] - b[24][0]).abs());
            add(
                [
                    sx * 0.35 + hx * 0.65,
                    sy * 0.35 + hy * 0.65,
                    w * 0.75,
                    (hy - sy).abs() * 0.7,
                ],
                [1., 0., settings.waist * 0.25, 2.],
            );
            for ids in [[23, 25, 27], [24, 26, 28]] {
                if visible(&ids) {
                    let hip = b[ids[0]];
                    let knee = b[ids[1]];
                    let ankle = b[ids[2]];
                    for (a, z) in [(hip, knee), (knee, ankle)] {
                        let dx = (z[0] - a[0]) * size[0] as f32 / size[1] as f32;
                        let dy = z[1] - a[1];
                        let length = dx.hypot(dy).max(0.001);
                        add(
                            [
                                (a[0] + z[0]) * 0.5,
                                (a[1] + z[1]) * 0.5,
                                w * 0.4,
                                (z[1] - a[1]).abs() * 0.8,
                            ],
                            [dy / length, -dx / length, settings.slim_legs * 0.22, 2.],
                        );
                    }
                    add(
                        [knee[0], knee[1], w * 0.5, (ankle[1] - hip[1]).abs() * 0.65],
                        [0., 1., -settings.long_legs * 0.12, 2.],
                    );
                }
            }
        }
    }
    p[4] = count as f32;
    if d.face_count == 478 {
        for (index, (left, right, top, bottom)) in
            [(33, 133, 159, 145), (362, 263, 386, 374), (61, 291, 13, 14)]
                .into_iter()
                .enumerate()
        {
            let f = &d.face;
            p[8 + index * 4..12 + index * 4].copy_from_slice(&[
                (f[left][0] + f[right][0]) * 0.5,
                (f[top][1] + f[bottom][1]) * 0.5,
                (f[left][0] - f[right][0]).abs() * 0.7,
                (f[top][1] - f[bottom][1]).abs().max(0.008) * 1.2,
            ]);
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_and_occluded_geometry_is_rejected() {
        let settings = Settings {
            enabled: true,
            waist: 1.0,
            slim_legs: 1.0,
            long_legs: 1.0,
            ..Default::default()
        };
        let mut detection = Box::<Detection>::default();
        detection.pose_count = 33;
        for (i, point) in [
            (11, [0.35, 0.2, 0., 1.]),
            (12, [0.65, 0.2, 0., 1.]),
            (23, [0.4, 0.5, 0., 1.]),
            (24, [0.6, 0.5, 0., 1.]),
            (25, [0.4, 0.7, 0., 1.]),
            (26, [0.6, 0.7, 0., 1.]),
            (27, [0.4, 0.95, 0., 1.]),
            (28, [0.6, 0.95, 0., 1.]),
        ] {
            detection.pose[i] = point;
        }
        let mut timed = Timed {
            pts: 1.0,
            detection,
        };
        assert_eq!(
            parameters(settings, Some(&timed), Some(&timed), 1., [640, 480])[4],
            7.
        );
        assert_eq!(
            parameters(settings, Some(&timed), None, 1., [640, 480])[4],
            0.
        );
        assert_eq!(
            parameters(settings, Some(&timed), Some(&timed), 0.9, [640, 480])[4],
            0.
        );
        assert_eq!(
            parameters(settings, Some(&timed), Some(&timed), 1.3, [640, 480])[2],
            0.
        );
        timed.detection.pose[23][3] = 0.1;
        assert_eq!(
            parameters(settings, Some(&timed), Some(&timed), 1., [640, 480])[4],
            0.
        );
    }

    fn read(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture) -> Vec<u8> {
        let stride = (texture.width() * 4).div_ceil(256) * 256;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: stride as u64 * texture.height() as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: None,
                },
            },
            texture.size(),
        );
        queue.submit([encoder.finish()]);
        let (tx, rx) = mpsc::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        device.poll(wgpu::Maintain::Wait);
        rx.recv().unwrap().unwrap();
        let map = buffer.slice(..).get_mapped_range();
        map.chunks_exact(stride as usize)
            .flat_map(|r| r[..texture.width() as usize * 4].iter().copied())
            .collect()
    }
    #[test]
    #[ignore = "requires DX12, native AI runtime, portrait/pose test fixtures"]
    fn real_ai_render_and_seek_reset() {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .unwrap();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&Default::default(), None)).unwrap();
        let ctx = egui::Context::default();
        let mut ai = Processor {
            settings: Settings {
                enabled: true,
                slim_face: 0.6,
                eyes: 0.6,
                chin: 0.3,
                waist: 0.7,
                slim_legs: 0.7,
                long_legs: 0.7,
                smooth: 0.7,
                white: 0.5,
            },
            ..Default::default()
        };
        for (name, size) in [("portrait", [426, 640]), ("pose", [640, 426])] {
            let pixels = std::fs::read(format!("test-media/ai/{name}.rgba")).unwrap();
            let source = texture(
                &device,
                size,
                wgpu::TextureFormat::Rgba8UnormSrgb,
                wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            );
            queue.write_texture(
                source.as_image_copy(),
                &pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size[0] * 4),
                    rows_per_image: None,
                },
                source.size(),
            );
            ai.pts = 1.0;
            ai.apply(&device, &queue, &source, &ctx);
            device.poll(wgpu::Maintain::Wait);
            ai.poll(&device, &queue); // Submit old-epoch work before seeking backwards.
            ai.invalidate();
            ai.pts = 0.0;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                if ai.poll(&device, &queue) {
                    ai.apply(&device, &queue, &source, &ctx);
                }
                let state = ai.state.as_ref().unwrap();
                assert!(state.error.is_empty(), "{}", state.error);
                if state.points.is_some() && state.skin.is_some() {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "AI worker timeout: {}",
                    ai.status
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let out = ai.apply(&device, &queue, &source, &ctx);
            let result = read(&device, &queue, &out);
            let state = ai.state.as_ref().unwrap();
            assert_eq!(
                state.points.as_ref().unwrap().pts,
                0.0,
                "late pre-seek landmarks must be rejected"
            );
            assert_eq!(
                state.skin.as_ref().unwrap().pts,
                0.0,
                "late pre-seek mask must be rejected"
            );
            if name == "portrait" {
                assert_eq!(state.points.as_ref().unwrap().detection.face_count, 478);
            } else {
                assert_eq!(state.points.as_ref().unwrap().detection.pose_count, 33);
            }
            assert!(state.params[4] > 0.);
            assert!(pixels.iter().zip(&result).filter(|(a, b)| a != b).count() > 1000);
            assert!(result.chunks_exact(4).all(|p| p[3] == 255));
            println!("{name}: {}", ai.status);
            // Export native-resolution comparison for visual inspection, outside shipped assets.
            std::fs::write(format!("test-media/ai/{name}-processed.rgba"), &result).unwrap();
            ai.invalidate();
            ai.pts = 0.0;
            let reset = ai.apply(&device, &queue, &source, &ctx);
            let reset = read(&device, &queue, &reset);
            assert!(
                pixels.iter().zip(&reset).all(|(a, b)| a.abs_diff(*b) <= 1),
                "seek must not reuse stale geometry"
            );
            ai.settings.enabled = false;
            assert_eq!(ai.apply(&device, &queue, &source, &ctx), source);
            assert!(ai.state.is_none());
            ai.settings.enabled = true;
        }
        // Transparent hidden RGB must stay invisible through bilinear warp sampling.
        let mut state = State::new(&device, [256, 1], 0, &ctx);
        let source = texture(
            &device,
            [256, 1],
            wgpu::TextureFormat::Rgba8UnormSrgb,
            wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let pixels: Vec<u8> = (0..=255u8).flat_map(|a| [180, 120, 95, a]).collect();
        queue.write_texture(
            source.as_image_copy(),
            &pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(1024),
                rows_per_image: None,
            },
            source.size(),
        );
        state.params[4] = 1.;
        state.params[20..28].copy_from_slice(&[0.5, 0.5, 0.4, 1., 0.01, 0., 0., 0.]);
        queue.write_buffer(&state.parameters, 0, bytemuck::cast_slice(&state.params));
        let mut encoder = device.create_command_encoder(&Default::default());
        state.draw(&mut encoder, &state.bind(&device, &source), false);
        queue.submit([encoder.finish()]);
        let result = read(&device, &queue, &state.output);
        assert_eq!(&result[..4], &[0, 0, 0, 0]);
        assert_eq!(result[255 * 4 + 3], 255);
    }

    #[test]
    #[ignore = "requires native AI runtime and test-media/ai/portrait-1080p60.mp4"]
    fn real_time_video_with_ai() {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .unwrap();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&Default::default(), None)).unwrap();
        let renderer = eframe::egui_wgpu::Renderer::new(
            &device,
            wgpu::TextureFormat::Bgra8Unorm,
            None,
            1,
            false,
        );
        let mut display = crate::gpu::Display::new(eframe::egui_wgpu::RenderState {
            adapter,
            available_adapters: Vec::new(),
            device: device.clone(),
            queue: queue.clone(),
            target_format: wgpu::TextureFormat::Bgra8Unorm,
            renderer: Arc::new(egui::mutex::RwLock::new(renderer)),
        });
        display.ai.settings = Settings {
            enabled: true,
            waist: 0.5,
            slim_legs: 0.5,
            long_legs: 0.5,
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut media =
            crate::media::Media::probe(std::path::Path::new("test-media/ai/portrait-1080p60.mp4"))
                .unwrap();
        media.gpu_device = crate::gpu::Device::from_wgpu(&device);
        let mut decoder = crate::native::Decoder::open(&media, true, false).unwrap();
        let start = std::time::Instant::now();
        let mut submissions = Vec::new();
        let mut tracked = 0;
        let mut segmented = 0;
        let mut count = 0;
        while let Some(frame) = decoder.video().unwrap() {
            let target = std::time::Duration::from_secs_f64(frame.pts);
            if let Some(wait) = target.checked_sub(start.elapsed()) {
                std::thread::sleep(wait);
            }
            let tick = std::time::Instant::now();
            display.begin_frame(frame.pts, &ctx);
            display.poll_ai();
            display
                .show(
                    frame.gpu.as_ref().expect("hardware texture"),
                    media.width,
                    media.height,
                )
                .unwrap();
            if frame.pts > 1.0 {
                submissions.push(tick.elapsed().as_secs_f64() * 1000.);
                let state = display.ai.state.as_ref().unwrap();
                assert!(state.error.is_empty(), "{}", state.error);
                tracked += usize::from(state.params[4] > 0.);
                segmented += usize::from(state.params[2] > 0.);
            }
            count += 1;
        }
        device.poll(wgpu::Maintain::Wait);
        submissions.sort_by(f64::total_cmp);
        assert!(count >= 180);
        assert!(
            tracked > submissions.len() * 8 / 10,
            "face geometry unavailable too often: {tracked}/{}",
            submissions.len()
        );
        assert!(
            segmented > submissions.len() * 8 / 10,
            "skin mask unavailable too often: {segmented}/{}",
            submissions.len()
        );
        println!("1080p60 {count} GPU decoded frames + all AI effects: {:.3}s; UI submit p50 {:.3}ms, p95 {:.3}ms, max {:.3}ms; fresh geometry {tracked}/{}, fresh skin {segmented}/{}; excludes window presentation; {}",
            start.elapsed().as_secs_f64(),submissions[submissions.len()/2],submissions[submissions.len()*95/100],
            submissions.last().unwrap(),submissions.len(),submissions.len(),display.ai.status);
    }
}
