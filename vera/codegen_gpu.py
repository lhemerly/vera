"""Shared exact GPU lowering for CUDA, GLSL and WGSL.

Eight little-endian 32-bit limbs represent values; multiplication/division use
512-bit intermediates. Every invocation owns a transactional sandbox. Furnace
outputs count commits, reverts, violations and committed effects, retaining an
argument digest inside each sandbox. No native 64-bit shader type is required.
"""

import re
from . import ast_nodes as A
from .errors import VeraError
from .semantic import Program, _TypeChecker, _resolve_type_ref
from .types import BOOL, MAX_PROBES, accumulator_type, id64, literal_value, symbol_id


def _name(s):
    return "v_" + re.sub("[^A-Za-z0-9_]", "_", s)


class GpuGenerator:
    def __init__(self, prog: Program, target: str, depth: int = 64):
        if prog.diags.has_errors():
            raise VeraError(
                "D_GPU", "GPU code generation requires a valid analyzed program"
            )
        if depth <= 0:
            raise VeraError("D_GPU", "furnace depth must be positive")
        self.prog, self.target, self.depth = prog, target, depth
        self.lines = []
        self.counter = 0
        self.env_types = {}
        self.table_offsets = {}
        self.col_offsets = {}
        self.agg_offsets = {}
        self.hash_offsets = {}
        nr = nc = na = nh = 0
        for name, t in prog.tables.items():
            if t.layout.key_bits >= 32:
                raise VeraError(
                    "D_GPU_RESOURCE", "GPU capacities must fit a 32-bit slot index"
                )
            self.table_offsets[name] = nr
            nr += t.layout.capacity
            for col in t.columns.values():
                self.col_offsets[name, col.name] = nc
                nc += t.layout.capacity
                if col.tracked_sum:
                    self.agg_offsets[name, col.name] = na
                    na += 1
                if col.unique:
                    self.hash_offsets[name, col.name] = nh
                    nh += t.layout.unique_hashes[col.name]
        self.nrows, self.ncols, self.naggs, self.nhash = nr, nc, na, nh

    def tn(self, name):
        return "vera_table_" + str(list(self.prog.tables).index(name))

    def cn(self, table, column):
        return (
            self.tn(table)
            + "_column_"
            + str(list(self.prog.tables[table].columns).index(column))
        )

    @property
    def ctx(self):
        return "(*ctx)" if self.target != "glsl" else "ctx"

    def w(self, s=""):
        self.lines.append(s)

    def var(self, name, ty, value=None):
        suffix = "" if value is None else " = " + value
        if self.target == "wgsl":
            m = re.fullmatch(r"(.+)\[(\d+)\]", ty)
            if m:
                ty = f"array<{m[1]}, {m[2]}>"
            return f"var {name}: {ty}{suffix};"
        return f"{ty} {name}{suffix};"

    def syntax(self, body):
        def decl(m):
            ty, name, size, value = m.group(1), m.group(2), m.group(3), m.group(4)
            if self.target == "wgsl":
                t = f"array<{ty}, {size}>" if size else ty
                return f"var {name}: {t}" + ((" = " + value) if value else "") + ";"
            return (
                ty
                + " "
                + name
                + (("[" + size + "]") if size else "")
                + ((" = " + value) if value else "")
                + ";"
            )

        body = re.sub(
            r"\b(u32|bool|V|W|R|D|Ctx|State)\s+([A-Za-z_]\w*)(?:\[(\d+)\])?\s*(?:=\s*([^;\n]+))?;",
            decl,
            body,
        )
        if self.target != "wgsl":
            body = re.sub(
                r"\bu32\b", "uint" if self.target == "glsl" else "uint32_t", body
            )
        return body

    def fn(self, name, ret, params, body):
        if self.target == "wgsl":
            ps = ", ".join(n + ": " + t for n, t in params)
            self.w(f"fn {name}({ps})" + (" -> " + ret if ret != "void" else "") + " {")
        else:
            ps = ", ".join(t + " " + n for n, t in params)
            self.w(
                ("__device__ inline " if self.target == "cuda" else "")
                + f"{ret} {name}({ps}) {{"
            )
        self.w(self.syntax(body))
        self.w("}\n")

    def cparams(self, rest=()):
        ct = (
            "ptr<function, Ctx>"
            if self.target == "wgsl"
            else ("inout Ctx" if self.target == "glsl" else "Ctx*")
        )
        return [("ctx", ct)] + list(rest)

    def struct(self, name, fields):
        self.w(f"struct {name} {{")
        for field, ty, size in fields:
            if self.target == "wgsl":
                ty = f"array<{ty}, {max(1, size)}>" if size is not None else ty
                self.w(f"    {field}: {ty},")
            else:
                self.w(
                    f"    {ty} {field}"
                    + (f"[{max(1, size)}]" if size is not None else "")
                    + ";"
                )
        self.w("};" if self.target != "wgsl" else "}")

    def emit(self):
        self.w("// Generated by VERA: exact 32-bit-limb GPU semantics.")
        if self.target == "glsl":
            self.w("#version 460 core\nlayout(local_size_x=32) in;")
            self.w(
                "layout(std430,binding=0) buffer Output { uint violations; uint committed; uint reverted; uint effects; };"
            )
        elif self.target == "wgsl":
            self.w(
                "struct Output { violations:atomic<u32>, committed:atomic<u32>, reverted:atomic<u32>, effects:atomic<u32>, }"
            )
            self.w("@group(0) @binding(0) var<storage,read_write> output:Output;")
        else:
            self.w(
                "#include <cstdint>\n#ifndef VERA_GPU_HOST\n#include <cuda_runtime.h>\n#else\n#define __device__\n#define __global__\nstruct VeraDim { uint32_t x; };\nstatic VeraDim blockIdx={0}, blockDim={32}, threadIdx={0};\ninline void atomicAdd(uint32_t* p,uint32_t x){*p+=x;}\n#endif"
            )
        self.struct("V", [("w", "u32", 8)])
        self.struct("W", [("w", "u32", 16)])
        self.struct("R", [("value", "V", None), ("ok", "bool", None)])
        self.struct("D", [("q", "W", None), ("r", "W", None)])
        self.struct(
            "State",
            [
                ("occ", "u32", self.nrows),
                ("gen", "u32", self.nrows),
                ("cols", "V", self.ncols),
                ("aggs", "V", self.naggs),
                ("hused", "u32", self.nhash),
                ("hslot", "u32", self.nhash),
                ("effect_count", "u32", None),
                ("effect_digest", "V", None),
            ],
        )
        self.struct(
            "Ctx",
            [
                ("s", "State", None),
                ("pre", "State", None),
                ("rng", "u32", 4),
                ("trap", "u32", None),
            ],
        )
        self._math()
        self._random()
        self._table_helpers()
        self._check_function("vera_invariants", list(self.prog.invariants.values()))
        for name, ti in self.prog.transitions.items():
            self._transition(name, ti)
        self._init()
        self._main()
        source = "\n".join(self.lines) + "\n"
        if self.target != "wgsl":
            source = re.sub(
                r"\bu32\b", "uint" if self.target == "glsl" else "uint32_t", source
            )
        return source

    def _math(self):
        f = self.fn
        f("v_zero", "V", [], """V r; for(u32 i=0u;i<8u;i=i+1u){r.w[i]=0u;} return r;""")
        f("v_small", "V", [("x", "u32")], """V r=v_zero(); r.w[0]=x; return r;""")
        f(
            "v_make",
            "V",
            [(f"x{i}", "u32") for i in range(8)],
            "V r; " + "".join(f"r.w[{i}]=x{i};" for i in range(8)) + " return r;",
        )
        f(
            "v_result",
            "R",
            [("v", "V"), ("ok", "bool")],
            """R r; r.value=v; r.ok=ok; return r;""",
        )
        f(
            "v_eq",
            "bool",
            [("a", "V"), ("b", "V")],
            """for(u32 i=0u;i<8u;i=i+1u){if(a.w[i]!=b.w[i]){return false;}} return true;""",
        )
        f("v_truth", "bool", [("a", "V")], """return a.w[0]!=0u;""")
        f(
            "v_bool",
            "V",
            [("a", "bool")],
            """if(a){return v_small(1u);} return v_zero();""",
        )
        f("v_negative", "bool", [("a", "V")], """return (a.w[7]&2147483648u)!=0u;""")
        f(
            "v_ucmp",
            "u32",
            [("a", "V"), ("b", "V")],
            """u32 i=8u; while(i>0u){i=i-1u;if(a.w[i]<b.w[i]){return 0u;}if(a.w[i]>b.w[i]){return 2u;}}return 1u;""",
        )
        f(
            "v_cmp",
            "u32",
            [("a", "V"), ("b", "V"), ("signed_", "bool")],
            """if(signed_ && v_negative(a)!=v_negative(b)){if(v_negative(a)){return 0u;}return 2u;}return v_ucmp(a,b);""",
        )
        f(
            "v_add",
            "V",
            [("a", "V"), ("b", "V")],
            """V r;u32 carry=0u;for(u32 i=0u;i<8u;i=i+1u){u32 x=a.w[i]+b.w[i];u32 c=0u;if(x<a.w[i]){c=1u;}u32 y=x+carry;if(y<x){c=1u;}r.w[i]=y;carry=c;}return r;""",
        )
        f(
            "v_not",
            "V",
            [("a", "V")],
            """V r;for(u32 i=0u;i<8u;i=i+1u){r.w[i]=~a.w[i];}return r;""",
        )
        f("v_neg", "V", [("a", "V")], """return v_add(v_not(a),v_small(1u));""")
        f("v_sub", "V", [("a", "V"), ("b", "V")], """return v_add(a,v_neg(b));""")
        f(
            "v_norm",
            "V",
            [("a", "V"), ("width", "u32"), ("signed_", "bool")],
            """V r=a;u32 fill=0u;u32 last=(width-1u)/32u;u32 bits=width-last*32u;if(signed_ && (a.w[last]&(1u<<(bits-1u)))!=0u){fill=4294967295u;}if(bits<32u){u32 mask=(1u<<bits)-1u;r.w[last]=(a.w[last]&mask)|(fill&~mask);}for(u32 i=last+1u;i<8u;i=i+1u){r.w[i]=fill;}return r;""",
        )
        f(
            "v_fits",
            "bool",
            [("a", "V"), ("width", "u32"), ("signed_", "bool")],
            """return v_eq(a,v_norm(a,width,signed_));""",
        )
        f(
            "v_xor",
            "V",
            [("a", "V"), ("b", "V")],
            """V r;for(u32 i=0u;i<8u;i=i+1u){r.w[i]=a.w[i]^b.w[i];}return r;""",
        )
        f(
            "v_shr",
            "V",
            [("a", "V"), ("bits", "u32")],
            """V r=v_zero();u32 words=bits/32u;u32 n=bits%32u;for(u32 i=0u;i<8u;i=i+1u){u32 j=i+words;if(j<8u){r.w[i]=a.w[j]>>n;if(n!=0u && j+1u<8u){r.w[i]=r.w[i]|(a.w[j+1u]<<(32u-n));}}}return r;""",
        )
        f(
            "v_shl",
            "V",
            [("a", "V"), ("bits", "u32")],
            """V r=v_zero();u32 words=bits/32u;u32 n=bits%32u;for(u32 i=words;i<8u;i=i+1u){u32 j=i-words;r.w[i]=a.w[j]<<n;if(n!=0u && j>0u){r.w[i]=r.w[i]|(a.w[j-1u]>>(32u-n));}}return r;""",
        )
        f("w_zero", "W", [], """W r;for(u32 i=0u;i<16u;i=i+1u){r.w[i]=0u;}return r;""")
        f(
            "w_from",
            "W",
            [("a", "V")],
            """W r=w_zero();for(u32 i=0u;i<8u;i=i+1u){r.w[i]=a.w[i];}return r;""",
        )
        f(
            "w_low",
            "V",
            [("a", "W")],
            """V r;for(u32 i=0u;i<8u;i=i+1u){r.w[i]=a.w[i];}return r;""",
        )
        f(
            "w_eqzero",
            "bool",
            [("a", "W")],
            """for(u32 i=0u;i<16u;i=i+1u){if(a.w[i]!=0u){return false;}}return true;""",
        )
        f(
            "w_ge",
            "bool",
            [("a", "W"), ("b", "W")],
            """u32 i=16u;while(i>0u){i=i-1u;if(a.w[i]>b.w[i]){return true;}if(a.w[i]<b.w[i]){return false;}}return true;""",
        )
        f(
            "w_sub",
            "W",
            [("a", "W"), ("b", "W")],
            """W r;u32 borrow=0u;for(u32 i=0u;i<16u;i=i+1u){u32 x=a.w[i]-b.w[i];u32 c=0u;if(a.w[i]<b.w[i]){c=1u;}u32 y=x-borrow;if(x<borrow){c=1u;}r.w[i]=y;borrow=c;}return r;""",
        )
        f(
            "w_shl1",
            "W",
            [("a", "W")],
            """W r;u32 carry=0u;for(u32 i=0u;i<16u;i=i+1u){r.w[i]=(a.w[i]<<1u)|carry;carry=a.w[i]>>31u;}return r;""",
        )
        f(
            "w_mul",
            "W",
            [("a", "V"), ("b", "V")],
            """u32 digits[32];for(u32 i=0u;i<32u;i=i+1u){digits[i]=0u;}for(u32 i=0u;i<16u;i=i+1u){u32 av=(a.w[i/2u]>>((i%2u)*16u))&65535u;u32 carry=0u;for(u32 j=0u;j<16u;j=j+1u){u32 bv=(b.w[j/2u]>>((j%2u)*16u))&65535u;u32 x=digits[i+j]+av*bv+carry;digits[i+j]=x&65535u;carry=x>>16u;}digits[i+16u]=carry;}W r;for(u32 i=0u;i<16u;i=i+1u){r.w[i]=digits[i*2u]|(digits[i*2u+1u]<<16u);}return r;""",
        )
        f(
            "w_div",
            "D",
            [("a", "W"), ("b", "W")],
            """D d;d.q=w_zero();d.r=w_zero();u32 i=512u;while(i>0u){i=i-1u;d.r=w_shl1(d.r);d.r.w[0]=d.r.w[0]|((a.w[i/32u]>>(i%32u))&1u);if(w_ge(d.r,b)){d.r=w_sub(d.r,b);d.q.w[i/32u]=d.q.w[i/32u]|(1u<<(i%32u));}}return d;""",
        )
        f(
            "v_mag_result",
            "R",
            [("mag", "W"), ("negative", "bool"), ("width", "u32"), ("signed_", "bool")],
            """for(u32 i=8u;i<16u;i=i+1u){if(mag.w[i]!=0u){return v_result(v_zero(),false);}}V r=w_low(mag);if(negative){r=v_neg(r);}if(!v_fits(r,width,signed_)){return v_result(v_zero(),false);}if(signed_ && !v_eq(r,v_zero()) && v_negative(r)!=negative){return v_result(v_zero(),false);}return v_result(r,true);""",
        )
        f(
            "v_arith",
            "R",
            [
                ("a", "V"),
                ("b", "V"),
                ("op", "u32"),
                ("width", "u32"),
                ("signed_", "bool"),
                ("scale", "V"),
            ],
            """if(op<2u){V r=v_zero();if(op==0u){r=v_add(a,b);}else{r=v_sub(a,b);}bool ok=v_fits(r,width,signed_);if(width==256u){if(signed_){if(op==0u && v_negative(a)==v_negative(b) && v_negative(a)!=v_negative(r)){ok=false;}if(op==1u && v_negative(a)!=v_negative(b) && v_negative(a)!=v_negative(r)){ok=false;}}else{if(op==0u && v_ucmp(r,a)==0u){ok=false;}if(op==1u && v_ucmp(a,b)==0u){ok=false;}}}return v_result(r,ok);}bool an=signed_ && v_negative(a);bool bn=signed_ && v_negative(b);V aa=a;V bb=b;if(an){aa=v_neg(a);}if(bn){bb=v_neg(b);}if(op==2u){W p=w_mul(aa,bb);if(!v_eq(scale,v_small(1u))){D d=w_div(p,w_from(scale));if(!w_eqzero(d.r)){return v_result(v_zero(),false);}p=d.q;}return v_mag_result(p,an!=bn,width,signed_);}if(v_eq(bb,v_zero())){return v_result(v_zero(),false);}if(signed_ && an && bn && v_eq(bb,v_small(1u)) && !v_fits(aa,width,true)){return v_result(v_zero(),false);}W num=w_from(aa);if(op==3u && !v_eq(scale,v_small(1u))){num=w_mul(aa,scale);}D d=w_div(num,w_from(bb));if(op==4u){return v_mag_result(d.r,an,width,signed_);}if(!v_eq(scale,v_small(1u)) && !w_eqzero(d.r)){return v_result(v_zero(),false);}return v_mag_result(d.q,an!=bn,width,signed_);""",
        )
        f(
            "v_mul64",
            "V",
            [("a", "V"), ("b", "V")],
            """W p=w_mul(v_norm(a,64u,false),v_norm(b,64u,false));return v_norm(w_low(p),64u,false);""",
        )
        f(
            "vera_mix",
            "V",
            [("a", "V")],
            """V z=v_norm(a,64u,false);z=v_xor(z,v_shr(z,30u));z=v_mul64(z,v_make(484763065u,3210233709u,0u,0u,0u,0u,0u,0u));z=v_xor(z,v_shr(z,27u));z=v_mul64(z,v_make(321982955u,2496678331u,0u,0u,0u,0u,0u,0u));return v_xor(z,v_shr(z,31u));""",
        )

    def _random(self):
        c = self.ctx
        self.fn(
            "xoshiro_next",
            "u32",
            self.cparams(),
            f"""u32 result={c}.rng[1]*5u;result=((result<<7u)|(result>>25u))*9u;u32 t={c}.rng[1]<<9u;{c}.rng[2]={c}.rng[2]^{c}.rng[0];{c}.rng[3]={c}.rng[3]^{c}.rng[1];{c}.rng[1]={c}.rng[1]^{c}.rng[2];{c}.rng[0]={c}.rng[0]^{c}.rng[3];{c}.rng[2]={c}.rng[2]^t;u32 x={c}.rng[3];{c}.rng[3]=(x<<11u)|(x>>21u);return result;""",
        )

    def const(self, value):
        if isinstance(value, (bytes, bytearray)):
            value = int.from_bytes(value, "little")
        value = int(value) % (1 << 256)
        return (
            "v_make("
            + ",".join(str((value >> (i * 32)) & 0xFFFFFFFF) + "u" for i in range(8))
            + ")"
        )

    def _table_helpers(self):
        c = self.ctx
        for name, t in self.prog.tables.items():
            tn = self.tn(name)
            cap = t.layout.capacity
            kb = t.layout.key_bits
            off = self.table_offsets[name]
            self.fn(tn + "_slot", "u32", [("key", "V")], f"return key.w[0]&{cap - 1}u;")
            gen = (
                "key.w[0]" if kb == 0 else f"((key.w[0]>>{kb}u)|(key.w[1]<<{32 - kb}u))"
            )
            high = "key.w[1]!=0u" if kb == 0 else f"(key.w[1]>>{kb}u)!=0u"
            self.fn(
                tn + "_exists",
                "bool",
                self.cparams([("key", "V"), ("old_", "bool")]),
                f"""for(u32 i=2u;i<8u;i=i+1u){{if(key.w[i]!=0u){{return false;}}}}if({high}){{return false;}}u32 slot={tn}_slot(key)+{off}u;u32 generation={gen};if(old_){{return {c}.pre.occ[slot]!=0u && {c}.pre.gen[slot]==generation;}}return {c}.s.occ[slot]!=0u && {c}.s.gen[slot]==generation;""",
            )
            self.fn(
                tn + "_key",
                "V",
                self.cparams([("slot", "u32"), ("old_", "bool")]),
                f"""u32 generation={c}.s.gen[slot+{off}u];if(old_){{generation={c}.pre.gen[slot+{off}u];}}return v_add(v_shl(v_small(generation),{kb}u),v_small(slot));""",
            )
            for col in t.columns.values():
                co = self.col_offsets[name, col.name]
                cn = self.cn(name, col.name)
                self.fn(
                    cn + "_get",
                    "V",
                    self.cparams([("key", "V"), ("old_", "bool")]),
                    f"""u32 slot={tn}_slot(key)+{co}u;if(old_){{return {c}.pre.cols[slot];}}return {c}.s.cols[slot];""",
                )
                body = f"u32 slot={tn}_slot(key)+{co}u;"
                if col.tracked_sum:
                    ao = self.agg_offsets[name, col.name]
                    body += f"{c}.s.aggs[{ao}]=v_add(v_sub({c}.s.aggs[{ao}],{c}.s.cols[slot]),value);"
                body += f"{c}.s.cols[slot]=value;"
                self.fn(
                    cn + "_set",
                    "void",
                    self.cparams([("key", "V"), ("value", "V")]),
                    body,
                )
                if col.unique:
                    ho = self.hash_offsets[name, col.name]
                    b = t.layout.unique_hashes[col.name]
                    for state in ("s", "pre"):
                        suffix = "" if state == "s" else "_pre"
                        flag = "false" if state == "s" else "true"
                        self.fn(
                            cn + "_lookup" + suffix,
                            "R",
                            self.cparams([("value", "V")]),
                            f"""V hash=vera_mix(value);u32 pos=hash.w[0]&{b - 1}u;for(u32 distance=0u;distance<{MAX_PROBES}u;distance=distance+1u){{if({c}.{state}.hused[pos+{ho}u]==0u){{return v_result(v_zero(),false);}}u32 slot={c}.{state}.hslot[pos+{ho}u];V existing={c}.{state}.cols[slot+{co}u];if(v_eq(existing,value)){{return v_result({tn}_key(ctx,slot,{flag}),true);}}V h=vera_mix(existing);u32 rd=(pos-(h.w[0]&{b - 1}u))&{b - 1}u;if(rd<distance){{return v_result(v_zero(),false);}}pos=(pos+1u)&{b - 1}u;}}return v_result(v_zero(),false);""",
                        )
                    self.fn(
                        cn + "_insert",
                        "bool",
                        self.cparams([("value", "V"), ("slot_", "u32")]),
                        f"""V hash=vera_mix(value);u32 pos=hash.w[0]&{b - 1}u;u32 distance=0u;u32 slot=slot_;for(u32 step=0u;step<{MAX_PROBES}u;step=step+1u){{if({c}.s.hused[pos+{ho}u]==0u){{{c}.s.hused[pos+{ho}u]=1u;{c}.s.hslot[pos+{ho}u]=slot;return true;}}u32 existing={c}.s.hslot[pos+{ho}u];V oldvalue={c}.s.cols[existing+{co}u];if(v_eq(oldvalue,{c}.s.cols[slot+{co}u])){{return false;}}V oldhash=vera_mix(oldvalue);u32 olddistance=(pos-(oldhash.w[0]&{b - 1}u))&{b - 1}u;if(olddistance<distance){{{c}.s.hslot[pos+{ho}u]=slot;slot=existing;distance=olddistance;}}pos=(pos+1u)&{b - 1}u;distance=distance+1u;}}return false;""",
                    )
            params = [("arg_" + _name(col.name), "V") for col in t.columns.values()]
            body = f"u32 slot={cap}u;for(u32 i=0u;i<{cap}u;i=i+1u){{if({c}.s.occ[i+{off}u]==0u){{slot=i;break;}}}}if(slot=={cap}u){{return v_result(v_zero(),false);}}if({c}.s.gen[slot+{off}u]==4294967295u){{return v_result(v_zero(),false);}}{c}.s.occ[slot+{off}u]=1u;{c}.s.gen[slot+{off}u]={c}.s.gen[slot+{off}u]+1u;"
            for col in t.columns.values():
                co = self.col_offsets[name, col.name]
                v = "arg_" + _name(col.name)
                body += f"{c}.s.cols[slot+{co}u]={v};"
                if col.tracked_sum:
                    ao = self.agg_offsets[name, col.name]
                    body += f"{c}.s.aggs[{ao}]=v_add({c}.s.aggs[{ao}],{v});"
            for col in t.columns.values():
                if col.unique:
                    body += f"if(!{self.cn(name, col.name)}_insert(ctx,arg_{_name(col.name)},slot)){{return v_result(v_zero(),false);}}"
            body += f"return v_result({tn}_key(ctx,slot,false),true);"
            self.fn(tn + "_alloc", "R", self.cparams(params), body)

    def temp(self, ty, value=None):
        self.counter += 1
        n = "tmp_" + str(self.counter)
        self.body.append(self.var(n, ty, value))
        return n

    def typ(self, expr, expected=None):
        known = getattr(self.prog, "expr_types", {}).get(id(expr))
        if known is not None:
            return known
        return _TypeChecker(self.prog, dict(self.env_types)).type_of(expr, expected)

    def aggregate_width(self, e):
        if isinstance(e, A.SumAgg):
            t = self.prog.tables[e.table]
            return accumulator_type(t.columns[e.column].type, t.layout.capacity).width
        if isinstance(e, A.Binary):
            if e.op in ("and", "or", "xor", "eq", "ne", "lt", "le", "gt", "ge"):
                return 0
            return max(self.aggregate_width(e.left), self.aggregate_width(e.right))
        if isinstance(e, A.Unary):
            return 0 if e.op == "not" else self.aggregate_width(e.operand)
        if isinstance(e, A.Select):
            return max(self.aggregate_width(e.a), self.aggregate_width(e.b))
        return 0

    def narrow(self, value, ty):
        if ty.is_bool:
            condition = f"v_eq({value},v_zero()) || v_eq({value},v_small(1u))"
        elif ty.kind == "enum":
            condition = f"v_ucmp({value},{self.const(len(ty.enum_tags))})==0u"
        else:
            condition = f"v_fits({value},{ty.width}u,{str(ty.is_signed).lower()})"
        self.body.append(f"if(!({condition})){{{self.ctx}.trap=1u;return false;}}")
        return value

    def typed_expr(self, e, env, ty, old=False):
        return self.narrow(self.expr(e, env, ty, old), ty)

    def expr(self, e, env, expected=None, old=False):
        if isinstance(e, A.Literal):
            ty = expected or self.typ(e)
            value = getattr(self.prog, "literal_values", {}).get(id(e))
            if value is None:
                value = literal_value(e.value, ty)
            return self.temp("V", self.const(value))
        if isinstance(e, A.Var):
            if e.name in env:
                return env[e.name]
            ty = expected or self.typ(e)
            if ty.kind == "enum" and e.name in ty.enum_tags:
                return self.temp("V", self.const(ty.enum_tags.index(e.name)))
            raise VeraError("D_GPU", f"unknown value {e.name!r}", e.line, e.col)
        if isinstance(e, A.KeyOf):
            return self.expr(e.expr, env, old=old)
        if isinstance(e, A.Select):
            cond = self.expr(e.cond, env, BOOL, old)
            out = self.temp("V", "v_zero()")
            self.body.append(f"if(v_truth({cond})){{")
            a = self.expr(e.a, env, expected or self.typ(e), old)
            self.body.append(f"{out}={a};")
            self.body.append("}else{")
            b = self.expr(e.b, env, expected or self.typ(e), old)
            self.body.append(f"{out}={b};")
            self.body.append("}")
            return out
        if isinstance(e, (A.TableGet, A.TableAccess, A.TableExists)):
            key = self.expr(e.key, env, id64(e.table), old)
            tn = self.tn(e.table)
            flag = "true" if old else "false"
            if isinstance(e, A.TableExists):
                return self.temp("V", f"v_bool({tn}_exists(ctx,{key},{flag}))")
            if isinstance(e, A.TableGet):
                self.body.append(
                    f"if(!{tn}_exists(ctx,{key},{flag})){{{self.ctx}.trap=1u;return false;}}"
                )
            return self.temp("V", f"{self.cn(e.table, e.column)}_get(ctx,{key},{flag})")
        if isinstance(e, (A.TableLookup, A.TableExistsUnique)):
            ty = self.prog.tables[e.table].columns[e.key_column].type
            key = self.expr(e.key, env, ty, old)
            suffix = "_pre" if old else ""
            r = self.temp(
                "R", f"{self.cn(e.table, e.key_column)}_lookup{suffix}(ctx,{key})"
            )
            if isinstance(e, A.TableExistsUnique):
                return self.temp("V", f"v_bool({r}.ok)")
            self.body.append(f"if(!{r}.ok){{{self.ctx}.trap=1u;return false;}}")
            return self.temp("V", r + ".value")
        if isinstance(e, A.Old):
            a = e.access
            key = self.expr(a.key, env, id64(a.table), True)
            return self.temp("V", f"{self.cn(a.table, a.column)}_get(ctx,{key},true)")
        if isinstance(e, A.SumAgg):
            state = "pre" if old or e.old else "s"
            return self.temp(
                "V", f"{self.ctx}.{state}.aggs[{self.agg_offsets[e.table, e.column]}]"
            )
        if isinstance(e, A.Unary):
            ty = self.typ(e, expected)
            a = self.expr(e.operand, env, ty, old)
            width = max(ty.width, self.aggregate_width(e))
            if e.op == "not":
                return self.temp("V", f"v_bool(!v_truth({a}))")
            if e.op == "neg":
                r = self.temp("V", f"v_neg({a})")
                self.body.append(
                    f"if(!v_fits({r},{width}u,{str(ty.is_signed).lower()})){{{self.ctx}.trap=1u;return false;}}"
                )
                self.body.append(
                    f"if(v_negative({a}) && v_eq({a},{r}) && !v_eq({a},v_zero())){{{self.ctx}.trap=1u;return false;}}"
                )
                return r
            raise VeraError("D_GPU", f"unsupported unary operation {e.op}")
        if isinstance(e, A.Binary):
            op = e.op
            if op in ("and", "or", "xor"):
                a = self.expr(e.left, env, BOOL, old)
                b = self.expr(e.right, env, BOOL, old)
                sym = {"and": "&&", "or": "||", "xor": "!="}[op]
                return self.temp("V", f"v_bool(v_truth({a}) {sym} v_truth({b}))")
            ty = (
                self.typ(e.left)
                if op in ("eq", "ne", "lt", "le", "gt", "ge")
                else self.typ(e, expected)
            )
            if isinstance(e.left, A.Literal) and not isinstance(e.right, A.Literal):
                ty = self.typ(e.right)
            a = self.expr(e.left, env, ty, old)
            b = self.expr(e.right, env, ty, old)
            signed = str(ty.is_signed).lower()
            if op in ("eq", "ne"):
                return self.temp(
                    "V", f"v_bool(" + ("!" if op == "ne" else "") + f"v_eq({a},{b}))"
                )
            if op in ("lt", "le", "gt", "ge", "min", "max"):
                comp = self.temp("u32", f"v_cmp({a},{b},{signed})")
                if op in ("min", "max"):
                    out = self.temp("V", a)
                    condition = f"{comp}==2u" if op == "min" else f"{comp}==0u"
                    self.body.append(f"if({condition}){{{out}={b};}}")
                    return out
                cond = {
                    "lt": f"{comp}==0u",
                    "le": f"{comp}!=2u",
                    "gt": f"{comp}==2u",
                    "ge": f"{comp}!=0u",
                }[op]
                return self.temp("V", f"v_bool({cond})")
            if op in ("add", "sub", "mul", "div", "rem"):
                scale = 10**ty.scale if ty.is_decimal else 1
                width = max(ty.width, self.aggregate_width(e))
                r = self.temp(
                    "R",
                    f"v_arith({a},{b},{('add', 'sub', 'mul', 'div', 'rem').index(op)}u,{width}u,{signed},{self.const(scale)})",
                )
                self.body.append(f"if(!{r}.ok){{{self.ctx}.trap=1u;return false;}}")
                return self.temp("V", r + ".value")
            raise VeraError("D_GPU", f"unsupported binary operation {op}")
        if isinstance(e, A.Fold):
            ty = _resolve_type_ref(self.prog, e.acc_type, "GPU fold")
            it = self.typ(e.lo)
            acc = self.typed_expr(e.init, env, ty, old)
            out = self.temp("V", acc)
            lo = self.typed_expr(e.lo, env, it, old)
            hi = self.typed_expr(e.hi, env, it, old)
            idx = self.temp("V", lo)
            counter = self.temp("u32", "0u")
            self.body.append(f"for(;{counter}<{e.n}u;{counter}={counter}+1u){{")
            self.body.append(
                f"if(v_cmp({idx},{hi},{str(it.is_signed).lower()})==2u){{break;}}"
            )
            inner = dict(env)
            inner[e.acc_name] = out
            inner[e.index_name] = idx
            oldtypes = dict(self.env_types)
            self.env_types[e.acc_name] = ty
            self.env_types[e.index_name] = it
            for s in e.body:
                self.stmt(s, inner, old)
            value = self.typed_expr(e.yield_expr, inner, ty, old)
            self.body.append(f"{out}={value};")
            self.body.append(f"if(v_eq({idx},{self.const(it.max_value)})){{break;}}")
            self.body.append(f"{idx}=v_add({idx},v_small(1u));")
            self.body.append("}")
            self.env_types = oldtypes
            return out
        raise VeraError(
            "D_GPU",
            f"unsupported expression {type(e).__name__}",
            getattr(e, "line", 0),
            getattr(e, "col", 0),
        )

    def stmt(self, s, env, old=False):
        c = self.ctx
        if isinstance(s, A.Let):
            ty = _resolve_type_ref(self.prog, s.type, "GPU let")
            v = self.typed_expr(s.value, env, ty, old)
            env[s.name] = v
            self.env_types[s.name] = ty
        elif isinstance(s, A.Guard):
            v = self.expr(s.cond, env, BOOL, old)
            self.body.append(f"if(!v_truth({v})){{return false;}}")
        elif isinstance(s, A.TableSet):
            key = self.expr(s.key, env, id64(s.table), old)
            tn = self.tn(s.table)
            ty = self.prog.tables[s.table].columns[s.column].type
            v = self.expr(s.value, env, ty, old)
            self.body.append(
                f"if(!{tn}_exists(ctx,{key},false)){{{self.ctx}.trap=1u;return false;}}"
            )
            self.narrow(v, ty)
            self.body.append(f"{self.cn(s.table, s.column)}_set(ctx,{key},{v});")
        elif isinstance(s, A.TableAlloc):
            t = self.prog.tables[s.table]
            fields = {
                name: self.typed_expr(expr, env, t.columns[name].type, old)
                for name, expr in s.fields
            }
            values = [fields[col.name] for col in t.columns.values()]
            r = self.temp(
                "R",
                f"{self.tn(s.table)}_alloc(ctx"
                + ("," + ",".join(values) if values else "")
                + ")",
            )
            self.body.append(f"if(!{r}.ok){{return false;}}")
            env[s.name] = self.temp("V", r + ".value")
            self.env_types[s.name] = id64(s.table)
        elif isinstance(s, A.Emit):
            eff = self.prog.effects[s.effect]
            values = [
                self.typed_expr(
                    arg, env, _resolve_type_ref(self.prog, p.type, "GPU effect"), old
                )
                for arg, p in zip(s.args, eff.params)
            ]
            self.body.append(f"{c}.s.effect_count={c}.s.effect_count+1u;")
            self.body.append(
                f"{c}.s.effect_digest=v_add({c}.s.effect_digest,{self.const(symbol_id(s.effect))});"
            )
            for v in values:
                self.body.append(
                    f"{c}.s.effect_digest=v_xor(v_shl({c}.s.effect_digest,1u),{v});"
                )
        elif isinstance(s, A.Yield):
            self.expr(s.value, env, old=old)
        elif isinstance(s, A.Commit):
            pass
        else:
            raise VeraError("D_GPU", f"unsupported statement {type(s).__name__}")

    def _check_function(self, name, invariants):
        self.body = []
        self.env_types = {}
        c = self.ctx
        for inv in invariants:
            if inv.quant_table is None:
                v = self.expr(inv.expr, {}, BOOL)
                self.body.append(f"if(!v_truth({v})){{return false;}}")
            else:
                t = self.prog.tables[inv.quant_table]
                off = self.table_offsets[inv.quant_table]
                tn = self.tn(inv.quant_table)
                slot = self.temp("u32", "0u")
                self.body.append(
                    f"for(;{slot}<{t.layout.capacity}u;{slot}={slot}+1u){{"
                )
                self.body.append(f"if({c}.s.occ[{slot}+{off}u]!=0u){{")
                key = self.temp("V", f"{tn}_key(ctx,{slot},false)")
                self.env_types = {inv.quant_var: id64(inv.quant_table)}
                v = self.expr(inv.expr, {inv.quant_var: key}, BOOL)
                self.body.append(f"if(!v_truth({v})){{return false;}}")
                self.body.append("}}")
        self.body.append("return true;")
        self.fn(name, "bool", self.cparams(), "\n".join(self.body))

    def _transition(self, name, ti):
        tn = "transition_" + _name(name)
        params = [("arg_" + _name(p.name), "V") for p in ti.decl.params]
        env = {p.name: "arg_" + _name(p.name) for p in ti.decl.params}
        self.env_types = dict(ti.param_types)
        self.body = []
        for _, e in ti.decl.requires:
            v = self.expr(e, env, BOOL, True)
            self.body.append(f"if(!v_truth({v})){{return false;}}")
        for s in ti.decl.body:
            self.stmt(s, env)
        self.body.append("return true;")
        self.fn(tn + "_apply", "bool", self.cparams(params), "\n".join(self.body))
        self.env_types = dict(ti.param_types)
        self.body = []
        for _, e in ti.decl.ensures:
            v = self.expr(e, env, BOOL)
            self.body.append(f"if(!v_truth({v})){{return false;}}")
        self.body.append("return vera_invariants(ctx);")
        self.fn(tn + "_post", "bool", self.cparams(params), "\n".join(self.body))
        args = ",".join(n for n, _ in params)
        args = ("," + args) if args else ""
        c = self.ctx
        checks = ""
        for p in ti.decl.params:
            pt = ti.param_types[p.name]
            v = "arg_" + _name(p.name)
            if pt.is_bool:
                checks += (
                    f"if(!v_eq({v},v_zero()) && !v_eq({v},v_small(1u))){{return 0u;}}"
                )
            elif pt.kind == "enum":
                checks += (
                    f"if(v_ucmp({v},{self.const(len(pt.enum_tags))})!=0u){{return 0u;}}"
                )
            else:
                checks += f"if(!v_fits({v},{pt.width}u,{str(pt.is_signed).lower()})){{return 0u;}}"
        self.fn(
            tn,
            "u32",
            self.cparams(params),
            f"""{c}.pre={c}.s;{c}.trap=0u;{checks}if(!{tn}_apply(ctx{args})){{{c}.s={c}.pre;if({c}.trap!=0u){{return 3u;}}return 0u;}}if(!{tn}_post(ctx{args})){{return 2u;}}return 1u;""",
        )

    def sample(self, ty, seed=False, slot="0u"):
        if ty.kind == "id64":
            t = self.prog.tables[ty.key_table]
            return (
                f"{self.tn(ty.key_table)}_key(ctx,({slot})%{t.layout.capacity}u,false)"
            )
        if ty.is_bool:
            return "v_small(xoshiro_next(ctx)%2u)" if not seed else "v_zero()"
        if ty.kind == "enum":
            return (
                f"v_small(xoshiro_next(ctx)%{max(1, len(ty.enum_tags))}u)"
                if not seed
                else "v_zero()"
            )
        if ty.kind == "bytes32":
            return "v_make(" + ",".join("xoshiro_next(ctx)" for _ in range(8)) + ")"
        if ty.kind == "sym32":
            return "v_small(xoshiro_next(ctx))"
        if ty.is_decimal:
            return f"w_low(w_mul(v_small((xoshiro_next(ctx)%8u)+1u),{self.const(10**ty.scale)}))"
        return (
            "v_small((xoshiro_next(ctx)%100u)+1u)"
            if seed
            else "v_small(xoshiro_next(ctx)%100u)"
        )

    def _init(self):
        c = self.ctx
        body = f"""for(u32 i=0u;i<{max(1, self.nrows)}u;i=i+1u){{{c}.s.occ[i]=0u;{c}.s.gen[i]=0u;}}for(u32 i=0u;i<{max(1, self.ncols)}u;i=i+1u){{{c}.s.cols[i]=v_zero();}}for(u32 i=0u;i<{max(1, self.naggs)}u;i=i+1u){{{c}.s.aggs[i]=v_zero();}}for(u32 i=0u;i<{max(1, self.nhash)}u;i=i+1u){{{c}.s.hused[i]=0u;{c}.s.hslot[i]=0u;}}{c}.s.effect_count=0u;{c}.s.effect_digest=v_zero();{c}.pre={c}.s;{c}.trap=0u;"""
        self.fn("vera_init", "void", self.cparams(), body)
        body = ""
        for name, t in self.prog.tables.items():
            off = self.table_offsets[name]
            for i in range(min(2, t.layout.capacity)):
                body += f"{c}.s.gen[{off + i}]=1u;"
        for name, t in self.prog.tables.items():
            tn = self.tn(name)
            off = self.table_offsets[name]
            for i in range(min(2, t.layout.capacity)):
                for col in t.columns.values():
                    value = self.sample(col.type, True, f"{i}u")
                    co = self.col_offsets[name, col.name]
                    body += f"{c}.s.cols[{co + i}]={value};"
                    if col.tracked_sum:
                        ao = self.agg_offsets[name, col.name]
                        body += f"{c}.s.aggs[{ao}]=v_add({c}.s.aggs[{ao}],{c}.s.cols[{co + i}]);"
                body += f"{c}.s.occ[{off + i}]=1u;"
                for col in t.columns.values():
                    if col.unique:
                        co = self.col_offsets[name, col.name]
                        body += f"if(!{self.cn(name, col.name)}_insert(ctx,{c}.s.cols[{co + i}],{i}u)){{return false;}}"
        body += f"{c}.pre={c}.s;return vera_invariants(ctx);"
        self.fn("vera_seed", "bool", self.cparams(), body)
        body = ""
        for i, name in enumerate(self.prog.transitions):
            ti = self.prog.transitions[name]
            body += f"if(which=={i}u){{"
            args = []
            for p in ti.decl.params:
                var = "param_" + _name(p.name)
                v = self.sample(ti.param_types[p.name], False, "xoshiro_next(ctx)")
                body += self.var(var, "V", v)
                args.append(var)
            body += (
                f"return transition_{_name(name)}(ctx"
                + (("," + ",".join(args)) if args else "")
                + ");}"
            )
        body += "return 0u;"
        self.fn("vera_dispatch", "u32", self.cparams([("which", "u32")]), body)

    def _main(self):
        if self.target == "wgsl":
            self.w(
                "@compute @workgroup_size(32)\nfn furnace(@builtin(global_invocation_id) gid:vec3<u32>) {"
            )
            self.w("var local_ctx:Ctx; let tid=gid.x;")
            ptr = "&local_ctx"
            ctx = "local_ctx"
            count = lambda k, v: f"atomicAdd(&output.{k},{v});"
        elif self.target == "glsl":
            self.w("void main(){\nCtx local_ctx;uint tid=gl_GlobalInvocationID.x;")
            ptr = "local_ctx"
            ctx = "local_ctx"
            count = lambda k, v: f"atomicAdd({k},{v});"
        else:
            self.w(
                'extern "C" __global__ void vera_furnace_kernel(uint32_t* output,uint32_t seed,uint32_t depth,uint32_t count){'
            )
            self.w(
                "uint32_t tid=blockIdx.x*blockDim.x+threadIdx.x;if(tid>=count){return;}Ctx local_ctx;"
            )
            ptr = "&local_ctx"
            ctx = "local_ctx"
            count = lambda k, v: (
                f"atomicAdd(output+{('violations', 'committed', 'reverted', 'effects').index(k)},{v});"
            )
        seed = "seed" if self.target == "cuda" else "12648430u"
        self.w(
            self.syntax(
                f"for(u32 j=0u;j<4u;j=j+1u){{{ctx}.rng[j]=(tid+1u)*747796405u+(j+1u)*2891336453u+{seed};}}"
            )
        )
        self.w(
            f"vera_init({ptr});if(!vera_seed({ptr})){{vera_init({ptr});if(!vera_invariants({ptr})){{return;}}}}"
        )
        limit = "depth" if self.target == "cuda" else str(self.depth) + "u"
        self.w(self.syntax(f"for(u32 step=0u;step<{limit};step=step+1u){{"))
        self.w(
            self.syntax(
                f"u32 before_effects={ctx}.s.effect_count;u32 which=xoshiro_next({ptr})%{max(1, len(self.prog.transitions))}u;u32 status=vera_dispatch({ptr},which);"
            )
        )
        self.w(
            "if(status==3u){"
            + count("violations", "1u")
            + count("reverted", "1u")
            + "break;}if(status==0u){"
            + count("reverted", "1u")
            + "}else{"
            + count("committed", "1u")
            + count("effects", f"{ctx}.s.effect_count-before_effects")
            + "if(status==2u){"
            + count("violations", "1u")
            + "break;}}"
        )
        self.w("}\n}")


def generate_cuda(prog: Program, depth: int = 64) -> str:
    return GpuGenerator(prog, "cuda", depth).emit()


def generate_wgsl(prog: Program, depth: int = 64) -> str:
    return GpuGenerator(prog, "wgsl", depth).emit()
