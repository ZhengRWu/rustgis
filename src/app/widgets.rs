//! 矢量图标与通用小部件。
//!
//! 全部用 `egui::Painter` 直接画，刻意不引入任何图片资源依赖（打包体积为零、
//! 任意 DPI 都清晰）。这里的函数都是「纯绘制」：只接收画师 + 区域 + 参数，
//! 不触碰应用状态，因此可被图层面板等复用。
//!
//! 注意：`draw_legend_symbol` 依赖图层行的展示数据（`Row`），放在
//! [`super::layers_panel`] 里；本模块只保留与具体行无关的底层图元。

use egui::{Color32, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, Vec2};

use crate::style;

/// 图标按钮：悬停高亮 + 点击，`glyph` 负责在给定区域里画图标。
pub(super) fn icon_button(
    ui: &mut Ui,
    tip: &str,
    enabled: bool,
    glyph: impl FnOnce(&egui::Painter, Rect, Color32),
) -> egui::Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(20.0), sense);
    let color = if enabled {
        style::TEXT
    } else {
        style::TEXT_WEAK
    };
    if enabled && resp.hovered() {
        ui.painter().rect_filled(rect, 3.0, style::ACCENT_HOVER_BG);
    }
    glyph(ui.painter(), rect.shrink(4.5), color);
    resp.on_hover_text(tip)
}

/// 加号：添加图层。
pub(super) fn draw_plus(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let h = rect.width().min(rect.height()) * 0.44;
    let stroke = Stroke::new(1.6_f32, color);
    painter.line_segment([Pos2::new(c.x - h, c.y), Pos2::new(c.x + h, c.y)], stroke);
    painter.line_segment([Pos2::new(c.x, c.y - h), Pos2::new(c.x, c.y + h)], stroke);
}

/// 方框内的正负号：与内容列表里折叠框同一套语汇（− 折叠全部 / + 展开全部）。
pub(super) fn draw_box_sign(painter: &egui::Painter, rect: Rect, plus: bool, color: Color32) {
    painter.rect_stroke(rect, 2.0, Stroke::new(1.2_f32, color), StrokeKind::Inside);
    let c = rect.center();
    let h = rect.width() * 0.30;
    let stroke = Stroke::new(1.5_f32, color);
    painter.line_segment([Pos2::new(c.x - h, c.y), Pos2::new(c.x + h, c.y)], stroke);
    if plus {
        painter.line_segment([Pos2::new(c.x, c.y - h), Pos2::new(c.x, c.y + h)], stroke);
    }
}

/// 图层组图标：带标签的文件夹。
pub(super) fn draw_group_icon(painter: &egui::Painter, rect: Rect, color: Color32) {
    let body = Rect::from_min_max(
        Pos2::new(rect.min.x, rect.min.y + rect.height() * 0.26),
        Pos2::new(rect.max.x, rect.max.y - rect.height() * 0.12),
    );
    let tab = Rect::from_min_max(
        Pos2::new(
            rect.min.x + rect.width() * 0.05,
            rect.min.y + rect.height() * 0.12,
        ),
        Pos2::new(
            rect.min.x + rect.width() * 0.55,
            rect.min.y + rect.height() * 0.32,
        ),
    );
    painter.rect_filled(tab, 1.0, color);
    painter.rect_filled(body, 1.5, color);
}

/// 内容列表根节点「图层」的图标：三层堆叠的菱形（对标 ArcGIS 的图层图标）。
pub(super) fn draw_layers_glyph(painter: &egui::Painter, rect: Rect, color: Color32) {
    let half_w = rect.width() * 0.44;
    let dy = rect.height() * 0.17;
    let cx = rect.center().x;
    for (i, alpha) in [0.40_f32, 0.68, 1.0].into_iter().enumerate() {
        let cy = rect.min.y + dy * 1.25 + i as f32 * dy * 1.18;
        let pts = vec![
            Pos2::new(cx, cy - dy),
            Pos2::new(cx + half_w, cy),
            Pos2::new(cx, cy + dy),
            Pos2::new(cx - half_w, cy),
        ];
        painter.add(Shape::convex_polygon(
            pts,
            color.gamma_multiply(alpha),
            Stroke::NONE,
        ));
    }
}

/// 折叠框：展开态显示 −，折叠态显示 +（截图里的 +/- 方框）。
pub(super) fn draw_expander(painter: &egui::Painter, rect: Rect, expanded: bool, hot: bool) {
    painter.rect_filled(rect, 1.5, style::WHITE);
    let border = if hot {
        style::ACCENT
    } else {
        style::BORDER_STRONG
    };
    painter.rect_stroke(rect, 1.5, Stroke::new(1.0_f32, border), StrokeKind::Inside);
    let c = rect.center();
    let h = rect.width() * 0.30;
    let stroke = Stroke::new(1.3_f32, style::TEXT_WEAK);
    painter.line_segment([Pos2::new(c.x - h, c.y), Pos2::new(c.x + h, c.y)], stroke);
    if !expanded {
        painter.line_segment([Pos2::new(c.x, c.y - h), Pos2::new(c.x, c.y + h)], stroke);
    }
}

/// 渲染进度圆环（状态栏用）：淡色轨道 + 蓝色圆弧。
///
/// `progress` 是「本帧已画好的块 / 视口需要的块」（0.0–1.0）。圆弧没有画满时
/// 顺时针旋转，所以刚开始解码（进度还是 0）也看得出「正在渲染」；画满即为完成，
/// 由调用方决定何时不再显示。圆环只占 14×14 点，正好对齐状态栏文字的中心线。
pub(super) fn render_ring(ui: &mut Ui, progress: f32) {
    let side = 14.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
    let center = rect.center();
    let radius = side * 0.5 - 1.5;
    let painter = ui.painter();
    painter.circle_stroke(center, radius, Stroke::new(2.0_f32, style::ACCENT_HOVER_STROKE));

    let progress = progress.clamp(0.0, 1.0);
    let complete = progress >= 0.999;
    let time = ui.input(|i| i.time) as f32;
    let sweep = if complete {
        std::f32::consts::TAU
    } else {
        // 至少转一小段弧（进度 0 时是 12% 的一段），并且随时间顺时针转：
        // 静止的圆环会被看成「卡住了」，转起来才像在干活。
        (progress * 0.88 + 0.12) * std::f32::consts::TAU
    };
    let start = if complete {
        -std::f32::consts::FRAC_PI_2
    } else {
        -std::f32::consts::FRAC_PI_2 + time * 4.0
    };
    let steps = ((sweep * 8.0).ceil() as usize).max(3);
    let points: Vec<Pos2> = (0..=steps)
        .map(|index| {
            let angle = start + sweep * (index as f32 / steps as f32);
            center + Vec2::new(angle.cos(), angle.sin()) * radius
        })
        .collect();
    painter.add(Shape::line(points, Stroke::new(2.0_f32, style::ACCENT)));
    if !complete {
        // 圆弧在转，界面必须持续重绘；否则圆环会停在某一帧的画面上。
        ui.ctx().request_repaint();
    }
}

/// 勾选框：勾选 / 取消勾选决定该节点是否参与绘制。
///
/// `muted` 用于"祖先未勾选"的子树——勾选状态照原样显示，只把颜色压暗。
pub(super) fn draw_checkbox(painter: &egui::Painter, rect: Rect, checked: bool, muted: bool) {
    let border = if muted {
        style::BORDER
    } else if checked {
        style::ACCENT
    } else {
        style::BORDER_STRONG
    };
    painter.rect_filled(rect, 2.0, style::WHITE);
    painter.rect_stroke(rect, 2.0, Stroke::new(1.0_f32, border), StrokeKind::Inside);
    if checked {
        let tick = if muted {
            style::TEXT_WEAK
        } else {
            style::ACCENT
        };
        let pts = vec![
            Pos2::new(
                rect.min.x + rect.width() * 0.24,
                rect.center().y + rect.height() * 0.05,
            ),
            Pos2::new(
                rect.min.x + rect.width() * 0.43,
                rect.max.y - rect.height() * 0.24,
            ),
            Pos2::new(
                rect.max.x - rect.width() * 0.20,
                rect.min.y + rect.height() * 0.26,
            ),
        ];
        painter.add(Shape::line(pts, Stroke::new(1.6_f32, tick)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 统计一帧里的圆环图元：轨道是 1 个圆，进度弧是 1 段折线。
    fn ring_shapes(progress: f32) -> (usize, Vec<usize>) {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, 40.0))),
            ..egui::RawInput::default()
        };
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render_ring(ui, progress);
            });
        });
        let mut circles = 0;
        let mut arcs = Vec::new();
        for clipped in &output.shapes {
            match &clipped.shape {
                Shape::Circle(_) => circles += 1,
                Shape::Path(path) if path.points.len() > 2 => arcs.push(path.points.len()),
                _ => {}
            }
        }
        (circles, arcs)
    }

    #[test]
    fn render_ring_draws_a_track_and_a_progress_arc() {
        let (circles, arcs) = ring_shapes(0.5);
        assert_eq!(circles, 1, "应有 1 条淡色轨道");
        assert_eq!(arcs.len(), 1, "应有 1 段蓝色进度弧");
        // 半圈大约 180°，按 8 段/弧度取点，至少二三十个顶点。
        assert!(arcs[0] > 8, "半圈进度弧应有足够的顶点，实际 {}", arcs[0]);
    }

    #[test]
    fn render_ring_shows_more_arc_as_progress_grows() {
        let (_, low) = ring_shapes(0.1);
        let (_, high) = ring_shapes(0.9);
        assert!(
            high[0] > low[0],
            "进度越大弧越长（{} 应多于 {}）",
            high[0],
            low[0]
        );
    }

    #[test]
    fn render_ring_survives_out_of_range_progress() {
        // 数值异常（负、越界、NaN）不应画崩或画出反向弧。
        for value in [-1.0, 0.0, 1.0, 2.0, f32::NAN] {
            let (circles, arcs) = ring_shapes(value);
            assert_eq!(circles, 1, "progress={value} 仍应有轨道");
            assert_eq!(arcs.len(), 1, "progress={value} 仍应有 1 段弧");
        }
    }
}
