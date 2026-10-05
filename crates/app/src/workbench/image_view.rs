//! Image files open in an image preview tab: the picture centered at 100%
//! (shrunk to fit when it's larger), click to zoom in and ⌥-click to zoom out, scrolling when
//! it's bigger than the editor; the status bar shows its size.

use std::path::Path;
use std::sync::Arc;

use render::{Canvas, Rect, TextStyle};

use super::{Focus, Hit, Workbench, UI};
use crate::editor::{Doc, EditorState};

/// The zoom steps.
const ZOOMS: &[f32] = &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.5, 2.0, 3.0, 5.0, 7.0, 10.0, 15.0, 20.0];

pub(crate) struct ImagePreview {
    image: Result<Arc<render::Image>, String>,
    bytes: u64,
    /// None: fit (100%, or smaller to fit); else a zoom factor.
    zoom: Option<f32>,
    /// Scroll offset when zoomed in past the view.
    scroll: (f32, f32),
    /// The scale drawn last (for zooming from "fit").
    shown: f32,
    view: Rect,
}

impl ImagePreview {
    pub(crate) fn load(path: &Path) -> Self {
        let bytes = std::fs::read(path);
        let len = bytes.as_ref().map_or(0, |b| b.len() as u64);
        let image = bytes.map_err(|e| e.to_string()).and_then(|b| crate::imageio::decode(&b)).map(Arc::new);
        ImagePreview { image, bytes: len, zoom: None, scroll: (0.0, 0.0), shown: 1.0, view: Rect::default() }
    }

    /// "1024x768" and the file size, for the status bar.
    pub fn status(&self) -> Vec<String> {
        let size = match self.bytes {
            b if b >= 1 << 20 => format!("{:.2}MB", b as f64 / (1 << 20) as f64),
            b if b >= 1 << 10 => format!("{:.2}KB", b as f64 / 1024.0),
            b => format!("{b}B"),
        };
        match &self.image {
            Ok(img) => vec![format!("{}x{}", img.width, img.height), size, format!("{}%", (self.shown * 100.0).round())],
            Err(_) => vec![size],
        }
    }

    fn zoom_step(&mut self, out: bool) {
        let cur = self.zoom.unwrap_or(self.shown);
        let next = if out { ZOOMS.iter().rev().find(|&&z| z < cur - 1e-3) } else { ZOOMS.iter().find(|&&z| z > cur + 1e-3) };
        if let Some(&z) = next {
            self.zoom = Some(z);
        }
    }

    pub fn scroll_by(&mut self, dx: f32, dy: f32) {
        self.scroll.0 -= dx;
        self.scroll.1 -= dy;
    }
}

impl Workbench {
    /// Opens `path` in an image preview tab (a preview tab when `preview`).
    pub(super) fn open_image(&mut self, path: &Path, preview: bool) {
        let g = self.active_group;
        let existing = self.groups[g].tabs.iter().position(|t| t.image.is_some() && self.docs[t.doc].as_ref().and_then(|d| d.buffer.path()) == Some(path));
        if let Some(i) = existing {
            self.groups[g].active = i;
            return;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut doc = Doc::virtual_named(&name);
        doc.set_path(path.to_path_buf());
        let doc = self.add_doc(doc);
        let mut ed = EditorState::new(doc);
        ed.image = Some(Box::new(ImagePreview::load(path)));
        ed.preview = preview && self.settings.bool("workbench.editor.enablePreview");
        let group = &mut self.groups[g];
        // A preview replaces the current preview tab.
        match group.tabs.iter().position(|t| t.preview).filter(|_| ed.preview) {
            Some(i) => {
                group.tabs[i] = ed;
                group.active = i;
            }
            None => {
                let at = if group.tabs.is_empty() { 0 } else { group.active + 1 };
                group.tabs.insert(at, ed);
                group.active = at;
            }
        }
        if let Some(tree) = &mut self.tree {
            tree.reveal(path);
        }
        self.focus = Focus::Editor;
    }

    pub(super) fn click_image(&mut self, g: usize, alt: bool) {
        self.active_group = g;
        let gr = &mut self.groups[g];
        if let Some(pv) = gr.tabs.get_mut(gr.active).and_then(|t| t.image.as_mut()) {
            pv.zoom_step(alt);
        }
    }

    /// Draws group `g`'s image preview in `r`.
    pub(super) fn draw_image(&mut self, c: &mut Canvas, g: usize, r: Rect) {
        let gr = &mut self.groups[g];
        let Some(pv) = gr.tabs.get_mut(gr.active).and_then(|t| t.image.as_mut()) else { return };
        c.fill(r, self.theme.color("editor.background"));
        pv.view = r;
        match &pv.image {
            Ok(img) => {
                let (iw, ih) = (img.width as f32, img.height as f32);
                // At 100% an image pixel is a point (CSS pixels).
                let px = 1.0;
                let fit = (r.w / (iw * px)).min(r.h / (ih * px)).min(1.0);
                let scale = pv.zoom.unwrap_or(fit);
                pv.shown = scale;
                let (w, h) = (iw * px * scale, ih * px * scale);
                let max = ((w - r.w).max(0.0) / 2.0, (h - r.h).max(0.0) / 2.0);
                pv.scroll = (pv.scroll.0.clamp(-max.0, max.0), pv.scroll.1.clamp(-max.1, max.1));
                let x = r.x + (r.w - w) / 2.0 - pv.scroll.0;
                let y = r.y + (r.h - h) / 2.0 - pv.scroll.1;
                c.push_clip(r);
                // A checkerboard shows through transparent parts.
                let (light, dark) = (self.theme.color("editor.background"), self.theme.color("editorWidget.background"));
                let img_r = Rect::new(x, y, w, h).intersect(&r);
                let cell = 8.0;
                if img.rgba.chunks_exact(4).any(|p| p[3] < 255) && img_r.w * img_r.h < 4_000_000.0 {
                    let mut yy = img_r.y;
                    while yy < img_r.bottom() {
                        let mut xx = img_r.x;
                        while xx < img_r.right() {
                            let odd = (((xx - x) / cell) as i64 + ((yy - y) / cell) as i64) % 2 == 1;
                            c.fill(Rect::new(xx, yy, cell.min(img_r.right() - xx), cell.min(img_r.bottom() - yy)), if odd { dark } else { light });
                            xx += cell;
                        }
                        yy += cell;
                    }
                }
                c.image(Rect::new(x, y, w, h), img);
                c.pop_clip();
            }
            Err(e) => {
                let style = TextStyle::ui(UI, self.theme.color("descriptionForeground"));
                c.text_in(Rect::new(r.x + 20.0, r.y + 10.0, r.w - 40.0, 22.0), &format!("The image couldn't be shown: {e}"), &style);
            }
        }
        self.hits.push((r, Hit::Image(g)));
    }
}
