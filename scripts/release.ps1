# Builds the Windows release into Releases/: the app with a pinned ffmpeg next
# to it, packed by Velopack into an installer (HesteClips-win-Setup.exe), a
# portable zip, and the full + delta packages the installed app updates from.
#
# semantic-release runs this with the version it picked (see .releaserc.json)
# and then uploads Releases/* to the GitHub release. It also runs locally:
#   pwsh scripts/release.ps1 -Version 0.0.1-local
# Needs Rust, and `vpk` (dotnet tool install -g vpk --version 1.2.161).
param(
    [Parameter(Mandatory)][string]$Version
)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$root = Split-Path $PSScriptRoot
Set-Location $root
$repo = 'https://github.com/Kaillr/hesteclips'

# The editor, thumbnails and probing run ffmpeg; shipping it means nothing else
# to install. Pinned, so an update only carries it when this changes.
$ffmpegVersion = '9.0.2'
$ffmpegSha256 = '60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba'

$work = Join-Path $root 'target\release-package'
$stage = Join-Path $work 'app'
$out = Join-Path $root 'Releases'
Remove-Item -Recurse -Force $stage, $out -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage, $out | Out-Null

# The version goes into the .exe's details; the repo itself isn't changed
# (Cargo.lock follows the version, so it's put back too).
$cargo = Get-Content Cargo.toml -Raw
$lock = Get-Content Cargo.lock -Raw
($cargo -replace '(?m)^version = "[^"]*"', "version = `"$Version`"") | Set-Content Cargo.toml -NoNewline
try {
    cargo build --release -p hesteclips
} finally {
    Set-Content Cargo.toml $cargo -NoNewline
    Set-Content Cargo.lock $lock -NoNewline
}
Copy-Item target\release\hesteclips.exe $stage

$zip = Join-Path $work "ffmpeg-$ffmpegVersion.zip"
if (-not (Test-Path $zip) -or (Get-FileHash $zip -Algorithm SHA256).Hash -ne $ffmpegSha256) {
    Invoke-WebRequest "https://github.com/GyanD/codexffmpeg/releases/download/$ffmpegVersion/ffmpeg-$ffmpegVersion-essentials_build.zip" -OutFile $zip
    $hash = (Get-FileHash $zip -Algorithm SHA256).Hash
    if ($hash -ne $ffmpegSha256) { throw "ffmpeg download has the wrong checksum: $hash" }
}
$unzipped = Join-Path $work 'ffmpeg'
Remove-Item -Recurse -Force $unzipped -ErrorAction SilentlyContinue
Expand-Archive $zip $unzipped
$ffmpegDir = Join-Path $unzipped "ffmpeg-$ffmpegVersion-essentials_build"
Copy-Item "$ffmpegDir\bin\ffmpeg.exe", "$ffmpegDir\bin\ffprobe.exe" $stage
Copy-Item "$ffmpegDir\LICENSE" (Join-Path $stage 'ffmpeg-LICENSE.txt')

# The previous release, so the update to this one can be a small delta. None
# the first time.
if ($env:GITHUB_TOKEN) {
    try {
        vpk download github --repoUrl $repo --token $env:GITHUB_TOKEN --outputDir $out
    } catch {
        Write-Warning "No previous release to make a delta from: $_"
    }
}

vpk pack `
    --packId HesteClips `
    --packVersion $Version `
    --packDir $stage `
    --mainExe hesteclips.exe `
    --runtime win-x64 `
    --packTitle HesteClips `
    --packAuthors Kaillr `
    --icon crates\app\assets\icon.ico `
    --outputDir $out
