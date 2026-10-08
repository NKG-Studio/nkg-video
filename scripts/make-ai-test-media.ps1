$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$dir = Join-Path $root 'test-media/ai'
New-Item -ItemType Directory -Force $dir | Out-Null
$ffmpeg = Join-Path $root 'tools/ffmpeg/bin/ffmpeg.exe'
foreach ($sample in @(@('portrait','426:640'),@('pose','640:426'))) {
    $name = $sample[0]
    Invoke-WebRequest "https://storage.googleapis.com/mediapipe-assets/$name.jpg" -OutFile "$dir/$name.jpg"
    & $ffmpeg -hide_banner -loglevel error -y -i "$dir/$name.jpg" -vf "scale=$($sample[1])" -pix_fmt rgba -f rawvideo "$dir/$name.rgba"
    if ($LASTEXITCODE -ne 0) { throw 'AI fixture conversion failed' }
}
& $ffmpeg -hide_banner -loglevel error -y -loop 1 -i "$dir/portrait.jpg" -vf "scale=720:1080,pad=1920:1080:(ow-iw)/2:0,zoompan=z=1.02:x='20+10*sin(on/30)':y=0:d=1:s=1920x1080:fps=60" -t 4 -an -pix_fmt yuv420p -c:v libx264 -preset ultrafast -crf 20 "$dir/portrait-1080p60.mp4"
if ($LASTEXITCODE -ne 0) { throw 'AI video fixture generation failed' }
# These upstream MediaPipe test assets are local-only, not included in releases.
