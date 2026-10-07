use crate::media::{Frame, Media};
use std::{
    collections::VecDeque,
    ffi::{c_char, c_int, c_void, CStr, CString},
    ptr,
    sync::{Arc, Mutex},
};

unsafe extern "C" {
    fn nkg_open(
        path: *const c_char,
        stream: c_int,
        hardware: c_int,
        decoder: *const c_char,
        rotation: c_int,
        gpu_device: *mut c_void,
        error: *mut c_char,
        len: c_int,
    ) -> *mut c_void;
    fn nkg_close(decoder: *mut c_void);
    fn nkg_error(decoder: *mut c_void) -> *const c_char;
    fn nkg_seek(decoder: *mut c_void, seconds: f64) -> c_int;
    fn nkg_previous(decoder: *mut c_void, before: f64) -> c_int;
    fn nkg_last(decoder: *mut c_void, duration: f64) -> c_int;
    fn nkg_gpu_next(
        decoder: *mut c_void,
        frame: *mut *mut c_void,
        width: *mut c_int,
        height: *mut c_int,
        pts: *mut f64,
    ) -> c_int;
    fn nkg_video_next(
        decoder: *mut c_void,
        pixels: *mut u8,
        bytes: usize,
        width: *mut c_int,
        height: *mut c_int,
        pts: *mut f64,
    ) -> c_int;
    fn nkg_audio_next(
        decoder: *mut c_void,
        samples: *mut *const f32,
        count: *mut c_int,
        pts: *mut f64,
    ) -> c_int;
}

// Constructed and used only inside its owning worker; never shared between threads.
pub struct Decoder {
    handle: *mut c_void,
    width: usize,
    height: usize,
    audio_cursor: f64,
    pending_audio: Option<(Vec<f32>, f64)>,
    history: VecDeque<Frame>,
    replay: Option<usize>,
    pub prefetch: Option<Arc<Mutex<VecDeque<Frame>>>>,
    resume_after: Option<f64>,
    gpu: bool,
    spare: Vec<u8>,
}
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            nkg_close(self.handle);
        }
    }
}
impl Decoder {
    pub fn open(media: &Media, hardware: bool, audio: bool) -> Result<Self, String> {
        let path = CString::new(media.path.to_str().ok_or("Non-UTF8 media path")?)
            .map_err(|e| e.to_string())?;
        let codec = if !audio && media.alpha {
            match media.codec.as_str() {
                "vp9" => "libvpx-vp9",
                "vp8" => "libvpx",
                _ => "",
            }
        } else {
            ""
        };
        let codec = CString::new(codec).unwrap();
        let stream = if audio {
            media.audio_index.ok_or("No audio stream")?
        } else {
            media.video_index
        };
        let stream = i32::try_from(stream).map_err(|_| "Invalid stream index")?;
        let mut error = [0i8; 512];
        // Native open copies strings and either returns an owned handle or frees all partial resources.
        let handle = unsafe {
            nkg_open(
                path.as_ptr(),
                stream,
                i32::from(hardware),
                codec.as_ptr(),
                media.rotation,
                if hardware && !audio {
                    media
                        .gpu_device
                        .as_ref()
                        .map_or(ptr::null_mut(), |d| d.raw())
                } else {
                    ptr::null_mut()
                },
                error.as_mut_ptr(),
                error.len() as i32,
            )
        };
        if handle.is_null() {
            return Err(unsafe { CStr::from_ptr(error.as_ptr()) }
                .to_string_lossy()
                .into_owned());
        }
        Ok(Self {
            handle,
            width: media.width,
            height: media.height,
            audio_cursor: 0.0,
            pending_audio: None,
            history: VecDeque::new(),
            replay: None,
            prefetch: None,
            resume_after: None,
            gpu: hardware && !audio && media.gpu_device.is_some(),
            spare: Vec::new(),
        })
    }
    fn result(&self, code: i32) -> Result<i32, String> {
        if code < 0 {
            Err(unsafe { CStr::from_ptr(nkg_error(self.handle)) }
                .to_string_lossy()
                .into_owned())
        } else {
            Ok(code)
        }
    }
    pub fn seek(&mut self, position: f64) -> Result<(), String> {
        self.result(unsafe { nkg_seek(self.handle, position) })?;
        self.history.clear();
        self.replay = None;
        self.resume_after = None;
        self.audio_cursor = position;
        self.pending_audio = None;
        Ok(())
    }
    pub fn previous(&mut self, before: f64) -> Result<(), String> {
        // Adjacent entries are consecutive decoded frames, including VFR. An
        // arbitrary seek clears history so gaps can never masquerade as neighbors.
        if let Some(index) = self
            .history
            .iter()
            .position(|f| (f.pts - before).abs() < 0.000001)
        {
            if index > 0 {
                self.replay = Some(index - 1);
                return Ok(());
            }
        }
        self.history.clear();
        self.replay = None;
        self.resume_after = None;
        if let Some(cache) = &self.prefetch {
            let cache = cache.lock().unwrap();
            if let Some(index) = cache.iter().position(|f| (f.pts - before).abs() < 0.000001) {
                if index > 0 {
                    self.history = cache.clone();
                    self.replay = Some(index - 1);
                    self.resume_after = self.history.back().map(|f| f.pts);
                    return Ok(());
                }
            }
        }
        self.result(unsafe { nkg_previous(self.handle, before) })?;
        Ok(())
    }
    pub fn last(&mut self, duration: f64) -> Result<(), String> {
        self.history.clear();
        self.replay = None;
        self.resume_after = None;
        self.result(unsafe { nkg_last(self.handle, duration) })?;
        Ok(())
    }
    pub fn video(&mut self) -> Result<Option<Frame>, String> {
        if let Some(index) = self.replay {
            if let Some(frame) = self.history.get(index) {
                self.replay = Some(index + 1);
                return Ok(Some(frame.clone()));
            }
            self.replay = None;
        }
        if let Some(pts) = self.resume_after.take() {
            // Imported frames belong to the other decoder. Rejoin our native
            // stream strictly after their last PTS when forward replay ends.
            self.result(unsafe { nkg_seek(self.handle, pts + 0.000002) })?;
        }
        if self.gpu {
            let (mut handle, mut w, mut h, mut pts) = (ptr::null_mut(), 0, 0, 0.0);
            if self.result(unsafe {
                nkg_gpu_next(self.handle, &mut handle, &mut w, &mut h, &mut pts)
            })? == 0
            {
                return Ok(None);
            }
            if handle.is_null() {
                return Err("Invalid GPU frame".into());
            }
            let gpu = Arc::new(unsafe { crate::gpu::Frame::from_raw(handle) });
            if w as usize != self.width || h as usize != self.height || !pts.is_finite() {
                return Err("Decoded frame dimensions or timestamp changed unexpectedly".into());
            }
            let frame = Frame {
                rgba: Arc::new(Vec::new()),
                gpu: Some(gpu),
                pts,
            };
            let capacity = (128 * 1024 * 1024 / (self.width * self.height * 4)).min(120);
            while !self.history.is_empty() && self.history.len() >= capacity {
                self.history.pop_front();
            }
            if capacity > 0 {
                self.history.push_back(frame.clone());
            }
            return Ok(Some(frame));
        }
        let (mut w, mut h, mut pts) = (0, 0, 0.0);
        let bytes = self.width * self.height * 4;
        self.spare.resize(bytes, 0);
        let result = unsafe {
            nkg_video_next(
                self.handle,
                self.spare.as_mut_ptr(),
                bytes,
                &mut w,
                &mut h,
                &mut pts,
            )
        };
        if self.result(result)? == 0 {
            return Ok(None);
        }
        if w as usize != self.width || h as usize != self.height || !pts.is_finite() {
            return Err("Decoded frame dimensions or timestamp changed unexpectedly".into());
        }
        // FFmpeg converts directly into an exclusively owned Rust buffer. Recycle only
        // evicted pixels with no display/prefetch references, never overwrite cached frames.
        let capacity = (128 * 1024 * 1024 / bytes).min(120);
        let reusable = if !self.history.is_empty() && self.history.len() >= capacity {
            self.history
                .pop_front()
                .and_then(|old| Arc::try_unwrap(old.rgba).ok())
        } else {
            None
        };
        let rgba = std::mem::replace(&mut self.spare, reusable.unwrap_or_default());
        let frame = Frame {
            gpu: None,
            rgba: Arc::new(rgba),
            pts,
        };
        // ponytail: retain at most 128 MiB / 120 frames; older reverse steps
        // fall back to seeking. Share pixels with the display queue, never copy.
        while !self.history.is_empty() && self.history.len() >= capacity {
            self.history.pop_front();
        }
        if capacity > 0 {
            self.history.push_back(frame.clone());
        }
        Ok(Some(frame))
    }
    pub fn audio(&mut self) -> Result<Option<Vec<f32>>, String> {
        let (samples, pts) = if let Some(pending) = self.pending_audio.take() {
            pending
        } else {
            let (mut data, mut count, mut pts) = (ptr::null(), 0, 0.0);
            if self
                .result(unsafe { nkg_audio_next(self.handle, &mut data, &mut count, &mut pts) })?
                == 0
            {
                return Ok(None);
            }
            if data.is_null()
                || !(1..=960000).contains(&count)
                || count % 2 != 0
                || !pts.is_finite()
            {
                return Err("Invalid decoded audio block".into());
            }
            (
                unsafe { std::slice::from_raw_parts(data, count as usize) }.to_vec(),
                pts,
            )
        };
        // Preserve audio stream offsets and gaps without allocating unbounded silence.
        let gap = ((pts - self.audio_cursor) * 48000.0).round().max(0.0) as usize;
        if gap > 0 {
            let frames = gap.min(48000);
            self.pending_audio = Some((samples, pts));
            self.audio_cursor += frames as f64 / 48000.0;
            return Ok(Some(vec![0.0; frames * 2]));
        }
        self.audio_cursor = pts + samples.len() as f64 / 96000.0;
        Ok(Some(samples))
    }
}

#[cfg(test)]
mod reverse_benchmark {
    #[test]
    #[ignore = "compare software RGBA against FFmpeg CLI; optional NKG_BENCH_VIDEO"]
    fn software_rgba_matches_ffmpeg() {
        use std::io::Read;
        let path = std::env::var_os("NKG_BENCH_VIDEO")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "test-media/h264.mp4".into());
        let media = crate::media::Media::probe(&path).unwrap();
        let mut decoder = super::Decoder::open(&media, false, false).unwrap();
        let mut command = crate::media::command("ffmpeg");
        command.args(["-v", "error"]);
        if media.alpha && (media.codec == "vp8" || media.codec == "vp9") {
            command.args([
                "-c:v",
                if media.codec == "vp8" {
                    "libvpx"
                } else {
                    "libvpx-vp9"
                },
            ]);
        }
        let mut process = command
            .arg("-i")
            .arg(&path)
            .args([
                "-map",
                "0:v:0",
                "-frames:v",
                "300",
                "-an",
                "-sn",
                "-dn",
                "-fps_mode",
                "passthrough",
                "-pix_fmt",
                "rgba",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = process.stdout.take().unwrap();
        let mut expected = vec![0; media.width * media.height * 4];
        let mut count = 0;
        while let Some(frame) = decoder.video().unwrap() {
            output.read_exact(&mut expected).unwrap();
            assert!(
                frame.rgba.as_slice() == expected,
                "RGBA mismatch at frame {count}"
            );
            count += 1;
            if count == 300 {
                break;
            }
        }
        assert_eq!(output.read(&mut [0]).unwrap(), 0);
        assert!(process.wait().unwrap().success());
        assert!(count > 0);
        println!("{count} software frames exactly match FFmpeg RGBA");
    }
    #[test]
    #[ignore = "software stage profile; optional NKG_BENCH_VIDEO"]
    fn software_stage_profile() {
        unsafe extern "C" {
            fn nkg_video_profile(
                decoder: *mut std::ffi::c_void,
                decode: *mut i64,
                convert: *mut i64,
                read: *mut i64,
            );
        }
        let path = std::env::var_os("NKG_BENCH_VIDEO")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "test-media/prores-alpha.mov".into());
        let media = crate::media::Media::probe(&path).unwrap();
        let mut decoder = super::Decoder::open(&media, false, false).unwrap();
        let (mut decode, mut convert, mut read) = (0, 0, 0);
        unsafe { nkg_video_profile(decoder.handle, &mut decode, &mut convert, &mut read) };
        let mut native = std::time::Duration::ZERO;
        let mut prepare = std::time::Duration::ZERO;
        let mut image = eframe::egui::ColorImage {
            size: [0, 0],
            pixels: Vec::new(),
        };
        let mut frames = 0;
        loop {
            let start = std::time::Instant::now();
            let Some(frame) = decoder.video().unwrap() else {
                break;
            };
            native += start.elapsed();
            let start = std::time::Instant::now();
            frame.write_image(&media, &mut image);
            std::hint::black_box(&image);
            prepare += start.elapsed();
            frames += 1;
            if frames == 300 {
                break;
            }
        }
        unsafe { nkg_video_profile(decoder.handle, &mut decode, &mut convert, &mut read) };
        assert!(frames > 0);
        println!("{frames} frames: read+decode={decode}us (av_read_frame={read}us) native-convert={convert}us Rust-frame={}us CPU-image={}us; sequential stages, excludes GPU upload/presentation",
            native.as_micros().saturating_sub((decode + convert) as u128), prepare.as_micros());
    }
    #[test]
    #[ignore = "cold reverse with image preparation; optional NKG_BENCH_VIDEO"]
    fn cold_reverse() {
        let path = std::env::var_os("NKG_BENCH_VIDEO")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "test-media/h264.mp4".into());
        let media = crate::media::Media::probe(&path).unwrap();
        let mut decoder = super::Decoder::open(&media, !media.alpha, false).unwrap();
        decoder.seek(media.duration * 0.8).unwrap();
        let mut frame = decoder.video().unwrap().unwrap();
        let mut image = eframe::egui::ColorImage {
            size: [0, 0],
            pixels: Vec::new(),
        };
        let mut times = Vec::new();
        for _ in 0..20 {
            if frame.pts <= 0.000001 {
                break;
            }
            let start = std::time::Instant::now();
            decoder.previous(frame.pts).unwrap();
            let previous = decoder.video().unwrap().unwrap();
            assert!(previous.pts < frame.pts);
            previous.write_image(&media, &mut image);
            times.push(start.elapsed().as_micros());
            frame = previous;
        }
        times.sort();
        println!(
            "cold reverse + image: {} steps median={}us p95={}us max={}us",
            times.len(),
            times[times.len() / 2],
            times[times.len() * 95 / 100],
            times.last().unwrap()
        );
    }
}
