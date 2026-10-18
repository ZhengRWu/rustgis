//! 图层渲染（自底向上）。
//!
//! 多边形填充使用导入时缓存的三角剖分（egui Mesh）；
//! 外环/孔/线要素描边；点要素渲染为圆点。
//!
//! # 性能结构
//!
//! 这个模块是"要素越多越卡"的正面战场，因此围绕三件事做优化：
//!
//! 1. **视口裁剪**（图层 → 路径 / 三角形 / 点）。epaint 对 `Mesh` 只按**整个网格**的
//!    包围盒剔除（`coarse_tessellation_culling`），屏幕外的顶点照样要被拷贝、
//!    上传再由 GPU 裁掉，所以逐要素裁剪必须由我们自己做。
//! 2. **帧缓存**（[`Renderer`]）。视图、画布矩形、图层版本三者都没变时
//!    （悬停、状态栏刷新、面板重绘），直接复用上一帧的形状，代价只剩一次 `Arc` 克隆。
//! 3. **缓冲复用**。重建时从顶点缓冲池取用 `Mesh`；描边路径的投影 / 清理写进
//!    跨帧复用的缓冲，不再每条路径、每帧各分配一次。
//!
//! 描边路径先经 [`crate::map::simplify`] 做屏幕空间清理与抽稀：
//! epaint 的闭合路径 miter 连接没有上限，路径一旦折返就会画出横穿视口的长尖刺，
//! 且缩小时顶点塌缩会让折返频繁出现（详见该模块文档）。

use std::sync::Arc;

use egui::epaint::{PathShape, Vertex, WHITE_UV};
use egui::{Color32, Mesh, Painter, Pos2, Rect, Shape, Stroke};

use crate::geo::proj::CrsTransform;
use crate::geo::{Aabb, Layer, LayerManager, ProjectionInfo};
use crate::map::{MapView, Projector, simplify};

/// 视口裁剪的像素余量：描边是骑在几何边界上的，抗锯齿羽化也会外溢一两个像素。
const CULL_PAD_PX: f32 = 4.0;

/// 点要素的圆周分段数。
///
/// 6 段（六边形扇形）在默认 3px 半径下与圆的偏差不到半个像素，肉眼不可辨，
/// 但完全绕开了 epaint 逐点构造圆弧路径 + `fill_and_stroke` 的开销
/// （实测 200k 点要 24.75ms，约 124ns/点）。
const POINT_SEGMENTS: usize = 6;

/// 单帧渲染统计（诊断与测试用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameStats {
    /// 本帧提交的形状数。
    pub shapes: usize,
    /// 本帧提交的顶点数（`Shape::Path` 按其点数计）。
    pub vertices: usize,
    /// 本帧是否重建了形状缓存（`false` = 命中缓存、直接复用上一帧）。
    pub rebuilt: bool,
}

/// 帧标识：只要它没变，上一帧的形状就依然成立。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FrameKey {
    center: [u64; 2],
    scale: u32,
    rect: [u32; 4],
    revision: u64,
    /// 工程 CRS：改变工程坐标系必须重建（顶点是按旧目标 CRS 变换过的）。
    project: Option<ProjectionInfo>,
}

impl FrameKey {
    fn new(rect: Rect, view: &MapView, revision: u64, project: Option<&ProjectionInfo>) -> Self {
        Self {
            center: [view.center[0].to_bits(), view.center[1].to_bits()],
            scale: view.scale.to_bits(),
            rect: [
                rect.min.x.to_bits(),
                rect.min.y.to_bits(),
                rect.max.x.to_bits(),
                rect.max.y.to_bits(),
            ],
            revision,
            project: project.cloned(),
        }
    }
}

/// 跨帧复用的地图渲染器。
///
/// 由调用方（`app`）长期持有：视图不变时它会把上一帧的形状原样再提交一次，
/// 视图变化时重建 —— 重建过程同样复用内部缓冲。
#[derive(Default)]
pub struct Renderer {
    shapes: Vec<Shape>,
    key: Option<FrameKey>,
    /// 顶点缓冲池：换下来的 `Mesh` 清空后留在这里，避免每帧重新分配几十万个顶点。
    pool: Vec<Mesh>,
    /// 描边点缓冲（`simplify` 的输出直接写进来，逐路径复用）。
    stroke_pts: Vec<Pos2>,
}

impl Renderer {
    /// 渲染所有可见图层到 `painter`（`rect` 为地图区域）。
    ///
    /// `project` 是工程 CRS：每个图层在绘制前把顶点从自己的源 CRS 变换到它。
    /// `None` 表示不做重投影（老工程 / 未设置工程坐标系）。
    pub fn render(
        &mut self,
        painter: &Painter,
        rect: Rect,
        layers: &LayerManager,
        view: &MapView,
        project: Option<&ProjectionInfo>,
    ) -> FrameStats {
        let key = FrameKey::new(rect, view, layers.revision(), project);
        let rebuilt = self.key.as_ref() != Some(&key);
        if rebuilt {
            self.rebuild(rect, layers, view, project, key);
        }
        painter.extend(self.shapes.iter().cloned());
        self.stats(rebuilt)
    }

    fn stats(&self, rebuilt: bool) -> FrameStats {
        let mut vertices = 0usize;
        for shape in &self.shapes {
            vertices += match shape {
                Shape::Mesh(mesh) => mesh.vertices.len(),
                Shape::Path(path) => path.points.len(),
                _ => 1,
            };
        }
        FrameStats {
            shapes: self.shapes.len(),
            vertices,
            rebuilt,
        }
    }

    /// 取一个空网格（优先复用池里的）。
    fn take_mesh(&mut self) -> Mesh {
        self.pool.pop().unwrap_or_default()
    }

    /// 把用不上 / 用过的网格归还缓冲池。
    ///
    /// `Shape::Mesh` 内部是 `Arc<Mesh>`：上一帧的形状可能仍被画布列表持有，
    /// 此时 `Arc::try_unwrap` 失败，直接丢弃（下一帧重新分配）即可。
    // `clippy::manual_clear` 在这里必须关掉：`Mesh::clear()` 会把顶点缓冲整个换掉
    // （`self.vertices = Default::default()`），容量留不下来——而留容量正是这个
    // 缓冲池的全部意义。按它的建议改，池子就白建了。
    #[allow(clippy::manual_clear)]
    fn release_mesh(&mut self, mesh: Arc<Mesh>) {
        if let Ok(mut mesh) = Arc::try_unwrap(mesh) {
            // 用 `truncate(0)` 而不是 `Mesh::clear()`，理由同上。
            mesh.vertices.truncate(0);
            mesh.indices.truncate(0);
            self.pool.push(mesh);
        }
    }

    fn push_mesh(&mut self, mesh: Mesh) {
        if mesh.is_empty() {
            self.release_mesh(Arc::new(mesh));
        } else {
            self.shapes.push(Shape::mesh(mesh));
        }
    }

    fn rebuild(
        &mut self,
        rect: Rect,
        layers: &LayerManager,
        view: &MapView,
        project: Option<&ProjectionInfo>,
        key: FrameKey,
    ) {
        // 先把上一帧的网格归还池子，再清空形状列表。
        for shape in std::mem::take(&mut self.shapes) {
            if let Shape::Mesh(mesh) = shape {
                self.release_mesh(mesh);
            }
        }
        self.key = Some(key);

        // 带线宽余量的视口范围：外环描边有一半在几何边界之外。
        let proj = Projector::new(rect, view);
        let visible = proj.visible_world_padded(CULL_PAD_PX);

        // `iter_visible` 已经把"自身或任一祖先未勾选"的图层滤掉，
        // 因此这里只需再挡掉没有几何（空包围盒）的图层。
        for layer in layers.iter_visible() {
            if layer.bounds.is_empty() {
                continue;
            }
            // 图层 CRS → 工程 CRS；同源或无法识别时为恒等（源坐标直接用）。
            let xf = project
                .and_then(|p| CrsTransform::between(&layer.projection, p))
                .filter(|xf| !xf.is_identity());
            // 图层级裁剪必须用**工程 CRS** 下的显示范围：逐顶点变换太贵。
            let shown_bounds = match &xf {
                Some(xf) => xf.apply_aabb(layer.bounds),
                None => layer.bounds,
            };
            // ① 整个图层在视口之外时一个顶点都不碰。
            if !shown_bounds.intersects(&visible) {
                continue;
            }
            // 逐要素 / 逐三角形的测试仍在**源 CRS** 里做，因此把视口反算回去。
            let source_visible = match &xf {
                Some(xf) => xf.unapply_aabb(visible),
                None => visible,
            };
            // ② 图层整体落在视口内时，逐要素的测试都可以省掉。
            let fully_visible = source_visible.contains_aabb(&layer.bounds);
            self.render_layer(&proj, xf.as_ref(), &source_visible, fully_visible, layer);
        }
    }

    fn render_layer(
        &mut self,
        proj: &Projector,
        xf: Option<&CrsTransform>,
        visible: &Aabb,
        fully_visible: bool,
        layer: &Layer,
    ) {
        let style = layer.style;
        self.render_fill(proj, xf, visible, fully_visible, layer);
        self.render_strokes(proj, xf, layer, style.outline_width);
        self.render_points(proj, xf, visible, fully_visible, layer);
    }

    /// 多边形填充：导入时缓存的三角剖分 → 一个 `Mesh`。
    fn render_fill(
        &mut self,
        proj: &Projector,
        xf: Option<&CrsTransform>,
        visible: &Aabb,
        fully_visible: bool,
        layer: &Layer,
    ) {
        if layer.triangles.is_empty() {
            return;
        }
        let fill = layer.style.fill;
        let mut mesh = self.take_mesh();
        mesh.reserve_triangles(layer.triangles.len());
        mesh.reserve_vertices(layer.triangles.len() * 3);
        for t in &layer.triangles {
            if !fully_visible && !triangle_in_view(&t[..], visible) {
                continue;
            }
            let base = mesh.vertices.len() as u32;
            let (x0, y0) = project_world(proj, xf, t[0], t[1]);
            let (x1, y1) = project_world(proj, xf, t[2], t[3]);
            let (x2, y2) = project_world(proj, xf, t[4], t[5]);
            mesh.vertices.push(vtx(Pos2::new(x0, y0), fill));
            mesh.vertices.push(vtx(Pos2::new(x1, y1), fill));
            mesh.vertices.push(vtx(Pos2::new(x2, y2), fill));
            mesh.add_triangle(base, base + 1, base + 2);
        }
        self.push_mesh(mesh);
    }

    /// 描边：外环（闭合）、孔（闭合）、线（开放）。
    fn render_strokes(
        &mut self,
        proj: &Projector,
        xf: Option<&CrsTransform>,
        layer: &Layer,
        width: f32,
    ) {
        let stroke = Stroke::new(width, layer.style.outline);
        let pad = CULL_PAD_PX + width * 0.5;
        for ring in layer.outer_rings.iter().chain(&layer.hole_rings) {
            simplify::stroke_path_into(ring, proj, xf, true, pad, &mut self.stroke_pts);
            if self.stroke_pts.len() >= 2 {
                self.shapes.push(Shape::Path(PathShape::closed_line(
                    self.stroke_pts.clone(),
                    stroke,
                )));
            }
        }
        for line in &layer.lines {
            simplify::stroke_path_into(line, proj, xf, false, pad, &mut self.stroke_pts);
            if self.stroke_pts.len() >= 2 {
                self.shapes.push(Shape::Path(PathShape::line(
                    self.stroke_pts.clone(),
                    stroke,
                )));
            }
        }
    }

    /// 点要素：按层批量构建成一个 `Mesh`（六边形扇形），不再逐点走 epaint 的路径机器。
    fn render_points(
        &mut self,
        proj: &Projector,
        xf: Option<&CrsTransform>,
        visible: &Aabb,
        fully_visible: bool,
        layer: &Layer,
    ) {
        let points = &layer.points;
        if points.is_empty() {
            return;
        }
        let (radius, color) = (layer.style.point_radius, layer.style.point_color);
        let mut rim = [(0.0f32, 0.0f32); POINT_SEGMENTS];
        for (k, slot) in rim.iter_mut().enumerate() {
            let a = std::f32::consts::TAU * k as f32 / POINT_SEGMENTS as f32;
            *slot = (a.cos() * radius, a.sin() * radius);
        }

        let mut mesh = self.take_mesh();
        mesh.reserve_vertices(points.len() * (POINT_SEGMENTS + 1));
        mesh.reserve_triangles(points.len() * POINT_SEGMENTS);
        for p in points {
            if !fully_visible && !visible.contains_point(*p) {
                continue;
            }
            let (cx, cy) = project_world(proj, xf, p[0], p[1]);
            let base = mesh.vertices.len() as u32;
            mesh.vertices.push(vtx(Pos2::new(cx, cy), color));
            for (dx, dy) in rim {
                mesh.vertices.push(vtx(Pos2::new(cx + dx, cy + dy), color));
            }
            for k in 0..POINT_SEGMENTS {
                let a = base + 1 + k as u32;
                let b = base + 1 + ((k + 1) % POINT_SEGMENTS) as u32;
                mesh.add_triangle(base, a, b);
            }
        }
        self.push_mesh(mesh);
    }
}

/// 渲染所有可见图层（一次性入口，每次都会重建形状）。
///
/// 只供测试与诊断使用：正式渲染走 [`Renderer`]，它能把"视图没变"的那些帧
/// 变成一次 `Arc` 克隆。
#[cfg(test)]
pub fn render_layers(painter: &Painter, rect: Rect, layers: &LayerManager, view: &MapView) {
    let mut renderer = Renderer::default();
    renderer.render(painter, rect, layers, view, None);
}

/// 源 CRS 的世界坐标 → 目标 CRS（`xf == None` 时原样）→ 屏幕坐标。
///
/// 变换失败（超出投影有效范围）时退回源坐标：宁可在屏幕上偏一点，也不让几何消失。
#[inline]
fn project_world(proj: &Projector, xf: Option<&CrsTransform>, x: f64, y: f64) -> (f32, f32) {
    let p = match xf {
        Some(xf) => xf.apply([x, y]).unwrap_or([x, y]),
        None => [x, y],
    };
    proj.project_xy(p[0], p[1])
}

#[inline(always)]
fn vtx(pos: Pos2, color: Color32) -> Vertex {
    Vertex {
        pos,
        uv: WHITE_UV,
        color,
    }
}

/// 三角形的世界包围盒是否与视口范围相交。
#[inline]
fn triangle_in_view(t: &[f64], view: &Aabb) -> bool {
    let x0 = t[0].min(t[2]).min(t[4]);
    if x0 > view.max[0] {
        return false;
    }
    let x1 = t[0].max(t[2]).max(t[4]);
    if x1 < view.min[0] {
        return false;
    }
    let y0 = t[1].min(t[3]).min(t[5]);
    if y0 > view.max[1] {
        return false;
    }
    let y1 = t[1].max(t[3]).max(t[5]);
    y1 >= view.min[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Color32, Context, Pos2, RawInput, epaint, pos2, vec2};

    use crate::geo::{Aabb, LayerStyle, PolygonData};

    /// 构造一个"世界坐标即屏幕坐标"的视图（rect 以原点为中心、scale = 1）。
    fn view_identity() -> MapView {
        MapView {
            center: [0.0, 0.0],
            scale: 1.0,
        }
    }

    /// 在 `rect` 中心画半径 `r`、`n` 个顶点的环，并在其中插入一个极小的折返。
    ///
    /// 折返刻意做成**近似**而非精确反向：精确反向时 epaint 的 `n0 + n1` 恰为 0，
    /// 外扩量是 NaN（几何直接消失）；真实数据是浮点舍入后的"几乎反向"，
    /// `length_sq` 极小但非零，外扩量可达数千像素 —— 这才是截图里那根可见的长尖刺。
    fn ring_with_needle(r: f64, n: usize) -> Vec<[f64; 2]> {
        let mut ring: Vec<[f64; 2]> = (0..n)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / n as f64;
                [a.cos() * r, a.sin() * r]
            })
            .collect();
        // 在 x 轴正向插入 "出去 1px、立刻折回" 的针尖
        let step = 1.0 / view_identity().scale as f64;
        ring.insert(n / 2, [r, 0.0]);
        ring.insert(n / 2 + 1, [r + step, 0.001]);
        ring.insert(n / 2 + 2, [r + step * 0.5, 0.0]);
        ring
    }

    fn ring_manager(rings: Vec<Vec<[f64; 2]>>) -> LayerManager {
        let mut bounds = Aabb::empty();
        for ring in &rings {
            for p in ring {
                bounds.extend_point(*p);
            }
        }
        let mut manager = LayerManager::default();
        manager.add(Layer {
            id: 0,
            name: "spike-test".to_string(),
            visible: true,
            style: LayerStyle::default(),
            bounds,
            feature_count: rings.len(),
            outer_rings: rings,
            ..Layer::default()
        });
        manager
    }

    fn single_ring_manager(ring: Vec<[f64; 2]>) -> LayerManager {
        ring_manager(vec![ring])
    }

    /// 让 egui 真正跑一帧 + 三角化，返回所有三角网格顶点的最大坐标绝对值。
    /// 尖刺会让这个值远远超出视口尺寸；出现非有限坐标则返回 `INFINITY`
    /// （注意 `f32::max` 会静默忽略 NaN，不能直接用它统计）。
    fn max_tessellated_coord(rect: Rect, draw: impl Fn(&Painter)) -> f32 {
        let ctx = Context::default();
        let input = RawInput {
            screen_rect: Some(rect),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                draw(&ui.painter_at(rect));
            });
        });
        let primitives = ctx.tessellate(output.shapes, 1.0);
        let mut worst = 0.0f32;
        for clipped in &primitives {
            if let epaint::Primitive::Mesh(mesh) = &clipped.primitive {
                for v in &mesh.vertices {
                    let m = v.pos.x.abs().max(v.pos.y.abs());
                    if !m.is_finite() {
                        return f32::INFINITY;
                    }
                    worst = worst.max(m);
                }
            }
        }
        worst
    }

    /// 跑一帧并收集提交给 egui 的形状。
    fn collect_shapes(rect: Rect, mut draw: impl FnMut(&Painter)) -> Vec<Shape> {
        let ctx = Context::default();
        let input = RawInput {
            screen_rect: Some(rect),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                draw(&ui.painter_at(rect));
            });
        });
        output.shapes.into_iter().map(|c| c.shape).collect()
    }

    fn path_points(shape: &Shape) -> Option<&Vec<Pos2>> {
        match shape {
            Shape::Path(p) => Some(&p.points),
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // 性能探针（手动运行）：
    // `cargo test --release --bin rustgis -- --ignored --nocapture probe_frame_cost`
    // 可用 `RUSTGIS_PROBE_RINGS` / `RUSTGIS_PROBE_VERTS` / `RUSTGIS_PROBE_POINTS`
    // 调整规模，`RUSTGIS_PROBE_ZOOM` 调整缩放倍数（1 = 全图视图）。
    // -----------------------------------------------------------------------

    fn env_usize(key: &str, default: usize) -> usize {
        std::env::var(key)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    }

    fn probe_ring(cx: f64, cy: f64, r: f64, n: usize, phase: f64) -> Vec<[f64; 2]> {
        let mut pts: Vec<[f64; 2]> = (0..n)
            .map(|i| {
                let a = phase + std::f64::consts::TAU * i as f64 / n as f64;
                let rr = if i % 2 == 0 { r } else { r * 0.72 };
                [cx + rr * a.cos(), cy + rr * a.sin()]
            })
            .collect();
        pts.push(pts[0]);
        pts
    }

    fn probe_layer(rings: usize, verts: usize, points: usize) -> Layer {
        let side = (rings as f64).sqrt().ceil().max(1.0) as usize;
        let polygons: Vec<crate::geo::PolygonData> = (0..rings)
            .map(|k| crate::geo::PolygonData {
                outer: probe_ring(
                    (k % side) as f64 * 0.5,
                    (k / side) as f64 * 0.5,
                    0.2,
                    verts,
                    k as f64 * 0.11,
                ),
                holes: Vec::new(),
            })
            .collect();
        let pts: Vec<[f64; 2]> = (0..points)
            .map(|k| [(k % 200) as f64 * 0.02, (k / 200) as f64 * 0.02])
            .collect();
        Layer::from_geometry("probe".to_string(), pts, Vec::new(), polygons)
    }

    /// 跑一帧（构建 Shape + egui 三角化），
    /// 返回 (构建 ms, 三角化 ms, 顶点数, 图元数, 本帧统计)。
    fn probe_frame(
        renderer: &mut Renderer,
        manager: &LayerManager,
        view: &MapView,
        rect: Rect,
    ) -> (f64, f64, usize, usize, FrameStats) {
        let ctx = Context::default();
        let input = RawInput {
            screen_rect: Some(rect),
            ..Default::default()
        };
        let build_ms = std::cell::Cell::new(0.0f64);
        let stats = std::cell::Cell::new(FrameStats::default());
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let painter = ui.painter_at(rect);
                let t = std::time::Instant::now();
                let s = renderer.render(&painter, rect, manager, view, None);
                build_ms.set(t.elapsed().as_secs_f64() * 1e3);
                stats.set(s);
            });
        });
        let t = std::time::Instant::now();
        let primitives = ctx.tessellate(output.shapes, 1.0);
        let tess_ms = t.elapsed().as_secs_f64() * 1e3;
        let mut vertices = 0usize;
        for clipped in &primitives {
            if let epaint::Primitive::Mesh(mesh) = &clipped.primitive {
                vertices += mesh.vertices.len();
            }
        }
        (
            build_ms.get(),
            tess_ms,
            vertices,
            primitives.len(),
            stats.get(),
        )
    }

    #[test]
    #[ignore = "perf probe"]
    fn probe_frame_cost() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(1600.0, 900.0));
        let (rings, verts, points) = (
            env_usize("RUSTGIS_PROBE_RINGS", 200),
            env_usize("RUSTGIS_PROBE_VERTS", 500),
            env_usize("RUSTGIS_PROBE_POINTS", 0),
        );
        let zoom: f64 = std::env::var("RUSTGIS_PROBE_ZOOM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0);

        let layer = probe_layer(rings, verts, points);
        let mut manager = LayerManager::default();
        manager.add(layer);
        let bounds = manager.total_bounds().unwrap();
        let mut view = MapView::fit(Some(bounds), rect);
        view.scale = (view.scale as f64 * zoom) as f32;

        let src_verts: usize = manager
            .iter_visible()
            .map(|l| {
                l.outer_rings.iter().map(|r| r.len()).sum::<usize>()
                    + l.lines.iter().map(|r| r.len()).sum::<usize>()
                    + l.points.len()
            })
            .sum();
        let tris: usize = manager.iter_visible().map(|l| l.triangles.len()).sum();

        let mut renderer = Renderer::default();
        // 预热一次（填充缓冲池）再计时，避免首帧的分配放大误差。
        let _ = probe_frame(&mut renderer, &manager, &view, rect);
        let (build, tess, vertices, prims, stats) =
            probe_frame(&mut renderer, &manager, &view, rect);
        // 缓存命中：视图没变，只应发生一次 `Arc` 克隆。
        let (hit, _, _, _, hit_stats) = probe_frame(&mut renderer, &manager, &view, rect);
        // 平移 1 像素：视口变化 ⇒ 必须整帧重建。
        let mut moved = view;
        moved.center[0] += 1.0 / view.scale as f64;
        let (pan, _, _, _, pan_stats) = probe_frame(&mut renderer, &manager, &moved, rect);

        println!(
            "规模：{rings} 环 × {verts} 顶点 + {points} 点 ⇒ 源顶点 {src_verts} / 三角 {tris}"
        );
        println!(
            "单帧：构建 {build:.2} ms + 三角化 {tess:.2} ms = {:.2} ms ⇒ 输出顶点 {vertices} / 图元 {prims} / 形状 {}",
            build + tess,
            stats.shapes
        );
        println!(
            "缓存命中：{hit:.2} ms（重建 {}）· 平移一帧：{pan:.2} ms（重建 {}）· 裁剪后形状 {} / 顶点 {}",
            hit_stats.rebuilt, pan_stats.rebuilt, pan_stats.shapes, pan_stats.vertices
        );
        println!(
            "理论帧率上限（仅此一项）：{:.1} fps",
            1000.0 / (build + tess).max(1e-6)
        );
    }

    /// 端到端回归：折返顶点交给 epaint 的闭合路径描边会画出失控尖刺
    /// （`add_line_loop` 的 miter 没有上限），而经过 `render_layers` 的清理后
    /// 三角化结果必须全部落在视口内。
    #[test]
    fn closed_path_spike_is_removed_before_tessellation() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0));
        let view = view_identity();
        let ring = ring_with_needle(120.0, 800);
        let manager = single_ring_manager(ring.clone());

        let limit = rect.max.x.max(rect.max.y) + 64.0;

        // (a) 未清理：直接把屏幕路径丢给 epaint —— 复现尖刺
        let raw_pts: Vec<Pos2> = ring
            .iter()
            .map(|w| crate::map::world_to_screen(*w, rect, &view))
            .collect();
        let raw_worst = max_tessellated_coord(rect, |painter| {
            painter.add(Shape::Path(PathShape::closed_line(
                raw_pts.clone(),
                Stroke::new(1.5_f32, Color32::RED),
            )));
        });
        assert!(
            raw_worst > limit,
            "用例未能复现尖刺（raw max coord = {raw_worst}），请检查构造"
        );

        // (b) 正式渲染路径：清理后不应再有越界几何
        let fixed_worst = max_tessellated_coord(rect, |painter| {
            render_layers(painter, rect, &manager, &view);
        });
        assert!(
            fixed_worst <= limit,
            "仍有越界几何：max coord = {fixed_worst}（raw = {raw_worst}）"
        );
    }

    // -----------------------------------------------------------------------
    // 视口裁剪
    // -----------------------------------------------------------------------

    /// 视口外的路径必须被整条丢弃；跨视口的路径必须与 `simplify::stroke_path`
    /// 的结果逐点一致（裁剪只应减少形状数量，不该改变几何）。
    #[test]
    fn off_view_rings_are_culled_without_changing_visible_geometry() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0));
        let view = view_identity(); // 世界 = 屏幕 - rect.center()，视口 = x∈[-200,200], y∈[-150,150]
        let inside = square(-100.0, -100.0, 100.0, 100.0);
        let outside = square(400.0, -100.0, 600.0, 100.0);
        let crossing = square(0.0, -50.0, 600.0, 50.0);
        let manager = ring_manager(vec![inside.clone(), outside, crossing.clone()]);

        let shapes = collect_shapes(rect, |painter| {
            render_layers(painter, rect, &manager, &view);
        });
        let paths: Vec<&Vec<Pos2>> = shapes.iter().filter_map(path_points).collect();
        assert_eq!(paths.len(), 2, "视口外的环没有被剔除");

        let expected_inside = simplify::stroke_path(&inside, rect, &view, true);
        let expected_crossing = simplify::stroke_path(&crossing, rect, &view, true);
        assert_eq!(*paths[0], expected_inside);
        assert_eq!(*paths[1], expected_crossing);
    }

    /// 整个图层与视口不相交时，一帧里不该出现任何形状。
    #[test]
    fn off_view_layer_draws_nothing() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0));
        let view = view_identity();
        let manager = ring_manager(vec![square(1000.0, 1000.0, 1100.0, 1100.0)]);
        let mut renderer = Renderer::default();
        let mut stats = FrameStats::default();
        collect_shapes(rect, |painter| {
            stats = renderer.render(painter, rect, &manager, &view, None);
        });
        assert_eq!(stats.shapes, 0);
        assert_eq!(stats.vertices, 0);
    }

    /// 三角形级裁剪：只有与视口相交的三角形才会进入网格。
    #[test]
    fn off_view_triangles_are_skipped() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0));
        let view = view_identity();
        let near = PolygonData {
            outer: square(-100.0, -100.0, 100.0, 100.0),
            holes: Vec::new(),
        };
        let far = PolygonData {
            outer: square(900.0, 900.0, 1000.0, 1000.0),
            holes: Vec::new(),
        };
        let layer =
            Layer::from_geometry("tris".to_string(), Vec::new(), Vec::new(), vec![near, far]);
        assert_eq!(layer.triangles.len(), 4, "两个矩形各应剖分成 2 个三角形");
        let mut manager = LayerManager::default();
        manager.add(layer);

        let mut renderer = Renderer::default();
        let mut stats = FrameStats::default();
        let shapes = collect_shapes(rect, |painter| {
            stats = renderer.render(painter, rect, &manager, &view, None);
        });
        // 这个图层有两个外环：近的在视口内、远的在外。视口外的环既不该进填充网格，
        // 也不该产生描边路径，因此形状恰好是「1 个填充网格 + 1 条外环描边」。
        assert_eq!(stats.shapes, 2, "视口外的外环不应产生任何形状");
        let mesh_vertices: usize = shapes
            .iter()
            .filter_map(|s| match s {
                Shape::Mesh(mesh) => Some(mesh.vertices.len()),
                _ => None,
            })
            .sum();
        assert_eq!(mesh_vertices, 6, "视口外的三角形没有被剔除");
    }

    /// 点要素：视口外的点被剔除，可见点批量进同一个网格。
    #[test]
    fn points_are_culled_and_batched() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0));
        let view = view_identity();
        let pts = vec![[-100.0, -50.0], [50.0, 50.0], [900.0, 900.0]];
        let layer = Layer::from_geometry("pts".to_string(), pts, Vec::new(), Vec::new());
        let mut manager = LayerManager::default();
        manager.add(layer);

        let mut renderer = Renderer::default();
        let mut stats = FrameStats::default();
        collect_shapes(rect, |painter| {
            stats = renderer.render(painter, rect, &manager, &view, None);
        });
        assert_eq!(stats.shapes, 1, "点要素应合并成一个网格");
        assert_eq!(stats.vertices, 2 * (POINT_SEGMENTS + 1));
    }

    // -----------------------------------------------------------------------
    // 帧缓存
    // -----------------------------------------------------------------------

    /// 视图 / 画布 / 图层版本都没变时必须命中缓存；任何一项变化都必须重建。
    #[test]
    fn frame_cache_follows_view_and_layer_revision() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0));
        let view = view_identity();
        let mut manager = single_ring_manager(square(-100.0, -100.0, 100.0, 100.0));
        let mut renderer = Renderer::default();
        let mut stats = FrameStats::default();

        let first = collect_shapes(rect, |painter| {
            stats = renderer.render(painter, rect, &manager, &view, None);
        });
        assert!(stats.rebuilt, "首帧必须构建");
        let first_paths: Vec<&Vec<Pos2>> = first.iter().filter_map(path_points).collect();

        let second = collect_shapes(rect, |painter| {
            stats = renderer.render(painter, rect, &manager, &view, None);
        });
        assert!(!stats.rebuilt, "视图未变却重建了缓存");
        let second_paths: Vec<&Vec<Pos2>> = second.iter().filter_map(path_points).collect();
        assert_eq!(first_paths, second_paths, "缓存命中时几何必须完全一致");

        // 平移一个像素
        let mut pan = view;
        pan.center[0] += 1.0;
        collect_shapes(rect, |painter| {
            stats = renderer.render(painter, rect, &manager, &pan, None);
        });
        assert!(stats.rebuilt, "视图变化后必须重建");

        // 图层变化（这里用可见性 + 版本号）
        manager.set_visible(0, false);
        collect_shapes(rect, |painter| {
            stats = renderer.render(painter, rect, &manager, &view, None);
        });
        assert!(stats.rebuilt, "图层版本变化后必须重建");
        assert_eq!(stats.shapes, 0, "隐藏的图层不应再提交任何形状");
    }

    /// 视口裁剪与抽稀的规模回归：缩到全图时视口外的路径必须被丢弃，
    /// 只有可见的少数路径进入描边流程。
    #[test]
    fn zoomed_view_culls_most_paths() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0));
        // 100 个环铺在 0..5 度，视图只覆盖其中约十分之一
        let rings: Vec<Vec<[f64; 2]>> = (0..100)
            .map(|k| {
                let (cx, cy) = ((k % 10) as f64, (k / 10) as f64);
                square(cx, cy, cx + 0.4, cy + 0.4)
            })
            .collect();
        let manager = ring_manager(rings);
        let bounds = manager.total_bounds().unwrap();
        let mut view = MapView::fit(Some(bounds), rect);
        view.scale *= 8.0; // 放大 8 倍 ⇒ 绝大多数环在视口之外

        let mut renderer = Renderer::default();
        let mut stats = FrameStats::default();
        collect_shapes(rect, |painter| {
            stats = renderer.render(painter, rect, &manager, &view, None);
        });
        let mut full = Renderer::default();
        let mut full_stats = FrameStats::default();
        view.scale /= 8.0;
        collect_shapes(rect, |painter| {
            full_stats = full.render(painter, rect, &manager, &view, None);
        });
        assert!(
            stats.shapes * 4 < full_stats.shapes,
            "放大 8 倍后仍然画了 {}/{} 条路径，视口裁剪没生效",
            stats.shapes,
            full_stats.shapes
        );
    }

    /// 绘制期重投影：WGS 84 工程里的 UTM 图层，顶点必须先按「源 CRS → 工程 CRS」
    /// 变换再投影到屏幕，否则米制坐标会被当成经纬度画到天文数字的位置。
    #[test]
    fn vector_vertices_are_reprojected_into_project_crs() {
        use crate::geo::proj::CrsTransform;
        use crate::geo::{CrsId, ProjectionInfo, ProjectionModel};

        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let utm = ProjectionInfo {
            model: ProjectionModel::Projected,
            crs: CrsId::Epsg(32632),
        };
        let wgs84 = ProjectionInfo {
            model: ProjectionModel::Geographic,
            crs: CrsId::Epsg(4326),
        };
        let mut manager = LayerManager::default();
        manager.add(Layer {
            id: 0,
            name: "utm-point".into(),
            visible: true,
            style: LayerStyle::default(),
            points: vec![[509_000.0, 5_000_000.0]],
            bounds: Aabb {
                min: [508_500.0, 4_999_500.0],
                max: [509_500.0, 5_000_500.0],
            },
            feature_count: 1,
            projection: utm.clone(),
            ..Layer::default()
        });

        let xf = CrsTransform::between(&utm, &wgs84).unwrap();
        let shown = xf.apply_aabb(manager.total_bounds().unwrap());
        let view = MapView::fit(Some(shown), rect);
        let projector = Projector::new(rect, &view);

        let mut renderer = Renderer::default();
        let mut stats = FrameStats::default();
        collect_shapes(rect, |painter| {
            stats = renderer.render(painter, rect, &manager, &view, Some(&wgs84));
        });
        assert_eq!(stats.vertices, POINT_SEGMENTS + 1, "点应画成一个网格");

        let expected = xf.apply([509_000.0, 5_000_000.0]).unwrap();
        let (ex, ey) = projector.project_xy(expected[0], expected[1]);
        let reprojected = renderer
            .shapes
            .iter()
            .filter_map(|shape| match shape {
                Shape::Mesh(mesh) => Some(&mesh.vertices),
                _ => None,
            })
            .flatten()
            .any(|v| (v.pos.x - ex).abs() < 0.01 && (v.pos.y - ey).abs() < 0.01);
        assert!(reprojected, "顶点应落在工程 CRS 投影后的屏幕位置");
        assert!(rect.contains(pos2(ex, ey)));

        // 同一视图下若不做变换，源坐标（米）会落到屏幕外很远。
        let (bx, by) = projector.project_xy(509_000.0, 5_000_000.0);
        assert!(
            (bx - ex).abs() > 1.0 || (by - ey).abs() > 1.0,
            "不做变换时屏幕坐标应当明显不同（{bx},{by} vs {ex},{ey}）"
        );
    }

    fn square(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<[f64; 2]> {
        vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]]
    }
}
