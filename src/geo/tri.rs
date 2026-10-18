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
}
