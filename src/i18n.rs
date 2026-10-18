//! 国际化（i18n）支持。
//!
//! 支持语言：简体中文（默认）、English、日本語。
//! 启动时通过 `sys-locale` 读取系统区域设置自动选择语言；
//! 用户可在「帮助 → 语言」菜单中手动切换。
//!
//! 使用方式：所有 UI 字符串以英文 key 引用，经 [`Language::t`] 解析为
//! 当前语言的文本；需要嵌入动态值的字符串使用 `{0}` 占位符，
//! 经 [`Language::tf`] 替换。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Chinese,
    English,
    Japanese,
}

impl Language {
    pub const ALL: [Language; 3] = [Language::Chinese, Language::English, Language::Japanese];

    /// 语言在本语言环境下的显示名（用于语言切换菜单）。
    pub fn native_name(self) -> &'static str {
        match self {
            Language::Chinese => "简体中文",
            Language::English => "English",
            Language::Japanese => "日本語",
        }
    }

    /// 读取系统区域设置并映射到支持的语言（默认简体中文）。
    pub fn from_system() -> Language {
        match sys_locale::get_locale().as_deref() {
            Some(l) if l.to_ascii_lowercase().starts_with("zh") => Language::Chinese,
            Some(l) if l.to_ascii_lowercase().starts_with("ja") => Language::Japanese,
            Some(l) if l.to_ascii_lowercase().starts_with("en") => Language::English,
            _ => Language::Chinese,
        }
    }

    /// 按键解析字符串；未收录的 key 原样返回。
    pub fn t(self, key: &str) -> &str {
        match self {
            Language::Chinese => zh(key),
            Language::English => en(key),
            Language::Japanese => ja(key),
        }
    }

    /// 解析含 `{0}` `{1}` ... 占位符的字符串并依次填入动态值。
    pub fn tf(self, key: &str, args: &[&str]) -> String {
        let template = self.t(key);
        let mut out =
            String::with_capacity(template.len() + args.iter().map(|a| a.len()).sum::<usize>());
        let mut chars = template.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '{' {
                let mut num = String::new();
                while matches!(chars.peek(), Some(d) if d.is_ascii_digit()) {
                    num.push(chars.next().unwrap());
                }
                if !num.is_empty() && chars.peek() == Some(&'}') {
                    chars.next(); // 消费 '}'
                    let idx: usize = num.parse().unwrap_or(0);
                    if let Some(a) = args.get(idx) {
                        out.push_str(a);
                    }
                } else {
                    out.push('{');
                    out.push_str(&num);
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// 简体中文
// ---------------------------------------------------------------------------
fn zh(key: &str) -> &str {
    match key {
        // 菜单
        "menu.file" => "文件(F)",
        "menu.edit" => "编辑(E)",
        "menu.view" => "视图(V)",
        "menu.bookmarks" => "书签(B)",
        "menu.insert" => "插入(I)",
        "menu.select" => "选择(S)",
        "menu.geoprocessing" => "地理处理(G)",
        "menu.customize" => "自定义(C)",
        "menu.window" => "窗口(W)",
        "menu.help" => "帮助(H)",
        // 文件
        "file.new" => "新建工程",
        "file.open" => "打开工程...",
        "file.save" => "保存",
        "file.saveAs" => "另存为...",
        "file.pageSetup" => "页面设置...",
        "file.print" => "打印...",
        "file.exit" => "退出",
        // 工程（新建 / 打开 / 保存）
        "window.untitled" => "未命名",
        "project.openTitle" => "打开工程",
        "project.saveAsTitle" => "保存工程",
        "project.fileFilter" => "RustGIS 工程文件",
        "project.savePromptTitle" => "保存更改",
        "project.savePromptBody" => "当前工程有未保存的更改。\n是否保存更改？",
        // 编辑
        "edit.undo" => "撤销",
        "edit.redo" => "重做",
        "edit.cut" => "剪切",
        "edit.copy" => "复制",
        "edit.paste" => "粘贴",
        "edit.selectAll" => "全选",
        "edit.properties" => "属性...",
        // 视图
        "view.zoomIn" => "放大",
        "view.zoomOut" => "缩小",
        "view.pan" => "平移",
        "view.zoomFull" => "缩放至全图",
        "view.dataFrame" => "数据帧视图",
        "view.layout" => "布局视图",
        "view.basemap" => "底图管理...",
        "view.bookmarks" => "书签管理...",
        // 书签
        "bookmark.add" => "添加书签...",
        "bookmark.manager" => "书签管理器...",
        // 插入
        "insert.text" => "文本...",
        "insert.picture" => "图片...",
        "insert.grid" => "经纬网格...",
        "insert.scalebar" => "比例尺...",
        "insert.legend" => "图例...",
        // 选择
        "select.byLocation" => "按位置选择...",
        "select.byAttribute" => "按属性选择...",
        "select.bySquare" => "按矩形选择...",
        "select.invert" => "反选...",
        "select.deselect" => "取消选择",
        // 窗口
        "window.contents" => "内容列表...",
        "window.find" => "查找...",
        "window.catalog" => "目录...",
        "window.arrange" => "平铺窗口",
        "window.close" => "关闭窗口",
        // 图层
        "layer.add" => "添加图层...",
        "layer.addRaster" => "添加栅格图层...",
        "layer.addSafe" => "添加 Sentinel-2 SAFE 产品包...",
        "layer.order" => "图层顺序...",
        "layer.properties" => "图层属性...",
        "layer.display" => "显示设置...",
        // 数据
        "data.connect" => "连接数据库...",
        "data.online" => "在线地图服务...",
        "data.import" => "导入数据...",
        "data.export" => "导出数据...",
        "data.table" => "数据表视图",
        "data.manage" => "数据管理...",
        // 地理处理（叠加分析工具箱：菜单项 / 工具名 / 对话框）
        "overlay.buffer" => "缓冲区…",
        "overlay.clip.menu" => "裁剪…",
        "overlay.intersect.menu" => "相交…",
        "overlay.union.menu" => "联合…",
        "overlay.merge.menu" => "合并…",
        "overlay.dissolve.menu" => "融合…",
        "overlay.clip" => "裁剪",
        "overlay.intersect" => "相交",
        "overlay.union" => "联合",
        "overlay.merge" => "合并",
        "overlay.dissolve" => "融合",
        "geo.windowTitle" => "{0}（地理处理）",
        "geo.tool" => "工具",
        "geo.input" => "输入要素",
        "geo.output" => "输出要素类",
        "geo.fields" => "融合字段",
        "geo.orderClip" => "第 1 个为输入要素，第 2 个为裁剪要素。",
        "geo.orderMulti" => "勾选顺序即叠加顺序，至少两个。",
        "geo.orderSingle" => "选择要融合的输入要素。",
        "geo.dissolveAll" => "未勾选任何字段时执行「融合全部」。",
        "geo.status.running" => "正在执行地理处理…",
        "geo.status.done" => "地理处理完成：{0}",
        "geo.error.fewInputs" => "该工具至少需要 {0} 个输入要素。",
        "geo.help.tool.clip" => {
            "裁剪：用「裁剪要素」（第 2 个输入，必须是面）切分输入要素。面取交集；线被裁剪面切断；点保留落在裁剪面内的那些。输出会带上输入要素的全部属性。"
        }
        "geo.help.tool.intersect" => {
            "相交：计算所有输入要素的公共部分。输入必须同属点 / 线 / 面一类。面取交集且输出各输入字段的合并属性（重名自动加 _1、_2 后缀）；线输出交点；点输出所有输入都覆盖的位置。"
        }
        "geo.help.tool.union" => {
            "联合：把多个面图层的并集切成互不重叠的要素，每个要素只保留它真正来源的那个图层的属性（重叠处按输入顺序先到先得，面积不会被重复计算）。"
        }
        "geo.help.tool.merge" => {
            "合并：把所有输入要素原样拼接成一个图层，不做任何几何运算——重叠部分会保留两份，属性列会自动对齐补齐。"
        }
        "geo.help.tool.dissolve" => {
            "融合：按「融合字段」的取值把要素分组，同一组内相邻的面合并成一条。不勾选任何字段即「融合全部」。"
        }
        "geo.help.inputs" => {
            "参与运算的输入要素图层。勾选顺序就是叠加顺序：第 1 个是主输入（决定字段列表与默认输出名），其余依次叠加。"
        }
        "geo.help.output" => {
            "输出要素类的完整路径。RustGIS 将结果写出为 GeoJSON 文件（.geojson），并自动加入地图显示。"
        }
        "geo.help.fields" => {
            "输入要素中用于融合的字段。按其取值分组融合；不勾选任何字段表示把所有要素融合成一个组。"
        }
        // 尚未实现的空间分析工具（占位）
        "analysis.siting" => "选址分析...",
        "analysis.network" => "网络分析...",
        "analysis.raster" => "栅格分析",
        "analysis.stats" => "空间统计...",
        "analysis.3d" => "3D 分析",
        // 地图
        "map.symbology" => "符号化...",
        "map.labels" => "标注...",
        "map.legend" => "图例...",
        "map.scale" => "比例尺...",
        "map.properties" => "地图属性...",
        // 自定义 / 工具
        "tools.geoprocessing" => "地理处理",
        "tools.modelBuilder" => "模型构建器...",
        "tools.toolbox" => "工具箱...",
        "customize.toolbars" => "工具栏...",
        "tools.options" => "选项...",
        // 帮助
        "help.help" => "使用帮助",
        "help.docs" => "文档...",
        "help.language" => "语言",
        "help.about" => "关于 RustGIS",
        // 状态栏
        "status.ready" => "就绪",
        "status.meta" => "投影: {0}  |  坐标系: {1}  |  RustGIS {2}",
        "projection.none" => "未设置",
        "projection.geographic" => "地理坐标系",
        "projection.projected" => "投影坐标系",
        "projection.unknown" => "未知",
        "crs.notSet" => "未设置",
        "crs.notIdentified" => "未识别",
        "status.dev" => "「{0}」功能开发中",
        "status.coords" => "经度 {0}  纬度 {1}",
        "status.scale" => "比例 1:{0}  1 像素 = {1} m",
        "status.added" => "已添加「{0}」（{1} 个要素）",
        "status.loading" => "正在解析「{0}」…",
        "status.loadError" => "加载失败：{0}",
        "status.loadAborted" => "导入线程异常退出，请重试。",
        "status.newProject" => "已创建新工程（未保存）",
        "status.saving" => "正在保存工程…",
        "status.saved" => "已保存「{0}」",
        "status.saveError" => "保存失败：{0}",
        "status.saveAborted" => "保存线程异常退出，请重试。",
        "status.opening" => "正在打开工程「{0}」…",
        "status.opened" => "已打开工程「{0}」",
        // 地图视图占位
        "mapview.title" => "地图视图",
        "mapview.desc1" => {
            "从左侧「内容列表」面板或「插入 → 添加图层」导入 GeoJSON / Shapefile 数据。"
        }
        "mapview.desc2" => "导入后可缩放、平移浏览。",
        // 图层面板（内容列表）
        "layers.panel" => "内容列表",
        "layers.root" => "图层",
        "layers.zoomToLayer" => "缩放至图层",
        "layers.zoomToGroup" => "缩放至组",
        "layers.add" => "添加",
        "layers.addTip" => "添加图层",
        "layers.newGroupTip" => "新建图层组",
        "layers.newGroupName" => "新建图层组",
        "layers.newGroupHere" => "在此新建图层组",
        "layers.expandAllTip" => "全部展开",
        "layers.collapseAllTip" => "全部折叠",
        "layers.moveToTop" => "移出组",
        "layers.loading" => "正在等待文件选择…",
        "layers.parsing" => "正在解析数据…",
        "layers.empty" => "尚无图层。\n点击工具条的「+」导入 GeoJSON / Shapefile 文件。",
        "layers.remove" => "移除",
        "layers.show" => "显示",
        "layers.hide" => "隐藏",
        // 对话框
        "dialog.developTitle" => "功能开发中 — {0}",
        "dialog.developHeading" => "「{0}」正在开发中",
        "dialog.developBody" => {
            "该功能尚未实现，敬请期待 RustGIS 后续版本。\n您的反馈将帮助我们确定开发优先级。"
        }
        "dialog.ok" => "确定",
        "dialog.save" => "保存",
        "dialog.dontSave" => "不保存",
        "dialog.cancel" => "取消",
        "dialog.errorTitle" => "错误",
        "dialog.addTitle" => "添加图层",
        "dialog.addBody" => {
            "请选择一个 GeoJSON（.geojson / .json）或 Shapefile（.shp）文件。\n导入后可直接缩放、平移浏览。"
        }
        "dialog.aboutTitle" => "关于 RustGIS",
        "dialog.aboutSubtitle" => "纯 Rust 复刻 ArcGIS 的桌面 GIS 平台",
        "dialog.aboutTagline" => "低内存占用 · 高性能 · 跨平台 · 内存安全",
        "dialog.aboutLicense" => "MIT / Apache-2.0 双协议",
        "dialog.about.font" => "渲染字体",
        // 缓冲区分析
        "buffer.title" => "缓冲区",
        "buffer.input" => "输入要素",
        "buffer.output" => "输出要素类",
        "buffer.distance" => "距离 [值或字段]",
        "buffer.distance.linear" => "线性单位",
        "buffer.distance.field" => "字段",
        "buffer.side" => "侧类型（可选）",
        "buffer.end" => "末端类型（可选）",
        "buffer.dissolve" => "融合类型（可选）",
        "buffer.dissolveFields" => "融合字段（可选）",
        "buffer.selectAll" => "全选",
        "buffer.deselectAll" => "取消全选",
        "buffer.addField" => "添加字段",
        "buffer.environments" => "环境...",
        "buffer.hideHelp" => "<< 隐藏帮助",
        "buffer.showHelp" => ">> 显示帮助",
        "buffer.toolHelp" => "工具帮助",
        "buffer.noField" => "（输入要素没有可用字段）",
        "buffer.noLayer" => "（无图层）",
        "buffer.browseOutput" => "选择输出文件",
        "buffer.defaultOutput" => "buffer.geojson",
        "buffer.addFieldTip" => "添加字段仅用于 ModelBuilder，此处不可用。",
        "buffer.side.full" => "FULL",
        "buffer.side.left" => "LEFT",
        "buffer.side.right" => "RIGHT",
        "buffer.side.outside" => "OUTSIDE_ONLY",
        "buffer.end.round" => "ROUND",
        "buffer.end.flat" => "FLAT",
        "buffer.end.square" => "SQUARE",
        "buffer.dissolve.none" => "NONE",
        "buffer.dissolve.all" => "ALL",
        "buffer.dissolve.list" => "LIST",
        "buffer.unit.meters" => "米",
        "buffer.unit.kilometers" => "千米",
        "buffer.unit.feet" => "英尺",
        "buffer.unit.miles" => "英里",
        "buffer.unit.yards" => "码",
        "buffer.unit.nauticalMiles" => "海里",
        "buffer.unit.degrees" => "度",
        "buffer.help.input" => "要执行缓冲的输入点、线或面要素图层。",
        "buffer.help.output" => {
            "输出要素类的完整路径。RustGIS 将结果写出为 GeoJSON 文件（.geojson），并自动加入地图显示。"
        }
        "buffer.help.distance" => {
            "缓冲距离为固定数值，或取自输入要素中存储距离的数值字段。\n未指定线性单位时，距离按输入要素空间参考的单位解释：投影坐标系按地图单位，地理坐标系按米，并按要素纬度做测地线近似。"
        }
        "buffer.help.side" => {
            "FULL—线在两侧生成缓冲区；面沿其外围生成缓冲区并包含输入面；点环绕该点生成圆形缓冲区。\nLEFT / RIGHT—仅对线有效，在线的拓扑左侧 / 右侧生成缓冲区。\nOUTSIDE_ONLY—仅对面有效，只在输入面之外生成缓冲区，重叠部分会被移除。"
        }
        "buffer.help.end" => {
            "ROUND—线缓冲区的末端为半圆形，与输入线端点的距离等于缓冲距离。\nFLAT—线缓冲区的末端在输入线端点处平切。\nSQUARE—线缓冲区的末端为方形，沿输入线方向外延缓冲距离。"
        }
        "buffer.help.dissolve" => {
            "NONE—无论是否重叠，均保持每个要素的独立缓冲区。这是默认设置。\nALL—将所有缓冲区融合为单个要素，从而移除重叠。\nLIST—融合共享所列字段（传递自输入要素）属性值的所有缓冲区。"
        }
        "buffer.help.dissolveFields" => "输入要素中用于融合的字段。仅当融合类型为 LIST 时生效。",
        "buffer.status.running" => "正在计算缓冲区…",
        "buffer.status.done" => "缓冲区分析完成：{0}",
        "buffer.error.noInput" => "请选择输入要素图层。",
        "buffer.error.badDistance" => "距离必须是非零数值。",
        "buffer.error.noOutput" => "请填写输出要素类路径。",
        "buffer.error.busy" => "已有后台任务正在进行，请稍候。",
        "buffer.error.noDissolveField" => "融合类型为 LIST 时至少要勾选一个融合字段。",
        // ── 栅格图层（v0.3） ──
        "layers.rasterOpening" => "正在打开栅格…",
        "layers.properties" => "属性…",
        "raster.noGeoReference" => {
            "该影像没有地理参考信息，已按像素坐标显示；可在「图层属性 → 范围」中查看详情。"
        }
        "dialog.addRasterTitle" => "添加栅格数据",
        "dialog.rasterProps" => "图层属性 - {0}",
        "dialog.pyramidTitle" => "构建金字塔",
        "dialog.pyramidBody" => {
            "{0}（{1}）尺寸较大且没有金字塔/概览，浏览会明显变慢。是否现在构建概览？"
        }
        "dialog.pyramidHint" => {
            "构建在后台进行，期间可继续操作；结果写入同目录的 .ovr 文件（与 QGIS / ArcGIS 的概览格式一致），删除该文件即可释放空间。"
        }
        "dialog.yes" => "是",
        "dialog.no" => "否",
        "status.rasterOpening" => "正在打开栅格文件…",
        "status.rasterAdded" => "已添加栅格图层 {0}（{1}，{2} 个波段，{3}）",
        "status.rasterStats" => "正在计算统计：{0}（{1}）",
        "status.rasterPyramid" => "正在构建金字塔：{0}（{1}）",
        "status.rasterStatsDone" => "统计完成：{0} 个波段",
        "status.rasterPyramidDone" => "金字塔构建完成：{0} 级，已写入 .ovr",
        "status.rasterJobError" => "栅格后台任务失败：{0}",
        // 右下角通知
        "toast.layerAdded" => "已添加图层",
        "toast.rasterAdded" => "已添加栅格图层",
        "toast.dropTitle" => "无法添加文件",
        "toast.dropUnsupported" => {
            "不支持的文件类型：{0}（可拖入 .geojson / .json / .shp / .tif / .tiff，或 Sentinel-2 .SAFE 目录）"
        },
        "mapview.dropHint" => "松开鼠标即可添加图层",
        "status.openedWithWarnings" => "已打开工程 {0}（含警告）",
        "props.tab.general" => "常规",
        "props.tab.source" => "源",
        "props.tab.extent" => "范围",
        "props.tab.display" => "显示",
        "props.tab.symbology" => "符号系统",
        "props.tab.time" => "时间",
        "props.ok" => "确定",
        "props.cancel" => "取消",
        "props.apply" => "应用",
        "props.name" => "名称",
        "props.layerType" => "图层类型",
        "props.typeRaster" => "栅格图层",
        "props.sourcePath" => "源路径",
        "props.visible" => "可见",
        "props.show" => "显示",
        "props.width" => "宽度",
        "props.bands" => "波段数",
        "props.dataType" => "像素数据类型",
        "props.compression" => "压缩方式",
        "props.block" => "分块",
        "props.fileSize" => "文件大小",
        "props.bigTiff" => "BigTIFF",
        "props.overviews" => "概览",
        "props.nodata" => "NoData",
        "props.photometric" => "光度解释",
        "props.bandInfo" => "波段信息",
        "props.bitsFormat" => "{0}（{1} 位）",
        "props.bandLayout" => "{0}（{1}）",
        "props.layoutPlanar" => "波段交错（Planar）",
        "props.layoutChunky" => "像素交错（Chunky）",
        "props.blockFormat" => "{0}：{1} × {2}（{3} × {4} 块，共 {5} 块）",
        "props.overviewFormat" => "内部 {0} / 外部 {1}（共 {2} 级）",
        "props.bandLine" => "#{0} {1}（{2}，{3} 位，NoData {4}）",
        "props.none" => "无",
        "props.yes" => "是",
        "props.no" => "否",
        "props.extentX" => "范围 X",
        "props.extentY" => "范围 Y",
        "props.pixelSize" => "像元大小",
        "props.crs" => "坐标系",
        "props.gcp" => "地面控制点",
        "props.warning" => "警告",
        "props.resampling" => "重采样",
        "props.nearest" => "最近邻",
        "props.resamplingHint" => "按视图比例尺自动选择层级，并用最近邻采样。",
        "props.styleStretch" => "拉伸",
        "props.styleRgb" => "RGB 合成",
        "props.styleRgbTitle" => "以 RGB 合成方式绘制栅格",
        "props.channel" => "通道",
        "props.band" => "波段",
        "props.red" => "红色",
        "props.green" => "绿色",
        "props.blue" => "蓝色",
        "props.alpha" => "Alpha",
        "props.background" => "显示背景值",
        "props.nodataAs" => "将 NoData 显示为",
        "props.transparent" => "透明",
        "props.stretchType" => "拉伸类型",
        "props.stretchNone" => "无",
        "props.stretchMinMax" => "最小-最大",
        "props.stretchPercent" => "百分比截断",
        "props.stretchStdDev" => "标准差",
        "props.percentClip" => "百分比截断",
        "props.min" => "最小值",
        "props.max" => "最大值",
        "props.invert" => "反向",
        "props.gamma" => "Gamma",
        "props.stretchHint" => "拉伸只作用于显示，不修改原始像素。",
        "props.statistics" => "统计数据",
        "props.statsDataset" => "统计数据集",
        "props.computeStats" => "计算统计",
        "props.statsLine" => "最小值 {0}  最大值 {1}  平均值 {2}  标准差 {3}",
        "props.statsFromFile" => "（来自文件）",
        "props.statsEstimate" => "（估算）",
        "props.statsMissing" => "尚未计算统计值。",
        "props.metadata" => "元数据项",
        "props.timeNone" => "该图层没有时间信息。",
        _ => key,
    }
}

// ---------------------------------------------------------------------------
// English
// ---------------------------------------------------------------------------
fn en(key: &str) -> &str {
    match key {
        // Menus
        "menu.file" => "File(F)",
        "menu.edit" => "Edit(E)",
        "menu.view" => "View(V)",
        "menu.bookmarks" => "Bookmarks(B)",
        "menu.insert" => "Insert(I)",
        "menu.select" => "Select(S)",
        "menu.geoprocessing" => "Geoprocessing(G)",
        "menu.customize" => "Customize(C)",
        "menu.window" => "Window(W)",
        "menu.help" => "Help(H)",
        // File
        "file.new" => "New Project",
        "file.open" => "Open Project...",
        "file.save" => "Save",
        "file.saveAs" => "Save As...",
        "file.pageSetup" => "Page Setup...",
        "file.print" => "Print...",
        "file.exit" => "Exit",
        // Project (new / open / save)
        "window.untitled" => "Untitled",
        "project.openTitle" => "Open Project",
        "project.saveAsTitle" => "Save Project As",
        "project.fileFilter" => "RustGIS Project",
        "project.savePromptTitle" => "Save Changes",
        "project.savePromptBody" => {
            "The current project has unsaved changes.\nDo you want to save them?"
        }
        // Edit
        "edit.undo" => "Undo",
        "edit.redo" => "Redo",
        "edit.cut" => "Cut",
        "edit.copy" => "Copy",
        "edit.paste" => "Paste",
        "edit.selectAll" => "Select All",
        "edit.properties" => "Properties...",
        // View
        "view.zoomIn" => "Zoom In",
        "view.zoomOut" => "Zoom Out",
        "view.pan" => "Pan",
        "view.zoomFull" => "Zoom to Full Extent",
        "view.dataFrame" => "Data Frame View",
        "view.layout" => "Layout View",
        "view.basemap" => "Basemap Manager...",
        "view.bookmarks" => "Bookmark Manager...",
        // Bookmarks
        "bookmark.add" => "Add Bookmark...",
        "bookmark.manager" => "Bookmark Manager...",
        // Insert
        "insert.text" => "Text...",
        "insert.picture" => "Picture...",
        "insert.grid" => "Grid...",
        "insert.scalebar" => "Scale Bar...",
        "insert.legend" => "Legend...",
        // Select
        "select.byLocation" => "Select By Location...",
        "select.byAttribute" => "Select By Attribute...",
        "select.bySquare" => "Select By Square...",
        "select.invert" => "Invert Selection...",
        "select.deselect" => "Clear Selection",
        // Window
        "window.contents" => "Table Of Contents...",
        "window.find" => "Find...",
        "window.catalog" => "Catalog...",
        "window.arrange" => "Tile Windows",
        "window.close" => "Close Window",
        // Layers
        "layer.add" => "Add Layer...",
        "layer.addRaster" => "Add Raster Layer...",
        "layer.addSafe" => "Add Sentinel-2 SAFE Product...",
        "layer.order" => "Layer Order...",
        "layer.properties" => "Layer Properties...",
        "layer.display" => "Display Settings...",
        // Data
        "data.connect" => "Connect to Database...",
        "data.online" => "Online Map Service...",
        "data.import" => "Import Data...",
        "data.export" => "Export Data...",
        "data.table" => "Attribute Table",
        "data.manage" => "Data Management...",
        // Geoprocessing (overlay toolset: menu items / tool names / dialog)
        "overlay.buffer" => "Buffer…",
        "overlay.clip.menu" => "Clip…",
        "overlay.intersect.menu" => "Intersect…",
        "overlay.union.menu" => "Union…",
        "overlay.merge.menu" => "Merge…",
        "overlay.dissolve.menu" => "Dissolve…",
        "overlay.clip" => "Clip",
        "overlay.intersect" => "Intersect",
        "overlay.union" => "Union",
        "overlay.merge" => "Merge",
        "overlay.dissolve" => "Dissolve",
        "geo.windowTitle" => "{0} (Geoprocessing)",
        "geo.tool" => "Tool",
        "geo.input" => "Input Features",
        "geo.output" => "Output Feature Class",
        "geo.fields" => "Dissolve Fields",
        "geo.orderClip" => "The 1st is the input; the 2nd is the clip feature.",
        "geo.orderMulti" => "The checking order is the overlay order; at least two.",
        "geo.orderSingle" => "Select the input features to dissolve.",
        "geo.dissolveAll" => "With no field checked, Dissolve All is performed.",
        "geo.status.running" => "Running geoprocessing…",
        "geo.status.done" => "Geoprocessing finished: {0}",
        "geo.error.fewInputs" => "This tool needs at least {0} input features.",
        "geo.help.tool.clip" => {
            "Clip: cuts the input features with the clip feature (the 2nd input, which must be a polygon layer). Polygons are intersected; lines are cut at the clip boundary; points inside the clip polygon are kept. All input attributes are carried over."
        }
        "geo.help.tool.intersect" => {
            "Intersect: computes the common part of all inputs. All inputs must share one geometry class (point, line, or polygon). Polygons keep the merged attributes of every input (duplicate names get _1, _2 suffixes); lines emit crossing points; points emit the positions covered by every input."
        }
        "geo.help.tool.union" => {
            "Union: splits the union of several polygon layers into non-overlapping features, each keeping only the attributes of the layer it actually came from (overlaps are resolved in input order, so no area is counted twice)."
        }
        "geo.help.tool.merge" => {
            "Merge: appends all input features into one layer with no geometric processing — overlaps are kept twice and attribute columns are aligned and padded automatically."
        }
        "geo.help.tool.dissolve" => {
            "Dissolve: groups features by the values of the dissolve fields and merges touching polygons inside each group. With no field checked this is Dissolve All."
        }
        "geo.help.inputs" => {
            "The input feature layers to process. The checking order is the overlay order: the 1st is the primary input (it drives the field list and the default output name), the rest are overlaid in turn."
        }
        "geo.help.output" => {
            "Full path of the output feature class. RustGIS writes the result as a GeoJSON file (.geojson) and adds it to the map."
        }
        "geo.help.fields" => {
            "Input fields used for dissolving: features are grouped by their values. Checking no field puts every feature into a single group."
        }
        // Not-yet-implemented spatial analysis tools (placeholders)
        "analysis.siting" => "Site Selection...",
        "analysis.network" => "Network Analysis...",
        "analysis.raster" => "Raster Analysis",
        "analysis.stats" => "Spatial Statistics...",
        "analysis.3d" => "3D Analysis",
        // Map
        "map.symbology" => "Symbology...",
        "map.labels" => "Labels...",
        "map.legend" => "Legend...",
        "map.scale" => "Scale Bar...",
        "map.properties" => "Map Properties...",
        // Tools
        "tools.geoprocessing" => "Geoprocessing",
        "tools.modelBuilder" => "Model Builder...",
        "tools.toolbox" => "Toolbox...",
        "customize.toolbars" => "Toolbars...",
        "tools.options" => "Options...",
        // Help
        "help.help" => "Help",
        "help.docs" => "Documentation...",
        "help.language" => "Language",
        "help.about" => "About RustGIS",
        // Status bar
        "status.ready" => "Ready",
        "status.meta" => "Projection: {0}  |  CRS: {1}  |  RustGIS {2}",
        "projection.none" => "None",
        "projection.geographic" => "Geographic",
        "projection.projected" => "Projected",
        "projection.unknown" => "Unknown",
        "crs.notSet" => "None",
        "crs.notIdentified" => "Unidentified",
        "status.dev" => "\"{0}\" is under development",
        "status.coords" => "Lon {0}  Lat {1}",
        "status.scale" => "Scale 1:{0}  1 px = {1} m",
        "status.added" => "Added \"{0}\" ({1} features)",
        "status.loading" => "Loading \"{0}\"…",
        "status.loadError" => "Failed to load: {0}",
        "status.loadAborted" => "The import thread exited unexpectedly. Please try again.",
        "status.newProject" => "New project created (unsaved)",
        "status.saving" => "Saving project…",
        "status.saved" => "Saved \"{0}\"",
        "status.saveError" => "Failed to save: {0}",
        "status.saveAborted" => "The save thread exited unexpectedly. Please try again.",
        "status.opening" => "Opening project \"{0}\"…",
        "status.opened" => "Opened project \"{0}\"",
        // Map view placeholder
        "mapview.title" => "Map View",
        "mapview.desc1" => {
            "Import GeoJSON / Shapefile data from the Layers panel (left) or the Layers / Data menus."
        }
        "mapview.desc2" => "Pan and zoom to explore after import.",
        // Layers panel (table of contents)
        "layers.panel" => "Table Of Contents",
        "layers.root" => "Layers",
        "layers.zoomToLayer" => "Zoom to Layer",
        "layers.zoomToGroup" => "Zoom to Group",
        "layers.add" => "Add",
        "layers.addTip" => "Add layer",
        "layers.newGroupTip" => "New group layer",
        "layers.newGroupName" => "New Group Layer",
        "layers.newGroupHere" => "New group layer here",
        "layers.expandAllTip" => "Expand all",
        "layers.collapseAllTip" => "Collapse all",
        "layers.moveToTop" => "Move out of group",
        "layers.loading" => "Waiting for file selection…",
        "layers.parsing" => "Parsing data…",
        "layers.empty" => {
            "No layers yet.\nClick \"+\" on the toolbar to import a GeoJSON or Shapefile."
        }
        "layers.remove" => "Remove",
        "layers.show" => "Show",
        "layers.hide" => "Hide",
        // Dialogs
        "dialog.developTitle" => "Under Development — {0}",
        "dialog.developHeading" => "{0} is under development.",
        "dialog.developBody" => {
            "This feature is not yet implemented and is on the RustGIS roadmap.\nYour feedback helps us prioritize development."
        }
        "dialog.ok" => "OK",
        "dialog.save" => "Save",
        "dialog.dontSave" => "Don't Save",
        "dialog.cancel" => "Cancel",
        "dialog.errorTitle" => "Error",
        "dialog.addTitle" => "Add Layer",
        "dialog.addBody" => {
            "Choose a GeoJSON (.geojson / .json) or Shapefile (.shp) file.\nYou can then pan and zoom to explore."
        }
        "dialog.aboutTitle" => "About RustGIS",
        "dialog.aboutSubtitle" => "A desktop GIS platform that re-creates ArcGIS in pure Rust",
        "dialog.aboutTagline" => "Low memory · High performance · Cross-platform · Memory safe",
        "dialog.aboutLicense" => "Dual-licensed under MIT / Apache-2.0",
        "dialog.about.font" => "Render font",
        // Buffer analysis
        "buffer.title" => "Buffer",
        "buffer.input" => "Input Features",
        "buffer.output" => "Output Feature Class",
        "buffer.distance" => "Distance [value or field]",
        "buffer.distance.linear" => "Linear unit",
        "buffer.distance.field" => "Field",
        "buffer.side" => "Side Type (optional)",
        "buffer.end" => "End Type (optional)",
        "buffer.dissolve" => "Dissolve Type (optional)",
        "buffer.dissolveFields" => "Dissolve Field(s) (optional)",
        "buffer.selectAll" => "Select All",
        "buffer.deselectAll" => "Deselect All",
        "buffer.addField" => "Add Field",
        "buffer.environments" => "Environments...",
        "buffer.hideHelp" => "<< Hide Help",
        "buffer.showHelp" => ">> Show Help",
        "buffer.toolHelp" => "Tool Help",
        "buffer.noField" => "(input has no usable fields)",
        "buffer.noLayer" => "(no layers)",
        "buffer.browseOutput" => "Choose output file",
        "buffer.defaultOutput" => "buffer.geojson",
        "buffer.addFieldTip" => "Add Field is used only in ModelBuilder and is unavailable here.",
        "buffer.side.full" => "FULL",
        "buffer.side.left" => "LEFT",
        "buffer.side.right" => "RIGHT",
        "buffer.side.outside" => "OUTSIDE_ONLY",
        "buffer.end.round" => "ROUND",
        "buffer.end.flat" => "FLAT",
        "buffer.end.square" => "SQUARE",
        "buffer.dissolve.none" => "NONE",
        "buffer.dissolve.all" => "ALL",
        "buffer.dissolve.list" => "LIST",
        "buffer.unit.meters" => "Meters",
        "buffer.unit.kilometers" => "Kilometers",
        "buffer.unit.feet" => "Feet",
        "buffer.unit.miles" => "Miles",
        "buffer.unit.yards" => "Yards",
        "buffer.unit.nauticalMiles" => "Nautical Miles",
        "buffer.unit.degrees" => "Degrees",
        "buffer.help.input" => "The input point, line, or polygon feature layer to be buffered.",
        "buffer.help.output" => {
            "Full path of the output feature class. RustGIS writes the result as a GeoJSON file (.geojson) and adds it to the map."
        }
        "buffer.help.distance" => {
            "The buffer distance as a fixed value, or from a numeric field of the input features that stores the distance.\nWhen no linear unit is given, the distance is interpreted in the units of the input spatial reference: map units for a projected coordinate system, meters for a geographic coordinate system (with a geodesic approximation per feature latitude)."
        }
        "buffer.help.side" => {
            "FULL—Lines are buffered on both sides; polygons are buffered around and include the input polygon; points are buffered as a circle.\nLEFT / RIGHT—Valid for lines only; the buffer is generated on the topological left / right of the line.\nOUTSIDE_ONLY—Valid for polygons only; the buffer is generated outside the input polygon and the overlap is removed."
        }
        "buffer.help.end" => {
            "ROUND—The ends of the line buffer are semicircular, a distance equal to the buffer distance from the input line endpoints.\nFLAT—The ends of the line buffer are flat and stop at the input line endpoints.\nSQUARE—The ends of the line buffer are square and extend the buffer distance along the input line direction."
        }
        "buffer.help.dissolve" => {
            "NONE—Each feature keeps its own buffer regardless of overlap. This is the default.\nALL—All buffers are dissolved into a single feature, removing overlap.\nLIST—Buffers that share the values of the listed fields (carried over from the input features) are dissolved."
        }
        "buffer.help.dissolveFields" => {
            "Input feature fields used for dissolving. Applies only when the dissolve type is LIST."
        }
        "buffer.status.running" => "Computing buffer…",
        "buffer.status.done" => "Buffer analysis finished: {0}",
        "buffer.error.noInput" => "Select an input feature layer.",
        "buffer.error.badDistance" => "The distance must be a non-zero number.",
        "buffer.error.noOutput" => "Provide a path for the output feature class.",
        "buffer.error.busy" => "Another background task is running; please wait.",
        "buffer.error.noDissolveField" => {
            "Dissolve type LIST requires at least one dissolve field."
        }
        // ── Raster layers (v0.3) ──
        "layers.rasterOpening" => "Opening raster…",
        "layers.properties" => "Properties…",
        "raster.noGeoReference" => {
            "This image has no georeferencing and is displayed in pixel coordinates; see Layer Properties → Extent for details."
        }
        "dialog.addRasterTitle" => "Add Raster Data",
        "dialog.rasterProps" => "Layer Properties - {0}",
        "dialog.pyramidTitle" => "Build Pyramids",
        "dialog.pyramidBody" => {
            "{0} ({1}) is large and has no pyramids or overviews, so panning and zooming will be slow. Build overviews now?"
        }
        "dialog.pyramidHint" => {
            "Building runs in the background; results are written to a .ovr file next to the image (the same overview format QGIS and ArcGIS use). Delete that file to reclaim the space."
        }
        "dialog.yes" => "Yes",
        "dialog.no" => "No",
        "status.rasterOpening" => "Opening raster file…",
        "status.rasterAdded" => "Added raster layer {0} ({1}, {2} band(s), {3})",
        "status.rasterStats" => "Computing statistics: {0} ({1})",
        "status.rasterPyramid" => "Building pyramids: {0} ({1})",
        "status.rasterStatsDone" => "Statistics finished: {0} band(s)",
        "status.rasterPyramidDone" => "Pyramids built: {0} level(s) written to .ovr",
        "status.rasterJobError" => "Raster background task failed: {0}",
        // Toast notifications (bottom-right corner)
        "toast.layerAdded" => "Layer added",
        "toast.rasterAdded" => "Raster layer added",
        "toast.dropTitle" => "Cannot add file",
        "toast.dropUnsupported" => {
            "Unsupported file type: {0} (drop .geojson / .json / .shp / .tif / .tiff, or a Sentinel-2 .SAFE folder)"
        },
        "mapview.dropHint" => "Release to add the file as a layer",
        "status.openedWithWarnings" => "Opened project {0} with warnings",
        "props.tab.general" => "General",
        "props.tab.source" => "Source",
        "props.tab.extent" => "Extent",
        "props.tab.display" => "Display",
        "props.tab.symbology" => "Symbology",
        "props.tab.time" => "Time",
        "props.ok" => "OK",
        "props.cancel" => "Cancel",
        "props.apply" => "Apply",
        "props.name" => "Name",
        "props.layerType" => "Layer type",
        "props.typeRaster" => "Raster layer",
        "props.sourcePath" => "Source",
        "props.visible" => "Visible",
        "props.show" => "Display",
        "props.width" => "Width",
        "props.bands" => "Bands",
        "props.dataType" => "Pixel type",
        "props.compression" => "Compression",
        "props.block" => "Block",
        "props.fileSize" => "File size",
        "props.bigTiff" => "BigTIFF",
        "props.overviews" => "Overviews",
        "props.nodata" => "NoData",
        "props.photometric" => "Photometric",
        "props.bandInfo" => "Band info",
        "props.bitsFormat" => "{0} ({1} bit)",
        "props.bandLayout" => "{0} ({1})",
        "props.layoutPlanar" => "band interleaved (Planar)",
        "props.layoutChunky" => "pixel interleaved (Chunky)",
        "props.blockFormat" => "{0}: {1} × {2} ({3} × {4} blocks, {5} in total)",
        "props.overviewFormat" => "internal {0} / external {1} ({2} levels)",
        "props.bandLine" => "#{0} {1} ({2}, {3} bit, NoData {4})",
        "props.none" => "None",
        "props.yes" => "Yes",
        "props.no" => "No",
        "props.extentX" => "Extent X",
        "props.extentY" => "Extent Y",
        "props.pixelSize" => "Cell size",
        "props.crs" => "Coordinate system",
        "props.gcp" => "Ground control points",
        "props.warning" => "Warning",
        "props.resampling" => "Resampling",
        "props.nearest" => "Nearest neighbour",
        "props.resamplingHint" => {
            "Pyramid levels are picked from the view scale and sampled with nearest neighbour."
        }
        "props.styleStretch" => "Stretch",
        "props.styleRgb" => "RGB composite",
        "props.styleRgbTitle" => "Draw the raster as an RGB composite",
        "props.channel" => "Channel",
        "props.band" => "Band",
        "props.red" => "Red",
        "props.green" => "Green",
        "props.blue" => "Blue",
        "props.alpha" => "Alpha",
        "props.background" => "Display background value",
        "props.nodataAs" => "Display NoData as",
        "props.transparent" => "Transparent",
        "props.stretchType" => "Stretch type",
        "props.stretchNone" => "None",
        "props.stretchMinMax" => "Min-Max",
        "props.stretchPercent" => "Percent clip",
        "props.stretchStdDev" => "Standard deviation",
        "props.percentClip" => "Percent clip",
        "props.min" => "Minimum",
        "props.max" => "Maximum",
        "props.invert" => "Invert",
        "props.gamma" => "Gamma",
        "props.stretchHint" => "Stretch affects display only; source pixels are not modified.",
        "props.statistics" => "Statistics",
        "props.statsDataset" => "Statistics dataset",
        "props.computeStats" => "Compute statistics",
        "props.statsLine" => "Min {0}  Max {1}  Mean {2}  Std {3}",
        "props.statsFromFile" => " (from file)",
        "props.statsEstimate" => " (estimated)",
        "props.statsMissing" => "Statistics have not been computed yet.",
        "props.metadata" => "Metadata item",
        "props.timeNone" => "This layer has no time information.",
        _ => key,
    }
}

// ---------------------------------------------------------------------------
// 日本語
// ---------------------------------------------------------------------------
fn ja(key: &str) -> &str {
    match key {
        // メニュー
        "menu.file" => "ファイル(F)",
        "menu.edit" => "編集(E)",
        "menu.view" => "表示(V)",
        "menu.bookmarks" => "ブックマーク(B)",
        "menu.insert" => "挿入(I)",
        "menu.select" => "選択(S)",
        "menu.geoprocessing" => "ジオプロセシング(G)",
        "menu.customize" => "カスタマイズ(C)",
        "menu.window" => "ウィンドウ(W)",
        "menu.help" => "ヘルプ(H)",
        // ファイル
        "file.new" => "新規プロジェクト",
        "file.open" => "プロジェクトを開く...",
        "file.save" => "保存",
        "file.saveAs" => "名前を付けて保存...",
        "file.pageSetup" => "用紙設定...",
        "file.print" => "印刷...",
        "file.exit" => "終了",
        // プロジェクト（新規 / 開く / 保存）
        "window.untitled" => "無題",
        "project.openTitle" => "プロジェクトを開く",
        "project.saveAsTitle" => "プロジェクトを名前を付けて保存",
        "project.fileFilter" => "RustGIS プロジェクトファイル",
        "project.savePromptTitle" => "変更を保存",
        "project.savePromptBody" => {
            "現在のプロジェクトに未保存の変更があります。\n変更を保存しますか？"
        }
        // 編集
        "edit.undo" => "元に戻す",
        "edit.redo" => "やり直す",
        "edit.cut" => "切り取り",
        "edit.copy" => "コピー",
        "edit.paste" => "ペースト",
        "edit.selectAll" => "すべて選択",
        "edit.properties" => "プロパティ...",
        // 表示
        "view.zoomIn" => "拡大",
        "view.zoomOut" => "縮小",
        "view.pan" => "移動",
        "view.zoomFull" => "全体を表示",
        "view.dataFrame" => "データフレーム表示",
        "view.layout" => "レイアウト表示",
        "view.basemap" => "背景地図の管理...",
        "view.bookmarks" => "ブックマーク管理...",
        // ブックマーク
        "bookmark.add" => "ブックマークの追加...",
        "bookmark.manager" => "ブックマークの管理...",
        // 挿入
        "insert.text" => "テキスト...",
        "insert.picture" => "画像...",
        "insert.grid" => "グリッド...",
        "insert.scalebar" => "縮尺...",
        "insert.legend" => "凡例...",
        // 選択
        "select.byLocation" => "位置による選択...",
        "select.byAttribute" => "属性による選択...",
        "select.bySquare" => "矩形による選択...",
        "select.invert" => "選択の反転...",
        "select.deselect" => "選択の解除",
        // ウィンドウ
        "window.contents" => "コンテンツ一覧...",
        "window.find" => "検索...",
        "window.catalog" => "カタログ...",
        "window.arrange" => "ウィンドウをタイル表示",
        "window.close" => "ウィンドウを閉じる",
        // 図層
        "layer.add" => "図層の追加...",
        "layer.addRaster" => "ラスタ図層の追加...",
        "layer.addSafe" => "Sentinel-2 SAFE プロダクトの追加...",
        "layer.order" => "図層の順序...",
        "layer.properties" => "図層のプロパティ...",
        "layer.display" => "表示設定...",
        // データ
        "data.connect" => "データベースに接続...",
        "data.online" => "オンライン地図サービス...",
        "data.import" => "データの取り込み...",
        "data.export" => "データのエクスポート...",
        "data.table" => "属性表",
        "data.manage" => "データ管理...",
        // 分析
        // ジオプロセシング（オーバーレイ ツールセット：メニュー / ツール名 / ダイアログ）
        "overlay.buffer" => "バッファ…",
        "overlay.clip.menu" => "クリップ…",
        "overlay.intersect.menu" => "インターセクト…",
        "overlay.union.menu" => "ユニオン…",
        "overlay.merge.menu" => "マージ…",
        "overlay.dissolve.menu" => "ディゾルブ…",
        "overlay.clip" => "クリップ",
        "overlay.intersect" => "インターセクト",
        "overlay.union" => "ユニオン",
        "overlay.merge" => "マージ",
        "overlay.dissolve" => "ディゾルブ",
        "geo.windowTitle" => "{0}（ジオプロセシング）",
        "geo.tool" => "ツール",
        "geo.input" => "入力フィーチャ",
        "geo.output" => "出力フィーチャクラス",
        "geo.fields" => "ディゾルブ フィールド",
        "geo.orderClip" => "1 番目が入力、2 番目がクリップ フィーチャです。",
        "geo.orderMulti" => "チェック順がオーバーレイ順です（2 つ以上）。",
        "geo.orderSingle" => "ディゾルブする入力フィーチャを選択します。",
        "geo.dissolveAll" => {
            "フィールドを 1 つもチェックしない場合は「すべてディゾルブ」を実行します。"
        }
        "geo.status.running" => "ジオプロセシングを実行しています…",
        "geo.status.done" => "ジオプロセシングが完了しました：{0}",
        "geo.error.fewInputs" => "このツールには少なくとも {0} 個の入力フィーチャが必要です。",
        "geo.help.tool.clip" => {
            "クリップ：クリップ フィーチャ（2 番目の入力。ポリゴン レイヤである必要があります）で入力フィーチャを切り取ります。ポリゴンは交差部分、ラインはクリップ境界で分割、ポイントはクリップ ポリゴン内のものが残ります。入力の属性はすべて引き継がれます。"
        }
        "geo.help.tool.intersect" => {
            "インターセクト：すべての入力の共通部分を計算します。入力はポイント / ライン / ポリゴンのいずれか同一のジオメトリ タイプである必要があります。ポリゴンは各入力のフィールドを統合した属性を保持し（重複名には _1、_2 が付きます）、ラインは交差点、ポイントは全入力が覆う位置を出力します。"
        }
        "geo.help.tool.union" => {
            "ユニオン：複数のポリゴン レイヤの和集合を重なりのないフィーチャに分割し、各フィーチャは実際の由来レイヤの属性のみを保持します（重なりは入力順で解決されるため、面積が二重に計上されることはありません）。"
        }
        "geo.help.tool.merge" => {
            "マージ：すべての入力フィーチャをジオメトリ演算なしで 1 つのレイヤに連結します。重なりはそのまま残り、属性列は自動で揃えられます。"
        }
        "geo.help.tool.dissolve" => {
            "ディゾルブ：ディゾルブ フィールドの値でフィーチャをグループ化し、同じグループ内で接するポリゴンを結合します。フィールドを 1 つも選択しない場合は「すべてディゾルブ」です。"
        }
        "geo.help.inputs" => {
            "演算に使用する入力フィーチャ レイヤ。チェック順がオーバーレイ順です。1 番目が主入力（フィールド一覧と既定の出力名を決めます）で、残りが順に重ねられます。"
        }
        "geo.help.output" => {
            "出力フィーチャクラスの完全なパス。RustGIS は結果を GeoJSON ファイル（.geojson）として書き出し、自動的に地図に追加します。"
        }
        "geo.help.fields" => {
            "ディゾルブに使用する入力フィールド。値ごとにグループ化して融合します。フィールドを選択しない場合はすべてのフィーチャが 1 つのグループになります。"
        }
        // 未実装の空間解析ツール（プレースホルダ）
        "analysis.siting" => "立地分析...",
        "analysis.network" => "ネットワーク分析...",
        "analysis.raster" => "ラスタ分析",
        "analysis.stats" => "空間統計...",
        "analysis.3d" => "3D 分析",
        // 地図
        "map.symbology" => "記号化...",
        "map.labels" => "ラベル...",
        "map.legend" => "凡例...",
        "map.scale" => "縮尺...",
        "map.properties" => "地図のプロパティ...",
        // ツール
        "tools.geoprocessing" => "地理処理",
        "tools.modelBuilder" => "モデルビルダー...",
        "tools.toolbox" => "ツールボックス...",
        "customize.toolbars" => "ツールバー...",
        "tools.options" => "オプション...",
        // ヘルプ
        "help.help" => "ヘルプ",
        "help.docs" => "ドキュメント...",
        "help.language" => "言語",
        "help.about" => "RustGIS について",
        // ステータスバー
        "status.ready" => "準備完了",
        "status.meta" => "投影: {0}  |  座標系: {1}  |  RustGIS {2}",
        "projection.none" => "未設定",
        "projection.geographic" => "地理座標系",
        "projection.projected" => "投影座標系",
        "projection.unknown" => "不明",
        "crs.notSet" => "未設定",
        "crs.notIdentified" => "未識別",
        "status.dev" => "「{0}」は開発中です",
        "status.coords" => "経度 {0}  緯度 {1}",
        "status.scale" => "縮尺 1:{0}  1 ピクセル = {1} m",
        "status.added" => "「{0}」を追加しました（{1} 件）",
        "status.loading" => "「{0}」を読み込み中…",
        "status.loadError" => "読み込みに失敗しました：{0}",
        "status.loadAborted" => "取り込みスレッドが異常終了しました。再試行してください。",
        "status.newProject" => "新規プロジェクトを作成しました（未保存）",
        "status.saving" => "プロジェクトを保存中…",
        "status.saved" => "「{0}」を保存しました",
        "status.saveError" => "保存に失敗しました：{0}",
        "status.saveAborted" => "保存スレッドが異常終了しました。再試行してください。",
        "status.opening" => "プロジェクト「{0}」を開いています…",
        "status.opened" => "プロジェクト「{0}」を開きました",
        // 地図ビュー
        "mapview.title" => "地図ビュー",
        "mapview.desc1" => {
            "左側の「コンテンツ一覧」パネルまたは「挿入 → 図層の追加」から GeoJSON / Shapefile をインポートしてください。"
        }
        "mapview.desc2" => "インポート後はパン・ズームで閲覧できます。",
        // 図層パネル（コンテンツ一覧）
        "layers.panel" => "コンテンツ一覧",
        "layers.root" => "図層",
        "layers.zoomToLayer" => "図層にズーム",
        "layers.zoomToGroup" => "グループにズーム",
        "layers.add" => "追加",
        "layers.addTip" => "図層を追加",
        "layers.newGroupTip" => "新規図層グループ",
        "layers.newGroupName" => "新規図層グループ",
        "layers.newGroupHere" => "ここに新規グループ",
        "layers.expandAllTip" => "すべて展開",
        "layers.collapseAllTip" => "すべて折りたたむ",
        "layers.moveToTop" => "グループから出す",
        "layers.loading" => "ファイル選択を待っています…",
        "layers.parsing" => "データを解析中…",
        "layers.empty" => {
            "図層がありません。\nツールバーの「+」から GeoJSON / Shapefile をインポートしてください。"
        }
        "layers.remove" => "削除",
        "layers.show" => "表示する",
        "layers.hide" => "非表示にする",
        // ダイアログ
        "dialog.developTitle" => "機能開発中 — {0}",
        "dialog.developHeading" => "「{0}」は開発中です",
        "dialog.developBody" => {
            "この機能は未実装です。今後の RustGIS バージョンで提供予定です。\nご意見をお聞かせください。開発の優先順位に反映します。"
        }
        "dialog.ok" => "確定",
        "dialog.save" => "保存",
        "dialog.dontSave" => "保存しない",
        "dialog.cancel" => "キャンセル",
        "dialog.errorTitle" => "エラー",
        "dialog.addTitle" => "図層の追加",
        "dialog.addBody" => {
            "GeoJSON (.geojson / .json) または Shapefile (.shp) ファイルを選択してください。\nインポート後はパン・ズームで閲覧できます。"
        }
        "dialog.aboutTitle" => "RustGIS について",
        "dialog.aboutSubtitle" => {
            "純粋な Rust で開発された ArcGIS にインスパイアされたデスクトップ GIS プラットフォーム"
        }
        "dialog.aboutTagline" => "低メモリ・高パフォーマンス・クロスプラットフォーム・メモリセーフ",
        "dialog.aboutLicense" => "MIT / Apache-2.0 ダイヤルライセンス",
        "dialog.about.font" => "レンダリングフォント",
        // バッファ分析
        "buffer.title" => "バッファ",
        "buffer.input" => "入力フィーチャ",
        "buffer.output" => "出力フィーチャクラス",
        "buffer.distance" => "距離 [値またはフィールド]",
        "buffer.distance.linear" => "単位",
        "buffer.distance.field" => "フィールド",
        "buffer.side" => "側タイプ（オプション）",
        "buffer.end" => "端点タイプ（オプション）",
        "buffer.dissolve" => "融合タイプ（オプション）",
        "buffer.dissolveFields" => "融合フィールド（オプション）",
        "buffer.selectAll" => "すべて選択",
        "buffer.deselectAll" => "すべて解除",
        "buffer.addField" => "フィールドの追加",
        "buffer.environments" => "環境設定...",
        "buffer.hideHelp" => "<< ヘルプを隠す",
        "buffer.showHelp" => ">> ヘルプを表示",
        "buffer.toolHelp" => "ツールヘルプ",
        "buffer.noField" => "（入力に使用可能なフィールドがありません）",
        "buffer.noLayer" => "（レイヤがありません）",
        "buffer.browseOutput" => "出力ファイルを選択",
        "buffer.defaultOutput" => "buffer.geojson",
        "buffer.addFieldTip" => {
            "フィールドの追加は ModelBuilder 専用のため、ここでは使用できません。"
        }
        "buffer.side.full" => "FULL",
        "buffer.side.left" => "LEFT",
        "buffer.side.right" => "RIGHT",
        "buffer.side.outside" => "OUTSIDE_ONLY",
        "buffer.end.round" => "ROUND",
        "buffer.end.flat" => "FLAT",
        "buffer.end.square" => "SQUARE",
        "buffer.dissolve.none" => "NONE",
        "buffer.dissolve.all" => "ALL",
        "buffer.dissolve.list" => "LIST",
        "buffer.unit.meters" => "メートル",
        "buffer.unit.kilometers" => "キロメートル",
        "buffer.unit.feet" => "フィート",
        "buffer.unit.miles" => "マイル",
        "buffer.unit.yards" => "ヤード",
        "buffer.unit.nauticalMiles" => "海里",
        "buffer.unit.degrees" => "度",
        "buffer.help.input" => {
            "バッファを作成する入力ポイント、ライン、またはポリゴンのフィーチャレイヤ。"
        }
        "buffer.help.output" => {
            "出力フィーチャクラスの完全なパス。RustGIS は結果を GeoJSON ファイル（.geojson）として書き出し、自動的に地図に追加します。"
        }
        "buffer.help.distance" => {
            "バッファ距離は固定値、または距離を格納した入力フィーチャの数値フィールドから取得します。\n単位を指定しない場合、距離は入力の空間参照の単位として解釈されます。投影座標系では地図単位、地理座標系ではメートル（フィーチャの緯度ごとに測地線近似）。"
        }
        "buffer.help.side" => {
            "FULL—ラインは両側にバッファを作成します。ポリゴンは周囲にバッファを作成し入力ポリゴンを含みます。ポイントは円形のバッファを作成します。\nLEFT / RIGHT—ラインのみ有効で、ラインの位相的な左側 / 右側にバッファを作成します。\nOUTSIDE_ONLY—ポリゴンのみ有効で、入力ポリゴンの外側だけにバッファを作成し、重なりは除去されます。"
        }
        "buffer.help.end" => {
            "ROUND—ラインバッファの端点は半円形になり、入力ラインの端点からバッファ距離だけ離れます。\nFLAT—ラインバッファの端点は平坦で、入力ラインの端点で止まります。\nSQUARE—ラインバッファの端点は方形になり、入力ラインの方向に沿ってバッファ距離だけ延長されます。"
        }
        "buffer.help.dissolve" => {
            "NONE—重なりに関係なく、各フィーチャが独立したバッファを保持します。これが既定です。\nALL—すべてのバッファを単一のフィーチャに融合し、重なりを除去します。\nLIST—一覧のフィールド（入力フィーチャから引き継がれる）の値が同じバッファを融合します。"
        }
        "buffer.help.dissolveFields" => {
            "融合に使用する入力フィーチャのフィールド。融合タイプが LIST の場合のみ有効です。"
        }
        "buffer.status.running" => "バッファを計算しています…",
        "buffer.status.done" => "バッファ分析が完了しました：{0}",
        "buffer.error.noInput" => "入力フィーチャレイヤを選択してください。",
        "buffer.error.badDistance" => "距離は 0 以外の数値である必要があります。",
        "buffer.error.noOutput" => "出力フィーチャクラスのパスを入力してください。",
        "buffer.error.busy" => "別のバックグラウンド処理が実行中です。しばらくお待ちください。",
        "buffer.error.noDissolveField" => {
            "ディゾルブ タイプが LIST の場合は、ディゾルブ フィールドを 1 つ以上選択してください。"
        }
        // ── ラスタレイヤ（v0.3） ──
        "layers.rasterOpening" => "ラスタを開いています…",
        "layers.properties" => "プロパティ…",
        "raster.noGeoReference" => {
            "この画像には地理参照がないため、ピクセル座標で表示しています。詳細は「レイヤプロパティ → 範囲」を参照してください。"
        }
        "dialog.addRasterTitle" => "ラスタデータの追加",
        "dialog.rasterProps" => "レイヤプロパティ - {0}",
        "dialog.pyramidTitle" => "ピラミッドの構築",
        "dialog.pyramidBody" => {
            "{0}（{1}）はサイズが大きく、ピラミッド/オーバービューがありません。このままでは表示が遅くなるため、今オーバービューを構築しますか？"
        }
        "dialog.pyramidHint" => {
            "構築はバックグラウンドで実行されるため、そのまま操作を続けられます。結果は同ディレクトリの .ovr ファイル（QGIS / ArcGIS と同じ概覧形式）に書き出され、削除すると容量を解放できます。"
        }
        "dialog.yes" => "はい",
        "dialog.no" => "いいえ",
        "status.rasterOpening" => "ラスタファイルを開いています…",
        "status.rasterAdded" => "ラスタレイヤ {0} を追加しました（{1}、{2} バンド、{3}）",
        "status.rasterStats" => "統計を計算中：{0}（{1}）",
        "status.rasterPyramid" => "ピラミッドを構築中：{0}（{1}）",
        "status.rasterStatsDone" => "統計が完了しました：{0} バンド",
        "status.rasterPyramidDone" => "ピラミッド構築完了：{0} レベル（.ovr に書き込みました）",
        "status.rasterJobError" => "ラスタのバックグラウンド処理に失敗しました：{0}",
        // 右下の通知
        "toast.layerAdded" => "レイヤーを追加しました",
        "toast.rasterAdded" => "ラスタレイヤーを追加しました",
        "toast.dropTitle" => "ファイルを追加できません",
        "toast.dropUnsupported" => {
            "未対応のファイル形式です：{0}（.geojson / .json / .shp / .tif / .tiff、または Sentinel-2 .SAFE フォルダーをドロップしてください）"
        },
        "mapview.dropHint" => "マウスを離すとレイヤーとして追加します",
        "status.openedWithWarnings" => "プロジェクト {0} を開きました（警告あり）",
        "props.tab.general" => "一般",
        "props.tab.source" => "ソース",
        "props.tab.extent" => "範囲",
        "props.tab.display" => "表示",
        "props.tab.symbology" => "シンボル",
        "props.tab.time" => "時間",
        "props.ok" => "OK",
        "props.cancel" => "キャンセル",
        "props.apply" => "適用",
        "props.name" => "名前",
        "props.layerType" => "レイヤ種別",
        "props.typeRaster" => "ラスタレイヤ",
        "props.sourcePath" => "ソース",
        "props.visible" => "表示",
        "props.show" => "表示",
        "props.width" => "幅",
        "props.bands" => "バンド数",
        "props.dataType" => "ピクセル種別",
        "props.compression" => "圧縮",
        "props.block" => "ブロック",
        "props.fileSize" => "ファイルサイズ",
        "props.bigTiff" => "BigTIFF",
        "props.overviews" => "オーバービュー",
        "props.nodata" => "NoData",
        "props.photometric" => "フォトメトリック",
        "props.bandInfo" => "バンド情報",
        "props.bitsFormat" => "{0}（{1} ビット）",
        "props.bandLayout" => "{0}（{1}）",
        "props.layoutPlanar" => "バンド分離（Planar）",
        "props.layoutChunky" => "ピクセル交互（Chunky）",
        "props.blockFormat" => "{0}：{1} × {2}（{3} × {4} ブロック、計 {5}）",
        "props.overviewFormat" => "内部 {0} / 外部 {1}（計 {2} レベル）",
        "props.bandLine" => "#{0} {1}（{2}、{3} ビット、NoData {4}）",
        "props.none" => "なし",
        "props.yes" => "はい",
        "props.no" => "いいえ",
        "props.extentX" => "範囲 X",
        "props.extentY" => "範囲 Y",
        "props.pixelSize" => "セルサイズ",
        "props.crs" => "座標系",
        "props.gcp" => "グラウンドコントロールポイント",
        "props.warning" => "警告",
        "props.resampling" => "リサンプリング",
        "props.nearest" => "最近傍",
        "props.resamplingHint" => {
            "表示縮尺に応じてピラミッドレベルを自動選択し、最近傍でサンプリングします。"
        }
        "props.styleStretch" => "ストレッチ",
        "props.styleRgb" => "RGB 合成",
        "props.styleRgbTitle" => "RGB 合成でラスタを描画",
        "props.channel" => "チャンネル",
        "props.band" => "バンド",
        "props.red" => "赤",
        "props.green" => "緑",
        "props.blue" => "青",
        "props.alpha" => "アルファ",
        "props.background" => "背景値の表示",
        "props.nodataAs" => "NoData の表示",
        "props.transparent" => "透明",
        "props.stretchType" => "ストレッチの種類",
        "props.stretchNone" => "なし",
        "props.stretchMinMax" => "最小-最大",
        "props.stretchPercent" => "パーセントクリップ",
        "props.stretchStdDev" => "標準偏差",
        "props.percentClip" => "パーセントクリップ",
        "props.min" => "最小値",
        "props.max" => "最大値",
        "props.invert" => "反転",
        "props.gamma" => "ガンマ",
        "props.stretchHint" => "ストレッチは表示のみに適用され、元のピクセルは変更されません。",
        "props.statistics" => "統計",
        "props.statsDataset" => "統計データセット",
        "props.computeStats" => "統計の計算",
        "props.statsLine" => "最小 {0}  最大 {1}  平均 {2}  標準偏差 {3}",
        "props.statsFromFile" => "（ファイル内）",
        "props.statsEstimate" => "（推定）",
        "props.statsMissing" => "統計はまだ計算されていません。",
        "props.metadata" => "メタデータ項目",
        "props.timeNone" => "このレイヤには時間情報がありません。",
        _ => key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_covers_all_keys() {
        // key 清单：任何一门语言未收录的 key 都会原样返回，
        // 因此通过“返回文本 != key”来校验收录完整性。
        let keys = [
            "menu.file",
            "menu.edit",
            "menu.view",
            "menu.bookmarks",
            "menu.insert",
            "menu.select",
            "menu.geoprocessing",
            "menu.customize",
            "menu.window",
            "menu.help",
            "file.new",
            "file.open",
            "file.save",
            "file.saveAs",
            "file.pageSetup",
            "file.print",
            "file.exit",
            "window.untitled",
            "project.openTitle",
            "project.saveAsTitle",
            "project.fileFilter",
            "project.savePromptTitle",
            "project.savePromptBody",
            "edit.undo",
            "edit.redo",
            "edit.cut",
            "edit.copy",
            "edit.paste",
            "edit.selectAll",
            "edit.properties",
            "view.zoomIn",
            "view.zoomOut",
            "view.pan",
            "view.zoomFull",
            "view.dataFrame",
            "view.layout",
            "view.basemap",
            "view.bookmarks",
            "bookmark.add",
            "bookmark.manager",
            "insert.text",
            "insert.picture",
            "insert.grid",
            "insert.scalebar",
            "insert.legend",
            "select.byLocation",
            "select.byAttribute",
            "select.bySquare",
            "select.invert",
            "select.deselect",
            "window.contents",
            "window.find",
            "window.catalog",
            "window.arrange",
            "window.close",
            "layer.add",
            "layer.addRaster",
            "layer.addSafe",
            "layer.order",
            "layer.properties",
            "layer.display",
            "data.connect",
            "data.online",
            "data.import",
            "data.export",
            "data.table",
            "data.manage",
            "overlay.buffer",
            "overlay.clip.menu",
            "overlay.intersect.menu",
            "overlay.union.menu",
            "overlay.merge.menu",
            "overlay.dissolve.menu",
            "overlay.clip",
            "overlay.intersect",
            "overlay.union",
            "overlay.merge",
            "overlay.dissolve",
            "geo.windowTitle",
            "geo.tool",
            "geo.input",
            "geo.output",
            "geo.fields",
            "geo.orderClip",
            "geo.orderMulti",
            "geo.orderSingle",
            "geo.dissolveAll",
            "geo.status.running",
            "geo.status.done",
            "geo.error.fewInputs",
            "geo.help.tool.clip",
            "geo.help.tool.intersect",
            "geo.help.tool.union",
            "geo.help.tool.merge",
            "geo.help.tool.dissolve",
            "geo.help.inputs",
            "geo.help.output",
            "geo.help.fields",
            "analysis.siting",
            "analysis.network",
            "analysis.raster",
            "analysis.stats",
            "analysis.3d",
            "map.symbology",
            "map.labels",
            "map.legend",
            "map.scale",
            "map.properties",
            "tools.geoprocessing",
            "tools.modelBuilder",
            "tools.toolbox",
            "tools.options",
            "customize.toolbars",
            "help.help",
            "help.docs",
            "help.language",
            "help.about",
            "status.ready",
            "status.meta",
            "status.dev",
            "status.coords",
            "projection.none",
            "projection.geographic",
            "projection.projected",
            "projection.unknown",
            "crs.notSet",
            "crs.notIdentified",
            "status.added",
            "status.loading",
            "status.loadError",
            "status.loadAborted",
            "status.newProject",
            "status.saving",
            "status.saved",
            "status.saveError",
            "status.saveAborted",
            "status.opening",
            "status.opened",
            "mapview.title",
            "mapview.desc1",
            "mapview.desc2",
            "layers.panel",
            "layers.root",
            "layers.zoomToLayer",
            "layers.zoomToGroup",
            "layers.add",
            "layers.addTip",
            "layers.newGroupTip",
            "layers.newGroupName",
            "layers.newGroupHere",
            "layers.expandAllTip",
            "layers.collapseAllTip",
            "layers.moveToTop",
            "layers.empty",
            "layers.remove",
            "layers.show",
            "layers.hide",
            "layers.loading",
            "layers.parsing",
            "dialog.developTitle",
            "dialog.developHeading",
            "dialog.developBody",
            "dialog.ok",
            "dialog.save",
            "dialog.dontSave",
            "dialog.cancel",
            "dialog.errorTitle",
            "dialog.addTitle",
            "dialog.addBody",
            "dialog.aboutTitle",
            "dialog.aboutSubtitle",
            "dialog.aboutTagline",
            "dialog.aboutLicense",
            "dialog.about.font",
            "buffer.title",
            "buffer.input",
            "buffer.output",
            "buffer.distance",
            "buffer.distance.linear",
            "buffer.distance.field",
            "buffer.side",
            "buffer.end",
            "buffer.dissolve",
            "buffer.dissolveFields",
            "buffer.selectAll",
            "buffer.deselectAll",
            "buffer.addField",
            "buffer.environments",
            "buffer.hideHelp",
            "buffer.showHelp",
            "buffer.toolHelp",
            "buffer.noField",
            "buffer.noLayer",
            "buffer.browseOutput",
            "buffer.defaultOutput",
            "buffer.addFieldTip",
            "buffer.side.full",
            "buffer.side.left",
            "buffer.side.right",
            "buffer.side.outside",
            "buffer.end.round",
            "buffer.end.flat",
            "buffer.end.square",
            "buffer.dissolve.none",
            "buffer.dissolve.all",
            "buffer.dissolve.list",
            "buffer.unit.meters",
            "buffer.unit.kilometers",
            "buffer.unit.feet",
            "buffer.unit.miles",
            "buffer.unit.yards",
            "buffer.unit.nauticalMiles",
            "buffer.unit.degrees",
            "buffer.help.input",
            "buffer.help.output",
            "buffer.help.distance",
            "buffer.help.side",
            "buffer.help.end",
            "buffer.help.dissolve",
            "buffer.help.dissolveFields",
            "buffer.status.running",
            "buffer.status.done",
            "buffer.error.noInput",
            "buffer.error.badDistance",
            "buffer.error.noOutput",
            "buffer.error.busy",
            "buffer.error.noDissolveField",
            // 栅格图层（v0.3）
            "layers.rasterOpening",
            "layers.properties",
            "raster.noGeoReference",
            "dialog.addRasterTitle",
            "dialog.rasterProps",
            "dialog.pyramidTitle",
            "dialog.pyramidBody",
            "dialog.pyramidHint",
            "dialog.yes",
            "dialog.no",
            "status.rasterOpening",
            "status.rasterAdded",
            "status.rasterStats",
            "status.rasterPyramid",
            "status.rasterStatsDone",
            "status.rasterPyramidDone",
            "status.rasterJobError",
            "toast.layerAdded",
            "toast.rasterAdded",
            "toast.dropTitle",
            "toast.dropUnsupported",
            "mapview.dropHint",
            "status.openedWithWarnings",
            "props.tab.general",
            "props.tab.source",
            "props.tab.extent",
            "props.tab.display",
            "props.tab.symbology",
            "props.tab.time",
            "props.ok",
            "props.cancel",
            "props.apply",
            "props.name",
            "props.layerType",
            "props.typeRaster",
            "props.sourcePath",
            "props.visible",
            "props.show",
            "props.width",
            "props.bands",
            "props.dataType",
            "props.compression",
            "props.block",
            "props.fileSize",
            "props.bigTiff",
            "props.overviews",
            "props.nodata",
            "props.photometric",
            "props.bandInfo",
            "props.bitsFormat",
            "props.bandLayout",
            "props.layoutPlanar",
            "props.layoutChunky",
            "props.blockFormat",
            "props.overviewFormat",
            "props.bandLine",
            "props.none",
            "props.yes",
            "props.no",
            "props.extentX",
            "props.extentY",
            "props.pixelSize",
            "props.crs",
            "props.gcp",
            "props.warning",
            "props.resampling",
            "props.nearest",
            "props.resamplingHint",
            "props.styleStretch",
            "props.styleRgb",
            "props.styleRgbTitle",
            "props.channel",
            "props.band",
            "props.red",
            "props.green",
            "props.blue",
            "props.alpha",
            "props.background",
            "props.nodataAs",
            "props.transparent",
            "props.stretchType",
            "props.stretchNone",
            "props.stretchMinMax",
            "props.stretchPercent",
            "props.stretchStdDev",
            "props.percentClip",
            "props.min",
            "props.max",
            "props.invert",
            "props.gamma",
            "props.stretchHint",
            "props.statistics",
            "props.statsDataset",
            "props.computeStats",
            "props.statsLine",
            "props.statsFromFile",
            "props.statsEstimate",
            "props.statsMissing",
            "props.metadata",
            "props.timeNone",
        ];
        for lang in Language::ALL {
            for key in keys {
                assert_ne!(
                    lang.t(key),
                    key,
                    "language {lang:?} is missing translation for {key:?}"
                );
            }
        }
    }

    #[test]
    fn placeholder_replacement() {
        assert_eq!(
            Language::Chinese.tf("dialog.developTitle", &["缓冲区分析..."]),
            "功能开发中 — 缓冲区分析..."
        );
        assert_eq!(
            Language::English.tf("status.dev", &["Save"]),
            "\"Save\" is under development"
        );
        assert_eq!(
            Language::Chinese.tf("status.added", &["roads", "12"]),
            "已添加「roads」（12 个要素）"
        );
    }
}
