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
pub struct Frame {
    handle: *mut c_void,
    texture: OnceLock<wgpu::Texture>,
}
// Immutable, owned COM resources. Only the owning decoder produces a frame;
// after publication, consumers only reference resources and enqueue fence waits.
unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}
impl Frame {
    pub unsafe fn from_raw(handle: *mut c_void) -> Self {
        Self {
            handle,
            texture: OnceLock::new(),
        }
    }
    pub fn texture(
        &self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> Result<&wgpu::Texture, String> {
        let ready = unsafe {
            device.as_hal::<wgpu::hal::api::Dx12, _, _>(|d| {
                d.is_some_and(|d| nkg_gpu_wait(self.handle, d.raw_queue().as_raw()) == 0)
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
                let resource = ID3D12Resource::from_raw(nkg_gpu_resource(self.handle));
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
        unsafe {
            nkg_gpu_frame_close(self.handle);
        }
    }
}

pub struct Display {
    pub state: RenderState,
    pub id: Option<egui::TextureId>,
}
impl Display {
    pub fn show(&mut self, frame: &Frame, width: usize, height: usize) -> Result<(), String> {
        let texture = frame.texture(&self.state.device, width as u32, height as u32)?;
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut renderer = self.state.renderer.write();
        if let Some(id) = self.id {
            renderer.update_egui_texture_from_wgpu_texture(
                &self.state.device,
                &view,
                wgpu::FilterMode::Linear,
                id,
            );
        } else {
            self.id = Some(renderer.register_native_texture(
                &self.state.device,
                &view,
                wgpu::FilterMode::Linear,
            ));
        }
        Ok(())
    }
    pub fn clear(&mut self) {
        if let Some(id) = self.id.take() {
            self.state.renderer.write().free_texture(&id);
        }
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
            .texture(&device, media.width as u32, media.height as u32)
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
        let start = std::time::Instant::now();
        let mut count = 0;
        while let Some(frame) = decoder.video().unwrap() {
            assert!(frame.rgba.is_empty());
            let texture = frame
                .gpu
                .as_ref()
                .unwrap()
                .texture(&device, media.width as u32, media.height as u32)
                .unwrap();
            render(texture);
            count += 1;
            if count == 300 {
                break;
            }
        }
        device.poll(wgpu::Maintain::Wait);
        println!("GPU decode + conversion + egui render: {count} frames in {:?}, {:.1} fps; excludes presentation",start.elapsed(),count as f64/start.elapsed().as_secs_f64());
        if count < 300 {
            if let Some(total) = media.frame_count {
                assert_eq!(count + 1, total);
            }
        }
        decoder.seek(media.duration * 0.5).unwrap();
        let middle = decoder.video().unwrap().unwrap();
        assert!(middle.pts + 0.000001 >= media.duration * 0.5);
        decoder.previous(middle.pts).unwrap();
        assert!(decoder.video().unwrap().unwrap().pts < middle.pts);
        decoder.last(media.duration).unwrap();
        assert!(decoder.video().unwrap().unwrap().gpu.is_some());
        assert!(decoder.video().unwrap().is_none());
        decoder.previous(first.pts + 0.1).unwrap();
        assert!(decoder.video().unwrap().unwrap().gpu.is_some());
    }
}
