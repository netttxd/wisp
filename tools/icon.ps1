# Makes the icon files from assets\logo.png:
#   assets\wisp.ico     the app icon (16 to 256 px)
#   assets\tray32.rgba  the 32 px tray icon as raw RGBA, built into wisp.exe
# With -Exe, also puts the icon into that exe, so Explorer shows it. Run after every build:
#   powershell -ExecutionPolicy Bypass -File tools\icon.ps1 -Exe target\release\wisp.exe
param([string]$Exe)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class Res {
    [DllImport("kernel32", CharSet = CharSet.Unicode, SetLastError = true)] public static extern IntPtr BeginUpdateResource(string file, bool deleteExisting);
    [DllImport("kernel32", SetLastError = true)] public static extern bool UpdateResource(IntPtr h, IntPtr type, IntPtr name, ushort lang, byte[] data, uint size);
    [DllImport("kernel32", SetLastError = true)] public static extern bool EndUpdateResource(IntPtr h, bool discard);
}
'@

$logo = [System.Drawing.Bitmap]::FromFile("$root\assets\logo.png")
function Resize([int]$n) {
    $b = New-Object System.Drawing.Bitmap $n, $n, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($b)
    $g.InterpolationMode = 'HighQualityBicubic'; $g.PixelOffsetMode = 'HighQuality'; $g.SmoothingMode = 'HighQuality'
    $g.DrawImage($logo, 0, 0, $n, $n); $g.Dispose()
    return $b
}

# Icon images, PNG-compressed (Windows Vista and newer read PNG inside icons).
$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$pngs = foreach ($n in $sizes) {
    $ms = New-Object System.IO.MemoryStream
    (Resize $n).Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    , $ms.ToArray()
}

# .ico file: header, one 16-byte entry per image, then the images.
$ico = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter $ico
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $n = $sizes[$i]; $s = if ($n -ge 256) { 0 } else { $n }
    $w.Write([byte]$s); $w.Write([byte]$s); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([uint16]1); $w.Write([uint16]32); $w.Write([uint32]$pngs[$i].Length); $w.Write([uint32]$offset)
    $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $w.Write($p) }
[System.IO.File]::WriteAllBytes("$root\assets\wisp.ico", $ico.ToArray())

# Tray icon: 32x32 raw RGBA.
$t = Resize 32
$data = $t.LockBits((New-Object System.Drawing.Rectangle 0, 0, 32, 32), 'ReadOnly', ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb))
$bgra = New-Object byte[] (32 * 32 * 4)
[System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $bgra, 0, $bgra.Length)
$t.UnlockBits($data)
for ($i = 0; $i -lt $bgra.Length; $i += 4) { $b = $bgra[$i]; $bgra[$i] = $bgra[$i + 2]; $bgra[$i + 2] = $b }
[System.IO.File]::WriteAllBytes("$root\assets\tray32.rgba", $bgra)
"wrote assets\wisp.ico and assets\tray32.rgba"

if ($Exe) {
    # RT_ICON (3) holds each image, RT_GROUP_ICON (14) lists them. Explorer shows group 1.
    $h = [Res]::BeginUpdateResource((Resolve-Path $Exe).Path, $false)
    if ($h -eq [IntPtr]::Zero) { throw "cannot open $Exe for writing" }
    $grp = New-Object System.IO.MemoryStream
    $gw = New-Object System.IO.BinaryWriter $grp
    $gw.Write([uint16]0); $gw.Write([uint16]1); $gw.Write([uint16]$sizes.Count)
    for ($i = 0; $i -lt $sizes.Count; $i++) {
        $n = $sizes[$i]; $s = if ($n -ge 256) { 0 } else { $n }
        if (-not [Res]::UpdateResource($h, [IntPtr]3, [IntPtr]($i + 1), 0, $pngs[$i], $pngs[$i].Length)) { throw 'UpdateResource RT_ICON failed' }
        $gw.Write([byte]$s); $gw.Write([byte]$s); $gw.Write([byte]0); $gw.Write([byte]0)
        $gw.Write([uint16]1); $gw.Write([uint16]32); $gw.Write([uint32]$pngs[$i].Length); $gw.Write([uint16]($i + 1))
    }
    $g = $grp.ToArray()
    if (-not [Res]::UpdateResource($h, [IntPtr]14, [IntPtr]1, 0, $g, $g.Length)) { throw 'UpdateResource RT_GROUP_ICON failed' }
    # App manifest saying Wisp is made for Windows 8 to 11: needed for see-through child windows, which fade one
    # wallpaper into the next. (Without it Windows treats the exe as a Windows 7 program.)
    $manifest = [System.Text.Encoding]::UTF8.GetBytes(@'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{4a2f28e3-53b9-4441-ba9c-d69d4a4a6e38}"/>
      <supportedOS Id="{1f676c76-80e1-4239-95bb-83d0f6d0da78}"/>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
</assembly>
'@)
    if (-not [Res]::UpdateResource($h, [IntPtr]24, [IntPtr]1, 0, $manifest, $manifest.Length)) { throw 'UpdateResource RT_MANIFEST failed' }
    if (-not [Res]::EndUpdateResource($h, $false)) { throw 'EndUpdateResource failed' }
    "icon set on $Exe"
}
