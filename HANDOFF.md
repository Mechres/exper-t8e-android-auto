# Exper Easypad 8 Wireless Android Auto Head Unit — Engineering Handoff

## 1. Project Overview & Objective
This project transforms the **Exper Easypad 8 (T8E)**—an Allwinner A31s (sun6i) quad-core Cortex-A7 tablet running Alpine Linux 3.20 on mainline Linux 7.2—into a dedicated in-car **Wireless Android Auto head unit**. 

- **Display:** Fullscreen edge-to-edge rendering on the native 8.0-inch $1024 \times 768$ LCD via Linux SimpleFB (`/dev/fb0`).
- **Input:** Capacitive multi-touch input via Goodix GT911 polled directly over I2C (`/dev/i2c-0` @ `0x14`).
- **Audio Output:** Low-latency 48 kHz stereo audio routed through the tablet's 3.5mm analog headphone jack directly into the car stereo AUX port.
- **Connectivity:** Autonomous in-car Wi-Fi hotspot (`Exper-Auto`) enabling direct TCP-based wireless Android Auto link to the driver's phone.

Repository path: `~/Projeler/exper-t8e-android-auto`

---

## 2. Hardware & Network Topology

```
+--------------------------------------------------------------------------------+
|                        EXPER EASYPAD 8 (HEAD UNIT)                             |
|                                                                                |
|  Allwinner A31s (4x Cortex-A7 @ 1.0 GHz) - 1 GB DDR3 - Alpine Linux 3.20 (7.2) |
|                                                                                |
|  +---------------------------+       +--------------------------------------+  |
|  |   8" LCD (1024x768)       |       |   Goodix GT911 Touchscreen           |  |
|  |   /dev/fb0 (32-bit BGRA)  |       |   /dev/i2c-0 @ 0x14 (Polled 66 Hz)   |  |
|  +-------------^-------------+       +------------------v-------------------+  |
|                |                                        |                      |
|          FFmpeg Decoder                           exper-auto                   |
|                |                                        |                      |
|  +-------------+-------------+       +------------------v-------------------+  |
|  |   3.5mm Stereo AUX Port   |       |   Wi-Fi Access Point (AP6210/BCM43362)| |
|  |   aplay 48kHz S16_LE      |<------+   SSID: Exper-Auto                   |  |
|  +---------------------------+       |   IP: 192.168.43.1/24 (dnsmasq/dhcp) |  |
|                                      +------------------^-------------------+  |
+---------------------------------------------------------|----------------------+
                                                          | Wi-Fi (802.11 b/g/n)
                                                          v
                                       +--------------------------------------+
                                       |            DRIVER'S PHONE            |
                                       |   Android Auto Head Unit Server      |
                                       |   IP: 192.168.43.40 : Port 5277      |
                                       +--------------------------------------+
```

### Network Configuration
- **Wi-Fi Hotspot (Tablet AP):**
  - SSID: `Exper-Auto`
  - WPA2 Passphrase: `experauto2026`
  - Gateway IP: `192.168.43.1` (netmask `255.255.255.0`)
  - DHCP Range: `192.168.43.10` – `192.168.43.50` (managed by `dnsmasq`)
  - Config files: `/etc/exper-auto/hostapd.conf`, `/etc/exper-auto/dnsmasq.conf`
- **Driver's Phone (Client):**
  - Static DHCP lease / assigned IP: `192.168.43.40`
  - Android Auto Head Unit Server Port: `5277` (TCP)
- **Host PC Debugging Link (USB Gadget):**
  - Tablet IP: `172.16.42.1` (`usb0`)
  - Host PC IP: `172.16.42.2`
  - SSH Access: `sshpass -p exper ssh root@172.16.42.1`

---

## 3. Architecture & Software Stack

### 3.1 Sub-crates in Repository
- **`src/exper-auto/`**:
  - The main application binary compiled for `armv7-unknown-linux-musleabihf`.
  - Implements head unit lifecycle traits: `AndroidAutoMainTrait`, `AndroidAutoVideoChannelTrait`, `AndroidAutoAudioOutputTrait`, `AndroidAutoInputChannelTrait`, and `AndroidAutoSensorTrait`.
  - Spawns and manages FFmpeg and `aplay` child processes.
  - Houses the singleton hardware I2C touch poller thread.
- **`src/android-auto/`**:
  - Fork of `android-auto` protocol crate with crucial upstream fixes:
    1. **mTLS 1.2 Handshake:** Configured with a patched `rustls-webpki` to accept Google Automotive Link X.509 v1 client certificates without rejecting on legacy basic constraints.
    2. **TCP Client Interface (`run_tcp_client`):** Connects outbound to phone port 5277 over TCP.
    3. **Expanded Multiplexer Queue:** TLS queue `chan` capacity expanded from 256 to 2048 packets to prevent channel deadlock under continuous video/touch streaming.
    4. **Lifecycle Hooks:** Added `main.connect().await` and `main.disconnect().await` calls inside `handle_client_generic`.

### 3.2 Video Pipeline
- **Stream Negotiation:** Negotiated to 480p H.264 at 30 fps (`Wifi::video_resolution::Enum::_480p`, `video_fps::Enum::_30`).
- **Decoder & Renderer:** Spawned child process running FFmpeg directly writing to framebuffer:
  ```bash
  ffmpeg -loglevel info \
    -f h264 -probesize 32 -analyzeduration 0 \
    -fflags nobuffer -flags low_delay \
    -i - \
    -vf scale=1024:768:flags=fast_bilinear \
    -pix_fmt bgra \
    -f fbdev /dev/fb0
  ```
- **ARM NEON Acceleration (`flags=fast_bilinear`):**
  - Benchmark findings on Cortex-A7 Allwinner A31s:
    - Raw H.264 decode (`-f null`): **235 FPS** (7.8x realtime).
    - `flags=neighbor`: **13-17 FPS** (unoptimized generic C fallback in libswscale, causing 50% frame deficit and massive queue drift).
    - `flags=fast_bilinear`: **30-40 FPS** (activates hand-optimized ARM NEON assembly in libswscale, achieving full 30 FPS realtime to `/dev/fb0` at 1.0x speed).
- **Concurrency & Backpressure Flow Control:**
  - `receive_video` delivers stamped packets (`VideoChunk { data, enq }`) into a `tokio::sync::mpsc::channel` of depth **16** via `tx.send().await`.
  - Android Auto protocol enforces `max_unacked = 1`: `MediaIndicationAck` is only transmitted back to the phone once `receive_video` has enqueued the chunk into the feeder queue. This naturally paces the phone's encoder to match the tablet's playback rate with zero packet drops.
- **Latency Diagnostics (`[VIDEO]` stats line, every 2 s while projecting):**
  ```
  [VIDEO] in=30.0/s out=30.0/s queue=1/16 lag last=5ms max=18ms ffmpeg=ok cpu0=n/a load=1.40
  ```

### 3.3 Audio Pipeline
- **Channel:** Media audio channel (`AudioChannelType::Media`).
- **Sink:** Spawned child process feeding ALSA:
  ```bash
  aplay -q -D default -r 48000 -f S16_LE -c 2 -
  ```
- **Auxiliary Output:** Automatically routes to headphone jack. ALSA mixer master playback volume unmuted and set to 100%.

### 3.2 Network Transport & TLS Multiplexing (`libandroid_auto`)
- **Transport:** Multiplexed TCP stream (`StreamMux`) carrying encrypted TLS traffic.
- **Bufferbloat & Latency Elimination:**
  - **Kernel Socket Level:** `SO_RCVBUF` and `SO_SNDBUF` are explicitly clamped to `65536` (64 KB) on the connected `TcpStream`. This disables Linux TCP auto-tuning from expanding socket buffers up to 6 MB, capping maximum kernel network buffering to $\le 50$ ms.
  - **TLS Worker Queue Prioritization (`StreamMux`):** In `src/android-auto/src/ssl.rs`, outbound messages (`PlainData` for touch/sensors, `Frame` for ACKs/ping) and inbound encrypted frames (`DecryptMe`) were previously multiplexed into a single 2048-frame queue. Under high-bitrate video, touch events were queued behind hundreds of inbound video frames, producing a 2-second round-trip delay (`Ping response = 1.69s`).
  - **Fix:** Inbound and outbound channels are decoupled. `SslStreamThread::run` uses `tokio::select! { biased; ... }` to **strictly prioritize outbound traffic**. Whenever a touch event or ACK is queued, it is encrypted and transmitted immediately ($< 1$ ms) before decrypting subsequent video packets.
  - **Inbound Decrypt Queue:** Bounded to 8 frames, enforcing backpressure directly to the phone via TCP window management. Ping dropped from 1,689 ms to **2.7 ms**.

### 3.3 Video Decoding Pipeline (H.264 & libswscale)
- **Architecture:** Zero-copy H.264 stream received via `AndroidAutoVideoChannelTrait::receive_video`, fed via bounded Tokio channel (`cap = 1`) into an FFmpeg subprocess decoding directly to `/dev/fb0`.
- **Latency & Backpressure:**
  - `VIDEO_QUEUE_CAP = 1`: By bounding the buffer depth to 1 chunk, Tokio backpressure stalls `tx.send().await` when FFmpeg is busy, which in turn delays Android Auto's `AVMediaAckIndication(max_unacked = 1)`.
  - The driver phone naturally matches the tablet's real-time rendering rate without accumulating multi-second frame queues (`lag = 0ms`, `queue = 0/1`).
- **Decoder & SIMD Scaler Flags:**
  - FFmpeg invocation:
    ```bash
    ffmpeg -loglevel info -threads 3 -f h264 -probesize 32 -analyzeduration 0 \
      -fflags nobuffer -flags low_delay -i - \
      -vf scale=1024:768:flags=fast_bilinear -pix_fmt bgra -f fbdev /dev/fb0
    ```
  - Fast bilinear scaling uses hand-optimized ARM NEON assembly routines, rendering at 30-55 FPS realtime on Cortex-A7.

### 3.4 Touchscreen Input Pipeline
- **Hardware Controller:** Goodix GT911 managed by the Linux kernel `Goodix-TS` driver on I2C bus 0 address `0x5d`.
- **Driver Model:** Standard Linux `evdev` event stream read directly from `/dev/input/event0` (`Goodix Capacitive TouchScreen`). Interrupt-driven, non-blocking zero CPU polling overhead.
- **Coordinates & Resolution Mapping:**
  - Physical LCD panel & GT911 hardware report:
    $$\text{X}_{\text{panel}} \in [0 \dots 1024], \quad \text{Y}_{\text{panel}} \in [0 \dots 768]$$
  - Android Auto negotiated video surface: $800 \times 480$.
  - `InputConfiguration.touchscreen` is advertised as `Some((800, 480))` matching the video canvas.
  - Runtime scaling formula:
    $$\text{X}_{\text{AA}} = \min\left(799, \left\lfloor\frac{\text{X}_{\text{panel}} \times 800 + 512}{1024}\right\rfloor\right)$$
    $$\text{Y}_{\text{AA}} = \min\left(479, \left\lfloor\frac{\text{Y}_{\text{panel}} \times 480 + 384}{768}\right\rfloor\right)$$
- **Android Auto Event Compatibility:**
  - `TouchAction::PRESS` (0, Android `ACTION_DOWN`) and `TouchAction::RELEASE` (1, Android `ACTION_UP`) with `TouchAction::DRAG` (2, Android `ACTION_MOVE`).
  - Instant dispatch without synchronous thread sleeps to prevent evdev queue backlog.
  - `disp_channel` omitted so the event routes to the primary display.
- **Drag Tracking:**
  - `DRAG` events filtered to $\Delta X \ge 3$ or $\Delta Y \ge 3$ pixels with 16 ms intervals (~60 FPS drag tracking).

### 3.5 SMP 4-Core CPU Activation
- **Kernel State:** Allwinner A31s is now operating with **all 4 Cortex-A7 cores active** (`nproc = 4`, `BogoMIPS = 192.00`).
- **Configuration:**
  - Removed `nosmp` from `/boot/boot.cmd` and regenerated `/boot/boot.scr`.
  - Recompiled kernel `zImage` with `CONFIG_CMDLINE_FORCE=n` and `CONFIG_CMDLINE_EXTEND=y`.
  - Removed `operating-points` from `cpu0` in Device Tree so `cpufreq-dt` exits cleanly (code 19) without crashing during multi-core boot.

---

## 4. Automation & OpenRC Boot Configuration

The tablet is fully autonomous and launches the head unit stack on power-up without requiring a host PC or manual terminal commands:

1. **Screen & Backlight (`/etc/local.d/01-backlight.start`):**
   Calls `/usr/local/bin/exper-backlight 100` on boot, powering on PG12 LCD logic and PA25/PWM0 backlight.
2. **Wi-Fi Hotspot (`/etc/local.d/02-wifi-ap.start`):**
   Calls `/usr/local/bin/wifi-mode ap`, configuring `wlan0` to `192.168.43.1` and starting `hostapd` and `dnsmasq`.
3. **Head Unit Daemon (`/etc/local.d/03-exper-auto.start`):**
   Launches `/usr/local/bin/exper-auto >> /var/log/exper-auto.log 2>&1 &`.
4. **Physical Power Key Toggle (`/usr/local/bin/exper-powerd`):**
   Listens on AXP221 PEK power key. Short press instantly toggles the screen and backlight off / on.

---

## 5. Build & Deployment Instructions

### Toolchain Requirements
- Cross-compiler: ARMv7 hard-float musl/glibc toolchain located at:
  `~/Projeler/telefon-canlandirma/piranha-a10/toolchain/armv7-eabihf--glibc--stable-2024.05-1/bin/`
- Target: `armv7-unknown-linux-musleabihf`

### Compiling `exper-auto`
```bash
export CC_armv7_unknown_linux_musleabihf="~/Projeler/telefon-canlandirma/piranha-a10/toolchain/armv7-eabihf--glibc--stable-2024.05-1/bin/arm-linux-gcc"
export AR_armv7_unknown_linux_musleabihf="~/Projeler/telefon-canlandirma/piranha-a10/toolchain/armv7-eabihf--glibc--stable-2024.05-1/bin/arm-linux-ar"

cd ~/Projeler/exper-t8e-android-auto/src/exper-auto
cargo build --release --target armv7-unknown-linux-musleabihf \
  --config 'target.armv7-unknown-linux-musleabihf.linker="~/Projeler/telefon-canlandirma/piranha-a10/toolchain/armv7-eabihf--glibc--stable-2024.05-1/bin/arm-linux-gcc"'

~/Projeler/telefon-canlandirma/piranha-a10/toolchain/armv7-eabihf--glibc--stable-2024.05-1/bin/arm-linux-strip \
  target/armv7-unknown-linux-musleabihf/release/exper-auto
```

### Deploying to Tablet
```bash
sshpass -p exper scp target/armv7-unknown-linux-musleabihf/release/exper-auto root@172.16.42.1:/usr/local/bin/exper-auto
```

---

## 6. How to Connect & Run in the Vehicle

1. **Power on Tablet:** Connect 5V power to the tablet DC barrel jack or USB OTG port. The tablet will boot Alpine Linux on its MicroSD card.
2. **Autonomous Startup:** Within ~25 seconds, the LCD backlight turns on, the `Exper-Auto` Wi-Fi hotspot becomes visible, and `exper-auto` enters its connection loop.
3. **Connect Phone:**
   - On the driver's phone, connect Wi-Fi to SSID: **`Exper-Auto`** (Password: `experauto2026`).
   - The phone will obtain IP `192.168.43.40`.
4. **Start Projection:**
   - In Android Auto developer settings on the phone, tap **"Start head unit server"**.
   - `exper-auto` detects port 5277, completes mTLS handshake, and immediately projects Android Auto (Google Maps, Spotify, Navigation) fullscreen on the tablet.
5. **Audio:** Plug the car's 3.5mm AUX analog cable into the tablet's headphone jack.

---

## 7. Troubleshooting & Diagnostic Cheatsheet

| Symptom | Diagnostic Step | Root Cause & Resolution |
| :--- | :--- | :--- |
| **Phone won't connect** | `sshpass -p exper ssh root@172.16.42.1 "nc -zv 192.168.43.40 5277"` | Verify phone is connected to `Exper-Auto` hotspot and Head Unit Server is running in Android Auto developer settings. |
| **Screen stays black** | `sshpass -p exper ssh root@172.16.42.1 "exper-backlight 100"` | Check if backlight / LCD power (PG12) was turned off by power key daemon. |
| **Video freezing or lag** | Check `cat /var/log/exper-auto.log` for ping latency | With 4 cores active and `flags=neighbor`, ping latency should stay below 15 ms. If backlog occurs, verify 512-queue is draining. |
| **Touch unresponsive** | `tail -f /var/log/exper-auto.log \| grep TOUCH` | Check if `[TOUCH] Sent PRESS/DRAG` messages appear. Check `/dev/i2c-0` permissions and I2C address `0x14`. |
| **Audio silent** | `sshpass -p exper ssh root@172.16.42.1 "amixer set 'Master' 100% unmute"` | Ensure ALSA Master channel is unmuted and headphone cable is fully seated in 3.5mm jack. |
| **Video picture delayed (seconds/minutes behind reality)** | `sshpass -p exper ssh root@172.16.42.1 "grep '\[VIDEO\]' /var/log/exper-auto.log \| tail -20"` | Compare `in=/s` vs `out=/s` and `lag max=`. If `in > out` persistently, FFmpeg cannot decode/scale at 30 fps: check `ffmpeg=`'s `frame=... fps=...` line in the log, `cpu0` frequency (may be stuck low — cpufreq-dt not bound), and `top` for FFmpeg CPU%. The queue is now capped at 64 chunks with keyframe resync, so lag is bounded even when the decoder is slow. |
| **Video freezes briefly, then jumps to live** | Look for `[VIDEO] Decoder behind ... resyncing at next keyframe` | Expected behavior of the anti-lag policy: the stale backlog is flushed and the stream resyncs at the next SPS/IDR. Frequent occurrences mean FFmpeg is chronically too slow — see the row above. |
| **Video stalls permanently** | `sshpass -p exper ssh root@172.16.42.1 "grep 'FFmpeg' /var/log/exper-auto.log \| tail"` | `ffmpeg=DEAD` / `FFmpeg stdin write failed` → the decoder crashed; check FFmpeg's own stderr output just above that line in the log. |
| **Verify CPU Cores** | `sshpass -p exper ssh root@172.16.42.1 "nproc && cat /sys/devices/system/cpu/online"` | Output must report `4` and `0-3`. If single core, ensure `/boot/boot.scr` and `zImage` were not replaced with `nosmp` builds. |

---

## 8. Status & Latency Resolution Summary

The root causes of the 2–3 second touch and video latency have been identified and resolved:

1. **Decoder Bottleneck Eliminated (56 FPS vs 22 FPS):**
   - Replaced FFmpeg CPU bilinear scaler (`-vf scale=1024:768:flags=fast_bilinear`) with native 1:1 padded rendering (`-vf pad=1024:768:112:144`).
   - Software scaling of $800 \times 480 \rightarrow 1024 \times 768$ saturated memory bandwidth on Cortex-A7 (capped at 22 FPS vs 30 FPS stream rate), creating a permanent accumulating backlog. Native padding runs at **56 FPS**, completely eliminating decode lag.
   - Exact 1:1 touch coordinate mapping:
     $$X_{\text{aa}} = (X_{\text{raw}} - 112).\text{clamp}(0, 799)$$
     $$Y_{\text{aa}} = (Y_{\text{raw}} - 144).\text{clamp}(0, 479)$$

2. **Decoupled Outbound Prioritization in TLS Transport:**
   - In [`src/android-auto/src/ssl.rs`](file://~/Projeler/exper-t8e-android-auto/src/android-auto/src/ssl.rs), outbound messages (touch events, pings, ACKs) now use a prioritized queue with `tokio::select! { biased; ... }`.
   - Touch events and ACKs leave the socket immediately (<1 ms) without waiting behind inbound encrypted video frames.

3. **Audio Sink Flow Control & ACKs Implemented:**
   - In [`src/android-auto/src/mediaaudio.rs`](file://~/Projeler/exper-t8e-android-auto/src/android-auto/src/mediaaudio.rs), `speechaudio.rs`, and `sysaudio.rs`, session tracking was added and `MediaIndicationAck` is now transmitted for every received audio frame.
   - Prevents the phone's Android Auto projection engine from exhausting the `max_unacked = 10` flow-control window and stalling media/video sync.

4. **Bounded Queue & Bufferbloat Prevention:**
   - `VIDEO_QUEUE_CAP` set to 4 chunks (~70 ms jitter cushion).
   - TCP socket `SO_RCVBUF` / `SO_SNDBUF` capped to 64 KB with `TCP_NODELAY`.

The updated binary is cross-compiled, stripped, and deployed to the tablet (`/usr/local/bin/exper-auto`, active PID running).

To verify real-time responsiveness:
1. Connect phone to Wi-Fi SSID **`Exper-Auto`** (Password: `experauto2026`).
2. In Android Auto developer settings on the phone, tap **"Start head unit server"**.
3. Confirm instantaneous touch feedback on the centered $800 \times 480$ canvas with ping response <15 ms and solid 30+ FPS video.
