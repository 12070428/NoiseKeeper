//! Windows専用部分: マイク入力、タスクトレイ、グローバルホットキー、スリープ抑止、自動起動。
use crate::config::Config;
use crate::engine::{Engine, Status};
use crate::{base_dir, report};
use chrono::Local;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Power::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

enum Msg { Audio(Vec<i16>), Format(u32), Manual, Quit(SyncSender<()>) }

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }
fn copy_wide(dst: &mut [u16], s: &str) {
    let n = dst.len() - 1;
    let v: Vec<u16> = s.encode_utf16().take(n).collect();
    dst[..v.len()].copy_from_slice(&v);
    dst[v.len()] = 0;
}
fn message_box(text: &str) {
    let (t, c) = (wide(text), wide("NoiseKeeper"));
    unsafe { MessageBoxW(null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONINFORMATION); }
}
fn app_log(s: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(base_dir().join("noisekeeper.log")) {
        let _ = writeln!(f, "{} {s}", Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
}

fn free_gb(p: &Path) -> Option<f64> {
    let w = wide(&p.to_string_lossy());
    let mut free = 0u64;
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, null_mut(), null_mut()) };
    (ok != 0).then_some(free as f64 / 1e9)
}

// ---- マイク入力 ----
fn default_mic_name() -> Option<String> {
    cpal::default_host().default_input_device().and_then(|d| d.name().ok())
}

struct Collector { tx: SyncSender<Msg>, buf: Vec<i16>, block: usize, channels: usize, alive: Arc<AtomicU64>, started: Instant }
impl Collector {
    fn push<T: Copy>(&mut self, data: &[T], to_f32: impl Fn(T) -> f32) {
        self.alive.store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
        for f in data.chunks_exact(self.channels) {
            let m = f.iter().map(|&x| to_f32(x)).sum::<f32>() / self.channels as f32;
            self.buf.push((m.clamp(-1.0, 1.0) * 32767.0) as i16);
            if self.buf.len() >= self.block {
                // 処理が詰まっても音声スレッドは止めない
                let full = std::mem::replace(&mut self.buf, Vec::with_capacity(self.block));
                let _ = self.tx.try_send(Msg::Audio(full));
            }
        }
    }
}

fn open_stream(tx: &SyncSender<Msg>, failed: &Arc<AtomicBool>, alive: &Arc<AtomicU64>, started: Instant) -> Result<(cpal::Stream, String, String), String> {
    let host = cpal::default_host();
    let dev = host.default_input_device().ok_or("マイクが見つかりません")?;
    let name = dev.name().unwrap_or_default();
    let sup = dev.default_input_config().map_err(|e| e.to_string())?;
    let cfg: cpal::StreamConfig = sup.config();
    let sr = cfg.sample_rate.0;
    tx.send(Msg::Format(sr)).map_err(|e| e.to_string())?;
    let mk = || Collector { tx: tx.clone(), buf: Vec::new(), block: (sr / 10) as usize, channels: cfg.channels.max(1) as usize, alive: alive.clone(), started };
    let f = failed.clone();
    let err = move |e: cpal::StreamError| { app_log(&format!("stream error: {e}")); f.store(true, Ordering::Relaxed); };
    let stream = match sup.sample_format() {
        cpal::SampleFormat::F32 => { let mut c = mk(); dev.build_input_stream(&cfg, move |d: &[f32], _| c.push(d, |x| x), err, None) }
        cpal::SampleFormat::I16 => { let mut c = mk(); dev.build_input_stream(&cfg, move |d: &[i16], _| c.push(d, |x| x as f32 / 32768.0), err, None) }
        cpal::SampleFormat::I32 => { let mut c = mk(); dev.build_input_stream(&cfg, move |d: &[i32], _| c.push(d, |x| x as f32 / 2147483648.0), err, None) }
        other => return Err(format!("未対応の音声形式: {other:?}")),
    }.map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    let info = format!("{name} / {sr}Hz / {}ch", cfg.channels);
    Ok((stream, name, info))
}

/// cpal の Stream はスレッドをまたげないため専用スレッドで保持し、切断時は自動で開き直す。
fn audio_thread(tx: SyncSender<Msg>, status: Arc<Mutex<Status>>, stop: Arc<AtomicBool>) {
    let started = Instant::now();
    let alive = Arc::new(AtomicU64::new(0));
    while !stop.load(Ordering::Relaxed) {
        let failed = Arc::new(AtomicBool::new(false));
        match open_stream(&tx, &failed, &alive, started) {
            Ok((stream, name, info)) => {
                app_log(&format!("recording: {info}"));
                { let mut s = status.lock().unwrap(); s.error.clear(); s.mic = name.clone(); }
                alive.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                let mut ticks = 0u32;
                loop {
                    std::thread::sleep(Duration::from_secs(2));
                    ticks += 1;
                    // Windowsの既定マイクが変わったら追従する
                    if ticks % 5 == 0 && default_mic_name().is_some_and(|n| n != name) { break; }
                    let stale = started.elapsed().as_millis() as u64 - alive.load(Ordering::Relaxed) > 10_000;
                    if stop.load(Ordering::Relaxed) || failed.load(Ordering::Relaxed) || stale { break; }
                }
                drop(stream);
                if !stop.load(Ordering::Relaxed) { status.lock().unwrap().error = "マイク再接続中".into(); }
            }
            Err(e) => {
                app_log(&format!("mic open failed: {e}"));
                status.lock().unwrap().error = e;
            }
        }
        if !stop.load(Ordering::Relaxed) { std::thread::sleep(Duration::from_secs(5)); }
    }
}

fn engine_thread(cfg: Config, out: PathBuf, rx: Receiver<Msg>, status: Arc<Mutex<Status>>) {
    let mut eng: Option<(Engine, u32)> = None;
    while let Ok(m) = rx.recv() {
        match m {
            Msg::Format(sr) => {
                if eng.as_ref().is_some_and(|e| e.1 == sr) { continue; }
                if let Some((mut e, _)) = eng.take() { e.shutdown(); }
                eng = Some((Engine::new(cfg.clone(), out.clone(), sr, status.clone(), Box::new(free_gb)), sr));
            }
            Msg::Audio(v) => if let Some((e, _)) = &mut eng { e.feed(&v, chrono::Utc::now().timestamp_millis()); },
            Msg::Manual => if let Some((e, _)) = &mut eng { e.manual_save(); },
            Msg::Quit(ack) => {
                if let Some((e, _)) = &mut eng { e.shutdown(); }
                let _ = ack.send(());
                return;
            }
        }
    }
}

// ---- 画面とタスクトレイ ----
const WM_TRAY: u32 = WM_APP + 1;
const ID_MANUAL: usize = 1; const ID_REPORT: usize = 2; const ID_FOLDER: usize = 3; const ID_CONFIG: usize = 4;
const ID_AUTORUN: usize = 5; const ID_QUIT: usize = 6; const ID_OPEN: usize = 7;
const HK_MANUAL: i32 = 1;

struct Ui {
    tx: SyncSender<Msg>, status: Arc<Mutex<Status>>, out: PathBuf, days: u32, key: String, stop: Arc<AtomicBool>,
    last_icon: Mutex<(u8, String)>, labels: Mutex<Vec<(isize, String)>>, autorun_box: Mutex<isize>,
}
static UI: OnceLock<Ui> = OnceLock::new();

fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut n: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = 1;
    n
}

/// 状態ごとの色で丸いアイコンを作る（緑=監視中、赤=保存中、灰=異常）。
fn make_icon(rgb: (u8, u8, u8)) -> HICON {
    const S: i32 = 32;
    let (mut color, mut mask) = (vec![0u8; (S * S * 4) as usize], vec![0xFFu8; (S * S / 8) as usize]);
    for y in 0..S { for x in 0..S {
        let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
        if dx * dx + dy * dy <= 14.5 * 14.5 {
            let i = ((y * S + x) * 4) as usize;
            color[i] = rgb.2; color[i + 1] = rgb.1; color[i + 2] = rgb.0; color[i + 3] = 255;
            mask[(y * 4 + x / 8) as usize] &= !(0x80 >> (x % 8));
        }
    } }
    unsafe {
        let bc = CreateBitmap(S, S, 1, 32, color.as_ptr() as *const c_void);
        let bm = CreateBitmap(S, S, 1, 1, mask.as_ptr() as *const c_void);
        let info = ICONINFO { fIcon: TRUE, xHotspot: 0, yHotspot: 0, hbmMask: bm, hbmColor: bc };
        let i = CreateIconIndirect(&info);
        DeleteObject(bc); DeleteObject(bm);
        i
    }
}

fn state_of(s: &Status) -> (u8, (u8, u8, u8), &'static str) {
    if !s.error.is_empty() { (2, (140, 140, 140), "異常") }
    else if s.recording { (1, (220, 50, 50), "保存中") }
    else { (0, (40, 170, 90), "監視中") }
}

fn refresh(hwnd: HWND, first: bool) {
    let ui = UI.get().unwrap();
    let s = ui.status.lock().unwrap().clone();
    let (state, rgb, name) = state_of(&s);
    let tip = if s.error.is_empty() { format!("NoiseKeeper {name}\n本日の検知 {} 件", s.events_today) } else { format!("NoiseKeeper\n{}", s.error) };
    {
        let mut last = ui.last_icon.lock().unwrap();
        if first || last.0 != state || last.1 != tip {
            let mut n = nid(hwnd);
            n.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
            n.uCallbackMessage = WM_TRAY;
            n.hIcon = make_icon(rgb);
            copy_wide(&mut n.szTip, &tip);
            unsafe {
                Shell_NotifyIconW(if first { NIM_ADD } else { NIM_MODIFY }, &n);
                DestroyIcon(n.hIcon);
            }
            *last = (state, tip);
        }
    }
    if unsafe { IsWindowVisible(hwnd) } == 0 { return; } // 非表示中は画面を更新しない
    let state_text = if s.error.is_empty() { name.to_string() } else { format!("{name}: {}", s.error) };
    let last_file = s.last_clip.rsplit('/').next().unwrap_or("");
    let texts = [
        format!("状態: {state_text}"),
        format!("マイク: {}", if s.mic.is_empty() { "－" } else { &s.mic }),
        format!("音量: {:.0} dB　環境音: {:.0} dB　検知ライン: {:.0} dB", s.level_db, s.floor_db, s.threshold_db),
        format!("本日の検知: {} 件　最後の保存: {}", s.events_today, if last_file.is_empty() { "－" } else { last_file }),
    ];
    let mut labels = ui.labels.lock().unwrap();
    for ((h, old), new) in labels.iter_mut().zip(texts) {
        if *old != new {
            let w = wide(&new);
            unsafe { SetWindowTextW(*h as HWND, w.as_ptr()); }
            *old = new;
        }
    }
}

fn balloon(hwnd: HWND, title: &str, text: &str) {
    let mut n = nid(hwnd);
    n.uFlags = NIF_INFO;
    copy_wide(&mut n.szInfoTitle, title);
    copy_wide(&mut n.szInfo, text);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &n); }
}

fn open_path(p: &Path) {
    let (op, f) = (wide("open"), wide(&p.to_string_lossy()));
    unsafe { ShellExecuteW(null_mut(), op.as_ptr(), f.as_ptr(), null(), null(), SW_SHOWNORMAL); }
}

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
fn autorun_enabled() -> bool {
    let (k, v) = (wide(RUN_KEY), wide("NoiseKeeper"));
    unsafe { RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, null_mut(), null_mut(), null_mut()) == ERROR_SUCCESS }
}
fn autorun_toggle() {
    let (k, v) = (wide(RUN_KEY), wide("NoiseKeeper"));
    unsafe {
        if autorun_enabled() {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr());
        } else if let Ok(exe) = std::env::current_exe() {
            let d = wide(&format!("\"{}\" --tray", exe.display()));
            RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), REG_SZ, d.as_ptr() as *const c_void, (d.len() * 2) as u32);
        }
    }
}
fn sync_autorun_box() {
    let h = *UI.get().unwrap().autorun_box.lock().unwrap();
    unsafe { SendMessageW(h as HWND, BM_SETCHECK, if autorun_enabled() { BST_CHECKED as usize } else { 0 }, 0); }
}

fn show_window(hwnd: HWND) {
    unsafe { ShowWindow(hwnd, SW_SHOW); ShowWindow(hwnd, SW_RESTORE); SetForegroundWindow(hwnd); }
    refresh(hwnd, false);
}

fn show_menu(hwnd: HWND) {
    let ui = UI.get().unwrap();
    unsafe {
        let m = CreatePopupMenu();
        let add = |id: usize, text: String, flags: u32| { let w = wide(&text); AppendMenuW(m, MF_STRING | flags, id, w.as_ptr()); };
        add(ID_OPEN, "画面を開く".into(), MF_DEFAULT);
        add(ID_MANUAL, format!("今すぐ保存\t{}", ui.key), 0);
        add(ID_REPORT, "レポート".into(), 0);
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        add(ID_QUIT, "終了".into(), 0);
        let mut p = POINT { x: 0, y: 0 };
        GetCursorPos(&mut p);
        SetForegroundWindow(hwnd);
        TrackPopupMenu(m, TPM_RIGHTBUTTON, p.x, p.y, 0, hwnd, null());
        DestroyMenu(m);
    }
}

fn do_manual(hwnd: HWND) {
    let _ = UI.get().unwrap().tx.send(Msg::Manual);
    balloon(hwnd, "NoiseKeeper", "保存開始");
}
fn do_report() {
    let ui = UI.get().unwrap();
    match report::generate(&ui.out, ui.days) {
        Ok(p) => open_path(&p),
        Err(e) => message_box(&format!("レポートを作成できません: {e}")),
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(ui) = UI.get() else { return DefWindowProcW(hwnd, msg, wp, lp) };
    match msg {
        WM_TRAY => match lp as u32 {
            WM_LBUTTONUP => show_window(hwnd),
            WM_RBUTTONUP => show_menu(hwnd),
            _ => {}
        },
        WM_TIMER => refresh(hwnd, false),
        WM_HOTKEY => if wp as i32 == HK_MANUAL { do_manual(hwnd) },
        WM_COMMAND => match wp & 0xFFFF {
            ID_OPEN => show_window(hwnd),
            ID_MANUAL => do_manual(hwnd),
            ID_REPORT => do_report(),
            ID_FOLDER => open_path(&ui.out),
            ID_CONFIG => open_path(&base_dir().join("config.txt")),
            ID_AUTORUN => { autorun_toggle(); sync_autorun_box(); }
            ID_QUIT => { DestroyWindow(hwnd); }
            _ => {}
        },
        WM_CLOSE => { ShowWindow(hwnd, SW_HIDE); } // ×ではタスクトレイへ
        WM_DESTROY => { shutdown(hwnd); PostQuitMessage(0); }
        WM_ENDSESSION => { if wp != 0 { shutdown(hwnd); } }
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    0
}

fn shutdown(hwnd: HWND) {
    let ui = UI.get().unwrap();
    if ui.stop.swap(true, Ordering::Relaxed) { return; }
    let (ack_tx, ack_rx) = sync_channel(1);
    if ui.tx.send(Msg::Quit(ack_tx)).is_ok() { let _ = ack_rx.recv_timeout(Duration::from_secs(5)); }
    unsafe { Shell_NotifyIconW(NIM_DELETE, &nid(hwnd)); }
}

fn parse_vk(key: &str) -> Option<u32> {
    let n: u32 = key.trim().to_uppercase().strip_prefix('F')?.parse().ok()?;
    (1..=24).contains(&n).then_some(VK_F1 as u32 + n - 1)
}

/// 画面の部品を作る。座標は96dpi基準で、DPIに合わせて拡大する。
unsafe fn build_controls(hwnd: HWND, hinst: HINSTANCE, usage: &str) -> (Vec<(isize, String)>, isize) {
    let dc = GetDC(null_mut());
    let dpi = GetDeviceCaps(dc, LOGPIXELSY as _);
    ReleaseDC(null_mut(), dc);
    let sc = |v: i32| v * dpi / 96;
    let face = wide("Yu Gothic UI");
    let font = CreateFontW(-sc(14), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
    let make = |class: &str, text: &str, style: u32, x: i32, y: i32, w: i32, h: i32, id: usize| {
        let (c, t) = (wide(class), wide(text));
        let ctl = CreateWindowExW(0, c.as_ptr(), t.as_ptr(), WS_CHILD | WS_VISIBLE | style, sc(x), sc(y), sc(w), sc(h), hwnd, id as HMENU, hinst, null());
        SendMessageW(ctl, WM_SETFONT, font as usize, 1);
        ctl
    };
    let mut labels = Vec::new();
    for i in 0..4 {
        let h = make("STATIC", "", 0, 14, 12 + i * 24, 532, 22, 0);
        labels.push((h as isize, String::new()));
    }
    let bs = BS_PUSHBUTTON as u32;
    for (i, (t, id)) in [("今すぐ保存", ID_MANUAL), ("レポート", ID_REPORT), ("保存フォルダ", ID_FOLDER), ("設定", ID_CONFIG), ("終了", ID_QUIT)].into_iter().enumerate() {
        make("BUTTON", t, bs, 14 + i as i32 * 107, 116, 100, 30, id);
    }
    let auto = make("BUTTON", "Windows起動時に自動で開始", BS_AUTOCHECKBOX as u32, 14, 154, 300, 24, ID_AUTORUN);
    make("STATIC", usage, 0, 14, 188, 532, 130, 0);
    let (mut r, style) = (RECT { left: 0, top: 0, right: sc(560), bottom: sc(330) }, WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX);
    AdjustWindowRect(&mut r, style, 0);
    SetWindowPos(hwnd, null_mut(), 0, 0, r.right - r.left, r.bottom - r.top, SWP_NOMOVE | SWP_NOZORDER);
    (labels, auto as isize)
}

pub fn run() {
    let name = wide("Local\\NoiseKeeperSingleInstance");
    let _guard = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        message_box("すでに起動しています");
        return;
    }
    let start_hidden = std::env::args().any(|a| a == "--tray");
    let dir = base_dir();
    let cfg = match Config::load_or_create(&dir.join("config.txt")) {
        Ok(c) => c,
        Err(e) => { message_box(&format!("config.txt のエラー\n\n{e}")); return; }
    };
    let out = { let p = PathBuf::from(&cfg.general.output_dir); if p.is_absolute() { p } else { dir.join(p) } };
    let _ = std::fs::create_dir_all(&out);
    if cfg.general.prevent_sleep { unsafe { SetProcessDPIAware(); SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED); } }
    else { unsafe { SetProcessDPIAware(); } }

    let (tx, rx) = sync_channel::<Msg>(128);
    let status = Arc::new(Mutex::new(Status { error: "マイク準備中".into(), ..Default::default() }));
    let stop = Arc::new(AtomicBool::new(false));
    { let (c, o, s) = (cfg.clone(), out.clone(), status.clone()); std::thread::spawn(move || engine_thread(c, o, rx, s)); }
    { let (t, s, st) = (tx.clone(), status.clone(), stop.clone()); std::thread::spawn(move || audio_thread(t, s, st)); }

    let key = cfg.hotkeys.manual_save.clone();
    let usage = format!(
        "使い方\n・起動したままにすると、大きな音を検知して前後の音声を自動で保存します\n・{key} か「今すぐ保存」で、過去{}秒＋その後{}秒を保存します\n・保存した音声と検知の一覧は「レポート」で確認できます\n・×で閉じてもタスクトレイで動き続けます（完全に止めるのは「終了」）\n・誤検知が多いときは「設定」の margin_db を上げます（変更後は再起動）",
        cfg.noise.pre_secs, cfg.noise.post_secs);
    let _ = UI.set(Ui { tx, status, out, days: cfg.general.report_days, key: key.clone(), stop, last_icon: Mutex::new((255, String::new())), labels: Mutex::new(Vec::new()), autorun_box: Mutex::new(0) });
    unsafe {
        let hinst = GetModuleHandleW(null());
        let class = wide("NoiseKeeperWindow");
        let title = wide("NoiseKeeper");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.hIcon = make_icon((40, 170, 90));
        wc.hbrBackground = (COLOR_BTNFACE + 1) as usize as HBRUSH;
        RegisterClassW(&wc);
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), style, CW_USEDEFAULT, CW_USEDEFAULT, 400, 300, null_mut(), null_mut(), hinst, null());
        let (labels, auto) = build_controls(hwnd, hinst, &usage);
        { let ui = UI.get().unwrap(); *ui.labels.lock().unwrap() = labels; *ui.autorun_box.lock().unwrap() = auto; }
        sync_autorun_box();
        refresh(hwnd, true);
        if !start_hidden { show_window(hwnd); }
        SetTimer(hwnd, 1, 1000, None);
        if !parse_vk(&key).is_some_and(|vk| RegisterHotKey(hwnd, HK_MANUAL, MOD_NOREPEAT, vk) != 0) {
            balloon(hwnd, "キー登録失敗", &key);
        }
        let mut m: MSG = std::mem::zeroed();
        while GetMessageW(&mut m, null_mut(), 0, 0) > 0 {
            TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    }
}
