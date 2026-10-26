use std::collections::HashSet;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Mutex;

static CONNECTED: AtomicBool = AtomicBool::new(false);
static GLOBAL_TOUCH_TX: std::sync::Mutex<Option<tokio::sync::mpsc::Sender<SendableAndroidAutoMessage>>> =
    std::sync::Mutex::new(None);

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
    audio_proc: Option<Child>,
}

#[derive(Clone)]
struct ExperAuto {
    inner: Arc<Mutex<ExperAutoInner>>,
    video_sender: Arc<RwLock<Option<tokio::sync::mpsc::Sender<Vec<u8>>>>>,
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
                audio_proc: None,
            })),
            video_sender: Arc::new(RwLock::new(None)),
            audio_sender: Arc::new(RwLock::new(None)),
            video_config: VideoConfiguration {
                resolution: Wifi::video_resolution::Enum::_480p,
                fps: Wifi::video_fps::Enum::_30,
                dpi: 160,
            },
            input_config: InputConfiguration {
                keycodes: vec![],
                touchscreen: Some((1024, 768)),
            },
            sensor_config: SensorInformation { sensors },
        }
    }
}

fn spawn_touch_worker() {
    tokio::task::spawn_blocking(move || {
        use std::os::unix::io::AsRawFd;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/i2c-0")
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[TOUCH] Failed to open /dev/i2c-0: {}", e);
                return;
            }
        };

        let fd = file.as_raw_fd();
        const I2C_SLAVE: libc::c_int = 0x0703;
        let res = unsafe { libc::ioctl(fd, I2C_SLAVE, 0x14 as libc::c_int) };
        if res < 0 {
            eprintln!("[TOUCH] Failed to set I2C_SLAVE 0x14: {}", std::io::Error::last_os_error());
            return;
        }

        println!("[TOUCH] Goodix GT911 hardware touch poller active on /dev/i2c-0 @ 0x14");

        let mut was_down = false;
        let mut last_x = 512;
        let mut last_y = 384;
        let mut last_send_time = std::time::Instant::now();

        loop {
            std::thread::sleep(std::time::Duration::from_millis(15));

            let reg_buf = [0x81u8, 0x4Eu8];
            let wr = unsafe {
                libc::write(fd, reg_buf.as_ptr() as *const libc::c_void, 2)
            };
            if wr != 2 {
                continue;
            }

            let mut read_buf = [0u8; 8];
            let rd = unsafe {
                libc::read(fd, read_buf.as_mut_ptr() as *mut libc::c_void, 8)
            };
            if rd != 8 {
                continue;
            }

            let status = read_buf[0];
            if (status & 0x80) != 0 {
                let is_connected = CONNECTED.load(Ordering::SeqCst);
                let sender_opt = if is_connected {
                    GLOBAL_TOUCH_TX.lock().unwrap().clone()
                } else {
                    None
                };

                if sender_opt.is_none() {
                    let clear_buf = [0x81u8, 0x4Eu8, 0x00u8];
                    let _ = unsafe {
                        libc::write(fd, clear_buf.as_ptr() as *const libc::c_void, 3)
                    };
                    was_down = false;
                    continue;
                }

                let android_send = sender_opt.unwrap();
                let points = status & 0x0F;
                if points > 0 {
                    let raw_x = (read_buf[2] as u32) | ((read_buf[3] as u32) << 8);
                    let raw_y = (read_buf[4] as u32) | ((read_buf[5] as u32) << 8);

                    // 1:1 hardware to 1024x768 touchscreen mapping
                    // Goodix has swapped X and Y:
                    // X_screen is raw_y (0..1024)
                    // Y_screen is raw_x (0..768)
                    let x = raw_y.clamp(0, 1024);
                    let y = raw_x.clamp(0, 768);

                    if !was_down {
                        was_down = true;
                        last_x = x;
                        last_y = y;
                        last_send_time = std::time::Instant::now();

                        let mut ev = Wifi::InputEventIndication::new();
                        ev.set_timestamp(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_micros() as u64,
                        );
                        ev.set_disp_channel(0);

                        let mut touch = Wifi::TouchEvent::new();
                        touch.set_touch_action(Wifi::touch_action::Enum::PRESS);
                        touch.set_action_index(0);

                        let mut loc = Wifi::TouchLocation::new();
                        loc.set_pointer_id(0);
                        loc.set_x(x);
                        loc.set_y(y);
                        touch.touch_location.push(loc);

                        ev.touch_event = ::protobuf::MessageField::some(touch);

                        let _ = android_send.try_send(AndroidAutoMessage::Input(ev).sendable());
                        println!("[TOUCH] Sent PRESS at X={} Y={}", x, y);
                    } else {
                        let dx = (x as i32 - last_x as i32).abs();
                        let dy = (y as i32 - last_y as i32).abs();
                        if (dx >= 4 || dy >= 4) && last_send_time.elapsed() >= std::time::Duration::from_millis(35) {
                            last_x = x;
                            last_y = y;
                            last_send_time = std::time::Instant::now();

                            let mut ev = Wifi::InputEventIndication::new();
                            ev.set_timestamp(
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_micros() as u64,
                            );
                            ev.set_disp_channel(0);

                            let mut touch = Wifi::TouchEvent::new();
                            touch.set_touch_action(Wifi::touch_action::Enum::DRAG);
                            touch.set_action_index(0);

                            let mut loc = Wifi::TouchLocation::new();
                            loc.set_pointer_id(0);
                            loc.set_x(x);
                            loc.set_y(y);
                            touch.touch_location.push(loc);

                            ev.touch_event = ::protobuf::MessageField::some(touch);

                            let _ = android_send.try_send(AndroidAutoMessage::Input(ev).sendable());
                            println!("[TOUCH] Sent DRAG at X={} Y={}", x, y);
                        }
                    }
                } else if was_down {
                    was_down = false;
                    let mut ev = Wifi::InputEventIndication::new();
                    ev.set_timestamp(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_micros() as u64,
                    );
                    ev.set_disp_channel(0);

                    let mut touch = Wifi::TouchEvent::new();
                    touch.set_touch_action(Wifi::touch_action::Enum::RELEASE);
                    touch.set_action_index(0);

                    let mut loc = Wifi::TouchLocation::new();
                    loc.set_pointer_id(0);
                    loc.set_x(last_x);
                    loc.set_y(last_y);
                    touch.touch_location.push(loc);

                    ev.touch_event = ::protobuf::MessageField::some(touch);

                    let _ = android_send.try_send(AndroidAutoMessage::Input(ev).sendable());
                    println!("[TOUCH] Sent RELEASE at X={} Y={}", last_x, last_y);
                }

                let clear_buf = [0x81u8, 0x4Eu8, 0x00u8];
                let _ = unsafe {
                    libc::write(fd, clear_buf.as_ptr() as *const libc::c_void, 3)
                };
            }
        }
    });
}

#[async_trait::async_trait]
impl AndroidAutoVideoChannelTrait for ExperAuto {
    async fn receive_video(&self, data: Vec<u8>, _timestamp: Option<u64>) {
        if let Ok(guard) = self.video_sender.read() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.try_send(data);
            }
        }
    }

    async fn setup_video(&self) -> Result<(), ()> {
        println!("[VIDEO] Setting up H.264 FFmpeg video decoder -> /dev/fb0 (1024x768 fullscreen)...");
        let _ = std::process::Command::new("exper-backlight").arg("100").status();

        // Kill any previous ffmpeg if present
        {
            let mut s = self.inner.lock().await;
            if let Some(mut p) = s.video_proc.take() {
                let _ = p.kill();
                let _ = p.wait();
            }
        }

        let proc = Command::new("ffmpeg")
            .args([
                "-loglevel", "warning",
                "-f", "h264",
                "-probesize", "32",
                "-analyzeduration", "0",
                "-fflags", "nobuffer",
                "-flags", "low_delay",
                "-i", "-",
                "-vf", "scale=1024:768:flags=neighbor",
                "-pix_fmt", "bgra",
                "-f", "fbdev",
                "/dev/fb0",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn();

        match proc {
            Ok(mut p) => {
                let mut stdin = p.stdin.take().unwrap();
                let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(512);
                *self.video_sender.write().unwrap() = Some(tx);

                tokio::task::spawn_blocking(move || {
                    while let Some(chunk) = rx.blocking_recv() {
                        if stdin.write_all(&chunk).is_err() {
                            break;
                        }
                        let _ = stdin.flush();
                    }
                });

                println!("[VIDEO] FFmpeg display pipeline running successfully!");
                let mut s = self.inner.lock().await;
                s.video_proc = Some(p);
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
        *self.video_sender.write().unwrap() = None;
        let mut s = self.inner.lock().await;
        if let Some(mut p) = s.video_proc.take() {
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("============================================================");
    println!("  Exper Easypad 8 Wireless Android Auto Head Unit Service   ");
    println!("============================================================");

    spawn_touch_worker();

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

    let target_ip = "192.168.43.40";
    let target_port = 5277;
    let target = format!("{}:{}", target_ip, target_port);

    loop {
        println!("[*] Waiting for phone connection on {}...", target);
        match tokio::net::TcpStream::connect(&target).await {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                println!("[+] Connected to phone at {}! Starting Android Auto handshake...", target);

                let (send, recv) = tokio::sync::mpsc::channel(100);
                let app = Box::new(ExperAuto::new(send, recv));

                if let Err(e) = android_auto::run_tcp_client(stream, config.clone(), &setup, &app).await {
                    eprintln!("[-] Session ended with error: {:?}", e);
                }
                println!("[*] Connection closed. Reconnecting in 2 seconds...");
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}
