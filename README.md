# Loupe Sharp

This is a small fork of [Loupe](https://gitlab.gnome.org/GNOME/loupe), GNOME's Image Viewer. It changes one thing: how large photos look when they are shown smaller than their full size.

**Why:** I shoot 60 MP photos, and when Loupe fits one to the window, edges like hair, eyelashes and fabric come out soft but still a bit jagged. Loupe hands the full-resolution image to the GPU and lets mipmaps do the shrinking. That's fast, but mipmaps only step down in halves, so a ratio like 1:3.7 lands between two levels.

**What this fork does:** once the zoom stops changing (about 0.1 s), it resamples the full image on the CPU with a Lanczos3 filter to exactly the number of physical screen pixels it covers, and then draws that 1:1 with no further scaling. While you're zooming, the normal GPU path is used, so it stays smooth. On a 60 MP JPEG the resample takes roughly 0.04–0.17 s, in the background.

I checked that what ends up on screen matches the resampled image pixel for pixel, including with fractional scaling (167%), EXIF rotation and mirroring.

![Zone plate test pattern, stock Loupe on the left, this fork on the right](docs/zoneplate-comparison.png)

*A zone plate test pattern shown fit-to-window. Left: stock Loupe 50.0. Right: this fork. The rings get finer towards the edge. Past the point the screen can show, the correct result is plain grey. Stock Loupe blurs the visible rings early and adds false ring patterns; the fork keeps them crisp and then goes cleanly grey.*

Everything else (decoding via glycin, colour management, gestures, editing, the interface) is Loupe as upstream ships it. Credit for all of that goes to the Loupe developers. The change itself is one file, [`src/widgets/image/sharp.rs`](src/widgets/image/sharp.rs), plus a few small hooks.

## Install (Arch / CachyOS)

The package replaces `loupe` and keeps the same app ID, so it stays your default image viewer.

```bash
git clone https://github.com/ClickCalickClick/loupe-sharp.git
cd loupe-sharp/packaging/arch
makepkg -si
```

It needs a Rust toolchain (`rust` or `rustup`). With CachyOS's `makepkg.conf` it's built with `-C target-cpu=native`.

Pacman won't update it when Arch ships a new Loupe. When that happens, rebase this branch on the new release tag and rebuild.

## Settings for testing

- `LOUPE_NO_SHARP=1` turns the feature off, for comparing.
- `LOUPE_SHARP_CAPTURE=<dir>` saves `gpu-trilinear.png` (what stock Loupe draws), `sharp-lanczos3.png` and `widget-<file>.png` (what is actually on screen) each time a sharp render finishes.

The branch with the changes is `sharp-downscale`, on top of upstream tag `50.0`. If this turns out to be useful beyond my own setup, I'd be glad to take it upstream.

---

# Image Viewer (Loupe)

<a href='https://flathub.org/apps/org.gnome.Loupe'><img width='240' alt='Download on Flathub' src='https://flathub.org/api/badge?svg&locale=en'/></a>

Loupe is GNOME's default Image Viewer.


## Technical Details

- Fast GPU-accelerated image rendering with tiled rendering for SVGs
- Extendable and sandboxed image decoding via [glycin](https://gnome.pages.gitlab.gnome.org/glycin/)
- Support for more than 15 image formats by default
- Editing with crop, rotate, and flip for PNG and JPEG
- Extensive support for touchpad and touchscreen gestures
- Accessible presentation of the most important metadata
- Sleek but powerful interface developed in conjunction with GNOME Human Interface Guidelines

![Image Viewer Screenshot](https://static.gnome.org/appdata/gnome-48/loupe/loupe-main.png)

## Supported Image Formats

Image Viewer uses [glycin](https://gitlab.gnome.org/GNOME/glycin) for loading images. You can check [glycin's README](https://gitlab.gnome.org/GNOME/glycin#supported-image-formats) for more details about the formats supported by the default loaders. However, glycin supports adding loaders for additional formats. Therefore, the supported formats on your system may vary and might be changed by installing or removing glycin loaders.

## Contributing

For informations on how to contribute, please check the [CONTRIBUTING.md](CONTRIBUTING.md).
