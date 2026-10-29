//! GDAL PAM（`.aux.xml`）统计持久化：与 ArcGIS「CalculateStatistics」写出的
//! `mosaic.tif.aux.xml`（PAMDataset / PAMRasterBand / STATISTICS_* MDI / HistItem）
//! 完全同一格式，双向兼容——本程序写的统计 ArcGIS / QGIS / GDAL 直接可读，
//! 它们写的统计本程序也能复用，避免重复计算。
//!
//! ```xml
//! <PAMDataset>
//!   <PAMRasterBand band="1">
//!     <Histograms><HistItem>
//!       <HistMin>190</HistMin><HistMax>2973</HistMax><BucketCount>256</BucketCount>
//!       <IncludeOutOfRange>1</IncludeOutOfRange><Approximate>0</Approximate>
//!       <HistCounts>56|333|…</HistCounts>
//!     </HistItem></Histograms>
//!     <Metadata>
//!       <MDI key="STATISTICS_MINIMUM">190</MDI>
//!       <MDI key="STATISTICS_MAXIMUM">2973</MDI>
//!       <MDI key="STATISTICS_MEAN">934.05</MDI>
//!       <MDI key="STATISTICS_STDDEV">492.89</MDI>
//!     </Metadata>
//!   </PAMRasterBand>
//! </PAMDataset>
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::reader::Reader;
use quick_xml::Writer;

use super::BandStatistics;

/// `foo.tif` → `foo.tif.aux.xml`（GDAL PAM 约定）。
pub fn aux_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".aux.xml");
    PathBuf::from(name)
}

/// 从同名 `.aux.xml` 读取各波段统计值（没有文件 / 解析失败返回全 `None`）。
///
/// 坏文件按"没有统计"处理：统计丢了可以重算，绝不能因为附属文件损坏
/// 而打不开影像。
pub fn load_statistics(path: &Path, band_count: usize) -> Vec<Option<BandStatistics>> {
    let mut out: Vec<Option<BandStatistics>> = vec![None; band_count];
    let text = match fs::read_to_string(aux_path(path)) {
        Ok(text) => text,
        Err(_) => return out,
    };
    if let Ok(bands) = parse_pam(&text) {
        for (index, stats) in bands {
            if index < band_count {
                out[index] = Some(stats);
            }
        }
    }
    out
}

/// 把一个波段的统计值写入（或合并进）`.aux.xml`。
///
/// 已有文件时做**事件级合并**：只替换该波段的 `STATISTICS_*` MDI 与
/// `<Histograms>` 子树，其余内容（描述、色表等）原样保留；文件不存在或
/// 不是合法 XML 时整体重建。写盘失败静默忽略——统计本身仍在内存里。
pub fn save_statistics(
    path: &Path,
    band: usize,
    stats: &BandStatistics,
    valid_percent: Option<f64>,
) {
    let aux = aux_path(path);
    let existing = fs::read_to_string(&aux).ok();
    let document = match existing {
        Some(text) => merge_into(&text, band + 1, stats, valid_percent)
            .unwrap_or_else(|_| fresh_document(band + 1, stats, valid_percent)),
        None => fresh_document(band + 1, stats, valid_percent),
    };
    let _ = fs::write(&aux, document);
}

// ---------------------------------------------------------------------------
// 写出
// ---------------------------------------------------------------------------

/// 浮点数的 XML 文本形式（`{}` 对 f64 输出最短往返表示，GDAL 同风格）。
fn num(value: f64) -> String {
    format!("{value}")
}

/// `<Metadata>` 块里的键值对。
fn statistics_pairs(stats: &BandStatistics, valid_percent: Option<f64>) -> Vec<(&str, String)> {
    let mut pairs = vec![
        ("STATISTICS_MINIMUM", num(stats.min)),
        ("STATISTICS_MAXIMUM", num(stats.max)),
        ("STATISTICS_MEAN", num(stats.mean)),
        ("STATISTICS_STDDEV", num(stats.std_dev)),
    ];
    if let Some(percent) = valid_percent {
        // 抖掉浮点噪声：GDAL 写 "100" 而不是 "100.000000"。
        let rounded = (percent * 10_000.0).round() / 10_000.0;
        pairs.push(("STATISTICS_VALID_PERCENT", num(rounded)));
    }
    pairs.push(("STATISTICS_SKIPFACTORX", "1".to_string()));
    pairs.push(("STATISTICS_SKIPFACTORY", "1".to_string()));
    pairs
}

/// 直方图是否存在（常量影像没有直方图）。
fn has_histogram(stats: &BandStatistics) -> bool {
    !stats.histogram.is_empty() && stats.max > stats.min
}

/// 事件级合并：把 `band_no`（1 起始）波段的统计与直方图写进已有 PAM 文本。
fn merge_into(
    existing: &str,
    band_no: usize,
    stats: &BandStatistics,
    valid_percent: Option<f64>,
) -> quick_xml::Result<String> {
    let mut reader = Reader::from_str(existing);
    reader.config_mut().trim_text(true);
    let mut out = Vec::with_capacity(existing.len() + 8192);
    {
        let mut writer = Writer::new(&mut out);
        let mut editing = false; // 当前处于目标波段的 PAMRasterBand 内
        let mut in_metadata = false; // 目标波段内正处于 <Metadata> 里
        let mut skip_depth = 0usize; // >0：正在丢弃某个子树
        let mut metadata_written = false; // 目标波段已写入统计 MDI
        let mut histogram_written = false; // 目标波段已写入直方图

        loop {
            match reader.read_event()? {
                Event::Start(e) => {
                    if skip_depth > 0 {
                        skip_depth += 1;
                        continue;
                    }
                    let qname = e.name();
                    let name = qname.as_ref();
                    if editing {
                        match name {
                            b"Metadata" => in_metadata = true,
                            b"Histograms" if has_histogram(stats) => {
                                // 用自己的直方图整体替换原有子树。
                                emit_histogram(&mut writer, stats)?;
                                histogram_written = true;
                                skip_depth = 1;
                                continue;
                            }
                            b"MDI" if in_metadata && is_statistics_key(&e) => {
                                // 丢弃旧的 STATISTICS_*，稍后在 </Metadata> 前补新值。
                                skip_depth = 1;
                                continue;
                            }
                            _ => {}
                        }
                    } else if name == b"PAMRasterBand" && band_attr(&e) == Some(band_no) {
                        editing = true;
                        metadata_written = false;
                        histogram_written = false;
                    }
                    writer.write_event(Event::Start(e))?;
                }
                Event::Empty(e) => {
                    if skip_depth > 0 {
                        continue;
                    }
                    let qname = e.name();
                    let name = qname.as_ref();
                    if editing
                        && name == b"MDI"
                        && in_metadata
                        && is_statistics_key(&e)
                    {
                        continue; // 丢弃自闭合的旧统计键
                    }
                    if editing && name == b"Histograms" && has_histogram(stats) {
                        emit_histogram(&mut writer, stats)?;
                        histogram_written = true;
                        continue;
                    }
                    if name == b"PAMRasterBand" && band_attr(&e) == Some(band_no) {
                        // 自闭合波段元素（异常写法）：直接展开写完整内容。
                        emit_histogram(&mut writer, stats)?;
                        emit_metadata(&mut writer, stats, valid_percent)?;
                        metadata_written = true;
                        histogram_written = true;
                        continue;
                    }
                    writer.write_event(Event::Empty(e))?;
                }
                Event::Text(t) => {
                    if skip_depth == 0 {
                        writer.write_event(Event::Text(t))?;
                    }
                }
                Event::End(e) => {
                    if skip_depth > 0 {
                        skip_depth -= 1;
                        continue;
                    }
                    let qname = e.name();
                    let name = qname.as_ref();
                    if editing && name == b"Metadata" && in_metadata {
                        // 关闭前补上新的 STATISTICS_*（旧的已丢弃）。
                        emit_metadata(&mut writer, stats, valid_percent)?;
                        metadata_written = true;
                        in_metadata = false;
                    }
                    if editing && name == b"PAMRasterBand" {
                        // 目标波段没有 Metadata / Histograms 子元素时补齐。
                        if !histogram_written && has_histogram(stats) {
                            emit_histogram(&mut writer, stats)?;
                            histogram_written = true;
                        }
                        if !metadata_written {
                            emit_metadata(&mut writer, stats, valid_percent)?;
                            metadata_written = true;
                        }
                        editing = false;
                    }
                    writer.write_event(Event::End(e))?;
                }
                Event::Eof => break,
                _ => {}
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// 全新构建一个只含目标波段统计的 PAM 文档。
fn fresh_document(band_no: usize, stats: &BandStatistics, valid_percent: Option<f64>) -> String {
    let mut out = Vec::with_capacity(2048);
    {
        let mut writer = Writer::new(&mut out);
        let run = |writer: &mut Writer<&mut Vec<u8>>| -> quick_xml::Result<()> {
            writer.write_event(Event::Text(BytesText::from_escaped("\n")))?;
            let mut band = BytesStart::new("PAMRasterBand");
            let no = band_no.to_string();
            band.push_attribute(("band", no.as_str()));
            writer.write_event(Event::Start(band))?;
            if has_histogram(stats) {
                emit_histogram(writer, stats)?;
            }
            emit_metadata(writer, stats, valid_percent)?;
            writer.write_event(Event::Text(BytesText::from_escaped("\n")))?;
            writer.write_event(Event::End(BytesEnd::new("PAMRasterBand")))?;
            writer.write_event(Event::Text(BytesText::from_escaped("\n")))?;
            Ok(())
        };
        if run(&mut writer).is_err() {
            return String::new();
        }
    }
    let mut xml = String::from("<PAMDataset>");
    xml.push_str(&String::from_utf8_lossy(&out));
    xml.push_str("</PAMDataset>\n");
    xml
}

/// 写出 `<Histograms><HistItem>…</HistItem></Histograms>`。
fn emit_histogram<W: std::io::Write>(
    writer: &mut Writer<W>,
    stats: &BandStatistics,
) -> quick_xml::Result<()> {
    let counts = stats
        .histogram
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join("|");
    writer.write_event(Event::Start(BytesStart::new("Histograms")))?;
    writer.write_event(Event::Start(BytesStart::new("HistItem")))?;
    let item: [(&str, String); 6] = [
        ("HistMin", num(stats.min)),
        ("HistMax", num(stats.max)),
        ("BucketCount", stats.histogram.len().to_string()),
        ("IncludeOutOfRange", "1".to_string()),
        ("Approximate", if stats.estimated { "1" } else { "0" }.to_string()),
        ("HistCounts", counts),
    ];
    for (tag, value) in item {
        writer.write_event(Event::Start(BytesStart::new(tag)))?;
        writer.write_event(Event::Text(BytesText::new(&value)))?;
        writer.write_event(Event::End(BytesEnd::new(tag)))?;
    }
    writer.write_event(Event::End(BytesEnd::new("HistItem")))?;
    writer.write_event(Event::End(BytesEnd::new("Histograms")))?;
    Ok(())
}

/// 写出 `<Metadata>` 块（STATISTICS_* MDI）。
fn emit_metadata<W: std::io::Write>(
    writer: &mut Writer<W>,
    stats: &BandStatistics,
    valid_percent: Option<f64>,
) -> quick_xml::Result<()> {
    writer.write_event(Event::Start(BytesStart::new("Metadata")))?;
    for (key, value) in statistics_pairs(stats, valid_percent) {
        let mut mdi = BytesStart::new("MDI");
        mdi.push_attribute(("key", key));
        writer.write_event(Event::Start(mdi))?;
        writer.write_event(Event::Text(BytesText::new(&value)))?;
        writer.write_event(Event::End(BytesEnd::new("MDI")))?;
    }
    writer.write_event(Event::End(BytesEnd::new("Metadata")))?;
    Ok(())
}

/// MDI 的 key 是否是统计键（合并时旧键一律让位于新计算的值）。
fn is_statistics_key(element: &BytesStart) -> bool {
    element
        .try_get_attribute("key")
        .ok()
        .flatten()
        .and_then(|attr| std::str::from_utf8(&attr.value).ok().map(str::to_string))
        .is_some_and(|key| key.starts_with("STATISTICS_"))
}

/// PAMRasterBand 的 band 属性（1 起始；缺失 / 非法返回 `None`）。
fn band_attr(element: &BytesStart) -> Option<usize> {
    element
        .try_get_attribute("band")
        .ok()
        .flatten()
        .and_then(|attr| std::str::from_utf8(&attr.value).ok()?.parse::<usize>().ok())
}

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

/// 单波段从 PAM 里读到的原始字段。
#[derive(Default)]
struct PamBand {
    min: Option<f64>,
    max: Option<f64>,
    mean: Option<f64>,
    std_dev: Option<f64>,
    hist_min: Option<f64>,
    hist_max: Option<f64>,
    approximate: bool,
    counts: Vec<u32>,
    counts_seen: bool,
}

/// 解析 PAM XML，返回（0 起始波段号，统计值）列表。
fn parse_pam(text: &str) -> quick_xml::Result<Vec<(usize, BandStatistics)>> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut band: Option<(usize, PamBand)> = None;
    let mut mdi_key: Option<String> = None;
    let mut text_target: Option<&'static str> = None;
    let mut mdi_value = String::new();
    let mut counts_text = String::new();

    macro_rules! take_band {
        ($band:expr) => {
            if let Some((index, parsed)) = $band.take() {
                if let Some(stats) = finish_band(parsed) {
                    out.push((index, stats));
                }
            }
        };
    }

    loop {
        match reader.read_event()? {
            Event::Start(e) => match e.name().as_ref() {
                b"PAMRasterBand" => {
                    take_band!(band);
                    if let Some(no) = band_attr(&e) {
                        band = Some((no.saturating_sub(1), PamBand::default()));
                    }
                }
                b"HistItem" => {
                    if let Some((_, parsed)) = band.as_mut() {
                        *parsed = PamBand {
                            // HistItem 不携带统计值：只重置直方图字段。
                            min: parsed.min,
                            max: parsed.max,
                            mean: parsed.mean,
                            std_dev: parsed.std_dev,
                            ..PamBand::default()
                        };
                    }
                }
                b"MDI" => {
                    mdi_key = attribute_value(&e, "key");
                    mdi_value.clear();
                    text_target = Some("MDI");
                }
                b"HistMin" => text_target = Some("HistMin"),
                b"HistMax" => text_target = Some("HistMax"),
                b"Approximate" => text_target = Some("Approximate"),
                b"HistCounts" => {
                    counts_text.clear();
                    text_target = Some("HistCounts");
                }
                _ => {}
            },
            Event::Empty(e) => {
                // 自闭合元素（如 `<MDI key="…"></MDI>` 的空值写法）。
                if e.name().as_ref() == b"MDI" {
                    if let Some(key) = attribute_value(&e, "key") {
                        if let Some((_, parsed)) = band.as_mut() {
                            apply_mdi(parsed, &key, "");
                        }
                    }
                }
            }
            Event::Text(t) => {
                let value = t.unescape().unwrap_or_default();
                match text_target {
                    Some("MDI") => mdi_value.push_str(&value),
                    Some("HistCounts") => counts_text.push_str(&value),
                    Some("HistMin") => {
                        if let Some((_, parsed)) = band.as_mut() {
                            parsed.hist_min = value.trim().parse().ok();
                        }
                    }
                    Some("HistMax") => {
                        if let Some((_, parsed)) = band.as_mut() {
                            parsed.hist_max = value.trim().parse().ok();
                        }
                    }
                    Some("Approximate") => {
                        if let Some((_, parsed)) = band.as_mut() {
                            parsed.approximate = value.trim() == "1";
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) => match e.name().as_ref() {
                b"MDI" => {
                    text_target = None;
                    if let (Some((_, parsed)), Some(key)) = (band.as_mut(), mdi_key.take()) {
                        apply_mdi(parsed, &key, mdi_value.trim());
                    }
                }
                b"HistItem" => {
                    text_target = None;
                    if let Some((_, parsed)) = band.as_mut() {
                        parsed.counts = counts_text
                            .split('|')
                            .filter(|part| !part.is_empty())
                            .filter_map(|part| part.trim().parse().ok())
                            .collect();
                        parsed.counts_seen = true;
                    }
                }
                b"HistMin" | b"HistMax" | b"BucketCount" | b"IncludeOutOfRange"
                | b"Approximate" | b"HistCounts" => text_target = None,
                b"PAMRasterBand" => {
                    text_target = None;
                    take_band!(band);
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    take_band!(band);
    Ok(out)
}

/// 读取元素的字符串属性（属性值按 UTF-8 直取；统计键与波段号都是 ASCII）。
fn attribute_value(element: &BytesStart, name: &str) -> Option<String> {
    element
        .try_get_attribute(name)
        .ok()
        .flatten()
        .and_then(|attr| std::str::from_utf8(&attr.value).ok().map(str::to_string))
}

/// 把 MDI 键值写进当前波段的字段。
fn apply_mdi(band: &mut PamBand, key: &str, value: &str) {
    let parsed = value.parse::<f64>().ok();
    match key {
        "STATISTICS_MINIMUM" => band.min = parsed,
        "STATISTICS_MAXIMUM" => band.max = parsed,
        "STATISTICS_MEAN" => band.mean = parsed,
        "STATISTICS_STDDEV" => band.std_dev = parsed,
        _ => {}
    }
}

/// 字段 → [`BandStatistics`]；没有 min/max 的波段（如只写了描述）忽略。
fn finish_band(band: PamBand) -> Option<BandStatistics> {
    let min = band.min?;
    let max = band.max?;
    // 直方图桶覆盖范围与统计范围一致时才采用：分位值按 min..max 线性映射，
    // 范围对不上会得出错误的截断值，宁可丢弃让下次重算。
    let histogram = if band.counts_seen
        && !band.counts.is_empty()
        && band
            .hist_min
            .zip(band.hist_max)
            .is_some_and(|(lo, hi)| hi > lo && close(lo, min) && close(hi, max))
    {
        band.counts
    } else {
        Vec::new()
    };
    Some(BandStatistics {
        min,
        max,
        mean: band.mean.unwrap_or_else(|| (min + max) / 2.0),
        std_dev: band.std_dev.unwrap_or(0.0),
        valid_count: 0,
        from_file: true,
        estimated: band.approximate,
        histogram,
        percent: None,
    })
}

/// 相对误差 1e-6 内视为相等（直方图范围与统计范围的协调判断）。
fn close(a: f64, b: f64) -> bool {
    let scale = a.abs().max(b.abs()).max(1.0);
    (a - b).abs() / scale < 1e-6
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ArcGIS 10.8 CalculateStatistics 的真实输出样式（256 桶直方图 + 统计 MDI）。
    const ARC_CSSAMPLE: &str = r##"<PAMDataset>
  <PAMRasterBand band="1">
    <Histograms>
      <HistItem>
        <HistMin>190</HistMin>
        <HistMax>2973</HistMax>
        <BucketCount>4</BucketCount>
        <IncludeOutOfRange>1</IncludeOutOfRange>
        <Approximate>0</Approximate>
        <HistCounts>10|20|30|40</HistCounts>
      </HistItem>
    </Histograms>
    <Metadata>
      <MDI key="STATISTICS_COVARIANCES">242945.4921054287</MDI>
      <MDI key="STATISTICS_EXCLUDEDVALUES"></MDI>
      <MDI key="STATISTICS_MAXIMUM">2973</MDI>
      <MDI key="STATISTICS_MEAN">934.05641181734</MDI>
      <MDI key="STATISTICS_MINIMUM">190</MDI>
      <MDI key="STATISTICS_SKIPFACTORX">1</MDI>
      <MDI key="STATISTICS_SKIPFACTORY">1</MDI>
      <MDI key="STATISTICS_STDDEV">492.89501124015</MDI>
    </Metadata>
  </PAMRasterBand>
</PAMDataset>"##;

    fn sample_stats() -> BandStatistics {
        BandStatistics {
            min: 1.0,
            max: 9.0,
            mean: 5.0,
            std_dev: 2.0,
            valid_count: 16,
            from_file: false,
            estimated: false,
            histogram: vec![1, 2, 3, 4],
            percent: None,
        }
    }

    #[test]
    fn parses_arcgis_aux_xml() {
        let bands = parse_pam(ARC_CSSAMPLE).unwrap();
        assert_eq!(bands.len(), 1);
        let (index, stats) = &bands[0];
        assert_eq!(*index, 0);
        assert_eq!(stats.min, 190.0);
        assert_eq!(stats.max, 2973.0);
        assert!((stats.mean - 934.056_411_817_34).abs() < 1e-9);
        assert!((stats.std_dev - 492.895_011_240_15).abs() < 1e-9);
        assert!(stats.from_file);
        assert!(!stats.estimated);
        assert_eq!(stats.histogram, vec![10, 20, 30, 40]);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join("rustgis-pam-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.tif");
        let _ = std::fs::remove_file(aux_path(&path));
        save_statistics(&path, 0, &sample_stats(), Some(100.0));

        let loaded = load_statistics(&path, 1);
        let stats = loaded[0].as_ref().expect("应有统计值");
        assert_eq!(stats.min, 1.0);
        assert_eq!(stats.max, 9.0);
        assert_eq!(stats.mean, 5.0);
        assert_eq!(stats.std_dev, 2.0);
        assert_eq!(stats.histogram, vec![1, 2, 3, 4]);
        assert!(stats.from_file);
        let _ = std::fs::remove_file(aux_path(&path));
    }

    #[test]
    fn merge_preserves_foreign_content_and_replaces_statistics() {
        let existing = r##"<PAMDataset>
  <PAMRasterBand band="1">
    <Metadata>
      <MDI key="DESCRIPTION">地表高程</MDI>
      <MDI key="STATISTICS_MINIMUM">-1</MDI>
      <MDI key="STATISTICS_MAXIMUM">-1</MDI>
    </Metadata>
    <RasterAttributeTable><Row src="keep"/></RasterAttributeTable>
  </PAMRasterBand>
</PAMDataset>"##;
        let merged =
            merge_into(existing, 1, &sample_stats(), Some(100.0)).expect("合并应成功");
        // 新统计写进去了。
        assert!(merged.contains(r#"<MDI key="STATISTICS_MINIMUM">1</MDI>"#));
        assert!(merged.contains(r#"<MDI key="STATISTICS_MEAN">5</MDI>"#));
        // 旧统计被替换，外来键 / 外来元素保留。
        assert!(!merged.contains(r#"<MDI key="STATISTICS_MINIMUM">-1</MDI>"#));
        assert!(merged.contains(r#"<MDI key="DESCRIPTION">地表高程</MDI>"#));
        assert!(merged.contains("<Row src=\"keep\"/>"));
        // 直方图补进去了（原文件没有 Histograms 子元素）。
        assert!(merged.contains("<HistItem>"));
        let (index, stats) = &parse_pam(&merged).unwrap()[0];
        assert_eq!(*index, 0);
        assert_eq!(stats.histogram, vec![1, 2, 3, 4]);
    }

    #[test]
    fn merge_replaces_existing_histogram() {
        let merged = merge_into(ARC_CSSAMPLE, 1, &sample_stats(), None).unwrap();
        let (_, stats) = &parse_pam(&merged).unwrap()[0];
        assert_eq!(stats.min, 1.0);
        assert_eq!(stats.max, 9.0);
        assert_eq!(stats.histogram, vec![1, 2, 3, 4]);
        // ArcGIS 的旧直方图数据不得残留。
        assert!(!merged.contains("10|20|30|40"));
    }

    #[test]
    fn invalid_xml_falls_back_to_fresh_document() {
        let merged = merge_into("<not-xml", 1, &sample_stats(), None);
        assert!(merged.is_err() || parse_pam(&merged.unwrap()).is_ok());
        let fresh = fresh_document(1, &sample_stats(), Some(99.5));
        let (_, stats) = &parse_pam(&fresh).unwrap()[0];
        assert_eq!(stats.mean, 5.0);
        assert!(fresh.contains(r#"<MDI key="STATISTICS_VALID_PERCENT">99.5</MDI>"#));
    }

    #[test]
    fn missing_fields_get_defaults() {
        let text = r#"<PAMDataset><PAMRasterBand band="2">
            <Metadata><MDI key="STATISTICS_MINIMUM">10</MDI><MDI key="STATISTICS_MAXIMUM">20</MDI></Metadata>
        </PAMRasterBand></PAMDataset>"#;
        let bands = parse_pam(text).unwrap();
        assert_eq!(bands[0].0, 1);
        assert_eq!(bands[0].1.mean, 15.0);
        assert_eq!(bands[0].1.std_dev, 0.0);
        assert!(bands[0].1.histogram.is_empty());
    }

    #[test]
    fn histogram_with_mismatched_range_is_dropped() {
        let text = r#"<PAMDataset><PAMRasterBand band="1">
            <Histograms><HistItem>
                <HistMin>0</HistMin><HistMax>1000</HistMax>
                <BucketCount>2</BucketCount><IncludeOutOfRange>1</IncludeOutOfRange>
                <Approximate>0</Approximate><HistCounts>5|5</HistCounts>
            </HistItem></Histograms>
            <Metadata><MDI key="STATISTICS_MINIMUM">1</MDI><MDI key="STATISTICS_MAXIMUM">9</MDI></Metadata>
        </PAMRasterBand></PAMDataset>"#;
        let (_, stats) = &parse_pam(text).unwrap()[0];
        assert_eq!(stats.min, 1.0);
        assert!(stats.histogram.is_empty(), "范围对不上的直方图应被丢弃");
    }
}
