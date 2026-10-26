# Exper Easypad 8 Wireless Android Auto Head Unit — Engineering Handoff

**Date:** 26 October 2026 · **Target Architecture:** Allwinner A31s (`sun6i`, 4× Cortex-A7 @ 1.008 GHz, 1 GB DDR3)  
**OS:** Alpine Linux 3.20 (Kernel 7.2-rc1) · **Display:** 8.0" 1024×768 LVDS (`/dev/fb0`) · **Touch:** Goodix GT911 (`/dev/input/event0`)  
**Workspace:** `~/Projeler/exper-t8e-android-auto`  
**Target Device IP:** `172.16.42.1` (`usb0`) / `192.168.43.1` (`wlan0` AP)

---

## 1. Project Overview & Objective

Transforms the Exper Easypad 8 (Model T8E) into a dedicated in-car **Wireless Android Auto Head Unit**:
- **Display Pipeline:** H.264 video decoded via low-latency FFmpeg writing directly to Linux SimpleFB (`/dev/fb0`).
- **Touch Input:** Native Linux evdev event stream (`/dev/input/event0`, `Goodix Capacitive TouchScreen`), translating gestures into Android Auto `InputEventIndication` (`PRESS`, `RELEASE`, `DRAG`).
- **Audio Output:** 48 kHz stereo ALSA sink (`aplay`) routed to 3.5mm analog AUX output into car stereo.
- **Wireless Projection:** Tablet runs standalone autonomous Wi-Fi AP (`Exper-Auto`, `192.168.43.1`). Connects to phone's Android Auto Head Unit Server (port 5277) over mTLS.

---

## 2. Hardware Topology & Benchmark Findings

```
+--------------------------------------------------------------------------------+
|                        EXPER EASYPAD 8 (HEAD UNIT)                             |
|                                                                                |
|  Allwinner A31s (4x Cortex-A7 @ 1.008 GHz fixed) - 1 GB DDR3 @ 432 MHz        |
|  SimpleFB (/dev/fb0): 1024x768 32-bit BGRA (DRAM backed, no 2D/3D GPU driver)  |
|                                                                                |
|  +---------------------------+       +--------------------------------------+  |
|  |   8" LCD (1024x768)       |       |   Goodix GT911 Touchscreen           |  |
|  |   /dev/fb0 (32-bit BGRA)  |       |   /dev/input/event0 (evdev, IRQ)     |  |
|  +-------------^-------------+       +------------------v-------------------+  |
|                |                                        |                      |
|          FFmpeg Decoder                           exper-auto                   |
|          (-framerate 60)                                |                      |
|                |                                        |                      |
|  +-------------+-------------+       +------------------v-------------------+  |
|  |   3.5mm Stereo AUX Port   |       |   Wi-Fi Access Point (BCM43362/AP6210)| |
|  |   aplay 48kHz S16_LE      |<------+   SSID: Exper-Auto                   |  |
|  +---------------------------+       |   IP: 192.168.43.1/24 (dnsmasq)      |  |
|                                      +------------------^-------------------+  |
+---------------------------------------------------------|----------------------+
                                                          | Wi-Fi (802.11 b/g/n)
                                                          v
                                       +--------------------------------------+
                                       |            DRIVER'S PHONE            |
                                       |   Android Auto Head Unit Server      |
                                       |   Dynamic IP: 192.168.43.x : 5277    |
                                       +--------------------------------------+
```

### Verified Hardware & Performance Benchmarks (Live on Device):
- **CPU Frequency:** Locked at **1.008 GHz** (`pll-cpu` & `cpu` = 1,008,000,000 Hz in `/sys/kernel/debug/clk/clk_summary`). No cpufreq throttling.
- **SMP Cores:** All **4 cores online** (`0-3`, `nproc = 4`).
- **Software Decoding vs. Scaling Benchmarks (Raw A31s Cortex-A7):**
  - **Raw H.264 decode to null (`-f null`):** **203 FPS** (3.38× realtime).
  - **1:1 Native Decode to fbdev (`800x480` -> `/dev/fb0`):** **104–109 FPS** (1.74× realtime, CPU load ~34%). Zero latency backlog.
  - **Software Upscaling to Fullscreen (`scale=1024:768:flags=fast_bilinear`):** **49 FPS** (0.82× realtime). Falls behind 60 FPS input by 180 ms every second, causing an ever-growing queue and 2–3s touch lag.
  - **Software Upscaling Width-Fit (`scale=1024:614` to fbdev):** **32 FPS** (0.52× realtime). Memory bus bandwidth saturated (~150 MB/s DDR3 traffic).
- **VPU Hardware Decoding (Cedrus / CedarX):**
  - Upstream Linux Cedrus driver does **not** support `sun6i` (A31/A31s). `sun6i-a31.dtsi` has no `video-codec` node, and `sunxi_sram.c` has no SRAM C mapping for A31.
- **GPU (PowerVR SGX544MP2):**
  - No open-source DRM/Mesa driver exists in mainline Linux. Display is driven strictly via U-Boot SimpleFB (`/dev/fb0`).
- **Allwinner Display Engine Hardware Scaler (DE1 / DEFE):**
  - The SoC hardware contains a dedicated display frontend scaler (`sun6i-a31-display-frontend`). Mainline Linux has driver support in `drivers/gpu/drm/sun4i/`, but `CONFIG_DRM_SUN4I` was disabled during initial bring-up in favor of SimpleFB. Enabling DRM KMS is required for hardware-accelerated zero-copy fullscreen scaling.

---

## 3. Critical Architecture Fixes Implemented

### 3.1 69-Second GOP Keyframe Starvation & Focus Bounce
- **Root Cause of the 1–2 Minute Delay:**
  - Google's Android Auto video encoder on the phone operates on a fixed **~69-second GOP (Group of Pictures)** cadence. It only emits an IDR keyframe (SPS/PPS) once every 69 seconds unless specifically provoked.
  - If the initial keyframe was missed while FFmpeg was initializing, FFmpeg received only non-key P-frames, which it cannot decode. FFmpeg silently discarded all frames until the next GOP keyframe:
    - 1 GOP missed = **69 seconds** (~1m 5s).
    - 2 GOPs missed = **138 seconds** (~2m 2s).
- **The Protocol Lever (Keyframe Focus Bounce):**
  - AAP has no explicit "request keyframe" protobuf message.
  - In `src/android-auto/src/video.rs`, on `StartIndication`, we trigger an immediate **Focus Bounce**:
    1. Send `VideoFocusIndication(focus_mode: UNFOCUSED, unrequested: true)`
    2. Wait 200 ms (`tokio::time::sleep`)
    3. Send `VideoFocusIndication(focus_mode: FOCUSED, unrequested: true)`
  - Android's encoder immediately re-initializes and forces an IDR keyframe with SPS/PPS in **0.2–0.5 seconds**.

### 3.2 Video Flow Control Deadlock (`max_unacked`)
- In `src/android-auto/src/video.rs`, `AVChannelSetupResponse.max_unacked` was previously set to `1`.
- At 60 FPS, this enforced a strict stop-and-wait lockstep where the phone waited for an ACK after every single packet. When touch events or audio ACKs contended on the TLS link, the stream deadlocked into `in=0.0/s` for 30 seconds until `BrokenPipe`.
- Increased `max_unacked` to **10** (matching OpenAuto and other audio channels).
- Changed `receive_video` from blocking `tx.send().await` to non-blocking `tx.try_send()`, preventing feeder backpressure from blocking the protocol async loop.

### 3.3 TCP Connect Timeout & Dynamic DHCP Resolution
- In `src/exper-auto/src/main.rs`, `TcpStream::connect` previously blocked on the Linux kernel SYN retransmission timer for **63–65 seconds** if the phone had not yet opened port 5277.
- Wrapped in `tokio::time::timeout(Duration::from_millis(1500))`. Retries every 500 ms. Connects within 0.5s of phone server launch.
- Added dynamic lookup from `/var/lib/misc/dnsmasq.leases` to automatically connect to whatever IP the phone is assigned on Wi-Fi (`192.168.43.x`).

### 3.4 Low-Latency 1:1 Rendering & Touch Alignment
- **FFmpeg Invocation:**
  ```bash
  ffmpeg -loglevel warning -threads 2 -flags2 fast \
    -fflags nobuffer+flush_packets+discardcorrupt -flags low_delay \
    -fpsprobesize 0 -probesize 2048 -analyzeduration 0 \
    -f h264 -framerate 60 -i - \
    -pix_fmt bgra -xoffset 112 -yoffset 144 -f fbdev /dev/fb0
  ```
- **Touch Coordinate Mapping:**
  $X_{\text{aa}} = \text{clamp}(raw_X - 112, 0, 799)$  
  $Y_{\text{aa}} = \text{clamp}(raw_Y - 144, 0, 479)$

---

## 4. Build & Deployment Instructions

### Cross-Toolchain
- Toolchain: `~/Projeler/telefon-canlandirma/piranha-a10/toolchain/armv7-eabihf--glibc--stable-2024.05-1/bin/`
- Target: `armv7-unknown-linux-musleabihf`

### Build Command
```bash
cd ~/Projeler/exper-t8e-android-auto/src/exper-auto
cargo build --release --target armv7-unknown-linux-musleabihf \
  --config 'target.armv7-unknown-linux-musleabihf.linker="~/Projeler/telefon-canlandirma/piranha-a10/toolchain/armv7-eabihf--glibc--stable-2024.05-1/bin/arm-linux-gcc"'

~/Projeler/telefon-canlandirma/piranha-a10/toolchain/armv7-eabihf--glibc--stable-2024.05-1/bin/arm-linux-strip \
  target/armv7-unknown-linux-musleabihf/release/exper-auto
```

### Deploy to Live Tablet
```bash
sshpass -p exper ssh root@172.16.42.1 "pkill -9 exper-auto || true; pkill -9 ffmpeg || true"
sshpass -p exper scp target/armv7-unknown-linux-musleabihf/release/exper-auto root@172.16.42.1:/usr/local/bin/exper-auto
sshpass -p exper ssh root@172.16.42.1 "chmod +x /usr/local/bin/exper-auto; nohup /usr/local/bin/exper-auto >> /var/log/exper-auto.log 2>&1 &"
```

---

## 5. Next Steps / Kernel Roadmap (When Resuming)

1. **Hardware Video Scaling via Display Engine (`sun4i-drm`):**
   - Enable `CONFIG_DRM_SUN4I=y` and compile DE1 Display Frontend (`sun6i-a31-display-frontend`) and Backend drivers.
   - Configure LVDS panel timing in DTS for the 1024×768 panel.
   - This will allow DRM plane scaling in hardware: FFmpeg decodes 800×480 directly into a DRM dumb buffer, and the hardware scaler scales it edge-to-edge on the 1024×768 LCD with 0% CPU overhead.
2. **Cedrus VPU Port Evaluation:**
   - Add DT node for `video-codec@1c0e000` in `sun6i-a31.dtsi`.
   - Add SRAM C claim logic in `drivers/soc/sunxi/sunxi_sram.c`.
   - Evaluate A31 VE register map compatibility against A20/A33.
