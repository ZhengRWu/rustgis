//! 中央地图视图：渲染 + 平移 / 缩放交互。
//!
//! 视图状态（[`crate::map::MapState`]）与投影 / 裁剪逻辑在 `crate::map`，
//! 这里只负责「每帧把交互喂给视图、把视图交给渲染器」。

use egui::{Id, RichText, Sense, Ui};

use crate::map;
use crate::style;

use super::RustGisApp;

fn measure_segment(a: [f64; 2], b: [f64; 2], model: Option<crate::geo::ProjectionModel>) -> f64 {
    if model == Some(crate::geo::ProjectionModel::Geographic) {
        let (lat1, lat2) = (a[1].to_radians(), b[1].to_radians());
        let dlat = lat2 - lat1;
        let dlon = (b[0] - a[0]).to_radians();
        let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
        6_371_008.8 * 2.0 * h.sqrt().atan2((1.0 - h).sqrt())
    } else {
        ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt()
    }
}

fn measure_polygon_area(points: &[[f64; 2]], model: Option<crate::geo::ProjectionModel>) -> f64 {
    let mut area = 0.0;
    for i in 0..points.len() {
        let a = points[i];
        let b = points[(i + 1) % points.len()];
        area += if model == Some(crate::geo::ProjectionModel::Geographic) {
            (b[0] - a[0]).to_radians() * (2.0 + a[1].to_radians().sin() + b[1].to_radians().sin())
        } else { a[0] * b[1] - b[0] * a[1] };
    }
    let area = area.abs() * 0.5;
    if model == Some(crate::geo::ProjectionModel::Geographic) { area * 6_371_008.8_f64.powi(2) } else { area }
}

fn format_measure_distance(meters_or_units: f64) -> String {
    if meters_or_units >= 1000.0 { format!("{:.3} km", meters_or_units / 1000.0) }
    else { format!("{:.1} m", meters_or_units) }
}

fn format_measure_area(square_meters_or_units: f64) -> String {
    if square_meters_or_units >= 1_000_000.0 { format!("{:.3} km²", square_meters_or_units / 1_000_000.0) }
    else { format!("{:.1} m²", square_meters_or_units) }
}

/// 滚轮缩放灵敏度：每 1 点滚动 delta 对应的对数缩放量。
///
/// egui 一次滚轮格 ≈ `line_scroll_speed` = 40 点，故一格 ≈ `e^(0.012×40) ≈ 1.62×`。
/// 原实现是线性式 `1 - 0.12·Δ` 且钳制在 `[0.5, 2.0]`：一格滚轮的平滑 delta 累计
/// 到 40 点，第一帧就撞死在钳制边界上，实测**一格缩放约 40 倍**。
/// 这里做两件事：把灵敏度取原系数的 1/10，并改用指数式
/// （多帧平滑累乘恰好等于 `e^(k·Δ总量)`，与帧率无关），彻底避开钳制饱和。
const WHEEL_ZOOM_SENSITIVITY: f32 = 0.012;

impl RustGisApp {
    /// 中央地图视图（渲染 + 平移/缩放交互）。
    pub(super) fn map_panel(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        self.finish_catalog_drag(ui, rect);
        if rect.width() < 8.0 || rect.height() < 8.0 {
            self.cursor_world = None;
            return;
        }
        // 记录本帧地图区域：缓冲结果落点判断（视图是否已包含结果）需要它。
        self.map_rect = Some(rect);

        let just_fit = self.needs_fit || self.map.view.is_none();
        if just_fit {
            self.map.view = Some(map::MapView::fit(
                self.layers
                    .total_bounds_for(self.project_projection.as_ref()),
                rect,
            ));
            self.needs_fit = false;
        }
        // 右键「缩放至图层 / 缩放至组」：范围由菜单回调算出，这里用本帧地图控件的
        // 实际像素矩形适配。放在全图适配之后——同一帧两者都排队时以用户明确点的为准。
        if let Some(bounds) = self.zoom_extent.take() {
            self.map.view = Some(map::MapView::fit(Some(bounds), rect));
            self.dirty = true;
        }
        let mut view = self.map.view.unwrap();

        // 交互必须先于渲染：拖动 / 缩放在本帧就作用到 `view` 上。
        // 原实现先按旧视图画完、再处理输入，画面永远落后鼠标一帧。
        let response = ui.interact(
            rect,
            Id::new("map_view"),
            Sense::drag() | Sense::click() | Sense::hover(),
        );

        // 左键拖动平移
        if self.map.tool == map::Tool::Pan && response.dragged_by(egui::PointerButton::Primary) {
            let d = response.drag_delta();
            view.center[0] -= d.x as f64 / view.scale as f64;
            view.center[1] += d.y as f64 / view.scale as f64;
            self.map.view = Some(view);
            self.dirty = true;
        }

        let mut zoom_box = None;
        if matches!(self.map.tool, map::Tool::ZoomIn | map::Tool::ZoomOut) {
            if response.hovered() || response.dragged() {
                ui.ctx().set_cursor_icon(match self.map.tool {
                    map::Tool::ZoomIn => egui::CursorIcon::ZoomIn,
                    _ => egui::CursorIcon::ZoomOut,
                });
            }
            if response.drag_started_by(egui::PointerButton::Primary) {
                self.map.zoom_drag_start = ui
                    .input(|i| i.pointer.press_origin())
                    .filter(|p| rect.contains(*p));
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.map.zoom_drag_start = None;
            }
            if let Some(start) = self.map.zoom_drag_start {
                if let Some(end) = ui.input(|i| i.pointer.latest_pos()) {
                    let selection = egui::Rect::from_two_pos(start, rect.clamp(end));
                    if response.drag_stopped_by(egui::PointerButton::Primary) {
                        if view.zoom_box(rect, selection, self.map.tool == map::Tool::ZoomIn) {
                            self.map.view = Some(view);
                            self.dirty = true;
                        }
                        self.map.zoom_drag_start = None;
                    } else {
                        zoom_box = Some(selection);
                    }
                }
                if !ui.input(|i| i.pointer.primary_down()) {
                    self.map.zoom_drag_start = None;
                }
            }
        } else {
            // 平移 / 识别工具不做框选缩放。
            self.map.zoom_drag_start = None;
            if self.map.tool == map::Tool::Info && (response.hovered() || response.dragged()) {
                // egui 没有「信息」光标，Help（问号）是语义最接近的一个。
                ui.ctx().set_cursor_icon(egui::CursorIcon::Help);
            }
            if self.map.tool == map::Tool::Measure && (response.hovered() || response.dragged()) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            }
        }

        // 识别工具：单击地图 → 查询最顶层可视要素 / 像元（只读，不移动视图）。
        if self.map.tool == map::Tool::Info
            && response.clicked_by(egui::PointerButton::Primary)
            && let Some(pos) = response.interact_pointer_pos()
        {
            self.identify_at(pos, rect, &view);
        }

        if self.map.tool == map::Tool::Measure {
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.map.measure_points.clear();
                self.map.measure_area = false;
                self.map.measure_closed = false;
            }
            if response.clicked_by(egui::PointerButton::Primary)
                && let Some(pos) = response.interact_pointer_pos()
            {
                let point = map::world_at(pos, rect, &view);
                let close_distance = 12.0_f32;
                let first_screen = self.map.measure_points.first().map(|p| map::world_to_screen(*p, rect, &view));
                let closes_polygon = self.map.measure_area
                    && self.map.measure_points.len() >= 3
                    && first_screen.is_some_and(|first| first.distance(pos) <= close_distance);
                if closes_polygon {
                    self.map.measure_closed = true;
                } else {
                    let shift = ui.input(|i| i.modifiers.shift);
                    if self.map.measure_points.is_empty() {
                        self.map.measure_area = shift;
                        self.map.measure_points.push(point);
                    } else if self.map.measure_area == shift {
                        self.map.measure_points.push(point);
                    } else {
                        self.map.measure_points = vec![point];
                        self.map.measure_area = shift;
                        self.map.measure_closed = false;
                    }
                }
            }
        }

        // 滚轮缩放（以光标为中心）。
        //
        // egui 的符号约定：`smooth_scroll_delta.y > 0` 表示"滚轮向前 / 内容下移"，
        // egui 自身的 ctrl+滚轮缩放也是正 delta 放大，故这里同号 —— 滚轮向上 = 放大。
        // 原实现写成 `1 - 0.12·Δ`（正 delta 缩小），方向正好相反，这就是"缩放搞反了"。
        let scroll = ui.input(|i| i.smooth_scroll_delta);
        if scroll.y.abs() > 1e-3
            && self.map.zoom_drag_start.is_none()
            && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
            && rect.contains(pos)
        {
            let factor = (WHEEL_ZOOM_SENSITIVITY * scroll.y).exp().clamp(0.5, 2.0);
            view.zoom_factor_at(pos, rect, factor);
            self.map.view = Some(view);
            self.dirty = true;
        }

        // 双击缩放至全图
        if self.map.tool == map::Tool::Pan && response.double_clicked() {
            view = map::MapView::fit(
                self.layers
                    .total_bounds_for(self.project_projection.as_ref()),
                rect,
            );
            self.map.view = Some(view);
            self.dirty = true;
        }

        // 鼠标世界坐标（状态栏展示）：按本帧最终视图换算，与画面一致。
        self.cursor_world = response.hover_pos().map(|p| map::world_at(p, rect, &view));

        let painter = ui.painter_at(rect);
        // 地图画布为白色（GIS 底图习惯），与整应用白底一致。
        painter.rect_filled(rect, 0.0, style::WHITE);


        if self.layers.is_empty() {
            // 空数据占位说明：与全应用一致，文字默认纯黑。
            let ink = style::TEXT;
            ui.vertical_centered_justified(|ui| {
                ui.add_space(rect.height() * 0.32);
                ui.colored_label(ink, RichText::new(self.t("mapview.title")).strong());
                ui.add_space(8.0);
                ui.colored_label(ink, RichText::new(self.t("mapview.desc1")));
                ui.add_space(4.0);
                ui.colored_label(ink, RichText::new(self.t("mapview.desc2")));
            });
        } else {
            // 栅格图层先绘制（影像在矢量之下，与 ArcGIS 默认叠放顺序一致）：
            // 按当前视图换算窗口 → 取合适分辨率 → 拉伸/NoData 处理后上传纹理。
            let rasters: Vec<&crate::raster::RasterLayer> =
                self.layers.iter_visible_rasters().collect();
            if !rasters.is_empty() {
                self.raster_painter.paint(
                    ui.ctx(),
                    &painter,
                    rect,
                    &view,
                    &rasters,
                    self.project_projection.as_ref(),
                );
            }
            // 缓存渲染：视图 / 画布 / 图层都没变时直接复用上一帧的形状。
            self.map_renderer.render(
                &painter,
                rect,
                &self.layers,
                &view,
                self.project_projection.as_ref(),
            );
        }

        if !self.map.measure_points.is_empty() {
            let mut screen_points: Vec<egui::Pos2> = self.map.measure_points.iter()
                .map(|p| map::world_to_screen(*p, rect, &view)).collect();
            let cursor = if self.map.tool == map::Tool::Measure && !self.map.measure_closed { response.hover_pos() } else { None };
            if let Some(pos) = cursor { screen_points.push(pos); }
            if self.map.measure_area && self.map.measure_closed && screen_points.len() >= 3 {
                screen_points.push(screen_points[0]);
            }
            if screen_points.len() > 1 {
            painter.add(egui::Shape::line(screen_points.clone(), egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(220, 70, 40))));
            }
            for p in screen_points.iter().take(self.map.measure_points.len()) {
                painter.circle_filled(*p, 4.0, egui::Color32::WHITE);
                painter.circle_stroke(*p, 4.0, egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(220, 70, 40)));
            }
            let mut meters = 0.0;
            for pair in self.map.measure_points.windows(2) {
                meters += measure_segment(pair[0], pair[1], self.project_projection.as_ref().map(|p| p.model));
            }
            let mut label = format!("{}: {}", self.t("measure.distance"), format_measure_distance(meters));
            if self.map.measure_area && self.map.measure_closed && self.map.measure_points.len() >= 3 {
                let area = measure_polygon_area(&self.map.measure_points, self.project_projection.as_ref().map(|p| p.model));
                label.push_str(&format!("\n{}: {}", self.t("measure.area"), format_measure_area(area)));
            }
            let anchor = cursor.unwrap_or_else(|| *screen_points.last().unwrap());
            let label_rect = egui::Rect::from_min_size(anchor + egui::vec2(10.0, 10.0), egui::vec2(190.0, 42.0));
            painter.rect_filled(label_rect, 3.0, egui::Color32::from_white_alpha(235));
            painter.text(label_rect.min + egui::vec2(5.0, 4.0), egui::Align2::LEFT_TOP, label, egui::FontId::proportional(13.0), style::TEXT);
        }


        if let Some(selection) = zoom_box {
            painter.rect_filled(selection, 0.0, style::ACCENT.gamma_multiply(0.08));
            painter.rect_stroke(
                selection,
                0.0,
                egui::Stroke::new(1.0_f32, style::ACCENT),
                egui::StrokeKind::Inside,
            );
        }

        // 「查找」定位高亮：双击结果后在该要素范围上闪一圈橙色框，几秒后消失。
        if let Some(bounds) = self.find.highlight {
            let now = ui.input(|input| input.time);
            if now < self.find.highlight_until {
                let min = map::world_to_screen(bounds.min, rect, &view);
                let max = map::world_to_screen(bounds.max, rect, &view);
                let r = egui::Rect::from_two_pos(min, max).expand(3.0);
                if rect.intersects(r) {
                    painter.rect_stroke(
                        r,
                        0.0,
                        egui::Stroke::new(2.0_f32, super::find::HIGHLIGHT_COLOR),
                        egui::StrokeKind::Outside,
                    );
                }
                // 高亮期间持续出帧，到点后自动清掉。
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
            } else {
                self.find.highlight = None;
            }
        }

        // 「转到 XY」定位标记：红色十字环，几秒后自动消失。
        if let Some(marker) = self.goto_xy.marker {
            let now = ui.input(|input| input.time);
            if now < self.goto_xy.marker_until {
                let p = map::world_to_screen(marker, rect, &view);
                if rect.expand(16.0).contains(p) {
                    let stroke = egui::Stroke::new(2.0_f32, style::DANGER);
                    painter.circle_stroke(p, 9.0, stroke);
                    painter.line_segment(
                        [p - egui::vec2(15.0, 0.0), p + egui::vec2(15.0, 0.0)],
                        stroke,
                    );
                    painter.line_segment(
                        [p - egui::vec2(0.0, 15.0), p + egui::vec2(0.0, 15.0)],
                        stroke,
                    );
                }
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
            } else {
                self.goto_xy.marker = None;
            }
        }

        // 拖放提示：文件悬停在窗口上（尚未松开）时铺一层淡色 + 虚线框，
        // 让用户确认「松手就会加入图层」。与右下角通知同色（ACCENT）。
        if ui.ctx().input(|i| !i.raw.hovered_files.is_empty()) || self.catalog.dragging_file.is_some() {
            let frame = rect.shrink(6.0);
            painter.rect_filled(frame, 0.0, style::ACCENT.gamma_multiply(0.06));
            painter.extend(egui::Shape::dashed_line(
                &[
                    frame.left_top(),
                    frame.right_top(),
                    frame.right_bottom(),
                    frame.left_bottom(),
                    frame.left_top(),
                ],
                egui::Stroke::new(2.0_f32, style::ACCENT),
                10.0,
                6.0,
            ));
            painter.text(
                frame.center(),
                egui::Align2::CENTER_CENTER,
                self.t("mapview.dropHint"),
                egui::FontId::proportional(15.0),
                style::ACCENT,
            );
        }
    }
}
