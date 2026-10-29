//! GeoTIFF raster mosaicking on a north-up output grid.
use super::{Affine, PixelWindow, RasterDataset};
use crate::geo::{Aabb, CrsId, ProjectionInfo, proj::CrsTransform};
use std::{
    fs::OpenOptions,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};
use tiff::{
    encoder::{TiffEncoder, colortype::Gray32Float},
    tags::{ExtraSamples, Tag},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MosaicOperator {
    First,
    Last,
    Blend,
    Minimum,
    Maximum,
    Mean,
    Sum,
}

impl MosaicOperator {
    pub const ALL: [Self; 7] = [
        Self::First,
        Self::Last,
        Self::Blend,
        Self::Minimum,
        Self::Maximum,
        Self::Mean,
        Self::Sum,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::First => "FIRST（优先采用列表靠前影像）",
            Self::Last => "LAST（优先采用列表靠后影像）",
            Self::Blend => "BLEND（按影像边缘距离加权）",
            Self::Minimum => "MINIMUM（最小值）",
            Self::Maximum => "MAXIMUM（最大值）",
            Self::Mean => "MEAN（均值）",
            Self::Sum => "SUM（求和）",
        }
    }
}

pub struct MosaicRequest {
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
}

struct Source {
    dataset: RasterDataset,
    transform: Affine,
    to_output: CrsTransform,
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn projection_transform(
    from: &ProjectionInfo,
    to: &ProjectionInfo,
) -> Result<CrsTransform, String> {
    CrsTransform::between(from, to).ok_or_else(|| {
        format!(
            "无法将坐标系“{}”转换为“{}”。",
            from.crs.label(),
            to.crs.label()
        )
    })
}

fn transformed_bounds(
    ds: &RasterDataset,
    affine: Affine,
    xf: CrsTransform,
) -> Result<Aabb, String> {
    let mut bounds = Aabb::empty();
    for i in 0..=32 {
        let t = f64::from(i) / 32.;
        for [col, row] in [
            [t * f64::from(ds.width()), 0.],
            [t * f64::from(ds.width()), f64::from(ds.height())],
            [0., t * f64::from(ds.height())],
            [f64::from(ds.width()), t * f64::from(ds.height())],
        ] {
            let q = xf
                .apply(affine.apply(col, row))
                .ok_or("栅格范围坐标转换失败。")?;
            if q.iter().any(|v| !v.is_finite()) {
                return Err("栅格范围包含无效坐标。".into());
            }
            bounds.extend_point(q);
        }
    }
    Ok(bounds)
}

/// Mosaic on the first input's CRS and pixel size. Values are written as Float32;
/// inputs are sampled with nearest-neighbor resampling and source NoData is ignored.
pub fn mosaic(
    request: MosaicRequest,
    operator: MosaicOperator,
    cancel: &AtomicBool,
    progress: &AtomicU32,
) -> Result<(), String> {
    if request.inputs.is_empty() {
        return Err("至少选择一个输入栅格。".into());
    }
    if request.output.exists() {
        return Err("输出文件已存在，请选择新文件名。".into());
    }
    let mut datasets = request
        .inputs
        .iter()
        .map(|path| RasterDataset::open(path).map_err(|e| format!("{}：{e}", path.display())))
        .collect::<Result<Vec<_>, _>>()?;
    let output_crs = datasets[0].georeference().crs.clone();
    let Some(base_transform) = datasets[0].georeference().transform else {
        return Err("首个输入栅格缺少地理参考。".into());
    };
    let cell_x = base_transform.a.abs();
    let cell_y = base_transform.e.abs();
    if cell_x == 0.
        || cell_y == 0.
        || base_transform.b.abs() > cell_x * 1e-10
        || base_transform.d.abs() > cell_y * 1e-10
        || base_transform.a <= 0.
        || base_transform.e >= 0.
    {
        return Err("首个输入栅格必须是北向上栅格（像元列向东、行向南），且像元大小有效。".into());
    }
    let epsg = match output_crs.crs {
        CrsId::Epsg(n) if n > 0 && n < 32767 => n as u16,
        _ => return Err("输出 GeoTIFF 需要已识别的标准 EPSG 坐标系。".into()),
    };
    let mut sources = Vec::with_capacity(datasets.len());
    let mut extent = Aabb::empty();
    let mut band_count = 0;
    for ds in datasets.drain(..) {
        let affine = ds
            .georeference()
            .transform
            .ok_or_else(|| format!("{} 缺少地理参考。", ds.path().display()))?;
        let to_output = projection_transform(&ds.georeference().crs, &output_crs)?;
        let bounds = transformed_bounds(&ds, affine, to_output)?;
        extent.extend_point(bounds.min);
        extent.extend_point(bounds.max);
        band_count = band_count.max(ds.band_count());
        sources.push(Source {
            dataset: ds,
            transform: affine,
            to_output,
        });
    }
    if band_count == 0 || band_count > 256 {
        return Err("输出波段数不受支持（1–256）。".into());
    }
    // Align the output grid to the first input's origin, as ArcGIS does for its default cell size.
    let left = base_transform.c + ((extent.min[0] - base_transform.c) / cell_x).floor() * cell_x;
    let top = base_transform.f - ((base_transform.f - extent.max[1]) / cell_y).floor() * cell_y;
    let width = (((extent.max[0] - left) / cell_x).ceil() as u64).max(1);
    let height = (((top - extent.min[1]) / cell_y).ceil() as u64).max(1);
    if width > u32::MAX as u64
        || height > u32::MAX as u64
        || width.saturating_mul(height) > 2_000_000_000
    {
        return Err("输出栅格过大。".into());
    }
    let (width, height) = (width as u32, height as u32);
    let row_bytes = u64::from(width) * band_count as u64 * 4;
    if row_bytes > 64 * 1024 * 1024 {
        return Err("输出单行超过 64 MiB。".into());
    }
    let rows = (8 * 1024 * 1024 / row_bytes.max(1)).clamp(1, 128) as u32;
    let temp = request.output.with_extension(format!(
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
        .open(&temp)
        .map_err(|e| e.to_string())?;
    let temporary = Temporary(temp);
    let result = (|| -> Result<(), String> {
        let mut encoder = TiffEncoder::new_big(file).map_err(|e| e.to_string())?;
        let mut image = encoder
            .new_image::<Gray32Float>(width, height)
            .map_err(|e| e.to_string())?;
        if band_count > 1 {
            image
                .extra_samples(&vec![ExtraSamples::Unspecified; band_count - 1])
                .map_err(|e| e.to_string())?;
        }
        image.rows_per_strip(rows).map_err(|e| e.to_string())?;
        image
            .encoder()
            .write_tag(
                Tag::ModelTransformationTag,
                &[
                    cell_x, 0., 0., left, 0., -cell_y, 0., top, 0., 0., 1., 0., 0., 0., 0., 1.,
                ][..],
            )
            .map_err(|e| e.to_string())?;
        let geographic = output_crs.model == crate::geo::ProjectionModel::Geographic;
        image
            .encoder()
            .write_tag(
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
            )
            .map_err(|e| e.to_string())?;
        image
            .encoder()
            .write_tag(Tag::GdalNodata, "nan")
            .map_err(|e| e.to_string())?;
        for y0 in (0..height).step_by(rows as usize) {
            if cancel.load(Ordering::Relaxed) {
                return Err("操作已取消。".into());
            }
            let h = rows.min(height - y0);
            let mut out = vec![f32::NAN; width as usize * h as usize * band_count];
            let mut counts = vec![0u16; width as usize * h as usize * band_count];
            let mut weights = vec![0.0f32; width as usize * h as usize * band_count];
            for source in &mut sources {
                if cancel.load(Ordering::Relaxed) {
                    return Err("操作已取消。".into());
                }
                let mut positions = Vec::with_capacity(width as usize * h as usize);
                let mut win_bounds = Aabb::empty();
                for y in 0..h {
                    for x in 0..width {
                        let world = [
                            left + (f64::from(x) + 0.5) * cell_x,
                            top - (f64::from(y0 + y) + 0.5) * cell_y,
                        ];
                        let p = source
                            .to_output
                            .unapply(world)
                            .and_then(|p| source.transform.inverse(p[0], p[1]));
                        if let Some(q) = p.filter(|q| q.iter().all(|v| v.is_finite())) {
                            let col = q[0].floor() as i64;
                            let row = q[1].floor() as i64;
                            positions.push(Some((col, row)));
                            if col >= 0
                                && row >= 0
                                && col < i64::from(source.dataset.width())
                                && row < i64::from(source.dataset.height())
                            {
                                win_bounds.extend_point([col as f64, row as f64]);
                            }
                        } else {
                            positions.push(None);
                        }
                    }
                }
                if win_bounds.is_empty() {
                    continue;
                }
                let x0 = win_bounds.min[0].floor().max(0.) as u32;
                let y1 = win_bounds.min[1].floor().max(0.) as u32;
                let x1 = (win_bounds.max[0].floor() as u32 + 1).min(source.dataset.width());
                let y2 = (win_bounds.max[1].floor() as u32 + 1).min(source.dataset.height());
                let win = PixelWindow::new(x0, y1, x1.saturating_sub(x0), y2.saturating_sub(y1));
                if win.is_empty() {
                    continue;
                }
                let read_bands: Vec<_> = (0..source.dataset.band_count()).collect();
                let samples = source
                    .dataset
                    .read_level_bands(0, win, &read_bands)
                    .map_err(|e| e.to_string())?;
                for (pixel, pos) in positions.into_iter().enumerate() {
                    let Some((col, row)) = pos else { continue };
                    if col < i64::from(win.x)
                        || row < i64::from(win.y)
                        || col >= i64::from(win.x + win.width)
                        || row >= i64::from(win.y + win.height)
                    {
                        continue;
                    };
                    let src = (row - i64::from(win.y)) as usize * win.width as usize
                        + (col - i64::from(win.x)) as usize;
                    let blend_weight = (col as f64 + 0.5)
                        .min(row as f64 + 0.5)
                        .min(f64::from(source.dataset.width()) - col as f64 - 0.5)
                        .min(f64::from(source.dataset.height()) - row as f64 - 0.5)
                        .max(0.5) as f32;
                    for b in 0..source.dataset.band_count() {
                        let v = samples[b][src];
                        let nodata = source.dataset.band(b).and_then(|bi| bi.nodata);
                        if !v.is_finite() || nodata.is_some_and(|n| v == n as f32) {
                            continue;
                        }
                        let idx = pixel * band_count + b;
                        let old = out[idx];
                        let count = counts[idx];
                        out[idx] = match operator {
                            MosaicOperator::First => {
                                if count == 0 {
                                    v
                                } else {
                                    old
                                }
                            }
                            MosaicOperator::Last => v,
                            MosaicOperator::Blend => {
                                let sum = if count == 0 { 0.0 } else { old };
                                sum + v * blend_weight
                            }
                            MosaicOperator::Minimum => {
                                if count == 0 {
                                    v
                                } else {
                                    old.min(v)
                                }
                            }
                            MosaicOperator::Maximum => {
                                if count == 0 {
                                    v
                                } else {
                                    old.max(v)
                                }
                            }
                            MosaicOperator::Mean | MosaicOperator::Sum => {
                                if count == 0 {
                                    v
                                } else {
                                    old + v
                                }
                            }
                        };
                        if matches!(operator, MosaicOperator::Blend) {
                            weights[idx] += blend_weight;
                        }
                        counts[idx] = count.saturating_add(1);
                    }
                }
            }
            if matches!(operator, MosaicOperator::Mean) {
                for (v, count) in out.iter_mut().zip(counts) {
                    if count > 0 {
                        *v /= f32::from(count);
                    }
                }
            }
            if matches!(operator, MosaicOperator::Blend) {
                for (value, weight) in out.iter_mut().zip(weights) {
                    if weight > 0.0 {
                        *value /= weight;
                    }
                }
            }
            image.write_strip(&out).map_err(|e| e.to_string())?;
            progress.store(
                ((y0 + h) as f64 / f64::from(height) * 1000.) as u32,
                Ordering::Relaxed,
            );
        }
        image.finish().map_err(|e| e.to_string())?;
        Ok(())
    })();
    result?;
    if cancel.load(Ordering::Relaxed) {
        return Err("操作已取消。".into());
    }
    OpenOptions::new()
        .write(true)
        .open(&temporary.0)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    std::fs::hard_link(&temporary.0, &request.output).map_err(|e| format!("发布输出失败：{e}"))?;
    Ok(())
}
