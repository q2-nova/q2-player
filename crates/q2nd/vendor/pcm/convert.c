/* MSA kernels for PCM conversion (Q2 only). The equations and the scalar code
 * are in src/audio/pcm_convert.rs; src/audio/pcm_msa.rs is the only caller.
 * `frames` (or `count`) is a multiple of four and within every buffer. No
 * allocation, calls, waits or retained pointers.
 *
 * ftrunc_s saturates and maps NaN to 0, as Rust's `as i32` does. ffint_s and
 * fmul round to nearest even under the default MSACSR, as the scalar FPU does
 * under the default FCSR. */
#include <msa.h>
#include <stddef.h>
#include <stdint.h>

#define FULL_SCALE 2147483648.0f
#define LOAD(p) __msa_ld_w((void *)(p), 0)
#define STORE(v, p) __msa_st_w((v4i32)(v), (void *)(p), 0)

static inline v4f32 splat(float x) { return (v4f32){x, x, x, x}; }
static inline v4i32 pcm(v4i32 bits, v4f32 full) { return __msa_ftrunc_s_w(__msa_fmul_w((v4f32)bits, full)); }
static inline v4i32 scaled(v4i32 x, v4f32 gain) { return __msa_ftrunc_s_w(__msa_fmul_w(__msa_ffint_s_w(x), gain)); }
static inline v4i32 unit(v4i32 x, v4f32 inverse) { return (v4i32)__msa_fmul_w(__msa_ffint_s_w(x), inverse); }

/* ilvr(b, a) is a0 b0 a1 b1; ilvl(b, a) is a2 b2 a3 b3. `gained` may be null. */
static inline void frames4(v4i32 a, v4i32 b, int32_t *unity, int32_t *gained, v4f32 gain) {
 v4i32 low = __msa_ilvr_w(b, a), high = __msa_ilvl_w(b, a);
 STORE(low, unity); STORE(high, unity + 4);
 if (gained) { STORE(scaled(low, gain), gained); STORE(scaled(high, gain), gained + 4); }
}

/* Two f32 planes to interleaved S32, and optionally that times the gain. */
void q2_pcm_stereo_f32(const float *l, const float *r, int32_t *unity, int32_t *gained, float gain, size_t frames) {
 v4f32 full = splat(FULL_SCALE), g = splat(gain);
 for (size_t i = 0; i < frames; i += 4)
  frames4(pcm(LOAD(l + i), full), pcm(LOAD(r + i), full), unity + 2 * i, gained ? gained + 2 * i : 0, g);
}

void q2_pcm_stereo_s32(const int32_t *l, const int32_t *r, int32_t *unity, int32_t *gained, float gain, size_t frames) {
 v4f32 g = splat(gain);
 for (size_t i = 0; i < frames; i += 4)
  frames4(LOAD(l + i), LOAD(r + i), unity + 2 * i, gained ? gained + 2 * i : 0, g);
}

void q2_pcm_mono_f32(const float *source, int32_t *unity, size_t count) {
 v4f32 full = splat(FULL_SCALE);
 for (size_t i = 0; i < count; i += 4) STORE(pcm(LOAD(source + i), full), unity + i);
}

void q2_pcm_gain(const int32_t *source, int32_t *out, float gain, size_t count) {
 v4f32 g = splat(gain);
 for (size_t i = 0; i < count; i += 4) STORE(scaled(LOAD(source + i), g), out + i);
}

/* Planar to interleaved f32 for the equalizer. */
void q2_pcm_stereo_f32_floats(const float *l, const float *r, float *floats, size_t frames) {
 for (size_t i = 0; i < frames; i += 4) {
  v4i32 a = LOAD(l + i), b = LOAD(r + i);
  STORE(__msa_ilvr_w(b, a), floats + 2 * i); STORE(__msa_ilvl_w(b, a), floats + 2 * i + 4);
 }
}

void q2_pcm_stereo_s32_floats(const int32_t *l, const int32_t *r, float *floats, size_t frames) {
 v4f32 inverse = splat(1.0f / FULL_SCALE);
 for (size_t i = 0; i < frames; i += 4) {
  v4i32 a = unit(LOAD(l + i), inverse), b = unit(LOAD(r + i), inverse);
  STORE(__msa_ilvr_w(b, a), floats + 2 * i); STORE(__msa_ilvl_w(b, a), floats + 2 * i + 4);
 }
}

/* Equalized f32 times an f64 scale to S32: the products truncate to saturated
 * i64 (NaN to 0), saturate to 32 bits, and the even words are the results. */
void q2_pcm_floats_pcm(const float *floats, int32_t *out, double scale, size_t count) {
 v2f64 k = (v2f64){scale, scale};
 for (size_t i = 0; i < count; i += 4) {
  v4f32 v = (v4f32)LOAD(floats + i);
  v2i64 low = __msa_sat_s_d(__msa_ftrunc_s_d(__msa_fmul_d(__msa_fexupr_d(v), k)), 31);
  v2i64 high = __msa_sat_s_d(__msa_ftrunc_s_d(__msa_fmul_d(__msa_fexupl_d(v), k)), 31);
  STORE(__msa_pckev_w((v4i32)high, (v4i32)low), out + i);
 }
}
