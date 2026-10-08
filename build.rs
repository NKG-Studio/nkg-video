use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo:rerun-if-changed=assets/app-icon.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/app-icon.ico")
            .set("ProductName", "NKG Video Player")
            .set("FileDescription", "NKG Video Player")
            .set("OriginalFilename", "nkg-video.exe")
            .compile()
            .expect("embed Windows application icon");
    }
    println!("cargo:rerun-if-changed=src/ai.cpp");
    println!("cargo:rerun-if-changed=tools/mediapipe/include");
    assert!(
        PathBuf::from(
            "tools/mediapipe/include/mediapipe/tasks/c/vision/face_landmarker/face_landmarker.h"
        )
        .is_file(),
        "Run scripts/setup-ai.ps1 first (MediaPipe C headers missing)"
    );
    cc::Build::new()
        .cpp(true)
        .file("src/ai.cpp")
        .include("tools/mediapipe/include")
        .flag_if_supported("/std:c++17")
        .flag_if_supported("/EHsc")
        .compile("nkg_ai");
    println!("cargo:rerun-if-changed=src/decode.c");
    println!("cargo:rerun-if-changed=src/gpu.cpp");
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    let sdk = env::var_os("FFMPEG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/ffmpeg-shared"));
    assert!(
        sdk.join("include/libavcodec/avcodec.h").is_file(),
        "Run scripts/setup-ffmpeg-native.ps1 first (FFmpeg shared SDK missing)"
    );
    cc::Build::new()
        .file("src/decode.c")
        .include(sdk.join("include"))
        .flag_if_supported("/std:c11")
        .compile("nkg_decode");
    cc::Build::new()
        .cpp(true)
        .file("src/gpu.cpp")
        .include(sdk.join("include"))
        .flag_if_supported("/std:c++17")
        .flag_if_supported("/EHsc")
        .compile("nkg_gpu");
    for name in ["d3d11", "d3d12", "dxgi"] {
        println!("cargo:rustc-link-lib={name}");
    }
    println!(
        "cargo:rustc-link-search=native={}",
        sdk.join("lib").display()
    );
    for name in ["avformat", "avcodec", "avutil", "swscale", "swresample"] {
        println!("cargo:rustc-link-lib=dylib={name}");
    }
    // Windows resolves linked DLLs beside the executable, including cargo test executables.
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let profile = out.ancestors().nth(3).unwrap();
    for entry in fs::read_dir(sdk.join("bin")).expect("FFmpeg bin directory") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|s| s == "dll")
            && [
                "avformat-",
                "avcodec-",
                "avutil-",
                "swscale-",
                "swresample-",
            ]
            .iter()
            .any(|prefix| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(prefix)
            })
        {
            for dest in [profile.to_path_buf(), profile.join("deps")] {
                fs::create_dir_all(&dest).unwrap();
                let target = dest.join(path.file_name().unwrap());
                if !target.exists() {
                    fs::copy(&path, &target).unwrap();
                }
            }
        }
    }
    fs::copy(sdk.join("LICENSE"), profile.join("FFmpeg-LICENSE.txt")).unwrap();
}
