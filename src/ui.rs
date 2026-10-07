use crate::media::{Audio, Clock, Frame, Media, Video};
use eframe::egui::{self, Color32, RichText, Stroke, Vec2, ViewportCommand};
use rodio::{OutputStream, OutputStreamHandle, Sink};
use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver},
        Arc,
    },
    thread,
    time::Duration,
};

const BG: Color32 = Color32::from_rgb(12, 12, 14);
const PANEL: Color32 = Color32::from_rgb(31, 31, 31);
const MUTED: Color32 = Color32::from_rgb(145, 150, 158);
const ACCENT: Color32 = Color32::from_rgb(0, 122, 204);

struct Loaded {
    media: Media,
    video: Video,
    first: Frame,
    audio: Option<Audio>,
    hardware: bool,
    mode: String,
    warning: String,
}
#[cfg(test)]
fn load(path: PathBuf, position: f64) -> Result<Loaded, String> {
    load_media(Media::probe(&path)?, position, None, &|| false)
}

fn load_media(
    media: Media,
    position: f64,
    hardware: Option<bool>,
    cancelled: &impl Fn() -> bool,
) -> Result<Loaded, String> {
    if cancelled() {
        return Err("Seek superseded".into());
    }
    // Start audio while the video decoder initializes, not after its first frame.
    let audio = if media.audio_index.is_some() {
        Audio::start(&media, position).ok()
    } else {
        None
    };
    let mut warning = String::new();
    let modes = if media.alpha || hardware == Some(false) {
        vec![(false, false)]
    } else if media.gpu_device.is_some() {
        vec![(true, true), (true, false), (false, false)]
    } else {
        vec![(true, false), (false, false)]
    };
    if media.alpha {
        warning = "Alpha preserved with software decoding".into();
    }
    for (hardware, gpu) in modes {
        let mut media = media.clone();
        if !gpu {
            media.gpu_device = None;
        }
        if cancelled() {
            return Err("Seek superseded".into());
        }
        let attempt = (|| {
            let video = Video::start(&media, position, hardware)?;
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            loop {
                if cancelled() {
                    return Err("Seek superseded".into());
                }
                match video.frames.recv_timeout(Duration::from_millis(5)) {
                    Ok(frame) => return Ok((video, frame?)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                        if std::time::Instant::now() < deadline => {}
                    Err(e) => return Err(format!("No decoded frame: {e}")),
                }
            }
        })();
        match attempt {
            Ok((video, first)) => {
                return Ok(Loaded {
                    media,
                    video,
                    first,
                    audio,
                    hardware,
                    mode: if gpu {
                        "D3D11VA → D3D12 · GPU texture"
                    } else if hardware {
                        "D3D11VA · Hardware"
                    } else {
                        "FFmpeg · Software"
                    }
                    .into(),
                    warning,
                })
            }
            Err(e) if gpu => {
                warning = format!("GPU texture interop unavailable; compatibility path. {e}")
            }
            Err(e) if hardware => warning = format!("Hardware unavailable; software fallback. {e}"),
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}
pub struct Player {
    media: Option<Media>,
    video: Option<Video>,
    raw: Option<Frame>,
    next: Option<Frame>,
    texture: Option<egui::TextureHandle>,
    gpu_display: Option<crate::gpu::Display>,
    prepared: Arc<egui::ColorImage>,
    audio: Option<Audio>,
    sink: Option<Sink>,
    output: Option<(OutputStream, OutputStreamHandle)>,
    clock: Clock,
    audio_start: f64,
    playing: bool,
    step: bool,
    eof: bool,
    pending: Option<Receiver<Result<Loaded, String>>>,
    generation: Arc<AtomicU64>,
    resume: bool,
    hardware: Option<bool>,
    mode: String,
    warning: String,
    error: String,
    volume: f32,
    muted: bool,
    checker: bool,
    fullscreen: bool,
    seek_value: f64,
    seeking: bool,
    seek_pending: bool,
    browser: bool,
    information: bool,
    directory: PathBuf,
    location: String,
    entries: Vec<(PathBuf, bool)>,
    browser_error: String,
    recent: Vec<PathBuf>,
    dropped: u64,
}

impl Player {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = PANEL;
        style.visuals.window_fill = PANEL;
        style.visuals.window_corner_radius = 0.into();
        style.visuals.selection.bg_fill = ACCENT;
        style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(45, 45, 48);
        style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(45, 45, 48);
        style.visuals.widgets.inactive.bg_stroke = Stroke::NONE;
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(62, 62, 66);
        style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(62, 62, 66);
        style.visuals.widgets.noninteractive.bg_stroke =
            Stroke::new(1.0_f32, Color32::from_gray(48));
        for v in [
            &mut style.visuals.widgets.noninteractive,
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
            &mut style.visuals.widgets.open,
        ] {
            v.corner_radius = 0.into();
        }
        style.spacing.button_padding = Vec2::new(12.0, 7.0);
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.slider_rail_height = 3.0;
        style.visuals.slider_trailing_fill = true;
        cc.egui_ctx.set_style(style);
        // Use an installed font for Chinese filenames; do not ship system font files.
        if let Ok(bytes) = fs::read("C:/Windows/Fonts/msyh.ttc") {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("cjk".into(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("cjk".into());
            cc.egui_ctx.set_fonts(fonts);
        }
        let output = OutputStream::try_default().ok();
        let directory = std::env::current_dir().unwrap_or_default();
        let mut app = Self {
            media: None,
            video: None,
            raw: None,
            next: None,
            texture: None,
            gpu_display: cc
                .wgpu_render_state
                .clone()
                .map(|state| crate::gpu::Display { state, id: None }),
            prepared: Arc::new(egui::ColorImage {
                size: [0, 0],
                pixels: Vec::new(),
            }),
            audio: None,
            sink: None,
            output,
            clock: Clock::new(0.0),
            audio_start: 0.0,
            playing: false,
            step: false,
            eof: false,
            pending: None,
            generation: Arc::new(AtomicU64::new(0)),
            resume: true,
            hardware: None,
            mode: "Ready".into(),
            warning: String::new(),
            error: String::new(),
            volume: 0.8,
            muted: false,
            checker: true,
            fullscreen: false,
            seek_value: 0.0,
            seeking: false,
            seek_pending: false,
            browser: false,
            information: false,
            location: display_path(&directory),
            directory,
            entries: vec![],
            browser_error: String::new(),
            recent: vec![],
            dropped: 0,
        };
        if let Some(path) = std::env::args_os().nth(1) {
            app.open(PathBuf::from(path), 0.0, true, cc.egui_ctx.clone());
        }
        app
    }
    fn pause_audio(&mut self) {
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
    }
    fn stop_audio(&mut self) {
        self.pause_audio();
        self.audio = None;
    }
    fn open(&mut self, path: PathBuf, position: f64, resume: bool, ctx: egui::Context) {
        self.stop_audio();
        self.video = None;
        self.next = None;
        self.pending = None;
        self.playing = false;
        self.eof = false;
        self.step = false;
        self.seek_pending = false;
        self.resume = resume;
        self.error.clear();
        self.warning.clear();
        self.mode = "Opening…".into();
        self.clock = Clock::new(position);
        self.seek_value = position;
        self.seeking = false;
        self.dropped = 0;
        self.raw = None;
        self.texture = None;
        if let Some(display) = &mut self.gpu_display {
            display.clear();
        }
        self.media = None;
        self.hardware = None;
        let id = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let generation = self.generation.clone();
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        let gpu_device = self
            .gpu_display
            .as_ref()
            .and_then(|d| crate::gpu::Device::from_wgpu(&d.state.device));
        thread::spawn(move || {
            let result = Media::probe(&path).and_then(|mut media| {
                media.gpu_device = gpu_device;
                load_media(media, position, None, &|| {
                    generation.load(Ordering::Relaxed) != id
                })
            });
            if generation.load(Ordering::Relaxed) == id {
                let _ = tx.send(result);
                ctx.request_repaint();
            }
        });
    }
    fn seek(&mut self, position: f64, resume: bool, ctx: &egui::Context) {
        self.seek_to(position, resume, false, ctx);
    }
    fn previous_frame(&mut self, ctx: &egui::Context) {
        if self.pending.is_none() && !self.seek_pending {
            if let Some(frame) = &self.raw {
                self.seek_to(frame.pts.max(0.0), false, true, ctx);
            }
        }
    }
    fn seek_to(&mut self, position: f64, resume: bool, previous: bool, ctx: &egui::Context) {
        let Some(media) = &self.media else {
            return;
        };
        let at_end = !previous && media.duration > 0.0 && position >= media.duration;
        let position = if previous {
            position
        } else {
            position.max(0.0).min(media.duration.max(0.0))
        };
        let Some(video) = &mut self.video else {
            return;
        };
        if let Err(e) = if at_end {
            video.last(position)
        } else if previous {
            video.previous(position)
        } else {
            video.seek(position)
        } {
            self.error = e;
            return;
        }
        self.pause_audio();
        if !previous && !at_end {
            if let Some(audio) = &mut self.audio {
                if let Err(e) = audio.seek(position) {
                    self.warning = e;
                    self.audio = None;
                }
            }
            self.attach_audio();
        }
        self.audio_start = position;
        self.clock = Clock::new(position);
        self.next = None;
        self.eof = false;
        self.step = false;
        self.playing = false;
        self.resume = resume && !at_end;
        self.seek_pending = true;
        self.seek_value = position;
        self.seeking = false;
        self.error.clear();
        ctx.request_repaint();
    }
    fn attach_audio(&mut self) {
        let Some(audio) = &mut self.audio else {
            return;
        };
        let Some(source) = audio.source.take() else {
            return;
        };
        let Some((_, handle)) = &self.output else {
            self.warning = "No audio output device; playing video only".into();
            self.audio = None;
            return;
        };
        match Sink::try_new(handle) {
            Ok(sink) => {
                sink.pause();
                sink.set_volume(if self.muted { 0.0 } else { self.volume });
                sink.append(source);
                self.sink = Some(sink);
            }
            Err(e) => self.warning = format!("Audio output unavailable: {e}"),
        }
    }
    fn toggle(&mut self, ctx: &egui::Context) {
        if self.pending.is_some() || self.seek_pending || self.media.is_none() {
            return;
        }
        if self.eof {
            self.seek(0.0, true, ctx);
            return;
        }
        self.playing = !self.playing;
        if self.playing {
            // Frame stepping invalidates the old audio position.
            if self.sink.is_none() && self.media.as_ref().is_some_and(|m| m.audio_index.is_some()) {
                self.seek(self.clock.position(), true, ctx);
                return;
            }
            self.clock.play();
            if let Some(sink) = &self.sink {
                sink.play();
            }
        } else {
            self.clock.pause();
            if let Some(sink) = &self.sink {
                sink.pause();
            }
        }
    }
    fn position(&self) -> f64 {
        if self.playing {
            if let (Some(audio), Some(sink)) = (&self.audio, &self.sink) {
                if !sink.empty() {
                    return self.audio_start
                        + audio.played.load(Ordering::Relaxed) as f64 / 96000.0;
                }
            }
        }
        self.clock.position()
    }
    fn timeline_position(&self) -> f64 {
        timeline_time(
            self.position(),
            self.media.as_ref().map_or(0.0, |m| m.duration),
            self.eof && self.next.is_none() && !self.seek_pending && self.error.is_empty(),
        )
    }
    fn display(&mut self, frame: Frame, ctx: &egui::Context) {
        if let Some(media) = &self.media {
            if let Some(gpu) = &frame.gpu {
                if let Some(display) = &mut self.gpu_display {
                    if let Err(e) = display.show(gpu, media.width, media.height) {
                        self.error = e;
                        self.playing = false;
                        self.clock.pause();
                        if let Some(sink) = &self.sink {
                            sink.pause();
                        }
                        return;
                    }
                    self.raw = Some(frame);
                    return;
                }
            }
            if Arc::get_mut(&mut self.prepared).is_none() {
                self.prepared = Arc::new(egui::ColorImage {
                    size: [0, 0],
                    pixels: Vec::new(),
                });
            }
            frame.write_image(media, Arc::get_mut(&mut self.prepared).unwrap());
            let image = self.prepared.clone();
            if let Some(texture) = &mut self.texture {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                self.texture = Some(ctx.load_texture("video", image, egui::TextureOptions::LINEAR));
            }
            self.raw = Some(frame);
        }
    }
    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(error) = self
            .audio
            .as_ref()
            .and_then(|a| a.error.lock().unwrap().take())
        {
            self.warning = format!("Audio decoding failed: {error}");
        }
        if let Some(result) = self.pending.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.pending = None;
            match result {
                Ok(loaded) => {
                    self.hardware = Some(loaded.hardware);
                    self.mode = loaded.mode;
                    self.warning = loaded.warning;
                    self.audio_start = self.clock.position();
                    if !self.recent.contains(&loaded.media.path) {
                        self.recent.insert(0, loaded.media.path.clone());
                        self.recent.truncate(12);
                    }
                    self.media = Some(loaded.media);
                    self.video = Some(loaded.video);
                    self.display(loaded.first, ctx);
                    self.audio = loaded.audio;
                    self.attach_audio();
                    self.playing = self.resume;
                    if self.playing {
                        self.clock.play();
                        if let Some(sink) = &self.sink {
                            sink.play();
                        }
                    }
                }
                Err(e) => {
                    self.error = e;
                    self.mode = "Unable to play".into();
                    self.media = None;
                }
            }
        }
        if self.seek_pending {
            if let Some(result) = self.video.as_ref().and_then(|v| match v.frames.try_recv() {
                Ok(frame) => Some(frame),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("No frame at seek position".into()))
                }
                Err(mpsc::TryRecvError::Empty) => None,
            }) {
                self.seek_pending = false;
                match result {
                    Ok(frame) => {
                        if !self.resume {
                            self.clock = Clock::new(frame.pts);
                        }
                        self.display(frame, ctx);
                        self.playing = self.resume;
                        if self.playing {
                            self.clock.play();
                            if let Some(sink) = &self.sink {
                                sink.play();
                            }
                        }
                    }
                    Err(e) => {
                        self.error = e;
                        self.pause_audio();
                    }
                }
            }
        }
        if self.playing {
            let position = self.position();
            self.clock.set(position);
        }
        if self.playing || self.step {
            let position = self.position();
            let mut selected = None;
            for _ in 0..8 {
                if self.next.is_none() {
                    if let Some(video) = &self.video {
                        match video.frames.try_recv() {
                            Ok(Ok(frame)) => self.next = Some(frame),
                            Ok(Err(e)) => {
                                self.error = e;
                                self.playing = false;
                                self.stop_audio();
                                self.clock.pause();
                                break;
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                self.eof = true;
                                break;
                            }
                            Err(mpsc::TryRecvError::Empty) => break,
                        }
                    }
                }
                if self
                    .next
                    .as_ref()
                    .is_some_and(|f| self.step || f.pts <= position)
                {
                    if selected.is_some() {
                        self.dropped += 1;
                    }
                    selected = self.next.take();
                    if self.step {
                        break;
                    }
                } else {
                    break;
                }
            }
            if let Some(frame) = selected {
                if self.step {
                    self.clock = Clock::new(frame.pts);
                    self.step = false;
                }
                self.display(frame, ctx);
            }
            if self.eof && self.next.is_none() {
                let end = self.raw.as_ref().map_or(position, |f| {
                    f.pts + self.media.as_ref().map_or(0.0, |m| 1.0 / m.fps)
                });
                if position >= end || self.step {
                    self.playing = false;
                    self.step = false;
                    self.clock.pause();
                    self.pause_audio();
                }
            }
        }
        // Prefetch while paused too, so stepping onto the last frame discovers EOF
        // without requiring an extra click. Keep the displayed frame's true PTS.
        if !self.playing && !self.step && !self.seek_pending && self.next.is_none() && !self.eof {
            if let Some(video) = &self.video {
                match video.frames.try_recv() {
                    Ok(Ok(frame)) => self.next = Some(frame),
                    Ok(Err(e)) => {
                        self.error = e;
                        self.eof = true;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => self.eof = true,
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
        }
        if self.playing {
            if let Some(video) = &mut self.video {
                video.suspend_prefetch();
            }
        } else if !self.seek_pending && self.pending.is_none() {
            if let (Some(video), Some(frame)) = (&mut self.video, &self.raw) {
                video.prefetch(frame.pts);
            }
        }
        if self.playing {
            // Let the presentation/vsync loop pace playback. A 5 ms deferred
            // repaint can miss the next refresh and turn 60 fps into uneven 30/60.
            ctx.request_repaint();
        } else if self.pending.is_some()
            || self.seek_pending
            || self.step
            || (self.video.is_some() && self.next.is_none() && !self.eof)
        {
            ctx.request_repaint_after(Duration::from_millis(5));
        }
    }
    fn browse(&mut self) {
        self.browser = true;
        self.read_directory();
    }
    fn read_directory(&mut self) {
        self.location = display_path(&self.directory);
        self.browser_error.clear();
        match fs::read_dir(&self.directory) {
            Ok(entries) => {
                self.entries = entries
                    .filter_map(Result::ok)
                    .map(|e| (e.path(), e.file_type().is_ok_and(|t| t.is_dir())))
                    .collect();
                self.entries.sort_by(|a, b| {
                    b.1.cmp(&a.1)
                        .then_with(|| a.0.file_name().cmp(&b.0.file_name()))
                });
            }
            Err(e) => self.browser_error = e.to_string(),
        }
    }
    fn titlebar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("title")
            .exact_height(30.0)
            .frame(egui::Frame::new().fill(BG))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let (_, drag) = ui.allocate_exact_size(
                        Vec2::new((ui.available_width() - 108.0).max(0.0), 30.0),
                        egui::Sense::click_and_drag(),
                    );
                    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                    if drag.drag_started() {
                        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                    }
                    if drag.double_clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
                    }
                    if icon(ui, "minimize", "最小化").clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
                    }
                    if icon(ui, "maximize", "最大化 / 还原").clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
                    }
                    if icon(ui, "close", "关闭").clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                });
            });
    }
    fn controls(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("transport")
            .exact_height(76.0)
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(20, 8)),
            )
            .show(ctx, |ui| {
                let duration = self.media.as_ref().map_or(0.0, |m| m.duration);
                let mut position = if self.seeking {
                    self.seek_value
                } else {
                    self.timeline_position()
                };
                ui.spacing_mut().slider_width = ui.available_width();
                let response = ui
                    .push_id("timeline", |ui| {
                        ui.add_enabled(
                            duration > 0.0 && self.media.is_some(),
                            egui::Slider::new(&mut position, 0.0..=duration.max(0.001))
                                .show_value(false),
                        )
                    })
                    .inner;
                // Keep the preview value during a drag; seeking is committed on release.
                if response.dragged() {
                    self.seeking = true;
                    self.seek_value = position;
                }
                if response.drag_stopped() || (response.changed() && !response.dragged()) {
                    self.seeking = false;
                    self.seek(
                        position,
                        if self.pending.is_some() || self.seek_pending {
                            self.resume
                        } else {
                            self.playing
                        },
                        ctx,
                    );
                }
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(
                        self.media.is_some() && self.pending.is_none() && !self.seek_pending,
                        |ui| {
                            if icon(
                                ui,
                                if self.playing { "pause" } else { "play" },
                                "播放 / 暂停 · Space",
                            )
                            .clicked()
                            {
                                self.toggle(ctx);
                            }
                            if icon(ui, "next", "下一帧 · →").clicked() {
                                self.next_frame();
                            }
                        },
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!("{}  /  {}", time(self.timeline_position()), time(duration)))
                            .size(12.0)
                            .color(MUTED),
                    );
                    if let Some(m) = &self.media {
                        let pts = self.raw.as_ref().map_or(0.0, |f| f.pts);
                        ui.label(RichText::new(format!("·  {}", frame_label(pts, m.fps, m.frame_count))).size(12.0).color(MUTED))
                            .on_hover_text(format!("当前帧号 / 末帧号，均从 0 开始。{}\n当前帧号仍按显示帧时间戳和平均帧率估算，可变帧率或非零视频起点可能有偏差。总帧数采用文件标注；未标注时不猜测末帧号。", m.frame_count.map_or_else(|| "总帧数未知。".into(), |n| format!("共 {n} 帧。"))));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon(ui, "fullscreen", "全屏 · F11").clicked() {
                            self.fullscreen = !self.fullscreen;
                            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(self.fullscreen));
                        }
                        ui.add_space(12.0);
                        ui.spacing_mut().slider_width = 64.0;
                        ui.add(egui::Slider::new(&mut self.volume, 0.0..=1.0).show_value(false))
                            .on_hover_text("音量");
                        if icon(
                            ui,
                            if self.muted { "muted" } else { "volume" },
                            "静音 / 恢复声音",
                        )
                        .clicked()
                        {
                            self.muted = !self.muted;
                        }
                    });
                });
                if let Some(sink) = &self.sink {
                    sink.set_volume(if self.muted { 0.0 } else { self.volume });
                }
            });
    }
    fn context_menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.set_min_width(200.0);
        if ui.button("打开视频…                 Ctrl+O").clicked() {
            self.browse();
            ui.close_menu();
        }
        if !self.recent.is_empty() {
            let mut selected = None;
            ui.menu_button("最近打开", |ui| {
                for path in &self.recent {
                    if ui
                        .button(path.file_name().unwrap_or_default().to_string_lossy())
                        .clicked()
                    {
                        selected = Some(path.clone());
                        ui.close_menu();
                    }
                }
            });
            if let Some(path) = selected {
                self.open(path, 0.0, true, ctx.clone());
                ui.close_menu();
            }
        }
        ui.separator();
        ui.add_enabled_ui(
            self.media.is_some() && self.pending.is_none() && !self.seek_pending,
            |ui| {
                if ui.button("从头播放").clicked() {
                    self.seek(0.0, true, ctx);
                    ui.close_menu();
                }
                if ui.button("下一帧                         →").clicked() {
                    self.next_frame();
                    ui.close_menu();
                }
                if ui.button("上一帧                         ←").clicked() {
                    self.previous_frame(ctx);
                    ui.close_menu();
                }
            },
        );
        ui.checkbox(&mut self.checker, "透明棋盘格背景");
        if ui
            .button(if self.fullscreen {
                "退出全屏                    F11"
            } else {
                "全屏                            F11"
            })
            .clicked()
        {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(self.fullscreen));
            ui.close_menu();
        }
        ui.separator();
        if ui.button("视频信息…").clicked() {
            self.information = true;
            ui.close_menu();
        }
    }
    fn information_ui(&mut self, ctx: &egui::Context) {
        if !self.information {
            return;
        }
        let mut close = false;
        let response = egui::Modal::new(egui::Id::new("video_information")).show(ctx, |ui| {
            ui.set_width(520.0_f32.min(ctx.screen_rect().width() - 48.0));
            ui.horizontal(|ui| {
                ui.heading("视频信息");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    close = icon(ui, "close", "关闭视频信息").clicked();
                });
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height((ctx.screen_rect().height() - 180.0).max(100.0))
                .show(ui, |ui| {
                    let value_width = (ui.available_width() - 134.0).max(80.0);
                    egui::Grid::new("media_details")
                        .num_columns(2)
                        .spacing([24.0, 9.0])
                        .show(ui, |ui| {
                            let mut row = |label: &str, value: String| {
                                ui.add_sized(
                                    [110.0, 18.0],
                                    egui::Label::new(RichText::new(label).color(MUTED))
                                        .wrap()
                                        .halign(egui::Align::Min),
                                );
                                ui.add_sized(
                                    [value_width, 18.0],
                                    egui::Label::new(value)
                                        .wrap()
                                        .halign(egui::Align::Min)
                                        .selectable(true),
                                );
                                ui.end_row();
                            };
                            if let Some(m) = &self.media {
                                row(
                                    "文件",
                                    m.path
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .into_owned(),
                                );
                                row("路径", display_path(&m.path));
                                row("时长", time(m.duration));
                                row("播放进度时间", time(self.timeline_position()));
                                if let Some(frame) = &self.raw {
                                    row("当前帧 PTS（相对起点）", format!("{:.6} s", frame.pts));
                                }
                                row("分辨率", format!("{} × {}", m.width, m.height));
                                row("帧率", format!("{:.3} fps", m.fps));
                                row(
                                    "总帧数",
                                    m.frame_count.map_or_else(
                                        || "未知（文件未标注）".into(),
                                        |n| n.to_string(),
                                    ),
                                );
                                row("视频编码", m.codec.to_uppercase());
                                row("像素格式", m.pixel_format.clone());
                                row("透明通道", if m.alpha { "有" } else { "无" }.into());
                                for (label, value) in &m.details {
                                    row(label, value.clone());
                                }
                            }
                            row("解码方式", self.mode.clone());
                            row("播放跳帧", self.dropped.to_string());
                        });
                    if !self.warning.is_empty() {
                        ui.separator();
                        ui.label(&self.warning);
                    }
                    if !self.error.is_empty() {
                        ui.separator();
                        ui.colored_label(Color32::LIGHT_RED, &self.error);
                    }
                });
            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close |= ui.button("关闭").clicked();
            });
        });
        if close || response.should_close() {
            self.information = false;
        }
    }
    fn next_frame(&mut self) {
        if self.video.is_some() && self.pending.is_none() && !self.seek_pending {
            self.clock = Clock::new(self.raw.as_ref().map_or(0.0, |f| f.pts));
            self.playing = false;
            self.pause_audio();
            self.step = true;
        }
    }
    fn browser_ui(&mut self, ctx: &egui::Context) {
        if !self.browser {
            return;
        }
        let mut open = true;
        let mut selected = None;
        let mut directory = None;
        egui::Window::new("Open video")
            .open(&mut open)
            .default_size([700.0, 450.0])
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("↑").clicked() {
                        directory = self.directory.parent().map(|p| p.to_path_buf());
                    }
                    let response = ui.add_sized(
                        [ui.available_width() - 60.0, 28.0],
                        egui::TextEdit::singleline(&mut self.location),
                    );
                    if ui.button("Go").clicked()
                        || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    {
                        let path = PathBuf::from(&self.location);
                        if path.is_dir() {
                            directory = Some(path);
                        } else {
                            selected = Some(path);
                        }
                    }
                });
                ui.horizontal(|ui| {
                    for drive in 'C'..='Z' {
                        let path = PathBuf::from(format!("{drive}:/"));
                        if path.is_dir() && ui.small_button(format!("{drive}:")).clicked() {
                            directory = Some(path);
                        }
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(340.0)
                    .show(ui, |ui| {
                        for (path, dir) in &self.entries {
                            let name = path.file_name().unwrap_or_default().to_string_lossy();
                            if ui
                                .selectable_label(
                                    false,
                                    format!("{}   {name}", if *dir { "▸" } else { "▷" }),
                                )
                                .clicked()
                            {
                                if *dir {
                                    directory = Some(path.clone());
                                } else {
                                    selected = Some(path.clone());
                                }
                            }
                        }
                    });
                if !self.browser_error.is_empty() {
                    ui.colored_label(Color32::LIGHT_RED, &self.browser_error);
                }
                ui.separator();
                ui.label(
                    RichText::new("MP4 · MKV · MOV · WebM · AVI · MPEG · TS · FLV · WMV · AV1")
                        .small()
                        .color(MUTED),
                );
            });
        self.browser = open;
        if let Some(path) = directory {
            self.directory = path;
            self.read_directory();
        }
        if let Some(path) = selected {
            self.browser = false;
            self.open(path, 0.0, true, ctx.clone());
        }
    }
}

fn icon(ui: &mut egui::Ui, kind: &str, label: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(36.0, 30.0), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    let p = ui.painter();
    if response.hovered() {
        p.rect_filled(
            rect,
            3.0,
            if kind == "close" {
                Color32::from_rgb(175, 45, 50)
            } else {
                Color32::from_gray(35)
            },
        );
    }
    let color = if !ui.is_enabled() {
        Color32::from_gray(60)
    } else if response.hovered() {
        Color32::WHITE
    } else {
        Color32::from_gray(185)
    };
    let stroke = Stroke::new(1.5_f32, color);
    let c = rect.center();
    let point = |x, y| c + Vec2::new(x, y);
    let line = |a: (f32, f32), b: (f32, f32)| {
        p.line_segment([point(a.0, a.1), point(b.0, b.1)], stroke);
    };
    match kind {
        "play" | "next" => {
            p.add(egui::Shape::convex_polygon(
                vec![point(-5.0, -7.0), point(6.0, 0.0), point(-5.0, 7.0)],
                color,
                Stroke::NONE,
            ));
            if kind == "next" {
                line((8.0, -7.0), (8.0, 7.0));
            }
        }
        "pause" => {
            for x in [-5.0, 2.0] {
                p.rect_filled(
                    egui::Rect::from_min_size(point(x, -7.0), Vec2::new(3.0, 14.0)),
                    0.0,
                    color,
                );
            }
        }
        "minimize" => line((-5.0, 2.0), (5.0, 2.0)),
        "maximize" => {
            p.rect_stroke(
                egui::Rect::from_center_size(c, Vec2::splat(10.0)),
                0.0,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        "close" => {
            line((-5.0, -5.0), (5.0, 5.0));
            line((-5.0, 5.0), (5.0, -5.0));
        }
        "fullscreen" => {
            for x in [-1.0, 1.0] {
                for y in [-1.0, 1.0] {
                    line((x * 7.0, y * 3.0), (x * 7.0, y * 7.0));
                    line((x * 7.0, y * 7.0), (x * 3.0, y * 7.0));
                }
            }
        }
        "volume" | "muted" => {
            p.add(egui::Shape::convex_polygon(
                vec![
                    point(-8.0, -3.0),
                    point(-4.0, -3.0),
                    point(1.0, -7.0),
                    point(1.0, 7.0),
                    point(-4.0, 3.0),
                    point(-8.0, 3.0),
                ],
                color,
                Stroke::NONE,
            ));
            if kind == "muted" {
                line((5.0, -3.0), (10.0, 3.0));
                line((5.0, 3.0), (10.0, -3.0));
            } else {
                p.add(egui::Shape::line(
                    vec![
                        point(5.0, -5.0),
                        point(8.0, -2.0),
                        point(8.0, 2.0),
                        point(5.0, 5.0),
                    ],
                    stroke,
                ));
            }
        }
        _ => {}
    }
    response.on_hover_text(label)
}
fn display_path(path: &std::path::Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    if let Some(unc) = path.strip_prefix("//?/UNC/") {
        format!("//{unc}")
    } else {
        path.strip_prefix("//?/").unwrap_or(&path).to_owned()
    }
}
fn frame_label(pts: f64, fps: f64, total: Option<u64>) -> String {
    // ponytail: PTS-based index is approximate for VFR; an exact global index needs a frame timestamp index.
    let current = (pts.max(0.0) * fps).round() as u64;
    total.and_then(|n| n.checked_sub(1)).map_or_else(
        || format!("帧 {current}"),
        |last| format!("帧 {current} / {last}"),
    )
}
fn timeline_time(position: f64, duration: f64, ended: bool) -> f64 {
    if duration > 0.0 {
        if ended {
            duration
        } else {
            position.clamp(0.0, duration)
        }
    } else {
        position.max(0.0)
    }
}
fn time(seconds: f64) -> String {
    let ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let s = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        s / 3600,
        (s / 60) % 60,
        s % 60,
        ms % 1000
    )
}

impl eframe::App for Player {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.poll(ctx);
        if !self.information {
            if let Some(path) =
                ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()))
            {
                self.open(path, 0.0, true, ctx.clone());
            }
        }
        if !ctx.wants_keyboard_input() && !self.browser && !self.information {
            if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
                self.toggle(ctx);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                self.next_frame();
            }
            if ctx.input(|i| {
                i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::Backspace)
            }) {
                self.previous_frame(ctx);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::O) && i.modifiers.ctrl) {
                self.browse();
            }
            if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
                self.fullscreen = !self.fullscreen;
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(self.fullscreen));
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.fullscreen = false;
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
            }
        }
        if !self.fullscreen {
            self.titlebar(ctx);
        }
        self.controls(ctx);
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(0))
            .show(ctx, |ui| {
                let rect = ui.available_rect_before_wrap();
                let canvas = ui.interact(rect, ui.id().with("canvas"), egui::Sense::click());
                let texture_id = self
                    .gpu_display
                    .as_ref()
                    .and_then(|d| d.id)
                    .or_else(|| self.texture.as_ref().map(|t| t.id()));
                if let (Some(texture_id), Some(media)) = (texture_id, &self.media) {
                    let height = rect.height().min(rect.width() / media.aspect);
                    let image_rect = egui::Rect::from_center_size(
                        rect.center(),
                        Vec2::new(height * media.aspect, height),
                    );
                    if self.checker && media.alpha {
                        let tile = 18.0;
                        for y in 0..(image_rect.height() / tile).ceil() as usize {
                            for x in 0..(image_rect.width() / tile).ceil() as usize {
                                let r = egui::Rect::from_min_size(
                                    image_rect.min + Vec2::new(x as f32 * tile, y as f32 * tile),
                                    Vec2::splat(tile),
                                )
                                .intersect(image_rect);
                                ui.painter().rect_filled(
                                    r,
                                    0.0,
                                    if (x + y) % 2 == 0 {
                                        Color32::from_gray(45)
                                    } else {
                                        Color32::from_gray(65)
                                    },
                                );
                            }
                        }
                    } else {
                        ui.painter().rect_filled(image_rect, 0.0, Color32::BLACK);
                    }
                    ui.painter().image(
                        texture_id,
                        image_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                } else {
                    ui.scope_builder(
                        egui::UiBuilder::new().max_rect(egui::Rect::from_center_size(
                            rect.center(),
                            Vec2::new(320.0, 80.0),
                        )),
                        |ui| {
                            ui.vertical_centered(|ui| {
                                if self.pending.is_some() {
                                    ui.spinner();
                                } else if !self.error.is_empty() {
                                    ui.label(
                                        RichText::new("无法播放此视频").color(Color32::LIGHT_RED),
                                    )
                                    .on_hover_text(&self.error);
                                    if ui.button("打开其他视频").clicked() {
                                        self.browse();
                                    }
                                } else {
                                    if ui
                                        .add(
                                            egui::Button::new(RichText::new("打开视频").size(16.0))
                                                .frame(false),
                                        )
                                        .clicked()
                                    {
                                        self.browse();
                                    }
                                    ui.label(
                                        RichText::new("或将文件拖到这里").size(12.0).color(MUTED),
                                    );
                                }
                            });
                        },
                    );
                }
                if texture_id.is_some() && !self.error.is_empty() {
                    ui.painter().text(
                        rect.center_top() + Vec2::new(0.0, 20.0),
                        egui::Align2::CENTER_TOP,
                        "播放已停止 · 右键查看详情",
                        egui::FontId::proportional(13.0),
                        Color32::LIGHT_RED,
                    );
                }
                if canvas.double_clicked() {
                    self.fullscreen = !self.fullscreen;
                    ctx.send_viewport_cmd(ViewportCommand::Fullscreen(self.fullscreen));
                } else if canvas.clicked() && self.media.is_some() {
                    self.toggle(ctx);
                }
                canvas.context_menu(|ui| self.context_menu(ui, ctx));
            });
        self.browser_ui(ctx);
        self.information_ui(ctx);
        if !self.information
            && !self.fullscreen
            && !ctx.input(|i| i.viewport().maximized.unwrap_or(false))
        {
            if let Some(p) = ctx.input(|i| i.pointer.hover_pos()) {
                let r = ctx.screen_rect();
                let left = p.x < r.left() + 5.0;
                let right = p.x > r.right() - 5.0;
                let top = p.y < r.top() + 5.0;
                let bottom = p.y > r.bottom() - 5.0;
                use egui::{CursorIcon as C, ResizeDirection as D};
                let edge = match (left, right, top, bottom) {
                    (true, _, true, _) => Some((D::NorthWest, C::ResizeNwSe)),
                    (_, true, true, _) => Some((D::NorthEast, C::ResizeNeSw)),
                    (true, _, _, true) => Some((D::SouthWest, C::ResizeNeSw)),
                    (_, true, _, true) => Some((D::SouthEast, C::ResizeNwSe)),
                    (true, _, _, _) => Some((D::West, C::ResizeHorizontal)),
                    (_, true, _, _) => Some((D::East, C::ResizeHorizontal)),
                    (_, _, true, _) => Some((D::North, C::ResizeVertical)),
                    (_, _, _, true) => Some((D::South, C::ResizeVertical)),
                    _ => None,
                };
                if let Some((direction, cursor)) = edge {
                    ctx.set_cursor_icon(cursor);
                    if ctx.input(|i| i.pointer.primary_pressed()) {
                        ctx.send_viewport_cmd(ViewportCommand::BeginResize(direction));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod playback_tests {
    use super::*;
    #[test]
    #[ignore = "native reverse timing; optional NKG_BENCH_VIDEO"]
    fn cached_reverse_latency() {
        let path = std::env::var_os("NKG_BENCH_VIDEO")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("test-media/h264.mp4"));
        let media = Media::probe(&path).unwrap();
        let mut decoder = crate::native::Decoder::open(&media, !media.alpha, false).unwrap();
        decoder.seek(media.duration * 0.5).unwrap();
        let mut frames = Vec::new();
        for _ in 0..8 {
            frames.push(decoder.video().unwrap().unwrap());
        }
        let begin = std::time::Instant::now();
        for index in (1..frames.len()).rev() {
            decoder.previous(frames[index].pts).unwrap();
            let actual = decoder.video().unwrap().unwrap();
            assert!(Arc::ptr_eq(&actual.rgba, &frames[index - 1].rgba));
        }
        println!(
            "7 cached reverse steps (no rendering): {:?}",
            begin.elapsed()
        );
        decoder.seek(media.duration * 0.5).unwrap();
        let frame = decoder.video().unwrap().unwrap();
        let begin = std::time::Instant::now();
        decoder.previous(frame.pts).unwrap();
        assert!(decoder.video().unwrap().unwrap().pts < frame.pts);
        println!(
            "uncached reverse step (no rendering): {:?}",
            begin.elapsed()
        );
    }
    #[test]
    fn fractional_frame_time() {
        assert_eq!(frame_label(0.0, 30.0, Some(60)), "帧 0 / 59");
        assert_eq!(frame_label(59.0 / 30.0, 30.0, Some(60)), "帧 59 / 59");
        assert_eq!(frame_label(2.0 / 30.0, 30.0, Some(3)), "帧 2 / 2");
        assert_eq!(frame_label(0.0, 30.0, None), "帧 0");
        assert_eq!(frame_label(0.0, 30.0, Some(1)), "帧 0 / 0");
        assert_eq!(frame_label(0.0, 30.0, Some(0)), "帧 0");
        assert_eq!(time(59.0 / 30.0), "00:00:01.967");
        assert_eq!(time(2.0), "00:00:02.000");
        assert_eq!(time(0.0), "00:00:00.000");
        assert_eq!(time(timeline_time(119.0 / 60.0, 2.0, true)), "00:00:02.000");
        assert_eq!(
            time(timeline_time(119.0 / 60.0, 2.0, false)),
            "00:00:01.983"
        );
        assert_eq!(timeline_time(2.1, 2.0, false), 2.0);
    }
    #[test]
    #[ignore = "requires generated media and D3D11VA"]
    fn decode_reaches_last_frame() {
        for (name, count) in [
            ("hevc.mkv", 60),
            ("vp9-alpha.webm", 72),
            ("h264.mp4", 600),
            ("vfr.mp4", 0),
        ] {
            let media = Media::probe(&PathBuf::from("test-media").join(name)).unwrap();
            let mut video = Video::start(&media, 0.0, !media.alpha).unwrap();
            let mut pts = Vec::new();
            loop {
                match video.frames.recv_timeout(Duration::from_secs(3)) {
                    Ok(frame) => pts.push(frame.unwrap().pts),
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(e) => panic!("{name}: {e}"),
                }
            }
            if count > 0 {
                assert_eq!(pts.len(), count, "{name}: missing tail frames");
                assert!((pts.last().unwrap() - (count - 1) as f64 / media.fps).abs() < 0.002);
            }
            video.last(media.duration).unwrap();
            let tail = video
                .frames
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert_eq!(tail.pts, *pts.last().unwrap(), "{name}: end seek lost tail");
            assert!(matches!(
                video.frames.recv_timeout(Duration::from_secs(3)),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ));
            assert_eq!(
                timeline_time(tail.pts, media.duration, true),
                media.duration
            );
        }
    }
    #[test]
    #[ignore = "requires generated test media and D3D11VA"]
    fn previous_frame_matches_decode() {
        for name in ["h264.mp4", "vfr.mp4", "vp9-alpha.webm"] {
            let media = Media::probe(&PathBuf::from("test-media").join(name)).unwrap();
            let mut reference = crate::native::Decoder::open(&media, !media.alpha, false).unwrap();
            reference.seek(0.0).unwrap();
            let mut frames = Vec::new();
            for _ in 0..8 {
                frames.push(reference.video().unwrap().unwrap());
            }
            // Warm reverse steps must return the same pixel allocation, without
            // native seeking, conversion, or copying. Replay can then cross back
            // into the live decoder without skipping/duplicating a frame.
            reference.previous(frames[7].pts).unwrap();
            let cached = reference.video().unwrap().unwrap();
            assert!(Arc::ptr_eq(&cached.rgba, &frames[6].rgba));
            let forward = reference.video().unwrap().unwrap();
            assert!(Arc::ptr_eq(&forward.rgba, &frames[7].rgba));
            assert!(reference.video().unwrap().unwrap().pts > forward.pts);
            let mut video = Video::start(&media, 0.0, !media.alpha).unwrap();
            for index in (1..frames.len()).rev() {
                video.previous(frames[index].pts).unwrap();
                let actual = video
                    .frames
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                assert!(
                    (actual.pts - frames[index - 1].pts).abs() < 0.000001,
                    "{name}: wrong previous PTS"
                );
                assert!(
                    actual.rgba == frames[index - 1].rgba,
                    "{name}: wrong previous pixels"
                );
                let next = video
                    .frames
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                assert!(
                    (next.pts - frames[index].pts).abs() < 0.000001,
                    "{name}: forward after backward"
                );
            }
            video.previous(frames[0].pts).unwrap();
            let first = video
                .frames
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert!((first.pts - frames[0].pts).abs() < 0.000001);
        }
    }
    #[test]
    fn readable_paths() {
        assert_eq!(
            display_path(std::path::Path::new(r"\\?\E:\视频\demo.mp4")),
            "E:/视频/demo.mp4"
        );
        assert_eq!(
            display_path(std::path::Path::new(r"\\?\UNC\server\share\demo.mp4")),
            "//server/share/demo.mp4"
        );
        assert_eq!(
            display_path(std::path::Path::new(r"E:\demo.mp4")),
            "E:/demo.mp4"
        );
    }
    #[test]
    #[ignore = "real FFmpeg seek timing; optional NKG_BENCH_VIDEO path"]
    fn seek_latency() {
        use std::time::Instant;
        let path = std::env::var_os("NKG_BENCH_VIDEO")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("test-media/h264.mp4"));
        let mut loaded = load(path, 0.0).unwrap();
        assert!(loaded.hardware, "{}", loaded.warning);
        let mut timings = Vec::new();
        for fraction in [0.1, 0.7, 0.3, 0.9, 0.5, 0.02, 0.8, 0.4, 0.6, 0.2] {
            let target = loaded.media.duration * fraction;
            let begin = Instant::now();
            loaded.video.seek(target).unwrap();
            if let Some(audio) = &mut loaded.audio {
                audio.seek(target).unwrap();
            }
            let dispatch = begin.elapsed();
            let frame = loaded
                .video
                .frames
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            let ready = begin.elapsed();
            assert!(
                frame.pts + 0.000001 >= target && frame.pts < target + 0.1,
                "{} vs {target}",
                frame.pts
            );
            let _image = egui::ColorImage::from_rgba_unmultiplied(
                [loaded.media.width, loaded.media.height],
                &frame.rgba,
            );
            let prepared = begin.elapsed();
            assert!(
                dispatch < Duration::from_millis(50),
                "UI command dispatch blocked"
            );
            timings.push(prepared.as_micros());
            println!("target={target:.3}s command={dispatch:?} decode={ready:?} render_ready={prepared:?} pts={:.6}",frame.pts);
        }
        // Rapid replacement while the old bounded queue is full must not deadlock or deliver stale frames.
        for fraction in [0.8, 0.1, 0.7, 0.25] {
            loaded.video.seek(loaded.media.duration * fraction).unwrap();
        }
        let frame = loaded
            .video
            .frames
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert!((frame.pts - loaded.media.duration * 0.25).abs() < 0.1);
        // EOF retains the worker and its hardware device so replay uses the same path.
        loaded
            .video
            .seek((loaded.media.duration - 0.1).max(0.0))
            .unwrap();
        while loaded
            .video
            .frames
            .recv_timeout(Duration::from_secs(3))
            .is_ok()
        {}
        loaded.video.seek(0.0).unwrap();
        assert!(
            loaded
                .video
                .frames
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap()
                .pts
                < 0.1
        );
        timings.sort();
        println!(
            "persistent seek render-ready median={:.3}ms max={:.3}ms",
            timings[timings.len() / 2] as f64 / 1000.0,
            timings.last().unwrap().to_owned() as f64 / 1000.0
        );
        assert!(
            timings[timings.len() / 2] < 150_000,
            "Warm seek regressed beyond 150 ms"
        );
    }
    #[test]
    #[ignore = "requires generated test media and D3D11VA"]
    fn persistent_seek_matches_linear_decode() {
        for name in ["h264.mp4", "vfr.mp4", "vp9-alpha.webm"] {
            let media = Media::probe(&PathBuf::from("test-media").join(name)).unwrap();
            let mut linear = crate::native::Decoder::open(&media, !media.alpha, false).unwrap();
            let mut seek = crate::native::Decoder::open(&media, !media.alpha, false).unwrap();
            linear.seek(0.0).unwrap();
            for target in [0.07, 0.51, 1.234] {
                let expected = loop {
                    let frame = linear.video().unwrap().unwrap();
                    if frame.pts + 0.000001 >= target {
                        break frame;
                    }
                };
                seek.seek(target).unwrap();
                let actual = seek.video().unwrap().unwrap();
                assert!(
                    (actual.pts - expected.pts).abs() < 0.000001,
                    "{name}: timestamps differ"
                );
                assert!(
                    actual.rgba == expected.rgba,
                    "{name}: wrong frame after seek"
                );
            }
        }
        let media = Media::probe(&PathBuf::from("test-media/h264.mp4")).unwrap();
        let mut linear = crate::native::Decoder::open(&media, false, true).unwrap();
        linear.seek(0.0).unwrap();
        let mut reference = Vec::new();
        while reference.len() < 2 * 96000 {
            reference.extend(linear.audio().unwrap().unwrap());
        }
        let mut seek = crate::native::Decoder::open(&media, false, true).unwrap();
        for target in [1.25, 0.5, 1.75] {
            seek.seek(target).unwrap();
            let actual = seek.audio().unwrap().unwrap();
            let offset = (target * 48000.0).round() as usize * 2;
            assert!(actual.len() > 20);
            let error = actual
                .iter()
                .zip(&reference[offset..])
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(error < 0.002, "PCM mismatch after seek {target}: {error}");
        }
    }
    #[test]
    #[ignore = "requires scripts/make-test-media.ps1 and a D3D11VA-capable GPU"]
    fn real_decode_formats_seek_and_alpha() {
        for name in [
            "h264.mp4",
            "hevc.mkv",
            "av1.mkv",
            "vp9.webm",
            "mpeg4.avi",
            "vp9-alpha.webm",
            "vp8-alpha.webm",
            "prores-alpha.mov",
            "vfr.mp4",
        ] {
            let loaded = load(PathBuf::from("test-media").join(name), 0.0)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            println!(
                "{name}: {} {}x{}",
                loaded.mode, loaded.media.width, loaded.media.height
            );
            assert_eq!(
                loaded.first.rgba.len(),
                loaded.media.width * loaded.media.height * 4
            );
            if ["h264.mp4", "hevc.mkv", "av1.mkv", "vp9.webm"].contains(&name) {
                assert!(
                    loaded.mode.contains("Hardware"),
                    "{name}: {}",
                    loaded.warning
                );
            }
            if name.contains("alpha") {
                assert!(loaded.media.alpha);
                assert!(loaded.first.rgba.chunks_exact(4).any(|p| p[3] == 0));
                assert!(loaded
                    .first
                    .rgba
                    .chunks_exact(4)
                    .any(|p| p[3] > 100 && p[3] < 240));
            }
            let second = loaded
                .video
                .frames
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert!(second.pts > loaded.first.pts);
        }
        let seek = load(PathBuf::from("test-media/h264.mp4"), 1.0).unwrap();
        assert!((seek.first.pts - 1.0).abs() < 0.04);
        assert!(load(PathBuf::from("Cargo.toml"), 0.0).is_err());
    }
}
