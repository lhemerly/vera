//! Compile and execute generated GLSL through a real EGL compute context.
#[path = "support/gpu_host.rs"]
mod gpu_host;
use vera_compiler::{codegen_gpu::generate_glsl, parse_source, semantic::analyze};

fn run(source: &str, probe: &str, expected: &str) {
    let program = analyze(parse_source(source).unwrap()).unwrap();
    let mut code = generate_glsl(&program, 2).unwrap();
    if !probe.is_empty() {
        code = code.replacen("void main(){", "void vera_furnace_main(){", 1);
        code.push_str(probe);
    }
    if let Some(result) = gpu_host::dispatch(&code) {
        let actual = format!(
            "violations={} committed={} reverted={} effects={}",
            result[0], result[1], result[2], result[3]
        );
        assert_eq!(actual, expected);
    }
}

#[test]
fn glsl_wide_scalars_unique_rollback_and_effects_execute() {
    run(
        r#"table T capacity 2 {key id:id64<T>;col v:dec128 [tracked_sum];col code:bytes32 [unique];}
invariant positive {forall k in T:ge(T[k].v,0);}
effect Created(code:bytes32);
transition create(value:dec128,code:bytes32,accept:bool) reads [] writes [T[*].v,T[*].code]
require positive:ge(value,0);
body {let %k:id64<T>=table_alloc(T,{v:value,code:code});emit Created(code);guard(accept,ERR_REJECT);commit;}
"#,
        r#"
void main(){if(gl_GlobalInvocationID.x!=0u){return;}Ctx ctx;vera_init(ctx);
V value=v_make(42u,0u,0u,16u,0u,0u,0u,0u);V code=v_make(7u,0u,0u,0u,0u,0u,0u,123u);
uint rollback=transition_v_create(ctx,value,code,v_zero());
uint status=transition_v_create(ctx,value,code,v_small(1u));
uint duplicate=transition_v_create(ctx,value,code,v_small(1u));
atomicAdd(violations,status+rollback+duplicate);atomicAdd(committed,ctx.s.cols[0].w[3]);
atomicAdd(reverted,ctx.s.cols[2].w[7]);atomicAdd(effects,ctx.s.effect_count);}
"#,
        "violations=1 committed=16 reverted=123 effects=1",
    );
}

#[test]
fn glsl_furnace_counts_commits_invariants_and_traps() {
    run(
        "transition good() reads [] writes [] body {commit;}",
        "",
        "violations=0 committed=64 reverted=0 effects=0",
    );
    run(
        r#"table T capacity 1 {key id:id64<T>;col v:u64;}
invariant zero {forall k in T:and(eq(T[k].v,0),eq(rem(T[k].v,2),0));}
transition bad() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:1});commit;}
"#,
        "",
        "violations=32 committed=32 reverted=0 effects=0",
    );
    run(
        "transition bad() reads [] writes [] body {let %x:u64=div(1,0);commit;}",
        "",
        "violations=32 committed=0 reverted=32 effects=0",
    );
}

#[test]
fn glsl_random_draws_match_cuda_and_wgsl() {
    run(
        "module Random;",
        r#"
void main(){if(gl_GlobalInvocationID.x!=0u){return;}Ctx ctx;vera_init(ctx);ctx.rng[0]=1u;ctx.rng[1]=2u;ctx.rng[2]=3u;ctx.rng[3]=4u;
V value=vera_sample_bytes32(ctx);atomicAdd(violations,v_bool(v_eq(value,v_make(11520u,0u,5927040u,70819200u,2031721883u,1637235492u,1287239034u,3734860849u))).w[0]);
atomicAdd(committed,ctx.rng[0]);atomicAdd(reverted,ctx.rng[1]);atomicAdd(effects,ctx.rng[3]);}
"#,
        "violations=1 committed=857776784 reverted=3957087773 effects=3837013768",
    );
}
