use std::sync::{
    mpsc::{self, Receiver, SyncSender},
    Arc,
};
use std::{
    ffi::{c_char, c_void, CStr},
    os::windows::ffi::OsStrExt,
    path::PathBuf,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub slim_face: f32,
    pub eyes: f32,
    pub chin: f32,
    pub waist: f32,
    pub slim_legs: f32,
    pub long_legs: f32,
    pub smooth: f32,
    pub white: f32,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            slim_face: 0.25,
            eyes: 0.2,
            chin: 0.0,
            waist: 0.0,
            slim_legs: 0.0,
            long_legs: 0.0,
            smooth: 0.4,
            white: 0.15,
        }
    }
}
impl Settings {
    pub fn flags(self) -> u32 {
        if !self.enabled {
            return 0;
        }
        let face = self.slim_face != 0.0
            || self.eyes != 0.0
            || self.chin != 0.0
            || self.smooth != 0.0
            || self.white != 0.0;
        let body = self.waist != 0.0 || self.slim_legs != 0.0 || self.long_legs != 0.0;
        u32::from(face)
            | (u32::from(body) << 1)
            | (u32::from(body || self.smooth != 0.0 || self.white != 0.0) << 2)
    }
}

pub struct Input {
    pub rgba: Arc<Vec<u8>>,
    pub size: [u32; 2],
    pub epoch: u64,
    pub pts: f64,
}
pub struct Output {
    pub epoch: u64,
    pub pts: f64,
    pub elapsed_ms: f32,
    pub data: Result<Box<Detection>, String>,
}
pub struct Worker {
    tx: SyncSender<Input>,
    rx: Receiver<Output>,
    pub busy: bool,
    pub failed: bool,
    pub last_pts: Option<f64>,
}
impl Worker {
    pub fn new(flags: u32, ctx: eframe::egui::Context) -> Self {
        let (tx, inputs) = mpsc::sync_channel::<Input>(1);
        let (outputs, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut engine = None;
            let mut epoch = u64::MAX;
            let mut timestamp = -1;
            let mut previous: Option<(f64, Box<Detection>)> = None;
            while let Ok(input) = inputs.recv() {
                if input.epoch != epoch {
                    engine = None;
                    previous = None;
                    timestamp = -1;
                    epoch = input.epoch;
                }
                let start = std::time::Instant::now();
                let data = (|| {
                    if engine.is_none() {
                        engine = Some(Engine::new(flags)?);
                    }
                    timestamp = ((input.pts * 1000.0).round() as i64).max(timestamp + 1);
                    let mut detection =
                        engine
                            .as_mut()
                            .unwrap()
                            .run(&input.rgba, input.size, timestamp)?;
                    if let Some((pts, old)) = &previous {
                        let dt = input.pts - pts;
                        if dt > 0.0 && dt < 0.2 {
                            smooth_points(
                                &mut detection.face,
                                detection.face_count,
                                &old.face,
                                old.face_count,
                                dt,
                            );
                            smooth_points(
                                &mut detection.pose,
                                detection.pose_count,
                                &old.pose,
                                old.pose_count,
                                dt,
                            );
                        }
                    }
                    if flags & 3 != 0 {
                        previous = Some((input.pts, detection.clone()));
                    }
                    Ok(detection)
                })();
                if outputs
                    .send(Output {
                        epoch,
                        pts: input.pts,
                        elapsed_ms: start.elapsed().as_secs_f32() * 1000.0,
                        data,
                    })
                    .is_err()
                {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self {
            tx,
            rx,
            busy: false,
            failed: false,
            last_pts: None,
        }
    }
    pub fn wants(&self, pts: f64) -> bool {
        !self.busy && !self.failed && self.last_pts != Some(pts)
    }
    pub fn submit(&mut self, input: Input) {
        if self.wants(input.pts) {
            let pts = input.pts;
            if self.tx.try_send(input).is_ok() {
                self.busy = true;
                self.last_pts = Some(pts);
            }
        }
    }
    pub fn poll(&mut self) -> Option<Output> {
        let result = self.rx.try_recv().ok()?;
        self.busy = false;
        self.failed = result.data.is_err();
        Some(result)
    }
    pub fn reset(&mut self) {
        self.last_pts = None;
        self.failed = false;
    }
}

fn smooth_points<const N: usize>(
    current: &mut [[f32; 4]; N],
    count: u32,
    previous: &[[f32; 4]; N],
    old_count: u32,
    dt: f64,
) {
    if count == 0 || count != old_count {
        return;
    }
    // Large movement / a cut should reacquire immediately, not smear old geometry.
    let movement = current
        .iter()
        .zip(previous)
        .map(|(a, b)| ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt())
        .sum::<f32>()
        / count as f32;
    if movement > 0.06 {
        return;
    }
    let weight = (1.0 - (-dt as f32 / 0.025).exp()).max((movement / 0.025).min(1.0));
    for (p, old) in current.iter_mut().zip(previous) {
        for i in 0..3 {
            p[i] = old[i] + (p[i] - old[i]) * weight;
        }
    }
}

unsafe extern "C" {
    fn nkg_ai_open(
        directory: *const u16,
        flags: u32,
        error: *mut c_char,
        capacity: i32,
    ) -> *mut c_void;
    fn nkg_ai_run(
        handle: *mut c_void,
        rgba: *const u8,
        width: i32,
        height: i32,
        timestamp: i64,
        output: *mut Detection,
        error: *mut c_char,
        capacity: i32,
    ) -> i32;
    fn nkg_ai_close(handle: *mut c_void);
}

#[repr(C)]
#[derive(Clone)]
pub struct Detection {
    pub face: [[f32; 4]; 478],
    pub pose: [[f32; 4]; 33],
    pub mask: [u8; 256 * 256 * 2],
    pub face_count: u32,
    pub pose_count: u32,
}
impl Default for Detection {
    fn default() -> Self {
        Self {
            face: [[0.0; 4]; 478],
            pose: [[0.0; 4]; 33],
            mask: [0; 256 * 256 * 2],
            face_count: 0,
            pose_count: 0,
        }
    }
}

struct Engine(*mut c_void);
impl Engine {
    fn new(flags: u32) -> Result<Self, String> {
        let dir = runtime_dir();
        let path: Vec<_> = dir.as_os_str().encode_wide().chain([0]).collect();
        let mut error = [0; 2048];
        let handle =
            unsafe { nkg_ai_open(path.as_ptr(), flags, error.as_mut_ptr(), error.len() as i32) };
        if handle.is_null() {
            Err(unsafe { CStr::from_ptr(error.as_ptr()) }
                .to_string_lossy()
                .into_owned())
        } else {
            Ok(Self(handle))
        }
    }
    fn run(
        &mut self,
        rgba: &[u8],
        size: [u32; 2],
        timestamp: i64,
    ) -> Result<Box<Detection>, String> {
        if size.contains(&0)
            || size.iter().any(|&v| v > 1024)
            || rgba.len() != size[0] as usize * size[1] as usize * 4
        {
            return Err("Invalid AI image".into());
        }
        let mut out = Box::<Detection>::default();
        let mut error = [0; 2048];
        let status = unsafe {
            nkg_ai_run(
                self.0,
                rgba.as_ptr(),
                size[0] as i32,
                size[1] as i32,
                timestamp,
                out.as_mut(),
                error.as_mut_ptr(),
                error.len() as i32,
            )
        };
        if status != 0 {
            return Err(unsafe { CStr::from_ptr(error.as_ptr()) }
                .to_string_lossy()
                .into_owned());
        }
        if out
            .face
            .iter()
            .chain(out.pose.iter())
            .flatten()
            .any(|v| !v.is_finite())
        {
            return Err("Non-finite AI landmarks".into());
        }
        Ok(out)
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { nkg_ai_close(self.0) };
    }
}
fn runtime_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("NKG_AI_DIR") {
        return path.into();
    }
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap().join("ai");
        if dir.join("mediapipe.dll").is_file() {
            return dir;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/mediapipe")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_and_tracking_reset() {
        let mut s = Settings::default();
        assert_eq!(s.flags(), 0);
        s.enabled = true;
        assert_eq!(s.flags(), 5);
        s.waist = 0.5;
        assert_eq!(s.flags(), 7);
        let old = [[0.5, 0.5, 0.0, 1.0]; 3];
        let mut small = [[0.501, 0.5, 0.0, 1.0]; 3];
        smooth_points(&mut small, 3, &old, 3, 0.016);
        assert!(small[0][0] > 0.5 && small[0][0] < 0.501);
        let mut cut = [[0.8, 0.5, 0.0, 1.0]; 3];
        smooth_points(&mut cut, 3, &old, 3, 0.016);
        assert_eq!(cut[0][0], 0.8);
        let mut missing = [[0.0; 4]; 3];
        smooth_points(&mut missing, 0, &old, 3, 0.016);
        assert_eq!(missing, [[0.0; 4]; 3]);
    }
    #[test]
    #[ignore = "requires scripts/setup-ai.ps1 and test-media/ai/pose.rgba"]
    fn native_models_detect_person() {
        let mut engine = Engine::new(7).unwrap();
        let pixels = std::fs::read("test-media/ai/pose.rgba").unwrap();
        for i in 0..6 {
            let start = std::time::Instant::now();
            let result = engine.run(&pixels, [640, 426], i * 33).unwrap();
            println!(
                "AI {:?}: face={} pose={} skin={} person={}",
                start.elapsed(),
                result.face_count,
                result.pose_count,
                result.mask.chunks_exact(2).filter(|p| p[0] > 128).count(),
                result.mask.chunks_exact(2).filter(|p| p[1] > 128).count()
            );
            assert_eq!(result.pose_count, 33);
            assert!(result.mask.chunks_exact(2).filter(|p| p[0] > 128).count() > 50);
            assert!(result.mask.chunks_exact(2).filter(|p| p[1] > 128).count() > 1000);
        }
        let blank = vec![0; 640 * 426 * 4];
        let result = engine.run(&blank, [640, 426], 1000).unwrap();
        assert_eq!(result.face_count, 0);
        assert_eq!(result.pose_count, 0);
        assert!(engine.run(&[], [640, 426], 1001).is_err());
        for flags in [1, 2, 4] {
            let mut engine = Engine::new(flags).unwrap();
            let (pixels, size) = if flags == 1 {
                (
                    std::fs::read("test-media/ai/portrait.rgba").unwrap(),
                    [426, 640],
                )
            } else {
                (pixels.clone(), [640, 426])
            };
            for i in 0..5 {
                let start = std::time::Instant::now();
                let result = engine.run(&pixels, size, i * 33).unwrap();
                println!("flags={flags} {:?}", start.elapsed());
                if flags == 1 {
                    assert_eq!(result.face_count, 478);
                }
            }
        }
    }
}
