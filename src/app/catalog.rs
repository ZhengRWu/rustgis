//! 本地目录浏览面板：浏览文件系统并将 GIS 数据加入当前工程。

use std::path::{Path, PathBuf};

use egui::Ui;

use super::RustGisApp;

pub(super) struct Catalog {
    pub open: bool,
    location: PathBuf,
    address: String,
    error: Option<String>,
    pub(super) dragging_file: Option<PathBuf>,
}

impl Default for Catalog {
    fn default() -> Self {
        let location = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self { open: false, address: location.display().to_string(), location, error: None, dragging_file: None }
    }
}

impl Catalog {
    fn navigate(&mut self, path: PathBuf) {
        if path.is_dir() {
            self.location = path;
            self.address = self.location.display().to_string();
            self.error = None;
        } else {
            self.error = Some(path.display().to_string());
        }
    }
}

impl RustGisApp {
    pub(super) fn catalog_panel(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.heading(self.t("window.catalog"));
            if ui.small_button("×").on_hover_text(self.t("catalog.close")).clicked() {
                self.catalog.open = false;
            }
        });
        ui.separator();

        let mut destination = None;
        ui.horizontal(|ui| {
            if ui.small_button("←").on_hover_text(self.t("catalog.parent")).clicked() {
                destination = self.catalog.location.parent().map(Path::to_path_buf);
            }
            ui.label(self.t("catalog.location"));
        });
        let address_hint = self.t("catalog.addressHint").to_string();
        let address = ui.add(egui::TextEdit::singleline(&mut self.catalog.address)
            .desired_width(f32::INFINITY)
            .hint_text(address_hint));
        if address.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
            destination = Some(PathBuf::from(self.catalog.address.trim()));
        }
        if let Some(path) = destination { self.catalog.navigate(path); }
        if let Some(error) = &self.catalog.error {
            ui.colored_label(egui::Color32::DARK_RED, self.tf("catalog.invalid", &[error]));
        }
        ui.separator();

        ui.label(self.t("catalog.system"));
        ui.horizontal_wrapped(|ui| {
            for root in system_roots() {
                if ui.selectable_label(root == self.catalog.location, root.display().to_string()).clicked() {
                    self.catalog.navigate(root);
                }
            }
        });
        ui.separator();
        let location = self.catalog.location.clone();
        egui::ScrollArea::vertical().id_salt("catalog_tree").show(ui, |ui| {
            self.catalog_directory(ui, &location);
        });
        self.paint_drag_ghost(ui);
    }

    /// 只绘制当前目录的直接子项，进入文件夹后刷新当前位置。
    fn catalog_directory(&mut self, ui: &mut Ui, path: &Path) {
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => {
                let mut values: Vec<_> = entries.filter_map(Result::ok).collect();
                values.sort_by_key(|e| (!e.path().is_dir(), e.file_name().to_string_lossy().to_lowercase()));
                values
            }
            Err(_) => {
                ui.weak(self.tf("catalog.unreadable", &[&path.display().to_string()]));
                return;
            }
        };
        for entry in entries {
            let child = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if child.is_dir() {
                if ui.button(format!("📁  {name}")).clicked() {
                    self.catalog.navigate(child);
                }
            } else {
                let supported = super::import::is_supported_drop(&child);
                // 仅可拖入导入的文件使用浅蓝底和蓝色边框，作为可操作项的视觉提示。
                let mut button = egui::Button::new(format!("📄  {name}"));
                if supported {
                    button = button
                        .fill(crate::style::ACCENT_HOVER_BG)
                        .stroke(egui::Stroke::new(1.0_f32, crate::style::ACCENT_HOVER_STROKE))
                        .sense(egui::Sense::click_and_drag());
                } else {
                    button = button.frame(false);
                }
                let response = ui.add(button);
                response.clone().on_hover_text(child.display().to_string());
                if supported && response.drag_started() {
                    self.catalog.dragging_file = Some(child.clone());
                }
                if response.double_clicked() && supported {
                    self.drop_queue.push_back(child);
                    self.poll_drop_queue();
                }
            }
        }
    }

    fn paint_drag_ghost(&self, ui: &Ui) {
        let Some(path) = self.catalog.dragging_file.as_ref() else { return; };
        let Some(pointer) = ui.ctx().input(|input| input.pointer.latest_pos()) else { return; };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let text = format!("📄  {name}");
        let font = egui::FontId::proportional(14.0);
        let galley = ui.painter().layout_no_wrap(text, font, egui::Color32::BLACK);
        let size = galley.size() + egui::vec2(20.0, 12.0);
        let rect = egui::Rect::from_min_size(pointer + egui::vec2(14.0, 16.0), size);
        let painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Tooltip,
            egui::Id::new("catalog_drag_ghost"),
        ));
        painter.rect_filled(rect, 4.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 225));
        painter.rect_stroke(
            rect,
            4.0,
            egui::Stroke::new(1.0_f32, crate::style::ACCENT_HOVER_STROKE),
            egui::StrokeKind::Inside,
        );
        painter.galley(rect.min + egui::vec2(10.0, 6.0), galley, egui::Color32::BLACK);
        ui.ctx().request_repaint();
    }

    /// 在地图视图中释放目录文件时，转交到现有的导入队列。
    pub(super) fn finish_catalog_drag(&mut self, ui: &Ui, map_rect: egui::Rect) {
        let released = ui.input(|input| input.pointer.button_released(egui::PointerButton::Primary));
        if !released { return; }
        let drop_pos = ui.input(|input| input.pointer.latest_pos());
        if let Some(path) = self.catalog.dragging_file.take() {
            if drop_pos.is_some_and(|pos| map_rect.contains(pos)) {
                self.drop_queue.push_back(path);
                self.poll_drop_queue();
            }
        }
    }
}

fn system_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut roots = Vec::new();
        for letter in b'A'..=b'Z' {
            let root = PathBuf::from(format!("{}:\\", letter as char));
            if root.exists() { roots.push(root); }
        }
        roots
    }
    #[cfg(not(windows))]
    { vec![PathBuf::from("/")] }
}
