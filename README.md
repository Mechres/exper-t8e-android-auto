# Wireless Android Auto head unit for the Exper Easypad 8 (T8E)

An experimental **wireless Android Auto head unit** that runs on the Exper Easypad 8 (Allwinner A31s) tablet with mainline Linux. The tablet joins your phone's hotspot, connects to the phone's Android Auto head unit server, decodes the H.264 video in software, scales it to the 1024×768 panel, plays the audio over the 3.5 mm jack and forwards touch input.

> **Not affiliated with Google.** "Android Auto" is a trademark of Google LLC. This is a hobby project built on the open-source [`android-auto`](https://github.com/uglyoldbob/android-auto) crate; it is for personal experimentation and comes with no warranty.

The board support (kernel patches, device trees, SD card scripts, hardware findings) lives in a separate repository:
**[Mechres/exper-easypad8-t8e-linux](https://github.com/Mechres/exper-easypad8-t8e-linux)**.

## How it works

```
 phone (hotspot, AA head unit server :5277)
        ▲ Wi-Fi (tablet = client, phone = default gateway)
        │ TLS, Android Auto protocol
 ┌──────┴────────────────────────────────────────────────────────────┐
 │ exper-auto ── H.264 ──► ffmpeg (decode, yuv420p) ──► drm-show      │
 │     │                                    NEON bilinear scale+crop   │
 │     │ touch (Goodix GT911, evdev)         ──► DRM overlay plane ──► LVDS panel
 │     └ audio PCM ──► aplay ──► ALSA ──► 3.5 mm jack                  │
 │ exper-powerd: power key / external power (AXP221)                   │
 └─────────────────────────────────────────────────────────────────────┘
```

| Component | Path | Purpose |
| :--- | :--- | :--- |
| `exper-auto` | `src/exper-auto` | Head unit: connects to the phone, runs the protocol, feeds ffmpeg/aplay, maps touch |
| `drm-show` | `src/drm-show` | Reads raw yuv420p frames, scales/crops/converts with an ARMv7 NEON kernel, shows them on a DRM overlay plane |
| `exper-powerd` | `src/exper-powerd` | Power-key screen toggle/shutdown and power-loss handling |
| `android-auto` | `src/android-auto` | Android Auto protocol library (submodule, LGPL, with small fixes) |
| `configs/`, `scripts/` | | OpenRC services, Wi-Fi client configuration, `wifi-mode.sh` |

## Highlights (measured on the tablet)

- ~60 frames per second for 800×480 H.264 scaled to the panel (software scaling; the A31s hardware scaler does not work, see the board repo's Finding 48). FFmpeg's `swscale` only reached 25–40 fps.
- 4:3 layout: the phone is asked for a 640×480 UI area (`margin_width = 160`), which is cropped and scaled 1.6× to 1024×768: no black bars and no stretching.
- Picture appears a few seconds after connecting; touch, audio and mobile data (through the phone's own hotspot) work.

More detail, measurements and the dead ends (FFmpeg `-fflags nobuffer`, keyframe focus-bounce loop, why the tablet is not the access point) are in [`HANDOFF.md`](HANDOFF.md).

## In the car

1. **Power:** 5 V / 2 A from a car USB charger into the tablet's Micro-USB port. `exper-powerd` blanks the screen shortly after the ignition cuts the power and shuts the tablet down after a few minutes (configurable).
2. **Audio:** 3.5 mm stereo AUX cable from the tablet's headphone jack to the car stereo.
3. **Data:** no cable to the phone, everything goes over Wi-Fi.

## Pairing (head unit server mode)

1. On the phone: Android Auto settings → enable **Developer settings** (tap *Version* ten times) → menu → **Start head unit server**.
2. Turn on the phone's **hotspot (2.4 GHz)** with the name/password you put into `wpa_supplicant.conf`.
3. The tablet joins the hotspot, finds the phone at its default gateway, connects to TCP port `5277` and the projection starts within a few seconds.

## Requirements

- Exper Easypad 8 (T8E) running the kernel from the [board repository](https://github.com/Mechres/exper-easypad8-t8e-linux) with the DRM device tree (`sun6i-a31s-exper-t8e-drm.dtb`), `cma=64M`, Alpine Linux with `ffmpeg`, `alsa-utils`, `wpa_supplicant`.
- An Android phone with Android Auto (developer settings enabled, see *Pairing*) and a **2.4 GHz** hotspot.
- Rust with the `armv7-unknown-linux-musleabihf` target and an ARMv7 hard-float cross toolchain.

## Build and install

See [`HANDOFF.md`](HANDOFF.md) section 4 for the exact commands. In short:

```bash
git clone --recurse-submodules https://github.com/Mechres/exper-t8e-android-auto.git
# build exper-auto, drm-show (needs the cortex-a7/NEON flags) and exper-powerd for armv7 musl (HANDOFF §4)
# copy the binaries to /usr/local/bin and configs/*.initd to /etc/init.d on the tablet, then:
rc-update add exper-wifi default && rc-update add exper-powerd default && rc-update add exper-auto default
```

Create `/etc/exper-auto/wpa_supplicant.conf` from [`configs/wpa_supplicant.conf.example`](configs/wpa_supplicant.conf.example) with your phone's hotspot name and password. The layout is selected with `EXPER_VIDEO_LAYOUT` (`4x3`, `fit`, `full`, `1024xH`) in `/etc/init.d/exper-auto`.

## License

LGPL-3.0-or-later, see [`COPYING.LESSER`](COPYING.LESSER) and [`COPYING`](COPYING) (the LGPL builds on the GPL). The bundled `android-auto` library is LGPL-3.0-or-later as well.
