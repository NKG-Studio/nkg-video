$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$toolsDir = Join-Path $root 'tools'
$sdk = Join-Path $toolsDir 'ffmpeg-shared'
if (Test-Path -LiteralPath (Join-Path $sdk 'include/libavcodec/avcodec.h')) { Write-Output "FFmpeg native SDK: $sdk"; return }
New-Item -ItemType Directory -Force $toolsDir | Out-Null
$archive = Join-Path $toolsDir 'ffmpeg-native-github.7z'
$url = 'https://github.com/GyanD/codexffmpeg/releases/download/8.1.2/ffmpeg-8.1.2-full_build-shared.7z'
$sha256 = 'cba748035c21ce1431d0823c7a3a711f38616f89f87a265dceddf9b7f6749d2d'
if (-not (Test-Path -LiteralPath $archive)) {
    & curl.exe -L --fail --silent --show-error $url -o $archive
    if ($LASTEXITCODE -ne 0) { throw 'FFmpeg shared SDK download failed' }
}
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $sha256) { throw 'FFmpeg shared SDK checksum mismatch' }
$unpack = Join-Path $toolsDir 'native-unpack'
New-Item -ItemType Directory -Force $unpack | Out-Null
& tar.exe -xf $archive -C $unpack
if ($LASTEXITCODE -ne 0) { throw 'FFmpeg shared SDK extraction failed' }
Copy-Item -LiteralPath (Join-Path $unpack 'ffmpeg-8.1.2-full_build-shared') -Destination $sdk -Recurse -Force
Write-Output "FFmpeg native SDK: $sdk"
