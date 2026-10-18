//! 栅格绘制：把视图拆成固定尺寸的贴图块，逐块取金字塔层级 → 解码 → CPU 合成 → 贴图。
//!
//! 分块渲染（与 QGIS 的 `QgsRasterIterator` + `QgsRasterDrawer` 同思路）：
//! 视口按 [`TILE_SIDE`] 切成网格，每块各自记录「覆盖的像素窗口」，屏上位置由同一个
//! 投影变换算出。好处是①没有任何「整幅纹理」尺寸上限，也就不会因为夹取上限把画面
//! 拉变形；②平移/缩放只需要补缺的块，旧块继续按世界位置贴住，延迟从「整窗重解」
//! 降到「单块解码」；③块到一块画一块，天然是渐进式显示。
//!
//! 合成在 CPU 上做（与 QGIS 的 `QgsRasterDrawer` + QPainter 一致）：块内逐像素走
//! 「最近邻映射 + 拉伸 / 反相 / Gamma + NoData」一次性遍历，产出 RGBA 后交给 egui
//! 上传成纹理；放大由 GPU 的双线性过滤完成，缩写在解码侧选层时已经重采样。
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use egui::{Color32, ColorImage, Rect, TextureHandle, TextureOptions, pos2};

use super::{
    BandStatistics, GeoReference, PixelWindow, RasterDataset, RasterDisplay, RasterLayer,
    SampleType, StretchType,
};
use crate::geo::Aabb;
use crate::map::{MapView, Projector};

/// 每块细节贴图覆盖的屏幕像素边长。
///
/// 块小则首屏补齐快（单块解码 ≈ 100 ms），块大则解码次数少。512 时 1450×740 的视口
/// 只需 3×2 块，首块约 0.15 秒就能盖住中间区域。
const TILE_SIDE: u32 = 512;
/// 同时在解码的块数上限。
///
/// 每块解码线程**自己开一份数据集**（实测 SAFE 打开约 10–12 ms，远小于单块 0.1–0.6 s
/// 的解码），所以这里是真的并行；上限按 CPU 核心数收敛，避免把磁盘队列塞满反而更慢。
const MAX_TILE_JOBS: usize = 4;
/// 每个图层最少缓存的块数（含暂时移出屏幕的历史块）。
const MAX_CACHED_TILES: usize = 24;
/// 在「当前视口需要的块数」之外额外保留的历史块数量。
///
/// 缓存预算必须**以视口需求为下限**，不能是固定值：一块贴图固定覆盖 512 个**物理**
/// 像素，而视口需要的块数 = 视口物理尺寸 / 512 再考虑网格错位，在 125%/150%/200%
/// 缩放的多分辨率屏幕上会翻倍。固定 24 块时，1:86993 的 10 m S2 影像在 150% 缩放的
/// 1920×1080 窗口上视口正好需要 28 块，超出的块永远派不出去——屏幕上就是一大片空白
/// （没有金字塔时连兜底底图都建不出来）。
const TILE_CACHE_HEADROOM: usize = 8;
/// 放大方向的复用阈值：缓存网格比需要的粗25% 以上就重铺网格。
const REUSE_DENSITY_TOLERANCE: f32 = 0.25;
/// 缩小方向的复用阈值：缓存网格比需要的细1.75 倍以内继续用。
///
/// 缩小时旧块只是「过采样」，画面依旧清楚（GPU 双线性下采样），没必要每格滚轮都
/// 重解码一次；放大超过阈值才重铺——与 QGIS 概览层级的容差策略一致。
const REUSE_OVER_DENSITY: f32 = 1.75;
/// 旧块最多允许被缩小多少倍仍继续绘制：再多就混叠（横条/ 摩尔纹），改画底图。
/// 放大方向不设上限——旧块总比 768 px 底图清晰，且双线性插值是平滑的；
/// 这一条对应QGIS 的`maxOversampling` / overview 层级选择语义。
const DRAW_MAX_DECIMATE: f32 = 2.0;
/// 纹理采样方式的门槛：密度（源像素 / 屏幕像素）小于它就用最近邻。
///
/// 只要还在**放大**（1 个源像素铺到多于 1 个屏幕像素），QGIS 等桌面 GIS 一律用最近邻：
/// 双线性在放大时只会把细节抹平，用户看到的就是「糊」。缩小方向才用双线性（避免锯齿、
/// 摩尔纹），这时像素是被合并的，平滑是想要的。
const NEAREST_BELOW_DENSITY: f32 = 1.0;
/// 超过这个像素数就改走后台线程解码：同步解码要 0.2 秒以上，会直接掉帧。
const ASYNC_DECODE_PIXELS: u64 = 4_000_000;
/// 底图长边（像素）：细节块就绪前先铺一张全图缩略，避免一进来是空白。
const BASE_CANVAS_SIDE: u32 = 768;

/// 绘制线程需要的元数据快照。
///
/// 直接读数据集要抢互斥锁，而后台统计/ 解码线程可能持有它数秒；这里在锁空闲时
/// 拷一份（含统计数据），绘制期间只读快照，界面线程因此永不等锁。
struct DatasetSnapshot {
    width: u32,
    height: u32,
    georeference: GeoReference,
    nodata: Option<f64>,
    stats: Vec<Option<BandStatistics>>,
    sample_types: Vec<SampleType>,
    /// 统计数据版本：统计值更新后需要按新量程重建纹理。
    version: u64,
}

impl DatasetSnapshot {
    fn capture(dataset: &RasterDataset) -> Self {
        let bands = dataset.band_count();
        Self {
            width: dataset.width(),
            height: dataset.height(),
            georeference: dataset.georeference().clone(),
            nodata: dataset.metadata().nodata,
            stats: (0..bands)
                .map(|band| dataset.statistics(band).cloned())
                .collect(),
            sample_types: (0..bands)
                .map(|band| {
                    dataset
                        .band(band)
                        .map_or(SampleType::U8, |info| info.sample_type)
                })
                .collect(),
            version: dataset.statistics_version(),
        }
    }

    fn band_count(&self) -> usize {
        self.sample_types.len()
    }

    fn statistics(&self, band: usize) -> Option<&BandStatistics> {
        self.stats.get(band).and_then(Option::as_ref)
    }

    fn sample_type(&self, band: usize) -> SampleType {
        self.sample_types
            .get(band)
            .copied()
            .unwrap_or(SampleType::U8)
    }

    /// 与数据集当前状态相比是否已经过期（统计值、尺寸变化）。
    fn is_stale(&self, dataset: &RasterDataset) -> bool {
        self.version != dataset.statistics_version()
            || self.width != dataset.width()
            || self.height != dataset.height()
    }
}

/// 一张已上传的贴图句柄。
///
/// 同尺寸重建时沿用同一句柄原地 `set`（零分配），尺寸变化才换新句柄——原地改尺寸会
/// 触发 egui 图集重分配，是花屏的典型来源。
#[derive(Default)]
struct TexPayload {
    handle: Option<TextureHandle>,
}

/// 一块细节贴图及其覆盖范围。
struct CachedTile {
    /// 这一块覆盖的像素窗口（解码时被裁到影像边界内）。
    window: [u32; 4],
    /// 构建时「原始像素 / 屏幕像素」的密度。
    density: f32,
    /// 构建时统计数据版本：统计更新后量程变了，块需要重建。
    version: u64,
    tex: TexPayload,
    /// 最近一次被用到的帧号：淘汰坑位时按它挑最久没用的。
    frame: u64,
}

/// 块在网格里的坐标。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct TileKey {
    /// 网格代号：缩放级别变了就换新代号，旧块与新块互不覆盖。
    level: u32,
    col: i32,
    row: i32,
}

/// 细节块网格：各块按 [`TileGrid::step`] 个源像素铺开。
#[derive(Clone, Copy)]
struct TileGrid {
    level: u32,
    /// 网格密度（源像素 / 屏幕像素），决定每块覆盖多少源像素。
    density: f32,
    /// 每块覆盖的源像素边长。
    step: u32,
}

/// 全图缩略底图：细节块就绪前先画它，块缺失的位置也靠它兜底。
struct BaseTexture {
    display: RasterDisplay,
    version: u64,
    tex: TexPayload,
}

/// 后台解码任务。
struct BuildJob {
    key: TileKey,
    /// 任务种类：线程失败时要知道该记在底图上还是细节块上。
    kind: BuildKind,
    rx: Receiver<BuiltTexture>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BuildKind {
    /// 全图缩略底图。
    Base,
    /// 视图中的一块。
    Detail,
}

/// 后台线程的产出：已经合成好的 RGBA 图。
struct BuiltTexture {
    texture: ColorImage,
    key: TileKey,
    window: [u32; 4],
    size: [u32; 2],
    display: RasterDisplay,
    density: f32,
    version: u64,
    kind: BuildKind,
}

/// 单个图层的绘制状态。
#[derive(Default)]
struct LayerState {
    meta: Option<Arc<DatasetSnapshot>>,
    base: Option<BaseTexture>,
    /// 细节块（可能同时存在新旧两个网格的块：旧的先画、新的盖上）。
    tiles: HashMap<TileKey, CachedTile>,
    /// 当前网格；`None` 表示还没铺过。
    grid: Option<TileGrid>,
    /// 正在解码的块。
    pending: HashSet<TileKey>,
    /// 已经确定建不出来的块（解码报错）。不记下来就会每帧重试：4 个线程被同一批
    /// 注定失败的块占满，底图永远抢不到坑位，界面还会持续空转。
    failed: HashSet<TileKey>,
    /// 底图最后一次失败时的显示设置；相同条件下不再重试。
    failed_base: Option<RasterDisplay>,
    jobs: Vec<BuildJob>,
    /// 帧号，用于淘汰块。
    frame: u64,
    /// 细节块对应的显示设置与统计版本：变了就整批作废。
    tiles_display: Option<RasterDisplay>,
    tiles_version: u64,
}

/// 栅格纹理缓存与绘制器。
#[derive(Default)]
pub struct RasterPainter {
    states: HashMap<u64, LayerState>,
    /// 上一次绘制的进度：`Some(0.0..=1.0)` 表示还有块在解码或没到齐，`None` 表示
    /// 全部画完。状态栏据此决定是否显示渲染中的蓝色圆环。
    ///
    /// 状态栏在同一个 `update()` 里比地图面板先画，所以这里读到的是上一帧的值：
    /// 后台有任务时绘制器本来就会每 30 ms 请求重绘，差一帧看不出来。
    progress: Option<f32>,
}

impl RasterPainter {
    /// 上一次绘制的进度（`None` = 已画完）。
    pub fn progress(&self) -> Option<f32> {
        self.progress
    }

    /// 绘制所有可见栅格图层（自底向上）。
    pub fn paint(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        rect: Rect,
        view: &MapView,
        layers: &[&RasterLayer],
    ) {
        if layers.is_empty() {
            self.states.clear();
            self.progress = None;
            return;
        }
        // 本帧的渲染进度：`total_cells` 是视口需要的块数、`done_cells` 是已经贴上的
        // 块数；`unfinished` 只要为真就一直把圆环挂在状态栏上。
        let mut total_cells = 0usize;
        let mut done_cells = 0usize;
        let mut unfinished = false;
        let projector = Projector::new(rect, view);
        let visible = projector.visible_world_padded(2.0);
        for layer in layers {
            if !layer.visible {
                continue;
            }
            if layer.bounds.is_empty() || !layer.bounds.intersects(&visible) {
                continue;
            }
            let state = self.states.entry(layer.id).or_default();
            // 帧号只在绘制时推进：块的 `frame` 因此等价于「最近一次可见的帧」，
            // 淘汰时只动本帧没被用到的块（以前这里从不自增，LRU 实际是随机的）。
            state.frame = state.frame.wrapping_add(1);
            collect_jobs(ctx, state, layer);
            refresh_snapshot(state, layer);
            let Some(meta) = state.meta.clone() else {
                // 数据集锁被后台任务（统计 / 建金字塔）占着，这一刻连元数据都取不到：
                // 算「还在渲染」，状态栏的圆环保持转着。
                unfinished = true;
                continue;
            };
            let Some(window) = visible_window(&meta, layer.bounds.intersect(&visible)) else {
                continue;
            };
            let Some(needed_screen) = pixel_window_screen_rect(&meta, &window, &projector) else {
                continue;
            };
            // 密度按**物理像素**算：纹理像素最终要落在物理屏幕上，界面里的一切长度
            // （`rect`、投影后的屏幕矩形）都是 egui 的「点」。Windows 缩放 125% / 150%
            // 时 `pixels_per_point > 1`，若仍按点算密度，每块纹理都会被拉伸 1.25–2 倍，
            // 整幅图在任何缩放级别都发虚。
            let ppp = f64::from(ctx.pixels_per_point()).max(1.0);
            let density =
                window.width as f32 / (needed_screen.width() as f64 * ppp).max(0.5) as f32;
            let large = needs_async(&meta);

            // 显示设置或统计数据版本变了：所有细节块作废（底图自己判 version）。
            if state.tiles_display.as_ref() != Some(&layer.display)
                || state.tiles_version != meta.version
            {
                clear_tiles(state);
                state.tiles_display = Some(layer.display.clone());
                state.tiles_version = meta.version;
            }

            // 换网格：缩放级别变了就铺新网格。旧块不删——它们继续按世界位置显示，
            // 新块到齐后自然盖在上面。
            let grid = match state.grid {
                Some(grid) if same_zoom(grid.density, density) => grid,
                Some(grid) => {
                    let grid = TileGrid {
                        level: grid.level.wrapping_add(1),
                        density,
                        step: tile_step(density),
                    };
                    state.grid = Some(grid);
                    grid
                }
                None => {
                    let grid = TileGrid {
                        level: 1,
                        density,
                        step: tile_step(density),
                    };
                    state.grid = Some(grid);
                    grid
                }
            };

            let base_missing = state
                .base
                .as_ref()
                .is_none_or(|base| base.display != layer.display || base.version != meta.version);
            if base_missing {
                state.base = None;
            }
            // 底图已经试过并失败：不再每帧重试（重试只会挤掉细节块的名额）。
            let base_attempt = base_missing && state.failed_base.as_ref() != Some(&layer.display);

            // 缓存预算：必须装得下当前视口需要的块，再留一点历史余量。
            // 固定 24 块在多分辨率屏幕/大窗口下装不下视口，多出来的块会永远派不出去。
            let tile_cap = tile_capacity(window, grid.step);

            // 派活：先算缺哪些块，从视口中心往外派（用户先看到中间）。
            let mut missing = Vec::new();
            for row in tile_range(window.y, window.height, grid.step) {
                for col in tile_range(window.x, window.width, grid.step) {
                    let key = TileKey {
                        level: grid.level,
                        col,
                        row,
                    };
                    if let Some(tile) = state.tiles.get_mut(&key) {
                        tile.frame = state.frame;
                        continue;
                    }
                    if state.pending.contains(&key) || state.failed.contains(&key) {
                        continue;
                    }
                    let Some(tile_window) =
                        tile_window(meta.width, meta.height, grid.step, col, row)
                    else {
                        continue;
                    };
                    missing.push((tile_distance(&tile_window, &window), key, tile_window));
                }
            }
            missing.sort_by(|a, b| a.0.total_cmp(&b.0));

            // 缓存满了先腾地方：先丢本帧没画到的块，新网格铺满之后才允许丢掉旧网格
            // 的兜底块（最久没用的优先）。少了这一步，块数一到上限就会让后面所有块
            // **永远派不出去**，屏幕停在几张旧的粗块上——用户看到的「有的地方清楚、
            // 有的地方一直是糊的」斑块就是这样来的。
            //
            // 反过来，若在旧网格还当着兜底的时候就把它们丢掉，缩放过程中会先闪出一片
            // 空白再被新块填上。
            let replace_old = missing.is_empty();
            make_room(state, grid.level, tile_cap, replace_old);

            // 底图优先：它是一整幅兜底，一旦被细节块占满线程就再也建不出来，
            // 放大时缺块的位置只剩白底（用户报的「大面积空白」）。
            if base_attempt && state.jobs.len() < MAX_TILE_JOBS {
                state.jobs.push(spawn_build(
                    layer,
                    Arc::clone(&meta),
                    BuildRequest {
                        key: TileKey {
                            level: 0,
                            col: 0,
                            row: 0,
                        },
                        window: full_window(&meta),
                        size: base_size(&meta),
                        density: base_density(&meta),
                        kind: BuildKind::Base,
                    },
                ));
            }

            // 派活的名额只跟**当前网格**的块数有关：旧网格的兜底块再多也不能挡住新
            // 网格的块，否则永远补不齐。
            let cells_total = tiles_needed(window, grid.step);
            let cells_missing = missing.len();
            let mut stored_sync = 0usize;
            let grid_tiles = state
                .tiles
                .keys()
                .filter(|key| key.level == grid.level)
                .count();
            for (dispatched, (_, key, tile_window)) in missing.into_iter().enumerate() {
                if grid_tiles + state.pending.len() + dispatched >= tile_cap {
                    break;
                }
                if large && state.jobs.len() >= MAX_TILE_JOBS {
                    break;
                }
                let size = tile_size(&tile_window, grid.step);
                if large {
                    state.pending.insert(key);
                    state.jobs.push(spawn_build(
                        layer,
                        Arc::clone(&meta),
                        BuildRequest {
                            key,
                            window: tile_window,
                            size,
                            density: grid.density,
                            kind: BuildKind::Detail,
                        },
                    ));
                } else if let Some(texture) = build_sync(layer, &meta, &tile_window, size) {
                    store_tile(
                        ctx,
                        layer,
                        state,
                        BuiltTexture {
                            texture,
                            key,
                            window: [
                                tile_window.x,
                                tile_window.y,
                                tile_window.width,
                                tile_window.height,
                            ],
                            size,
                            display: layer.display.clone(),
                            density: grid.density,
                            version: meta.version,
                            kind: BuildKind::Detail,
                        },
                    );
                    stored_sync += 1;
                }
            }
            // 进度只统计「视口需要的块」：`cells_missing` 里包含了已经派给后台、正在
            // 解码的块，所以它天然就是「还没贴上的块数」。
            let cells_done = cells_total
                .saturating_sub(cells_missing)
                .saturating_add(stored_sync)
                .min(cells_total);
            total_cells += cells_total;
            done_cells += cells_done;
            if cells_done < cells_total || !state.jobs.is_empty() {
                unfinished = true;
            }
            if !state.jobs.is_empty() {
                // 后台线程出结果时没有窗口事件，主动唤醒界面来取。
                ctx.request_repaint_after(Duration::from_millis(30));
            }

            // 先画底图，再叠细节块：旧网格的块在下、当前网格的块在上。
            if let Some(base) = &state.base
                && let Some(screen) =
                    pixel_window_screen_rect(&meta, &full_window(&meta), &projector)
            {
                Self::draw_texture(painter, &base.tex, screen);
            }
            // 细节块：只要「显示设置/ 统计版本」没变、且没有被缩得太小，就继续按世界
            // 位置绘制——哪怕它属于上一个缩放网格。
            //
            // 这是 QGIS 选层级（overview level）的思路：屏幕上永远显示「当前可用的
            // 最清晰那些块」，放大时旧块直接拉伸（GPU 双线性过滤），新网格的块解码好
            // 后逐块无缝替换；绝不会因为差一个缩放级别就整屏退回 768 px 底图（用户报
            // 的「放大变成一张糊掉的整幅图」）。只有缩小太多（1 个纹理像素要摊到 2 个
            // 以上屏幕像素）时才不画，避免混叠出横条——那种情况交给新网格重建。
            let mut drawn: Vec<(bool, &CachedTile, Rect)> = state
                .tiles
                .iter()
                .filter(|(_, tile)| tile.version == meta.version && drawable(tile.density, density))
                .filter_map(|(key, tile)| {
                    let tile_window = PixelWindow::new(
                        tile.window[0],
                        tile.window[1],
                        tile.window[2],
                        tile.window[3],
                    );
                    let screen = pixel_window_screen_rect(&meta, &tile_window, &projector)?;
                    Some((key.level == grid.level, tile, screen))
                })
                .collect();
            // 当前网格的块最后画（盖在上面）。
            //
            // 以前按 density 升序排，扩大比例尺时**上一个网格（更糊）的块密度更大**，
            // 于是它们被画在最上层：新解出来的清晰块明明已经在下面，屏幕却一直是糊的，
            // 必须平移画面（改变帧号让旧块被淘汰）才恢复——正是用户反复报的
            // 「放大后不渲染当前层级，移动画面才正常」。
            drawn.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.density.total_cmp(&b.1.density)));
            for (_, tile, screen) in drawn {
                Self::draw_texture(painter, &tile.tex, screen);
            }

            evict_tiles(state, tile_cap, replace_old, grid.level);
        }
        // 全部块都贴上了才收起圆环：`unfinished` 由上面三处置位（缺元数据、缺块、
        // 还有解码任务在飞）。
        self.progress = if unfinished {
            let fraction = if total_cells == 0 {
                0.0
            } else {
                done_cells as f32 / total_cells as f32
            };
            Some(fraction.clamp(0.0, 1.0))
        } else {
            None
        };
        // 清理已被删除图层的纹理（句柄随状态一起释放）。
        let removed: Vec<u64> = self
            .states
            .keys()
            .copied()
            .filter(|id| !layers.iter().any(|layer| layer.id == *id))
            .collect();
        for id in removed {
            self.states.remove(&id);
        }
    }

    /// 贴一张已经合成好的纹理。
    fn draw_texture(painter: &egui::Painter, tex: &TexPayload, screen: Rect) {
        let Some(handle) = &tex.handle else {
            return;
        };
        painter.image(handle.id(), screen, full_uv(), Color32::WHITE);
    }

    /// 测试用：当前缓存的细节块总数。
    #[cfg(test)]
    pub(crate) fn cached_textures(&self) -> usize {
        self.states.values().map(|state| state.tiles.len()).sum()
    }

    /// 测试用：指定图层缓存的所有细节块窗口。
    #[cfg(test)]
    pub(crate) fn cached_windows(&self, layer: u64) -> Vec<[u32; 4]> {
        let Some(state) = self.states.get(&layer) else {
            return Vec::new();
        };
        let mut windows: Vec<[u32; 4]> = state.tiles.values().map(|tile| tile.window).collect();
        windows.sort_unstable();
        windows
    }

    /// 测试用：指定图层是否已有全图底图。
    #[cfg(test)]
    pub(crate) fn has_base_canvas(&self, layer: u64) -> bool {
        self.states
            .get(&layer)
            .is_some_and(|state| state.base.is_some())
    }
}

/// 取回后台线程的产出并转成贴图。
fn collect_jobs(ctx: &egui::Context, state: &mut LayerState, layer: &RasterLayer) {
    if state.jobs.is_empty() {
        return;
    }
    let mut waiting = Vec::new();
    let mut finished = Vec::new();
    for job in std::mem::take(&mut state.jobs) {
        match job.rx.try_recv() {
            Ok(built) => finished.push(built),
            Err(TryRecvError::Empty) => waiting.push(job),
            Err(TryRecvError::Disconnected) => {
                // 解码线程失败（读窗口报错 / 内存上限 / 解码得到的数据不足）。
                // 记下来别再派同一块，否则每帧重试会占满全部线程、底图永远抢不到坑位。
                state.pending.remove(&job.key);
                match job.kind {
                    BuildKind::Base => state.failed_base = Some(layer.display.clone()),
                    BuildKind::Detail => {
                        state.failed.insert(job.key);
                    }
                }
            }
        }
    }
    state.jobs = waiting;
    for built in finished {
        // 期间用户可能改了显示设置：按旧设置算出来的图直接丢掉，下一帧会重新请求。
        if built.display != layer.display {
            state.pending.remove(&built.key);
            continue;
        }
        store_tile(ctx, layer, state, built);
    }
}

/// 把构建结果写进缓存：底图占一个槽位，细节块各自占一个槽位。
fn store_tile(
    ctx: &egui::Context,
    layer: &RasterLayer,
    state: &mut LayerState,
    built: BuiltTexture,
) {
    let BuiltTexture {
        texture,
        key,
        window,
        size,
        display,
        density,
        version,
        kind,
    } = built;
    if kind == BuildKind::Base {
        let previous = state.base.take();
        let name = format!("raster-base-{}", layer.id);
        let tex = install_texture(
            ctx,
            name,
            previous.map(|base| base.tex),
            texture,
            TextureOptions::LINEAR,
        );
        state.base = Some(BaseTexture {
            display,
            version,
            tex,
        });
        return;
    }
    let name = format!(
        "raster-{}-l{}-c{}-r{}-{}x{}",
        layer.id, key.level, key.col, key.row, size[0], size[1]
    );
    // 放大方向（1 个源像素铺到 >1 个屏幕像素）一律最近邻：双线性此时只会把细节抹平，
    // 屏幕上任何一处放大都是「糊」的观感——这正是用户反复报的「放大就模糊」。缩小方向
    // 用双线性，避免锯齿与摩尔纹（与 QGIS / ArcGIS 的默认重采样一致）。
    let options = if density.max(f32::EPSILON) < NEAREST_BELOW_DENSITY {
        TextureOptions::NEAREST
    } else {
        TextureOptions::LINEAR
    };
    let tex = install_texture(ctx, name, None, texture, options);
    state.pending.remove(&key);
    // 同一块被重新解码替换：旧句柄随 `insert` 的返回值一起释放。
    state.tiles.insert(
        key,
        CachedTile {
            window,
            density,
            version,
            tex,
            frame: state.frame,
        },
    );
}

/// 丢弃一个图层的全部细节块（句柄随之释放）。
fn clear_tiles(state: &mut LayerState) {
    state.tiles.clear();
    state.pending.clear();
    state.failed.clear();
    state.failed_base = None;
    state.grid = None;
}

/// 派活前腾地方：先丢本帧没画到的块（任何网格），`replace_old` 为真时再丢旧网格的
/// 兜底块，最久未用的优先，直到低于 `cap`。本帧可见的当前网格块绝不淘汰。
fn make_room(state: &mut LayerState, level: u32, cap: usize, replace_old: bool) {
    if state.tiles.len() + state.pending.len() < cap {
        return;
    }
    let mut stale: Vec<(u8, u64, TileKey)> = state
        .tiles
        .iter()
        .filter_map(|(key, tile)| {
            if tile.frame != state.frame {
                Some((0, tile.frame, *key))
            } else if replace_old && key.level != level {
                Some((1, tile.frame, *key))
            } else {
                None
            }
        })
        .collect();
    stale.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    for (_, _, key) in stale {
        if state.tiles.len() + state.pending.len() < cap {
            break;
        }
        state.tiles.remove(&key);
    }
}

/// 块数超上限时按「最久没用」淘汰，避免纹理显存跟着走过的地图无限增长。
fn evict_tiles(state: &mut LayerState, cap: usize, replace_old: bool, level: u32) {
    if state.tiles.len() <= cap {
        return;
    }
    // 只淘汰本帧没画到的块（`replace_old` 时也可淘汰旧网格的兜底块）：本帧画过的块
    // 对应的 draw 命令还在队列里，句柄一释放就会画成空白（「屏幕闪白一片」的来源之一）。
    let mut order: Vec<(u8, u64, TileKey)> = state
        .tiles
        .iter()
        .filter_map(|(key, tile)| {
            if tile.frame != state.frame {
                Some((0, tile.frame, *key))
            } else if replace_old && key.level != level {
                Some((1, tile.frame, *key))
            } else {
                None
            }
        })
        .collect();
    order.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let excess = state.tiles.len() - cap;
    for (_, _, key) in order.into_iter().take(excess) {
        state.tiles.remove(&key);
    }
}

/// 装上一张新贴图：同尺寸沿用同一句柄原地更新，尺寸变了就换新句柄。
fn install_texture(
    ctx: &egui::Context,
    name: String,
    previous: Option<TexPayload>,
    image: ColorImage,
    options: TextureOptions,
) -> TexPayload {
    let handle = match previous.and_then(|payload| payload.handle) {
        Some(mut handle) if handle.size() == image.size => {
            handle.set(image, options);
            handle
        }
        _ => ctx.load_texture(name, image, options),
    };
    TexPayload {
        handle: Some(handle),
    }
}

/// 一次纹理构建请求：块坐标、窗口、目标像素尺寸与种类。
struct BuildRequest {
    key: TileKey,
    window: PixelWindow,
    size: [u32; 2],
    density: f32,
    kind: BuildKind,
}

/// 后台构建纹理：线程自己开一份数据集，不碰图层那把锁。
///
/// 早期实现让所有块共享 `layer.handle()` 的 `Mutex<RasterDataset>`，于是 `MAX_TILE_JOBS`
/// 个线程只是排队等锁——实际并行度始终是 1。数据集打开只要约 10 ms，让每个解码线程
/// 自带一份后才是真并行，绘制线程的 `try_dataset()` 也不会再被秒级解码挡住。
fn spawn_build(layer: &RasterLayer, meta: Arc<DatasetSnapshot>, request: BuildRequest) -> BuildJob {
    let BuildRequest {
        key,
        window,
        size,
        density,
        kind,
    } = request;
    let (sender, receiver) = std::sync::mpsc::channel();
    let path = layer.path().to_path_buf();
    let display = layer.display.clone();
    std::thread::spawn(move || {
        let Ok(mut dataset) = RasterDataset::open(&path) else {
            return;
        };
        let Some(image) = build_image(&mut dataset, &meta, &window, size, &display) else {
            return;
        };
        let _ = sender.send(BuiltTexture {
            texture: image,
            key,
            window: [window.x, window.y, window.width, window.height],
            size,
            display,
            density,
            version: meta.version,
            kind,
        });
    });
    BuildJob {
        key,
        kind,
        rx: receiver,
    }
}

/// 同步构建（小影像）：后台正持锁时本帧先跳过，避免界面等待。
fn build_sync(
    layer: &RasterLayer,
    meta: &DatasetSnapshot,
    window: &PixelWindow,
    size: [u32; 2],
) -> Option<ColorImage> {
    let mut dataset = layer.try_dataset()?;
    build_image(&mut dataset, meta, window, size, &layer.display)
}

/// 锁空闲时刷新元数据快照；后台持锁时沿用旧快照继续绘制。
fn refresh_snapshot(state: &mut LayerState, layer: &RasterLayer) {
    let Some(dataset) = layer.try_dataset() else {
        return;
    };
    let stale = state
        .meta
        .as_ref()
        .is_none_or(|meta| meta.is_stale(&dataset));
    if stale {
        state.meta = Some(Arc::new(DatasetSnapshot::capture(&dataset)));
    }
}

/// 这块贴图现在还画不画得出去。
///
/// 只要求「不是缩得太狠」：缩小时一个纹理像素要摊到2 个以上屏幕像素就会混叠出
/// 横条，此时不画、等新网格的块；放大时旧块再旧也比底图清晰，直接交给GPU 拉伸
/// ——这样滚轮缩放全程都显示「当前最清晰的那些块」，不会整屏退回糊掉的底图。
fn drawable(cached: f32, density: f32) -> bool {
    cached >= density.max(f32::EPSILON) / DRAW_MAX_DECIMATE
}

/// 视觉上是否为同一缩放级别。
///
/// 两个方向给不同的宽限：放大时缓存网格变粗，超过 25% 就该重铺；缩小时缓存网格
/// 只是过采样，1.75 倍以内继续用——每格滚轮都重解码，既慢又会让画面在重建的空档里
/// 变糊（QGIS 的概览层级容差也是这个思路）。
fn same_zoom(cached: f32, density: f32) -> bool {
    let cached = cached.max(f32::EPSILON);
    let density = density.max(f32::EPSILON);
    if cached < density {
        density / cached <= 1.0 + REUSE_DENSITY_TOLERANCE
    } else {
        cached / density <= REUSE_OVER_DENSITY
    }
}

/// 是否需要把解码放到后台线程：大影像同步解码会卡住界面。
fn needs_async(meta: &DatasetSnapshot) -> bool {
    u64::from(meta.width) * u64::from(meta.height) > ASYNC_DECODE_PIXELS
}

fn full_uv() -> Rect {
    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))
}

fn full_window(meta: &DatasetSnapshot) -> PixelWindow {
    PixelWindow::new(0, 0, meta.width.max(1), meta.height.max(1))
}

/// 底图尺寸：长边缩到[`BASE_CANVAS_SIDE`]。
fn base_size(meta: &DatasetSnapshot) -> [u32; 2] {
    let longest = meta.width.max(meta.height).max(1);
    let scale = f64::from(BASE_CANVAS_SIDE) / f64::from(longest);
    [
        ((f64::from(meta.width) * scale).round() as u32).max(1),
        ((f64::from(meta.height) * scale).round() as u32).max(1),
    ]
}

/// 底图对应的密度（原始像素 / 底图像素），用于和细节块比较清晰度。
fn base_density(meta: &DatasetSnapshot) -> f32 {
    let size = base_size(meta);
    meta.width as f32 / size[0].max(1) as f32
}

/// 网格步长：每块覆盖多少源像素（≈ 一块贴图覆盖 [`TILE_SIDE`] 个屏幕像素）。
///
/// 只取决于密度，横纵同值⇒ 块在屏幕上总是正方形，不会出现「圆变椭圆」。
fn tile_step(density: f32) -> u32 {
    (TILE_SIDE as f32 * density.max(f32::EPSILON))
        .round()
        .max(1.0) as u32
}

/// 网格里覆盖 `[start, start + count)` 的块下标范围。
fn tile_range(start: u32, count: u32, step: u32) -> std::ops::RangeInclusive<i32> {
    let step = step.max(1);
    let first = (start / step) as i32;
    let last = (start.saturating_add(count.saturating_sub(1)) / step) as i32;
    first..=last
}

/// 视口在某步长下需要的块数。
fn tiles_needed(window: PixelWindow, step: u32) -> usize {
    tile_range(window.y, window.height, step).count()
        * tile_range(window.x, window.width, step).count()
}

/// 缓存预算：视口需要的块数 + [`TILE_CACHE_HEADROOM`]，且不低于 [`MAX_CACHED_TILES`]。
///
/// 关键在于它是**随视口变化的下限**而不是常数：一块贴图固定覆盖 512 个物理像素，
/// 视口需要的块数随窗口尺寸与 `pixels_per_point` 增长。固定 24 块时 1:86993 的 10 m
/// S2 影像在 150% 缩放的 1920×1080 窗口上需要 28 块，多出来的 4 块永远派不出去，屏幕上
/// 就是一大片空白；2560×1440@100% 正好需要 24 块，一块都派不出去。
fn tile_capacity(window: PixelWindow, step: u32) -> usize {
    tiles_needed(window, step)
        .saturating_add(TILE_CACHE_HEADROOM)
        .max(MAX_CACHED_TILES)
}

/// 一块覆盖的像素窗口：按步长切块，裁到影像边界内。
fn tile_window(width: u32, height: u32, step: u32, col: i32, row: i32) -> Option<PixelWindow> {
    let step = step.max(1);
    let x = u32::try_from(col).ok()?.saturating_mul(step);
    let y = u32::try_from(row).ok()?.saturating_mul(step);
    if x >= width || y >= height {
        return None;
    }
    let tile_width = step.min(width - x);
    let tile_height = step.min(height - y);
    Some(PixelWindow::new(x, y, tile_width, tile_height))
}

/// 块的纹理尺寸：与源窗口同比例，最长边 ≈ [`TILE_SIDE`]。
///
/// 缩小方向（`step > TILE_SIDE`）在 CPU 上重采样，避免整块抽稀出摩尔纹；放大方向
/// （`step < TILE_SIDE`）**不放大**，纹理按源像素大小上传，交给 GPU 的双线性过滤
/// 拉伸——CPU 上做最近邻放大只会把像素拉成方块（与 QGIS 默认最近邻同观感，但明显
/// 更糙），而线性过滤在界面线程零成本。
fn tile_size(window: &PixelWindow, step: u32) -> [u32; 2] {
    let scale = (f64::from(TILE_SIDE) / f64::from(step.max(1))).min(1.0);
    [
        ((f64::from(window.width) * scale).round() as u32).max(1),
        ((f64::from(window.height) * scale).round() as u32).max(1),
    ]
}

/// 块到视口中心的距离（源像素）：派活时让中间先出来。
fn tile_distance(tile: &PixelWindow, view: &PixelWindow) -> f64 {
    let center_x = f64::from(view.x) + f64::from(view.width) / 2.0;
    let center_y = f64::from(view.y) + f64::from(view.height) / 2.0;
    let tile_x = f64::from(tile.x) + f64::from(tile.width) / 2.0;
    let tile_y = f64::from(tile.y) + f64::from(tile.height) / 2.0;
    (tile_x - center_x).powi(2) + (tile_y - center_y).powi(2)
}

/// 像素窗口四角投影后的屏幕矩形（包围盒）。
fn pixel_window_screen_rect(
    meta: &DatasetSnapshot,
    window: &PixelWindow,
    projector: &Projector,
) -> Option<Rect> {
    let corners = [
        (window.x, window.y),
        (window.x + window.width, window.y),
        (window.x, window.y + window.height),
        (window.x + window.width, window.y + window.height),
    ];
    let mut min = (f32::INFINITY, f32::INFINITY);
    let mut max = (f32::NEG_INFINITY, f32::NEG_INFINITY);
    for (col, row) in corners {
        let world = pixel_to_world(meta, col, row);
        let (sx, sy) = projector.project_xy(world[0], world[1]);
        if !(sx.is_finite() && sy.is_finite()) {
            return None;
        }
        min = (min.0.min(sx), min.1.min(sy));
        max = (max.0.max(sx), max.1.max(sy));
    }
    if !(max.0 > min.0 && max.1 > min.1) {
        return None;
    }
    Some(Rect::from_min_max(pos2(min.0, min.1), pos2(max.0, max.1)))
}

/// 世界里可见范围→像素窗口（向上取整，保证覆盖整个可见区域）。
fn visible_window(meta: &DatasetSnapshot, world: Aabb) -> Option<PixelWindow> {
    if world.is_empty() {
        return None;
    }
    let georef = &meta.georeference;
    let width = f64::from(meta.width);
    let height = f64::from(meta.height);
    let corners = [
        (world.min[0], world.min[1]),
        (world.max[0], world.min[1]),
        (world.min[0], world.max[1]),
        (world.max[0], world.max[1]),
    ];
    let mut min_col = f64::INFINITY;
    let mut min_row = f64::INFINITY;
    let mut max_col = f64::NEG_INFINITY;
    let mut max_row = f64::NEG_INFINITY;
    for (x, y) in corners {
        let (col, row) = world_to_pixel_coords(georef, x, y, width, height)?;
        min_col = min_col.min(col);
        min_row = min_row.min(row);
        max_col = max_col.max(col);
        max_row = max_row.max(row);
    }
    let x0 = min_col.floor().clamp(0.0, width) as u32;
    let y0 = min_row.floor().clamp(0.0, height) as u32;
    let x1 = max_col.ceil().clamp(0.0, width) as u32;
    let y1 = max_row.ceil().clamp(0.0, height) as u32;
    let window = PixelWindow::new(x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
        .clamp_to(meta.width, meta.height);
    (!window.is_empty()).then_some(window)
}

/// 世界坐标 →像素坐标；无地理参考时按「像素底边为 y 转0」的约定换算。
fn world_to_pixel_coords(
    georef: &super::GeoReference,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Option<(f64, f64)> {
    match georef.world_to_pixel(x, y) {
        Some(pixel) => Some((pixel[0], pixel[1])),
        // 无地理参考：图像像素坐标即世界坐标，但世界y 朝上而像素行号朝下。
        None => {
            if !(x.is_finite() && y.is_finite()) {
                return None;
            }
            Some((x.clamp(0.0, width), height - y.clamp(0.0, height)))
        }
    }
}

fn pixel_to_world(meta: &DatasetSnapshot, col: u32, row: u32) -> [f64; 2] {
    meta.georeference
        .pixel_to_world(f64::from(col), f64::from(row))
        .unwrap_or([f64::from(col), f64::from(meta.height) - f64::from(row)])
}

/// 显示设置解析出的三个通道波段，以及可选的 Alpha 波段。
fn plan_bands(display: &RasterDisplay, band_count: usize) -> Option<([usize; 3], Option<usize>)> {
    if band_count == 0 {
        return None;
    }
    let bands = [
        display.bands[0].min(band_count - 1),
        display.bands[1].min(band_count - 1),
        display.bands[2].min(band_count - 1),
    ];
    let alpha_band = display
        .alpha_band
        .filter(|band| *band < band_count && !bands.contains(band));
    Some((bands, alpha_band))
}

/// 取样波段去重：单波段按灰度显示时三个通道是同一个波段，只需读一次。
///
/// 返回（取样波段列表、每个通道在列表中的下标、Alpha 在列表中的下标）。
fn sample_plan(
    bands: [usize; 3],
    alpha_band: Option<usize>,
) -> (Vec<usize>, [usize; 3], Option<usize>) {
    let mut sample_bands: Vec<usize> = Vec::with_capacity(4);
    let mut channel_plane = [0usize; 3];
    for channel in 0..3 {
        let band = bands[channel];
        channel_plane[channel] = match sample_bands.iter().position(|existing| *existing == band) {
            Some(index) => index,
            None => {
                sample_bands.push(band);
                sample_bands.len() - 1
            }
        };
    }
    let alpha = alpha_band.map(|alpha| {
        sample_bands.push(alpha);
        sample_bands.len() - 1
    });
    (sample_bands, channel_plane, alpha)
}

/// 读取窗口并合成为 RGBA 图像。
fn build_image(
    dataset: &mut RasterDataset,
    meta: &DatasetSnapshot,
    window: &PixelWindow,
    size: [u32; 2],
    display: &RasterDisplay,
) -> Option<ColorImage> {
    let band_count = meta.band_count();
    if band_count == 0 || size[0] == 0 || size[1] == 0 {
        return None;
    }
    let (bands, alpha_band) = plan_bands(display, band_count)?;
    let (sample_bands, channel_plane, _) = sample_plan(bands, alpha_band);
    let ranges = [
        ChannelStretch::new(
            channel_range(meta, bands[0], display, 0),
            display.invert[0],
            display.gamma[0],
        ),
        ChannelStretch::new(
            channel_range(meta, bands[1], display, 1),
            display.invert[1],
            display.gamma[1],
        ),
        ChannelStretch::new(
            channel_range(meta, bands[2], display, 2),
            display.invert[2],
            display.gamma[2],
        ),
    ];
    // 只读层级数据（不重采样），把「最近邻映射」与「拉伸合成」合并成一次遍历：
    // 省掉一块「重采样后」的中间缓冲和一遍内存往返。
    let (planes, level_window) = dataset
        .read_display_source(*window, size[0], size[1], &sample_bands)
        .ok()?;

    let level_width = level_window.width as usize;
    let level_height = level_window.height as usize;
    let plain_count = channel_plane.iter().copied().max().unwrap_or(0) + 1;
    if level_width == 0 || level_height == 0 || planes.len() < plain_count {
        return None;
    }
    let level_pixels = level_width.checked_mul(level_height)?;
    if planes.iter().any(|plane| plane.len() < level_pixels) {
        return None;
    }
    let nodata = meta.nodata;
    // Alpha 波段总是排在取样列表末尾（三通道之后）。
    let alpha_plane = alpha_band.and_then(|_| planes.get(sample_bands.len() - 1));
    let columns = super::nearest_indexes(level_window.width, size[0]);
    let rows = super::nearest_indexes(level_window.height, size[1]);
    let expected = size[0] as usize * size[1] as usize;
    let mut pixels = Vec::with_capacity(expected);
    let red_plane = &planes[channel_plane[0]];
    let green_plane = &planes[channel_plane[1]];
    let blue_plane = &planes[channel_plane[2]];
    // 灰度显示：三个通道取同一波段、同一拉伸参数，只算一次灰度值。
    let mono = channel_plane[0] == channel_plane[1]
        && channel_plane[1] == channel_plane[2]
        && ranges[0].same_as(&ranges[1])
        && ranges[1].same_as(&ranges[2])
        && alpha_plane.is_none();
    let transparent_nodata = display.nodata_transparent;
    let background = display.background;
    for &row in &rows {
        let row_base = row as usize * level_width;
        if mono {
            for &column in &columns {
                let value = red_plane[row_base + column as usize];
                if is_nodata(f64::from(value), nodata) {
                    if transparent_nodata {
                        pixels.push(Color32::TRANSPARENT);
                    } else if let Some(background) = background {
                        pixels.push(Color32::from_rgb(
                            background[0],
                            background[1],
                            background[2],
                        ));
                    } else {
                        pixels.push(Color32::from_rgb(0, 0, 0));
                    }
                    continue;
                }
                let gray = ranges[0].map(value);
                pixels.push(Color32::from_rgb(gray, gray, gray));
            }
            continue;
        }
        for &column in &columns {
            let offset = row_base + column as usize;
            let mut rgba = [0u8, 0, 0, 255];
            let mut transparent = false;
            for (channel, value) in [red_plane[offset], green_plane[offset], blue_plane[offset]]
                .into_iter()
                .enumerate()
            {
                if is_nodata(f64::from(value), nodata) {
                    if transparent_nodata {
                        transparent = true;
                    }
                    rgba[channel] = background.map_or(0, |background| background[channel]);
                    continue;
                }
                rgba[channel] = ranges[channel].map(value);
            }
            if let Some(alpha) = alpha_plane.and_then(|plane| plane.get(offset)) {
                rgba[3] = if is_nodata(f64::from(*alpha), nodata) {
                    0
                } else {
                    alpha.clamp(0.0, 255.0) as u8
                };
            }
            if transparent || rgba[3] == 0 {
                pixels.push(Color32::TRANSPARENT);
            } else if rgba[3] == 255 {
                // 不透明是绝大多数情况：跳过 alpha 预乘换算。
                pixels.push(Color32::from_rgb(rgba[0], rgba[1], rgba[2]));
            } else {
                pixels.push(Color32::from_rgba_unmultiplied(
                    rgba[0], rgba[1], rgba[2], rgba[3],
                ));
            }
        }
    }
    if pixels.len() < expected {
        return None;
    }
    let image = ColorImage {
        size: [size[0] as usize, size[1] as usize],
        pixels,
    };
    // 无地理参考时窗口来自像素坐标换算，行序与图像一致；有地理参考时 e<0，同样自上而下。
    Some(image)
}

/// 判断是否为NoData（浮点值按相等比较，与 GDAL 约定一致）。
#[inline(always)]
fn is_nodata(value: f64, nodata: Option<f64>) -> bool {
    nodata.is_some_and(|nodata| value == nodata)
}

/// 单通道显示范围（优先手动min/max，其次统计值，最后按数据类型给默认量程）。
fn channel_range(
    meta: &DatasetSnapshot,
    band: usize,
    display: &RasterDisplay,
    channel: usize,
) -> (f64, f64) {
    if display.max[channel] > display.min[channel] {
        return (display.min[channel], display.max[channel]);
    }
    if display.stretch == StretchType::None {
        return default_range(meta, band);
    }
    let Some(stats) = meta.statistics(band) else {
        return default_range(meta, band);
    };
    match display.stretch {
        StretchType::MinMax => (stats.min, stats.max),
        StretchType::PercentClip => stats.clipped(display.percent_clip),
        StretchType::StdDev => {
            let low = stats.mean - 2.0 * stats.std_dev;
            let high = stats.mean + 2.0 * stats.std_dev;
            if high > low {
                (low, high)
            } else {
                (stats.min, stats.max)
            }
        }
        StretchType::None => default_range(meta, band),
    }
}

/// 无统计值时的默认量程：浮点按0..1，整型按数据类型满量程。
fn default_range(meta: &DatasetSnapshot, band: usize) -> (f64, f64) {
    if let Some(stats) = meta.statistics(band) {
        return (stats.min, stats.max);
    }
    match meta.sample_type(band) {
        SampleType::U8 | SampleType::I8 => (0.0, 255.0),
        SampleType::U16 | SampleType::I16 => (0.0, 65535.0),
        SampleType::U32 | SampleType::I32 => (0.0, 4_294_967_295.0),
        SampleType::U64 | SampleType::I64 => (0.0, 4_294_967_295.0),
        SampleType::F32 | SampleType::F64 => (0.0, 1.0),
    }
}

/// 单通道拉伸参数：预先算好倒数，逐像素只做乘加，省掉除法与`round`。
struct ChannelStretch {
    low: f32,
    inv_span: f32,
    invert: bool,
    inv_gamma: f32,
}

impl ChannelStretch {
    fn new((low, high): (f64, f64), invert: bool, gamma: f32) -> Self {
        let span = (high - low) as f32;
        Self {
            low: low as f32,
            inv_span: if span > 0.0 { 1.0 / span } else { 0.0 },
            invert,
            inv_gamma: if gamma > 0.0 { 1.0 / gamma } else { 1.0 },
        }
    }

    /// 拉伸 + 反向 + Gamma →0..255。
    #[inline(always)]
    fn map(&self, value: f32) -> u8 {
        if self.inv_span <= 0.0 || !value.is_finite() {
            return 0;
        }
        let mut normalized = ((value - self.low) * self.inv_span).clamp(0.0, 1.0);
        if self.invert {
            normalized = 1.0 - normalized;
        }
        if (self.inv_gamma - 1.0).abs() > f32::EPSILON {
            normalized = normalized.powf(self.inv_gamma);
        }
        (normalized * 255.0 + 0.5) as u8
    }

    fn same_as(&self, other: &Self) -> bool {
        (self.low - other.low).abs() <= f32::EPSILON
            && (self.inv_span - other.inv_span).abs() <= f32::EPSILON
            && self.invert == other.invert
            && (self.inv_gamma - other.inv_gamma).abs() <= f32::EPSILON
    }
}

/// 单点拉伸（测试用）。
#[cfg(test)]
fn stretch_channel(value: f32, range: (f32, f32), invert: bool, gamma: f32) -> u8 {
    ChannelStretch::new((f64::from(range.0), f64::from(range.1)), invert, gamma).map(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::MapView;

    #[test]
    fn stretch_maps_into_byte_range() {
        assert_eq!(stretch_channel(0.0, (0.0, 100.0), false, 1.0), 0);
        assert_eq!(stretch_channel(100.0, (0.0, 100.0), false, 1.0), 255);
        assert_eq!(stretch_channel(50.0, (0.0, 100.0), false, 1.0), 128);
        // 反向
        assert_eq!(stretch_channel(0.0, (0.0, 100.0), true, 1.0), 255);
    }

    #[test]
    fn gamma_adjusts_midtones() {
        // ESRI 约定：out = in^(1/gamma)；gamma > 1 提亮，gamma < 1 变暗。
        let plain = stretch_channel(50.0, (0.0, 100.0), false, 1.0);
        let brightened = stretch_channel(50.0, (0.0, 100.0), false, 2.0);
        let darkened = stretch_channel(50.0, (0.0, 100.0), false, 0.62585);
        assert!(brightened > plain);
        assert!(darkened < plain);
    }

    #[test]
    fn zero_span_returns_black() {
        assert_eq!(stretch_channel(5.0, (1.0, 1.0), false, 1.0), 0);
    }

    #[test]
    fn nodata_detection() {
        assert!(is_nodata(-9999.0, Some(-9999.0)));
        assert!(!is_nodata(0.0, Some(-9999.0)));
        assert!(!is_nodata(0.0, None));
    }

    #[test]
    fn tile_windows_cover_the_whole_image() {
        // 1024×1024 的影像、步长 400：3×3 块，边缘块被裁到影像边界内。
        let mut covered = 0u64;
        for row in 0..3 {
            for col in 0..3 {
                let window = tile_window(1024, 1024, 400, col, row).unwrap();
                assert!(window.x + window.width <= 1024);
                assert!(window.y + window.height <= 1024);
                covered += u64::from(window.width) * u64::from(window.height);
            }
        }
        assert_eq!(covered, 1024 * 1024, "块必须铺满整幅影像");
        // 越界的块不存在。
        assert!(tile_window(1024, 1024, 400, 3, 0).is_none());
        assert!(tile_window(1024, 1024, 400, 0, 3).is_none());
    }

    #[test]
    fn tile_range_covers_every_touched_tile() {
        // 窗口 100..500、步长 200：命中的块是 0、1、2。
        let range = tile_range(100, 400, 200);
        assert_eq!(*range.start(), 0);
        assert_eq!(*range.end(), 2);
        // 恰好对齐时不多算一块。
        assert_eq!(tile_range(0, 400, 200).count(), 2);
        // 零宽窗口不炸。
        assert_eq!(tile_range(0, 0, 200).count(), 1);
    }

    #[test]
    fn tile_size_keeps_aspect_and_respects_limit() {
        // 缩小方向：一块 4 倍源像素的窗口缩到 TILE_SIDE 见方。
        assert_eq!(
            tile_size(&PixelWindow::new(0, 0, 2048, 2048), 2048),
            [512, 512]
        );
        // 放大方向不放大：纹理按源像素上传，由 GPU 线性过滤拉伸。
        assert_eq!(
            tile_size(&PixelWindow::new(0, 0, 400, 400), 400),
            [400, 400]
        );
        assert_eq!(tile_size(&PixelWindow::new(0, 0, 100, 80), 100), [100, 80]);
        // 边缘块按源像素比例收缩，长宽比保持。
        let size = tile_size(&PixelWindow::new(800, 0, 24, 400), 400);
        let got = f64::from(size[0]) / f64::from(size[1]);
        assert!((got - 24.0 / 400.0).abs() < 0.01, "横纵比必须保持：{got}");
    }

    #[test]
    fn tile_step_scales_with_density() {
        // 密度 1（1 源像素 = 1 屏幕像素）时一块盖 512 个源像素。
        assert_eq!(tile_step(1.0), TILE_SIDE);
        // 缩小 4 倍时一块盖 4 倍源像素。
        assert_eq!(tile_step(4.0), TILE_SIDE * 4);
        // 异常密度不能退化成 0。
        assert_eq!(tile_step(0.0), 1);
    }

    #[test]
    fn tiles_closest_to_view_center_come_first() {
        let view = PixelWindow::new(0, 0, 1000, 1000);
        let center = tile_window(1024, 1024, 400, 1, 1).unwrap();
        let corner = tile_window(1024, 1024, 400, 0, 0).unwrap();
        assert!(tile_distance(&center, &view) < tile_distance(&corner, &view));
    }

    #[test]
    fn stale_tiles_are_dropped_only_when_too_coarse() {
        // 放大：旧块（密度 8）比当前需要的（密度 1）粗，但比底图清楚，继续用。
        assert!(drawable(8.0, 1.0));
        // 缩小太多（一块要被摊到 2 倍以上屏幕像素）：不画，避免混叠。
        assert!(!drawable(1.0, 8.0));
        // 刚好在 2 倍以内：仍然画。
        assert!(drawable(1.0, 1.9));
    }

    #[test]
    fn full_cache_makes_room_for_the_new_grid() {
        let mut state = LayerState::default();
        let tile = |frame: u64| CachedTile {
            window: [0, 0, 8, 8],
            density: 16.0,
            version: 1,
            tex: TexPayload::default(),
            frame,
        };
        for index in 0..=MAX_CACHED_TILES {
            let key = TileKey {
                level: 1,
                col: index as i32,
                row: 0,
            };
            state.tiles.insert(key, tile(index as u64));
        }
        let fresh = TileKey {
            level: 2,
            col: 0,
            row: 0,
        };
        state.tiles.insert(fresh, tile(MAX_CACHED_TILES as u64));
        // 这一帧用到的是 col 0 的当前网格块，其余旧网格块都不在本帧画到的范围内。
        state.frame = MAX_CACHED_TILES as u64;

        make_room(&mut state, 2, MAX_CACHED_TILES, true);

        assert!(state.tiles.contains_key(&fresh), "当前网格的块不能被腾掉");
        assert!(
            state.tiles.len() + state.pending.len() < MAX_CACHED_TILES,
            "腾完必须留出新块的名额"
        );
        // 先丢最久没用的旧网格块。
        assert!(!state.tiles.contains_key(&TileKey {
            level: 1,
            col: 0,
            row: 0
        }));
    }

    #[test]
    fn old_grid_tiles_survive_until_the_new_grid_is_complete() {
        // 旧网格的块在新网格到齐前是唯一的兜底；提前丢掉会在缩放过程中闪出空白。
        let mut state = LayerState {
            frame: 5,
            ..LayerState::default()
        };
        for col in 0..4 {
            state.tiles.insert(
                TileKey {
                    level: 2,
                    col,
                    row: 0,
                },
                CachedTile {
                    window: [0, 0, 8, 8],
                    density: 16.0,
                    version: 1,
                    tex: TexPayload::default(),
                    frame: 5,
                },
            );
        }

        // 新网格还缺块：旧块必须留着。
        make_room(&mut state, 3, 4, false);
        assert_eq!(state.tiles.len(), 4, "新块到齐前旧网格的兜底块不能丢");

        // 新网格已铺满：旧块才能让位。
        make_room(&mut state, 3, 4, true);
        assert!(state.tiles.len() < 4, "新网格铺满后旧块应被替换");
    }

    #[test]
    fn offscreen_tiles_of_the_current_grid_are_reclaimed() {
        // 在同一缩放级别里平移很久之后，缓存会被「本网格但已移出屏幕」的块塞满；
        // 这时如果只按 level 淘汰就永远腾不出名额，视口里的块全部派不出去 → 空白。
        let mut state = LayerState {
            frame: 7,
            ..LayerState::default()
        };
        for index in 0..MAX_CACHED_TILES as i32 {
            state.tiles.insert(
                TileKey {
                    level: 3,
                    col: index,
                    row: 0,
                },
                CachedTile {
                    window: [0, 0, 8, 8],
                    density: 16.0,
                    version: 1,
                    tex: TexPayload::default(),
                    frame: 0,
                },
            );
        }
        let visible = TileKey {
            level: 3,
            col: 100,
            row: 0,
        };
        state.tiles.insert(
            visible,
            CachedTile {
                window: [0, 0, 8, 8],
                density: 16.0,
                version: 1,
                tex: TexPayload::default(),
                frame: 7,
            },
        );

        make_room(&mut state, 3, MAX_CACHED_TILES, false);

        assert!(state.tiles.contains_key(&visible), "本帧可见的块不能被淘汰");
        assert!(state.tiles.len() < MAX_CACHED_TILES, "必须腾出名额");
    }

    #[test]
    fn cache_budget_follows_the_viewport_not_a_fixed_number() {
        // 1:86993 的 10 m S2 影像（密度 2.436 → step 1247），在 150% 缩放的
        // 1920×1080 窗口上视口需要 28 块：旧实现固定 24 块，多出的 4 块永远派不出去。
        let scaled = PixelWindow::new(1975, 3509, 7030, 3962);
        assert_eq!(tiles_needed(scaled, 1247), 28);
        assert!(tile_capacity(scaled, 1247) > MAX_CACHED_TILES);

        // 2560×1440@100% 正好需要 24 块：`>= MAX_CACHED_TILES` 一上来就 break，
        // 一块都派不出去，整屏空白。
        let wide = PixelWindow::new(2367, 3731, 6246, 3518);
        assert_eq!(tiles_needed(wide, 1247), MAX_CACHED_TILES);
        assert!(tile_capacity(wide, 1247) > MAX_CACHED_TILES);

        // 小视口仍然沿用 24 块的下限，避免缓存无谓缩小。
        let small = PixelWindow::new(0, 0, 800, 600);
        assert_eq!(tile_capacity(small, 256), MAX_CACHED_TILES);
    }

    #[test]
    fn current_grid_is_drawn_above_the_previous_one() {
        // (是否当前网格, 密度)：旧网格先画、当前网格后画（盖在上面）。
        let mut order = [(true, 1.0f32), (false, 8.0f32)];
        order.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        assert!(
            !order[0].0,
            "放大后上一个网格的糊块不能再盖在清晰块上面（会出现「放大不渲染、平移才恢复」）"
        );
    }

    #[test]
    fn base_canvas_does_not_starve_behind_detail_tiles() {
        let path = crate::raster::fixture_tests::large_geotiff("paint-base-first");
        let layer = RasterLayer::open(&path, "big".to_owned()).unwrap();
        let rect = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(1600.0, 900.0));
        let view = MapView::fit(Some(layer.bounds), rect);
        let ctx = egui::Context::default();
        let mut painter = RasterPainter::default();

        paint_once(&ctx, &mut painter, rect, &view, &layer);
        let state = painter.states.get(&layer.id).expect("图层状态");
        assert!(
            state.jobs.iter().any(|job| job.kind == BuildKind::Base),
            "第一帧就要给底图派活：细节块占满线程后底图再也建不出来"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn oversized_display_reads_are_streamed_in_strips() {
        let path = crate::raster::fixture_tests::large_geotiff("paint-stream");
        let mut dataset = RasterDataset::open(&path).unwrap();
        let window = full_window(&DatasetSnapshot::capture(&dataset));
        let bands = [0usize];

        // 参照值：上限足够时整窗一次读完（平面是原始分辨率，由调用方再采样）。
        dataset.max_read_bytes = u64::MAX / 4;
        let (reference, reference_window) =
            dataset.read_display_source(window, 320, 240, &bands).unwrap();
        assert_eq!(reference_window.width, window.width, "上限够时应整窗读完");
        assert_eq!(reference[0].len(), 2048 * 2048);

        // 上限压到只够一条带：必须自动分条读取，且采样结果与整窗读取逐像素一致。
        dataset.max_read_bytes = 256 * 1024;
        let (streamed, out) = dataset.read_display_source(window, 320, 240, &bands).unwrap();
        assert_eq!(out, PixelWindow::new(0, 0, 320, 240));
        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].len(), 320 * 240);
        let columns = crate::raster::nearest_indexes(window.width, 320);
        let rows = crate::raster::nearest_indexes(window.height, 240);
        for y in 0..240usize {
            for x in 0..320usize {
                let expect = reference[0]
                    [rows[y] as usize * window.width as usize + columns[x] as usize];
                assert_eq!(
                    streamed[0][y * 320 + x],
                    expect,
                    "分条读取的结果在 ({x}, {y}) 处与整窗读取不一致"
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn large_images_decode_in_the_background_and_get_a_base_canvas() {
        let path = crate::raster::fixture_tests::large_geotiff("paint-async");
        let layer = RasterLayer::open(&path, "big".to_owned()).unwrap();
        let rect = Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        let view = MapView::fit(Some(layer.bounds), rect);
        let ctx = egui::Context::default();
        let mut painter = RasterPainter::default();

        // 第一帧：大影像绝不能在UI 线程解码，只发起后台任务。
        paint_once(&ctx, &mut painter, rect, &view, &layer);
        assert_eq!(painter.cached_textures(), 0, "大影像不应在 UI 线程同步解码");
        // 还没贴上一块：状态栏的圆环要显示「渲染中」，进度从 0 开始。
        let progress = painter.progress().expect("首帧应报告渲染中");
        assert!(
            (0.0..1.0).contains(&progress),
            "首帧进度应是 0..1 之间的部分进度，实际 {progress}"
        );

        // 后台线程完成后（下一帧取回结果）应该画出全图底图。
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(25));
            paint_once(&ctx, &mut painter, rect, &view, &layer);
            if painter.has_base_canvas(layer.id) {
                break;
            }
        }
        assert!(painter.has_base_canvas(layer.id), "应画出全图底图");

        // 视口需要的块全部贴上之后，进度回到 `None`：状态栏的圆环随之收起。
        for _ in 0..200 {
            paint_once(&ctx, &mut painter, rect, &view, &layer);
            if painter.progress().is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            painter.progress().is_none(),
            "块到齐后应报告渲染完成，实际 {:?}",
            painter.progress()
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 无头画一帧（只跑栅格绘制这一层）。
    fn paint_once(
        ctx: &egui::Context,
        painter: &mut RasterPainter,
        rect: Rect,
        view: &MapView,
        layer: &RasterLayer,
    ) {
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let egui_painter = ctx.layer_painter(egui::LayerId::background());
            painter.paint(ctx, &egui_painter, rect, view, &[layer]);
        });
    }

    #[test]
    fn large_images_are_detected_for_background_decoding() {
        let path = crate::raster::fixture_tests::sample_geotiff("paint-async-size");
        let dataset = RasterDataset::open(&path).unwrap();
        let large = DatasetSnapshot {
            width: 10_980,
            height: 10_980,
            ..DatasetSnapshot::capture(&dataset)
        };
        assert!(needs_async(&large));
        assert_eq!(base_size(&large), [BASE_CANVAS_SIDE, BASE_CANVAS_SIDE]);
        assert!((base_density(&large) - 10_980.0 / 768.0).abs() < 0.01);

        let small = DatasetSnapshot {
            width: 512,
            height: 256,
            ..DatasetSnapshot::capture(&dataset)
        };
        assert!(!needs_async(&small));
        assert_eq!(base_size(&small), [BASE_CANVAS_SIDE, BASE_CANVAS_SIDE / 2]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn zoom_tolerance_accepts_small_changes_only() {
        assert!(same_zoom(10.0, 11.0));
        assert!(!same_zoom(10.0, 20.0));
    }

    #[test]
    fn snapshot_is_stale_after_statistics_are_stored() {
        let path = crate::raster::fixture_tests::sample_geotiff("paint-stale");
        let mut dataset = RasterDataset::open(&path).unwrap();
        let snapshot = DatasetSnapshot::capture(&dataset);
        assert!(!snapshot.is_stale(&dataset));
        dataset
            .compute_statistics(0, &std::sync::atomic::AtomicBool::new(false), &mut |_| {})
            .unwrap();
        assert!(snapshot.is_stale(&dataset), "统计更新后快照应判定过期");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn grayscale_band_is_sampled_once_and_drawn_mono() {
        let path = crate::raster::fixture_tests::sample_geotiff("paint-mono");
        let mut dataset = RasterDataset::open(&path).unwrap();
        let display = RasterDisplay::for_band_count(1, false);
        let window = PixelWindow::new(0, 0, 64, 64);
        let meta = DatasetSnapshot::capture(&dataset);
        let image = build_image(&mut dataset, &meta, &window, [32, 32], &display).unwrap();
        assert_eq!(image.size, [32, 32]);
        // 灰度显示三个通道必须一样，且取样波段只有一个。
        assert!(
            image
                .pixels
                .iter()
                .all(|pixel| pixel.r() == pixel.g() && pixel.g() == pixel.b() && pixel.a() == 255)
        );
        assert!(image.pixels.iter().any(|pixel| pixel.r() > 0));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn alpha_band_produces_transparency() {
        let path = crate::raster::fixture_tests::sample_rgb_geotiff("paint-alpha");
        let mut dataset = RasterDataset::open(&path).unwrap();
        let mut display = RasterDisplay::for_band_count(3, false);
        // 用波段1 当Alpha：像素值递增，因此有一部分像素应变成半透明/全透明。
        display.bands = [0, 0, 0];
        display.alpha_band = Some(1);
        let window = PixelWindow::new(0, 0, 64, 64);
        let meta = DatasetSnapshot::capture(&dataset);
        let image = build_image(&mut dataset, &meta, &window, [16, 16], &display).unwrap();
        assert_eq!(image.size, [16, 16]);
        assert!(image.pixels.iter().any(|pixel| pixel.a() < 255));
        let _ = std::fs::remove_file(&path);
    }
}
