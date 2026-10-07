param([string]$Version = '0.1.0')
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw 'Expected a numeric release version' }
& cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw 'Release build failed' }
$name = "nkg-video-$Version-windows-x64"
$stage = Join-Path $root "artifacts/$name"
if (Test-Path -LiteralPath $stage) { throw "Output already exists: $stage" }
New-Item -ItemType Directory -Path $stage -Force | Out-Null
$sdk = Join-Path $root 'tools/ffmpeg-shared'
Copy-Item -LiteralPath 'target/release/nkg-video.exe' -Destination $stage
Copy-Item -LiteralPath (Join-Path $sdk 'bin/ffprobe.exe') -Destination $stage
Get-ChildItem -LiteralPath (Join-Path $sdk 'bin') -Filter '*.dll' | Copy-Item -Destination $stage
Copy-Item -LiteralPath (Join-Path $sdk 'LICENSE') -Destination (Join-Path $stage 'FFmpeg-LICENSE.txt')
Copy-Item -LiteralPath (Join-Path $sdk 'README.txt') -Destination (Join-Path $stage 'FFmpeg-README.txt')
foreach ($file in @('README.md','TIMING.md','LICENSE','THIRD_PARTY.md')) {
    Copy-Item -LiteralPath $file -Destination $stage
}
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
$vs = & $vswhere -latest -products '*' -property installationPath
$crt = Get-ChildItem -Path (Join-Path $vs 'VC/Redist/MSVC/*/x64/Microsoft.VC*.CRT') -Directory |
    Sort-Object FullName -Descending | Select-Object -First 1
if (-not $crt) { throw 'Visual C++ x64 redistributable directory missing' }
foreach ($dll in @('vcruntime140.dll','vcruntime140_1.dll')) {
    Copy-Item -LiteralPath (Join-Path $crt.FullName $dll) -Destination $stage
}
$metadata = & cargo metadata --locked --offline --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Cannot collect dependency notices' }
$licenses = Join-Path $stage 'licenses'
New-Item -ItemType Directory -Path $licenses | Out-Null
$metadata.packages | Select-Object name,version,license,repository,source |
    ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $licenses 'packages.json') -Encoding utf8
foreach ($package in $metadata.packages | Where-Object source) {
    $dir = Split-Path -Parent $package.manifest_path
    $dest = Join-Path $licenses "$($package.name)-$($package.version)"
    $files = Get-ChildItem -LiteralPath $dir | Where-Object Name -Match '^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT)'
    if ($files) {
        New-Item -ItemType Directory -Path $dest -Force | Out-Null
        $files | Copy-Item -Destination $dest -Recurse
    }
}
$zip = Join-Path $root "artifacts/$name.zip"
Compress-Archive -LiteralPath $stage -DestinationPath $zip -CompressionLevel Optimal
$hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $name.zip" | Set-Content -LiteralPath "$zip.sha256" -Encoding ascii
Write-Output $zip
