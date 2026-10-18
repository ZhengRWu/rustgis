//! 叠加分析（Geoprocessing → 矢量叠加）引擎：裁剪 / 相交 / 联合 / 合并 / 融合。
//!
//! 与 [`super::buffer`]（缓冲区）共用同一套底座：
//!
//! * 坐标先归一化到千级跨度（[`Frame`]）再做布尔运算，规避经纬度图层
//!   （跨度 0.0x）浮点有效位不足导致的退化；
//! * 布尔运算交给 `geo::BooleanOps`（i_overlay），输入统一成「外环 CCW /
//!   孔 CW」（[`crate::geo::buffer::oriented`]）后使用 `FillRule::Positive`；
//! * 输入要素按属性行归并（`buffer::collect_features`），输出用
//!   [`Layer::from_features`] 重新组装属性表。
//!
//! 与缓冲区的差别：几何类型不再限于「缓冲成面」，而且**属性要跟着几何走**
//! —— 每个输出要素携带来自各个输入的属性（重名字段按 ArcGIS 的惯例加
//! `_1`、`_2` 后缀），因此结果图层可以直接导出、按属性着色。
//!
//! 各工具刻意与 ArcMap 的同名工具保持相同语义：
//!
//! | 工具 | 语义 |
//! | --- | --- |
//! | 裁剪 | 用「裁剪要素」（面）把输入要素切开，只留面内部分；属性原样保留 |
//! | 相交 | 多个同类型图层求公共部分；面走布尔求交，线输出交点，点取交集 |
//! | 联合 | 多个面图层叠加，重叠处不重复；按「先到先得」保留来源属性 |
//! | 合并 | 只把多个图层的要素拼进一个图层，几何一律不做运算（字段取并集） |
//! | 融合 | 按一个或多个字段分组，同组几何合并成一条要素 |

use std::collections::HashMap;

use geo::algorithm::bool_ops::{BooleanOps, FillRule, OpType};
use geo::algorithm::contains::Contains;
use geo::algorithm::orient::{Direction, Orient};
use geo::{Coord, LineString, MultiPolygon, Point, Polygon, unary_union};

use crate::geo::buffer::{collect_features, oriented, union_all};
use crate::geo::{Aabb, Coord2D, FeatureData, Geometry2D, Layer, PolygonData};

/// 归一化空间的目标跨度（与 `buffer::NORMALIZED_SPAN_MAX` 同量级）。
const NORMALIZED_SPAN: f64 = 1_000.0;

/// 归一化空间里的长度容差（去重、断点合并）。
const T_EPS: f64 = 1e-9;

/// 判定两条线段平行的相对容差（叉积 / (|r||s|)）。
const LINE_EPS: f64 = 1e-12;

/// 一段「几何 + 外接框 + 属性」的中间结果。
///
/// 属性与几何一起流动是叠加分析的核心差别（缓冲区结果是纯几何）：
/// 外接框随几何一同缓存，用于逐对运算前的快速排除。
type Piece = (MultiPolygon<f64>, Aabb, Vec<(String, String)>);

/// 折线 + 属性（线要素的中间表示，坐标已归一化）。
type LinePiece = (Vec<Coord<f64>>, Vec<(String, String)>);

/// 点 + 属性（点要素的中间表示，坐标已归一化）。
type PointPiece = (Coord<f64>, Vec<(String, String)>);

// ---------------------------------------------------------------------------
// 工具类型
// ---------------------------------------------------------------------------

/// 矢量叠加分析的五个工具。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayOp {
    Clip,
    Intersect,
    Union,
    Merge,
    Dissolve,
}

impl OverlayOp {
    /// 全部工具（对话框的工具下拉、菜单项顺序都按这个数组）。
    pub const ALL: [OverlayOp; 5] = [
        OverlayOp::Clip,
        OverlayOp::Intersect,
        OverlayOp::Union,
        OverlayOp::Merge,
        OverlayOp::Dissolve,
    ];

    /// 工具的中文名（引擎侧的错误提示用；界面上的名字走 i18n）。
    pub fn name(self) -> &'static str {
        match self {
            OverlayOp::Clip => "裁剪",
            OverlayOp::Intersect => "相交",
            OverlayOp::Union => "联合",
            OverlayOp::Merge => "合并",
            OverlayOp::Dissolve => "融合",
        }
    }

    /// i18n 键（工具名：对话框标题、工具下拉、状态提示都用它）。
    pub fn i18n_key(self) -> &'static str {
        match self {
            OverlayOp::Clip => "overlay.clip",
            OverlayOp::Intersect => "overlay.intersect",
            OverlayOp::Union => "overlay.union",
            OverlayOp::Merge => "overlay.merge",
            OverlayOp::Dissolve => "overlay.dissolve",
        }
    }

    /// 菜单项文案的 i18n 键（带省略号，表示点了会打开对话框）。
    pub fn menu_key(self) -> &'static str {
        match self {
            OverlayOp::Clip => "overlay.clip.menu",
            OverlayOp::Intersect => "overlay.intersect.menu",
            OverlayOp::Union => "overlay.union.menu",
            OverlayOp::Merge => "overlay.merge.menu",
            OverlayOp::Dissolve => "overlay.dissolve.menu",
        }
    }

    /// 由菜单项 key 反查工具：`menus.rs` 的菜单表与 `app::on_menu_item`
    /// 共用这同一份映射，避免两处各写一遍字符串。
    pub fn from_menu_key(key: &str) -> Option<OverlayOp> {
        OverlayOp::ALL
            .iter()
            .copied()
            .find(|op| op.menu_key() == key)
    }

    /// 工具帮助（对话框右栏「工具说明」）的 i18n 键。
    pub fn help_key(self) -> &'static str {
        match self {
            OverlayOp::Clip => "geo.help.tool.clip",
            OverlayOp::Intersect => "geo.help.tool.intersect",
            OverlayOp::Union => "geo.help.tool.union",
            OverlayOp::Merge => "geo.help.tool.merge",
            OverlayOp::Dissolve => "geo.help.tool.dissolve",
        }
    }

    /// 默认输出图层名后缀（`{输入名}{后缀}`，与 ArcGIS 的 `x_Clip` 一致）。
    pub fn suffix(self) -> &'static str {
        match self {
            OverlayOp::Clip => "_Clip",
            OverlayOp::Intersect => "_Intersect",
            OverlayOp::Union => "_Union",
            OverlayOp::Merge => "_Merge",
            OverlayOp::Dissolve => "_Dissolve",
        }
    }

    /// 默认输出图层名。
    pub fn default_output_name(self, input: &str) -> String {
        format!("{input}{}", self.suffix())
    }

    /// 最少输入图层数（其余工具都是「主输入 + 至少一个叠加输入」）。
    pub fn min_inputs(self) -> usize {
        match self {
            OverlayOp::Dissolve => 1,
            _ => 2,
        }
    }

    /// 是否需要勾选「融合字段」（只有融合工具用得到）。
    pub fn uses_fields(self) -> bool {
        matches!(self, OverlayOp::Dissolve)
    }

    /// 结果图层的强调色：每个工具一个色相，便于在内容列表里区分结果。
    ///
    /// ArcGIS 会给每个结果图层随机配色；这里取固定色（可复现、易测试）。
    pub fn accent(self) -> egui::Color32 {
        match self {
            OverlayOp::Clip => egui::Color32::from_rgb(0, 113, 188),
            OverlayOp::Intersect => egui::Color32::from_rgb(0, 137, 123),
            OverlayOp::Union => egui::Color32::from_rgb(106, 90, 176),
            OverlayOp::Merge => egui::Color32::from_rgb(216, 106, 44),
            OverlayOp::Dissolve => egui::Color32::from_rgb(56, 142, 60),
        }
    }
}

/// 图层的几何类型（决定叠加运算通道）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryClass {
    Point,
    Line,
    Polygon,
}

impl GeometryClass {
    fn label(self) -> &'static str {
        match self {
            GeometryClass::Point => "点",
            GeometryClass::Line => "线",
            GeometryClass::Polygon => "面",
        }
    }
}

/// 图层的几何类型：按「面 > 线 > 点」取（与图例符号的规则一致）。
///
/// 返回 `None` 表示图层没有几何数据（空图层 / 只有属性）。
pub fn class_of(layer: &Layer) -> Option<GeometryClass> {
    if !layer.outer_rings.is_empty() {
        Some(GeometryClass::Polygon)
    } else if !layer.lines.is_empty() {
        Some(GeometryClass::Line)
    } else if !layer.points.is_empty() {
        Some(GeometryClass::Point)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 按工具分派执行叠加分析。
///
/// `layers` 是对话框解析出来的输入图层（顺序 = 叠加顺序，第一个是主输入，
/// 结果图层的名字、投影、样式都从它继承）；`fields` 只有融合工具使用。
pub fn run(op: OverlayOp, layers: &[&Layer], fields: &[String]) -> Result<Layer, String> {
    if layers.len() < op.min_inputs() {
        return Err(format!(
            "{}至少需要 {} 个输入要素图层",
            op.name(),
            op.min_inputs()
        ));
    }
    match op {
        OverlayOp::Clip => clip(layers[0], layers[1]),
        OverlayOp::Intersect => intersect(layers),
        OverlayOp::Union => union_layers(layers),
        OverlayOp::Merge => merge(layers),
        OverlayOp::Dissolve => dissolve(layers[0], fields),
    }
}

/// 裁剪：用 `clip_layer` 的面把 `input` 切开，只保留面内的部分。
///
/// 面走布尔求交（孔的语义自动正确），线按裁剪面切段，点做落在面内判定。
/// 三种几何的输出要素都带输入要素的完整属性。
pub fn clip(input: &Layer, clip_layer: &Layer) -> Result<Layer, String> {
    let features = collect_features(input);
    if features.is_empty() {
        return Err("输入要素图层没有几何数据".to_string());
    }
    let clip_polys: Vec<PolygonData> = collect_features(clip_layer)
        .iter()
        .flat_map(|f| f.polys.iter().cloned())
        .collect();
    if clip_polys.is_empty() {
        return Err("裁剪要素必须是面图层".to_string());
    }

    let frame = Frame::new(&[input, clip_layer]);
    let shapes: Vec<Polygon<f64>> = clip_polys
        .iter()
        .filter_map(|poly| frame.polygon(poly))
        .collect();
    if shapes.is_empty() {
        return Err("裁剪要素的几何无效".to_string());
    }
    // 多个裁剪要素先合成一张掩膜（ArcGIS 的 Clip 也是「任意裁剪要素之内」）。
    let mask = oriented(unary_union(shapes.iter()));
    let boxes = polygon_boxes(&mask);

    let mut out: Vec<FeatureData> = Vec::new();
    for feature in &features {
        let attrs = feature.attrs(input);

        let mut pieces: Vec<MultiPolygon<f64>> = Vec::new();
        for poly in &feature.polys {
            if let Some(shape) = frame.polygon(poly) {
                let piece = MultiPolygon::new(vec![shape]).boolean_op_with_fill_rule(
                    &mask,
                    OpType::Intersection,
                    FillRule::Positive,
                );
                if !piece.0.is_empty() {
                    pieces.push(piece);
                }
            }
        }
        if !pieces.is_empty() {
            let merged = union_all(&pieces);
            push_shapes(&mut out, &frame, &merged, &attrs);
        }

        for line in &feature.lines {
            for segment in clip_line(line, &frame, &mask, &boxes) {
                out.push(FeatureData {
                    geometry: Geometry2D::Line(segment),
                    attrs: attrs.clone(),
                });
            }
        }

        for point in &feature.points {
            let p = frame.norm(*point);
            if mask.contains(&Point::new(p.x, p.y)) {
                out.push(FeatureData {
                    geometry: Geometry2D::Point(*point),
                    attrs: attrs.clone(),
                });
            }
        }
    }

    if out.is_empty() {
        return Err("裁剪结果为空（裁剪要素与输入要素没有重叠）".to_string());
    }
    Ok(finish(input, OverlayOp::Clip, out))
}

/// 相交：多个同类型图层求公共部分。
///
/// * 面：所有输入的几何两两求交（包围盒先做一次快速排除），属性合并；
/// * 点：所有输入都出现的点（同坐标视为同一点），属性合并；
/// * 线：输出「交点」要素（ArcGIS 的线相交同样是输出交点）；
///   线重叠段（共线）不参与，属于简化。
pub fn intersect(layers: &[&Layer]) -> Result<Layer, String> {
    if layers.len() < 2 {
        return Err("相交至少需要 2 个输入要素图层".to_string());
    }
    let class = class_of(layers[0]).ok_or_else(|| "输入要素图层没有几何数据".to_string())?;
    for layer in layers {
        if class_of(layer) != Some(class) {
            return Err(format!(
                "相交要求所有输入图层为同一几何类型（点 / 线 / 面），但「{}」是{}",
                layer.name,
                class_of(layer).map(GeometryClass::label).unwrap_or("空")
            ));
        }
    }
    match class {
        GeometryClass::Polygon => intersect_polygons(layers),
        GeometryClass::Line => intersect_lines(layers),
        GeometryClass::Point => intersect_points(layers),
    }
}

/// 联合：多个面图层叠加成一个互不重叠的图层。
///
/// 重叠部分归**先出现的输入**所有（ArcGIS 的 Union 会给每条输出要素带上
/// 各输入的属性，重叠处用一个零面积弧段表示；这里用「先到先得 + 属性跟随
/// 实际占有者」的等价简化，保证输出面之间不重叠、面积之和等于并集面积）。
pub fn union_layers(layers: &[&Layer]) -> Result<Layer, String> {
    if layers.is_empty() {
        return Err("联合需要一个输入要素图层".to_string());
    }
    for layer in layers {
        if class_of(layer) != Some(GeometryClass::Polygon) {
            return Err(format!(
                "联合只支持面要素（「{}」不是面图层，点 / 线请用合并）",
                layer.name
            ));
        }
    }

    let frame = Frame::new(layers);
    let mut covered = MultiPolygon::new(Vec::new());
    let mut covered_box = Aabb::empty();
    let mut out: Vec<FeatureData> = Vec::new();

    for layer in layers {
        for feature in collect_features(layer) {
            let shape = feature_shapes(&feature, &frame);
            if shape.0.is_empty() {
                continue;
            }
            let box_ = bounds_of(&shape);
            let piece = if covered.0.is_empty() || !covered_box.intersects(&box_) {
                shape.clone()
            } else {
                shape.boolean_op_with_fill_rule(&covered, OpType::Difference, FillRule::Positive)
            };
            if !piece.0.is_empty() {
                push_shapes(&mut out, &frame, &piece, &feature.attrs(layer));
            }
            // 已占用区域随输入顺序扩张（联合可结合，结果与归并顺序无关）。
            covered = oriented(unary_union([&covered, &shape]));
            covered_box = bounds_of(&covered);
        }
    }

    if out.is_empty() {
        return Err("联合结果为空".to_string());
    }
    Ok(finish(layers[0], OverlayOp::Union, out))
}

/// 合并：把多个图层的要素拼进一个图层，几何完全不动。
///
/// 字段取并集（先出现者在前），单个图层里没有的字段留空——与 ArcGIS 的
/// Merge（Data Management）一致。点 / 线 / 面都支持，但要求类型一致。
pub fn merge(layers: &[&Layer]) -> Result<Layer, String> {
    if layers.is_empty() {
        return Err("合并需要一个输入要素图层".to_string());
    }
    let class = class_of(layers[0]).ok_or_else(|| "输入要素图层没有几何数据".to_string())?;
    for layer in layers {
        if class_of(layer) != Some(class) {
            return Err(format!(
                "合并要求所有输入图层为同一几何类型（点 / 线 / 面），但「{}」是{}",
                layer.name,
                class_of(layer).map(GeometryClass::label).unwrap_or("空")
            ));
        }
    }

    let mut out: Vec<FeatureData> = Vec::new();
    for layer in layers {
        for feature in collect_features(layer) {
            let attrs = feature.attrs(layer);
            for point in &feature.points {
                out.push(FeatureData {
                    geometry: Geometry2D::Point(*point),
                    attrs: attrs.clone(),
                });
            }
            for line in &feature.lines {
                out.push(FeatureData {
                    geometry: Geometry2D::Line(line.clone()),
                    attrs: attrs.clone(),
                });
            }
            for poly in &feature.polys {
                out.push(FeatureData {
                    geometry: Geometry2D::Polygon {
                        outer: poly.outer.clone(),
                        holes: poly.holes.clone(),
                    },
                    attrs: attrs.clone(),
                });
            }
        }
    }

    if out.is_empty() {
        return Err("合并结果为空".to_string());
    }
    Ok(finish(layers[0], OverlayOp::Merge, out))
}

/// 融合：按 `fields` 的取值分组，同组几何合并成一条要素。
///
/// `fields` 为空 = 「融合全部」（ArcGIS 的 Dissolve All），输出一条要素。
/// 面按并集合并（相邻面真正粘成一条），点 / 线按原样拼接。
/// 输出要素只保留融合字段本身——统计字段（SUM / COUNT…）不在本次范围内。
///
/// 注意输出图层没有「多部件要素」概念（与导入路径一致：一个部件就是一条
/// 属性行），所以同一组里**互不相连**的多边形会各占一行（ArcGIS 会合成
/// 一条多部件要素）。属性值完全相同，只是行数不同。
pub fn dissolve(input: &Layer, fields: &[String]) -> Result<Layer, String> {
    let features = collect_features(input);
    if features.is_empty() {
        return Err("输入要素图层没有几何数据".to_string());
    }
    for name in fields {
        if !input.fields.iter().any(|f| f == name) {
            return Err(format!("输入要素中不存在字段「{name}」"));
        }
    }
    let class = class_of(input).ok_or_else(|| "输入要素图层没有几何数据".to_string())?;
    let frame = Frame::new(&[input]);

    // 按字段值分组，同时记住首次出现的顺序（输出顺序可复现）。
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, feature) in features.iter().enumerate() {
        let key = field_key(input, feature.row, fields);
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(i);
    }

    let mut out: Vec<FeatureData> = Vec::new();
    for key in order {
        let members = &groups[&key];
        let attrs = group_attrs(input, features[members[0]].row, fields);
        match class {
            GeometryClass::Polygon => {
                let shapes: Vec<MultiPolygon<f64>> = members
                    .iter()
                    .map(|&i| feature_shapes(&features[i], &frame))
                    .filter(|m| !m.0.is_empty())
                    .collect();
                if shapes.is_empty() {
                    continue;
                }
                let merged = union_all(&shapes);
                push_shapes(&mut out, &frame, &merged, &attrs);
            }
            GeometryClass::Line => {
                for &i in members {
                    for line in &features[i].lines {
                        out.push(FeatureData {
                            geometry: Geometry2D::Line(line.clone()),
                            attrs: attrs.clone(),
                        });
                    }
                }
            }
            GeometryClass::Point => {
                for &i in members {
                    for point in &features[i].points {
                        out.push(FeatureData {
                            geometry: Geometry2D::Point(*point),
                            attrs: attrs.clone(),
                        });
                    }
                }
            }
        }
    }

    if out.is_empty() {
        return Err("融合结果为空".to_string());
    }
    Ok(finish(input, OverlayOp::Dissolve, out))
}

// ---------------------------------------------------------------------------
// 归一化标架
// ---------------------------------------------------------------------------

/// 归一化标架：平移到范围中心 + 等比缩放到千级跨度。
///
/// 布尔运算的鲁棒性依赖坐标量级：经纬度图层的跨度只有 0.0x，双精度会把大
/// 部分有效位浪费在整数部分（`buffer::MetricScale` 里有同样的说明）。所有
/// 输入共用同一个标架，保证「是否重叠」的判定一致，也保证结果不会被拉伸。
#[derive(Debug, Clone, Copy)]
struct Frame {
    cx: f64,
    cy: f64,
    /// 归一化单位 → 图层单位。
    k: f64,
}

impl Frame {
    /// 覆盖所有输入图层范围的标架。
    fn new(layers: &[&Layer]) -> Frame {
        let mut bounds = Aabb::empty();
        for layer in layers {
            bounds.merge(layer.bounds);
        }
        if bounds.is_empty() {
            return Frame {
                cx: 0.0,
                cy: 0.0,
                k: 1.0,
            };
        }
        let center = bounds.center();
        let span = bounds.width().max(bounds.height());
        let k = if span.is_finite() && span > 0.0 {
            span / NORMALIZED_SPAN
        } else {
            1.0
        };
        Frame {
            cx: center[0],
            cy: center[1],
            k,
        }
    }

    fn norm(&self, p: Coord2D) -> Coord<f64> {
        Coord {
            x: (p[0] - self.cx) / self.k,
            y: (p[1] - self.cy) / self.k,
        }
    }

    fn denorm(&self, c: Coord<f64>) -> Coord2D {
        [self.cx + c.x * self.k, self.cy + c.y * self.k]
    }

    /// 环 → geo 闭合环（剔除非有限坐标与连续重复点；不足 3 点返回 `None`）。
    fn ring(&self, ring: &[Coord2D]) -> Option<LineString<f64>> {
        let mut coords: Vec<Coord<f64>> = Vec::with_capacity(ring.len() + 1);
        for p in ring {
            if !p[0].is_finite() || !p[1].is_finite() {
                continue;
            }
            let c = self.norm(*p);
            if coords.last().is_some_and(|l| l == &c) {
                continue;
            }
            coords.push(c);
        }
        while coords.len() > 1 && coords.first() == coords.last() {
            coords.pop();
        }
        if coords.len() < 3 {
            return None;
        }
        coords.push(coords[0]);
        Some(LineString::new(coords))
    }

    /// 输入面 → geo 多边形（外环 + 孔，方向规范化）。
    fn polygon(&self, poly: &PolygonData) -> Option<Polygon<f64>> {
        let exterior = self.ring(&poly.outer)?;
        let interiors: Vec<LineString<f64>> = poly
            .holes
            .iter()
            .filter_map(|hole| self.ring(hole))
            .collect();
        Some(Polygon::new(exterior, interiors).orient(Direction::Default))
    }

    /// 归一化空间的多边形 → 图层坐标的 [`PolygonData`]（丢弃退化的环）。
    fn shapes(&self, multi: &MultiPolygon<f64>) -> Vec<PolygonData> {
        let mut out = Vec::with_capacity(multi.0.len());
        for poly in &multi.0 {
            let Some(outer) = self.ring_to_layer(poly.exterior()) else {
                continue;
            };
            let holes = poly
                .interiors()
                .iter()
                .filter_map(|hole| self.ring_to_layer(hole))
                .collect();
            out.push(PolygonData { outer, holes });
        }
        out
    }

    /// 归一化环 → 图层坐标环：还原后去掉相邻重复点与闭合重复点。
    fn ring_to_layer(&self, ring: &LineString<f64>) -> Option<Vec<Coord2D>> {
        let mut out: Vec<Coord2D> = Vec::with_capacity(ring.0.len());
        for c in &ring.0 {
            let p = self.denorm(*c);
            let tol = 1e-9 * p[0].abs().max(p[1].abs()).max(1.0);
            if out
                .last()
                .is_some_and(|l| (l[0] - p[0]).abs() <= tol && (l[1] - p[1]).abs() <= tol)
            {
                continue;
            }
            out.push(p);
        }
        while out.len() > 1 {
            let (first, last) = (out[0], out[out.len() - 1]);
            let tol = 1e-9 * first[0].abs().max(first[1].abs()).max(1.0);
            if (first[0] - last[0]).abs() <= tol && (first[1] - last[1]).abs() <= tol {
                out.pop();
            } else {
                break;
            }
        }
        (out.len() >= 3).then_some(out)
    }
}

// ---------------------------------------------------------------------------
// 分通道实现
// ---------------------------------------------------------------------------

fn intersect_polygons(layers: &[&Layer]) -> Result<Layer, String> {
    let frame = Frame::new(layers);

    // 累加器：当前公共部分（几何 + 累积属性）。
    let mut acc: Vec<Piece> = Vec::new();
    for feature in collect_features(layers[0]) {
        let shape = feature_shapes(&feature, &frame);
        if shape.0.is_empty() {
            continue;
        }
        let box_ = bounds_of(&shape);
        acc.push((shape, box_, feature.attrs(layers[0])));
    }

    for layer in &layers[1..] {
        let others: Vec<Piece> = collect_features(layer)
            .iter()
            .filter_map(|feature| {
                let shape = feature_shapes(feature, &frame);
                (!shape.0.is_empty()).then(|| {
                    let box_ = bounds_of(&shape);
                    (shape, box_, feature.attrs(layer))
                })
            })
            .collect();

        let mut next = Vec::new();
        for (geom, geom_box, attrs) in &acc {
            for (other, other_box, other_attrs) in &others {
                // 包围盒不相交时布尔求交必然为空，先排除（大幅减少调用次数）。
                if !geom_box.intersects(other_box) {
                    continue;
                }
                let piece =
                    geom.boolean_op_with_fill_rule(other, OpType::Intersection, FillRule::Positive);
                if piece.0.is_empty() {
                    continue;
                }
                let box_ = bounds_of(&piece);
                next.push((piece, box_, merge_attrs(attrs, other_attrs)));
            }
        }
        acc = next;
        if acc.is_empty() {
            break;
        }
    }

    let mut out = Vec::new();
    for (geom, _, attrs) in &acc {
        push_shapes(&mut out, &frame, geom, attrs);
    }
    if out.is_empty() {
        return Err("相交结果为空（输入要素之间没有公共部分）".to_string());
    }
    Ok(finish(layers[0], OverlayOp::Intersect, out))
}

fn intersect_points(layers: &[&Layer]) -> Result<Layer, String> {
    let mut acc: Vec<(Coord2D, Vec<(String, String)>)> = Vec::new();
    for feature in collect_features(layers[0]) {
        let attrs = feature.attrs(layers[0]);
        for point in &feature.points {
            acc.push((*point, attrs.clone()));
        }
    }

    for layer in &layers[1..] {
        // 点图层用「坐标 → 属性」索引做存在性判定（同坐标视为同一点）。
        let mut index: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for feature in collect_features(layer) {
            let attrs = feature.attrs(layer);
            for point in &feature.points {
                index
                    .entry(point_key(*point))
                    .or_insert_with(|| attrs.clone());
            }
        }
        acc = acc
            .into_iter()
            .filter_map(|(point, attrs)| {
                index
                    .get(&point_key(point))
                    .map(|other| (point, merge_attrs(&attrs, other)))
            })
            .collect();
        if acc.is_empty() {
            break;
        }
    }

    if acc.is_empty() {
        return Err("相交结果为空（输入点图层之间没有公共点）".to_string());
    }
    let out = acc
        .into_iter()
        .map(|(point, attrs)| FeatureData {
            geometry: Geometry2D::Point(point),
            attrs,
        })
        .collect();
    Ok(finish(layers[0], OverlayOp::Intersect, out))
}

fn intersect_lines(layers: &[&Layer]) -> Result<Layer, String> {
    let frame = Frame::new(layers);

    // 候选：第 1、2 层的所有线段两两求交（带两层的属性）。
    let first = layer_lines(layers[0], &frame);
    let second = layer_lines(layers[1], &frame);
    let mut acc: Vec<PointPiece> = Vec::new();
    for (line_a, attrs_a) in &first {
        for (line_b, attrs_b) in &second {
            let attrs = merge_attrs(attrs_a, attrs_b);
            for a in line_a.windows(2) {
                for b in line_b.windows(2) {
                    if let Some(point) = segment_crossing(a[0], a[1], b[0], b[1]) {
                        acc.push((point, attrs.clone()));
                    }
                }
            }
        }
    }

    // 其余图层：交点必须落在该图层的某条线上。
    for layer in &layers[2..] {
        let others = layer_lines(layer, &frame);
        acc = acc
            .into_iter()
            .filter_map(|(point, attrs)| {
                others
                    .iter()
                    .find(|(line, _)| polyline_contains(line, point))
                    .map(|(_, other_attrs)| (point, merge_attrs(&attrs, other_attrs)))
            })
            .collect();
        if acc.is_empty() {
            break;
        }
    }

    // 同一条线被多个要素共享时会产生重复交点，按「坐标 + 属性」去重。
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (point, attrs) in acc {
        let p = frame.denorm(point);
        let key = format!(
            "{:.9},{:.9}|{}",
            point.x,
            point.y,
            attrs
                .iter()
                .map(|(k, v)| format!("{k}={v}\u{1e}"))
                .collect::<String>()
        );
        if seen.insert(key) {
            out.push(FeatureData {
                geometry: Geometry2D::Point(p),
                attrs,
            });
        }
    }

    if out.is_empty() {
        return Err("相交结果为空（输入线要素之间没有交点）".to_string());
    }
    Ok(finish(layers[0], OverlayOp::Intersect, out))
}

/// 图层的折线 + 各自的属性（归一化坐标）。
fn layer_lines(layer: &Layer, frame: &Frame) -> Vec<LinePiece> {
    let mut out = Vec::new();
    for feature in collect_features(layer) {
        if feature.lines.is_empty() {
            continue;
        }
        let attrs = feature.attrs(layer);
        for line in &feature.lines {
            let coords: Vec<Coord<f64>> = line.iter().map(|p| frame.norm(*p)).collect();
            if coords.len() >= 2 {
                out.push((coords, attrs.clone()));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 几何工具
// ---------------------------------------------------------------------------

/// 要素的面几何 → 归一化 `MultiPolygon`（方向规范化）。
fn feature_shapes(feature: &crate::geo::buffer::Feature, frame: &Frame) -> MultiPolygon<f64> {
    oriented(MultiPolygon::new(
        feature
            .polys
            .iter()
            .filter_map(|poly| frame.polygon(poly))
            .collect(),
    ))
}

/// 归一化空间里的一组多边形 → 输出要素（每个多边形一条要素，共享属性）。
fn push_shapes(
    out: &mut Vec<FeatureData>,
    frame: &Frame,
    multi: &MultiPolygon<f64>,
    attrs: &[(String, String)],
) {
    for poly in frame.shapes(multi) {
        out.push(FeatureData {
            geometry: Geometry2D::Polygon {
                outer: poly.outer,
                holes: poly.holes,
            },
            attrs: attrs.to_vec(),
        });
    }
}

/// 合并两组属性；重名字段加 `_1`、`_2`… 后缀（ArcGIS 的 Intersect 同样处理）。
fn merge_attrs(base: &[(String, String)], extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = base.to_vec();
    for (key, value) in extra {
        let mut name = key.clone();
        let mut n = 1;
        while out.iter().any(|(k, _)| k == &name) {
            name = format!("{key}_{n}");
            n += 1;
        }
        out.push((name, value.clone()));
    }
    out
}

/// 点的坐标键（按 9 位小数取整，用于「同一点」判定）。
fn point_key(point: Coord2D) -> String {
    format!("{:.9},{:.9}", point[0], point[1])
}

/// 融合分组键：把字段值按记录分隔符拼起来（字段值里不会出现该控制字符）。
fn field_key(layer: &Layer, row: Option<usize>, fields: &[String]) -> String {
    let mut key = String::new();
    for name in fields {
        key.push_str(&field_value(layer, row, name));
        key.push('\u{1f}');
    }
    key
}

/// 取某个字段的值（越界 / 无属性表时为空串）。
fn field_value(layer: &Layer, row: Option<usize>, name: &str) -> String {
    row.and_then(|r| layer.attrs.get(r))
        .and_then(|values| {
            layer
                .fields
                .iter()
                .position(|f| f == name)
                .and_then(|i| values.get(i))
        })
        .cloned()
        .unwrap_or_default()
}

/// 融合输出要素的属性：只保留融合字段本身（保证列存在，空值也保留占位）。
fn group_attrs(layer: &Layer, row: Option<usize>, fields: &[String]) -> Vec<(String, String)> {
    fields
        .iter()
        .map(|name| (name.clone(), field_value(layer, row, name)))
        .collect()
}

/// 归一化空间的包围盒（先排除不可能相交的要素对）。
fn bounds_of(multi: &MultiPolygon<f64>) -> Aabb {
    let mut bounds = Aabb::empty();
    for poly in &multi.0 {
        extend_polygon(&mut bounds, poly);
    }
    bounds
}

fn polygon_boxes(multi: &MultiPolygon<f64>) -> Vec<Aabb> {
    multi
        .0
        .iter()
        .map(|poly| {
            let mut bounds = Aabb::empty();
            extend_polygon(&mut bounds, poly);
            bounds
        })
        .collect()
}

fn extend_polygon(bounds: &mut Aabb, poly: &Polygon<f64>) {
    for c in poly
        .exterior()
        .0
        .iter()
        .chain(poly.interiors().iter().flat_map(|hole| hole.0.iter()))
    {
        if c.x.is_finite() && c.y.is_finite() {
            bounds.extend_point([c.x, c.y]);
        }
    }
}

/// 用掩膜多边形切分一条折线，返回落在面内的子折线（图层坐标）。
///
/// 做法：把折线每一段按「与掩膜所有边界线段的交点」切成子段，再用子段的
/// 中点做点在面内判定，保留在面内的子段并把首尾相接的子段合并成一条。
/// 点落在边界上时（geo 的 `Contains` 对边界返回 false）会整段丢弃，这是
/// 与 ArcGIS 的一处已知差异：跨边界的线在极小的退化情形下可能少一段。
fn clip_line(
    line: &[Coord2D],
    frame: &Frame,
    mask: &MultiPolygon<f64>,
    boxes: &[Aabb],
) -> Vec<Vec<Coord2D>> {
    let coords: Vec<Coord<f64>> = line.iter().map(|p| frame.norm(*p)).collect();
    if coords.len() < 2 {
        return Vec::new();
    }

    let mut result: Vec<Vec<Coord2D>> = Vec::new();
    let mut current: Vec<Coord<f64>> = Vec::new();

    for window in coords.windows(2) {
        let (a, b) = (window[0], window[1]);
        let segment_box = Aabb {
            min: [a.x.min(b.x), a.y.min(b.y)],
            max: [a.x.max(b.x), a.y.max(b.y)],
        };
        let mut params: Vec<f64> = vec![0.0, 1.0];
        for (i, poly) in mask.0.iter().enumerate() {
            if !boxes
                .get(i)
                .is_some_and(|bounds| bounds.intersects(&segment_box))
            {
                continue;
            }
            for ring in std::iter::once(poly.exterior()).chain(poly.interiors().iter()) {
                for edge in ring.0.windows(2) {
                    if let Some(t) = segment_param(a, b, edge[0], edge[1]) {
                        params.push(t);
                    }
                }
            }
        }
        params.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
        params.dedup_by(|x, y| (*x - *y).abs() <= T_EPS);

        for pair in params.windows(2) {
            let (t0, t1) = (pair[0], pair[1]);
            if t1 - t0 <= T_EPS {
                continue;
            }
            let mid = lerp(a, b, (t0 + t1) / 2.0);
            if !mask.contains(&Point::new(mid.x, mid.y)) {
                flush(&mut current, &mut result, frame);
                continue;
            }
            let start = lerp(a, b, t0);
            if current.last().is_none_or(|last| {
                (last.x - start.x).abs() > T_EPS || (last.y - start.y).abs() > T_EPS
            }) {
                flush(&mut current, &mut result, frame);
                current.push(start);
            }
            current.push(lerp(a, b, t1));
        }
    }
    flush(&mut current, &mut result, frame);
    result
}

/// 把攒下的归一化折线收尾成图层坐标的一段（不足 2 点丢弃）。
fn flush(current: &mut Vec<Coord<f64>>, result: &mut Vec<Vec<Coord2D>>, frame: &Frame) {
    if current.len() >= 2 {
        result.push(current.iter().map(|c| frame.denorm(*c)).collect());
    }
    current.clear();
}

fn lerp(a: Coord<f64>, b: Coord<f64>, t: f64) -> Coord<f64> {
    Coord {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
    }
}

/// 线段 `a→b` 与线段 `c→d` 的交点在 `a→b` 上的参数 `t`（不相交返回 `None`）。
fn segment_param(a: Coord<f64>, b: Coord<f64>, c: Coord<f64>, d: Coord<f64>) -> Option<f64> {
    let r = (b.x - a.x, b.y - a.y);
    let s = (d.x - c.x, d.y - c.y);
    let denom = r.0 * s.1 - r.1 * s.0;
    let r_len = r.0.hypot(r.1);
    let s_len = s.0.hypot(s.1);
    if r_len <= 0.0 || s_len <= 0.0 || denom.abs() <= LINE_EPS * r_len * s_len {
        return None;
    }
    let q = (c.x - a.x, c.y - a.y);
    let t = (q.0 * s.1 - q.1 * s.0) / denom;
    let u = (q.0 * r.1 - q.1 * r.0) / denom;
    ((0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u)).then_some(t)
}

/// 两条线段的交点（含端点相交；平行或共线返回 `None`）。
fn segment_crossing(
    a: Coord<f64>,
    b: Coord<f64>,
    c: Coord<f64>,
    d: Coord<f64>,
) -> Option<Coord<f64>> {
    segment_param(a, b, c, d).map(|t| lerp(a, b, t))
}

/// 折线上是否存在与 `point` 重合的点位（线相交的「落在线上」判定）。
///
/// 用点到线段的距离判定：`point == point` 会让参数法退化，距离法更稳。
fn polyline_contains(line: &[Coord<f64>], point: Coord<f64>) -> bool {
    line.windows(2).any(|window| {
        let (a, b) = (window[0], window[1]);
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let len2 = dx * dx + dy * dy;
        let t = if len2 <= 0.0 {
            0.0
        } else {
            (((point.x - a.x) * dx + (point.y - a.y) * dy) / len2).clamp(0.0, 1.0)
        };
        let px = a.x + dx * t - point.x;
        let py = a.y + dy * t - point.y;
        px.hypot(py) <= 1e-7
    })
}

/// 组装结果图层：命名、继承投影、按工具配强调色。
fn finish(input: &Layer, op: OverlayOp, features: Vec<FeatureData>) -> Layer {
    let mut layer = Layer::from_features(op.default_output_name(&input.name), features);
    layer.projection = input.projection.clone();
    let accent = op.accent();
    layer.style.fill = accent.gamma_multiply(0.45);
    layer.style.outline = accent;
    layer.style.outline_width = 1.2;
    layer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::load_file;
    use geo::algorithm::area::Area;
    use std::path::PathBuf;

    fn square_ring(x: f64, y: f64, size: f64) -> Vec<Coord2D> {
        vec![
            [x, y],
            [x + size, y],
            [x + size, y + size],
            [x, y + size],
            [x, y],
        ]
    }

    /// 面图层：每个方块一条属性行（字段 Id / Zone）。
    fn polygons(name: &str, squares: &[(f64, f64, f64, &str, &str)]) -> Layer {
        let features = squares
            .iter()
            .map(|(x, y, size, id, zone)| FeatureData {
                geometry: Geometry2D::Polygon {
                    outer: square_ring(*x, *y, *size),
                    holes: Vec::new(),
                },
                attrs: vec![
                    ("Id".to_string(), id.to_string()),
                    ("Zone".to_string(), zone.to_string()),
                ],
            })
            .collect();
        Layer::from_features(name.to_string(), features)
    }

    fn area_of(layer: &Layer) -> f64 {
        let mut total = 0.0;
        for ring in &layer.outer_rings {
            let coords: Vec<Coord<f64>> = ring.iter().map(|p| Coord { x: p[0], y: p[1] }).collect();
            if coords.len() >= 4 {
                total += Polygon::new(LineString::new(coords), Vec::new()).unsigned_area();
            }
        }
        total
    }

    /// 面积断言：布尔运算 + 归一化往返后会有 ~1e-10 量级的相对误差。
    fn assert_area(layer: &Layer, expected: f64) {
        let actual = area_of(layer);
        assert!(
            (actual - expected).abs() <= 1e-6 * expected.abs().max(1.0),
            "面积 {actual} 与期望 {expected} 不符"
        );
    }

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rustgis_overlay_{}_{}.geojson",
            tag,
            std::process::id()
        ))
    }

    #[test]
    fn clip_keeps_only_the_overlapping_part_and_all_attributes() {
        let input = polygons("roads", &[(0.0, 0.0, 100.0, "1", "A")]);
        let mask = polygons("mask", &[(50.0, 0.0, 100.0, "9", "M")]);
        let out = clip(&input, &mask).expect("clip");
        assert_eq!(out.feature_count, 1);
        assert_area(&out, 5_000.0);
        // 裁剪只保留输入属性，不引入裁剪要素的字段。
        assert_eq!(out.fields, vec!["Id".to_string(), "Zone".to_string()]);
        assert_eq!(out.attrs[0], vec!["1".to_string(), "A".to_string()]);
    }

    #[test]
    fn clip_drops_features_outside_the_mask() {
        let input = polygons(
            "pts",
            &[(0.0, 0.0, 10.0, "1", "A"), (500.0, 500.0, 10.0, "2", "B")],
        );
        let mask = polygons("mask", &[(0.0, 0.0, 20.0, "9", "M")]);
        let out = clip(&input, &mask).expect("clip");
        assert_eq!(out.feature_count, 1);
        assert_eq!(out.attrs[0][0], "1");
    }

    #[test]
    fn clip_without_overlap_reports_error() {
        let input = polygons("a", &[(0.0, 0.0, 10.0, "1", "A")]);
        let mask = polygons("b", &[(100.0, 100.0, 10.0, "9", "M")]);
        let err = clip(&input, &mask).unwrap_err();
        assert!(err.contains("裁剪结果为空"), "{err}");
    }

    #[test]
    fn clip_requires_a_polygon_mask() {
        let input = polygons("a", &[(0.0, 0.0, 10.0, "1", "A")]);
        let mut points =
            Layer::from_geometry("p".to_string(), vec![[1.0, 1.0]], Vec::new(), Vec::new());
        points.fields = vec!["Id".to_string()];
        points.attrs = vec![vec!["5".to_string()]];
        points.point_rows = vec![0];
        let err = clip(&input, &points).unwrap_err();
        assert!(err.contains("裁剪要素必须是面图层"), "{err}");
    }

    #[test]
    fn clip_splits_lines_and_keeps_points_inside() {
        let mut lines = Layer::from_geometry(
            "line".to_string(),
            Vec::new(),
            vec![vec![[-10.0, 50.0], [110.0, 50.0]]],
            Vec::new(),
        );
        lines.fields = vec!["Id".to_string()];
        lines.attrs = vec![vec!["7".to_string()]];
        lines.line_rows = vec![0];
        let mut points = Layer::from_geometry(
            "pts".to_string(),
            vec![[50.0, 50.0], [500.0, 500.0]],
            Vec::new(),
            Vec::new(),
        );
        points.fields = vec!["Id".to_string()];
        points.attrs = vec![vec!["1".to_string()], vec!["2".to_string()]];
        points.point_rows = vec![0, 1];

        // 线：只保留 [0,100] 区间内的一段（x∈[0,100]），长度 100。
        let mask = polygons("mask", &[(0.0, 0.0, 100.0, "9", "M")]);
        let clipped_line = clip(&lines, &mask).expect("clip line");
        assert_eq!(clipped_line.lines.len(), 1);
        let seg = &clipped_line.lines[0];
        assert!(
            (seg[0][0]).abs() < 1e-6 && (seg[1][0] - 100.0).abs() < 1e-6,
            "{seg:?}"
        );
        assert_eq!(clipped_line.attrs[0][0], "7");

        // 点：面内的保留，面外的丢弃。
        let clipped_points = clip(&points, &mask).expect("clip point");
        assert_eq!(clipped_points.points.len(), 1);
        assert_eq!(clipped_points.attrs[0][0], "1");
    }

    #[test]
    fn intersect_polygons_returns_the_shared_part_with_merged_attributes() {
        let a = polygons("a", &[(0.0, 0.0, 100.0, "1", "A")]);
        let b = polygons("b", &[(50.0, 50.0, 100.0, "2", "B")]);
        let out = intersect(&[&a, &b]).expect("intersect");
        assert_eq!(out.feature_count, 1);
        assert_area(&out, 2_500.0);
        // 两层同名字段（Id / Zone）按 ArcGIS 惯例加后缀。
        assert_eq!(out.fields, vec!["Id", "Zone", "Id_1", "Zone_1"]);
        assert_eq!(out.attrs[0], vec!["1", "A", "2", "B"]);
    }

    #[test]
    fn intersect_rejects_mixed_geometry_classes() {
        let a = polygons("a", &[(0.0, 0.0, 10.0, "1", "A")]);
        let mut points =
            Layer::from_geometry("p".to_string(), vec![[1.0, 1.0]], Vec::new(), Vec::new());
        points.fields = vec!["Id".to_string()];
        points.attrs = vec![vec!["5".to_string()]];
        points.point_rows = vec![0];
        let err = intersect(&[&a, &points]).unwrap_err();
        assert!(err.contains("同一几何类型"), "{err}");
    }

    #[test]
    fn intersect_lines_emits_crossing_points() {
        let mut a = Layer::from_geometry(
            "a".to_string(),
            Vec::new(),
            vec![vec![[0.0, 50.0], [100.0, 50.0]]],
            Vec::new(),
        );
        a.fields = vec!["Id".to_string()];
        a.attrs = vec![vec!["1".to_string()]];
        a.line_rows = vec![0];
        let mut b = Layer::from_geometry(
            "b".to_string(),
            Vec::new(),
            vec![vec![[50.0, 0.0], [50.0, 100.0]]],
            Vec::new(),
        );
        b.fields = vec!["Id".to_string()];
        b.attrs = vec![vec!["2".to_string()]];
        b.line_rows = vec![0];

        let out = intersect(&[&a, &b]).expect("intersect lines");
        assert_eq!(out.points.len(), 1);
        assert!((out.points[0][0] - 50.0).abs() < 1e-6 && (out.points[0][1] - 50.0).abs() < 1e-6);
        assert_eq!(out.attrs[0], vec!["1", "2"]);
    }

    #[test]
    fn intersect_points_keeps_only_common_positions() {
        let mut a = Layer::from_geometry(
            "a".to_string(),
            vec![[1.0, 1.0], [2.0, 2.0]],
            Vec::new(),
            Vec::new(),
        );
        a.fields = vec!["Id".to_string()];
        a.attrs = vec![vec!["1".to_string()], vec!["2".to_string()]];
        a.point_rows = vec![0, 1];
        let mut b = Layer::from_geometry(
            "b".to_string(),
            vec![[2.0, 2.0], [3.0, 3.0]],
            Vec::new(),
            Vec::new(),
        );
        b.fields = vec!["Id".to_string()];
        b.attrs = vec![vec!["9".to_string()], vec!["8".to_string()]];
        b.point_rows = vec![0, 1];

        let out = intersect(&[&a, &b]).expect("intersect points");
        assert_eq!(out.points.len(), 1);
        assert_eq!(out.points[0], [2.0, 2.0]);
        assert_eq!(out.attrs[0], vec!["2", "9"]);
    }

    #[test]
    fn union_partitions_overlaps_without_duplicating_area() {
        let a = polygons("a", &[(0.0, 0.0, 100.0, "1", "A")]);
        let b = polygons("b", &[(50.0, 0.0, 100.0, "2", "B")]);
        let out = union_layers(&[&a, &b]).expect("union");
        // 并集面积 = 100*100 + 100*100 - 50*100 = 15000。
        assert_area(&out, 15_000.0);
        // 输出面互不重叠：重叠部分归先出现的输入（a），b 只剩右半边。
        assert_eq!(out.feature_count, 2);
        assert_eq!(out.attrs[0][0], "1");
        assert_eq!(out.attrs[1][0], "2");
        let b_area = {
            let ring = &out.outer_rings[1];
            let coords: Vec<Coord<f64>> = ring.iter().map(|p| Coord { x: p[0], y: p[1] }).collect();
            Polygon::new(LineString::new(coords), Vec::new()).unsigned_area()
        };
        assert!((b_area - 5_000.0).abs() <= 1e-6 * 5_000.0, "{b_area}");
    }

    #[test]
    fn union_rejects_line_layers() {
        let mut lines = Layer::from_geometry(
            "l".to_string(),
            Vec::new(),
            vec![vec![[0.0, 0.0], [1.0, 1.0]]],
            Vec::new(),
        );
        lines.fields = vec!["Id".to_string()];
        lines.attrs = vec![vec!["1".to_string()]];
        lines.line_rows = vec![0];
        let err = union_layers(&[&lines]).unwrap_err();
        assert!(err.contains("联合只支持面要素"), "{err}");
    }

    #[test]
    fn merge_concatenates_layers_and_unions_fields() {
        let a = polygons("a", &[(0.0, 0.0, 10.0, "1", "A")]);
        let b = polygons(
            "b",
            &[(20.0, 0.0, 10.0, "2", "B"), (40.0, 0.0, 10.0, "3", "B")],
        );
        let out = merge(&[&a, &b]).expect("merge");
        assert_eq!(out.feature_count, 3);
        assert_eq!(out.fields, vec!["Id", "Zone"]);
        assert_eq!(out.attrs[2], vec!["3", "B"]);
        // 几何原样保留（不做任何布尔运算）。
        assert_area(&out, 300.0);
    }

    #[test]
    fn merge_dissolves_nothing_even_when_shapes_touch() {
        let a = polygons("a", &[(0.0, 0.0, 10.0, "1", "A")]);
        let b = polygons("b", &[(10.0, 0.0, 10.0, "2", "A")]);
        let out = merge(&[&a, &b]).expect("merge");
        assert_eq!(out.feature_count, 2, "合并只拼接、不融合");
    }

    #[test]
    fn dissolve_groups_features_by_field_value() {
        let input = polygons(
            "zones",
            &[
                (0.0, 0.0, 10.0, "1", "A"),
                (10.0, 0.0, 10.0, "2", "A"),
                (50.0, 0.0, 10.0, "3", "B"),
            ],
        );
        let out = dissolve(&input, &["Zone".to_string()]).expect("dissolve");
        assert_eq!(out.feature_count, 2, "两个 Zone 值 → 两条要素");
        assert_eq!(out.fields, vec!["Zone"]);
        assert_eq!(out.attrs[0], vec!["A"]);
        // A 组相邻两块粘成一条 20x10 的面。
        assert_area(&out, 300.0);
    }

    #[test]
    fn dissolve_all_without_fields_merges_everything() {
        let input = polygons(
            "zones",
            &[(0.0, 0.0, 10.0, "1", "A"), (50.0, 0.0, 10.0, "2", "B")],
        );
        let out = dissolve(&input, &[]).expect("dissolve all");
        // 两个不相连的方块融为一组，但输出图层「一个部件 = 一行」，
        // 所以是 2 行、属性为空、总面积不变（ArcGIS 会合成一条多部件要素）。
        assert_eq!(out.feature_count, 2);
        assert!(out.fields.is_empty());
        assert_area(&out, 200.0);
    }

    #[test]
    fn dissolve_reports_missing_field() {
        let input = polygons("zones", &[(0.0, 0.0, 10.0, "1", "A")]);
        let err = dissolve(&input, &["Nope".to_string()]).unwrap_err();
        assert!(err.contains("不存在字段"), "{err}");
    }

    #[test]
    fn run_checks_the_minimum_number_of_inputs() {
        let a = polygons("a", &[(0.0, 0.0, 10.0, "1", "A")]);
        let err = run(OverlayOp::Union, &[&a], &[]).unwrap_err();
        assert!(err.contains("至少需要 2 个"), "{err}");
        assert!(run(OverlayOp::Dissolve, &[&a], &[]).is_ok());
    }

    #[test]
    fn exported_geojson_round_trips_attributes() {
        let a = polygons("a", &[(0.0, 0.0, 100.0, "1", "A")]);
        let b = polygons("b", &[(50.0, 0.0, 100.0, "2", "B")]);
        let out = intersect(&[&a, &b]).expect("intersect");
        let path = temp_path("roundtrip");
        crate::geo::buffer::write_geojson(&out, &path).expect("write");
        let reloaded = load_file(&path).expect("reload");
        let _ = std::fs::remove_file(&path);
        assert_eq!(reloaded.feature_count, 1);
        // GeoJSON 的 properties 是 map，导入路径按字段名字典序成列，
        // 所以往返后列序会变（Id、Id_1、Zone、Zone_1），值一一对应。
        assert_eq!(reloaded.fields, vec!["Id", "Id_1", "Zone", "Zone_1"]);
        assert_eq!(reloaded.attrs[0], vec!["1", "2", "A", "B"]);
    }

    #[test]
    fn default_output_names_follow_arcgis_suffixes() {
        assert_eq!(OverlayOp::Clip.default_output_name("roads"), "roads_Clip");
        assert_eq!(
            OverlayOp::Dissolve.default_output_name("roads"),
            "roads_Dissolve"
        );
    }

    #[test]
    fn polygon_boxes_match_the_shapes() {
        let poly = PolygonData {
            outer: square_ring(0.0, 0.0, 10.0),
            holes: Vec::new(),
        };
        let frame = Frame::new(&[]);
        let shape = MultiPolygon::new(vec![frame.polygon(&poly).expect("polygon")]);
        let boxes = polygon_boxes(&shape);
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].min, [0.0, 0.0]);
        assert_eq!(boxes[0].max, [10.0, 10.0]);
    }
}
