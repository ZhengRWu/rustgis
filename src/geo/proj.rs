//! 坐标参考系（CRS）与按需坐标重投影。
//!
//! 对标 QGIS / GDAL 的「工程 CRS + 图层 CRS」模型：图层几何 / 影像始终按**源 CRS**
//! 保存（工程文件里的几何坐标因此不需要改写），绘制时再按「图层 CRS → 工程 CRS」
//! 逐点变换。栅格还会把每块贴图细分成网格、逐顶点变换，所以 UTM 影像投到经纬度工程
//! 里时会像 QGIS 一样呈梯形，而不是被拉成一个轴对齐矩形。
//!
//! 这里实现的是**纯 Rust、无 C 依赖**的常用投影子集（QGIS 默认走 PROJ，本模块的
//! 覆盖面小得多，但覆盖了本项目遇到的常见数据）：
//!
//! - 地理坐标系：WGS 84 / CGCS2000 / NAD83 / ETRS89（椭球差异在毫米级，统一按
//!   WGS 84 椭球处理）；
//! - 横轴墨卡托：WGS 84 UTM 北 / 南半球（EPSG:326xx / 327xx）；
//! - 高斯-克吕格：CGCS2000 的 6°/3° 分带与按中央经线编号的 EPSG 代码
//!   （4491–4554，含工程默认的 4549）；
//! - Web 墨卡托（EPSG:3857）。
//!
//! 识别不了的 CRS 返回 `None`：调用方保持旧行为（不重投影），绝不猜错坐标系。

use super::{Aabb, Coord2D, CrsId, ProjectionInfo, ProjectionModel};

/// WGS 84 椭球长半轴（米）。
const WGS84_A: f64 = 6_378_137.0;
/// WGS 84 椭球扁率倒数。
const WGS84_INV_F: f64 = 298.257_223_563;

/// 参考椭球。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ellipsoid {
    /// 长半轴（米）。
    pub a: f64,
    /// 扁率倒数。
    pub inv_f: f64,
}

impl Ellipsoid {
    /// WGS 84（GPS 与 S2 等卫星影像的默认椭球）。
    pub const WGS84: Self = Self {
        a: WGS84_A,
        inv_f: WGS84_INV_F,
    };

    fn f(&self) -> f64 {
        1.0 / self.inv_f
    }

    /// 第一偏心率平方。
    fn e2(&self) -> f64 {
        let f = self.f();
        2.0 * f - f * f
    }

    /// 第二偏心率平方 `e'^2 = e^2 / (1 - e^2)`。
    fn ep2(&self) -> f64 {
        let e2 = self.e2();
        e2 / (1.0 - e2)
    }

    /// 卯酉圈曲率半径 `N`。
    fn prime_vertical(&self, phi: f64) -> f64 {
        self.a / (1.0 - self.e2() * phi.sin().powi(2)).sqrt()
    }

    /// 子午圈曲率半径 `M`（逆变换里修正纬度用）。
    fn meridian_radius(&self, phi: f64) -> f64 {
        let e2 = self.e2();
        self.a * (1.0 - e2) / (1.0 - e2 * phi.sin().powi(2)).powf(1.5)
    }

    /// 赤道到纬度 `phi` 的子午线弧长（Snyder 级数，展开到 e^6，毫米级）。
    fn meridian_arc(&self, phi: f64) -> f64 {
        let e2 = self.e2();
        let e4 = e2 * e2;
        let e6 = e4 * e2;
        self.a
            * ((1.0 - e2 / 4.0 - 3.0 * e4 / 64.0 - 5.0 * e6 / 256.0) * phi
                - (3.0 * e2 / 8.0 + 3.0 * e4 / 32.0 + 45.0 * e6 / 1024.0) * (2.0 * phi).sin()
                + (15.0 * e4 / 256.0 + 45.0 * e6 / 1024.0) * (4.0 * phi).sin()
                - (35.0 * e6 / 3072.0) * (6.0 * phi).sin())
    }
}

/// 横轴墨卡托（UTM / 高斯-克吕格）。
///
/// 公式取自 Snyder《Map Projections — A Working Manual》的 Transverse Mercator
/// 级数（UTM 与国内 6°/3° 带都只是参数不同）。中央经线附近误差远小于 1 mm，
/// 带边缘（±3°）也在厘米级，足够压图像素。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransverseMercator {
    /// 中央经线（度）。
    pub lon0_deg: f64,
    /// 原点纬度（度），UTM / 高斯-克吕格均为 0。
    pub lat0_deg: f64,
    /// 中央经线比例因子。
    pub k0: f64,
    /// 假东（米）。
    pub false_easting: f64,
    /// 假北（米）。
    pub false_northing: f64,
    /// 参考椭球。
    pub ellipsoid: Ellipsoid,
}

impl TransverseMercator {
    /// UTM 第 `zone` 带（1..=60）。
    pub fn utm(zone: u32, south: bool) -> Self {
        Self {
            lon0_deg: 6.0 * f64::from(zone) - 183.0,
            lat0_deg: 0.0,
            k0: 0.9996,
            false_easting: 500_000.0,
            false_northing: if south { 10_000_000.0 } else { 0.0 },
            ellipsoid: Ellipsoid::WGS84,
        }
    }

    /// CGCS2000 高斯-克吕格按带号（6° 带 → 中央经线 `6z-3`；3° 带 → `3z`）。
    ///
    /// 国内 6° 带 EPSG 习惯把带号放进假东（`z * 1_000_000`）。
    pub fn gauss_kruger_zone(zone: u32, three_degree: bool) -> Self {
        let lon0 = if three_degree {
            3.0 * f64::from(zone)
        } else {
            6.0 * f64::from(zone) - 3.0
        };
        Self {
            lon0_deg: lon0,
            lat0_deg: 0.0,
            k0: 1.0,
            false_easting: f64::from(zone) * 1_000_000.0,
            false_northing: 0.0,
            ellipsoid: Ellipsoid::WGS84,
        }
    }

    /// CGCS2000 高斯-克吕格按中央经线（假东固定 500 km，即 `CM_117E` 这类）。
    pub fn gauss_kruger_cm(lon0_deg: f64) -> Self {
        Self {
            lon0_deg,
            lat0_deg: 0.0,
            k0: 1.0,
            false_easting: 500_000.0,
            false_northing: 0.0,
            ellipsoid: Ellipsoid::WGS84,
        }
    }

    /// 经纬度（度）→ 投影坐标（米）。
    pub fn forward(&self, lon_deg: f64, lat_deg: f64) -> Option<Coord2D> {
        if !lon_deg.is_finite() || !lat_deg.is_finite() || lat_deg.abs() >= 90.0 {
            return None;
        }
        let ep2 = self.ellipsoid.ep2();
        let phi = lat_deg.to_radians();
        let dlam = (lon_deg - self.lon0_deg).to_radians();
        let (sin_phi, cos_phi) = phi.sin_cos();
        if cos_phi.abs() < 1e-12 {
            return None;
        }
        let tan_phi = sin_phi / cos_phi;
        let t = tan_phi * tan_phi;
        let c = ep2 * cos_phi * cos_phi;
        let a = dlam * cos_phi;
        let n = self.ellipsoid.prime_vertical(phi);
        // 从原点纬度量起的子午线弧长。
        let m = self.ellipsoid.meridian_arc(phi)
            - self.ellipsoid.meridian_arc(self.lat0_deg.to_radians());

        let x = self.false_easting
            + self.k0
                * n
                * (a + (1.0 - t + c) * a.powi(3) / 6.0
                    + (5.0 - 18.0 * t + t * t + 72.0 * c - 58.0 * ep2) * a.powi(5) / 120.0);
        let y = self.false_northing
            + self.k0
                * (m + n
                    * tan_phi
                    * (a * a / 2.0
                        + (5.0 - t + 9.0 * c + 4.0 * c * c) * a.powi(4) / 24.0
                        + (61.0 - 58.0 * t + t * t + 600.0 * c - 330.0 * ep2) * a.powi(6) / 720.0));
        (x.is_finite() && y.is_finite()).then_some([x, y])
    }

    /// 投影坐标（米）→ 经纬度（度）。
    pub fn inverse(&self, x: f64, y: f64) -> Option<Coord2D> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let e2 = self.ellipsoid.e2();
        let ep2 = self.ellipsoid.ep2();
        let m = (y - self.false_northing) / self.k0
            + self.ellipsoid.meridian_arc(self.lat0_deg.to_radians());
        // 足点纬度。
        let mu = m
            / (self.ellipsoid.a
                * (1.0 - e2 / 4.0 - 3.0 * e2 * e2 / 64.0 - 5.0 * e2.powi(3) / 256.0));
        let e1 = (1.0 - (1.0 - e2).sqrt()) / (1.0 + (1.0 - e2).sqrt());
        let phi1 = mu
            + (3.0 * e1 / 2.0 - 27.0 * e1.powi(3) / 32.0) * (2.0 * mu).sin()
            + (21.0 * e1 * e1 / 16.0 - 55.0 * e1.powi(4) / 32.0) * (4.0 * mu).sin()
            + (151.0 * e1.powi(3) / 96.0) * (6.0 * mu).sin()
            + (1097.0 * e1.powi(4) / 512.0) * (8.0 * mu).sin();
        let (sin1, cos1) = phi1.sin_cos();
        if cos1.abs() < 1e-12 {
            return None;
        }
        let c1 = ep2 * cos1 * cos1;
        let t1 = (sin1 / cos1).powi(2);
        let n1 = self.ellipsoid.prime_vertical(phi1);
        let r1 = self.ellipsoid.meridian_radius(phi1);
        let d = (x - self.false_easting) / (n1 * self.k0);

        let phi = phi1
            - (n1 * sin1 / cos1 / r1)
                * (d * d / 2.0
                    - (5.0 + 3.0 * t1 + 10.0 * c1 - 4.0 * c1 * c1 - 9.0 * ep2) * d.powi(4) / 24.0
                    + (61.0 + 90.0 * t1 + 298.0 * c1 + 45.0 * t1 * t1
                        - 252.0 * ep2
                        - 3.0 * c1 * c1)
                        * d.powi(6)
                        / 720.0);
        let lam = self.lon0_deg.to_radians()
            + (d - (1.0 + 2.0 * t1 + c1) * d.powi(3) / 6.0
                + (5.0 - 2.0 * c1 + 28.0 * t1 - 3.0 * c1 * c1 + 8.0 * ep2 + 24.0 * t1 * t1)
                    * d.powi(5)
                    / 120.0)
                / cos1;
        let out = [lam.to_degrees(), phi.to_degrees()];
        (out[0].is_finite() && out[1].is_finite()).then_some(out)
    }
}

/// 本模块认识的 CRS。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Crs {
    /// 地理坐标系（经纬度，度）。
    Geographic,
    /// WGS 84 / Pseudo-Mercator（EPSG:3857）。
    WebMercator,
    /// 横轴墨卡托（UTM / 高斯-克吕格）。
    TransverseMercator(TransverseMercator),
}

impl Crs {
    /// 从图层 / 工程里保存的 [`ProjectionInfo`] 解析，无法识别时返回 `None`。
    pub fn from_projection(info: &ProjectionInfo) -> Option<Self> {
        match &info.crs {
            CrsId::Epsg(code) => Self::from_epsg(*code).or(match info.model {
                ProjectionModel::Geographic => Some(Self::Geographic),
                _ => None,
            }),
            CrsId::Name(name) => Self::from_name(name, info.model),
            CrsId::None => match info.model {
                ProjectionModel::Geographic => Some(Self::Geographic),
                _ => None,
            },
        }
    }

    /// EPSG 代码 → CRS。
    pub fn from_epsg(code: u32) -> Option<Self> {
        match code {
            // 地理坐标系：WGS 84 / CGCS2000 / NAD83 / ETRS89（统一按 WGS 84 椭球）。
            4326 | 4490 | 4269 | 4258 | 4979 => Some(Self::Geographic),
            3857 => Some(Self::WebMercator),
            // WGS 84 UTM：北半球 326xx、南半球 327xx。
            32601..=32660 => Some(Self::TransverseMercator(TransverseMercator::utm(
                code - 32600,
                false,
            ))),
            32701..=32760 => Some(Self::TransverseMercator(TransverseMercator::utm(
                code - 32700,
                true,
            ))),
            // CGCS2000 6° 带（按带号 13..=23）。
            4491..=4501 => Some(Self::TransverseMercator(
                TransverseMercator::gauss_kruger_zone(code - 4478, false),
            )),
            // CGCS2000 6° 带（按中央经线 75E..=135E）。
            4502..=4512 => Some(Self::TransverseMercator(
                TransverseMercator::gauss_kruger_cm(75.0 + 6.0 * f64::from(code - 4502)),
            )),
            // CGCS2000 3° 带（按带号 25..=45）。
            4513..=4533 => Some(Self::TransverseMercator(
                TransverseMercator::gauss_kruger_zone(code - 4488, true),
            )),
            // CGCS2000 3° 带（按中央经线 75E..=135E，步长 3°）。
            4534..=4554 => Some(Self::TransverseMercator(
                TransverseMercator::gauss_kruger_cm(75.0 + 3.0 * f64::from(code - 4534)),
            )),
            _ => None,
        }
    }

    /// 从 WKT 名称识别（ArcGIS 的 `.prj` 常常只给名字、没有 EPSG 代码）。
    pub fn from_name(name: &str, model: ProjectionModel) -> Option<Self> {
        let up = name.to_ascii_uppercase();
        if up.contains("WEB_MERCATOR")
            || up.contains("PSEUDO_MERCATOR")
            || up.contains("AUXILIARY_SPHERE")
        {
            return Some(Self::WebMercator);
        }
        if let Some(tm) = utm_from_name(&up) {
            return Some(Self::TransverseMercator(tm));
        }
        if let Some(tm) = gauss_kruger_from_name(&up) {
            return Some(Self::TransverseMercator(tm));
        }
        // 纯地理名称（如 `GCS_WGS_1984`）：所有地理坐标系在本项目里等价。
        match model {
            ProjectionModel::Geographic => Some(Self::Geographic),
            _ => None,
        }
    }

    /// 本 CRS → 地理坐标（经纬度，度）。
    pub fn to_geographic(self, p: Coord2D) -> Option<Coord2D> {
        match self {
            Self::Geographic => Some(p),
            Self::WebMercator => web_mercator_inverse(p),
            Self::TransverseMercator(tm) => tm.inverse(p[0], p[1]),
        }
    }

    /// 地理坐标（经纬度，度）→ 本 CRS。
    pub fn project(self, p: Coord2D) -> Option<Coord2D> {
        match self {
            Self::Geographic => Some(p),
            Self::WebMercator => web_mercator_forward(p),
            Self::TransverseMercator(tm) => tm.forward(p[0], p[1]),
        }
    }
}

fn web_mercator_forward(p: Coord2D) -> Option<Coord2D> {
    let lon = p[0].to_radians();
    let lat = p[1].clamp(-85.051_128_78, 85.051_128_78).to_radians();
    let x = WGS84_A * lon;
    let y = WGS84_A * (std::f64::consts::FRAC_PI_4 + lat / 2.0).tan().ln();
    (x.is_finite() && y.is_finite()).then_some([x, y])
}

fn web_mercator_inverse(p: Coord2D) -> Option<Coord2D> {
    let lon = p[0] / WGS84_A;
    let lat = 2.0 * (p[1] / WGS84_A).exp().atan() - std::f64::consts::FRAC_PI_2;
    let out = [lon.to_degrees(), lat.to_degrees()];
    (out[0].is_finite() && out[1].is_finite()).then_some(out)
}

/// 从名称里识别 UTM（如 `WGS_1984_UTM_Zone_32N`）。
fn utm_from_name(up: &str) -> Option<TransverseMercator> {
    if !up.contains("UTM") {
        return None;
    }
    let zone = number_after(up, "ZONE").or_else(|| number_after(up, "UTM"))?;
    if !(1..=60).contains(&zone) {
        return None;
    }
    // 带号后面的第一个字母：N = 北半球，S = 南半球。
    let south =
        up.contains("SOUTH") || after_number(up, zone).is_some_and(|tail| tail.starts_with('S'));
    Some(TransverseMercator::utm(zone, south))
}

/// 从名称里识别 CGCS2000 高斯-克吕格（`..._3_DEGREE_GK_CM_117E` / `..._GK_Zone_39`）。
fn gauss_kruger_from_name(up: &str) -> Option<TransverseMercator> {
    if !(up.contains("GAUSS") || up.contains("KRUGER") || up.contains("_GK_")) {
        return None;
    }
    if let Some(cm) = number_after(up, "CM").filter(|cm| (60..=150).contains(cm)) {
        return Some(TransverseMercator::gauss_kruger_cm(f64::from(cm)));
    }
    let zone = number_after(up, "ZONE")?;
    // 名称里「3 度带」的写法有 `3_DEGREE` / `3-DEGREE` / `3 DEGREE` 三种。
    let three_degree = ["3_DEGREE", "3-DEGREE", "3 DEGREE"]
        .iter()
        .any(|mark| up.contains(mark));
    Some(TransverseMercator::gauss_kruger_zone(zone, three_degree))
}

/// 名称中 `key` 之后出现的第一个数字（`CM_117E` → 117）。
fn number_after(up: &str, key: &str) -> Option<u32> {
    let idx = up.find(key)? + key.len();
    let rest = up[idx..].trim_start_matches(|c: char| !c.is_ascii_digit());
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// 数字 `n` 之后（跳过非字母）的第一个字母起头的子串。
fn after_number(up: &str, n: u32) -> Option<&str> {
    let text = n.to_string();
    let idx = up.find(&text)? + text.len();
    Some(up[idx..].trim_start_matches(|c: char| !c.is_ascii_alphabetic()))
}

/// 包围盒四边采样段数（共 `4 * (N + 1)` 个点）。
const AABB_SAMPLES: usize = 16;

/// 一个坐标系到另一个坐标系的逐点变换。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrsTransform {
    from: Crs,
    to: Crs,
}

impl CrsTransform {
    /// 构造变换；任一侧无法识别时返回 `None`（调用方保持源坐标直用）。
    pub fn between(from: &ProjectionInfo, to: &ProjectionInfo) -> Option<Self> {
        Some(Self {
            from: Crs::from_projection(from)?,
            to: Crs::from_projection(to)?,
        })
    }

    /// 两端是同一个 CRS：逐点变换可整体跳过。
    pub fn is_identity(&self) -> bool {
        self.from == self.to
    }

    /// 源坐标 → 目标坐标。
    pub fn apply(&self, p: Coord2D) -> Option<Coord2D> {
        if self.is_identity() {
            return Some(p);
        }
        self.to.project(self.from.to_geographic(p)?)
    }

    /// 目标坐标 → 源坐标（把视口反算回源 CRS，逐要素裁剪用）。
    pub fn unapply(&self, p: Coord2D) -> Option<Coord2D> {
        if self.is_identity() {
            return Some(p);
        }
        self.from.project(self.to.to_geographic(p)?)
    }

    /// 源 CRS 包围盒 → 目标 CRS 包围盒（四边采样，因为投影不是仿射）。
    pub fn apply_aabb(&self, bounds: Aabb) -> Aabb {
        self.map_aabb(bounds, |xf, p| xf.apply(p))
    }

    /// 目标 CRS 包围盒 → 源 CRS 包围盒。
    pub fn unapply_aabb(&self, bounds: Aabb) -> Aabb {
        self.map_aabb(bounds, |xf, p| xf.unapply(p))
    }

    fn map_aabb(&self, bounds: Aabb, f: impl Fn(&Self, Coord2D) -> Option<Coord2D>) -> Aabb {
        if bounds.is_empty() {
            return bounds;
        }
        let mut out = Aabb::empty();
        for p in edge_samples(&bounds, AABB_SAMPLES) {
            if let Some(q) = f(self, p) {
                out.extend_point(q);
            }
        }
        if out.is_empty() { bounds } else { out }
    }
}

/// 包围盒四条边上的等距采样点（含角点）。
fn edge_samples(bounds: &Aabb, per_edge: usize) -> Vec<Coord2D> {
    let [x0, y0] = bounds.min;
    let [x1, y1] = bounds.max;
    let mut out = Vec::with_capacity(4 * (per_edge + 1));
    for i in 0..=per_edge {
        let t = i as f64 / per_edge as f64;
        out.push([x0 + (x1 - x0) * t, y0]);
        out.push([x0 + (x1 - x0) * t, y1]);
        out.push([x0, y0 + (y1 - y0) * t]);
        out.push([x1, y0 + (y1 - y0) * t]);
    }
    out
}

/// 图层包围盒 → 工程 CRS 下的显示包围盒；无法重投影时原样返回。
pub fn display_aabb(bounds: Aabb, from: &ProjectionInfo, to: &ProjectionInfo) -> Aabb {
    CrsTransform::between(from, to).map_or(bounds, |xf| xf.apply_aabb(bounds))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epsg(code: u32) -> ProjectionInfo {
        ProjectionInfo {
            model: ProjectionModel::Projected,
            crs: CrsId::Epsg(code),
        }
    }

    fn geographic() -> ProjectionInfo {
        ProjectionInfo {
            model: ProjectionModel::Geographic,
            crs: CrsId::Epsg(4326),
        }
    }

    fn tm(code: u32) -> TransverseMercator {
        let Crs::TransverseMercator(tm) = Crs::from_epsg(code).unwrap() else {
            panic!("EPSG:{code} 应当是横轴墨卡托");
        };
        tm
    }

    #[test]
    fn utm_zone_32_central_meridian_maps_to_false_easting() {
        let tm = tm(32632);
        assert!((tm.lon0_deg - 9.0).abs() < 1e-12);
        let p = tm.forward(9.0, 0.0).unwrap();
        assert!((p[0] - 500_000.0).abs() < 1e-6, "easting = {}", p[0]);
        assert!(p[1].abs() < 1e-6, "northing = {}", p[1]);
    }

    #[test]
    fn utm_round_trip_is_centimetre_accurate() {
        let tm = tm(32632);
        for (lon, lat) in [(9.0, 48.0), (8.5, 47.2), (9.6, 48.9), (8.99, 0.5)] {
            let xy = tm.forward(lon, lat).unwrap();
            let back = tm.inverse(xy[0], xy[1]).unwrap();
            assert!(
                (back[0] - lon).abs() < 1e-8 && (back[1] - lat).abs() < 1e-8,
                "({lon},{lat}) → {xy:?} → {back:?}"
            );
        }
    }

    #[test]
    fn south_utm_has_ten_million_false_northing() {
        let tm = tm(32732);
        assert!((tm.false_northing - 10_000_000.0).abs() < 1e-9);
        let p = tm.forward(9.0, -0.5).unwrap();
        assert!((p[0] - 500_000.0).abs() < 1e-6);
        // 赤道附近南纬 0.5°：北向坐标略小于 1000 万。
        assert!(
            p[1] > 9_900_000.0 && p[1] < 10_000_000.0,
            "northing = {}",
            p[1]
        );
    }

    #[test]
    fn cgcs2000_three_degree_cm_4549_uses_120e() {
        // 4534 = CM 75E，之后每 +1 东移 3°：4549 = 120E。
        let tm = tm(4549);
        assert!((tm.lon0_deg - 120.0).abs() < 1e-12);
        assert!((tm.false_easting - 500_000.0).abs() < 1e-9);
        let p = tm.forward(120.0, 30.0).unwrap();
        assert!((p[0] - 500_000.0).abs() < 1e-6);
        // 中央经线上东坐标 = 假东，北坐标就是子午线弧长，北纬 30° 约 3 320 113 m。
        assert!((p[1] - 3_320_113.4).abs() < 1.0, "northing = {}", p[1]);
    }

    #[test]
    fn geographic_to_utm_transform_round_trips() {
        let xf = CrsTransform::between(&geographic(), &epsg(32632)).unwrap();
        assert!(!xf.is_identity());
        let xy = xf.apply([9.2, 48.4]).unwrap();
        assert!(xy[0] > 500_000.0 && xy[1] > 5_000_000.0, "{xy:?}");
        let back = xf.unapply(xy).unwrap();
        assert!((back[0] - 9.2).abs() < 1e-8 && (back[1] - 48.4).abs() < 1e-8);
    }

    #[test]
    fn same_geographic_crs_is_identity() {
        let xf = CrsTransform::between(&geographic(), &geographic()).unwrap();
        assert!(xf.is_identity());
        assert_eq!(xf.apply([116.4, 39.9]), Some([116.4, 39.9]));
    }

    #[test]
    fn wkt_name_without_epsg_is_recognized() {
        let info = ProjectionInfo {
            model: ProjectionModel::Projected,
            crs: CrsId::Name("WGS_1984_UTM_Zone_32N".into()),
        };
        let Crs::TransverseMercator(tm) = Crs::from_projection(&info).unwrap() else {
            panic!("UTM 名称未被识别");
        };
        assert!((tm.lon0_deg - 9.0).abs() < 1e-12);
        assert_eq!(tm.false_northing, 0.0);
    }

    #[test]
    fn geographic_name_is_recognized() {
        let info = ProjectionInfo {
            model: ProjectionModel::Geographic,
            crs: CrsId::Name("GCS_WGS_1984".into()),
        };
        assert_eq!(Crs::from_projection(&info), Some(Crs::Geographic));
    }

    /// GDAL / ArcGIS 常见的 CGCS2000 高斯-克吕格 WKT 名称（3 度带按带号 / 按中央经线）。
    #[test]
    fn gauss_kruger_wkt_names_are_recognized() {
        let zoned = ProjectionInfo {
            model: ProjectionModel::Projected,
            crs: CrsId::Name("CGCS2000 / 3-degree Gauss-Kruger zone 39".into()),
        };
        let Crs::TransverseMercator(tm) = Crs::from_projection(&zoned).unwrap() else {
            panic!("3 度带带号名称未被识别");
        };
        assert!((tm.lon0_deg - 117.0).abs() < 1e-12, "带号 39 应为 117°E");
        assert!((tm.false_easting - 39_000_000.0).abs() < 1e-9);

        let centered = ProjectionInfo {
            model: ProjectionModel::Projected,
            crs: CrsId::Name("CGCS2000 / 3-degree Gauss-Kruger CM 117E".into()),
        };
        let Crs::TransverseMercator(tm) = Crs::from_projection(&centered).unwrap() else {
            panic!("中央经线名称未被识别");
        };
        assert!((tm.lon0_deg - 117.0).abs() < 1e-12);
        assert!((tm.false_easting - 500_000.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_projected_crs_is_not_guessed() {
        let info = ProjectionInfo {
            model: ProjectionModel::Projected,
            crs: CrsId::None,
        };
        assert_eq!(Crs::from_projection(&info), None);
        assert_eq!(CrsTransform::between(&info, &epsg(32632)), None);
    }

    #[test]
    fn aabb_edges_are_sampled() {
        let xf = CrsTransform::between(&geographic(), &epsg(32632)).unwrap();
        let source = Aabb {
            min: [8.0, 47.0],
            max: [10.0, 49.0],
        };
        let out = xf.apply_aabb(source);
        assert!(!out.is_empty());
        assert!(out.width() > 100_000.0 && !out.contains_aabb(&source));
        // 投影后的范围是曲面梯形的外接矩形，反算回来会比源范围略大，但必须完整覆盖源范围。
        let back = xf.unapply_aabb(out);
        assert!(
            back.contains_aabb(&source),
            "back = {back:?} / source = {source:?}"
        );
        assert!(
            (back.width() - source.width()).abs() < 0.2
                && (back.height() - source.height()).abs() < 0.2,
            "反算包围盒膨胀过大：{back:?}"
        );
    }
}
