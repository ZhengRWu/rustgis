//! JPEG 2000 波段文件的读取封装（Sentinel-2 SAFE 的像素来源）。
//!
//! `j2k` 解码器按「源区域 + 缩放倍率」解码：SAFE 的 10 m 波段是 10980 × 10980 的
//! 单文件，全图解码要几百 MB，而显示只需要视窗内的像素。这里把「文件字节缓存」
//! 和「区域解码」两件事分开：
//!
//! * [`Jp2Cache`] 按需读入整份码流并做 LRU 淘汰（解码器要求完整切片）；
//! * [`decode_region`] 只取出请求区域，并按 2 的幂次（1/2、1/4、1/8）降采样，
//!   减少大规模缩小时的解码量。取样走「分量平面」，避免打包像素格式对
//!   色彩空间的限制（`j2k` 的打包路径只接受 Gray / RGB）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use j2k::{J2kDecoder, J2kNativeComponentPlane, Rect};

use super::RasterError;

/// 同时缓存几个 JP2 文件的码流。10 m 波段单文件约 20～30 MB，
/// 保留 6 个足够覆盖一次 RGB 合成的三个 10 m 波段加若干 20 m / 60 m 波段。
const MAX_CACHED_FILES: usize = 6;

/// 探测文件头时读入的字节数：JP2 的 `jp2h` 与码流主头部都在最前面。
const HEADER_PROBE_BYTES: usize = 64 * 1024;

/// JPEG 2000 文件的基本信息。
#[derive(Debug, Clone, Copy)]
pub(crate) struct Jp2Header {
    pub width: u32,
    pub height: u32,
    pub components: u8,
    pub bit_depth: u8,
    /// 分块布局 `(块宽, 块高, 横向块数, 纵向块数)`；不是分块码流时为 `None`。
    pub tiles: Option<(u32, u32, u32, u32)>,
}

/// 读取 JP2 / J2K 文件的头部信息（不解码像素）。
pub(crate) fn inspect(path: &Path) -> Result<Jp2Header, RasterError> {
    let prefix = read_prefix(path, HEADER_PROBE_BYTES)?;
    if let Ok(info) = J2kDecoder::inspect(&prefix) {
        return Ok(Jp2Header {
            width: info.dimensions.0,
            height: info.dimensions.1,
            components: u8::try_from(info.components).unwrap_or(1).max(1),
            bit_depth: info.bit_depth,
            tiles: info.tile_layout.map(|layout| {
                (
                    layout.tile_width,
                    layout.tile_height,
                    layout.tiles_x,
                    layout.tiles_y,
                )
            }),
        });
    }
    match parse_ihdr(&prefix) {
        Some(header) => Ok(header),
        None => Err(RasterError::Corrupt(format!(
            "无法解析 JPEG 2000 文件头：{}",
            path.display()
        ))),
    }
}

/// 从 JP2 的 `ihdr` 盒子里读出尺寸与位深（j2k 的解析器在截断切片上可能失败）。
fn parse_ihdr(bytes: &[u8]) -> Option<Jp2Header> {
    let mut offset = 0usize;
    while offset + 8 <= bytes.len() {
        let length = u32::from_be_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?) as usize;
        let kind = bytes.get(offset + 4..offset + 8)?;
        let (body_start, body_len) = match length {
            1 => {
                // 64 位扩展长度。
                let extended =
                    u64::from_be_bytes(bytes.get(offset + 8..offset + 16)?.try_into().ok()?);
                (
                    offset + 16,
                    usize::try_from(extended).ok()?.saturating_sub(16),
                )
            }
            0 => (offset + 8, bytes.len().saturating_sub(offset + 8)),
            other => (offset + 8, other.saturating_sub(8)),
        };
        if kind == b"jp2h" {
            let body = bytes.get(body_start..body_start.saturating_add(body_len))?;
            return parse_ihdr_body(body);
        }
        // 码流盒（`jp2c`）：没有 `jp2h` 时不再往后找。
        if kind == b"jp2c" {
            return None;
        }
        offset = body_start.checked_add(body_len)?;
    }
    None
}

fn parse_ihdr_body(body: &[u8]) -> Option<Jp2Header> {
    let mut offset = 0usize;
    while offset + 8 <= body.len() {
        let length = u32::from_be_bytes(body.get(offset..offset + 4)?.try_into().ok()?) as usize;
        let kind = body.get(offset + 4..offset + 8)?;
        if kind == b"ihdr" {
            let payload = body.get(offset + 8..offset + 8 + 10)?;
            let height = u32::from_be_bytes(payload.get(0..4)?.try_into().ok()?);
            let width = u32::from_be_bytes(payload.get(4..8)?.try_into().ok()?);
            let components = u16::from_be_bytes(payload.get(8..10)?.try_into().ok()?);
            let bpc = *body.get(offset + 18)?;
            // bpc 为 0xFF 表示各分量位深不一致，取常见的 16 位。
            let bit_depth = if bpc == 0xFF { 16 } else { (bpc & 0x7F) + 1 };
            return Some(Jp2Header {
                width,
                height,
                components: u8::try_from(components).unwrap_or(1).max(1),
                bit_depth,
                // 走 ihdr 兜底时没有 SIZ 盒，无法得知分块布局。
                tiles: None,
            });
        }
        offset = offset.checked_add(length.max(8))?;
    }
    None
}

fn read_prefix(path: &Path, limit: usize) -> Result<Vec<u8>, RasterError> {
    use std::io::Read;
    let file = std::fs::File::open(path)
        .map_err(|error| RasterError::Io(format!("{}：{error}", path.display())))?;
    let mut buffer = Vec::new();
    file.take(limit as u64)
        .read_to_end(&mut buffer)
        .map_err(|error| RasterError::Io(format!("{}：{error}", path.display())))?;
    Ok(buffer)
}

/// 解码结果：每个分量一张平面，样本已转成无符号整数。
///
/// 所有平面共用同一个区域坐标系（左上角对齐解码区域），索引由 [`Self::value`]
/// 换算，调用方不必关心分量采样因子。
pub(crate) struct DecodedRegion {
    pub width: u32,
    pub height: u32,
    /// 实际使用的缩小分母（1 = 全分辨率）：请求的层数可能被码流的分辨率阶梯截断，
    /// 调用方必须按这个值把解码坐标映射回源像素。
    pub denominator: u32,
    planes: Vec<SamplePlane>,
}

/// 单个分量的解码平面。
struct SamplePlane {
    width: u32,
    height: u32,
    /// 8 位与 16 位样本统一存成 `u16`（16 位深影像不会丢精度）。
    samples: Vec<u16>,
}

impl DecodedRegion {
    /// 取某个分量的采样值（坐标相对解码区域左上角）。
    pub fn value(&self, x: u32, y: u32, channel: usize) -> f32 {
        let Some(plane) = self
            .planes
            .get(channel.min(self.planes.len().saturating_sub(1)))
        else {
            return 0.0;
        };
        if x >= plane.width || y >= plane.height {
            return 0.0;
        }
        plane.samples[y as usize * plane.width as usize + x as usize] as f32
    }
}

/// JP2 码流缓存（按文件路径保存字节，LRU 淘汰）。
pub(crate) struct Jp2Cache {
    files: HashMap<PathBuf, Arc<Vec<u8>>>,
    order: Vec<PathBuf>,
}

impl Jp2Cache {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
            order: Vec::new(),
        }
    }

    fn bytes(&mut self, path: &Path) -> Result<Arc<Vec<u8>>, RasterError> {
        let cached = self.files.get(path).cloned();
        if let Some(bytes) = cached {
            self.touch(path);
            return Ok(bytes);
        }
        let bytes = Arc::new(
            std::fs::read(path)
                .map_err(|error| RasterError::Io(format!("{}：{error}", path.display())))?,
        );
        self.files.insert(path.to_path_buf(), Arc::clone(&bytes));
        self.order.push(path.to_path_buf());
        while self.order.len() > MAX_CACHED_FILES {
            let stale = self.order.remove(0);
            self.files.remove(&stale);
        }
        Ok(bytes)
    }

    fn touch(&mut self, path: &Path) {
        if let Some(index) = self.order.iter().position(|candidate| candidate == path) {
            let entry = self.order.remove(index);
            self.order.push(entry);
        }
    }
}

/// 解码 `roi`（源像素坐标）并按 2 的幂次 `levels` 降采样（0 = 全分辨率）。
///
/// 走「分量平面」而不是打包像素格式：`j2k` 的打包路径只接受 `Gray` / `RGB`
/// 色彩空间，Sentinel-2 之外（以及 ICC 标记过的码流）会被直接拒绝；
/// 分量平面按分量原样返回，采样因子与位深都保留。
///
/// 层数按「需要多少就取多少」给：JPEG 2000 的每一级小波分辨率只解出对应频带，
/// 缩得越狠解出的码块越少，因此大幅缩小时取深层能把解码时间按面积压下来。
pub(crate) fn decode_region(
    cache: &mut Jp2Cache,
    path: &Path,
    roi: Rect,
    levels: u8,
) -> Result<DecodedRegion, RasterError> {
    let bytes = cache.bytes(path)?;
    let mut decoder = J2kDecoder::new(bytes.as_slice())
        .map_err(|error| RasterError::Corrupt(format!("{}：{error}", path.display())))?;
    let (frame_width, frame_height) = decoder.info().dimensions;
    // 码流自带的小波分辨率阶梯有限：层数不能超过它声明的层数（最低一层是缩略图）。
    let available = decoder.info().resolution_levels.saturating_sub(1);
    let levels = levels.min(available).min(MAX_REDUCTION_LEVELS);
    let denominator = 1u32 << levels;
    let scaled = scaled_covering(roi, denominator);
    if scaled.w == 0 || scaled.h == 0 {
        return Ok(DecodedRegion {
            width: 0,
            height: 0,
            denominator: 1,
            planes: Vec::new(),
        });
    }
    // 降采样只能整幅取出某一层小波分辨率，区域解码则只能取全分辨率；
    // 区域足够大时整幅降采样反而更省，取两者中便宜的那个。
    let reduced_area = u64::from(frame_width.div_ceil(denominator))
        * u64::from(frame_height.div_ceil(denominator));
    let region_area = u64::from(roi.w) * u64::from(roi.h);
    let use_reduction = levels > 0 && reduced_area <= region_area;

    if use_reduction {
        // 降采样失败（分辨率阶梯不足、后端拒绝该层）时退回全分辨率区域解码，
        // 正确性优先：宁可多解码，也不能让显示变得没图。
        match decoder.decode_native_components_at_reduction(levels) {
            Ok(decoded) => {
                let planes = decoded
                    .planes()
                    .iter()
                    .map(|plane| crop_plane(plane, scaled))
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(DecodedRegion {
                    width: scaled.w,
                    height: scaled.h,
                    denominator,
                    planes,
                });
            }
            Err(_) => {
                decoder = J2kDecoder::new(bytes.as_slice()).map_err(|error| {
                    RasterError::Corrupt(format!("{}：{error}", path.display()))
                })?;
            }
        }
    }
    // 大区域切成条带解码：`j2k` 会保留整块解码后的分量，一次要的像素太多时
    // 它会以自己的 512 MB 上限拒绝（真实 L2A 的 10 m 三分量波段很容易触发）。
    let components = usize::from(decoder.info().components.max(1));
    let row_bytes = u64::from(roi.w) * components as u64 * 2;
    let rows_per_strip = (MAX_STRIP_BYTES / row_bytes.max(1))
        .max(1)
        .min(u64::from(roi.h));
    if rows_per_strip >= u64::from(roi.h) {
        let planes = decode_full_region(&mut decoder, roi, path)?;
        return Ok(DecodedRegion {
            width: roi.w,
            height: roi.h,
            denominator: 1,
            planes,
        });
    }
    let mut planes: Vec<SamplePlane> = Vec::new();
    let mut row = 0u32;
    while row < roi.h {
        let rows = u32::try_from(rows_per_strip).unwrap_or(1).min(roi.h - row);
        let strip = Rect {
            x: roi.x,
            y: roi.y + row,
            w: roi.w,
            h: rows,
        };
        // 每条带用新解码器，避免上一条带的解码状态继续占用内存。
        let mut strip_decoder = J2kDecoder::new(bytes.as_slice())
            .map_err(|error| RasterError::Corrupt(format!("{}：{error}", path.display())))?;
        let decoded = decode_full_region(&mut strip_decoder, strip, path)?;
        for (index, plane) in decoded.into_iter().enumerate() {
            if index >= planes.len() {
                planes.push(SamplePlane {
                    width: roi.w,
                    height: roi.h,
                    samples: vec![0; roi.w as usize * roi.h as usize],
                });
            }
            let target = &mut planes[index];
            let width = plane.width.min(target.width) as usize;
            for line in 0..plane.height.min(rows) {
                let source = line as usize * plane.width as usize;
                let destination = (row + line) as usize * target.width as usize;
                target.samples[destination..destination + width]
                    .copy_from_slice(&plane.samples[source..source + width]);
            }
        }
        row += rows;
    }
    Ok(DecodedRegion {
        width: roi.w,
        height: roi.h,
        denominator: 1,
        planes,
    })
}

/// 允许使用的最深小波分辨率层数（÷256）：再深就只剩缩略图，没有显示价值。
const MAX_REDUCTION_LEVELS: u8 = 8;

/// `roi` 按 `1 / denominator` 缩小后覆盖的像素范围（与 `j2k::Rect::scaled_covering` 一致）。
fn scaled_covering(roi: Rect, denominator: u32) -> Rect {
    let denominator = denominator.max(1);
    let x0 = roi.x / denominator;
    let y0 = roi.y / denominator;
    let x1 = roi.x.saturating_add(roi.w).div_ceil(denominator);
    let y1 = roi.y.saturating_add(roi.h).div_ceil(denominator);
    Rect {
        x: x0,
        y: y0,
        w: x1.saturating_sub(x0),
        h: y1.saturating_sub(y0),
    }
}

/// 单次 `j2k` 解码请求的最大输出字节数（条带切分阈值）。
///
/// `j2k` 自己还会额外保留解码后的分量，实测约为输出量的 2 倍，因此这里取
/// 64 MB，留出足够余量不触及它的 512 MB 上限。
const MAX_STRIP_BYTES: u64 = 64 * 1024 * 1024;

/// 解码一整块 `roi` 并裁成紧凑平面。
fn decode_full_region(
    decoder: &mut J2kDecoder<'_>,
    roi: Rect,
    path: &Path,
) -> Result<Vec<SamplePlane>, RasterError> {
    let decoded = decoder
        .decode_native_region_components(roi)
        .map_err(|error| RasterError::Corrupt(format!("{}：{error}", path.display())))?;
    decoded
        .planes()
        .iter()
        .map(|plane| {
            crop_plane(
                plane,
                Rect {
                    x: 0,
                    y: 0,
                    w: plane.dimensions().0,
                    h: plane.dimensions().1,
                },
            )
        })
        .collect()
}

/// 从整幅解码平面里裁出 `rect`（缩放后坐标系）。
fn crop_plane(plane: &J2kNativeComponentPlane, rect: Rect) -> Result<SamplePlane, RasterError> {
    let (plane_width, plane_height) = plane.dimensions();
    let x0 = rect.x.min(plane_width);
    let y0 = rect.y.min(plane_height);
    let width = rect.w.min(plane_width.saturating_sub(x0));
    let height = rect.h.min(plane_height.saturating_sub(y0));
    let bytes_per_sample = usize::from(plane.bytes_per_sample().max(1));
    let signed = plane.signed();
    let data = plane.data();
    let mut samples = Vec::with_capacity(width as usize * height as usize);
    for row in 0..height {
        let start = (y0 as usize + row as usize) * plane_width as usize * bytes_per_sample;
        for column in 0..width {
            let offset = start + (x0 as usize + column as usize) * bytes_per_sample;
            let sample = match (
                bytes_per_sample,
                data.get(offset..offset + bytes_per_sample),
            ) {
                (2, Some(pair)) => u16::from_le_bytes([pair[0], pair[1]]),
                (_, Some(byte)) => u16::from(byte[0]),
                _ => 0,
            };
            // 有符号分量（少见）按 `i16` 解释，负值截到 0。
            let sample = if signed && bytes_per_sample == 2 {
                i16::from_le_bytes(sample.to_le_bytes()).max(0) as u16
            } else {
                sample
            };
            samples.push(sample);
        }
    }
    Ok(SamplePlane {
        width,
        height,
        samples,
    })
}
