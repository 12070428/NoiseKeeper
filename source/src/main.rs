#![cfg_attr(windows, windows_subsystem = "windows")]
mod config;
mod dsp;
mod engine;
mod report;
mod wav;
#[cfg(windows)]
mod win;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use config::Config;
use engine::{Engine, Status};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub fn base_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf())).unwrap_or_else(|| ".".into())
}

/// WAVをマイク入力の代わりに流して全処理を実行する検証用モード。
/// 使い方: noisekeeper --simulate in.wav --out dir [--start 2026-11-05T23:00:00] [--manual-at 秒]
fn simulate(args: &[String]) -> Result<(), String> {
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let input = get("--simulate").ok_or("--simulate が必要です")?;
    let out = PathBuf::from(get("--out").unwrap_or_else(|| "sim_out".into()));
    let cfg = match get("--config") { Some(p) => Config::load_or_create(p.as_ref())?, None => Config::default() };
    let start: DateTime<Local> = match get("--start") {
        Some(s) => {
            let n = NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S").map_err(|e| e.to_string())?;
            Local.from_local_datetime(&n).single().ok_or("時刻が不正です")?
        }
        None => Local::now(),
    };
    let manual_at: Option<f64> = get("--manual-at").and_then(|s| s.parse().ok());
    let (sr, samples) = wav::read_mono(input.as_ref())?;
    let days = cfg.general.report_days;
    let status = Arc::new(Mutex::new(Status::default()));
    let mut e = Engine::new(cfg, out.clone(), sr, status, Box::new(|_| None));
    let block = (sr / 10) as usize;
    let start_ms = start.timestamp_millis();
    let t0 = std::time::Instant::now();
    let mut done = 0usize;
    let mut manual = manual_at.map(|s| (s * sr as f64) as usize);
    for c in samples.chunks(block) {
        done += c.len();
        e.feed(c, start_ms + done as i64 * 1000 / sr as i64);
        if manual.is_some_and(|m| done >= m) { e.manual_save(); manual = None; }
    }
    let spent = t0.elapsed().as_secs_f64();
    e.shutdown();
    report::generate(&out, days.max(3650))?;
    println!("processed {:.1}s of audio in {:.3}s ({:.3}% of one core)", done as f64 / sr as f64, spent, spent / (done as f64 / sr as f64) * 100.0);
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--simulate") {
        if let Err(e) = simulate(&args) { eprintln!("error: {e}"); std::process::exit(1); }
        return;
    }
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    eprintln!("このビルドでは --simulate のみ使用できます");
}
