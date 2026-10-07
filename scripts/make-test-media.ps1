$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)
$ff = Join-Path (Get-Location) 'tools/ffmpeg/bin/ffmpeg.exe'
New-Item -ItemType Directory -Force test-media | Out-Null
function Encode([string[]]$Arguments) {
    & $ff -hide_banner -loglevel error -y @Arguments
    if ($LASTEXITCODE -ne 0) { throw "FFmpeg fixture creation failed: $Arguments" }
}
Encode @('-f','lavfi','-i','testsrc2=size=640x360:rate=30:duration=20','-f','lavfi','-i','sine=frequency=440:sample_rate=48000:duration=20','-c:v','libx264','-preset','ultrafast','-c:a','aac','-shortest','test-media/h264.mp4')
Encode @('-i','test-media/h264.mp4','-t','2','-an','-c:v','libx265','-preset','ultrafast','-x265-params','log-level=error','test-media/hevc.mkv')
Encode @('-i','test-media/h264.mp4','-t','2','-an','-c:v','libaom-av1','-cpu-used','8','-crf','45','test-media/av1.mkv')
Encode @('-i','test-media/h264.mp4','-t','2','-an','-c:v','libvpx-vp9','-deadline','realtime','-cpu-used','8','test-media/vp9.webm')
Encode @('-i','test-media/h264.mp4','-t','2','-an','-c:v','mpeg4','test-media/mpeg4.avi')
$alpha = "nullsrc=s=320x180:r=24:d=3,format=rgba,geq=r=40:g=160:b=240:a='if(between(X,80,240)*between(Y,30,150),180,0)'"
Encode @('-f','lavfi','-i',$alpha,'-c:v','libvpx-vp9','-pix_fmt','yuva420p','-deadline','realtime','-cpu-used','8','test-media/vp9-alpha.webm')
Encode @('-f','lavfi','-i',$alpha,'-c:v','libvpx','-pix_fmt','yuva420p','-auto-alt-ref','0','test-media/vp8-alpha.webm')
Encode @('-f','lavfi','-i',$alpha,'-c:v','prores_ks','-profile:v','4','-pix_fmt','yuva444p10le','test-media/prores-alpha.mov')
Encode @('-i','test-media/h264.mp4','-t','3','-an','-vf',"select='if(lt(t,1),not(mod(n,2)),not(mod(n,3)))'",'-fps_mode','vfr','-c:v','libx264','test-media/vfr.mp4')
