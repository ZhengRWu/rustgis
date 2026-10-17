//! 缓冲区（Buffer）分析引擎 —— 几何运算全部交给 [`geo`] crate。
//!
//! # 分工
//!
//! * 缓冲（偏移 + 圆角/端帽 + 自交清理）：`geo::Buffer`（底层 i_overlay 的
//!   stroke / outline，支持 Miter/Round/Bevel 连接与 Butt/Round/Square 端帽）。
//! * 布尔运算（融合、挖孔、求交）：`geo::BooleanOps` 与 `geo::unary_union`。
//! * 三角剖分：`geo::TriangulateEarcut`（见 [`crate::geo::tri`]）。
//!
//! # 总体思路
//!
//! 1. **归一化空间**：把输入坐标按 `p_norm = (p - 中心) / 单位距离` 变换到
//!    「缓冲距离 = 1」的空间里再做缓冲与布尔运算。坐标量级始终是
//!    「图层跨度 / 缓冲距离」——既不随经纬度（度）也不随投影（米）漂移，
//!    也不会让底层浮点容差大到吃掉缓冲环。
//! 2. **按要素缓冲**：点 → 圆；线 → 完整缓冲（端帽按末端类型）或单侧缓冲；
//!    面 → 外环外扩、孔收缩，负距离则整体内缩。
//! 3. **按融合类型分组**：NONE 每要素一组、ALL 全部一组、LIST 按字段取值组合
//!    分组；组内用 `unary_union` 消除重叠，组间保持独立要素。
//! 4. **地理坐标系近似测地线**：输入是地理坐标系且距离是线性单位时，按分组中心
//!    纬度把「米」换算成经纬度方向的度数（经向除以 `111320·cosφ`、纬向除以
//!    `110540`），再在压缩后的空间里做欧氏缓冲 —— 等价于一个各向异性的线性度量，
//!    小范围内与测地线缓冲一致（与 ArcGIS PLANAR 方法的行为相同）。
//!
//! 已知边界：投影坐标系一律按「图层单位 = 米」处理；跨大范围（数十度）的
//! 各向异性近似会失真；单侧缓冲（LEFT/RIGHT）用「完整缓冲 ∩ 半平面并集」实现。

use std::collections::HashMap;
use std::path::Path;

use geo::algorithm::area::Area;
use geo::algorithm::bool_ops::{BooleanOps, FillRule, OpType};
use geo::algorithm::buffer::{BufferStyle, LineCap, LineJoin};
use geo::algorithm::orient::{Direction, Orient};
use geo::{unary_union, Buffer as GeoBuffer, Coord, LineString, MultiPolygon, Point, Polygon};

use crate::geo::{Aabb, Coord2D, Layer, PolygonData, ProjectionModel};

/// 圆弧每象限段数（ArcGIS 默认 8 ⇒ 整圆 32 边形）。
const SEGMENTS_PER_QUADRANT: usize = 8;

/// i_overlay 用「最大角度步长」描述圆角/圆弧，这里由每象限段数换算
/// （8 段/象限 ⇒ 步长 π/16 ≈ 0.196，与 geo 默认的 0.20 一致）。
const JOIN_ANGLE: f64 = std::f64::consts::FRAC_PI_2 / SEGMENTS_PER_QUADRANT as f64;

/// 每米对应的纬度度数（WGS84 平均值，够用且无需椭球参数）。
const DEG_LAT_PER_METER: f64 = 1.0 / 110_540.0;
/// 赤道上每米对应的经度度数。
const DEG_LON_PER_METER_EQUATOR: f64 = 1.0 / 111_320.0;

/// 归一化空间的坐标跨度上限（约值）。
///
/// 归一化本来只按"缓冲距离 = 1"缩放，于是 **缓冲距离远小于图层跨度** 时
/// 归一化坐标会大到 1e7 量级；而底层布尔引擎的容差是相对坐标量级的
/// （`1e-9 · max(|坐标|)`），容差一旦接近要素尺寸，顶点会被并成一个点、
/// 环被当作退化丢掉，缓冲结果就会莫名变空。
/// 因此给缩放因子加一个下限（跨度 / 该常数，两个轴同倍放大），把归一化坐标
/// 压在千级，让引擎容差始终比要素尺寸小若干数量级——典型数据（缓冲距离与
/// 跨度同量级）完全不受影响，仍是"缓冲距离 = 1"。
/// 分组归一化（每组用自己的标架）后，这里量化的是**组内**跨度。
const NORMALIZED_SPAN_MAX: f64 = 1_000.0;

/// 融合时每次 `unary_union` 参与的形状数（见 `union_all` 的分层合并）。
const UNION_CHUNK: usize = 128;

/// 大量碎片互相重叠时改用两两归并（见 `overlap_ratio`）。
const PAIRWISE_CHUNK: usize = 2;

/// 重叠度阈值：超过则判定为「大量近乎重合的碎片」，改用两两归并。
const OVERLAP_RATIO: f64 = 32.0;

// ---------------------------------------------------------------------------
// 参数类型（与 ArcGIS「缓冲区」对话框一一对应）
// ---------------------------------------------------------------------------

/// 线性单位下拉（`度` 是角度单位，用于地理坐标系下的「按度数缓冲」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinearUnit {
    Meters,
    Kilometers,
    Feet,
    Miles,
    Yards,
    NauticalMiles,
    Degrees,
}

impl LinearUnit {
    pub const ALL: [LinearUnit; 7] = [
        LinearUnit::Meters,
        LinearUnit::Kilometers,
        LinearUnit::Feet,
        LinearUnit::Miles,
        LinearUnit::Yards,
        LinearUnit::NauticalMiles,
        LinearUnit::Degrees,
    ];

    /// 每单位多少米；`度` 无线性换算（返回 `None`）。
    pub fn meters(self) -> Option<f64> {
        match self {
            LinearUnit::Meters => Some(1.0),
            LinearUnit::Kilometers => Some(1000.0),
            LinearUnit::Feet => Some(0.3048),
            LinearUnit::Miles => Some(1609.344),
            LinearUnit::Yards => Some(0.9144),
            LinearUnit::NauticalMiles => Some(1852.0),
            LinearUnit::Degrees => None,
        }
    }

    /// i18n 键（三语表都收录，见 `i18n::every_language_covers_all_keys`）。
    pub fn i18n_key(self) -> &'static str {
        match self {
            LinearUnit::Meters => "buffer.unit.meters",
            LinearUnit::Kilometers => "buffer.unit.kilometers",
            LinearUnit::Feet => "buffer.unit.feet",
            LinearUnit::Miles => "buffer.unit.miles",
            LinearUnit::Yards => "buffer.unit.yards",
            LinearUnit::NauticalMiles => "buffer.unit.nautical_miles",
            LinearUnit::Degrees => "buffer.unit.degrees",
        }
    }
}

/// 距离来源：固定值 / 输入要素字段。
#[derive(Debug, Clone, PartialEq)]
pub enum DistanceSource {
    Value(f64),
    Field(String),
}

/// 侧类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideType {
    /// 双侧 / 面按「环绕并包含输入面」。
    Full,
    /// 仅线要素：拓扑左侧。
    Left,
    /// 仅线要素：拓扑右侧。
    Right,
    /// 仅面要素：只在外侧生成，并从结果中挖掉输入面内部。
    OutsideOnly,
}

impl SideType {
    pub const ALL: [SideType; 4] = [
        SideType::Full,
        SideType::Left,
        SideType::Right,
        SideType::OutsideOnly,
    ];

    pub fn i18n_key(self) -> &'static str {
        match self {
            SideType::Full => "buffer.side.full",
            SideType::Left => "buffer.side.left",
            SideType::Right => "buffer.side.right",
            SideType::OutsideOnly => "buffer.side.outside_only",
        }
    }
}

/// 末端类型（对线要素有效）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndType {
    /// 半圆端（默认）。
    Round,
    /// 平端：止于输入线端点。
    Flat,
    /// 方端：沿走向各外延一个缓冲距离。
    Square,
}

impl EndType {
    pub const ALL: [EndType; 3] = [EndType::Round, EndType::Flat, EndType::Square];

    pub fn i18n_key(self) -> &'static str {
        match self {
            EndType::Round => "buffer.end.round",
            EndType::Flat => "buffer.end.flat",
            EndType::Square => "buffer.end.square",
        }
    }
}

/// 融合类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DissolveType {
    /// 每个要素保留独立缓冲区（默认）。
    None,
    /// 全部融合为一个要素，消除重叠。
    All,
    /// 按列出字段的取值组合融合。
    List,
}

impl DissolveType {
    pub const ALL: [DissolveType; 3] = [
        DissolveType::None,
        DissolveType::All,
        DissolveType::List,
    ];

    pub fn i18n_key(self) -> &'static str {
        match self {
            DissolveType::None => "buffer.dissolve.none",
            DissolveType::All => "buffer.dissolve.all",
            DissolveType::List => "buffer.dissolve.list",
        }
    }
}

/// 一次缓冲区分析的全部参数（对话框确定时快照一份给后台线程）。
#[derive(Debug, Clone, PartialEq)]
pub struct BufferParams {
    pub distance: DistanceSource,
    pub unit: LinearUnit,
    pub side: SideType,
    pub end: EndType,
    pub dissolve: DissolveType,
    pub dissolve_fields: Vec<String>,
}

impl Default for BufferParams {
    fn default() -> Self {
        Self {
            distance: DistanceSource::Value(500.0),
            unit: LinearUnit::Meters,
            side: SideType::Full,
            end: EndType::Round,
            dissolve: DissolveType::None,
            dissolve_fields: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// 主入口
// ---------------------------------------------------------------------------

/// 输出图层默认名（与 ArcGIS 的 `输入名_Buffer` 一致）。
pub fn default_output_name(input: &str) -> String {
    format!("{input}_Buffer")
}

/// 对图层执行缓冲区分析，返回可直接加入地图的结果图层（投影与输入一致）。
pub fn run(layer: &Layer, params: &BufferParams) -> Result<Layer, String> {
    if layer.points.is_empty() && layer.lines.is_empty() && layer.outer_rings.is_empty() {
        return Err("输入要素图层没有几何数据".to_string());
    }
    let features = collect_features(layer);
    if features.is_empty() {
        return Err("输入要素图层没有几何数据".to_string());
    }

    // 每个要素的缓冲距离（用户单位）。
    let distances = feature_distances(layer, &features, params)?;

    // 先分组、再按组归一化：每组用自己的包围盒中心做原点、用组内平均
    // |距离| 做缩放。这样「缓冲距离 ≪ 图层跨度」时（例如全国范围点集
    // 上的 100 米缓冲），归一化坐标不会有 1e7 量级的偏移，底层布尔引擎
    // 的相对容差才不会大到把环顶点并成一点。
    let groups = group_indices(layer, &features, params);
    let outside_only = matches!(params.side, SideType::OutsideOnly);

    let mut polygons: Vec<PolygonData> = Vec::new();
    for members in &groups {
        let reference = members
            .iter()
            .map(|i| distances[*i].abs())
            .sum::<f64>()
            / members.len() as f64;
        if !reference.is_finite() || reference == 0.0 {
            return Err("缓冲距离不能为 0".to_string());
        }
        let (center, span) = group_frame(&features, members);
        let scale = MetricScale::new(
            layer.projection.model,
            center,
            reference,
            span,
            params.unit,
        )?;

        let mut pieces: Vec<MultiPolygon<f64>> = Vec::with_capacity(members.len());
        let mut originals: Vec<MultiPolygon<f64>> = Vec::new();
        for i in members {
            // 归一化到「缓冲距离 = 1」：负距离（侵蚀）保留符号，
            // 因此除以参考距离的**绝对值**，不能除以带符号的均值。
            let radius = distances[*i] / reference;
            pieces.push(buffer_feature(&features[*i], radius, params, &scale));
            if outside_only {
                let polys: Vec<Polygon<f64>> = features[*i]
                    .polys
                    .iter()
                    .filter_map(|p| geo_polygon(p, &scale))
                    .collect();
                if !polys.is_empty() {
                    originals.push(oriented(unary_union(polys.iter())));
                }
            }
        }

        let merged = union_all(&pieces);
        // 「排除输入面」：只在外侧生成 ⇒ 从缓冲结果里挖掉输入面。
        let merged = if originals.is_empty() {
            merged
        } else {
            let clip = union_all(&originals);
            merged.boolean_op_with_fill_rule(&clip, OpType::Difference, FillRule::Positive)
        };
        // 立即用本组的标架还原（不同组的标架不同，不能延后统一处理）。
        polygons.extend(polygons_to_data(&merged, &scale));
    }

    if polygons.is_empty() {
        return Err("缓冲结果为空（距离过小或要素退化）".to_string());
    }

    let mut result = Layer::from_geometry(default_output_name(&layer.name), Vec::new(), Vec::new(), polygons);
    result.projection = layer.projection.clone();
    // 缓冲区用半透明强调色填充，与源图层区分。
    result.style.fill = crate::style::ACCENT.gamma_multiply(0.45);
    result.style.outline = crate::style::ACCENT;
    result.style.outline_width = 1.2;
    Ok(result)
}

/// 把结果图层写成 GeoJSON 文件（对话框「输出要素类」指向的路径）。
///
/// 每个几何带上它所属要素的属性（行号取自 `*_rows`，值取自 [`Layer::attrs`]，
/// 列序同 [`Layer::fields`]）。缓冲区结果是纯几何（没有属性行），写出来仍是
/// `"properties": {}`；叠加分析（裁剪 / 相交 / 联合 / 合并 / 融合）的结果带
/// 属性，必须落盘，否则输出文件会丢字段。
pub fn write_geojson(layer: &Layer, path: &Path) -> Result<(), String> {
    use serde_json::{json, Map, Value};

    let ring_value = |ring: &[Coord2D]| -> Value {
        let mut coords: Vec<Value> = Vec::with_capacity(ring.len() + 1);
        for p in ring {
            coords.push(json!([p[0], p[1]]));
        }
        if let Some(first) = ring.first() {
            coords.push(json!([first[0], first[1]]));
        }
        Value::Array(coords)
    };

    // 属性行 → JSON 对象：空值不写（与导入路径的往返行为一致，避免冒出空字段）。
    let props = |row: Option<usize>| -> Value {
        let mut map = Map::new();
        if let Some(values) = row.and_then(|r| layer.attrs.get(r)) {
            for (i, name) in layer.fields.iter().enumerate() {
                let value = values.get(i).map(String::as_str).unwrap_or("");
                if !value.is_empty() {
                    map.insert(name.clone(), Value::String(value.to_string()));
                }
            }
        }
        Value::Object(map)
    };

    let mut features: Vec<Value> = Vec::new();
    let mut hole_cursor = 0usize;
    for (i, outer) in layer.outer_rings.iter().enumerate() {
        let count = layer.hole_counts.get(i).copied().unwrap_or(0);
        let end = (hole_cursor + count).min(layer.hole_rings.len());
        let holes = &layer.hole_rings[hole_cursor.min(end)..end];
        hole_cursor += count;
        let mut rings = vec![ring_value(outer)];
        rings.extend(holes.iter().map(|h| ring_value(h)));
        features.push(json!({
            "type": "Feature",
            "properties": props(layer.ring_rows.get(i).copied()),
            "geometry": { "type": "Polygon", "coordinates": rings },
        }));
    }
    for (i, line) in layer.lines.iter().enumerate() {
        let coords: Vec<Value> = line.iter().map(|p| json!([p[0], p[1]])).collect();
        features.push(json!({
            "type": "Feature",
            "properties": props(layer.line_rows.get(i).copied()),
            "geometry": { "type": "LineString", "coordinates": coords },
        }));
    }
    for (i, p) in layer.points.iter().enumerate() {
        features.push(json!({
            "type": "Feature",
            "properties": props(layer.point_rows.get(i).copied()),
            "geometry": { "type": "Point", "coordinates": [p[0], p[1]] },
        }));
    }

    let text = serde_json::to_string(&json!({
        "type": "FeatureCollection",
        "features": features,
    }))
    .map_err(|e| format!("GeoJSON 序列化失败：{e}"))?;
    std::fs::write(path, text).map_err(|e| format!("写入文件失败：{e}"))
}

// ---------------------------------------------------------------------------
// 输入要素整理
// ---------------------------------------------------------------------------

/// 一个输入要素（同一属性行下的全部几何）。
///
/// `pub(crate)` 供叠加分析引擎（[`super::overlay`]）复用：裁剪 / 相交 / 联合 /
/// 合并 / 融合都要「按属性行归并输入几何」这一步，与缓冲区完全一致。
pub(crate) struct Feature {
    pub(crate) points: Vec<Coord2D>,
    pub(crate) lines: Vec<Vec<Coord2D>>,
    pub(crate) polys: Vec<PolygonData>,
    /// 属性行号（`None` = 该图层没有属性表）。
    pub(crate) row: Option<usize>,
}

impl Feature {
    /// 该要素的属性键值对（按图层字段列序展开，跳过空值）。
    pub(crate) fn attrs(&self, layer: &Layer) -> Vec<(String, String)> {
        let Some(row) = self.row.and_then(|r| layer.attrs.get(r)) else {
            return Vec::new();
        };
        layer
            .fields
            .iter()
            .enumerate()
            .filter_map(|(i, name)| {
                let value = row.get(i)?;
                (!value.is_empty()).then(|| (name.clone(), value.clone()))
            })
            .collect()
    }
}

/// 按属性行号把几何归并成要素；无行号映射时每个几何各自成一个要素。
pub(crate) fn collect_features(layer: &Layer) -> Vec<Feature> {
    let mut feats: Vec<Feature> = Vec::new();
    let mut index: HashMap<usize, usize> = HashMap::new();

    for (i, p) in layer.points.iter().enumerate() {
        let s = slot_for(&mut feats, &mut index, layer.point_rows.get(i).copied());
        feats[s].points.push(*p);
    }
    for (i, line) in layer.lines.iter().enumerate() {
        let s = slot_for(&mut feats, &mut index, layer.line_rows.get(i).copied());
        feats[s].lines.push(line.clone());
    }
    let mut hole_cursor = 0usize;
    for (i, outer) in layer.outer_rings.iter().enumerate() {
        let count = layer.hole_counts.get(i).copied().unwrap_or(0);
        let end = (hole_cursor + count).min(layer.hole_rings.len());
        let holes: Vec<Vec<Coord2D>> = if hole_cursor < end {
            layer.hole_rings[hole_cursor..end].to_vec()
        } else {
            Vec::new()
        };
        hole_cursor += count;
        let s = slot_for(&mut feats, &mut index, layer.ring_rows.get(i).copied());
        feats[s].polys.push(PolygonData {
            outer: outer.clone(),
            holes,
        });
    }
    feats
}

fn slot_for(
    feats: &mut Vec<Feature>,
    index: &mut HashMap<usize, usize>,
    row: Option<usize>,
) -> usize {
    match row {
        Some(r) => {
            if let Some(i) = index.get(&r) {
                return *i;
            }
            feats.push(Feature {
                points: Vec::new(),
                lines: Vec::new(),
                polys: Vec::new(),
                row: Some(r),
            });
            let i = feats.len() - 1;
            index.insert(r, i);
            i
        }
        None => {
            feats.push(Feature {
                points: Vec::new(),
                lines: Vec::new(),
                polys: Vec::new(),
                row: None,
            });
            feats.len() - 1
        }
    }
}

/// 逐要素求缓冲距离（用户单位）。
fn feature_distances(
    layer: &Layer,
    features: &[Feature],
    params: &BufferParams,
) -> Result<Vec<f64>, String> {
    match &params.distance {
        DistanceSource::Value(v) => {
            if !v.is_finite() || *v == 0.0 {
                return Err("距离必须是有限的非零数值".to_string());
            }
            Ok(vec![*v; features.len()])
        }
        DistanceSource::Field(name) => {
            let col = layer
                .fields
                .iter()
                .position(|f| f == name)
                .ok_or_else(|| format!("输入要素中不存在字段「{name}」"))?;
            let mut out = Vec::with_capacity(features.len());
            for (i, f) in features.iter().enumerate() {
                let raw = f
                    .row
                    .and_then(|r| layer.attrs.get(r))
                    .and_then(|row| row.get(col))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                let v: f64 = raw
                    .parse()
                    .map_err(|_| format!("第 {} 行的字段「{name}」不是数值：{raw:?}", i + 1))?;
                if v == 0.0 {
                    return Err(format!("第 {} 行的字段「{name}」距离为 0", i + 1));
                }
                out.push(v);
            }
            Ok(out)
        }
    }
}

// ---------------------------------------------------------------------------
// 归一化空间（缓冲距离 = 1）
// ---------------------------------------------------------------------------

/// 图层坐标 ↔ 归一化坐标的换算。
///
/// `dx` / `dy` 是「一个单位缓冲距离」在图层坐标里对应的 x / y 跨度：
/// 投影坐标系下两者相等（按图层单位）；地理坐标系 + 线性单位下，
/// 经向按 `111320·cosφ`、纬向按 `110540` 各自换算 —— 于是归一化空间里的
/// 欧氏圆回到图层坐标就是一圈近似测地线的椭圆。
#[derive(Debug, Clone, Copy)]
struct MetricScale {
    cx: f64,
    cy: f64,
    dx: f64,
    dy: f64,
}

impl MetricScale {
    fn new(
        model: ProjectionModel,
        center: Coord2D,
        reference: f64,
        span: f64,
        unit: LinearUnit,
    ) -> Result<MetricScale, String> {
        let (cx, cy) = (center[0], center[1]);
        let geographic = matches!(model, ProjectionModel::Geographic);
        let value = reference.abs();
        let mut scale = match (geographic, unit.meters()) {
            (true, Some(meters)) => {
                let meters = value * meters;
                let cos_lat = cosd(cy).abs().max(0.01);
                MetricScale {
                    cx,
                    cy,
                    dx: meters * DEG_LON_PER_METER_EQUATOR / cos_lat,
                    dy: meters * DEG_LAT_PER_METER,
                }
            }
            // 投影坐标系（一律按图层单位处理）或角度单位：各向同性。
            _ => MetricScale {
                cx,
                cy,
                dx: value,
                dy: value,
            },
        };
        // 下限量纲是「图层坐标 / 归一化单位」（与 dx、dy 相同），因此两个轴
        // 同倍放大，既不破坏各向异性（测地线近似）也不改变缓冲距离语义 ——
        // 它只把归一化坐标压在 NORMALIZED_SPAN_MAX 量级，保证平面排列引擎的
        // 相对容差 `1e-9·|坐标|` 始终远小于缓冲环。见 NORMALIZED_SPAN_MAX。
        if scale.dx > 0.0 && scale.dy > 0.0 {
            let min_scale = span / NORMALIZED_SPAN_MAX;
            let smallest = scale.dx.min(scale.dy);
            if min_scale > smallest {
                let factor = min_scale / smallest;
                scale.dx *= factor;
                scale.dy *= factor;
            }
        }
        if !scale.dx.is_finite() || !scale.dy.is_finite() || scale.dx <= 0.0 || scale.dy <= 0.0 {
            return Err("缓冲距离过小或图层跨度过大，无法归一化".to_string());
        }
        Ok(scale)
    }

    fn normalize(&self, p: Coord2D) -> Coord2D {
        [(p[0] - self.cx) / self.dx, (p[1] - self.cy) / self.dy]
    }

    fn denormalize(&self, p: Coord2D) -> Coord2D {
        [self.cx + p[0] * self.dx, self.cy + p[1] * self.dy]
    }
}

// ---------------------------------------------------------------------------
// 融合分组
// ---------------------------------------------------------------------------

/// 按融合类型把要素分组（存的是 `features` 下标）：NONE 每个要素一组、
/// ALL 全部一组、LIST 按字段取值组合分组。同组要素共用一个归一化标架。
fn group_indices(layer: &Layer, features: &[Feature], params: &BufferParams) -> Vec<Vec<usize>> {
    match params.dissolve {
        DissolveType::None => (0..features.len()).map(|i| vec![i]).collect(),
        DissolveType::All => vec![(0..features.len()).collect()],
        DissolveType::List => {
            let mut order: Vec<String> = Vec::new();
            let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
            for (i, f) in features.iter().enumerate() {
                let key = dissolve_key(layer, f.row, &params.dissolve_fields);
                if !buckets.contains_key(&key) {
                    order.push(key.clone());
                    buckets.insert(key.clone(), Vec::new());
                }
                buckets
                    .get_mut(&key)
                    .expect("刚插入的分组必然存在")
                    .push(i);
            }
            order
                .into_iter()
                .filter_map(|k| buckets.remove(&k))
                .collect()
        }
    }
}

/// 一组要素的归一化标架：包围盒中心 + 跨度（两轴取大者）。
fn group_frame(features: &[Feature], members: &[usize]) -> (Coord2D, f64) {
    let mut bounds = Aabb::empty();
    for i in members {
        let f = &features[*i];
        for p in &f.points {
            bounds.extend_point(*p);
        }
        for line in &f.lines {
            for p in line {
                bounds.extend_point(*p);
            }
        }
        for poly in &f.polys {
            for p in &poly.outer {
                bounds.extend_point(*p);
            }
            for hole in &poly.holes {
                for p in hole {
                    bounds.extend_point(*p);
                }
            }
        }
    }
    let span = if bounds.is_empty() {
        0.0
    } else {
        bounds.width().max(bounds.height())
    };
    (bounds.center(), span)
}

/// 融合字段取值组合（用不可见分隔符拼接，避免 "a|b" 与 "a|" + "b" 撞键）。
fn dissolve_key(layer: &Layer, row: Option<usize>, fields: &[String]) -> String {
    fields
        .iter()
        .map(|f| {
            let col = layer.fields.iter().position(|x| x == f);
            row.and_then(|r| layer.attrs.get(r))
                .and_then(|values| col.and_then(|c| values.get(c)).cloned())
                .unwrap_or_default()
        })
        .collect::<Vec<String>>()
        .join("\u{1f}")
}

// ---------------------------------------------------------------------------
// 几何引擎（缓冲/布尔/偏移全部交给 `geo` crate）
// ---------------------------------------------------------------------------

/// 把归一化空间里的 `MultiPolygon` 统一成「外环 CCW / 孔 CW」。
///
/// `geo::unary_union` 与 `FillRule::Positive` 都要求方向一致，而 i_overlay
/// 的输出方向不保证，因此每次进布尔运算前先定向。
/// `pub(crate)`：叠加分析（[`super::overlay`]）用同一个标架约定。
pub(crate) fn oriented(multi: MultiPolygon<f64>) -> MultiPolygon<f64> {
    MultiPolygon::new(
        multi
            .0
            .into_iter()
            .map(|poly| poly.orient(Direction::Default))
            .collect(),
    )
}

/// 环 → geo 闭合环（剔除非有限坐标与连续重复点；不足 3 点返回 `None`）。
fn geo_ring(ring: &[Coord2D], scale: &MetricScale) -> Option<LineString<f64>> {
    let mut coords: Vec<Coord<f64>> = Vec::with_capacity(ring.len() + 1);
    for p in ring {
        if !p[0].is_finite() || !p[1].is_finite() {
            continue;
        }
        let q = scale.normalize(*p);
        let c = Coord { x: q[0], y: q[1] };
        if coords.last().is_some_and(|l| l.x == c.x && l.y == c.y) {
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

/// 折线 → geo 折线（剔除非有限坐标与连续重复点；不足 2 点返回 `None`）。
fn geo_line(line: &[Coord2D], scale: &MetricScale) -> Option<LineString<f64>> {
    let mut coords: Vec<Coord<f64>> = Vec::with_capacity(line.len());
    for p in line {
        if !p[0].is_finite() || !p[1].is_finite() {
            continue;
        }
        let q = scale.normalize(*p);
        let c = Coord { x: q[0], y: q[1] };
        if coords.last().is_some_and(|l| l.x == c.x && l.y == c.y) {
            continue;
        }
        coords.push(c);
    }
    if coords.len() < 2 {
        return None;
    }
    Some(LineString::new(coords))
}

/// 输入面 → geo 多边形（外环 + 孔，方向规范化）。
fn geo_polygon(poly: &PolygonData, scale: &MetricScale) -> Option<Polygon<f64>> {
    let exterior = geo_ring(&poly.outer, scale)?;
    let interiors: Vec<LineString<f64>> = poly
        .holes
        .iter()
        .filter_map(|hole| geo_ring(hole, scale))
        .collect();
    Some(Polygon::new(exterior, interiors).orient(Direction::Default))
}

/// 点要素的缓冲样式：永远是圆端（ArcGIS 的末端类型只对线有效）。
fn point_style(radius: f64) -> BufferStyle<f64> {
    BufferStyle::new(radius).line_cap(LineCap::Round(JOIN_ANGLE))
}

/// 线要素的缓冲样式：端帽按「末端类型」，连接一律圆角（ArcGIS 默认）。
fn line_style(radius: f64, end: EndType) -> BufferStyle<f64> {
    let cap = match end {
        EndType::Round => LineCap::Round(JOIN_ANGLE),
        EndType::Flat => LineCap::Butt,
        EndType::Square => LineCap::Square,
    };
    BufferStyle::new(radius)
        .line_cap(cap)
        .line_join(LineJoin::Round(JOIN_ANGLE))
}

/// 面要素的缓冲样式：面忽略端帽，只保留圆角连接。
fn polygon_style(radius: f64) -> BufferStyle<f64> {
    BufferStyle::new(radius).line_join(LineJoin::Round(JOIN_ANGLE))
}

/// 合并一堆多边形（并集，消除重叠）。
///
/// `pub(crate)`：叠加分析（裁剪 / 相交 / 联合）的每一段中间结果都靠它归并。
pub(crate) fn union_all(pieces: &[MultiPolygon<f64>]) -> MultiPolygon<f64> {
    // 重叠度决定归并策略（`overlap_ratio` 的文档里有实测依据）。
    let chunk = if overlap_ratio(pieces) > OVERLAP_RATIO {
        PAIRWISE_CHUNK
    } else {
        UNION_CHUNK
    };
    union_levels(pieces, chunk)
}

/// 分层归并：每轮把 `chunk` 个形状交给一次 `unary_union`，直到只剩一个。
///
/// 一次把上万个形状交给 `unary_union` 时，交点数会按平方增长（典型：
/// 5000 个互相重叠的圆），因此分批；`union` 可结合，结果不变。
fn union_levels(pieces: &[MultiPolygon<f64>], chunk: usize) -> MultiPolygon<f64> {
    let chunk = chunk.max(2);
    let mut level: Vec<MultiPolygon<f64>> = pieces
        .iter()
        .filter(|multi| !multi.0.is_empty())
        .map(|multi| oriented(multi.clone()))
        .collect();
    if level.is_empty() {
        return MultiPolygon::new(Vec::new());
    }
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(chunk));
        for group in level.chunks(chunk) {
            if group.len() == 1 {
                next.push(group[0].clone());
            } else {
                next.push(oriented(unary_union(group.iter())));
            }
        }
        if next.len() == level.len() {
            // 理论上不会发生（每组至少合并两个），兜底避免死循环。
            break;
        }
        level = next;
    }
    match level.pop() {
        Some(last) => oriented(last),
        None => MultiPolygon::new(Vec::new()),
    }
}

/// 碎片总面积 ÷ 外接框面积，用来判断「是否大量碎片互相重叠」。
///
/// 实测（100 个半径相同、圆心间距为 0 / 1e-4 / 0.1 / 2.0 的圆，见
/// `probe_dense_cluster`）：圆心几乎重合时，一次 overlay 归并 100 个圆要
/// **117 s**，而两两归并只要 **29 ms**——i_overlay 在大量「几乎重合」的
/// 输入上会退化。而正常的密集数据（5000 个间距 30、半径 25 的点）用一次
/// overlay 归并 128 个是最快的（140 ms，两两归并要 1.47 s）。
/// 两种情形的重叠度差别很大（前者 ≈78，后者 ≈2.2），据此选择策略。
fn overlap_ratio(pieces: &[MultiPolygon<f64>]) -> f64 {
    let mut area_sum = 0.0;
    let mut min = [f64::INFINITY; 2];
    let mut max = [f64::NEG_INFINITY; 2];
    for multi in pieces {
        area_sum += multi.unsigned_area();
        for poly in &multi.0 {
            for c in &poly.exterior().0 {
                min[0] = min[0].min(c.x);
                min[1] = min[1].min(c.y);
                max[0] = max[0].max(c.x);
                max[1] = max[1].max(c.y);
            }
        }
    }
    let box_area = (max[0] - min[0]).max(0.0) * (max[1] - min[1]).max(0.0);
    if !box_area.is_finite() || box_area <= 0.0 {
        return f64::INFINITY;
    }
    area_sum / box_area
}

/// 单侧缓冲（`geo`/i_overlay 的 stroke 没有单侧选项，因此自己拼）。
///
/// 语义 = 「完整缓冲」∩「线这一侧」。而「线的左侧」正是各段左侧半平面的
/// 并集：把每段半平面裁成一个足够大的矩形再并起来，与完整缓冲求交即可。
/// 端帽/圆角都由完整缓冲提供，所以在交点处自动是圆角、在两端自动是
/// ROUND/FLAT/SQUARE 三种端型。
fn one_sided(line: &LineString<f64>, radius: f64, end: EndType, left: bool) -> MultiPolygon<f64> {
    let full = oriented(line.buffer_with_style(line_style(radius, end)));
    if full.0.is_empty() {
        return full;
    }
    let side = half_plane_union(line, radius, left);
    if side.0.is_empty() {
        return MultiPolygon::new(Vec::new());
    }
    full.intersection_with_fill_rule(&side, FillRule::Positive)
}

/// 各段「左/右半平面」的并集（每个半平面裁成一个足够大的矩形）。
fn half_plane_union(line: &LineString<f64>, radius: f64, left: bool) -> MultiPolygon<f64> {
    let coords = &line.0;
    let mut min = [f64::INFINITY; 2];
    let mut max = [f64::NEG_INFINITY; 2];
    for c in coords {
        min[0] = min[0].min(c.x);
        min[1] = min[1].min(c.y);
        max[0] = max[0].max(c.x);
        max[1] = max[1].max(c.y);
    }
    // 半平面是无限区域；矩形的边长取「包围盒对角线 + 缓冲半径」的若干倍，
    // 保证与完整缓冲求交时不会在切向被截断。
    let diag = ((max[0] - min[0]).powi(2) + (max[1] - min[1]).powi(2)).sqrt();
    let reach = ((diag + radius.abs()) * 4.0).max(1e-9);

    let mut rects: Vec<Polygon<f64>> = Vec::with_capacity(coords.len());
    for pair in coords.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let len = (dx * dx + dy * dy).sqrt();
        if !len.is_finite() || len <= 0.0 {
            continue;
        }
        let (ux, uy) = (dx / len, dy / len);
        // 左法线 = 方向逆时针转 90°；右法线取反。
        let (nx, ny) = if left { (-uy, ux) } else { (uy, -ux) };
        let at = |c: Coord<f64>, along: f64, across: f64| Coord {
            x: c.x + ux * along + nx * across,
            y: c.y + uy * along + ny * across,
        };
        let ring = vec![
            at(a, -reach, 0.0),
            at(b, reach, 0.0),
            at(b, reach, reach),
            at(a, -reach, reach),
        ];
        rects.push(Polygon::new(LineString::new(ring), Vec::new()).orient(Direction::Default));
    }
    if rects.is_empty() {
        return MultiPolygon::new(Vec::new());
    }
    // 折线里连续近乎共线的段（长直线被密集打点）会产生大量近乎重合的矩形，
    // 与 `union_all` 同源的问题，因此走同一套归并策略。
    union_all(
        &rects
            .into_iter()
            .map(|rect| MultiPolygon::new(vec![rect]))
            .collect::<Vec<_>>(),
    )
}

/// 一个要素（点/线/面）在归一化空间里的缓冲结果。
fn buffer_feature(
    feature: &Feature,
    radius: f64,
    params: &BufferParams,
    scale: &MetricScale,
) -> MultiPolygon<f64> {
    let mut pieces: Vec<MultiPolygon<f64>> = Vec::new();

    // 负距离只对面有意义（ArcGIS：负值 = 向内缓冲）。
    if radius > 0.0 {
        for p in &feature.points {
            let q = scale.normalize(*p);
            pieces.push(Point::new(q[0], q[1]).buffer_with_style(point_style(radius)));
        }
    }

    for line in &feature.lines {
        if radius <= 0.0 {
            continue;
        }
        let Some(ls) = geo_line(line, scale) else {
            continue;
        };
        pieces.push(match params.side {
            SideType::Left => one_sided(&ls, radius, params.end, true),
            SideType::Right => one_sided(&ls, radius, params.end, false),
            _ => oriented(ls.buffer_with_style(line_style(radius, params.end))),
        });
    }

    for poly in &feature.polys {
        if radius == 0.0 {
            continue;
        }
        if let Some(p) = geo_polygon(poly, scale) {
            pieces.push(oriented(p.buffer_with_style(polygon_style(radius))));
        }
    }

    union_all(&pieces)
}

/// 结果多边形 → 输出环（外环 + 孔，全部还原到图层坐标）。
fn polygons_to_data(multi: &MultiPolygon<f64>, scale: &MetricScale) -> Vec<PolygonData> {
    let mut out = Vec::with_capacity(multi.0.len());
    for poly in &multi.0 {
        let Some(outer) = ring_to_layer(poly.exterior(), scale) else {
            continue;
        };
        let holes: Vec<Vec<Coord2D>> = poly
            .interiors()
            .iter()
            .filter_map(|hole| ring_to_layer(hole, scale))
            .collect();
        out.push(PolygonData { outer, holes });
    }
    out
}

/// geo 环 → 图层坐标环（去掉重复的收尾点；不足 3 点算退化）。
fn ring_to_layer(ring: &LineString<f64>, scale: &MetricScale) -> Option<Vec<Coord2D>> {
    let mut pts: Vec<Coord2D> = ring
        .0
        .iter()
        .map(|c| scale.denormalize([c.x, c.y]))
        .filter(|p| p[0].is_finite() && p[1].is_finite())
        .collect();
    while pts.len() > 1 {
        let (first, last) = (pts[0], pts[pts.len() - 1]);
        let tol = 1e-9 * first[0].abs().max(first[1].abs()).max(1.0);
        if (first[0] - last[0]).abs() <= tol && (first[1] - last[1]).abs() <= tol {
            pts.pop();
        } else {
            break;
        }
    }
    if pts.len() >= 3 {
        Some(pts)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 基础几何
// ---------------------------------------------------------------------------

/// 环的有向面积（逆时针为正）。
#[cfg(test)]
fn signed_area(ring: &[Coord2D]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        sum += a[0] * b[1] - b[0] * a[1];
    }
    sum * 0.5
}

fn cosd(deg: f64) -> f64 {
    (deg * std::f64::consts::PI / 180.0).cos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::{CrsId, ProjectionInfo};

    fn projected() -> ProjectionInfo {
        ProjectionInfo {
            model: ProjectionModel::Projected,
            crs: CrsId::None,
        }
    }

    fn geographic() -> ProjectionInfo {
        ProjectionInfo {
            model: ProjectionModel::Geographic,
            crs: CrsId::None,
        }
    }

    fn projected_layer() -> Layer {
        Layer {
            name: "test".to_string(),
            projection: projected(),
            ..Layer::default()
        }
    }

    fn point_layer(points: Vec<Coord2D>) -> Layer {
        let mut layer = Layer::from_geometry("pts".to_string(), points, Vec::new(), Vec::new());
        layer.projection = projected();
        layer
    }

    fn outer_ring(ring: &[Coord2D]) -> f64 {
        signed_area(ring).abs()
    }

    fn total_outer_area(layer: &Layer) -> f64 {
        layer.outer_rings.iter().map(|r| outer_ring(r)).sum()
    }

    fn total_hole_area(layer: &Layer) -> f64 {
        layer.hole_rings.iter().map(|r| outer_ring(r)).sum()
    }

    fn params(distance: f64) -> BufferParams {
        BufferParams {
            distance: DistanceSource::Value(distance),
            ..BufferParams::default()
        }
    }

    #[test]
    fn point_buffer_area_is_close_to_circle() {
        let layer = point_layer(vec![[0.0, 0.0]]);
        let out = run(&layer, &params(500.0)).expect("缓冲区应成功");
        let area = total_outer_area(&out);
        let ideal = std::f64::consts::PI * 500.0 * 500.0;
        assert!(area <= ideal, "多边形近似面积不应超过理想圆：{area} vs {ideal}");
        assert!(area > ideal * 0.98, "面积偏差过大：{area} vs {ideal}");
        assert_eq!(out.hole_rings.len(), 0, "点缓冲不应有孔");
    }

    #[test]
    fn line_buffer_is_stadium() {
        let layer = Layer::from_geometry(
            "line".to_string(),
            Vec::new(),
            vec![vec![[0.0, 0.0], [1000.0, 0.0]]],
            Vec::new(),
        );
        let out = run(&layer, &params(100.0)).expect("缓冲区应成功");
        let area = total_outer_area(&out);
        // 矩形 1000x200 + 两端半圆（半径 100）
        let ideal = 1000.0 * 200.0 + std::f64::consts::PI * 100.0 * 100.0;
        assert!((area - ideal).abs() / ideal < 0.01, "面积偏差过大：{area} vs {ideal}");
    }

    #[test]
    fn square_polygon_buffer_grows_with_round_corners() {
        let square = vec![
            [0.0, 0.0],
            [100.0, 0.0],
            [100.0, 100.0],
            [0.0, 100.0],
        ];
        let layer = Layer::from_geometry(
            "poly".to_string(),
            Vec::new(),
            Vec::new(),
            vec![PolygonData {
                outer: square,
                holes: Vec::new(),
            }],
        );
        let out = run(&layer, &params(10.0)).expect("缓冲区应成功");
        let area = total_outer_area(&out);
        let ideal = 100.0 * 100.0 + 4.0 * 100.0 * 10.0 + std::f64::consts::PI * 10.0 * 10.0;
        assert!((area - ideal).abs() / ideal < 0.01, "面积偏差过大：{area} vs {ideal}");
        assert_eq!(out.hole_rings.len(), 0);
    }

    #[test]
    fn outside_only_keeps_input_face_as_hole() {
        let square = vec![
            [0.0, 0.0],
            [100.0, 0.0],
            [100.0, 100.0],
            [0.0, 100.0],
        ];
        let layer = Layer::from_geometry(
            "poly".to_string(),
            Vec::new(),
            Vec::new(),
            vec![PolygonData {
                outer: square,
                holes: Vec::new(),
            }],
        );
        let mut p = params(10.0);
        p.side = SideType::OutsideOnly;
        let out = run(&layer, &p).expect("缓冲区应成功");
        assert_eq!(out.outer_rings.len(), 1);
        assert_eq!(out.hole_rings.len(), 1, "输入面应被挖成孔");
        let hole = total_hole_area(&out);
        assert!((hole - 10000.0).abs() / 10000.0 < 1e-6, "孔面积应等于输入面：{hole}");
    }

    #[test]
    fn negative_distance_erodes_polygon() {
        let square = vec![
            [0.0, 0.0],
            [100.0, 0.0],
            [100.0, 100.0],
            [0.0, 100.0],
        ];
        let layer = Layer::from_geometry(
            "poly".to_string(),
            Vec::new(),
            Vec::new(),
            vec![PolygonData {
                outer: square,
                holes: Vec::new(),
            }],
        );
        let out = run(&layer, &params(-10.0)).expect("缓冲区应成功");
        let area = total_outer_area(&out);
        assert!((area - 6400.0).abs() / 6400.0 < 0.05, "向内缓冲面积应接近 80x80：{area}");
    }

    #[test]
    fn polygon_hole_is_shrunk_by_buffer() {
        let layer = Layer::from_geometry(
            "donut".to_string(),
            Vec::new(),
            Vec::new(),
            vec![PolygonData {
                outer: vec![[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]],
                holes: vec![vec![
                    [40.0, 40.0],
                    [40.0, 60.0],
                    [60.0, 60.0],
                    [60.0, 40.0],
                ]],
            }],
        );
        let out = run(&layer, &params(5.0)).expect("缓冲区应成功");
        assert_eq!(out.hole_rings.len(), 1, "孔应保留");
        let hole = total_hole_area(&out);
        // 孔是 40..60 的 20x20；正距离缓冲让外环外扩、孔向内收缩 5 ⇒ 10x10。
        assert!((hole - 100.0).abs() / 100.0 < 0.01, "孔应从 20x20 收缩到 10x10：{hole}");
    }

    #[test]
    fn dissolve_all_merges_overlapping_points() {
        let layer = point_layer(vec![[0.0, 0.0], [300.0, 0.0]]);
        let mut p = params(500.0);
        p.dissolve = DissolveType::All;
        let out = run(&layer, &p).expect("缓冲区应成功");
        assert_eq!(out.outer_rings.len(), 1, "重叠缓冲应融合为单个要素");
        assert_eq!(out.hole_rings.len(), 0);
    }

    #[test]
    fn dissolve_none_keeps_separate_features() {
        let layer = point_layer(vec![[0.0, 0.0], [3000.0, 0.0]]);
        let out = run(&layer, &params(500.0)).expect("缓冲区应成功");
        assert_eq!(out.outer_rings.len(), 2);
    }

    #[test]
    fn dissolve_list_groups_by_field_values() {
        let mut layer = Layer::from_geometry(
            "pts".to_string(),
            vec![[0.0, 0.0], [100.0, 0.0], [5000.0, 0.0]],
            Vec::new(),
            Vec::new(),
        );
        layer.projection = projected();
        layer.fields = vec!["grp".to_string()];
        layer.attrs = vec![
            vec!["A".to_string()],
            vec!["A".to_string()],
            vec!["B".to_string()],
        ];
        layer.point_rows = vec![0, 1, 2];
        let mut p = params(500.0);
        p.dissolve = DissolveType::List;
        p.dissolve_fields = vec!["grp".to_string()];
        let out = run(&layer, &p).expect("缓冲区应成功");
        assert_eq!(out.outer_rings.len(), 2, "A 组融合成一个、B 组一个");
    }

    #[test]
    fn distance_from_field_changes_size() {
        let mut layer = Layer::from_geometry(
            "pts".to_string(),
            vec![[0.0, 0.0], [100000.0, 0.0]],
            Vec::new(),
            Vec::new(),
        );
        layer.projection = projected();
        layer.fields = vec!["dist".to_string()];
        layer.attrs = vec![vec!["100".to_string()], vec!["200".to_string()]];
        layer.point_rows = vec![0, 1];
        let mut p = params(0.0);
        p.distance = DistanceSource::Field("dist".to_string());
        let out = run(&layer, &p).expect("缓冲区应成功");
        let mut areas: Vec<f64> = out.outer_rings.iter().map(|r| outer_ring(r)).collect();
        areas.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let ratio = areas[1] / areas[0];
        assert!((ratio - 4.0).abs() < 0.1, "半径两倍 ⇒ 面积四倍：{ratio}");
    }

    #[test]
    fn geographic_layer_uses_anisotropic_scale() {
        // 纬度 45° 上，1km 缓冲的经度跨度应约为纬度跨度的 1/cos45 ≈ 1.414 倍。
        let mut layer = point_layer(vec![[0.0, 45.0]]);
        layer.projection = geographic();
        let out = run(&layer, &params(1000.0)).expect("缓冲区应成功");
        let ring = &out.outer_rings[0];
        let mut min = [f64::INFINITY; 2];
        let mut max = [f64::NEG_INFINITY; 2];
        for p in ring {
            for k in 0..2 {
                min[k] = min[k].min(p[k]);
                max[k] = max[k].max(p[k]);
            }
        }
        let ratio = (max[0] - min[0]) / (max[1] - min[1]);
        assert!((ratio - 2.0_f64.sqrt()).abs() < 0.05, "经纬跨度比应接近 1/cos45：{ratio}");
    }

    #[test]
    fn empty_layer_reports_error() {
        let layer = projected_layer();
        assert!(run(&layer, &params(100.0)).is_err());
    }

    #[test]
    fn zero_distance_reports_error() {
        let layer = point_layer(vec![[0.0, 0.0]]);
        assert!(run(&layer, &params(0.0)).is_err());
    }

    #[test]
    fn left_side_buffer_covers_half_the_area() {
        let layer = Layer::from_geometry(
            "line".to_string(),
            Vec::new(),
            vec![vec![[0.0, 0.0], [1000.0, 0.0]]],
            Vec::new(),
        );
        let mut p = params(100.0);
        p.side = SideType::Left;
        let out = run(&layer, &p).expect("缓冲区应成功");
        let area = total_outer_area(&out);
        // 半侧条带 1000x100 + 端部半圆（外端为 1/4 圆 + 起点 1/4 圆）
        let ideal = 1000.0 * 100.0 + 0.5 * std::f64::consts::PI * 100.0 * 100.0;
        assert!((area - ideal).abs() / ideal < 0.05, "面积偏差过大：{area} vs {ideal}");
    }

    #[test]
    fn flattens_to_polygons_with_style() {
        let layer = point_layer(vec![[0.0, 0.0]]);
        let out = run(&layer, &params(100.0)).expect("缓冲区应成功");
        assert!(out.style.outline_width > 0.0);
        assert!(!out.triangles.is_empty(), "结果应已三角化以便填充");
    }

    /// 端到端：真实 GeoJSON 文件 → 导入（属性采集 + 行号映射）→ 按字段融合缓冲
    /// → 写回 GeoJSON → 重新导入。覆盖单元测试手工构造图层时绕开的
    /// `Layer::from_parts` 属性行与几何行的对齐。
    #[test]
    fn end_to_end_import_buffer_export_reload() {
        let path = {
            let mut p = std::env::temp_dir();
            p.push(format!("rustgis_buffer_e2e_{}.geojson", std::process::id()));
            p
        };
        let fc = serde_json::json!({
            "type": "FeatureCollection",
            "features": [
                { "type": "Feature", "properties": { "grp": "A", "dist": 100.0 },
                  "geometry": { "type": "Point", "coordinates": [0.0, 0.0] } },
                { "type": "Feature", "properties": { "grp": "A", "dist": 100.0 },
                  "geometry": { "type": "Point", "coordinates": [0.0005, 0.0] } },
                { "type": "Feature", "properties": { "grp": "B", "dist": 200.0 },
                  "geometry": { "type": "Point", "coordinates": [0.1, 0.0] } }
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&fc).unwrap()).expect("写临时 GeoJSON");

        let layer = crate::geo::load_file(&path).expect("导入应成功");
        let _ = std::fs::remove_file(&path);
        assert_eq!(layer.feature_count, 3);
        assert_eq!(layer.attrs.len(), 3, "每个要素一行属性");

        // 按字段距离缓冲 + 按 grp 融合：A 组两点（相距 0.0005°≈56 米，缓冲
        // 半径 100 米）彼此重叠 ⇒ 1 环；B 组单点（距 A 组 0.1°）⇒ 1 环。
        let mut p = BufferParams {
            distance: DistanceSource::Field("dist".to_string()),
            dissolve: DissolveType::List,
            dissolve_fields: vec!["grp".to_string()],
            ..BufferParams::default()
        };
        p.unit = LinearUnit::Meters;
        let out = run(&layer, &p).expect("缓冲区应成功");
        assert_eq!(out.outer_rings.len(), 2, "应融合成 A、B 两个要素");
        assert_eq!(out.feature_count, 2);
        assert!(!out.triangles.is_empty(), "结果应已三角化");

        // 写盘 + 重新导入：结果必须是可解析的 GeoJSON 面。
        let out_path = {
            let mut q = std::env::temp_dir();
            q.push(format!("rustgis_buffer_e2e_out_{}.geojson", std::process::id()));
            q
        };
        write_geojson(&out, &out_path).expect("写结果 GeoJSON");
        let reloaded = crate::geo::load_file(&out_path).expect("结果应可重新导入");
        let _ = std::fs::remove_file(&out_path);
        assert_eq!(reloaded.outer_rings.len(), 2);
        assert!(!reloaded.triangles.is_empty());
    }

    /// 侵蚀（负距离）在归一化后必须保留符号：参考距离是负数时不能把它
    /// 归一化成 +1（否则内缩会变成外扩）。
    #[test]
    fn negative_reference_keeps_erosion_sign() {
        let mut layer = Layer::from_geometry(
            "poly".to_string(),
            Vec::new(),
            Vec::new(),
            vec![PolygonData {
                outer: vec![[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]],
                holes: Vec::new(),
            }],
        );
        layer.projection = projected();
        layer.fields = vec!["dist".to_string()];
        layer.attrs = vec![vec!["-10".to_string()]];
        layer.ring_rows = vec![0];
        let p = BufferParams {
            distance: DistanceSource::Field("dist".to_string()),
            ..BufferParams::default()
        };
        let out = run(&layer, &p).expect("缓冲区应成功");
        let area = total_outer_area(&out);
        assert!((area - 6400.0).abs() / 6400.0 < 0.05, "字段负距离应向内侵蚀：{area}");
    }

    /// 末端类型只影响线：ROUND = 两端半圆、FLAT = 平切、SQUARE = 沿走向外延半径。
    #[test]
    fn line_end_types_match_expected_areas() {
        let layer = Layer::from_geometry(
            "line".to_string(),
            Vec::new(),
            vec![vec![[0.0, 0.0], [1000.0, 0.0]]],
            Vec::new(),
        );
        let area_for = |end: EndType| {
            let mut p = params(100.0);
            p.end = end;
            total_outer_area(&run(&layer, &p).expect("缓冲区应成功"))
        };
        let (r, flat, square) = (area_for(EndType::Round), area_for(EndType::Flat), area_for(EndType::Square));
        // 条带 1000×200 = 20 万；圆端 + 一个整圆、平端 + 0、方端 + 4·r²。
        assert!((flat - 200_000.0).abs() / 200_000.0 < 0.01, "平端面积：{flat}");
        assert!(
            (r - (200_000.0 + std::f64::consts::PI * 100.0 * 100.0)).abs() / r < 0.02,
            "圆端面积：{r}"
        );
        assert!(
            (square - (200_000.0 + 4.0 * 100.0 * 100.0)).abs() / square < 0.01,
            "方端面积：{square}"
        );
    }

    /// RIGHT 与 LEFT 关于线轴对称：面积相同、包范围互为镜像。
    #[test]
    fn right_side_mirrors_left_side() {
        let layer = Layer::from_geometry(
            "line".to_string(),
            Vec::new(),
            vec![vec![[0.0, 0.0], [1000.0, 0.0]]],
            Vec::new(),
        );
        let side_box = |side: SideType| {
            let mut p = params(100.0);
            p.side = side;
            let out = run(&layer, &p).expect("缓冲区应成功");
            let area = total_outer_area(&out);
            let bounds = out.bounds;
            (area, bounds)
        };
        let (left_area, left_bounds) = side_box(SideType::Left);
        let (right_area, right_bounds) = side_box(SideType::Right);
        assert!((left_area - right_area).abs() / left_area < 1e-6, "两侧面积应相同");
        // 线沿 +x：左侧在 +y（0..100），右侧在 -y（-100..0）。
        assert!((left_bounds.min[1] - 0.0).abs() < 1e-6);
        assert!((right_bounds.max[1] - 0.0).abs() < 1e-6);
        assert!((left_bounds.max[1] - 100.0).abs() < 1e-6);
        assert!((right_bounds.min[1] + 100.0).abs() < 1e-6);
    }

    /// 性能探针（`cargo test --bin rustgis probe_buffer_cost -- --ignored --nocapture`）：
    /// 记录 geo 后端在典型规模下的耗时，供产品文档引用。
    #[test]
    #[ignore = "perf probe"]
    fn probe_buffer_cost() {
        use std::time::Instant;

        // 1) 5000 个点（100×50 网格、间距 30）的散点缓冲 + ALL 融合：
        //    每对相邻缓冲圆只与 4 个邻居相交，是融合作业的典型形态。
        let points: Vec<Coord2D> = (0..5000)
            .map(|i| [(i % 100) as f64 * 30.0, (i / 100) as f64 * 30.0])
            .collect();
        let layer = point_layer(points);
        let mut p = params(25.0);
        p.dissolve = DissolveType::All;
        let started = Instant::now();
        let out = run(&layer, &p).expect("缓冲区应成功");
        println!(
            "5000 点网格 + ALL 融合  {:>10?}  环 {} / 三角 {}",
            started.elapsed(),
            out.outer_rings.len(),
            out.triangles.len()
        );

        // 2) 5000 顶点折线（含起伏）的单要素缓冲。
        let line: Vec<Coord2D> = (0..5000)
            .map(|i| [i as f64 * 10.0, (i as f64 * 0.05).sin() * 500.0])
            .collect();
        let mut layer = Layer::from_geometry("line".to_string(), Vec::new(), vec![line], Vec::new());
        layer.projection = projected();
        let started = Instant::now();
        let out = run(&layer, &params(30.0)).expect("缓冲区应成功");
        println!(
            "5000 顶点折线缓冲       {:>10?}  环 {} / 三角 {}",
            started.elapsed(),
            out.outer_rings.len(),
            out.triangles.len()
        );

        // 3) 500 个重叠方形面的缓冲 + ALL 融合。
        let squares: Vec<PolygonData> = (0..500)
            .map(|i| {
                let x = (i % 25) as f64 * 60.0;
                let y = (i / 25) as f64 * 60.0;
                PolygonData {
                    outer: vec![
                        [x, y],
                        [x + 100.0, y],
                        [x + 100.0, y + 100.0],
                        [x, y + 100.0],
                    ],
                    holes: Vec::new(),
                }
            })
            .collect();
        let mut layer = Layer::from_geometry("polys".to_string(), Vec::new(), Vec::new(), squares);
        layer.projection = projected();
        let mut p = params(20.0);
        p.dissolve = DissolveType::All;
        let started = Instant::now();
        let out = run(&layer, &p).expect("缓冲区应成功");
        println!(
            "500 重叠面 + ALL 融合   {:>10?}  环 {} / 三角 {}",
            started.elapsed(),
            out.outer_rings.len(),
            out.triangles.len()
        );

        // 4) 同一条折线的单侧（LEFT）缓冲：半平面并集的规模与折线顶点数同阶。
        let line: Vec<Coord2D> = (0..5000)
            .map(|i| [i as f64 * 10.0, (i as f64 * 0.05).sin() * 500.0])
            .collect();
        let mut layer = Layer::from_geometry("line".to_string(), Vec::new(), vec![line], Vec::new());
        layer.projection = projected();
        let mut p = params(30.0);
        p.side = SideType::Left;
        let started = Instant::now();
        let out = run(&layer, &p).expect("缓冲区应成功");
        println!(
            "5000 顶点折线单侧缓冲   {:>10?}  环 {} / 三角 {}",
            started.elapsed(),
            out.outer_rings.len(),
            out.triangles.len()
        );
    }

    /// 分批融合（> `UNION_CHUNK` 个碎片）的结果应与一次融合一致：
    /// 200 个间距 10、边长 20 的方形做 ALL 融合，并集面积应等于外接矩形面积。
    #[test]
    fn many_overlapping_features_dissolve_all_matches_expected_area() {
        let squares: Vec<PolygonData> = (0..200)
            .map(|i| {
                let x = (i % 20) as f64 * 10.0;
                let y = (i / 20) as f64 * 10.0;
                PolygonData {
                    outer: vec![
                        [x, y],
                        [x + 20.0, y],
                        [x + 20.0, y + 20.0],
                        [x, y + 20.0],
                    ],
                    holes: Vec::new(),
                }
            })
            .collect();
        let mut layer = Layer::from_geometry("grid".to_string(), Vec::new(), Vec::new(), squares);
        layer.projection = projected();
        let mut p = params(1.0);
        p.dissolve = DissolveType::All;
        let out = run(&layer, &p).expect("缓冲区应成功");
        assert_eq!(out.outer_rings.len(), 1, "融合后应只剩一个外环");
        // 20 列间距 10、边长 20 → 外接 210×110，再加 1 的圆角缓冲外扩。
        let expected = 210.0 * 110.0;
        let area = total_outer_area(&out);
        assert!(
            area > expected && (area - expected) / expected < 0.05,
            "融合面积异常：{area}（预期略大于 {expected}）"
        );
    }

    /// 诊断探针：密集点簇在 ALL 融合下的耗时，比较「完全重合」「近乎重合」
    /// 「半径量级分散」「远大于半径」四种形态，判断病态耗时来自哪一类输入。
    #[test]
    #[ignore = "perf probe"]
    fn probe_dense_cluster() {
        use std::time::Instant;
        const N: usize = 100;
        let cases: [(&str, f64); 4] = [
            ("完全重合", 0.0),
            ("近乎重合", 1e-4),
            ("半径量级", 0.1),
            ("远超半径", 2.0),
        ];
        for (name, spread) in cases {
            let points: Vec<Coord2D> = (0..N)
                .map(|i| {
                    let angle = i as f64 * 0.013;
                    [angle.cos() * spread, angle.sin() * spread]
                })
                .collect();
            let layer = point_layer(points);
            let mut p = params(60.0);
            p.dissolve = DissolveType::All;
            let started = Instant::now();
            let out = run(&layer, &p).expect("缓冲区应成功");
            println!(
                "n={N} {name}（散布 {spread:?}）ALL {:>12?} 环 {}",
                started.elapsed(),
                out.outer_rings.len()
            );
        }
    }

    /// 单侧 + 平端：应正好是半个条带（1000×100）。
    #[test]
    fn one_sided_flat_end_is_half_band() {
        let layer = Layer::from_geometry(
            "line".to_string(),
            Vec::new(),
            vec![vec![[0.0, 0.0], [1000.0, 0.0]]],
            Vec::new(),
        );
        let mut p = params(100.0);
        p.side = SideType::Left;
        p.end = EndType::Flat;
        let out = run(&layer, &p).expect("缓冲区应成功");
        let area = total_outer_area(&out);
        assert!((area - 100_000.0).abs() / 100_000.0 < 0.01, "单侧平端面积：{area}");
    }

    /// 多段折线（含拐角）在默认圆角连接下的面积应大于「各段矩形之和」，
    /// 小于「外接 Minkowski 和」——这里只校验拐角被圆角补齐（不低于条带面积）。
    #[test]
    fn polyline_corners_are_rounded() {
        let layer = Layer::from_geometry(
            "L".to_string(),
            Vec::new(),
            vec![vec![[0.0, 0.0], [1000.0, 0.0], [1000.0, 1000.0]]],
            Vec::new(),
        );
        let out = run(&layer, &params(50.0)).expect("缓冲区应成功");
        let area = total_outer_area(&out);
        // 两段条带 2·1000·100 = 20 万，减去拐角重叠，再加外侧 3/4 圆 + 两端半圆。
        let band = 200_000.0;
        let corners = 0.75 * std::f64::consts::PI * 50.0 * 50.0;
        let caps = std::f64::consts::PI * 50.0 * 50.0;
        let ideal = band + corners + caps;
        assert!(
            area > band * 0.95 && area < ideal * 1.05,
            "折线缓冲面积异常：{area}（条带 {band}，理想 {ideal}）"
        );
        assert_eq!(out.outer_rings.len(), 1, "一条折线应生成单个外环");
    }
}