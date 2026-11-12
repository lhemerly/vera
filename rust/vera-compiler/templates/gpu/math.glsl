// Shared exact-limb math and deterministic PRNG.
V v_zero() {
V r; for(uint i = 0u;i<8u;i=i+1u){r.w[i]=0u;} return r;
}

V v_small(uint x) {
V r = v_zero(); r.w[0]=x; return r;
}

V v_make(uint x0, uint x1, uint x2, uint x3, uint x4, uint x5, uint x6, uint x7) {
V r; r.w[0]=x0;r.w[1]=x1;r.w[2]=x2;r.w[3]=x3;r.w[4]=x4;r.w[5]=x5;r.w[6]=x6;r.w[7]=x7; return r;
}

R v_result(V v, bool ok) {
R r; r.value=v; r.ok=ok; return r;
}

bool v_eq(V a, V b) {
for(uint i = 0u;i<8u;i=i+1u){if(a.w[i]!=b.w[i]){return false;}} return true;
}

bool v_truth(V a) {
return a.w[0]!=0u;
}

V v_bool(bool a) {
if(a){return v_small(1u);} return v_zero();
}

bool v_negative(V a) {
return (a.w[7]&2147483648u)!=0u;
}

uint v_ucmp(V a, V b) {
uint i = 8u; while(i>0u){i=i-1u;if(a.w[i]<b.w[i]){return 0u;}if(a.w[i]>b.w[i]){return 2u;}}return 1u;
}

uint v_cmp(V a, V b, bool signed_) {
if(signed_ && v_negative(a)!=v_negative(b)){if(v_negative(a)){return 0u;}return 2u;}return v_ucmp(a,b);
}

V v_add(V a, V b) {
V r;uint carry = 0u;for(uint i = 0u;i<8u;i=i+1u){uint x = a.w[i]+b.w[i];uint c = 0u;if(x<a.w[i]){c=1u;}uint y = x+carry;if(y<x){c=1u;}r.w[i]=y;carry=c;}return r;
}

V v_not(V a) {
V r;for(uint i = 0u;i<8u;i=i+1u){r.w[i]=~a.w[i];}return r;
}

V v_neg(V a) {
return v_add(v_not(a),v_small(1u));
}

V v_sub(V a, V b) {
return v_add(a,v_neg(b));
}

V v_norm(V a, uint width, bool signed_) {
V r = a;uint fill = 0u;uint last = (width-1u)/32u;uint bits = width-last*32u;if(signed_ && (a.w[last]&(1u<<(bits-1u)))!=0u){fill=4294967295u;}if(bits<32u){uint mask = (1u<<bits)-1u;r.w[last]=(a.w[last]&mask)|(fill&~mask);}for(uint i = last+1u;i<8u;i=i+1u){r.w[i]=fill;}return r;
}

bool v_fits(V a, uint width, bool signed_) {
return v_eq(a,v_norm(a,width,signed_));
}

V v_xor(V a, V b) {
V r;for(uint i = 0u;i<8u;i=i+1u){r.w[i]=a.w[i]^b.w[i];}return r;
}

V v_shr(V a, uint bits) {
V r = v_zero();uint words = bits/32u;uint n = bits%32u;for(uint i = 0u;i<8u;i=i+1u){uint j = i+words;if(j<8u){r.w[i]=a.w[j]>>n;if(n!=0u && j+1u<8u){r.w[i]=r.w[i]|(a.w[j+1u]<<(32u-n));}}}return r;
}

V v_shl(V a, uint bits) {
V r = v_zero();uint words = bits/32u;uint n = bits%32u;for(uint i = words;i<8u;i=i+1u){uint j = i-words;r.w[i]=a.w[j]<<n;if(n!=0u && j>0u){r.w[i]=r.w[i]|(a.w[j-1u]>>(32u-n));}}return r;
}

W w_zero() {
W r;for(uint i = 0u;i<16u;i=i+1u){r.w[i]=0u;}return r;
}

W w_from(V a) {
W r = w_zero();for(uint i = 0u;i<8u;i=i+1u){r.w[i]=a.w[i];}return r;
}

V w_low(W a) {
V r;for(uint i = 0u;i<8u;i=i+1u){r.w[i]=a.w[i];}return r;
}

bool w_eqzero(W a) {
for(uint i = 0u;i<16u;i=i+1u){if(a.w[i]!=0u){return false;}}return true;
}

bool w_ge(W a, W b) {
uint i = 16u;while(i>0u){i=i-1u;if(a.w[i]>b.w[i]){return true;}if(a.w[i]<b.w[i]){return false;}}return true;
}

W w_sub(W a, W b) {
W r;uint borrow = 0u;for(uint i = 0u;i<16u;i=i+1u){uint x = a.w[i]-b.w[i];uint c = 0u;if(a.w[i]<b.w[i]){c=1u;}uint y = x-borrow;if(x<borrow){c=1u;}r.w[i]=y;borrow=c;}return r;
}

W w_shl1(W a) {
W r;uint carry = 0u;for(uint i = 0u;i<16u;i=i+1u){r.w[i]=(a.w[i]<<1u)|carry;carry=a.w[i]>>31u;}return r;
}

W w_mul(V a, V b) {
uint digits[32];for(uint i = 0u;i<32u;i=i+1u){digits[i]=0u;}for(uint i = 0u;i<16u;i=i+1u){uint av = (a.w[i/2u]>>((i%2u)*16u))&65535u;uint carry = 0u;for(uint j = 0u;j<16u;j=j+1u){uint bv = (b.w[j/2u]>>((j%2u)*16u))&65535u;uint x = digits[i+j]+av*bv+carry;digits[i+j]=x&65535u;carry=x>>16u;}digits[i+16u]=carry;}W r;for(uint i = 0u;i<16u;i=i+1u){r.w[i]=digits[i*2u]|(digits[i*2u+1u]<<16u);}return r;
}

D w_div(W a, W b) {
D d;d.q=w_zero();d.r=w_zero();uint i = 512u;while(i>0u){i=i-1u;d.r=w_shl1(d.r);d.r.w[0]=d.r.w[0]|((a.w[i/32u]>>(i%32u))&1u);if(w_ge(d.r,b)){d.r=w_sub(d.r,b);d.q.w[i/32u]=d.q.w[i/32u]|(1u<<(i%32u));}}return d;
}

R v_mag_result(W mag, bool negative, uint width, bool signed_) {
for(uint i = 8u;i<16u;i=i+1u){if(mag.w[i]!=0u){return v_result(v_zero(),false);}}V r = w_low(mag);if(negative){r=v_neg(r);}if(!v_fits(r,width,signed_)){return v_result(v_zero(),false);}if(signed_ && !v_eq(r,v_zero()) && v_negative(r)!=negative){return v_result(v_zero(),false);}return v_result(r,true);
}

R v_arith(V a, V b, uint op, uint width, bool signed_, V scale) {
if(op<2u){V r = v_zero();if(op==0u){r=v_add(a,b);}else{r=v_sub(a,b);}bool ok = v_fits(r,width,signed_);if(width==256u){if(signed_){if(op==0u && v_negative(a)==v_negative(b) && v_negative(a)!=v_negative(r)){ok=false;}if(op==1u && v_negative(a)!=v_negative(b) && v_negative(a)!=v_negative(r)){ok=false;}}else{if(op==0u && v_ucmp(r,a)==0u){ok=false;}if(op==1u && v_ucmp(a,b)==0u){ok=false;}}}return v_result(r,ok);}bool an = signed_ && v_negative(a);bool bn = signed_ && v_negative(b);V aa = a;V bb = b;if(an){aa=v_neg(a);}if(bn){bb=v_neg(b);}if(op==2u){W p = w_mul(aa,bb);if(!v_eq(scale,v_small(1u))){D d = w_div(p,w_from(scale));if(!w_eqzero(d.r)){return v_result(v_zero(),false);}p=d.q;}return v_mag_result(p,an!=bn,width,signed_);}if(v_eq(bb,v_zero())){return v_result(v_zero(),false);}if(signed_ && an && bn && v_eq(bb,v_small(1u)) && !v_fits(aa,width,true)){return v_result(v_zero(),false);}W num = w_from(aa);if(op==3u && !v_eq(scale,v_small(1u))){num=w_mul(aa,scale);}D d = w_div(num,w_from(bb));if(op==4u){return v_mag_result(d.r,an,width,signed_);}if(!v_eq(scale,v_small(1u)) && !w_eqzero(d.r)){return v_result(v_zero(),false);}return v_mag_result(d.q,an!=bn,width,signed_);
}

V v_mul64(V a, V b) {
// Exact low 64 bits; never construct a 512-bit product for hashing.
uint a0=a.w[0]&65535u;uint a1=a.w[0]>>16u;uint b0=b.w[0]&65535u;uint b1=b.w[0]>>16u;
uint p0=a0*b0;uint p1=a1*b0+(p0>>16u);uint p2=a0*b1+(p1&65535u);
V r=v_zero();r.w[0]=(p2<<16u)|(p0&65535u);r.w[1]=a1*b1+(p1>>16u)+(p2>>16u)+a.w[1]*b.w[0]+a.w[0]*b.w[1];return r;
}

V vera_mix(V a) {
V z = v_norm(a,64u,false);z=v_xor(z,v_shr(z,30u));z=v_mul64(z,v_make(484763065u,3210233709u,0u,0u,0u,0u,0u,0u));z=v_xor(z,v_shr(z,27u));z=v_mul64(z,v_make(321982955u,2496678331u,0u,0u,0u,0u,0u,0u));return v_xor(z,v_shr(z,31u));
}

uint xoshiro_next(inout Ctx ctx) {
uint result = ctx.rng[1]*5u;result=((result<<7u)|(result>>25u))*9u;uint t = ctx.rng[1]<<9u;ctx.rng[2]=ctx.rng[2]^ctx.rng[0];ctx.rng[3]=ctx.rng[3]^ctx.rng[1];ctx.rng[1]=ctx.rng[1]^ctx.rng[2];ctx.rng[0]=ctx.rng[0]^ctx.rng[3];ctx.rng[2]=ctx.rng[2]^t;uint x = ctx.rng[3];ctx.rng[3]=(x<<11u)|(x>>21u);return result;
}
