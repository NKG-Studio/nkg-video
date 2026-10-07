$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$toolsDir = Join-Path $projectRoot 'tools'
New-Item -ItemType Directory -Force $toolsDir | Out-Null
$archive = Join-Path $toolsDir 'ffmpeg.zip'
if (-not (Test-Path -LiteralPath (Join-Path $toolsDir 'ffmpeg/bin/ffmpeg.exe'))) {
    Invoke-WebRequest 'https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip' -OutFile $archive
    $expected = ((Invoke-WebRequest 'https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip.sha256').Content -split '\s+')[0]
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $expected) { throw 'FFmpeg archive checksum mismatch' }
    $expanded = Join-Path $toolsDir 'download'
    Expand-Archive -LiteralPath $archive -DestinationPath $expanded -Force
    $package = Get-ChildItem -LiteralPath $expanded -Directory | Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'bin/ffmpeg.exe') } | Select-Object -First 1
    if (-not $package) { throw 'FFmpeg package layout was not recognized' }
    Copy-Item -LiteralPath $package.FullName -Destination (Join-Path $toolsDir 'ffmpeg') -Recurse -Force
}
& (Join-Path $toolsDir 'ffmpeg/bin/ffmpeg.exe') -version
