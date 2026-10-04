// Copyright (c) 2026 Loupe Sharp contributors
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! High quality downscaling for zoomed out raster images
//!
//! When an image is displayed smaller than its original size, the GPU path
//! scales the full resolution texture with mipmaps. For large photos at
//! ratios between two mipmap levels this produces aliased, jagged edges.
//!
//! Once the zoom level settles, the full resolution texture is resampled on
//! the CPU with a Lanczos3 filter to exactly the size it occupies in physical
//! pixels. The result is drawn without any further scaling.

use std::time::Duration;

use fast_image_resize as fr;

use super::*;

/// Time the zoom has to stay unchanged before a sharp render is started
const SETTLE_DELAY: Duration = Duration::from_millis(120);

/// Identifies what a sharp render was produced for
#[derive(Debug, Clone)]
pub struct SharpKey {
    /// Full resolution texture the render is based on
    source: gdk::Texture,
    /// Decoded pixels of `source`, if available without copying
    pixels: Option<tiling::SourcePixels>,
    /// Physical pixel size of the render
    width: u32,
    height: u32,
}

impl PartialEq for SharpKey {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.width == other.width && self.height == other.height
    }
}

#[derive(Debug)]
pub struct SharpRender {
    key: SharpKey,
    texture: gdk::Texture,
}

impl imp::LpImage {
    /// Returns the size at which the image needs a sharp render
    ///
    /// `None` if the image is not shown downscaled or not suitable.
    fn sharp_key(&self, frame_buffer: &tiling::FrameBuffer) -> Option<SharpKey> {
        if std::env::var_os("LOUPE_NO_SHARP").is_some_and(|x| !x.is_empty()) {
            return None;
        }

        let obj = self.obj();
        if obj.metadata().is_svg() || self.operations.borrow().is_some() {
            return None;
        }

        let (source, pixels) = frame_buffer.single_full_texture()?;

        // Zoom in physical pixels
        let zoom = tiling::zoom_normalize(obj.zoom());
        if zoom >= 1. {
            return None;
        }

        let width = (source.width() as f64 * zoom).round() as u32;
        let height = (source.height() as f64 * zoom).round() as u32;
        if width == 0 || height == 0 {
            return None;
        }

        Some(SharpKey {
            source,
            pixels,
            width,
            height,
        })
    }

    /// Draws the sharp render if one matching the current state exists
    ///
    /// Otherwise, schedules creating one and returns `false`. The snapshot is
    /// expected to be transformed such that the image origin is at `(0, 0)`
    /// and units are application pixels.
    pub(super) fn snapshot_sharp(
        &self,
        snapshot: &gtk::Snapshot,
        frame_buffer: &tiling::FrameBuffer,
        options: &tiling::RenderOptions,
    ) -> bool {
        let Some(key) = self.sharp_key(frame_buffer) else {
            self.sharp.replace(None);
            return false;
        };

        if let Some(render) = &*self.sharp.borrow()
            && render.key == key
        {
            let area = graphene::Rect::new(0., 0., key.width as f32, key.height as f32);
            let scaling = options.scaling as f32;

            // Draw in physical pixels, one texture pixel per screen pixel
            snapshot.scale(1. / scaling, 1. / scaling);
            if let Some(background_color) = &options.background_color {
                snapshot.append_color(background_color, &area);
            }
            snapshot.append_scaled_texture(&render.texture, gsk::ScalingFilter::Nearest, &area);
            snapshot.scale(scaling, scaling);

            return true;
        }

        self.schedule_sharp();
        false
    }

    /// Start a sharp render once the zoom stopped changing
    fn schedule_sharp(&self) {
        if let Some(source_id) = self.sharp_timeout.take() {
            source_id.remove();
        }

        let source_id = glib::timeout_add_local_once(
            SETTLE_DELAY,
            glib::clone!(
                #[weak(rename_to = imp)]
                self,
                move || {
                    imp.sharp_timeout.replace(None);
                    imp.start_sharp();
                }
            ),
        );

        self.sharp_timeout.replace(Some(source_id));
    }

    fn start_sharp(&self) {
        if self.zoom_animation().state() == adw::AnimationState::Playing {
            self.schedule_sharp();
            return;
        }

        let Some(key) = self.sharp_key(&self.active_frame_buffer()) else {
            return;
        };

        if self.sharp_pending.borrow().as_ref() == Some(&key) {
            return;
        }
        self.sharp_pending.replace(Some(key.clone()));

        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = imp)]
            self,
            async move {
                let job_key = key.clone();
                let start = std::time::Instant::now();
                let result = gio::spawn_blocking(move || resample(&job_key)).await;

                if imp.sharp_pending.borrow().as_ref() == Some(&key) {
                    imp.sharp_pending.replace(None);
                }

                match result {
                    Ok(Ok(texture)) => {
                        tracing::debug!(
                            "Sharp render {}x{} took {:?}",
                            key.width,
                            key.height,
                            start.elapsed()
                        );
                        imp.capture_comparison(&texture);
                        imp.sharp.replace(Some(SharpRender { key, texture }));
                        imp.obj().queue_draw();
                        imp.capture_widget();
                    }
                    Ok(Err(err)) => tracing::warn!("Sharp render failed: {err}"),
                    Err(_) => tracing::warn!("Sharp render thread panicked"),
                }
            }
        ));
    }

    /// Debug aid: save GPU-scaled and sharp renders to `LOUPE_SHARP_CAPTURE`
    fn capture_comparison(&self, sharp: &gdk::Texture) {
        let Some(dir) = std::env::var_os("LOUPE_SHARP_CAPTURE") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);

        let Some(renderer) = self.obj().native().and_then(|x| x.renderer()) else {
            return;
        };

        let scaling = self.scaling();
        let options = tiling::RenderOptions {
            scaling_filter: gsk::ScalingFilter::Trilinear,
            scaling,
            background_color: None,
        };
        let snapshot = gtk::Snapshot::new();
        // Undo the conversion to application pixels to get a physical pixel render
        snapshot.scale(scaling as f32, scaling as f32);
        self.active_frame_buffer()
            .add_to_snapshot(&snapshot, self.applicable_zoom(), &options);

        if let Some(node) = snapshot.to_node() {
            let viewport = graphene::Rect::new(0., 0., sharp.width() as f32, sharp.height() as f32);
            let gpu = renderer.render_texture(node, Some(&viewport));
            let _ = gpu.save_to_png(dir.join("gpu-trilinear.png"));
        }
        let _ = sharp.save_to_png(dir.join("sharp-lanczos3.png"));
        tracing::info!("Saved comparison renders to {}", dir.display());
    }

    /// Debug aid: save the widget as drawn on screen to `LOUPE_SHARP_CAPTURE`
    fn capture_widget(&self) {
        let Some(dir) = std::env::var_os("LOUPE_SHARP_CAPTURE") else {
            return;
        };
        let obj = self.obj();
        let Some(renderer) = obj.native().and_then(|x| x.renderer()) else {
            return;
        };

        let scaling = self.scaling() as f32;
        let (width, height) = (obj.width() as f32, obj.height() as f32);
        let snapshot = gtk::Snapshot::new();
        snapshot.scale(scaling, scaling);
        WidgetImpl::snapshot(self, &snapshot);

        if let Some(node) = snapshot.to_node() {
            let viewport = graphene::Rect::new(0., 0., width * scaling, height * scaling);
            let path = std::path::Path::new(&dir)
                .join(format!("widget-{}.png", obj.basename().unwrap_or_default()));
            let _ = renderer
                .render_texture(node, Some(&viewport))
                .save_to_png(&path);
            tracing::info!("Saved widget render to {}", path.display());
        }
    }

    /// Drop sharp render, for example when the image is replaced
    pub(super) fn clear_sharp(&self) {
        if let Some(source_id) = self.sharp_timeout.take() {
            source_id.remove();
        }
        self.sharp.replace(None);
        self.sharp_pending.replace(None);
    }
}

/// How the resizer has to treat a memory format
///
/// Returns the pixel type and whether the alpha channel is unpremultiplied
/// and last. `None` if the format can't be resized as is.
fn resize_layout(format: gdk::MemoryFormat) -> Option<(fr::PixelType, bool)> {
    use fr::PixelType::*;
    use gdk::MemoryFormat as F;

    Some(match format {
        F::B8g8r8a8Premultiplied
        | F::A8r8g8b8Premultiplied
        | F::R8g8b8a8Premultiplied
        | F::A8b8g8r8Premultiplied
        | F::B8g8r8x8
        | F::X8r8g8b8
        | F::R8g8b8x8
        | F::X8b8g8r8 => (U8x4, false),
        F::B8g8r8a8 | F::R8g8b8a8 => (U8x4, true),
        F::R8g8b8 | F::B8g8r8 => (U8x3, false),
        F::G8 | F::A8 => (U8, false),
        F::G8a8Premultiplied => (U8x2, false),
        F::G8a8 => (U8x2, true),
        F::R16g16b16 => (U16x3, false),
        F::R16g16b16a16Premultiplied => (U16x4, false),
        F::R16g16b16a16 => (U16x4, true),
        F::G16 | F::A16 => (U16, false),
        F::G16a16Premultiplied => (U16x2, false),
        F::G16a16 => (U16x2, true),
        F::R32g32b32Float => (F32x3, false),
        F::R32g32b32a32FloatPremultiplied => (F32x4, false),
        F::R32g32b32a32Float => (F32x4, true),
        F::A32Float => (F32, false),
        // Half floats and alpha-first straight alpha
        _ => return None,
    })
}

/// Resample the image to the key's size with Lanczos3
///
/// The output keeps the source's memory format and color state, such that
/// GTK's color handling is the same as for the original texture. If the
/// decoder's buffer is available, it's read in place without a copy.
/// Runs in a worker thread.
fn resample(key: &SharpKey) -> anyhow::Result<gdk::Texture> {
    let source = &key.source;
    let src_width = source.width() as u32;
    let src_height = source.height() as u32;
    let color_state = source.color_state();

    let direct = key
        .pixels
        .as_ref()
        .and_then(|pixels| Some((pixels, resize_layout(source.format())?)));

    let downloaded;
    let (bytes, stride, format, pixel_type, unpremultiplied): (&[u8], usize, _, _, _) =
        if let Some((pixels, (pixel_type, unpremultiplied))) = direct {
            (
                &pixels.bytes,
                pixels.stride,
                source.format(),
                pixel_type,
                unpremultiplied,
            )
        } else {
            // Rare formats: convert to float, keeping the color state
            let format = gdk::MemoryFormat::R32g32b32a32FloatPremultiplied;
            let mut downloader = gdk::TextureDownloader::new(source);
            downloader.set_format(format);
            downloader.set_color_state(&color_state);
            let (bytes, stride) = downloader.download_bytes();
            downloaded = bytes;
            (&downloaded, stride, format, fr::PixelType::F32x4, false)
        };

    let row = src_width as usize * pixel_type.size();
    let rows = (src_height as usize - 1) * stride + row;
    anyhow::ensure!(bytes.len() >= rows, "Pixel buffer too small");

    let mut dst = fr::images::Image::new(key.width, key.height, pixel_type);
    let options = fr::ResizeOptions::new()
        .resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3))
        .use_alpha(unpremultiplied);
    let mut resizer = fr::Resizer::new();

    let path = if key.pixels.is_some() && direct.is_some() {
        "decoder buffer"
    } else {
        "converted copy"
    };
    tracing::debug!(
        "Resampling {:?} ({}) from {path}",
        source.format(),
        color_state_name(&color_state)
    );

    match fr::images::ImageRef::new(src_width, src_height, bytes, pixel_type) {
        Ok(src) if stride == row => resizer.resize(&src, &mut dst, &options)?,
        _ => {
            tracing::debug!("Repacking rows (stride {stride}, row {row})");
            // The resizer needs tightly packed, aligned rows
            let mut src = fr::images::Image::new(src_width, src_height, pixel_type);
            for (y, out) in src.buffer_mut().chunks_exact_mut(row).enumerate() {
                out.copy_from_slice(&bytes[y * stride..y * stride + row]);
            }
            resizer.resize(&src, &mut dst, &options)?;
        }
    }

    let texture = gdk::MemoryTextureBuilder::new()
        .set_bytes(Some(&glib::Bytes::from_owned(dst.into_vec())))
        .set_width(key.width as i32)
        .set_height(key.height as i32)
        .set_stride(key.width as usize * pixel_type.size())
        .set_format(format)
        .set_color_state(&color_state)
        .build();

    Ok(texture)
}

fn color_state_name(color_state: &gdk::ColorState) -> String {
    for (name, known) in [
        ("sRGB", gdk::ColorState::srgb()),
        ("sRGB linear", gdk::ColorState::srgb_linear()),
        ("Rec.2100 PQ", gdk::ColorState::rec2100_pq()),
        ("Rec.2100 linear", gdk::ColorState::rec2100_linear()),
    ] {
        if *color_state == known {
            return name.to_string();
        }
    }
    color_state
        .create_cicp_params()
        .map(|x| {
            format!(
                "CICP {}/{}/{}",
                x.color_primaries(),
                x.transfer_function(),
                x.matrix_coefficients()
            )
        })
        .unwrap_or_else(|| "other".to_string())
}
