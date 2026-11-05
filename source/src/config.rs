//! config.txt の読み書き。初回起動時に日本語コメント付きの既定値を書き出す。
use serde::Deserialize;
use std::path::Path;

pub const DEFAULT_TOML: &str = r#"# NoiseKeeper 設定（変更後は再起動）

[general]
output_dir = "data"       # 保存先
min_free_disk_gb = 5.0    # 空き容量がこれ未満なら音声を保存しない
report_days = 14          # レポートの日数
prevent_sleep = true      # スリープ抑止

[hotkeys]
manual_save = "F9"        # 手動保存（F1〜F24）

[noise]
pre_secs = 60             # さかのぼる秒数
post_secs = 30            # 音がやんだ後に録る秒数
max_clip_secs = 600       # 1ファイルの最大秒数
margin_db = 12.0          # 環境音より何dB大きければ検知
min_level_db = -55.0      # これ未満の音は無視（dBFS）
min_duration_ms = 300     # この時間続いたら検知
impact_margin_db = 20.0   # 環境音よりこのdB以上大きければ短くても検知
band_low_hz = 40.0        # 判定する帯域の下限
band_high_hz = 8000.0     # 判定する帯域の上限
max_clips_per_day = 500   # 自動保存の1日あたり上限

"#;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    pub output_dir: String,
    pub min_free_disk_gb: f64,
    pub report_days: u32,
    pub prevent_sleep: bool,
}
impl Default for General {
    fn default() -> Self {
        Self { output_dir: "data".into(), min_free_disk_gb: 5.0, report_days: 14, prevent_sleep: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hotkeys {
    pub manual_save: String,
}
impl Default for Hotkeys {
    fn default() -> Self {
        Self { manual_save: "F9".into() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Noise {
    pub pre_secs: u32,
    pub post_secs: u32,
    pub max_clip_secs: u32,
    pub margin_db: f32,
    pub min_level_db: f32,
    pub min_duration_ms: u32,
    pub impact_margin_db: f32,
    pub band_low_hz: f32,
    pub band_high_hz: f32,
    pub max_clips_per_day: u32,
}
impl Default for Noise {
    fn default() -> Self {
        Self {
            pre_secs: 60, post_secs: 30, max_clip_secs: 600, margin_db: 12.0, min_level_db: -55.0,
            min_duration_ms: 300, impact_margin_db: 20.0, band_low_hz: 40.0, band_high_hz: 8000.0, max_clips_per_day: 500,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub hotkeys: Hotkeys,
    pub noise: Noise,
}

impl Config {
    pub fn parse(s: &str) -> Result<Self, String> {
        let c: Config = toml::from_str(s).map_err(|e| e.to_string())?;
        c.validate()?;
        Ok(c)
    }
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            std::fs::write(path, DEFAULT_TOML).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        let b = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        // メモ帳でShift-JIS保存されてもコメント以外は読めるようにする
        Self::parse(String::from_utf8_lossy(&b).trim_start_matches('\u{feff}'))
    }
    fn validate(&self) -> Result<(), String> {
        let n = &self.noise;
        if n.band_low_hz <= 0.0 || n.band_low_hz >= n.band_high_hz {
            return Err("noise.band_low_hz は 0 より大きく band_high_hz 未満にしてください".into());
        }
        if n.pre_secs > 3600 {
            return Err("noise.pre_secs は 3600 以下にしてください".into());
        }
        if n.max_clip_secs == 0 {
            return Err("noise.max_clip_secs は 1 以上にしてください".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_text_matches_defaults() {
        let c = Config::parse(DEFAULT_TOML).unwrap();
        assert_eq!(c.noise.pre_secs, 60);
        assert_eq!(c.noise.post_secs, 30);
        assert_eq!(c.hotkeys.manual_save, "F9");
    }
    #[test]
    fn rejects_typos_and_bad_time() {
        assert!(Config::parse("[noise]\npre_sec = 3").is_err());
    }
}
