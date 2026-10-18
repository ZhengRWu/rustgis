//! GeoTIFF 地理参考与 GDAL 元数据解析。
//!
//! 只依赖 TIFF 标签快照，不读取像素：
//! - `ModelTransformationTag`（34264）→ 仿射（含旋转）；
//! - 否则 `ModelPixelScaleTag`（33550）+ `ModelTiepointTag`（33922）→ 正北朝上仿射；
//! - `GeoKeyDirectoryTag`（34735）的 3072 / 2048 键 → EPSG 代码；
//! - `GeoAsciiParamsTag`（34737）→ 坐标系名称兜底；
//! - `GDAL_METADATA`（42112）→ 波段描述与已有统计值。
//!
//! 缺失地理参考不是错误：返回 `transform: None` 与一条提示，由上层降级为像素坐标。

use tiff::decoder::ifd::Value;

use super::tiff_file::{tag_ascii, tag_f64_vec, tag_u16_vec};
use super::{Affine, CrsId, ProjectionInfo, ProjectionModel};
use crate::geo::parse_prj_wkt;

const TAG_MODEL_TRANSFORMATION: u16 = 34264;
const TAG_MODEL_PIXEL_SCALE: u16 = 33550;
const TAG_MODEL_TIEPOINT: u16 = 33922;
const TAG_GEO_KEY_DIRECTORY: u16 = 34735;
const TAG_GEO_ASCII_PARAMS: u16 = 34737;
const TAG_GDAL_METADATA: u16 = 42112;

/// 文件内已有的波段统计值（GDAL `STATISTICS_*` 元数据）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct StoredBandStats {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub std_dev: f64,
}

/// 解析结果。
pub(crate) struct GeoTiffInfo {
    pub transform: Option<Affine>,
    pub crs: ProjectionInfo,
    pub warnings: Vec<String>,
    /// 控制点数量；1 个 tiepoint 视为普通定位点，不计入 GCP。
    pub gcp_count: usize,
    /// 其余 GDAL 元数据（键值对）。
    pub metadata: Vec<(String, String)>,
    /// 每个波段的名称（按 sample 索引，缺省为空串）。
    pub descriptions: Vec<String>,
    /// 每个波段的已有统计值。
    pub stats: Vec<Option<StoredBandStats>>,
}

/// 解析 GeoTIFF / GDAL 相关标签。
pub(crate) fn parse(tags: &[(u16, Value)], samples: u16) -> GeoTiffInfo {
    let mut warnings = Vec::new();

    // ---- 仿射变换 ----
    let transform = model_transformation(tags).or_else(|| pixel_scale_tiepoint(tags));
    if transform.is_none() {
        warnings.push(
            "文件缺少仿射地理变换标签（ModelTransformation / ModelPixelScale + ModelTiepoint）"
                .to_string(),
        );
    } else if transform.is_some_and(|affine| !affine.is_north_up()) {
        warnings.push("影像含旋转项，显示窗口按外接矩形近似".to_string());
    }

    // ---- 坐标系 ----
    let crs = geo_keys_crs(tags)
        .or_else(|| ascii_params_crs(tags))
        .unwrap_or_else(|| {
            warnings.push("未能从 GeoTIFF 标签解析出坐标系（缺少 EPSG 代码或 WKT）".to_string());
            ProjectionInfo::none()
        });

    // ---- 控制点 ----
    let gcp_count = tag_f64_vec(tags, TAG_MODEL_TIEPOINT)
        .map(|values| values.len() / 6)
        .filter(|count| *count > 1)
        .unwrap_or(0);

    // ---- GDAL 元数据 ----
    let mut descriptions = vec![String::new(); samples as usize];
    let mut stats = vec![None; samples as usize];
    let mut metadata = Vec::new();
    let mut pending_stats: Vec<(usize, String, f64)> = Vec::new();
    if let Some(xml) = tag_ascii(tags, TAG_GDAL_METADATA) {
        for item in parse_gdal_items(&xml) {
            let sample = item.sample.unwrap_or(0) as usize;
            match item.name.to_ascii_uppercase().as_str() {
                "DESCRIPTION" => {
                    if let Some(slot) = descriptions.get_mut(sample) {
                        *slot = item.value;
                    }
                }
                "STATISTICS_MINIMUM" => push_stat(&mut pending_stats, sample, "min", &item.value),
                "STATISTICS_MAXIMUM" => push_stat(&mut pending_stats, sample, "max", &item.value),
                "STATISTICS_MEAN" => push_stat(&mut pending_stats, sample, "mean", &item.value),
                "STATISTICS_STDDEV" => push_stat(&mut pending_stats, sample, "std", &item.value),
                _ => metadata.push((item.name, item.value)),
            }
        }
    }
    for (sample, key, value) in pending_stats {
        let Some(entry) = stats.get_mut(sample) else {
            continue;
        };
        let stats_entry = entry.get_or_insert(StoredBandStats {
            min: f64::NAN,
            max: f64::NAN,
            mean: f64::NAN,
            std_dev: f64::NAN,
        });
        match key.as_str() {
            "min" => stats_entry.min = value,
            "max" => stats_entry.max = value,
            "mean" => stats_entry.mean = value,
            "std" => stats_entry.std_dev = value,
            _ => {}
        }
    }
    // 统计值必须 min/max 齐备才可用。
    for entry in &mut stats {
        if entry.is_some_and(|s| !(s.min.is_finite() && s.max.is_finite())) {
            *entry = None;
        }
    }

    GeoTiffInfo {
        transform,
        crs,
        warnings,
        gcp_count,
        metadata,
        descriptions,
        stats,
    }
}

/// `ModelTransformationTag`：4×4 矩阵，行优先。
fn model_transformation(tags: &[(u16, Value)]) -> Option<Affine> {
    let values = tag_f64_vec(tags, TAG_MODEL_TRANSFORMATION)?;
    if values.len() < 8 {
        return None;
    }
    // GeoTransform 顺序：[c, a, b, f, d, e]；矩阵 m[0..2] 为 x 方向、m[3..5] 为 y 方向。
    Some(Affine::new([
        values[3], values[0], values[1], values[7], values[4], values[5],
    ]))
}

/// `ModelPixelScaleTag` + `ModelTiepointTag`：正北朝上的仿射。
fn pixel_scale_tiepoint(tags: &[(u16, Value)]) -> Option<Affine> {
    let scale = tag_f64_vec(tags, TAG_MODEL_PIXEL_SCALE)?;
    let tiepoint = tag_f64_vec(tags, TAG_MODEL_TIEPOINT)?;
    if scale.len() < 2 || tiepoint.len() < 6 {
        return None;
    }
    let scale_x = scale[0];
    let scale_y = scale[1].abs();
    if !(scale_x.is_finite() && scale_y.is_finite()) || scale_x == 0.0 || scale_y == 0.0 {
        return None;
    }
    // tiepoint = (i, j, k, x, y, z)
    let (i, j, x, y) = (tiepoint[0], tiepoint[1], tiepoint[3], tiepoint[4]);
    Some(Affine::new([
        x - i * scale_x,
        scale_x,
        0.0,
        y + j * scale_y,
        0.0,
        -scale_y,
    ]))
}

/// 从 `GeoKeyDirectoryTag` 提取坐标系：优先投影坐标系（3072），其次地理坐标系（2048）。
fn geo_keys_crs(tags: &[(u16, Value)]) -> Option<ProjectionInfo> {
    let keys = tag_u16_vec(tags, TAG_GEO_KEY_DIRECTORY)?;
    if keys.len() < 4 {
        return None;
    }
    let key_count = usize::from(keys[3]);
    let mut projected = None;
    let mut geographic = None;
    for index in 0..key_count {
        let start = 4 + index * 4;
        let Some(chunk) = keys.get(start..start + 4) else {
            break;
        };
        let (key_id, location, count, value) = (chunk[0], chunk[1], chunk[2], chunk[3]);
        // 只处理「值直接内联在 value_offset 中」的短键。
        if location != 0 || count != 1 {
            continue;
        }
        match key_id {
            3072 => projected = Some(value),
            2048 => geographic = Some(value),
            _ => {}
        }
    }
    if let Some(code) = projected.filter(|code| (1..=32766).contains(code)) {
        return Some(ProjectionInfo {
            model: ProjectionModel::Projected,
            crs: CrsId::Epsg(u32::from(code)),
        });
    }
    if let Some(code) = geographic.filter(|code| (1..=32766).contains(code)) {
        return Some(ProjectionInfo {
            model: ProjectionModel::Geographic,
            crs: CrsId::Epsg(u32::from(code)),
        });
    }
    None
}

/// `GeoAsciiParamsTag`：WKT 文本 → 完整解析；否则取第一个 `|` 分隔的名称。
fn ascii_params_crs(tags: &[(u16, Value)]) -> Option<ProjectionInfo> {
    let text = tag_ascii(tags, TAG_GEO_ASCII_PARAMS)?;
    let text = text.trim_matches('\0').trim();
    if text.is_empty() {
        return None;
    }
    if text.contains("PROJCS[") || text.contains("GEOGCS[") {
        let projection = parse_prj_wkt(text);
        if projection.crs != CrsId::None {
            return Some(projection);
        }
    }
    let name = text.split('|').next().unwrap_or(text).trim();
    if name.is_empty() {
        return None;
    }
    Some(ProjectionInfo {
        model: ProjectionModel::Unknown,
        crs: CrsId::Name(name.to_string()),
    })
}

/// GDAL 元数据条目。
struct GdalItem {
    name: String,
    sample: Option<u32>,
    value: String,
}

/// 解析 GDAL `<GDALMetadata><Item name= sample=>value</Item>...</GDALMetadata>`。
fn parse_gdal_items(xml: &str) -> Vec<GdalItem> {
    let mut items = Vec::new();
    let mut rest = xml;
    while let Some(open) = rest.find("<Item") {
        let after = &rest[open + 5..];
        let Some(tag_end) = after.find('>') else {
            break;
        };
        let attributes = &after[..tag_end];
        let body_start = tag_end + 1;
        let Some(close) = after[body_start..].find("</Item>") else {
            break;
        };
        let body = &after[body_start..body_start + close];
        let name = attribute(attributes, "name").unwrap_or_default();
        let sample = attribute(attributes, "sample").and_then(|s| s.parse::<u32>().ok());
        items.push(GdalItem {
            name,
            sample,
            value: unescape_xml(body.trim()),
        });
        rest = &after[body_start + close + 7..];
    }
    items
}

/// 读取 XML 开始标签中的属性值（`name="x"` 或 `name='x'`）。
fn attribute(attributes: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let start = attributes.find(&needle)? + needle.len();
    let rest = attributes[start..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value = &rest[1..];
    let end = value.find(quote)?;
    Some(value[..end].to_string())
}

fn unescape_xml(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn push_stat(pending: &mut Vec<(usize, String, f64)>, sample: usize, key: &str, text: &str) {
    if let Ok(value) = text.trim().parse::<f64>() {
        pending.push((sample, key.to_string(), value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(code: u16, value: Value) -> (u16, Value) {
        (code, value)
    }

    #[test]
    fn pixel_scale_and_tiepoint_make_affine() {
        let tags = vec![
            tag(
                TAG_MODEL_PIXEL_SCALE,
                Value::List(vec![
                    Value::Double(0.5),
                    Value::Double(0.5),
                    Value::Double(0.0),
                ]),
            ),
            tag(
                TAG_MODEL_TIEPOINT,
                Value::List(vec![
                    Value::Double(0.0),
                    Value::Double(0.0),
                    Value::Double(0.0),
                    Value::Double(100.0),
                    Value::Double(200.0),
                    Value::Double(0.0),
                ]),
            ),
        ];
        let info = parse(&tags, 1);
        let affine = info.transform.expect("应解析出仿射变换");
        assert!((affine.a - 0.5).abs() < 1e-12);
        assert!((affine.e + 0.5).abs() < 1e-12);
        let world = affine.apply(10.0, 20.0);
        assert!((world[0] - 105.0).abs() < 1e-9);
        assert!((world[1] - 190.0).abs() < 1e-9);
        assert!(info.warnings.iter().all(|w| !w.contains("仿射")));
    }

    #[test]
    fn missing_georeference_reports_warning() {
        let info = parse(&[], 1);
        assert!(info.transform.is_none());
        assert!(info.warnings.iter().any(|w| w.contains("仿射")));
        assert!(info.warnings.iter().any(|w| w.contains("坐标系")));
        assert_eq!(info.gcp_count, 0);
    }

    #[test]
    fn geo_keys_extract_epsg() {
        // 头部：版本 1.1.0，1 个键；键 3072（ProjectedCSType）内联值 32650。
        let keys = vec![1, 1, 0, 1, 3072, 0, 1, 32650];
        let tags = vec![tag(
            TAG_GEO_KEY_DIRECTORY,
            Value::List(keys.into_iter().map(Value::Short).collect()),
        )];
        let info = parse(&tags, 1);
        assert_eq!(info.crs.model, ProjectionModel::Projected);
        assert_eq!(info.crs.crs, CrsId::Epsg(32650));
    }

    #[test]
    fn geographic_geo_key_falls_back() {
        let keys = vec![1, 1, 0, 1, 2048, 0, 1, 4326];
        let tags = vec![tag(
            TAG_GEO_KEY_DIRECTORY,
            Value::List(keys.into_iter().map(Value::Short).collect()),
        )];
        let info = parse(&tags, 1);
        assert_eq!(info.crs.model, ProjectionModel::Geographic);
        assert_eq!(info.crs.crs, CrsId::Epsg(4326));
    }

    #[test]
    fn gdal_metadata_descriptions_and_stats() {
        let xml = concat!(
            "<GDALMetadata>",
            "<Item name=\"DESCRIPTION\" sample=\"0\">Red &amp; NIR</Item>",
            "<Item name=\"DESCRIPTION\" sample=\"1\">Band_2</Item>",
            "<Item name=\"STATISTICS_MINIMUM\" sample=\"0\">0</Item>",
            "<Item name=\"STATISTICS_MAXIMUM\" sample=\"0\">255</Item>",
            "<Item name=\"STATISTICS_MEAN\" sample=\"0\">100.5</Item>",
            "<Item name=\"STATISTICS_STDDEV\" sample=\"0\">12.25</Item>",
            "<Item name=\"AREA_OR_POINT\">Area</Item>",
            "</GDALMetadata>",
        );
        let tags = vec![tag(TAG_GDAL_METADATA, Value::Ascii(xml.to_string()))];
        let info = parse(&tags, 2);
        assert_eq!(info.descriptions[0], "Red & NIR");
        assert_eq!(info.descriptions[1], "Band_2");
        let stats = info.stats[0].expect("应有统计值");
        assert!((stats.min - 0.0).abs() < 1e-12);
        assert!((stats.max - 255.0).abs() < 1e-12);
        assert!((stats.mean - 100.5).abs() < 1e-12);
        assert!((stats.std_dev - 12.25).abs() < 1e-12);
        assert!(info.stats[1].is_none());
        assert!(
            info.metadata
                .iter()
                .any(|(key, value)| key == "AREA_OR_POINT" && value == "Area")
        );
    }

    #[test]
    fn model_transformation_wins_over_scale() {
        let matrix: Vec<f64> = vec![
            2.0, 0.0, 0.0, 1000.0, 0.0, -2.0, 0.0, 2000.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];
        let tags = vec![
            tag(
                TAG_MODEL_TRANSFORMATION,
                Value::List(matrix.into_iter().map(Value::Double).collect()),
            ),
            tag(
                TAG_MODEL_PIXEL_SCALE,
                Value::List(vec![Value::Double(9.0), Value::Double(9.0)]),
            ),
        ];
        let info = parse(&tags, 1);
        let affine = info.transform.expect("应解析出仿射变换");
        assert!((affine.a - 2.0).abs() < 1e-12);
        assert!((affine.c - 1000.0).abs() < 1e-12);
        assert!((affine.f - 2000.0).abs() < 1e-12);
    }

    #[test]
    fn ascii_params_name_fallback() {
        let tags = vec![tag(
            TAG_GEO_ASCII_PARAMS,
            Value::Ascii("WGS 84 / UTM zone 50N|\0".to_string()),
        )];
        let info = parse(&tags, 1);
        assert_eq!(
            info.crs.crs,
            CrsId::Name("WGS 84 / UTM zone 50N".to_string())
        );
    }
}
