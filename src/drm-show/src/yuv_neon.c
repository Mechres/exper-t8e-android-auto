/* yuv420p -> XRGB8888 (BGRX bayt sirasi), bilineer olcekleme; ARMv7 NEON.
 * Yatay oran span:32 (800->1024 icin 25:32, 640->1024 icin 20:32; chroma icin ayni): her 32 cikis
 * pikseli `span` kaynak pikseli kapsar, indeks/agirlik tablosu bloktan bloga ayni olur ve vtbl4 ile
 * 8 piksel birden toplanir. Dikey agirliklar cagiran tarafindan hazirlanir. Agirliklar 7 bit (toplam 128). */
#include <arm_neon.h>
#include <stdint.h>

static uint8_t g_idx[32], g_w1[32], g_w0[32];
static int g_span;

static void init_tables(int span)
{
	for (int k = 0; k < 32; k++) {
		int p = k * span;          /* kaynak konumu = k*span/32 (kose hizali) */
		g_idx[k] = (uint8_t)(p / 32);
		g_w1[k]  = (uint8_t)((p % 32) * 4);
		g_w0[k]  = (uint8_t)(128 - g_w1[k]);
	}
	g_span = span;
}

/* iki satiri dikey harmanla: dst = (a*w0 + b*w1) >> 7; n 8'in kati */
static void vblend(uint8_t *dst, const uint8_t *a, const uint8_t *b, int n, int w1)
{
	uint8x8_t vw1 = vdup_n_u8((uint8_t)w1), vw0 = vdup_n_u8((uint8_t)(128 - w1));
	for (int i = 0; i < n; i += 8) {
		uint16x8_t acc = vmull_u8(vld1_u8(a + i), vw0);
		acc = vmlal_u8(acc, vld1_u8(b + i), vw1);
		vst1_u8(dst + i, vshrn_n_u16(acc, 7));
	}
}

/* yatay 25:32 genisletme: src (n_out/32*25 bayt + 8 dolgu) -> dst (n_out) */
static void hscale(uint8_t *dst, const uint8_t *src, int n_out, int span)
{
	for (int b = 0; b < n_out / 32; b++) {
		const uint8_t *s = src + b * span;
		uint8x8x4_t t = { { vld1_u8(s), vld1_u8(s + 8), vld1_u8(s + 16), vld1_u8(s + 24) } };
		for (int g = 0; g < 4; g++) {
			uint8x8_t i0 = vld1_u8(g_idx + g * 8);
			uint8x8_t i1 = vadd_u8(i0, vdup_n_u8(1));
			uint8x8_t x0 = vtbl4_u8(t, i0), x1 = vtbl4_u8(t, i1);
			uint16x8_t acc = vmull_u8(x0, vld1_u8(g_w0 + g * 8));
			acc = vmlal_u8(acc, x1, vld1_u8(g_w1 + g * 8));
			vst1_u8(dst + b * 32 + g * 8, vshrn_n_u16(acc, 7));
		}
	}
}

static inline uint8x8_t sat8(int32x4_t lo, int32x4_t hi)
{
	return vqmovun_s16(vcombine_s16(vqshrn_n_s32(lo, 8), vqshrn_n_s32(hi, 8)));
}

/* 16 piksel/dongu: BT.601 sinirli aralik */
static void convert_row(uint8_t *out, const uint8_t *y, const uint8_t *u, const uint8_t *v, int dw)
{
	const uint8x8_t c16 = vdup_n_u8(16), c128 = vdup_n_u8(128), ff = vdup_n_u8(255);
	const int32x4_t rnd = vdupq_n_s32(128);
	for (int i = 0; i < dw; i += 16) {
		uint8x8x2_t uz = vzip_u8(vld1_u8(u + i / 2), vld1_u8(u + i / 2));
		uint8x8x2_t vz = vzip_u8(vld1_u8(v + i / 2), vld1_u8(v + i / 2));
		for (int h = 0; h < 2; h++) {
			int16x8_t yy = vreinterpretq_s16_u16(vsubl_u8(vld1_u8(y + i + h * 8), c16));
			int16x8_t dd = vreinterpretq_s16_u16(vsubl_u8(uz.val[h], c128));
			int16x8_t ee = vreinterpretq_s16_u16(vsubl_u8(vz.val[h], c128));
			int32x4_t cl = vmlal_n_s16(rnd, vget_low_s16(yy), 298);
			int32x4_t ch = vmlal_n_s16(rnd, vget_high_s16(yy), 298);
			int32x4_t rl = vmlal_n_s16(cl, vget_low_s16(ee), 409);
			int32x4_t rh = vmlal_n_s16(ch, vget_high_s16(ee), 409);
			int32x4_t bl = vmlal_n_s16(cl, vget_low_s16(dd), 516);
			int32x4_t bh = vmlal_n_s16(ch, vget_high_s16(dd), 516);
			int32x4_t gl = vmlsl_n_s16(vmlsl_n_s16(cl, vget_low_s16(dd), 100), vget_low_s16(ee), 208);
			int32x4_t gh = vmlsl_n_s16(vmlsl_n_s16(ch, vget_high_s16(dd), 100), vget_high_s16(ee), 208);
			uint8x8x4_t px = { { sat8(bl, bh), sat8(gl, gh), sat8(rl, rh), ff } };
			vst4_u8(out + (i + h * 8) * 4, px);
		}
	}
}

/* Satirlar [r0, r1). y/u/v: kirpma dikdortgeninin sol ust kosesini gosterir; ystride/cstride: tam
 * karenin luma/chroma satir uzunlugu; cw/ch: kirpma boyutu (luma, cw 16'nin kati). ly_*: luma dikey
 * (kaynak satir, 7 bit agirlik), cy_*: chroma dikey. span: 32 cikis pikselinin kapsadigi kaynak piksel. */
void yuv_scale_rows(const uint8_t *y, int ystride, const uint8_t *u, const uint8_t *v, int cstride,
		    int cw, int ch, uint8_t *out, int pitch, int dw, int r0, int r1,
		    const uint16_t *ly_i, const uint8_t *ly_w, const uint16_t *cy_i,
		    const uint8_t *cy_w, int span)
{
	if (g_span != span)
		init_tables(span);
	const int ccw = cw / 2, cch = ch / 2, dcw = dw / 2;
	uint8_t ty[1024 + 64] __attribute__((aligned(16)));
	uint8_t tu[512 + 64]  __attribute__((aligned(16)));
	uint8_t tv[512 + 64]  __attribute__((aligned(16)));
	uint8_t yh[1024 + 16] __attribute__((aligned(16)));
	uint8_t uh[512 + 16]  __attribute__((aligned(16)));
	uint8_t vh[512 + 16]  __attribute__((aligned(16)));

	for (int r = r0; r < r1; r++) {
		int i = ly_i[r], i1 = i + 1 < ch ? i + 1 : ch - 1;
		vblend(ty, y + i * ystride, y + i1 * ystride, cw, ly_w[r]);
		for (int k = cw; k < cw + 16; k++) ty[k] = ty[cw - 1];
		i = cy_i[r]; i1 = i + 1 < cch ? i + 1 : cch - 1;
		vblend(tu, u + i * cstride, u + i1 * cstride, ccw, cy_w[r]);
		vblend(tv, v + i * cstride, v + i1 * cstride, ccw, cy_w[r]);
		for (int k = ccw; k < ccw + 16; k++) { tu[k] = tu[ccw - 1]; tv[k] = tv[ccw - 1]; }
		hscale(yh, ty, dw, span);
		hscale(uh, tu, dcw, span);
		hscale(vh, tv, dcw, span);
		convert_row(out + (r - r0) * pitch, yh, uh, vh, dw);
	}
}
