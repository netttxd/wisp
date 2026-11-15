# Wisp

A light live wallpaper app for Windows. Plays videos, GIFs and Wallpaper Engine Workshop items
(videos and scenes) behind your desktop icons, using very little RAM, CPU and GPU.

- **Videos and GIFs**: any video file (mp4, mkv, webm, mov, gif...). Big videos get a screen-sized
  copy so they decode lighter.
- **Wallpaper Engine scenes**: played by Wisp's own engine, with particles, effects, clocks and text,
  and each scene's own options (turn its clock off, change its colors...).
- **Your Wallpaper Engine library**: every wallpaper you already have shows up in Wisp's Library.
- **Discover**: search the Steam Workshop, MoeWalls and MotionBGs, and put a wallpaper on in one click.
- **Workshop links**: paste a Workshop link and it plays when Steam has downloaded it.
- **Instant changes**: a new wallpaper fades in over the old one, and you can go back for a few seconds.
- **Fill, Fit or Stretch**, with a live preview; drag or zoom to choose which part shows.
- **Sound**: on or off with a volume slider; videos and scene music.
- **Several screens**, each with its own wallpaper, or one picture across all of them.
- **Quality modes**: Fast, Sharp and Anime (Anime4K upscaling).
- **Stays out of the way**: stops while a game is fullscreen, windows cover the desktop, or a laptop is on battery.
  Shows how much memory, CPU and GPU it uses.
- **Updates itself**: when a new version is out, Wisp asks and installs it (from 0.3.1 on).

## Download

Get **Wisp.zip** from the [latest release](../../releases/latest), unzip it anywhere, and run `wisp.exe`.
Wisp sits in the tray; click its icon to open the app.

Windows may warn that the app is unrecognized (it isn't code-signed): click **More info**, then **Run anyway**.

## Requirements

- Windows 10 or 11, 64-bit
- For Wallpaper Engine scenes and Workshop items: Wallpaper Engine installed through Steam
  (scenes use its built-in effects)

## Building from source

Needs Rust and the MinGW toolchain (`x86_64-pc-windows-gnu`). Put `libmpv-2.dll` and `vulkan-1.dll`
from the [mpv-winbuild-cmake](https://github.com/shinchiro/mpv-winbuild-cmake/releases) `mpv-dev-x86_64`
build in a `vendor` folder, then run:

```
powershell -ExecutionPolicy Bypass -File tools\package.ps1
```

The app lands in `dist\`.

## License

Wisp is free to use, but it is not open source: the code is here to read, not to copy or reuse.
See [LICENSE.txt](LICENSE.txt).
It includes mpv and Anime4K under their own licenses: see [LICENSES.txt](LICENSES.txt).
