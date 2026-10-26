//! exper-powerd: guc tusu / harici guc izleyicisi (Exper Easypad 8, AXP221 PMIC).
//!
//!  - Guc tusu (axp20x-pek): kisa basis = ekrani (arka isigi) ac/kapat, uzun basis = kapat.
//!  - Harici guc (axp22x-ac / axp20x-usb) gidince (arac kontagi kapandi): `loss_screen_off` sn sonra ekrani kapat,
//!    `loss_poweroff` sn sonra kapat; guc gelince ekrani ac. Pil `low_batt` % altina inerse kapat.
//! Ayarlar: /etc/exper-auto/powerd.conf (anahtar=deger). Test icin ortam degiskenleri:
//!   EXPER_POWERD_CONF, _SUPPLY_DIR, _BACKLIGHT_DIR, _INPUT (aygit yolu), _POWEROFF_CMD.
use std::io::Read;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const KEY_POWER: u16 = 116;
const EV_KEY: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Cfg {
    long_press_ms: u64,
    loss_screen_off_s: u64,
    loss_poweroff_s: u64,
    low_batt_pct: u32,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg { long_press_ms: 3000, loss_screen_off_s: 15, loss_poweroff_s: 300, low_batt_pct: 5 }
    }
}

fn parse_cfg(text: &str) -> Cfg {
    let mut c = Cfg::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let Ok(v) = v.trim().parse::<u64>() else { continue };
        match k.trim() {
            "long_press_ms" => c.long_press_ms = v,
            "loss_screen_off_s" => c.loss_screen_off_s = v,
            "loss_poweroff_s" => c.loss_poweroff_s = v,
            "low_batt_pct" => c.low_batt_pct = v as u32,
            _ => {}
        }
    }
    c
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Action {
    ScreenOn,
    ScreenOff,
    Poweroff,
}

/// Saf durum makinesi; zaman milisaniye olarak disaridan verilir (test edilebilir).
struct Power {
    cfg: Cfg,
    screen_on: bool,
    /// ekran, kullanicinin guc tusuyla mi (true) yoksa guc kaybiyla mi kapatildi
    off_by_user: bool,
    off_by_loss: bool,
    press_at: Option<u64>,
    long_fired: bool,
    lost_since: Option<u64>,
    shutting_down: bool,
}

impl Power {
    fn new(cfg: Cfg) -> Self {
        Power { cfg, screen_on: true, off_by_user: false, off_by_loss: false, press_at: None, long_fired: false, lost_since: None, shutting_down: false }
    }

    fn poweroff(&mut self, out: &mut Vec<Action>) {
        if !self.shutting_down {
            self.shutting_down = true;
            out.push(Action::Poweroff);
        }
    }

    fn on_key(&mut self, pressed: bool, now: u64) -> Vec<Action> {
        let mut out = Vec::new();
        if pressed {
            self.press_at = Some(now);
            self.long_fired = false;
        } else if let Some(t) = self.press_at.take() {
            // uzun basis zaten tick'te islendi; burada yalnizca kisa basis
            if !self.long_fired && now.saturating_sub(t) < self.cfg.long_press_ms {
                if self.screen_on {
                    self.screen_on = false;
                    self.off_by_user = true;
                    out.push(Action::ScreenOff);
                } else {
                    self.screen_on = true;
                    self.off_by_user = false;
                    self.off_by_loss = false;
                    out.push(Action::ScreenOn);
                }
            }
        }
        out
    }

    /// ext: harici guc var mi; batt: (yuzde, sarj oluyor mu) bilinmiyorsa None.
    fn on_tick(&mut self, now: u64, ext: bool, batt: Option<(u32, bool)>) -> Vec<Action> {
        let mut out = Vec::new();
        if let Some(t) = self.press_at {
            if !self.long_fired && now.saturating_sub(t) >= self.cfg.long_press_ms {
                self.long_fired = true;
                self.poweroff(&mut out);
            }
        }
        if ext {
            self.lost_since = None;
            if self.off_by_loss {
                self.off_by_loss = false;
                if !self.off_by_user && !self.screen_on {
                    self.screen_on = true;
                    out.push(Action::ScreenOn);
                }
            }
        } else {
            let since = *self.lost_since.get_or_insert(now);
            let lost_s = now.saturating_sub(since) / 1000;
            if lost_s >= self.cfg.loss_screen_off_s && self.screen_on {
                self.screen_on = false;
                self.off_by_loss = true;
                out.push(Action::ScreenOff);
            }
            if lost_s >= self.cfg.loss_poweroff_s {
                self.poweroff(&mut out);
            }
            if let Some((pct, charging)) = batt {
                if pct <= self.cfg.low_batt_pct && !charging {
                    self.poweroff(&mut out);
                }
            }
        }
        out
    }
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn external_power(dir: &str) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return true }; // okunamiyorsa guc var say (yanlis kapatma olmasin)
    let mut found = false;
    for e in rd.flatten() {
        let p = e.path();
        let ty = read_trim(&format!("{}/type", p.display())).unwrap_or_default();
        if ty == "Mains" || ty == "USB" {
            found = true;
            if read_trim(&format!("{}/online", p.display())).as_deref() == Some("1") {
                return true;
            }
        }
    }
    !found
}

fn battery(dir: &str) -> Option<(u32, bool)> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if read_trim(&format!("{}/type", p.display())).as_deref() == Some("Battery") {
            let pct = read_trim(&format!("{}/capacity", p.display()))?.parse().ok()?;
            let st = read_trim(&format!("{}/status", p.display())).unwrap_or_default();
            return Some((pct, st == "Charging" || st == "Full"));
        }
    }
    None
}

fn find_input(name: &str) -> Option<String> {
    for e in std::fs::read_dir("/sys/class/input").ok()?.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with("event") && read_trim(&format!("/sys/class/input/{n}/device/name")).as_deref() == Some(name) {
            return Some(format!("/dev/input/{n}"));
        }
    }
    None
}

fn set_screen(bl_dir: &str, on: bool) {
    // pwm-backlight: bl_power 0 = acik, 4 (FB_BLANK_POWERDOWN) = kapali (enable GPIO'yu da keser)
    let r = std::fs::write(format!("{bl_dir}/bl_power"), if on { "0" } else { "4" });
    if r.is_err() && !on {
        let _ = std::fs::write(format!("{bl_dir}/brightness"), "0");
    }
    println!("[powerd] ekran {}", if on { "ACIK" } else { "KAPALI" });
}

enum Ev {
    Key(bool),
    Tick,
}

fn main() {
    let conf_path = env_or("EXPER_POWERD_CONF", "/etc/exper-auto/powerd.conf");
    let cfg = std::fs::read_to_string(&conf_path).map(|t| parse_cfg(&t)).unwrap_or_default();
    let supply = env_or("EXPER_POWERD_SUPPLY_DIR", "/sys/class/power_supply");
    let bl = env_or("EXPER_POWERD_BACKLIGHT_DIR", "/sys/class/backlight/backlight");
    let poweroff_cmd = env_or("EXPER_POWERD_POWEROFF_CMD", "poweroff");
    println!("[powerd] basladi: {cfg:?}");

    let (tx, rx) = mpsc::channel();
    let input = std::env::var("EXPER_POWERD_INPUT").ok().or_else(|| find_input("axp20x-pek"));
    match input {
        Some(path) => {
            println!("[powerd] guc tusu: {path}");
            let txk = tx.clone();
            std::thread::spawn(move || {
                let mut f = match std::fs::File::open(&path) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("[powerd] {path} acilamadi: {e}");
                        return;
                    }
                };
                let mut buf = [0u8; 16]; // 32 bit cekirdek struct input_event
                while f.read_exact(&mut buf).is_ok() {
                    let ty = u16::from_ne_bytes([buf[8], buf[9]]);
                    let code = u16::from_ne_bytes([buf[10], buf[11]]);
                    let val = i32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]);
                    if ty == EV_KEY && code == KEY_POWER && val != 2 {
                        let _ = txk.send(Ev::Key(val == 1));
                    }
                }
            });
        }
        None => eprintln!("[powerd] axp20x-pek bulunamadi, guc tusu devre disi"),
    }
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        if tx.send(Ev::Tick).is_err() {
            break;
        }
    });

    set_screen(&bl, true);
    let start = Instant::now();
    let mut st = Power::new(cfg);
    for ev in rx {
        let now = start.elapsed().as_millis() as u64;
        let acts = match ev {
            Ev::Key(p) => st.on_key(p, now),
            Ev::Tick => st.on_tick(now, external_power(&supply), battery(&supply)),
        };
        for a in acts {
            match a {
                Action::ScreenOn => set_screen(&bl, true),
                Action::ScreenOff => set_screen(&bl, false),
                Action::Poweroff => {
                    println!("[powerd] KAPATILIYOR");
                    let mut parts = poweroff_cmd.split_whitespace();
                    if let Some(cmd) = parts.next() {
                        let _ = Command::new(cmd).args(parts).spawn();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Cfg {
        Cfg { long_press_ms: 3000, loss_screen_off_s: 15, loss_poweroff_s: 300, low_batt_pct: 5 }
    }

    #[test]
    fn short_press_toggles_screen() {
        let mut p = Power::new(cfg());
        assert!(p.on_key(true, 1000).is_empty());
        assert_eq!(p.on_key(false, 1300), vec![Action::ScreenOff]);
        p.on_key(true, 5000);
        assert_eq!(p.on_key(false, 5200), vec![Action::ScreenOn]);
    }

    #[test]
    fn long_press_powers_off_once_and_not_toggle() {
        let mut p = Power::new(cfg());
        p.on_key(true, 0);
        assert!(p.on_tick(2000, true, None).is_empty());
        assert_eq!(p.on_tick(3000, true, None), vec![Action::Poweroff]);
        assert!(p.on_tick(4000, true, None).is_empty());
        assert!(p.on_key(false, 4500).is_empty()); // birakinca ekran degismez
    }

    #[test]
    fn power_loss_blanks_then_shuts_down_and_restores() {
        let mut p = Power::new(cfg());
        assert!(p.on_tick(1000, false, None).is_empty());
        assert!(p.on_tick(15_000, false, None).is_empty()); // 14 sn
        assert_eq!(p.on_tick(16_000, false, None), vec![Action::ScreenOff]);
        assert_eq!(p.on_tick(300_999, false, None), vec![]); // 299.999 sn
        assert_eq!(p.on_tick(301_000, false, None), vec![Action::Poweroff]);
        assert!(p.on_tick(302_000, false, None).is_empty()); // tekrar etmez
    }

    #[test]
    fn power_return_restores_screen_only_if_loss_blanked_it() {
        let mut p = Power::new(cfg());
        p.on_tick(0, false, None);
        assert_eq!(p.on_tick(16_000, false, None), vec![Action::ScreenOff]);
        assert_eq!(p.on_tick(20_000, true, None), vec![Action::ScreenOn]);
        // kullanici ekrani kapattiysa guc gelince acilmaz
        p.on_key(true, 30_000);
        assert_eq!(p.on_key(false, 30_100), vec![Action::ScreenOff]);
        p.on_tick(31_000, false, None);
        p.on_tick(50_000, false, None);
        assert!(p.on_tick(60_000, true, None).is_empty());
    }

    #[test]
    fn low_battery_without_charger_powers_off() {
        let mut p = Power::new(cfg());
        assert_eq!(p.on_tick(1000, false, Some((5, false))), vec![Action::Poweroff]);
        let mut q = Power::new(cfg());
        assert!(q.on_tick(1000, true, Some((3, false))).is_empty()); // guc var: kapatma
    }

    #[test]
    fn config_parsing_and_defaults() {
        let c = parse_cfg("# yorum\nloss_poweroff_s = 60\nlong_press_ms=2500 # x\nbilinmeyen=1\nlow_batt_pct=abc\n");
        assert_eq!(c.loss_poweroff_s, 60);
        assert_eq!(c.long_press_ms, 2500);
        assert_eq!(c.loss_screen_off_s, 15);
        assert_eq!(c.low_batt_pct, 5);
    }
}
