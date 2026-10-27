//! 识别（Identify）：单击地图，读取最顶层可视要素 / 像元的信息。
//!
//! 命中规则与绘制顺序保持一致：
//! - 矢量图层永远画在栅格之上，因此先自上而下找矢量，再自上而下找栅格；
//! - 「自上而下」指内容列表的顶层优先（与 [`LayerManager::iter_visible`] 的反序一致）；
//! - 某一层在该点没有要素 / 像元时继续向下找，命中即停——只报告最顶层那一层，
//!   不把各层结果混在一起（ArcGIS 的 Identify 也会把最上层结果排在前面）。
//!
//! 矢量命中容差按**屏幕像素**折算到图层源 CRS（5 px），因此放大缩小时手感一致；
//! 栅格按点击点所在的像元取值，多波段时逐波段显示原始值（不做拉伸）。

use egui::{Context, RichText};

use super::RustGisApp;
use crate::geo::proj::CrsTransform;
use crate::geo::{Coord2D, Layer, LayerManager, ProjectionInfo};
use crate::raster::{PixelWindow, RasterDataset};
use crate::style;

/// 点选矢量要素的容差（屏幕像素）：点到线 / 点的距离在此范围内即算命中。
pub(super) const HIT_TOLERANCE_PX: f32 = 5.0;

/// 一个波段的采样结果；`None` 表示 NoData / 非有限值。
#[derive(Clone, Debug, PartialEq)]
pub(super) struct BandValue {
    pub name: String,
    pub value: Option<f64>,
}

/// 矢量命中：要素属性 + 点击位置（工程 CRS）。
#[derive(Clone, Debug, PartialEq)]
pub(super) struct VectorHit {
    pub layer: String,
    pub fid: usize,
    pub attributes: Vec<(String, String)>,
    pub location: Coord2D,
}

/// 栅格命中：像元行列 + 每个波段的原始值 + 点击位置（工程 CRS）。
#[derive(Clone, Debug, PartialEq)]
pub(super) struct RasterHit {
    pub layer: String,
    pub col: u32,
    pub row: u32,
    pub bands: Vec<BandValue>,
    pub location: Coord2D,
}

/// 一次识别的结果。
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Hit {
    Vector(VectorHit),
    Raster(RasterHit),
    /// 点击处没有任何可见要素 / 像元。
    None,
    /// 命中的栅格正在被后台解码 / 统计占用，稍后重试。
    RasterBusy,
    /// 读取像元失败（消息已本地化前的原始错误文本）。
    RasterError(String),
}

/// 「识别结果」窗口状态。
#[derive(Default)]
pub(super) struct IdentifyState {
    pub open: bool,
    pub hit: Option<Hit>,
}

impl RustGisApp {
    /// 在地图点击处执行识别：`screen` 为像素坐标，`view` 为本帧最终视图。
    pub(super) fn identify_at(
        &mut self,
        screen: egui::Pos2,
        rect: egui::Rect,
        view: &crate::map::MapView,
    ) {
        let world = crate::map::world_at(screen, rect, view);
        let tolerance = f64::from(HIT_TOLERANCE_PX) / f64::from(view.scale).max(f64::EPSILON);
        self.identify.hit = Some(hit_at(
            &self.layers,
            self.project_projection.as_ref(),
            world,
            tolerance,
        ));
        self.identify.open = true;
    }

    /// 「识别结果」浮窗（非模态：可以继续缩放、点选下一处）。
    pub(super) fn show_identify_window(&mut self, ctx: &Context) {
        if !self.identify.open {
            return;
        }
        let hit = self.identify.hit.clone();
        let lang = self.language;
        let title = lang.t("identify.title").to_string();
        let main_rect = ctx
            .input(|input| input.viewport().outer_rect)
            .unwrap_or_else(|| ctx.screen_rect());
        let pos = egui::pos2(
            (main_rect.right() - 396.0).max(main_rect.left() + 16.0),
            main_rect.top() + 96.0,
        );
        let viewport_id = egui::ViewportId::from_hash_of("identify_result_viewport");
        let viewport = egui::ViewportBuilder::default()
            .with_title(title.clone())
            .with_inner_size([380.0, 460.0])
            .with_min_inner_size([280.0, 180.0])
            .with_resizable(true)
            .with_close_button(true)
            .with_position(pos);

        let close_requested = ctx.show_viewport_immediate(viewport_id, viewport, |viewport_ctx, class| {
            if class == egui::ViewportClass::Embedded {
                let mut open = true;
                egui::Window::new(title.clone())
                    .id(egui::Id::new("identify_window"))
                    .open(&mut open)
                    .default_pos(pos)
                    .default_width(360.0)
                    .max_width(460.0)
                    .resizable(true)
                    .show(viewport_ctx, |ui| show_hit(ui, hit.as_ref(), lang));
                !open
            } else {
                if viewport_ctx.input(|input| input.viewport().close_requested()) {
                    true
                } else {
                    egui::CentralPanel::default().show(viewport_ctx, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("identify_results")
                            .max_height(420.0)
                            .show(ui, |ui| show_hit(ui, hit.as_ref(), lang));
                    });
                    false
                }
            }
        });
        if close_requested {
            self.identify.open = false;
        }
    }
}

fn show_hit(ui: &mut egui::Ui, hit: Option<&Hit>, lang: crate::i18n::Language) {
    if let Some(hit) = hit {
        hit_ui(ui, hit, lang);
    }
}

/// 命中信息 → 界面。`lang` 显式传入，避免闭包里再借 `self`。
fn hit_ui(ui: &mut egui::Ui, hit: &Hit, lang: crate::i18n::Language) {
    match hit {
        Hit::Vector(hit) => {
            ui.label(RichText::new(&hit.layer).strong());
            ui.weak(lang.t("identify.typeVector"));
            ui.add_space(4.0);
            coordinate_row(ui, lang, &hit.location);
            ui.separator();
            ui.label(lang.tf("identify.fid", &[&hit.fid.to_string()]));
            ui.add_space(4.0);
            if hit.attributes.is_empty() {
                ui.weak(lang.t("identify.noAttributes"));
            } else {
                egui::Grid::new("identify_attributes")
                    .num_columns(2)
                    .striped(true)
                    .show(ui, |ui| {
                        for (field, value) in &hit.attributes {
                            ui.label(field);
                            ui.label(if value.is_empty() { "—" } else { value });
                            ui.end_row();
                        }
                    });
            }
        }
        Hit::Raster(hit) => {
            ui.label(RichText::new(&hit.layer).strong());
            ui.weak(lang.t("identify.typeRaster"));
            ui.add_space(4.0);
            coordinate_row(ui, lang, &hit.location);
            ui.label(lang.tf(
                "identify.pixel",
                &[
                    lang.t("identify.col"),
                    &hit.col.to_string(),
                    lang.t("identify.row"),
                    &hit.row.to_string(),
                ],
            ));
            ui.separator();
            egui::Grid::new("identify_bands")
                .num_columns(2)
                .striped(true)
                .show(ui, |ui| {
                    for band in &hit.bands {
                        ui.label(&band.name);
                        match band.value {
                            Some(value) => ui.label(format_number(value)),
                            None => ui.weak(lang.t("identify.nodata")),
                        };
                        ui.end_row();
                    }
                });
        }
        Hit::None => {
            ui.weak(lang.t("identify.noHit"));
        }
        Hit::RasterBusy => {
            ui.weak(lang.t("identify.busy"));
        }
        Hit::RasterError(message) => {
            ui.colored_label(style::DANGER, lang.tf("identify.readError", &[message]));
        }
    }
}

fn coordinate_row(ui: &mut egui::Ui, lang: crate::i18n::Language, location: &Coord2D) {
    ui.label(lang.tf(
        "identify.location",
        &[&format_coord(location[0]), &format_coord(location[1])],
    ));
}

/// 工程坐标的显示精度：经纬度量级要 6 位，投影坐标（米）3 位足够。
fn format_coord(v: f64) -> String {
    let abs = v.abs();
    if !v.is_finite() {
        "—".to_string()
    } else if abs >= 1000.0 {
        format!("{v:.3}")
    } else {
        format!("{v:.6}")
    }
}

/// 波段值：整数原样显示，小数最多 6 位；极大 / 极小值用科学计数法。
fn format_number(v: f64) -> String {
    if !v.is_finite() {
        return "—".to_string();
    }
    if v != 0.0 && (v.abs() >= 1e7 || v.abs() < 1e-4) {
        return format!("{v:.6e}");
    }
    let text = format!("{v:.6}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

// ---------------------------------------------------------------------------
// 命中检测
// ---------------------------------------------------------------------------

/// 点击点（工程 CRS，`tolerance` 也是工程 CRS 长度）→ 命中结果。
pub(super) fn hit_at(
    layers: &LayerManager,
    project: Option<&ProjectionInfo>,
    click: Coord2D,
    tolerance: f64,
) -> Hit {
    if let Some(hit) = vector_hit(layers, project, click, tolerance) {
        return Hit::Vector(hit);
    }
    raster_hit(layers, project, click)
}

#[allow(clippy::type_complexity)]
fn vector_hit(
    layers: &LayerManager,
    project: Option<&ProjectionInfo>,
    click: Coord2D,
    tolerance: f64,
) -> Option<VectorHit> {
    let visible: Vec<&Layer> = layers.iter_visible().collect();
    for layer in visible.into_iter().rev() {
        if layer.bounds.is_empty() {
            continue;
        }
        let p = source_point(&layer.projection, project, click);
        let tol = source_tolerance(&layer.projection, project, click, tolerance);
        if !bounds_contains(layer.bounds, p, tol) {
            continue;
        }
        let Some(row) = feature_at(layer, p, tol) else {
            continue;
        };
        let values = layer.attrs.get(row).cloned().unwrap_or_default();
        let attributes = layer
            .fields
            .iter()
            .enumerate()
            .map(|(i, field)| (field.clone(), values.get(i).cloned().unwrap_or_default()))
            .collect();
        return Some(VectorHit {
            layer: layer.name.clone(),
            fid: row + 1,
            attributes,
            location: click,
        });
    }
    None
}

/// 在单个图层里找该点命中的要素：面 → 线 → 点；同类型取绘制顺序最靠后的
/// （后绘制的压在上面）。
fn feature_at(layer: &Layer, p: Coord2D, tol: f64) -> Option<usize> {
    let mut polygon_hit = None;
    let mut hole_start = 0usize;
    for (i, outer) in layer.outer_rings.iter().enumerate() {
        let count = layer.hole_counts.get(i).copied().unwrap_or(0);
        let hole_end = (hole_start + count).min(layer.hole_rings.len());
        let holes = &layer.hole_rings[hole_start..hole_end];
        hole_start = hole_end;
        if polygon_hit_at(p, outer, holes, tol) {
            polygon_hit = Some(row_of(&layer.ring_rows, i));
        }
    }
    if polygon_hit.is_some() {
        return polygon_hit;
    }
    for (i, line) in layer.lines.iter().enumerate().rev() {
        if dist_to_polyline(p, line) <= tol {
            return Some(row_of(&layer.line_rows, i));
        }
    }
    for (i, point) in layer.points.iter().enumerate().rev() {
        if dist2(*point, p) <= tol * tol {
            return Some(row_of(&layer.point_rows, i));
        }
    }
    None
}

/// 无属性映射的批量结果图层（缓冲 / 叠加）用几何下标当 FID。
fn row_of(rows: &[usize], index: usize) -> usize {
    rows.get(index).copied().unwrap_or(index)
}

fn polygon_hit_at(p: Coord2D, outer: &[Coord2D], holes: &[Vec<Coord2D>], tol: f64) -> bool {
    if outer.len() < 3 {
        return false;
    }
    let inside_outer = point_in_ring(p, outer);
    // 点在外环边界上（含框选时的半个像素误差）：算命中。
    if !inside_outer && dist_to_ring(p, outer) > tol {
        return false;
    }
    // 孔洞与孔边界都不属于面；落在孔里且不贴边时不算命中。
    for hole in holes {
        if inside_outer && dist_to_ring(p, hole) <= tol {
            return true;
        }
        if inside_outer && point_in_ring(p, hole) {
            return false;
        }
    }
    inside_outer || dist_to_ring(p, outer) <= tol
}

fn point_in_ring(p: Coord2D, ring: &[Coord2D]) -> bool {
    if ring.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let (xi, yi) = (ring[i][0], ring[i][1]);
        let (xj, yj) = (ring[j][0], ring[j][1]);
        if (yi > p[1]) != (yj > p[1]) && p[0] < (xj - xi) * (p[1] - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// 点到闭合环（含最后一段闭合边）的最短距离。
fn dist_to_ring(p: Coord2D, ring: &[Coord2D]) -> f64 {
    if ring.is_empty() {
        return f64::INFINITY;
    }
    let mut best = f64::INFINITY;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        best = best.min(dist_to_segment(p, a, b));
    }
    best
}

/// 点到折线（不闭合）的最短距离。
fn dist_to_polyline(p: Coord2D, line: &[Coord2D]) -> f64 {
    if line.is_empty() {
        return f64::INFINITY;
    }
    if line.len() == 1 {
        return dist2(line[0], p).sqrt();
    }
    let mut best = f64::INFINITY;
    for pair in line.windows(2) {
        best = best.min(dist_to_segment(p, pair[0], pair[1]));
    }
    best
}

fn dist_to_segment(p: Coord2D, a: Coord2D, b: Coord2D) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    if len2 <= f64::EPSILON {
        return dist2(p, a).sqrt();
    }
    let t = (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0);
    let q = [a[0] + dx * t, a[1] + dy * t];
    dist2(p, q).sqrt()
}

fn dist2(a: Coord2D, b: Coord2D) -> f64 {
    let (dx, dy) = (a[0] - b[0], a[1] - b[1]);
    dx * dx + dy * dy
}

// ---------------------------------------------------------------------------
// 栅格
// ---------------------------------------------------------------------------

fn raster_hit(layers: &LayerManager, project: Option<&ProjectionInfo>, click: Coord2D) -> Hit {
    let rasters: Vec<&crate::raster::RasterLayer> = layers.iter_visible_rasters().collect();
    for raster in rasters.into_iter().rev() {
        let p = source_point(&raster.projection, project, click);
        if !bounds_contains(raster.bounds, p, 0.0) {
            continue;
        }
        // 后台统计 / 解码正在持锁时不要卡住 UI：提示稍后重试。
        let Some(mut dataset) = raster.try_dataset() else {
            return Hit::RasterBusy;
        };
        match sample_raster(&mut dataset, p) {
            Ok(Some(hit)) => {
                return Hit::Raster(RasterHit {
                    layer: raster.name.clone(),
                    col: hit.0,
                    row: hit.1,
                    bands: hit.2,
                    location: click,
                });
            }
            // 点在影像包围盒内、但不在有效像元网格里（旋转仿射）：继续找下一层。
            Ok(None) => continue,
            Err(err) => return Hit::RasterError(err),
        }
    }
    Hit::None
}

type PixelSample = (u32, u32, Vec<BandValue>);

/// 取 `p`（栅格源 CRS）所在像元的所有波段值；点不在影像内时返回 `Ok(None)`。
fn sample_raster(dataset: &mut RasterDataset, p: Coord2D) -> Result<Option<PixelSample>, String> {
    let (width, height) = (dataset.width(), dataset.height());
    let (col, row) = match dataset.georeference().world_to_pixel(p[0], p[1]) {
        Some(pixel) => (pixel[0], pixel[1]),
        // 无地理参考：显示时约定「像素底边为 y=0、世界 y 朝上」。
        None => (p[0], f64::from(height) - p[1]),
    };
    if !(col.is_finite() && row.is_finite())
        || col < 0.0
        || row < 0.0
        || col >= f64::from(width)
        || row >= f64::from(height)
    {
        return Ok(None);
    }
    let (col, row) = (col.floor() as u32, row.floor() as u32);
    let band_count = dataset.band_count();
    if band_count == 0 {
        return Ok(None);
    }
    let indexes: Vec<usize> = (0..band_count).collect();
    let samples = dataset
        .read_level_bands(0, PixelWindow::new(col, row, 1, 1), &indexes)
        .map_err(|err| err.to_string())?;
    let mut bands = Vec::with_capacity(band_count);
    for (index, values) in samples.iter().enumerate() {
        let info = dataset.band(index);
        let name = info
            .map(|band| band.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("Band {}", index + 1));
        let nodata = info
            .and_then(|band| band.nodata)
            .or(dataset.metadata().nodata);
        let value = values
            .first()
            .copied()
            .map(f64::from)
            .filter(|value| value.is_finite() && !is_nodata(*value, nodata));
        bands.push(BandValue { name, value });
    }
    Ok(Some((col, row, bands)))
}

/// 浮点 NoData 比较：解码成 f32 后再提升，允许极小的表示误差。
fn is_nodata(value: f64, nodata: Option<f64>) -> bool {
    nodata.is_some_and(|nodata| {
        nodata.is_finite()
            && (value - nodata).abs() <= nodata.abs().max(1.0) * f64::from(f32::EPSILON) * 8.0
    })
}

// ---------------------------------------------------------------------------
// CRS / 容差
// ---------------------------------------------------------------------------

/// 工程 CRS 的点 → 图层源 CRS（无法识别任一侧时保持原值）。
fn source_point(layer: &ProjectionInfo, project: Option<&ProjectionInfo>, p: Coord2D) -> Coord2D {
    project
        .and_then(|project| CrsTransform::between(layer, project))
        .and_then(|xf| xf.unapply(p))
        .unwrap_or(p)
}

/// 把工程 CRS 下的像素容差折算到图层源 CRS（按两个方向的实际缩放取最大值）。
fn source_tolerance(
    layer: &ProjectionInfo,
    project: Option<&ProjectionInfo>,
    p: Coord2D,
    tolerance: f64,
) -> f64 {
    if tolerance <= 0.0 {
        return 0.0;
    }
    let origin = source_point(layer, project, p);
    let dx = source_point(layer, project, [p[0] + tolerance, p[1]]);
    let dy = source_point(layer, project, [p[0], p[1] + tolerance]);
    dist2(origin, dx).sqrt().max(dist2(origin, dy).sqrt())
}

fn bounds_contains(bounds: crate::geo::Aabb, p: Coord2D, pad: f64) -> bool {
    !bounds.is_empty()
        && p[0] >= bounds.min[0] - pad
        && p[0] <= bounds.max[0] + pad
        && p[1] >= bounds.min[1] - pad
        && p[1] <= bounds.max[1] + pad
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::PolygonData;
    use std::path::Path;

    fn square(name: &str, x: f64, y: f64, size: f64) -> Layer {
        let ring = vec![
            [x, y],
            [x + size, y],
            [x + size, y + size],
            [x, y + size],
            [x, y],
        ];
        let mut layer = Layer::from_geometry(
            name.to_string(),
            Vec::new(),
            Vec::new(),
            vec![PolygonData {
                outer: ring,
                holes: Vec::new(),
            }],
        );
        layer.fields = vec!["Id".to_string()];
        layer.attrs = vec![vec![name.to_string()]];
        layer
    }

    #[test]
    fn topmost_visible_vector_layer_wins() {
        let mut layers = LayerManager::default();
        let lower = layers.add(square("lower", 0.0, 0.0, 100.0));
        let upper = layers.add(square("upper", 50.0, 50.0, 100.0));

        // 重叠处：后添加的 upper 在内容列表顶部，也应该压在上面。
        let hit = hit_at(&layers, None, [75.0, 75.0], 0.0);
        assert!(
            matches!(&hit, Hit::Vector(hit) if hit.layer == "upper"),
            "{hit:?}"
        );

        // 只有 lower 覆盖的位置。
        let hit = hit_at(&layers, None, [20.0, 20.0], 0.0);
        assert!(
            matches!(&hit, Hit::Vector(hit) if hit.layer == "lower"),
            "{hit:?}"
        );

        // 顶层隐藏后应落到下一层，而不是报告一个不可见图层。
        layers.set_visible(upper, false);
        let hit = hit_at(&layers, None, [75.0, 75.0], 0.0);
        assert!(
            matches!(&hit, Hit::Vector(hit) if hit.layer == "lower"),
            "{hit:?}"
        );
        assert!(layers.node(lower).is_some());
    }

    #[test]
    fn polygon_hole_is_not_a_hit_but_its_edge_is() {
        let outer = vec![
            [0.0, 0.0],
            [100.0, 0.0],
            [100.0, 100.0],
            [0.0, 100.0],
            [0.0, 0.0],
        ];
        let hole = vec![
            [40.0, 40.0],
            [60.0, 40.0],
            [60.0, 60.0],
            [40.0, 60.0],
            [40.0, 40.0],
        ];
        let mut layer = Layer::from_geometry(
            "donut".to_string(),
            Vec::new(),
            Vec::new(),
            vec![PolygonData {
                outer,
                holes: vec![hole],
            }],
        );
        layer.fields = vec!["Id".to_string()];
        layer.attrs = vec![vec!["1".to_string()]];
        let mut layers = LayerManager::default();
        layers.add(layer);

        // 孔内不命中。
        assert_eq!(hit_at(&layers, None, [50.0, 50.0], 1.0), Hit::None);
        // 面上命中。
        assert!(matches!(
            hit_at(&layers, None, [20.0, 20.0], 1.0),
            Hit::Vector(_)
        ));
        // 孔边界算命中。
        assert!(matches!(
            hit_at(&layers, None, [50.0, 40.2], 0.5),
            Hit::Vector(_)
        ));
    }

    #[test]
    fn line_and_point_hits_use_tolerance() {
        let mut layer = Layer::from_geometry(
            "line".to_string(),
            vec![[10.0, 50.0]],
            vec![vec![[0.0, 0.0], [100.0, 0.0]]],
            Vec::new(),
        );
        layer.fields = vec!["Id".to_string()];
        layer.attrs = vec![vec!["p".to_string()], vec!["l".to_string()]];
        let mut layers = LayerManager::default();
        layers.add(layer);

        // 距离线 2 个单位：容差 5 → 命中。
        assert!(matches!(
            hit_at(&layers, None, [50.0, 2.0], 5.0),
            Hit::Vector(_)
        ));
        // 距离 6 个单位：超容差。
        assert_eq!(hit_at(&layers, None, [50.0, 6.0], 5.0), Hit::None);
        // 点在容差内。
        assert!(matches!(
            hit_at(&layers, None, [12.0, 50.0], 3.0),
            Hit::Vector(_)
        ));
    }

    #[test]
    fn raster_pixel_returns_every_band() {
        let path = crate::raster::fixture_tests::sample_rgb_geotiff("identify-rgb.tif");
        let raster =
            crate::raster::RasterLayer::open(&path, "rgb".to_string()).expect("open fixture");
        let (world, expected) = {
            let dataset = raster.dataset();
            let (col, row) = (10u32, 12u32);
            let world = dataset
                .georeference()
                .pixel_to_world(f64::from(col) + 0.5, f64::from(row) + 0.5)
                .expect("georeferenced fixture");
            let base = (row * 64 + col) * 3;
            let expected: Vec<u8> = (0..3).map(|band| ((base + band) % 253) as u8).collect();
            (world, expected)
        };
        let mut layers = LayerManager::default();
        layers.add_raster(raster);

        let hit = hit_at(&layers, None, world, 0.0);
        let Hit::Raster(hit) = hit else {
            panic!("应命中栅格：{hit:?}");
        };
        assert_eq!(hit.layer, "rgb");
        assert_eq!((hit.col, hit.row), (10, 12));
        assert_eq!(hit.bands.len(), 3);
        for (band, expected) in hit.bands.iter().zip(expected) {
            assert_eq!(band.value, Some(f64::from(expected))); // 多通道逐波段输出
        }
        let _ = std::fs::remove_file(Path::new(&path));
    }

    #[test]
    fn vector_above_raster_wins_even_inside_the_image() {
        let path = crate::raster::fixture_tests::sample_rgb_geotiff("identify-order.tif");
        let raster =
            crate::raster::RasterLayer::open(&path, "rgb".to_string()).expect("open fixture");
        let world = raster
            .dataset()
            .georeference()
            .pixel_to_world(5.0, 5.0)
            .expect("georeferenced fixture");
        let mut layers = LayerManager::default();
        layers.add_raster(raster);
        // 矢量在渲染顺序上永远压在所有栅格之上。
        layers.add(square("vector", world[0] - 10.0, world[1] - 10.0, 20.0));

        assert!(matches!(hit_at(&layers, None, world, 0.0), Hit::Vector(_)));

        // 把矢量移除后，同一个点应回落到栅格像元。
        let id = layers.panel_rows()[0].node.id();
        layers.remove(id);
        assert!(matches!(hit_at(&layers, None, world, 0.0), Hit::Raster(_)));
        let _ = std::fs::remove_file(Path::new(&path));
    }

    #[test]
    fn identify_at_opens_the_result_window() {
        let ctx = Context::default();
        let mut app = RustGisApp::new(&ctx);
        app.layers.add(square("parcel", 0.0, 0.0, 100.0));
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0));
        let view = crate::map::MapView {
            center: [50.0, 50.0],
            scale: 2.0,
        };
        app.identify_at(rect.center(), rect, &view);
        assert!(app.identify.open);
        assert!(
            matches!(&app.identify.hit, Some(Hit::Vector(hit)) if hit.layer == "parcel"),
            "{:?}",
            app.identify.hit
        );
    }
}
