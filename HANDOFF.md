# Exper Easypad 8 Wireless Android Auto Head Unit — Engineering Handoff

**Date:** 27 October 2026 (updated) · **Target Architecture:** Allwinner A31s (`sun6i`, 4× Cortex-A7 @ 1.008 GHz, 1 GB DDR3)  
**OS:** Alpine Linux 3.20 (Kernel 7.2-rc1) · **Display:** 8.0" 1024×768 LVDS (`/dev/fb0`) · **Touch:** Goodix GT911 (`/dev/input/event0`)  
**Workspace:** `~/Projeler/exper-t8e-android-auto`  
**Target Device IP:** `172.16.42.1` (`usb0`) / `192.168.43.1` (`wlan0` AP)

---

## 1. Project Overview & Objective

Transforms the Exper Easypad 8 (Model T8E) into a dedicated in-car **Wireless Android Auto Head Unit**:
- **Display Pipeline:** H.264 video decoded via low-latency FFmpeg writing directly to Linux SimpleFB (`/dev/fb0`).
- **Touch Input:** Native Linux evdev event stream (`/dev/input/event0`, `Goodix Capacitive TouchScreen`), translating gestures into Android Auto `InputEventIndication` (`PRESS`, `RELEASE`, `DRAG`).
- **Audio Output:** 48 kHz stereo ALSA sink (`aplay`) routed to 3.5mm analog AUX output into car stereo.
- **Wireless Projection:** Tablet is a Wi-Fi **client of the phone's hotspot** (SSID `<phone hotspot SSID>`, 2.4 GHz) and connects to the phone's Android Auto Head Unit Server (port 5277, phone = default gateway) over mTLS. (The old own-AP design, `Exper-Auto`/`192.168.43.1`, is still available via `exper-ap`/`wifi-mode.sh ap` but is disabled: see section 5b for why.)

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

### 3.4 Video Pipeline: FFmpeg (decode) -> `drm-show` (NEON scale) -> DRM overlay plane

fbdev/swscale path was dropped. Now the kernel runs **sun4i-drm** (DRM KMS) and:

```
phone H.264 --(exper-auto)--> ffmpeg -pix_fmt yuv420p -f rawvideo - | drm-show --fmt yuv420p --src 800x480 --dst 0,77,1024,614
```
- `src/drm-show/` (Rust + `src/yuv_neon.c`): reads raw yuv420p frames, bilinear-scales 800x480 -> dst rect and converts to XRGB with ARMv7 NEON
  (horizontal ratio is exactly 25:32, so `vtbl4` gathers 8 pixels at once), two threads, writes into DRM dumb buffers and
  flips an **overlay plane** from a separate flip thread (mailbox = latest frame wins, stale frames are dropped, no latency build-up).
- Measured on the tablet (real FFmpeg decode of an 800x480 H.264 test stream): **~71 frames/s read, 63 fps end-to-end**,
  limited by the ~55 Hz panel refresh. FFmpeg `swscale` alone managed only 25-40 fps for the same scaling.
- Layout (`EXPER_VIDEO_LAYOUT`, set in `/etc/init.d/exper-auto`; sources in `configs/`):
  - `fit`: whole 800x480 -> 1024x614 centred (aspect kept, 77 px black bars); `full`: stretched to 1024x768; `1024xH`: in between.
  - **`4x3` (current default)**: the phone is asked for `margin_width=160` (`VideoConfig.margin_width`, now configurable in
    `android-auto::VideoConfiguration`). The phone really honours it: the UI is drawn in the centre 640x480 (x=80..720) with black
    side margins. `drm-show --crop 80,0,640,480` cuts that region and scales it 1.6x to 1024x768: right aspect ratio, no bars.
  - **Touch with margins**: the phone expects touch coordinates relative to the UI area *without* the margin (it adds the margin
    itself), and the announced touchscreen size must be the UI size (640x480). `send_touch_event`:
    `x = (crop.x - margin_w/2) + (raw_x - dst.x) * crop.w / dst.w`. (Sending frame coordinates made the left menu icon hit the mic.)
- `prepare_display()` unbinds fbcon, blanks `/dev/fb0` and sets the backlight to max via sysfs (`exper-backlight` no longer exists).
- Needs `cma=64M` on the kernel cmdline (default 16 MB CMA cannot hold 4 x 2.5 MB dumb buffers + fbdev) -> already in `/boot/boot.cmd`.

### 3.4b FFmpeg `-fflags nobuffer` breaks this stream (picture only after ~69 s)
With `-fflags nobuffer` FFmpeg decoded **zero frames** of the phone's raw H.264 until the next IDR (bisected offline with a recorded
stream: `EXPER_DUMP_H264=/tmp/aa.h264`, then replay with `ffmpeg -f h264 -i - ...`). The phone *does* send SPS/PPS+IDR right after the
focus bounce. Flags now: `-fflags flush_packets+discardcorrupt -flags low_delay -probesize 2048 -analyzeduration 0`; picture appears in
~2 s. Debug helpers: `EXPER_DEBUG_H264=1` (NAL types per chunk), `EXPER_DUMP_H264=path` (first ~3 MB of the stream).
- Keyframe focus bounce (`android-auto/src/video.rs`) now runs **once per connection / max once per 5 s until media arrives**: the old
  version bounced on every `StartIndication`, and every bounce makes the phone restart the stream => endless loop, no video.
- `receive_video`: queue 512 chunks; if it ever fills, drop everything until the next SPS/IDR instead of random P frames.

### 3.5 Hardware scaler (DE1 frontend) on A31s: investigated, NOT working
Wired up in this session: `DRM_SUN4I` (module), `sun6i_a31s_quirks` got `supports_lvds` + `has_lvds_alt` + `sun6i_tcon_setup_lvds_phy`
(mainline had no LVDS support for A31/A31s TCON), `allwinner,sun6i-a31-display-frontend` added to `sun4i_frontend_of_table`
(data: A10 quirks; A33 data behaves the same). Frontend registers are programmed correctly (in/out size, scale factors, buffer
addresses, coefficients, FRM_CTRL start) but **the overlay stays empty/black** for RGB and NV12, unscaled and scaled, with FE0 or FE1
(`sun4i-backend frontend_idx=0|1` module parameter, added for the experiment). `FRM_CTRL` keeps REG_RDY/COEF_RDY set (never
latched). No A31 DEFE/DEBE documentation or BSP source available offline. Primary + overlay planes **without** the frontend
(unscaled XRGB) work fine. If revisited: look for the BE input select / FE start handshake in the Allwinner A31 BSP (`de_fe.c`, `de_be.c`).

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

## 5. Tablet / SD card state (rebuilt 26-27 Oct 2026)

The original SD card died (unstable reads). New 4 GB SanDisk built with `Exper Easypad 8/scripts/50-sdcard-alpine.sh` (now fixes
rootfs ownership + setuid bits). Everything else was installed over the USB network (PC does NAT: `ip_forward` + iptables MASQUERADE,
tablet needs `ip route replace default via 172.16.42.2 dev usb0; echo nameserver 1.1.1.1 > /etc/resolv.conf` after every boot).

- Kernel `7.2.0-gf36b54d9862f-dirty`, DTB `sun6i-a31s-exper-t8e-drm.dtb` (boot.scr.drm is the active `boot.scr`; `boot.scr.main` = plain DTB).
  `/boot` is read-only: `mount -o remount,rw /boot` to update.
- DRM modules load at boot from `/etc/modules-load.d/exper-drm.conf` (frontend, backend, drc, drm). LVDS panel: 1024x768, 24-bit VESA
  mapping, 60 MHz pixel clock, 1350x810 total (matches U-Boot `lcd mode`), PWM backlight PH13 + PA25 enable, panel power PG12.
- Packages: ffmpeg, alsa-utils, hostapd, dnsmasq, iw, wpa_supplicant, evtest, libdrm-tests (`modetest`).
- Services (OpenRC, sources in `configs/` and `scripts/`): `exper-wifi` (runs `exper-wifi.sh`: `wpa_supplicant` with
  `/etc/exper-auto/wpa_supplicant.conf` -> phone hotspot `<phone hotspot SSID>` / `<hotspot password>`, `udhcpc`, re-tries when the link drops) and
  `exper-auto` (needs `exper-wifi`; finds the phone as the `wlan0` default gateway, override with `EXPER_PHONE_ADDR`).
  `exper-ap` (hostapd+dnsmasq, SSID `Exper-Auto` / `<ap password>`, 192.168.43.1) is installed but removed from the default runlevel
  (`rc-update add exper-ap default` + `rc-update del exper-wifi default` to switch back). hostapd must not be restarted within the
  same second (BCM43362 `BRCMF_C_UP -512`), see `wifi-mode.sh`.
- The phone must have its **hotspot on (2.4 GHz)** before/while the tablet boots; the tablet keeps retrying.
- Backups of the working card: `Exper Easypad 8/backup/sd-yedek/*.tar.gz` (rootfs + boot).
- Root SSH works (`root` / `exper`); `exper` user is in `wheel` with sudo.

## 5b. Known issues / observations

- **Hot loop (not reproduced since)**: once, two tokio worker threads spun at 100% doing `recvfrom(fd, "", 1) = 0` (EOF on a socket that
  is never closed/ignored) while the phone connection was otherwise healthy. Happened after the pipeline was starved by the `full`
  layout. If it returns: `strace -f -p <tid>`, `ls -l /proc/<pid>/fd`.
- `full` layout (1024x768 stretch of the whole frame) saturates the CPU together with decode+TLS+Wi-Fi AP (load ~4); `4x3`/`fit` are fine.
- **Why the tablet is a hotspot client now (root cause of "no mobile data")**: with the tablet as AP, Android (checked with `adb shell
  dumpsys connectivity`, Infinix X6833B / Android 14) marks the manually-joined `Exper-Auto` as `explicitlySelected + acceptUnvalidated`
  and keeps it as the default network although it has no internet, so apps (Spotify, browser, Maps) hang. Changing DHCP (no router/DNS)
  or `network_avoid_bad_wifi=1` did not help. Without manual selection Android refuses to stay on a no-internet Wi-Fi and drops it,
  so there is no phone-side setting that fixes the "phone joins HU's AP" mode (only the real wireless-AA Bluetooth+`WifiNetworkSpecifier`
  handshake would). With the phone as the hotspot its own mobile data is the uplink and everything works. Home Wi-Fi networks were
  "forgotten" on the phone during the experiments (passwords must be re-entered); no other phone settings were left changed.

## 6. Next Steps

1. Real-phone test done (picture, touch with `4x3`, audio over AUX OK). Hotspot mode verified with real phone (video, touch, audio, mobile data all OK). Remaining: long drive test, colour check (BT.601 limited assumed; phone stream may be BT.709).
3. Re-create `exper-powerd` (screen sleep / power key) - lost with the old card.
4. Clean the debug cmdline baked into the kernel (`ignore_loglevel debug initcall_debug`) and the bring-up probes (see `Exper Easypad 8/HANDOFF.md` section 6.4).
5. Optional: Cedrus VPU port, or revisit the DE1 frontend (section 3.5) for zero-CPU scaling.
