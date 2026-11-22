param(
    [switch]$WhisperOnly,
    [string]$ModelPath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $features = if ($WhisperOnly) { @('--no-default-features') } else { @() }
    cargo build --release --locked @features
    if ($LASTEXITCODE) { throw 'Cargo build failed.' }
    $backend = if ($WhisperOnly) { 'whisper' } else { 'parakeet' }
    $bundle = Join-Path $root "dist/r3akt-sigil-windows-x64-$backend"
    New-Item -ItemType Directory -Path $bundle -Force | Out-Null
    Copy-Item -LiteralPath @('target/release/r3akt-sigil.exe', 'gateway.example.toml', 'README.md', 'LICENSE') -Destination $bundle -Force
    $documentation = New-Item -ItemType Directory -Path (Join-Path $bundle 'docs') -Force
    Copy-Item -LiteralPath @('docs/architecture.md', 'docs/developer-guide.md', 'docs/kodiak.md') -Destination $documentation.FullName -Force
    $images = New-Item -ItemType Directory -Path (Join-Path $documentation.FullName 'images') -Force
    Copy-Item -LiteralPath 'docs/images/sigil-logo.webp' -Destination $images.FullName -Force
    if (-not $WhisperOnly) {
        foreach ($dll in @('sherpa-onnx-c-api.dll', 'onnxruntime.dll', 'onnxruntime_providers_shared.dll')) {
            Copy-Item -LiteralPath "target/release/$dll" -Destination $bundle -Force
        }
    }
    if ($ModelPath) {
        $model = New-Item -ItemType Directory -Path (Join-Path $bundle 'models') -Force
        Copy-Item -LiteralPath $ModelPath -Destination $model.FullName -Recurse -Force
    }
    Compress-Archive -Path (Join-Path $bundle '*') -DestinationPath "$bundle.zip" -Force
    Write-Output "$bundle.zip"
} finally {
    Pop-Location
}
