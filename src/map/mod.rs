//! 地图视图：世界坐标（经度/纬度）↔ 屏幕像素 的变换、缩放与平移。

pub mod render;
pub mod simplify;

use egui::{Pos2, Rect, pos2};

use crate::geo::Aabb;

/// 地图视图状态。
///
/// `scale` 为“每世界单位（度）对应的像素数”。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapView {
    pub center: [f64; 2],
    pub scale: f32,
}

/// 地图鼠标交互工具。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Pan,
    ZoomIn,
    ZoomOut,
    /// 识别：单击地图查询最顶层可视要素 / 像元信息（不改变视图）。
    Info,
    Measure,
    /// 数字化（新建矢量后自动进入）：在地图上点出点 / 线 / 面要素。
    ///
    /// 与 QGIS 的「开启编辑 + 添加要素」不同，这里不区分"编辑会话"与"工具"：
    /// 该工具只在存在编辑会话（[`crate::app::new_vector::Editing`]）时才有意义，
    /// 退出编辑会把它切回 [`Tool::Pan`]。
    Edit,
}

/// 地图交互总状态。
#[derive(Debug, Clone, PartialEq)]
pub struct MapState {
    /// None 表示尚未初始化（首次渲染时按图层范围或默认视图设置）。
    pub view: Option<MapView>,
    pub tool: Tool,
    pub zoom_drag_start: Option<Pos2>,
    pub measure_points: Vec<[f64; 2]>,
    pub measure_area: bool,
    pub measure_closed: bool,
}

impl Default for MapState {
    fn default() -> Self {
        Self {
            view: None,
            tool: Tool::Pan,
            zoom_drag_start: None,
            measure_points: Vec::new(),
            measure_area: false,
            measure_closed: false,
        }
    }
}

impl MapView {
    /// 框选放大使框内范围适配视口；框选缩小将当前视口缩进选框。
    /// 忽略点击和过窄的框，避免产生极端缩放。
    pub fn zoom_box(&mut self, viewport: Rect, selection: Rect, zoom_in: bool) -> bool {
        let selection = selection.intersect(viewport);
        if viewport.width() < 8.0
            || viewport.height() < 8.0
            || selection.width() < 4.0
            || selection.height() < 4.0
        {
            return false;
        }
        let ratio =
            (selection.width() / viewport.width()).max(selection.height() / viewport.height());
        if zoom_in {
            self.center = world_at(selection.center(), viewport, self);
            self.zoom_center(1.0 / ratio);
        } else {
            let old_center = self.center;
            self.zoom_center(
                (selection.width() / viewport.width()).min(selection.height() / viewport.height()),
            );
            let offset = selection.center() - viewport.center();
            self.center = [
                old_center[0] - f64::from(offset.x) / f64::from(self.scale),
                old_center[1] + f64::from(offset.y) / f64::from(self.scale),
            ];
        }
        true
    }
    /// 适配到给定包围盒（留 10% 边距）；无数据时使用默认视图（中国范围）。
    pub fn fit(bounds: Option<Aabb>, rect: Rect) -> MapView {
        match bounds {
            Some(b) if !b.is_empty() => {
                let w = b.width().max(1e-9);
                let h = b.height().max(1e-9);
                let scale = ((rect.width() as f64 / w).min(rect.height() as f64 / h) * 0.9)
                    .clamp(1e-6, 1e6) as f32;
                MapView {
                    center: b.center(),
                    scale,
                }
            }
            _ => MapView {
                center: [104.5, 36.0],
                scale: (rect.width() as f64 / 120.0).clamp(1e-3, 1e3) as f32,
            },
        }
    }

    /// 以屏幕点 `pos` 为中心按指定倍率缩放（`pos` 下的世界坐标保持不变）。
    pub fn zoom_factor_at(&mut self, pos: Pos2, rect: Rect, factor: f32) {
        let before = world_at(pos, rect, self);
        self.scale = (self.scale * factor).clamp(1e-6, 1e9);
        let sc = rect.center();
        self.center[0] = before[0] - (pos.x - sc.x) as f64 / self.scale as f64;
        self.center[1] = before[1] + (pos.y - sc.y) as f64 / self.scale as f64;
    }

    /// 以屏幕中心缩放（中心点不变）。
    pub fn zoom_center(&mut self, factor: f32) {
        self.scale = (self.scale * factor).clamp(1e-6, 1e9);
    }
}

/// 世界坐标 → 屏幕像素（世界 y 朝北，屏幕 y 朝下）。
///
/// 这是 [`Projector`] 的**参考实现**：热路径用预折算的 `Projector`，本函数保留
/// 逐点调用的可读写法，供测试对照两者的语义是否漂移。
#[allow(dead_code)] // 二进制里全部走 `Projector`，本函数只被测试引用。
pub fn world_to_screen(w: [f64; 2], rect: Rect, view: &MapView) -> Pos2 {
    pos2(
        ((w[0] - view.center[0]) * view.scale as f64 + rect.center().x as f64) as f32,
        (rect.center().y as f64 - (w[1] - view.center[1]) * view.scale as f64) as f32,
    )
}

/// 屏幕像素 → 世界坐标。
pub fn world_at(p: Pos2, rect: Rect, view: &MapView) -> [f64; 2] {
    [
        (p.x as f64 - rect.center().x as f64) / view.scale as f64 + view.center[0],
        (rect.center().y as f64 - p.y as f64) / view.scale as f64 + view.center[1],
    ]
}

/// 预折算的仿射投影器（世界坐标 → 屏幕像素）。
///
/// [`world_to_screen`] 每次调用都要取 `rect.center()`、做两次减法与两次类型转换；
/// 渲染一帧要投影几十万到上百万个顶点，这些固定开销会被放大成可观的常数项。
/// `Projector` 把变换预先折算成 `screen = w * k + b` 两条乘加
/// （`ky` 为负：世界 y 朝北、屏幕 y 朝下），并提供视口对应的世界范围供裁剪使用。
///
/// 每帧构造一次、逐图层复用；`Projector` 是 `Copy`。
#[derive(Debug, Clone, Copy)]
pub struct Projector {
    kx: f64,
    bx: f64,
    ky: f64,
    by: f64,
    rect: Rect,
}

impl Projector {
    pub fn new(rect: Rect, view: &MapView) -> Self {
        let k = view.scale as f64;
        let (cx, cy) = (rect.center().x as f64, rect.center().y as f64);
        Self {
            kx: k,
            bx: cx - view.center[0] * k,
            ky: -k,
            by: cy + view.center[1] * k,
            rect,
        }
    }

    /// 世界坐标 → 屏幕像素（不对结果做任何检查，调用方自行过滤非有限值）。
    #[inline(always)]
    pub fn project_xy(&self, wx: f64, wy: f64) -> (f32, f32) {
        (
            (wx * self.kx + self.bx) as f32,
            (wy * self.ky + self.by) as f32,
        )
    }

    /// 便捷版 [`Self::project_xy`]：`Pos2` 会顺带做一次构造，热路径请直接用
    /// `project_xy`，本方法只被测试与一次性调用使用。
    #[allow(dead_code)]
    #[inline(always)]
    pub fn project(&self, w: [f64; 2]) -> Pos2 {
        let (x, y) = self.project_xy(w[0], w[1]);
        Pos2::new(x, y)
    }

    /// 当前视口对应的世界坐标范围（用于视口裁剪）。
    #[allow(dead_code)] // 渲染路径带线宽余量，走 [`Self::visible_world_padded`]。
    pub fn visible_world(&self) -> Aabb {
        self.visible_world_padded(0.0)
    }

    /// 画布矩形（视口粗筛用）。
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// 视口四周再放宽 `pad_px` 像素后的世界坐标范围。
    /// 线宽 / 点半径 / 路径包围盒判定都要算进去，否则贴边的要素会被误裁。
    pub fn visible_world_padded(&self, pad_px: f32) -> Aabb {
        let p = pad_px as f64;
        let (x0, x1) = (
            (self.rect.min.x as f64 - p - self.bx) / self.kx,
            (self.rect.max.x as f64 + p - self.bx) / self.kx,
        );
        let (y0, y1) = (
            (self.rect.min.y as f64 - p - self.by) / self.ky,
            (self.rect.max.y as f64 + p - self.by) / self.ky,
        );
        Aabb {
            min: [x0.min(x1), y0.min(y1)],
            max: [x0.max(x1), y0.max(y1)],
        }
    }

    /// `px` 像素对应的世界长度（用于把线宽算进裁剪余量）。
    #[allow(dead_code)] // 供后续按世界单位计算裁剪余量时使用；当前渲染统一按像素留白。
    pub fn px_to_world(&self, px: f32) -> f64 {
        px as f64 / self.kx.abs().max(1e-12)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn box_zoom_fits_selected_extent_and_preserves_aspect_ratio() {
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let selection =
            egui::Rect::from_two_pos(egui::pos2(100.0, 100.0), egui::pos2(300.0, 200.0));
        let mut view = super::MapView {
            center: [0.0, 0.0],
            scale: 10.0,
        };
        let center = super::world_at(selection.center(), viewport, &view);
        assert!(view.zoom_box(viewport, selection, true));
        assert_eq!(view.center, center);
        assert_eq!(view.scale, 40.0);
    }

    #[test]
    fn box_zoom_out_places_old_view_inside_selection() {
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let selection =
            egui::Rect::from_two_pos(egui::pos2(100.0, 100.0), egui::pos2(300.0, 200.0));
        let mut view = super::MapView {
            center: [0.0, 0.0],
            scale: 12.0,
        };
        let corners = [viewport.left_top(), viewport.right_bottom()]
            .map(|p| super::world_at(p, viewport, &view));
        assert!(view.zoom_box(viewport, selection, false));
        assert_eq!(view.scale, 2.0);
        assert_eq!(
            super::world_to_screen([0.0, 0.0], viewport, &view),
            selection.center()
        );
        for corner in corners {
            assert!(
                selection
                    .expand(0.001)
                    .contains(super::world_to_screen(corner, viewport, &view))
            );
        }
        let previous = view;
        assert!(!view.zoom_box(
            viewport,
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1.0, 50.0)),
            true
        ));
        assert_eq!(view, previous);
    }
    use super::*;

    #[test]
    fn roundtrip_world_screen() {
        let rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1000.0, 800.0));
        let view = MapView {
            center: [116.4, 39.9],
            scale: 100.0,
        };
        let w: [f64; 2] = [117.0, 40.5];
        let p = world_to_screen(w, rect, &view);
        let back = world_at(p, rect, &view);
        assert!((back[0] - w[0]).abs() < 1e-4);
        assert!((back[1] - w[1]).abs() < 1e-4);
    }

    #[test]
    fn zoom_factor_at_keeps_cursor_point() {
        let rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1000.0, 800.0));
        let mut view = MapView {
            center: [116.4, 39.9],
            scale: 100.0,
        };
        let cursor = pos2(300.0, 200.0);
        let before = world_at(cursor, rect, &view);
        view.zoom_factor_at(cursor, rect, 1.8);
        let after = world_at(cursor, rect, &view);
        assert!((before[0] - after[0]).abs() < 1e-6);
        assert!((before[1] - after[1]).abs() < 1e-6);
        assert!(view.scale > 100.0);
    }

    /// `Projector` 必须与 `world_to_screen` / `visible_world` 语义完全一致：
    /// 它是渲染热路径上的等价加速实现，一旦漂移，裁剪与投影就会互相矛盾。
    #[test]
    fn projector_matches_world_to_screen() {
        let rect = Rect::from_min_max(pos2(30.0, 50.0), pos2(1030.0, 850.0));
        for view in [
            MapView {
                center: [116.4, 39.9],
                scale: 100.0,
            },
            MapView {
                center: [-73.9, 40.7],
                scale: 0.0025,
            },
            MapView {
                center: [0.0, 0.0],
                scale: 1e5,
            },
        ] {
            let proj = Projector::new(rect, &view);
            for w in [[116.0, 40.5], [117.3, 39.1], [-74.5, 41.2], [180.0, -85.0]] {
                let a = world_to_screen(w, rect, &view);
                let b = proj.project(w);
                assert!((a.x - b.x).abs() < 1e-3, "{a:?} vs {b:?}");
                assert!((a.y - b.y).abs() < 1e-3, "{a:?} vs {b:?}");
            }

            // 视口四角反投影回去必须落在 rect 边界上
            let world = proj.visible_world();
            for (sx, sy) in [
                (rect.min.x, rect.min.y),
                (rect.max.x, rect.max.y),
                (rect.center().x, rect.center().y),
            ] {
                let (wx, wy) = world_from_screen_raw(sx as f64, sy as f64, &proj);
                assert!(wx >= world.min[0] - 1e-9 && wx <= world.max[0] + 1e-9);
                assert!(wy >= world.min[1] - 1e-9 && wy <= world.max[1] + 1e-9);
            }
        }
    }

    /// 测试用：直接用 `Projector` 的仿射系数反解屏幕点（等价于 `world_at`）。
    fn world_from_screen_raw(sx: f64, sy: f64, p: &Projector) -> (f64, f64) {
        ((sx - p.bx) / p.kx, (sy - p.by) / p.ky)
    }

    #[test]
    fn fit_covers_bounds() {
        let rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1000.0, 800.0));
        let bounds = Aabb {
            min: [100.0, 20.0],
            max: [120.0, 40.0],
        };
        let view = MapView::fit(Some(bounds), rect);
        // 数据范围应完整落在视图内
        let w = [100.0, 20.0];
        let p = world_to_screen(w, rect, &view);
        assert!(p.x >= 0.0 && p.x <= 1000.0);
        assert!(p.y >= 0.0 && p.y <= 800.0);
    }
}
