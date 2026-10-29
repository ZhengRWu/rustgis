//! 矢量图层落盘：ESRI Shapefile（`.shp` / `.shx` / `.dbf` / `.prj`）与 GeoJSON。
//!
//! 对标 QGIS「浏览器 → 新建 → ShapeFile…」（`QgsNewVectorLayerDialog` +
//! `QgsOgrProviderUtils::createEmptyDataSource`）：一个矢量数据集 = 几何文件 +
//! 属性表 + 坐标系 sidecar。本模块只做「把 [`Layer`] 写成一个数据集」这一步：
//!
//! - **Shapefile**：几何走 `shapefile::Writer`，属性走 `dbase`（Shapefile 的属性表
//!   就是 dBASE 表，字段的**类型 / 长度 / 精度**在建表时就固定，因此新建矢量对话框
//!   里定义的 [`FieldDef`] 必须一路带到这里）；坐标系写同名的 `.prj`（WKT1，
//!   与 ArcGIS / QGIS 读的同一格式）。三个文件由 `Writer::from_path` 一次创建，
//!   `.prj` 由本模块补写；
//! - **GeoJSON**：直接复用 [`crate::geo::buffer::write_geojson`]（缓冲区 / 叠加结果
//!   已在用），属性全是字符串，字段类型不体现在文件里。
//!
//! 只在能识别的坐标系下写 `.prj`：写不出正确的 WKT 比不写更糟（会让下游把 CGCS2000
//! 的平面坐标当成经纬度）。

use std::path::Path;

use crate::geo::{CrsId, Geometry2D, Layer, ProjectionInfo, ProjectionModel};

/// 新建矢量时可选的落盘格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorFormat {
    /// ESRI Shapefile（`.shp` + `.shx` + `.dbf` + `.prj`）。
    Shapefile,
    /// GeoJSON（`.geojson`，单文件）。
    GeoJson,
}

impl VectorFormat {
    /// 主文件扩展名（不含点）。
    pub fn extension(self) -> &'static str {
        match self {
            Self::Shapefile => "shp",
            Self::GeoJson => "geojson",
        }
    }

    /// 系统保存对话框的扩展名过滤（不含点）。
    pub fn filters(self) -> &'static [&'static str] {
        match self {
            Self::Shapefile => &["shp"],
            Self::GeoJson => &["geojson", "json"],
        }
    }
}

/// 新建矢量时定义的属性字段（长度 / 精度只对 dBASE 有意义）。
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDef {
    pub name: String,
    pub kind: FieldKind,
    /// 文本 1..=255，整数 1..=10，实数 1..=20（与 QGIS 的校验范围一致）。
    pub length: u8,
    /// 小数位数 0..=15（仅实数）。
    pub precision: u8,
}

impl FieldDef {
    /// 「id / 整数 / 10」——QGIS 新建 Shapefile 时预置的默认字段。
    pub fn default_id() -> Self {
        Self {
            name: "id".to_string(),
            kind: FieldKind::Integer,
            length: 10,
            precision: 0,
        }
    }
}

/// 字段类型（比 QGIS 少了 GDAL 3.9 才有 bool，其余一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// 文本（dBASE Character）。
    Text,
    /// 整数（dBASE Integer，32 位）。
    Integer,
    /// 实数（dBASE Float）。
    Real,
    /// 日期（dBASE Date，`YYYY-MM-DD`）。
    Date,
}

impl FieldKind {
    /// 该类型下「长度 / 精度」输入是否生效（QGIS 里只有文本和实数让填）。
    pub fn editable(self) -> (bool, bool) {
        match self {
            Self::Text => (true, false),
            Self::Integer => (true, false),
            Self::Real => (true, true),
            Self::Date => (false, false),
        }
    }

    /// 切换类型时长度 / 精度的默认值（与 QGIS `mTypeBox_currentIndexChanged` 一致）。
    pub fn default_metrics(self) -> (u8, u8) {
        match self {
            Self::Text => (80, 0),
            Self::Integer => (10, 0),
            Self::Real => (20, 6),
            Self::Date => (0, 0),
        }
    }
}

/// 把图层按 `fields` 的字段定义写成 `path` 指定的数据集。
///
/// `geom` 决定 Shapefile 的几何类型（Shapefile 一个文件只能有一种几何类型，
/// QGIS 建完空数据源后同样只接受该类型的要素），GeoJSON 不受此限制。
pub fn write_layer(
    layer: &Layer,
    fields: &[FieldDef],
    path: &Path,
    format: VectorFormat,
    geom: GeometryKind,
) -> Result<(), String> {
    match format {
        VectorFormat::GeoJson => crate::geo::buffer::write_geojson(layer, path),
        VectorFormat::Shapefile => write_shapefile(layer, fields, path, geom),
    }
}

/// 新建矢量对话框里的几何类型（同时决定 Shapefile 写出的 shape type）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryKind {
    Point,
    Line,
    Polygon,
}

impl GeometryKind {
    /// 成形所需的最少节点数（数字化时不足这个数不该结束要素）。
    pub fn min_vertices(self) -> usize {
        match self {
            Self::Point => 1,
            Self::Line => 2,
            Self::Polygon => 3,
        }
    }
}

fn write_shapefile(
    layer: &Layer,
    fields: &[FieldDef],
    path: &Path,
    geom: GeometryKind,
) -> Result<(), String> {
    let mut builder = dbase::TableWriterBuilder::new();
    for field in fields {
        let name: dbase::FieldName = field.name.as_str().try_into().map_err(|_| {
            format!(
                "字段名“{}”超过 dBASE 的 10 字节上限（Shapefile 限制，请改短或改用 GeoJSON）",
                field.name
            )
        })?;
        builder = match field.kind {
            FieldKind::Text => builder.add_character_field(name, field.length.clamp(1, 255)),
            FieldKind::Integer => builder.add_integer_field(name),
            FieldKind::Real => {
                builder.add_float_field(name, field.length.clamp(1, 20), field.precision.min(15))
            }
            FieldKind::Date => builder.add_date_field(name),
        };
    }
    let mut writer = shapefile::Writer::from_path(path, builder)
        .map_err(|e| format!("创建 Shapefile 失败：{e}"))?;

    // 只写与几何类型匹配的要素：.shp 头部只有一个 shape type，混入其它类型会让
    // 文件在 QGIS / ArcGIS 里打不开。
    match geom {
        GeometryKind::Polygon => {
            let mut cursor = 0usize;
            for (i, outer) in layer.outer_rings.iter().enumerate() {
                let count = layer.hole_counts.get(i).copied().unwrap_or(0);
                let end = (cursor + count).min(layer.hole_rings.len());
                let holes = &layer.hole_rings[cursor.min(end)..end];
                cursor += count;
                let mut rings = vec![shapefile::PolygonRing::Outer(to_points(outer))];
                rings.extend(
                    holes
                        .iter()
                        .map(|h| shapefile::PolygonRing::Inner(to_points(h))),
                );
                let shape = shapefile::Polygon::with_rings(rings);
                let record = record_at(layer, fields, layer.ring_rows.get(i).copied());
                writer
                    .write_shape_and_record(&shape, &record)
                    .map_err(|e| format!("写入要素失败：{e}"))?;
            }
        }
        GeometryKind::Line => {
            for (i, line) in layer.lines.iter().enumerate() {
                let shape = shapefile::Polyline::new(to_points(line));
                let record = record_at(layer, fields, layer.line_rows.get(i).copied());
                writer
                    .write_shape_and_record(&shape, &record)
                    .map_err(|e| format!("写入要素失败：{e}"))?;
            }
        }
        GeometryKind::Point => {
            for (i, p) in layer.points.iter().enumerate() {
                let shape = shapefile::Point::new(p[0], p[1]);
                let record = record_at(layer, fields, layer.point_rows.get(i).copied());
                writer
                    .write_shape_and_record(&shape, &record)
                    .map_err(|e| format!("写入要素失败：{e}"))?;
            }
        }
    }
    drop(writer);

    // 坐标系 sidecar：与 QGIS / ArcGIS 的 `.prj` 同一格式（WKT1）。
    if let Some(wkt) = prj_wkt(&layer.projection) {
        let prj = path.with_extension("prj");
        if let Err(e) = std::fs::write(&prj, wkt) {
            return Err(format!("写入 {} 失败：{e}", prj.display()));
        }
    }
    Ok(())
}

fn to_points(ring: &[crate::geo::Coord2D]) -> Vec<shapefile::Point> {
    ring.iter().map(|p| shapefile::Point::new(p[0], p[1])).collect()
}

/// 属性行 → dBASE 记录（按字段类型把字符串解析回对应的 dBASE 值）。
fn record_at(layer: &Layer, fields: &[FieldDef], row: Option<usize>) -> dbase::Record {
    let mut record = dbase::Record::default();
    let Some(values) = row.and_then(|r| layer.attrs.get(r)) else {
        return record;
    };
    for (field, value) in fields.iter().zip(values) {
        let value = value.trim();
        let converted = match field.kind {
            FieldKind::Text => {
                dbase::FieldValue::Character((!value.is_empty()).then(|| value.to_string()))
            }
            FieldKind::Integer => dbase::FieldValue::Integer(value.parse::<i32>().unwrap_or(0)),
            FieldKind::Real => {
                dbase::FieldValue::Float(value.parse::<f32>().ok().filter(|v| v.is_finite()))
            }
            FieldKind::Date => dbase::FieldValue::Date(parse_date(value)),
        };
        record.insert(field.name.clone(), converted);
    }
    record
}

/// `YYYY-MM-DD` → dBASE Date（`Date::new(日, 月, 年)`，越界返回 `Err`）。
fn parse_date(text: &str) -> Option<dbase::Date> {
    let mut parts = text.split('-');
    let year = parts.next()?.parse::<u16>().ok()?;
    let month = parts.next()?.parse::<u8>().ok()?;
    let day = parts.next()?.parse::<u8>().ok()?;
    dbase::Date::new(u32::from(day), u32::from(month), u32::from(year)).ok()
}

/// 坐标系 → `.prj` 内容（WKT1）。
///
/// 只覆盖本仓库能真正识别、且能写出标准 WKT 的几种；识别不了就返回 `None`
/// （宁可不写 `.prj`，也不写一个骗下游的坐标系）。
pub fn prj_wkt(projection: &ProjectionInfo) -> Option<String> {
    match (&projection.crs, projection.model) {
        (CrsId::Epsg(4326), _) | (CrsId::None, ProjectionModel::Geographic) => Some(
            "GEOGCS[\"GCS_WGS_1984\",DATUM[\"D_WGS_1984\",SPHEROID[\"WGS_1984\",6378137.0,298.257223563]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]]"
                .to_string(),
        ),
        (CrsId::Epsg(4490), _) => Some(
            "GEOGCS[\"GCS_China_Geodetic_Coordinate_System_2000\",DATUM[\"D_China_2000\",SPHEROID[\"CGCS2000\",6378137.0,298.257222101]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]]"
                .to_string(),
        ),
        (CrsId::Epsg(3857), _) => Some(
            "PROJCS[\"WGS_1984_Web_Mercator_Auxiliary_Sphere\",GEOGCS[\"GCS_WGS_1984\",DATUM[\"D_WGS_1984\",SPHEROID[\"WGS_1984\",6378137.0,298.257223563]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]],PROJECTION[\"Mercator_Auxiliary_Sphere\"],PARAMETER[\"False_Easting\",0.0],PARAMETER[\"False_Northing\",0.0],PARAMETER[\"Central_Meridian\",0.0],PARAMETER[\"Standard_Parallel_1\",0.0],PARAMETER[\"Auxiliary_Sphere_Type\",0.0],UNIT[\"Meter\",1.0]]"
                .to_string(),
        ),
        _ => None,
    }
}

/// 把几何塞进图层（数字化路径）：属性补一行、包围盒与三角片增量更新。
///
/// 与导入路径（[`Layer::from_parts`]）的区别是**增量**：导入是一次性批量三角剖分，
/// 这里每画完一个要素剖一次（一个要素只有几十个顶点，开销可忽略）。
pub fn push_geometry(layer: &mut Layer, geometry: Geometry2D, attrs: Vec<String>) {
    let row = layer.attrs.len();
    let mut values = attrs;
    values.resize(layer.fields.len(), String::new());
    layer.attrs.push(values);
    match geometry {
        Geometry2D::Point(p) => {
            layer.bounds.extend_point(p);
            layer.points.push(p);
            layer.point_rows.push(row);
        }
        Geometry2D::Line(pts) => {
            for p in &pts {
                layer.bounds.extend_point(*p);
            }
            layer.lines.push(pts);
            layer.line_rows.push(row);
        }
        Geometry2D::Polygon { outer, holes } => {
            for p in outer.iter().chain(holes.iter().flatten()) {
                layer.bounds.extend_point(*p);
            }
            // 剖分要求环是**闭合**的（少于 4 个点会被当成退化输入丢掉，见
            // `tri::triangulate_one`）；导入路径的 GeoJSON / Shapefile 环本来就
            // 闭合，数字化画出来的只有互不重复的节点，这里补上收尾点再剖分。
            let mut closed = outer.clone();
            if let (Some(first), Some(last)) = (closed.first().copied(), closed.last().copied())
                && first != last
            {
                closed.push(first);
            }
            // 自相交环（数字化时路径交叉）先拆成简单环——对标 QGIS 对无效面的
            // 偶奇渲染：蝴蝶结填成两瓣，而不是把交叉面积糊成一整块（earcut 对
            // 自相交输入会剖出错误的三角片）。拆出的每一瓣都挂在**同一个要素**
            // 上：`ring_rows` 让各瓣共用一行属性，Shapefile / GeoJSON 写出器
            // 也按 `ring_rows[i]` 取属性，一个要素自然变成多个多边形记录。
            let parts = crate::geo::tri::polygon_parts(&closed, &holes);
            let (outers, hole_sets): (Vec<_>, Vec<_>) = parts
                .iter()
                .map(|(o, h)| (o.clone(), h.clone()))
                .unzip();
            layer
                .triangles
                .extend(crate::geo::tri::triangulate_polygons(&outers, &hole_sets));
            for (part_outer, part_holes) in parts {
                layer.hole_counts.push(part_holes.len());
                layer.hole_rings.extend(part_holes);
                layer.outer_rings.push(part_outer);
                layer.ring_rows.push(row);
            }
        }
    }
    layer.feature_count += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::{FeatureData, Layer, ProjectionInfo};

    fn layer_with(features: Vec<FeatureData>) -> Layer {
        Layer::from_features("t".to_string(), features)
    }

    fn temp_path(tag: &str, ext: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rustgis_newvector_{tag}.{ext}"));
        p
    }

    /// 写 Shapefile → 用生产代码的导入路径读回来 → 几何与属性必须一致。
    #[test]
    fn shapefile_roundtrip_keeps_geometry_and_attributes() {
        let path = temp_path("roundtrip", "shp");
        let layer = layer_with(vec![
            FeatureData {
                geometry: Geometry2D::Polygon {
                    outer: vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]],
                    holes: vec![],
                },
                attrs: vec![
                    ("name".to_string(), "地块A".to_string()),
                    ("id".to_string(), "7".to_string()),
                    ("area".to_string(), "12.5".to_string()),
                ],
            },
            FeatureData {
                geometry: Geometry2D::Polygon {
                    outer: vec![[20.0, 0.0], [30.0, 0.0], [30.0, 10.0], [20.0, 10.0]],
                    holes: vec![],
                },
                attrs: vec![
                    ("name".to_string(), "地块B".to_string()),
                    ("id".to_string(), "8".to_string()),
                    ("area".to_string(), "99.5".to_string()),
                ],
            },
        ]);
        let fields = vec![
            FieldDef {
                name: "name".to_string(),
                kind: FieldKind::Text,
                length: 20,
                precision: 0,
            },
            FieldDef {
                name: "id".to_string(),
                kind: FieldKind::Integer,
                length: 10,
                precision: 0,
            },
            FieldDef {
                name: "area".to_string(),
                kind: FieldKind::Real,
                length: 20,
                precision: 6,
            },
        ];
        let mut out = layer.clone();
        out.projection = ProjectionInfo::wgs84();
        write_layer(&out, &fields, &path, VectorFormat::Shapefile, GeometryKind::Polygon)
            .expect("写 Shapefile");

        assert!(path.with_extension("prj").exists(), "应写出 .prj");
        let back = crate::geo::load_file(&path).expect("应能读回");
        assert_eq!(back.feature_count, 2);
        assert_eq!(back.outer_rings.len(), 2);
        // 属性：列序按 dBASE 字段顺序，值按类型还原成字符串。
        assert!(back.fields.iter().any(|f| f == "name"));
        let name_col = back.fields.iter().position(|f| f == "name").unwrap();
        let names: Vec<&str> = back.attrs.iter().map(|r| r[name_col].as_str()).collect();
        assert!(names.contains(&"地块A"), "{names:?}");
        let id_col = back.fields.iter().position(|f| f == "id").unwrap();
        assert!(back.attrs.iter().any(|r| r[id_col] == "7"));

        for ext in ["shp", "shx", "dbf", "prj"] {
            let _ = std::fs::remove_file(path.with_extension(ext));
        }
    }

    /// 数字化增量写几何：属性行数、三角片数与包围盒都要跟着长。
    #[test]
    fn push_geometry_grows_layer() {
        let mut layer = Layer::default();
        layer.fields = vec!["name".to_string()];
        push_geometry(
            &mut layer,
            Geometry2D::Polygon {
                outer: vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]],
                holes: vec![],
            },
            vec!["A".to_string()],
        );
        assert_eq!(layer.feature_count, 1);
        assert_eq!(layer.triangles.len(), 2, "四边形应剖成 2 个三角");
        assert_eq!(layer.attrs, vec![vec!["A".to_string()]]);
        assert_eq!(layer.bounds.min, [0.0, 0.0]);
        assert_eq!(layer.bounds.max, [4.0, 4.0]);
        // 属性值少于字段数时要补空串，多于时截断——属性表按列存取不能越界。
        push_geometry(&mut layer, Geometry2D::Point([9.0, 9.0]), vec![]);
        assert_eq!(layer.attrs[1], vec![String::new()]);
    }

    /// 自相交环（数字化路径交叉）应拆成两瓣简单环，仍算**一个**要素：
    /// 各瓣共用一行属性（`ring_rows`），三角片只覆盖有效面积（两瓣之和）。
    #[test]
    fn push_geometry_splits_self_intersecting_ring() {
        let mut layer = Layer::default();
        layer.fields = vec!["name".to_string()];
        push_geometry(
            &mut layer,
            Geometry2D::Polygon {
                outer: vec![[0.0, 0.0], [10.0, 10.0], [10.0, 0.0], [0.0, 10.0]],
                holes: vec![],
            },
            vec!["蝴蝶结".to_string()],
        );
        // 一个要素：一行属性，两个外环都指向它。
        assert_eq!(layer.feature_count, 1);
        assert_eq!(layer.attrs.len(), 1);
        assert_eq!(layer.outer_rings.len(), 2, "蝴蝶结应拆成两瓣");
        assert_eq!(layer.ring_rows, vec![0, 0], "两瓣共用一行属性");
        assert_eq!(layer.hole_counts, vec![0, 0]);
        // 三角片总面积 = 两瓣之和（50），不含交叉糊成的面积。
        let area: f64 = layer
            .triangles
            .iter()
            .map(|t| {
                ((t[2] - t[0]) * (t[5] - t[1]) - (t[4] - t[0]) * (t[3] - t[1])).abs() / 2.0
            })
            .sum();
        assert!((area - 50.0).abs() < 1e-9, "area = {area}");
    }
}
