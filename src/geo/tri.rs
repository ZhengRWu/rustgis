//! 多边形三角剖分 —— 委托给 [`geo`] 的 Earcut 实现（底层 `earcut` crate）。
//!
//! 本模块只负责三件事：
//!
//! 1. 把「外环 + 该外环的孔」翻译成 [`geo::Polygon`]；
//! 2. 调用 `earcut_triangles_raw()` 并摊平成渲染层要的 `[f64; 6]`；
//! 3. 顶点规模大时按环切块做多线程（各环互不影响，结果按输入顺序拼接）。
//!
//! 剖分算法本身（含孔洞处理、耳切顺序）由 `earcut` 提供，这里不再自研。
//! 退化输入（点少于 3 个、共线、含 NaN/Inf）一律返回空三角，绝不 panic。

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread;

use geo::algorithm::bool_ops::{BooleanOps, OpType};
use geo::algorithm::intersects::Intersects;
use geo::algorithm::triangulate_earcut::TriangulateEarcut;
use geo::{Coord, LineString, Polygon};

/// 一个三角形：`[x0, y0, x1, y1, x2, y2]`。
pub type Tri = [f64; 6];

/// 顶点数超过该阈值才启用多线程：小图层直接串行，避免线程开销盖过收益。
pub const PARALLEL_MIN_VERTICES: usize = 10_000;

/// 并行时最多使用的工作线程数。
pub const MAX_WORKERS: usize = 8;

/// 把所有外环各自当作一个多边形剖分（无孔）。
///
/// 等价于 [`triangulate_polygons`] 传空的孔列表，保留此入口是为了兼容既有调用方
/// （导入路径统一走 `triangulate_polygons`，只有探针测试还用它）。
#[allow(dead_code)] // 兼容入口：当前调用方统一走 triangulate_polygons。
pub fn triangulate_rings(rings: &[Vec<[f64; 2]>]) -> Vec<Tri> {
    triangulate_polygons(rings, &[])
}

/// 剖分一组多边形：`outers[i]` 与 `holes[i]`（可能为空）配对。
///
/// * `holes` 比 `outers` 短（或缺失某下标）时，对应多边形按无孔处理；
/// * 输出按输入顺序拼接，与串行结果逐字节一致（测试保证）；
/// * 单个环顶点数超过 `MAX_WORKERS` 分块时，不影响结果顺序。
pub fn triangulate_polygons(outers: &[Vec<[f64; 2]>], holes: &[Vec<Vec<[f64; 2]>>]) -> Vec<Tri> {
    if outers.is_empty() {
        return Vec::new();
    }

    let vertices: usize = outers.iter().map(|ring| ring.len()).sum::<usize>()
        + holes.iter().flatten().map(|ring| ring.len()).sum::<usize>();

    let workers = worker_count(outers.len(), vertices);
    if workers <= 1 {
        let mut out = Vec::new();
        for (i, outer) in outers.iter().enumerate() {
            out.extend(triangulate_one(outer, holes_at(holes, i)));
        }
        return out;
    }

    let chunk = outers.len().div_ceil(workers);
    let mut out = Vec::new();
    thread::scope(|scope| {
        let handles: Vec<_> = outers
            .chunks(chunk)
            .enumerate()
            .map(|(index, slice)| {
                let base = index * chunk;
                scope.spawn(move || {
                    let mut local = Vec::new();
                    for (offset, outer) in slice.iter().enumerate() {
                        local.extend(triangulate_one(outer, holes_at(holes, base + offset)));
                    }
                    local
                })
            })
            .collect();
        for handle in handles {
            // 工作线程内部已用 catch_unwind 兜住 earcut 的 panic；万一仍有
            // 意外，退化为「丢弃该块」而不是让整个导入崩掉。
            if let Ok(part) = handle.join() {
                out.extend(part);
            }
        }
    });
    out
}

/// 一个多边形的剖分（外环 + 孔），返回扁平的三角形列表。
fn triangulate_one(outer: &[[f64; 2]], holes: &[Vec<[f64; 2]>]) -> Vec<Tri> {
    if outer.len() < 3 {
        return Vec::new();
    }
    let exterior = ring(outer);
    if exterior.0.len() < 4 {
        // 去重/闭合后不足一个三角形。
        return Vec::new();
    }
    let interiors: Vec<LineString<f64>> = holes
        .iter()
        .filter(|hole| hole.len() >= 3)
        .map(|hole| ring(hole))
        .filter(|hole| hole.0.len() >= 4)
        .collect();
    let polygon = Polygon::new(exterior, interiors);

    // earcut 对极度退化的输入理论上仍可能 panic（索引越界之类），
    // 兜住它并退化为「该多边形不填充」，与旧实现的「绝不 panic」约定一致。
    let raw = match catch_unwind(AssertUnwindSafe(|| polygon.earcut_triangles_raw())) {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::with_capacity(raw.triangle_indices.len() / 3);
    for index in raw.triangle_indices.as_chunks::<3>().0 {
        let (Some(a), Some(b), Some(c)) = (
            raw.vertices.get(index[0]),
            raw.vertices.get(index[1]),
            raw.vertices.get(index[2]),
        ) else {
            continue;
        };
        out.push([a[0], a[1], b[0], b[1], c[0], c[1]]);
    }
    out
}

/// `[x, y]` 点列 → 关闭的 [`LineString`]，同时剔除 NaN/Inf 顶点。
fn ring(points: &[[f64; 2]]) -> LineString<f64> {
    let coords: Vec<Coord<f64>> = points
        .iter()
        .filter(|p| p[0].is_finite() && p[1].is_finite())
        .map(|p| Coord { x: p[0], y: p[1] })
        .collect();
    // `Polygon::new` 会调用 `close()` 补上收尾点，这里不需要自己补。
    LineString::new(coords)
}

fn holes_at(holes: &[Vec<Vec<[f64; 2]>>], index: usize) -> &[Vec<[f64; 2]>] {
    holes.get(index).map(Vec::as_slice).unwrap_or(&[])
}

// ─────────────────────────── 自相交环的合法化 ───────────────────────────

/// 环是否「简单」（除相邻边共享端点外，任何两条边都不相交）。
///
/// 数字化画出的蝴蝶结（路径交叉）不是简单环——earcut 对这类输入会剖出错误的
/// 三角片（渲染成残缺的碎片），必须先拆分。
pub fn ring_is_simple(ring: &[[f64; 2]]) -> bool {
    let n = ring.len();
    if n < 3 {
        return true;
    }
    // 线段列表：丢掉零长段（重复顶点），首尾自动闭合。
    let segs: Vec<([f64; 2], [f64; 2])> = (0..n)
        .map(|i| (ring[i], ring[(i + 1) % n]))
        .filter(|(a, b)| a != b)
        .collect();
    let m = segs.len();
    if m < 3 {
        return true;
    }
    let shares_endpoint = |(a0, a1): ([f64; 2], [f64; 2]), (b0, b1): ([f64; 2], [f64; 2])| {
        a0 == b0 || a0 == b1 || a1 == b0 || a1 == b1
    };
    for i in 0..m {
        for j in (i + 1)..m {
            // 相邻段只在共享端点处相接，豁免；非相邻段即便共享端点也算自接触。
            let adjacent = j == i + 1 || (i == 0 && j == m - 1);
            if adjacent && shares_endpoint(segs[i], segs[j]) {
                continue;
            }
            let (a0, a1) = segs[i];
            let (b0, b1) = segs[j];
            let line_a = geo::Line::new(coord(a0), coord(a1));
            let line_b = geo::Line::new(coord(b0), coord(b1));
            if line_a.intersects(&line_b) {
                return false;
            }
        }
    }
    true
}

/// 把（可能自相交的）多边形拆成简单多边形集合：`Vec<(外环, 孔)>`。
///
/// 对标 QGIS 对无效面的渲染语义：Qt `OddEvenFill` / OGC makeValid 的偶奇规则——
/// 交叉处的「翻转面积」不算实体，蝴蝶结填成两瓣（见 `QgsMapToolCapture` 的
/// `ValidateGeometries` 能力与无效面的显示效果）。实现走 `geo::BooleanOps`
/// 的 union，其默认填充规则就是 `FillRule::EvenOdd`。
///
/// 简单输入原样返回（不跑布尔运算：避免坐标噪声与无谓开销）。
pub fn polygon_parts(
    outer_closed: &[[f64; 2]],
    holes: &[Vec<[f64; 2]>],
) -> Vec<(Vec<[f64; 2]>, Vec<Vec<[f64; 2]>>)> {
    if ring_is_simple(outer_closed) {
        return vec![(outer_closed.to_vec(), holes.to_vec())];
    }
    let line = |pts: &[[f64; 2]]| {
        LineString::new(pts.iter().map(|p| coord(*p)).collect::<Vec<Coord<f64>>>())
    };
    let polygon = Polygon::new(
        line(outer_closed),
        holes.iter().map(|h| line(h)).collect(),
    );
    let fixed = polygon.boolean_op(&polygon, OpType::Union);
    fixed
        .0
        .iter()
        .map(|part| {
            (
                part.exterior()
                    .points()
                    .map(|p| [p.x(), p.y()])
                    .collect::<Vec<[f64; 2]>>(),
                part.interiors()
                    .iter()
                    .map(|ring| ring.points().map(|p| [p.x(), p.y()]).collect())
                    .collect(),
            )
        })
        .filter(|(outer, _)| outer.len() >= 3)
        .collect()
}

fn coord(p: [f64; 2]) -> Coord<f64> {
    Coord { x: p[0], y: p[1] }
}

/// 线程数：顶点规模不够就串行；否则按核数（上限 [`MAX_WORKERS`]）切块。
fn worker_count(rings: usize, vertices: usize) -> usize {
    if rings <= 1 || vertices < PARALLEL_MIN_VERTICES {
        return 1;
    }
    let cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    cores.min(MAX_WORKERS).min(rings).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(scale: f64, offset: f64) -> Vec<[f64; 2]> {
        vec![
            [offset, offset],
            [offset + scale, offset],
            [offset + scale, offset + scale],
            [offset, offset + scale],
        ]
    }

    fn triangle_area(tri: &Tri) -> f64 {
        let (ax, ay, bx, by, cx, cy) = (tri[0], tri[1], tri[2], tri[3], tri[4], tri[5]);
        ((bx - ax) * (cy - ay) - (cx - ax) * (by - ay)).abs() / 2.0
    }

    fn total_area(tris: &[Tri]) -> f64 {
        tris.iter().map(triangle_area).sum()
    }

    #[test]
    fn square_produces_two_triangles_with_full_area() {
        let tris = triangulate_rings(&[square(1.0, 0.0)]);
        assert_eq!(tris.len(), 2);
        assert!((total_area(&tris) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn ring_with_hole_is_filled_excluding_hole() {
        let outer = square(10.0, 0.0);
        let hole = square(4.0, 3.0);
        let tris = triangulate_polygons(std::slice::from_ref(&outer), &[vec![hole]]);
        assert!(!tris.is_empty());
        let area = total_area(&tris);
        assert!((area - (100.0 - 16.0)).abs() < 1e-9, "area = {area}");
    }

    #[test]
    fn orientation_does_not_matter() {
        let ccw = square(2.0, 0.0);
        let mut cw = ccw.clone();
        cw.reverse();
        let a = triangulate_rings(&[ccw]);
        let b = triangulate_rings(&[cw]);
        assert!((total_area(&a) - total_area(&b)).abs() < 1e-12);
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn closed_ring_with_duplicate_endpoint_is_accepted() {
        let mut ring = square(3.0, 0.0);
        ring.push(ring[0]);
        let tris = triangulate_rings(&[ring]);
        assert!((total_area(&tris) - 9.0).abs() < 1e-12);
    }

    #[test]
    fn degenerate_inputs_produce_no_triangles() {
        assert!(triangulate_rings(&[]).is_empty());
        assert!(triangulate_rings(&[vec![]]).is_empty());
        assert!(triangulate_rings(&[vec![[0.0, 0.0]]]).is_empty());
        assert!(triangulate_rings(&[vec![[0.0, 0.0], [1.0, 1.0]]]).is_empty());
        // 三个共线点：面积为 0，可以输出退化三角形，但绝不能超过外接矩形。
        let collinear = triangulate_rings(&[vec![[0.0, 0.0], [1.0, 0.0], [2.0, 0.0]]]);
        assert!(collinear.iter().all(|t| triangle_area(t) < 1e-12));
        // 全部顶点重合。
        let same = vec![[5.0, 5.0]; 4];
        assert!(triangulate_rings(&[same]).is_empty());
    }

    #[test]
    fn non_finite_vertices_are_dropped_not_panicking() {
        let ring = vec![
            [0.0, 0.0],
            [f64::NAN, 0.0],
            [2.0, 0.0],
            [2.0, 2.0],
            [f64::INFINITY, 2.0],
            [0.0, 2.0],
        ];
        let tris = triangulate_rings(std::slice::from_ref(&ring));
        assert!(tris.iter().all(|t| t.iter().all(|v| v.is_finite())));
        assert!((total_area(&tris) - 4.0).abs() < 1e-12);

        let mut all_bad = ring.clone();
        all_bad.retain(|p| p[0].is_finite());
        assert!(total_area(&triangulate_rings(&[all_bad])) > 0.0);
        // 只剩 2 个有限点时返回空。
        assert!(
            triangulate_rings(&[vec![[0.0, 0.0], [1.0, 1.0], [f64::NAN, f64::NAN]]]).is_empty()
        );
    }

    #[test]
    fn holes_shorter_than_outers_fall_back_to_no_hole() {
        let outers = vec![square(1.0, 0.0), square(2.0, 10.0)];
        let holes = vec![vec![square(0.5, 0.25)]];
        let tris = triangulate_polygons(&outers, &holes);
        assert!((total_area(&tris) - (1.0 - 0.25) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn output_order_follows_input_order() {
        let outers = vec![square(1.0, 0.0), square(1.0, 100.0), square(1.0, 200.0)];
        let tris = triangulate_polygons(&outers, &[]);
        let mut blocks = [Vec::new(), Vec::new(), Vec::new()];
        for tri in &tris {
            let band = (tri[1] / 100.0).floor() as usize;
            blocks[band.min(2)].push(*tri);
        }
        assert!(blocks.iter().all(|b| b.len() == 2));
    }

    #[test]
    fn parallel_path_matches_serial() {
        // 构造超过 PARALLEL_MIN_VERTICES 的输入，强制走多线程分支。
        let outers: Vec<Vec<[f64; 2]>> = (0..600)
            .map(|i| {
                let mut ring: Vec<[f64; 2]> = (0..32)
                    .map(|k| {
                        let angle = std::f64::consts::TAU * k as f64 / 32.0;
                        [i as f64 * 10.0 + angle.cos(), angle.sin()]
                    })
                    .collect();
                ring.push(ring[0]);
                ring
            })
            .collect();
        let vertices: usize = outers.iter().map(|r| r.len()).sum();
        assert!(vertices >= PARALLEL_MIN_VERTICES, "vertices = {vertices}");
        assert!(worker_count(outers.len(), vertices) > 1);

        let parallel = triangulate_polygons(&outers, &[]);
        let mut serial = Vec::new();
        for ring in &outers {
            serial.extend(triangulate_one(ring, &[]));
        }
        assert_eq!(parallel.len(), serial.len());
        assert_eq!(parallel, serial);
    }

    #[test]
    fn large_polygon_with_holes_keeps_area() {
        let outer = vec![[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]];
        let holes = vec![
            vec![[10.0, 10.0], [30.0, 10.0], [30.0, 30.0], [10.0, 30.0]],
            vec![[60.0, 60.0], [80.0, 60.0], [80.0, 80.0], [60.0, 80.0]],
        ];
        let tris = triangulate_polygons(std::slice::from_ref(&outer), &[holes]);
        let area = total_area(&tris);
        let expected = 100.0 * 100.0 - 20.0 * 20.0 - 20.0 * 20.0;
        assert!((area - expected).abs() < 1e-6, "area = {area}");
    }

    // ─────────────────────── 自相交环的合法化 ───────────────────────

    /// 蝴蝶结：路径在 (5,5) 处自交叉，两瓣各 25 平方单位。
    fn bowtie() -> Vec<[f64; 2]> {
        vec![[0.0, 0.0], [10.0, 10.0], [10.0, 0.0], [0.0, 10.0]]
    }

    #[test]
    fn self_intersecting_bowtie_is_detected() {
        assert!(!ring_is_simple(&bowtie()));
        assert!(ring_is_simple(&square(10.0, 0.0)));
        // 闭合环（首尾重复）不影响判定。
        let mut closed = square(5.0, 0.0);
        closed.push(closed[0]);
        assert!(ring_is_simple(&closed));
        // 相邻边共享端点是正常的；非相邻边共享端点算自接触。
        let touch = vec![[0.0, 0.0], [10.0, 0.0], [5.0, 0.0], [5.0, 10.0], [0.0, 10.0]];
        assert!(!ring_is_simple(&touch));
    }

    #[test]
    fn bowtie_splits_into_two_simple_parts() {
        let parts = polygon_parts(&bowtie(), &[]);
        assert_eq!(parts.len(), 2, "蝴蝶结应拆成两瓣");
        // 每瓣都是简单环，且总面积 = 两瓣之和（50）。
        let mut total = 0.0;
        for (outer, holes) in &parts {
            assert!(holes.is_empty());
            assert!(ring_is_simple(outer));
            let tris = triangulate_one(outer, holes);
            total += total_area(&tris);
        }
        assert!((total - 50.0).abs() < 1e-9, "total = {total}");
    }

    #[test]
    fn simple_ring_passes_through_unchanged() {
        let outer = square(10.0, 0.0);
        let holes = vec![square(2.0, 4.0)];
        let parts = polygon_parts(&outer, &holes);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].0, outer);
        assert_eq!(parts[0].1, holes);
    }

    #[test]
    fn triangulation_of_bowtie_covers_only_valid_area() {
        // 数字化路径的整体效果：闭合 → 拆分 → 剖分，填充面积只含有效区域。
        let mut closed = bowtie();
        closed.push(closed[0]);
        let parts = polygon_parts(&closed, &[]);
        let (outers, hole_sets): (Vec<_>, Vec<_>) = parts
            .iter()
            .map(|(o, h)| (o.clone(), h.clone()))
            .unzip();
        let tris = triangulate_polygons(&outers, &hole_sets);
        assert!((total_area(&tris) - 50.0).abs() < 1e-9);
    }
}
