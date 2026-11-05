//! 録音エンジン本体。OSに依存しないため、WAVを流し込むテストで全機能を検証できる。
use crate::config::Config;
use crate::dsp::{classify, db, Analyzer, EventStats, Floor, Frame, FRAME_MS};
use crate::wav::WavWriter;
use chrono::{DateTime, Local};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// これより長く続く音は「持続音」とし、新しい環境音として扱う。
pub const CONTINUOUS_SECS: f32 = 60.0;
const RELEASE_MS: u32 = 400;
const HYSTERESIS_DB: f32 = 3.0;

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub level_db: f32, pub floor_db: f32, pub threshold_db: f32, pub recording: bool,
    pub events_today: u32, pub last_clip: String, pub error: String, pub mic: String,
}

/// 過去の音声を保持するリングバッファ。
pub struct Ring { buf: Vec<i16>, total: u64 }
impl Ring {
    pub fn new(cap: usize) -> Self { Self { buf: vec![0; cap.max(1)], total: 0 } }
    #[inline]
    pub fn push(&mut self, x: i16) {
        let n = self.buf.len() as u64;
        self.buf[(self.total % n) as usize] = x;
        self.total += 1;
    }
    pub fn total(&self) -> u64 { self.total }
    pub fn oldest(&self) -> u64 { self.total.saturating_sub(self.buf.len() as u64) }
    /// [from, to) を取り出す。すでに消えた範囲は詰めて返す。
    pub fn range(&self, from: u64, to: u64, out: &mut Vec<i16>) {
        out.clear();
        let (from, to) = (from.max(self.oldest()), to.min(self.total));
        let n = self.buf.len() as u64;
        out.extend((from..to).map(|i| self.buf[(i % n) as usize]));
    }
}

struct Active { start: u64, last_on: u64, stats: EventStats, over_frames: u32, confirmed: bool }
struct LongClip { wav: WavWriter, rel: String, start: u64, written: u64, end: u64 }

#[derive(Default)]
struct MinuteStat { key: i64, sum: f64, n: u32, peak: f32, floor: f32 }

pub struct Engine {
    cfg: Config, out: PathBuf, sr: u32, ring: Ring, an: Analyzer, floor: Floor, event: Option<Active>,
    clip: Option<LongClip>, t0: Option<(i64, u64)>, seq: u64, run_id: String, minute: MinuteStat,
    sec_sum: f64, sec_n: u32, scratch: Vec<i16>, last_tick: u64, clip_day: String, clips_today: u32,
    pub status: Arc<Mutex<Status>>, free_gb: Box<dyn Fn(&Path) -> Option<f64> + Send>, disk_warned: bool,
}

fn stamp(t: DateTime<Local>) -> String { t.format("%Y-%m-%dT%H:%M:%S").to_string() }
fn to_local(ms: i64) -> DateTime<Local> {
    DateTime::from_timestamp_millis(ms).unwrap_or_default().with_timezone(&Local)
}
fn r1(x: f32) -> f64 { (x as f64 * 10.0).round() / 10.0 }

impl Engine {
    pub fn new(cfg: Config, out: PathBuf, sr: u32, status: Arc<Mutex<Status>>, free_gb: Box<dyn Fn(&Path) -> Option<f64> + Send>) -> Self {
        let an = Analyzer::new(sr, cfg.noise.band_low_hz, cfg.noise.band_high_hz);
        Self {
            ring: Ring::new(((cfg.noise.pre_secs + 5) * sr) as usize), an, floor: Floor::new(), event: None, clip: None,
            t0: None, seq: 0, run_id: Local::now().format("%Y%m%d%H%M%S").to_string(), minute: MinuteStat::default(),
            sec_sum: 0.0, sec_n: 0, scratch: Vec::new(), last_tick: 0, clip_day: String::new(), clips_today: 0,
            status, free_gb, disk_warned: false, cfg, out, sr,
        }
    }

    fn time_of(&self, sample: u64) -> DateTime<Local> { to_local(self.ms_of(sample)) }
    /// 時刻は整数で持ち、日時への変換は必要なときだけ行う（常時処理の負荷を下げるため）。
    fn ms_of(&self, sample: u64) -> i64 {
        let (t, base) = self.t0.unwrap_or((0, 0));
        t + ((sample as i128 - base as i128) * 1000 / self.sr as i128) as i64
    }

    fn log(&self, t: DateTime<Local>, v: Value) {
        let dir = self.out.join("logs");
        let path = dir.join(format!("{}.jsonl", t.format("%Y-%m-%d")));
        let r = std::fs::create_dir_all(&dir).and_then(|_| {
            let mut f = OpenOptions::new().create(true).append(true).open(path)?;
            writeln!(f, "{v}")
        });
        if let Err(e) = r { self.set_error(format!("ログ書き込み失敗: {e}")); }
    }
    fn set_error(&self, e: String) { if let Ok(mut s) = self.status.lock() { s.error = e; } }

    /// 音声ブロックを処理する。`now_ms` はブロック末尾の時刻。
    pub fn feed(&mut self, samples: &[i16], now_ms: i64) {
        let end = self.ring.total() + samples.len() as u64;
        if self.t0.is_none() {
            let t = to_local(now_ms - samples.len() as i64 * 1000 / self.sr as i64);
            self.log(t, json!({"type": "start", "t": stamp(t)}));
        }
        if self.t0.is_none() || (self.ms_of(end) - now_ms).abs() > 2000 { self.t0 = Some((now_ms, end)); }
        for &x in samples {
            self.ring.push(x);
            if let Some(f) = self.an.push(x) { self.on_frame(f); }
        }
        self.tick(now_ms);
    }

    fn threshold(&self) -> f32 { (self.floor.db + self.cfg.noise.margin_db).max(self.cfg.noise.min_level_db) }

    fn on_frame(&mut self, f: Frame) {
        let total = self.ring.total();
        self.minute_add(&f, self.ms_of(total));
        if self.floor.warming_up() { self.floor.update(f.level_db); } else { self.detect(f, total); }
        self.pump(total);
    }

    fn detect(&mut self, f: Frame, total: u64) {
        let th = self.threshold();
        let (over, on) = (f.level_db >= th, f.level_db >= th - HYSTERESIS_DB);
        let release = (RELEASE_MS * self.sr / 1000) as u64;
        let need = self.cfg.noise.min_duration_ms.div_ceil(FRAME_MS);

        if self.event.is_none() {
            if !over { self.floor.update(f.level_db); return; }
            let start = total - self.an.frame_len() as u64;
            self.event = Some(Active { start, last_on: total, stats: EventStats::default(), over_frames: 0, confirmed: false });
        }
        let e = self.event.as_mut().unwrap();
        e.stats.add_flux(&f);
        if on { e.stats.add(&f); e.last_on = total; }
        if over { e.over_frames += 1; }
        // 十分に大きい音は一瞬でも検知する（ドアの音や足音など）
        let loud = f.level_db >= (self.floor.db + self.cfg.noise.impact_margin_db).max(th);
        let (confirm, start) = (!e.confirmed && (e.over_frames >= need || loud), e.start);
        if confirm {
            e.confirmed = true;
            self.begin_auto_clip(start, total);
        }
        if over {
            let post = self.cfg.noise.post_secs as u64 * self.sr as u64;
            if let Some(c) = &mut self.clip { c.end = c.end.max(total + post); }
        }
        let e = self.event.as_ref().unwrap();
        if e.stats.secs() >= CONTINUOUS_SECS {
            self.finish_event();
            self.floor.rebase(f.level_db);
        } else if total - e.last_on > release {
            let confirmed = e.confirmed;
            self.finish_event();
            if !confirmed { self.floor.update(f.level_db); }
        }
    }

    fn event_id(&mut self) -> String {
        self.seq += 1;
        format!("{}-{}", self.run_id, self.seq)
    }

    fn clip_info(&self, sample: u64) -> Option<(String, f64)> {
        self.clip.as_ref().map(|c| (c.rel.clone(), (sample.saturating_sub(c.start) as f64 / self.sr as f64 * 10.0).round() / 10.0))
    }

    fn finish_event(&mut self) {
        let Some(e) = self.event.take() else { return };
        if !e.confirmed { return; }
        let t = self.time_of(e.start);
        let id = self.event_id();
        let clip = self.clip_info(e.start);
        self.log(t, json!({
            "type": "event", "id": id, "t": stamp(t), "secs": r1(e.stats.secs()), "peak": r1(e.stats.peak_db),
            "mean": r1(e.stats.mean_db()), "low": ((e.stats.low_ratio() as f64) * 100.0).round() / 100.0, "flux": r1(e.stats.flux_db()),
            "class": classify(&e.stats, CONTINUOUS_SECS).key(), "source": "auto",
            "clip": clip.as_ref().map(|c| c.0.clone()), "offset": clip.as_ref().map(|c| c.1),
        }));
        if let Ok(mut s) = self.status.lock() { s.events_today += 1; }
    }

    fn disk_ok(&mut self) -> bool {
        let ok = (self.free_gb)(&self.out).is_none_or(|g| g >= self.cfg.general.min_free_disk_gb);
        if !ok && !self.disk_warned { self.set_error("空き容量不足のため保存停止中".into()); }
        self.disk_warned = !ok;
        ok
    }

    fn roll_day(&mut self, t: DateTime<Local>) {
        let day = t.format("%Y-%m-%d").to_string();
        if day == self.clip_day { return; }
        self.clips_today = std::fs::read_dir(self.out.join("clips").join(&day))
            .map(|d| d.flatten().filter(|f| f.file_name().to_string_lossy().ends_with("_auto.wav")).count() as u32).unwrap_or(0);
        if let Ok(mut s) = self.status.lock() { s.events_today = 0; }
        self.clip_day = day;
    }

    fn open_clip(&mut self, start: u64, end: u64, suffix: &str) -> bool {
        if !self.disk_ok() { return false; }
        let start = start.max(self.ring.oldest());
        let t = self.time_of(start);
        let rel = format!("clips/{}/{}_{suffix}.wav", t.format("%Y-%m-%d"), t.format("%Y%m%d_%H%M%S"));
        match WavWriter::create(&self.out.join(&rel), self.sr) {
            Ok(wav) => {
                if let Ok(mut s) = self.status.lock() { s.last_clip = rel.clone(); }
                self.clip = Some(LongClip { wav, rel, start, written: start, end });
                true
            }
            Err(e) => { self.set_error(format!("音声ファイルを作成できません: {e}")); false }
        }
    }

    fn begin_auto_clip(&mut self, start: u64, total: u64) {
        self.roll_day(self.time_of(start));
        if self.clip.is_some() || self.clips_today >= self.cfg.noise.max_clips_per_day { return; }
        let (pre, post) = (self.cfg.noise.pre_secs as u64 * self.sr as u64, self.cfg.noise.post_secs as u64 * self.sr as u64);
        if self.open_clip(start.saturating_sub(pre), total + post, "auto") { self.clips_today += 1; }
    }

    /// 手動保存。録音中なら延長し、そうでなければ過去N秒から保存を始める。
    pub fn manual_save(&mut self) {
        let total = self.ring.total();
        let (pre, post) = (self.cfg.noise.pre_secs as u64 * self.sr as u64, self.cfg.noise.post_secs as u64 * self.sr as u64);
        let t = self.time_of(total);
        self.roll_day(t);
        if let Some(c) = &mut self.clip { c.end = c.end.max(total + post); }
        else { self.open_clip(total.saturating_sub(pre), total + post, "manual"); }
        let info = self.clip_info(total);
        let id = self.event_id();
        self.log(t, json!({
            "type": "event", "id": id, "t": stamp(t), "secs": 0.0, "peak": null, "mean": null, "low": null, "flux": null,
            "class": "manual", "source": "manual", "clip": info.as_ref().map(|c| c.0.clone()), "offset": info.as_ref().map(|c| c.1),
        }));
    }

    /// リングから音声ファイルへ書き出す。
    fn pump(&mut self, total: u64) {
        let mut take = std::mem::take(&mut self.scratch);
        if let Some(c) = &mut self.clip {
            let to = total.min(c.end);
            if to > c.written {
                self.ring.range(c.written, to, &mut take);
                if let Err(e) = c.wav.write(&take, self.sr) { self.status.lock().unwrap().error = format!("書き込み失敗: {e}"); }
                c.written = to;
            }
            let max = self.cfg.noise.max_clip_secs as u64 * self.sr as u64;
            if c.written >= c.end { self.close_clip(); }
            else if c.written - c.start >= max {
                let (next, end) = (c.written, c.end);
                self.close_clip();
                if self.open_clip(next, end, "auto") {
                    let t = self.time_of(next);
                    let id = self.event_id();
                    let rel = self.clip.as_ref().map(|c| c.rel.clone());
                    self.log(t, json!({"type":"event","id":id,"t":stamp(t),"secs":0.0,"peak":null,"mean":null,"low":null,"flux":null,
                        "class":"continued","source":"continued","clip":rel,"offset":0.0}));
                }
            }
        }
        self.scratch = take;
    }
    fn close_clip(&mut self) {
        if let Some(c) = self.clip.take() {
            if let Err(e) = c.wav.finish(self.sr) { self.set_error(format!("保存失敗: {e}")); }
        }
    }

    fn minute_add(&mut self, f: &Frame, ms: i64) {
        let key = ms.div_euclid(60_000);
        if self.minute.n > 0 && self.minute.key != key { self.flush_minute(); }
        self.sec_sum += f.power as f64;
        self.sec_n += 1;
        let m = &mut self.minute;
        if m.n == 0 { m.key = key; m.peak = f.peak_db; }
        m.sum += f.power as f64;
        m.n += 1;
        m.peak = m.peak.max(f.peak_db);
        m.floor = self.floor.db;
    }
    fn flush_minute(&mut self) {
        let m = std::mem::take(&mut self.minute);
        if m.n == 0 { return; }
        let Some(t) = DateTime::from_timestamp(m.key * 60, 0).map(|d| d.with_timezone(&Local)) else { return };
        self.log(t, json!({"type":"minute","t":stamp(t),"avg":r1(db((m.sum / m.n as f64) as f32)),"peak":r1(m.peak),"floor":r1(m.floor)}));
    }

    fn tick(&mut self, now_ms: i64) {
        let total = self.ring.total();
        if total - self.last_tick < self.sr as u64 && self.last_tick != 0 { return; }
        self.last_tick = total;
        self.roll_day(to_local(now_ms));
        let level = db((self.sec_sum / self.sec_n.max(1) as f64) as f32);
        (self.sec_sum, self.sec_n) = (0.0, 0);
        let th = self.threshold();
        if let Ok(mut s) = self.status.lock() {
            s.floor_db = self.floor.db;
            s.level_db = level;
            s.threshold_db = th;
            s.recording = self.clip.is_some();
        }
    }

    /// 終了処理。書きかけの音声とログをすべて確定する。
    pub fn shutdown(&mut self) {
        if self.event.is_some() { self.finish_event(); }
        self.pump(self.ring.total());
        self.close_clip();
        self.flush_minute();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ring_keeps_latest_and_clamps() {
        let mut r = Ring::new(4);
        for i in 0..10 { r.push(i); }
        let mut v = vec![];
        r.range(0, 10, &mut v);
        assert_eq!(v, vec![6, 7, 8, 9]);
        r.range(7, 9, &mut v);
        assert_eq!(v, vec![7, 8]);
    }
}
