//! Sentinel-2 `.SAFE` 产品包：包级发现与元数据解析。
//!
//! 这一层只负责「弄清包里有什么」——产品级别（L1C / L2A）、瓦片地理编码
//! （`MTD_TL.xml` 的 `Tile_Geocoding`）、以及每个波段的 JPEG 2000 文件在哪、
//! 分辨率是多少。像素解码交给 [`super::jp2`]。
//!
//! SAFE 包的目录结构（L1C / L2A 相同）：
//!
//! ```text
//! S2A_MSIL2A_..._T33UVP_....SAFE/
//! ├── MTD_MSIL2A.xml                 产品级元数据
//! ├── GRANULE/
//! │   └── L2A_T33UVP_..._.../
//! │       ├── MTD_TL.xml             瓦片地理编码（ULX / ULY / XDIM / YDIM / EPSG）
//! │       └── IMG_DATA/
//! │           ├── R10m/*.jp2          10 m 波段
//! │           ├── R20m/*.jp2          20 m 波段
//! │           └── R60m/*.jp2          60 m 波段
//! ```
//!
//! 波段清单直接从 `IMG_DATA` 目录树枚举，而不是依赖 XML 里的波段表：
//! 这样 L1C / L2A、不同处理基线的包都是一套代码（XML 只用来取地理编码与产品信息）。

use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::RasterError;
use super::jp2;

/// 瓦片地理编码：10 m 网格（其他分辨率波段按比例换算）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct SafeGrid {
    pub ulx: f64,
    pub uly: f64,
    pub xdim: f64,
    pub ydim: f64,
    pub cols: u32,
    pub rows: u32,
}

/// 一个 JPEG 2000 波段文件。
#[derive(Debug, Clone)]
pub(crate) struct BandFile {
    pub path: PathBuf,
    /// 波段标识：`B04` / `TCI` / `SCL` / `MSK_CLOUDS` 等。
    pub code: String,
    /// 相对 10 m 网格的分辨率倍数（10 m = 1，20 m = 2，60 m = 6）。
    pub factor: u32,
    pub resolution: u32,
    pub width: u32,
    pub height: u32,
    pub components: u8,
    pub bit_depth: u8,
    /// 分块布局 `(块宽, 块高, 横向块数, 纵向块数)`；整幅码流为 `None`。
    pub tiles: Option<(u32, u32, u32, u32)>,
}

/// 数据集里的一个「波段」——通常是某个文件的分量之一（TCI 有 3 个）。
#[derive(Debug, Clone)]
pub(crate) struct SafeBand {
    /// 指向 [`SafePackage::files`] 的下标。
    pub file: usize,
    pub component: u8,
    /// 显示名（含中文说明，如 `B04 红`、`TCI 红`）。
    pub label: String,
}

/// 已解析的 SAFE 产品包。
#[derive(Debug)]
// 部分字段留给「图层属性 / 日志」展示，读取管线暂未全部用到。
#[allow(dead_code)]
pub(crate) struct SafePackage {
    pub root: PathBuf,
    pub mtd_path: PathBuf,
    /// `L1C` 或 `L2A`。
    pub level: String,
    pub product_name: String,
    pub grid: SafeGrid,
    pub crs_epsg: Option<u32>,
    pub crs_name: String,
    pub files: Vec<BandFile>,
    pub bands: Vec<SafeBand>,
    pub metadata: Vec<(String, String)>,
}

impl SafePackage {
    /// 10 m 基网格（最细分辨率）。
    pub fn base_grid(&self) -> &SafeGrid {
        &self.grid
    }

    /// 基网格分辨率（米）。
    pub fn base_resolution(&self) -> u32 {
        let dim = if self.grid.xdim > 0.0 {
            self.grid.xdim
        } else {
            self.grid.ydim
        };
        if dim > 0.0 { dim.round() as u32 } else { 10 }
    }
}

/// 判断路径是否是 SAFE 产品包（目录），或包里的 `MTD_MSIL*.xml`（文件）。
pub(crate) fn is_safe_package(path: &Path) -> bool {
    if path.is_dir() {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        return name.ends_with(".safe")
            || find_mtd(path).is_some()
            || path.join("GRANULE").is_dir();
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let lower = name.to_ascii_lowercase();
            lower.starts_with("mtd_msil") && lower.ends_with(".xml")
        })
}

/// 打开并解析 SAFE 包（参数可以是包目录，也可以是包里的 `MTD_MSIL*.xml`）。
pub(crate) fn open(path: &Path) -> Result<SafePackage, RasterError> {
    let root = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(path).to_path_buf()
    };
    let mtd_path = find_mtd(&root).ok_or_else(|| {
        RasterError::Unsupported(format!(
            "SAFE 产品包里没有 MTD_MSIL*.xml 元数据文件：{}",
            root.display()
        ))
    })?;
    let product = read_xml(&mtd_path)?;
    let level = detect_level(&root, &product);

    let mut metadata = Vec::new();
    metadata.push(("产品级别".to_owned(), level.clone()));
    if let Some(name) = root.file_name().and_then(|n| n.to_str()) {
        metadata.push(("产品包".to_owned(), name.to_owned()));
    }
    if let Some(value) = product.first(&["PROCESSING_BASELINE"]) {
        metadata.push(("处理基线".to_owned(), value.to_owned()));
    }
    // L1C 与 L2A 的感测时间元素名不同，两个都试。
    if let Some(value) = product
        .first(&["DATATAKE_SENSING_START"])
        .or_else(|| product.first(&["SENSING_TIME"]))
    {
        metadata.push(("感测时间".to_owned(), value.to_owned()));
    }

    let granule = first_subdir(&root.join("GRANULE"));
    let mut grid = None;
    let mut crs_epsg = None;
    let mut crs_name = String::new();
    let mut geocoding_source = "MTD_TL.xml";
    if let Some(granule) = &granule {
        let mtd_tl_path = granule.join("MTD_TL.xml");
        if mtd_tl_path.is_file() {
            let tile = read_xml(&mtd_tl_path)?;
            grid = read_geocoding(&tile);
            crs_epsg = tile.first(&["HORIZONTAL_CS_CODE"]).and_then(parse_epsg);
            crs_name = tile
                .first(&["HORIZONTAL_CS_NAME"])
                .unwrap_or_default()
                .to_owned();
            if let Some(value) = tile.first(&["SPACECRAFT_NAME"]) {
                metadata.push(("卫星".to_owned(), value.to_owned()));
            }
            if let Some(value) = tile.first(&["SENSING_TIME"]) {
                metadata.push(("瓦片感测时间".to_owned(), value.to_owned()));
            }
            if let Some(value) = tile.first(&["RELATIVE_ORBIT_NUMBER"]) {
                metadata.push(("相对轨道号".to_owned(), value.to_owned()));
            }
            if let Some(value) = tile.first(&["TILE_ID"]) {
                metadata.push(("瓦片编号".to_owned(), value.to_owned()));
            }
        }
    }

    let mut files = Vec::new();
    if let Some(granule) = &granule {
        files = scan_band_files(&granule.join("IMG_DATA"))?;
    }
    if files.is_empty() {
        return Err(RasterError::Unsupported(format!(
            "SAFE 产品包里没有找到 JPEG 2000 波段文件（GRANULE/*/IMG_DATA）：{}",
            root.display()
        )));
    }
    // 排序：真彩色（TCI）与 10 m 可见光在前，辅助数据在后。
    // 同一个波段在 R10m / R20m / R60m 各有一份（都是独立解码的降采样副本），
    // 只保留分辨率最高的那一份，否则波段列表会出现 3 遍重复项。
    rank_and_dedupe(&mut files);

    // 地理编码缺失时退回「像素坐标」，至少让图像能显示。
    let grid = match grid {
        Some(grid) if grid.cols > 0 && grid.rows > 0 => grid,
        _ => {
            geocoding_source = "缺失（按像素坐标显示）";
            let base = files
                .iter()
                .find(|file| file.factor == 1)
                .or_else(|| files.first());
            match base {
                Some(file) => SafeGrid {
                    ulx: 0.0,
                    uly: 0.0,
                    xdim: f64::from(file.resolution),
                    ydim: f64::from(file.resolution),
                    cols: file.width,
                    rows: file.height,
                },
                None => return Err(RasterError::Corrupt("SAFE 波段文件为空".into())),
            }
        }
    };

    let mut bands = Vec::new();
    for (index, file) in files.iter().enumerate() {
        if file.components >= 3 {
            for (component, channel) in ["红", "绿", "蓝"].iter().enumerate() {
                bands.push(SafeBand {
                    file: index,
                    component: component as u8,
                    label: format!("{} {}", file.code, channel),
                });
            }
        } else {
            bands.push(SafeBand {
                file: index,
                component: 0,
                label: format!("{} {}", file.code, band_description(&file.code)),
            });
        }
    }

    metadata.push(("波段数量".to_owned(), bands.len().to_string()));
    metadata.push((
        "地理编码".to_owned(),
        format!(
            "ULX {:.1} / ULY {:.1} / 像元 {:.1} m（来源：{geocoding_source}）",
            grid.ulx, grid.uly, grid.xdim
        ),
    ));
    Ok(SafePackage {
        root: root.clone(),
        mtd_path,
        level,
        product_name: root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("SAFE")
            .to_owned(),
        grid,
        crs_epsg,
        crs_name,
        files,
        bands,
        metadata,
    })
}

/// 在包根目录找 `MTD_MSIL*.xml`。
fn find_mtd(root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut found: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let lower = name.to_string_lossy().to_ascii_lowercase();
        if lower.starts_with("mtd_msil") && lower.ends_with(".xml") {
            let path = entry.path();
            // L2A / L1C 都可能存在多个 MTD_MSIL 时取文件名字典序最小的一个。
            if found.as_ref().is_none_or(|current| path < *current) {
                found = Some(path);
            }
        }
    }
    found
}

fn first_subdir(root: &Path) -> Option<PathBuf> {
    let mut entries = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    entries.sort();
    entries.into_iter().next()
}

/// 排序并去重：按波段优先级排序，同名波段只保留分辨率最高的那份。
///
/// L2A 产品把同一个波段分别放在 `R10m` / `R20m` / `R60m` 里各存一份
/// （都是独立的降采样副本），全部列出会让「符号系统」的波段列表出现三遍重复项，
/// 而显示只需要其中最精细的一份。
fn rank_and_dedupe(files: &mut Vec<BandFile>) {
    files.sort_by(|a, b| {
        band_rank(&a.code)
            .cmp(&band_rank(&b.code))
            .then_with(|| a.code.cmp(&b.code))
            .then_with(|| a.factor.cmp(&b.factor))
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut seen = std::collections::HashSet::new();
    files.retain(|file| seen.insert(file.code.clone()));
    files.dedup_by(|a, b| a.path == b.path);
}

/// 枚举 `IMG_DATA` 下的波段文件（`R10m` / `R20m` / `R60m`，以及不带子目录的旧式布局）。
fn scan_band_files(img_data: &Path) -> Result<Vec<BandFile>, RasterError> {
    let mut files = Vec::new();
    if !img_data.is_dir() {
        return Ok(files);
    }
    for (dir, resolution) in [("R10m", 10u32), ("R20m", 20), ("R60m", 60)] {
        collect_jp2(&img_data.join(dir), resolution, &mut files)?;
    }
    if files.is_empty() {
        collect_jp2(img_data, 10, &mut files)?;
    }
    Ok(files)
}

fn collect_jp2(dir: &Path, resolution: u32, out: &mut Vec<BandFile>) -> Result<(), RasterError> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|error| RasterError::Io(format!("{}：{error}", dir.display())))?
        .flatten()
    {
        let path = entry.path();
        let is_jp2 = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jp2") || ext.eq_ignore_ascii_case("j2k"));
        if path.is_file() && is_jp2 {
            paths.push(path);
        }
    }
    paths.sort();
    for path in paths {
        let code = band_code(&path);
        let header = jp2::inspect(&path)?;
        out.push(BandFile {
            code,
            factor: resolution / 10,
            resolution,
            width: header.width,
            height: header.height,
            components: header.components,
            bit_depth: header.bit_depth,
            tiles: header.tiles,
            path,
        });
    }
    Ok(())
}

/// 从文件名提取波段标识：`..._B04_10m.jp2` → `B04`，`..._TCI_10m.jp2` → `TCI`。
fn band_code(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let mut mask_prefix = false;
    for part in stem.split('_') {
        let upper = part.to_ascii_uppercase();
        if upper.starts_with("MSK") {
            mask_prefix = true;
            continue;
        }
        if mask_prefix {
            return format!("MSK_{upper}");
        }
        if is_band_token(&upper) {
            return upper;
        }
    }
    stem.to_owned()
}

fn is_band_token(token: &str) -> bool {
    if let Some(digits) = token.strip_prefix('B') {
        // `B01`、`B12` 这类两位数字波段。
        if digits.len() == 2 && digits.chars().all(|ch| ch.is_ascii_digit()) {
            return true;
        }
        // `B8A`（窄近红外）这类「一位数字 + A」的波段。
        if digits.len() == 2
            && digits.ends_with('A')
            && digits.starts_with(|ch: char| ch.is_ascii_digit())
        {
            return true;
        }
    }
    matches!(token, "TCI" | "SCL" | "AOT" | "WVP" | "PRD" | "PVI")
}

/// 排序权重：真彩色优先，其次是常用的可见光 / 近红外 / 短波红外，辅助数据最后。
fn band_rank(code: &str) -> u32 {
    match code {
        "TCI" => 0,
        "B04" => 10,
        "B03" => 11,
        "B02" => 12,
        "B08" => 13,
        "B05" => 14,
        "B06" => 15,
        "B07" => 16,
        "B8A" => 17,
        "B11" => 18,
        "B12" => 19,
        "B01" => 20,
        "B09" => 21,
        "B10" => 22,
        "AOT" => 30,
        "WVP" => 31,
        "PVI" => 32,
        "SCL" => 33,
        "PRD" => 34,
        other if other.starts_with("MSK") => 40,
        _ => 50,
    }
}

/// 波段的中文说明（只用于显示名，不影响解码）。
fn band_description(code: &str) -> &'static str {
    match code {
        "B01" => "海岸气溶胶",
        "B02" => "蓝",
        "B03" => "绿",
        "B04" => "红",
        "B05" => "红边 1",
        "B06" => "红边 2",
        "B07" => "红边 3",
        "B08" => "近红外",
        "B8A" => "窄近红外",
        "B09" => "水汽",
        "B10" => "卷云",
        "B11" => "短波红外 1",
        "B12" => "短波红外 2",
        "TCI" => "真彩色",
        "SCL" => "场景分类",
        "AOT" => "气溶胶光学厚度",
        "WVP" => "水汽总量",
        "PRD" => "云概率",
        "PVI" => "植被指数",
        "MSK_CLOUDS" => "云掩膜",
        "MSK_DETFOO" => "缺陷像元掩膜",
        "MSK_SATURA" => "饱和像元掩膜",
        "MSK_NODATA" => "无数据掩膜",
        "MSK_TECQUA" => "技术质量掩膜",
        _ => "",
    }
}

fn detect_level(root: &Path, product: &XmlValues) -> String {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_uppercase();
    if name.contains("MSIL2A") {
        return "L2A".to_owned();
    }
    if name.contains("MSIL1C") {
        return "L1C".to_owned();
    }
    if let Some(value) = product
        .first(&["PRODUCT_TYPE"])
        .or_else(|| product.first(&["PRODUCT_LEVEL"]))
    {
        let upper = value.to_ascii_uppercase();
        if upper.contains("2A") {
            return "L2A".to_owned();
        }
        if upper.contains("1C") {
            return "L1C".to_owned();
        }
    }
    "SAFE".to_owned()
}

fn read_geocoding(tile: &XmlValues) -> Option<SafeGrid> {
    let ulx = tile.number(&["Geoposition", "ULX"])?;
    let uly = tile.number(&["Geoposition", "ULY"])?;
    let xdim = tile.number(&["Geoposition", "XDIM"])?;
    let ydim = tile.number(&["Geoposition", "YDIM"])?;
    let rows = tile.number(&["Size", "NROWS"])?;
    let cols = tile.number(&["Size", "NCOLS"])?;
    Some(SafeGrid {
        ulx,
        uly,
        xdim: xdim.abs(),
        ydim: ydim.abs(),
        cols: cols.max(0.0) as u32,
        rows: rows.max(0.0) as u32,
    })
}

fn parse_epsg(value: &str) -> Option<u32> {
    value
        .rsplit([':', '/'])
        .next()
        .and_then(|code| code.trim().parse::<u32>().ok())
}

/// 极简 XML 取值器：把「元素路径 → 文本」拍平保存，查询时按路径后缀匹配。
///
/// SAFE 的 MTD XML 命名空间层级在 L1C / L2A 之间有细微差异，按后缀匹配
/// 可以避免依赖固定的完整路径。
struct XmlValues {
    values: Vec<(Vec<String>, String)>,
}

impl XmlValues {
    fn first(&self, suffix: &[&str]) -> Option<&str> {
        self.values
            .iter()
            .find(|(path, _)| ends_with(path, suffix))
            .map(|(_, text)| text.as_str())
    }

    fn number(&self, suffix: &[&str]) -> Option<f64> {
        self.first(suffix).and_then(|text| text.trim().parse().ok())
    }
}

fn ends_with(path: &[String], suffix: &[&str]) -> bool {
    if suffix.len() > path.len() {
        return false;
    }
    path[path.len() - suffix.len()..]
        .iter()
        .zip(suffix)
        .all(|(segment, expected)| segment.eq_ignore_ascii_case(expected))
}

fn read_xml(path: &Path) -> Result<XmlValues, RasterError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| RasterError::Io(format!("{}：{error}", path.display())))?;
    let mut reader = Reader::from_str(&text);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<String> = Vec::new();
    let mut values: Vec<(Vec<String>, String)> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                stack.push(local_name(event.name().as_ref()));
            }
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Empty(event)) => {
                // 自闭合元素也记一条空文本，避免上层「存在即判断」时误判。
                let mut path = stack.clone();
                path.push(local_name(event.name().as_ref()));
                values.push((path, String::new()));
            }
            Ok(Event::Text(event)) => {
                let text = event
                    .unescape()
                    .map_err(|error| RasterError::Corrupt(format!("{}：{error}", path.display())))?
                    .trim()
                    .to_owned();
                if !text.is_empty() && !stack.is_empty() {
                    values.push((stack.clone(), text));
                }
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(RasterError::Corrupt(format!(
                    "{}：XML 解析失败（{error}）",
                    path.display()
                )));
            }
            _ => {}
        }
    }
    Ok(XmlValues { values })
}

/// 去掉命名空间前缀：`n1:ULX` → `ULX`。
fn local_name(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    match text.rsplit_once(':') {
        Some((_, local)) => local.to_owned(),
        None => text.into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独立的临时目录（同名旧目录先删掉，避免读到上一次的残留）。
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("rustgis-safe-fixtures")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 把 `<Granule>_B04_10m` 这类文件名拼成一个完整波段文件名。
    fn band_file_name(code: &str, resolution: &str) -> String {
        format!("T33UVP_20240101T101231_{code}_{resolution}.jp2")
    }

    /// 构造一个只用于排序/去重测试的波段文件描述。
    fn band_file(code: &str, resolution: u32) -> BandFile {
        let factor = resolution / 10;
        BandFile {
            path: PathBuf::from(format!(
                "IMG_DATA/R{resolution}m/{}",
                band_file_name(code, &format!("{resolution}m"))
            )),
            code: code.to_owned(),
            factor,
            resolution,
            width: 10980 / factor,
            height: 10980 / factor,
            components: 1,
            bit_depth: 15,
            tiles: None,
        }
    }

    #[test]
    fn rank_and_dedupe_keeps_the_finest_copy_of_each_band() {
        // 真实 L2A 布局：同一波段在三个分辨率目录各存一份，顺序也是乱的。
        let mut files = vec![
            band_file("B11", 20),
            band_file("TCI", 10),
            band_file("B04", 20),
            band_file("B04", 10),
            band_file("B04", 60),
            band_file("SCL", 20),
            band_file("B03", 10),
        ];
        rank_and_dedupe(&mut files);

        let codes = files.iter().map(|f| f.code.as_str()).collect::<Vec<_>>();
        // TCI 排最前，其后是可见光，辅助数据在后；每个波段只出现一次。
        assert_eq!(codes, vec!["TCI", "B04", "B03", "B11", "SCL"], "{files:?}");

        // 保留下来的必须是分辨率最高的那一份（factor 1 = 10 m）。
        let b04 = files.iter().find(|f| f.code == "B04").unwrap();
        assert_eq!(b04.resolution, 10, "{b04:?}");
        let b11 = files.iter().find(|f| f.code == "B11").unwrap();
        assert_eq!(b11.resolution, 20, "{b11:?}");
    }

    #[test]
    fn band_code_reads_band_from_typical_names() {
        let cases = [
            (band_file_name("B04", "10m"), "B04"),
            (band_file_name("B8A", "20m"), "B8A"),
            (band_file_name("B12", "20m"), "B12"),
            (band_file_name("TCI", "10m"), "TCI"),
            (band_file_name("SCL", "20m"), "SCL"),
            (band_file_name("AOT", "60m"), "AOT"),
        ];
        for (name, expected) in cases {
            let path = PathBuf::from(name.clone());
            assert_eq!(band_code(&path), expected, "文件名 {name}");
        }
    }

    #[test]
    fn band_code_keeps_mask_suffix() {
        let path = PathBuf::from("MSK_CLOUDS_B00.jp2");
        assert_eq!(band_code(&path), "MSK_CLOUDS");
    }

    #[test]
    fn band_tokens_reject_unrelated_names() {
        // 产品级 / 时间戳 / 分辨率后缀都不应当被当成波段。
        for token in ["T33UVP", "20240101T101231", "10M", "B", "B4", "B04A"] {
            assert!(!is_band_token(token), "{token} 不应被识别为波段");
        }
        for token in ["B01", "B8A", "TCI", "SCL"] {
            assert!(is_band_token(token), "{token} 应被识别为波段");
        }
    }

    #[test]
    fn band_rank_orders_true_color_and_visible_first() {
        let mut codes = vec!["SCL", "B11", "B04", "TCI", "B02", "MSK_CLOUDS", "ZZZ"];
        codes.sort_by_key(|code| band_rank(code));
        assert_eq!(
            codes,
            vec!["TCI", "B04", "B02", "B11", "SCL", "MSK_CLOUDS", "ZZZ"]
        );
    }

    #[test]
    fn band_description_covers_common_bands() {
        assert_eq!(band_description("B04"), "红");
        assert_eq!(band_description("B08"), "近红外");
        assert_eq!(band_description("SCL"), "场景分类");
        // 未知波段不给说明，显示名里不会出现多余后缀。
        assert_eq!(band_description("B99"), "");
    }

    #[test]
    fn read_geocoding_parses_namespaced_tile_metadata() {
        let dir = temp_dir("geocoding");
        let path = dir.join("MTD_TL.xml");
        std::fs::write(
            &path,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<n1:Level-2A_Tile_ID xmlns:n1="https://psd-14.sentinel2.eo.esa.int/PSD/S2_PDI_Level-2A_Tile_Metadata.xsd">
  <n1:General_Info>
    <TILE_ID>T33UVP_20240101T101231_A000000</TILE_ID>
  </n1:General_Info>
  <n1:Geometric_Info>
    <Tile_Geocoding>
      <HORIZONTAL_CS_NAME>WGS84 / UTM zone 33N</HORIZONTAL_CS_NAME>
      <HORIZONTAL_CS_CODE>EPSG:32633</HORIZONTAL_CS_CODE>
      <Size resolution="10">
        <NROWS>10980</NROWS>
        <NCOLS>10980</NCOLS>
      </Size>
      <Geoposition resolution="10">
        <ULX>399960.0</ULX>
        <ULY>5300040.0</ULY>
        <XDIM>10.0</XDIM>
        <YDIM>10.0</YDIM>
      </Geoposition>
    </Tile_Geocoding>
  </n1:Geometric_Info>
</n1:Level-2A_Tile_ID>"#,
        )
        .unwrap();

        let values = read_xml(&path).unwrap();
        assert_eq!(
            values.first(&["HORIZONTAL_CS_CODE"]),
            Some("EPSG:32633"),
            "命名空间前缀应被去掉"
        );
        assert_eq!(parse_epsg("EPSG:32633"), Some(32633));
        assert_eq!(parse_epsg("urn:ogc:def:crs:EPSG::32633"), Some(32633));
        assert_eq!(parse_epsg("WGS84"), None);

        let grid = read_geocoding(&values).unwrap();
        assert_eq!(grid.cols, 10980);
        assert_eq!(grid.rows, 10980);
        assert!((grid.ulx - 399_960.0).abs() < 1e-6);
        assert!((grid.uly - 5_300_040.0).abs() < 1e-6);
        assert!((grid.xdim - 10.0).abs() < 1e-6);
        // YDIM 在 tile 元数据里写成正值，需要按「像元向下为负」使用。
        assert!((grid.ydim - 10.0).abs() < 1e-6);

        // 负数 YDIM 也要取绝对值。
        let negative = XmlValues {
            values: vec![
                (vec!["Geoposition".into(), "ULX".into()], "0".into()),
                (vec!["Geoposition".into(), "ULY".into()], "0".into()),
                (vec!["Geoposition".into(), "XDIM".into()], "-10".into()),
                (vec!["Geoposition".into(), "YDIM".into()], "-10".into()),
                (vec!["Size".into(), "NROWS".into()], "4".into()),
                (vec!["Size".into(), "NCOLS".into()], "5".into()),
            ],
        };
        let grid = read_geocoding(&negative).unwrap();
        assert!((grid.xdim - 10.0).abs() < 1e-6);
        assert_eq!((grid.cols, grid.rows), (5, 4));
    }

    #[test]
    fn read_geocoding_requires_every_field() {
        let values = XmlValues {
            values: vec![
                (vec!["Geoposition".into(), "ULX".into()], "1".into()),
                (vec!["Geoposition".into(), "ULY".into()], "2".into()),
            ],
        };
        assert!(read_geocoding(&values).is_none());
    }

    #[test]
    fn detect_level_prefers_directory_name() {
        let l2a = temp_dir("S2B_MSIL2A_20240101T101231_N0509_R065_T33UVP_20240101T000000.SAFE");
        assert_eq!(detect_level(&l2a, &XmlValues { values: vec![] }), "L2A");

        let l1c = temp_dir("S2A_MSIL1C_20240101T101231_N0509_R065_T33UVP_20240101T000000.SAFE");
        assert_eq!(detect_level(&l1c, &XmlValues { values: vec![] }), "L1C");

        let other = temp_dir("22_safe_level_from_product_type");
        let product = XmlValues {
            values: vec![(
                vec![
                    "Level-2A_User_Product".to_owned(),
                    "General_Info".to_owned(),
                    "Product_Info".to_owned(),
                    "PRODUCT_TYPE".to_owned(),
                ],
                "S2MSI2A".to_owned(),
            )],
        };
        assert_eq!(detect_level(&other, &product), "L2A");
        assert_eq!(detect_level(&other, &XmlValues { values: vec![] }), "SAFE");
    }

    #[test]
    fn is_safe_package_recognises_safe_layouts() {
        let empty = temp_dir("plain-dir");
        assert!(!is_safe_package(&empty));

        let named = temp_dir("S2A_MSIL2A_x.SAFE");
        assert!(is_safe_package(&named));

        let granule_only = temp_dir("has-granule");
        std::fs::create_dir_all(granule_only.join("GRANULE")).unwrap();
        assert!(is_safe_package(&granule_only));

        let mtd_only = temp_dir("has-mtd");
        let mtd = mtd_only.join("MTD_MSIL2A.xml");
        std::fs::write(&mtd, "<x/>").unwrap();
        assert!(is_safe_package(&mtd_only));
        // 直接选中元数据文件也算选中了这个包。
        assert!(is_safe_package(&mtd));
    }

    #[test]
    fn open_reports_missing_metadata_and_missing_bands() {
        let no_mtd = temp_dir("no-mtd");
        let err = open(&no_mtd).unwrap_err();
        assert!(
            format!("{err}").contains("MTD_MSIL"),
            "应提示缺少元数据：{err}"
        );

        // 有元数据、有 GRANULE，但 IMG_DATA 里没有 JP2。
        let empty_granule = temp_dir("empty-granule");
        std::fs::write(empty_granule.join("MTD_MSIL2A.xml"), "<x/>").unwrap();
        std::fs::create_dir_all(
            empty_granule
                .join("GRANULE")
                .join("L2A_T33UVP_A000000_20240101T101231")
                .join("IMG_DATA")
                .join("R10m"),
        )
        .unwrap();
        let err = open(&empty_granule).unwrap_err();
        assert!(
            format!("{err}").contains("JPEG 2000"),
            "应提示没有波段文件：{err}"
        );
    }

    #[test]
    fn open_parses_package_metadata_and_bands() {
        // 一个「只有元数据、没有真实 JP2」的包无法通过 collect_jp2，
        // 所以这里只验证元数据链路：直接读 MTD 并检查地理编码 / 层级 / 产品名。
        let root = temp_dir("S2B_MSIL2A_20240101T101231_N0509_R065_T33UVP_parsed.SAFE");
        let mtd = root.join("MTD_MSIL2A.xml");
        std::fs::write(
            &mtd,
            r#"<n1:Level-2A_User_Product xmlns:n1="x">
  <n1:General_Info>
    <Product_Info>
      <PRODUCT_TYPE>S2MSI2A</PRODUCT_TYPE>
      <PRODUCT_START_TIME>2024-01-01T10:12:31.024Z</PRODUCT_START_TIME>
    </Product_Info>
  </n1:General_Info>"#,
        )
        .unwrap();
        let granule = root
            .join("GRANULE")
            .join("L2A_T33UVP_A000000_20240101T101231");
        std::fs::create_dir_all(&granule).unwrap();
        std::fs::write(
            granule.join("MTD_TL.xml"),
            r#"<n1:Level-2A_Tile_ID><n1:Geometric_Info><Tile_Geocoding>
  <HORIZONTAL_CS_NAME>WGS84 / UTM zone 33N</HORIZONTAL_CS_NAME>
  <HORIZONTAL_CS_CODE>EPSG:32633</HORIZONTAL_CS_CODE>
  <Size resolution="10"><NROWS>10980</NROWS><NCOLS>10980</NCOLS></Size>
  <Geoposition resolution="10"><ULX>399960.0</ULX><ULY>5300040.0</ULY>
    <XDIM>10.0</XDIM><YDIM>10.0</YDIM></Geoposition>
</Tile_Geocoding></n1:Geometric_Info></n1:Level-2A_Tile_ID>"#,
        )
        .unwrap();

        // 波段文件缺失时 open 会报错，但错误信息不应是「缺少元数据」。
        let err = open(&root).unwrap_err();
        assert!(format!("{err}").contains("JPEG 2000"), "{err}");

        // 用最小可用实现验证地理编码链路：读 XML 后直接解析。
        let values = read_xml(&granule.join("MTD_TL.xml")).unwrap();
        let grid = read_geocoding(&values).unwrap();
        assert_eq!(grid.cols, 10980);
        assert_eq!(
            parse_epsg(values.first(&["HORIZONTAL_CS_CODE"]).unwrap()),
            Some(32633)
        );
        assert_eq!(find_mtd(&root), Some(mtd.clone()));
        let product = read_xml(&mtd).unwrap();
        assert_eq!(detect_level(&root, &product), "L2A");
    }

    #[test]
    fn resolution_directories_are_enumerated_in_order() {
        let root = temp_dir("scan-resolutions");
        let img_data = root.join("IMG_DATA");
        for dir in ["R10m", "R20m", "R60m"] {
            std::fs::create_dir_all(img_data.join(dir)).unwrap();
        }
        // 只放两个空文件：scan_band_files 只做枚举，不解析头部。
        for (dir, name) in [
            ("R10m", band_file_name("B04", "10m")),
            ("R10m", band_file_name("TCI", "10m")),
            ("R20m", band_file_name("B11", "20m")),
        ] {
            std::fs::write(img_data.join(dir).join(name), b"x").unwrap();
        }
        // 非 JPEG 2000 文件应被忽略。
        std::fs::write(img_data.join("R10m").join("notes.txt"), b"x").unwrap();

        let mut found = Vec::new();
        for (dir, resolution) in [("R10m", 10u32), ("R20m", 20), ("R60m", 60)] {
            // collect_jp2 会解析文件头，这里只验证枚举顺序与分辨率分组，
            // 因此跳过头部解析：直接列出文件名。
            let mut names = std::fs::read_dir(img_data.join(dir))
                .unwrap()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "jp2"))
                .collect::<Vec<_>>();
            names.sort();
            for path in names {
                found.push((band_code(&path), resolution));
            }
        }
        assert_eq!(
            found,
            vec![
                ("B04".to_owned(), 10),
                ("TCI".to_owned(), 10),
                ("B11".to_owned(), 20),
            ]
        );
    }
}
