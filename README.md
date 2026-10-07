# NKG Video

Windows 原生 Rust 视频播放器。FFmpeg 负责解封装和解码，Rust 管理帧、PTS、音频时钟、定位和显示；不使用 libmpv 或浏览器 video 元素。

## 启动

直接使用：[下载 Windows x64 Release](https://github.com/NKG-Studio/nkg-video/releases/latest)。解压整个压缩包后运行 `nkg-video.exe`，不要单独移动 exe。支持 Windows 10/11 x64，需要支持 Direct3D 12 的显卡及驱动；包内包含 FFmpeg、ffprobe 和 VC++ 运行库。

源码及发行版采用 GPL-3.0-or-later，第三方组件说明见 [THIRD_PARTY.md](THIRD_PARTY.md)。维护者可运行 `./scripts/package-release.ps1` 生成带依赖、许可证及 SHA-256 校验的便携包。

本机已准备 FFmpeg，可直接运行：

```powershell
cargo run --release
cargo run --release -- "D:\Videos\demo.mp4"
```

新环境需要 Rust MSVC 工具链和 FFmpeg：

```powershell
./scripts/setup-ffmpeg.ps1
./scripts/setup-ffmpeg-native.ps1
cargo run --release
```

构建还需要 Visual Studio 的 C/C++ 工具链。`setup-ffmpeg-native.ps1` 下载固定版本 FFmpeg 8.1.2 shared SDK 并校验 SHA-256；`build.rs` 编译小型 C ABI 桥接层，将所需 DLL 和许可证放到可执行文件旁。不要只复制 exe，须携带旁边的 FFmpeg DLL。可用 `FFMPEG_DIR` 指定其他兼容的 SDK 目录。

打开文件时仅调用一次 ffprobe 获取元数据；可通过 `NKG_FFMPEG_DIR` 指定它的目录。视频、音频解码和定位全部在进程内执行，不再启动 ffmpeg 子进程。

## 已实现

- 深色、扁平的全自绘界面；主界面只保留视频、细进度条和播放控件，没有侧栏、菜单栏或技术状态栏。打开文件、最近文件、透明背景及媒体详情收在视频右键菜单中。
- 自定义最小化、最大化/还原、关闭、标题栏拖动与双击最大化，全屏和窗口边缘缩放。
- 文件打开、路径输入、文件拖放、会话内最近文件。
- 播放/暂停、进度定位、下一帧、重新播放、音量和静音；有音频时按已消费 PCM 样本同步视频，无音频时使用单调时钟。
- 时间后显示从 0 开始的当前帧号 / 末帧号，总帧数保留在悬浮提示和视频信息中，估算方式可悬停查看。右键「视频信息」打开独立模态窗口，包含颜色矩阵、原色、传递特性、范围、像素格式、比特率及音频参数；未标注的信息不推测。显示路径使用 /，隐藏 Windows 扩展路径前缀。支持关闭按钮、Esc 或点击遮罩关闭。
- FFmpeg 覆盖的主流本地容器和编码，包括 MP4、MKV、MOV、WebM、AVI、TS 等；具体硬解能力由显卡、驱动和编码 profile 决定。
- 优先 D3D11VA 硬解。通过解码器硬件配置及实际帧的 D3D11 格式验证硬解；首次打开硬解失败后才回退软件解码，在右键菜单「视频信息」中查看实际模式及回退原因。
- 透明 WebM VP8/VP9 使用 libvpx 保留 Alpha；ProRes 4444 等含 Alpha 格式用软件解码。棋盘格/黑色背景切换，原始 RGBA 帧独立保留。
- 视频右键菜单可勾选「美颜（磨皮 / 美白）」，默认关闭，会话内保留选择；暂停时切换也立即更新。使用 GPU 保边磨皮和轻度美白，兼容硬解、软解及透明视频，不改变原始帧和 Alpha。按肤色估计范围，也可能影响相近颜色的物体；不含人脸识别、瘦脸或 AI 修复。
- 使用真实帧 PTS，支持可变帧率；定位时替换整个帧接收器，旧任务无法覆盖新帧。
- 每个音视频流持有常驻解封装器和解码器，硬解设备及颜色转换器也会复用。跳转仅发送命令，执行 avformat_seek_file / avcodec_flush_buffers，替换有界帧队列；旧位置的数据无法进入新画面。到达 EOF 后仍保留解码器供重播。
- 精确跳转先补解码至目标时间，再进行 GPU 颜色转换；兼容路径才进行回读。音频独立定位、重置重采样器并做预滚及样本裁剪，保留音轨偏移与静音间隔。

快捷键：Space 播放/暂停，→ 下一帧，← / Backspace 上一帧（暂停），Ctrl+O 打开，F11 全屏，Esc 退出全屏。

时间显示精确到毫秒；帧号从 0 开始（60 帧对应 0–59），主界面显示「帧 0 / 59」至「帧 59 / 59」，两边均为帧号；悬浮提示和视频信息显示总共 60 帧。总帧数未标注时显示未知，不按时长猜测。确认 EOF 后，时间与进度条统一显示总时长；真实帧 PTS 保留在视频信息中。终点定位寻找真实尾帧，不再减去平均帧间隔。完整规则见 [TIMING.md](TIMING.md)。帧索引目前仍按平均帧率估算，可变帧率的限制可悬停查看。

## 验证

```powershell
cargo test
# 美颜 GPU 像素检查（需要 Direct3D 12）
cargo test beauty_preserves -- --ignored --nocapture
./scripts/make-test-media.ps1
cargo test real_decode -- --ignored --nocapture
cargo clippy --all-targets -- -D warnings
# 可选：指定本地视频，测量常驻解码器多点精确跳转
cargo test --release persistent_seek_matches -- --ignored --nocapture
$env:NKG_BENCH_VIDEO = "D:\Videos\demo.mp4"
cargo test --release seek_latency -- --ignored --nocapture
```

集成测试生成并验证 H.264、HEVC、AV1、VP9、MPEG-4、VP8/VP9 Alpha、ProRes 4444、VFR，以及定位、Alpha 像素和错误输入。硬解断言需要支持上述编码的 Windows GPU；在本机 RTX 5080 / Intel Graphics 环境通过。

## 当前边界

这一版尚未实现 AI、字幕和 HDR 色彩管理。HDR 输入暂按 SDR RGBA 输出，不保证正确的 HDR 观感。

默认链路：同一显卡上的 D3D11VA 解码 → D3D11 VideoProcessor 颜色转换 → D3D12 共享 BGRA 纹理 → 可选美颜 Shader → wgpu / egui 显示。跨 API 使用共享 fence 在 GPU 队列上同步，正常硬解帧不执行 CPU 回读、libswscale、CPU 像素复制或纹理重新上传。暂停及回退缓存保留独立的原始 GPU 纹理；美颜开启时复用一张输出纹理，关闭时绕过该处理。软件路径把美颜和 Alpha 预乘合并到一次 GPU 绘制。AI 尚未接入，模型设备张量互操作仍需单独实现。
共享纹理首次初始化失败时回退到硬解加 CPU 回读的兼容路径，硬解也失败时才软解；透明格式继续使用保留 Alpha 的软件路径。实际模式和原因在视频信息中显示。不是所有格式与驱动都保证 GPU 互操作。

`seek_latency` 同一解码会话内测量十个跳转落点，报告命令提交、目标帧解码完成、RGBA 转为 UI 图像后的时间；这不是显示器实际呈现耗时。长 GOP 仍需补解码，不能保证所有任意位置都在几毫秒内精确显示。测试同时覆盖连续替换请求、满队列、EOF 后重播，以及逐像素对比顺序解码和定位结果、PCM 样本一致性。

音频使用独立的常驻 FFmpeg 解码器、libswresample 和 rodio；缺数据时填充的临时静音不推进媒体时钟，设备缓冲仍会带来少量输出延迟。只支持默认音视频轨，播放或定位中发生错误会提示，不在同一会话内悄悄改用其他解码后端。

SDK 和运行库留在 tools/ 或 target/ 中并被 Git 忽略。Gyan shared 构建包含 GPL 组件；分发时应保留 FFmpeg 许可证并遵守对应构建的再分发要求。
参考：[FFmpeg 硬解说明](https://ffmpeg.org/ffmpeg.html)、[FFmpeg VP9 Alpha 实现](https://www.ffmpeg.org/doxygen/trunk/libvpxdec_8c_source.html)、[egui 自定义窗口](https://github.com/emilk/egui/tree/main/examples/custom_window_frame)。





最近连续解码帧使用共享帧缓存（GPU 路径为显存纹理，兼容路径为 CPU 像素）（最多 128 MiB / 120 帧）。另设一个按需唤醒的后台解码线程：暂停/逐帧查看时，以当前 PTS 为中心提前准备前后连续帧，偏重后方；移动约四分之一窗口时补充，播放中使用已有顺序缓存避免重复解码争抢 GPU。后台缓存块同样最多 128 MiB / 120 帧；更新时保留旧块，新旧块与前台历史最多约 384 MiB 帧数据（GPU 路径在显存中，不计驱动对齐），另有有限队列、解码器和纹理开销。

后台解码器和硬解设备按需创建后复用；新定位使旧预取失效，在解码帧之间取消，关闭视频退出线程。前台可共享导入后台连续帧，重放完后按真实 PTS 接回解码器。冷缓存、超出缓存范围或后退速度超过预取吞吐时，仍可能等待；不保证所有回退都瞬时完成。后台预取失败时前台继续正常解码。

连续逐帧移动不取消正在生成的缓存块：先完成并发布当前块，再处理队列中最新的位置，防止频繁重启导致缓存断供。只有显式跳转与关闭视频才使旧任务失效。`continuous_reverse` 用连续 60 次回退验证跨块供给，并报告取帧耗时（不含 UI 渲染）。

恢复播放会挂起后台回退预取并使正在生成的块失效，暂停后再按需恢复；正常播放持续请求重绘，由呈现循环控制节奏，不再每次延后 5 ms。兼容路径的 GPU 回读目标、可独占的淘汰帧缓冲和已完成纹理提交的 UI 图像缓冲均复用；透明帧仍正确预乘 Alpha。`playback_throughput` 测量真实视频的解码与 UI 图像准备吞吐（不含纹理上传和显示），支持 `NKG_BENCH_VIDEO`，并检查播放时后台预取已禁用。


`gpu_decode_and_import` 使用真实 D3D11VA / D3D12 设备，检查 GPU 帧没有 CPU RGBA 缓冲，通过 egui 离屏渲染后回读一次验证画面，并验证跳转、后退与尾帧。测吞吐阶段不回读，等待 GPU 完成后计时；不含窗口呈现和显示器刷新。

```powershell
$env:NKG_BENCH_VIDEO = "D:/Videos/demo.mp4"
cargo test --release gpu_decode_and_import -- --ignored --nocapture
```

本机 RTX 5080，3304×1440 / 60 fps 的 `FXUI_PopupAccessoriesPreview_03.mp4`：119 帧 GPU 解码、转换及 egui 离屏渲染约 241 ms（494 fps）。这是处理吞吐，不是实际窗口 FPS；窗口使用 AutoVsync，刷新上限由显示器决定。CPU 兼容路径此前同文件解码与图像准备约 79 fps，测试阶段不同，不能当作严格同条件的呈现帧率对比。
回退缓存未命中时，前驱搜索从约两个标称帧间隔开始；若找不到真实前驱则逐次扩大搜索范围。平均帧率只决定初始搜索窗口，最终仍按真实 PTS 选择上一帧，避免对高码率/未压缩视频固定扫描一整秒。`cold_reverse` 可通过 `NKG_BENCH_VIDEO` 测量未命中缓存的回退与 CPU 图像准备，不包含纹理上传和屏幕呈现。

本机 `FXUI_PopupAccessoriesPreview_02.avi`（3304×1440 BGRA rawvideo、30 fps、约 1.14 GB）：20 次冷回退中位数从 290 ms 降到 48 ms，本次最大 58 ms；50 ms 按键间隔、预取缓存已准备的 30 次回退取帧最大约 5 ms。未压缩视频仍需从文件读取大帧，不适用压缩编码的 D3D11VA 硬解路径。

### GPU 资源复用（2026-10-07）

参考 [VLC 的 `assert_ProcessorInput`](https://github.com/videolan/vlc/blob/master/modules/video_output/win32/direct3d11.cpp) 按解码纹理的 array slice 缓存输入视图；固定的 VideoProcessor 参数仅在尺寸或旋转变化时设置，颜色参数变化时单独更新。参考 [mpv 的跨 API 表面队列](https://github.com/mpv-player/mpv/blob/master/video/out/d3d11/hwdec_dxva2dxgi.c) 的空闲检测思路，为现有 D3D11→D3D12 链路实现共享输出资源池，没有引入播放器依赖。

复用条件是原始帧的所有 Rust 引用已释放、wgpu 已提交的读取已完成，并且 D3D11 转换 fence 已完成。显示和历史缓存仍持有的帧不会被覆盖；尺寸或旋转变化后使用新池。每个池额外保留最多 3 个空闲输出表面，且像素数据不超过 64 MiB（不含驱动对齐），它们不计入上面的帧缓存预算；GPU 未完成的资源另计。没有空闲表面时分配新表面，不在 CPU 上等待 GPU。

后续 Shader 调用 `gpu::Frame::texture` 时必须使用对应设备的同一 wgpu 队列，并持有该帧直到使用它的命令提交完成；不能只留下纹理克隆而提前释放帧。现有 UI 由 `Player::raw` 保持引用。资源回收回调只持有原生资源，不捕获 wgpu 对象，避免退出时的引用环。

本机 RTX 5080，合成 H.264 1920×1080 / 60 fps，Release 下每轮处理 300 帧。改动前后交替运行各 5 次，解码、转换、egui 离屏渲染并等待 GPU 完成的中位数：**359.90 ms → 178.14 ms**，对应 **833.6 → 1684.1 fps**，耗时约降低 50.5%。不包含窗口呈现、显示器刷新，也不代表所有素材或其他显卡都有相同收益。透明视频与 CPU 兼容路径不使用此资源池。

复现素材和优化后检查：

```powershell
./tools/ffmpeg/bin/ffmpeg.exe -hide_banner -loglevel error -f lavfi -i "testsrc2=size=1920x1080:rate=60:duration=6" -an -c:v libx264 -preset ultrafast -crf 20 -y test-media/gpu-1080p60.mp4
$env:NKG_BENCH_VIDEO = "test-media/gpu-1080p60.mp4"
cargo test --release gpu_decode_and_import -- --ignored --nocapture
```

该检查报告共享输出资源创建数，并验证首帧、定位与回退后的 GPU/CPU 画面对比、完成帧的实际复用、解码器关闭后保留帧不变，以及后台解码线程与渲染队列并发回收。性能计时段不回读。另已验证 HEVC、AV1、VP9、VFR 与 90° 旋转素材；原有透明视频、音频和定位回归继续保留。

### 未压缩透明 AVI 的热点与优化（2026-10-07）

使用 `FXUI_PopupAccessoriesPreview_02.avi` 实测：3304×1440、BGRA rawvideo、30 fps、60 帧、约 1.14 GB。该素材软件解码，硬解表面池不参与。顺序分阶段检查的三轮中位数（60 帧，阶段串行，不能与并行播放耗时直接相加）：

| 原路径阶段 | 耗时 | 本次处理 |
|---|---:|---|
| 读取、解包及解码 | 511.6 ms | 继续由 FFmpeg 负责；进一步计时确认大部分在 `av_read_frame` |
| 原生颜色转换 | 143.3 ms | 保留 FFmpeg 的格式、步长和旋转处理，直接写入 Rust 独占缓冲 |
| Rust 帧准备（包含整帧拷贝） | 104.3 ms | 取消中间整帧拷贝；改后约 30.3 ms |
| CPU Alpha 预乘与 UI 图像准备 | 383.0 ms | 正常透明视频显示改走 GPU Alpha 处理 |

参考 [mpv GPU 渲染中的纹理上传和 Alpha 处理](https://github.com/mpv-player/mpv/blob/master/video/out/gpu/video.c)，透明 RGBA 帧直接上传到复用的纹理，再由一个 Shader 在线性光下预乘 Alpha，输出 sRGB 纹理给现有 egui 渲染。预乘规则对齐 [egui 0.31.1 的 Color32](https://github.com/emilk/egui/blob/0.31.1/crates/ecolor/src/color32.rs)，保持透明像素和半透明边缘；原始 RGBA 缓存不变。固定尺寸时复用上传纹理、输出纹理、绑定及管线，额外两张纹理的像素数据约 36.3 MiB（不含上传暂存及驱动开销）。FFmpeg 写入调用方提供的输出平面，沿用其 [sws_scale 示例](https://www.ffmpeg.org/doxygen/trunk/scale_video_8c-example.html) 的缓冲使用方式；只有没有其他引用的淘汰帧缓冲才可复用。

RTX 5080、Release、同一素材，旧版 CPU Alpha 路径与新版 GPU Alpha 路径交替各运行五轮。首帧初始化后计时 59 帧，包含后台解码、纹理上传、egui 离屏绘制和等待 GPU 完成，不包含窗口呈现、控件/棋盘格绘制或显示器刷新：

| 指标（五轮中位数） | 改前 | 改后 |
|---|---:|---:|
| 总处理耗时 | 896.70 ms | 709.51 ms |
| 处理吞吐 | 65.8 fps | 83.2 fps |
| UI 线程准备与上传的 CPU 提交耗时 | 588.46 ms | 153.99 ms |

总处理耗时降低约 20.9%，吞吐提高约 26.4%。各阶段可重叠，CPU 提交耗时不等于 GPU 执行时间。文件系统缓存与系统负载会影响结果，不代表冷盘读取或窗口帧率。改后 `av_read_frame` 三轮中位数约 473.6 ms / 60 帧，仍是主要热点；其中包含读取、解包及包内存准备，尚未分离纯磁盘耗时。

```powershell
$env:NKG_BENCH_VIDEO = "E:/Work/Video_Res/UITest/popupacceddsories/FXUI_PopupAccessoriesPreview_02.avi"
cargo test --release software_stage_profile -- --ignored --nocapture
cargo test --release software_upload_and_render -- --ignored --nocapture
cargo test --release software_rgba_matches_ffmpeg -- --ignored --nocapture
# 仅用于同版本对照 CPU Alpha 显示路径；不恢复旧版解码器的整帧拷贝
$env:NKG_BENCH_CPU_ALPHA = "1"
cargo test --release software_upload_and_render -- --ignored --nocapture
Remove-Item Env:NKG_BENCH_CPU_ALPHA
```

GPU 检查覆盖全部 256 个 Alpha 值与颜色梯度，和 egui CPU 参考的通道差不超过 1/255；读取失败及不合法缓冲会报错。该 AVI 的全部 60 帧原始 RGBA 与 FFmpeg 命令行逐字节一致。分阶段计时只在显式运行 profile 检查时开启；正常播放不读取这些计时器。`software_stage_profile` 故意保留 CPU 图像准备作为参考测量，新版透明视频 UI 不走这一步。
