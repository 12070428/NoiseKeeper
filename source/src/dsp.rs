//! 軽量な信号処理: 双二次フィルタ、環境音の追従、イベント検出、音の分類。

#[derive(Clone, Copy, Default)]
pub struct Biquad { b0: f32, b1: f32, b2: f32, a1: f32, a2: f32, z1: f32, z2: f32 }

impl Biquad {
    fn from(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0, z1: 0.0, z2: 0.0 }
    }
    pub fn lowpass(sr: f32, f: f32) -> Self {
        let w = std::f32::consts::TAU * f.min(sr * 0.45) / sr;
        let (s, c) = w.sin_cos();
        let a = s / std::f32::consts::SQRT_2;
        Self::from((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + a, -2.0 * c, 1.0 - a)
    }
    pub fn highpass(sr: f32, f: f32) -> Self {
        let w = std::f32::consts::TAU * f.min(sr * 0.45) / sr;
        let (s, c) = w.sin_cos();
        let a = s / std::f32::consts::SQRT_2;
        Self::from((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + a, -2.0 * c, 1.0 - a)
    }
    #[inline]
    pub fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

pub const FRAME_MS: u32 = 50;
const EPS: f32 = 1e-10;
pub fn db(power: f32) -> f32 { 10.0 * (power + EPS).log10() }

/// 1フレーム（50ms）の特徴量。
#[derive(Clone, Copy, Debug, Default)]
pub struct Frame { pub level_db: f32, pub peak_db: f32, pub power: f32, pub low_power: f32 }

/// 帯域通過後の音量と 500Hz 以下の成分を求める。
pub struct Analyzer {
    hp: Biquad, lp: Biquad, low: Biquad,
    frame_len: usize, n: usize, sum: f32, low_sum: f32, peak: f32,
}
impl Analyzer {
    pub fn new(sr: u32, band_low: f32, band_high: f32) -> Self {
        let s = sr as f32;
        Self {
            hp: Biquad::highpass(s, band_low), lp: Biquad::lowpass(s, band_high), low: Biquad::lowpass(s, 500.0),
            frame_len: (sr * FRAME_MS / 1000) as usize, n: 0, sum: 0.0, low_sum: 0.0, peak: 0.0,
        }
    }
    pub fn frame_len(&self) -> usize { self.frame_len }
    #[inline]
    pub fn push(&mut self, sample: i16) -> Option<Frame> {
        let x = self.lp.run(self.hp.run(sample as f32 / 32768.0));
        let l = self.low.run(x);
        self.sum += x * x;
        self.low_sum += l * l;
        self.peak = self.peak.max(x.abs());
        self.n += 1;
        if self.n < self.frame_len { return None; }
        let n = self.n as f32;
        let f = Frame {
            level_db: db(self.sum / n), peak_db: db(self.peak * self.peak),
            power: self.sum / n, low_power: self.low_sum / n,
        };
        self.n = 0; self.sum = 0.0; self.low_sum = 0.0; self.peak = 0.0;
        Some(f)
    }
}

/// 環境音レベルの追従。静かになる変化には速く、大きくなる変化にはゆっくり追う。
pub struct Floor { pub db: f32, frames: u32 }
const WARMUP_FRAMES: u32 = 100; // 5秒
impl Floor {
    pub fn new() -> Self { Self { db: 0.0, frames: 0 } }
    pub fn warming_up(&self) -> bool { self.frames < WARMUP_FRAMES }
    pub fn rebase(&mut self, db: f32) { self.db = db; }
    pub fn update(&mut self, level: f32) {
        if self.frames == 0 { self.db = level; }
        self.frames = self.frames.saturating_add(1);
        let tau_frames = if self.warming_up() { 10.0 } else if level < self.db { 40.0 } else { 1200.0 };
        self.db += (level - self.db) / tau_frames;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class { Low, Speech, Impact, Continuous, Other }
impl Class {
    pub fn key(self) -> &'static str {
        match self {
            Class::Low => "low", Class::Speech => "speech",
            Class::Impact => "impact", Class::Continuous => "continuous", Class::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EventStats {
    pub frames: u32, pub peak_db: f32, pub sum_power: f32, pub sum_low: f32,
    pub sum_db: f32, pub sum_flux: f32, pub flux_frames: u32, last_db: Option<f32>,
}
impl EventStats {
    pub fn add(&mut self, f: &Frame) {
        if self.frames == 0 { self.peak_db = f.peak_db; }
        self.frames += 1;
        self.peak_db = self.peak_db.max(f.peak_db);
        self.sum_power += f.power; self.sum_low += f.low_power;
        self.sum_db += f.level_db;
    }
    /// 音量の変化量を積算する。音節で細かく上下する音（話し声）ほど大きくなる。
    pub fn add_flux(&mut self, f: &Frame) {
        if let Some(p) = self.last_db { self.sum_flux += (f.level_db - p).abs(); self.flux_frames += 1; }
        self.last_db = Some(f.level_db);
    }
    pub fn secs(&self) -> f32 { self.frames as f32 * FRAME_MS as f32 / 1000.0 }
    pub fn low_ratio(&self) -> f32 { self.sum_low / (self.sum_power + EPS) }
    pub fn mean_db(&self) -> f32 { self.sum_db / self.frames.max(1) as f32 }
    pub fn flux_db(&self) -> f32 { self.sum_flux / self.flux_frames.max(1) as f32 }
}

/// 長さ・低音の割合・音量の揺れから音の種類を推定する（単発の判定）。
pub fn classify(s: &EventStats, continuous_secs: f32) -> Class {
    let (d, low, flux) = (s.secs(), s.low_ratio(), s.flux_db());
    if d >= continuous_secs { Class::Continuous }
    else if d < 0.4 { Class::Impact }
    else if low >= 0.55 && d <= 5.0 { Class::Low }
    else if low < 0.45 && d >= 0.5 && flux >= 2.0 { Class::Speech }
    else { Class::Other }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tone_db(f: f32, sr: f32, secs: f32, filt: &mut Biquad) -> f32 {
        let n = (sr * secs) as usize;
        let mut p = 0.0;
        for i in 0..n {
            let y = filt.run((std::f32::consts::TAU * f * i as f32 / sr).sin());
            if i > n / 2 { p += y * y; }
        }
        db(p / (n / 2) as f32)
    }
    #[test]
    fn filters_pass_and_cut() {
        let sr = 48000.0;
        let pass = tone_db(100.0, sr, 1.0, &mut Biquad::lowpass(sr, 500.0));
        let cut = tone_db(4000.0, sr, 1.0, &mut Biquad::lowpass(sr, 500.0));
        assert!(pass - cut > 30.0, "{pass} {cut}");
        let hp_cut = tone_db(10.0, sr, 4.0, &mut Biquad::highpass(sr, 40.0));
        assert!(hp_cut < -25.0, "{hp_cut}");
    }
    #[test]
    fn floor_ignores_short_spikes_and_follows_silence() {
        let mut f = Floor::new();
        for _ in 0..200 { f.update(-60.0); }
        assert!((f.db + 60.0).abs() < 1.0);
        for _ in 0..10 { f.update(-20.0); }
        assert!(f.db < -59.0, "{}", f.db);
        for _ in 0..200 { f.update(-70.0); }
        assert!(f.db < -68.0, "{}", f.db);
    }
    #[test]
    fn classes() {
        let mut s = EventStats::default();
        let f = Frame { level_db: -30.0, peak_db: -25.0, power: 1e-3, low_power: 9e-4 };
        for _ in 0..4 { s.add(&f); }
        assert_eq!(classify(&s, 60.0), Class::Impact);
        for _ in 0..30 { s.add(&f); }
        assert_eq!(classify(&s, 60.0), Class::Low);
        assert_eq!(classify(&s, 1.0), Class::Continuous);
    }
}
