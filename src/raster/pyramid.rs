//! 概览金字塔：按 GDAL / ArcGIS 的约定写出外部概览文件 `<影像>.ovr`。
//!
//! 与 GDAL `gdaladdo` 保持一致：
//! - 文件名是「数据集路径 + `.ovr`」（`foo.tif` → `foo.tif.ovr`），文件里只有概览 IFD；
//! - 每级按 2 的幂逐级减半（2×2 均值），直到短边不大于 256 像素，最多 8 级；
//! - 每级一个 IFD，`NewSubfileType = 1`（缩略图），波段数与样本类型跟原图一致；
//! - 默认 LZW 压缩、条带存储，QGIS / ArcGIS / GDAL 都能直接读。
//!
//! 反过来，我们自己读概览也走同一条链路（`open_tiff` 里的 `LevelSource::External`），
//! 所以别人用 GDAL 生成的 `.ovr` 我们同样能用。

use std::fs::File;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use tiff::encoder::colortype::{
    ColorType, Gray8, Gray16, Gray32, Gray32Float, Gray64, Gray64Float, GrayI8, GrayI16, GrayI32,
    GrayI64, RGB8, RGB16, RGB32, RGB32Float, RGB64Float, RGBA8, RGBA16, RGBA32, RGBA32Float,
    RGBA64Float,
};
use tiff::encoder::{Compression, TiffEncoder};
use tiff::tags::Tag;

use super::tiff_file::{TiffFile, TiffInfo};
use super::{PixelWindow, RasterError, SampleType, overview_path};

/// 概览最多 8 级（1/2 … 1/256），短边不大于 256 像素后停止。
const MAX_LEVELS: usize = 8;
const MIN_SIZE: u32 = 256;
/// 每条带的输出行数：太小会让 IFD 里的条带偏移表暴涨，太大又占内存。
const ROWS_PER_STRIP: u32 = 64;
/// 整层缓冲的上限：`tiff` crate 只在「一次写完一整幅」的接口里做压缩，
/// 超过这个尺寸的层就退化成未压缩的流式条带（格式相同，只是文件更大）。
const MAX_BUFFERED_BYTES: u64 = 256 * 1024 * 1024;

/// 一级概览：在 `.ovr` 文件里的 IFD 序号、尺寸与相对原始分辨率的缩小倍数。
#[derive(Debug, Clone, Copy)]
pub(crate) struct OverviewLevel {
    pub image: usize,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

/// 一层的构建计划：目标尺寸 + 数据来源尺寸。
#[derive(Debug, Clone, Copy)]
struct Plan {
    width: u32,
    height: u32,
    src_width: u32,
    src_height: u32,
    scale: f64,
}

/// 构建外部概览 `<data_path>.ovr`，返回写入的层级清单。
///
/// `base` 是已打开的原始影像；层级之间逐级减半（第 2 级从第 1 级算起），
/// 这样整幅影像只解码一次，中间层也不用常驻内存（写完立刻从 `.ovr` 读回来当下一级的输入）。
pub(crate) fn build(
    data_path: &Path,
    base: &mut TiffFile,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<Vec<OverviewLevel>, RasterError> {
    let info = base.info.clone();
    if info.width == 0 || info.height == 0 || info.samples == 0 {
        return Err(RasterError::Corrupt("影像尺寸或波段数非法".into()));
    }
    let format = format_for(&info)?;

    // 1. 规划层级：逐级减半，直到短边不大于 256。
    let base_width = f64::from(info.width);
    let (mut width, mut height) = (info.width, info.height);
    let mut plans = Vec::new();
    while plans.len() < MAX_LEVELS && (width > MIN_SIZE || height > MIN_SIZE) {
        let src_width = width;
        let src_height = height;
        width = width.div_ceil(2);
        height = height.div_ceil(2);
        plans.push(Plan {
            width,
            height,
            src_width,
            src_height,
            scale: base_width / f64::from(width.max(1)),
        });
    }
    if plans.is_empty() {
        return Err(RasterError::Unsupported(
            "影像尺寸已小于概览最小尺寸（256 像素），无需构建".into(),
        ));
    }

    // 2. 写文件：一个文件、每级一个 IFD。先写临时文件，成功后再改名，
    //    这样中途取消/出错不会留下半截 `.ovr` 被下次打开误当成概览。
    let final_path = overview_path(data_path);
    let ovr_path = temp_overview_path(data_path);
    let temp = TempOverview::new(ovr_path.clone());
    // `tiff` crate 的压缩只在整幅写盘（`ImageEncoder::write_data`）时生效，手工写条带
    // 永远是不压缩的；所以要么整层进内存换 LZW，要么退化成未压缩的流式条带。
    let compressed = plans
        .iter()
        .all(|plan| plan_bytes(plan, &format) <= MAX_BUFFERED_BYTES);
    let mut encoder = TiffEncoder::new(File::create(&ovr_path)?)?;
    if compressed {
        encoder = encoder.with_compression(Compression::Lzw);
    }
    let total_rows: u64 = plans.iter().map(|plan| u64::from(plan.src_height)).sum();
    let mut done_rows = 0u64;
    // 每个 IFD 的类型必须在编译期确定，所以按类型分支调用同一个泛型函数。
    macro_rules! write_format {
        ($($format:ident => $color:ty),* $(,)?) => {
            match format {
                $(
                    Format::$format => write_all::<_, $color>(
                        &mut encoder,
                        &ovr_path,
                        &plans,
                        base,
                        cancel,
                        compressed,
                        &mut done_rows,
                        total_rows,
                        progress,
                    )?,
                )*
            }
        };
    }
    let written = write_format!(
        Gray8 => Gray8,
        GrayI8 => GrayI8,
        Gray16 => Gray16,
        GrayI16 => GrayI16,
        Gray32 => Gray32,
        GrayI32 => GrayI32,
        Gray64 => Gray64,
        GrayI64 => GrayI64,
        Gray32Float => Gray32Float,
        Gray64Float => Gray64Float,
        Rgb8 => RGB8,
        Rgb16 => RGB16,
        Rgb32 => RGB32,
        Rgb32Float => RGB32Float,
        Rgb64Float => RGB64Float,
        Rgba8 => RGBA8,
        Rgba16 => RGBA16,
        Rgba32 => RGBA32,
        Rgba32Float => RGBA32Float,
        Rgba64Float => RGBA64Float,
    );
    // TiffEncoder 自身没有收尾步骤：每级 IFD 在 ImageEncoder::finish 时已写盘，
    // 这里只需保证文件句柄被关闭（drop）后再重新打开校验。
    drop(encoder);
    std::fs::rename(&ovr_path, &final_path)?;
    temp.disarm();
    progress(1.0);
    Ok(written)
}

/// 构建中的临时概览文件（`<影像>.ovr.tmp`），成功后在 [build] 里改名成正式文件。
fn temp_overview_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".ovr.tmp");
    PathBuf::from(name)
}

/// 半截文件守卫：构建失败 / 取消时自动删掉临时 `.ovr`。
struct TempOverview {
    path: PathBuf,
    keep: bool,
}

impl TempOverview {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn disarm(mut self) {
        self.keep = true;
    }
}

impl Drop for TempOverview {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// 按类型逐级写入；返回各层在文件里的 IFD 序号与尺寸。
#[allow(clippy::too_many_arguments)]
fn write_all<W: Write + Seek, C: OverviewColor>(
    encoder: &mut TiffEncoder<W>,
    ovr_path: &Path,
    plans: &[Plan],
    base: &mut TiffFile,
    cancel: &AtomicBool,
    compressed: bool,
    done_rows: &mut u64,
    total_rows: u64,
    progress: &mut dyn FnMut(f32),
) -> Result<Vec<OverviewLevel>, RasterError>
where
    C::Inner: OverviewSample,
    [C::Inner]: tiff::encoder::TiffValue,
{
    let mut written = Vec::with_capacity(plans.len());
    for (index, plan) in plans.iter().enumerate() {
        // 第一级读原图，之后读上一级写好的 IFD（已落盘，可以另开只读句柄）。
        let mut source = if index == 0 {
            Source::Base(base)
        } else {
            let previous = plans[index - 1];
            Source::Overview(Box::new(OverviewReader {
                file: TiffFile::open(ovr_path)?,
                image: index - 1,
                width: previous.width,
            }))
        };
        write_level::<W, C>(
            encoder,
            *plan,
            &mut source,
            cancel,
            compressed,
            done_rows,
            total_rows,
            progress,
        )?;
        written.push(OverviewLevel {
            image: index,
            width: plan.width,
            height: plan.height,
            scale: plan.scale,
        });
    }
    Ok(written)
}

/// 数据来源：原始影像，或刚写好的一级概览。
enum Source<'a> {
    Base(&'a mut TiffFile),
    Overview(Box<OverviewReader>),
}

/// 已落盘的上一级概览：重新打开同一个 `.ovr`，读回它的 IFD。
struct OverviewReader {
    file: TiffFile,
    image: usize,
    width: u32,
}

impl Source<'_> {
    fn read_rows(
        &mut self,
        y: u32,
        rows: u32,
        channels: u16,
    ) -> Result<Vec<Vec<f32>>, RasterError> {
        let bands: Vec<u16> = (0..channels).collect();
        match self {
            Self::Base(base) => {
                let width = base.info.width;
                base.read_window_multi(0, PixelWindow::new(0, y, width, rows), &bands)
            }
            Self::Overview(overview) => {
                let reader = &mut **overview;
                reader.file.read_window_multi(
                    reader.image,
                    PixelWindow::new(0, y, reader.width, rows),
                    &bands,
                )
            }
        }
    }
}

/// 写一层：2×2 均值降采样 + 按条带输出。
#[allow(clippy::too_many_arguments)]
fn write_level<W: Write + Seek, C: OverviewColor>(
    encoder: &mut TiffEncoder<W>,
    plan: Plan,
    source: &mut Source<'_>,
    cancel: &AtomicBool,
    compressed: bool,
    done_rows: &mut u64,
    total_rows: u64,
    progress: &mut dyn FnMut(f32),
) -> Result<(), RasterError>
where
    C::Inner: OverviewSample,
    [C::Inner]: tiff::encoder::TiffValue,
{
    let channels = C::CHANNELS;
    let src_width = plan.src_width as usize;
    let dst_width = plan.width as usize;
    let rows_per_strip = ROWS_PER_STRIP.min(plan.height.max(1));
    let strip_samples = rows_per_strip as usize * dst_width * channels;

    let mut image = encoder.new_image::<C>(plan.width, plan.height)?;
    image.rows_per_strip(rows_per_strip)?;
    // GDAL 会给概览 IFD 打 NewSubfileType = 1（缩略图），部分软件靠它识别概览。
    image.encoder().write_tag(Tag::NewSubfileType, 1u32)?;

    let mut strip: Vec<C::Inner> = Vec::with_capacity(strip_samples);
    // 压缩路径要整层一次性交给 `write_data`，未压缩路径靠 `strip` 流式写条带。
    let mut level: Vec<C::Inner> = if compressed {
        Vec::with_capacity(dst_width * plan.height as usize * channels)
    } else {
        Vec::new()
    };
    for dst_y in 0..plan.height {
        if cancel.load(Ordering::Relaxed) {
            return Err(RasterError::Cancelled);
        }
        let src_y0 = dst_y * 2;
        let rows = (plan.src_height - src_y0).min(2);
        let planes = source.read_rows(src_y0, rows, channels as u16)?;
        let mut down = Vec::with_capacity(planes.len());
        for plane in &planes {
            down.push(downsample_row(plane, src_width, rows as usize, dst_width));
        }
        for x in 0..dst_width {
            for row in &down {
                strip.push(C::Inner::from_f64(f64::from(row[x])));
            }
        }
        if compressed {
            level.extend_from_slice(&strip);
            strip.clear();
        } else if strip.len() >= strip_samples {
            image.write_strip(&strip)?;
            strip.clear();
        }
        *done_rows += u64::from(rows);
        progress((*done_rows as f32 / total_rows.max(1) as f32).min(1.0));
    }
    if compressed {
        // 整幅写盘：crate 会把整层按条带切好并做 LZW 压缩。
        image.write_data(&level)?;
    } else {
        if !strip.is_empty() {
            image.write_strip(&strip)?;
        }
        image.finish()?;
    }
    Ok(())
}

/// 一层的未压缩字节数，用来决定是否值得整层缓冲。
fn plan_bytes(plan: &Plan, format: &Format) -> u64 {
    let channels = format.channels() as u64;
    let sample = format.sample_bytes();
    u64::from(plan.width) * u64::from(plan.height) * channels * sample
}

/// 2×2 均值降采样一行：`source` 含 `rows`（1 或 2）行、`src_width` 列的样本。
fn downsample_row(source: &[f32], src_width: usize, rows: usize, dst_width: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(dst_width);
    for dst_x in 0..dst_width {
        let x0 = (dst_x * 2).min(src_width.saturating_sub(1));
        let x1 = (x0 + 1).min(src_width.saturating_sub(1));
        let mut sum = 0.0f64;
        let mut count = 0u32;
        for y in 0..rows {
            let row_start = y * src_width;
            for x in [x0, x1] {
                if let Some(value) = source.get(row_start + x) {
                    sum += f64::from(*value);
                    count += 1;
                }
            }
        }
        out.push(if count > 0 {
            (sum / f64::from(count)) as f32
        } else {
            0.0
        });
    }
    out
}

// ---------------------------------------------------------------------------
// 样本类型 → TIFF 色彩类型
// ---------------------------------------------------------------------------

/// 概览可写出的色彩类型（受 `tiff` 编码器的 colortype 限制）。
#[derive(Debug, Clone, Copy)]
enum Format {
    Gray8,
    GrayI8,
    Gray16,
    GrayI16,
    Gray32,
    GrayI32,
    Gray64,
    GrayI64,
    Gray32Float,
    Gray64Float,
    Rgb8,
    Rgb16,
    Rgb32,
    Rgb32Float,
    Rgb64Float,
    Rgba8,
    Rgba16,
    Rgba32,
    Rgba32Float,
    Rgba64Float,
}

impl Format {
    /// 每像素样本数。
    fn channels(self) -> usize {
        match self {
            Self::Gray8
            | Self::GrayI8
            | Self::Gray16
            | Self::GrayI16
            | Self::Gray32
            | Self::GrayI32
            | Self::Gray64
            | Self::GrayI64
            | Self::Gray32Float
            | Self::Gray64Float => 1,
            Self::Rgb8 | Self::Rgb16 | Self::Rgb32 | Self::Rgb32Float | Self::Rgb64Float => 3,
            Self::Rgba8 | Self::Rgba16 | Self::Rgba32 | Self::Rgba32Float | Self::Rgba64Float => 4,
        }
    }

    /// 单个样本的字节数。
    fn sample_bytes(self) -> u64 {
        match self {
            Self::Gray8 | Self::GrayI8 | Self::Rgb8 | Self::Rgba8 => 1,
            Self::Gray16 | Self::GrayI16 | Self::Rgb16 | Self::Rgba16 => 2,
            Self::Gray32
            | Self::GrayI32
            | Self::Gray32Float
            | Self::Rgb32
            | Self::Rgb32Float
            | Self::Rgba32
            | Self::Rgba32Float => 4,
            Self::Gray64
            | Self::GrayI64
            | Self::Gray64Float
            | Self::Rgb64Float
            | Self::Rgba64Float => 8,
        }
    }
}

/// 校验并把「波段数 + 位深 + 样本类型」映射到能写出的 TIFF 色彩类型。
fn format_for(info: &TiffInfo) -> Result<Format, RasterError> {
    let expected_bits = match info.sample_type {
        SampleType::U8 | SampleType::I8 => 8,
        SampleType::U16 | SampleType::I16 => 16,
        SampleType::U32 | SampleType::I32 | SampleType::F32 => 32,
        SampleType::U64 | SampleType::I64 | SampleType::F64 => 64,
    };
    if info.bits_per_sample != expected_bits {
        return Err(RasterError::Unsupported(format!(
            "暂不支持为 {} 位采样构建 .ovr 概览，请用 GDAL `gdaladdo` 生成",
            info.bits_per_sample
        )));
    }
    let signed = matches!(
        info.sample_type,
        SampleType::I8 | SampleType::I16 | SampleType::I32 | SampleType::I64
    );
    let format = match (info.samples, info.sample_type) {
        (1, SampleType::U8) => Format::Gray8,
        (1, SampleType::I8) => Format::GrayI8,
        (1, SampleType::U16) => Format::Gray16,
        (1, SampleType::I16) => Format::GrayI16,
        (1, SampleType::U32) => Format::Gray32,
        (1, SampleType::I32) => Format::GrayI32,
        (1, SampleType::U64) => Format::Gray64,
        (1, SampleType::I64) => Format::GrayI64,
        (1, SampleType::F32) => Format::Gray32Float,
        (1, SampleType::F64) => Format::Gray64Float,
        (3, SampleType::U8) => Format::Rgb8,
        (3, SampleType::U16) => Format::Rgb16,
        (3, SampleType::U32) => Format::Rgb32,
        (3, SampleType::F32) => Format::Rgb32Float,
        (3, SampleType::F64) => Format::Rgb64Float,
        (4, SampleType::U8) => Format::Rgba8,
        (4, SampleType::U16) => Format::Rgba16,
        (4, SampleType::U32) => Format::Rgba32,
        (4, SampleType::F32) => Format::Rgba32Float,
        (4, SampleType::F64) => Format::Rgba64Float,
        _ => {
            // TIFF 的 RGB / RGBA 色彩类型没有有符号整数版本，也没有 1/3/4 之外的组合。
            return Err(RasterError::Unsupported(format!(
                "暂不支持为 {} 波段{signed}整数影像构建 .ovr 概览，请用 GDAL `gdaladdo` 生成",
                info.samples,
                signed = if signed { "有符号" } else { "" }
            )));
        }
    };
    Ok(format)
}

// ---------------------------------------------------------------------------
// 数值转换
// ---------------------------------------------------------------------------

/// 概览写盘时的像素类型：`CHANNELS` 为每像素样本数。
trait OverviewColor: ColorType {
    const CHANNELS: usize;
}

/// 把 f64 均值转成目标样本类型（Rust 的 `as` 转换对越界值饱和，与 GDAL 的截断规则一致）。
trait OverviewSample: Copy {
    fn from_f64(value: f64) -> Self;
}

macro_rules! impl_sample {
    ($($ty:ty),*) => {
        $(
            impl OverviewSample for $ty {
                fn from_f64(value: f64) -> Self {
                    value.round() as $ty
                }
            }
        )*
    };
}

impl_sample!(u8, i8, u16, i16, u32, i32, u64, i64);

impl OverviewSample for f32 {
    fn from_f64(value: f64) -> Self {
        value as f32
    }
}

impl OverviewSample for f64 {
    fn from_f64(value: f64) -> Self {
        value
    }
}

macro_rules! impl_color {
    ($($ct:ty => $channels:expr),* $(,)?) => {
        $(
            impl OverviewColor for $ct {
                const CHANNELS: usize = $channels;
            }
        )*
    };
}

impl_color!(
    Gray8 => 1,
    GrayI8 => 1,
    Gray16 => 1,
    GrayI16 => 1,
    Gray32 => 1,
    GrayI32 => 1,
    Gray64 => 1,
    GrayI64 => 1,
    Gray32Float => 1,
    Gray64Float => 1,
    RGB8 => 3,
    RGB16 => 3,
    RGB32 => 3,
    RGB32Float => 3,
    RGB64Float => 3,
    RGBA8 => 4,
    RGBA16 => 4,
    RGBA32 => 4,
    RGBA32Float => 4,
    RGBA64Float => 4,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsample_averages_2x2() {
        // 2 行 × 4 列 → 2 列
        let source = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let row = downsample_row(&source, 4, 2, 2);
        assert_eq!(row.len(), 2);
        // (1+2+5+6)/4 = 3.5，(3+4+7+8)/4 = 5.5
        assert!((row[0] - 3.5).abs() < 1e-6);
        assert!((row[1] - 5.5).abs() < 1e-6);
    }

    #[test]
    fn downsample_handles_odd_last_row() {
        let source = [1.0, 2.0];
        let row = downsample_row(&source, 2, 1, 1);
        assert!((row[0] - 1.5).abs() < 1e-6);
    }

    #[test]
    fn sample_conversion_saturates() {
        assert_eq!(u8::from_f64(300.0), 255);
        assert_eq!(u8::from_f64(-4.0), 0);
        assert_eq!(u8::from_f64(2.5), 3);
        assert_eq!(i16::from_f64(-40_000.0), i16::MIN);
        assert_eq!(f32::from_f64(1.5), 1.5);
    }
}
