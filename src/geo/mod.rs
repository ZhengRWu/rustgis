//! 地理数据模型与文件导入（GeoJSON / Shapefile）。
//!
//! 设计约定（v0.2.2）：
//! - 统一 2D 坐标 `[f64; 2]`（x=经度/东，y=纬度/北）；
//! - 源文件中的 Multi* 几何在导入时展开为多个单几何要素；
//! - 导入时按要素采集属性表（`fields` / `attrs` / `*_rows`），供按字段缓冲与融合；
//! - 多边形（外环 + 孔）在导入时用 `geo` 的 Earcut 剖分（见 [`tri`]，缓存三角、
//!   渲染零开销；孔参与填充）。

pub mod tri;

pub mod buffer;

pub mod overlay;

use std::path::Path;

use egui::Color32;
use geo_types::Geometry;

use crate::geo::tri::triangulate_polygons;
use crate::style;

/// 2D 坐标（x=经度/东，y=纬度/北）。
pub type Coord2D = [f64; 2];

// ---------------------------------------------------------------------------
// 投影 / 坐标系
//
// 导入阶段对源数据 CRS 的识别（v0.2.1）：
// - GeoJSON：RFC 7946 默认 WGS 84（EPSG:4326），但**不读**非标准的
//   `crs` 成员——遇到时按默认处理；
// - Shapefile：读取同目录的 `.prj` 文件（WKT 坐标参考系定义）。
//
// v0.2.1 只记录与展示投影 / 坐标系，**不做坐标重投影**；真正的坐标变换
// 属于 `rustgis-spatial`（见产品文档 §6.2 路线图）。
// ---------------------------------------------------------------------------

/// 投影模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionModel {
    /// 地理坐标系（经纬度，未投影）。
    Geographic,
    /// 投影坐标系（投影变换后的平面坐标）。
    Projected,
    /// 无法识别（如缺失 `.prj` 的 Shapefile）。
    Unknown,
}

impl ProjectionModel {
    /// 磁盘上的序列化值（工程文件 / 将来图层属性对话框共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Geographic => "geographic",
            Self::Projected => "projected",
            Self::Unknown => "unknown",
        }
    }
}

/// 数据坐标系标识（EPSG 代码为主，WKT 名称兜底）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrsId {
    /// 有 EPSG 代码（如 `4326`）。
    Epsg(u32),
    /// 只能从 WKT 首行拿到名称（如 `GCS_WGS_1984`）。
    Name(String),
    /// 无法识别。
    None,
}

impl CrsId {
    /// 状态栏展示文本（如 `EPSG:4326` / `GCS_WGS_1984` / `未识别`）。
    pub fn label(&self) -> String {
        match self {
            Self::Epsg(code) => format!("EPSG:{code}"),
            Self::Name(name) => name.clone(),
            Self::None => "未识别".to_string(),
        }
    }

    /// 磁盘 / 网络序列化字符串（与 [`label`] 的区别：`None` 序列化为
    /// **空串**而非本地化文案——序列化值必须与界面语言无关）。
    pub fn storage(&self) -> String {
        match self {
            Self::None => String::new(),
            other => other.label(),
        }
    }
}

/// 一份数据的投影 / 坐标系描述（工程默认值与图层信息共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionInfo {
    /// 投影模型。
    pub model: ProjectionModel,
    /// 坐标系标识（EPSG 代码或 WKT 名称）。
    pub crs: CrsId,
}

impl Default for ProjectionInfo {
    /// 未设置（= [`ProjectionInfo::none`]：模型未知 + 坐标系未识别）。
    fn default() -> Self {
        Self::none()
    }
}

impl ProjectionInfo {
    /// 未设置（空工程 / 无法识别的源数据）。
    pub fn none() -> Self {
        Self {
            model: ProjectionModel::Unknown,
            crs: CrsId::None,
        }
    }
}

/// 从 Shapefile 的 `.prj` 文件（WKT 文本）解析投影 / 坐标系。
///
/// 识别规则（面向常见坐标系，不追求完整 WKT 解析）：
/// - `PROJCS` / `PROJCTP` → 投影坐标系；`GEOGCS` / `GEOGTRP` → 地理坐标系；
/// - `AUTHORITY["EPSG","<n>"]` → EPSG 代码（取**数据坐标系**自身的声明，
///   括号嵌套层级最浅的那个——`PROJCS` 内部的 `GEOGCS` 是基准地理系，代码不同）；
/// - WKT 首行元素名（如 `PROJCS["名称",` 中第一个双引号串）为坐标系名称，
///   作 EPSG 缺失时的兜底。
pub fn parse_prj_wkt(wkt: &str) -> ProjectionInfo {
    // 首行形如 `PROJCS["CGCS 2000 / ...",` / `GEOGCS["WGS 84",`：
    // 取第一个双引号对内的元素名。
    let name = wkt
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .and_then(|first| {
            let rest = first.find('"')?;
            let rest = &first[rest + 1..];
            let end = rest.find('"')?;
            let name = &rest[..end];
            (!name.is_empty()).then(|| name.to_string())
        })
        .unwrap_or_default();

    let model = if wkt.contains("PROJCS") || wkt.contains("PROJCTP") {
        ProjectionModel::Projected
    } else if is_geographic_wkt(wkt) {
        ProjectionModel::Geographic
    } else {
        ProjectionModel::Unknown
    };

    let crs = parse_epsg_authority(wkt).map(CrsId::Epsg).unwrap_or_else(|| {
        if model == ProjectionModel::Unknown || name.is_empty() {
            CrsId::None
        } else {
            CrsId::Name(name)
        }
    });

    ProjectionInfo { model, crs }
}

fn is_geographic_wkt(wkt: &str) -> bool {
    wkt.contains("GEOGCS") || wkt.contains("GEOGTRP") || wkt.contains("GEOGRAPHIC")
}

/// 提取 WKT 中数据坐标系**自身**的 `AUTHORITY["EPSG","<n>"]` 声明。
///
/// `PROJCS` 内嵌的 `GEOGCS` 会带基准地理系的 EPSG（如 CGCS2000 投影系里
/// 嵌套 EPSG:4490），与投影系自身的代码（如 4549）不同。判据是**括号
/// 嵌套深度**：声明所在的括号层级越浅，越属于数据坐标系自身（`PROJCS`
/// 自身层级 1，内嵌 `GEOGCS` 层级 2）；同层级取最后出现的一个（WKT 惯例
/// 中元素自身声明在其组件之后）。
fn parse_epsg_authority(wkt: &str) -> Option<u32> {
    const PAT: &str = "AUTHORITY[\"EPSG\",\"";
    let mut best: Option<(i32, usize, u32)> = None; // (深度, 位置, 代码)
    let mut depth: i32 = 0;
    let mut in_name = false; // `'...'` 包裹的名称内不计括号
    for (i, c) in wkt.char_indices() {
        match c {
            '\'' => in_name = !in_name,
            '[' if !in_name => depth += 1,
            ']' if !in_name => depth -= 1,
            'A' if !in_name && wkt[i..].starts_with(PAT) => {
                let rest = &wkt[i + PAT.len()..];
                let digits: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(code) = digits.parse::<u32>() {
                    // 深度浅者优先；同深度取后出现的。
                    if best.as_ref().is_none_or(|(bd, bp, _)| {
                        depth < *bd || (depth == *bd && i >= *bp)
                    }) {
                        best = Some((depth, i, code));
                    }
                }
            }
            _ => {}
        }
    }
    best.map(|(_, _, code)| code)
}

/// 统一 2D 几何（由源文件几何展开而来）。
#[derive(Debug, Clone, PartialEq)]
pub enum Geometry2D {
    Point(Coord2D),
    Line(Vec<Coord2D>),
    Polygon {
        outer: Vec<Coord2D>,
        holes: Vec<Vec<Coord2D>>,
    },
}

/// 单个展开几何（内部中间结构）。
struct Part {
    geometry: Geometry2D,
    /// 该几何对应的属性键值对（GeoJSON `properties` / Shapefile 记录）。
    /// 缺省为空——没有属性数据时属性表整体为空，不影响渲染。
    attrs: Vec<(String, String)>,
}

/// 带属性的输出要素：叠加分析 / 矢量转换的通用输出单元。
///
/// 与私有的 [`Part`] 同构（几何 + 属性键值对），区别是它对外可见——
/// 地理处理工具（裁剪 / 相交 / 联合 / 合并 / 融合）要在几何运算之外
/// 重新拼接每个输出要素的属性，所以需要一个公开的载体。
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureData {
    pub geometry: Geometry2D,
    /// 输出要素的属性键值对；列序由 [`Layer::from_features`] 按首次出现顺序决定。
    pub attrs: Vec<(String, String)>,
}

/// 把键值对形式的属性按 `fields` 的列序铺平成一行（缺失字段填空串）。
fn fill_row(fields: &[String], attrs: &[(String, String)]) -> Vec<String> {
    let mut row = vec![String::new(); fields.len()];
    for (k, v) in attrs {
        if let Some(i) = fields.iter().position(|f| f == k) {
            row[i] = v.clone();
        }
    }
    row
}

/// 轴对齐包围盒。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: Coord2D,
    pub max: Coord2D,
}

impl Aabb {
    pub fn empty() -> Self {
        Self {
            min: [f64::INFINITY; 2],
            max: [f64::NEG_INFINITY; 2],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.min[0] > self.max[0] || self.min[1] > self.max[1]
    }

    pub fn extend_point(&mut self, p: Coord2D) {
        for ((mn, mx), q) in self.min.iter_mut().zip(self.max.iter_mut()).zip(p) {
            *mn = (*mn).min(q);
            *mx = (*mx).max(q);
        }
    }

    pub fn merge(&mut self, other: Aabb) {
        if other.is_empty() {
            return;
        }
        self.extend_point(other.min);
        self.extend_point(other.max);
    }

    pub fn width(&self) -> f64 {
        self.max[0] - self.min[0]
    }

    pub fn height(&self) -> f64 {
        self.max[1] - self.min[1]
    }

    pub fn center(&self) -> Coord2D {
        [
            (self.min[0] + self.max[0]) / 2.0,
            (self.min[1] + self.max[1]) / 2.0,
        ]
    }

    /// 两个包围盒是否相交（含仅边界相触）。空盒与任何盒都不相交。
    pub fn intersects(&self, other: &Aabb) -> bool {
        if self.is_empty() || other.is_empty() {
            return false;
        }
        self.min[0] <= other.max[0]
            && other.min[0] <= self.max[0]
            && self.min[1] <= other.max[1]
            && other.min[1] <= self.max[1]
    }

    /// 点是否落在盒内（含边界）。
    pub fn contains_point(&self, p: Coord2D) -> bool {
        self.min[0] <= p[0] && p[0] <= self.max[0] && self.min[1] <= p[1] && p[1] <= self.max[1]
    }

    /// `self` 是否完整包含 `other`（空盒不算被包含）。
    pub fn contains_aabb(&self, other: &Aabb) -> bool {
        if self.is_empty() || other.is_empty() {
            return false;
        }
        self.min[0] <= other.min[0]
            && other.max[0] <= self.max[0]
            && self.min[1] <= other.min[1]
            && other.max[1] <= self.max[1]
    }

    /// 四周各外扩 `margin`（世界单位），用于把线宽 / 点半径也算进裁剪范围。
    #[allow(dead_code)] // 视口裁剪目前统一按像素留白（`Projector::visible_world_padded`）。
    pub fn expand(&self, margin: f64) -> Aabb {
        if self.is_empty() {
            return *self;
        }
        Aabb {
            min: [self.min[0] - margin, self.min[1] - margin],
            max: [self.max[0] + margin, self.max[1] + margin],
        }
    }
}

impl Default for Aabb {
    fn default() -> Self {
        Self::empty()
    }
}

/// 图例符号类型（图层面板行内图例 / 地图图例用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegendKind {
    /// 面要素：填充块 + 边框。
    Polygon,
    /// 线要素：斜线段。
    Line,
    /// 点要素：圆点。
    Point,
}

/// 图层渲染样式。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerStyle {
    pub fill: Color32,
    pub outline: Color32,
    pub outline_width: f32,
    pub point_color: Color32,
    pub point_radius: f32,
}

impl Default for LayerStyle {
    fn default() -> Self {
        // 默认符号化按白底地图画布选型（色板见 `crate::style`）。
        Self {
            fill: style::layer_fill(),
            outline: style::LAYER_OUTLINE,
            outline_width: 1.5,
            point_color: style::LAYER_POINT,
            point_radius: 3.0,
        }
    }
}

/// 多边形数据（外环 + 孔），工程文件读写与 [`Layer::from_geometry`] 共用。
#[derive(Debug, Clone)]
pub struct PolygonData {
    pub outer: Vec<Coord2D>,
    pub holes: Vec<Vec<Coord2D>>,
}

/// 一个已导入的矢量图层（几何已展开、三角已缓存）。
#[derive(Debug, Clone)]
pub struct Layer {
    pub id: u64,
    pub name: String,
    pub visible: bool,
    pub style: LayerStyle,
    pub bounds: Aabb,
    pub feature_count: usize,
    pub points: Vec<Coord2D>,
    pub lines: Vec<Vec<Coord2D>>,
    pub outer_rings: Vec<Vec<Coord2D>>,
    pub hole_rings: Vec<Vec<Coord2D>>,
    /// 每个外环的孔数量（与 `outer_rings` 等长、按下标对应）。
    /// `hole_rings` 是扁平存储，工程文件保存/加载时用它恢复"孔属于哪个多边形"。
    pub hole_counts: Vec<usize>,
    /// ear-clipping 结果：每个元素为 3 个顶点的 6 个坐标分量。
    pub triangles: Vec<[f64; 6]>,
    /// 图层源数据的投影 / 坐标系（导入时从 GeoJSON 默认值 / `.prj` 识别）。
    pub projection: ProjectionInfo,
    /// 属性字段名（**不含** FID —— FID 即属性行号，见 `attrs`）。
    pub fields: Vec<String>,
    /// 属性表：每行对应一个源要素，列序与 `fields` 一致（缺失值为空串）。
    /// 与要素数量等长，行号即 FID（1 基展示、0 基存取）。
    pub attrs: Vec<Vec<String>>,
    /// 各几何列表元素 → 属性行号的映射（与对应几何列表等长）。
    /// 批量生成的结果图层（如缓冲区）无属性来源，这三个映射为空。
    pub point_rows: Vec<usize>,
    pub line_rows: Vec<usize>,
    pub ring_rows: Vec<usize>,
}

impl Default for Layer {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            visible: true,
            style: LayerStyle::default(),
            bounds: Aabb::empty(),
            feature_count: 0,
            points: Vec::new(),
            lines: Vec::new(),
            outer_rings: Vec::new(),
            hole_rings: Vec::new(),
            hole_counts: Vec::new(),
            triangles: Vec::new(),
            projection: ProjectionInfo::none(),
            fields: Vec::new(),
            attrs: Vec::new(),
            point_rows: Vec::new(),
            line_rows: Vec::new(),
            ring_rows: Vec::new(),
        }
    }
}

impl Layer {
    /// 图例符号类型：按「面 > 线 > 点」取该图层中存在的最高优先级几何。
    pub fn legend_kind(&self) -> LegendKind {
        if !self.outer_rings.is_empty() {
            LegendKind::Polygon
        } else if !self.lines.is_empty() {
            LegendKind::Line
        } else {
            LegendKind::Point
        }
    }

    fn from_parts(name: String, parts: Vec<Part>) -> Layer {
        let mut layer = Layer {
            name,
            feature_count: parts.len(),
            ..Layer::default()
        };
        // 字段名按「首次出现顺序」收集（GeoJSON 的 properties 是 map，顺序稳定），
        // 每行按同一列序铺平，缺失字段留空串。
        let mut fields: Vec<String> = Vec::new();
        for part in &parts {
            for (k, _) in &part.attrs {
                if !fields.iter().any(|f| f == k) {
                    fields.push(k.clone());
                }
            }
        }
        let mut attrs: Vec<Vec<String>> = Vec::with_capacity(parts.len());
        // 外环先攒下来，最后一次性并行三角化（见 `tri::triangulate_polygons`）；
        // 逐环串行剖分是导入耗时的大头（本仓库 1.7MB 样例占 ~72%）。
        let mut rings: Vec<Vec<Coord2D>> = Vec::new();
        let mut holes_of_ring: Vec<Vec<Vec<Coord2D>>> = Vec::new();
        let mut hole_counts: Vec<usize> = Vec::new();
        for (row, part) in parts.into_iter().enumerate() {
            attrs.push(fill_row(&fields, &part.attrs));
            match part.geometry {
                Geometry2D::Point(p) => {
                    layer.point_rows.push(row);
                    layer.points.push(p);
                    layer.bounds.extend_point(p);
                }
                Geometry2D::Line(pts) => {
                    for p in &pts {
                        layer.bounds.extend_point(*p);
                    }
                    layer.line_rows.push(row);
                    layer.lines.push(pts);
                }
                Geometry2D::Polygon { outer, holes } => {
                    for p in outer.iter().chain(holes.iter().flatten()) {
                        layer.bounds.extend_point(*p);
                    }
                    hole_counts.push(holes.len());
                    layer.ring_rows.push(row);
                    layer.hole_rings.extend(holes.iter().cloned());
                    holes_of_ring.push(holes);
                    rings.push(outer);
                }
            }
        }
        layer.triangles = triangulate_polygons(&rings, &holes_of_ring);
        layer.outer_rings = rings;
        layer.hole_counts = hole_counts;
        layer.attrs = attrs;
        layer.fields = fields;
        layer
    }

    /// 用现成的 2D 几何数据直接构建图层（工程文件加载路径，`project::load` 调用）。
    ///
    /// 与导入路径（[`load_file`]）走同一套 `from_parts`：包围盒统计 + ear-clipping
    /// 三角剖分。`id` / `visible` / `style` 用默认值，由调用方按文件内容覆盖。
    pub fn from_geometry(
        name: String,
        points: Vec<Coord2D>,
        lines: Vec<Vec<Coord2D>>,
        polygons: Vec<PolygonData>,
    ) -> Layer {
        let mut parts = Vec::with_capacity(points.len() + lines.len() + polygons.len());
        for p in points {
            parts.push(Part {
                geometry: Geometry2D::Point(p),
                attrs: Vec::new(),
            });
        }
        for l in lines {
            parts.push(Part {
                geometry: Geometry2D::Line(l),
                attrs: Vec::new(),
            });
        }
        for polygon in polygons {
            parts.push(Part {
                geometry: Geometry2D::Polygon {
                    outer: polygon.outer,
                    holes: polygon.holes,
                },
                attrs: Vec::new(),
            });
        }
        Self::from_parts(name, parts)
    }

    /// 用「几何 + 属性」要素列表构建图层（地理处理工具的输出路径）。
    ///
    /// 与 [`Layer::from_geometry`] 的唯一差别是保留属性：字段名按要素的
    /// 首次出现顺序收集，属性值按同一列序铺平，因此裁剪 / 相交 / 联合 /
    /// 合并 / 融合的结果图层都带完整的属性表（可用「按属性」导出与着色）。
    pub fn from_features(name: String, features: Vec<FeatureData>) -> Layer {
        let parts = features
            .into_iter()
            .map(|f| Part {
                geometry: f.geometry,
                attrs: f.attrs,
            })
            .collect();
        Self::from_parts(name, parts)
    }
}

// ---------------------------------------------------------------------------
// 图层树（v0.3：图层组）
// ---------------------------------------------------------------------------

/// 图层组：可包含图层与嵌套子组。
///
/// `visible` 是整棵子树的"总开关"——取消勾选**不会**改写后代的勾选状态，
/// 只是让它们不参与绘制（重新勾选即原样恢复），与 ArcGIS 内容列表的行为一致。
#[derive(Debug, Clone)]
pub struct Group {
    pub id: u64,
    pub name: String,
    pub visible: bool,
    /// 内容列表中的展开状态（纯视图状态，随工程一起保存）。
    pub expanded: bool,
    pub children: Vec<LayerNode>,
}

/// 图层树节点：图层组或叶子图层。
///
/// `Layer` 明显大于 `Group`（它带着几何缓冲与属性表），但 `Layer` 才是常态、
/// 且访问极频繁——装箱只会给每次遍历加一层间接调用，得不偿失，故显式忽略。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum LayerNode {
    Group(Group),
    Layer(Layer),
}

impl LayerNode {
    pub fn id(&self) -> u64 {
        match self {
            Self::Group(g) => g.id,
            Self::Layer(l) => l.id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Group(g) => &g.name,
            Self::Layer(l) => &l.name,
        }
    }

    pub fn visible(&self) -> bool {
        match self {
            Self::Group(g) => g.visible,
            Self::Layer(l) => l.visible,
        }
    }

    pub fn set_visible(&mut self, visible: bool) {
        match self {
            Self::Group(g) => g.visible = visible,
            Self::Layer(l) => l.visible = visible,
        }
    }

    pub fn is_group(&self) -> bool {
        matches!(self, Self::Group(_))
    }

    /// 组是否处于展开状态（叶子恒为 `false`）。
    pub fn expanded(&self) -> bool {
        matches!(self, Self::Group(g) if g.expanded)
    }

    pub fn toggle_expanded(&mut self) {
        if let Self::Group(g) = self {
            g.expanded = !g.expanded;
        }
    }

    pub fn set_expanded(&mut self, value: bool) {
        if let Self::Group(g) = self {
            g.expanded = value;
        }
    }

    /// 子节点（叶子返回空切片）。
    pub fn children(&self) -> &[LayerNode] {
        match self {
            Self::Group(g) => &g.children,
            Self::Layer(_) => &[],
        }
    }

    /// 子节点（仅组可写；叶子返回 `None`）。
    pub fn children_mut(&mut self) -> Option<&mut Vec<LayerNode>> {
        match self {
            Self::Group(g) => Some(&mut g.children),
            Self::Layer(_) => None,
        }
    }

    pub fn layer(&self) -> Option<&Layer> {
        match self {
            Self::Layer(l) => Some(l),
            Self::Group(_) => None,
        }
    }

    /// 叶子图层的图例符号类型（组返回 `None`）。
    pub fn legend_kind(&self) -> Option<LegendKind> {
        self.layer().map(Layer::legend_kind)
    }

    /// 子树中是否含有至少一个叶子图层（空组不算）。
    fn has_layers(&self) -> bool {
        match self {
            Self::Layer(_) => true,
            Self::Group(g) => g.children.iter().any(LayerNode::has_layers),
        }
    }
}

/// 内容列表中的一行（前序遍历拍平，带缩进深度与拖拽落点信息）。
pub struct PanelRow<'a> {
    /// 缩进深度；面板顶部合成的「图层」根节点为 0，其下依次 +1。
    pub depth: usize,
    /// 祖先（不含自身）是否全部勾选；`false` 时该行应弱化显示。
    pub ancestors_visible: bool,
    /// 所在父组 id（顶层为 `None`）。
    pub parent: Option<u64>,
    /// 在父组（或顶层列表）中的下标。
    pub index: usize,
    pub node: &'a LayerNode,
}

/// 图层集合（内容列表树）。`roots[0]` 是面板最顶层，也是最后绘制的。
#[derive(Default)]
pub struct LayerManager {
    roots: Vec<LayerNode>,
    next_id: u64,
    selected: Option<u64>,
    /// 会改变渲染结果的变更计数（渲染缓存据此判断是否需要重建）。
    revision: u64,
}

impl LayerManager {
    /// 是否没有任何叶子图层（只有空组也算空）。
    pub fn is_empty(&self) -> bool {
        !self.roots.iter().any(LayerNode::has_layers)
    }

    /// 渲染缓存版本号：只要图层集合 / 可见性 / 顺序 / 几何发生改变就会自增。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// 克隆整棵树（保存工程时移交后台线程序列化用）。
    pub fn roots_cloned(&self) -> Vec<LayerNode> {
        self.roots.clone()
    }

    /// 整体替换图层树（工程文件加载路径）：重新编号、选中第一个顶层节点。
    pub fn replace_with(&mut self, mut roots: Vec<LayerNode>) {
        self.next_id = 0;
        for node in roots.iter_mut() {
            renumber(node, &mut self.next_id);
        }
        self.roots = roots;
        self.selected = self.roots.first().map(LayerNode::id);
        self.bump();
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// 新节点的落点：当前选中项是组（且该组仍存在）→ 插到组内最上层；否则插到顶层最上层。
    fn insertion_point(&self) -> (Option<u64>, usize) {
        if let Some(id) = self.selected
            && self.node(id).is_some_and(LayerNode::is_group)
        {
            return (Some(id), 0);
        }
        (None, 0)
    }

    /// 加入一个图层，返回新图层 id。选中项是组时会自动放进该组。
    pub fn add(&mut self, mut layer: Layer) -> u64 {
        layer.id = self.take_id();
        let id = layer.id;
        let (parent, index) = self.insertion_point();
        self.selected = Some(id);
        let _ = insert_node(&mut self.roots, parent, index, LayerNode::Layer(layer));
        self.bump();
        id
    }

    /// 新建图层组（名称由调用方按当前语言给出），返回新组 id。
    pub fn add_group(&mut self, name: String) -> u64 {
        let (parent, index) = self.insertion_point();
        let id = self.take_id();
        let group = Group {
            id,
            name,
            visible: true,
            expanded: true,
            children: Vec::new(),
        };
        self.selected = Some(id);
        let _ = insert_node(&mut self.roots, parent, index, LayerNode::Group(group));
        self.bump();
        id
    }

    /// 自底向上的绘制顺序：面板最顶层的图层最后绘制（压在最上面）。
    ///
    /// 只产出"有效可见"（自身与所有祖先都勾选）的图层——渲染与统计都只需要这些。
    pub fn iter_visible(&self) -> impl Iterator<Item = &Layer> {
        let mut out = Vec::new();
        push_visible_leaves(&self.roots, true, &mut out);
        out.reverse();
        out.into_iter()
    }

    /// 内容列表的行（前序；折叠的组不展开其子节点）。
    pub fn panel_rows(&self) -> Vec<PanelRow<'_>> {
        let mut out = Vec::new();
        walk_panel(&self.roots, 0, true, None, &mut out);
        out
    }

    pub fn selected_id(&self) -> Option<u64> {
        self.selected
    }

    pub fn select(&mut self, id: u64) {
        self.selected = Some(id);
    }

    /// 按 id 取节点（图层与组共用同一 id 空间）。
    pub fn node(&self, id: u64) -> Option<&LayerNode> {
        find_node(&self.roots, id)
    }

    pub fn set_visible(&mut self, id: u64, visible: bool) {
        if let Some(node) = find_node_mut(&mut self.roots, id) {
            node.set_visible(visible);
        }
        self.bump();
    }

    /// 展开 / 折叠单个组（纯视图状态，不影响渲染，故不增加版本号）。
    pub fn toggle_expanded(&mut self, id: u64) {
        if let Some(node) = find_node_mut(&mut self.roots, id) {
            node.toggle_expanded();
        }
    }

    /// 一次性展开 / 折叠所有组。
    pub fn set_all_expanded(&mut self, expanded: bool) {
        for node in &mut self.roots {
            set_expanded_recursive(node, expanded);
        }
    }

    pub fn remove(&mut self, id: u64) {
        if take_node(&mut self.roots, id).is_some() {
            // 选中的节点可能正是被删掉的节点，或其子孙。
            if let Some(sel) = self.selected
                && find_node(&self.roots, sel).is_none()
            {
                self.selected = None;
            }
            self.bump();
        }
    }

    /// 把节点移动到 `parent`（`None` = 顶层）下的第 `index` 个位置。
    ///
    /// 拒绝「把节点移进自身后代」的请求（会形成环），此时返回 `false` 且不改动树。
    pub fn move_node(&mut self, id: u64, parent: Option<u64>, index: usize) -> bool {
        let allowed = match parent {
            None => find_node(&self.roots, id).is_some(),
            Some(pid) => {
                pid != id
                    && match (find_node(&self.roots, pid), find_node(&self.roots, id)) {
                        (Some(p), Some(n)) => p.is_group() && !contains_id(n, pid),
                        _ => false,
                    }
            }
        };
        if !allowed {
            return false;
        }
        let Some(node) = take_node(&mut self.roots, id) else {
            return false;
        };
        match insert_node(&mut self.roots, parent, index, node) {
            Ok(()) => {
                self.bump();
                true
            }
            Err(node) => {
                // 目标父节点在摘除后消失（正常流程不可达）：原样放回顶层，绝不丢数据。
                self.roots.insert(0, node);
                false
            }
        }
    }

    /// 与既有节点不重名的展示名（`base`、`base 2`、`base 3`…）。
    pub fn unique_name(&self, base: &str) -> String {
        if !self.has_name(base) {
            return base.to_string();
        }
        (2..)
            .map(|k| format!("{base} {k}"))
            .find(|name| !self.has_name(name))
            .unwrap_or_else(|| base.to_string())
    }

    fn has_name(&self, name: &str) -> bool {
        self.roots.iter().any(|n| any_name_eq(n, name))
    }

    /// 全部有效可见层级的联合包围盒。
    pub fn total_bounds(&self) -> Option<Aabb> {
        let mut union = Aabb::empty();
        for layer in self.iter_visible() {
            union.merge(layer.bounds);
        }
        if union.is_empty() {
            None
        } else {
            Some(union)
        }
    }
}

// ---------------------------------------------------------------------------
// 树的内部工具
// ---------------------------------------------------------------------------

/// 面板顺序收集"有效可见"的叶子图层（`parent_visible` 为祖先链的累计结果）。
fn push_visible_leaves<'a>(nodes: &'a [LayerNode], parent_visible: bool, out: &mut Vec<&'a Layer>) {
    for node in nodes {
        let visible = parent_visible && node.visible();
        match node {
            LayerNode::Layer(l) => {
                if visible {
                    out.push(l);
                }
            }
            LayerNode::Group(g) => push_visible_leaves(&g.children, visible, out),
        }
    }
}

/// 内容列表前序遍历（只下钻已展开的组）。
fn walk_panel<'a>(
    nodes: &'a [LayerNode],
    depth: usize,
    ancestors_visible: bool,
    parent: Option<u64>,
    out: &mut Vec<PanelRow<'a>>,
) {
    for (index, node) in nodes.iter().enumerate() {
        out.push(PanelRow {
            depth,
            ancestors_visible,
            parent,
            index,
            node,
        });
        if let LayerNode::Group(g) = node
            && g.expanded
        {
            walk_panel(
                &g.children,
                depth + 1,
                ancestors_visible && g.visible,
                Some(g.id),
                out,
            );
        }
    }
}

fn find_node(nodes: &[LayerNode], id: u64) -> Option<&LayerNode> {
    nodes.iter().find_map(|node| {
        if node.id() == id {
            Some(node)
        } else {
            find_node(node.children(), id)
        }
    })
}

fn find_node_mut(nodes: &mut [LayerNode], id: u64) -> Option<&mut LayerNode> {
    nodes.iter_mut().find_map(|node| {
        if node.id() == id {
            Some(node)
        } else {
            find_node_mut(node.children_mut()?, id)
        }
    })
}

/// 取某个组 id 对应的子节点列表（叶子的 id 返回 `None`）。
fn children_of_mut(nodes: &mut [LayerNode], id: u64) -> Option<&mut Vec<LayerNode>> {
    nodes.iter_mut().find_map(|node| {
        if node.id() == id {
            node.children_mut()
        } else {
            children_of_mut(node.children_mut()?, id)
        }
    })
}

// `LayerNode` 内联了完整的 `Layer`（约 240 字节），`Err` 分支因此偏大。但这条分支
// 只在"目标组刚好不存在"时走到（调用方均先校验），为此把节点装箱反而让整棵树的
// 每一次访问都多一层间接寻址，不划算。
#[allow(clippy::result_large_err)]
fn insert_node(
    roots: &mut Vec<LayerNode>,
    parent: Option<u64>,
    index: usize,
    node: LayerNode,
) -> Result<(), LayerNode> {
    match parent {
        None => {
            let at = index.min(roots.len());
            roots.insert(at, node);
            Ok(())
        }
        Some(pid) => {
            let Some(children) = children_of_mut(roots, pid) else {
                return Err(node);
            };
            let at = index.min(children.len());
            children.insert(at, node);
            Ok(())
        }
    }
}

/// 从树中摘除并返回节点（含整棵子树）。
fn take_node(nodes: &mut Vec<LayerNode>, id: u64) -> Option<LayerNode> {
    if let Some(i) = nodes.iter().position(|n| n.id() == id) {
        return Some(nodes.remove(i));
    }
    for node in nodes.iter_mut() {
        if let Some(children) = node.children_mut()
            && let Some(found) = take_node(children, id)
        {
            return Some(found);
        }
    }
    None
}

fn contains_id(node: &LayerNode, id: u64) -> bool {
    node.id() == id || node.children().iter().any(|c| contains_id(c, id))
}

fn any_name_eq(node: &LayerNode, name: &str) -> bool {
    node.name() == name || node.children().iter().any(|c| any_name_eq(c, name))
}

fn set_expanded_recursive(node: &mut LayerNode, expanded: bool) {
    node.set_expanded(expanded);
    if let Some(children) = node.children_mut() {
        for child in children {
            set_expanded_recursive(child, expanded);
        }
    }
}

/// 前序重编号（图层与组共用同一 id 空间，保证 `find_node` 唯一）。
fn renumber(node: &mut LayerNode, next: &mut u64) {
    match node {
        LayerNode::Group(g) => {
            g.id = *next;
            *next += 1;
            for child in &mut g.children {
                renumber(child, next);
            }
        }
        LayerNode::Layer(l) => {
            l.id = *next;
            *next += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// 导入
// ---------------------------------------------------------------------------

/// 按扩展名加载 GeoJSON（.geojson/.json）或 Shapefile（.shp）为图层。
pub fn load_file(path: &Path) -> Result<Layer, String> {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "layer".to_string());
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let (parts, projection) = match ext.as_str() {
        "shp" => (load_shapefile(path)?, shapefile_projection(path)),
        "geojson" | "json" => {
            // RFC 7946：GeoJSON 坐标默认 WGS 84 经纬度（EPSG:4326）。
            // 非标准的 `crs` 成员不读取——识别到也按默认处理（v0.2.1 约定）。
            (
                load_geojson_file(path)?,
                ProjectionInfo {
                    model: ProjectionModel::Geographic,
                    crs: CrsId::Epsg(4326),
                },
            )
        }
        other => {
            return Err(format!(
                "不支持的文件格式：.{other}（支持 .geojson / .json / .shp）"
            ))
        }
    };
    if parts.is_empty() {
        return Err("文件中没有可渲染的几何要素".to_string());
    }
    let mut layer = Layer::from_parts(name, parts);
    layer.projection = projection;
    Ok(layer)
}

/// Shapefile 的投影 / 坐标系：读同目录 `.prj`（WKT 文本）；
/// 文件缺失或无法识别时返回"未知"（不视为导入错误——没有 `.prj` 的
/// Shapefile 很常见，几何仍可渲染，只是坐标含义未知）。
fn shapefile_projection(shp_path: &Path) -> ProjectionInfo {
    let prj = shp_path.with_extension("prj");
    let Ok(wkt) = std::fs::read_to_string(&prj) else {
        return ProjectionInfo::none();
    };
    parse_prj_wkt(&wkt)
}

/// 导入 GeoJSON 文件（FeatureCollection / Feature / Geometry）。
///
/// 必须一次性读入内存再用 `from_slice` 解析：`serde_json::from_reader` 在
/// `File` 这种无缓冲读取器上会退化成**逐字节系统调用**——实测本仓库 1.7MB
/// 样例文件光这一项就 1.5s，占导入总耗时 95% 以上；换 `from_slice` 后降到 ~15ms。
/// 内存换时间的取舍在这里是划算的：矢量文件本身就是一次性全量加载的。
fn load_geojson_file(path: &Path) -> Result<Vec<Part>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("无法打开文件：{e}"))?;
    let gj: geojson::GeoJson =
        serde_json::from_slice(&bytes).map_err(|e| format!("GeoJSON 解析失败：{e}"))?;
    let mut parts = Vec::new();
    collect_geojson_parts(gj, &mut parts);
    Ok(parts)
}

/// 按值消费 GeoJSON 树，把几何展开为 2D 要素。
fn collect_geojson_parts(gj: geojson::GeoJson, parts: &mut Vec<Part>) {
    match gj {
        geojson::GeoJson::FeatureCollection(fc) => {
            for f in fc.features {
                // 先把 properties 取出来：geometry 被 move 后就没法再访问 f 了。
                let attrs = properties_attrs(f.properties.as_ref());
                if let Some(geom) = f.geometry {
                    expand_geojson_value(geom.value, &attrs, parts);
                }
            }
        }
        geojson::GeoJson::Feature(f) => {
            let attrs = properties_attrs(f.properties.as_ref());
            if let Some(geom) = f.geometry {
                expand_geojson_value(geom.value, &attrs, parts);
            }
        }
        // 纯 Geometry 类型没有 properties 概念，传空 slice（属性表整体为空）。
        geojson::GeoJson::Geometry(geom) => expand_geojson_value(geom.value, &[], parts),
    }
}

/// `properties` 对象 → 属性键值对。
///
/// 注意：`serde_json::Map` 在本 crate 未启用 `preserve_order`（Cargo.toml 里是
/// `serde_json = "1"`，锁文件里也没有 indexmap），因此它是 **BTreeMap**，迭代顺序
/// 是键的字典序，而不是 JSON 文本里的书写顺序——字段顺序因此是字典序。
fn properties_attrs(
    properties: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Vec<(String, String)> {
    properties
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), json_value_to_string(v)))
                .collect()
        })
        .unwrap_or_default()
}

/// GeoJSON 属性值 → 展示字符串：字符串原样、数字/布尔用其字面量、`null` 记空串、
/// 数组/对象紧凑序列化（保留结构，属性表里可读）。
fn json_value_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        // Number::to_string：整数按整数输出（不会出现 ".0"），小数保留原样。
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// `geojson::Value` → 2D 要素（Multi* 拆分为多个 Part）。
///
/// 直接从 `geojson::Value` 展开，不再经 `geo_types::Geometry` 中转——
/// 后者会把整棵树再拷一遍（每个坐标一次 `Coord` 构造 + 一次 `coords_of` 复制）。
///
/// `attrs` 是该要素的属性副本：Multi* / GeometryCollection 展开出的每个 Part
/// 都带同一份（属性行号即要素序号，FID 由行号承担，不另造字段）。
fn expand_geojson_value(
    value: geojson::Value,
    attrs: &[(String, String)],
    parts: &mut Vec<Part>,
) {
    match value {
        geojson::Value::Point(p) => {
            if let Some(c) = coord2d(&p) {
                parts.push(Part {
                    geometry: Geometry2D::Point(c),
                    attrs: attrs.to_vec(),
                });
            }
        }
        geojson::Value::MultiPoint(mp) => {
            for p in &mp {
                if let Some(c) = coord2d(p) {
                    parts.push(Part {
                        geometry: Geometry2D::Point(c),
                        attrs: attrs.to_vec(),
                    });
                }
            }
        }
        geojson::Value::LineString(ls) => {
            push_line(&ls, attrs, parts);
        }
        geojson::Value::MultiLineString(mls) => {
            for ls in &mls {
                push_line(ls, attrs, parts);
            }
        }
        // 按 RFC 7946：首个环为外环、其余为孔。
        geojson::Value::Polygon(poly) => push_polygon(poly, attrs, parts),
        geojson::Value::MultiPolygon(mp) => {
            for poly in mp {
                push_polygon(poly, attrs, parts);
            }
        }
        geojson::Value::GeometryCollection(gc) => {
            for g in gc {
                expand_geojson_value(g.value, attrs, parts);
            }
        }
    }
}

fn push_line(line: &[geojson::Position], attrs: &[(String, String)], parts: &mut Vec<Part>) {
    let pts = positions_to_coords(line);
    if pts.len() >= 2 {
        parts.push(Part {
            geometry: Geometry2D::Line(pts),
            attrs: attrs.to_vec(),
        });
    }
}

/// 空 Polygon（无环）直接丢弃；`poly[0]` 为外环、`poly[1..]` 为孔。
fn push_polygon(
    poly: geojson::PolygonType,
    attrs: &[(String, String)],
    parts: &mut Vec<Part>,
) {
    let mut rings = poly.into_iter().map(|r| positions_to_coords(&r));
    let Some(outer) = rings.next() else {
        return;
    };
    parts.push(Part {
        geometry: Geometry2D::Polygon {
            outer,
            holes: rings.collect(),
        },
        attrs: attrs.to_vec(),
    });
}

/// 位置 → 2D 坐标；丢弃 Z 分量，元素不足 2 个（畸形数据）则跳过该点。
fn coord2d(p: &[f64]) -> Option<Coord2D> {
    match *p {
        [x, y, ..] => Some([x, y]),
        _ => None,
    }
}

fn positions_to_coords(positions: &[geojson::Position]) -> Vec<Coord2D> {
    positions.iter().filter_map(|p| coord2d(p)).collect()
}

/// 导入 Shapefile（.shp，自动寻找 .shx/.dbf；dbf 编码自动检测，含 GBK）。
fn load_shapefile(path: &Path) -> Result<Vec<Part>, String> {
    // 缺 .dbf 时 shapefile 会直接拒绝打开（Error::MissingDbf）；退回只读几何的
    // ShapeReader，让「无属性数据」的 shp 仍能导入（每个几何补一行空属性）。
    let mut reader = match shapefile::Reader::from_path(path) {
        Ok(reader) => reader,
        Err(_) => return load_shapefile_geometry_only(path),
    };
    // 先整表读出：字段顺序只存在于 dbf 表信息里，而取表信息要按值消费 reader，
    // 所以不能一边迭代记录一边取。
    let rows = reader
        .read()
        .map_err(|e| format!("Shapefile 读取失败：{e}"))?;
    // dbase::Record 内部是 HashMap，遍历它拿不到列序；列序只能从 dbf 模式取。
    // （dbase::TableInfo 没有公开的字段访问器，所以另开一个 dbase Reader 读模式。）
    let field_names = dbf_field_names(&path.with_extension("dbf"))?;
    let mut parts = Vec::new();
    for (shape, record) in rows {
        // Null 形状没有几何等价物，跳过。
        let Ok(geom) = Geometry::<f64>::try_from(shape) else {
            continue;
        };
        let attrs = record_attrs(&record, &field_names);
        expand_geometry(&geom, &mut parts, &attrs);
    }
    Ok(parts)
}

/// 无 .dbf 的 Shapefile：只读几何，属性表为空。
fn load_shapefile_geometry_only(path: &Path) -> Result<Vec<Part>, String> {
    let shapes = shapefile::ShapeReader::from_path(path)
        .and_then(|reader| reader.read())
        .map_err(|e| format!("Shapefile 打开失败：{e}"))?;
    let mut parts = Vec::new();
    for shape in shapes {
        let Ok(geom) = Geometry::<f64>::try_from(shape) else {
            continue;
        };
        expand_geometry(&geom, &mut parts, &[]);
    }
    Ok(parts)
}

/// 读 .dbf 的字段名，按 dbf 模式顺序。shapefile 约定同名同目录的 `.dbf`，
/// `dbase::Reader::fields()` 返回的正是 header 里的模式顺序。
fn dbf_field_names(dbf_path: &Path) -> Result<Vec<String>, String> {
    let reader = shapefile::dbase::ReaderBuilder::new()
        .open(dbf_path)
        .map_err(|e| format!(".dbf 打开失败：{e}"))?;
    Ok(reader.fields().iter().map(|f| f.name().to_string()).collect())
}

/// dbase 记录 → 属性键值对。`field_names` 由调用方按 dbf 模式顺序给出，
/// 因为 `Record` 是 HashMap、自身不保序（缺字段记空串）。
fn record_attrs(
    record: &shapefile::dbase::Record,
    field_names: &[String],
) -> Vec<(String, String)> {
    field_names
        .iter()
        .map(|name| {
            let value = record
                .get(name)
                .map(field_value_to_string)
                .unwrap_or_default();
            (name.clone(), value)
        })
        .collect()
}

/// dbase 字段值 → 人类可读字符串；空值（`Option::None`）记空串。
fn field_value_to_string(value: &shapefile::dbase::FieldValue) -> String {
    use shapefile::dbase::FieldValue;
    match value {
        FieldValue::Character(s) => s.clone().unwrap_or_default(),
        FieldValue::Numeric(n) => n.map(|x| x.to_string()).unwrap_or_default(),
        FieldValue::Float(f) => f.map(|x| x.to_string()).unwrap_or_default(),
        FieldValue::Logical(b) => b.map(|x| x.to_string()).unwrap_or_default(),
        FieldValue::Date(d) => d.map(format_dbase_date).unwrap_or_default(),
        FieldValue::Integer(i) => i.to_string(),
        FieldValue::Currency(c) => c.to_string(),
        FieldValue::DateTime(dt) => format_dbase_datetime(*dt),
        FieldValue::Double(d) => d.to_string(),
        FieldValue::Memo(s) => s.clone(),
    }
}

/// dbf 日期 → `YYYY-MM-DD`（属性表里可读、字典序即时间序）。
fn format_dbase_date(date: shapefile::dbase::Date) -> String {
    format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
}

/// FoxPro DateTime → `YYYY-MM-DD HH:MM:SS`。
fn format_dbase_datetime(dt: shapefile::dbase::DateTime) -> String {
    let (d, t) = (dt.date(), dt.time());
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        d.year(),
        d.month(),
        d.day(),
        t.hours(),
        t.minutes(),
        t.seconds()
    )
}

/// 把 geo_types 几何展开为 2D 要素（Multi* 拆分为多个 Part）。
fn expand_geometry(geom: &Geometry<f64>, parts: &mut Vec<Part>, attrs: &[(String, String)]) {
    // 一个 shape 拆成多个 Part 时，每个 Part 都 clone 同一份属性副本。
    let push = |g: Geometry2D, parts: &mut Vec<Part>| {
        parts.push(Part {
            geometry: g,
            attrs: attrs.to_vec(),
        });
    };
    match geom {
        Geometry::Point(p) => push(Geometry2D::Point([p.x(), p.y()]), parts),
        Geometry::Line(line) => {
            if line.start != line.end {
                push(
                    Geometry2D::Line(
                        [[line.start.x, line.start.y], [line.end.x, line.end.y]].to_vec(),
                    ),
                    parts,
                );
            }
        }
        Geometry::LineString(ls) => {
            let pts: Vec<Coord2D> = coords_of(&ls.0);
            if pts.len() >= 2 {
                push(Geometry2D::Line(pts), parts);
            }
        }
        Geometry::MultiPoint(mp) => {
            for p in &mp.0 {
                push(Geometry2D::Point([p.x(), p.y()]), parts);
            }
        }
        Geometry::MultiLineString(mls) => {
            for ls in &mls.0 {
                let pts = coords_of(&ls.0);
                if pts.len() >= 2 {
                    push(Geometry2D::Line(pts), parts);
                }
            }
        }
        Geometry::Polygon(poly) => {
            push(
                Geometry2D::Polygon {
                    outer: coords_of(poly.exterior().0.as_slice()),
                    holes: poly
                        .interiors()
                        .iter()
                        .map(|r| coords_of(r.0.as_slice()))
                        .collect(),
                },
                parts,
            );
        }
        Geometry::MultiPolygon(mp) => {
            for poly in &mp.0 {
                push(
                    Geometry2D::Polygon {
                        outer: coords_of(poly.exterior().0.as_slice()),
                        holes: poly
                            .interiors()
                            .iter()
                            .map(|r| coords_of(r.0.as_slice()))
                            .collect(),
                    },
                    parts,
                );
            }
        }
        Geometry::Rect(r) => {
            let (min, max) = (r.min(), r.max());
            push(
                Geometry2D::Polygon {
                    outer: vec![
                        [min.x, min.y],
                        [max.x, min.y],
                        [max.x, max.y],
                        [min.x, max.y],
                        [min.x, min.y],
                    ],
                    holes: Vec::new(),
                },
                parts,
            );
        }
        Geometry::Triangle(t) => {
            let (a, b, c) = (t.v1(), t.v2(), t.v3());
            push(
                Geometry2D::Polygon {
                    outer: vec![
                        [a.x, a.y],
                        [b.x, b.y],
                        [c.x, c.y],
                        [a.x, a.y],
                    ],
                    holes: Vec::new(),
                },
                parts,
            );
        }
        Geometry::GeometryCollection(gc) => {
            for g in &gc.0 {
                expand_geometry(g, parts, attrs);
            }
        }
    }
}

fn coords_of(coords: &[geo_types::Coord<f64>]) -> Vec<Coord2D> {
    coords.iter().map(|c| [c.x, c.y]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geojson_to_layer(json: &str) -> Layer {
        let gj: geojson::GeoJson = serde_json::from_str(json).unwrap();
        let mut parts = Vec::new();
        collect_geojson_parts(gj, &mut parts);
        Layer::from_parts("test".into(), parts)
    }

    #[test]
    fn geojson_point_line_polygon() {
        let json = r#"{
          "type": "FeatureCollection",
          "features": [
            {"type": "Feature", "geometry": {"type": "Point", "coordinates": [116.0, 39.0]}, "properties": {"name": "A"}},
            {"type": "Feature", "geometry": {"type": "LineString", "coordinates": [[0,0],[1,1],[2,0]]}, "properties": {}},
            {"type": "Feature", "geometry": {"type": "Polygon", "coordinates": [[[0,0],[4,0],[4,4],[0,4],[0,0]]]}, "properties": {}}
          ]
        }"#;
        let layer = geojson_to_layer(json);
        assert_eq!(layer.feature_count, 3);
        assert_eq!(layer.points.len(), 1);
        assert_eq!(layer.lines.len(), 1);
        assert_eq!(layer.outer_rings.len(), 1);
        // 4x4 正方形 → 2 个三角
        assert_eq!(layer.triangles.len(), 2);
        assert!(!layer.bounds.is_empty());
        // 并集包围盒：x∈[0,116]，y∈[0,39]
        assert!((layer.bounds.center()[0] - 58.0).abs() < 1e-9);
        assert!((layer.bounds.center()[1] - 19.5).abs() < 1e-9);
    }

    #[test]
    fn geojson_multipolygon_expands() {
        let json = r#"{
          "type": "FeatureCollection",
          "features": [
            {"type": "Feature", "geometry": {"type": "MultiPolygon", "coordinates": [
              [ [[0,0],[2,0],[2,2],[0,0]] ],
              [ [[10,10],[12,10],[12,12],[10,10]] ]
            ]}, "properties": {}}
          ]
        }"#;
        let layer = geojson_to_layer(json);
        // MultiPolygon 展开为 2 个多边形要素
        assert_eq!(layer.feature_count, 2);
        assert_eq!(layer.outer_rings.len(), 2);
    }

    /// 孔必须与所属外环对应：`hole_counts` / `hole_rings` 的顺序决定了工程文件
    /// 能否还原"孔属于哪个多边形"，是导入路径最容易错的地方。
    #[test]
    fn geojson_holes_stay_aligned_with_outer_rings() {
        let json = r#"{
          "type": "FeatureCollection",
          "features": [
            {"type": "Feature", "geometry": {"type": "Polygon", "coordinates": [
              [[0.0,0.0],[10.0,0.0],[10.0,10.0],[0.0,10.0],[0.0,0.0]],
              [[2.0,2.0],[4.0,2.0],[4.0,4.0],[2.0,2.0]]
            ]}, "properties": {}},
            {"type": "Feature", "geometry": {"type": "MultiPolygon", "coordinates": [
              [ [[20.0,20.0],[22.0,20.0],[22.0,22.0],[20.0,20.0]] ],
              [ [[30.0,30.0],[31.0,30.0],[31.0,31.0],[30.0,30.0]],
                [[30.2,30.2],[30.4,30.2],[30.2,30.2]] ]
            ]}, "properties": {}}
          ]
        }"#;
        let layer = geojson_to_layer(json);
        assert_eq!(layer.outer_rings.len(), 3, "1 个 Polygon + MultiPolygon 的 2 个多边形");
        assert_eq!(layer.hole_counts, vec![1, 0, 1]);
        assert_eq!(layer.hole_rings.len(), 2);
        assert_eq!(layer.feature_count, 3);
    }

    /// 带 Z 的坐标只取 XY，且不得因多余分量报错。
    #[test]
    fn geojson_keeps_xy_and_drops_z() {
        let json = r#"{"type":"FeatureCollection","features":[
          {"type":"Feature","geometry":{"type":"Point","coordinates":[1.0,2.0,3.0]},"properties":{}},
          {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0.0,0.0,9.0],[1.0,1.0,9.0]]},"properties":{}}
        ]}"#;
        let layer = geojson_to_layer(json);
        assert_eq!(layer.points, vec![[1.0, 2.0]]);
        assert_eq!(layer.lines, vec![vec![[0.0, 0.0], [1.0, 1.0]]]);
    }

    /// 空 Polygon / 单点 LineString / null geometry 都要安全跳过，不能 panic。
    #[test]
    fn geojson_skips_degenerate_geometry() {
        let json = r#"{"type":"FeatureCollection","features":[
          {"type":"Feature","geometry":{"type":"Polygon","coordinates":[]},"properties":{}},
          {"type":"Feature","geometry":{"type":"LineString","coordinates":[[0.0,0.0]]},"properties":{}},
          {"type":"Feature","geometry":null,"properties":{}},
          {"type":"Feature","geometry":{"type":"GeometryCollection","geometries":[
            {"type":"Point","coordinates":[5.0,6.0]}
          ]},"properties":{}}
        ]}"#;
        let layer = geojson_to_layer(json);
        assert_eq!(layer.outer_rings.len(), 0);
        assert_eq!(layer.lines.len(), 0);
        assert_eq!(layer.points, vec![[5.0, 6.0]]);
    }

    // -----------------------------------------------------------------------
    // 导入时采集属性（GeoJSON properties / Shapefile dbf 记录）
    //
    // 注意：serde_json 的 `Map` 在本 crate 未启用 `preserve_order`，实际是 BTreeMap，
    // 所以 `fields` 的顺序是**键的字典序**，而不是 JSON 文本里的书写顺序。
    // 值本身仍由 `fill_row` 按 `fields` 列序精确对齐，不会错位。
    // -----------------------------------------------------------------------

    #[test]
    fn geojson_collects_property_values() {
        let json = r#"{"type":"FeatureCollection","features":[
          {"type":"Feature","geometry":{"type":"Point","coordinates":[0.0,0.0]},
           "properties":{"name":"A","pop":12,"ok":true,"none":null}}
        ]}"#;
        let layer = geojson_to_layer(json);
        assert_eq!(layer.fields, vec!["name", "none", "ok", "pop"]);
        assert_eq!(layer.attrs.len(), 1);
        let row = &layer.attrs[0];
        assert_eq!(row[0], "A"); // 字符串原样
        assert_eq!(row[1], ""); // null → 空串
        assert_eq!(row[2], "true"); // 布尔 → true/false
        assert_eq!(row[3], "12"); // 整数不带 ".0"
    }

    #[test]
    fn geojson_missing_fields_become_empty_strings() {
        let json = r#"{"type":"FeatureCollection","features":[
          {"type":"Feature","geometry":{"type":"Point","coordinates":[0.0,0.0]},
           "properties":{"name":"A","pop":1}},
          {"type":"Feature","geometry":{"type":"Point","coordinates":[1.0,1.0]},
           "properties":{"name":"B"}}
        ]}"#;
        let layer = geojson_to_layer(json);
        assert_eq!(layer.fields, vec!["name", "pop"]);
        assert_eq!(layer.attrs.len(), 2);
        // 第二行缺 pop：补空串而不是错位。
        assert_eq!(layer.attrs[0], vec!["A", "1"]);
        assert_eq!(layer.attrs[1], vec!["B", ""]);
    }

    #[test]
    fn geojson_multi_geometry_shares_one_attr_copy() {
        let json = r#"{"type":"FeatureCollection","features":[
          {"type":"Feature","geometry":{"type":"MultiPolygon","coordinates":[
            [ [[0,0],[2,0],[2,2],[0,0]] ],
            [ [[10,10],[12,10],[12,12],[10,10]] ]
          ]},"properties":{"name":"M"}}
        ]}"#;
        let layer = geojson_to_layer(json);
        assert_eq!(layer.feature_count, 2);
        assert_eq!(layer.attrs.len(), 2);
        // 一个 feature 展开出的多个 Part 带同一份属性副本。
        assert_eq!(layer.attrs[0], layer.attrs[1]);
        assert_eq!(layer.attrs[0], vec!["M"]);
    }

    #[test]
    fn geojson_merges_fields_across_features() {
        let json = r#"{"type":"FeatureCollection","features":[
          {"type":"Feature","geometry":{"type":"Point","coordinates":[0.0,0.0]},"properties":{"b":2,"a":1}},
          {"type":"Feature","geometry":{"type":"Point","coordinates":[1.0,1.0]},"properties":{"c":3}}
        ]}"#;
        let layer = geojson_to_layer(json);
        // 字段按首次出现顺序合并；BTreeMap 下即字典序。
        assert_eq!(layer.fields, vec!["a", "b", "c"]);
        assert_eq!(layer.attrs[0], vec!["1", "2", ""]);
        assert_eq!(layer.attrs[1], vec!["", "", "3"]);
    }

    #[test]
    fn geojson_without_properties_yields_empty_attr_rows() {
        // 纯 Geometry 类型：没有 properties 概念，不能 panic。
        let json = r#"{"type":"GeometryCollection","geometries":[
          {"type":"Point","coordinates":[1.0,2.0]},
          {"type":"Point","coordinates":[3.0,4.0]}
        ]}"#;
        let layer = geojson_to_layer(json);
        assert!(layer.fields.is_empty());
        assert_eq!(layer.feature_count, 2);
        // 属性行数仍与要素数一致（每行空）。
        assert_eq!(layer.attrs.len(), 2);
        assert!(layer.attrs.iter().all(|r| r.is_empty()));
    }

    /// 数组/对象属性按紧凑 JSON 序列化保留结构。
    #[test]
    fn geojson_container_values_are_serialized() {
        let json = r#"{"type":"FeatureCollection","features":[
          {"type":"Feature","geometry":{"type":"Point","coordinates":[0.0,0.0]},
           "properties":{"tags":[1,2],"meta":{"k":"v"}}}
        ]}"#;
        let layer = geojson_to_layer(json);
        assert_eq!(layer.fields, vec!["meta", "tags"]);
        assert_eq!(layer.attrs[0], vec!["{\"k\":\"v\"}", "[1,2]"]);
    }

    /// `record_attrs` 直接单测：覆盖各 `FieldValue` 变体 → 字符串（含日期格式）。
    #[test]
    fn record_attrs_formats_field_values_by_schema_order() {
        use dbase::{Date, DateTime, FieldValue, Record, Time};
        let mut rec = Record::default();
        rec.insert("s".into(), FieldValue::Character(Some("文本".into())));
        rec.insert("n".into(), FieldValue::Numeric(Some(12.0)));
        rec.insert("i".into(), FieldValue::Integer(-3));
        rec.insert("f".into(), FieldValue::Float(Some(1.5)));
        rec.insert("dd".into(), FieldValue::Double(2.25));
        rec.insert("b".into(), FieldValue::Logical(Some(true)));
        rec.insert("dt".into(), FieldValue::Date(Some(Date::new(5, 6, 2024).unwrap())));
        rec.insert("none".into(), FieldValue::Character(None));
        rec.insert(
            "ts".into(),
            FieldValue::DateTime(DateTime::new(
                Date::new(5, 6, 2024).unwrap(),
                Time::new(7, 8, 9).unwrap(),
            )),
        );
        // 列序由调用方（dbf 模式）给出，与 Record 的 HashMap 内部顺序无关。
        let names: Vec<String> = ["s", "n", "i", "f", "dd", "b", "dt", "none", "ts"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let attrs = record_attrs(&rec, &names);
        let order: Vec<&str> = attrs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            order,
            vec!["s", "n", "i", "f", "dd", "b", "dt", "none", "ts"]
        );
        let get = |k: &str| attrs.iter().find(|(n, _)| n == k).unwrap().1.clone();
        assert_eq!(get("s"), "文本");
        assert_eq!(get("n"), "12");
        assert_eq!(get("i"), "-3");
        assert_eq!(get("f"), "1.5");
        assert_eq!(get("dd"), "2.25");
        assert_eq!(get("b"), "true");
        assert_eq!(get("dt"), "2024-06-05"); // YYYY-MM-DD
        assert_eq!(get("none"), ""); // 空值 → 空串
        assert_eq!(get("ts"), "2024-06-05 07:08:09");
    }

    // -----------------------------------------------------------------------
    // 端到端导入链路：真实文件 → load_file → 几何展开 → 三角剖分 → Layer
    //
    // 这两条用例直接落盘生成文件再走生产代码路径，覆盖「添加图层」的完整链路
    // （含文件 IO 与 Shapefile 读取），而不只是内部函数。
    // -----------------------------------------------------------------------

    /// 生成"锯齿"环（大量凹角，模拟真实海岸线 / 行政边界），首尾闭合。
    fn zigzag_ring(cx: f64, cy: f64, r: f64, n: usize, phase: f64) -> Vec<Coord2D> {
        let mut pts: Vec<Coord2D> = (0..n)
            .map(|i| {
                let a = phase + std::f64::consts::TAU * i as f64 / n as f64;
                let rr = if i % 2 == 0 { r } else { r * 0.72 };
                [cx + rr * a.cos(), cy + rr * a.sin()]
            })
            .collect();
        pts.push(pts[0]);
        pts
    }

    fn ring_center(k: usize) -> (f64, f64) {
        (100.0 + (k % 8) as f64 * 0.5, 30.0 + (k / 8) as f64 * 0.5)
    }

    fn temp_path(tag: &str, ext: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rustgis_import_{}_{}.{}", std::process::id(), tag, ext));
        p
    }

    fn write_geojson(path: &Path, polygons: usize, ring_verts: usize) {
        let features: Vec<serde_json::Value> = (0..polygons)
            .map(|k| {
                let (cx, cy) = ring_center(k);
                let ring = zigzag_ring(cx, cy, 0.2, ring_verts, k as f64 * 0.11);
                serde_json::json!({
                    "type": "Feature",
                    "properties": { "id": k },
                    "geometry": { "type": "Polygon", "coordinates": [ring] }
                })
            })
            .collect();
        let fc = serde_json::json!({ "type": "FeatureCollection", "features": features });
        std::fs::write(path, serde_json::to_vec(&fc).unwrap()).unwrap();
    }

    fn write_shapefile(path: &Path, polygons: usize, ring_verts: usize) {
        use dbase::{FieldName, FieldValue, Record};
        let builder = dbase::TableWriterBuilder::new()
            .add_character_field(FieldName::try_from("name").unwrap(), 32);
        let mut writer = shapefile::Writer::from_path(path, builder).expect("创建 Shapefile");
        for k in 0..polygons {
            let (cx, cy) = ring_center(k);
            let ring = zigzag_ring(cx, cy, 0.2, ring_verts, k as f64 * 0.11);
            // Shapefile 约定外环为顺时针，故逆序写入。
            let pts: Vec<shapefile::Point> = ring
                .iter()
                .rev()
                .map(|p| shapefile::Point::new(p[0], p[1]))
                .collect();
            let poly = shapefile::Polygon::new(shapefile::PolygonRing::Outer(pts));
            let mut rec = Record::default();
            rec.insert(
                "name".to_string(),
                FieldValue::Character(Some(format!("p{k}"))),
            );
            writer
                .write_shape_and_record(&poly, &rec)
                .expect("写入多边形");
        }
    }

    /// 性能探针：对真实文件分段计时，定位导入瓶颈。
    /// `RUSTGIS_PROBE_FILE=<路径> cargo test --release --bin rustgis -- --ignored --nocapture probe_load_file`
    #[test]
    #[ignore = "perf probe"]
    fn probe_load_file() {
        let Ok(path) = std::env::var("RUSTGIS_PROBE_FILE") else {
            println!("skip: 未设置 RUSTGIS_PROBE_FILE");
            return;
        };
        let path = std::path::PathBuf::from(path);

        let t = std::time::Instant::now();
        let bytes = std::fs::read(&path).expect("读取失败");
        println!("fs::read         {:>10?}  ({} bytes)", t.elapsed(), bytes.len());

        let t = std::time::Instant::now();
        let layer = load_file(&path).expect("导入失败");
        println!("load_file (总)   {:>10?}", t.elapsed());

        let t = std::time::Instant::now();
        let gj: geojson::GeoJson = serde_json::from_slice(&bytes).expect("解析失败");
        println!("serde_json parse {:>10?}", t.elapsed());

        let t = std::time::Instant::now();
        let mut parts = Vec::new();
        collect_geojson_parts(gj, &mut parts);
        println!("expand geometry  {:>10?}  ({} parts)", t.elapsed(), parts.len());

        let t = std::time::Instant::now();
        let mut rings = Vec::new();
        for p in &parts {
            if let Geometry2D::Polygon { outer, .. } = &p.geometry {
                rings.push(outer.clone());
            }
        }
        let tris = crate::geo::tri::triangulate_rings(&rings);
        println!("triangulate      {:>10?}", t.elapsed());

        let verts: usize = parts
            .iter()
            .map(|p| match &p.geometry {
                Geometry2D::Point(_) => 1,
                Geometry2D::Line(l) => l.len(),
                Geometry2D::Polygon { outer, holes } => {
                    outer.len() + holes.iter().map(|h| h.len()).sum::<usize>()
                }
            })
            .sum();
        println!(
            "要素 {} 个 / 顶点 {} 个 / 三角 {} 个 / 外环 {} 个 / 并行度 {:?}",
            parts.len(),
            verts,
            tris.len(),
            layer.outer_rings.len(),
            std::thread::available_parallelism().map(|n| n.get())
        );
        let biggest = layer.outer_rings.iter().map(|r| r.len()).max().unwrap_or(0);
        println!("最大外环顶点数 {biggest}");
    }

    #[test]
    fn imports_large_geojson_end_to_end() {
        let path = temp_path("big", "geojson");
        let (polygons, ring_verts) = (60usize, 1_000usize);
        write_geojson(&path, polygons, ring_verts);

        let layer = load_file(&path).expect("GeoJSON 应能导入");
        let _ = std::fs::remove_file(&path);

        assert_eq!(layer.feature_count, polygons);
        assert_eq!(layer.outer_rings.len(), polygons);
        // 每个环 ring_verts 个顶点（含闭合点去重后）⇒ 每个环 ring_verts - 2 个三角形
        assert_eq!(layer.triangles.len(), polygons * (ring_verts - 2));
        assert!(!layer.bounds.is_empty());
    }

    #[test]
    fn imports_large_shapefile_end_to_end() {
        let path = temp_path("big", "shp");
        let (polygons, ring_verts) = (60usize, 1_000usize);
        write_shapefile(&path, polygons, ring_verts);

        let layer = load_file(&path).expect("Shapefile 应能导入");
        for ext in ["shp", "shx", "dbf"] {
            let _ = std::fs::remove_file(path.with_extension(ext));
        }

        assert_eq!(layer.feature_count, polygons);
        assert_eq!(layer.outer_rings.len(), polygons);
        assert_eq!(layer.triangles.len(), polygons * (ring_verts - 2));
        assert!(!layer.bounds.is_empty());
    }

    /// Shapefile 端到端：dbf 记录按模式顺序采集为 fields/attrs。
    #[test]
    fn shapefile_collects_record_attributes() {
        use dbase::{FieldName, FieldValue, Record};
        let path = temp_path("attrs", "shp");
        let builder = dbase::TableWriterBuilder::new()
            .add_character_field(FieldName::try_from("name").unwrap(), 32)
            .add_numeric_field(FieldName::try_from("pop").unwrap(), 10, 0);
        let mut writer = shapefile::Writer::from_path(&path, builder).expect("创建 Shapefile");
        for k in 0..2u32 {
            let (cx, cy) = ring_center(k as usize);
            let ring = zigzag_ring(cx, cy, 0.2, 8, k as f64 * 0.11);
            let pts: Vec<shapefile::Point> = ring
                .iter()
                .rev()
                .map(|p| shapefile::Point::new(p[0], p[1]))
                .collect();
            let poly = shapefile::Polygon::new(shapefile::PolygonRing::Outer(pts));
            let mut rec = Record::default();
            rec.insert(
                "name".to_string(),
                FieldValue::Character(Some(format!("p{k}"))),
            );
            rec.insert(
                "pop".to_string(),
                FieldValue::Numeric(Some(k as f64 * 10.0)),
            );
            writer
                .write_shape_and_record(&poly, &rec)
                .expect("写入多边形");
        }
        drop(writer); // 先落盘，再导入。

        let layer = load_file(&path).expect("Shapefile 应能导入");
        for ext in ["shp", "shx", "dbf"] {
            let _ = std::fs::remove_file(path.with_extension(ext));
        }

        // 字段顺序 = dbf 模式顺序（name 在前、pop 在后），不是 HashMap 的遍历顺序。
        assert_eq!(layer.fields, vec!["name", "pop"]);
        assert_eq!(layer.attrs.len(), 2);
        assert_eq!(layer.attrs[0], vec!["p0", "0"]);
        assert_eq!(layer.attrs[1], vec!["p1", "10"]);
    }

    /// 无 .dbf 的 shp：仍可导入（退回只读几何），属性表为空但行数与要素数一致。
    #[test]
    fn shapefile_without_dbf_imports_with_empty_attrs() {
        let path = temp_path("nodbf", "shp");
        write_shapefile(&path, 3, 8);
        let _ = std::fs::remove_file(path.with_extension("dbf"));

        let layer = load_file(&path).expect("无 dbf 的 Shapefile 也应能导入");
        for ext in ["shp", "shx"] {
            let _ = std::fs::remove_file(path.with_extension(ext));
        }

        assert_eq!(layer.feature_count, 3);
        assert!(layer.fields.is_empty());
        assert_eq!(layer.attrs.len(), 3);
        assert!(layer.attrs.iter().all(|r| r.is_empty()));
    }

    // -----------------------------------------------------------------------
    // 图层树（组 / 可见性级联 / 拖拽落点）
    // -----------------------------------------------------------------------

    /// 造一个世界坐标即包围盒的图层（只用于树的拓扑与可见性断言）。
    fn boxed_layer(name: &str, x: f64) -> Layer {
        Layer::from_geometry(
            name.to_string(),
            Vec::new(),
            Vec::new(),
            vec![PolygonData {
                outer: vec![[x, 0.0], [x + 1.0, 0.0], [x + 1.0, 1.0], [x, 1.0], [x, 0.0]],
                holes: Vec::new(),
            }],
        )
    }

    /// 面板顺序 = 前序（父在前、子紧随）；绘制顺序 = 面板顺序取反。
    #[test]
    fn tree_orders_follow_panel_and_draw_direction() {
        let mut mgr = LayerManager::default();
        let a = mgr.add_group("A".into()); // 选中 A ⇒ 新图层落进 A
        mgr.add(boxed_layer("a1", 0.0));
        mgr.select(a); // 重新选中组，下一个图层仍进 A
        mgr.add(boxed_layer("a2", 1.0));
        mgr.select(u64::MAX); // 选中项已失效 ⇒ 新图层退回顶层
        mgr.add(boxed_layer("c", 2.0));

        let panel: Vec<&str> = mgr.panel_rows().iter().map(|r| r.node.name()).collect();
        assert_eq!(panel, vec!["c", "A", "a2", "a1"]);

        // 绘制顺序：面板最底部的先画、最顶层的最后画。
        let draw: Vec<&str> = mgr.iter_visible().map(|l| l.name.as_str()).collect();
        assert_eq!(draw, vec!["a1", "a2", "c"]);

        let rows = mgr.panel_rows();
        assert_eq!(rows[0].depth, 0); // c
        assert_eq!(rows[1].depth, 0); // A
        assert_eq!(rows[2].depth, 1); // a2 在 A 内
        assert_eq!(rows[2].parent, Some(a));
        assert_eq!(rows[2].index, 0);
    }

    /// 取消勾选组只影响绘制，不改写后代的勾选状态；渲染遍历按祖先链级联。
    #[test]
    fn group_visibility_cascades_without_touching_children() {
        let mut mgr = LayerManager::default();
        let g = mgr.add_group("G".into());
        mgr.add(boxed_layer("in", 0.0));
        mgr.select(u64::MAX);
        mgr.add(boxed_layer("out", 5.0));

        assert_eq!(mgr.iter_visible().count(), 2);

        mgr.set_visible(g, false);
        let names: Vec<&str> = mgr.iter_visible().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["out"], "组隐藏后其子图层不应参与绘制");

        let inner = mgr.node(g).unwrap().children()[0].id();
        assert!(
            mgr.node(inner).unwrap().visible(),
            "组隐藏不应改写子图层的勾选状态"
        );

        // 包围盒随之收缩到只剩可见图层。
        let bounds = mgr.total_bounds().unwrap();
        assert!((bounds.min[0] - 5.0).abs() < 1e-9);

        mgr.set_visible(g, true);
        assert_eq!(mgr.iter_visible().count(), 2, "重新勾选应原样恢复");
    }

    /// 折叠的组不出现在面板行里，但仍然是树的一部分。
    #[test]
    fn collapsed_group_hides_descendants_in_panel() {
        let mut mgr = LayerManager::default();
        let g = mgr.add_group("G".into());
        mgr.add(boxed_layer("in", 0.0));
        mgr.toggle_expanded(g);

        let names: Vec<&str> = mgr.panel_rows().iter().map(|r| r.node.name()).collect();
        assert_eq!(names, vec!["G"]);
        assert_eq!(
            mgr.iter_visible().count(),
            1,
            "折叠只影响面板，不影响图层树"
        );

        mgr.set_all_expanded(true);
        assert_eq!(mgr.panel_rows().len(), 2);
        mgr.set_all_expanded(false);
        assert_eq!(mgr.panel_rows().len(), 1);
    }

    /// 拖拽落点：同层重排、跨组搬家；移进自身后代必须被拒绝。
    #[test]
    fn move_node_reorders_reparents_and_rejects_cycles() {
        let mut mgr = LayerManager::default();
        let g = mgr.add_group("G".into());
        let x = mgr.add(boxed_layer("x", 0.0));
        mgr.select(u64::MAX);
        let y = mgr.add(boxed_layer("y", 1.0));

        // 顶层顺序：y, G
        assert_eq!(mgr.panel_rows()[0].node.name(), "y");
        // 把 G 拖到顶层末尾 ⇒ x 随之
        assert!(mgr.move_node(g, None, 1));
        assert_eq!(mgr.panel_rows()[0].node.name(), "y");
        // 把 x 从 G 里搬到顶层第一位
        assert!(mgr.move_node(x, None, 0));
        let names: Vec<&str> = mgr.panel_rows().iter().map(|r| r.node.name()).collect();
        assert_eq!(names, vec!["x", "y", "G"]);

        // 把 y 放进 G
        assert!(mgr.move_node(y, Some(g), 0));
        assert_eq!(mgr.panel_rows()[2].parent, Some(g));

        // 环检测：G 不能移进 G 自己，也不能移进自己的后代
        assert!(!mgr.move_node(g, Some(g), 0));
        let inner = mgr.node(g).unwrap().children()[0].id();
        assert!(!mgr.move_node(g, Some(inner), 0));
        assert!(mgr.node(g).is_some(), "拒绝的移动不应破坏树");
    }

    /// 删除组会连同后代一起移除；选中项失效时被清空。
    #[test]
    fn remove_takes_the_whole_subtree() {
        let mut mgr = LayerManager::default();
        let g = mgr.add_group("G".into());
        let inner = mgr.add(boxed_layer("in", 0.0));
        mgr.select(u64::MAX);
        mgr.add(boxed_layer("out", 1.0));

        mgr.select(inner);
        mgr.remove(g);
        assert!(mgr.node(g).is_none());
        assert!(mgr.node(inner).is_none());
        assert_eq!(mgr.iter_visible().count(), 1);
        assert_eq!(mgr.selected_id(), None, "选中项随子树一起失效");

        // 只剩一个根节点：全局展开/折叠不应 panic
        mgr.set_all_expanded(false);
    }

    /// 只有空组时 panel 视为"无图层"。
    #[test]
    fn manager_with_only_empty_groups_is_empty() {
        let mut mgr = LayerManager::default();
        assert!(mgr.is_empty());
        mgr.add_group("空组".into());
        assert!(mgr.is_empty());
        assert_eq!(mgr.total_bounds(), None);
        mgr.add(boxed_layer("l", 0.0));
        assert!(!mgr.is_empty());
    }

    /// 重名组会被自动编号，避免内容列表里出现两个同名节点。
    #[test]
    fn unique_name_avoids_duplicates() {
        let mut mgr = LayerManager::default();
        mgr.add_group("新建图层组".into());
        mgr.select(u64::MAX);
        mgr.add_group("新建图层组".into());
        assert_eq!(mgr.unique_name("新建图层组"), "新建图层组 2");
        assert_eq!(mgr.unique_name("独一无二"), "独一无二");
    }

    /// 工程加载路径：`replace_with` 重新编号后 id 必须全局唯一且能按 id 找回。
    #[test]
    fn replace_with_renumbers_ids_uniquely() {
        let mut mgr = LayerManager::default();
        let g = mgr.add_group("G".into());
        mgr.add(boxed_layer("in", 0.0));
        let snapshot = mgr.roots_cloned();

        let mut fresh = LayerManager::default();
        fresh.replace_with(snapshot);
        let ids: Vec<u64> = fresh.panel_rows().iter().map(|r| r.node.id()).collect();
        assert_eq!(ids, vec![0, 1], "id 应按前序重排");
        for id in ids {
            assert!(fresh.node(id).is_some());
        }
        assert!(fresh.node(g + 100).is_none());
    }

    // -----------------------------------------------------------------------
    // 投影 / 坐标系识别（`.prj` WKT 解析）
    // -----------------------------------------------------------------------

    /// 地理坐标系（CGCS2000 经纬度）：模型 + EPSG 代码 + 名称兜底路径。
    #[test]
    fn parse_prj_wkt_geographic() {
        let wkt = r#"GEOGCS["CGCS 2000",
    DATUM["China_2000",
        SPHEROID["CGCS2000",6378137,298.257222101]],
    PRIMEM["Greenwich",0],
    UNIT["degree",0.0174532925199433],
    AUTHORITY["EPSG","4490"]]"#;
        let info = parse_prj_wkt(wkt);
        assert_eq!(info.model, ProjectionModel::Geographic);
        assert_eq!(info.crs, CrsId::Epsg(4490));
    }

    /// 投影坐标系（CGCS2000 高斯-克吕格 3 度带）：EPSG 取**最深层级**的
    /// 声明——内嵌 GEOGCS 的 4490 是基准地理系，投影系自身是 4549。
    #[test]
    fn parse_prj_wkt_projected_takes_deepest_epsg() {
        let wkt = r#"PROJCS["CGCS 2000 / 3 degree Gauss-Kruger zone 38",
    PROJECTION["Gauss_Kruger"],
    AUTHORITY["EPSG","4549"],
    PARAMETER["central_meridian",114],
    GEOGCS["CGCS 2000",
        DATUM["China_2000",
            SPHEROID["CGCS2000",6378137,298.257222101]],
        PRIMEM["Greenwich",0],
        UNIT["degree",0.0174532925199433],
        AUTHORITY["EPSG","4490"]],
    UNIT["metre",1],
    AXIS["Easting",EAST],
    AXIS["Northing",NORTH]]"#;
        let info = parse_prj_wkt(wkt);
        assert_eq!(info.model, ProjectionModel::Projected);
        assert_eq!(
            info.crs,
            CrsId::Epsg(4549),
            "取括号深度最浅的 EPSG 声明（PROJCS 自身），忽略嵌套 GEOGCS 的基准地理系代码 4490"
        );
    }

    /// 无 EPSG 声明的投影系：模型仍可识别，名称（WKT 首行）兜底。
    #[test]
    fn parse_prj_wkt_projected_name_fallback() {
        let wkt = r#"PROJCS["UTM Zone 50N",
    PROJECTION["Transverse_Mercator"],
    GEOGCS["WGS 84",
        DATUM["WGS_1984",
            SPHEROID["WGS 84",6378137,298.257223563]],
        PRIMEM["Greenwich",0],
        UNIT["degree",0.0174532925199433]]]"#;
        let info = parse_prj_wkt(wkt);
        assert_eq!(info.model, ProjectionModel::Projected);
        assert_eq!(info.crs, CrsId::Name("UTM Zone 50N".to_string()));
    }

    /// 完全无法识别的内容：模型未知 + 坐标系未识别（`none()`）。
    #[test]
    fn parse_prj_wkt_unknown() {
        let info = parse_prj_wkt("garbage data");
        assert_eq!(info.model, ProjectionModel::Unknown);
        assert_eq!(info.crs, CrsId::None);
    }
}
