// Shared exact-limb math and deterministic PRNG.
fn v_zero() -> V {
var r: V; for(var i: u32 = 0u;i<8u;i=i+1u){r.w[i]=0u;} return r;
}

fn v_small(x: u32) -> V {
var r: V = v_zero(); r.w[0]=x; return r;
}

fn v_make(x0: u32, x1: u32, x2: u32, x3: u32, x4: u32, x5: u32, x6: u32, x7: u32) -> V {
var r: V; r.w[0]=x0;r.w[1]=x1;r.w[2]=x2;r.w[3]=x3;r.w[4]=x4;r.w[5]=x5;r.w[6]=x6;r.w[7]=x7; return r;
}

fn v_result(v: V, ok: bool) -> R {
var r: R; r.value=v; r.ok=ok; return r;
}

fn v_eq(a: V, b: V) -> bool {
for(var i: u32 = 0u;i<8u;i=i+1u){if(a.w[i]!=b.w[i]){return false;}} return true;
}

fn v_truth(a: V) -> bool {
return a.w[0]!=0u;
}

fn v_bool(a: bool) -> V {
if(a){return v_small(1u);} return v_zero();
}

fn v_negative(a: V) -> bool {
return (a.w[7]&2147483648u)!=0u;
}

fn v_ucmp(a: V, b: V) -> u32 {
var i: u32 = 8u; while(i>0u){i=i-1u;if(a.w[i]<b.w[i]){return 0u;}if(a.w[i]>b.w[i]){return 2u;}}return 1u;
}

fn v_cmp(a: V, b: V, signed_: bool) -> u32 {
if(signed_ && v_negative(a)!=v_negative(b)){if(v_negative(a)){return 0u;}return 2u;}return v_ucmp(a,b);
}

fn v_add(a: V, b: V) -> V {
var r: V;var carry: u32 = 0u;for(var i: u32 = 0u;i<8u;i=i+1u){var x: u32 = a.w[i]+b.w[i];var c: u32 = 0u;if(x<a.w[i]){c=1u;}var y: u32 = x+carry;if(y<x){c=1u;}r.w[i]=y;carry=c;}return r;
}

fn v_not(a: V) -> V {
var r: V;for(var i: u32 = 0u;i<8u;i=i+1u){r.w[i]=~a.w[i];}return r;
}

fn v_neg(a: V) -> V {
return v_add(v_not(a),v_small(1u));
}

fn v_sub(a: V, b: V) -> V {
return v_add(a,v_neg(b));
}

fn v_norm(a: V, width: u32, signed_: bool) -> V {
var r: V = a;var fill: u32 = 0u;var last: u32 = (width-1u)/32u;var bits: u32 = width-last*32u;if(signed_ && (a.w[last]&(1u<<(bits-1u)))!=0u){fill=4294967295u;}if(bits<32u){var mask: u32 = (1u<<bits)-1u;r.w[last]=(a.w[last]&mask)|(fill&~mask);}for(var i: u32 = last+1u;i<8u;i=i+1u){r.w[i]=fill;}return r;
}

fn v_fits(a: V, width: u32, signed_: bool) -> bool {
return v_eq(a,v_norm(a,width,signed_));
}

fn v_xor(a: V, b: V) -> V {
var r: V;for(var i: u32 = 0u;i<8u;i=i+1u){r.w[i]=a.w[i]^b.w[i];}return r;
}

fn v_shr(a: V, bits: u32) -> V {
var r: V = v_zero();var words: u32 = bits/32u;var n: u32 = bits%32u;for(var i: u32 = 0u;i<8u;i=i+1u){var j: u32 = i+words;if(j<8u){r.w[i]=a.w[j]>>n;if(n!=0u && j+1u<8u){r.w[i]=r.w[i]|(a.w[j+1u]<<(32u-n));}}}return r;
}

fn v_shl(a: V, bits: u32) -> V {
var r: V = v_zero();var words: u32 = bits/32u;var n: u32 = bits%32u;for(var i: u32 = words;i<8u;i=i+1u){var j: u32 = i-words;r.w[i]=a.w[j]<<n;if(n!=0u && j>0u){r.w[i]=r.w[i]|(a.w[j-1u]>>(32u-n));}}return r;
}

fn w_zero() -> W {
var r: W;for(var i: u32 = 0u;i<16u;i=i+1u){r.w[i]=0u;}return r;
}

fn w_from(a: V) -> W {
var r: W = w_zero();for(var i: u32 = 0u;i<8u;i=i+1u){r.w[i]=a.w[i];}return r;
}

fn w_low(a: W) -> V {
var r: V;for(var i: u32 = 0u;i<8u;i=i+1u){r.w[i]=a.w[i];}return r;
}

fn w_eqzero(a: W) -> bool {
for(var i: u32 = 0u;i<16u;i=i+1u){if(a.w[i]!=0u){return false;}}return true;
}

fn w_ge(a: W, b: W) -> bool {
var i: u32 = 16u;while(i>0u){i=i-1u;if(a.w[i]>b.w[i]){return true;}if(a.w[i]<b.w[i]){return false;}}return true;
}

fn w_sub(a: W, b: W) -> W {
var r: W;var borrow: u32 = 0u;for(var i: u32 = 0u;i<16u;i=i+1u){var x: u32 = a.w[i]-b.w[i];var c: u32 = 0u;if(a.w[i]<b.w[i]){c=1u;}var y: u32 = x-borrow;if(x<borrow){c=1u;}r.w[i]=y;borrow=c;}return r;
}

fn w_shl1(a: W) -> W {
var r: W;var carry: u32 = 0u;for(var i: u32 = 0u;i<16u;i=i+1u){r.w[i]=(a.w[i]<<1u)|carry;carry=a.w[i]>>31u;}return r;
}

fn w_mul(a: V, b: V) -> W {
var digits: array<u32, 32>;for(var i: u32 = 0u;i<32u;i=i+1u){digits[i]=0u;}for(var i: u32 = 0u;i<16u;i=i+1u){var av: u32 = (a.w[i/2u]>>((i%2u)*16u))&65535u;var carry: u32 = 0u;for(var j: u32 = 0u;j<16u;j=j+1u){var bv: u32 = (b.w[j/2u]>>((j%2u)*16u))&65535u;var x: u32 = digits[i+j]+av*bv+carry;digits[i+j]=x&65535u;carry=x>>16u;}digits[i+16u]=carry;}var r: W;for(var i: u32 = 0u;i<16u;i=i+1u){r.w[i]=digits[i*2u]|(digits[i*2u+1u]<<16u);}return r;
}

fn w_div(a: W, b: W) -> D {
var d: D;d.q=w_zero();d.r=w_zero();var i: u32 = 512u;while(i>0u){i=i-1u;d.r=w_shl1(d.r);d.r.w[0]=d.r.w[0]|((a.w[i/32u]>>(i%32u))&1u);if(w_ge(d.r,b)){d.r=w_sub(d.r,b);d.q.w[i/32u]=d.q.w[i/32u]|(1u<<(i%32u));}}return d;
}

fn v_mag_result(mag: W, negative: bool, width: u32, signed_: bool) -> R {
for(var i: u32 = 8u;i<16u;i=i+1u){if(mag.w[i]!=0u){return v_result(v_zero(),false);}}var r: V = w_low(mag);if(negative){r=v_neg(r);}if(!v_fits(r,width,signed_)){return v_result(v_zero(),false);}if(signed_ && !v_eq(r,v_zero()) && v_negative(r)!=negative){return v_result(v_zero(),false);}return v_result(r,true);
}

fn v_arith(a: V, b: V, op: u32, width: u32, signed_: bool, scale: V) -> R {
if(op<2u){var r: V = v_zero();if(op==0u){r=v_add(a,b);}else{r=v_sub(a,b);}var ok: bool = v_fits(r,width,signed_);if(width==256u){if(signed_){if(op==0u && v_negative(a)==v_negative(b) && v_negative(a)!=v_negative(r)){ok=false;}if(op==1u && v_negative(a)!=v_negative(b) && v_negative(a)!=v_negative(r)){ok=false;}}else{if(op==0u && v_ucmp(r,a)==0u){ok=false;}if(op==1u && v_ucmp(a,b)==0u){ok=false;}}}return v_result(r,ok);}var an: bool = signed_ && v_negative(a);var bn: bool = signed_ && v_negative(b);var aa: V = a;var bb: V = b;if(an){aa=v_neg(a);}if(bn){bb=v_neg(b);}if(op==2u){var p: W = w_mul(aa,bb);if(!v_eq(scale,v_small(1u))){var d: D = w_div(p,w_from(scale));if(!w_eqzero(d.r)){return v_result(v_zero(),false);}p=d.q;}return v_mag_result(p,an!=bn,width,signed_);}if(v_eq(bb,v_zero())){return v_result(v_zero(),false);}if(signed_ && an && bn && v_eq(bb,v_small(1u)) && !v_fits(aa,width,true)){return v_result(v_zero(),false);}var num: W = w_from(aa);if(op==3u && !v_eq(scale,v_small(1u))){num=w_mul(aa,scale);}var d: D = w_div(num,w_from(bb));if(op==4u){return v_mag_result(d.r,an,width,signed_);}if(!v_eq(scale,v_small(1u)) && !w_eqzero(d.r)){return v_result(v_zero(),false);}return v_mag_result(d.q,an!=bn,width,signed_);
}

fn v_mul64(a: V, b: V) -> V {
var p: W = w_mul(v_norm(a,64u,false),v_norm(b,64u,false));return v_norm(w_low(p),64u,false);
}

fn vera_mix(a: V) -> V {
var z: V = v_norm(a,64u,false);z=v_xor(z,v_shr(z,30u));z=v_mul64(z,v_make(484763065u,3210233709u,0u,0u,0u,0u,0u,0u));z=v_xor(z,v_shr(z,27u));z=v_mul64(z,v_make(321982955u,2496678331u,0u,0u,0u,0u,0u,0u));return v_xor(z,v_shr(z,31u));
}

fn xoshiro_next(ctx: ptr<function, Ctx>) -> u32 {
var result: u32 = (*ctx).rng[1]*5u;result=((result<<7u)|(result>>25u))*9u;var t: u32 = (*ctx).rng[1]<<9u;(*ctx).rng[2]=(*ctx).rng[2]^(*ctx).rng[0];(*ctx).rng[3]=(*ctx).rng[3]^(*ctx).rng[1];(*ctx).rng[1]=(*ctx).rng[1]^(*ctx).rng[2];(*ctx).rng[0]=(*ctx).rng[0]^(*ctx).rng[3];(*ctx).rng[2]=(*ctx).rng[2]^t;var x: u32 = (*ctx).rng[3];(*ctx).rng[3]=(x<<11u)|(x>>21u);return result;
}
