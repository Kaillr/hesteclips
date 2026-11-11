# Puts OBS Studio's game capture hook into a folder: graphics-hook32/64.dll
# (loaded into a game to copy its picture), inject-helper32/64.exe (loads it)
# and get-graphics-offsets32/64.exe (finds where to hook D3D9 and DXGI). They
# are OBS's own files, unchanged and signed by OBS Project, LLC, which is what
# anti-cheat software recognises. GPLv2: the licence goes with them.
#
# The release copies them next to the app (game-hook\); for development:
#   pwsh scripts/fetch-game-hook.ps1
# fills target\game-hook, where a debug build finds them.
param(
    [string]$Dest
)
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot
if (-not $Dest) { $Dest = Join-Path $root 'target\game-hook' }

# Pinned, so an update only carries them when this changes.
$obsVersion = '32.2.2'
$obsSha256 = '4D6E40E3AB155F56B30DE517380566A206D74B63CDF5AD49AA596924768F97E1'

$work = Join-Path $root 'target\obs-download'
New-Item -ItemType Directory -Force $work, $Dest | Out-Null
$zip = Join-Path $work "OBS-Studio-$obsVersion-Windows-x64.zip"
if (-not (Test-Path $zip) -or (Get-FileHash $zip -Algorithm SHA256).Hash -ne $obsSha256) {
    Invoke-WebRequest "https://github.com/obsproject/obs-studio/releases/download/$obsVersion/OBS-Studio-$obsVersion-Windows-x64.zip" -OutFile $zip
    $hash = (Get-FileHash $zip -Algorithm SHA256).Hash
    if ($hash -ne $obsSha256) { throw "OBS download has the wrong checksum: $hash" }
}

Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [System.IO.Compression.ZipFile]::OpenRead($zip)
try {
    $wanted = @{
        'data/obs-plugins/win-capture/graphics-hook32.dll' = 'graphics-hook32.dll'
        'data/obs-plugins/win-capture/graphics-hook64.dll' = 'graphics-hook64.dll'
        'data/obs-plugins/win-capture/inject-helper32.exe' = 'inject-helper32.exe'
        'data/obs-plugins/win-capture/inject-helper64.exe' = 'inject-helper64.exe'
        'data/obs-plugins/win-capture/get-graphics-offsets32.exe' = 'get-graphics-offsets32.exe'
        'data/obs-plugins/win-capture/get-graphics-offsets64.exe' = 'get-graphics-offsets64.exe'
        'data/obs-studio/license/gplv2.txt' = 'OBS-LICENSE.txt'
    }
    foreach ($entry in $archive.Entries) {
        if ($wanted.ContainsKey($entry.FullName)) {
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $Dest $wanted[$entry.FullName]), $true)
        }
    }
} finally {
    $archive.Dispose()
}
foreach ($name in $wanted.Values) {
    if (-not (Test-Path (Join-Path $Dest $name))) { throw "$name is missing from the OBS download" }
}
@"
OBS Studio's game capture hook, version $obsVersion, unchanged, from
https://github.com/obsproject/obs-studio/releases/tag/$obsVersion
Licensed under the GNU GPL v2 (OBS-LICENSE.txt). Its source code:
https://github.com/obsproject/obs-studio/tree/$obsVersion/plugins/win-capture
HesteClips runs these as separate programs; they aren't part of HesteClips.
"@ | Set-Content (Join-Path $Dest 'README.txt')
