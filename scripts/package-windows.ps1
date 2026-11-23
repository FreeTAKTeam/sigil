param(
    [switch]$WhisperOnly,
    [string]$ModelPath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    # The package version is the first `version = "..."` line, in [package].
    $versionLine = Select-String -LiteralPath 'Cargo.toml' -Pattern '^version = "([^"]+)"' | Select-Object -First 1
    if (-not $versionLine) { throw 'No package version in Cargo.toml.' }
    $version = $versionLine.Matches[0].Groups[1].Value
    $features = if ($WhisperOnly) { @('--no-default-features') } else { @() }
    cargo build --release --locked @features
    if ($LASTEXITCODE) { throw 'Cargo build failed.' }
    $targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
    $release = Join-Path $targetDir 'release'
    $backend = if ($WhisperOnly) { 'whisper' } else { 'parakeet' }
    $bundle = Join-Path $root "dist/r3akt-sigil-$version-windows-x64-$backend"
    if (Test-Path -LiteralPath $bundle) { Remove-Item -LiteralPath $bundle -Recurse -Force }
    New-Item -ItemType Directory -Path $bundle -Force | Out-Null
    Copy-Item -LiteralPath @((Join-Path $release 'r3akt-sigil.exe'), 'gateway.example.toml', 'README.md', 'LICENSE', 'THIRD_PARTY_NOTICES.md', 'KODIAK_EXTRACTION.md', 'lxmf.toml', 'send_lxmf.py', 'test_send_lxmf.py') -Destination $bundle -Force
    $notice = New-Item -ItemType Directory -Path (Join-Path $bundle 'crates/kodiak-laya') -Force
    Copy-Item -LiteralPath 'crates/kodiak-laya/NOTICE' -Destination $notice.FullName -Force
    $documentation = New-Item -ItemType Directory -Path (Join-Path $bundle 'docs') -Force
    Copy-Item -LiteralPath @('docs/architecture.md', 'docs/developer-guide.md', 'docs/kodiak.md', 'docs/kodiak-laya-benchmark.md') -Destination $documentation.FullName -Force
    $images = New-Item -ItemType Directory -Path (Join-Path $documentation.FullName 'images') -Force
    Copy-Item -LiteralPath 'docs/images/sigil-logo.webp' -Destination $images.FullName -Force
    if (-not $WhisperOnly) {
        foreach ($dll in @('sherpa-onnx-c-api.dll', 'onnxruntime.dll', 'onnxruntime_providers_shared.dll')) {
            Copy-Item -LiteralPath (Join-Path $release $dll) -Destination $bundle -Force
        }
    }
    if ($ModelPath) {
        $model = New-Item -ItemType Directory -Path (Join-Path $bundle 'models') -Force
        Copy-Item -LiteralPath $ModelPath -Destination $model.FullName -Recurse -Force
    }
    $archive = "$bundle.zip"
    Compress-Archive -Path (Join-Path $bundle '*') -DestinationPath $archive -Force
    $hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath "$archive.sha256" -Value "$hash  $(Split-Path -Leaf $archive)" -Encoding Ascii
    Write-Output $archive
} finally {
    Pop-Location
}
