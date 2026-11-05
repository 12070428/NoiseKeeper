//! 16bit PCM WAV の書き込みと読み込み。途中で電源が落ちても再生できるよう定期的にヘッダーを更新する。
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub struct WavWriter { w: BufWriter<File>, samples: u32, since_checkpoint: u32, checkpoint_every: u32 }

fn header(sr: u32, samples: u32) -> [u8; 44] {
    let data = samples.saturating_mul(2);
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&data.saturating_add(36).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&1u16.to_le_bytes());
    h[24..28].copy_from_slice(&sr.to_le_bytes());
    h[28..32].copy_from_slice(&(sr * 2).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

impl WavWriter {
    pub fn create(path: &Path, sr: u32) -> io::Result<Self> {
        if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
        let mut w = BufWriter::with_capacity(1 << 16, File::create(path)?);
        w.write_all(&header(sr, 0))?;
        let mut me = Self { w, samples: 0, since_checkpoint: 0, checkpoint_every: sr * 5 };
        me.checkpoint(sr)?;
        Ok(me)
    }
    pub fn write(&mut self, s: &[i16], sr: u32) -> io::Result<()> {
        for x in s { self.w.write_all(&x.to_le_bytes())?; }
        self.samples = self.samples.saturating_add(s.len() as u32);
        self.since_checkpoint = self.since_checkpoint.saturating_add(s.len() as u32);
        if self.since_checkpoint >= self.checkpoint_every { self.checkpoint(sr)?; }
        Ok(())
    }
    fn checkpoint(&mut self, sr: u32) -> io::Result<()> {
        self.w.flush()?;
        let f = self.w.get_mut();
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&header(sr, self.samples))?;
        f.seek(SeekFrom::End(0))?;
        self.since_checkpoint = 0;
        Ok(())
    }
    pub fn finish(mut self, sr: u32) -> io::Result<()> { self.checkpoint(sr) }
}

/// WAV を読み込みモノラル化して返す（--simulate 用）。
pub fn read_mono(path: &Path) -> Result<(u32, Vec<i16>), String> {
    let mut b = Vec::new();
    File::open(path).and_then(|mut f| f.read_to_end(&mut b)).map_err(|e| e.to_string())?;
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" { return Err("WAVではありません".into()); }
    let (mut p, mut sr, mut ch) = (12usize, 0u32, 0usize);
    while p + 8 <= b.len() {
        let id = &b[p..p + 4];
        let len = u32::from_le_bytes(b[p + 4..p + 8].try_into().unwrap()) as usize;
        let body = p + 8;
        if id == b"fmt " {
            ch = u16::from_le_bytes(b[body + 2..body + 4].try_into().unwrap()) as usize;
            sr = u32::from_le_bytes(b[body + 4..body + 8].try_into().unwrap());
            if u16::from_le_bytes(b[body + 14..body + 16].try_into().unwrap()) != 16 { return Err("16bit WAVのみ対応".into()); }
        } else if id == b"data" {
            if ch == 0 { return Err("fmtチャンクがありません".into()); }
            let end = (body + len).min(b.len());
            let v = b[body..end].chunks_exact(2 * ch)
                .map(|f| (f.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as i32).sum::<i32>() / ch as i32) as i16)
                .collect();
            return Ok((sr, v));
        }
        p = body + len + (len & 1);
    }
    Err("dataチャンクがありません".into())
}
