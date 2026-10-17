//! 工程文件（.rustgis）：工程整体状态（图层 + 地图视图）的读写。
//!
//! 对标 ArcGIS 的"工程文件"概念（相当于 .aprx / .mxd）：单个文件描述整个工程，
//! 「文件 → 保存 / 另存为」落盘，「文件 → 打开工程」按原样恢复图层与视图。
//!
//! 设计约定：
//! - JSON 格式（`serde_json`），带 `version` 版本字段（格式破坏性变更时递增）；
//! - 只存**源几何**（点 / 线 / 多边形，含孔与归属分组），不存三角剖分结果；
//!   加载时经 [`crate::geo::Layer::from_geometry`] 重建三角（与导入路径同一套算法）；
//! - [`save`] / [`load`] 预期在**后台线程**调用（大工程 JSON 可达 MB 级），
//!   UI 线程只做非阻塞轮询（见 `app.rs` 的 `ProjectStage` 流水线）。

use std::path::Path;

use egui::Color32;
use serde::{Deserialize, Serialize};

use crate::geo::{
    CrsId, Group, Layer, LayerNode, LayerStyle, PolygonData, ProjectionInfo, ProjectionModel,
};
use crate::map::MapView;

/// 工程文件格式版本（破坏性变更时递增；高于当前值则拒绝加载）。
///
/// - v1：扁平的 `layers` 列表；
/// - v2：支持图层组的嵌套 `nodes` 树（仍可读 v1，见 [`ProjectFile::legacy_layers`]）；
/// - v3：工程级 / 图层级投影与坐标系（`projection` 字段，旧版本读取时缺省
///   为"未识别"，见 [`ProjectionInfo::none`]）。
/// - v4：图层属性表（`fields` / `attrs` / `*_rows`，旧版本读取时缺省为空表）。
pub const PROJECT_VERSION: u32 = 4;
/// 工程文件扩展名（文件选择器 / 落盘补全共用）。
pub const PROJECT_EXT: &str = "rustgis";
/// 新建工程的默认文件名（"另存为"对话框预填）。
pub const DEFAULT_FILE_NAME: &str = "untitled.rustgis";

/// 加载后得到的工程数据（可直接应用到应用状态）。
#[derive(Debug)]
pub struct ProjectData {
    /// 图层树（顶层节点，面板自上而下的顺序）。
    pub nodes: Vec<LayerNode>,
    /// 保存时的地图视图；`None` 表示加载后自动适配数据范围。
    pub view: Option<MapView>,
    /// 保存时的工程默认投影 / 坐标系；旧版本工程文件缺省为 `None`（未设置）。
    pub projection: Option<ProjectionInfo>,
}

// ---------------------------------------------------------------------------
// 文件格式（磁盘上的 JSON 结构）
// ---------------------------------------------------------------------------

/// 工程文件整体。
#[derive(Serialize, Deserialize)]
struct ProjectFile {
    version: u32,
    /// 地图视图（中心 + 比例尺）；缺省时打开后自动适配。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    view: Option<ViewDto>,
    /// v3：工程默认投影 / 坐标系；缺省（旧版本文件）为未设置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    projection: Option<ProjectionDto>,
    /// v2：图层树（顶层节点，面板自上而下）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    nodes: Vec<NodeDto>,
    /// v1 遗留：扁平图层列表（自底向上）。仅用于读取旧工程。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    layers: Vec<LayerDto>,
}

impl ProjectFile {
    /// 读出图层树：v2 直接取 `nodes`；v1 把扁平列表按"顶层图层"包装。
    fn to_nodes(&self) -> Result<Vec<LayerNode>, String> {
        if !self.nodes.is_empty() {
            return Ok(self.nodes.iter().map(NodeDto::to_node).collect());
        }
        if self.layers.is_empty() {
            return Ok(Vec::new());
        }
        if self.version > 1 {
            return Err("工程文件缺少图层树数据".to_string());
        }
        // v1 的 `layers` 是自底向上的渲染顺序，转成面板顺序要反过来。
        Ok(self
            .layers
            .iter()
            .rev()
            .map(|l| LayerNode::Layer(l.to_layer()))
            .collect())
    }
}

/// 图层树节点的文件表示（`kind` 区分图层与组）。
///
/// 与 [`crate::geo::LayerNode`] 同理：`LayerDto` 的几何/属性大表是常态载荷，
/// 装箱只会在反序列化时多一层间接。
#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "node", rename_all = "lowercase")]
enum NodeDto {
    Layer(LayerDto),
    Group(GroupDto),
}

/// 图层组的文件表示。
#[derive(Serialize, Deserialize)]
struct GroupDto {
    name: String,
    #[serde(default = "default_true")]
    visible: bool,
    #[serde(default = "default_true")]
    expanded: bool,
    #[serde(default)]
    children: Vec<NodeDto>,
}

impl From<&LayerNode> for NodeDto {
    fn from(n: &LayerNode) -> Self {
        match n {
            LayerNode::Layer(l) => Self::Layer(l.into()),
            LayerNode::Group(g) => Self::Group(GroupDto {
                name: g.name.clone(),
                visible: g.visible,
                expanded: g.expanded,
                children: g.children.iter().map(NodeDto::from).collect(),
            }),
        }
    }
}

impl NodeDto {
    /// 还原为可渲染节点（id 置 0，由 `LayerManager::replace_with` 重编号）。
    fn to_node(&self) -> LayerNode {
        match self {
            Self::Layer(l) => LayerNode::Layer(l.to_layer()),
            Self::Group(g) => LayerNode::Group(Group {
                id: 0,
                name: g.name.clone(),
                visible: g.visible,
                expanded: g.expanded,
                children: g.children.iter().map(NodeDto::to_node).collect(),
            }),
        }
    }
}

/// 地图视图快照。
#[derive(Serialize, Deserialize, Clone)]
struct ViewDto {
    center: [f64; 2],
    scale: f32,
}

/// 投影 / 坐标系快照（工程级与图层级共用）。
///
/// `crs` 是字符串（`EPSG:4326` / WKT 名称 / `""`），避免在磁盘上
/// 引入嵌套枚举——旧版本文件读取时缺省为空字符串（= 未识别）。
#[derive(Serialize, Deserialize, Clone)]
struct ProjectionDto {
    /// 投影模型（`geographic` / `projected` / `unknown`）。
    model: String,
    /// 坐标系标识（`EPSG:<code>` / 名称 / `""`）。
    crs: String,
}

impl From<&ProjectionInfo> for ProjectionDto {
    fn from(p: &ProjectionInfo) -> Self {
        Self {
            model: p.model.as_str().to_string(),
            crs: p.crs.storage(),
        }
    }
}

impl From<&ProjectionDto> for ProjectionInfo {
    fn from(d: &ProjectionDto) -> Self {
        let model = match d.model.as_str() {
            "geographic" => ProjectionModel::Geographic,
            "projected" => ProjectionModel::Projected,
            _ => ProjectionModel::Unknown,
        };
        let crs = if d.crs.is_empty() {
            CrsId::None
        } else if let Some(code) = d.crs.strip_prefix("EPSG:")
            && let Ok(code) = code.parse::<u32>()
        {
            CrsId::Epsg(code)
        } else {
            CrsId::Name(d.crs.clone())
        };
        ProjectionInfo { model, crs }
    }
}

impl From<MapView> for ViewDto {
    fn from(v: MapView) -> Self {
        Self {
            center: v.center,
            scale: v.scale,
        }
    }
}

impl From<ViewDto> for MapView {
    fn from(d: ViewDto) -> Self {
        Self {
            center: d.center,
            scale: d.scale,
        }
    }
}

/// 图层渲染样式快照（颜色按非预乘 RGBA 存储，见 [`color_to_u8`]）。
#[derive(Serialize, Deserialize, Clone)]
struct StyleDto {
    fill: [u8; 4],
    outline: [u8; 4],
    outline_width: f32,
    point_color: [u8; 4],
    point_radius: f32,
}

impl From<LayerStyle> for StyleDto {
    fn from(s: LayerStyle) -> Self {
        Self {
            fill: color_to_u8(s.fill),
            outline: color_to_u8(s.outline),
            outline_width: s.outline_width,
            point_color: color_to_u8(s.point_color),
            point_radius: s.point_radius,
        }
    }
}

impl From<StyleDto> for LayerStyle {
    fn from(d: StyleDto) -> Self {
        Self {
            fill: color_from_u8(d.fill),
            outline: color_from_u8(d.outline),
            outline_width: d.outline_width,
            point_color: color_from_u8(d.point_color),
            point_radius: d.point_radius,
        }
    }
}

/// 单图层的文件表示。
#[derive(Serialize, Deserialize)]
struct LayerDto {
    name: String,
    #[serde(default = "default_true")]
    visible: bool,
    style: StyleDto,
    #[serde(default)]
    points: Vec<[f64; 2]>,
    #[serde(default)]
    lines: Vec<Vec<[f64; 2]>>,
    /// 多边形要素（外环 + 逐多边形的孔，保留"孔属于哪个多边形"的分组）。
    #[serde(default)]
    polygons: Vec<PolygonDto>,
    /// v3：图层源数据的投影 / 坐标系；旧版本文件缺省为"未识别"。
    #[serde(default)]
    projection: Option<ProjectionDto>,
    /// v4：属性字段名（不含 FID，FID 即属性行号）。
    ///
    /// 用 `skip_serializing_if` 跳过空表：多数图层没有属性，直接写空数组会
    /// 在工程文件里产生大量噪音；空表缺省的语义与 `#[serde(default)]` 一致，
    /// 对读取方无差别。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fields: Vec<String>,
    /// v4：属性表（每行一个源要素，列序与 `fields` 一致，缺失为空串）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attrs: Vec<Vec<String>>,
    /// v4：各几何列表元素 → 属性行号的映射（与对应几何列表等长）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    point_rows: Vec<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    line_rows: Vec<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ring_rows: Vec<usize>,
}

fn default_true() -> bool {
    true
}

/// 单个多边形（外环 + 孔）。
#[derive(Serialize, Deserialize)]
struct PolygonDto {
    outer: Vec<[f64; 2]>,
    #[serde(default)]
    holes: Vec<Vec<[f64; 2]>>,
}

impl From<&Layer> for LayerDto {
    fn from(l: &Layer) -> Self {
        let mut hole_idx = 0usize;
        let polygons = l
            .outer_rings
            .iter()
            .zip(l.hole_counts.iter())
            .map(|(outer, &count)| {
                let holes: Vec<Vec<[f64; 2]>> = l
                    .hole_rings
                    .iter()
                    .skip(hole_idx)
                    .take(count)
                    .map(|h| h.to_vec())
                    .collect();
                hole_idx += count;
                PolygonDto {
                    outer: outer.to_vec(),
                    holes,
                }
            })
            .collect();
        Self {
            name: l.name.clone(),
            visible: l.visible,
            style: l.style.into(),
            points: l.points.to_vec(),
            lines: l.lines.to_vec(),
            polygons,
            projection: Some(ProjectionDto::from(&l.projection)),
            // v4：属性表原样落盘（FID = 行号，几何到属性的映射逐元素保存）。
            fields: l.fields.clone(),
            attrs: l.attrs.clone(),
            point_rows: l.point_rows.clone(),
            line_rows: l.line_rows.clone(),
            ring_rows: l.ring_rows.clone(),
        }
    }
}

impl LayerDto {
    /// 还原为可渲染图层（`id` 置 0，由调用方重编号；三角剖分在此重建）。
    fn to_layer(&self) -> Layer {
        let polygons = self
            .polygons
            .iter()
            .map(|p| PolygonData {
                outer: p.outer.to_vec(),
                holes: p.holes.to_vec(),
            })
            .collect();
        // 注意：这里刻意不写 `Layer { ... }` 全量字面量。`from_geometry` 内部已经
        // 走 `..Layer::default()`，以后 `Layer` 再加字段时本函数无需改动，只在
        // 需要覆盖文件里有记录的字段时追加赋值即可。
        let mut layer = Layer::from_geometry(
            self.name.clone(),
            self.points.to_vec(),
            self.lines.to_vec(),
            polygons,
        );
        layer.visible = self.visible;
        layer.style = self.style.clone().into();
        layer.projection = self
            .projection
            .as_ref()
            .map(ProjectionInfo::from)
            .unwrap_or_default();
        // v4：还原属性表与几何→属性映射（`from_geometry` 会清空这些表，
        // 因为它只认几何，不知道属性来源）。
        layer.fields = self.fields.clone();
        layer.attrs = self.attrs.clone();
        layer.point_rows = self.point_rows.clone();
        layer.line_rows = self.line_rows.clone();
        layer.ring_rows = self.ring_rows.clone();
        layer
    }
}

// ---------------------------------------------------------------------------
// 颜色序列化（Color32 内部按预乘 u8 存储，直接存原始字节会在
// 半透明色上丢失原色；这里统一转成非预乘 RGBA 往返，误差 ≤ 1 灰级）
// ---------------------------------------------------------------------------

fn color_to_u8(c: Color32) -> [u8; 4] {
    c.to_srgba_unmultiplied()
}

fn color_from_u8([r, g, b, a]: [u8; 4]) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

// ---------------------------------------------------------------------------
// 公共 API（预期在后台线程调用）
// ---------------------------------------------------------------------------

/// 序列化工程并写入磁盘（大工程 JSON 可达 MB 级，勿在 UI 线程调用）。
///
/// `projection` 是工程级默认投影 / 坐标系；`None` = 未设置
/// （旧行为：状态栏显示"未设置"）。
pub fn save(
    path: &Path,
    nodes: &[LayerNode],
    view: Option<MapView>,
    projection: Option<ProjectionInfo>,
) -> Result<(), String> {
    let file = ProjectFile {
        version: PROJECT_VERSION,
        view: view.map(ViewDto::from),
        projection: projection.as_ref().map(ProjectionDto::from),
        nodes: nodes.iter().map(NodeDto::from).collect(),
        layers: Vec::new(),
    };
    let bytes = serde_json::to_vec(&file).map_err(|e| format!("工程序列化失败：{e}"))?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建目录失败（{}）：{e}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("写入文件失败：{e}"))
}

/// 读取并解析工程文件（含几何展开与三角剖分，勿在 UI 线程调用）。
pub fn load(path: &Path) -> Result<ProjectData, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("无法读取工程文件：{e}"))?;
    let file: ProjectFile =
        serde_json::from_slice(&bytes).map_err(|e| format!("工程文件格式错误：{e}"))?;
    if file.version > PROJECT_VERSION {
        return Err(format!(
            "工程文件版本 v{} 过新（当前应用最高支持 v{PROJECT_VERSION}）",
            file.version
        ));
    }
    let nodes = file.to_nodes()?;
    Ok(ProjectData {
        nodes,
        view: file.view.clone().map(MapView::from),
        projection: file.projection.as_ref().map(ProjectionInfo::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 样例图层：2 点 + 1 线 + 1 带孔正方形（样式用不透明色，可精确往返）。
    fn sample_layer() -> Layer {
        let mut l = Layer::from_geometry(
            "roads".to_string(),
            vec![[116.0, 39.0], [116.1, 39.1]],
            vec![vec![[0.0, 0.0], [1.0, 1.0], [2.0, 0.0]]],
            vec![PolygonData {
                outer: vec![
                    [0.0, 0.0],
                    [4.0, 0.0],
                    [4.0, 4.0],
                    [0.0, 4.0],
                    [0.0, 0.0],
                ],
                holes: vec![vec![
                    [1.0, 1.0],
                    [2.0, 1.0],
                    [2.0, 2.0],
                    [1.0, 2.0],
                    [1.0, 1.0],
                ]],
            }],
        );
        l.visible = false;
        l.style = LayerStyle {
            fill: Color32::from_rgb(74, 144, 226),
            outline: Color32::from_rgb(0, 113, 188),
            outline_width: 2.5,
            point_color: Color32::from_rgb(227, 92, 44),
            point_radius: 4.0,
        };
        l
    }

    fn temp_project_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "rustgis_project_{}_{}.{}",
            std::process::id(),
            tag, PROJECT_EXT
        ));
        p
    }

    #[test]
    fn save_load_roundtrip() {
        let path = temp_project_path("roundtrip");
        let src = sample_layer();
        let nodes = vec![LayerNode::Layer(src.clone())];
        let view = Some(MapView {
            center: [116.4, 39.9],
            scale: 123.0,
        });
        // 工程默认投影（CGCS2000 高斯-克吕格 3 度带，EPSG:4549）也应往返。
        let projection = Some(ProjectionInfo {
            model: ProjectionModel::Projected,
            crs: CrsId::Epsg(4549),
        });
        save(&path, &nodes, view, projection.clone()).expect("保存应成功");

        let data = load(&path).expect("加载应成功");
        let _ = std::fs::remove_file(&path);

        assert_eq!(data.view, view);
        assert_eq!(data.projection, projection, "工程级投影应精确往返");
        assert_eq!(data.nodes.len(), 1);
        let l = data.nodes[0].layer().expect("根节点应为图层");
        assert_eq!(l.name, "roads");
        assert!(!l.visible);
        assert_eq!(l.style, src.style, "不透明样式应精确往返");
        assert_eq!(l.points, src.points);
        assert_eq!(l.lines, src.lines);
        assert_eq!(l.outer_rings, src.outer_rings);
        assert_eq!(l.hole_rings, src.hole_rings);
        assert_eq!(l.hole_counts, src.hole_counts);
        // 三角剖分在加载时重建。这里不复述剖分器的内部三角形个数（它会随
        // 带孔桥接算法演进变动），改为校验真正的几何不变量：覆盖面积 =
        // 外环 4x4 = 16 减去 1x1 的孔 = 15（即孔被真正挖空）。
        assert!(!l.triangles.is_empty(), "加载后应重建三角剖分");
        let area: f64 = l
            .triangles
            .iter()
            .map(|t| {
                ((t[2] - t[0]) * (t[5] - t[1]) - (t[4] - t[0]) * (t[3] - t[1])).abs() * 0.5
            })
            .sum();
        assert!(
            (area - 15.0).abs() < 1e-6,
            "带孔多边形覆盖面积应为 15（外环 16 − 孔 1），实际 {area}"
        );
        assert!(!l.bounds.is_empty());
        assert_eq!(l.projection, src.projection, "图层级投影应往返");
    }

    /// 图层组必须连同嵌套结构与展开状态一起往返。
    #[test]
    fn group_tree_roundtrip() {
        let path = temp_project_path("groups");
        let inner = LayerNode::Layer(sample_layer());
        let nodes = vec![
            LayerNode::Group(Group {
                id: 0,
                name: "底图".to_string(),
                visible: false,
                expanded: false,
                children: vec![
                    LayerNode::Group(Group {
                        id: 0,
                        name: "栅格".to_string(),
                        visible: true,
                        expanded: true,
                        children: vec![inner.clone()],
                    }),
                ],
            }),
            LayerNode::Layer(sample_layer()),
        ];
        save(&path, &nodes, None, None).expect("保存应成功");
        let data = load(&path).expect("加载应成功");
        let _ = std::fs::remove_file(&path);

        assert_eq!(data.nodes.len(), 2);
        assert!(data.projection.is_none(), "未设置投影的工程应保持未设置");
        let LayerNode::Group(outer) = &data.nodes[0] else {
            panic!("第一个顶层节点应为组");
        };
        assert_eq!(outer.name, "底图");
        assert!(!outer.visible);
        assert!(!outer.expanded);
        let LayerNode::Group(inner_group) = &outer.children[0] else {
            panic!("应为嵌套子组");
        };
        assert_eq!(inner_group.name, "栅格");
        assert!(inner_group.expanded);
        assert_eq!(inner_group.children.len(), 1);
        assert_eq!(inner_group.children[0].name(), "roads");
        assert!(data.nodes[1].layer().is_some());
    }

    /// v1 扁平工程（只有 `layers`）必须仍能打开：转成顶层图层。
    #[test]
    fn legacy_v1_flat_project_still_loads() {
        let path = temp_project_path("v1");
        let json = r#"{
            "version": 1,
            "view": null,
            "layers": [
                {"name": "bottom", "visible": true,
                 "style": {"fill":[74,144,226,80],"outline":[0,113,188,255],
                           "outline_width":1.5,"point_color":[227,92,44,255],"point_radius":3.0},
                 "points": [], "lines": [], "polygons": []},
                {"name": "top", "visible": false,
                 "style": {"fill":[74,144,226,80],"outline":[0,113,188,255],
                           "outline_width":1.5,"point_color":[227,92,44,255],"point_radius":3.0},
                 "points": [], "lines": [], "polygons": []}
            ]
        }"#;
        std::fs::write(&path, json).unwrap();
        let data = load(&path).expect("旧版本工程应能打开");
        let _ = std::fs::remove_file(&path);

        let names: Vec<&str> = data.nodes.iter().map(LayerNode::name).collect();
        // v1 的 layers 自底向上 ⇒ 面板顺序取反
        assert_eq!(names, vec!["top", "bottom"]);
        assert!(data.nodes[0].layer().is_some());
        assert!(data.projection.is_none(), "v1 文件没有 projection 字段 ⇒ 未设置");
        assert_eq!(
            data.nodes[0].layer().unwrap().projection,
            ProjectionInfo::none(),
            "v1 图层没有 projection 字段 ⇒ 未识别"
        );
    }

    #[test]
    fn empty_project_roundtrip() {
        let path = temp_project_path("empty");
        save(&path, &[], None, None).expect("保存应成功");
        let data = load(&path).expect("加载应成功");
        let _ = std::fs::remove_file(&path);
        assert!(data.nodes.is_empty());
        assert!(data.view.is_none());
        assert!(data.projection.is_none());
    }

    #[test]
    fn semi_transparent_color_roundtrip_within_tolerance() {
        // 预乘 u8 存储对低 alpha 色有舍入：alpha 无损，RGB 允许 2 灰级以内误差。
        let c = Color32::from_rgba_unmultiplied(74, 144, 226, 80);
        let back = color_from_u8(color_to_u8(c));
        let src = c.to_srgba_unmultiplied();
        let dst = back.to_srgba_unmultiplied();
        assert_eq!(dst[3], src[3], "alpha 应无损");
        for i in 0..3 {
            let (s, d) = (src[i], dst[i]);
            assert!(
                (s as i32 - d as i32).abs() <= 2,
                "通道 {i} 误差过大：{s} → {d}"
            );
        }
    }

    #[test]
    fn rejects_invalid_json() {
        let path = temp_project_path("bad");
        std::fs::write(&path, b"this is not json").unwrap();
        let err = load(&path).expect_err("非法 JSON 应报错");
        assert!(err.contains("格式错误"), "实际错误：{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_newer_version() {
        let path = temp_project_path("v99");
        std::fs::write(&path, br#"{"version": 99, "view": null, "layers": []}"#).unwrap();
        let err = load(&path).expect_err("过新版本应报错");
        assert!(err.contains("v99"), "实际错误：{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_reports_error() {
        let path = temp_project_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(load(&path).is_err());
    }

    // -----------------------------------------------------------------------
    // v4：图层属性表
    // -----------------------------------------------------------------------

    /// 属性表必须逐项往返：字段名、行内容、几何→属性行号映射。
    #[test]
    fn attribute_table_roundtrip() {
        let path = temp_project_path("attrs");
        let mut src = sample_layer();
        // sample_layer 是 2 点 + 1 线 + 1 面 ⇒ 恰好 4 行属性。
        src.fields = vec!["name".to_string(), "pop".to_string()];
        src.attrs = vec![
            vec!["A".to_string(), "100".to_string()],
            vec!["B".to_string(), "200".to_string()],
            // 缺失字段在导入路径里就是空串，往返也应保持空串而不是 None。
            vec!["C".to_string(), String::new()],
            vec!["D".to_string(), "400".to_string()],
        ];
        src.point_rows = vec![0, 1];
        src.line_rows = vec![2];
        src.ring_rows = vec![3];

        let nodes = vec![LayerNode::Layer(src.clone())];
        save(&path, &nodes, None, None).expect("保存应成功");
        let data = load(&path).expect("加载应成功");
        let _ = std::fs::remove_file(&path);

        let l = data.nodes[0].layer().expect("根节点应为图层");
        assert_eq!(l.fields, src.fields, "字段名应逐项往返");
        assert_eq!(l.attrs, src.attrs, "属性行应逐项往返");
        assert_eq!(l.point_rows, src.point_rows, "点→行号映射应往返");
        assert_eq!(l.line_rows, src.line_rows, "线→行号映射应往返");
        assert_eq!(l.ring_rows, src.ring_rows, "面→行号映射应往返");
        // 几何照常重建，属性表的加入不应影响几何路径。
        assert_eq!(l.points, src.points);
        assert_eq!(l.lines, src.lines);
        assert_eq!(l.outer_rings, src.outer_rings);
    }

    /// v3 旧文件（完全没有新字段）仍能打开：新字段反序列化为空表，几何/样式照常。
    #[test]
    fn legacy_v3_project_without_attrs_still_loads() {
        let path = temp_project_path("v3noattrs");
        // 手写 v3 的 JSON 字面量：没有 fields/attrs/*_rows 任何键。
        let json = r#"{
            "version": 3,
            "view": null,
            "nodes": [
                {"kind": "layer", "node": {
                    "name": "old",
                    "visible": true,
                    "style": {"fill":[74,144,226,255],"outline":[0,113,188,255],
                              "outline_width":1.5,"point_color":[227,92,44,255],"point_radius":3.0},
                    "points": [[116.0,39.0]], "lines": [], "polygons": []
                }}
            ]
        }"#;
        std::fs::write(&path, json).unwrap();
        let data = load(&path).expect("v3 旧工程应能打开");
        let _ = std::fs::remove_file(&path);

        let l = data.nodes[0].layer().expect("根节点应为图层");
        assert_eq!(l.name, "old");
        assert_eq!(l.points, vec![[116.0, 39.0]], "几何照常恢复");
        assert!(l.visible, "样式/可见性照常恢复");
        assert!(l.fields.is_empty(), "缺字段 ⇒ 字段名为空");
        assert!(l.attrs.is_empty(), "缺字段 ⇒ 属性表为空");
        assert!(l.point_rows.is_empty());
        assert!(l.line_rows.is_empty());
        assert!(l.ring_rows.is_empty());
    }

    /// v4 空属性表往返：不 panic，且不会把空表写成 `[[]]` 之类的畸形。
    ///
    /// 同时覆盖两种磁盘形态：本应用写出的（省略空表键）与手写的（显式空数组）。
    #[test]
    fn v4_empty_attribute_table_roundtrip() {
        let path = temp_project_path("v4empty");
        // 完全无几何、无属性的图层：三张表都是真正的空表。
        let src = Layer::from_geometry("no-attrs".to_string(), vec![], vec![], vec![]);
        let nodes = vec![LayerNode::Layer(src)];
        save(&path, &nodes, None, None).expect("保存应成功");

        // 形态一：本应用写出的文件省略空表键（skip_serializing_if 去噪）。
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let node = &raw["nodes"][0]["node"];
        for key in ["fields", "attrs", "point_rows", "line_rows", "ring_rows"] {
            assert!(node.get(key).is_none(), "空属性表键 {key} 不应写入磁盘");
        }

        let data = load(&path).expect("加载应成功");
        let _ = std::fs::remove_file(&path);
        let l = data.nodes[0].layer().expect("根节点应为图层");
        assert!(l.fields.is_empty() && l.attrs.is_empty());
        assert!(l.point_rows.is_empty() && l.line_rows.is_empty() && l.ring_rows.is_empty());

        // 形态一补充：有几何但无属性字段的图层，"每要素一个空行"必须原样往返——
        // 也不能丢掉行数或变成 `[[]]` 之类的畸形。
        let path_geo = temp_project_path("v4geomnoattrs");
        let geo_src = sample_layer(); // 2 点 + 1 线 + 1 面 = 4 个要素
        save(&path_geo, &[LayerNode::Layer(geo_src)], None, None).expect("保存应成功");
        let geo = load(&path_geo).expect("加载应成功");
        let _ = std::fs::remove_file(&path_geo);
        let gl = geo.nodes[0].layer().expect("根节点应为图层");
        assert!(gl.fields.is_empty(), "无字段 ⇒ fields 为空");
        assert_eq!(gl.attrs.len(), 4, "4 个要素应有 4 行（空行也算一行）");
        assert!(
            gl.attrs.iter().all(|r| r.is_empty()),
            "每个要素的行都应是空行：{:?}",
            gl.attrs
        );
        assert_eq!(
            gl.attrs,
            vec![Vec::<String>::new(); 4],
            "不得把空行写成 `[[]]` 之类的畸形"
        );

        // 形态二：手写显式空数组的 v4 文件也必须正常加载（键存在但为空数组）。
        let path2 = temp_project_path("v4emptyexplicit");
        let json = r#"{
            "version": 4,
            "nodes": [
                {"kind": "layer", "node": {
                    "name": "empty-attrs",
                    "style": {"fill":[74,144,226,255],"outline":[0,113,188,255],
                              "outline_width":1.5,"point_color":[227,92,44,255],"point_radius":3.0},
                    "points": [], "lines": [], "polygons": [],
                    "fields": [], "attrs": [], "point_rows": [], "line_rows": [], "ring_rows": []
                }}
            ]
        }"#;
        std::fs::write(&path2, json).unwrap();
        let data2 = load(&path2).expect("显式空数组的 v4 文件应能打开");
        let _ = std::fs::remove_file(&path2);
        let l2 = data2.nodes[0].layer().unwrap();
        assert!(l2.fields.is_empty() && l2.attrs.is_empty());
        assert!(l2.point_rows.is_empty() && l2.line_rows.is_empty() && l2.ring_rows.is_empty());
    }

    /// 新写出的工程文件必须声明 v4。
    #[test]
    fn saved_file_declares_version_4() {
        let path = temp_project_path("version");
        save(&path, &[], None, None).expect("保存应成功");
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(raw["version"], serde_json::json!(4));
    }
}
