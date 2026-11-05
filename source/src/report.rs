//! ログを集計して、ブラウザで開ける report.html を生成する。外部通信は行わない。
use chrono::{Duration, Local};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("report.html");

pub fn generate(out: &Path, days: u32) -> Result<PathBuf, String> {
    let (mut minutes, mut events, mut starts) = (Vec::new(), Vec::new(), Vec::new());
    let mut names: Vec<_> = std::fs::read_dir(out.join("logs")).map(|d| {
        d.flatten().map(|f| f.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".jsonl")).collect()
    }).unwrap_or_default();
    names.sort();
    let oldest = (Local::now() - Duration::days(days as i64)).format("%Y-%m-%d").to_string();
    let newest = names.last().cloned().unwrap_or_default();
    let keep = |n: &String| *n >= oldest || newest.get(..10).is_some_and(|x| n.as_str() >= x);
    for n in names.iter().filter(|n| keep(n)) {
        let Ok(text) = std::fs::read_to_string(out.join("logs").join(n)) else { continue };
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue }; // 電源断で壊れた行は無視
            match v["type"].as_str() {
                Some("minute") => minutes.push(json!([v["t"], v["avg"], v["peak"], v["floor"]])),
                Some("event") => events.push(v),
                Some("start") => starts.push(v["t"].clone()),
                _ => {}
            }
        }
    }
    let data = json!({"generated": Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(), "minutes": minutes, "events": events, "starts": starts});
    let safe = data.to_string().replace("</", "<\\/");
    let html = TEMPLATE.replace("/*__DATA__*/null", &safe);
    let path = out.join("report.html");
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    std::fs::write(&path, html).map_err(|e| e.to_string())?;
    Ok(path)
}
