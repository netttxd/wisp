# Builds Wisp and puts everything a user needs in dist\: wisp.exe, libmpv-2.dll (+ its vulkan-1.dll) and the licenses.
# Zip dist\ to share it. Run from the project folder:
#   powershell -ExecutionPolicy Bypass -File tools\package.ps1
# libmpv-2.dll comes from vendor\: the mpv-dev-x86_64 build from https://github.com/shinchiro/mpv-winbuild-cmake/releases
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$mingw = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages" -Directory -Filter 'BrechtSanders.WinLibs*' | Select-Object -First 1
if ($mingw) { $env:Path = "$env:USERPROFILE\.cargo\bin;$($mingw.FullName)\mingw64\bin;$env:Path" }
# MinGW's tools break on paths over 260 characters, so build in a short folder.
$env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\wisp-build"
Push-Location $root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'build failed' }
} finally { Pop-Location }

$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null
Copy-Item "$env:CARGO_TARGET_DIR\release\wisp.exe" $dist -Force
& "$PSScriptRoot\icon.ps1" -Exe (Join-Path $dist 'wisp.exe')

$vendor = Join-Path $root 'vendor'
foreach ($f in 'libmpv-2.dll', 'vulkan-1.dll') {
    if (-not (Test-Path "$vendor\$f")) { throw "vendor\$f missing (see the top of this script)" }
    Copy-Item "$vendor\$f" $dist -Force
}
# Older packages had mpv.exe; Wisp plays videos itself now.
Remove-Item (Join-Path $dist 'mpv.exe') -ErrorAction SilentlyContinue

@"
Wisp ships with two pieces of other people's work:

mpv (libmpv-2.dll): the video player built into Wisp. Licensed under the GNU GPL v2 or later.
  Source code: https://github.com/mpv-player/mpv
  License: https://github.com/mpv-player/mpv/blob/master/LICENSE.GPL
  This build: https://github.com/shinchiro/mpv-winbuild-cmake

Anime4K: the shaders behind the "Anime" quality mode, built into wisp.exe.
$(Get-Content (Join-Path $root 'assets\anime4k\LICENSE') -Raw)
"@ | Set-Content (Join-Path $dist 'LICENSES.txt') -Encoding utf8
# Wisp's own license (proprietary): next to the exe, so everyone who gets Wisp sees it.
Copy-Item (Join-Path $root 'LICENSE.txt') $dist -Force

Get-ChildItem $dist | ForEach-Object { '{0}  {1:N1} MB' -f $_.Name, ($_.Length / 1MB) }
