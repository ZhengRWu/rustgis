//! 栅格数据模块（v0.3）：TIF / GeoTIFF 的分块读取、地理参考、金字塔与统计。
//!
//! 设计要点：
//! - 打开文件只读取 IFD 头信息，不加载整幅影像；像素按 strip / tile **按需解码**，
//!   解码后的块进入有上限的 LRU 缓存，因此大影像的内存占用与文件大小无关。
//! - 地理参考来自 GeoTIFF 标签（ModelPixelScale / ModelTiepoint / ModelTransformation /
//!   GeoKeyDirectory）与 GDAL 元数据；缺失时给出明确状态而不是报错崩溃。
//! - 显示按缩放级别选择内部 / 外部 / 本地构建的金字塔层级，再按屏幕分辨率重采样，
//!   保证缩放浏览时每帧只解码少量像素。
//! - 统计值优先读文件内已有的 GDAL 统计（STATISTICS_*），没有时按需计算，
//!   计算在后台线程执行，不阻塞界面。

mod geotiff;
mod jp2;
mod paint;
mod pyramid;
mod safe;
mod stats;
mod tiff_file;

#[cfg(test)]
pub(crate) mod fixture_tests;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::geo::{Aabb, Coord2D, CrsId, ProjectionInfo, ProjectionModel};
use j2k::Rect;

pub use paint::RasterPainter;

/// 路径是否是 Sentinel-2 SAFE 产品包（`.SAFE` 目录或其中的 `MTD_MSIL*.xml`）。
///
/// 供导入流水线区分「栅格」与其他文件类型，避免把 SAFE 目录当成未知格式。
pub fn is_safe_package(path: &Path) -> bool {
    safe::is_safe_package(path)
}

/// 单次窗口读取的默认内存上限（256 MiB），超过则拒绝读取并返回结构化错误。
pub const DEFAULT_MAX_READ_BYTES: u64 = 256 * 1024 * 1024;

/// 超过这个像素数时统计改为整幅降采样抽样（而不是逐窗全分辨率读取）。
const STATS_SAMPLE_PIXELS: u64 = 16_000_000;

/// Sentinel-2 波段允许使用的最深小波降采样层数（÷256）。
const MAX_SAFE_REDUCTION_LEVELS: u8 = 8;

/// 统计抽样目标：长边最多 2048 像素，保持长宽比。
fn statistics_target(width: u32, height: u32) -> (u32, u32) {
    const LONG_SIDE: u32 = 2048;
    let longest = width.max(height).max(1);
    if longest <= LONG_SIDE {
        return (width.max(1), height.max(1));
    }
    let scale = f64::from(LONG_SIDE) / f64::from(longest);
    (
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    )
}

/// 栅格错误模型：所有失败都映射到明确分支，便于界面给出可读提示。
#[derive(Debug)]
pub enum RasterError {
    /// 文件不存在或不是普通文件。
    NotFound(PathBuf),
    /// 不是可解析的 TIFF（头信息损坏 / 魔数不符）。
    NotTiff(String),
    /// 使用了不支持的格式特性（压缩 / 色彩空间 / 位深组合）。
    Unsupported(String),
    /// 文件损坏（IFD、块偏移或数据长度异常）。
    Corrupt(String),
    /// 请求窗口超出影像范围。
    OutOfBounds {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    /// 单次读取超过内存上限。
    TooLarge { requested: u64, limit: u64 },
    /// 波段索引非法。
    BadBand { index: usize, count: usize },
    /// 后台任务被取消。
    Cancelled,
    /// IO 错误。
    Io(String),
}

impl RasterError {
    /// 稳定的错误码，供界面或测试匹配（不随文案变化）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "file-not-found",
            Self::NotTiff(_) => "not-tiff",
            Self::Unsupported(_) => "unsupported",
            Self::Corrupt(_) => "corrupt",
            Self::OutOfBounds { .. } => "out-of-bounds",
            Self::TooLarge { .. } => "too-large",
            Self::BadBand { .. } => "bad-band",
            Self::Cancelled => "cancelled",
            Self::Io(_) => "io",
        }
    }
}

impl std::fmt::Display for RasterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(path) => write!(f, "栅格文件不存在：{}", path.display()),
            Self::NotTiff(msg) => write!(f, "不是有效的 TIFF 文件：{msg}"),
            Self::Unsupported(msg) => write!(f, "不支持的 TIFF 特性：{msg}"),
            // 数据集既可能是 TIFF 也可能是 JPEG 2000（SAFE），文案保持格式中立。
            Self::Corrupt(msg) => write!(f, "栅格数据已损坏：{msg}"),
            Self::OutOfBounds {
                x,
                y,
                width,
                height,
            } => write!(f, "读取窗口超出影像范围：[{x}, {y}] {width}×{height}"),
            Self::TooLarge { requested, limit } => write!(
                f,
                "单次读取需要 {}，超过内存上限 {}，请改用更小的窗口",
                format_bytes(*requested),
                format_bytes(*limit)
            ),
            Self::BadBand { index, count } => {
                write!(f, "波段索引越界：{}（共 {count} 个波段）", index + 1)
            }
            Self::Cancelled => write!(f, "操作已取消"),
            Self::Io(msg) => write!(f, "文件读写失败：{msg}"),
        }
    }
}

impl std::error::Error for RasterError {}

impl From<std::io::Error> for RasterError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl From<tiff::TiffError> for RasterError {
    fn from(err: tiff::TiffError) -> Self {
        match err {
            tiff::TiffError::UnsupportedError(inner) => Self::Unsupported(inner.to_string()),
            tiff::TiffError::FormatError(inner) => Self::Corrupt(inner.to_string()),
            tiff::TiffError::LimitsExceeded => Self::Corrupt("TIFF 尺寸超出解码器限制".into()),
            tiff::TiffError::IoError(inner) => Self::Io(inner.to_string()),
            tiff::TiffError::UsageError(inner) => Self::Corrupt(inner.to_string()),
            tiff::TiffError::IntSizeError => Self::Corrupt("TIFF 数值超出可表示范围".into()),
        }
    }
}

/// 像素窗口（左上角 + 宽高，单位：像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelWindow {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelWindow {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// 裁剪到影像范围内；完全落在范围外时返回空窗口。
    pub fn clamp_to(self, width: u32, height: u32) -> Self {
        let x0 = self.x.min(width);
        let y0 = self.y.min(height);
        let x1 = self.x.saturating_add(self.width).min(width);
        let y1 = self.y.saturating_add(self.height).min(height);
        Self::new(x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
    }
}

/// 像素数据类型（与 TIFF 的 BitsPerSample + SampleFormat 组合对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleType {
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

impl SampleType {
    pub fn label(&self) -> &'static str {
        match self {
            Self::U8 => "8 位无符号整型",
            Self::U16 => "16 位无符号整型",
            Self::U32 => "32 位无符号整型",
            Self::U64 => "64 位无符号整型",
            Self::I8 => "8 位有符号整型",
            Self::I16 => "16 位有符号整型",
            Self::I32 => "32 位有符号整型",
            Self::I64 => "64 位有符号整型",
            Self::F32 => "32 位浮点型",
            Self::F64 => "64 位浮点型",
        }
    }

    /// 每个样本占用的字节数。
    pub fn bytes(&self) -> u32 {
        match self {
            Self::U8 | Self::I8 => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::U64 | Self::I64 | Self::F64 => 8,
        }
    }

    pub(crate) fn from_tags(bits: u16, sample_format: u16) -> Self {
        match (sample_format, bits) {
            (3, 32) => Self::F32,
            (3, 64) => Self::F64,
            (3, _) => Self::F32,
            (2, 8) => Self::I8,
            (2, 16) => Self::I16,
            (2, 32) => Self::I32,
            (2, 64) => Self::I64,
            (_, 8) => Self::U8,
            (_, 16) => Self::U16,
            (_, 32) => Self::U32,
            (_, 64) => Self::U64,
            _ => Self::U16,
        }
    }
}

/// TIFF 压缩方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionKind {
    None,
    Lzw,
    Deflate,
    DeflateOld,
    PackBits,
    Jpeg,
    WebP,
    Zstd,
    Fax,
    Huffman,
    /// JPEG 2000（Sentinel-2 SAFE 的波段文件）。
    Jpeg2000,
    Other(u16),
}

impl CompressionKind {
    pub(crate) fn from_code(code: u16) -> Self {
        match code {
            1 => Self::None,
            2 => Self::Huffman,
            3 | 4 => Self::Fax,
            5 => Self::Lzw,
            6 | 7 => Self::Jpeg,
            8 => Self::Deflate,
            0x80B2 => Self::DeflateOld,
            0x8005 => Self::PackBits,
            0xC350 => Self::Zstd,
            0xC351 => Self::WebP,
            other => Self::Other(other),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::None => "无压缩".to_string(),
            Self::Lzw => "LZW".to_string(),
            Self::Deflate => "Deflate".to_string(),
            Self::DeflateOld => "Deflate（旧）".to_string(),
            Self::PackBits => "PackBits".to_string(),
            Self::Jpeg => "JPEG".to_string(),
            Self::WebP => "WebP".to_string(),
            Self::Zstd => "ZSTD".to_string(),
            Self::Fax => "CCITT 传真".to_string(),
            Self::Huffman => "Huffman".to_string(),
            Self::Jpeg2000 => "JPEG 2000".to_string(),
            Self::Other(code) => format!("未知压缩（{code}）"),
        }
    }

    /// 是否可以解码（未知压缩方式给出明确错误，而不是静默失败）。
    pub fn is_readable(&self) -> bool {
        !matches!(self, Self::Other(_))
    }
}

/// 分块方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockLayout {
    Strips,
    Tiles,
    /// JPEG 2000 码流（Sentinel-2 SAFE）：解码器按码流内部瓦片与区域解码，
    /// 没有 TIFF 式的 strip / tile 表。
    Jpeg2000,
}

impl BlockLayout {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Strips => "条带（Strip）",
            Self::Tiles => "瓦片（Tile）",
            Self::Jpeg2000 => "JPEG 2000 码流（按区域解码）",
        }
    }
}

/// 波段在影像中的色彩解释。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorInterp {
    Gray,
    Red,
    Green,
    Blue,
    Alpha,
    Undefined,
}

impl ColorInterp {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Gray => "灰度",
            Self::Red => "红",
            Self::Green => "绿",
            Self::Blue => "蓝",
            Self::Alpha => "Alpha",
            Self::Undefined => "未定义",
        }
    }
}

/// 单一波段的元数据。
#[derive(Debug, Clone)]
pub struct BandInfo {
    /// 0 起始的波段索引。
    pub index: usize,
    /// 显示名称（GDAL 描述或 `Band_n`）。
    pub name: String,
    pub color: ColorInterp,
    pub bits: u16,
    pub sample_type: SampleType,
    pub nodata: Option<f64>,
    /// 文件内已有的统计值（GDAL_METADATA），没有则为 `None`。
    pub stored_stats: Option<BandStatistics>,
}

/// 波段统计值。
#[derive(Debug, Clone, Default)]
pub struct BandStatistics {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub std_dev: f64,
    /// 参与统计的有效像素数（已排除 NoData）。
    pub valid_count: u64,
    /// 是否来自文件内已有统计（否则为本次计算）。
    pub from_file: bool,
    /// 是否抽样估算（大影像按步长抽样）。
    pub estimated: bool,
    /// 4096 桶直方图，用于任意百分比截断（仅本地计算时提供）。
    pub histogram: Vec<u32>,
    /// 2% / 98% 分位值（本地计算时提供）。
    pub percent: Option<(f64, f64)>,
}

impl BandStatistics {
    /// 按百分比截断返回 (低, 高)。`clip` 为单侧百分比（0.5 表示各截 0.5%）。
    pub fn clipped(&self, clip: f32) -> (f64, f64) {
        if !self.histogram.is_empty() && self.max > self.min {
            let low = (f64::from(clip) / 100.0).clamp(0.0, 0.49);
            let high = 1.0 - low;
            let low_v = self.quantile(low).unwrap_or(self.min);
            let high_v = self.quantile(high).unwrap_or(self.max);
            if high_v > low_v {
                return (low_v, high_v);
            }
        }
        if let Some((lo, hi)) = self.percent
            && hi > lo
        {
            return (lo, hi);
        }
        (self.min, self.max)
    }

    /// 直方图分位值（`q` 取 0..=1）。
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.histogram.is_empty() || self.max <= self.min {
            return None;
        }
        let total: u64 = self.histogram.iter().map(|v| u64::from(*v)).sum();
        if total == 0 {
            return None;
        }
        let target = (q.clamp(0.0, 1.0) * total as f64) as u64;
        let mut acc = 0u64;
        let bins = self.histogram.len();
        for (i, count) in self.histogram.iter().enumerate() {
            acc += u64::from(*count);
            if acc >= target.max(1) {
                let frac = (i as f64 + 0.5) / bins as f64;
                return Some(self.min + frac * (self.max - self.min));
            }
        }
        Some(self.max)
    }
}

/// 栅格数据集统一元数据（界面「图层属性 → 源」直接展示）。
#[derive(Debug, Clone)]
pub struct RasterMetadata {
    pub width: u32,
    pub height: u32,
    pub band_count: usize,
    pub sample_type: SampleType,
    pub bits_per_sample: u16,
    pub nodata: Option<f64>,
    pub compression: CompressionKind,
    pub planar: bool,
    pub block: BlockLayout,
    pub block_width: u32,
    pub block_height: u32,
    pub blocks_across: u32,
    pub blocks_down: u32,
    pub photometric: String,
    pub file_size: u64,
    pub big_tiff: bool,
    pub internal_overviews: usize,
    pub has_external_overviews: bool,
    pub bands: Vec<BandInfo>,
}

impl RasterMetadata {
    pub fn total_blocks(&self) -> u64 {
        u64::from(self.blocks_across) * u64::from(self.blocks_down)
    }
}

/// 仿射变换：像素坐标（列 col、行 row）→ 世界坐标。
///
/// 与 GDAL GeoTransform 一致：`x = a*col + b*row + c`，`y = d*col + e*row + f`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Affine {
    pub fn new(gt: [f64; 6]) -> Self {
        Self {
            a: gt[1],
            b: gt[2],
            c: gt[0],
            d: gt[4],
            e: gt[5],
            f: gt[3],
        }
    }

    /// 转回 GDAL GeoTransform 形式。
    pub fn geotransform(&self) -> [f64; 6] {
        [self.c, self.a, self.b, self.f, self.d, self.e]
    }
    pub fn apply(&self, col: f64, row: f64) -> Coord2D {
        [
            self.a * col + self.b * row + self.c,
            self.d * col + self.e * row + self.f,
        ]
    }

    /// 逆变换：世界坐标 → 像素坐标（列、行），行列式接近 0 时返回 `None`。
    pub fn inverse(&self, x: f64, y: f64) -> Option<Coord2D> {
        let det = self.a * self.e - self.b * self.d;
        if det.abs() < 1e-15 {
            return None;
        }
        let dx = x - self.c;
        let dy = y - self.f;
        Some([
            (self.e * dx - self.b * dy) / det,
            (-self.d * dx + self.a * dy) / det,
        ])
    }

    /// 是否为正北朝上（无旋转）。
    pub fn is_north_up(&self) -> bool {
        self.b.abs() < 1e-12 && self.d.abs() < 1e-12
    }

    /// 影像在像素范围 `0..width / 0..height` 内的世界坐标包围盒。
    pub fn bounds(&self, width: u32, height: u32) -> Aabb {
        let w = f64::from(width);
        let h = f64::from(height);
        let mut bounds = Aabb::empty();
        for (col, row) in [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)] {
            bounds.extend_point(self.apply(col, row));
        }
        bounds
    }
}

/// GeoTIFF 地理参考状态。
#[derive(Debug, Clone)]
pub struct GeoReference {
    /// 像素 → 世界坐标的仿射变换；缺失时为 `None`。
    pub transform: Option<Affine>,
    pub crs: ProjectionInfo,
    /// 解析过程中的提示（如「仅有 GCP、缺少仿射变换」）。
    pub warnings: Vec<String>,
    /// GCP 数量（GeoTIFF ModelTiepoint 多控制点）。
    pub gcp_count: usize,
}

impl GeoReference {
    pub fn missing() -> Self {
        Self {
            transform: None,
            crs: ProjectionInfo::none(),
            warnings: vec!["文件缺少 GeoTIFF 地理参考标签".to_string()],
            gcp_count: 0,
        }
    }

    pub fn is_missing(&self) -> bool {
        self.transform.is_none()
    }

    /// 像素坐标 → 世界坐标。
    pub fn pixel_to_world(&self, col: f64, row: f64) -> Option<Coord2D> {
        self.transform.map(|affine| affine.apply(col, row))
    }

    /// 世界坐标 → 像素坐标。
    pub fn world_to_pixel(&self, x: f64, y: f64) -> Option<Coord2D> {
        self.transform
            .as_ref()
            .and_then(|affine| affine.inverse(x, y))
    }

    /// 空间参考名称（供界面展示）。
    pub fn crs_label(&self) -> String {
        if self.is_missing() {
            "无空间参考".to_string()
        } else {
            self.crs.crs.label()
        }
    }
}

/// 拉伸类型（对应 ArcGIS 符号系统的「拉伸类型」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StretchType {
    /// 不拉伸，直接按 0..255 之类的数据范围映射。
    None,
    /// 使用最小 / 最大值。
    MinMax,
    /// 百分比截断。
    PercentClip,
    /// 均值 ± n 倍标准差（n = 2）。
    StdDev,
}

impl StretchType {
    pub const ALL: [StretchType; 4] = [
        StretchType::None,
        StretchType::MinMax,
        StretchType::PercentClip,
        StretchType::StdDev,
    ];
}

/// 栅格图层显示设置（RGB 合成 + 拉伸，对应「图层属性 → 符号系统」）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RasterDisplay {
    /// RGB 三通道的波段索引（0 起始）；单波段影像三者相同即为灰度。
    pub bands: [usize; 3],
    /// Alpha 波段（可选，0 起始）。
    pub alpha_band: Option<usize>,
    pub stretch: StretchType,
    /// 百分比截断的单侧百分比，默认 0.5。
    pub percent_clip: f32,
    /// 「最小值 / 最大值」手动覆盖（`max <= min` 时使用统计值）。
    pub min: [f64; 3],
    pub max: [f64; 3],
    pub invert: [bool; 3],
    /// Gamma 拉伸（每通道），1.0 表示不调整。
    pub gamma: [f32; 3],
    /// 是否把 NoData 显示为透明。
    pub nodata_transparent: bool,
    /// 「显示背景值」的 RGB 颜色（`None` 表示透明）。
    pub background: Option<[u8; 3]>,
}

impl Default for RasterDisplay {
    fn default() -> Self {
        Self {
            bands: [0, 0, 0],
            alpha_band: None,
            stretch: StretchType::PercentClip,
            percent_clip: 0.5,
            min: [0.0; 3],
            max: [0.0; 3],
            invert: [false; 3],
            gamma: [1.0; 3],
            nodata_transparent: true,
            background: None,
        }
    }
}

impl RasterDisplay {
    /// 依据波段数推断默认设置：≥3 波段用 1/2/3 合成，否则灰度。
    pub fn for_band_count(count: usize, has_alpha: bool) -> Self {
        let mut display = Self::default();
        if count >= 3 {
            display.bands = [0, 1, 2];
            if has_alpha && count >= 4 {
                display.alpha_band = Some(3);
            }
        }
        display
    }
}

/// 概览层级的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LevelSource {
    /// 主文件内部的缩小 IFD。
    Internal,
    /// 外部 `.ovr` 文件。
    External,
    /// Sentinel-2 SAFE 的虚拟层级（`image` 存降采样倍率的 2 的幂次）。
    Safe,
}

#[derive(Debug, Clone, Copy)]
struct LevelEntry {
    source: LevelSource,
    /// 在该文件内的 IFD 序号。
    image: usize,
    width: u32,
    height: u32,
    /// 相对原始分辨率的缩小倍数（1.0 = 原始）。
    scale: f64,
}

impl LevelEntry {
    fn window_for(&self, base: PixelWindow) -> PixelWindow {
        let s = self.scale.max(1.0);
        let x0 = (f64::from(base.x) / s).floor() as u32;
        let y0 = (f64::from(base.y) / s).floor() as u32;
        let x1 = ((f64::from(base.x + base.width) / s).ceil() as u32).min(self.width);
        let y1 = ((f64::from(base.y + base.height) / s).ceil() as u32).min(self.height);
        PixelWindow::new(
            x0.min(self.width),
            y0.min(self.height),
            x1.saturating_sub(x0),
            y1.saturating_sub(y0),
        )
    }
}

/// 数据集的底层存储：普通 TIFF 文件，或 Sentinel-2 SAFE 产品包。
///
/// 两条路径共用上层的层级选择、统计与显示管线，差异只在「像素从哪来」。
// 每个图层只有一个数据集，枚举本身已经在 `Arc<Mutex<..>>` 里堆分配过一次，
// 为了「变体大小差」再塞一层 Box 只会多一次指针跳转。
#[allow(clippy::large_enum_variant)]
enum DatasetSource {
    /// TIFF / GeoTIFF（含内部概览与外部 `.ovr` 两级像素来源）。
    Tiff {
        base: tiff_file::TiffFile,
        external: Option<tiff_file::TiffFile>,
    },
    /// Sentinel-2 SAFE 产品包（JPEG 2000 波段文件集）。
    Safe {
        package: safe::SafePackage,
        cache: jp2::Jp2Cache,
    },
}

/// 栅格数据集：一个已打开、可共享读取的 TIF / GeoTIFF，或 Sentinel-2 SAFE 包。
pub struct RasterDataset {
    path: PathBuf,
    source: DatasetSource,
    levels: Vec<LevelEntry>,
    meta: RasterMetadata,
    georef: GeoReference,
    metadata_items: Vec<(String, String)>,
    stats: Vec<Option<BandStatistics>>,
    /// 统计数据版本号：每次写入统计值自增，绘制线程据此判断是否需要重建元数据快照。
    statistics_version: u64,
    max_read_bytes: u64,
}

impl std::fmt::Debug for RasterDataset {
    /// 只打印路径与元数据，避免把解码器、缓存等内部状态倒进日志。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RasterDataset")
            .field("path", &self.path)
            .field("width", &self.meta.width)
            .field("height", &self.meta.height)
            .field("band_count", &self.meta.band_count)
            .finish_non_exhaustive()
    }
}

impl RasterDataset {
    /// 打开本地栅格：TIF / GeoTIFF 文件，或 Sentinel-2 `.SAFE` 产品包
    /// （也可以直接选包里的 `MTD_MSIL*.xml`）。只解析头信息，不加载像素。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RasterError> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(RasterError::NotFound(path));
        }
        if safe::is_safe_package(&path) {
            return Self::open_safe(&path);
        }
        if !path.is_file() {
            return Err(RasterError::Unsupported(format!(
                "不是可识别的栅格文件或 SAFE 产品包：{}",
                path.display()
            )));
        }
        Self::open_tiff(path)
    }

    /// 打开 TIFF / GeoTIFF（含内部概览与外部 `.ovr` 概览）。
    fn open_tiff(path: PathBuf) -> Result<Self, RasterError> {
        let base = tiff_file::TiffFile::open(&path)?;
        let info = base.info.clone();
        let geotiff = geotiff::parse(&base.tags, info.samples);
        // 从「缺失地理参考」的空状态出发：有标签时再补上变换与 CRS。
        let mut georef = GeoReference::missing();
        georef.transform = geotiff.transform;
        georef.crs = geotiff.crs.clone();
        georef.warnings = geotiff.warnings.clone();
        georef.gcp_count = geotiff.gcp_count;

        let mut levels = Vec::new();
        let base_width = f64::from(info.width);
        for (index, level) in info.levels.iter().enumerate() {
            if index > 0 && (level.width >= info.width || level.height >= info.height) {
                continue;
            }
            levels.push(LevelEntry {
                source: LevelSource::Internal,
                image: level.image_index,
                width: level.width,
                height: level.height,
                scale: base_width / f64::from(level.width.max(1)),
            });
        }

        // 外部金字塔：GDAL 约定 `foo.tif.ovr`。
        let ovr_path = overview_path(&path);
        let external = if ovr_path.is_file() {
            tiff_file::TiffFile::open(&ovr_path).ok()
        } else {
            None
        };
        if let Some(ext) = &external {
            for (index, level) in ext.info.levels.iter().enumerate() {
                if level.width < info.width && level.height < info.height {
                    levels.push(LevelEntry {
                        source: LevelSource::External,
                        image: index,
                        width: level.width,
                        height: level.height,
                        scale: base_width / f64::from(level.width.max(1)),
                    });
                }
            }
        }

        // 本程序构建的概览就写在 `.ovr` 里（与 GDAL / ArcGIS 一致），
        // 因此上面这一段 External 分支已经覆盖，不再有额外的私有缓存文件。

        levels.sort_by(|a, b| a.scale.total_cmp(&b.scale));
        levels.dedup_by(|a, b| a.width == b.width && a.height == b.height);

        let mut meta = build_metadata(&info, &geotiff, &levels);
        meta.has_external_overviews = external.is_some();

        let stats = meta
            .bands
            .iter()
            .map(|band| band.stored_stats.clone())
            .collect();

        Ok(Self {
            path,
            source: DatasetSource::Tiff { base, external },
            levels,
            meta,
            georef,
            metadata_items: geotiff.metadata,
            stats,
            statistics_version: 0,
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
        })
    }

    /// 打开 Sentinel-2 SAFE 产品包。
    ///
    /// SAFE 包自带 10 / 20 / 60 m 三套分辨率网格，因此不需要也不应该构建金字塔：
    /// 这里直接把缩略层级映射成「虚拟层级」（1 / 2 / 4 … / 32 倍缩小），
    /// 显示时按缩放级别选择层级，`read_level_bands` 再按波段自身分辨率
    /// 换算源窗口并调用 JPEG 2000 的区域解码。
    ///
    /// 虚拟层级不占存储：每一级都只是「缩小若干倍再看」，所以往深处多铺几级
    /// 只有好处——全幅缩略显示时能直接取 1/16、1/32 的窗口，解码像素少一个数量级。
    fn open_safe(path: &Path) -> Result<Self, RasterError> {
        let package = safe::open(path)?;
        let base = package.base_grid();
        let width = base.cols;
        let height = base.rows;
        let mut levels = Vec::with_capacity(6);
        for shift in 0..6u32 {
            let divisor = 1u32 << shift;
            let level_width = width.div_ceil(divisor).max(1);
            let level_height = height.div_ceil(divisor).max(1);
            if shift > 0 && (level_width == width || level_height == height) {
                break;
            }
            levels.push(LevelEntry {
                source: LevelSource::Safe,
                image: shift as usize,
                width: level_width,
                height: level_height,
                scale: f64::from(divisor),
            });
        }
        let mut meta = build_safe_metadata(&package);
        // 虚拟层级就是「概览」：这样导入时不会再询问是否构建金字塔。
        meta.internal_overviews = levels.len().saturating_sub(1);
        let mut georef = GeoReference::missing();
        georef.transform = Some(Affine {
            a: base.xdim,
            b: 0.0,
            c: base.ulx,
            d: 0.0,
            e: -base.ydim,
            f: base.uly,
        });
        georef.crs = match package.crs_epsg {
            Some(code) => ProjectionInfo {
                model: ProjectionModel::Projected,
                crs: CrsId::Epsg(code),
            },
            None if !package.crs_name.is_empty() => ProjectionInfo {
                model: ProjectionModel::Projected,
                crs: CrsId::Name(package.crs_name.clone()),
            },
            None => ProjectionInfo::none(),
        };
        let mut metadata_items = package.metadata.clone();
        metadata_items.push((
            "数据集网格".to_owned(),
            format!(
                "{} m（{} × {} 像素）",
                package.base_resolution(),
                width,
                height
            ),
        ));
        metadata_items.push(("波段文件数".to_owned(), package.bands.len().to_string()));
        // 与 TIFF 数据集保持一致：每个波段一个统计槽位（SAFE 的统计值只能现算，
        // 元数据里没有 GDAL 风格的统计字段）。
        let stats = vec![None; meta.band_count];
        Ok(Self {
            path: package.root.clone(),
            source: DatasetSource::Safe {
                package,
                cache: jp2::Jp2Cache::new(),
            },
            levels,
            meta,
            georef,
            metadata_items,
            stats,
            statistics_version: 0,
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn metadata(&self) -> &RasterMetadata {
        &self.meta
    }

    pub fn georeference(&self) -> &GeoReference {
        &self.georef
    }

    /// GDAL 元数据条目（键值对，供「元数据」页展示）。
    pub fn metadata_items(&self) -> &[(String, String)] {
        &self.metadata_items
    }

    pub fn width(&self) -> u32 {
        self.meta.width
    }

    pub fn height(&self) -> u32 {
        self.meta.height
    }

    pub fn band_count(&self) -> usize {
        self.meta.band_count
    }

    pub fn band(&self, index: usize) -> Option<&BandInfo> {
        self.meta.bands.get(index)
    }

    pub fn bounds(&self) -> Aabb {
        match &self.georef.transform {
            Some(affine) => affine.bounds(self.meta.width, self.meta.height),
            // 没有地理参考时退化为像素坐标，图层仍可显示与缩放。
            None => Aabb {
                min: [0.0, 0.0],
                max: [f64::from(self.meta.width), f64::from(self.meta.height)],
            },
        }
    }

    pub fn level_count(&self) -> usize {
        self.levels.len()
    }

    /// 概览数量（不含原始分辨率）。
    pub fn overview_count(&self) -> usize {
        self.levels.len().saturating_sub(1)
    }

    /// 选择显示层级：返回 `(层级序号, 该层相对原始分辨率的缩小倍数)`。
    ///
    /// 取「缩小倍数不超过每输出像素所需源像素数」的最粗层级——也就是**每个输出像素
    /// 至少能拿到一个源像素**。层级是 2 的幂，容差一旦放宽到 2 倍就会稳定挑到比需要
    /// 粗一级的那层（缩放比 16.6 时挑 scale 32），纹理实际分辨率只有所需的一半，屏幕
    /// 上永远是「插值放大过的糊图」——这正是用户报的「不管放大成什么层级都很模糊」。
    /// 分块之后每块只有约 512² 像素，多解一级很便宜，不需要再用容差换速度。
    fn pick_level(&self, src_px_per_out_px: f64) -> (usize, f64) {
        let tolerance = src_px_per_out_px.max(1.0) * 1.001;
        let mut chosen = 0usize;
        for (index, level) in self.levels.iter().enumerate() {
            if level.scale <= tolerance {
                chosen = index;
            }
        }
        let scale = self.levels.get(chosen).map_or(1.0, |l| l.scale);
        (chosen, scale)
    }

    /// 读取原始分辨率下的一段窗口（单波段，返回 f32 采样值）。
    pub fn read_window(
        &mut self,
        window: PixelWindow,
        band: usize,
    ) -> Result<Vec<f32>, RasterError> {
        self.read_level_window(0, window, band)
    }

    /// 读取指定层级的一段窗口（单波段）。
    pub fn read_level_window(
        &mut self,
        level: usize,
        window: PixelWindow,
        band: usize,
    ) -> Result<Vec<f32>, RasterError> {
        let mut bands = self.read_level_bands(level, window, &[band])?;
        Ok(bands.remove(0))
    }

    /// 读取指定层级的一段窗口（多波段）。
    pub fn read_level_bands(
        &mut self,
        level: usize,
        window: PixelWindow,
        bands: &[usize],
    ) -> Result<Vec<Vec<f32>>, RasterError> {
        let entry = *self.levels.get(level).ok_or(RasterError::OutOfBounds {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        })?;
        let window = window.clamp_to(entry.width, entry.height);
        if window.is_empty() {
            return Err(RasterError::OutOfBounds {
                x: window.x,
                y: window.y,
                width: window.width,
                height: window.height,
            });
        }
        // 按源数据类型估算解码内存：位深越高，同一窗口占用的内存越大。
        let bytes = window.area() * bands.len() as u64 * u64::from(self.meta.sample_type.bytes());
        if bytes > self.max_read_bytes {
            return Err(RasterError::TooLarge {
                requested: bytes,
                limit: self.max_read_bytes,
            });
        }
        let sample_indexes: Vec<u16> = bands
            .iter()
            .map(|&band| {
                if band >= self.meta.band_count {
                    Err(RasterError::BadBand {
                        index: band,
                        count: self.meta.band_count,
                    })
                } else {
                    Ok(band as u16)
                }
            })
            .collect::<Result<_, _>>()?;

        match &mut self.source {
            DatasetSource::Tiff { base, external } => match entry.source {
                LevelSource::Internal => {
                    base.read_window_multi(entry.image, window, &sample_indexes)
                }
                LevelSource::External => match external {
                    Some(ext) => ext.read_window_multi(entry.image, window, &sample_indexes),
                    None => Err(RasterError::Corrupt("外部概览文件未打开".into())),
                },
                LevelSource::Safe => Err(RasterError::Corrupt("层级来源与数据集不匹配".into())),
            },
            DatasetSource::Safe { package, cache } => match entry.source {
                LevelSource::Safe => read_safe_bands(
                    package,
                    cache,
                    window,
                    bands,
                    u32::try_from(entry.image).unwrap_or(0),
                ),
                _ => Err(RasterError::Corrupt("层级来源与数据集不匹配".into())),
            },
        }
    }

    /// 读取显示所需的数据但不重采样：返回（各波段层级像素, 该数据对应的层级窗口）。
    ///
    /// 显示路径（`raster::paint`）用它把「最近邻重采样」与「拉伸合成」合并成一次遍历，
    /// 省掉一整块中间缓冲和一遍内存往返。
    pub fn read_display_source(
        &mut self,
        base_window: PixelWindow,
        out_width: u32,
        out_height: u32,
        bands: &[usize],
    ) -> Result<(Vec<Vec<f32>>, PixelWindow), RasterError> {
        let base_window = base_window.clamp_to(self.width(), self.height());
        if base_window.is_empty() || out_width == 0 || out_height == 0 {
            return Ok((
                bands.iter().map(|_| Vec::new()).collect(),
                PixelWindow::new(0, 0, 0, 0),
            ));
        }
        let ratio = f64::from(base_window.width) / f64::from(out_width);
        let (level, _scale) = self.pick_level(ratio);
        let entry = self.levels[level];
        let level_window = entry.window_for(base_window);
        // 没有任何概览层的大影像（典型：未建 `.ovr` 的 10980² Sentinel-2 RGB），
        // 整窗一次读完必然超过内存上限。以前这里直接报 `TooLarge`，于是底图缩略图、
        // 远缩放下的层级块、统计采样全部失败——屏幕上就留下大片空白。现在按条带
        // 分批读出并立刻最近邻采样：峰值内存只跟一条带有关，结果与一次读完等价。
        if self.display_need_bytes(level_window, bands.len()) > self.max_read_bytes {
            let planes =
                self.read_level_bands_streamed(level, level_window, bands, out_width, out_height)?;
            // 数据已经按 `out_width × out_height` 采样好，这里回报一个「1:1 的层级
            // 窗口」，调用方按同一套最近邻索引表映射就会得到恒等映射。
            return Ok((planes, PixelWindow::new(0, 0, out_width, out_height)));
        }
        let data = self.read_level_bands(level, level_window, bands)?;
        Ok((data, level_window))
    }

    /// 一次显示读取按源数据类型估算的内存占用（字节）。
    fn display_need_bytes(&self, window: PixelWindow, band_count: usize) -> u64 {
        window.area() * band_count as u64 * u64::from(self.meta.sample_type.bytes())
    }

    /// 超大窗口的分条带显示读取：逐条带解码，边读边最近邻采样到 `out_width × out_height`。
    ///
    /// 只有「一窗读完会超上限」时才走这里（见 [`Self::read_display_source`]）。条带厚度
    /// 取内存上限的 1/4 与 256 行的较小值：前者保证任何位深/波段数都不越界，后者避免
    /// 逐条带重复解码带来的额外开销。
    fn read_level_bands_streamed(
        &mut self,
        level: usize,
        window: PixelWindow,
        bands: &[usize],
        out_width: u32,
        out_height: u32,
    ) -> Result<Vec<Vec<f32>>, RasterError> {
        let columns = nearest_indexes(window.width, out_width);
        let rows = nearest_indexes(window.height, out_height);
        let out_w = out_width as usize;
        let out_h = out_height as usize;
        let mut planes = vec![vec![0.0f32; out_w * out_h]; bands.len()];
        if columns.is_empty() || rows.is_empty() {
            return Ok(planes);
        }
        let src_w = window.width as usize;
        let row_bytes =
            u64::from(window.width) * bands.len() as u64 * u64::from(self.meta.sample_type.bytes());
        let strip_rows = ((self.max_read_bytes / 4) / row_bytes.max(1)).clamp(1, 256) as u32;
        let mut out_y = 0usize;
        while out_y < rows.len() {
            let strip_start = rows[out_y];
            let strip_end = strip_start.saturating_add(strip_rows).min(window.height);
            let mut last = out_y;
            while last < rows.len() && rows[last] < strip_end {
                last += 1;
            }
            // 源行不足一条带时（放大方向）至少处理一行，避免原地打转。
            let last = last.max(out_y + 1);
            let strip_height = strip_end.max(strip_start + 1) - strip_start;
            let strip =
                PixelWindow::new(window.x, window.y + strip_start, window.width, strip_height);
            let data = self.read_level_bands(level, strip, bands)?;
            let needed = strip_height as usize * src_w;
            if data.iter().any(|plane| plane.len() < needed) {
                return Err(RasterError::Corrupt("条带解码得到的样本数少于期望".into()));
            }
            for (y, &row) in rows
                .iter()
                .enumerate()
                .take(last.min(rows.len()))
                .skip(out_y)
            {
                let src_row = row.saturating_sub(strip_start) as usize;
                let src_base = src_row * src_w;
                let dst_base = y * out_w;
                for (channel, plane) in data.iter().enumerate() {
                    let dst = &mut planes[channel][dst_base..dst_base + out_w];
                    for (offset, &column) in columns.iter().enumerate() {
                        dst[offset] = plane[src_base + column as usize];
                    }
                }
            }
            out_y = last;
        }
        Ok(planes)
    }

    /// 按显示分辨率采样：自动选择金字塔层级并最近邻重采样到 `out_width × out_height`。
    ///
    /// 地图绘制走 `read_display_source`（采样与合成合并成一次遍历），这个接口留给
    /// 需要「完整重采样结果」的调用方（分析、导出、测试）。
    #[allow(dead_code)]
    pub fn read_sampled(
        &mut self,
        base_window: PixelWindow,
        out_width: u32,
        out_height: u32,
        bands: &[usize],
    ) -> Result<Vec<Vec<f32>>, RasterError> {
        let (data, level_window) =
            self.read_display_source(base_window, out_width, out_height, bands)?;
        if level_window.is_empty() {
            return Ok(data);
        }
        Ok(data
            .into_iter()
            .map(|plane| resample_nearest(&plane, level_window, out_width, out_height))
            .collect())
    }

    // ---- 统计 ----

    pub fn statistics(&self, band: usize) -> Option<&BandStatistics> {
        self.stats.get(band).and_then(|s| s.as_ref())
    }

    pub fn has_statistics(&self, band: usize) -> bool {
        self.statistics(band).is_some()
    }

    /// 计算波段统计值。大影像按步长抽样（`estimated = true`）。
    pub fn compute_statistics(
        &mut self,
        band: usize,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(f32),
    ) -> Result<BandStatistics, RasterError> {
        if band >= self.band_count() {
            return Err(RasterError::BadBand {
                index: band,
                count: self.band_count(),
            });
        }
        let nodata = self.meta.nodata;
        let total = u64::from(self.width()) * u64::from(self.height());
        let mut acc = stats::StatsAccumulator::new();
        let band = band as u16;
        if total > STATS_SAMPLE_PIXELS {
            // 大影像整幅降采样后再统计：TIFF 走概览、SAFE 走小波分辨率，
            // 只解码一两千万像素就够，避免逐窗全分辨率解码（L2A 的 10 m 波段
            // 全分辨率有 1.2 亿像素，单窗解码就会超出解码器的内存上限）。
            let (out_width, out_height) = statistics_target(self.width(), self.height());
            let (planes, _window) = self.read_display_source(
                PixelWindow::new(0, 0, self.width(), self.height()),
                out_width,
                out_height,
                &[usize::from(band)],
            )?;
            for plane in &planes {
                for &value in plane {
                    acc.add(f64::from(value), nodata);
                }
            }
            progress(1.0);
            let mut result = acc.finish();
            result.estimated = true;
            self.store_statistics(band, result)
        } else {
            let stride = ((total as f64 / 16_000_000f64).sqrt().ceil() as u32).max(1);
            let rows_per_window = (2048 * stride).max(1);
            let mut row = 0u32;
            while row < self.height() {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(RasterError::Cancelled);
                }
                let rows = rows_per_window.min(self.height() - row);
                let window = PixelWindow::new(0, row, self.width(), rows);
                let plane = self.read_window(window, usize::from(band))?;
                let width = usize::try_from(window.width).unwrap_or(0);
                for y in (0..usize::try_from(rows).unwrap_or(0)).step_by(stride as usize) {
                    let start = y * width;
                    let end = start + width;
                    if end > plane.len() {
                        break;
                    }
                    for &value in plane[start..end].iter().step_by(stride as usize) {
                        acc.add(f64::from(value), nodata);
                    }
                }
                row += rows;
                progress((row as f32 / self.height().max(1) as f32).min(1.0));
            }
            let mut result = acc.finish();
            result.estimated = stride > 1;
            self.store_statistics(band, result)
        }
    }

    /// 写回统计结果：槽位数由数据集构造时决定，这里做防御性取值。
    fn store_statistics(
        &mut self,
        band: u16,
        result: BandStatistics,
    ) -> Result<BandStatistics, RasterError> {
        if let Some(slot) = self.stats.get_mut(band as usize) {
            *slot = Some(result.clone());
        }
        if let Some(info) = self.meta.bands.get_mut(band as usize) {
            info.stored_stats = Some(result.clone());
        }
        self.statistics_version = self.statistics_version.wrapping_add(1);
        Ok(result)
    }

    /// 统计数据版本号：写入统计值时自增，绘制线程据此判断缓存的元数据快照是否过期。
    pub fn statistics_version(&self) -> u64 {
        self.statistics_version
    }

    // ---- 金字塔 ----

    pub fn has_overviews(&self) -> bool {
        self.overview_count() > 0
    }

    /// 构建外部概览文件 `<影像>.ovr`（GDAL / ArcGIS 兼容），返回构建的层级数量。
    ///
    /// SAFE 包自带 10 / 20 / 60 m 多分辨率网格，不需要金字塔：直接返回 0。
    pub fn build_overviews(
        &mut self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(f32),
    ) -> Result<usize, RasterError> {
        let path = self.path.clone();
        let written = match &mut self.source {
            DatasetSource::Tiff { base, .. } => pyramid::build(&path, base, cancel, progress)?,
            DatasetSource::Safe { .. } => return Ok(0),
        };
        // 写完之后当作「外部概览」重新打开 `.ovr`：与启动时打开别人用 GDAL 生成的
        // `.ovr` 走完全相同的路径，避免两套读取逻辑。
        let ovr_path = overview_path(&path);
        let external = tiff_file::TiffFile::open(&ovr_path)?;
        let levels = written
            .iter()
            .map(|level| LevelEntry {
                source: LevelSource::External,
                image: level.image,
                width: level.width,
                height: level.height,
                scale: level.scale,
            })
            .collect::<Vec<_>>();
        if let DatasetSource::Tiff { external: slot, .. } = &mut self.source {
            *slot = Some(external);
        }
        // 与打开时的顺序一致：按倍数排序、按尺寸去重，同时丢掉已有的外部层级。
        self.levels
            .retain(|level| level.source != LevelSource::External);
        self.levels.extend(levels);
        self.levels.sort_by(|a, b| a.scale.total_cmp(&b.scale));
        self.levels
            .dedup_by(|a, b| a.width == b.width && a.height == b.height);
        self.meta.has_external_overviews = true;
        Ok(self
            .levels
            .iter()
            .filter(|level| level.source == LevelSource::External)
            .count())
    }
}

/// 栅格图层：图层树中的一个叶子节点，共享一份惰性读取的数据集。
///
/// 数据集用 `Arc<Mutex<_>>` 共享：界面渲染与后台统计 / 金字塔构建可以同时持有一份，
/// 互斥锁在毫秒级的窗口读取期间持有，不会长时间阻塞界面。
#[derive(Clone)]
pub struct RasterLayer {
    pub id: u64,
    pub name: String,
    pub visible: bool,
    pub path: PathBuf,
    pub dataset: Arc<Mutex<RasterDataset>>,
    pub display: RasterDisplay,
    /// 缓存的包围盒（世界坐标；无地理参考时为像素坐标），避免每帧加锁。
    pub bounds: Aabb,
    pub projection: ProjectionInfo,
}

impl std::fmt::Debug for RasterLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RasterLayer")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("path", &self.path)
            .field("display", &self.display)
            .finish_non_exhaustive()
    }
}

impl RasterLayer {
    /// 打开栅格文件并建立图层（只读文件头，不加载像素）。
    pub fn open(path: impl AsRef<Path>, name: String) -> Result<Self, RasterError> {
        let path = path.as_ref().to_path_buf();
        let dataset = RasterDataset::open(&path)?;
        let bounds = dataset.bounds();
        let projection = dataset.georeference().crs.clone();
        let display =
            RasterDisplay::for_band_count(dataset.band_count(), dataset.band_count() == 4);
        Ok(Self {
            id: 0,
            name,
            visible: true,
            path,
            dataset: Arc::new(Mutex::new(dataset)),
            display,
            bounds,
            projection,
        })
    }

    /// 按指定显示设置重新组装（工程加载路径）。
    pub fn with_display(mut self, display: RasterDisplay) -> Self {
        self.display = display;
        self
    }

    /// 栅格文件 / 目录路径（后台解码线程据此各自打开一份数据集）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn dataset(&self) -> std::sync::MutexGuard<'_, RasterDataset> {
        // 互斥锁中毒只可能来自读取线程 panic；此时仍给出数据比让界面崩溃更安全。
        self.dataset.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// 不阻塞地尝试访问数据集：后台统计或解码线程正在持锁时返回 `None`。
    ///
    /// 绘制线程每帧都会读元数据，用 `try_lock` 可以避免被秒级的后台读取拖住界面。
    pub fn try_dataset(&self) -> Option<std::sync::MutexGuard<'_, RasterDataset>> {
        match self.dataset.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    /// 在后台线程中安全访问数据集（克隆 `Arc` 后传入）。
    pub fn handle(&self) -> Arc<Mutex<RasterDataset>> {
        Arc::clone(&self.dataset)
    }
}

/// 概览文件路径（`foo.tif` → `foo.tif.ovr`）。
pub fn overview_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".ovr");
    PathBuf::from(name)
}

fn build_metadata(
    info: &tiff_file::TiffInfo,
    geotiff: &geotiff::GeoTiffInfo,
    levels: &[LevelEntry],
) -> RasterMetadata {
    let mut bands = Vec::with_capacity(info.samples as usize);
    for index in 0..info.samples as usize {
        let description = geotiff
            .descriptions
            .get(index)
            .filter(|name| !name.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("Band_{}", index + 1));
        let color = color_interpretation(&info.photometric, index, info.samples as usize);
        let stored = geotiff
            .stats
            .get(index)
            .and_then(|s| s.as_ref())
            .map(|s| BandStatistics {
                min: s.min,
                max: s.max,
                mean: s.mean,
                std_dev: s.std_dev,
                valid_count: 0,
                from_file: true,
                estimated: false,
                histogram: Vec::new(),
                percent: None,
            });
        bands.push(BandInfo {
            index,
            name: description,
            color,
            bits: info.bits_per_sample,
            sample_type: info.sample_type,
            nodata: info.nodata,
            stored_stats: stored,
        });
    }

    let file_size = info.file_size;
    RasterMetadata {
        width: info.width,
        height: info.height,
        band_count: info.samples as usize,
        sample_type: info.sample_type,
        bits_per_sample: info.bits_per_sample,
        nodata: info.nodata,
        compression: info.compression,
        planar: info.planar,
        block: info.layout,
        block_width: info.block_width,
        block_height: info.block_height,
        blocks_across: info.blocks_across,
        blocks_down: info.blocks_down,
        photometric: info.photometric.clone(),
        file_size,
        big_tiff: info.big_tiff,
        internal_overviews: levels
            .iter()
            .filter(|l| l.source == LevelSource::Internal && l.scale > 1.0)
            .count(),
        has_external_overviews: false,
        bands,
    }
}

fn color_interpretation(photometric: &str, index: usize, samples: usize) -> ColorInterp {
    let has_alpha = samples == 4 && (photometric == "RGB" || photometric == "YCbCr")
        || samples == 2 && (photometric == "BlackIsZero" || photometric == "WhiteIsZero");
    match photometric {
        "RGB" | "YCbCr" => match index {
            0 => ColorInterp::Red,
            1 => ColorInterp::Green,
            2 => ColorInterp::Blue,
            3 if has_alpha => ColorInterp::Alpha,
            _ => ColorInterp::Undefined,
        },
        "BlackIsZero" | "WhiteIsZero" => match index {
            0 => ColorInterp::Gray,
            1 if has_alpha => ColorInterp::Alpha,
            _ => ColorInterp::Undefined,
        },
        _ => ColorInterp::Undefined,
    }
}

/// 为 SAFE 产品包构造元数据。波段顺序与 [`safe::SafePackage::bands`] 一一对应。
fn build_safe_metadata(package: &safe::SafePackage) -> RasterMetadata {
    let grid = package.base_grid();
    // 分块布局优先用 JPEG 2000 码流自带的 tile 尺寸（S2 的波段通常是整幅一块，
    // 也有按 1024×1024 分块的），拿不到时退回整幅一层。
    let (block_width, block_height, blocks_across, blocks_down) =
        package.files.iter().find_map(|file| file.tiles).map_or(
            (grid.cols, grid.rows, 1, 1),
            |(width, height, across, down)| (width, height, across.max(1), down.max(1)),
        );
    let bands = package
        .bands
        .iter()
        .enumerate()
        .map(|(index, band)| {
            let file = &package.files[band.file];
            let color = if file.components >= 3 {
                match band.component {
                    0 => ColorInterp::Red,
                    1 => ColorInterp::Green,
                    2 => ColorInterp::Blue,
                    _ => ColorInterp::Undefined,
                }
            } else {
                ColorInterp::Gray
            };
            BandInfo {
                index,
                name: band.label.clone(),
                color,
                bits: u16::from(file.bit_depth),
                sample_type: if file.bit_depth <= 8 {
                    SampleType::U8
                } else {
                    SampleType::U16
                },
                nodata: Some(0.0),
                stored_stats: None,
            }
        })
        .collect::<Vec<_>>();
    let file_size = package
        .files
        .iter()
        .filter_map(|file| std::fs::metadata(&file.path).ok())
        .map(|meta| meta.len())
        .sum();
    // 数据集级别按「最深的波段」报告位深与样本类型：L2A 的真彩色是 8 位，
    // 而光谱波段是 15 位，取最大值才能让内存估算与显示拉伸按 16 位处理。
    let deepest_bits = package
        .files
        .iter()
        .map(|file| u16::from(file.bit_depth))
        .max()
        .unwrap_or(16);
    RasterMetadata {
        width: grid.cols,
        height: grid.rows,
        band_count: bands.len(),
        sample_type: if deepest_bits <= 8 {
            SampleType::U8
        } else {
            SampleType::U16
        },
        bits_per_sample: deepest_bits,
        nodata: Some(0.0),
        compression: CompressionKind::Jpeg2000,
        planar: false,
        block: BlockLayout::Jpeg2000,
        block_width,
        block_height,
        blocks_across,
        blocks_down,
        photometric: if package.bands.len() >= 3 {
            "RGB（每个波段一个 JPEG 2000 文件）".to_owned()
        } else {
            "单波段".to_owned()
        },
        file_size,
        big_tiff: false,
        internal_overviews: 0,
        has_external_overviews: false,
        bands,
    }
}

/// 读取 SAFE 包的一段窗口（窗口坐标以 `level_shift` 对应虚拟层级为准）。
///
/// 按波段各自的分辨率把窗口换算成源像素范围，JPEG 2000 解码时按 2 的幂次
/// 降采样（最多 1/8），再把结果最近邻映射到请求的窗口尺寸。
fn read_safe_bands(
    package: &safe::SafePackage,
    cache: &mut jp2::Jp2Cache,
    window: PixelWindow,
    bands: &[usize],
    level_shift: u32,
) -> Result<Vec<Vec<f32>>, RasterError> {
    let mut planes = Vec::with_capacity(bands.len());
    for &band in bands {
        let info = package.bands.get(band).ok_or(RasterError::BadBand {
            index: band,
            count: package.bands.len(),
        })?;
        let file = package.files.get(info.file).ok_or_else(|| {
            RasterError::Corrupt(format!("SAFE 波段 {} 的文件索引越界", info.label))
        })?;
        planes.push(read_safe_plane(
            cache,
            file,
            info.component,
            window,
            level_shift,
        )?);
    }
    Ok(planes)
}

fn read_safe_plane(
    cache: &mut jp2::Jp2Cache,
    file: &safe::BandFile,
    component: u8,
    window: PixelWindow,
    level_shift: u32,
) -> Result<Vec<f32>, RasterError> {
    let output = window.area();
    let (out_w, out_h) = (window.width.max(1), window.height.max(1));
    // 层级坐标 → 基网格（10 m）像素坐标。
    let divisor = u64::from(1u32 << level_shift.min(8));
    let factor = u64::from(file.factor.max(1));
    let base_x = u64::from(window.x) * divisor;
    let base_y = u64::from(window.y) * divisor;
    let base_w = u64::from(window.width) * divisor;
    let base_h = u64::from(window.height) * divisor;
    // 基网格范围 → 该波段自身分辨率的像素范围。
    let x0 = (base_x / factor).min(u64::from(file.width)) as u32;
    let y0 = (base_y / factor).min(u64::from(file.height)) as u32;
    let x1 = base_x
        .saturating_add(base_w)
        .div_ceil(factor)
        .min(u64::from(file.width)) as u32;
    let y1 = base_y
        .saturating_add(base_h)
        .div_ceil(factor)
        .min(u64::from(file.height)) as u32;
    let roi = Rect {
        x: x0,
        y: y0,
        w: x1.saturating_sub(x0).max(1),
        h: y1.saturating_sub(y0).max(1),
    };
    // 源像素 / 输出像素 约为 factor / 2^shift：据此选小波降采样层数（2 的幂次）。
    let want = f64::from(roi.w) / f64::from(out_w);
    let levels = (want.max(1.0).log2().round() as u8).min(MAX_SAFE_REDUCTION_LEVELS);
    let decoded = jp2::decode_region(cache, &file.path, roi, levels)?;
    let mut plane = vec![0.0f32; usize::try_from(output).unwrap_or(0)];
    if decoded.width == 0 || decoded.height == 0 {
        return Ok(plane);
    }
    // 解码区域在源像素里的起点（`scaled_covering` 向下取整）。
    let denominator = f64::from(decoded.denominator.max(1));
    let decoded_x0 = f64::from(roi.x) / denominator;
    let decoded_y0 = f64::from(roi.y) / denominator;
    let channel = usize::from(component);
    let last_x = decoded.width - 1;
    let last_y = decoded.height - 1;
    for row in 0..out_h {
        // 输出像素中心 → 基网格像素 → 波段像素 → 解码像素。
        let source_y = source_index(
            window.y,
            row,
            divisor,
            file.factor,
            denominator,
            decoded_y0,
            last_y,
        );
        for column in 0..out_w {
            let source_x = source_index(
                window.x,
                column,
                divisor,
                file.factor,
                denominator,
                decoded_x0,
                last_x,
            );
            plane[row as usize * out_w as usize + column as usize] =
                decoded.value(source_x, source_y, channel);
        }
    }
    Ok(plane)
}

/// 层级窗口里的第 `index` 个输出像素中心 → 解码区域里的像素下标。
///
/// `window_start` 是**层级像素**（已按 `1 / divisor` 缩小），因此它和步长一样要乘
/// `divisor` 才换算成基网格像素；漏乘会让所有行/列都落到解码区域的同一行上，屏幕上
/// 表现为整片竖条纹（曾经真实发生过：缩放层级 `level_shift > 0` 时才暴露）。
fn source_index(
    window_start: u32,
    index: u32,
    divisor: u64,
    factor: u32,
    denominator: f64,
    decoded_start: f64,
    last: u32,
) -> u32 {
    let base = (f64::from(window_start) + f64::from(index) + 0.5) * divisor.max(1) as f64;
    (base / f64::from(factor.max(1)) / denominator - decoded_start)
        .floor()
        .clamp(0.0, f64::from(last)) as u32
}

/// 最近邻重采样：把 `window` 范围内的 `plane` 映射到 `out_w × out_h`。
#[allow(dead_code)]
fn resample_nearest(plane: &[f32], window: PixelWindow, out_w: u32, out_h: u32) -> Vec<f32> {
    let (out_w, out_h) = (out_w as usize, out_h as usize);
    let area = out_w.saturating_mul(out_h);
    if plane.is_empty() || window.is_empty() {
        return vec![0.0f32; area];
    }
    let src_w = window.width as usize;
    // 索引表按「半像素中心对齐」预先生成：逐像素只做一次查表，不再做浮点乘法。
    let columns = nearest_indexes(window.width, out_w as u32);
    let rows = nearest_indexes(window.height, out_h as u32);
    let mut out = Vec::with_capacity(area);
    for row in &rows {
        let Some(src) = plane.get(*row as usize * src_w..) else {
            continue;
        };
        if columns.len() == src_w {
            let len = src_w.min(src.len());
            out.extend_from_slice(&src[..len]);
            continue;
        }
        for column in &columns {
            out.push(src.get(*column as usize).copied().unwrap_or(0.0));
        }
    }
    out.resize(area, 0.0);
    out
}

/// 最近邻采样索引表（输出像素下标 → 源像素下标）。
pub(crate) fn nearest_indexes(src: u32, out: u32) -> Vec<u32> {
    if src == 0 || out == 0 {
        return Vec::new();
    }
    let scale = f64::from(src) / f64::from(out);
    (0..out)
        .map(|index| {
            let position = (f64::from(index) + 0.5) * scale;
            (position.floor() as u32).min(src - 1)
        })
        .collect()
}

/// 把字节数格式化为人类可读的字符串。
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_window_clamps() {
        let window = PixelWindow::new(10, 10, 100, 100).clamp_to(50, 40);
        assert_eq!(window, PixelWindow::new(10, 10, 40, 30));
        assert!(PixelWindow::new(80, 80, 10, 10).clamp_to(50, 40).is_empty());
    }

    #[test]
    fn affine_roundtrip() {
        let affine = Affine::new([100.0, 2.0, 0.0, 200.0, 0.0, -2.0]);
        let world = affine.apply(10.0, 20.0);
        assert!((world[0] - 120.0).abs() < 1e-9);
        assert!((world[1] - 160.0).abs() < 1e-9);
        let pixel = affine.inverse(world[0], world[1]).unwrap();
        assert!((pixel[0] - 10.0).abs() < 1e-9);
        assert!((pixel[1] - 20.0).abs() < 1e-9);
    }

    #[test]
    fn horizontal_quantile_between_bounds() {
        let mut stats = BandStatistics {
            min: 0.0,
            max: 100.0,
            histogram: vec![1; 100],
            ..Default::default()
        };
        stats.percent = Some((1.0, 99.0));
        let (low, high) = stats.clipped(0.5);
        assert!(low > 0.0 && low < 50.0);
        assert!(high > 50.0 && high < 100.0);
    }

    #[test]
    fn compression_kind_maps_codes() {
        assert!(matches!(
            CompressionKind::from_code(5),
            CompressionKind::Lzw
        ));
        assert!(!CompressionKind::from_code(9999).is_readable());
    }

    #[test]
    fn statistics_target_caps_the_long_side() {
        // 小影像原样返回，不做无谓的重采样。
        assert_eq!(statistics_target(1000, 500), (1000, 500));
        // 大影像长边压到 2048，短边按比例缩放：L2A 的 10980² 全幅要降到这一步，
        // 否则单窗全分辨率解码会超出 JPEG 2000 解码器的内存上限。
        assert_eq!(statistics_target(10980, 10980), (2048, 2048));
        assert_eq!(statistics_target(10980, 5490), (2048, 1024));
    }

    #[test]
    fn level_window_offset_scales_with_divisor() {
        // 层级窗口从第 269 个层级像素开始（÷8 的层级），解码降采样分母也是 8：
        // 第 0 行必须正好落在解码区域第 0 行，而不是被夹到第 0 行后整列复制。
        assert_eq!(source_index(269, 0, 8, 1, 8.0, 269.0, 583), 0);
        assert_eq!(source_index(269, 1, 8, 1, 8.0, 269.0, 583), 1);
        // 起点漏乘 divisor 时每一行都会落到同一行（深度放大时整屏竖条纹）。
        assert_eq!(source_index(269, 200, 8, 1, 8.0, 269.0, 583), 200);
    }
}
