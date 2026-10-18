//! 真实 TIF 文件的端到端测试。
//!
//! 用 `tiff` 编码器在临时目录生成夹具，覆盖「普通 TIF / 带 CRS 的 GeoTIFF /
//! 多波段 / 压缩 / 大尺寸 / 无地理参考」六类文件，验证打开、元数据、窗口读取、
//! 波段读取、概览与统计，以及接入图层树与工程文件的完整链路。

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use tiff::encoder::{Compression, TiffEncoder, colortype};
use tiff::tags::Tag;

use super::{
    BlockLayout, CompressionKind, PixelWindow, RasterDataset, RasterDisplay, RasterError,
    RasterLayer, SampleType, StretchType, tiff_file,
};
use crate::geo::{LayerManager, LayerNode, LegendKind, ProjectionModel};

// ---------------------------------------------------------------------------
// 夹具工具
// ---------------------------------------------------------------------------

/// 每个测试独立的临时文件路径（同名旧文件先删除，避免读到上一次的残留）。
fn fixture_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("rustgis-raster-fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    for candidate in [path.clone(), super::overview_path(&path)] {
        let _ = std::fs::remove_file(&candidate);
        let _ = std::fs::remove_file(candidate.with_extension("tmp"));
    }
    path
}

/// 写入单波段 8 位栅格（默认无压缩）。
fn write_gray8(path: &Path, width: u32, height: u32, pixels: &[u8]) {
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = TiffEncoder::new(file).unwrap();
    let image = encoder
        .new_image::<colortype::Gray8>(width, height)
        .unwrap();
    image.write_data(pixels).unwrap();
}

/// 供其它模块测试复用：写一张 2048×2048（约 419 万像素）、带地理参考的灰度 GeoTIFF。
///
/// 像素数刚好超过绘制器的「同步解码上限」，用来验证大影像会走后台线程 + 底图路径。
pub(crate) fn large_geotiff(name: &str) -> PathBuf {
    let path = fixture_path(name);
    let file = std::fs::File::create(&path).unwrap();
    let mut encoder = TiffEncoder::new(file).unwrap();
    let mut image = encoder.new_image::<colortype::Gray8>(2048, 2048).unwrap();
    write_geotiff_tags(&mut image, 32650);
    let pixels: Vec<u8> = (0..2048 * 2048).map(|i| (i % 251) as u8).collect();
    image.write_data(&pixels).unwrap();
    drop(encoder);
    path
}

/// 供其它模块测试复用：写一张 64×64、带地理参考（EPSG:32650）的 GeoTIFF。
pub(crate) fn sample_geotiff(name: &str) -> PathBuf {
    let path = fixture_path(name);
    let file = std::fs::File::create(&path).unwrap();
    let mut encoder = TiffEncoder::new(file).unwrap();
    let mut image = encoder.new_image::<colortype::Gray8>(64, 64).unwrap();
    write_geotiff_tags(&mut image, 32650);
    let pixels: Vec<u8> = (0..64 * 64).map(|i| (i % 251) as u8).collect();
    image.write_data(&pixels).unwrap();
    drop(encoder);
    path
}

/// 供其它模块测试复用：写一张 64×64、三波段、带地理参考（EPSG:32650）的 GeoTIFF。
pub(crate) fn sample_rgb_geotiff(name: &str) -> PathBuf {
    let path = fixture_path(name);
    let file = std::fs::File::create(&path).unwrap();
    let mut encoder = TiffEncoder::new(file).unwrap();
    let mut image = encoder.new_image::<colortype::RGB8>(64, 64).unwrap();
    write_geotiff_tags(&mut image, 32650);
    let pixels: Vec<u8> = (0..64 * 64 * 3).map(|i| (i % 253) as u8).collect();
    image.write_data(&pixels).unwrap();
    drop(encoder);
    path
}

/// GeoTIFF 标准标签：像素比例 + 定位点 + GeoKey 目录（EPSG 代码）。
fn write_geotiff_tags<C: tiff::encoder::colortype::ColorType>(
    image: &mut tiff::encoder::ImageEncoder<'_, std::fs::File, C, tiff::encoder::TiffKindStandard>,
    epsg: u16,
) {
    // ModelPixelScaleTag：x / y / z 像素大小。
    image
        .encoder()
        .write_tag(Tag::ModelPixelScaleTag, &[2.0f64, 3.0, 0.0][..])
        .unwrap();
    // ModelTiepointTag：像素 (0,0,0) → 世界 (400000, 5000000, 0)。
    image
        .encoder()
        .write_tag(
            Tag::ModelTiepointTag,
            &[0.0f64, 0.0, 0.0, 400_000.0, 5_000_000.0, 0.0][..],
        )
        .unwrap();
    // GeoKeyDirectory：GTModelType=Projected、GTRasterType=PixelIsArea、3072=EPSG、3076=米。
    let keys: [u16; 20] = [
        1, 1, 0, 4, // 版本 1.1.0，4 个键
        1024, 0, 1, 1, // GTModelTypeGeoKey = 1（投影坐标系）
        1025, 0, 1, 1, // GTRasterTypeGeoKey = 1（PixelIsArea）
        3072, 0, 1, epsg, // ProjectedCSTypeGeoKey
        3076, 0, 1, 9001, // ProjLinearUnitsGeoKey = 米
    ];
    image
        .encoder()
        .write_tag(Tag::Unknown(34735), &keys[..])
        .unwrap();
}

// ---------------------------------------------------------------------------
// 1. 普通 TIF：元数据 + 窗口读取
// ---------------------------------------------------------------------------

#[test]
fn plain_tiff_metadata_and_window_read() {
    let path = fixture_path("plain-gray8.tif");
    let width = 64u32;
    let height = 32u32;
    // 像素值 = x + y*64，便于校验窗口定位。
    let pixels: Vec<u8> = (0..height)
        .flat_map(|y| (0..width).map(move |x| (x + y * width) as u8))
        .collect();
    write_gray8(&path, width, height, &pixels);

    let mut dataset = RasterDataset::open(&path).unwrap();
    let meta = dataset.metadata().clone();
    assert_eq!(meta.width, 64);
    assert_eq!(meta.height, 32);
    assert_eq!(meta.band_count, 1);
    assert_eq!(meta.sample_type, SampleType::U8);
    assert_eq!(meta.bits_per_sample, 8);
    assert_eq!(meta.compression, CompressionKind::None);
    assert_eq!(meta.block, BlockLayout::Strips);
    assert!(!meta.big_tiff);
    assert!(meta.file_size > 0);
    assert_eq!(meta.bands.len(), 1);
    assert_eq!(meta.bands[0].name, "Band_1");

    // 无地理参考：明确状态而不是崩溃。
    assert!(dataset.georeference().is_missing());
    assert!(!dataset.georeference().warnings.is_empty());
    let bounds = dataset.bounds();
    assert_eq!(bounds.min, [0.0, 0.0]);
    assert_eq!(bounds.max, [64.0, 32.0]);

    // 窗口读取只解码相交的块。
    let window = PixelWindow::new(10, 5, 4, 3);
    let plane = dataset.read_window(window, 0).unwrap();
    assert_eq!(plane.len(), 12);
    for row in 0..3u32 {
        for col in 0..4u32 {
            // 夹具按 u8 写入，(x + y*64) 超过 255 后回绕。
            let expected = ((10 + col + (5 + row) * 64) % 256) as f32;
            assert_eq!(plane[(row * 4 + col) as usize], expected);
        }
    }

    // 越界窗口会被裁剪；完全在外则返回结构化错误。
    let clamped = dataset
        .read_window(PixelWindow::new(60, 30, 100, 100), 0)
        .unwrap();
    assert_eq!(clamped.len(), 2 * 4);
    let err = dataset
        .read_window(PixelWindow::new(100, 100, 4, 4), 0)
        .unwrap_err();
    assert!(matches!(err, RasterError::OutOfBounds { .. }));
    let err = dataset.read_window(window, 3).unwrap_err();
    assert!(matches!(err, RasterError::BadBand { index: 3, count: 1 }));

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 2. 多波段 + 压缩
// ---------------------------------------------------------------------------

#[test]
fn multiband_compressed_variants_decode() {
    let width = 16u32;
    let height = 8u32;
    // RGB 三个通道取值各不相同，用于确认波段隔离正确。
    let pixels: Vec<u8> = (0..height)
        .flat_map(|y| (0..width).flat_map(move |x| [x as u8 * 4, y as u8 * 8, (x + y) as u8]))
        .collect();

    let variants = [
        (
            "rgb-none.tif",
            Compression::Uncompressed,
            CompressionKind::None,
        ),
        ("rgb-lzw.tif", Compression::Lzw, CompressionKind::Lzw),
        (
            "rgb-deflate.tif",
            Compression::Deflate(tiff::encoder::DeflateLevel::Balanced),
            CompressionKind::Deflate,
        ),
        (
            "rgb-packbits.tif",
            Compression::Packbits,
            CompressionKind::PackBits,
        ),
    ];

    for (name, compression, expected) in variants {
        let path = fixture_path(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = TiffEncoder::new(file)
            .unwrap()
            .with_compression(compression);
        let image = encoder.new_image::<colortype::RGB8>(width, height).unwrap();
        image.write_data(&pixels).unwrap();
        drop(encoder);

        let mut dataset = RasterDataset::open(&path).unwrap();
        let meta = dataset.metadata().clone();
        assert_eq!(meta.compression, expected, "{name}");
        assert_eq!(meta.band_count, 3, "{name}");
        assert!(!meta.planar, "{name} 应为交错存储");

        let window = PixelWindow::new(2, 3, 3, 2);
        let planes = dataset.read_level_bands(0, window, &[0, 1, 2]).unwrap();
        assert_eq!(planes.len(), 3);
        for row in 0..2u32 {
            for col in 0..3u32 {
                let x = 2 + col;
                let y = 3 + row;
                let index = (row * 3 + col) as usize;
                assert_eq!(planes[0][index], (x * 4) as f32, "{name} 红");
                assert_eq!(planes[1][index], (y * 8) as f32, "{name} 绿");
                assert_eq!(planes[2][index], (x + y) as f32, "{name} 蓝");
            }
        }

        // 默认符号系统：前三个波段做 RGB 合成。
        let display = RasterDisplay::for_band_count(3, false);
        assert_eq!(display.bands, [0, 1, 2]);
        assert_eq!(display.stretch, StretchType::PercentClip);
        assert!((display.percent_clip - 0.5).abs() < f32::EPSILON);

        let _ = std::fs::remove_file(&path);
    }
}

// ---------------------------------------------------------------------------
// 3. GeoTIFF：仿射变换 + EPSG
// ---------------------------------------------------------------------------

#[test]
fn geotiff_affine_crs_and_pixel_mapping() {
    let path = fixture_path("georef-epsg32650.tif");
    let width = 100u32;
    let height = 50u32;
    let pixels = vec![7u8; (width * height) as usize];
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = TiffEncoder::new(file).unwrap();
        let mut image = encoder
            .new_image::<colortype::Gray8>(width, height)
            .unwrap();
        write_geotiff_tags(&mut image, 32650);
        image.write_data(&pixels).unwrap();
        drop(encoder);
    }

    let dataset = RasterDataset::open(&path).unwrap();
    let georef = dataset.georeference().clone();
    assert!(!georef.is_missing());
    let affine = georef.transform.unwrap();
    assert!(affine.is_north_up());
    // 像元宽 2、像元高 -3（北向上，行号增大时世界 Y 减小）。
    let gt = affine.geotransform();
    assert_eq!((gt[1], gt[5]), (2.0, -3.0));

    // 像素 (0,0) 是左上角；行号向下增大时世界 Y 减小。
    assert_eq!(
        georef.pixel_to_world(0.0, 0.0).unwrap(),
        [400_000.0, 5_000_000.0]
    );
    assert_eq!(
        georef.pixel_to_world(10.0, 20.0).unwrap(),
        [400_020.0, 4_999_940.0]
    );
    let back = georef.world_to_pixel(400_020.0, 4_999_940.0).unwrap();
    assert!((back[0] - 10.0).abs() < 1e-9);
    assert!((back[1] - 20.0).abs() < 1e-9);

    let bounds = dataset.bounds();
    assert_eq!(bounds.min, [400_000.0, 5_000_000.0 - 150.0]);
    assert_eq!(bounds.max, [400_200.0, 5_000_000.0]);
    assert!(
        georef.crs_label().contains("32650"),
        "{}",
        georef.crs_label()
    );
    assert_eq!(dataset.metadata().bands[0].index, 0);

    let layer = RasterLayer::open(&path, "georef".to_string()).unwrap();
    assert_eq!(layer.projection.model, ProjectionModel::Projected);
    assert_eq!(layer.bounds.max, [400_200.0, 5_000_000.0]);

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 4. 无地理参考 / 损坏文件
// ---------------------------------------------------------------------------

#[test]
fn missing_georeference_reports_status_not_error() {
    let path = fixture_path("no-georef.tif");
    write_gray8(&path, 8, 8, &[3u8; 64]);
    let dataset = RasterDataset::open(&path).unwrap();
    assert!(dataset.georeference().is_missing());
    assert_eq!(dataset.georeference().gcp_count, 0);
    assert!(dataset.georeference().transform.is_none());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn invalid_files_return_structured_errors() {
    // 不存在的文件。
    let missing = fixture_path("does-not-exist.tif");
    let err = RasterDataset::open(&missing).unwrap_err();
    assert!(matches!(err, RasterError::NotFound(_)));
    assert_eq!(err.code(), "file-not-found");

    // 扩展名对但内容不是 TIFF。
    let broken = fixture_path("broken.tif");
    std::fs::write(&broken, b"this is definitely not a tiff file").unwrap();
    let err = RasterDataset::open(&broken).unwrap_err();
    assert!(
        matches!(err, RasterError::NotTiff(_) | RasterError::Corrupt(_)),
        "unexpected error: {err}"
    );
    let _ = std::fs::remove_file(&broken);
}

// ---------------------------------------------------------------------------
// 5. 大尺寸 TIF：按块解码 + 显示采样
// ---------------------------------------------------------------------------

#[test]
fn large_tiff_reads_only_needed_blocks() {
    let path = fixture_path("large-2048.tif");
    let width = 2048u32;
    let height = 2048u32;
    // 每行 2048 字节；行内值取 x/8，便于抽样校验。
    let pixels: Vec<u8> = (0..height)
        .flat_map(|_| (0..width).map(|x| (x / 8) as u8))
        .collect();
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = TiffEncoder::new(file).unwrap();
        let mut image = encoder
            .new_image::<colortype::Gray8>(width, height)
            .unwrap();
        image.rows_per_strip(16).unwrap();
        image.write_data(&pixels).unwrap();
        drop(encoder);
    }

    let mut dataset = RasterDataset::open(&path).unwrap();
    let meta = dataset.metadata().clone();
    assert_eq!(meta.block_width, 2048);
    assert_eq!(meta.block_height, 16);
    assert_eq!(meta.blocks_across, 1);
    assert_eq!(meta.blocks_down, 128);
    assert_eq!(meta.total_blocks(), 128);

    let window = PixelWindow::new(1000, 1000, 64, 64);
    let plane = dataset.read_window(window, 0).unwrap();
    assert_eq!(plane.len(), 64 * 64);
    assert_eq!(plane[0], (1000 / 8) as f32);
    assert_eq!(plane[63], (1063 / 8) as f32);

    // 显示采样：远小于原始尺寸时仍返回请求的尺寸。
    let sampled = dataset
        .read_sampled(PixelWindow::new(0, 0, width, height), 128, 128, &[0])
        .unwrap();
    assert_eq!(sampled.len(), 1);
    assert_eq!(sampled[0].len(), 128 * 128);

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 6. 内部概览（多 IFD）+ 采样层级选择
// ---------------------------------------------------------------------------

#[test]
fn internal_overview_level_is_used_for_sampling() {
    let path = fixture_path("overviews-multipage.tif");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = TiffEncoder::new(file).unwrap();
        encoder
            .write_image::<colortype::Gray8>(64, 64, &vec![10u8; 64 * 64][..])
            .unwrap();
        encoder
            .write_image::<colortype::Gray8>(16, 16, &vec![20u8; 16 * 16][..])
            .unwrap();
        encoder
            .write_image::<colortype::Gray8>(4, 4, &vec![30u8; 4 * 4][..])
            .unwrap();
        drop(encoder);
    }

    let mut dataset = RasterDataset::open(&path).unwrap();
    assert_eq!(dataset.width(), 64);
    assert_eq!(dataset.height(), 64);
    assert_eq!(dataset.level_count(), 3);
    assert_eq!(dataset.overview_count(), 2);
    assert!(dataset.has_overviews());

    // 输出 16×16：比值 4，选中 16×16 那一层（scale = 4）。
    let sampled = dataset
        .read_sampled(PixelWindow::new(0, 0, 64, 64), 16, 16, &[0])
        .unwrap();
    assert_eq!(sampled[0].len(), 16 * 16);
    assert!(sampled[0].iter().all(|v| (*v - 20.0).abs() < 1e-6));

    // 层级窗口读取：直接读第 2 层（4×4）。
    let raw = dataset
        .read_level_window(2, PixelWindow::new(0, 0, 4, 4), 0)
        .unwrap();
    assert_eq!(raw.len(), 16);
    assert!(raw.iter().all(|v| (*v - 30.0).abs() < 1e-6));

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 7. 构建外部概览（`.ovr`，与 GDAL / ArcGIS 同格式）
// ---------------------------------------------------------------------------

#[test]
fn builds_ovr_and_reads_it() {
    let path = fixture_path("pyramid-build.tif");
    let width = 512u32;
    let height = 512u32;
    let pixels: Vec<u8> = (0..height)
        .flat_map(|y| (0..width).map(move |x| ((x + y) / 2) as u8))
        .collect();
    write_gray8(&path, width, height, &pixels);

    let mut dataset = RasterDataset::open(&path).unwrap();
    assert!(!dataset.has_overviews());
    assert!(!dataset.metadata().has_external_overviews);

    let cancel = AtomicBool::new(false);
    let mut reported = 0.0f32;
    let levels = dataset
        .build_overviews(&cancel, &mut |ratio| {
            reported = reported.max(ratio);
        })
        .unwrap();
    assert!(levels >= 1, "至少要构建一层概览");
    assert!(reported > 0.0);
    assert!(dataset.has_overviews());
    assert!(dataset.metadata().has_external_overviews);
    let ovr_path = super::overview_path(&path);
    assert!(ovr_path.is_file(), "概览应写在 <影像>.ovr 里");

    // `.ovr` 本身就是一个只含概览 IFD 的 TIFF：不带扩展名的同名文件
    // 也能被 GDAL / ArcGIS 直接识别。
    let ovr = tiff_file::TiffFile::open(&ovr_path).unwrap();
    assert_eq!(ovr.info.levels.len(), levels, "概览层数应与写入一致");
    assert_eq!(ovr.info.samples, 1);
    assert_eq!(ovr.info.levels[0].width, width / 2);
    assert_eq!(ovr.info.levels[0].height, height / 2);
    assert_eq!(ovr.info.compression, CompressionKind::Lzw);

    // 走概览层级采样，结果仍然是请求的尺寸。
    let sampled = dataset
        .read_sampled(PixelWindow::new(0, 0, width, height), 64, 64, &[0])
        .unwrap();
    assert_eq!(sampled[0].len(), 64 * 64);
    assert!(sampled[0].iter().all(|v| v.is_finite()));

    // 重新打开后应直接识别外部概览。
    drop(dataset);
    let reopened = RasterDataset::open(&path).unwrap();
    assert!(reopened.has_overviews());
    assert!(reopened.metadata().has_external_overviews);
    assert_eq!(reopened.overview_count(), levels);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&ovr_path);
}

/// 16 位灰度（卫星影像最常见的位深）也要能写 `.ovr` 并读回正确的值。
#[test]
fn builds_u16_ovr_with_averaged_values() {
    let path = fixture_path("pyramid-u16.tif");
    let width = 512u32;
    let height = 512u32;
    // 棋盘格 0 / 2000：每 2×2 的均值恰好是 1000，便于校验降采样数值。
    let pixels: Vec<u16> = (0..height)
        .flat_map(|y| (0..width).map(move |x| if (x + y) % 2 == 0 { 0 } else { 2000 }))
        .collect();
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = TiffEncoder::new(file)
            .unwrap()
            .with_compression(Compression::Lzw);
        encoder
            .write_image::<colortype::Gray16>(width, height, &pixels)
            .unwrap();
    }

    let mut dataset = RasterDataset::open(&path).unwrap();
    let meta = dataset.metadata().clone();
    assert_eq!(meta.sample_type, SampleType::U16);
    let levels = dataset
        .build_overviews(&AtomicBool::new(false), &mut |_| {})
        .unwrap();
    assert!(levels >= 1);

    // 第一级概览：值应为 2×2 均值 1000，且不再有棋盘格条纹。
    let plane = dataset
        .read_level_bands(1, PixelWindow::new(10, 10, 16, 16), &[0])
        .unwrap();
    assert!(
        plane[0].iter().all(|value| (value - 1000.0).abs() < 1.0),
        "16 位概览取平均失败：{:?}",
        &plane[0][..8]
    );

    let ovr = tiff_file::TiffFile::open(&super::overview_path(&path)).unwrap();
    assert_eq!(ovr.info.sample_type, SampleType::U16);
    assert_eq!(ovr.info.bits_per_sample, 16);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(super::overview_path(&path));
}

#[test]
fn cancelled_overview_build_leaves_no_ovr() {
    let path = fixture_path("pyramid-cancel.tif");
    let width = 512u32;
    let height = 512u32;
    let pixels = vec![7u8; (width * height) as usize];
    write_gray8(&path, width, height, &pixels);

    let mut dataset = RasterDataset::open(&path).unwrap();
    let cancel = AtomicBool::new(true);
    let result = dataset.build_overviews(&cancel, &mut |_| {});
    assert!(matches!(result, Err(RasterError::Cancelled)));

    let ovr_path = super::overview_path(&path);
    assert!(!ovr_path.exists(), "取消后不应留下 .ovr");
    assert!(
        !ovr_path.with_extension("tmp").exists(),
        "取消后不应留下临时文件"
    );

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 8. 统计值：文件内已有 + 本地计算
// ---------------------------------------------------------------------------

#[test]
fn statistics_are_read_from_gdal_metadata() {
    let path = fixture_path("stats-gdal.tif");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = TiffEncoder::new(file).unwrap();
        let mut image = encoder.new_image::<colortype::Gray8>(8, 8).unwrap();
        let xml = "<GDALMetadata>\n\
                   <Item name=\"DESCRIPTION\" sample=\"0\">Elevation</Item>\n\
                   <Item name=\"STATISTICS_MINIMUM\" sample=\"0\">12.5</Item>\n\
                   <Item name=\"STATISTICS_MAXIMUM\" sample=\"0\">88.5</Item>\n\
                   <Item name=\"STATISTICS_MEAN\" sample=\"0\">50.5</Item>\n\
                   <Item name=\"STATISTICS_STDDEV\" sample=\"0\">7.25</Item>\n\
                   <Item name=\"AREA_OR_POINT\" sample=\"0\">Area</Item>\n\
                   </GDALMetadata>";
        image.encoder().write_tag(Tag::Unknown(42112), xml).unwrap();
        image.write_data(&[1u8; 64]).unwrap();
        drop(encoder);
    }

    let dataset = RasterDataset::open(&path).unwrap();
    assert_eq!(dataset.metadata().bands[0].name, "Elevation");
    let stats = dataset.statistics(0).expect("应读到文件内统计值");
    assert!(stats.from_file);
    assert_eq!(stats.min, 12.5);
    assert_eq!(stats.max, 88.5);
    assert_eq!(stats.mean, 50.5);
    assert_eq!(stats.std_dev, 7.25);
    // 其余 GDAL 条目进入元数据列表。
    assert!(
        dataset
            .metadata_items()
            .iter()
            .any(|(key, value)| key == "AREA_OR_POINT" && value == "Area")
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn statistics_are_computed_from_pixels() {
    let path = fixture_path("stats-computed.tif");
    let width = 8u32;
    let height = 8u32;
    let pixels: Vec<u8> = (0..64u8).collect();
    write_gray8(&path, width, height, &pixels);

    let mut dataset = RasterDataset::open(&path).unwrap();
    assert!(!dataset.has_statistics(0));
    let cancel = AtomicBool::new(false);
    let stats = dataset.compute_statistics(0, &cancel, &mut |_| {}).unwrap();
    assert!(!stats.from_file);
    assert!(!stats.estimated);
    assert_eq!(stats.valid_count, 64);
    assert!((stats.min - 0.0).abs() < 1e-6);
    assert!((stats.max - 63.0).abs() < 1e-6);
    assert!((stats.mean - 31.5).abs() < 1e-6);
    assert!((stats.std_dev - 18.473_786).abs() < 1e-3);
    assert_eq!(dataset.statistics(0).unwrap().valid_count, 64);

    // 取消信号生效。
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let err = dataset
        .compute_statistics(0, &cancel, &mut |_| {})
        .unwrap_err();
    assert!(matches!(err, RasterError::Cancelled));

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 9. 图层体系与工程文件
// ---------------------------------------------------------------------------

#[test]
fn raster_layer_joins_layer_manager_and_project() {
    let path = fixture_path("layer-project.tif");
    let width = 32u32;
    let height = 16u32;
    write_gray8(&path, width, height, &vec![9u8; (width * height) as usize]);

    let layer = RasterLayer::open(&path, "dem".to_string()).unwrap();
    let mut manager = LayerManager::default();
    let id = manager.add_raster(layer);

    // 面板行与图例类型。
    let rows = manager.panel_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].node.name(), "dem");
    assert!(matches!(
        rows[0].node.legend_kind(),
        Some(LegendKind::Raster)
    ));
    assert!(!rows[0].node.is_group());
    assert_eq!(rows[0].node.id(), id);

    // 可见栅格迭代与范围合并。
    assert_eq!(manager.iter_visible_rasters().count(), 1);
    let total = manager.total_bounds().unwrap();
    assert_eq!(total.min, [0.0, 0.0]);
    assert_eq!(total.max, [32.0, 16.0]);

    // 修改符号系统后渲染版本号必须变化（触发重绘）。
    let revision = manager.revision();
    let mut display = manager
        .node(id)
        .and_then(LayerNode::raster)
        .unwrap()
        .display
        .clone();
    display.stretch = StretchType::MinMax;
    display.gamma = [0.62585; 3];
    manager.set_raster_display(id, display.clone());
    assert!(manager.revision() > revision);
    let applied = &manager.node(id).unwrap().raster().unwrap().display;
    assert_eq!(applied.stretch, StretchType::MinMax);
    assert_eq!(applied.gamma, [0.62585; 3]);

    // 工程文件往返：只保存路径与显示设置，加载时重新打开数据集。
    let project = std::env::temp_dir()
        .join("rustgis-raster-fixtures")
        .join("layer-project.rustgis");
    let _ = std::fs::remove_file(&project);
    crate::project::save(&project, &manager.roots_cloned(), None, None).unwrap();
    let loaded = crate::project::load(&project).unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert_eq!(loaded.nodes.len(), 1);
    let node = &loaded.nodes[0];
    let raster = node.raster().expect("应还原为栅格图层");
    assert_eq!(raster.name, "dem");
    assert_eq!(raster.display.stretch, StretchType::MinMax);
    assert_eq!(raster.display.gamma, [0.62585; 3]);
    assert_eq!(raster.bounds.max, [32.0, 16.0]);

    // 路径失效时给出警告而不是整体失败。
    let missing = std::env::temp_dir()
        .join("rustgis-raster-fixtures")
        .join("missing-raster.tif");
    let broken_layer = RasterLayer::open(&path, "gone".to_string()).unwrap();
    let mut broken = LayerManager::default();
    broken.add_raster(broken_layer);
    let broken_project = std::env::temp_dir()
        .join("rustgis-raster-fixtures")
        .join("broken.rustgis");
    let _ = std::fs::remove_file(&broken_project);
    crate::project::save(&broken_project, &broken.roots_cloned(), None, None).unwrap();
    // 把工程里的路径改成不存在的文件。
    let raw = std::fs::read_to_string(&broken_project).unwrap();
    let raw = raw.replace(
        &path.to_string_lossy().replace('\\', "\\\\"),
        &missing.to_string_lossy().replace('\\', "\\\\"),
    );
    std::fs::write(&broken_project, raw).unwrap();
    let loaded = crate::project::load(&broken_project).unwrap();
    assert!(loaded.nodes.is_empty());
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("gone"));

    let _ = std::fs::remove_file(&project);
    let _ = std::fs::remove_file(&broken_project);
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 瓦片（Tiles）分块：`tiff` 编码器只会写条带，瓦片布局按规范手工拼字节
// ---------------------------------------------------------------------------

/// 手写一张 8 位无压缩、瓦片分块的小端 TIFF。
///
/// 瓦片边长固定为 `tile`；边缘瓦片按 TIFF 规范补齐到整块，有效区域之外的
/// 填充值不会被解码出来。
fn write_tiled_gray8(path: &Path, width: u32, height: u32, tile: u32, pixels: &[u8]) {
    let across = width.div_ceil(tile);
    let down = height.div_ceil(tile);
    let tile_bytes = (tile * tile) as usize;
    let block_count = (across * down) as usize;

    // 12 个 IFD 条目：条目区之后紧跟瓦片偏移数组、瓦片字节数数组，再后面是像素。
    const ENTRY_COUNT: usize = 12;
    let ifd_offset = 8u32;
    let ifd_len = 2 + ENTRY_COUNT * 12 + 4;
    let offsets_array = ifd_offset + ifd_len as u32;
    let counts_array = offsets_array + (block_count as u32) * 4;
    let data_offset = counts_array + (block_count as u32) * 4;

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&ifd_offset.to_le_bytes());
    out.extend_from_slice(&(ENTRY_COUNT as u16).to_le_bytes());

    // SHORT = 3，LONG = 4；单值 SHORT 直接内联在值字段里。
    let entry = |out: &mut Vec<u8>, tag: u16, kind: u16, count: u32, value: u32| {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    };
    entry(&mut out, 256, 3, 1, width); // ImageWidth
    entry(&mut out, 257, 3, 1, height); // ImageLength
    entry(&mut out, 258, 3, 1, 8); // BitsPerSample
    entry(&mut out, 259, 3, 1, 1); // Compression = None
    entry(&mut out, 262, 3, 1, 1); // Photometric = BlackIsZero
    entry(&mut out, 277, 3, 1, 1); // SamplesPerPixel
    entry(&mut out, 284, 3, 1, 1); // PlanarConfiguration = Chunky
    entry(&mut out, 322, 3, 1, tile); // TileWidth
    entry(&mut out, 323, 3, 1, tile); // TileLength
    entry(&mut out, 324, 4, block_count as u32, offsets_array); // TileOffsets
    entry(&mut out, 325, 4, block_count as u32, counts_array); // TileByteCounts
    entry(&mut out, 339, 3, 1, 1); // SampleFormat = Uint
    out.extend_from_slice(&0u32.to_le_bytes()); // 没有下一个 IFD
    assert_eq!(out.len(), offsets_array as usize);

    for block in 0..block_count as u32 {
        let offset = data_offset + block * tile_bytes as u32;
        out.extend_from_slice(&offset.to_le_bytes());
    }
    for _ in 0..block_count {
        out.extend_from_slice(&(tile_bytes as u32).to_le_bytes());
    }
    assert_eq!(out.len(), data_offset as usize);

    // 瓦片按「行优先」排列，每块都是完整的 tile×tile，越界部分补 0。
    for block in 0..block_count as u32 {
        let tile_x = block % across;
        let tile_y = block / across;
        for row in 0..tile {
            for col in 0..tile {
                let x = tile_x * tile + col;
                let y = tile_y * tile + row;
                let value = if x < width && y < height {
                    pixels[(y * width + x) as usize]
                } else {
                    0
                };
                out.push(value);
            }
        }
    }

    std::fs::write(path, out).unwrap();
}

#[test]
fn tiled_tiff_reads_blocks_and_cross_boundary_windows() {
    let path = fixture_path("tiled-gray8.tif");
    let (width, height, tile) = (20u32, 20u32, 16u32);
    let pixels: Vec<u8> = (0..(width * height) as usize)
        .map(|index| (index % 200 + 1) as u8)
        .collect();
    write_tiled_gray8(&path, width, height, tile, &pixels);

    let mut dataset = RasterDataset::open(&path).unwrap();
    {
        let meta = dataset.metadata();
        assert_eq!(meta.block, BlockLayout::Tiles);
        assert_eq!((meta.block_width, meta.block_height), (tile, tile));
        assert_eq!((meta.blocks_across, meta.blocks_down), (2, 2));
        assert_eq!(meta.total_blocks(), 4);
        assert_eq!(meta.compression, CompressionKind::None);
        assert_eq!(meta.sample_type, SampleType::U8);
    }

    // 整幅读取：四块瓦片拼回，右侧与底部边缘瓦片的填充被裁掉。
    let full = dataset
        .read_level_window(0, PixelWindow::new(0, 0, width, height), 0)
        .unwrap();
    assert_eq!(full.len(), (width * height) as usize);
    for (index, value) in full.iter().enumerate() {
        assert_eq!(*value, f32::from(pixels[index]), "整幅像素 {index} 不一致");
    }

    // 跨瓦片窗口：从瓦片 (0,0) 跨到 (1,1)。
    let window = dataset
        .read_level_window(0, PixelWindow::new(10, 10, 8, 8), 0)
        .unwrap();
    for row in 0..8u32 {
        for col in 0..8u32 {
            let x = 10 + col;
            let y = 10 + row;
            let expected = f32::from(pixels[(x + y * width) as usize]);
            assert_eq!(
                window[(row * 8 + col) as usize],
                expected,
                "跨块窗口 ({x},{y}) 不一致"
            );
        }
    }

    // 显示采样路径同样可用（走同一套按块解码 + 最近邻重采样）。
    let sampled = dataset
        .read_sampled(PixelWindow::new(0, 0, width, height), 8, 8, &[0])
        .unwrap();
    assert_eq!(sampled[0].len(), 64);
    assert!(sampled[0].iter().all(|value| value.is_finite()));

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 13. Sentinel-2 SAFE 产品包（JPEG 2000 波段）
// ---------------------------------------------------------------------------

/// 找一个可用的 JPEG 2000 样例码流（j2k 0.12 没有编码入口，只能外部准备）。
///
/// 依次尝试 `RUSTGIS_JP2_SAMPLE` 环境变量、`$TEMP/jp2sample/`、项目 `target/jp2sample/`。
pub(crate) fn jp2_sample() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(value) = std::env::var("RUSTGIS_JP2_SAMPLE") {
        candidates.push(PathBuf::from(value));
    }
    let bases = [
        std::env::temp_dir(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
    ];
    for base in bases {
        let dir = base.join("jp2sample");
        candidates.push(dir.join("p0_03.j2k"));
        candidates.push(dir.join("p0_05.j2k"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// 按真实 SAFE 的目录布局拼一个最小产品包，波段文件用同一份 JP2 样例复制。
///
/// MTD_TL 里的 NROWS/NCOLS 取自样例的真实尺寸，因此地理编码与实际像素一致。
pub(crate) fn safe_fixture(name: &str, bands: &[(&str, &str)]) -> Option<PathBuf> {
    let sample = jp2_sample()?;
    let header = super::jp2::inspect(&sample).ok()?;

    let root = std::env::temp_dir()
        .join("rustgis-raster-fixtures")
        .join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(
        root.join("MTD_MSIL2A.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<n1:Level-2A_User_Product xmlns:n1="https://psd-14.sentinel2.eo.esa.int/PSD/S2_PDI_Level-2A_User_Product_Metadata.xsd">
  <n1:General_Info>
    <Product_Info>
      <PRODUCT_TYPE>S2MSI2A</PRODUCT_TYPE>
      <PRODUCT_START_TIME>2024-01-01T10:12:31.024Z</PRODUCT_START_TIME>
      <Datatake>
        <DATATAKE_SENSING_START>2024-01-01T10:12:31.024Z</DATATAKE_SENSING_START>
        <SPACECRAFT_NAME>Sentinel-2B</SPACECRAFT_NAME>
        <SENSING_ORBIT_NUMBER>65</SENSING_ORBIT_NUMBER>
      </Datatake>
      <PROCESSING_BASELINE>05.09</PROCESSING_BASELINE>
    </Product_Info>
  </n1:General_Info>
</n1:Level-2A_User_Product>"#,
    )
    .unwrap();

    let granule = root
        .join("GRANULE")
        .join("L2A_T33UVP_A000000_20240101T101231");
    let img_data = granule.join("IMG_DATA");
    let base = img_data.join("R10m");
    std::fs::create_dir_all(&base).unwrap();

    std::fs::write(
        granule.join("MTD_TL.xml"),
        format!(
            r#"<n1:Level-2A_Tile_ID xmlns:n1="https://psd-14.sentinel2.eo.esa.int/PSD/S2_PDI_Level-2A_Tile_Metadata.xsd">
  <n1:General_Info>
    <TILE_ID>T33UVP_20240101T101231_A000000_20240101T101231</TILE_ID>
    <SENSING_TIME>2024-01-01T10:12:31.024Z</SENSING_TIME>
  </n1:General_Info>
  <n1:Geometric_Info>
    <Tile_Geocoding>
      <HORIZONTAL_CS_NAME>WGS84 / UTM zone 33N</HORIZONTAL_CS_NAME>
      <HORIZONTAL_CS_CODE>EPSG:32633</HORIZONTAL_CS_CODE>
      <Size resolution="10">
        <NROWS>{rows}</NROWS>
        <NCOLS>{cols}</NCOLS>
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
            rows = header.height,
            cols = header.width,
        ),
    )
    .unwrap();

    for (code, resolution) in bands {
        // 全部落在 R10m 目录：多波段读取路径不依赖分辨率差异。
        let name = format!("T33UVP_20240101T101231_{code}_{resolution}.jp2");
        std::fs::copy(&sample, base.join(name)).unwrap();
    }

    Some(root)
}

#[test]
fn safe_package_opens_bands_metadata_and_georeference() {
    let Some(root) = safe_fixture(
        "safe-multiband.SAFE",
        &[("B04", "10m"), ("B03", "10m"), ("B02", "10m")],
    ) else {
        eprintln!(
            "跳过：没有可用的 JPEG 2000 样例（设置 RUSTGIS_JP2_SAMPLE 或放到 $TEMP/jp2sample）"
        );
        return;
    };

    assert!(super::is_safe_package(&root));

    let mut dataset = RasterDataset::open(&root).unwrap();
    let meta = dataset.metadata().clone();
    assert_eq!(meta.band_count, 3);
    assert_eq!(meta.compression, CompressionKind::Jpeg2000);
    assert_eq!(meta.block, BlockLayout::Jpeg2000);
    // 分块布局来自 JPEG 2000 的 tile 尺寸（整幅一块时就是图像尺寸）。
    assert!(meta.block_width > 0 && meta.block_width <= meta.width);
    assert!(meta.blocks_across * meta.block_width >= meta.width);
    assert!(meta.blocks_down * meta.block_height >= meta.height);
    assert!(meta.file_size > 0);
    // 波段名带中文说明：`B04 红`。
    assert!(
        meta.bands.iter().any(|band| band.name.contains("B04")),
        "波段名应包含波段号：{:?}",
        meta.bands
            .iter()
            .map(|band| band.name.clone())
            .collect::<Vec<_>>()
    );

    // 地理参考来自 MTD_TL：ULX/ULY + 10 m 像元，Y 轴向下为负。
    let georef = dataset.georeference().clone();
    assert!(!georef.is_missing(), "{:?}", georef.warnings);
    let bounds = dataset.bounds();
    assert!((bounds.min[0] - 399_960.0).abs() < 1e-6);
    assert!((bounds.max[1] - 5_300_040.0).abs() < 1e-6);
    let expected_max_x = 399_960.0 + f64::from(meta.width) * 10.0;
    assert!((bounds.max[0] - expected_max_x).abs() < 1e-6);
    let expected_min_y = 5_300_040.0 - f64::from(meta.height) * 10.0;
    assert!((bounds.min[1] - expected_min_y).abs() < 1e-6);
    assert_eq!(
        dataset.georeference().crs_label(),
        "EPSG:32633",
        "坐标系应来自 HORIZONTAL_CS_CODE"
    );

    // 读一个横跨中心的窗口：值域来自真实 JPEG 2000 解码（16 位）。
    let window = PixelWindow::new(4, 4, 16, 8);
    let plane = dataset.read_window(window, 0).unwrap();
    assert_eq!(plane.len(), 16 * 8);
    assert!(plane.iter().all(|value| value.is_finite()));
    // 样例码流是有内容的图像：至少要能解出非零样本（防止「静默全零」）。
    assert!(
        plane.iter().any(|value| *value > 0.0),
        "解码结果全为零，JPEG 2000 解码可能没有真正生效"
    );

    // 超出波段数量仍然报结构化错误。
    let err = dataset.read_window(window, 9).unwrap_err();
    assert!(
        matches!(err, RasterError::BadBand { index: 9, count: 3 }),
        "{err:?}"
    );

    // SAFE 自带多分辨率，不需要构建金字塔缓存。
    let built = dataset
        .build_overviews(&AtomicBool::new(false), &mut |_| {})
        .unwrap();
    assert_eq!(built, 0);
    // 虚拟层级一直铺到 1/32：全幅缩略显示时能直接取深层窗口，解码像素少一个数量级。
    assert!(
        dataset.level_count() >= 5,
        "SAFE 虚拟层级应有 5 级以上：{}",
        dataset.level_count()
    );
    assert_eq!(dataset.overview_count(), dataset.level_count() - 1);

    // 统计值只能在显示时现算（SAFE 元数据里没有 GDAL 统计字段）：
    // 这里顺带守住「统计槽位必须按波段数预置」，否则后台统计线程会 panic。
    let statistics = dataset
        .compute_statistics(0, &AtomicBool::new(false), &mut |_| {})
        .unwrap();
    assert!(statistics.valid_count > 0, "{statistics:?}");
    assert!(statistics.max > statistics.min, "{statistics:?}");
    assert!(dataset.statistics(0).is_some());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn safe_package_reads_downscaled_display_source() {
    let Some(root) = safe_fixture("safe-tci-downscale.SAFE", &[("TCI", "10m")]) else {
        eprintln!(
            "跳过：没有可用的 JPEG 2000 样例（设置 RUSTGIS_JP2_SAMPLE 或放到 $TEMP/jp2sample）"
        );
        return;
    };

    let mut dataset = RasterDataset::open(&root).unwrap();
    let (width, height) = (dataset.metadata().width, dataset.metadata().height);
    // TCI 是三分量码流：按真实布局应拆成三个显示波段；单分量样例则只有一个波段。
    let components = super::jp2::inspect(&jp2_sample().unwrap())
        .unwrap()
        .components;
    let expected_bands = if components >= 3 { 3 } else { 1 };
    assert_eq!(dataset.metadata().band_count, expected_bands);

    // 整幅缩到 8×8：走 Downscale::Eighth，只解码需要的区域。
    let bands: Vec<usize> = (0..expected_bands).collect();
    let (planes, level_window) = dataset
        .read_display_source(PixelWindow::new(0, 0, width, height), 8, 8, &bands)
        .unwrap();
    assert_eq!(planes.len(), expected_bands);
    assert!(!level_window.is_empty());
    for plane in &planes {
        assert_eq!(
            plane.len(),
            (level_window.width * level_window.height) as usize
        );
        assert!(plane.iter().all(|value| value.is_finite()));
    }

    let _ = std::fs::remove_dir_all(&root);
}
