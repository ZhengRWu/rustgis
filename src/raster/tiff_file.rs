//! 底层 TIFF 解码：IFD 布局解析、strip / tile 按需解码与内存块缓存。

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use tiff::decoder::ifd::Value;
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

use super::{BlockLayout, CompressionKind, PixelWindow, RasterError, SampleType};

/// 块缓存默认上限（解码后的 f32 数据，192 MiB）。
const DEFAULT_CACHE_BYTES: usize = 192 * 1024 * 1024;

/// 单个 IFD（影像）的分块布局。
#[derive(Debug, Clone, Copy)]
pub(crate) struct LevelLayout {
    /// 文件内的 IFD 序号。
    pub image_index: usize,
    pub width: u32,
    pub height: u32,
    /// PlanarConfiguration = Planar（分平面存储）。
    pub planar: bool,
    pub layout: BlockLayout,
    pub block_width: u32,
    pub block_height: u32,
    pub blocks_across: u32,
    pub blocks_down: u32,
    /// 每像素采样数（波段数）。
    pub samples: u16,
}

impl LevelLayout {
    pub fn blocks_per_plane(&self) -> u32 {
        self.blocks_across.saturating_mul(self.blocks_down)
    }

    /// 块左上角像素坐标与有效宽高（边缘块被裁剪）。
    pub fn block_span(&self, block: u32) -> (u32, u32, u32, u32) {
        let col = block % self.blocks_across.max(1);
        let row = block / self.blocks_across.max(1);
        let x = col * self.block_width;
        let y = row * self.block_height;
        let width = self.block_width.min(self.width.saturating_sub(x));
        let height = self.block_height.min(self.height.saturating_sub(y));
        (x, y, width, height)
    }
}

/// 文件级元信息（只读一次）。
#[derive(Debug, Clone)]
pub(crate) struct TiffInfo {
    pub width: u32,
    pub height: u32,
    pub samples: u16,
    pub sample_type: SampleType,
    pub bits_per_sample: u16,
    pub compression: CompressionKind,
    pub planar: bool,
    pub layout: BlockLayout,
    pub block_width: u32,
    pub block_height: u32,
    pub blocks_across: u32,
    pub blocks_down: u32,
    pub photometric: String,
    pub nodata: Option<f64>,
    pub big_tiff: bool,
    pub file_size: u64,
    /// 所有 IFD（第 0 个为原始分辨率，其余为文件内概览）。
    pub levels: Vec<LevelLayout>,
}

/// 已解码的块：`data` 为行优先 f32。Chunky 时按样本交错，Planar 时只含一个样本。
struct Block {
    width: u32,
    height: u32,
    samples: u16,
    data: Vec<f32>,
}

impl Block {
    fn bytes(&self) -> usize {
        self.data.len() * std::mem::size_of::<f32>()
    }
}

/// 简易 LRU 块缓存。
struct BlockCache {
    map: HashMap<u64, Block>,
    order: VecDeque<u64>,
    bytes: usize,
    budget: usize,
}

impl BlockCache {
    fn new(budget: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            budget,
        }
    }

    fn contains(&self, key: u64) -> bool {
        self.map.contains_key(&key)
    }

    fn get(&mut self, key: u64) -> Option<&Block> {
        if !self.map.contains_key(&key) {
            return None;
        }
        touch(&mut self.order, key);
        self.map.get(&key)
    }

    fn insert(&mut self, key: u64, block: Block) {
        if self.map.contains_key(&key) {
            return;
        }
        let size = block.bytes();
        while self.bytes + size > self.budget && !self.order.is_empty() {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.map.remove(&old) {
                self.bytes = self.bytes.saturating_sub(removed.bytes());
            }
        }
        self.bytes += size;
        self.order.push_back(key);
        self.map.insert(key, block);
    }
}

fn touch(order: &mut VecDeque<u64>, key: u64) {
    if let Some(pos) = order.iter().position(|&item| item == key) {
        order.remove(pos);
    }
    order.push_back(key);
}

/// 打开后的 TIFF 文件，可按窗口 / 波段读取像素。
pub(crate) struct TiffFile {
    decoder: Decoder<BufReader<File>>,
    pub info: TiffInfo,
    /// 第 0 个 IFD 的全部标签快照（GeoTIFF 与 GDAL 元数据解析用）。
    pub tags: Vec<(u16, Value)>,
    cache: BlockCache,
    current_image: usize,
}

impl TiffFile {
    pub fn open(path: &Path) -> Result<Self, RasterError> {
        let meta = std::fs::metadata(path).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => RasterError::NotFound(path.to_path_buf()),
            _ => RasterError::Io(err.to_string()),
        })?;
        if !meta.is_file() {
            return Err(RasterError::NotFound(path.to_path_buf()));
        }
        let big_tiff = header_is_big_tiff(path)?;
        let file = File::open(path)?;
        let mut decoder = Decoder::new(BufReader::new(file))?;

        let mut levels: Vec<LevelLayout> = Vec::new();
        let mut tags: Vec<(u16, Value)> = Vec::new();
        let mut image_index = 0usize;
        loop {
            let layout = layout_of(&mut decoder, image_index)?;
            if image_index == 0 {
                tags = decoder
                    .tag_iter()
                    .filter_map(Result::ok)
                    .map(|(tag, value)| (tag.to_u16(), value))
                    .collect();
            }
            levels.push(layout);
            if !decoder.more_images() {
                break;
            }
            decoder.next_image()?;
            image_index += 1;
        }
        let base = *levels
            .first()
            .ok_or_else(|| RasterError::NotTiff("没有找到任何 IFD".into()))?;

        // 位深与采样格式从第 0 个 IFD 的标签快照读取（此时解码器已停在最后一个 IFD）。
        let bits_per_sample = tag_u16_vec(&tags, 258)
            .and_then(|v| v.first().copied())
            .unwrap_or(8);
        let sample_format = tag_u16_vec(&tags, 339)
            .and_then(|v| v.first().copied())
            .unwrap_or(1);
        let compression_code = tag_u16_vec(&tags, 259)
            .and_then(|v| v.first().copied())
            .unwrap_or(1);
        let photometric_code = tag_u16_vec(&tags, 262)
            .and_then(|v| v.first().copied())
            .unwrap_or(1);
        let nodata = tag_ascii(&tags, 42113).and_then(|text| {
            text.split(|c: char| c.is_whitespace() || c == ',')
                .find(|token| !token.is_empty())
                .and_then(|token| token.parse::<f64>().ok())
        });
        let compression = CompressionKind::from_code(compression_code);
        if !compression.is_readable() {
            return Err(RasterError::Unsupported(format!(
                "{}（压缩码 {compression_code}）",
                compression.label()
            )));
        }

        // 枚举结束时解码器停在最后一个 IFD。
        let current_image = levels.len().saturating_sub(1);
        Ok(Self {
            decoder,
            info: TiffInfo {
                width: base.width,
                height: base.height,
                samples: base.samples,
                sample_type: SampleType::from_tags(bits_per_sample, sample_format),
                bits_per_sample,
                compression,
                planar: base.planar,
                layout: base.layout,
                block_width: base.block_width,
                block_height: base.block_height,
                blocks_across: base.blocks_across,
                blocks_down: base.blocks_down,
                photometric: photometric_name(photometric_code).to_string(),
                nodata,
                big_tiff,
                file_size: meta.len(),
                levels,
            },
            tags,
            cache: BlockCache::new(DEFAULT_CACHE_BYTES),
            current_image,
        })
    }

    /// 读取指定 IFD（层级）的一段窗口，返回每个请求波段的 f32 平面。
    pub fn read_window_multi(
        &mut self,
        image_index: usize,
        window: PixelWindow,
        bands: &[u16],
    ) -> Result<Vec<Vec<f32>>, RasterError> {
        let level = *self
            .info
            .levels
            .iter()
            .find(|level| level.image_index == image_index)
            .ok_or_else(|| RasterError::Corrupt(format!("找不到 IFD #{image_index}")))?;
        let window = window.clamp_to(level.width, level.height);
        if window.is_empty() {
            return Err(RasterError::OutOfBounds {
                x: window.x,
                y: window.y,
                width: window.width,
                height: window.height,
            });
        }
        for &band in bands {
            if band >= level.samples {
                return Err(RasterError::BadBand {
                    index: band as usize,
                    count: level.samples as usize,
                });
            }
        }

        let width = usize::try_from(window.width).map_err(|_| RasterError::TooLarge {
            requested: u64::MAX,
            limit: 0,
        })?;
        let height = usize::try_from(window.height).map_err(|_| RasterError::TooLarge {
            requested: u64::MAX,
            limit: 0,
        })?;
        let area = width.checked_mul(height).ok_or(RasterError::TooLarge {
            requested: u64::MAX,
            limit: 0,
        })?;
        let mut out: Vec<Vec<f32>> = bands.iter().map(|_| vec![0.0f32; area]).collect();

        let block_width = level.block_width.max(1);
        let block_height = level.block_height.max(1);
        let col0 = window.x / block_width;
        let col1 = window.x.saturating_add(window.width - 1) / block_width;
        let row0 = window.y / block_height;
        let row1 = window.y.saturating_add(window.height - 1) / block_height;

        for row in row0..=row1 {
            for col in col0..=col1 {
                let block = row * level.blocks_across + col;
                for (plane_index, &band) in bands.iter().enumerate() {
                    self.ensure_block(&level, block, band)?;
                    let key = block_key(&level, block, band);
                    let Some(cached) = self.cache.get(key) else {
                        continue;
                    };
                    let (bx, by, _, _) = level.block_span(block);
                    let x0 = window.x.max(bx);
                    let y0 = window.y.max(by);
                    let x1 = window.x.saturating_add(window.width).min(bx + cached.width);
                    let y1 = window
                        .y
                        .saturating_add(window.height)
                        .min(by + cached.height);
                    let samples = cached.samples as usize;
                    if samples == 0 || x1 <= x0 || y1 <= y0 {
                        continue;
                    }
                    let cols = usize::try_from(x1 - x0).unwrap_or(0);
                    let src_col = usize::try_from(x0 - bx).unwrap_or(0);
                    let dst_col = usize::try_from(x0 - window.x).unwrap_or(0);
                    let Some(plane) = out.get_mut(plane_index) else {
                        continue;
                    };
                    // 交错块每行占「宽 × 采样数」个值，平面块每行只占「宽」个。
                    let row_stride = usize::try_from(cached.width).unwrap_or(0)
                        * if level.planar { 1 } else { samples };
                    // 逐行按切片拷贝：平移时窗口动辄上百万像素，逐像素取下标会明显拖慢。
                    for gy in y0..y1 {
                        let dst_start =
                            usize::try_from(gy - window.y).unwrap_or(0) * width + dst_col;
                        let dst_end = dst_start.saturating_add(cols);
                        let src_start = usize::try_from(gy - by).unwrap_or(0) * row_stride;
                        let Some(dst) = plane.get_mut(dst_start..dst_end) else {
                            continue;
                        };
                        let Some(src_row) = cached.data.get(src_start..) else {
                            continue;
                        };
                        if level.planar {
                            let start = src_col;
                            let end = start.saturating_add(cols);
                            if let Some(src) = src_row.get(start..end) {
                                dst.copy_from_slice(src);
                            }
                        } else {
                            // 交错存储：跳过其他采样，只取当前波段。
                            let start = src_col.saturating_mul(samples);
                            let Some(src) = src_row.get(start..) else {
                                continue;
                            };
                            for (slot, texel) in dst.iter_mut().zip(src.chunks_exact(samples)) {
                                if let Some(value) = texel.get(band as usize) {
                                    *slot = *value;
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn ensure_image(&mut self, image_index: usize) -> Result<(), RasterError> {
        if self.current_image == image_index {
            return Ok(());
        }
        self.decoder.seek_to_image(image_index)?;
        self.current_image = image_index;
        Ok(())
    }

    fn ensure_block(
        &mut self,
        level: &LevelLayout,
        block: u32,
        band: u16,
    ) -> Result<(), RasterError> {
        let key = block_key(level, block, band);
        if self.cache.contains(key) {
            return Ok(());
        }
        self.ensure_image(level.image_index)?;
        let chunk = if level.planar {
            block
                .checked_add(u32::from(band) * level.blocks_per_plane())
                .ok_or_else(|| RasterError::Corrupt("块索引溢出".into()))?
        } else {
            block
        };
        let (_, _, width, height) = level.block_span(block);
        let samples = if level.planar { 1 } else { level.samples };
        let mut data = match self.decoder.read_chunk(chunk) {
            Ok(result) => decoding_to_f32(result),
            // LZW 回退：tiff crate 对边缘块走「逐行小缓冲」读取路径，个别 LZW 流
            // 会在最后一小段读取时触发 weezl 的 NoProgress 误报（把已解出的字节
            // 一并丢弃，报 "no lzw end code found"，而流本身完整、带结束码）。
            // 回退路径用 weezl 直接整段解码，再自行裁剪与应用预测器。
            Err(err) if self.info.compression == CompressionKind::Lzw => {
                self.read_chunk_lzw_fallback(level, chunk)
                    .map_err(|fallback| {
                        // 回退也失败：保留原始错误（更贴近根因），并附上回退细节。
                        match fallback {
                            RasterError::Io(msg) => {
                                RasterError::Io(format!("{msg}（原始错误：{err}）"))
                            }
                            other => other,
                        }
                    })?
            }
            Err(err) => return Err(err.into()),
        };
        let expected = width as usize * height as usize * samples as usize;
        if data.len() < expected {
            return Err(RasterError::Corrupt(format!(
                "块 #{chunk} 解码得到 {} 个样本，少于期望的 {expected} 个",
                data.len()
            )));
        }
        data.truncate(expected);
        self.cache.insert(
            key,
            Block {
                width,
                height,
                samples,
                data,
            },
        );
        Ok(())
    }

    /// LZW 块的容错解码：绕过 tiff crate 的流式包装，直接按块偏移 / 字节计数
    /// 取出压缩字节，用 weezl 一次性解码到全块尺寸，再裁掉瓦片边缘填充。
    ///
    /// 与 `read_chunk` 的输出等价（含字节序修正与水平差分预测器）。
    fn read_chunk_lzw_fallback(
        &mut self,
        level: &LevelLayout,
        chunk: u32,
    ) -> Result<Vec<f32>, RasterError> {
        use std::io::{Read, Seek, SeekFrom};

        let (offsets_tag, counts_tag) = match level.layout {
            BlockLayout::Tiles => (Tag::TileOffsets, Tag::TileByteCounts),
            _ => (Tag::StripOffsets, Tag::StripByteCounts),
        };
        let offsets: Vec<u64> = self
            .decoder
            .find_tag_unsigned_vec(offsets_tag)?
            .ok_or_else(|| RasterError::Corrupt("缺少块偏移表".into()))?;
        let counts: Vec<u64> = self
            .decoder
            .find_tag_unsigned_vec(counts_tag)?
            .ok_or_else(|| RasterError::Corrupt("缺少块字节计数表".into()))?;
        let index = chunk as usize;
        let (offset, count) = match (offsets.get(index), counts.get(index)) {
            (Some(offset), Some(count)) => (*offset, *count),
            _ => {
                return Err(RasterError::Corrupt(format!(
                    "块 #{chunk} 的偏移 / 字节计数超出表范围"
                )))
            }
        };
        let bytes_per_sample = usize::try_from(self.info.sample_type.bytes()).unwrap_or(0);
        // 平面（Planar）块每样本一个平面；交错块含全部采样。
        let samples_in_chunk = usize::from(if level.planar { 1 } else { level.samples });
        let bps = usize::from(self.info.bits_per_sample);
        if bps != bytes_per_sample * 8 {
            return Err(RasterError::Unsupported(format!(
                "LZW 回退不支持 {bps} 位采样（{}）",
                self.info.sample_type.label()
            )));
        }

        // 取出原始压缩字节（`read_chunk` 每次都会先定位，所以这里移动读位置是安全的）。
        let reader = self.decoder.inner();
        reader
            .seek(SeekFrom::Start(offset))
            .map_err(|err| RasterError::Io(err.to_string()))?;
        let mut compressed = vec![0u8; usize::try_from(count).unwrap_or(0)];
        reader
            .read_exact(&mut compressed)
            .map_err(|err| RasterError::Io(err.to_string()))?;

        // 整段一次性解码：不经过 tiff 的逐行小缓冲包装。
        let mut decoder = weezl::decode::Configuration::with_tiff_size_switch(
            weezl::BitOrder::Msb,
            8,
        )
        .build();
        let mut decoded = decoder
            .decode(&compressed)
            .map_err(|err| RasterError::Io(format!("LZW 回退解码失败：{err}")))?;

        // 全块布局（边缘瓦片在 TIFF 里按完整尺寸编码，行尾带填充）。
        let full_row_bytes = usize::try_from(level.block_width).unwrap_or(0)
            * samples_in_chunk
            * bytes_per_sample;
        let (_, _, width, height) = level.block_span(chunk % level.blocks_per_plane());
        // 实际数据行宽（去掉行尾填充）。
        let data_row_bytes = usize::try_from(width).unwrap_or(0)
            * samples_in_chunk
            * bytes_per_sample;
        if full_row_bytes == 0 || data_row_bytes == 0 {
            return Err(RasterError::Corrupt(format!("块 #{chunk} 布局异常")));
        }

        // 水平差分预测器（Predictor = 2）：先做字节序修正，再逐行还原差分。
        let big_endian = self.decoder.byte_order() == tiff::tags::ByteOrder::BigEndian;
        if bps > 8 && big_endian {
            for chunk_bytes in decoded.chunks_exact_mut(bps / 8) {
                chunk_bytes.reverse();
            }
        }
        let predictor = self
            .decoder
            .find_tag_unsigned::<u16>(Tag::Predictor)
            .ok()
            .flatten()
            .unwrap_or(1);
        if predictor == 2 {
            apply_horizontal_predictor(&mut decoded, full_row_bytes, bps, samples_in_chunk)?;
        }

        // 裁掉行尾填充：从全块行布局里抽取每行的前 data_row_bytes 字节。
        let mut trimmed = Vec::with_capacity(data_row_bytes * usize::try_from(height).unwrap_or(0));
        for row in 0..usize::try_from(height).unwrap_or(0) {
            let start = row * full_row_bytes;
            let Some(src) = decoded.get(start..start + data_row_bytes) else {
                return Err(RasterError::Corrupt(format!(
                    "块 #{chunk} LZW 回退解码得到 {} 字节，少于裁剪所需的 {} 字节",
                    decoded.len(),
                    start + data_row_bytes
                )));
            };
            trimmed.extend_from_slice(src);
        }

        // 字节 → f32（与 decoding_to_f32 的映射一致）。
        let plane = bytes_to_f32(&trimmed, self.info.sample_type);
        Ok(plane)
    }
}

fn block_key(level: &LevelLayout, block: u32, band: u16) -> u64 {
    // 缓存键必须带上 IFD 序号：不同层级的块号会重复。
    let image = (level.image_index as u64) << 48;
    let plane = if level.planar { u64::from(band) } else { 0 };
    image | (plane << 32) | u64::from(block)
}

fn layout_of(
    decoder: &mut Decoder<BufReader<File>>,
    image_index: usize,
) -> Result<LevelLayout, RasterError> {
    let (width, height) = decoder.dimensions()?;
    if width == 0 || height == 0 {
        return Err(RasterError::Corrupt("影像宽高为 0".into()));
    }
    let samples = decoder
        .find_tag_unsigned::<u16>(Tag::SamplesPerPixel)?
        .unwrap_or(1)
        .max(1);
    let planar = matches!(
        decoder.find_tag_unsigned::<u16>(Tag::PlanarConfiguration)?,
        Some(2)
    ) && samples > 1;
    let (layout, block_width, block_height) =
        match decoder.find_tag_unsigned::<u32>(Tag::TileWidth)? {
            Some(tile_width) => {
                let tile_length = decoder
                    .find_tag_unsigned::<u32>(Tag::TileLength)?
                    .unwrap_or(tile_width);
                (BlockLayout::Tiles, tile_width.max(1), tile_length.max(1))
            }
            None => {
                let rows = decoder
                    .find_tag_unsigned::<u32>(Tag::RowsPerStrip)?
                    .unwrap_or(height);
                (BlockLayout::Strips, width, rows.clamp(1, height))
            }
        };
    Ok(LevelLayout {
        image_index,
        width,
        height,
        planar,
        layout,
        block_width,
        block_height,
        blocks_across: width.div_ceil(block_width).max(1),
        blocks_down: height.div_ceil(block_height).max(1),
        samples,
    })
}

/// TIFF 头魔数：42 = 经典 TIFF，43 = BigTIFF。
fn header_is_big_tiff(path: &Path) -> Result<bool, RasterError> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 4];
    file.read_exact(&mut header)
        .map_err(|_| RasterError::NotTiff("文件头不完整".into()))?;
    if &header[0..2] != b"II" && &header[0..2] != b"MM" {
        return Err(RasterError::NotTiff(
            "文件头不是 TIFF 的 II / MM 字节序标记".into(),
        ));
    }
    let magic = if &header[0..2] == b"II" {
        u16::from_le_bytes([header[2], header[3]])
    } else {
        u16::from_be_bytes([header[2], header[3]])
    };
    match magic {
        42 => Ok(false),
        43 => Ok(true),
        other => Err(RasterError::NotTiff(format!("未知的 TIFF 魔数 {other}"))),
    }
}

fn decoding_to_f32(result: DecodingResult) -> Vec<f32> {
    match result {
        DecodingResult::U8(values) => values.into_iter().map(f32::from).collect(),
        DecodingResult::U16(values) => values.into_iter().map(f32::from).collect(),
        DecodingResult::U32(values) => values.into_iter().map(|v| v as f32).collect(),
        DecodingResult::U64(values) => values.into_iter().map(|v| v as f32).collect(),
        DecodingResult::I8(values) => values.into_iter().map(f32::from).collect(),
        DecodingResult::I16(values) => values.into_iter().map(f32::from).collect(),
        DecodingResult::I32(values) => values.into_iter().map(|v| v as f32).collect(),
        DecodingResult::I64(values) => values.into_iter().map(|v| v as f32).collect(),
        DecodingResult::F16(values) => values.into_iter().map(f32::from).collect(),
        DecodingResult::F32(values) => values,
        DecodingResult::F64(values) => values.into_iter().map(|v| v as f32).collect(),
    }
}

/// 水平差分预测器还原（Predictor = 2，字节序已修正）。
///
/// 逐行按「同波段前一个采样」还原差分：`v[i] += v[i - samples]`（升序，
/// `v[i - samples]` 已是还原后的值，与 libtiff 行为一致）。
fn apply_horizontal_predictor(
    data: &mut [u8],
    row_bytes: usize,
    bits_per_sample: usize,
    samples: usize,
) -> Result<(), RasterError> {
    if row_bytes == 0 || data.len() % row_bytes != 0 {
        return Err(RasterError::Corrupt(
            "预测器还原时数据长度与行宽不对齐".into(),
        ));
    }
    match bits_per_sample {
        8 => {
            for row in data.chunks_exact_mut(row_bytes) {
                for i in samples..row.len() {
                    row[i] = row[i].wrapping_add(row[i - samples]);
                }
            }
        }
        16 => {
            for row in data.chunks_exact_mut(row_bytes) {
                let values: &mut [u16] = bytemuck_cast_u16(row);
                for i in samples..values.len() {
                    values[i] = values[i].wrapping_add(values[i - samples]);
                }
            }
        }
        32 => {
            for row in data.chunks_exact_mut(row_bytes) {
                let values: &mut [u32] = bytemuck_cast_u32(row);
                for i in samples..values.len() {
                    values[i] = values[i].wrapping_add(values[i - samples]);
                }
            }
        }
        other => {
            return Err(RasterError::Unsupported(format!(
                "LZW 回退不支持 {other} 位采样的差分预测器"
            )));
        }
    }
    Ok(())
}

/// 把字节行安全地按 u16 视图访问（行宽保证 2 字节对齐）。
fn bytemuck_cast_u16(row: &mut [u8]) -> &mut [u16] {
    debug_assert!(row.len() % 2 == 0);
    let ptr = row.as_mut_ptr().cast::<u16>();
    // SAFETY：长度为偶数且 u16 对齐要求（2）不高于其来源切片的自然对齐约束，
    // 这里行宽来自「采样数 × 2 字节」，必然满足。
    unsafe { std::slice::from_raw_parts_mut(ptr, row.len() / 2) }
}

/// 把字节行安全地按 u32 视图访问（行宽保证 4 字节对齐）。
fn bytemuck_cast_u32(row: &mut [u8]) -> &mut [u32] {
    debug_assert!(row.len() % 4 == 0);
    let ptr = row.as_mut_ptr().cast::<u32>();
    // SAFETY：长度为 4 的倍数；u32 对齐要求（4）不高于 u8 切片起始地址的
    // 分配对齐（块缓冲按 Vec<u8> 分配，实际对齐 ≥ 4），行宽按整采样划分。
    unsafe { std::slice::from_raw_parts_mut(ptr, row.len() / 4) }
}

/// 把原始字节按采样类型转成 f32 平面（映射与 [`decoding_to_f32`] 一致）。
fn bytes_to_f32(bytes: &[u8], sample_type: SampleType) -> Vec<f32> {
    match sample_type {
        SampleType::U8 => bytes.iter().map(|b| f32::from(*b)).collect(),
        SampleType::I8 => bytes.iter().map(|b| f32::from(*b as i8)).collect(),
        SampleType::U16 => bytes
            .chunks_exact(2)
            .map(|c| f32::from(u16::from_le_bytes([c[0], c[1]])))
            .collect(),
        SampleType::I16 => bytes
            .chunks_exact(2)
            .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])))
            .collect(),
        SampleType::U32 => bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32)
            .collect(),
        SampleType::I32 => bytes
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32)
            .collect(),
        SampleType::F32 => bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        SampleType::U64 => bytes
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()) as f32)
            .collect(),
        SampleType::I64 => bytes
            .chunks_exact(8)
            .map(|c| i64::from_le_bytes(c.try_into().unwrap()) as f32)
            .collect(),
        SampleType::F64 => bytes
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()) as f32)
            .collect(),
    }
}

fn photometric_name(code: u16) -> &'static str {
    match code {
        0 => "WhiteIsZero",
        1 => "BlackIsZero",
        2 => "RGB",
        3 => "RGBPalette",
        4 => "TransparencyMask",
        5 => "CMYK",
        6 => "YCbCr",
        8 => "CIELab",
        _ => "未定义",
    }
}

/// 从标签快照中取 ASCII 值。
pub(crate) fn tag_ascii(tags: &[(u16, Value)], code: u16) -> Option<String> {
    let value = tags.iter().find(|(tag, _)| *tag == code).map(|(_, v)| v)?;
    match value {
        Value::Ascii(text) => Some(text.clone()),
        _ => None,
    }
}

/// 从标签快照中取 u16 数组。
pub(crate) fn tag_u16_vec(tags: &[(u16, Value)], code: u16) -> Option<Vec<u16>> {
    let value = tags.iter().find(|(tag, _)| *tag == code).map(|(_, v)| v)?;
    Some(as_f64_vec(value).into_iter().map(|v| v as u16).collect())
}

/// 从标签快照中取 f64 数组。
pub(crate) fn tag_f64_vec(tags: &[(u16, Value)], code: u16) -> Option<Vec<f64>> {
    let value = tags.iter().find(|(tag, _)| *tag == code).map(|(_, v)| v)?;
    let values = as_f64_vec(value);
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn as_f64_vec(value: &Value) -> Vec<f64> {
    match value {
        Value::Byte(v) => vec![f64::from(*v)],
        Value::Short(v) => vec![f64::from(*v)],
        Value::SignedByte(v) => vec![f64::from(*v)],
        Value::SignedShort(v) => vec![f64::from(*v)],
        Value::Signed(v) => vec![f64::from(*v)],
        Value::Unsigned(v) => vec![f64::from(*v)],
        Value::UnsignedBig(v) => vec![*v as f64],
        Value::Float(v) => vec![f64::from(*v)],
        Value::Double(v) => vec![*v],
        Value::List(items) => items.iter().flat_map(as_f64_vec).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_tiff_header_detection() {
        let dir = std::env::temp_dir().join("rustgis-tiff-header");
        std::fs::create_dir_all(&dir).unwrap();
        let classic = dir.join("classic.tif");
        std::fs::write(&classic, [b'I', b'I', 42, 0, 8, 0, 0, 0]).unwrap();
        assert!(!header_is_big_tiff(&classic).unwrap());
        let big = dir.join("big.tif");
        std::fs::write(&big, [b'M', b'M', 0, 43, 8, 0, 0, 0]).unwrap();
        assert!(header_is_big_tiff(&big).unwrap());
        let bad = dir.join("bad.tif");
        std::fs::write(&bad, b"not a tiff").unwrap();
        assert!(header_is_big_tiff(&bad).is_err());
    }

    #[test]
    fn block_span_clips_edges() {
        let level = LevelLayout {
            image_index: 0,
            width: 100,
            height: 70,
            planar: false,
            layout: BlockLayout::Tiles,
            block_width: 32,
            block_height: 32,
            blocks_across: 4,
            blocks_down: 3,
            samples: 3,
        };
        assert_eq!(level.block_span(0), (0, 0, 32, 32));
        assert_eq!(level.block_span(3), (96, 0, 4, 32));
        assert_eq!(level.block_span(11), (96, 64, 4, 6));
    }
}
