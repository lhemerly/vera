//! Execute generated GPU semantics through CUDA's ordinary C++ host ABI.
//! These tests need no CUDA hardware and cover exact scalar and state behavior.
use num_bigint::BigInt;
use num_traits::ToPrimitive;
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
use vera_compiler::{
    codegen_gpu::{generate_cuda, generate_glsl, generate_wgsl},
    ir::CheckedProgram,
    parse_source,
    semantic::analyze,
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let dir = Self(std::env::temp_dir().join(format!(
            "vera-gpu-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )));
        fs::create_dir_all(&dir.0).unwrap();
        dir
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn checked(source: &str) -> CheckedProgram {
    analyze(parse_source(source).unwrap()).unwrap()
}
fn run_cpp(source: &str, main: &str) {
    let folder = Scratch::new();
    let code = generate_cuda(&checked(source), 2).unwrap();
    let path = folder.0.join("check.cpp");
    let executable = folder.0.join("check");
    fs::write(
        &path,
        format!("#define VERA_GPU_HOST\n#include <cassert>\n{code}\nint main(){{{main}}}\n"),
    )
    .unwrap();
    let built = match Command::new("g++")
        .args(["-std=c++17", "-O1"])
        .arg(&path)
        .arg("-o")
        .arg(&executable)
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            assert!(
                !std::env::var("VERA_REQUIRE_GPU_TESTS").is_ok_and(|v| v == "1"),
                "VERA_REQUIRE_GPU_TESTS=1 requires g++"
            );
            eprintln!("C++ GPU conformance skipped: g++ unavailable");
            return;
        }
        Err(error) => panic!("C++ compiler failed to start: {error}"),
    };
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let run = Command::new(executable).output().unwrap();
    assert!(
        run.status.success(),
        "GPU kernel failed: {}\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}
fn constant(value: &BigInt) -> String {
    let mask = (BigInt::from(1u8) << 256usize) - 1u8;
    let value = value & mask;
    let words = (0..8)
        .map(|i| {
            format!(
                "{}u",
                ((&value >> (i * 32usize)) & BigInt::from(u32::MAX))
                    .to_u32()
                    .unwrap()
            )
        })
        .collect::<Vec<_>>();
    format!("v_make({})", words.join(","))
}

const WIDE: &str = r#"module Wide;
table T capacity 2 { key id:id64<T>; col value:dec128 [tracked_sum]; col code:bytes32 [unique]; }
invariant positive {forall k in T:ge(T[k].value,0);}
effect Created(code:bytes32);
transition create(value:dec128,code:bytes32,accept:bool) reads [] writes [T[*].value,T[*].code]
require positive:ge(value,0);
body {let %k:id64<T>=table_alloc(T,{value:value,code:code});emit Created(code);guard(accept,ERR_REJECT);commit;}
transition find(code:bytes32) reads [T[*].code] writes []
body {let %k:id64<T>=table_lookup(T,code,code);guard(table_exists_unique(T,code,code),ERR_MISSING);commit;}
"#;

#[test]
fn wide_unique_rows_and_effects_are_transactional() {
    let a = (BigInt::from(123u8) << 224usize) + 7;
    let b = (BigInt::from(124u8) << 224usize) + 7;
    let value = (BigInt::from(1u8) << 100usize) + 42;
    let a = constant(&a);
    let b = constant(&b);
    let v = constant(&value);
    run_cpp(
        WIDE,
        &format!(
            r#"
Ctx ctx;vera_init(&ctx);
assert(transition_v_create(&ctx,{v},{a},v_zero())==0u);
assert(ctx.s.occ[0]==0u && ctx.s.gen[0]==0u && ctx.s.effect_count==0u);
for(unsigned i=0;i<16;i++)assert(ctx.s.hused[i]==0u);
assert(transition_v_create(&ctx,{v},{a},v_small(1u))==1u);
assert(transition_v_create(&ctx,{v},{a},v_small(1u))==0u);
assert(transition_v_create(&ctx,{v},{b},v_small(1u))==1u);
assert(v_eq(ctx.s.cols[0],{v}) && v_eq(ctx.s.cols[3],{b}));
assert(v_eq(ctx.s.aggs[0],v_add({v},{v})) && ctx.s.effect_count==2u);
assert(transition_v_find(&ctx,{a})==1u && transition_v_find(&ctx,{b})==1u);
assert(transition_v_create(&ctx,{v},v_zero(),v_small(1u))==0u);
assert(ctx.s.effect_count==2u && ctx.s.gen[0]==1u && ctx.s.gen[1]==1u);
"#
        ),
    );
}

#[test]
fn exact_limb_arithmetic_uses_512_bit_intermediates() {
    type ArithmeticCase = (BigInt, BigInt, u32, u32, bool, BigInt, Option<BigInt>);
    let one = BigInt::from(1u8);
    let scale = BigInt::from(10u8).pow(18);
    let cases: Vec<ArithmeticCase> = vec![
        (255.into(), 1.into(), 0, 8, false, one.clone(), None),
        (
            (&one << 64usize) - 1u8,
            (&one << 64usize) - 1u8,
            2,
            64,
            false,
            one.clone(),
            None,
        ),
        (
            (-7).into(),
            3.into(),
            3,
            64,
            true,
            one.clone(),
            Some((-2).into()),
        ),
        (
            (-7).into(),
            3.into(),
            4,
            64,
            true,
            one.clone(),
            Some((-1).into()),
        ),
        (
            -(&one << 63usize),
            (-1).into(),
            4,
            64,
            true,
            one.clone(),
            None,
        ),
        (
            &scale * 3,
            &scale * 2,
            2,
            128,
            true,
            scale.clone(),
            Some(&scale * 6),
        ),
        (
            &scale * 3,
            &scale * 2,
            3,
            128,
            true,
            scale.clone(),
            Some(&scale * 3 / 2),
        ),
        (scale.clone(), &scale * 3, 3, 128, true, scale.clone(), None),
        (
            &one << 200usize,
            &one << 40usize,
            2,
            256,
            false,
            one.clone(),
            Some(&one << 240usize),
        ),
        (
            &one << 240usize,
            &one << 20usize,
            2,
            256,
            false,
            one.clone(),
            None,
        ),
        (
            &one << 240usize,
            &one << 100usize,
            3,
            256,
            false,
            one.clone(),
            Some(&one << 140usize),
        ),
        (
            (&one << 256usize) - 1u8,
            one.clone(),
            0,
            256,
            false,
            one.clone(),
            None,
        ),
        (
            (&one << 255usize) - 1u8,
            one.clone(),
            0,
            256,
            true,
            one.clone(),
            None,
        ),
        (
            -(&one << 255usize),
            (-1).into(),
            3,
            256,
            true,
            one.clone(),
            None,
        ),
    ];
    let mut main = String::new();
    for (a, b, op, width, signed, scale, expected) in cases {
        main.push_str(&format!(
            "{{R r=v_arith({},{},{op}u,{width}u,{signed},{});",
            constant(&a),
            constant(&b),
            constant(&scale)
        ));
        if let Some(expected) = expected {
            main.push_str(&format!(
                "assert(r.ok && v_eq(r.value,{}));",
                constant(&expected)
            ));
        } else {
            main.push_str("assert(!r.ok);");
        }
        main.push('}');
    }
    run_cpp("module Arithmetic;", &main);
}

#[test]
fn eager_booleans_lazy_select_and_traps_preserve_state() {
    run_cpp(
        r#"table T capacity 2 {key id:id64<T>;col v:u8;}
transition eager(k:id64<T>,zero:u8) reads [] writes [T[k].v]
require live:table_exists(T,k);
body {table_set(T,k,v,9);guard(and(false,eq(div(1,zero),0)),ERR_FALSE);commit;}
transition lazy(k:id64<T>,zero:u8) reads [] writes [T[k].v]
require live:table_exists(T,k);
body {table_set(T,k,v,select(eq(zero,0),7,div(1,zero)));commit;}
"#,
        r#"Ctx ctx;vera_init(&ctx);ctx.s.occ[0]=1u;ctx.s.gen[0]=1u;
assert(transition_v_eager(&ctx,v_small(2u),v_zero())==3u);
assert(ctx.trap==1u && v_eq(ctx.s.cols[0],v_zero()));
assert(transition_v_lazy(&ctx,v_small(2u),v_zero())==1u);
assert(v_eq(ctx.s.cols[0],v_small(7u)));
assert(transition_v_lazy(&ctx,v_small(4u),v_zero())==0u);
assert(transition_v_lazy(&ctx,v_small(2u),v_small(256u))==0u);
"#,
    );
}

#[test]
fn folds_preserve_initializer_order_and_do_not_wrap_indexes() {
    run_cpp(
        r#"table T capacity 2 {key id:id64<T>;col v:i64;col result:i64;}
transition order(k:id64<T>) reads [T[k].v] writes [T[k].v,T[k].result]
require live:table_exists(T,k);
body {let %x:i64=fold_bounded<1>(a:i64=fold_bounded<1>(b:i64=0,j in [0..0]) {table_set(T,k,v,7);yield 0;},i in [table_get(T,k,v)..0]) {yield 1;};table_set(T,k,result,%x);commit;}
transition limit(k:id64<T>,lo:u8,hi:u8) reads [] writes [T[k].result]
require live:table_exists(T,k);
body {let %v:i64=fold_bounded<4>(acc:i64=0,i in [lo..hi]) {yield add(acc,1);};table_set(T,k,result,%v);commit;}
"#,
        r#"Ctx ctx;vera_init(&ctx);ctx.s.occ[0]=1u;ctx.s.gen[0]=1u;
assert(transition_v_order(&ctx,v_small(2u))==1u && v_eq(ctx.s.cols[2],v_zero()));
assert(transition_v_limit(&ctx,v_small(2u),v_small(254u),v_small(255u))==1u);
assert(v_eq(ctx.s.cols[2],v_small(2u)));
assert(transition_v_limit(&ctx,v_small(2u),v_small(3u),v_small(1u))==1u);
assert(v_eq(ctx.s.cols[2],v_zero()));
"#,
    );
}

#[test]
fn old_key_folds_read_pre_state_through_local_bindings() {
    run_cpp(
        r#"table T capacity 2 {key id:id64<T>;col owner:id64<T>;col v:i64;col result:i64;}
transition go(k:id64<T>) reads [T[*].owner,T[*].v] writes [T[k].owner,T[k].result]
require live:table_exists(T,k);
ensure snapshot:eq(T[k].result,old(T[table_get(T,k,owner)].v));
body {table_set(T,k,owner,key_of<T>(3));let %v:i64=old(T[fold_bounded<1>(a:id64<T>=k,i in [0..0]) {let %owner:id64<T>=table_get(T,k,owner);guard(true,ERR_REJECT);yield %owner;}].v);table_set(T,k,result,%v);commit;}
"#,
        r#"Ctx ctx;vera_init(&ctx);ctx.s.occ[0]=1u;ctx.s.gen[0]=1u;ctx.s.cols[0]=v_small(2u);ctx.s.cols[2]=v_small(7u);
assert(transition_v_go(&ctx,v_small(2u))==1u);
assert(v_eq(ctx.s.cols[4],v_small(7u)) && v_eq(ctx.s.cols[0],v_small(3u)));
"#,
    );
}

#[test]
fn allocation_initializers_evaluate_in_source_order() {
    run_cpp(
        r#"table T capacity 2 {key id:id64<T>;col v:i64;}
table R capacity 2 {key id:id64<R>;col first:i64;col second:i64;}
invariant seven {forall r in R:eq(R[r].first,7);}
transition go(k:id64<T>) reads [T[k].v] writes [T[k].v,R[*].first,R[*].second]
require live:table_exists(T,k);
body {let %r:id64<R>=table_alloc(R,{second:fold_bounded<1>(acc:i64=0,i in [0..0]) {table_set(T,k,v,7);yield 0;},first:table_get(T,k,v)});commit;}
"#,
        r#"Ctx ctx;vera_init(&ctx);ctx.s.occ[0]=1u;ctx.s.gen[0]=1u;
assert(transition_v_go(&ctx,v_small(2u))==1u && v_eq(ctx.s.cols[2],v_small(7u)));
"#,
    );
}

#[test]
fn aggregate_arithmetic_is_wide_and_scalar_boundaries_are_checked() {
    run_cpp(
        r#"table T capacity 2 {key id:id64<T>;col v:u64 [tracked_sum];}
effect Seen(value:u64);
transition fill() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:18446744073709551615});commit;}
transition wide() reads [T[*].v] writes [] body {guard(gt(add(sum(T.v),1),sum(T.v)),ERR_BAD);commit;}
transition binding() reads [T[*].v] writes [] body {let %v:u64=sum(T.v);commit;}
transition effect_arg() reads [T[*].v] writes [] body {emit Seen(sum(T.v));commit;}
transition fold_acc() reads [T[*].v] writes [] body {let %v:u64=fold_bounded<1>(acc:u64=sum(T.v),i in [0..0]) {yield acc;};commit;}
transition fold_bound() reads [T[*].v] writes [] body {let %v:u64=fold_bounded<1>(acc:u64=0,i in [sum(T.v)..0]) {yield acc;};commit;}
transition write(k:id64<T>) reads [T[*].v] writes [T[k].v] require live:table_exists(T,k);body {table_set(T,k,v,sum(T.v));commit;}
"#,
        r#"Ctx ctx;vera_init(&ctx);assert(transition_v_fill(&ctx)==1u && transition_v_fill(&ctx)==1u);
assert(transition_v_wide(&ctx)==1u);
assert(transition_v_binding(&ctx)==3u && transition_v_effect_arg(&ctx)==3u);
assert(transition_v_fold_acc(&ctx)==3u && transition_v_fold_bound(&ctx)==3u);
assert(transition_v_write(&ctx,v_small(2u))==3u && ctx.s.effect_count==0u);
"#,
    );
}

#[test]
fn generation_exhaustion_is_handled_and_invalid_handles_trap() {
    run_cpp(
        r#"table T capacity 1 {key id:id64<T>;col v:u64;}
transition fill() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:7});commit;}
transition read(k:id64<T>) reads [T[*].v] writes [] body {let %v:u64=table_get(T,k,v);commit;}
"#,
        r#"Ctx ctx;vera_init(&ctx);ctx.s.gen[0]=4294967295u;
assert(transition_v_fill(&ctx)==0u && ctx.s.occ[0]==0u && ctx.s.gen[0]==4294967295u);
ctx.s.gen[0]=0u;assert(transition_v_fill(&ctx)==1u);
assert(transition_v_read(&ctx,v_small(1u))==1u);
assert(transition_v_read(&ctx,v_small(2u))==3u);
assert(transition_v_read(&ctx,v_make(1u,1u,0u,0u,0u,0u,0u,0u))==3u);
"#,
    );
}

#[test]
fn invariant_violations_and_arithmetic_traps_reach_furnace_counters() {
    run_cpp(r#"table T capacity 1 {key id:id64<T>;col v:u64;}
invariant zero {forall k in T:and(eq(T[k].v,0),eq(rem(T[k].v,2),0));}
transition bad() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:1});commit;}
"#, "uint32_t output[4]={0};vera_furnace_kernel(output,42u,2u,1u);assert(output[0]==1u && output[1]==1u && output[2]==0u);");
    run_cpp("transition bad() reads [] writes [] body {let %v:u64=div(1,0);commit;}", "uint32_t output[4]={0};vera_furnace_kernel(output,42u,2u,1u);assert(output[0]==1u && output[1]==0u && output[2]==1u);");
}

#[test]
fn source_names_cannot_collide_with_gpu_helpers() {
    run_cpp("transition foo() reads [] writes [] body {commit;}transition foo_apply() reads [] writes [] body {commit;}transition foo_post() reads [] writes [] body {commit;}", "Ctx ctx;vera_init(&ctx);assert(transition_v_foo(&ctx)==1u && transition_v_foo_apply(&ctx)==1u && transition_v_foo_post(&ctx)==1u);");
}

#[test]
fn bytes32_random_draws_have_a_fixed_limb_order() {
    run_cpp("module Random;", "Ctx ctx;vera_init(&ctx);ctx.rng[0]=1u;ctx.rng[1]=2u;ctx.rng[2]=3u;ctx.rng[3]=4u;V value=vera_sample_bytes32(&ctx);assert(v_eq(value,v_make(11520u,0u,5927040u,70819200u,2031721883u,1637235492u,1287239034u,3734860849u)));assert(ctx.rng[0]==857776784u && ctx.rng[1]==3957087773u && ctx.rng[2]==2428778008u && ctx.rng[3]==3837013768u);");
}

#[test]
fn all_scalar_parameters_enforce_their_exact_domains() {
    run_cpp(
        r#"effect Seen(flag:bool,a:u8,b:u16,c:u32,d:u64,e:i64,f:dec64,g:dec128,h:sym32,i:bytes32,j:enum{A,B});
transition kinds(flag:bool,a:u8,b:u16,c:u32,d:u64,e:i64,f:dec64,g:dec128,h:sym32,i:bytes32,j:enum{A,B}) reads [] writes [] body {emit Seen(flag,a,b,c,d,e,f,g,h,i,j);commit;}
"#,
        r#"Ctx ctx;vera_init(&ctx);V max64=v_make(4294967295u,4294967295u,0u,0u,0u,0u,0u,0u);V min64=v_make(0u,2147483648u,4294967295u,4294967295u,4294967295u,4294967295u,4294967295u,4294967295u);V min128=v_make(0u,0u,0u,2147483648u,4294967295u,4294967295u,4294967295u,4294967295u);V maxbytes=v_not(v_zero());
assert(transition_v_kinds(&ctx,v_small(1u),v_small(255u),v_small(65535u),v_small(4294967295u),max64,min64,min64,min128,v_small(4294967295u),maxbytes,v_small(1u))==1u);
assert(ctx.s.effect_count==1u);
assert(transition_v_kinds(&ctx,v_small(2u),v_zero(),v_zero(),v_zero(),v_zero(),v_zero(),v_zero(),v_zero(),v_zero(),v_zero(),v_zero())==0u);
assert(transition_v_kinds(&ctx,v_zero(),v_zero(),v_zero(),v_zero(),v_zero(),v_zero(),v_zero(),v_zero(),v_zero(),v_zero(),v_small(2u))==0u);
assert(ctx.s.effect_count==1u);
"#,
    );
}

#[test]
fn all_gpu_targets_lower_the_same_checked_program() {
    let program = checked(WIDE);
    let glsl = generate_glsl(&program, 2).unwrap();
    let wgsl = generate_wgsl(&program, 2).unwrap();
    assert!(glsl.contains("#version 460") && wgsl.contains("@compute @workgroup_size(32)"));
    assert!(generate_cuda(&program, 0).is_err());
    let too_large = checked("table T capacity 4294967296 {key id:id64<T>;col v:u8;}");
    assert_eq!(
        generate_wgsl(&too_large, 1).unwrap_err().code,
        "D_GPU_RESOURCE"
    );
    if Command::new("glslangValidator")
        .arg("--version")
        .output()
        .is_ok()
    {
        let folder = Scratch::new();
        let path = folder.0.join("wide.comp");
        fs::write(&path, glsl).unwrap();
        let built = Command::new("glslangValidator").arg(path).output().unwrap();
        assert!(
            built.status.success(),
            "{}{}",
            String::from_utf8_lossy(&built.stdout),
            String::from_utf8_lossy(&built.stderr)
        );
    }
}

const EFFECT_CONTRACT: &str = r#"
effect Item(value:u64, payload:bytes32, enabled:bool);
effect Done();
transition send(n:u64, payload:bytes32, accept:bool) reads [] writes []
require bounded:le(n,3);
ensure count:eq(effect_count(),add(n,2));
ensure final:effect_is(Done,add(n,1));
ensure absent:not(effect_is(Item,18446744073709551615));
ensure wrong_type:not(effect_is(Item,add(n,1)));
ensure guarded:select(effect_is(Item,add(n,1)),eq(effect_arg(Item,add(n,1),value),0),true);
ensure payloads:fold_bounded<4>(ok:bool=true,i in [0..n]) {
  yield and(ok,and(effect_is(Item,i),and(eq(effect_arg(Item,i,value),i),and(eq(effect_arg(Item,i,payload),payload),eq(effect_arg(Item,i,enabled),true)))));
};
body {
  let %unused:u64=fold_bounded<4>(acc:u64=0,i in [0..n]) {
    emit Item(i,payload,true);
    yield acc;
  };
  emit Done();
  guard(accept,ERR_REJECT);
  commit;
}
"#;

#[test]
fn effect_contracts_execute_in_gpu_with_order_payload_and_transaction_scope() {
    run_cpp(
        EFFECT_CONTRACT,
        r#"
Ctx ctx{};vera_init(&ctx);
V payload=v_make(1u,2u,3u,4u,5u,6u,7u,8u);
assert(transition_v_send(&ctx,v_small(3u),payload,v_small(1u))==1u);
assert(ctx.s.effect_count==5u);
assert(transition_v_send(&ctx,v_zero(),payload,v_small(1u))==1u);
assert(ctx.s.effect_count==7u);
assert(transition_v_send(&ctx,v_small(2u),payload,v_zero())==0u);
assert(ctx.s.effect_count==7u);
assert(transition_v_send(&ctx,v_small(1u),payload,v_small(1u))==1u);
assert(ctx.s.effect_count==10u);
"#,
    );
    let code = generate_wgsl(&checked(EFFECT_CONTRACT), 2).unwrap();
    let module = naga::front::wgsl::parse_str(&code)
        .unwrap_or_else(|error| panic!("{}", error.emit_to_string(&code)));
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .unwrap();
}

#[test]
fn gpu_effect_contracts_reject_wrong_payload_order_and_invalid_access() {
    for source in [
        "effect A(v:u64);effect B();transition go() reads [] writes [] ensure e:effect_is(A,0);body {emit B();emit A(9);commit;}",
        "effect A(v:u64);transition go() reads [] writes [] ensure e:eq(effect_arg(A,0,v),10);body {emit A(9);commit;}",
        "effect A(v:u64);transition go() reads [] writes [] ensure e:eq(effect_arg(A,18446744073709551615,v),0);body {emit A(9);commit;}",
        "effect A(v:u64);effect B();transition go() reads [] writes [] ensure e:eq(effect_arg(A,0,v),0);body {emit B();commit;}",
    ] {
        run_cpp(source, "Ctx ctx{};vera_init(&ctx);assert(transition_v_go(&ctx)==2u);");
    }
}

#[test]
fn gpu_effect_buffers_follow_declared_bounds_without_arbitrary_policy_caps() {
    let large_body = "let %x:u64=fold_bounded<1024>(acc:u64=0,i in [0..1023]){emit A(0,0,0,0);yield acc;};commit;";
    let declarations = "effect A(a:u64,b:u64,c:u64,d:u64);";
    let larger_contract = format!("{declarations}transition go() reads [] writes [] ensure e:and(eq(effect_count(),1024),eq(effect_arg(A,0,d),0));body {{{large_body}}}");
    for target in [
        vera_compiler::codegen_gpu::GpuTarget::Glsl,
        vera_compiler::codegen_gpu::GpuTarget::Wgsl,
        vera_compiler::codegen_gpu::GpuTarget::Cuda,
    ] {
        vera_compiler::codegen_gpu::generate_gpu(&checked(&larger_contract), target, 2).unwrap();
    }
    let separate = format!("{declarations}transition large() reads [] writes [] body {{{large_body}}}transition small() reads [] writes [] ensure e:eq(effect_count(),1);body {{emit A(1,2,3,4);commit;}}");
    let code = generate_cuda(&checked(&separate), 2).unwrap();
    assert!(code.contains("emitted_kind[1]"));
    let without_query =
        format!("{declarations}transition large() reads [] writes [] body {{{large_body}}}");
    assert!(!generate_cuda(&checked(&without_query), 2)
        .unwrap()
        .contains("emitted_kind"));
}

#[test]
fn gpu_effect_indices_preserve_wide_aggregate_carriers() {
    let source = r#"table T capacity 2 {key id:id64<T>;col v:u64 [tracked_sum];}
effect A(v:u64);
transition go() reads [] writes [T[*].v]
ensure absent:not(effect_is(A,sum(T.v)));
body {let %a:id64<T>=table_alloc(T,{v:18446744073709551615});let %b:id64<T>=table_alloc(T,{v:1});emit A(0);commit;}"#;
    run_cpp(
        source,
        "Ctx ctx{};vera_init(&ctx);assert(transition_v_go(&ctx)==1u);",
    );
    let invalid = source.replace(
        "not(effect_is(A,sum(T.v)))",
        "eq(effect_arg(A,sum(T.v),v),0)",
    );
    run_cpp(
        &invalid,
        "Ctx ctx{};vera_init(&ctx);assert(transition_v_go(&ctx)==2u);",
    );
}

#[test]
fn gpu_effect_contract_storage_covers_payload_queries_nested_in_indices() {
    run_cpp(
        "effect A(v:u64);transition go() reads [] writes [] ensure e:effect_is(A,effect_arg(A,1,v));body {emit A(0);emit A(0);commit;}",
        "Ctx ctx{};vera_init(&ctx);assert(transition_v_go(&ctx)==1u);",
    );
}

#[test]
fn compositional_gpu_function_helpers_preserve_values_failure_and_argument_order() {
    let source = include_str!("fixtures/functions.vera");
    run_cpp(
        source,
        r#"
Ctx ctx{};vera_init(&ctx);V payload=v_make(1u,2u,3u,4u,5u,6u,7u,8u);
assert(transition_v_run(&ctx,v_small(3u),payload)==1u);
assert(ctx.s.effect_count==1u);
assert(transition_v_lazy(&ctx)==1u);
assert(transition_v_bad_pre(&ctx)==3u);assert(ctx.s.effect_count==2u);
assert(transition_v_bad_arithmetic(&ctx)==3u);
assert(transition_v_ordered_requires(&ctx,v_small(255u))==3u);
assert(transition_v_ordered_requires(&ctx,v_zero())==3u);
assert(transition_v_argument_order(&ctx)==1u);assert(ctx.s.effect_count==5u);
"#,
    );
    let program = checked(source);
    let cuda = generate_cuda(&program, 2).unwrap();
    assert_eq!(
        cuda.matches("__device__ inline R vera_function_").count(),
        program.functions().len()
    );
    let code = generate_wgsl(&program, 2).unwrap();
    let module = naga::front::wgsl::parse_str(&code)
        .unwrap_or_else(|error| panic!("{}", error.emit_to_string(&code)));
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .unwrap();
}

#[test]
fn gpu_scalar_function_boundaries_narrow_arguments_and_promote_results() {
    run_cpp(
        include_str!("fixtures/function_aggregate.vera"),
        r#"
Ctx ctx{};vera_init(&ctx);assert(transition_v_init(&ctx)==1u);
assert(transition_v_round(&ctx)==3u);assert(transition_v_promoted(&ctx)==1u);
"#,
    );
}

#[test]
fn native_gpu_records_unions_functions_and_structured_tables_execute() {
    let source = include_str!("fixtures/gpu_structured.vera");
    run_cpp(
        source,
        r#"
Ctx ctx{};vera_init(&ctx);
VeraT0 p=vera_type_0_zero();p.f_0=v_small(7u);p.f_1=v_make(1u,2u,3u,4u,5u,6u,7u,8u);
VeraT1 w=vera_type_1_zero();w.f_0=v_make(9u,10u,11u,12u,13u,14u,15u,16u);w.f_1=v_make(17u,18u,19u,20u,21u,22u,23u,24u);
assert(transition_v_save(&ctx,p,w,v_zero())==0u);assert(ctx.s.effect_count==0u);
assert(transition_v_save(&ctx,p,w,v_small(1u))==1u);assert(ctx.s.effect_count==1u);
assert(transition_v_lookup(&ctx,p)==1u);assert(ctx.s.effect_count==2u);
assert(transition_v_save(&ctx,p,w,v_small(1u))==0u);assert(ctx.s.effect_count==2u);
assert(transition_v_wrongtag(&ctx)==3u);assert(ctx.s.effect_count==2u);
assert(transition_v_lazy(&ctx)==1u);assert(transition_v_ordered(&ctx)==1u);
assert(vera_type_1_eq(ctx.s.col_0_2[0].f_1,w));
assert(ctx.s.col_0_2[0].f_1.f_1.w[7]==24u);
VeraT0 invalid=p;invalid.f_0.w[2]=1u;assert(transition_v_save(&ctx,invalid,w,v_small(1u))==0u);
"#,
    );
    let program = checked(source);
    let code = generate_wgsl(&program, 2).unwrap();
    let module = naga::front::wgsl::parse_str(&code)
        .unwrap_or_else(|error| panic!("{}", error.emit_to_string(&code)));
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .unwrap();
}

#[test]
fn gpu_structured_hash_matches_canonical_bytes_including_union_padding() {
    use vera_compiler::{
        ir::VariantId,
        types::{mix64, ValueType},
        value::{encode_value, Value},
    };
    let source="record R {flag:bool;amount:i64;blob:bytes32;} union U {Large:R;Small:u8;} transition go() reads [] writes [] body {commit;}";
    let program = checked(source);
    let record_id = program.type_id("R").unwrap();
    let union_id = program.type_id("U").unwrap();
    let record = Value::Record {
        ty: record_id,
        fields: vec![
            Value::Bool(true),
            Value::Int((-3).into()),
            Value::Int((BigInt::from(1u8) << 248usize) + 7u8),
        ],
    };
    let values = [
        record.clone(),
        Value::Variant {
            ty: union_id,
            tag: VariantId(0),
            payload: Box::new(record),
        },
        Value::Variant {
            ty: union_id,
            tag: VariantId(1),
            payload: Box::new(Value::Int(9.into())),
        },
    ];
    let types = [
        ValueType::Named(record_id),
        ValueType::Named(union_id),
        ValueType::Named(union_id),
    ];
    let hashes = values
        .iter()
        .zip(&types)
        .map(|(v, t)| {
            let bytes = encode_value(&program, v, t).unwrap();
            mix64(bytes.iter().fold(14695981039346656037u64, |h, b| {
                (h ^ u64::from(*b)).wrapping_mul(1099511628211)
            }))
        })
        .collect::<Vec<_>>();
    let main = format!(
        r#"
VeraT0 r=vera_type_0_zero();r.f_0=v_small(1u);r.f_1=v_neg(v_small(3u));r.f_2=v_make(7u,0u,0u,0u,0u,0u,0u,16777216u);
V h=vera_type_0_hash(r);assert(h.w[0]=={}u&&h.w[1]=={}u);
VeraT1 u=vera_type_1_zero();u.p0=r;h=vera_type_1_hash(u);assert(h.w[0]=={}u&&h.w[1]=={}u);
u.tag=1u;u.p1=v_small(9u);h=vera_type_1_hash(u);assert(h.w[0]=={}u&&h.w[1]=={}u);
"#,
        hashes[0] as u32,
        hashes[0] >> 32,
        hashes[1] as u32,
        hashes[1] >> 32,
        hashes[2] as u32,
        hashes[2] >> 32
    );
    run_cpp(source, &main);
}

#[test]
fn gpu_empty_records_and_unit_union_payloads_are_real_values() {
    let source = r#"record Unit {} union Option {None:Unit;Some:bytes32;}
function none()->Option body {yield variant<Option,None>(record<Unit>());}
function take(x:Option)->Unit require empty:is_variant<Option,None>(x);body {yield payload<Option,None>(x);}
effect Saved(value:Unit,option:Option);
transition go() reads [] writes []
ensure unit:eq(effect_arg(Saved,0,value),record<Unit>());
ensure variant:is_variant<Option,None>(effect_arg(Saved,0,option));
body {let %x:Unit=take(none());emit Saved(%x,none());commit;}"#;
    run_cpp(
        source,
        "Ctx ctx{};vera_init(&ctx);assert(transition_v_go(&ctx)==1u);",
    );
    let code = generate_wgsl(&checked(source), 2).unwrap();
    let module = naga::front::wgsl::parse_str(&code)
        .unwrap_or_else(|error| panic!("{}", error.emit_to_string(&code)));
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .unwrap();
}

#[test]
fn gpu_structured_scalar_projections_promote_and_constructors_narrow() {
    run_cpp(
        r#"record R {v:u64;} union U {Value:u64;}
table T capacity 2 {key id:id64<T>;col v:u64 [tracked_sum];}
transition fill() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:18446744073709551615});commit;}
transition project() reads [T[*].v] writes [] body {
 let %r:R=record<R>(v:18446744073709551615);
 let %u:U=variant<U,Value>(18446744073709551615);
 guard(eq(add(field(%r,v),field(%r,v)),sum(T.v)),ERR_BAD);
 guard(eq(add(payload<U,Value>(%u),payload<U,Value>(%u)),sum(T.v)),ERR_BAD);
 commit;
}
transition narrow_record() reads [T[*].v] writes [] body {let %r:R=record<R>(v:sum(T.v));commit;}
transition narrow_union() reads [T[*].v] writes [] body {let %u:U=variant<U,Value>(sum(T.v));commit;}"#,
        "Ctx ctx{};vera_init(&ctx);assert(transition_v_fill(&ctx)==1u);assert(transition_v_fill(&ctx)==1u);assert(transition_v_project(&ctx)==1u);assert(transition_v_narrow_record(&ctx)==3u);assert(transition_v_narrow_union(&ctx)==3u);",
    );
}

#[test]
fn gpu_nominal_type_and_call_interfaces_grow_linearly() {
    let mut previous = None;
    for size in [8, 16, 32] {
        let mut source = String::from("record R {");
        for n in 0..size {
            source.push_str(&format!("field_{n}:u64;"));
        }
        source.push_str("} union U {");
        for n in 0..size {
            source.push_str(&format!("V{n}:R;"));
        }
        source.push('}');
        source.push_str("function f0(x:R)->R body {yield x;}");
        for n in 1..size {
            source.push_str(&format!(
                "function f{n}(x:R)->R body {{yield f{}(x);}}",
                n - 1
            ));
        }
        source.push_str(&format!("effect E(x:R); transition go(x:U) reads [] writes [] body {{let %r:R=f{}(payload<U,V0>(x));emit E(%r);commit;}}",size-1));
        let start = std::time::Instant::now();
        let code = generate_wgsl(&checked(&source), 2).unwrap();
        let module = naga::front::wgsl::parse_str(&code).unwrap();
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .unwrap();
        let expressions = module
            .functions
            .iter()
            .map(|(_, f)| f.expressions.len())
            .sum::<usize>();
        eprintln!("structured interfaces size={size} bytes={} functions={} expressions={expressions} generation_parse_validation={:?}",code.len(),module.functions.len(),start.elapsed());
        if let Some((bytes, functions, exprs)) = previous {
            // Doubling fields, alternatives, and helper-chain length must not
            // duplicate callee bodies or enumerate products of union variants.
            assert!(code.len() < 2 * bytes);
            assert!(module.functions.len() < 2 * functions);
            assert!(expressions < 2 * exprs);
        }
        previous = Some((code.len(), module.functions.len(), expressions));
    }
}

#[test]
fn low64_multiplication_and_mix_match_native_reference_vectors() {
    run_cpp(
        "transition go() reads [] writes [] body {commit;}",
        r#"
const uint64_t edge[]={0,1,65535,65536,4294967295ULL,4294967296ULL,9223372036854775808ULL,18446744073709551615ULL};
auto check=[](uint64_t a,uint64_t b){
V av=v_make(uint32_t(a),uint32_t(a>>32),7u,9u,11u,13u,15u,17u);
V bv=v_make(uint32_t(b),uint32_t(b>>32),19u,21u,23u,25u,27u,29u);
V got=v_mul64(av,bv);uint64_t expected=a*b;
assert(got.w[0]==uint32_t(expected)&&got.w[1]==uint32_t(expected>>32));
for(unsigned i=2;i<8;++i)assert(got.w[i]==0);
uint64_t mixed=a;mixed=(mixed^(mixed>>30))*0xbf58476d1ce4e5b9ULL;mixed=(mixed^(mixed>>27))*0x94d049bb133111ebULL;mixed^=mixed>>31;
V h=vera_mix(av);assert(h.w[0]==uint32_t(mixed)&&h.w[1]==uint32_t(mixed>>32));for(unsigned i=2;i<8;++i)assert(h.w[i]==0);
};
for(uint64_t a:edge)for(uint64_t b:edge)check(a,b);
uint64_t rng=0x3456789abcdef123ULL;
for(unsigned i=0;i<10000;++i){rng^=rng<<13;rng^=rng>>7;rng^=rng<<17;uint64_t a=rng;rng^=rng<<13;rng^=rng>>7;rng^=rng<<17;check(a,rng);}
"#,
    );
}

#[test]
fn gpu_bounded_bytes_and_nested_canonical_codecs_execute() {
    let source = include_str!("fixtures/gpu_bounded_bytes.vera");
    run_cpp(
        source,
        r#"
Ctx ctx{};vera_init(&ctx);VeraB4 x=vera_bytes_4_zero();x.len=3u;x.data[0]=1u;x.data[1]=2u;x.data[2]=3u;
assert(transition_v_save(&ctx,x)==1u);assert(transition_v_lookup(&ctx,x)==1u);
assert(ctx.s.col_0_1[0].f_1.len==3u&&ctx.s.col_0_1[0].f_1.data[0]==255u);
assert(transition_v_empty(&ctx)==1u);assert(transition_v_get(&ctx,x,v_small(2u))==1u);
assert(transition_v_get(&ctx,x,v_small(3u))==3u);assert(transition_v_narrow(&ctx,x)==3u);
VeraB17 bad=vera_bytes_17_zero();bad.len=17u;bad.data[0]=99u;
assert(transition_v_invalid(&ctx,bad)==3u);assert(transition_v_lazy(&ctx,bad)==1u);
bad.data[0]=0u;bad.data[4]=2u;assert(transition_v_invalid(&ctx,bad)==3u);
bad.data[4]=1u;bad.data[5]=5u;assert(transition_v_invalid(&ctx,bad)==3u);
bad.data[5]=1u;bad.data[13]=7u;bad.data[16]=1u;assert(transition_v_invalid(&ctx,bad)==3u);
bad.data[16]=0u;assert(transition_v_invalid(&ctx,bad)==1u);
bad.data[0]=1u;assert(transition_v_invalid(&ctx,bad)==3u);
bad=vera_bytes_17_zero();bad.len=17u;bad.data[0]=1u;assert(transition_v_invalid(&ctx,bad)==1u);
bad.len=16u;assert(transition_v_invalid(&ctx,bad)==3u);
"#,
    );
    let code = generate_wgsl(&checked(source), 2).unwrap();
    let module = naga::front::wgsl::parse_str(&code)
        .unwrap_or_else(|e| panic!("{}", e.emit_to_string(&code)));
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .unwrap();
}

#[test]
fn gpu_byte_indices_preserve_wide_aggregate_carriers() {
    run_cpp(r#"table T capacity 2 {key id:id64<T>;col v:u64 [tracked_sum];}
table B capacity 2 {key id:id64<B>;col v:u8 [tracked_sum];}
transition fill_byte() reads [] writes [B[*].v] body {let %k:id64<B>=table_alloc(B,{v:255});commit;}
transition fill() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:9223372036854775808});commit;}
transition get() reads [T[*].v] writes [] body {let %x:u8=bytes_get(hex<1>("ff"),sum(T.v));commit;}
transition slice() reads [T[*].v] writes [] body {let %x:bytes<1>=bytes_slice<1>(hex<1>("ff"),sum(T.v),0);commit;}
transition count() reads [T[*].v] writes [] body {let %x:bytes<1>=bytes_slice<1>(hex<1>("ff"),0,sum(T.v));commit;}
transition promote() reads [T[*].v,B[*].v] writes [] body {guard(gt(add(sum(T.v),bytes_len(hex<1>("01"))),sum(T.v)),ERR_BAD);guard(gt(add(sum(B.v),bytes_get(hex<1>("ff"),0)),sum(B.v)),ERR_BAD);commit;}"#,
"Ctx ctx{};vera_init(&ctx);assert(transition_v_fill(&ctx)==1u);assert(transition_v_fill(&ctx)==1u);assert(transition_v_get(&ctx)==3u);assert(transition_v_slice(&ctx)==3u);assert(transition_v_count(&ctx)==3u);assert(transition_v_fill_byte(&ctx)==1u);assert(transition_v_fill_byte(&ctx)==1u);assert(transition_v_promote(&ctx)==1u);");
}

#[test]
fn gpu_canonical_byte_hash_and_scalar_wire_vectors_match_native() {
    use vera_compiler::{
        types::{mix64, ValueType},
        value::{encode_value, Value},
    };
    let source = include_str!("fixtures/gpu_bounded_bytes.vera");
    let program = checked(source);
    let wire = encode_value(
        &program,
        &Value::Bytes(vec![1, 2, 3]),
        &ValueType::Bytes { capacity: 4 },
    )
    .unwrap();
    let hash = mix64(wire.iter().fold(14695981039346656037u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(1099511628211)
    }));
    run_cpp(source,&format!(r#"Ctx ctx{{}};vera_init(&ctx);VeraB4 x=vera_bytes_4_zero();x.len=3u;x.data[0]=1u;x.data[1]=2u;x.data[2]=3u;
V h=vera_bytes_4_hash(x);assert(h.w[0]=={}u&&h.w[1]=={}u);assert(transition_v_save(&ctx,x)==1u);
uint32_t expected[17]={{0,0,0,0,1,3,0,0,0,0,0,0,0,255,2,3,0}};
for(unsigned i=0;i<17;i++){{assert(ctx.emitted_e0_p0.data[i]==expected[i]);}}
"#,hash as u32,hash>>32).replace("emitted_e0_p0.data", "emitted_e0_p0[0].data"));
    run_cpp(
        r#"transition go() reads [] writes [] body {
 guard(eq(encode<i64>(-3),hex<8>("fdffffffffffffff")),ERR_BAD);
 guard(eq(decode<i64>(hex<8>("fdffffffffffffff")),-3),ERR_BAD);
 guard(eq(encode<bytes<2>>(hex<2>("ff")),hex<10>("0100000000000000ff00")),ERR_BAD);
 guard(not(can_decode<bool>(hex<1>("02"))),ERR_BAD);
 guard(not(can_decode<bytes<2>>(hex<10>("0100000001000000ff00"))),ERR_BAD);
 guard(not(can_decode<bytes<2>>(hex<10>("0100000000000000ff01"))),ERR_BAD);
 guard(eq(decode<bytes<2>>(hex<10>("0100000000000000ff00")),hex<2>("ff")),ERR_BAD);
 commit; }"#,
        "Ctx ctx{};vera_init(&ctx);assert(transition_v_go(&ctx)==1u);",
    );
}

#[test]
fn gpu_byte_capacity_and_codec_schema_growth_stays_structural() {
    let mut baseline = None;
    for capacity in [0, 64, 1024, 8192] {
        let source=format!("function cycle(x:bytes<{capacity}>)->bytes<{capacity}> body {{yield decode<bytes<{capacity}>>(encode<bytes<{capacity}>>(x));}} transition go(x:bytes<{capacity}>) reads [] writes [] body {{let %v:bytes<{capacity}>=cycle(x);commit;}}");
        let start = std::time::Instant::now();
        let code = generate_wgsl(&checked(&source), 2).unwrap();
        let module = naga::front::wgsl::parse_str(&code).unwrap();
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .unwrap();
        let expressions = module
            .functions
            .iter()
            .map(|(_, f)| f.expressions.len())
            .sum::<usize>();
        eprintln!("byte capacity={capacity} source_bytes={} functions={} expressions={expressions} generate_validate={:?}",code.len(),module.functions.len(),start.elapsed());
        if let Some(bytes) = baseline {
            assert!(code.len() < bytes * 2);
        } else {
            baseline = Some(code.len());
        }
    }
    let mut previous = None;
    for depth in [2, 4, 8] {
        let mut source = String::from("record R0 {data:bytes<4>;} ");
        for n in 1..depth {
            source.push_str(&format!("record R{n} {{a:R{};b:R{};}}", n - 1, n - 1));
        }
        source.push_str(&format!("transition go(x:R{}) reads [] writes [] body {{let %x:R{}=decode<R{}>(encode<R{}>(x));commit;}}",depth-1,depth-1,depth-1,depth-1));
        let start = std::time::Instant::now();
        let code = generate_wgsl(&checked(&source), 2).unwrap();
        let module = naga::front::wgsl::parse_str(&code).unwrap();
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .unwrap();
        eprintln!(
            "codec DAG depth={depth} source_bytes={} functions={} generate_validate={:?}",
            code.len(),
            module.functions.len(),
            start.elapsed()
        );
        if let Some(bytes) = previous {
            assert!(code.len() < bytes * 2);
        }
        previous = Some(code.len());
    }
}

#[test]
fn gpu_byte_seed_uniqueness_and_real_index_limits_are_explicit() {
    run_cpp(include_str!("fixtures/gpu_bounded_bytes.vera"),"Ctx ctx{};vera_init(&ctx);assert(vera_seed(&ctx));assert(ctx.s.occ[0]==1u&&ctx.s.occ[1]==1u);assert(!vera_bytes_4_eq(ctx.s.col_0_0[0],ctx.s.col_0_0[1]));");
    run_cpp("table T capacity 2 {key id:id64<T>;col value:bytes<0> [unique];}","Ctx ctx{};vera_init(&ctx);assert(!vera_seed(&ctx));vera_init(&ctx);assert(ctx.s.occ[0]==0u&&ctx.s.occ[1]==0u);");
    let p = checked("transition go(x:bytes<4294967296>) reads [] writes [] body {commit;}");
    let error = generate_wgsl(&p, 2).unwrap_err();
    assert_eq!(error.code, "D_GPU_RESOURCE");
    assert!(error.message.contains("32-bit indexing"));
}
