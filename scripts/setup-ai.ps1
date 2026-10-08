$ErrorActionPreference = 'Stop'
$root = Join-Path (Split-Path -Parent $PSScriptRoot) 'tools/mediapipe'
New-Item -ItemType Directory -Force $root, (Join-Path $root 'models') | Out-Null
function Fetch($Url, $Path, $Hash) {
    if (!(Test-Path -LiteralPath $Path) -or (Get-FileHash -LiteralPath $Path).Hash -ne $Hash) {
        Invoke-WebRequest $Url -OutFile $Path
    }
    if ((Get-FileHash -LiteralPath $Path).Hash -ne $Hash) { throw "Checksum mismatch: $Path" }
}
$wheel = Join-Path $root 'mediapipe.whl'
Fetch 'https://files.pythonhosted.org/packages/4a/95/14e45f779280d2cc9d7495cb149a67f8a44d5beb801df7f662353976d0ae/mediapipe-1.1.0-py3-none-win_amd64.whl' $wheel '955ac7934825aa8c8ff78fd1e47bf1f2b78fcafc95a8ebc010f6ee977c2eaf53'
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::OpenRead($wheel)
try {
    foreach ($pair in @(
        @('mediapipe/tasks/c/libmediapipe.dll', 'mediapipe.dll'),
        @('mediapipe-1.1.0.dist-info/licenses/LICENSE', 'LICENSE'),
        @('mediapipe-1.1.0.dist-info/licenses/NOTICE', 'NOTICE')
    )) {
        [IO.Compression.ZipFileExtensions]::ExtractToFile($archive.GetEntry($pair[0]), (Join-Path $root $pair[1]), $true)
    }
} finally { $archive.Dispose() }

$models = @(
    @('face_landmarker.task', 'face_landmarker/face_landmarker/float16/1/face_landmarker.task', '64184e229b263107bc2b804c6625db1341ff2bb731874b0bcc2fe6544e0bc9ff'),
    @('pose_landmarker.task', 'pose_landmarker/pose_landmarker_lite/float16/1/pose_landmarker_lite.task', '59929e1d1ee95287735ddd833b19cf4ac46d29bc7afddbbf6753c459690d574a'),
    @('selfie_multiclass.tflite', 'image_segmenter/selfie_multiclass_256x256/float32/1/selfie_multiclass_256x256.tflite', 'c6748b1253a99067ef71f7e26ca71096cd449baefa8f101900ea23016507e0e0')
)
foreach ($model in $models) {
    Fetch ('https://storage.googleapis.com/mediapipe-models/' + $model[1]) (Join-Path $root ('models/' + $model[0])) $model[2]
}

# Official C headers pinned to the matching release; keep original license notices.
$revision = '821db8a4428baf58bf4c09a0330c937e6e0a3753'
$queue = [Collections.Generic.Queue[string]]::new()
foreach ($header in @('face_landmarker/face_landmarker.h', 'pose_landmarker/pose_landmarker.h', 'image_segmenter/image_segmenter.h')) {
    $queue.Enqueue('mediapipe/tasks/c/vision/' + $header)
}
$seen = @{}
while ($queue.Count) {
    $header = $queue.Dequeue()
    if ($seen.ContainsKey($header)) { continue }
    $seen[$header] = $true
    $content = (Invoke-WebRequest "https://raw.githubusercontent.com/google-ai-edge/mediapipe/$revision/$header").Content
    $dest = Join-Path $root ('include/' + $header)
    New-Item -ItemType Directory -Force (Split-Path -Parent $dest) | Out-Null
    [IO.File]::WriteAllText($dest, $content)
    foreach ($match in [regex]::Matches($content, '#include "([^"]+)"')) { $queue.Enqueue($match.Groups[1].Value) }
}
Write-Output 'MediaPipe 1.1.0 native runtime and face/pose/skin models ready (no Python required).'
