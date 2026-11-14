//! Showing PNG files as images, with fit/zoom, panning and a checkerboard
//! behind transparent pixels.

use std::path::Path;

use egui::{
    pos2, vec2, Color32, ColorImage, Rect, RichText, Sense, TextureFilter, TextureHandle, TextureOptions,
    TextureWrapMode, Ui,
};

/// Zoom limits, as a scale factor.
const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 64.0;
/// Size of one checkerboard square, in points.
const CHECKER_SIZE: f32 = 8.0;

/// Whether `path` is an image Zbugger shows as a picture rather than text.
pub fn is_image(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"))
}

pub struct ImageView {
    /// Size of the image in the file, in pixels.
    width: u32,
    height: u32,
    file_size: u64,
    /// Decoded pixels, until they're uploaded to the GPU on first display.
    pixels: Option<image::RgbaImage>,
    texture: Option<TextureHandle>,
    checker: Option<TextureHandle>,
    /// Shown smaller than the file because it exceeds the GPU's texture limit.
    downscaled: bool,
    /// `None` fits the image to the view.
    zoom: Option<f32>,
    /// The zoom that fit the image to the view last frame (for the toolbar,
    /// which is drawn before the view is measured).
    fit_zoom: f32,
}

impl ImageView {
    /// Decode a PNG file.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("not a readable PNG: {e}")))?;
        let pixels = image.to_rgba8();
        Ok(Self {
            width: pixels.width(),
            height: pixels.height(),
            file_size: bytes.len() as u64,
            pixels: Some(pixels),
            texture: None,
            checker: None,
            downscaled: false,
            zoom: None,
            fit_zoom: 1.0,
        })
    }

    #[cfg(test)]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Upload the pixels to the GPU the first time the image is shown,
    /// scaling down anything larger than the GPU allows.
    fn texture(&mut self, ui: &Ui, name: &str) -> &TextureHandle {
        if self.texture.is_none() {
            let mut pixels = self.pixels.take().unwrap_or_default();
            let max_side = ui.ctx().input(|i| i.max_texture_side) as u32;
            if pixels.width() > max_side || pixels.height() > max_side {
                let scale = max_side as f32 / pixels.width().max(pixels.height()) as f32;
                let (w, h) = ((pixels.width() as f32 * scale) as u32, (pixels.height() as f32 * scale) as u32);
                pixels = image::imageops::resize(&pixels, w.max(1), h.max(1), image::imageops::FilterType::Triangle);
                self.downscaled = true;
            }
            let size = [pixels.width() as usize, pixels.height() as usize];
            let image = ColorImage::from_rgba_unmultiplied(size, pixels.as_raw());
            let options = TextureOptions {
                // Crisp pixels when zoomed in, smooth when zoomed out.
                magnification: TextureFilter::Nearest,
                minification: TextureFilter::Linear,
                wrap_mode: TextureWrapMode::ClampToEdge,
                mipmap_mode: Some(TextureFilter::Linear),
            };
            self.texture = Some(ui.ctx().load_texture(name, image, options));
        }
        self.texture.as_ref().expect("just created")
    }

    /// A 2×2 checker that repeats, drawn behind the image.
    fn checker(&mut self, ui: &Ui) -> &TextureHandle {
        self.checker.get_or_insert_with(|| {
            let (light, dark) = if ui.visuals().dark_mode {
                (Color32::from_gray(70), Color32::from_gray(50))
            } else {
                (Color32::from_gray(255), Color32::from_gray(220))
            };
            let image = ColorImage { size: [2, 2], pixels: vec![light, dark, dark, light] };
            ui.ctx().load_texture("image-checker", image, TextureOptions::NEAREST_REPEAT)
        })
    }

    pub fn ui(&mut self, ui: &mut Ui, path: &Path) {
        let name = format!("image:{}", path.display());
        let texture_id = self.texture(ui, &name).id();
        let checker_id = self.checker(ui).id();
        let (width, height) = (self.width as f32, self.height as f32);

        // Toolbar: size, zoom and zoom buttons.
        ui.horizontal(|ui| {
            ui.label(format!("{} × {} px", self.width, self.height));
            ui.label(RichText::new(format_size(self.file_size)).weak());
            if self.downscaled {
                ui.label(RichText::new("shown downscaled (larger than the GPU's texture limit)").weak())
                    .on_hover_text("Zooming past this resolution shows the downscaled copy.");
            }
            ui.separator();
            if ui.selectable_label(self.zoom.is_none(), "Fit").on_hover_text("Fit the image to the view").clicked() {
                self.zoom = None;
            }
            if ui.selectable_label(self.zoom == Some(1.0), "1:1").on_hover_text("Actual size").clicked() {
                self.zoom = Some(1.0);
            }
            if ui.button("−").on_hover_text("Zoom out (Ctrl+wheel)").clicked() {
                self.zoom = Some((self.current_zoom() / 1.25).max(MIN_ZOOM));
            }
            if ui.button("+").on_hover_text("Zoom in (Ctrl+wheel)").clicked() {
                self.zoom = Some((self.current_zoom() * 1.25).min(MAX_ZOOM));
            }
            ui.label(format!("{:.0}%", self.current_zoom() * 100.0));
        });
        ui.separator();

        egui::ScrollArea::both().id_salt(&name).auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
            // Fit shrinks large images to the view but never enlarges small ones.
            self.fit_zoom = (viewport.width() / width).min(viewport.height() / height).min(1.0);
            let zoom = self.current_zoom();
            let size = vec2(width * zoom, height * zoom);
            let (area, response) = ui.allocate_exact_size(size.max(viewport.size()), Sense::drag());
            let image_rect = Rect::from_center_size(area.center(), size);

            // Checkerboard behind transparent pixels: a repeating texture, so
            // it's one quad however large the image is.
            let tiles = image_rect.size() / (2.0 * CHECKER_SIZE);
            let painter = ui.painter();
            painter.image(checker_id, image_rect, Rect::from_min_max(pos2(0.0, 0.0), tiles.to_pos2()), Color32::WHITE);
            painter.image(texture_id, image_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);

            if response.hovered() {
                // Ctrl+wheel (or a pinch) zooms.
                let factor = ui.input(|i| i.zoom_delta());
                if factor != 1.0 {
                    self.zoom = Some((zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM));
                }
                ui.ctx().set_cursor_icon(if response.dragged() {
                    egui::CursorIcon::Grabbing
                } else if size.x > viewport.width() || size.y > viewport.height() {
                    egui::CursorIcon::Grab
                } else {
                    egui::CursorIcon::Default
                });
            }
            // Drag to pan.
            if response.dragged() {
                ui.scroll_with_delta(response.drag_delta());
            }
        });
    }

    fn current_zoom(&self) -> f32 {
        self.zoom.unwrap_or(self.fit_zoom)
    }
}

fn format_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / (1 << 10) as f64),
        b => format!("{b} bytes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_png_size_and_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dot.png");
        let mut image = image::RgbaImage::new(3, 2);
        image.put_pixel(2, 1, image::Rgba([255, 0, 0, 128]));
        image.save(&path).unwrap();

        let view = ImageView::load(&path).unwrap();
        assert_eq!(view.size(), (3, 2));
        assert_eq!(view.pixels.as_ref().unwrap().get_pixel(2, 1).0, [255, 0, 0, 128]);
        assert!(is_image(&path));
        assert!(is_image(Path::new("ICON.PNG")));
        assert!(!is_image(Path::new("main.cpp")));
    }

    #[test]
    fn rejects_files_that_are_not_png() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake.png");
        std::fs::write(&path, "not an image").unwrap();
        let err = ImageView::load(&path).err().expect("an error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn png_documents_open_as_images() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Logo.PNG");
        image::RgbaImage::new(4, 4).save_with_format(&path, image::ImageFormat::Png).unwrap();

        let mut doc = crate::document::Document::open(&path).unwrap();
        assert_eq!(doc.image.as_ref().map(ImageView::size), Some((4, 4)));
        assert_eq!(doc.language_name(), "PNG Image");
        assert!(doc.editor.text().is_empty(), "no text is loaded");
        // Saving does nothing and never touches the file.
        let before = std::fs::read(&path).unwrap();
        doc.save().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!doc.is_dirty());
    }

    #[test]
    fn sizes_read_nicely() {
        assert_eq!(format_size(512), "512 bytes");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0 MB");
    }
}
