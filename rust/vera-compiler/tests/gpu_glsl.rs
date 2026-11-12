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

#[test]
fn glsl_effect_contracts_validate_transaction_local_order_and_payload() {
    run(
        r#"effect A(v:bytes32);effect B();
transition go(v:bytes32) reads [] writes []
ensure count:eq(effect_count(),2);
ensure tag:effect_is(A,0);
ensure payload:eq(effect_arg(A,0,v),v);
ensure end:effect_is(B,1);
ensure missing:not(effect_is(A,18446744073709551615));
body {emit A(v);emit B();commit;}"#,
        r#"void main(){if(gl_GlobalInvocationID.x!=0u){return;}Ctx ctx;vera_init(ctx);
V payload=v_make(1u,2u,3u,4u,5u,6u,7u,8u);
uint first=transition_v_go(ctx,payload);uint second=transition_v_go(ctx,payload);
atomicAdd(violations,first);atomicAdd(committed,second);atomicAdd(effects,ctx.s.effect_count);}"#,
        "violations=1 committed=1 reverted=0 effects=4",
    );
}

#[test]
fn glsl_compositional_function_helpers_execute_success_and_failure() {
    run(
        include_str!("fixtures/functions.vera"),
        r#"void main(){if(gl_GlobalInvocationID.x!=0u){return;}Ctx ctx;vera_init(ctx);
V payload=v_make(1u,2u,3u,4u,5u,6u,7u,8u);
uint first=transition_v_run(ctx,v_small(3u),payload);uint second=transition_v_lazy(ctx);uint bad=transition_v_bad_pre(ctx);
atomicAdd(violations,first);atomicAdd(committed,second);atomicAdd(reverted,bad);atomicAdd(effects,ctx.s.effect_count);}"#,
        "violations=1 committed=1 reverted=3 effects=2",
    );
}

#[test]
fn glsl_records_unions_and_typed_function_results_execute() {
    run(
        include_str!("fixtures/gpu_structured.vera"),
        r#"
void main(){if(gl_GlobalInvocationID.x!=0u){return;}Ctx ctx;vera_init(ctx);
VeraT0 p=vera_type_0_zero();p.f_0=v_small(7u);p.f_1=v_make(1u,2u,3u,4u,5u,6u,7u,8u);
VeraT1 w=vera_type_1_zero();w.f_0=v_make(9u,10u,11u,12u,13u,14u,15u,16u);w.f_1=v_make(17u,18u,19u,20u,21u,22u,23u,24u);
uint saved=transition_v_save(ctx,p,w,v_small(1u));uint found=transition_v_lookup(ctx,p);
uint bad=transition_v_wrongtag(ctx);uint ordered=transition_v_ordered(ctx);
atomicAdd(violations,saved);atomicAdd(committed,found);atomicAdd(reverted,bad);atomicAdd(effects,ordered+ctx.s.col_0_2[0].f_1.f_1.w[7]);}
"#,
        "violations=1 committed=1 reverted=3 effects=25",
    );
}

#[test]
fn glsl_bounded_bytes_and_shared_canonical_codecs_execute() {
    run(
        include_str!("fixtures/gpu_bounded_bytes.vera"),
        r#"
void main(){if(gl_GlobalInvocationID.x!=0u){return;}Ctx ctx;vera_init(ctx);
VeraB4 x=vera_bytes_4_zero();x.len=3u;x.data[0]=1u;x.data[1]=2u;x.data[2]=3u;
uint saved=transition_v_save(ctx,x);uint found=transition_v_lookup(ctx,x);
VeraB17 bad=vera_bytes_17_zero();bad.len=17u;bad.data[0]=99u;
uint failed=transition_v_invalid(ctx,bad);uint lazy=transition_v_lazy(ctx,bad);uint empty=transition_v_empty(ctx);
atomicAdd(violations,saved);atomicAdd(committed,found);atomicAdd(reverted,failed);atomicAdd(effects,lazy+empty+ctx.s.col_0_1[0].f_1.data[0]);}
"#,
        "violations=1 committed=1 reverted=3 effects=257",
    );
}
