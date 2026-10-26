//! drm-show: stdin'den gelen ham kareleri bir DRM overlay plane'ine yazar.
//!
//! Girdi bicimleri (--fmt):
//!   yuv420p  800x480 YUV -> NEON ile bilineer olceklenip hedef dikdortgene XRGB olarak yazilir (CPU olcekleme)
//!   xr24     ham BGRX kare; plane'e oldugu gibi (olcekleme yok) basilir
//!
//! Kullanim: ffmpeg ... -pix_fmt yuv420p -f rawvideo - | drm-show --fmt yuv420p --src 800x480 --dst 0,77,1024,614
//!   --src WxH        kaynak kare boyutu (varsayilan 800x480)
//!   --crop X,Y,W,H   (yuv420p) kaynak karede kirpilacak dikdortgen, hepsi cift (varsayilan tum kare)
//!   --dst X,Y,W,H    ekrandaki hedef dikdortgen (varsayilan 0,0,1024,768)
//!   --card PATH      DRM aygiti (varsayilan /dev/dri/card0)
//! DRM_SHOW_BENCH=1: 120 karede bir zamanlama ozeti basar.
//!
//! Not: A31s DE1 frontend'i (donanim olcekleyici) mainline'da bu kartta goruntu uretmiyor
//! (bkz. HANDOFF); bu yuzden olcekleme yazilimla (scale.rs / yuv_neon.c) yapiliyor.
mod scale;

use drm::Device as BasicDevice;
use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{Device as ControlDevice, PlaneType, framebuffer, plane, property};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

struct Card(File);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl BasicDevice for Card {}
impl ControlDevice for Card {}

fn parse_pair(s: &str, sep: char) -> Option<(u32, u32)> {
    let (a, b) = s.split_once(sep)?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Uretici (okuma+olcekleme) ile ekranlama is parcacigi arasindaki posta kutusu.
/// Tampon sahipligi: bir tampon ya bostur, ya `ready` (siradaki), ya `inflight` (set_plane
/// donmedi), ya `displayed` (ekranda). Uretici yalnizca bu uclusunde olmayan bir tampona yazar.
#[derive(Default)]
struct Shared {
    ready: Option<usize>,
    inflight: Option<usize>,
    displayed: Option<usize>,
    quit: bool,
    flips: u64,
    dropped: u64,
}

fn main() {
    let mut src = (800u32, 480u32);
    let mut dst = (0i32, 0i32, 1024u32, 768u32);
    let mut path = String::from("/dev/dri/card0");
    let mut yuv = false;
    let mut crop: Option<(usize, usize, usize, usize)> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--src" => src = parse_pair(&args.next().unwrap_or_default(), 'x').expect("--src WxH"),
            "--dst" => {
                let v: Vec<i64> = args
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .map(|p| p.parse().expect("--dst X,Y,W,H"))
                    .collect();
                assert!(v.len() == 4, "--dst X,Y,W,H");
                dst = (v[0] as i32, v[1] as i32, v[2] as u32, v[3] as u32);
            }
            "--crop" => {
                let v: Vec<usize> = args
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .map(|p| p.parse().expect("--crop X,Y,W,H"))
                    .collect();
                assert!(v.len() == 4, "--crop X,Y,W,H");
                crop = Some((v[0], v[1], v[2], v[3]));
            }
            "--card" => path = args.next().unwrap_or_default(),
            "--fmt" => match args.next().as_deref() {
                Some("yuv420p") => yuv = true,
                Some("xr24") => yuv = false,
                other => {
                    eprintln!("bilinmeyen --fmt: {other:?} (yuv420p | xr24)");
                    std::process::exit(2);
                }
            },
            _ => {
                eprintln!("bilinmeyen arguman: {a}");
                std::process::exit(2);
            }
        }
    }
    let bench = std::env::var_os("DRM_SHOW_BENCH").is_some();

    let card = Card(OpenOptions::new().read(true).write(true).open(&path).expect("DRM aygiti acilamadi"));
    let res = card.resource_handles().expect("resource_handles");
    let crtc = *res.crtcs().first().expect("CRTC yok");

    // Bu CRTC'ye bagli ilk overlay plane (primary fbdev konsolu icin birakilir)
    let mut chosen: Option<plane::Handle> = None;
    for p in card.plane_handles().expect("plane_handles") {
        let info = card.get_plane(p).expect("get_plane");
        if !res.filter_crtcs(info.possible_crtcs()).contains(&crtc) {
            continue;
        }
        let props = card.get_properties(p).expect("get_properties");
        for (h, v) in props.iter() {
            let pi = card.get_property(*h).expect("get_property");
            if pi.name().to_str() == Ok("type")
                && matches!(pi.value_type().convert_value(*v), property::Value::Enum(_))
                && *v == PlaneType::Overlay as u64
                && chosen.is_none()
            {
                chosen = Some(p);
            }
        }
    }
    let plane = chosen.expect("overlay plane bulunamadi");

    // yuv420p: tampon hedef boyutta (olcekleme CPU'da); xr24: kaynak boyutta
    let (bw, bh) = if yuv { (dst.2, dst.3) } else { src };
    const NBUF: usize = 4;
    let mut bufs = Vec::new();
    for _ in 0..NBUF {
        let db = card.create_dumb_buffer((bw, bh), DrmFourcc::Xrgb8888, 32).expect("dumb buffer");
        let fb = card.add_framebuffer(&db, 24, 32).expect("add_framebuffer");
        bufs.push((db, fb));
    }
    let fbs: Vec<framebuffer::Handle> = bufs.iter().map(|b| b.1).collect();

    let crop = crop.unwrap_or((0, 0, src.0 as usize, src.1 as usize));
    let scaler = scale::Scaler::new(src.0 as usize, src.1 as usize, crop, dst.2 as usize, dst.3 as usize);
    let frame_len = if yuv { scaler.frame_len() } else { (src.0 * src.1 * 4) as usize };
    let mut frame = vec![0u8; frame_len];
    let mut stdin = std::io::stdin().lock();

    let shared = (Mutex::new(Shared::default()), Condvar::new());
    let (lock, cv) = (&shared.0, &shared.1);
    let dst_rect = (dst.0, dst.1, dst.2, dst.3);
    let src_rect = (0u32, 0u32, bw << 16, bh << 16); // 16.16 sabit nokta

    std::thread::scope(|sc| {
        // --- ekranlama: her zaman en yeni hazir kareyi gosterir ---
        sc.spawn(|| loop {
            let idx = {
                let mut g = lock.lock().unwrap();
                while g.ready.is_none() && !g.quit {
                    g = cv.wait(g).unwrap();
                }
                match g.ready.take() {
                    Some(i) => {
                        g.inflight = Some(i);
                        i
                    }
                    None => return, // quit
                }
            };
            card.set_plane(plane, crtc, Some(fbs[idx]), 0, dst_rect, src_rect).expect("set_plane");
            let mut g = lock.lock().unwrap();
            g.displayed = Some(idx);
            g.inflight = None;
            g.flips += 1;
        });

        // --- uretici: oku, olcekle, yayinla ---
        let (mut t_read, mut t_scale, mut frames) = (0u128, 0u128, 0u64);
        let mut last = Instant::now();
        loop {
            let t0 = Instant::now();
            if stdin.read_exact(&mut frame).is_err() {
                break;
            }
            let t1 = Instant::now();

            let idx = {
                let g = lock.lock().unwrap();
                (0..NBUF)
                    .find(|i| Some(*i) != g.ready && Some(*i) != g.inflight && Some(*i) != g.displayed)
                    .expect("bos tampon yok")
            };
            let (db, _) = &mut bufs[idx];
            let pitch = db.pitch() as usize;
            {
                let mut map = card.map_dumb_buffer(db).expect("map_dumb_buffer");
                let m: &mut [u8] = map.as_mut();
                if yuv {
                    let h = scaler.dst_height();
                    let mid = h / 2;
                    let (top, bot) = m.split_at_mut(mid * pitch);
                    std::thread::scope(|s2| {
                        s2.spawn(|| scaler.rows_any(&frame, top, pitch, 0, mid));
                        scaler.rows_any(&frame, bot, pitch, mid, h);
                    });
                } else {
                    let row = (src.0 * 4) as usize;
                    for y in 0..src.1 as usize {
                        m[y * pitch..y * pitch + row].copy_from_slice(&frame[y * row..(y + 1) * row]);
                    }
                }
            }
            let t2 = Instant::now();
            {
                let mut g = lock.lock().unwrap();
                if g.ready.replace(idx).is_some() {
                    g.dropped += 1; // ekranlama yetismedi: eski kare atilir
                }
            }
            cv.notify_all();

            if bench {
                t_read += (t1 - t0).as_micros();
                t_scale += (t2 - t1).as_micros();
                frames += 1;
                if frames % 120 == 0 {
                    let g = lock.lock().unwrap();
                    let dt = last.elapsed().as_secs_f64();
                    eprintln!(
                        "[bench] {:.1} kare/sn okundu, {} ekranlandi, {} atildi | okuma {:.1} ms, olcekleme {:.1} ms /kare",
                        120.0 / dt, g.flips, g.dropped,
                        t_read as f64 / 120000.0, t_scale as f64 / 120000.0
                    );
                    drop(g);
                    t_read = 0;
                    t_scale = 0;
                    last = Instant::now();
                }
            }
        }
        lock.lock().unwrap().quit = true;
        cv.notify_all();
    });

    let g = shared.0.lock().unwrap();
    eprintln!("[drm-show] {} kare ekranlandi, {} atildi, cikiliyor", g.flips, g.dropped);
    drop(g);
    let _ = card.set_plane(plane, crtc, None, 0, (0, 0, 0, 0), (0, 0, 0, 0));
    for (db, fb) in bufs {
        let _ = card.destroy_framebuffer(fb);
        let _ = card.destroy_dumb_buffer(db);
    }
}
