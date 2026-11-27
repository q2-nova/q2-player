/* Stereo f64 biquads. Local SIMD only; scalar callers retain their ABI.
 * No allocation, fused arithmetic, coefficient design or state across calls. */
#include <stddef.h>
#include <stdint.h>
#if defined(__mips_msa)
#include <msa.h>
#else
/* Native tests exercise the same packet/FFI layout and equations. */
typedef double v2f64 __attribute__((vector_size(16)));
typedef uint64_t v2u64 __attribute__((vector_size(16)));
#endif
typedef struct { double b[3], a[2], s[2][2]; } Filter;
typedef struct { v2f64 b0,b1,b2,a0,a1,s0,s1; } Packed;
static v2f64 splat(double x) { return (v2f64){x,x}; }
static v2f64 silence(v2f64 x) {
 v2u64 absolute=(v2u64)x & (v2u64){UINT64_C(0x7fffffffffffffff),UINT64_C(0x7fffffffffffffff)};
 return (v2f64)((v2u64)x & ~(v2u64)((v2f64)absolute < splat(1e-24)));
}
void q2_eq_stereo(Filter *f,size_t count,float *pcm,size_t frames,double gain) {
 /* Rust enforces the 20 headphone + 10 graphic maximum before entry. */
 Packed packed[30];
 for(size_t k=0;k<count;k++) {
  Filter *q=&f[k];Packed *p=&packed[k];
  p->b0=splat(q->b[0]);p->b1=splat(q->b[1]);p->b2=splat(q->b[2]);
  p->a0=splat(q->a[0]);p->a1=splat(q->a[1]);
  p->s0=(v2f64){q->s[0][0],q->s[1][0]};p->s1=(v2f64){q->s[0][1],q->s[1][1]};
 }
 for(size_t frame=0;frame<frames;frame++) {
  v2f64 x={(double)pcm[frame*2],(double)pcm[frame*2+1]};x=x*splat(gain);
  for(size_t k=0;k<count;k++) {
   Packed *q=&packed[k];v2f64 y=q->b0*x+q->s0;
   q->s0=silence((q->b1*x-q->a0*y)+q->s1);
   q->s1=silence(q->b2*x-q->a1*y);x=y;
  }
  pcm[frame*2]=(float)x[0];pcm[frame*2+1]=(float)x[1];
 }
 for(size_t k=0;k<count;k++) {
  Packed *p=&packed[k];Filter *q=&f[k];
  q->s[0][0]=p->s0[0];q->s[1][0]=p->s0[1];q->s[0][1]=p->s1[0];q->s[1][1]=p->s1[1];
 }
}
