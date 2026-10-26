use std::collections::HashSet;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Mutex;

static CONNECTED: AtomicBool = AtomicBool::new(false);
static GLOBAL_TOUCH_TX: std::sync::Mutex<Option<tokio::sync::mpsc::Sender<SendableAndroidAutoMessage>>> =
    std::sync::Mutex::new(None);

/// Video feeder queue depth. Bounded to 16 chunks (~250ms max jitter buffer)
/// to allow smooth Wi-Fi packet bursts without ever blocking the protocol event loop.
/// Besleyici kuyrugu. Buyuk tutulur: gecici CPU tikanmalarinda kare atmamak icin (aksi halde referans
/// zinciri bozulup goruntu bir sonraki anahtar kareye ~69 sn donuyor).
const VIDEO_QUEUE_CAP: usize = 512;

/// One H.264 chunk on its way to FFmpeg, stamped with its enqueue time so the feeder can
/// report real end-to-end pipeline lag instead of guessing.
struct VideoChunk {
    data: Vec<u8>,
    enq: std::time::Instant,
}

/// Process-wide video pipeline counters, read by the stats reporter task.
struct VideoStats {
    /// Chunks accepted into the feeder queue.
    enqueued: AtomicU64,
    /// Chunks actually written to FFmpeg's stdin.
    written: AtomicU64,
    /// Current feeder queue depth.
    queued: AtomicUsize,
    /// Age of the chunk most recently handed to FFmpeg.
    last_age_ms: AtomicU64,
    /// Oldest chunk age seen since the last `reset()`.
    max_age_ms: AtomicU64,
    /// Set when a write to FFmpeg's stdin fails (decoder exited / pipe broken).
    ffmpeg_dead: AtomicBool,
}

impl VideoStats {
    const fn new() -> Self {
        Self {
            enqueued: AtomicU64::new(0),
            written: AtomicU64::new(0),
            queued: AtomicUsize::new(0),
            last_age_ms: AtomicU64::new(0),
            max_age_ms: AtomicU64::new(0),
            ffmpeg_dead: AtomicBool::new(false),
        }
    }

    fn reset(&self) {
        self.enqueued.store(0, Ordering::Relaxed);
        self.written.store(0, Ordering::Relaxed);
        self.queued.store(0, Ordering::Relaxed);
        self.last_age_ms.store(0, Ordering::Relaxed);
        self.max_age_ms.store(0, Ordering::Relaxed);
        self.ffmpeg_dead.store(false, Ordering::Relaxed);
    }
}

static VIDEO_STATS: VideoStats = VideoStats::new();
/// True between `setup_video` and video teardown; gates the stats reporter.
static VIDEO_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Incremented on every `setup_video`; lets a feeder from a previous (torn-down) session know
/// its write errors belong to a dead FFmpeg instance, not the current one.
static VIDEO_GEN: AtomicU64 = AtomicU64::new(0);

use android_auto::{
    AndroidAutoAudioInputTrait, AndroidAutoAudioOutputTrait, AndroidAutoConfiguration,
    AndroidAutoInputChannelTrait, AndroidAutoMainTrait, AndroidAutoMessage,
    AndroidAutoSensorTrait, AndroidAutoVideoChannelTrait, AudioChannelType,
    HeadUnitInfo, InputConfiguration, SendableAndroidAutoMessage,
    SensorInformation, VideoConfiguration, Wifi,
};

struct ExperAutoInner {
    connected: bool,
    android_send: tokio::sync::mpsc::Sender<SendableAndroidAutoMessage>,
    android_recv: Option<tokio::sync::mpsc::Receiver<SendableAndroidAutoMessage>>,
    video_proc: Option<Child>,
    display_proc: Option<Child>,
    audio_proc: Option<Child>,
}

#[derive(Clone)]
struct ExperAuto {
    inner: Arc<Mutex<ExperAutoInner>>,
    video_sender: Arc<RwLock<Option<tokio::sync::mpsc::Sender<VideoChunk>>>>,
    audio_sender: Arc<RwLock<Option<tokio::sync::mpsc::Sender<Vec<u8>>>>>,
    video_config: VideoConfiguration,
    input_config: InputConfiguration,
    sensor_config: SensorInformation,
}

impl ExperAuto {
    fn new(
        android_send: tokio::sync::mpsc::Sender<SendableAndroidAutoMessage>,
        android_recv: tokio::sync::mpsc::Receiver<SendableAndroidAutoMessage>,
    ) -> Self {
        let mut sensors = HashSet::new();
        sensors.insert(Wifi::sensor_type::Enum::DRIVING_STATUS);
        sensors.insert(Wifi::sensor_type::Enum::NIGHT_DATA);

        Self {
            inner: Arc::new(Mutex::new(ExperAutoInner {
                connected: false,
                android_send,
                android_recv: Some(android_recv),
                video_proc: None,
                display_proc: None,
                audio_proc: None,
            })),
            video_sender: Arc::new(RwLock::new(None)),
            audio_sender: Arc::new(RwLock::new(None)),
            video_config: VideoConfiguration {
                resolution: Wifi::video_resolution::Enum::_480p,
                fps: Wifi::video_fps::Enum::_30,
                dpi: 160,
                margin_width: layout().margin.0,
                margin_height: layout().margin.1,
            },
            input_config: InputConfiguration {
                keycodes: vec![],
                // margin istenirse dokunmatik boyutu da margin disindaki arayuz alani kadar bildirilir
                touchscreen: Some((FRAME_W as u16 - layout().margin.0, FRAME_H as u16 - layout().margin.1)),
            },
            sensor_config: SensorInformation { sensors },
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct LinuxInputEvent {
    tv_sec: u32,
    tv_usec: u32,
    type_: u16,
    code: u16,
    value: i32,
}

/// Panel 1024x768. Android Auto kanvasi 800x480 (5:3); panel 4:3 oldugu icin videoyu donanimsal
/// olarak degil, drm-show icinde (NEON) olcekleyip su dikdortgene basariz.
const PANEL_W: u32 = 1024;
const PANEL_H: u32 = 768;

/// Video yerlesimi: AA kanvasinin hangi bolgesi (crop) panelin hangi dikdortgenine (dst) basiliyor ve
/// telefondan hangi kenar boslugu (margin) isteniyor. `EXPER_VIDEO_LAYOUT` ile secilir:
///  - "fit" (varsayilan): tum 800x480 -> 1024x614, dikeyde ortali (oran korunur, ust/alt siyah bant)
///  - "full": tum 800x480 -> 1024x768 (gerilir, bant yok)
///  - "1024xH" (480..768): tum kare -> 1024xH, dikeyde ortali
///  - "4x3": telefondan margin_width=160 istenir (arayuz ortada 640x480'e daralir), o bolge kirpilip 1024x768'e
///    buyutulur: oran dogru ve bant yok
#[derive(Clone, Copy)]
struct Layout {
    dst: (u32, u32, u32, u32),
    crop: (u32, u32, u32, u32),
    margin: (u16, u16),
}

const FRAME_W: u32 = 800;
const FRAME_H: u32 = 480;

fn layout() -> &'static Layout {
    static L: std::sync::OnceLock<Layout> = std::sync::OnceLock::new();
    L.get_or_init(|| {
        let full_frame = (0, 0, FRAME_W, FRAME_H);
        let fit = Layout { dst: (0, (PANEL_H - 614) / 2, PANEL_W, 614), crop: full_frame, margin: (0, 0) };
        match std::env::var("EXPER_VIDEO_LAYOUT").as_deref() {
            Ok("full") => Layout { dst: (0, 0, PANEL_W, PANEL_H), ..fit },
            Ok("4x3") => Layout {
                dst: (0, 0, PANEL_W, PANEL_H),
                crop: (80, 0, 640, FRAME_H),
                margin: (160, 0),
            },
            // "1024xH": genislik 1024 (NEON olcekleyici icin), yukseklik H, dikeyde ortali
            Ok(l) if l.starts_with("1024x") => match l[5..].parse::<u32>() {
                Ok(h) if (480..=PANEL_H).contains(&h) => Layout { dst: (0, (PANEL_H - h) / 2, PANEL_W, h), ..fit },
                _ => fit,
            },
            _ => fit,
        }
    })
}

/// Tani: gelen H.264 parcalarindaki NAL turlerini yazar (ilk 30 parca + SPS/PPS/IDR iceren her parca).
/// 7=SPS 8=PPS 5=IDR 1=P 6=SEI. Anahtar karenin ne zaman geldigini gormek icin.
fn nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && (data[i + 2] == 1 || (data[i + 2] == 0 && data[i + 3] == 1)) {
            let h = if data[i + 2] == 1 { i + 3 } else { i + 4 };
            if h < data.len() {
                types.push(data[h] & 0x1f);
            }
            i = h;
        } else {
            i += 1;
        }
    }
    types
}

fn log_nal_types(data: &[u8]) {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var_os("EXPER_DEBUG_H264").is_some() || std::env::var_os("EXPER_DUMP_H264").is_some()) {
        return;
    }
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let types = nal_types(data);
    // Tani: EXPER_DUMP_H264=/yol ile gelen ham akisin ilk ~3 MB'i dosyaya yazilir (FFmpeg ayarlarini yeniden oynatarak denemek icin)
    if let Ok(path) = std::env::var("EXPER_DUMP_H264") {
        static WRITTEN: AtomicU64 = AtomicU64::new(0);
        if WRITTEN.load(Ordering::Relaxed) < 3_000_000 {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = f.write_all(data);
                WRITTEN.fetch_add(data.len() as u64, Ordering::Relaxed);
            }
        }
    }
    if n < 30 || types.iter().any(|t| matches!(t, 5 | 7 | 8)) {
        println!("[H264] chunk#{} len={} nal={:?}", n, data.len(), types);
    }
}

/// Ekrani video icin hazirla: konsolu (fbcon) ayir, ekrani siyaha boya, arka isigi ac.
fn prepare_display() {
    // fbcon bagli kalirsa kernel/getty yazilari siyah bantlarin uzerine cizilir
    for i in 0..4 {
        let name = std::fs::read_to_string(format!("/sys/class/vtconsole/vtcon{i}/name")).unwrap_or_default();
        if name.contains("frame buffer") {
            let _ = std::fs::write(format!("/sys/class/vtconsole/vtcon{i}/bind"), "0");
        }
    }
    // primary plane (fbdev emulasyonu) siyah
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("/dev/fb0") {
        use std::io::Write;
        let zeros = vec![0u8; (PANEL_W * PANEL_H * 4) as usize];
        let _ = f.write_all(&zeros);
    }
    let bl = "/sys/class/backlight/backlight";
    if let Ok(max) = std::fs::read_to_string(format!("{bl}/max_brightness")) {
        let _ = std::fs::write(format!("{bl}/brightness"), max.trim());
    }
}

fn send_touch_event(
    android_send: &tokio::sync::mpsc::Sender<SendableAndroidAutoMessage>,
    action: Wifi::touch_action::Enum,
    raw_x: u32,
    raw_y: u32,
) {
    // Panel koordinatini (1024x768) video dikdortgeni ve kirpma uzerinden 800x480 AA kanvasina esle.
    let l = layout();
    let (dx, dy, dw, dh) = l.dst;
    let (cx, cy, cw, ch) = l.crop;
    // Telefon, margin istenirse dokunma koordinatini margin DISLANMIS arayuz alanina gore (0..FRAME-margin)
    // bekliyor ve kenar boslugunu kendisi ekliyor: kirpma baslangicindan margin/2'yi cikar.
    let (mw, mh) = (l.margin.0 as u32, l.margin.1 as u32);
    let (ui_w, ui_h) = (FRAME_W - mw, FRAME_H - mh);
    let x = (cx.saturating_sub(mw / 2) + raw_x.saturating_sub(dx).min(dw - 1) * cw / dw).min(ui_w - 1);
    let y = (cy.saturating_sub(mh / 2) + raw_y.saturating_sub(dy).min(dh - 1) * ch / dh).min(ui_h - 1);

    let mut ev = Wifi::InputEventIndication::new();
    ev.set_timestamp(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64,
    );

    let mut touch = Wifi::TouchEvent::new();
    touch.set_touch_action(action);
    touch.set_action_index(0);

    let mut loc = Wifi::TouchLocation::new();
    loc.set_pointer_id(0);
    loc.set_x(x);
    loc.set_y(y);
    touch.touch_location.push(loc);

    ev.touch_event = ::protobuf::MessageField::some(touch);

    let _ = android_send.try_send(AndroidAutoMessage::Input(ev).sendable());
    if action != Wifi::touch_action::Enum::DRAG {
        println!("[TOUCH] Sent {:?} at raw=({}, {}) -> aa=({}, {})", action, raw_x, raw_y, x, y);
    }
}

fn spawn_touch_worker() {
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = match std::fs::File::open("/dev/input/event0") {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[TOUCH] Failed to open /dev/input/event0: {}", e);
                return;
            }
        };

        println!("[TOUCH] Goodix GT911 touch event listener active on /dev/input/event0");

        let mut is_down = false;
        let mut cur_x: u32 = 512;
        let mut cur_y: u32 = 384;
        let mut last_sent_x: u32 = 512;
        let mut last_sent_y: u32 = 384;
        let mut last_send_time = std::time::Instant::now();
        let mut pending_down = false;
        let mut pending_up = false;

        let mut buf = [0u8; 16];

        while file.read_exact(&mut buf).is_ok() {
            let ev: LinuxInputEvent = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const LinuxInputEvent) };

            const EV_SYN: u16 = 0x00;
            const EV_KEY: u16 = 0x01;
            const EV_ABS: u16 = 0x03;
            const BTN_TOUCH: u16 = 330;
            const ABS_X: u16 = 0;
            const ABS_Y: u16 = 1;
            const ABS_MT_POSITION_X: u16 = 53;
            const ABS_MT_POSITION_Y: u16 = 54;
            const ABS_MT_TRACKING_ID: u16 = 57;

            match ev.type_ {
                EV_KEY => {
                    if ev.code == BTN_TOUCH {
                        if ev.value == 1 {
                            pending_down = true;
                        } else if ev.value == 0 {
                            pending_up = true;
                        }
                    }
                }
                EV_ABS => {
                    if ev.code == ABS_X || ev.code == ABS_MT_POSITION_X {
                        cur_x = (ev.value.clamp(0, 1024)) as u32;
                    } else if ev.code == ABS_Y || ev.code == ABS_MT_POSITION_Y {
                        cur_y = (ev.value.clamp(0, 768)) as u32;
                    } else if ev.code == ABS_MT_TRACKING_ID {
                        if ev.value >= 0 {
                            pending_down = true;
                        } else {
                            pending_up = true;
                        }
                    }
                }
                EV_SYN => {
                    let is_connected = CONNECTED.load(Ordering::SeqCst);
                    let sender_opt = if is_connected {
                        GLOBAL_TOUCH_TX.lock().unwrap().clone()
                    } else {
                        None
                    };

                    if pending_down {
                        pending_down = false;
                        is_down = true;
                        last_sent_x = cur_x;
                        last_sent_y = cur_y;
                        last_send_time = std::time::Instant::now();

                        if let Some(android_send) = sender_opt.as_ref() {
                            send_touch_event(android_send, Wifi::touch_action::Enum::PRESS, cur_x, cur_y);
                        }
                    }

                    if pending_up {
                        pending_up = false;
                        is_down = false;

                        if let Some(android_send) = sender_opt.as_ref() {
                            send_touch_event(android_send, Wifi::touch_action::Enum::RELEASE, last_sent_x, last_sent_y);
                        }
                    } else if is_down {
                        let dx = (cur_x as i32 - last_sent_x as i32).abs();
                        let dy = (cur_y as i32 - last_sent_y as i32).abs();
                        // 16 ms interval (~60 FPS drag tracking) with 3px deadband
                        if (dx >= 3 || dy >= 3) && last_send_time.elapsed() >= std::time::Duration::from_millis(16) {
                            last_sent_x = cur_x;
                            last_sent_y = cur_y;
                            last_send_time = std::time::Instant::now();

                            if let Some(android_send) = sender_opt.as_ref() {
                                send_touch_event(android_send, Wifi::touch_action::Enum::DRAG, cur_x, cur_y);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    });
}

#[async_trait::async_trait]
impl AndroidAutoVideoChannelTrait for ExperAuto {
    async fn receive_video(&self, data: Vec<u8>, _timestamp: Option<u64>) {
        let sender = if let Ok(guard) = self.video_sender.read() {
            guard.clone()
        } else {
            None
        };
        log_nal_types(&data);
        if let Some(tx) = sender {
            // Kuyruk dolarsa rastgele P kare atmak akisi bozar; bunun yerine bir sonraki SPS/IDR'ye kadar
            // tum parcalari atip temiz bir noktadan devam ederiz.
            static NEED_KEY: AtomicBool = AtomicBool::new(false);
            let has_key = nal_types(&data).iter().any(|t| matches!(t, 5 | 7));
            if NEED_KEY.load(Ordering::Relaxed) {
                if !has_key {
                    return;
                }
                NEED_KEY.store(false, Ordering::Relaxed);
            }
            VIDEO_STATS.enqueued.fetch_add(1, Ordering::Relaxed);
            VIDEO_STATS.queued.fetch_add(1, Ordering::Relaxed);
            // Non-blocking try_send: NEVER stall the async protocol event loop,
            // ensuring MediaIndicationAck and Pings are transmitted without delay!
            if tx
                .try_send(VideoChunk {
                    data,
                    enq: std::time::Instant::now(),
                })
                .is_err()
            {
                VIDEO_STATS.queued.fetch_sub(1, Ordering::Relaxed);
                NEED_KEY.store(true, Ordering::Relaxed);
            }
        }
    }

    async fn setup_video(&self) -> Result<(), ()> {
        let (vx, vy, vw, vh) = layout().dst;
        println!(
            "[VIDEO] H.264 -> FFmpeg (yuv420p) -> drm-show (NEON olcekleme) -> {}x{} @ ({},{})",
            vw, vh, vx, vy
        );
        prepare_display();

        // Kill any previous pipeline if present
        {
            let mut s = self.inner.lock().await;
            if let Some(mut p) = s.video_proc.take() {
                let _ = p.kill();
                let _ = p.wait();
            }
            if let Some(mut p) = s.display_proc.take() {
                let _ = p.kill();
                let _ = p.wait();
            }
        }

        // FFmpeg: sadece cozer; olcekleme ve renk donusumu drm-show'da (NEON), cunku swscale
        // bu CPU'da 1024x768'e olceklerken ~25-40 fps'te takiliyor.
        let proc = Command::new("ffmpeg")
            .args([
                "-loglevel", "warning",
                "-threads", "2",
                "-flags2", "fast",
                // NOT: "nobuffer" burada kullanilmaz; bu akista (ham H.264, boru) hic kare cikmasina yol aciyor
                // ve ilk goruntu bir sonraki anahtar kareye (~69 sn) kadar gecikiyor.
                "-fflags", "flush_packets+discardcorrupt",
                "-flags", "low_delay",
                "-fpsprobesize", "0",
                "-probesize", "2048",
                "-analyzeduration", "0",
                "-f", "h264",
                "-framerate", "60",
                "-i", "-",
                "-pix_fmt", "yuv420p",
                "-f", "rawvideo",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();

        match proc {
            Ok(mut p) => {
                let mut stdin = p.stdin.take().unwrap();
                let (cx, cy, cw, ch) = layout().crop;
                let display = Command::new("drm-show")
                    .args([
                        "--fmt", "yuv420p",
                        "--src", "800x480",
                        "--crop", &format!("{},{},{},{}", cx, cy, cw, ch),
                        "--dst", &format!("{},{},{},{}", vx, vy, vw, vh),
                    ])
                    .stdin(Stdio::from(p.stdout.take().unwrap()))
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn();
                let display = match display {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("[VIDEO] drm-show baslatilamadi: {}", e);
                        let _ = p.kill();
                        let _ = p.wait();
                        return Ok(());
                    }
                };
                let (tx, mut rx) = tokio::sync::mpsc::channel::<VideoChunk>(VIDEO_QUEUE_CAP);
                VIDEO_STATS.reset();
                VIDEO_ACTIVE.store(true, Ordering::Relaxed);
                let session_gen = VIDEO_GEN.fetch_add(1, Ordering::Relaxed) + 1;
                *self.video_sender.write().unwrap() = Some(tx);

                tokio::task::spawn_blocking(move || {
                    while let Some(chunk) = rx.blocking_recv() {
                        VIDEO_STATS.queued.fetch_sub(1, Ordering::Relaxed);
                        let age_ms = chunk.enq.elapsed().as_millis() as u64;
                        VIDEO_STATS.last_age_ms.store(age_ms, Ordering::Relaxed);
                        VIDEO_STATS.max_age_ms.fetch_max(age_ms, Ordering::Relaxed);

                        if let Err(e) = stdin.write_all(&chunk.data) {
                            if VIDEO_GEN.load(Ordering::Relaxed) == session_gen {
                                eprintln!("[VIDEO] FFmpeg stdin write failed: {} (decoder exited?)", e);
                                VIDEO_STATS.ffmpeg_dead.store(true, Ordering::Relaxed);
                            }
                            break;
                        }
                        let _ = stdin.flush();
                        VIDEO_STATS.written.fetch_add(1, Ordering::Relaxed);
                    }
                    println!("[VIDEO] FFmpeg feeder stopped");
                });

                println!(
                    "[VIDEO] FFmpeg + drm-show pipeline running (queue={} chunks)!",
                    VIDEO_QUEUE_CAP
                );
                let mut s = self.inner.lock().await;
                s.video_proc = Some(p);
                s.display_proc = Some(display);
                Ok(())
            }
            Err(e) => {
                eprintln!("[VIDEO] Failed to spawn FFmpeg display pipeline: {}", e);
                Ok(())
            }
        }
    }

    async fn teardown_video(&self) {
        println!("[VIDEO] Teardown video pipeline");
        VIDEO_ACTIVE.store(false, Ordering::Relaxed);
        *self.video_sender.write().unwrap() = None;
        let mut s = self.inner.lock().await;
        if let Some(mut p) = s.video_proc.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
        // ffmpeg kapaninca drm-show stdin EOF alir ve plane'i kapatip cikar; takili kalirsa oldur
        if let Some(mut p) = s.display_proc.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
    }

    async fn wait_for_focus(&self) {}
    async fn set_focus(&self, _focus: bool) {}

    fn retrieve_video_configuration(&self) -> &VideoConfiguration {
        &self.video_config
    }
}

#[async_trait::async_trait]
impl AndroidAutoAudioOutputTrait for ExperAuto {
    async fn open_output_channel(&self, t: AudioChannelType) -> Result<(), ()> {
        println!("[AUDIO] Opening audio channel: {:?}", t);
        if let AudioChannelType::Media = t {
            let mut s = self.inner.lock().await;
            if s.audio_proc.is_none() {
                println!("[AUDIO] Starting aplay 48kHz stereo ALSA sink -> 3.5mm AUX...");
                let proc = Command::new("aplay")
                    .args(["-q", "-D", "default", "-r", "48000", "-f", "S16_LE", "-c", "2", "-"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn();
                if let Ok(mut p) = proc {
                    let mut stdin = p.stdin.take().unwrap();
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(30);
                    *self.audio_sender.write().unwrap() = Some(tx);

                    tokio::task::spawn_blocking(move || {
                        while let Some(chunk) = rx.blocking_recv() {
                            if stdin.write_all(&chunk).is_err() {
                                break;
                            }
                        }
                    });

                    println!("[AUDIO] ALSA audio output active!");
                    s.audio_proc = Some(p);
                }
            }
        }
        Ok(())
    }

    async fn close_output_channel(&self, t: AudioChannelType) -> Result<(), ()> {
        println!("[AUDIO] Closing audio channel: {:?}", t);
        if let AudioChannelType::Media = t {
            *self.audio_sender.write().unwrap() = None;
            let mut s = self.inner.lock().await;
            if let Some(mut p) = s.audio_proc.take() {
                let _ = p.kill();
                let _ = p.wait();
            }
        }
        Ok(())
    }

    async fn receive_output_audio(&self, t: AudioChannelType, data: Vec<u8>) {
        if let AudioChannelType::Media = t {
            if let Ok(guard) = self.audio_sender.read() {
                if let Some(tx) = guard.as_ref() {
                    let _ = tx.try_send(data);
                }
            }
        }
    }

    async fn start_output_audio(&self, _t: AudioChannelType) {}
    async fn stop_output_audio(&self, _t: AudioChannelType) {}
}

#[async_trait::async_trait]
impl AndroidAutoAudioInputTrait for ExperAuto {
    async fn open_input_channel(&self) -> Result<(), ()> { Ok(()) }
    async fn close_input_channel(&self) -> Result<(), ()> { Ok(()) }
    async fn start_input_audio(&self) {}
    async fn stop_input_audio(&self) {}
    async fn audio_input_ack(&self, _chan: u8, _ack: Wifi::AVMediaAckIndication) {}
}

#[async_trait::async_trait]
impl AndroidAutoInputChannelTrait for ExperAuto {
    async fn binding_request(&self, _code: u32) -> Result<(), ()> { Ok(()) }
    fn retrieve_input_configuration(&self) -> &InputConfiguration {
        &self.input_config
    }
}

#[async_trait::async_trait]
impl AndroidAutoSensorTrait for ExperAuto {
    fn get_supported_sensors(&self) -> &SensorInformation {
        &self.sensor_config
    }

    async fn start_sensor(&self, stype: Wifi::sensor_type::Enum) -> Result<(), ()> {
        println!("[SENSOR] Phone requested sensor: {:?}", stype);
        if self.sensor_config.sensors.contains(&stype) {
            let mut m3 = Wifi::SensorEventIndication::new();
            match stype {
                Wifi::sensor_type::Enum::DRIVING_STATUS => {
                    let mut ds = Wifi::DrivingStatus::new();
                    ds.set_status(Wifi::DrivingStatusEnum::UNRESTRICTED as i32);
                    m3.driving_status.push(ds);
                }
                Wifi::sensor_type::Enum::NIGHT_DATA => {
                    let mut nm = Wifi::NightMode::new();
                    nm.set_is_night(false);
                    m3.night_mode.push(nm);
                }
                _ => {}
            }
            let s = self.inner.lock().await;
            let m = AndroidAutoMessage::Sensor(m3);
            let _ = s.android_send.send(m.sendable()).await;
            Ok(())
        } else {
            Err(())
        }
    }
}

#[async_trait::async_trait]
impl AndroidAutoMainTrait for ExperAuto {
    async fn connect(&self) {
        println!("============================================================");
        println!("  [***] ANDROID AUTO PROJECTION ACTIVATED! [***]            ");
        println!("  Head Unit is now projecting fullscreen to tablet LCD      ");
        println!("  Audio sink connected to 3.5mm stereo AUX port             ");
        println!("============================================================");
        CONNECTED.store(true, Ordering::SeqCst);
        let mut s = self.inner.lock().await;
        s.connected = true;
        *GLOBAL_TOUCH_TX.lock().unwrap() = Some(s.android_send.clone());
    }

    async fn disconnect(&self) {
        println!("[!] Android Auto Disconnected!");
        CONNECTED.store(false, Ordering::SeqCst);
        *GLOBAL_TOUCH_TX.lock().unwrap() = None;
        VIDEO_ACTIVE.store(false, Ordering::Relaxed);
        *self.video_sender.write().unwrap() = None;
        *self.audio_sender.write().unwrap() = None;

        let mut s = self.inner.lock().await;
        s.connected = false;
        if let Some(mut p) = s.video_proc.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
        if let Some(mut p) = s.audio_proc.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
    }

    async fn get_receiver(&self) -> Option<tokio::sync::mpsc::Receiver<SendableAndroidAutoMessage>> {
        let mut s = self.inner.lock().await;
        s.android_recv.take()
    }
}

/// Logs video pipeline health every 2 seconds while a projection is active: ingestion rate vs.
/// playback rate, queue depth, actual display lag (chunk age), drops, FFmpeg liveness, CPU
/// frequency and load. This is the primary diagnostic for video-latency issues.
fn spawn_video_stats_reporter() {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        // Consume the immediate first tick so rates are measured over a full 2 s window.
        tick.tick().await;
        let mut prev: (u64, u64) = (0, 0);
        loop {
            tick.tick().await;
            if !VIDEO_ACTIVE.load(Ordering::Relaxed) {
                prev = (0, 0);
                continue;
            }
            let enq = VIDEO_STATS.enqueued.load(Ordering::Relaxed);
            let wr = VIDEO_STATS.written.load(Ordering::Relaxed);
            let in_rate = (enq.saturating_sub(prev.0)) as f64 / 2.0;
            let out_rate = (wr.saturating_sub(prev.1)) as f64 / 2.0;
            prev = (enq, wr);

            let queued = VIDEO_STATS.queued.load(Ordering::Relaxed);
            let last_age = VIDEO_STATS.last_age_ms.load(Ordering::Relaxed);
            let max_age = VIDEO_STATS.max_age_ms.load(Ordering::Relaxed);
            let ffmpeg_state = if VIDEO_STATS.ffmpeg_dead.load(Ordering::Relaxed) {
                "DEAD"
            } else {
                "ok"
            };
            let freq =
                std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq")
                    .map(|s| format!("{} kHz", s.trim()))
                    .unwrap_or_else(|_| "n/a".to_string());
            let load = std::fs::read_to_string("/proc/loadavg")
                .map(|s| s.split_whitespace().next().unwrap_or("?").to_string())
                .unwrap_or_else(|_| "?".to_string());
            println!(
                "[VIDEO] in={:.1}/s out={:.1}/s queue={}/{} lag last={}ms max={}ms ffmpeg={} cpu0={} load={}",
                in_rate, out_rate, queued, VIDEO_QUEUE_CAP, last_age, max_age, ffmpeg_state,
                freq, load
            );
        }
    });
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("============================================================");
    println!("  Exper Easypad 8 Wireless Android Auto Head Unit Service   ");
    println!("============================================================");

    spawn_touch_worker();
    spawn_video_stats_reporter();

    let setup = android_auto::setup();
    let config = AndroidAutoConfiguration {
        unit: HeadUnitInfo {
            name: "Exper-Auto".to_string(),
            car_model: "Easypad 8 T8E".to_string(),
            car_year: "2026".to_string(),
            car_serial: "T8E001".to_string(),
            left_hand: true,
            head_manufacturer: "Exper".to_string(),
            head_model: "T8E".to_string(),
            sw_build: "7.2.0".to_string(),
            sw_version: "1.7.0".to_string(),
            native_media: true,
            hide_clock: Some(false),
        },
        custom_certificate: None,
    };

    /// Telefonun Android Auto sunucusunun adresi. Oncelik:
    /// 1) EXPER_PHONE_ADDR (ip veya ip:port), 2) wlan0 varsayilan agi gecidi (tablet telefonun hotspot'una
    /// baglandiginda telefon = gateway), 3) kendi AP'miz (dnsmasq kira kaydi), 4) 192.168.43.40.
    fn get_phone_target() -> String {
        if let Ok(a) = std::env::var("EXPER_PHONE_ADDR") {
            return if a.contains(':') { a } else { format!("{}:5277", a) };
        }
        if let Ok(route) = std::fs::read_to_string("/proc/net/route") {
            for line in route.lines().skip(1) {
                let f: Vec<&str> = line.split_whitespace().collect();
                // Iface Destination Gateway Flags ...; varsayilan rota: Destination == 00000000
                if f.len() > 3 && f[0] == "wlan0" && f[1] == "00000000" {
                    if let Ok(g) = u32::from_str_radix(f[2], 16) {
                        // /proc/net/route adresleri little-endian hex yazar
                        let b = g.to_le_bytes();
                        return format!("{}.{}.{}.{}:5277", b[0], b[1], b[2], b[3]);
                    }
                }
            }
        }
        if let Ok(content) = std::fs::read_to_string("/var/lib/misc/dnsmasq.leases") {
            for line in content.lines().rev() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 && parts[2].starts_with("192.168.43.") {
                    return format!("{}:5277", parts[2]);
                }
            }
        }
        "192.168.43.40:5277".to_string()
    }

    loop {
        let target = get_phone_target();
        println!("[*] Probing phone head unit server on {}...", target);

        // Bounded 1.5s timeout on connect:
        // Prevents the Linux kernel TCP SYN retransmission loop from blocking
        // for 63-65 seconds (1m 5s!) when the phone is not ready yet.
        match tokio::time::timeout(
            Duration::from_millis(1500),
            tokio::net::TcpStream::connect(&target),
        )
        .await
        {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                // Cap TCP socket buffers to 64 KB to completely eliminate Linux kernel bufferbloat
                // (prevents multi-megabyte / multi-second socket backlogs during high bitrate video).
                use std::os::unix::io::AsRawFd;
                let fd = stream.as_raw_fd();
                let buf_size: libc::c_int = 65536;
                unsafe {
                    let _ = libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_RCVBUF,
                        &buf_size as *const _ as *const libc::c_void,
                        std::mem::size_of_val(&buf_size) as libc::socklen_t,
                    );
                    let _ = libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_SNDBUF,
                        &buf_size as *const _ as *const libc::c_void,
                        std::mem::size_of_val(&buf_size) as libc::socklen_t,
                    );
                }

                println!("[+] Connected to phone at {}! Starting Android Auto handshake...", target);

                let (send, recv) = tokio::sync::mpsc::channel(100);
                let app = Box::new(ExperAuto::new(send, recv));

                if let Err(e) = android_auto::run_tcp_client(stream, config.clone(), &setup, &app).await {
                    eprintln!("[-] Session ended with error: {:?}", e);
                }
                println!("[*] Connection closed. Reconnecting in 1 second...");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Ok(Err(_)) | Err(_) => {
                // Connection refused or timed out; poll again in 500ms for instantaneous connect
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}
