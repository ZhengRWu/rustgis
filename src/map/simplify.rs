//! 描边路径的屏幕空间清理与抽稀。
//!
//! # 为什么需要这一步
//!
//! epaint 对**闭合路径**走 `add_line_loop`，其中 `CUT_OFF_SHARP_CORNERS` 被硬编码为
//! `false`，于是永远使用 miter 连接，顶点外扩量为 `normal / |normal|²`，
//! 其中 `normal = (n0 + n1) / 2`（`n0`/`n1` 为相邻两段的单位法线）。
//!
//! 当路径在某顶点处**折返**（前一段与后一段方向接近相反）时 `n0 + n1 → 0`，
//! 外扩量趋于无穷 —— 渲染出来就是一根横穿视口的长尖刺。
//!
//! 为什么只有缩小时才出现：放大时相邻顶点在屏幕上相距较远，方向稳定；
//! 缩小时大量顶点塌缩到同一像素，方向剧烈抖动乃至精确反向，折返随之出现。
//!
//! # 做法
//!
//! 把世界坐标转到屏幕坐标后依次处理：
//!
//! 1. **去重** —— 丢弃与前一点距离小于 [`MIN_SEGMENT`] 的顶点；
//! 2. **去折返** —— miter 外扩倍率超过 [`MITER_LIMIT`] 的顶点做「削平」：
//!    沿角平分线的垂直方向拆成相距 [`STRADDLE_PX`] 的一对点（等价于 bevel
//!    斜切），转角从接近 180° 降到 ~90°，miter 不再失控，同时**保住折返处
//!    的真实轮廓**——细长多边形（数字化自相交拆出的碎片、地块缝隙）的尖角
//!    不会被删掉。邻边过短削不动时才退回删除（闭合环还会补查接缝处）；
//! 3. **抽稀** —— 按 [`SIMPLIFY_TOLERANCE`] 做 Douglas-Peucker。
//!
//! 第 3 步既是尺度自适应的抗锯齿（用户可见的"缩小后自动平滑边、减少顶点"），
//! 也直接降低了 egui 每帧的描边三角化开销。

use egui::{Pos2, Rect};

use crate::geo::proj::CrsTransform;
use crate::map::{MapView, Projector};

/// 相邻屏幕点距离小于该值（像素）即视为重复顶点。
const MIN_SEGMENT: f32 = 0.1;

/// miter 外扩倍率上限。epaint 的实际外扩倍率为 `1 / |cos(φ/2)|`（φ 为转角），
/// 超过该值即视为折返；`4.0` 约等于"转角超过 151° 视为折返"。
const MITER_LIMIT: f32 = 4.0;

/// 折返顶点「削平」的间距（像素）：拆成的一对点相距 `2 × STRADDLE_PX`。
/// 1px 在任何缩放下都不可辨，却能把折返的转角压到 ~90°。
const STRADDLE_PX: f32 = 1.0;

/// Douglas-Peucker 抽稀容差（屏幕像素）。亚像素级，肉眼不可辨。
const SIMPLIFY_TOLERANCE: f32 = 0.75;

/// 顶点数少于该值时不值得跑 Douglas-Peucker。
const SIMPLIFY_MIN_POINTS: usize = 64;

/// 折返清理的最大轮数。
/// 单轮内线性扫描必然收敛；只有闭合环的接缝清理可能牵动链首/链尾，
/// 故外层再多滚几轮直至稳定。上限仅作病态输入的兜底。
const MAX_CLEAN_ROUNDS: usize = 4;

/// 视口粗筛的默认留白（像素）。线宽 / 抗锯齿羽化都在这几像素之内。
#[allow(dead_code)] // 仅被测试与 [`stroke_path`] 使用；二进制里走 `stroke_path_into` 热路径。
const DEFAULT_CULL_PAD_PX: f32 = 4.0;

/// 世界坐标折线 / 环 → 干净且抽稀后的屏幕路径。
///
/// `closed = true`（多边形外环 / 孔）时返回值**不含**与首点重复的收尾点，
/// 由 `PathShape::closed_line` 负责闭合。路径少于 2 个点时返回空。
///
/// 这是给测试与一次性调用用的简单入口；渲染热路径请用 [`stroke_path_into`]，
/// 它可以复用缓冲并跳过视口外的路径。
#[allow(dead_code)] // 二进制只走 `stroke_path_into`；本入口供测试与诊断对照使用。
pub fn stroke_path(world: &[[f64; 2]], rect: Rect, view: &MapView, closed: bool) -> Vec<Pos2> {
    let proj = Projector::new(rect, view);
    let mut out = Vec::new();
    stroke_path_into(world, &proj, None, closed, DEFAULT_CULL_PAD_PX, &mut out);
    out
}

/// [`stroke_path`] 的热路径版本：结果写入调用方缓冲 `out`（跨帧复用，避免每帧分配），
/// 并先按屏幕包围盒做视口粗筛 —— 整条路径（含 `pad_px` 留白）都落在视口之外时
/// 直接放弃，不再做去重 / 去折返 / 抽稀，也不产生任何形状。
///
/// 这是缩小视图时的绝对多数情况：视口外路径的处理成本降到"一次投影扫描"。
/// 进入处理流程但结果退化（少于 2 点、或闭合环收口）时 `out` 同样为空。
pub fn stroke_path_into(
    world: &[[f64; 2]],
    proj: &Projector,
    xf: Option<&CrsTransform>,
    closed: bool,
    pad_px: f32,
    out: &mut Vec<Pos2>,
) {
    out.clear();

    // 1) 投影（一趟扫描，顺带求出屏幕包围盒，供视口粗筛使用）
    out.reserve(world.len());
    let mut x0 = f32::INFINITY;
    let mut y0 = f32::INFINITY;
    let mut x1 = f32::NEG_INFINITY;
    let mut y1 = f32::NEG_INFINITY;
    for w in world {
        // 先做「源 CRS → 工程 CRS」变换（不重投影时 `xf == None`）；
        // 变换失败（如超出投影有效范围）退回源坐标，保证几何不丢。
        let p = match xf {
            Some(xf) => xf.apply(*w).unwrap_or(*w),
            None => *w,
        };
        let (x, y) = proj.project_xy(p[0], p[1]);
        // 非有限坐标（NaN / inf）无法参与几何运算，直接丢弃。
        if !x.is_finite() || !y.is_finite() {
            continue;
        }
        out.push(Pos2::new(x, y));
        x0 = x0.min(x);
        x1 = x1.max(x);
        y0 = y0.min(y);
        y1 = y1.max(y);
    }
    if out.is_empty() {
        return;
    }

    // 2) 视口粗筛
    let clip = proj.rect().expand(pad_px);
    if x1 < clip.min.x || x0 > clip.max.x || y1 < clip.min.y || y0 > clip.max.y {
        out.clear();
        return;
    }

    dedup(out, closed);
    remove_reversals(out, closed);
    if out.len() >= SIMPLIFY_MIN_POINTS {
        let simplified = douglas_peucker(out, SIMPLIFY_TOLERANCE);
        *out = simplified;
        // Douglas-Peucker 抽掉中间点后，原本平缓的转角会变成急剧折返，
        // 因此抽稀之后必须再清一次 —— 否则真实数据上仍会有环画出尖刺
        // （实测 Natural Earth 全图视图下，仅抽稀不清会残留 13/288 个环）。
        remove_reversals(out, closed);
    }

    // 闭合环的退化收口：
    // - 少于 3 个点根本成不了面，`PathShape::closed_line` 会退化成"来回走一条线"，
    //   两段的法线正好互为相反数，miter 外扩照样爆炸；
    // - 只剩 3 点却仍含折返时已"删无可删"（再删就不成面了），
    //   这种环在屏幕上就是一根亚像素细缝。
    // 实测中国省级数据全图视图下，这一步是最后 4/557 个漏网环的收口。
    if closed && (out.len() < 3 || (out.len() == 3 && has_reversal(out, true))) {
        out.clear();
        return;
    }

    if out.len() < 2 {
        out.clear();
    }
}

/// 丢掉连续重复（含闭合环的首尾重复）顶点。
fn dedup(pts: &mut Vec<Pos2>, closed: bool) {
    if pts.is_empty() {
        return;
    }
    let mut w = 1usize;
    for r in 1..pts.len() {
        if (pts[r] - pts[w - 1]).length() >= MIN_SEGMENT {
            pts[w] = pts[r];
            w += 1;
        }
    }
    pts.truncate(w);

    if closed && pts.len() > 2 && (pts[pts.len() - 1] - pts[0]).length() < MIN_SEGMENT {
        pts.pop();
    }
}

/// 丢掉会让 miter 外扩爆掉的折返顶点（削不动时），其余削平成一对点。
///
/// 单轮的线性扫描必然收敛；只有闭合环的"接缝"清理可能牵动链首/链尾，
/// 因此外层反复滚动直到不再变化（每轮至少少一个顶点，故必然终止）。
fn remove_reversals(pts: &mut Vec<Pos2>, closed: bool) {
    for _ in 0..MAX_CLEAN_ROUNDS {
        let before = pts.len();
        linear_pass(pts);
        if closed {
            seam_pass(pts);
        }
        if pts.len() == before || pts.len() < 3 {
            break;
        }
    }
}

/// 线性扫描：原地用写游标维护输出栈，末尾三点构成折返就削平中间点（可能连锁）。
///
/// 写游标 `w` 恒不大于读游标 `r`，因此不需要额外的输出缓冲 ——
/// 这一步在每个可见路径上都要跑，省掉的是每帧每路径一次 `Vec` 分配。
fn linear_pass(pts: &mut Vec<Pos2>) {
    let mut r = 0usize; // 读游标
    let mut w = 0usize; // 写游标
    while r < pts.len() {
        pts[w] = pts[r];
        w += 1;
        r += 1;
        while w >= 3 {
            if !is_reversal(pts[w - 3], pts[w - 2], pts[w - 1]) {
                break;
            }
            let (a, b, c) = (pts[w - 3], pts[w - 2], pts[w - 1]);
            if let Some((b1, b2)) = straddle_reversal(a, b, c) {
                // 削平：b 拆成 b1、b2 两点。插入点落在已写区内（w-1 ≤ r-1），
                // 未读元素整体右移一格，读游标同步 +1 即可继续对上。
                pts[w - 2] = b1;
                pts.insert(w - 1, b2);
                w += 1;
                r += 1;
            } else {
                // 邻边过短削不动（亚像素级），退回删除：用末尾点覆盖中间点
                // 再回退游标，等价于移除中间点，且是 O(1)。
                pts[w - 2] = pts[w - 1];
                w -= 1;
            }
        }
    }
    pts.truncate(w);
}

/// 折返顶点的削平：把 `b` 沿垂直于角平分线的方向拆成相距 `2 × STRADDLE_PX`
/// 的一对点。折返的转角（接近 180°）由此降为两个 ~90° 的普通转角，
/// miter 外扩回到 [`MITER_LIMIT`] 之内，而折返处的真实轮廓得以保留。
///
/// 邻边短于 `2 × STRADDLE_PX` 时削平会引入新的病态几何，返回 `None`
/// 让调用方退回删除（这种环在屏幕上本就是亚像素级的，删了不可见）。
fn straddle_reversal(a: Pos2, b: Pos2, c: Pos2) -> Option<(Pos2, Pos2)> {
    let u = b - a;
    let v = c - b;
    let (lu, lv) = (u.length(), v.length());
    if lu < 2.0 * STRADDLE_PX || lv < 2.0 * STRADDLE_PX {
        return None;
    }
    // 角平分线方向（入边与出边单位向量之差）；折返处两者接近反向，差向量不会退化。
    let m = u.normalized() - v.normalized();
    let mlen = m.length();
    if mlen < 1e-6 {
        return None;
    }
    let p = m.rot90() / mlen; // 垂直于角平分线
    Some((b + p * STRADDLE_PX, b - p * STRADDLE_PX))
}

/// 闭合环的接缝补查：线性扫描看不到"最后一个顶点的出边"与"首顶点的出入边"，
/// 折返也可能正好落在接缝上。
fn seam_pass(pts: &mut Vec<Pos2>) {
    loop {
        let n = pts.len();
        // 3 点环也要查接缝：细长三角形的尖角折返正好可能落在接缝上
        // （削平后变 4 点；走删除路径时退化到 2 点，由收口逻辑清空）。
        if n < 3 {
            return;
        }
        if is_reversal(pts[n - 2], pts[n - 1], pts[0]) {
            match straddle_reversal(pts[n - 2], pts[n - 1], pts[0]) {
                // 环序 ...a → b1 → b2 → c(=pts[0])：b2 追加到末尾正好落在
                // b1 与首点之间（闭合环的接缝就在末尾与首点之间）。
                Some((b1, b2)) => {
                    pts[n - 1] = b1;
                    pts.push(b2);
                }
                None => {
                    pts.pop();
                }
            }
        } else if is_reversal(pts[n - 1], pts[0], pts[1]) {
            match straddle_reversal(pts[n - 1], pts[0], pts[1]) {
                // 环序 ...a(末尾) → b1 → b2 → c(=pts[1])：b1 插到最前、
                // b2 顶替原首点，接缝两侧的顺序保持不变。
                Some((b1, b2)) => {
                    pts[0] = b2;
                    pts.insert(0, b1);
                }
                None => {
                    pts.remove(0);
                }
            }
        } else {
            return;
        }
    }
}

/// 路径上是否还存在折返顶点（闭合路径含接缝）。
fn has_reversal(pts: &[Pos2], closed: bool) -> bool {
    let n = pts.len();
    if n < 3 {
        return false;
    }
    let last = if closed { n } else { n - 1 };
    (0..last).any(|i| is_reversal(pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]))
}

/// 顶点 `b` 处是否构成折返（转角过尖，miter 外扩会失控）。
fn is_reversal(a: Pos2, b: Pos2, c: Pos2) -> bool {
    let u = b - a;
    let v = c - b;
    let (lu, lv) = (u.length(), v.length());
    if lu < MIN_SEGMENT || lv < MIN_SEGMENT {
        // 退化线段：单位法线不稳定，miter 同样会炸。
        return true;
    }
    let cos_phi = (u.x * v.x + u.y * v.y) / (lu * lv);
    // 外扩倍率 1/|cos(φ/2)| > MITER_LIMIT  ⟺  1 + cosφ < 2 / MITER_LIMIT²
    cos_phi < 2.0 / (MITER_LIMIT * MITER_LIMIT) - 1.0
}

/// Douglas-Peucker 抽稀（迭代实现，避免深递归爆栈）。
fn douglas_peucker(pts: &[Pos2], tol: f32) -> Vec<Pos2> {
    let n = pts.len();
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;

    let mut stack: Vec<(usize, usize)> = vec![(0, n - 1)];
    while let Some((i, j)) = stack.pop() {
        if j <= i + 1 {
            continue;
        }
        let (a, b) = (pts[i], pts[j]);
        let mut far = i;
        let mut far_d = -1.0f32;
        for (k, &p) in pts.iter().enumerate().take(j).skip(i + 1) {
            let d = dist_to_segment(p, a, b);
            if d > far_d {
                far_d = d;
                far = k;
            }
        }
        if far_d > tol {
            keep[far] = true;
            stack.push((i, far));
            stack.push((far, j));
        }
    }

    let mut out = Vec::with_capacity(n);
    for (k, &p) in pts.iter().enumerate() {
        if keep[k] {
            out.push(p);
        }
    }
    out
}

fn dist_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len_sq = ab.length_sq();
    if len_sq <= 0.0 {
        return (p - a).length();
    }
    let ap = p - a;
    let t = ((ap.x * ab.x + ap.y * ab.y) / len_sq).clamp(0.0, 1.0);
    let proj = Pos2::new(a.x + ab.x * t, a.y + ab.y * t);
    (p - proj).length()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::world_to_screen;
    use egui::pos2;

    /// 构造一个"世界坐标即屏幕坐标"的视图：rect 以原点为中心、scale = 1。
    fn identity_view() -> (Rect, MapView) {
        (
            Rect::from_min_max(pos2(-500.0, -500.0), pos2(500.0, 500.0)),
            MapView {
                center: [0.0, 0.0],
                scale: 1.0,
            },
        )
    }

    fn world(pts: &[(f32, f32)]) -> Vec<[f64; 2]> {
        pts.iter().map(|&(x, y)| [x as f64, y as f64]).collect()
    }

    /// 复刻 epaint `add_line_loop` 的 miter 外扩倍率：`|normal| / |normal|²`。
    fn miter_factor(a: Pos2, b: Pos2, c: Pos2) -> f32 {
        let u = (b - a).normalized();
        let v = (c - b).normalized();
        let normal = (u.rot90() + v.rot90()) / 2.0;
        let len_sq = normal.length_sq();
        if len_sq <= 0.0 {
            return f32::INFINITY;
        }
        normal.length() / len_sq
    }

    /// 路径上所有顶点的最大 miter 外扩倍率（闭合路径含接缝）。
    /// 出现非有限结果一律返回 `INFINITY`（`f32::max` 会静默忽略 NaN）。
    fn max_miter(pts: &[Pos2], closed: bool) -> f32 {
        let n = pts.len();
        if n < 3 {
            return 0.0;
        }
        let mut worst = 0.0f32;
        let last = if closed { n } else { n - 1 };
        for i in 0..last {
            let (a, b, c) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]);
            let f = miter_factor(a, b, c);
            if !f.is_finite() {
                return f32::INFINITY;
            }
            worst = worst.max(f);
        }
        worst
    }

    #[test]
    fn dedup_collapses_coincident_points() {
        let (rect, view) = identity_view();
        let ring = world(&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 0.0),
        ]);
        let pts = stroke_path(&ring, rect, &view, true);
        // 重复点被去掉，闭合环的收尾重复点也去掉
        assert_eq!(pts.len(), 3);
    }

    #[test]
    fn hairpin_vertex_is_flattened_not_removed() {
        let (rect, view) = identity_view();
        // A → B → C，其中 C 几乎回到 A 附近：B 是折返顶点
        let line = world(&[(0.0, 0.0), (100.0, 0.0), (0.2, 0.05), (0.2, 40.0)]);
        let pts = stroke_path(&line, rect, &view, false);
        assert!(
            max_miter(&pts, false) <= MITER_LIMIT + 1e-3,
            "折返未被清除，最大 miter 倍率 = {}",
            max_miter(&pts, false)
        );
        // 削平（而非删除）：折返顶点拆成一对点，折返处的路径形状保留。
        assert!(pts.len() >= 4, "削平后应保留折返处的一对点，实得 {pts:?}");
    }

    #[test]
    fn clean_paths_are_left_alone() {
        let (rect, view) = identity_view();
        // 一个规整的矩形环：清理后应保持原样
        let ring = world(&[
            (0.0, 0.0),
            (80.0, 0.0),
            (80.0, 60.0),
            (0.0, 60.0),
            (0.0, 0.0),
        ]);
        let pts = stroke_path(&ring, rect, &view, true);
        assert_eq!(pts.len(), 4);
        assert!(max_miter(&pts, true) < 2.0);
    }

    /// 回归用例（对应截图里的尖刺）：缩小时顶点塌缩到同一像素导致的折返，
    /// 清理后必须不再产生失控的 miter 外扩。
    #[test]
    fn zoomed_out_ring_has_no_runaway_miter() {
        let (rect, view) = identity_view();
        let mut ring: Vec<(f32, f32)> = Vec::new();
        // 细密圆周（模拟真实边界）……
        let n = 400;
        for i in 0..n {
            let a = std::f32::consts::TAU * i as f32 / n as f32;
            ring.push((a.cos() * 200.0, a.sin() * 200.0));
        }
        // ……中间插入一个"缩小时塌缩成同像素"的极小折返（长度 0.05px < MIN_SEGMENT）
        ring.insert(120, (ring[119].0 + 0.02, ring[119].1 + 0.02));
        ring.insert(121, (ring[118].0 - 0.02, ring[118].1 - 0.02));

        // 先确认"未清理"的原始路径确实会炸（这就是 bug 的成因）
        let raw: Vec<Pos2> = ring
            .iter()
            .map(|&(x, y)| world_to_screen([x as f64, y as f64], rect, &view))
            .collect();
        let raw_worst = max_miter(&raw, true);
        assert!(
            raw_worst > MITER_LIMIT,
            "用例本身未复现尖刺，raw miter = {raw_worst}"
        );

        let pts = stroke_path(&world(&ring), rect, &view, true);
        let fixed = max_miter(&pts, true);
        assert!(
            fixed <= MITER_LIMIT + 1e-3,
            "清理后仍有失控 miter：{fixed}（raw={raw_worst}）"
        );
    }

    #[test]
    fn dense_ring_is_simplified_when_small_on_screen() {
        // 4000 个顶点的环，但只占 100×100 像素
        let mut ring: Vec<(f32, f32)> = Vec::new();
        for i in 0..4000 {
            let a = std::f32::consts::TAU * i as f32 / 4000.0;
            ring.push((a.cos() * 50.0, a.sin() * 50.0));
        }
        let (rect, view) = identity_view();
        let pts = stroke_path(&world(&ring), rect, &view, true);
        assert!(pts.len() < 400, "抽稀不足：4000 个顶点只降到 {}", pts.len());
        assert!(pts.len() >= 8, "抽稀过度：只剩 {} 个顶点", pts.len());
    }

    #[test]
    fn degenerate_sliver_ring_is_flattened_safely() {
        let (rect, view) = identity_view();
        // 3 个点、其中路径"去而复返"（B 之后折回 A 与 B 之间）：折返顶点被
        // 削平成一对点后保留描边——与 QGIS 一致，细长多边形不丢轮廓；
        // 但绝不允许 miter 失控。
        let sliver = world(&[(0.0, 0.0), (200.0, 0.1), (100.0, 0.05)]);
        let pts = stroke_path(&sliver, rect, &view, true);
        assert!(
            !pts.is_empty() && max_miter(&pts, true) <= MITER_LIMIT + 1e-3,
            "细缝环描边丢失或 miter 失控：{pts:?}"
        );
        // 同为 3 点的正常三角形必须保留
        let triangle = world(&[(0.0, 0.0), (100.0, 0.0), (50.0, 80.0)]);
        assert_eq!(stroke_path(&triangle, rect, &view, true).len(), 3);
    }

    /// 回归用例：细长三角形（数字化自相交拆出的碎片）的尖角折返必须被削平
    /// 而不是连轮廓一起丢掉——否则填充在、描边没了（截图 bug）。
    #[test]
    fn thin_sliver_triangle_keeps_its_outline() {
        let (rect, view) = identity_view();
        // 顶角 ~7° 的细长三角：底边两个顶点相距 12px，尖点在 100px 之外。
        let ring = world(&[(0.0, 0.0), (12.0, 0.0), (100.0, 100.0)]);
        let pts = stroke_path(&ring, rect, &view, true);
        assert!(
            !pts.is_empty(),
            "细长三角形的描边不应被整体丢弃"
        );
        assert!(
            max_miter(&pts, true) <= MITER_LIMIT + 1e-3,
            "削平后 miter 仍失控：{}",
            max_miter(&pts, true)
        );
    }

    /// 视口粗筛：完全在视口外的路径必须被整条丢弃（这正是缩小视图时
    /// 绝大多数路径的归宿），而跨越视口边缘、或贴边但线宽留白能盖到的路径必须保留。
    #[test]
    fn paths_outside_view_are_culled() {
        let (rect, view) = identity_view();
        let proj = Projector::new(rect, &view);
        let mut out = Vec::new();

        let far = world(&[(900.0, 0.0), (950.0, 10.0), (990.0, -10.0)]);
        stroke_path_into(&far, &proj, None, false, DEFAULT_CULL_PAD_PX, &mut out);
        assert!(out.is_empty(), "视口外的路径未被剔除");

        // 跨越视口边缘：只按包围盒判定，保留后再交给后面的清理流程
        let crossing = world(&[(0.0, 0.0), (900.0, 0.0)]);
        stroke_path_into(&crossing, &proj, None, false, DEFAULT_CULL_PAD_PX, &mut out);
        assert_eq!(out.len(), 2);

        // 贴在右边界外 3px：仍在 4px 留白内，必须保留（否则贴着视口边的线会闪断）
        let touching = world(&[(503.0, 0.0), (520.0, 0.0)]);
        stroke_path_into(&touching, &proj, None, false, DEFAULT_CULL_PAD_PX, &mut out);
        assert_eq!(out.len(), 2);

        // 与 `stroke_path` 简单入口的结论必须一致
        assert!(stroke_path(&far, rect, &view, false).is_empty());
    }

    #[test]
    fn degenerate_inputs_do_not_panic() {
        let (rect, view) = identity_view();
        assert!(stroke_path(&[], rect, &view, true).is_empty());
        assert!(stroke_path(&world(&[(1.0, 1.0)]), rect, &view, false).is_empty());
        // 所有点重合
        let same = world(&[(5.0, 5.0); 8]);
        assert!(stroke_path(&same, rect, &view, true).is_empty());
        // 非有限坐标
        let bad = vec![[f64::NAN, 0.0], [1.0, f64::INFINITY], [2.0, 2.0]];
        assert!(stroke_path(&bad, rect, &view, false).is_empty());
    }

    /// 真实数据诊断（手动运行）：
    /// `RUSTGIS_DIAG_SHP=<path> cargo test --release --bin rustgis -- --ignored --nocapture diag_`
    ///
    /// 用「导入后自动缩放至全图」的那个视图（`MapView::fit`）去量化：
    /// 有多少环会因为顶点塌缩而触发失控 miter，以及清理后是否归零。
    #[test]
    #[ignore = "manual diagnostic on a real file"]
    fn diag_real_file_spikes() {
        let Ok(path) = std::env::var("RUSTGIS_DIAG_SHP") else {
            println!("未设置 RUSTGIS_DIAG_SHP，跳过");
            return;
        };
        let layer = crate::geo::load_file(std::path::Path::new(&path)).expect("加载失败");
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        let view = MapView::fit(Some(layer.bounds), rect);
        println!(
            "要素 {} · 外环 {} · 顶点合计 {} · 每度像素 scale={:.5}",
            layer.feature_count,
            layer.outer_rings.len(),
            layer.outer_rings.iter().map(|r| r.len()).sum::<usize>(),
            view.scale
        );

        let mut raw_bad = 0usize;
        let mut fixed_bad = 0usize;
        let mut raw_pts = 0usize;
        let mut fixed_pts = 0usize;
        let mut details: Vec<(usize, usize, f32)> = Vec::new();
        for ring in &layer.outer_rings {
            let raw: Vec<Pos2> = ring
                .iter()
                .map(|w| world_to_screen(*w, rect, &view))
                .collect();
            raw_pts += raw.len();
            if max_miter(&raw, true) > MITER_LIMIT {
                raw_bad += 1;
            }
            let fixed = stroke_path(ring, rect, &view, true);
            fixed_pts += fixed.len();
            let m = max_miter(&fixed, true);
            if m > MITER_LIMIT {
                fixed_bad += 1;
                details.push((ring.len(), fixed.len(), m));
            }
        }
        println!("触发失控 miter 的环：清理前 {raw_bad} / 清理后 {fixed_bad}");
        println!("描边顶点数：清理前 {raw_pts} / 清理后 {fixed_pts}");
        for d in details.iter().take(12) {
            println!(
                "  残留：原始 {} 顶点 → 清理后 {} 顶点，max miter = {:.1}",
                d.0, d.1, d.2
            );
        }
        assert_eq!(fixed_bad, 0, "清理后仍有环会画出尖刺");
        assert!(raw_bad > 0, "该文件未复现问题，换一份数据试试");
    }
}
