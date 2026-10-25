# Exper Easypad 8 (T8E) - Wireless Android Auto Head Unit

Transforming the **Exper Easypad 8 (Model T8E)** Allwinner A31s tablet running mainline Linux into a standalone **Wireless Android Auto** in-car infotainment display.

---

## Hardware Architecture & Car Integration

```
+-------------------------------------------------------+
|                    Android Phone                      |
| (Runs Google Maps, Waze, Spotify, Navigation, Calls)  |
+---------------------------+---------------------------+
                            |
           [ Wi-Fi: H.264 Video Stream + PCM Audio + Touch ]
                            |
                            v
+-------------------------------------------------------+
|            Exper Easypad 8 (Alpine Linux)             |
|                                                       |
|  +--------------------+       +--------------------+  |
|  |  Android Auto SDK  | ----> | 1024x768 Screen    |  |
|  |  (H.264 Decoder &  | <---- | (Goodix GT911      |  |
|  |   Touch Forwarder) |       |  Multi-Touch)      |  |
|  +---------+----------+       +--------------------+  |
|            |                                          |
+------------|------------------------------------------+
             | [3.5mm Headphone Jack (sun6i-a31-codec)]
             v
   +--------------------+
   |   Car Stereo AUX   |  ---> [ Car Speakers ]
   +--------------------+
```

### Physical Connections in the Car
1. **Power:** 5V / 2A DC power from the car's 12V cigarette lighter charger into the tablet's Micro-USB port.
2. **Audio:** 3.5mm male-to-male stereo AUX cable from the tablet's headphone jack into the car stereo AUX input.
3. **Data:** **Zero wires to the phone**. Connection is entirely over Wi-Fi.

---

## Subsystem Mapping

| Component | Hardware Specification | Role in Android Auto |
| :--- | :--- | :--- |
| **Display** | 1024×768 single-channel LVDS panel | Fullscreen Android Auto UI projection |
| **Touchscreen** | Goodix GT911 5-point capacitive | Touch input forwarder (taps, drags, pinches) |
| **Audio DAC** | Allwinner `sun6i-a31-codec` | Stereo audio playback (music, turn-by-turn prompts) |
| **Wireless** | Broadcom BCM43362 (SDIO `wlan0`) | Local Wi-Fi link for high-throughput video/audio |
| **Power Daemon**| `exper-powerd` (AXP221 PEK) | Screen sleep/wake with vehicle ignition / power key |

---

## Connection Protocol (Wireless Android Auto)

Wireless Android Auto communicates over standard TCP/IP:
- **Port:** TCP `5277` (Google Android Auto Headunit Server protocol) or TCP `5000`
- **Video:** H.264 NAL elementary stream decoded and scaled to $1024 \times 768$
- **Audio:** Raw stereo PCM ($48\text{ kHz}, 16\text{-bit}$) routed directly to ALSA
- **Touch Input:** Multitouch motion events translated to absolute screen coordinates

### Pairing Methods
1. **Direct Headunit Server Mode:**
   - In Android Auto Settings on the phone, enable Developer Settings (tap *Version* 10 times).
   - Tap menu $\rightarrow$ **Start head unit server**.
   - Phone connects to the tablet's Wi-Fi network; head unit connects to the phone's TCP port.
2. **Automated Wi-Fi Launch:**
   - Companion launcher (e.g. Android Auto Wireless ADB / WiFi Launcher) initiates projection whenever the phone connects to the tablet's car Wi-Fi hotspot.

---

## Project Structure

```
exper-t8e-android-auto/
├── README.md               # Project documentation and specifications
├── configs/                # Wi-Fi AP, network, and audio daemon configs
│   ├── hostapd.conf        # Local vehicle Wi-Fi hotspot
│   ├── dnsmasq.conf        # DHCP server configuration
│   └── asound.conf         # Low-latency ALSA routing
├── scripts/                # Head unit launchers and display helpers
│   ├── start-aa.sh         # Main startup orchestrator
│   └── setup-hotspot.sh    # Wi-Fi network initialization
└── src/                    # Android Auto protocol client implementation
```
