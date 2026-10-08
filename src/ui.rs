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
struct Player {
    id: u64,
    path: Option<PathBuf>,
    open_request: Option<PathBuf>,
    media: Option<Media>,
    video: Option<Video>,
    raw: Option<Frame>,
    next: Option<Frame>,
    texture: Option<egui::TextureHandle>,
    gpu_display: Option<crate::gpu::Display>,
    prepared: Arc<egui::ColorImage>,
    audio: Option<Audio>,
    sink: Option<Sink>,
    output: Option<OutputStreamHandle>,
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
    fn new(
        id: u64,
        state: Option<eframe::egui_wgpu::RenderState>,
        output: Option<OutputStreamHandle>,
    ) -> Self {
        let directory = std::env::current_dir().unwrap_or_default();
        Self {
            id,
            path: None,
            open_request: None,
            media: None,
            video: None,
            raw: None,
            next: None,
            texture: None,
            gpu_display: state.map(crate::gpu::Display::new),
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
        }
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
        self.path = Some(path.clone());
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
        if let Some(display) = &mut self.gpu_display {
            display.ai.invalidate();
        }
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
        let Some(handle) = &self.output else {
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
            self.clock.position(),
            self.media.as_ref().map_or(0.0, |m| m.duration),
            self.eof && self.next.is_none() && !self.seek_pending && self.error.is_empty(),
        )
    }
    fn display(&mut self, frame: Frame, ctx: &egui::Context) {
        if let Some(media) = &self.media {
            if let Some(display) = &mut self.gpu_display {
                display.begin_frame(frame.pts, ctx);
                let result = if let Some(gpu) = &frame.gpu {
                    Some(display.show(gpu, media.width, media.height))
                } else if media.alpha || display.effects_enabled() {
                    Some(display.show_rgba(&frame.rgba, media.width, media.height))
                } else {
                    // A disabled filter must not leave its last output on screen.
                    display.clear();
                    None
                };
                if let Some(result) = result {
                    if let Err(e) = result {
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
        let refresh_ai = self
            .gpu_display
            .as_mut()
            .is_some_and(|display| display.poll_ai());
        let previous_pts = self.raw.as_ref().map(|f| f.pts);
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
            self.clock.sync(position, std::time::Instant::now());
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
        if refresh_ai
            && !self.seek_pending
            && self.pending.is_none()
            && self.raw.as_ref().map(|f| f.pts) == previous_pts
        {
            if let Some(frame) = self.raw.take() {
                self.display(frame, ctx);
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
            || self
                .gpu_display
                .as_ref()
                .is_some_and(|display| display.ai.pending())
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
    fn titlebar(ctx: &egui::Context) {
        egui::TopBottomPanel::top("title")
            .exact_height(30.0)
            .frame(egui::Frame::new().fill(BG))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let (drag_rect, drag) = ui.allocate_exact_size(
                        Vec2::new((ui.available_width() - 108.0).max(0.0), 30.0),
                        egui::Sense::click_and_drag(),
                    );
                    let logo_id = egui::Id::new("app_logo");
                    let logo = ctx.data(|d| d.get_temp::<egui::TextureHandle>(logo_id))
                        .unwrap_or_else(|| {
                            let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/app-icon.png"))
                                .expect("embedded application icon");
                            let image = egui::ColorImage::from_rgba_unmultiplied(
                                [icon.width as usize, icon.height as usize], &icon.rgba);
                            let texture = ctx.load_texture("app_logo", image, egui::TextureOptions::LINEAR);
                            ctx.data_mut(|d| d.insert_temp(logo_id, texture.clone()));
                            texture
                        });
                    ui.painter().image(logo.id(), egui::Rect::from_center_size(
                        egui::pos2(drag_rect.left() + 17.0, drag_rect.center().y), Vec2::splat(26.0)),
                        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)), Color32::WHITE);
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
    fn controls(&mut self, ui: &mut egui::Ui) -> egui::Response {
        let ctx = &ui.ctx().clone();
        egui::TopBottomPanel::bottom(egui::Id::new(("transport", self.id)))
            .exact_height(72.0)
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .show_inside(ui, |ui| {
                let compact = ui.available_width() < 330.0;
                ui.spacing_mut().item_spacing.x = 5.0;
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
                    let right_width = if compact { 77.0 } else { 146.0 };
                    let status_width = (ui.available_width() - right_width - 5.0).max(1.0);
                    ui.allocate_ui_with_layout(Vec2::new(status_width, 30.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        egui::ScrollArea::horizontal()
                            .id_salt(("status", self.id))
                            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing = Vec2::splat(5.0);
                                    status_badge(ui, Color32::from_rgb(80, 170, 240), "clock", "视频时间",
                                        if compact { time(self.timeline_position()) } else { format!("{} / {}", time(self.timeline_position()), time(duration)) })
                                        .on_hover_text(format!("播放时间 / 总时长：{} / {}", time(self.timeline_position()), time(duration)));
                                    if let Some(m) = &self.media {
                                        let pts = self.raw.as_ref().map_or(0.0, |f| f.pts);
                                        status_badge(ui, Color32::from_rgb(80, 200, 145), "film", "视频帧数", frame_label(pts, m.fps, m.frame_count).trim_start_matches("视频帧数 ").into())
                                            .on_hover_text(format!("当前帧号 / 末帧号，均从 0 开始。{}\n当前帧号仍按显示帧时间戳和平均帧率估算，可变帧率或非零视频起点可能有偏差。总帧数采用文件标注；未标注时不猜测末帧号。", m.frame_count.map_or_else(|| "总帧数未知。".into(), |n| format!("共 {n} 帧。"))));
                                    }
                                    let stats = ctx.data(|d| d.get_temp::<FrameStats>(egui::Id::new("frame_stats"))).unwrap_or_default();
                                    status_badge(ui, Color32::from_rgb(235, 180, 80), "timer", "播放器帧时间",
                                        stats.display.map_or_else(|| "-- ms".into(), |(ms, _, _)| format!("{ms:.2} ms")))
                                        .on_hover_text(stats.display.map_or_else(|| "等待帧时间统计".into(), |(ms, _, cpu_ms)| format!(
                                            "播放器帧时间：{ms:.2} ms\nCPU 更新与渲染提交：{cpu_ms:.2} ms\nGPU 执行耗时：未单独采集\n帧时间为相邻界面帧的实际间隔，包含垂直同步、限帧及空闲等待；不是 CPU 与 GPU 耗时之和，也不是屏幕呈现延迟。每秒显示一次平均值。")));
                                    status_badge(ui, Color32::from_rgb(185, 140, 245), "monitor", "播放器帧率",
                                        stats.display.map_or_else(|| "-- FPS".into(), |(_, fps, _)| format!("{fps:.1} FPS")))
                                        .on_hover_text("最近一秒的软件界面实际刷新频率，每秒更新；不是视频素材帧率，暂停时按需刷新。");
                                });
                            });
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon(ui, "fullscreen", "全屏 · F11").clicked() {
                            toggle_fullscreen(ctx);
                        }
                        if !compact {
                            ui.spacing_mut().slider_width = 64.0;
                            ui.add(egui::Slider::new(&mut self.volume, 0.0..=1.0).show_value(false))
                                .on_hover_text("音量");
                        }
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
            }).response
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
                self.open_request = Some(path);
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
        if let Some(display) = &mut self.gpu_display {
            let mut changed = ui
                .add_enabled(
                    !display.ai.settings.enabled,
                    egui::Checkbox::new(&mut display.beauty, "美颜（磨皮 / 美白）"),
                )
                .on_hover_text("按肤色估计处理范围；启用 AI 后改用 AI 菜单中的皮肤磨皮 / 美白设置")
                .changed();
            ui.menu_button("视频滤镜", |ui| {
                let filters = &mut display.filters;
                let before = *filters;
                ui.horizontal(|ui| {
                    if ui.button("重置").clicked() {
                        *filters = Default::default();
                    }
                    if ui.button("黑白").clicked() {
                        *filters = crate::gpu::Filters {
                            saturation: 0.0,
                            ..Default::default()
                        };
                    }
                    if ui.button("复古").clicked() {
                        *filters = crate::gpu::Filters {
                            sepia: 1.0,
                            ..Default::default()
                        };
                    }
                });
                ui.add(egui::Slider::new(&mut filters.exposure, -2.0..=2.0).text("曝光 EV"));
                ui.add(egui::Slider::new(&mut filters.contrast, 0.0..=2.0).text("对比度"));
                ui.add(egui::Slider::new(&mut filters.saturation, 0.0..=2.0).text("饱和度"));
                ui.add(egui::Slider::new(&mut filters.sepia, 0.0..=1.0).text("复古"));
                ui.add(egui::Slider::new(&mut filters.vignette, 0.0..=1.0).text("暗角"));
                ui.checkbox(&mut filters.invert, "反色");
                changed |= *filters != before;
            });
            ui.menu_button("AI 美颜 / 美型 / 美体", |ui| {
                let settings = &mut display.ai.settings;
                let before = *settings;
                ui.checkbox(&mut settings.enabled, "启用 AI 人像美化");
                ui.add_enabled_ui(settings.enabled, |ui| {
                    ui.add(egui::Slider::new(&mut settings.slim_face, 0.0..=1.0).text("瘦脸"));
                    ui.add(egui::Slider::new(&mut settings.eyes, 0.0..=1.0).text("大眼"));
                    ui.add(egui::Slider::new(&mut settings.chin, -1.0..=1.0).text("下巴"));
                    ui.add(egui::Slider::new(&mut settings.smooth, 0.0..=1.0).text("皮肤磨皮"));
                    ui.add(egui::Slider::new(&mut settings.white, 0.0..=1.0).text("皮肤美白"));
                    ui.separator();
                    ui.add(egui::Slider::new(&mut settings.waist, 0.0..=1.0).text("瘦腰"));
                    ui.add(egui::Slider::new(&mut settings.slim_legs, 0.0..=1.0).text("瘦腿"));
                    ui.add(egui::Slider::new(&mut settings.long_legs, 0.0..=1.0).text("长腿"));
                });
                ui.label("单人模式；身体关键点清晰可见时生效");
                if !display.ai.status.is_empty() {
                    ui.label(&display.ai.status);
                }
                if ui.button("恢复默认").clicked() {
                    *settings = Default::default();
                }
                changed |= *settings != before;
            });
            if changed {
                if let Some(frame) = self.raw.take() {
                    self.display(frame, ctx);
                }
                ctx.request_repaint();
            }
        }
        if ui
            .button(if ctx.input(|i| i.viewport().fullscreen.unwrap_or(false)) {
                "退出全屏                    F11"
            } else {
                "全屏                            F11"
            })
            .clicked()
        {
            toggle_fullscreen(ctx);
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
        let response =
            egui::Modal::new(egui::Id::new(("video_information", self.id))).show(ctx, |ui| {
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
                                        row(
                                            "当前帧 PTS（相对起点）",
                                            format!("{:.6} s", frame.pts),
                                        );
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
        egui::Window::new("打开视频（新标签）")
            .id(egui::Id::new(("browser", self.id)))
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
            self.open_request = Some(path);
        }
    }
}

fn status_badge(ui: &mut egui::Ui, color: Color32, kind: &str, label: &str, text: String) -> egui::Response {
    let galley = ui.fonts(|fonts| {
        fonts.layout_no_wrap(text.clone(), egui::FontId::proportional(12.0), color)
    });
    // Use the same 30-point slot as transport buttons; paint everything about its center.
    let (rect, response) = ui.allocate_exact_size(Vec2::new(galley.size().x + 35.0, 30.0), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, format!("{label} {text}")));
    let painter = ui.painter();
    painter.rect_stroke(rect.shrink2(Vec2::new(0.0, 3.0)), 3.0, Stroke::new(1.0_f32, color), egui::StrokeKind::Inside);
    painter.galley(egui::pos2(rect.left() + 28.0, rect.center().y - galley.size().y / 2.0), galley, color);
    // Lucide 0.468.0 SVG geometry, rendered natively at 16 x 16 (assets/lucide).
    let origin = egui::pos2(rect.left() + 7.0, rect.center().y - 8.0);
    let point = |x: f32, y: f32| origin + Vec2::new(x, y) * (16.0 / 24.0);
    let stroke = Stroke::new(4.0_f32 / 3.0, color);
    let line = |a: (f32, f32), b: (f32, f32)| { painter.line_segment([point(a.0, a.1), point(b.0, b.1)], stroke); };
    match kind {
        "clock" => {
            painter.circle_stroke(point(12.0, 12.0), 20.0 / 3.0, stroke);
            line((12.0, 6.0), (12.0, 12.0));
            line((12.0, 12.0), (16.0, 14.0));
        }
        "film" => {
            painter.rect_stroke(egui::Rect::from_min_max(point(3.0, 3.0), point(21.0, 21.0)), 1.0, stroke, egui::StrokeKind::Middle);
            line((7.0, 3.0), (7.0, 21.0));
            line((17.0, 3.0), (17.0, 21.0));
            line((3.0, 12.0), (21.0, 12.0));
            for y in [7.5, 16.5] {
                line((3.0, y), (7.0, y));
                line((17.0, y), (21.0, y));
            }
        }
        "timer" => {
            line((10.0, 2.0), (14.0, 2.0));
            line((12.0, 14.0), (15.0, 11.0));
            painter.circle_stroke(point(12.0, 14.0), 16.0 / 3.0, stroke);
        }
        "monitor" => {
            painter.rect_stroke(egui::Rect::from_min_max(point(2.0, 3.0), point(22.0, 17.0)), 1.0, stroke, egui::StrokeKind::Middle);
            line((8.0, 21.0), (16.0, 21.0));
            line((12.0, 17.0), (12.0, 21.0));
        }
        _ => {}
    }
    response.on_hover_text(label)
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
        || format!("视频帧数 {current}"),
        |last| format!("视频帧数 {current} / {last}"),
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

pub struct App {
    dock: egui_dock::DockState<Player>,
    active: u64,
    next_id: u64,
    close_requests: Vec<u64>,
    render_state: Option<eframe::egui_wgpu::RenderState>,
    // Keep the shared output stream alive until all player sinks have been dropped.
    output: Option<(OutputStream, OutputStreamHandle)>,
    recent: Vec<PathBuf>,
}

impl App {
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
        // Use one font for Chinese and digits so their baselines and metrics agree.
        // Load the installed font without redistributing it.
        if let Ok(bytes) = fs::read("C:/Windows/Fonts/msyh.ttc") {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("cjk".into(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".into());
            cc.egui_ctx.set_fonts(fonts);
        }
        let output = OutputStream::try_default().ok();
        let state = cc.wgpu_render_state.clone();
        let player = Player::new(0, state.clone(), output.as_ref().map(|(_, h)| h.clone()));
        let mut app = Self {
            dock: egui_dock::DockState::new(vec![player]),
            active: 0,
            next_id: 1,
            close_requests: Vec::new(),
            render_state: state,
            output,
            recent: Vec::new(),
        };
        for path in std::env::args_os().skip(1) {
            app.open(PathBuf::from(path), &cc.egui_ctx);
        }
        app
    }

    fn active_player(&mut self) -> Option<&mut Player> {
        self.dock
            .iter_all_tabs_mut()
            .map(|(_, p)| p)
            .find(|p| p.id == self.active)
    }

    fn open(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.recent.retain(|p| p != &path);
        self.recent.insert(0, path.clone());
        self.recent.truncate(12);
        // An empty starter tab is reusable; opening a file never replaces a video.
        let empty = self
            .dock
            .iter_all_tabs()
            .find(|(_, p)| p.path.is_none())
            .map(|(_, p)| p.id);
        if let Some(id) = empty {
            let player = self
                .dock
                .iter_all_tabs_mut()
                .find(|(_, p)| p.id == id)
                .unwrap()
                .1;
            self.active = player.id;
            player.open(path, 0.0, true, ctx.clone());
        } else {
            let mut player = Player::new(
                self.next_id,
                self.render_state.clone(),
                self.output.as_ref().map(|(_, h)| h.clone()),
            );
            self.next_id += 1;
            self.active = player.id;
            player.open(path, 0.0, true, ctx.clone());
            self.dock.push_to_focused_leaf(player);
        }
        for (_, player) in self.dock.iter_all_tabs_mut() {
            player.recent.clone_from(&self.recent);
        }
        let location = self
            .dock
            .iter_all_tabs()
            .find(|(_, p)| p.id == self.active)
            .and_then(|((surface, node), _)| {
                self.dock[surface][node]
                    .iter_tabs()
                    .position(|p| p.id == self.active)
                    .map(|index| (surface, node, egui_dock::TabIndex(index)))
            });
        if let Some((surface, node, tab)) = location {
            self.dock.set_active_tab((surface, node, tab));
            self.dock.set_focused_node_and_surface((surface, node));
        }
        ctx.request_repaint();
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.stop_audio();
    }
}

fn toggle_fullscreen(ctx: &egui::Context) {
    let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
    ctx.send_viewport_cmd(ViewportCommand::Fullscreen(!fullscreen));
}

fn fullscreen_controls_visible(ctx: &egui::Context, fullscreen: bool, interacting: bool) -> bool {
    let id = egui::Id::new("fullscreen_controls_until");
    if !fullscreen {
        ctx.data_mut(|d| d.remove::<f64>(id));
        return true;
    }
    let (now, active) = ctx.input(|i| (i.time,
        i.events.iter().any(|e| matches!(e, egui::Event::PointerMoved(_))) || i.pointer.any_down()));
    let until = ctx.data_mut(|d| {
        if active || interacting { d.insert_temp(id, now + 2.0); }
        d.get_temp::<f64>(id).unwrap_or(now)
    });
    if now < until {
        ctx.request_repaint_after(Duration::from_secs_f64(until - now));
        true
    } else {
        false
    }
}

impl Player {
    fn canvas(&mut self, ui: &mut egui::Ui, selected: bool) -> egui::Response {
        let ctx = &ui.ctx().clone();
        let rect = ui.available_rect_before_wrap();
        let canvas = ui.interact(rect, ui.id().with("canvas"), egui::Sense::click());
        canvas.context_menu(|ui| self.context_menu(ui, ctx));
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
                            ui.label(RichText::new("无法播放此视频").color(Color32::LIGHT_RED))
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
                            ui.label(RichText::new("或将文件拖到这里").size(12.0).color(MUTED));
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
            toggle_fullscreen(ctx);
        } else if canvas.clicked() && selected && self.media.is_some() {
            self.toggle(ctx);
        }
        canvas
    }
}

struct Viewer<'a> {
    active: &'a mut u64,
    close_requests: &'a mut Vec<u64>,
}
impl egui_dock::TabViewer for Viewer<'_> {
    type Tab = Player;
    fn title(&mut self, tab: &mut Player) -> egui::WidgetText {
        tab.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map_or_else(|| "打开视频".into(), |n| n.to_string_lossy().into_owned())
            .into()
    }
    fn id(&mut self, tab: &mut Player) -> egui::Id {
        egui::Id::new(("player", tab.id))
    }
    fn on_tab_button(&mut self, tab: &mut Player, response: &egui::Response) {
        if response.clicked() || response.drag_started() {
            *self.active = tab.id;
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Player) {
        let selected = *self.active == tab.id;
        let pane_rect = ui.max_rect();
        let controls = tab.controls(ui);
        if controls.contains_pointer() && ui.input(|i| i.pointer.any_pressed()) {
            *self.active = tab.id;
        }
        let response = tab.canvas(ui, selected);
        if response.clicked() || response.secondary_clicked() {
            *self.active = tab.id;
        }
        if *self.active == tab.id {
            ui.painter().rect_stroke(
                pane_rect,
                0.0,
                Stroke::new(1.0_f32, ACCENT),
                egui::StrokeKind::Inside,
            );
        }
    }
    fn on_close(&mut self, tab: &mut Player) -> bool {
        // This frame may already reference the tab's GPU texture. Drop next frame.
        self.close_requests.push(tab.id);
        false
    }
    fn scroll_bars(&self, _: &Player) -> [bool; 2] {
        [false, false]
    }
    fn allowed_in_windows(&self, _: &mut Player) -> bool {
        false
    }
}

#[derive(Clone, Default)]
struct FrameStats {
    elapsed: f64,
    frames: u64,
    cpu_seconds: f64,
    // Frame interval (ms), application FPS, CPU time (ms), from the same window.
    display: Option<(f64, f64, f64)>,
}

impl FrameStats {
    fn record(&mut self, dt: f32, cpu_seconds: f32) {
        if !dt.is_finite() || dt <= 0.0 || !cpu_seconds.is_finite() || cpu_seconds < 0.0 {
            return;
        }
        self.elapsed += f64::from(dt);
        self.frames += 1;
        self.cpu_seconds += f64::from(cpu_seconds);
        if self.elapsed >= 1.0 {
            self.display = Some((
                self.elapsed * 1000.0 / self.frames as f64,
                self.frames as f64 / self.elapsed,
                self.cpu_seconds * 1000.0 / self.frames as f64,
            ));
            self.elapsed = 0.0;
            self.frames = 0;
            self.cpu_seconds = 0.0;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if let Some(seconds) = frame.info().cpu_usage {
            let dt = ctx.input(|i| i.unstable_dt);
            ctx.data_mut(|d| {
                d.get_temp_mut_or_default::<FrameStats>(egui::Id::new("frame_stats"))
                    .record(dt, seconds);
            });
        }
        self.update_ui(ctx);
    }
}

impl App {
    fn update_ui(&mut self, ctx: &egui::Context) {
        for id in self.close_requests.drain(..) {
            let location = self
                .dock
                .iter_all_tabs()
                .find(|(_, p)| p.id == id)
                .and_then(|((surface, node), _)| {
                    self.dock[surface][node]
                        .iter_tabs()
                        .position(|p| p.id == id)
                        .map(|index| (surface, node, egui_dock::TabIndex(index)))
                });
            if let Some(location) = location {
                self.dock.remove_tab(location);
            }
        }
        // Poll every tab, including tabs hidden behind another tab.
        for (_, player) in self.dock.iter_all_tabs_mut() {
            player.poll(ctx);
        }
        if self.active_player().is_none() {
            self.active = self
                .dock
                .find_active_focused()
                .map(|(_, p)| p.id)
                .or_else(|| self.dock.iter_all_tabs().next().map(|(_, p)| p.id))
                .unwrap_or(0);
        }
        let modal = self
            .dock
            .iter_all_tabs()
            .any(|(_, p)| p.browser || p.information);
        if !modal {
            for file in ctx.input(|i| i.raw.dropped_files.clone()) {
                if let Some(path) = file.path {
                    self.open(path, ctx);
                }
            }
        }
        let mut browse = false;
        if !ctx.wants_keyboard_input() && !modal {
            if let Some(player) = self.active_player() {
                if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
                    player.toggle(ctx);
                }
                if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                    player.next_frame();
                }
                if ctx.input(|i| {
                    i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::Backspace)
                }) {
                    player.previous_frame(ctx);
                }
            }
            browse = ctx.input(|i| i.key_pressed(egui::Key::O) && i.modifiers.ctrl);
            if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
                toggle_fullscreen(ctx);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
            }
        }
        let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        let show_controls = fullscreen_controls_visible(ctx, fullscreen,
            modal || ctx.memory(|m| m.any_popup_open()));
        ctx.send_viewport_cmd(ViewportCommand::CursorVisible(!fullscreen || show_controls));
        if !fullscreen {
            Player::titlebar(ctx);
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(if fullscreen { Color32::BLACK } else { BG }).inner_margin(0))
            .show(ctx, |ui| {
                if fullscreen && self.active_player().is_some() {
                    let screen = ui.max_rect();
                    let player = self.active_player().unwrap();
                    // Fullscreen bypasses docking without changing the saved tab/split layout.
                    player.canvas(ui, true);
                    if show_controls {
                        egui::Area::new(egui::Id::new(("fullscreen_controls", player.id)))
                            .order(egui::Order::Foreground)
                            .fixed_pos(egui::pos2(screen.left(), screen.bottom() - 72.0))
                            .show(ctx, |ui| {
                                ui.set_width(screen.width());
                                ui.set_height(72.0);
                                player.controls(ui);
                            });
                    } else {
                        ctx.set_cursor_icon(egui::CursorIcon::None);
                    }
                } else if self.dock.iter_all_tabs().next().is_none() {
                    ui.centered_and_justified(|ui| {
                        browse |= ui.button("打开视频或拖入多个文件").clicked();
                    });
                } else {
                    egui_dock::DockArea::new(&mut self.dock)
                        .show_leaf_close_all_buttons(false)
                        .show_leaf_collapse_buttons(false)
                        .style(egui_dock::Style::from_egui(ui.style()))
                        .show_inside(
                            ui,
                            &mut Viewer {
                                active: &mut self.active,
                                close_requests: &mut self.close_requests,
                            },
                        );
                }
            });
        if !self.close_requests.is_empty() {
            ctx.request_repaint();
        }
        if browse {
            if self.active_player().is_none() {
                let mut player = Player::new(
                    self.next_id,
                    self.render_state.clone(),
                    self.output.as_ref().map(|(_, h)| h.clone()),
                );
                self.active = self.next_id;
                self.next_id += 1;
                player.recent.clone_from(&self.recent);
                self.dock.push_to_first_leaf(player);
            }
            if let Some(player) = self.active_player() {
                player.browse();
            }
        }
        let mut requests = Vec::new();
        for (_, player) in self.dock.iter_all_tabs_mut() {
            player.browser_ui(ctx);
            player.information_ui(ctx);
            if let Some(path) = player.open_request.take() {
                requests.push(path);
            }
        }
        for path in requests {
            self.open(path, ctx);
        }
        if !modal && !fullscreen && !ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
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
    fn fullscreen_controls_hide_wake_and_preserve_layout() {
        let ctx = egui::Context::default();
        let mut app = dock_app();
        let mut run = |time: f64, fullscreen: bool, events: Vec<egui::Event>| {
            let mut input = egui::RawInput {
                time: Some(time),
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1280.0, 800.0))),
                events,
                ..Default::default()
            };
            input.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().fullscreen = Some(fullscreen);
            ctx.run(input, |ctx| app.update_ui(ctx))
        };
        let hidden = run(0.0, true, vec![]);
        assert!(hidden.viewport_output[&egui::ViewportId::ROOT].commands.contains(&ViewportCommand::CursorVisible(false)));
        assert!(egui::containers::panel::PanelState::load(&ctx, egui::Id::new(("transport", 0u64))).is_none());
        let shown = run(0.1, true, vec![egui::Event::PointerMoved(egui::pos2(100.0, 100.0))]);
        assert!(shown.viewport_output[&egui::ViewportId::ROOT].commands.contains(&ViewportCommand::CursorVisible(true)));
        assert!(egui::containers::panel::PanelState::load(&ctx, egui::Id::new(("transport", 0u64))).is_some());
        let hidden = run(2.2, true, vec![]);
        assert!(hidden.viewport_output[&egui::ViewportId::ROOT].commands.contains(&ViewportCommand::CursorVisible(false)));
        let shown = run(2.3, true, vec![egui::Event::PointerMoved(egui::pos2(200.0, 100.0))]);
        assert!(shown.viewport_output[&egui::ViewportId::ROOT].commands.contains(&ViewportCommand::CursorVisible(true)));
        run(3.0, false, vec![]);
        assert!(ctx.data(|d| d.get_temp::<f64>(egui::Id::new("fullscreen_controls_until"))).is_none());
        assert_eq!(app.dock.iter_all_tabs().count(), 1);
        assert_eq!(app.active, 0);
    }

    #[test]
    fn transport_icons_and_badges_share_centerline() {
        fn collect(shape: &egui::Shape, buttons: &mut Vec<f32>, badges: &mut Vec<f32>) {
            match shape {
                egui::Shape::Vec(shapes) => {
                    for shape in shapes { collect(shape, buttons, badges); }
                }
                egui::Shape::Path(path) if path.closed && path.points.len() == 3 => {
                    buttons.push(egui::Rect::from_points(&path.points).center().y);
                }
                egui::Shape::Rect(rect) if rect.rect.height() == 24.0 => {
                    badges.push(rect.rect.center().y);
                }
                _ => {}
            }
        }
        for scale in [1.0, 1.5, 2.0] {
            for width in [320.0, 1280.0] {
                let ctx = egui::Context::default();
                ctx.set_pixels_per_point(scale);
                let mut player = Player::new(0, None, None);
                let output = ctx.run(egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(width, 300.0))),
                    ..Default::default()
                }, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| { player.controls(ui); });
                });
                let (mut buttons, mut badges) = (Vec::new(), Vec::new());
                for shape in output.shapes { collect(&shape.shape, &mut buttons, &mut badges); }
                assert_eq!(buttons.len(), 2);
                assert!(!badges.is_empty());
                for y in badges {
                    assert!((y - buttons[0]).abs() <= 0.5, "width={width}, scale={scale}: badge {y}, button {}", buttons[0]);
                }
            }
        }
    }

    #[test]
    fn frame_stats_publish_once_per_second() {
        let mut stats = FrameStats::default();
        for _ in 0..3 {
            stats.record(0.25, 0.002);
            assert!(stats.display.is_none());
        }
        stats.record(0.25, 0.006);
        let (ms, fps, cpu_ms) = stats.display.unwrap();
        assert_eq!(ms, 250.0);
        assert_eq!(ms * fps, 1000.0);
        assert!((cpu_ms - 3.0).abs() < 0.001);
        assert_eq!(fps, 4.0);
        let previous = stats.display;
        stats.record(0.5, 0.010);
        assert_eq!(stats.display, previous);
        stats.record(0.5, 0.010);
        let (ms, fps, cpu_ms) = stats.display.unwrap();
        assert_eq!(ms, 500.0);
        assert_eq!(ms * fps, 1000.0);
        assert!((cpu_ms - 10.0).abs() < 0.001);
        assert_eq!(fps, 2.0);
        stats.record(0.0, 0.0);
        stats.record(f32::NAN, 0.0);
        assert_eq!(stats.frames, 0);
    }

    #[test]
    fn status_badges_spacing_and_wrap() {
        let ctx = egui::Context::default();
        for width in [800.0, 260.0] {
            let mut rects = Vec::new();
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(width, 300.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::splat(5.0);
                            for text in [
                                "00:00:07.833 / 00:00:07.833",
                                "234 / 234",
                                "1.25 ms",
                                "60.0 FPS",
                            ] {
                                rects.push(status_badge(ui, ACCENT, "clock", "视频时间", text.into()).rect);
                            }
                        });
                    });
                },
            );
            for pair in rects.windows(2) {
                assert!(!pair[0].intersects(pair[1]));
                let gap = if (pair[0].top() - pair[1].top()).abs() < 0.1 {
                    pair[1].left() - pair[0].right()
                } else {
                    pair[1].top() - pair[0].bottom()
                };
                assert!((gap - 5.0).abs() < 0.1, "badge gap: {gap}");
            }
            assert!(rects.iter().all(|r| r.right() <= width));
            if width < 300.0 {
                assert!(rects.last().unwrap().top() > rects[0].top());
            }
        }
    }

    fn dock_app() -> App {
        App {
            dock: egui_dock::DockState::new(vec![Player::new(0, None, None)]),
            active: 0,
            next_id: 1,
            close_requests: Vec::new(),
            render_state: None,
            output: None,
            recent: Vec::new(),
        }
    }

    fn draw(app: &mut App, ctx: &egui::Context, events: Vec<egui::Event>) {
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| app.update_ui(ctx),
        );
    }

    #[test]
    fn dock_batch_layout_ids_and_close() {
        let ctx = egui::Context::default();
        let mut app = dock_app();
        draw(&mut app, &ctx, vec![]);
        // Invalid media exercises independent loading/errors without test assets.
        for _ in 0..4 {
            app.open(PathBuf::from("Cargo.toml"), &ctx);
        }
        draw(&mut app, &ctx, vec![]);
        assert_eq!(
            app.dock
                .iter_all_nodes()
                .filter(|(_, n)| n.is_leaf())
                .count(),
            1
        );
        assert_eq!(app.dock.find_active_focused().unwrap().1.id, 3);
        // Each tab fills the same region until explicitly split by the user.
        for index in 0..4 {
            app.dock.set_active_tab((
                egui_dock::SurfaceIndex::main(),
                egui_dock::NodeIndex(0),
                egui_dock::TabIndex(index),
            ));
            draw(&mut app, &ctx, vec![]);
        }
        let tabs: Vec<_> = app
            .dock
            .iter_all_tabs()
            .map(|((s, n), p)| (p.id, app.dock[s][n].rect().unwrap()))
            .collect();
        assert_eq!(tabs.len(), 4);
        for (i, (id, rect)) in tabs.iter().enumerate() {
            assert!(rect.width() > 300.0 && rect.height() > 200.0, "{rect:?}");
            assert!(!tabs[..i].iter().any(|(other, _)| id == other));
            let controls =
                egui::containers::panel::PanelState::load(&ctx, egui::Id::new(("transport", *id)))
                    .unwrap()
                    .rect;
            assert!(
                rect.contains_rect(controls),
                "each video's controls must stay in its own pane"
            );
            assert!(
                (controls.height() - 72.0).abs() < 0.1,
                "timeline and controls must be visible"
            );
        }
        let surface = egui_dock::SurfaceIndex::main();
        let root = egui_dock::NodeIndex(0);
        let moved = app
            .dock
            .remove_tab((surface, root, egui_dock::TabIndex(3)))
            .unwrap();
        let [_, right] = app.dock.split(
            (surface, root),
            egui_dock::Split::Right,
            0.5,
            egui_dock::Node::leaf(moved),
        );
        app.open(PathBuf::from("Cargo.toml"), &ctx);
        draw(&mut app, &ctx, vec![]);
        assert_eq!(
            app.dock
                .iter_all_nodes()
                .filter(|(_, n)| n.is_leaf())
                .count(),
            2
        );
        assert_eq!(
            app.dock[surface][right].tabs_count(),
            2,
            "opening in a split adds a tab to that group"
        );
        let generations: Vec<_> = app
            .dock
            .iter_all_tabs()
            .map(|(_, p)| (p.generation.clone(), p.generation.load(Ordering::Relaxed)))
            .collect();
        for (_, player) in app.dock.iter_all_tabs_mut() {
            let mut viewer = Viewer {
                active: &mut app.active,
                close_requests: &mut app.close_requests,
            };
            assert!(!egui_dock::TabViewer::on_close(&mut viewer, player));
        }
        assert_eq!(app.dock.iter_all_tabs().count(), 5);
        draw(&mut app, &ctx, vec![]);
        assert_eq!(app.dock.iter_all_tabs().count(), 0);
        for (generation, before) in generations {
            assert!(generation.load(Ordering::Relaxed) > before);
        }
        draw(&mut app, &ctx, vec![]);
        app.open(PathBuf::from("Cargo.toml"), &ctx);
        draw(&mut app, &ctx, vec![]);
        assert_eq!(app.dock.iter_all_tabs().count(), 1);
        assert!(app.active_player().is_some());
    }

    #[test]
    #[ignore = "requires generated test-media/h264.mp4 and vp9-alpha.webm"]
    fn real_dock_independent_playback() {
        let ctx = egui::Context::default();
        let mut app = dock_app();
        draw(&mut app, &ctx, vec![]);
        app.open(PathBuf::from("test-media/h264.mp4"), &ctx);
        app.open(PathBuf::from("test-media/vp9-alpha.webm"), &ctx);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while app
            .dock
            .iter_all_tabs()
            .any(|(_, p)| p.raw.as_ref().is_none_or(|f| f.pts < 0.1))
        {
            draw(&mut app, &ctx, vec![]);
            for (_, p) in app.dock.iter_all_tabs() {
                assert!(p.error.is_empty(), "{}", p.error);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "both videos must advance"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let ids: Vec<_> = app
            .dock
            .iter_all_tabs()
            .map(|(_, p)| p.texture.as_ref().unwrap().id())
            .collect();
        assert_ne!(ids[0], ids[1], "video textures must be independent");
        // Space affects only the selected video.
        draw(
            &mut app,
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::Space,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(!app.active_player().unwrap().playing);
        assert!(
            app.dock
                .iter_all_tabs()
                .find(|(_, p)| p.id == 0)
                .unwrap()
                .1
                .playing
        );
        let other_before = app
            .dock
            .iter_all_tabs()
            .find(|(_, p)| p.id == 0)
            .unwrap()
            .1
            .raw
            .as_ref()
            .unwrap()
            .pts;
        app.active_player().unwrap().seek(0.8, false, &ctx);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while app.active_player().unwrap().seek_pending {
            draw(&mut app, &ctx, vec![]);
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(app.active_player().unwrap().raw.as_ref().unwrap().pts >= 0.8 - 0.000001);
        // Merge into a tab stack: the hidden player must still advance.
        let ((surface, node), _) = app.dock.iter_all_tabs().find(|(_, p)| p.id == 0).unwrap();
        let first = app
            .dock
            .remove_tab((surface, node, egui_dock::TabIndex(0)))
            .unwrap();
        app.dock.push_to_first_leaf(first);
        let ((surface, node), _) = app.dock.iter_all_tabs().next().unwrap();
        app.dock
            .set_active_tab((surface, node, egui_dock::TabIndex(0)));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while app
            .dock
            .iter_all_tabs()
            .find(|(_, p)| p.id == 0)
            .unwrap()
            .1
            .raw
            .as_ref()
            .unwrap()
            .pts
            <= other_before + 0.1
        {
            draw(&mut app, &ctx, vec![]);
            assert!(
                std::time::Instant::now() < deadline,
                "hidden tab must advance"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let closed = app
            .dock
            .remove_tab((surface, node, egui_dock::TabIndex(0)))
            .unwrap();
        let generation = closed.generation.clone();
        let before = generation.load(Ordering::Relaxed);
        drop(closed);
        assert!(generation.load(Ordering::Relaxed) > before);
        draw(&mut app, &ctx, vec![]);
        assert_eq!(app.dock.iter_all_tabs().count(), 1);
        assert!(app.active_player().unwrap().playing);
    }

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
        assert_eq!(frame_label(0.0, 30.0, Some(60)), "视频帧数 0 / 59");
        assert_eq!(frame_label(59.0 / 30.0, 30.0, Some(60)), "视频帧数 59 / 59");
        assert_eq!(frame_label(2.0 / 30.0, 30.0, Some(3)), "视频帧数 2 / 2");
        assert_eq!(frame_label(0.0, 30.0, None), "视频帧数 0");
        assert_eq!(frame_label(0.0, 30.0, Some(1)), "视频帧数 0 / 0");
        assert_eq!(frame_label(0.0, 30.0, Some(0)), "视频帧数 0");
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
