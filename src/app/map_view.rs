//! 中央地图视图：渲染 + 平移 / 缩放交互。
//!
//! 视图状态（[`crate::map::MapState`]）与投影 / 裁剪逻辑在 `crate::map`，
//! 这里只负责「每帧把交互喂给视图、把视图交给渲染器」。

use egui::{Id, RichText, Sense, Ui};

use crate::map;
use crate::style;

use super::RustGisApp;

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
        if rect.width() < 8.0 || rect.height() < 8.0 {
            self.cursor_world = None;
            return;
        }
        // 记录本帧地图区域：缓冲结果落点判断（视图是否已包含结果）需要它。
        self.map_rect = Some(rect);

        if self.needs_fit || self.map.view.is_none() {
            self.map.view = Some(map::MapView::fit(self.layers.total_bounds(), rect));
            self.needs_fit = false;
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
        if response.dragged() {
            let d = response.drag_delta();
            view.center[0] -= d.x as f64 / view.scale as f64;
            view.center[1] += d.y as f64 / view.scale as f64;
            self.map.view = Some(view);
            self.dirty = true;
        }

        // 滚轮缩放（以光标为中心）。
        //
        // egui 的符号约定：`smooth_scroll_delta.y > 0` 表示"滚轮向前 / 内容下移"，
        // egui 自身的 ctrl+滚轮缩放也是正 delta 放大，故这里同号 —— 滚轮向上 = 放大。
        // 原实现写成 `1 - 0.12·Δ`（正 delta 缩小），方向正好相反，这就是"缩放搞反了"。
        let scroll = ui.input(|i| i.smooth_scroll_delta);
        if scroll.y.abs() > 1e-3
            && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
            && rect.contains(pos)
        {
            let factor = (WHEEL_ZOOM_SENSITIVITY * scroll.y).exp().clamp(0.5, 2.0);
            view.zoom_factor_at(pos, rect, factor);
            self.map.view = Some(view);
            self.dirty = true;
        }

        // 双击缩放至全图
        if response.double_clicked() {
            view = map::MapView::fit(self.layers.total_bounds(), rect);
            self.map.view = Some(view);
            self.dirty = true;
        }

        // 鼠标世界坐标（状态栏展示）：按本帧最终视图换算，与画面一致。
        self.cursor_world = response.hover_pos().map(|p| map::world_at(p, rect, &view));

        let painter = ui.painter_at(rect);
        // 地图画布为白色（GIS 底图习惯），与整应用白底一致。
        painter.rect_filled(rect, 0.0, style::WHITE);

        if self.layers.is_empty() {
            // 空数据占位说明：白底画布上用次级文字色，呈"水印"观感
            let ink = style::TEXT_WEAK;
            ui.vertical_centered_justified(|ui| {
                ui.add_space(rect.height() * 0.32);
                ui.colored_label(ink, RichText::new(self.t("mapview.title")).strong());
                ui.add_space(8.0);
                ui.colored_label(ink, RichText::new(self.t("mapview.desc1")));
                ui.add_space(4.0);
                ui.colored_label(ink, RichText::new(self.t("mapview.desc2")));
            });
        } else {
            // 缓存渲染：视图 / 画布 / 图层都没变时直接复用上一帧的形状。
            self.map_renderer
                .render(&painter, rect, &self.layers, &view);
        }
    }
}
