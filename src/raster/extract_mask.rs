//! Native Rust mask extraction: source-grid pixel centers, polygon holes,
//! raster NoData masks, bounded strip reads and atomic, no-clobber publication.
use super::{Affine, PixelWindow, RasterDataset};
use crate::geo::{Aabb, CrsId, Layer, ProjectionInfo, ProjectionModel, proj::CrsTransform};
use geo::{Intersects, LineString, Point, Polygon};
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};
use tiff::{
    encoder::{TiffEncoder, colortype::Gray32Float},
    tags::{ExtraSamples, Tag},
};

pub enum MaskSource {
    Polygons(Layer),
    Raster(PathBuf),
}

enum PreparedMask {
    Polygons(Vec<(geo::Rect<f64>, Polygon<f64>)>),
    Raster {
        dataset: RasterDataset,
        to_mask: CrsTransform,
    },
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn coordinate_transform(
    from: &ProjectionInfo,
    to: &ProjectionInfo,
) -> Result<CrsTransform, String> {
    CrsTransform::between(from, to).ok_or_else(|| {
        format!("无法转换坐标系“{}”到“{}”。当前纯 Rust 转换支持经纬度、Web Mercator、UTM 与高斯–克吕格坐标系。",
            from.crs.label(),to.crs.label())
    })
}

/// Densify source-space edges before projection so curved transformed edges
/// remain represented for polygon inclusion and the output crop envelope.
fn transform_ring(points: &[[f64; 2]], xf: CrsTransform) -> Result<Vec<[f64; 2]>, String> {
    if points.len() < 2 {
        return Err("掩膜边界至少需要两个坐标。".into());
    }
    if xf.is_identity() {
        return Ok(points.to_vec());
    }
    let mut out = Vec::with_capacity(points.len() * 17);
    for edge in points.windows(2) {
        for step in 0..16 {
            let t = f64::from(step) / 16.;
            let p = [
                edge[0][0] + (edge[1][0] - edge[0][0]) * t,
                edge[0][1] + (edge[1][1] - edge[0][1]) * t,
            ];
            out.push(
                xf.apply(p)
                    .filter(|q| q.iter().all(|v| v.is_finite()))
                    .ok_or("坐标转换失败；请检查输入范围和坐标系。")?,
            );
        }
    }
    out.push(
        xf.apply(*points.last().unwrap())
            .filter(|q| q.iter().all(|v| v.is_finite()))
            .ok_or("坐标转换失败；请检查输入范围和坐标系。")?,
    );
    Ok(out)
}

fn transform_bounds(bounds: Aabb, xf: CrsTransform) -> Result<Aabb, String> {
    let ring = [
        bounds.min,
        [bounds.max[0], bounds.min[1]],
        bounds.max,
        [bounds.min[0], bounds.max[1]],
        bounds.min,
    ];
    let mut out = Aabb::empty();
    for point in transform_ring(&ring, xf)? {
        out.extend_point(point);
    }
    Ok(out)
}

fn polygons_bounds(polygons: &[(geo::Rect<f64>, Polygon<f64>)]) -> Aabb {
    let mut out = Aabb::empty();
    for (bounds, _) in polygons {
        out.extend_point([bounds.min().x, bounds.min().y]);
        out.extend_point([bounds.max().x, bounds.max().y]);
    }
    out
}

fn pixel_bounds(
    bounds: Aabb,
    transform: Affine,
    width: u32,
    height: u32,
) -> Result<PixelWindow, String> {
    let mut pixels = Aabb::empty();
    for [x, y] in [
        bounds.min,
        bounds.max,
        [bounds.min[0], bounds.max[1]],
        [bounds.max[0], bounds.min[1]],
    ] {
        let q = transform.inverse(x, y).ok_or("栅格地理变换不可逆。")?;
        if !q.iter().all(|v| v.is_finite()) {
            return Err("地理范围包含无效坐标。".into());
        }
        pixels.extend_point(q);
    }
    // Projection round-trips (e.g. UTM → geographic → UTM) place a
    // mathematically integral pixel edge a fraction of a pixel away from the
    // integer: the Transverse Mercator series is millimetre-accurate, which is
    // ~1e-5..1e-3 px for metre-sized pixels. Snap only those near-integer
    // coordinates so a transformed mask does not accidentally gain a whole
    // border of pixels from round-trip noise.
    let floor_edge = |v: f64| {
        let n = v.round();
        if (v - n).abs() < 1e-3 { n } else { v.floor() }
    };
    let ceil_edge = |v: f64| {
        let n = v.round();
        if (v - n).abs() < 1e-3 { n } else { v.ceil() }
    };
    let x = floor_edge(pixels.min[0]).clamp(0., f64::from(width)) as u32;
    let y = floor_edge(pixels.min[1]).clamp(0., f64::from(height)) as u32;
    let right = ceil_edge(pixels.max[0]).clamp(0., f64::from(width)) as u32;
    let bottom = ceil_edge(pixels.max[1]).clamp(0., f64::from(height)) as u32;
    Ok(PixelWindow::new(
        x,
        y,
        right.saturating_sub(x),
        bottom.saturating_sub(y),
    ))
}

fn valid(value: f32, nodata: Option<f64>) -> bool {
    value.is_finite() && !nodata.is_some_and(|n| value == n as f32)
}

/// All source bands are written as Float32; outside-mask/source NoData becomes NaN.
/// The original pixel grid, resolution, rotation and EPSG CRS are retained.
pub fn extract(
    input: &Path,
    mask: MaskSource,
    output: &Path,
    cancel: &AtomicBool,
    progress: &AtomicU32,
) -> Result<(), String> {
    let mut source = RasterDataset::open(input).map_err(|e| e.to_string())?;
    let transform = source
        .georeference()
        .transform
        .ok_or("输入栅格缺少地理参考。")?;
    let crs = source.georeference().crs.clone();
    let epsg = match crs.crs {
        CrsId::Epsg(n) if n > 0 && n < 32767 => n as u16,
        _ => return Err("输出 GeoTIFF 需要已识别的标准 EPSG 坐标系。".into()),
    };
    let (mut mask, bounds) = match mask {
        MaskSource::Polygons(layer) => {
            let to_source = coordinate_transform(&layer.projection, &crs)?;
            if layer.outer_rings.is_empty() {
                return Err("掩膜必须是面图层。".into());
            }
            use geo::BoundingRect;
            let ring = |v: &Vec<[f64; 2]>| {
                LineString::from(v.iter().map(|p| (p[0], p[1])).collect::<Vec<_>>())
            };
            let mut holes = 0;
            let mut polygons = Vec::new();
            for (i, outer) in layer.outer_rings.iter().enumerate() {
                let count = layer.hole_counts.get(i).copied().unwrap_or(0);
                let interiors = layer
                    .hole_rings
                    .get(holes..holes + count)
                    .ok_or("掩膜孔洞数据无效。")?;
                holes += count;
                let outer = transform_ring(outer, to_source)?;
                let holes = interiors
                    .iter()
                    .map(|points| transform_ring(points, to_source))
                    .collect::<Result<Vec<_>, _>>()?;
                let polygon = Polygon::new(ring(&outer), holes.iter().map(ring).collect());
                let bounds = polygon.bounding_rect().ok_or("掩膜包含空多边形。")?;
                polygons.push((bounds, polygon));
            }
            let bounds = polygons_bounds(&polygons);
            (PreparedMask::Polygons(polygons), bounds)
        }
        MaskSource::Raster(path) => {
            let dataset = RasterDataset::open(path).map_err(|e| e.to_string())?;
            let to_source = coordinate_transform(&dataset.georeference().crs, &crs)?;
            let to_mask = coordinate_transform(&crs, &dataset.georeference().crs)?;
            if dataset.georeference().transform.is_none() {
                return Err("掩膜栅格缺少地理参考。".into());
            }
            let bounds = transform_bounds(dataset.bounds(), to_source)?;
            (PreparedMask::Raster { dataset, to_mask }, bounds)
        }
    };
    let region = pixel_bounds(bounds, transform, source.width(), source.height())?;
    if region.is_empty() {
        return Err("掩膜与输入栅格没有重叠区域。".into());
    }
    let bands = source.band_count();
    if bands == 0 || bands > 256 {
        return Err("输入波段数量不受支持（1–256）。".into());
    }
    let nodata: Vec<_> = (0..bands)
        .map(|b| source.band(b).and_then(|b| b.nodata))
        .collect();
    let row_bytes = u64::from(region.width) * bands as u64 * 8;
    if row_bytes > 64 * 1024 * 1024 {
        return Err("单行数据超过 64 MiB，请先裁小输入范围。".into());
    }
    let rows = (8 * 1024 * 1024 / row_bytes.max(1)).clamp(1, 64) as u32;
    if output.exists() {
        return Err("输出文件已存在，请选择新的文件名。".into());
    }
    let temp_path = output.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|e| e.to_string())?;
    let temporary = Temporary(temp_path);
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut encoder = TiffEncoder::new_big(file)?;
        let mut image = encoder.new_image::<Gray32Float>(region.width, region.height)?;
        if bands > 1 {
            image.extra_samples(&vec![ExtraSamples::Unspecified; bands - 1])?;
        }
        image.rows_per_strip(rows)?;
        let origin = transform.apply(f64::from(region.x), f64::from(region.y));
        let gt = transform.geotransform();
        image.encoder().write_tag(
            Tag::ModelTransformationTag,
            &[
                gt[1], gt[2], 0., origin[0], gt[4], gt[5], 0., origin[1], 0., 0., 1., 0., 0., 0.,
                0., 1.,
            ][..],
        )?;
        let geographic = crs.model == ProjectionModel::Geographic;
        image.encoder().write_tag(
            Tag::GeoKeyDirectoryTag,
            &[
                1u16,
                1,
                0,
                3,
                1024,
                0,
                1,
                if geographic { 2 } else { 1 },
                1025,
                0,
                1,
                1,
                if geographic { 2048 } else { 3072 },
                0,
                1,
                epsg,
            ][..],
        )?;
        image.encoder().write_tag(Tag::GdalNodata, "nan")?;
        let mut included = 0u64;
        for row in (0..region.height).step_by(rows as usize) {
            if cancel.load(Ordering::Relaxed) {
                return Err("操作已取消。".into());
            }
            let window = PixelWindow::new(
                region.x,
                region.y + row,
                region.width,
                rows.min(region.height - row),
            );
            let samples = source.read_level_bands(0, window, &(0..bands).collect::<Vec<_>>())?;
            let raster_mask = if let PreparedMask::Raster { dataset, to_mask } = &mut mask {
                let affine = dataset.georeference().transform.unwrap();
                let mut bounds = Aabb::empty();
                let mut mask_points = Vec::with_capacity(window.area() as usize);
                for y in 0..window.height {
                    for x in 0..window.width {
                        let world = transform
                            .apply(f64::from(window.x + x) + 0.5, f64::from(window.y + y) + 0.5);
                        let point = to_mask
                            .apply(world)
                            .ok_or("坐标转换失败；请检查掩膜坐标系与输入范围。")?;
                        bounds.extend_point(point);
                        mask_points.push(point);
                    }
                }
                let w = pixel_bounds(bounds, affine, dataset.width(), dataset.height())?;
                let values = if w.is_empty() {
                    Vec::new()
                } else {
                    dataset.read_window(w, 0)?
                };
                Some((
                    affine,
                    w,
                    values,
                    dataset.band(0).and_then(|b| b.nodata),
                    mask_points,
                ))
            } else {
                None
            };
            let mut output_samples = vec![f32::NAN; window.area() as usize * bands];
            for y in 0..window.height {
                if cancel.load(Ordering::Relaxed) {
                    return Err("操作已取消。".into());
                }
                for x in 0..window.width {
                    let world = transform
                        .apply(f64::from(window.x + x) + 0.5, f64::from(window.y + y) + 0.5);
                    let inside =
                        if let Some((affine, w, values, nodata, mask_points)) = &raster_mask {
                            let world = mask_points[(y * window.width + x) as usize];
                            let q = affine
                                .inverse(world[0], world[1])
                                .ok_or("掩膜地理变换不可逆。")?;
                            let col = q[0].floor() as i64;
                            let row = q[1].floor() as i64;
                            col >= i64::from(w.x)
                                && row >= i64::from(w.y)
                                && col < i64::from(w.x + w.width)
                                && row < i64::from(w.y + w.height)
                                && valid(
                                    values[((row - i64::from(w.y)) * i64::from(w.width) + col
                                        - i64::from(w.x))
                                        as usize],
                                    *nodata,
                                )
                        } else if let PreparedMask::Polygons(ref polygons) = mask {
                            let point = Point::new(world[0], world[1]);
                            polygons.iter().any(|(bounds, polygon)| {
                                bounds.intersects(&point) && polygon.intersects(&point)
                            })
                        } else {
                            false
                        };
                    if inside {
                        included += 1;
                        let pixel = (y * window.width + x) as usize;
                        for band in 0..bands {
                            let value = samples[band][pixel];
                            if valid(value, nodata[band]) {
                                output_samples[pixel * bands + band] = value;
                            }
                        }
                    }
                }
            }
            image.write_strip(&output_samples)?;
            progress.store(
                ((row + window.height) as f64 / region.height as f64 * 1000.) as u32,
                Ordering::Relaxed,
            );
        }
        if included == 0 {
            return Err("掩膜内没有输入像元中心，未生成输出。".into());
        }
        image.finish()?;
        Ok(())
    })();
    result.map_err(|e| e.to_string())?;
    if cancel.load(Ordering::Relaxed) {
        return Err("操作已取消。".into());
    }
    OpenOptions::new()
        .write(true)
        .open(&temporary.0)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    // hard_link fails if the destination exists, including a file created while
    // the worker was running. Never overwrite inputs or unrelated user files.
    std::fs::hard_link(&temporary.0, output).map_err(|e| format!("发布输出失败：{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::PolygonData;
    use std::fs::File;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "rustgis-mask-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn write(path: &Path, values: &[f32], bands: usize, transform: Affine) {
        write_sized(path, values, bands, transform, 32650, 4, 4);
    }
    fn write_epsg(path: &Path, values: &[f32], bands: usize, transform: Affine, epsg: u16) {
        write_sized(path, values, bands, transform, epsg, 4, 4);
    }
    fn write_sized(
        path: &Path,
        values: &[f32],
        bands: usize,
        transform: Affine,
        epsg: u16,
        width: u32,
        height: u32,
    ) {
        let mut encoder = TiffEncoder::new(File::create(path).unwrap()).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(width, height).unwrap();
        if bands > 1 {
            image
                .extra_samples(&vec![ExtraSamples::Unspecified; bands - 1])
                .unwrap();
        }
        image.rows_per_strip(height).unwrap();
        let gt = transform.geotransform();
        image
            .encoder()
            .write_tag(
                Tag::ModelTransformationTag,
                &[
                    gt[1], gt[2], 0., gt[0], gt[4], gt[5], 0., gt[3], 0., 0., 1., 0., 0., 0., 0.,
                    1.,
                ][..],
            )
            .unwrap();
        image
            .encoder()
            .write_tag(
                Tag::GeoKeyDirectoryTag,
                &[
                    1u16, 1, 0, 3, 1024, 0, 1, 1, 1025, 0, 1, 1, 3072, 0, 1, epsg,
                ][..],
            )
            .unwrap();
        image.encoder().write_tag(Tag::GdalNodata, "-9999").unwrap();
        image.write_strip(values).unwrap();
        image.finish().unwrap();
    }
    fn transform() -> Affine {
        Affine::new([400000., 2., 0., 5000000., 0., -3.])
    }
    fn polygon(affine: Affine, hole: bool) -> Layer {
        let ring = |a: f64, b: f64, c: f64, d: f64| {
            vec![
                affine.apply(a, b),
                affine.apply(c, b),
                affine.apply(c, d),
                affine.apply(a, d),
                affine.apply(a, b),
            ]
        };
        let mut layer = Layer::from_geometry(
            "mask".into(),
            vec![],
            vec![],
            vec![PolygonData {
                outer: ring(1., 1., 4., 4.),
                holes: if hole {
                    vec![ring(2., 2., 3., 3.)]
                } else {
                    vec![]
                },
            }],
        );
        layer.projection = ProjectionInfo {
            model: ProjectionModel::Projected,
            crs: CrsId::Epsg(32650),
        };
        layer
    }
    fn run(input: &Path, mask: MaskSource, output: &Path) -> Result<(), String> {
        extract(
            input,
            mask,
            output,
            &AtomicBool::new(false),
            &AtomicU32::new(0),
        )
    }
    #[test]
    fn mask_polygon_preserves_bands_holes_nodata_and_georeference() {
        let f = Fixture::new();
        let input = f.path("input.tif");
        let output = f.path("output.tif");
        let mut data: Vec<f32> = (0..16)
            .flat_map(|n| [n as f32, 100. + n as f32, 200. + n as f32])
            .collect();
        data[5 * 3] = -9999.;
        write(&input, &data, 3, transform());
        run(
            &input,
            MaskSource::Polygons(polygon(transform(), true)),
            &output,
        )
        .unwrap();
        let mut result = RasterDataset::open(&output).unwrap();
        assert_eq!(
            (result.width(), result.height(), result.band_count()),
            (3, 3, 3)
        );
        assert_eq!(
            result.georeference().transform.unwrap().apply(0., 0.),
            transform().apply(1., 1.)
        );
        assert_eq!(result.georeference().crs.crs, CrsId::Epsg(32650));
        assert!(result.band(0).unwrap().nodata.unwrap().is_nan());
        let first = result.read_window(PixelWindow::new(0, 0, 3, 3), 0).unwrap();
        assert!(first[0].is_nan());
        assert!(first[4].is_nan());
        assert_eq!(first[8], 15.);
        let third = result.read_window(PixelWindow::new(0, 0, 3, 3), 2).unwrap();
        assert_eq!(third[0], 205.);
        assert!(third[4].is_nan());
        assert_eq!(third[8], 215.);
    }
    #[test]
    fn mask_raster_uses_validity_including_zero() {
        let f = Fixture::new();
        let input = f.path("input.tif");
        let mask = f.path("mask.tif");
        let output = f.path("output.tif");
        write(
            &input,
            &(0..16).map(|n| n as f32).collect::<Vec<_>>(),
            1,
            transform(),
        );
        let mut values = vec![0.; 16];
        values[2] = -9999.;
        values[3] = f32::NAN;
        write(&mask, &values, 1, transform());
        run(&input, MaskSource::Raster(mask), &output).unwrap();
        let mut result = RasterDataset::open(output).unwrap();
        let samples = result.read_window(PixelWindow::new(0, 0, 4, 4), 0).unwrap();
        assert_eq!(samples[0], 0.);
        assert_eq!(samples[1], 1.);
        assert!(samples[2].is_nan());
        assert!(samples[3].is_nan());
        assert_eq!(samples[15], 15.);
    }
    #[test]
    fn mask_rotation_is_preserved() {
        let f = Fixture::new();
        let input = f.path("input.tif");
        let output = f.path("output.tif");
        let affine = Affine::new([400000., 2., 0.5, 5000000., 0.25, -3.]);
        write(&input, &vec![7.; 16], 1, affine);
        run(
            &input,
            MaskSource::Polygons(polygon(affine, false)),
            &output,
        )
        .unwrap();
        let result = RasterDataset::open(output).unwrap();
        let gt = result.georeference().transform.unwrap().geotransform();
        assert_eq!((gt[1], gt[2], gt[4], gt[5]), (2., 0.5, 0.25, -3.));
    }

    #[test]
    fn geographic_polygon_mask_reprojects_to_projected_raster() {
        let f = Fixture::new();
        let input = f.path("utm.tif");
        let output = f.path("result.tif");
        let affine = transform();
        write(
            &input,
            &(0..16).map(|i| i as f32).collect::<Vec<_>>(),
            1,
            affine,
        );
        let projected = ProjectionInfo {
            model: ProjectionModel::Projected,
            crs: CrsId::Epsg(32650),
        };
        let geographic = ProjectionInfo {
            model: ProjectionModel::Geographic,
            crs: CrsId::Epsg(4326),
        };
        let to_geographic = CrsTransform::between(&projected, &geographic).unwrap();
        let mut mask = polygon(affine, false);
        for point in mask.outer_rings.iter_mut().flatten() {
            *point = to_geographic.apply(*point).unwrap();
        }
        mask.projection = geographic;
        run(&input, MaskSource::Polygons(mask), &output).unwrap();
        let mut result = RasterDataset::open(output).unwrap();
        let values = result.read_window(PixelWindow::new(0, 0, 3, 3), 0).unwrap();
        assert_eq!(values[0], 5.);
        assert_eq!(values[4], 10.);
        assert_eq!(values[8], 15.);
    }

    #[test]
    fn geographic_raster_mask_reprojects_and_samples_nearest_neighbor() {
        let f = Fixture::new();
        let input = f.path("utm.tif");
        let mask_path = f.path("geographic-mask.tif");
        let output = f.path("result.tif");
        let affine = transform();
        write(
            &input,
            &(0..16).map(|i| i as f32).collect::<Vec<_>>(),
            1,
            affine,
        );
        let source_crs = ProjectionInfo {
            model: ProjectionModel::Projected,
            crs: CrsId::Epsg(32650),
        };
        let mask_crs = ProjectionInfo {
            model: ProjectionModel::Geographic,
            crs: CrsId::Epsg(4326),
        };
        let to_geographic = CrsTransform::between(&source_crs, &mask_crs).unwrap();
        let corners = [
            affine.apply(0., 0.),
            affine.apply(4., 0.),
            affine.apply(0., 4.),
            affine.apply(4., 4.),
        ];
        let geographic: Vec<_> = corners
            .iter()
            .map(|p| to_geographic.apply(*p).unwrap())
            .collect();
        let min_lon = geographic
            .iter()
            .map(|p| p[0])
            .fold(f64::INFINITY, f64::min)
            - 0.001;
        let max_lon = geographic
            .iter()
            .map(|p| p[0])
            .fold(f64::NEG_INFINITY, f64::max)
            + 0.001;
        let min_lat = geographic
            .iter()
            .map(|p| p[1])
            .fold(f64::INFINITY, f64::min)
            - 0.001;
        let max_lat = geographic
            .iter()
            .map(|p| p[1])
            .fold(f64::NEG_INFINITY, f64::max)
            + 0.001;
        let mask_affine = Affine::new([
            min_lon,
            (max_lon - min_lon) / 32.,
            0.,
            max_lat,
            0.,
            -(max_lat - min_lat) / 32.,
        ]);
        write_sized(
            &mask_path,
            &vec![1.; 32 * 32],
            1,
            mask_affine,
            4326,
            32,
            32,
        );
        run(&input, MaskSource::Raster(mask_path), &output).unwrap();
        let mut result = RasterDataset::open(output).unwrap();
        let values = result.read_window(PixelWindow::new(0, 0, 4, 4), 0).unwrap();
        assert_eq!(values, (0..16).map(|i| i as f32).collect::<Vec<_>>());
    }
    #[test]
    fn unsupported_crs_overwrite_and_cancellation_are_reported() {
        let f = Fixture::new();
        let input = f.path("input.tif");
        let output = f.path("output.tif");
        write(&input, &vec![7.; 16], 1, transform());
        let mut layer = polygon(transform(), false);
        layer.projection.crs = CrsId::Epsg(999999);
        assert!(
            run(&input, MaskSource::Polygons(layer), &output)
                .unwrap_err()
                .contains("无法转换坐标系")
        );
        assert!(!output.exists());
        let error = extract(
            &input,
            MaskSource::Polygons(polygon(transform(), false)),
            &output,
            &AtomicBool::new(true),
            &AtomicU32::new(0),
        )
        .unwrap_err();
        assert!(error.contains("取消"));
        assert!(!output.exists());
        assert_eq!(std::fs::read_dir(&f.0).unwrap().count(), 1);
        std::fs::write(&output, b"preserve me").unwrap();
        assert!(
            run(
                &input,
                MaskSource::Polygons(polygon(transform(), false)),
                &output
            )
            .is_err()
        );
        assert_eq!(std::fs::read(output).unwrap(), b"preserve me");
    }
}
