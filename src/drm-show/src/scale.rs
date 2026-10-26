//! yuv420p -> XRGB8888 (BGRX bayt sirasi) bilineer olcekleme + BT.601 sinirli aralik donusumu.
//! Tamsayi aritmetigi; satirlar iki is parcacigina bolunur (A31s: 4x Cortex-A7, NEON yok sayilmaz
//! ama LLVM'in otomatik vektorlestirmesine guveniyoruz).

pub struct Scaler {
    /// tam kare
    sw: usize,
    sh: usize,
    /// kirpma dikdortgeni (luma; x,y,w,h cift sayi)
    cx: usize,
    cy: usize,
    cw: usize,
    ch: usize,
    dw: usize,
    dh: usize,
    // yatay tablolar (dst x basina): kaynak indeksi ve 0..=256 agirlik
    lx_i: Vec<u16>,
    lx_f: Vec<u16>,
    cx_i: Vec<u16>,
    cx_f: Vec<u16>,
    // dikey tablolar (dst y basina)
    ly_i: Vec<u16>,
    ly_f: Vec<u16>,
    cy_i: Vec<u16>,
    cy_f: Vec<u16>,
}

/// pos: kaynak ornekleme konumu (piksel merkezli), n: o eksendeki kaynak boyutu.
fn tap(pos_num: i64, pos_den: i64, n: usize) -> (u16, u16) {
    // pos = pos_num / pos_den - 0.5 ; 8 bit kesir
    let pos256 = (pos_num * 256) / pos_den - 128;
    if pos256 <= 0 {
        return (0, 0);
    }
    let max = ((n as i64) - 1) * 256;
    if pos256 >= max {
        return ((n - 2) as u16, 256);
    }
    ((pos256 >> 8) as u16, (pos256 & 255) as u16)
}

impl Scaler {
    /// sw x sh: tam kare; crop: (x, y, w, h) luma koordinatlarinda kirpma (hepsi cift); dw x dh: cikis.
    pub fn new(sw: usize, sh: usize, crop: (usize, usize, usize, usize), dw: usize, dh: usize) -> Self {
        let (cx, cy, cw, ch) = crop;
        assert!(cx % 2 == 0 && cy % 2 == 0 && cw % 2 == 0 && ch % 2 == 0, "kirpma degerleri cift olmali");
        assert!(cx + cw <= sw && cy + ch <= sh, "kirpma kare disina tasiyor");
        let (ccw, cch) = (cw / 2, ch / 2);
        let mut s = Scaler {
            sw, sh, cx, cy, cw, ch, dw, dh,
            lx_i: vec![], lx_f: vec![], cx_i: vec![], cx_f: vec![],
            ly_i: vec![], ly_f: vec![], cy_i: vec![], cy_f: vec![],
        };
        for x in 0..dw {
            let n = (2 * x + 1) as i64;
            let (i, f) = tap(n * cw as i64, 2 * dw as i64, cw);
            s.lx_i.push(i);
            s.lx_f.push(f);
            let (i, f) = tap(n * ccw as i64, 2 * dw as i64, ccw);
            s.cx_i.push(i);
            s.cx_f.push(f);
        }
        for y in 0..dh {
            let n = (2 * y + 1) as i64;
            let (i, f) = tap(n * ch as i64, 2 * dh as i64, ch);
            s.ly_i.push(i);
            s.ly_f.push(f);
            let (i, f) = tap(n * cch as i64, 2 * dh as i64, cch);
            s.cy_i.push(i);
            s.cy_f.push(f);
        }
        s
    }

    /// Cikis satirlarini [r0, r1) isler; out: o araligin ilk satirinin basindan baslayan dilim.
    pub fn rows(&self, yuv: &[u8], out: &mut [u8], pitch: usize, r0: usize, r1: usize) {
        let (sw, sh, dw) = (self.sw, self.sh, self.dw);
        let (cw, ch) = (self.cw, self.ch);
        let (cwf, chf) = (sw / 2, sh / 2); // tam karenin chroma boyutu
        let (ccw, cch) = (cw / 2, ch / 2);
        let yp = &yuv[..sw * sh];
        let up = &yuv[sw * sh..sw * sh + cwf * chf];
        let vp = &yuv[sw * sh + cwf * chf..sw * sh + 2 * cwf * chf];
        let (yx, yy0) = (self.cx, self.cy);
        let (ux, uy0) = (self.cx / 2, self.cy / 2);

        // +1: yatay harmanda i+1 okunur
        let mut ty = vec![0u8; cw + 1];
        let mut tu = vec![0u8; ccw + 1];
        let mut tv = vec![0u8; ccw + 1];
        let mut yy = vec![0i32; dw];
        let mut uu = vec![0i32; dw];
        let mut vv = vec![0i32; dw];

        for r in r0..r1 {
            // dikey harman (vektorlesir: u16 carpimlar)
            let (i, f) = (self.ly_i[r] as usize, self.ly_f[r]);
            let i1 = (i + 1).min(ch - 1);
            let a = &yp[(yy0 + i) * sw + yx..][..cw];
            let b = &yp[(yy0 + i1) * sw + yx..][..cw];
            blend(&mut ty[..cw], a, b, f);
            let (i, f) = (self.cy_i[r] as usize, self.cy_f[r]);
            let i1 = (i + 1).min(cch - 1);
            blend(&mut tu[..ccw], &up[(uy0 + i) * cwf + ux..][..ccw], &up[(uy0 + i1) * cwf + ux..][..ccw], f);
            blend(&mut tv[..ccw], &vp[(uy0 + i) * cwf + ux..][..ccw], &vp[(uy0 + i1) * cwf + ux..][..ccw], f);
            ty[cw] = ty[cw - 1];
            tu[ccw] = tu[ccw - 1];
            tv[ccw] = tv[ccw - 1];

            gather(&mut yy, &ty, &self.lx_i, &self.lx_f);
            gather(&mut uu, &tu, &self.cx_i, &self.cx_f);
            gather(&mut vv, &tv, &self.cx_i, &self.cx_f);

            // BT.601 sinirli aralik -> RGB (BGRX)
            let o = (r - r0) * pitch;
            let dst = &mut out[o..o + dw * 4];
            for (((px, &y), &u), &v) in dst.chunks_exact_mut(4).zip(&yy).zip(&uu).zip(&vv) {
                let c = 298 * (y - 16) + 128;
                let d = u - 128;
                let e = v - 128;
                px[0] = ((c + 516 * d) >> 8).clamp(0, 255) as u8;
                px[1] = ((c - 100 * d - 208 * e) >> 8).clamp(0, 255) as u8;
                px[2] = ((c + 409 * e) >> 8).clamp(0, 255) as u8;
                px[3] = 0xff;
            }
        }
    }

    /// NEON cekirdegi icin: 32 cikis pikselinin kapsadigi kaynak piksel sayisi (cw*32/dw), uygunsa.
    fn neon_span(&self) -> Option<usize> {
        if !cfg!(target_arch = "arm") || self.dw % 32 != 0 || self.dw > 1024 || self.cw % 16 != 0 || self.cw > 1024 {
            return None;
        }
        let span = self.cw * 32;
        (span % self.dw == 0 && (1..=31).contains(&(span / self.dw))).then(|| span / self.dw)
    }

    pub fn neon_ok(&self) -> bool {
        self.neon_span().is_some()
    }

    /// NEON yolu: satirlar [r0, r1), out o araligin ilk satirindan baslar.
    #[cfg(target_arch = "arm")]
    pub fn rows_neon(&self, yuv: &[u8], out: &mut [u8], pitch: usize, r0: usize, r1: usize) {
        unsafe extern "C" {
            fn yuv_scale_rows(
                y: *const u8, ystride: i32, u: *const u8, v: *const u8, cstride: i32,
                cw: i32, ch: i32, out: *mut u8, pitch: i32, dw: i32, r0: i32, r1: i32,
                ly_i: *const u16, ly_w: *const u8, cy_i: *const u16, cy_w: *const u8, span: i32,
            );
        }
        let span = self.neon_span().expect("neon_ok degil");
        assert!(yuv.len() >= self.frame_len() && out.len() >= (r1 - r0) * pitch && pitch >= self.dw * 4);
        let (cwf, chf) = (self.sw / 2, self.sh / 2);
        let lw: Vec<u8> = self.ly_f.iter().map(|f| (f.min(&256) / 2) as u8).collect();
        let cwt: Vec<u8> = self.cy_f.iter().map(|f| (f.min(&256) / 2) as u8).collect();
        // SAFETY: kirpma ve boyutlar new()'de dogrulandi; C tarafi yalnizca kirpma dikdortgenindeki
        // satirlara/sutunlara (cw/ch) ve out'un [r0,r1) satirlarina erisir.
        unsafe {
            let base = yuv.as_ptr();
            let y = base.add(self.cy * self.sw + self.cx);
            let u = base.add(self.sw * self.sh + (self.cy / 2) * cwf + self.cx / 2);
            let v = base.add(self.sw * self.sh + cwf * chf + (self.cy / 2) * cwf + self.cx / 2);
            yuv_scale_rows(
                y, self.sw as i32, u, v, cwf as i32, self.cw as i32, self.ch as i32,
                out.as_mut_ptr(), pitch as i32, self.dw as i32, r0 as i32, r1 as i32,
                self.ly_i.as_ptr(), lw.as_ptr(), self.cy_i.as_ptr(), cwt.as_ptr(), span as i32,
            );
        }
    }
    #[cfg(not(target_arch = "arm"))]
    pub fn rows_neon(&self, _: &[u8], _: &mut [u8], _: usize, _: usize, _: usize) {}

    pub fn rows_any(&self, yuv: &[u8], out: &mut [u8], pitch: usize, r0: usize, r1: usize) {
        if self.neon_ok() {
            self.rows_neon(yuv, out, pitch, r0, r1)
        } else {
            self.rows(yuv, out, pitch, r0, r1)
        }
    }

    pub fn frame_len(&self) -> usize {
        self.sw * self.sh * 3 / 2
    }
    pub fn dst_height(&self) -> usize {
        self.dh
    }
}

#[inline(always)]
fn blend(dst: &mut [u8], a: &[u8], b: &[u8], f: u16) {
    let g = 256 - f;
    for ((d, &x), &y) in dst.iter_mut().zip(a).zip(b) {
        *d = ((x as u16 * g + y as u16 * f) >> 8) as u8;
    }
}

#[inline(always)]
fn gather(dst: &mut [i32], src: &[u8], idx: &[u16], frac: &[u16]) {
    for ((d, &i), &f) in dst.iter_mut().zip(idx).zip(frac) {
        let i = i as usize;
        debug_assert!(i + 1 < src.len());
        // SAFETY: tap() i <= n-2, src n+1 uzunlugunda (son eleman kopyalandi)
        let (x, y) = unsafe { (*src.get_unchecked(i) as u32, *src.get_unchecked(i + 1) as u32) };
        *d = ((x * (256 - f as u32) + y * f as u32) >> 8) as i32;
    }
}
