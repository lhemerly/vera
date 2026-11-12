//! Parse and validate WGSL, lower with Naga, and execute on a real EGL GPU.
#[path = "support/gpu_host.rs"]
mod gpu_host;
use naga::{
    back::glsl,
    proc::{BoundsCheckPolicies, BoundsCheckPolicy},
    valid::{Capabilities, ValidationFlags, Validator},
    AddressSpace, ResourceBinding, ShaderStage, StorageAccess, TypeInner,
};
use vera_compiler::{codegen_gpu::generate_wgsl, parse_source, semantic::analyze};

fn run(source: &str, probe: &str, entry: &str) -> Option<[u32; 4]> {
    let program = analyze(parse_source(source).unwrap()).unwrap();
    let code = generate_wgsl(&program, 2).unwrap() + probe;
    let module = naga::front::wgsl::parse_str(&code)
        .unwrap_or_else(|error| panic!("invalid generated WGSL: {}", error.emit_to_string(&code)));
    let info = Validator::new(ValidationFlags::all(), Capabilities::empty())
        .validate(&module)
        .unwrap_or_else(|error| panic!("WGSL validation failed: {error:?}"));
    // The actual host allocates exactly one 16-byte storage buffer, as did the
    // WebGPU harness. Check that ABI before dispatch instead of relying on a
    // graphics API to infer a compatible bind group.
    let outputs: Vec<_> = module
        .global_variables
        .iter()
        .filter(|(_, global)| global.binding.is_some())
        .collect();
    assert_eq!(
        outputs.len(),
        1,
        "shader must have exactly one bound output buffer"
    );
    let (_, output) = outputs[0];
    assert_eq!(
        output.binding,
        Some(ResourceBinding {
            group: 0,
            binding: 0
        })
    );
    assert_eq!(
        output.space,
        AddressSpace::Storage {
            access: StorageAccess::LOAD | StorageAccess::STORE
        }
    );
    let TypeInner::Struct { span, .. } = module.types[output.ty].inner else {
        panic!("output must be the counter structure")
    };
    assert_eq!(span, 16, "output ABI must contain four u32 counters");
    let options = glsl::Options {
        version: glsl::Version::Desktop(460),
        writer_flags: glsl::WriterFlags::empty(),
        binding_map: [(
            ResourceBinding {
                group: 0,
                binding: 0,
            },
            0,
        )]
        .into_iter()
        .collect(),
        zero_initialize_workgroup_memory: true,
    };
    let pipeline = glsl::PipelineOptions {
        shader_stage: ShaderStage::Compute,
        entry_point: entry.into(),
        multiview: None,
    };
    let policies = BoundsCheckPolicies {
        index: BoundsCheckPolicy::ReadZeroSkipWrite,
        buffer: BoundsCheckPolicy::ReadZeroSkipWrite,
        image_load: BoundsCheckPolicy::ReadZeroSkipWrite,
        binding_array: BoundsCheckPolicy::ReadZeroSkipWrite,
    };
    let mut glsl = String::new();
    glsl::Writer::new(&mut glsl, &module, &info, &options, &pipeline, policies)
        .expect("validated WGSL must lower to GLSL")
        .write()
        .expect("WGSL compute shader must emit complete GLSL");
    gpu_host::dispatch(&glsl)
}
fn expect(source: &str, probe: &str, entry: &str, expected: [u32; 4]) {
    if let Some(result) = run(source, probe, entry) {
        assert_eq!(result, expected);
    }
}

#[test]
fn wide_unique_allocation_rollback_and_effects_execute() {
    expect(
        r#"table T capacity 2 {key id:id64<T>;col v:dec128 [tracked_sum];col code:bytes32 [unique];}
invariant positive {forall k in T:ge(T[k].v,0);}
effect Created(code:bytes32);
transition create(value:dec128,code:bytes32,accept:bool) reads [] writes [T[*].v,T[*].code]
require positive:ge(value,0);
body {let %k:id64<T>=table_alloc(T,{v:value,code:code});emit Created(code);guard(accept,ERR_REJECT);commit;}
"#,
        r#"
@compute @workgroup_size(1)
fn probe(){var ctx:Ctx;vera_init(&ctx);
var value:V=v_make(42u,0u,0u,16u,0u,0u,0u,0u);
var code:V=v_make(7u,0u,0u,0u,0u,0u,0u,123u);
var rollback:u32=transition_v_create(&ctx,value,code,v_zero());
var status:u32=transition_v_create(&ctx,value,code,v_small(1u));
var duplicate:u32=transition_v_create(&ctx,value,code,v_small(1u));
atomicStore(&output.violations,status+rollback+duplicate);
atomicStore(&output.committed,ctx.s.cols[0].w[3]);
atomicStore(&output.reverted,ctx.s.cols[2].w[7]);
atomicStore(&output.effects,ctx.s.effect_count);}
"#,
        "probe",
        [1, 16, 123, 1],
    );
}

#[test]
fn old_key_folds_and_ensures_use_the_pre_state() {
    expect(
        r#"table T capacity 2 {key id:id64<T>;col owner:id64<T>;col v:i64;col result:i64;}
transition go(k:id64<T>) reads [T[*].owner,T[*].v] writes [T[k].owner,T[k].result]
require live:table_exists(T,k);
ensure snapshot:eq(T[k].result,old(T[table_get(T,k,owner)].v));
body {table_set(T,k,owner,key_of<T>(3));let %v:i64=old(T[fold_bounded<1>(a:id64<T>=k,i in [0..0]) {let %owner:id64<T>=table_get(T,k,owner);guard(true,ERR_REJECT);yield %owner;}].v);table_set(T,k,result,%v);commit;}
"#,
        r#"@compute @workgroup_size(1)
fn probe(){var ctx:Ctx;vera_init(&ctx);ctx.s.occ[0]=1u;ctx.s.gen[0]=1u;ctx.s.cols[0]=v_small(2u);ctx.s.cols[2]=v_small(7u);
var status:u32=transition_v_go(&ctx,v_small(2u));
atomicStore(&output.violations,status);atomicStore(&output.committed,ctx.s.cols[4].w[0]);
atomicStore(&output.reverted,ctx.s.cols[0].w[0]);atomicStore(&output.effects,ctx.trap);}
"#,
        "probe",
        [1, 7, 3, 0],
    );
}

#[test]
fn widened_sums_and_scalar_traps_execute() {
    expect(
        r#"table T capacity 2 {key id:id64<T>;col v:u64 [tracked_sum];}
effect Seen(value:u64);
transition fill() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:18446744073709551615});commit;}
transition wide() reads [T[*].v] writes [] body {guard(gt(add(sum(T.v),1),sum(T.v)),ERR_BAD);commit;}
transition narrow() reads [T[*].v] writes [] body {emit Seen(sum(T.v));commit;}
"#,
        r#"@compute @workgroup_size(1)
fn probe(){var ctx:Ctx;vera_init(&ctx);var a:u32=transition_v_fill(&ctx);var b:u32=transition_v_fill(&ctx);
var wide:u32=transition_v_wide(&ctx);var narrow:u32=transition_v_narrow(&ctx);
atomicStore(&output.violations,a+b);atomicStore(&output.committed,wide);
atomicStore(&output.reverted,narrow);atomicStore(&output.effects,ctx.s.effect_count);}
"#,
        "probe",
        [2, 1, 3, 0],
    );
}

#[test]
fn exact_512_bit_math_executes_without_native_int64() {
    expect(
        "module Arithmetic;",
        r#"@compute @workgroup_size(1)
fn probe(){
var product:R=v_arith(v_make(0u,0u,0u,0u,0u,0u,256u,0u),v_make(0u,256u,0u,0u,0u,0u,0u,0u),2u,256u,false,v_small(1u));
var quotient:R=v_arith(v_make(0u,0u,0u,0u,0u,0u,0u,65536u),v_make(0u,0u,0u,16u,0u,0u,0u,0u),3u,256u,false,v_small(1u));
var signed_rem:R=v_arith(v_make(4294967289u,4294967295u,4294967295u,4294967295u,4294967295u,4294967295u,4294967295u,4294967295u),v_small(3u),4u,64u,true,v_small(1u));
var inexact:R=v_arith(v_make(2808348672u,232830643u,0u,0u,0u,0u,0u,0u),v_make(4130078720u,698491930u,0u,0u,0u,0u,0u,0u),3u,128u,true,v_make(2808348672u,232830643u,0u,0u,0u,0u,0u,0u));
atomicStore(&output.violations,v_bool(product.ok && product.value.w[7]==65536u).w[0]);
atomicStore(&output.committed,v_bool(quotient.ok && quotient.value.w[4]==4096u).w[0]);
atomicStore(&output.reverted,v_bool(signed_rem.ok && signed_rem.value.w[0]==4294967295u && signed_rem.value.w[7]==4294967295u).w[0]);
atomicStore(&output.effects,v_bool(!inexact.ok).w[0]);}
"#,
        "probe",
        [1, 1, 1, 1],
    );
}

#[test]
fn furnace_counts_invariant_failures_and_unhandled_traps() {
    expect(
        r#"table T capacity 1 {key id:id64<T>;col v:u64;}
invariant zero {forall k in T:and(eq(T[k].v,0),eq(rem(T[k].v,2),0));}
transition bad() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:1});commit;}
"#,
        "",
        "furnace",
        [32, 32, 0, 0],
    );
    expect(
        "transition bad() reads [] writes [] body {let %x:u64=div(1,0);commit;}",
        "",
        "furnace",
        [32, 0, 32, 0],
    );
}

#[test]
fn bytes32_random_draws_match_the_other_gpu_targets() {
    expect(
        "module Random;",
        r#"@compute @workgroup_size(1)
fn probe(){var ctx:Ctx;vera_init(&ctx);ctx.rng[0]=1u;ctx.rng[1]=2u;ctx.rng[2]=3u;ctx.rng[3]=4u;
var value:V=vera_sample_bytes32(&ctx);
atomicStore(&output.violations,v_bool(v_eq(value,v_make(11520u,0u,5927040u,70819200u,2031721883u,1637235492u,1287239034u,3734860849u))).w[0]);
atomicStore(&output.committed,ctx.rng[0]);atomicStore(&output.reverted,ctx.rng[1]);atomicStore(&output.effects,ctx.rng[3]);}
"#,
        "probe",
        [1, 857776784, 3957087773, 3837013768],
    );
}

#[test]
fn wgsl_effect_contracts_validate_transaction_local_order_and_payload() {
    expect(
        r#"effect A(v:bytes32);effect B();
transition go(v:bytes32) reads [] writes []
ensure count:eq(effect_count(),2);
ensure tag:effect_is(A,0);
ensure payload:eq(effect_arg(A,0,v),v);
ensure end:effect_is(B,1);
ensure missing:not(effect_is(A,18446744073709551615));
body {emit A(v);emit B();commit;}"#,
        r#"@compute @workgroup_size(1) fn probe(){var ctx:Ctx;vera_init(&ctx);
let payload=v_make(1u,2u,3u,4u,5u,6u,7u,8u);
let first=transition_v_go(&ctx,payload);let second=transition_v_go(&ctx,payload);
atomicAdd(&output.violations,first);atomicAdd(&output.committed,second);atomicAdd(&output.effects,ctx.s.effect_count);}"#,
        "probe",
        [1, 1, 0, 4],
    );
}
