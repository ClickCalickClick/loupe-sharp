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
#[derive(Debug, Clone, PartialEq)]
pub struct SharpKey {
    /// Full resolution texture the render is based on
    source: gdk::Texture,
    /// Physical pixel size of the render
    width: u32,
    height: u32,
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

        let source = frame_buffer.single_full_texture()?;
        if !sharp_supported_format(source.format()) {
            return None;
        }

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
                let result =
                    gio::spawn_blocking(move || resample(&job_key.source, job_key.width, job_key.height))
                        .await;

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
            let viewport =
                graphene::Rect::new(0., 0., sharp.width() as f32, sharp.height() as f32);
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
            let path = std::path::Path::new(&dir).join(format!("widget-{}.png", obj.basename().unwrap_or_default()));
            let _ = renderer.render_texture(node, Some(&viewport)).save_to_png(&path);
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

fn sharp_supported_format(format: gdk::MemoryFormat) -> bool {
    !matches!(
        format,
        gdk::MemoryFormat::R16g16b16Float
            | gdk::MemoryFormat::R16g16b16a16FloatPremultiplied
            | gdk::MemoryFormat::R16g16b16a16Float
            | gdk::MemoryFormat::R32g32b32Float
            | gdk::MemoryFormat::R32g32b32a32FloatPremultiplied
            | gdk::MemoryFormat::R32g32b32a32Float
    )
}

fn is_high_bit_depth(format: gdk::MemoryFormat) -> bool {
    matches!(
        format,
        gdk::MemoryFormat::R16g16b16
            | gdk::MemoryFormat::R16g16b16a16Premultiplied
            | gdk::MemoryFormat::R16g16b16a16
            | gdk::MemoryFormat::G16
            | gdk::MemoryFormat::G16a16
            | gdk::MemoryFormat::G16a16Premultiplied
            | gdk::MemoryFormat::A16
    )
}

/// Resample texture to `width` x `height` with Lanczos3
///
/// Works on premultiplied data such that transparent areas do not bleed.
/// Runs in a worker thread.
fn resample(source: &gdk::Texture, width: u32, height: u32) -> anyhow::Result<gdk::Texture> {
    let (format, pixel_type, bpp) = if is_high_bit_depth(source.format()) {
        (
            gdk::MemoryFormat::R16g16b16a16Premultiplied,
            fr::PixelType::U16x4,
            8,
        )
    } else {
        (
            gdk::MemoryFormat::R8g8b8a8Premultiplied,
            fr::PixelType::U8x4,
            4,
        )
    };

    let src_width = source.width() as u32;
    let src_height = source.height() as u32;

    let mut downloader = gdk::TextureDownloader::new(source);
    downloader.set_format(format);
    let (bytes, stride) = downloader.download_bytes();

    let row = src_width as usize * bpp;
    let packed;
    let src_data: &[u8] = if stride == row {
        &bytes
    } else {
        packed = bytes
            .chunks(stride)
            .flat_map(|x| &x[..row])
            .copied()
            .collect::<Vec<u8>>();
        &packed
    };

    let src = fr::images::ImageRef::new(src_width, src_height, src_data, pixel_type)?;
    let mut dst = fr::images::Image::new(width, height, pixel_type);

    let options = fr::ResizeOptions::new()
        .resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3))
        // Data is already premultiplied
        .use_alpha(false);
    fr::Resizer::new().resize(&src, &mut dst, &options)?;

    let texture = gdk::MemoryTexture::new(
        width as i32,
        height as i32,
        format,
        &glib::Bytes::from_owned(dst.into_vec()),
        width as usize * bpp,
    );

    Ok(texture.upcast())
}
