//! 本地目录浏览面板：浏览文件系统并将 GIS 数据加入当前工程。

use std::path::{Path, PathBuf};

use egui::Ui;

use super::RustGisApp;

pub(super) struct Catalog {
    pub open: bool,
    /// 当前目录（`pub(super)`：新建矢量对话框要用它预填落盘路径）。
    pub(super) location: PathBuf,
    address: String,
    error: Option<String>,
    merge_shapefile: bool,
    pub(super) dragging_file: Option<PathBuf>,
    pending_action: Option<CatalogAction>,
    rename_value: String,
    feedback: Option<Result<String, String>>,
}

#[derive(Clone)]
enum CatalogAction {
    Rename(PathBuf),
    Delete(PathBuf),
    /// 新建文件夹：`PathBuf` 是**父目录**，新名字在 `rename_value` 里。
    NewFolder(PathBuf),
}

impl Default for Catalog {
    fn default() -> Self {
        let location = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            open: false,
            address: location.display().to_string(),
            location,
            error: None,
            merge_shapefile: false,
            dragging_file: None,
            pending_action: None,
            rename_value: String::new(),
            feedback: None,
        }
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
        if let Some(feedback) = &self.catalog.feedback {
            match feedback {
                Ok(message) => { ui.weak(message); }
                Err(message) => { ui.colored_label(egui::Color32::DARK_RED, message); }
            }
        }

        let mut destination = None;
        ui.horizontal(|ui| {
            if ui.small_button("←").on_hover_text(self.t("catalog.parent")).clicked() {
                destination = self.catalog.location.parent().map(Path::to_path_buf);
            }
            ui.label(self.t("catalog.location"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(26.0, 24.0),
                    egui::Sense::click(),
                );
                response.clone().on_hover_text(self.t("catalog.mergeShapefileTip"));
                if response.clicked() {
                    self.catalog.merge_shapefile = !self.catalog.merge_shapefile;
                }
                let painter = ui.painter();
                if self.catalog.merge_shapefile || response.hovered() {
                    painter.rect_filled(
                        rect,
                        3.0,
                        if self.catalog.merge_shapefile {
                            crate::style::SELECTION
                        } else {
                            crate::style::ACCENT_HOVER_BG
                        },
                    );
                    painter.rect_stroke(
                        rect,
                        3.0,
                        egui::Stroke::new(
                            1.0_f32,
                            crate::style::ACCENT_HOVER_STROKE,
                        ),
                        egui::StrokeKind::Inside,
                    );
                }
                paint_shapefile_group_icon(
                    painter,
                    rect,
                    if self.catalog.merge_shapefile {
                        crate::style::ACCENT
                    } else {
                        crate::style::TEXT_WEAK
                    },
                );
            });
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
        // 空白区域的右键菜单：先登记一块「不占布局」的背景交互区
        // （`ui.interact` 只注册控件、不推进光标），随后画的文件条目排在它上面，
        // 因此点条目走条目自己的菜单、点空白处走这里的「新建」菜单。
        let blank_rect = ui.available_rect_before_wrap();
        let blank = ui.interact(
            blank_rect,
            egui::Id::new("catalog_blank"),
            egui::Sense::click(),
        );
        self.catalog_blank_menu(&blank);
        egui::ScrollArea::vertical().id_salt("catalog_tree").show(ui, |ui| {
            self.catalog_directory(ui, &location);
        });
        self.paint_drag_ghost(ui);
        self.show_catalog_action_dialog(ui.ctx());
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
            if self.catalog.merge_shapefile && is_shapefile_sidecar(&child) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if child.is_dir() {
                let response = ui.button(format!("📁  {name}"));
                if response.clicked() {
                    self.catalog.navigate(child.clone());
                }
                response.context_menu(|ui| {
                    if ui.button(self.t("catalog.openInExplorer")).clicked() {
                        open_in_file_manager(child.clone());
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button(self.t("catalog.copy")).clicked() {
                        self.copy_catalog_item(&child);
                        ui.close_menu();
                    }
                    if ui.button(self.t("catalog.rename")).clicked() {
                        self.begin_catalog_rename(&child);
                        ui.close_menu();
                    }
                    if ui.button(self.t("catalog.delete")).clicked() {
                        self.catalog.pending_action = Some(CatalogAction::Delete(child.clone()));
                        ui.close_menu();
                    }
                });
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
                    self.drop_queue.push_back(child.clone());
                    self.poll_drop_queue();
                }
                response.context_menu(|ui| {
                    if ui.button(self.t("catalog.openInExplorer")).clicked() {
                        open_in_file_manager(child.clone());
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button(self.t("catalog.copy")).clicked() {
                        self.copy_catalog_item(&child);
                        ui.close_menu();
                    }
                    if ui.button(self.t("catalog.rename")).clicked() {
                        self.begin_catalog_rename(&child);
                        ui.close_menu();
                    }
                    if ui.button(self.t("catalog.delete")).clicked() {
                        self.catalog.pending_action = Some(CatalogAction::Delete(child.clone()));
                        ui.close_menu();
                    }
                });
            }
        }
    }

    fn copy_catalog_item(&mut self, source: &Path) {
        let copy_suffix = self.t("catalog.copySuffix").to_string();
        match copy_item_to_sibling(source, &copy_suffix) {
            Ok(destination) => self.catalog.feedback = Some(Ok(self.tf(
                "catalog.copied",
                &[&destination.file_name().unwrap_or_default().to_string_lossy()],
            ))),
            Err(error) => self.catalog.feedback = Some(Err(self.tf("catalog.operationError", &[&error]))),
        }
    }

    /// 目录栏空白处右键 →「新建」子菜单（对标 QGIS 浏览器：New → Directory… / ShapeFile…）。
    fn catalog_blank_menu(&mut self, response: &egui::Response) {
        response.context_menu(|ui| {
            let new_label = self.t("catalog.new").to_string();
            ui.menu_button(new_label, |ui| {
                if ui.button(self.t("catalog.newVector")).clicked() {
                    self.open_new_vector_dialog();
                    ui.close_menu();
                }
                if ui.button(self.t("catalog.newFolder")).clicked() {
                    self.catalog.rename_value = self.t("catalog.newFolderName").to_string();
                    self.catalog.pending_action =
                        Some(CatalogAction::NewFolder(self.catalog.location.clone()));
                    ui.close_menu();
                }
            });
        });
    }

    fn begin_catalog_rename(&mut self, path: &Path) {
        self.catalog.rename_value = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        self.catalog.pending_action = Some(CatalogAction::Rename(path.to_path_buf()));
    }

    fn show_catalog_action_dialog(&mut self, ctx: &egui::Context) {
        let Some(action) = self.catalog.pending_action.clone() else { return; };
        if let CatalogAction::Delete(path) = &action {
            self.show_catalog_delete_window(ctx, path);
            return;
        }
        // 「重命名」与「新建文件夹」都是「输入一个名字」的对话框，共用同一段界面。
        let is_rename = matches!(action, CatalogAction::Rename(_));
        let is_folder = matches!(action, CatalogAction::NewFolder(_));
        let title = self
            .t(if is_rename {
                "catalog.rename"
            } else if is_folder {
                "catalog.newFolder"
            } else {
                "catalog.delete"
            })
            .to_string();
        let mut open = true;
        let mut commit = false;
        let mut cancel = false;
        egui::Window::new(title)
            .id(egui::Id::new("catalog_file_action"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                if is_rename || is_folder {
                    let hint = if is_folder {
                        self.t("catalog.folderName").to_string()
                    } else {
                        String::new()
                    };
                    ui.add(
                        egui::TextEdit::singleline(&mut self.catalog.rename_value)
                            .desired_width(300.0)
                            .hint_text(hint),
                    );
                    ui.horizontal(|ui| {
                        if ui.button(self.t("dialog.ok")).clicked() { commit = true; }
                        if ui.button(self.t("dialog.cancel")).clicked() { cancel = true; }
                    });
                } else {
                    ui.label(self.tf("catalog.confirmDelete", &[&action_path(&action).display().to_string()]));
                    ui.horizontal(|ui| {
                        if ui.button(self.t("catalog.delete")).clicked() { commit = true; }
                        if ui.button(self.t("dialog.cancel")).clicked() { cancel = true; }
                    });
                }
            });
        if !open || cancel {
            self.catalog.pending_action = None;
            return;
        }
        if commit {
            let result = match action {
                CatalogAction::Rename(path) => rename_catalog_item(&path, &self.catalog.rename_value),
                CatalogAction::Delete(path) => delete_catalog_item(&path),
                CatalogAction::NewFolder(parent) => {
                    create_catalog_folder(&parent, &self.catalog.rename_value)
                }
            };
            self.catalog.feedback = Some(match result {
                Ok(()) => Ok(self.t("catalog.operationDone").to_string()),
                Err(error) => Err(self.tf("catalog.operationError", &[&error])),
            });
            self.catalog.pending_action = None;
        }
    }

    fn show_catalog_delete_window(&mut self, ctx: &egui::Context, path: &Path) {
        let title = self.t("catalog.delete").to_string();
        let message = self.tf("catalog.confirmDelete", &[&path.display().to_string()]);
        let delete_label = self.t("catalog.delete").to_string();
        let cancel_label = self.t("dialog.cancel").to_string();
        let main_rect = ctx
            .input(|input| input.viewport().outer_rect)
            .unwrap_or_else(|| ctx.screen_rect());
        let pos = egui::pos2(
            main_rect.left() + (main_rect.width() - 380.0) * 0.5,
            main_rect.top() + (main_rect.height() - 150.0) * 0.5,
        );
        let viewport_id = egui::ViewportId::from_hash_of("catalog_delete_viewport");
        let viewport = egui::ViewportBuilder::default()
            .with_title(title.clone())
            .with_inner_size([380.0, 150.0])
            .with_resizable(false)
            .with_close_button(true)
            .with_position(pos);

        let result = ctx.show_viewport_immediate(viewport_id, viewport, |viewport_ctx, class| {
            if class == egui::ViewportClass::Embedded {
                let mut open = true;
                let mut result = 0;
                egui::Window::new(title.clone())
                    .id(egui::Id::new("catalog_delete_window"))
                    .collapsible(false)
                    .resizable(false)
                    .open(&mut open)
                    .show(viewport_ctx, |ui| {
                        ui.label(&message);
                        ui.horizontal(|ui| {
                            if ui.button(&delete_label).clicked() {
                                result = 1;
                            }
                            if ui.button(&cancel_label).clicked() {
                                result = 2;
                            }
                        });
                    });
                if !open {
                    result = 2;
                }
                result
            } else {
                if viewport_ctx.input(|input| input.viewport().close_requested()) {
                    return 2;
                }
                let mut result = 0;
                egui::CentralPanel::default().show(viewport_ctx, |ui| {
                    ui.add_space(8.0);
                    ui.label(&message);
                    ui.add_space(8.0);
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui.button(&cancel_label).clicked() {
                                result = 2;
                            }
                            if ui.button(&delete_label).clicked() {
                                result = 1;
                            }
                        },
                    );
                });
                result
            }
        });
        match result {
            1 => {
                self.catalog.feedback = Some(match delete_catalog_item(path) {
                    Ok(()) => Ok(self.t("catalog.operationDone").to_string()),
                    Err(error) => Err(self.tf("catalog.operationError", &[&error])),
                });
                self.catalog.pending_action = None;
            }
            2 => self.catalog.pending_action = None,
            _ => {}
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

/// 分叉向上的线框图标，用于表示将 Shapefile 配套文件归组。
fn paint_shapefile_group_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let center = rect.center();
    let stroke = egui::Stroke::new(1.6_f32, color);
    let top = center.y - 8.0;
    let junction = center.y + 2.0;
    painter.line_segment(
        [egui::pos2(center.x, junction), egui::pos2(center.x, top + 4.0)],
        stroke,
    );
    painter.line_segment(
        [egui::pos2(center.x - 4.5, top + 8.5), egui::pos2(center.x, top + 4.0)],
        stroke,
    );
    painter.line_segment(
        [egui::pos2(center.x + 4.5, top + 8.5), egui::pos2(center.x, top + 4.0)],
        stroke,
    );
    painter.line_segment(
        [egui::pos2(center.x, junction), egui::pos2(center.x - 7.0, center.y + 9.0)],
        stroke,
    );
    painter.line_segment(
        [egui::pos2(center.x, junction), egui::pos2(center.x + 7.0, center.y + 9.0)],
        stroke,
    );
}

fn open_in_file_manager(path: PathBuf) {
    std::thread::spawn(move || {
        #[cfg(target_os = "windows")]
        {
            let mut command = std::process::Command::new("explorer.exe");
            if path.is_dir() {
                command.arg(path);
            } else {
                command.arg(format!("/select,{}", path.display()));
            }
            let _ = command.spawn();
        }
        #[cfg(target_os = "macos")]
        {
            let mut command = std::process::Command::new("open");
            if path.is_dir() {
                command.arg(path);
            } else {
                command.arg("-R").arg(path);
            }
            let _ = command.spawn();
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let target = if path.is_dir() {
                path
            } else {
                Path::new(&path)
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or(path)
            };
            let _ = std::process::Command::new("xdg-open").arg(target).spawn();
        }
    });
}

fn is_shapefile_sidecar(path: &Path) -> bool {
    if path.file_name().and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".shp.xml"))
    {
        return true;
    }
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "cpg" | "dbf" | "prj" | "sbn" | "sbx" | "shx"
            )
        })
}

fn action_path(action: &CatalogAction) -> &Path {
    match action {
        CatalogAction::Rename(path) | CatalogAction::Delete(path) => path,
        CatalogAction::NewFolder(parent) => parent,
    }
}

/// 在当前目录下新建文件夹（目录栏右键 → 新建 → 文件夹）。
fn create_catalog_folder(parent: &Path, requested_name: &str) -> Result<(), String> {
    let name = requested_name.trim();
    if name.is_empty() {
        return Err("文件夹名称不能为空".to_string());
    }
    if name.contains(['/', '\\']) {
        return Err("只能输入文件夹名，不能包含路径分隔符".to_string());
    }
    let target = parent.join(name);
    if target.exists() {
        return Err(format!("已存在同名文件夹：{}", target.display()));
    }
    std::fs::create_dir(&target).map_err(|error| format!("{}：{error}", target.display()))
}

fn shapefile_base(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".shp.xml") {
        name.get(..name.len() - ".shp.xml".len())
    } else {
        path.file_stem()?.to_str()
    }
}

fn requested_shapefile_base(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".shp.xml") {
        Some(name[..name.len() - ".shp.xml".len()].to_string())
    } else if lower.ends_with(".shp") {
        Some(name[..name.len() - ".shp".len()].to_string())
    } else if !name.is_empty() {
        Some(name.to_string())
    } else {
        None
    }
}

fn component_suffix(path: &Path) -> String {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    if name.to_ascii_lowercase().ends_with(".shp.xml") {
        ".shp.xml".to_string()
    } else {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!(".{ext}"))
            .unwrap_or_default()
    }
}

/// Return dataset files when the item belongs to a Shapefile or TIFF family.
fn associated_files(path: &Path) -> Vec<PathBuf> {
    let Some(parent) = path.parent() else { return vec![path.to_path_buf()]; };
    let Some(stem) = shapefile_base(path) else { return vec![path.to_path_buf()]; };
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase();
    let shape_exts = ["cpg", "dbf", "prj", "sbn", "sbx", "shp", "shx"];
    let is_shape_xml = path.file_name().and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".shp.xml"));
    let is_shape = shape_exts.contains(&extension.as_str()) || is_shape_xml;
    let is_tiff = matches!(extension.as_str(), "tif" | "tiff" | "ovr");
    if !is_shape && !is_tiff { return vec![path.to_path_buf()]; }

    let Ok(entries) = std::fs::read_dir(parent) else { return vec![path.to_path_buf()]; };
    let mut family: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|candidate| {
            shapefile_base(candidate).is_some_and(|candidate_stem| candidate_stem.eq_ignore_ascii_case(stem))
                && (candidate.file_name().and_then(|name| name.to_str())
                    .is_some_and(|name| name.to_ascii_lowercase().ends_with(".shp.xml"))
                    || candidate.extension().and_then(|s| s.to_str()).is_some_and(|ext| {
                    let ext = ext.to_ascii_lowercase();
                    if is_shape { shape_exts.contains(&ext.as_str()) }
                    else { matches!(ext.as_str(), "tif" | "tiff" | "ovr") }
                }))
        })
        .collect();
    let has_primary = family.iter().any(|candidate| {
        candidate.extension().and_then(|s| s.to_str()).is_some_and(|ext| {
            let ext = ext.to_ascii_lowercase();
            if is_shape { ext == "shp" } else { matches!(ext.as_str(), "tif" | "tiff") }
        })
    });
    if has_primary { family.sort(); family } else { vec![path.to_path_buf()] }
}

fn rename_catalog_item(path: &Path, requested_name: &str) -> Result<(), String> {
    let requested_name = requested_name.trim();
    let name_path = Path::new(requested_name);
    let Some(name) = name_path.file_name().filter(|_| !requested_name.is_empty()) else {
        return Err("文件名不能为空".to_string());
    };
    if name.to_string_lossy() != requested_name || requested_name.contains(['/', '\\']) {
        return Err("只能输入文件名，不能包含目录路径".to_string());
    }
    let parent = path.parent().ok_or_else(|| "无法确定所在目录".to_string())?;
    let family = if path.is_dir() { vec![path.to_path_buf()] } else { associated_files(path) };
    let grouped = family.len() > 1 || family.first().is_some_and(|item| item != path);
    let is_shape_family = family.iter().any(|item| {
        item.extension().and_then(|s| s.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("shp"))
    });
    let new_stem = if is_shape_family {
        requested_shapefile_base(requested_name).unwrap_or_else(|| name.to_string_lossy().into_owned())
    } else {
        name_path.file_stem().unwrap_or(name).to_string_lossy().into_owned()
    };
    if grouped && new_stem.trim().is_empty() {
        return Err("文件名不能为空".to_string());
    }
    let mut moves = Vec::new();
    for source in &family {
        let target_name = if grouped {
            format!("{new_stem}{}", component_suffix(source))
        } else { requested_name.to_string() };
        moves.push((source.clone(), parent.join(target_name)));
    }
    for (_, target) in &moves {
        if target.exists() && !family.contains(target) {
            return Err(format!("目标已存在：{}", target.display()));
        }
    }
    let mut completed: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (source, target) in &moves {
        if source == target { continue; }
        if let Err(error) = std::fs::rename(source, target) {
            for (old, new) in completed.iter().rev() { let _ = std::fs::rename(new, old); }
            return Err(format!("{}：{error}", source.display()));
        }
        completed.push((source.clone(), target.clone()));
    }
    Ok(())
}

fn delete_catalog_item(path: &Path) -> Result<(), String> {
    if path.is_dir() {
        return std::fs::remove_dir_all(path).map_err(|error| format!("{}：{error}", path.display()));
    }
    for file in associated_files(path) {
        std::fs::remove_file(&file).map_err(|error| format!("{}：{error}", file.display()))?;
    }
    Ok(())
}

fn copy_item_to_sibling(path: &Path, copy_suffix: &str) -> Result<PathBuf, String> {
    let parent = path.parent().ok_or_else(|| "无法确定所在目录".to_string())?;
    let family = if path.is_dir() { vec![path.to_path_buf()] } else { associated_files(path) };
    let grouped = family.len() > 1 || family.first().is_some_and(|item| item != path);
    let primary = family.iter().find(|item| item.extension().and_then(|s| s.to_str()).is_some_and(|ext| {
        matches!(ext.to_ascii_lowercase().as_str(), "shp" | "tif" | "tiff")
    })).unwrap_or(&family[0]);
    let source_stem = primary.file_stem().unwrap_or_default().to_string_lossy();
    let source_name = path.file_name().unwrap_or_default().to_string_lossy();
    let mut index = 1;
    let (destination_stem, destination) = loop {
        let suffix = if index == 1 { format!(" - {copy_suffix}") } else { format!(" - {copy_suffix} ({index})") };
        let stem = format!("{source_stem}{suffix}");
        let file_name = if path.is_dir() {
            format!("{source_name}{suffix}")
        } else if grouped {
            let ext = primary.extension().unwrap_or_default().to_string_lossy();
            format!("{stem}.{ext}")
        } else if let Some(ext) = path.extension() {
            format!("{stem}.{}", ext.to_string_lossy())
        } else { stem.clone() };
        let dest = parent.join(file_name);
        let family_conflict = grouped && family.iter().any(|source| {
            parent.join(format!("{stem}{}", component_suffix(source))).exists()
        });
        if !dest.exists() && !family_conflict { break (stem, dest); }
        index += 1;
    };
    if grouped {
        for source in &family {
            copy_file(source, &parent.join(format!("{destination_stem}{}", component_suffix(source))))?;
        }
    } else if path.is_dir() {
        copy_directory(path, &destination)?;
    } else { copy_file(path, &destination)?; }
    Ok(destination)
}

fn copy_file(source: &Path, destination: &Path) -> Result<(), String> {
    std::fs::copy(source, destination)
        .map(|_| ())
        .map_err(|error| format!("{}：{error}", source.display()))
}

fn copy_directory(source: &Path, destination: &Path) -> Result<(), String> {
    std::fs::create_dir(destination).map_err(|error| format!("{}：{error}", destination.display()))?;
    for entry in std::fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let target = destination.join(entry.file_name());
        if entry.path().is_dir() { copy_directory(&entry.path(), &target)?; }
        else { copy_file(&entry.path(), &target)?; }
    }
    Ok(())
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
