use eframe::{egui, egui_wgpu::RenderState, wgpu};
use std::{
    ffi::c_void,
    sync::{Arc, OnceLock},
};
use windows::{
    core::Interface,
    Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Resource},
};

unsafe extern "C" {
    fn nkg_gpu_frame_close(frame: *mut c_void);
    fn nkg_gpu_frame_recycle(frame: *mut c_void);
    fn nkg_gpu_resource(frame: *mut c_void) -> *mut c_void;
    fn nkg_gpu_wait(frame: *mut c_void, queue: *mut c_void) -> i32;
}
#[derive(Debug)]
pub struct Device(pub ID3D12Device);
impl Device {
    pub fn from_wgpu(device: &wgpu::Device) -> Option<Arc<Self>> {
        unsafe {
            device.as_hal::<wgpu::hal::api::Dx12, _, _>(|d| {
                d.map(|d| Arc::new(Self(d.raw_device().clone())))
            })
        }
    }
    pub fn raw(&self) -> *mut c_void {
        self.0.as_raw()
    }
}
struct Handle(*mut c_void);
// Owns only COM resources and a mutex-protected pool; never the decoder context.
unsafe impl Send for Handle {}
impl Handle {
    fn recycle(self) {
        unsafe { nkg_gpu_frame_recycle(self.0) };
        std::mem::forget(self);
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { nkg_gpu_frame_close(self.0) };
    }
}
pub struct Frame {
    handle: Option<Handle>,
    texture: OnceLock<wgpu::Texture>,
    queue: OnceLock<wgpu::Queue>,
}
// Immutable, owned COM resources. Only the owning decoder produces a frame;
// after publication, consumers only reference resources and enqueue fence waits.
unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}
impl Frame {
    pub unsafe fn from_raw(handle: *mut c_void) -> Self {
        Self {
            handle: Some(Handle(handle)),
            texture: OnceLock::new(),
            queue: OnceLock::new(),
        }
    }
    // Keep this Frame alive until all commands using its texture have been submitted.
    // Display does so through Player::raw; cached/history frames retain their own Arc.
    pub fn texture(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
    ) -> Result<&wgpu::Texture, String> {
        if self.queue.get_or_init(|| queue.clone()) != queue {
            return Err("GPU frame used with a different queue".into());
        }
        let handle = self.handle.as_ref().unwrap().0;
        let ready = unsafe {
            device.as_hal::<wgpu::hal::api::Dx12, _, _>(|d| {
                d.is_some_and(|d| nkg_gpu_wait(handle, d.raw_queue().as_raw()) == 0)
            })
        };
        if !ready {
            return Err("GPU shared fence wait failed".into());
        }
        Ok(self.texture.get_or_init(|| {
            let size = wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            };
            let format = wgpu::TextureFormat::Bgra8UnormSrgb;
            unsafe {
                let resource = ID3D12Resource::from_raw(nkg_gpu_resource(handle));
                let native = wgpu::hal::dx12::Device::texture_from_raw(
                    resource,
                    format,
                    wgpu::TextureDimension::D2,
                    size,
                    1,
                    1,
                );
                device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
                    native,
                    &wgpu::TextureDescriptor {
                        label: Some("Video GPU frame"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                        view_formats: &[],
                    },
                )
            }
        }))
    }
}
impl Drop for Frame {
    fn drop(&mut self) {
        let handle = self.handle.take().unwrap();
        if let Some(queue) = self.queue.get() {
            // wgpu retains submitted resources; the native handle retains the surface.
            // Do not capture a wgpu object in its queue callback (a shutdown cycle).
            drop(self.texture.take());
            queue.on_submitted_work_done(move || {
                handle.recycle();
            });
            // If the callback is discarded, Handle::drop destroys instead of recycling.
        } else {
            handle.recycle();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Filters {
    pub exposure: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub sepia: f32,
    pub invert: bool,
    pub vignette: f32,
}
impl Default for Filters {
    fn default() -> Self {
        Self {
            exposure: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            sepia: 0.0,
            invert: false,
            vignette: 0.0,
        }
    }
}
impl Filters {
    fn uniform(self) -> [f32; 8] {
        [
            self.exposure.exp2(),
            self.contrast,
            self.saturation,
            self.sepia,
            u32::from(self.invert) as f32,
            self.vignette,
            u32::from(self != Self::default()) as f32,
            0.0,
        ]
    }
}

struct VideoPass {
    output: wgpu::Texture,
    view: wgpu::TextureView,
    pipeline: wgpu::RenderPipeline,
    beauty: bool,
    parameters: wgpu::Buffer,
    filters: std::cell::Cell<Filters>,
}
impl VideoPass {
    fn new(device: &wgpu::Device, width: u32, height: u32, beauty: bool) -> Self {
        let descriptor = wgpu::TextureDescriptor {
            label: Some("Processed video"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        };
        let output = device.create_texture(&descriptor);
        let view = output.create_view(&Default::default());
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Video beauty and alpha"),
            source: wgpu::ShaderSource::Wgsl(include_str!("alpha.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Video processing"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(if beauty { "fs_beauty" } else { "fs_main" }),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: descriptor.format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        use wgpu::util::DeviceExt;
        let parameters = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Video filter parameters"),
            contents: bytemuck::cast_slice(&Filters::default().uniform()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        Self {
            output,
            view,
            pipeline,
            beauty,
            parameters,
            filters: std::cell::Cell::new(Filters::default()),
        }
    }
    fn bind(&self, device: &wgpu::Device, input: &wgpu::Texture) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Video processing input"),
            layout: &self.pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &input.create_view(&Default::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.parameters.as_entire_binding(),
                },
            ],
        })
    }
    fn render(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bind: &wgpu::BindGroup,
        filters: Filters,
    ) {
        if self.filters.get() != filters {
            queue.write_buffer(
                &self.parameters,
                0,
                bytemuck::cast_slice(&filters.uniform()),
            );
            self.filters.set(filters);
        }
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind, &[]);
            pass.draw(0..3, 0..1);
        }
        queue.submit([encoder.finish()]);
    }
}

struct SoftwareUpload {
    input: wgpu::Texture,
    bind: wgpu::BindGroup,
    pass: VideoPass,
}
impl SoftwareUpload {
    fn new(device: &wgpu::Device, width: u32, height: u32, beauty: bool) -> Self {
        let pass = VideoPass::new(device, width, height, beauty);
        let input = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Software video upload"),
            size: pass.output.size(),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let bind = pass.bind(device, &input);
        Self { input, bind, pass }
    }
    fn upload(&self, device: &wgpu::Device, queue: &wgpu::Queue, rgba: &[u8], filters: Filters) {
        self.write(queue, rgba);
        self.pass.render(device, queue, &self.bind, filters);
    }
    fn write(&self, queue: &wgpu::Queue, rgba: &[u8]) {
        queue.write_texture(
            self.input.as_image_copy(),
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.input.width() * 4),
                rows_per_image: None,
            },
            self.input.size(),
        );
    }
}

pub struct Display {
    pub state: RenderState,
    pub id: Option<egui::TextureId>,
    software: Option<SoftwareUpload>,
    beauty_pass: Option<VideoPass>,
    pub beauty: bool,
    pub filters: Filters,
    pub ai: crate::ai_gpu::Processor,
    context: egui::Context,
}
impl Display {
    pub fn new(state: RenderState) -> Self {
        Self {
            state,
            id: None,
            software: None,
            beauty_pass: None,
            beauty: false,
            filters: Filters::default(),
            ai: Default::default(),
            context: Default::default(),
        }
    }
    pub fn show_rgba(&mut self, rgba: &[u8], width: usize, height: usize) -> Result<(), String> {
        let beauty = self.beauty && !self.ai.settings.enabled;
        let limit = self.state.device.limits().max_texture_dimension_2d as usize;
        if width == 0
            || height == 0
            || width > limit
            || height > limit
            || rgba.len() != width * height * 4
        {
            return Err("Invalid software video texture dimensions or pixels".into());
        }
        if self.software.as_ref().is_none_or(|s| {
            s.input.width() != width as u32
                || s.input.height() != height as u32
                || s.pass.beauty != beauty
        }) {
            self.software = Some(SoftwareUpload::new(
                &self.state.device,
                width as u32,
                height as u32,
                beauty,
            ));
        }
        let upload = self.software.as_ref().unwrap();
        if self.ai.settings.flags() != 0 {
            upload.write(&self.state.queue, rgba);
            let input = self.ai.apply(
                &self.state.device,
                &self.state.queue,
                &upload.input,
                &self.context,
            );
            let bind = upload.pass.bind(&self.state.device, &input);
            upload
                .pass
                .render(&self.state.device, &self.state.queue, &bind, self.filters);
        } else {
            self.ai.clear();
            upload.upload(&self.state.device, &self.state.queue, rgba, self.filters);
        }
        let view = upload.pass.output.create_view(&Default::default());
        self.show_view(&view);
        Ok(())
    }
    pub fn show(&mut self, frame: &Frame, width: usize, height: usize) -> Result<(), String> {
        let texture = frame.texture(
            &self.state.device,
            &self.state.queue,
            width as u32,
            height as u32,
        )?;
        let processed = if self.ai.settings.flags() != 0 {
            Some(self.ai.apply(
                &self.state.device,
                &self.state.queue,
                texture,
                &self.context,
            ))
        } else {
            self.ai.clear();
            None
        };
        let texture = processed.as_ref().unwrap_or(texture);
        let beauty = self.beauty && !self.ai.settings.enabled;
        let view = if self.effects_enabled() {
            if self
                .beauty_pass
                .as_ref()
                .is_none_or(|p| p.output.size() != texture.size() || p.beauty != beauty)
            {
                self.beauty_pass = Some(VideoPass::new(
                    &self.state.device,
                    width as u32,
                    height as u32,
                    beauty,
                ));
            }
            let pass = self.beauty_pass.as_ref().unwrap();
            let bind = pass.bind(&self.state.device, texture);
            pass.render(&self.state.device, &self.state.queue, &bind, self.filters);
            pass.output.create_view(&Default::default())
        } else {
            texture.create_view(&Default::default())
        };
        self.show_view(&view);
        Ok(())
    }
    pub fn effects_enabled(&self) -> bool {
        self.beauty || self.filters != Filters::default() || self.ai.settings.flags() != 0
    }
    pub fn begin_frame(&mut self, pts: f64, ctx: &egui::Context) {
        self.ai.pts = pts;
        self.context = ctx.clone();
    }
    pub fn poll_ai(&mut self) -> bool {
        self.ai.poll(&self.state.device, &self.state.queue)
    }
    fn show_view(&mut self, view: &wgpu::TextureView) {
        let mut renderer = self.state.renderer.write();
        if let Some(id) = self.id {
            renderer.update_egui_texture_from_wgpu_texture(
                &self.state.device,
                view,
                wgpu::FilterMode::Linear,
                id,
            );
        } else {
            self.id = Some(renderer.register_native_texture(
                &self.state.device,
                view,
                wgpu::FilterMode::Linear,
            ));
        }
    }
    pub fn clear(&mut self) {
        if let Some(id) = self.id.take() {
            self.state.renderer.write().free_texture(&id);
        }
        self.software = None;
        self.beauty_pass = None;
        self.ai.clear();
    }
}
impl Drop for Display {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" {
        fn nkg_gpu_frame_allocations(frame: *mut c_void) -> u64;
    }
    fn read_rgba(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture) -> Vec<u8> {
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
        let (tx, rx) = std::sync::mpsc::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        device.poll(wgpu::Maintain::Wait);
        rx.recv().unwrap().unwrap();
        let mapped = buffer.slice(..).get_mapped_range();
        let pixels = mapped
            .chunks_exact(stride as usize)
            .flat_map(|row| row[..texture.width() as usize * 4].iter().copied())
            .collect();
        drop(mapped);
        buffer.unmap();
        pixels
    }
    #[test]
    #[ignore = "GPU alpha validation and software playback; NKG_BENCH_VIDEO, NKG_BENCH_CPU_ALPHA"]
    fn software_upload_and_render() {
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
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
                .unwrap();
        let renderer = eframe::egui_wgpu::Renderer::new(
            &device,
            wgpu::TextureFormat::Bgra8Unorm,
            None,
            1,
            false,
        );
        let mut display = Display::new(RenderState {
            adapter,
            available_adapters: Vec::new(),
            device: device.clone(),
            queue: queue.clone(),
            target_format: wgpu::TextureFormat::Bgra8Unorm,
            renderer: Arc::new(egui::mutex::RwLock::new(renderer)),
        });
        // All 256 alpha values and color values, including nonzero RGB at alpha=0.
        let pixels: Vec<u8> = (0..=255u8)
            .flat_map(|a| (0..=255u8).flat_map(move |v| [v, 255 - v, v / 2, a]))
            .collect();
        display.show_rgba(&pixels, 256, 256).unwrap();
        let actual = read_rgba(
            &device,
            &queue,
            &display.software.as_ref().unwrap().pass.output,
        );
        for (rgba, result) in pixels.chunks_exact(4).zip(actual.chunks_exact(4)) {
            let expected =
                egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3])
                    .to_array();
            for c in 0..4 {
                assert!(
                    expected[c].abs_diff(result[c]) <= 1,
                    "alpha mismatch: {rgba:?} -> {result:?}, expected {expected:?}"
                );
            }
        }
        assert!(display.show_rgba(&[], 256, 256).is_err());
        display.clear();
        let path = std::env::var_os("NKG_BENCH_VIDEO")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "test-media/prores-alpha.mov".into());
        let media = crate::media::Media::probe(&path).unwrap();
        let video = crate::media::Video::start(&media, 0.0, false).unwrap();
        let first = video
            .frames
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        let legacy = std::env::var_os("NKG_BENCH_CPU_ALPHA").is_some();
        let mut image = Arc::new(egui::ColorImage {
            size: [0, 0],
            pixels: Vec::new(),
        });
        let renderer_handle = display.state.renderer.clone();
        let mut prepare = |frame: &crate::media::Frame| {
            if legacy {
                frame.write_image(&media, Arc::get_mut(&mut image).unwrap());
                display.state.renderer.write().update_texture(
                    &device,
                    &queue,
                    egui::TextureId::Managed(0),
                    &egui::epaint::ImageDelta::full(image.clone(), egui::TextureOptions::LINEAR),
                );
                egui::TextureId::Managed(0)
            } else {
                display
                    .show_rgba(&frame.rgba, media.width, media.height)
                    .unwrap();
                display.id.unwrap()
            }
        };
        let id = prepare(&first);
        let size = wgpu::Extent3d {
            width: media.width as u32,
            height: media.height as u32,
            depth_or_array_layers: 1,
        };
        let output = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Software playback test"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = output.create_view(&Default::default());
        let rect = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(media.width as f32, media.height as f32),
        );
        let mut mesh = egui::Mesh::with_texture(id);
        mesh.add_rect_with_uv(
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        let jobs = [egui::ClippedPrimitive {
            clip_rect: rect,
            primitive: egui::epaint::Primitive::Mesh(mesh),
        }];
        let screen = eframe::egui_wgpu::ScreenDescriptor {
            size_in_pixels: [size.width, size.height],
            pixels_per_point: 1.0,
        };
        device.poll(wgpu::Maintain::Wait);
        let begin = std::time::Instant::now();
        let mut count = 0;
        let mut waits = std::time::Duration::ZERO;
        let mut uploads = std::time::Duration::ZERO;
        loop {
            let start = std::time::Instant::now();
            let frame = match video
                .frames
                .recv_timeout(std::time::Duration::from_secs(10))
            {
                Ok(frame) => frame.unwrap(),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(e) => panic!("{e}"),
            };
            waits += start.elapsed();
            let start = std::time::Instant::now();
            assert_eq!(prepare(&frame), id);
            uploads += start.elapsed();
            let mut renderer = renderer_handle.write();
            let mut encoder = device.create_command_encoder(&Default::default());
            let callbacks = renderer.update_buffers(&device, &queue, &mut encoder, &jobs, &screen);
            {
                let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                renderer.render(&mut pass.forget_lifetime(), &jobs, &screen);
            }
            queue.submit(callbacks.into_iter().chain([encoder.finish()]));
            count += 1;
            if count == 300 {
                break;
            }
        }
        device.poll(wgpu::Maintain::Wait);
        let elapsed = begin.elapsed();
        assert!(count > 0);
        println!("alpha={} {count} frames: {:.3}ms {:.1}fps wait={waits:?} prepare+upload={uploads:?}; includes GPU completion, excludes presentation",
            if legacy { "CPU" } else { "GPU" }, elapsed.as_secs_f64()*1000.0, count as f64/elapsed.as_secs_f64());
    }
    #[test]
    #[ignore = "requires DX12 hardware"]
    fn beauty_preserves_alpha_edges_and_toggle() {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
                .unwrap();
        let renderer = eframe::egui_wgpu::Renderer::new(
            &device,
            wgpu::TextureFormat::Bgra8Unorm,
            None,
            1,
            false,
        );
        let mut display = Display::new(RenderState {
            adapter,
            available_adapters: Vec::new(),
            device: device.clone(),
            queue: queue.clone(),
            target_format: wgpu::TextureFormat::Bgra8Unorm,
            renderer: Arc::new(egui::mutex::RwLock::new(renderer)),
        });
        let pixels: Vec<u8> = (0..16)
            .flat_map(|y| {
                (0..16).flat_map(move |x| {
                    if x < 8 {
                        let noise = if (x + y) % 2 == 0 { 0 } else { 12 };
                        [180 + noise, 120 + noise, 95 + noise, 255]
                    } else {
                        [0, 0, 255, 255]
                    }
                })
            })
            .collect();
        display.show_rgba(&pixels, 16, 16).unwrap();
        let id = display.id;
        let original = read_rgba(
            &device,
            &queue,
            &display.software.as_ref().unwrap().pass.output,
        );
        display.beauty = true;
        display.show_rgba(&pixels, 16, 16).unwrap();
        let beauty = read_rgba(
            &device,
            &queue,
            &display.software.as_ref().unwrap().pass.output,
        );
        let channel = |x: usize, y: usize| (y * 16 + x) * 4;
        assert!(
            beauty[channel(3, 3)] > original[channel(3, 3)],
            "skin should brighten"
        );
        assert!(
            beauty[channel(3, 3)].abs_diff(beauty[channel(4, 3)]) < 12,
            "small skin variations should smooth"
        );
        assert!(
            beauty[channel(7, 3) + 2] < 130,
            "blue must not bleed across the edge"
        );
        assert_eq!(
            &beauty[channel(8, 3)..channel(16, 3)],
            &pixels[channel(8, 3)..channel(16, 3)]
        );
        // Hardware frames use BGRA sRGB input; verify identical shader behavior.
        let bgra = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: 16,
                height: 16,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let bgra_pixels: Vec<u8> = pixels
            .chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        queue.write_texture(
            bgra.as_image_copy(),
            &bgra_pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(64),
                rows_per_image: None,
            },
            bgra.size(),
        );
        let pass = VideoPass::new(&device, 16, 16, true);
        pass.render(
            &device,
            &queue,
            &pass.bind(&device, &bgra),
            Filters::default(),
        );
        assert_eq!(beauty, read_rgba(&device, &queue, &pass.output));
        let output = display.software.as_ref().unwrap().pass.output.clone();
        display.filters = Filters {
            exposure: 0.4,
            contrast: 1.1,
            saturation: 0.7,
            sepia: 0.3,
            invert: true,
            vignette: 0.5,
        };
        display.show_rgba(&pixels, 16, 16).unwrap();
        assert_eq!(
            output,
            display.software.as_ref().unwrap().pass.output,
            "filter adjustments must reuse the output texture"
        );
        pass.render(&device, &queue, &pass.bind(&device, &bgra), display.filters);
        assert_eq!(
            read_rgba(&device, &queue, &output),
            read_rgba(&device, &queue, &pass.output)
        );
        display.filters = Filters::default();
        display.beauty = false;
        display.show_rgba(&pixels, 16, 16).unwrap();
        assert_eq!(display.id, id);
        assert_eq!(
            original,
            read_rgba(
                &device,
                &queue,
                &display.software.as_ref().unwrap().pass.output
            )
        );
        display.beauty = true;
        let alpha: Vec<u8> = (0..=255u8).flat_map(|a| [180, 120, 95, a]).collect();
        display.show_rgba(&alpha, 256, 1).unwrap();
        let result = read_rgba(
            &device,
            &queue,
            &display.software.as_ref().unwrap().pass.output,
        );
        assert_eq!(&result[..4], &[0, 0, 0, 0]);
        for (a, p) in result.chunks_exact(4).enumerate() {
            assert_eq!(p[3], a as u8);
        }
        // Hidden RGB at alpha=0 must have no influence on a neighboring skin pixel.
        display
            .show_rgba(&[180, 120, 95, 128, 255, 0, 0, 0], 2, 1)
            .unwrap();
        let edge = read_rgba(
            &device,
            &queue,
            &display.software.as_ref().unwrap().pass.output,
        );
        display.show_rgba(&[180, 120, 95, 128], 1, 1).unwrap();
        let single = read_rgba(
            &device,
            &queue,
            &display.software.as_ref().unwrap().pass.output,
        );
        assert_eq!(&edge[..4], &single);
        display.clear();
        assert!(display.id.is_none());
        assert!(display.software.is_none());
        assert!(
            display.beauty,
            "selection should survive opening another video"
        );
    }

    #[test]
    #[ignore = "requires DX12 hardware; validates filters and measures 1080p processing"]
    fn filters_pixels_and_performance() {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .unwrap();
        println!("Adapter: {:?}", adapter.get_info());
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
                .unwrap();
        let base = Filters::default();
        let pixels: Vec<u8> = (0..=255u8).flat_map(|a| [100, 140, 180, a]).collect();
        let upload = SoftwareUpload::new(&device, 256, 1, false);
        // Independent expected encoded RGB, before linear-light alpha premultiplication.
        for (filters, expected) in [
            (base, [100, 140, 180]),
            (
                Filters {
                    saturation: 0.0,
                    ..base
                },
                [134, 134, 134],
            ),
            (Filters { sepia: 1.0, ..base }, [159, 133, 106]),
            (
                Filters {
                    invert: true,
                    ..base
                },
                [155, 115, 75],
            ),
            (
                Filters {
                    exposure: 1.0,
                    ..base
                },
                [200, 255, 255],
            ),
            (
                Filters {
                    exposure: -1.0,
                    ..base
                },
                [50, 70, 90],
            ),
            (
                Filters {
                    contrast: 0.0,
                    ..base
                },
                [128, 128, 128],
            ),
            (
                Filters {
                    contrast: 2.0,
                    ..base
                },
                [73, 153, 233],
            ),
            (
                Filters {
                    saturation: 0.0,
                    invert: true,
                    ..base
                },
                [121, 121, 121],
            ),
            (base, [100, 140, 180]),
        ] {
            upload.upload(&device, &queue, &pixels, filters);
            let actual = read_rgba(&device, &queue, &upload.pass.output);
            for (a, p) in actual.chunks_exact(4).enumerate() {
                let expected = egui::Color32::from_rgba_unmultiplied(
                    expected[0],
                    expected[1],
                    expected[2],
                    a as u8,
                )
                .to_array();
                assert_eq!(p[3], a as u8);
                for c in 0..3 {
                    assert!(
                        p[c].abs_diff(expected[c]) <= 2,
                        "{filters:?}: alpha={a}, {p:?} != {expected:?}"
                    );
                }
            }
            assert_eq!(&actual[..4], &[0, 0, 0, 0]);
        }
        let upload = SoftwareUpload::new(&device, 16, 16, false);
        let pixels = [100, 140, 180, 255].repeat(256);
        upload.upload(
            &device,
            &queue,
            &pixels,
            Filters {
                vignette: 1.0,
                ..base
            },
        );
        let actual = read_rgba(&device, &queue, &upload.pass.output);
        assert_eq!(
            &actual[(8 * 16 + 8) * 4..(8 * 16 + 8) * 4 + 4],
            &[100, 140, 180, 255]
        );
        assert!(
            actual[0] < 30 && actual[3] == 255,
            "corners should darken without changing alpha"
        );

        // Every effect still applies after beauty, including its non-skin early exit.
        let beauty = SoftwareUpload::new(&device, 16, 16, true);
        beauty.upload(
            &device,
            &queue,
            &pixels,
            Filters {
                saturation: 0.0,
                ..base
            },
        );
        let gray = read_rgba(&device, &queue, &beauty.pass.output);
        assert!(gray.chunks_exact(4).all(|p| p[0] == p[1] && p[1] == p[2]));

        // Resident texture, reused bind/pipeline/output, no upload or readback in timing.
        let upload = SoftwareUpload::new(&device, 1920, 1080, false);
        upload.upload(
            &device,
            &queue,
            &[100, 140, 180, 255].repeat(1920 * 1080),
            base,
        );
        let all = Filters {
            exposure: 0.3,
            contrast: 1.2,
            saturation: 0.8,
            sepia: 0.4,
            invert: true,
            vignette: 0.5,
        };
        let mut times = [Vec::new(), Vec::new()];
        for round in 0..5 {
            for index in [round % 2, 1 - round % 2] {
                let filters = [base, all][index];
                upload.pass.render(&device, &queue, &upload.bind, filters);
                device.poll(wgpu::Maintain::Wait);
                let start = std::time::Instant::now();
                for _ in 0..120 {
                    upload.pass.render(&device, &queue, &upload.bind, filters);
                }
                device.poll(wgpu::Maintain::Wait);
                times[index].push(start.elapsed().as_secs_f64() * 1000.0 / 120.0);
            }
        }
        for (name, mut times) in ["alpha only", "all color filters"].into_iter().zip(times) {
            times.sort_by(f64::total_cmp);
            println!("1080p {name}: median {:.3} ms/frame (5 x 120; CPU submission + GPU completion; excludes decode/upload/presentation)", times[2]);
        }
    }

    #[test]
    #[ignore = "requires DX12 hardware and FFmpeg; optional NKG_BENCH_VIDEO"]
    fn gpu_decode_and_import() {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .unwrap();
        println!("Adapter: {:?}", adapter.get_info());
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
                .unwrap();
        let path = std::env::var_os("NKG_BENCH_VIDEO")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "test-media/h264.mp4".into());
        let mut media = crate::media::Media::probe(&path).unwrap();
        let mut cpu = crate::native::Decoder::open(&media, true, false).unwrap();
        let reference = cpu.video().unwrap().unwrap();
        media.gpu_device = Device::from_wgpu(&device);
        assert!(media.gpu_device.is_some());
        let mut decoder = crate::native::Decoder::open(&media, true, false).unwrap();
        let first = decoder.video().unwrap().unwrap();
        assert!(first.rgba.is_empty());
        let texture = first
            .gpu
            .as_ref()
            .unwrap()
            .texture(&device, &queue, media.width as u32, media.height as u32)
            .unwrap();
        let stride = (media.width as u32 * 4).div_ceil(256) * 256;
        let output = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("egui test output"),
            size: texture.size(),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = output.create_view(&Default::default());
        let mut renderer =
            eframe::egui_wgpu::Renderer::new(&device, output.format(), None, 1, false);
        let id = renderer.register_native_texture(
            &device,
            &texture.create_view(&Default::default()),
            wgpu::FilterMode::Nearest,
        );
        let rect = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(media.width as f32, media.height as f32),
        );
        let mut mesh = egui::Mesh::with_texture(id);
        mesh.add_rect_with_uv(
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        let jobs = [egui::ClippedPrimitive {
            clip_rect: rect,
            primitive: egui::epaint::Primitive::Mesh(mesh),
        }];
        let screen = eframe::egui_wgpu::ScreenDescriptor {
            size_in_pixels: [media.width as u32, media.height as u32],
            pixels_per_point: 1.0,
        };
        let mut render = |texture: &wgpu::Texture| {
            renderer.update_egui_texture_from_wgpu_texture(
                &device,
                &texture.create_view(&Default::default()),
                wgpu::FilterMode::Nearest,
                id,
            );
            let mut encoder = device.create_command_encoder(&Default::default());
            let callbacks = renderer.update_buffers(&device, &queue, &mut encoder, &jobs, &screen);
            {
                let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                renderer.render(&mut pass.forget_lifetime(), &jobs, &screen);
            }
            queue.submit(callbacks.into_iter().chain([encoder.finish()]));
        };
        render(texture);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: stride as u64 * media.height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let verify = |reference: &crate::media::Frame| {
            let mut encoder = device.create_command_encoder(&Default::default());
            encoder.copy_texture_to_buffer(
                output.as_image_copy(),
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
            let (tx, rx) = std::sync::mpsc::channel();
            buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
            device.poll(wgpu::Maintain::Wait);
            rx.recv().unwrap().unwrap();
            let bytes = buffer.slice(..).get_mapped_range();
            let mut error = 0u64;
            for y in 0..media.height {
                for x in 0..media.width {
                    let p = y * stride as usize + x * 4;
                    let q = (y * media.width + x) * 4;
                    for c in 0..3 {
                        error += bytes[p + 2 - c].abs_diff(reference.rgba[q + c]) as u64;
                    }
                    assert_eq!(bytes[p + 3], 255);
                }
            }
            let mean = error as f64 / (media.width * media.height * 3) as f64;
            println!("GPU vs CPU mean RGB error: {mean:.3}");
            assert!(mean < 4.0, "GPU conversion mismatch: {mean}");
            drop(bytes);
            buffer.unmap();
        };
        verify(&reference);
        let start = std::time::Instant::now();
        let mut count = 0;
        while let Some(frame) = decoder.video().unwrap() {
            assert!(frame.rgba.is_empty());
            let texture = frame
                .gpu
                .as_ref()
                .unwrap()
                .texture(&device, &queue, media.width as u32, media.height as u32)
                .unwrap();
            render(texture);
            count += 1;
            if count == 300 {
                break;
            }
        }
        device.poll(wgpu::Maintain::Wait);
        println!("GPU decode + conversion + egui render: {count} frames in {:?}, {:.1} fps; excludes presentation",start.elapsed(),count as f64/start.elapsed().as_secs_f64());
        let allocations = unsafe {
            nkg_gpu_frame_allocations(first.gpu.as_ref().unwrap().handle.as_ref().unwrap().0)
        };
        println!(
            "Shared output allocations: {allocations} for {} frames",
            count + 1
        );
        if count < 300 {
            if let Some(total) = media.frame_count {
                assert_eq!(count + 1, total);
            }
        }
        decoder.seek(media.duration * 0.5).unwrap();
        device.poll(wgpu::Maintain::Wait);
        let middle = decoder.video().unwrap().unwrap();
        if count > 0 && media.width * media.height * 4 <= 64 * 1024 * 1024 {
            assert_eq!(
                unsafe {
                    nkg_gpu_frame_allocations(
                        middle.gpu.as_ref().unwrap().handle.as_ref().unwrap().0,
                    )
                },
                allocations,
                "A completed, released surface should be reused after seeking"
            );
        }
        assert!(middle.pts + 0.000001 >= media.duration * 0.5);
        render(
            middle
                .gpu
                .as_ref()
                .unwrap()
                .texture(&device, &queue, media.width as u32, media.height as u32)
                .unwrap(),
        );
        cpu.seek(middle.pts).unwrap();
        verify(&cpu.video().unwrap().unwrap());
        decoder.previous(middle.pts).unwrap();
        let previous = decoder.video().unwrap().unwrap();
        assert!(previous.pts < middle.pts);
        render(
            previous
                .gpu
                .as_ref()
                .unwrap()
                .texture(&device, &queue, media.width as u32, media.height as u32)
                .unwrap(),
        );
        cpu.seek(previous.pts).unwrap();
        verify(&cpu.video().unwrap().unwrap());
        decoder.last(media.duration).unwrap();
        assert!(decoder.video().unwrap().unwrap().gpu.is_some());
        assert!(decoder.video().unwrap().is_none());
        decoder.previous(first.pts + 0.1).unwrap();
        assert!(decoder.video().unwrap().unwrap().gpu.is_some());
        // Cached decoder views must never alias/overwrite retained output frames.
        drop(decoder);
        render(texture);
        verify(&reference);
        // Exercise worker-thread retirement concurrently with UI queue callbacks.
        let video = crate::media::Video::start(&media, 0.0, true).unwrap();
        for _ in 0..140 {
            let frame = match video.frames.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(frame) => frame.unwrap(),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(e) => panic!("GPU worker stalled: {e}"),
            };
            render(
                frame
                    .gpu
                    .as_ref()
                    .unwrap()
                    .texture(&device, &queue, media.width as u32, media.height as u32)
                    .unwrap(),
            );
        }
        drop(video);
        device.poll(wgpu::Maintain::Wait);
    }
}
