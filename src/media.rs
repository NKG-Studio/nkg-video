use rodio::Source;
use serde_json::Value;
use std::{
    collections::VecDeque,
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, TryRecvError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

pub fn binary(name: &str) -> PathBuf {
    let filename = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.into()
    };
    if let Some(dir) = env::var_os("NKG_FFMPEG_DIR") {
        return PathBuf::from(dir).join(&filename);
    }
    if let Ok(exe) = env::current_exe() {
        for parent in exe.ancestors().skip(1).take(4) {
            for candidate in [
                parent.join(&filename),
                parent.join("tools/ffmpeg/bin").join(&filename),
            ] {
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    for dir in [
        env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf)),
        Some(PathBuf::from("tools/ffmpeg/bin")),
    ]
    .into_iter()
    .flatten()
    {
        let path = dir.join(&filename);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from(filename)
}

pub fn command(name: &str) -> Command {
    let mut cmd = Command::new(binary(name));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd.stdin(Stdio::null());
    cmd
}

#[derive(Clone, Debug)]
pub struct Media {
    pub gpu_device: Option<Arc<crate::gpu::Device>>,
    pub path: PathBuf,
    pub width: usize,
    pub height: usize,
    pub duration: f64,
    pub fps: f64,
    pub codec: String,
    pub alpha: bool,
    pub video_index: u64,
    pub audio_index: Option<u64>,
    pub pixel_format: String,
    pub frame_count: Option<u64>,
    pub details: Vec<(&'static str, String)>,
    pub rotation: i32,
    pub aspect: f32,
}

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|v| v.is_finite())
}
fn ratio(s: &str) -> Option<f64> {
    let (a, b) = s.split_once('/')?;
    let result = a.parse::<f64>().ok()? / b.parse::<f64>().ok()?;
    (result.is_finite() && result > 0.0).then_some(result)
}

impl Media {
    pub fn probe(path: &Path) -> Result<Self, String> {
        if !path.is_file() {
            return Err("The selected path is not a file.".into());
        }
        let output = command("ffprobe").args(["-v", "error", "-show_streams", "-show_format", "-of", "json"]).arg(path).output()
            .map_err(|e| format!("Cannot start ffprobe: {e}. Run scripts/setup-ffmpeg.ps1 or set NKG_FFMPEG_DIR."))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(1000)
                .collect());
        }
        Self::from_json(
            path,
            &serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?,
        )
    }
    fn from_json(path: &Path, value: &Value) -> Result<Self, String> {
        let streams = value["streams"]
            .as_array()
            .ok_or("No media streams found")?;
        let v = streams
            .iter()
            .find(|s| {
                s["codec_type"] == "video" && s["disposition"]["attached_pic"].as_u64() != Some(1)
            })
            .ok_or("No playable video stream")?;
        let mut width = v["width"].as_u64().ok_or("Missing video width")? as usize;
        let mut height = v["height"].as_u64().ok_or("Missing video height")? as usize;
        frame_size(width, height)?;
        let rotation = v["side_data_list"]
            .as_array()
            .and_then(|a| a.iter().find_map(|s| number(&s["rotation"])))
            .unwrap_or(0.0)
            .round() as i32;
        let rotation = rotation.rem_euclid(360);
        let sar = v["sample_aspect_ratio"]
            .as_str()
            .and_then(|s| ratio(&s.replace(':', "/")))
            .unwrap_or(1.0);
        let mut aspect = width as f64 * sar / height as f64;
        if rotation == 90 || rotation == 270 {
            std::mem::swap(&mut width, &mut height);
            aspect = 1.0 / aspect;
        }
        let codec = v["codec_name"].as_str().unwrap_or("unknown").to_owned();
        let pixel = v["pix_fmt"].as_str().unwrap_or("");
        let alpha = v["tags"]["alpha_mode"] == "1"
            || v["tags"]["ALPHA_MODE"] == "1"
            || pixel.contains("yuva")
            || pixel.contains("gbrap")
            || ["rgba", "bgra", "argb", "abgr", "ya"]
                .iter()
                .any(|p| pixel.starts_with(p));
        let format = &value["format"];
        let text = |s: &Value, key: &str| {
            s[key]
                .as_str()
                .filter(|v| !v.is_empty() && *v != "unknown" && *v != "N/A")
                .unwrap_or("未标注")
                .to_owned()
        };
        let bitrate = |s: &Value| {
            number(&s["bit_rate"])
                .filter(|n| *n > 0.0)
                .map_or_else(|| "未标注".into(), |n| format!("{:.2} kb/s", n / 1000.0))
        };
        let mut details = vec![
            ("容器", text(format, "format_long_name")),
            (
                "文件大小",
                number(&format["size"])
                    .map_or_else(|| "未知".into(), |n| format!("{:.2} MiB", n / 1048576.0)),
            ),
            ("总比特率", bitrate(format)),
            ("编码器", text(v, "codec_long_name")),
            ("编码 Profile", text(v, "profile")),
            ("视频比特率", bitrate(v)),
            ("颜色矩阵", text(v, "color_space")),
            ("色彩原色", text(v, "color_primaries")),
            ("传递特性", text(v, "color_transfer")),
            (
                "颜色范围",
                match v["color_range"].as_str() {
                    Some("tv") => "Limited / 有限范围".into(),
                    Some("pc") => "Full / 全范围".into(),
                    _ => text(v, "color_range"),
                },
            ),
            ("色度位置", text(v, "chroma_location")),
            (
                "位深（源标注）",
                number(&v["bits_per_raw_sample"])
                    .filter(|n| *n > 0.0)
                    .map_or_else(|| "未标注".into(), |n| format!("{n} bit")),
            ),
            ("像素宽高比", text(v, "sample_aspect_ratio")),
        ];
        if let Some(a) = streams.iter().find(|s| s["codec_type"] == "audio") {
            details.extend([
                ("音频编码", text(a, "codec_long_name")),
                ("音频比特率", bitrate(a)),
                (
                    "采样率",
                    number(&a["sample_rate"])
                        .map_or_else(|| "未标注".into(), |n| format!("{n} Hz")),
                ),
                (
                    "声道",
                    format!("{} · {}", a["channels"], text(a, "channel_layout")),
                ),
            ]);
        }
        Ok(Self {
            gpu_device: None,
            path: fs::canonicalize(path).map_err(|e| e.to_string())?,
            width,
            height,
            duration: number(&value["format"]["duration"])
                .or_else(|| number(&v["duration"]))
                .unwrap_or(0.0)
                .max(0.0),
            fps: v["avg_frame_rate"]
                .as_str()
                .and_then(ratio)
                .or_else(|| v["r_frame_rate"].as_str().and_then(ratio))
                .unwrap_or(30.0),
            codec,
            alpha,
            pixel_format: pixel.into(),
            frame_count: v["nb_frames"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| v["nb_frames"].as_u64())
                .filter(|n| *n > 0),
            details,
            rotation,
            aspect: aspect as f32,
            video_index: v["index"].as_u64().unwrap_or(0),
            audio_index: streams
                .iter()
                .find(|s| s["codec_type"] == "audio")
                .and_then(|s| s["index"].as_u64()),
        })
    }
}

fn frame_size(w: usize, h: usize) -> Result<usize, String> {
    w.checked_mul(h)
        .and_then(|n| n.checked_mul(4))
        .filter(|&n| n > 0 && n <= 256 * 1024 * 1024)
        .ok_or("Video dimensions exceed the 256 MiB/frame limit".into())
}

#[derive(Clone)]
pub struct Frame {
    pub gpu: Option<Arc<crate::gpu::Frame>>,
    pub rgba: Arc<Vec<u8>>,
    pub pts: f64,
}
impl Frame {
    pub fn write_image(&self, media: &Media, image: &mut eframe::egui::ColorImage) {
        let size = [media.width, media.height];
        image.size = size;
        image.pixels.resize(
            media.width * media.height,
            eframe::egui::Color32::TRANSPARENT,
        );
        if media.alpha {
            for (pixel, rgba) in image.pixels.iter_mut().zip(self.rgba.chunks_exact(4)) {
                *pixel = eframe::egui::Color32::from_rgba_unmultiplied(
                    rgba[0], rgba[1], rgba[2], rgba[3],
                );
            }
        } else {
            // FFmpeg's opaque RGBA has A=255; it is already premultiplied.
            bytemuck::cast_slice_mut(&mut image.pixels).copy_from_slice(&self.rgba);
        }
    }
}

struct Segment<T> {
    position: f64,
    previous: bool,
    last: bool,
    output: mpsc::SyncSender<Result<T, String>>,
}

fn segment<T>(position: f64, capacity: usize) -> (Segment<T>, Receiver<Result<T, String>>) {
    let (output, rx) = mpsc::sync_channel(capacity);
    (
        Segment {
            position,
            output,
            previous: false,
            last: false,
        },
        rx,
    )
}

// One worker and one decoder lifetime per stream. Replacing the output receiver
// invalidates every buffered old frame without copying or tagging pixel buffers.
fn worker<T: Send + 'static>(
    media: Media,
    hardware: bool,
    audio: bool,
    start: f64,
    prefetch: Option<Arc<Mutex<VecDeque<Frame>>>>,
    next: fn(&mut crate::native::Decoder) -> Result<Option<T>, String>,
) -> (mpsc::Sender<Segment<T>>, Receiver<Result<T, String>>) {
    let capacity = if audio { 8 } else { 3 };
    let (commands, requests) = mpsc::channel::<Segment<T>>();
    let (initial, frames) = segment(start, capacity);
    thread::spawn(move || {
        let mut decoder = match crate::native::Decoder::open(&media, hardware, audio) {
            Ok(d) => d,
            Err(e) => {
                let _ = initial.output.send(Err(e));
                return;
            }
        };
        decoder.prefetch = prefetch;
        let mut current = initial;
        'seek: loop {
            while let Ok(new) = requests.try_recv() {
                current = new;
            }
            let seek_result = if current.last {
                decoder.last(current.position)
            } else if current.previous {
                decoder.previous(current.position)
            } else {
                decoder.seek(current.position)
            };
            let mut failure = seek_result.err();
            'decode: loop {
                match requests.try_recv() {
                    Ok(new) => {
                        current = new;
                        continue 'seek;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => break 'seek,
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                let mut value = if let Some(e) = failure.take() {
                    Err(e)
                } else {
                    match next(&mut decoder) {
                        Ok(Some(frame)) => Ok(frame),
                        Ok(None) => break,
                        Err(e) => Err(e),
                    }
                };
                let failed = value.is_err();
                loop {
                    match current.output.try_send(value) {
                        Ok(()) => break,
                        Err(mpsc::TrySendError::Disconnected(_)) => break 'decode,
                        Err(mpsc::TrySendError::Full(v)) => {
                            value = v;
                            match requests.recv_timeout(Duration::from_millis(1)) {
                                Ok(new) => {
                                    current = new;
                                    continue 'seek;
                                }
                                Err(mpsc::RecvTimeoutError::Disconnected) => break 'seek,
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                            }
                        }
                    }
                }
                if failed {
                    break;
                }
            }
            drop(current); // EOF disconnects this segment's receiver, but keeps the decoder alive.
            match requests.recv() {
                Ok(new) => current = new,
                Err(_) => break,
            }
        }
    });
    (commands, frames)
}

struct Prefetch {
    frames: Arc<Mutex<VecDeque<Frame>>>,
    target: Arc<AtomicU64>,
    revision: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    wake: mpsc::SyncSender<()>,
    last: Option<f64>,
    threshold: f64,
}
impl Drop for Prefetch {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.revision.fetch_add(1, Ordering::Relaxed);
    }
}
impl Prefetch {
    fn cancel(&mut self) {
        self.enabled.store(false, Ordering::Release);
        self.revision.fetch_add(1, Ordering::Release);
        self.last = None;
        self.frames.lock().unwrap().clear();
    }
    fn new(media: Media, hardware: bool) -> Self {
        let frames = Arc::new(Mutex::new(VecDeque::new()));
        let target = Arc::new(AtomicU64::new(0));
        let revision = Arc::new(AtomicU64::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let enabled = Arc::new(AtomicBool::new(false));
        let (wake, requests) = mpsc::sync_channel(1);
        // One bounded block beside the active decoder's 128 MiB history.
        let capacity = (128 * 1024 * 1024 / (media.width * media.height * 4)).min(120);
        let threshold = (capacity / 4).max(1) as f64 / media.fps;
        let (cache, desired, version, stop) = (
            frames.clone(),
            target.clone(),
            revision.clone(),
            stopped.clone(),
        );
        let allowed = enabled.clone();
        thread::spawn(move || {
            let mut decoder = None;
            while requests.recv().is_ok() {
                if stop.load(Ordering::Relaxed) || capacity < 3 {
                    break;
                }
                if !allowed.load(Ordering::Acquire) {
                    continue;
                }
                let id = version.load(Ordering::Acquire);
                let position = f64::from_bits(desired.load(Ordering::Relaxed));
                if decoder.is_none() {
                    match crate::native::Decoder::open(&media, hardware, false) {
                        Ok(d) => decoder = Some(d),
                        Err(_) => break, // Optional cache cannot break foreground playback.
                    }
                }
                let decoder = decoder.as_mut().unwrap();
                let start = (position - (capacity * 3 / 4) as f64 / media.fps).max(0.0);
                if decoder.seek(start).is_err() {
                    continue;
                }
                let mut block = VecDeque::new();
                let mut ahead = 0;
                while ahead < (capacity / 4).max(1)
                    && allowed.load(Ordering::Acquire)
                    && version.load(Ordering::Acquire) == id
                    && !stop.load(Ordering::Relaxed)
                {
                    match decoder.video() {
                        Ok(Some(frame)) => {
                            if frame.pts >= position {
                                ahead += 1;
                            }
                            if block.len() == capacity {
                                block.pop_front();
                            }
                            block.push_back(frame);
                        }
                        _ => break,
                    }
                }
                if allowed.load(Ordering::Acquire)
                    && version.load(Ordering::Acquire) == id
                    && !stop.load(Ordering::Relaxed)
                {
                    *cache.lock().unwrap() = block;
                }
            }
        });
        Self {
            frames,
            target,
            revision,
            stopped,
            enabled,
            wake,
            last: None,
            threshold,
        }
    }
    fn request(&mut self, position: f64) {
        // Stepping must not repeatedly cancel a block before it can publish.
        // Explicit seeks still invalidate the generation via cancel().
        if self
            .last
            .is_some_and(|last| (last - position).abs() < self.threshold)
        {
            return;
        }
        self.last = Some(position);
        self.enabled.store(true, Ordering::Release);
        self.target.store(position.to_bits(), Ordering::Relaxed);
        let _ = self.wake.try_send(()); // Finish active block, then read newest target.
    }
}

pub struct Video {
    pub frames: Receiver<Result<Frame, String>>,
    commands: mpsc::Sender<Segment<Frame>>,
    prefetch: Prefetch,
}
impl Video {
    pub fn suspend_prefetch(&mut self) {
        if self.prefetch.enabled.swap(false, Ordering::AcqRel) {
            self.prefetch.revision.fetch_add(1, Ordering::Release);
            self.prefetch.last = None;
        }
    }
    pub fn last(&mut self, duration: f64) -> Result<(), String> {
        self.prefetch.cancel();
        let (mut request, rx) = segment(duration, 3);
        request.last = true;
        self.commands
            .send(request)
            .map_err(|_| "Video decoder stopped")?;
        self.frames = rx;
        Ok(())
    }
    pub fn previous(&mut self, before: f64) -> Result<(), String> {
        let (mut request, rx) = segment(before, 3);
        request.previous = true;
        self.commands
            .send(request)
            .map_err(|_| "Video decoder stopped")?;
        self.frames = rx;
        Ok(())
    }
    pub fn start(media: &Media, start: f64, hardware: bool) -> Result<Self, String> {
        frame_size(media.width, media.height)?;
        let prefetch = Prefetch::new(media.clone(), hardware);
        let (commands, frames) = worker(
            media.clone(),
            hardware,
            false,
            start,
            Some(prefetch.frames.clone()),
            crate::native::Decoder::video,
        );
        Ok(Self {
            frames,
            commands,
            prefetch,
        })
    }
    pub fn prefetch(&mut self, position: f64) {
        self.prefetch.request(position);
    }
    pub fn seek(&mut self, position: f64) -> Result<(), String> {
        self.prefetch.cancel();
        let (request, rx) = segment(position, 3);
        self.commands
            .send(request)
            .map_err(|_| "Video decoder stopped")?;
        self.frames = rx;
        Ok(())
    }
}

pub struct Audio {
    pub source: Option<Pcm>,
    pub played: Arc<AtomicU64>,
    pub error: Arc<Mutex<Option<String>>>,
    commands: mpsc::Sender<Segment<Vec<f32>>>,
}
pub struct Pcm {
    rx: Receiver<Result<Vec<f32>, String>>,
    chunk: std::vec::IntoIter<f32>,
    played: Arc<AtomicU64>,
    silence_left: usize,
    error: Arc<Mutex<Option<String>>>,
}
impl Iterator for Pcm {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if self.silence_left > 0 {
            self.silence_left -= 1;
            return Some(0.0);
        }
        if let Some(v) = self.chunk.next() {
            self.played.fetch_add(1, Ordering::Relaxed);
            return Some(v);
        }
        match self.rx.try_recv() {
            Ok(Ok(chunk)) => {
                self.chunk = chunk.into_iter();
                self.next()
            }
            Err(TryRecvError::Empty) => {
                self.silence_left = 1;
                Some(0.0)
            } // Complete a stereo frame on underrun.
            Ok(Err(e)) => {
                *self.error.lock().unwrap() = Some(e);
                None
            }
            Err(TryRecvError::Disconnected) => None,
        }
    }
}
impl Source for Pcm {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        2
    }
    fn sample_rate(&self) -> u32 {
        48000
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}
impl Audio {
    pub fn start(media: &Media, start: f64) -> Result<Self, String> {
        if media.audio_index.is_none() {
            return Err("No audio stream".into());
        }
        let (commands, rx) = worker(
            media.clone(),
            false,
            true,
            start,
            None,
            crate::native::Decoder::audio,
        );
        let played = Arc::new(AtomicU64::new(0));
        let error = Arc::new(Mutex::new(None));
        let source = Some(Pcm {
            rx,
            chunk: Vec::new().into_iter(),
            played: played.clone(),
            silence_left: 0,
            error: error.clone(),
        });
        Ok(Self {
            source,
            played,
            commands,
            error,
        })
    }
    pub fn seek(&mut self, position: f64) -> Result<(), String> {
        let (request, rx) = segment(position, 8);
        self.commands
            .send(request)
            .map_err(|_| "Audio decoder stopped")?;
        self.played = Arc::new(AtomicU64::new(0));
        self.error = Arc::new(Mutex::new(None));
        self.source = Some(Pcm {
            rx,
            chunk: Vec::new().into_iter(),
            played: self.played.clone(),
            silence_left: 0,
            error: self.error.clone(),
        });
        Ok(())
    }
}
pub struct Clock {
    position: f64,
    since: Option<Instant>,
}
impl Clock {
    pub fn new(position: f64) -> Self {
        Self {
            position,
            since: None,
        }
    }
    pub fn position(&self) -> f64 {
        self.position + self.since.map_or(0.0, |t| t.elapsed().as_secs_f64())
    }
    pub fn pause(&mut self) {
        self.position = self.position();
        self.since = None;
    }
    pub fn play(&mut self) {
        if self.since.is_none() {
            self.since = Some(Instant::now());
        }
    }
    pub fn set(&mut self, position: f64) {
        self.position = position;
        if self.since.is_some() {
            self.since = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "playback decode + UI image throughput; optional NKG_BENCH_VIDEO"]
    fn playback_throughput() {
        let path = env::var_os("NKG_BENCH_VIDEO")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("test-media/h264.mp4"));
        let media = Media::probe(&path).unwrap();
        let mut video = Video::start(&media, 0.0, !media.alpha).unwrap();
        let first = video
            .frames
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        video.prefetch(first.pts + 1.0);
        video.suspend_prefetch();
        assert!(!video.prefetch.enabled.load(Ordering::Acquire));
        let begin = Instant::now();
        let mut timings = Vec::new();
        let mut waits = Duration::ZERO;
        let mut images = Duration::ZERO;
        let mut image = eframe::egui::ColorImage {
            size: [0, 0],
            pixels: Vec::new(),
        };
        for _ in 0..300 {
            let start = Instant::now();
            let frame = match video.frames.recv_timeout(Duration::from_secs(5)) {
                Ok(frame) => frame.unwrap(),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(e) => panic!("{e}"),
            };
            waits += start.elapsed();
            let image_start = Instant::now();
            frame.write_image(&media, &mut image);
            std::hint::black_box(&image);
            images += image_start.elapsed();
            timings.push(start.elapsed().as_micros());
        }
        let elapsed = begin.elapsed();
        timings.sort();
        assert!(!timings.is_empty());
        println!("wait={waits:?} image={images:?}");
        println!("decode + prepare {} frames: {:?}, {:.1} fps, p95={} us max={} us; excludes presentation",timings.len(),elapsed,timings.len() as f64/elapsed.as_secs_f64(),timings[timings.len()*95/100],timings.last().unwrap());
        assert!(!video.prefetch.enabled.load(Ordering::Acquire));
    }
    #[test]
    #[ignore = "continuous reverse benchmark; optional NKG_BENCH_VIDEO"]
    fn continuous_reverse() {
        let path = env::var_os("NKG_BENCH_VIDEO")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("test-media/h264.mp4"));
        let media = Media::probe(&path).unwrap();
        let mut video = Video::start(&media, media.duration * 0.5, !media.alpha).unwrap();
        let mut frame = video
            .frames
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        video.prefetch(frame.pts);
        let revision = video.prefetch.revision.load(Ordering::Acquire);
        let deadline = Instant::now() + Duration::from_secs(5);
        while video.prefetch.frames.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let mut timings = Vec::new();
        for _ in 0..60 {
            if frame.pts <= 0.000001 {
                break;
            }
            let begin = Instant::now();
            video.previous(frame.pts).unwrap();
            let previous = video
                .frames
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert!(previous.pts < frame.pts);
            timings.push(begin.elapsed().as_micros());
            frame = previous;
            video.prefetch(frame.pts);
            assert_eq!(
                video.prefetch.revision.load(Ordering::Acquire),
                revision,
                "step cancelled pending prefetch"
            );
            thread::sleep(Duration::from_millis(50));
        }
        timings.sort();
        println!(
            "{} reverse steps: median={} us, p95={} us, max={} us; >50ms={}",
            timings.len(),
            timings[timings.len() / 2],
            timings[timings.len() * 95 / 100],
            timings.last().unwrap(),
            timings.iter().filter(|n| **n > 50000).count()
        );
    }
    #[test]
    #[ignore = "requires generated media and D3D11VA"]
    fn background_reverse_window() {
        for name in ["h264.mp4", "vfr.mp4", "vp9-alpha.webm"] {
            let media = Media::probe(&PathBuf::from("test-media").join(name)).unwrap();
            let mut reference = crate::native::Decoder::open(&media, !media.alpha, false).unwrap();
            reference.seek(0.0).unwrap();
            let mut expected = Vec::new();
            while let Some(frame) = reference.video().unwrap() {
                let end = frame.pts > 1.6;
                expected.push(frame);
                if end {
                    break;
                }
            }
            let index = expected.len() - 2;
            let mut video = Video::start(&media, expected[index].pts, !media.alpha).unwrap();
            let first = video
                .frames
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            video.prefetch(0.2);
            video.prefetch(first.pts); // supersede stale target
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let cache = video.prefetch.frames.lock().unwrap();
                let ready = cache.iter().any(|f| (f.pts - first.pts).abs() < 0.000001)
                    && cache
                        .iter()
                        .any(|f| (f.pts - expected[index - 1].pts).abs() < 0.000001);
                drop(cache);
                if ready {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{name}: background cache did not cover target"
                );
                thread::sleep(Duration::from_millis(5));
            }
            // Foreground decoder started at first. Its predecessor is ONLY in
            // the background cache; pointer identity proves no slow re-decode.
            let cached = video
                .prefetch
                .frames
                .lock()
                .unwrap()
                .iter()
                .find(|f| (f.pts - expected[index - 1].pts).abs() < 0.000001)
                .unwrap()
                .rgba
                .clone();
            let begin = Instant::now();
            video.previous(first.pts).unwrap();
            let previous = video
                .frames
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert!(Arc::ptr_eq(&previous.rgba, &cached));
            assert!(previous.rgba == expected[index - 1].rgba);
            println!("{name}: background cache reverse {:?}", begin.elapsed());
            // Replay must cross the imported block boundary into native decode.
            let mut last = previous.pts;
            for _ in 0..150 {
                match video.frames.recv_timeout(Duration::from_secs(3)) {
                    Ok(frame) => {
                        let frame = frame.unwrap();
                        assert!(frame.pts > last);
                        last = frame.pts;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(e) => panic!("{name}: {e}"),
                }
            }
        }
    }
    #[test]
    fn timestamps_alpha_and_limits() {
        assert!(frame_size(usize::MAX, 10).is_err());
        assert!(ratio("1/0").is_none());
        let path = env::current_exe().unwrap();
        let m = Media::from_json(&path, &serde_json::json!({"streams":[{"index":0,"codec_type":"video","width":32,"height":16,"codec_name":"vp9","tags":{"alpha_mode":"1"},"avg_frame_rate":"30000/1001"},{"index":1,"codec_type":"audio"}],"format":{"duration":"1.5"}})).unwrap();
        assert!(m.alpha);
        assert_eq!(m.audio_index, Some(1));
        assert!((m.fps - 29.97003).abs() < 0.001);
        let mut opaque = m.clone();
        opaque.alpha = false;
        let mut image = eframe::egui::ColorImage {
            size: [0, 0],
            pixels: Vec::new(),
        };
        for (media, pixel) in [(&opaque, [12, 100, 200, 255]), (&m, [12, 100, 200, 90])] {
            let frame = Frame {
                gpu: None,
                rgba: Arc::new(pixel.repeat(media.width * media.height)),
                pts: 0.0,
            };
            frame.write_image(media, &mut image);
            let expected = eframe::egui::ColorImage::from_rgba_unmultiplied(
                [media.width, media.height],
                &frame.rgba,
            );
            assert_eq!(image.pixels, expected.pixels);
        }
        let metadata = Media::from_json(&path, &serde_json::json!({"streams":[{"index":0,"codec_type":"video","width":32,"height":16,"nb_frames":"120","bit_rate":"1250000","color_space":"bt709","color_range":"tv"}],"format":{"bit_rate":"1400000"}})).unwrap();
        assert_eq!(metadata.frame_count, Some(120));
        assert!(metadata
            .details
            .contains(&("视频比特率", "1250.00 kb/s".into())));
        assert!(metadata
            .details
            .contains(&("总比特率", "1400.00 kb/s".into())));
        assert!(metadata.details.contains(&("颜色矩阵", "bt709".into())));
        assert!(metadata
            .details
            .contains(&("颜色范围", "Limited / 有限范围".into())));
        assert!(metadata.details.contains(&("传递特性", "未标注".into())));
        let (tx, rx) = mpsc::sync_channel(1);
        let played = Arc::new(AtomicU64::new(0));
        let mut pcm = Pcm {
            rx,
            chunk: vec![].into_iter(),
            silence_left: 0,
            played: played.clone(),
            error: Arc::new(Mutex::new(None)),
        };
        assert_eq!(pcm.next(), Some(0.0));
        assert_eq!(played.load(Ordering::Relaxed), 0);
        assert_eq!(pcm.next(), Some(0.0));
        tx.send(Ok(vec![0.25, -0.25])).unwrap();
        assert_eq!(pcm.next(), Some(0.25));
        assert_eq!(pcm.next(), Some(-0.25));
        drop(tx);
        assert_eq!(pcm.next(), None);
        assert_eq!(played.load(Ordering::Relaxed), 2);
        let mut clock = Clock::new(2.0);
        clock.pause();
        assert_eq!(clock.position(), 2.0);
    }
}
