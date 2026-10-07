#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod gpu;
mod media;
mod native;
mod ui;
fn main() -> eframe::Result {
    let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::default();
    setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    setup.power_preference = eframe::wgpu::PowerPreference::HighPerformance;
    eframe::run_native(
        "NKG Video",
        eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                .with_decorations(false)
                .with_inner_size([1280.0, 800.0])
                .with_min_inner_size([800.0, 520.0]),
            renderer: eframe::Renderer::Wgpu,
            wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
                wgpu_setup: setup.into(),
                ..Default::default()
            },
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(ui::Player::new(cc)))),
    )
}
