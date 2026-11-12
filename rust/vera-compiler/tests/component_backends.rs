//! Stateful helpers preserve invocation boundaries across actual backends.
#[path = "support/gpu_host.rs"]
mod gpu_host;
use std::{
    collections::BTreeMap,
    fs,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
use vera_compiler::{
    analyze,
    codegen_gpu::{generate_cuda, generate_glsl, generate_wgsl},
    codegen_rust::{generate_rust, RustOptions},
    ir::CheckedProgram,
    parse_source,
    runtime::{EffectRecord, FailureKind, Machine, State},
    value::Value,
};
const SOURCE: &str = include_str!("fixtures/components.vera");
fn checked(s: &str) -> CheckedProgram {
    analyze(parse_source(s).unwrap()).unwrap()
}
fn seeded(p: &CheckedProgram, value: u8) -> State {
    let mut s = State::empty(p).unwrap();
    s.tables[0].occupied[0] = true;
    s.tables[0].generations[0] = 1;
    s.tables[0].columns[0][0] = Value::Int(value.into());
    s.outbox.push(EffectRecord {
        effect: p.effect_id("Tick").unwrap(),
        args: vec![Value::Int(77u8.into())],
    });
    s
}
fn args(accept: bool) -> BTreeMap<String, Value> {
    [
        ("k".into(), Value::Int(2u8.into())),
        ("accept".into(), Value::Bool(accept)),
    ]
    .into()
}
#[test]
fn native_two_calls_have_fresh_bindings_and_atomic_outer_rollback() {
    let p = checked(SOURCE);
    let mut m = Machine::with_state(&p, seeded(&p, 3)).unwrap();
    m.execute("twice", &args(true), true).unwrap();
    assert_eq!(m.state.tables[0].columns[0][0], Value::Int(5u8.into()));
    assert_eq!(m.state.outbox.len(), 4);
    assert_eq!(m.last_bindings.len(), 2);
    let before = m.state.clone();
    assert!(m.call("twice", &args(false)).is_err());
    assert_eq!(m.state, before);
    let mut m = Machine::with_state(&p, seeded(&p, 9)).unwrap();
    let before = m.state.clone();
    let e = m
        .call("bad", &[("k".into(), Value::Int(2u8.into()))].into())
        .unwrap_err();
    assert_eq!(e.kind, FailureKind::Trap);
    assert_eq!(e.code, "ERR_OPERATION_PRECONDITION");
    assert_eq!(m.state, before);
}
#[test]
fn operation_replay_admits_only_owner_invariants() {
    let p = checked(&(SOURCE.to_owned() + "invariant external {forall k in T:le(T[k].v,5);}"));
    let s = seeded(&p, 9);
    assert!(Machine::with_state(&p, s.clone()).is_err());
    let op = p.operation_id("Counter.inc").unwrap();
    let mut m = Machine::with_state_operation(&p, s, op).unwrap();
    m.execute_operation(op, &[("k".into(), Value::Int(2u8.into()))].into(), true)
        .unwrap();
    assert_eq!(m.state.tables[0].columns[0][0], Value::Int(10u8.into()));
}
static SEQ: AtomicU64 = AtomicU64::new(0);
fn binary(code: &str, extension: &str, compiler: &str, flags: &[&str]) {
    let dir = std::env::temp_dir().join(format!(
        "vera-component-backend-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join(format!("case.{extension}"));
    let executable = dir.join("case");
    fs::write(&source, code).unwrap();
    let r = Command::new(compiler)
        .args(flags)
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let r = Command::new(&executable).output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn rust_helpers_execute_without_exposing_operation_intents() {
    let p = checked(SOURCE);
    let code = generate_rust(
        &p,
        RustOptions {
            use_rt: false,
            include_main: false,
        },
    )
    .unwrap();
    assert_eq!(code.matches("fn vera_operation_0(").count(), 1);
    binary(
        &(code
            + r#"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_v[0]=3;s.outbox.push(Effect::Tick{v:77});let pre=s.clone();twice(&mut s,2,true).unwrap();assert_eq!(s.t.col_v[0],5);assert_eq!(s.outbox.len(),4);Params::Twice{k:2,accept:true}.check_postconditions(&pre,&s).unwrap();let before=s.clone();assert!(twice(&mut s,2,false).is_err());assert_eq!(s,before);s.t.col_v[0]=9;let before=s.clone();assert_eq!(bad(&mut s,2),Err(ErrorCode::OperationPrecondition));assert_eq!(s,before);}"#),
        "rs",
        "rustc",
        &["--edition=2021", "-O"],
    );
}
const CUDA_PROBE: &str = r#"int main(){Ctx ctx;vera_init(&ctx);ctx.s.occ[0]=1;ctx.s.gen[0]=1;ctx.s.cols[0]=v_small(3);assert(transition_v_twice(&ctx,v_small(2),v_small(1))==1);assert(ctx.s.cols[0].w[0]==5);assert(ctx.s.effect_count==3);assert(transition_v_twice(&ctx,v_small(2),v_zero())==0);assert(ctx.s.cols[0].w[0]==5);assert(ctx.s.effect_count==3);ctx.s.cols[0]=v_small(9);assert(transition_v_bad(&ctx,v_small(2))==3);assert(ctx.s.cols[0].w[0]==9);assert(ctx.s.effect_count==3);}"#;
#[test]
fn cuda_host_executes_operation_boundaries_and_rollback() {
    let p = checked(SOURCE);
    binary(
        &format!(
            "#define VERA_GPU_HOST\n#include <cassert>\n{}\n{}",
            generate_cuda(&p, 2).unwrap(),
            CUDA_PROBE
        ),
        "cpp",
        "g++",
        &["-std=c++17", "-O1"],
    );
}
#[test]
fn real_glsl_monitors_call_local_old_and_effects() {
    let p = checked(SOURCE);
    let mut code =
        generate_glsl(&p, 2)
            .unwrap()
            .replacen("void main(){", "void vera_furnace_main(){", 1);
    code.push_str(r#"void main(){if(gl_GlobalInvocationID.x!=0u){return;}Ctx ctx;vera_init(ctx);ctx.s.occ[0]=1u;ctx.s.gen[0]=1u;ctx.s.cols[0]=v_small(3u);uint ok=transition_v_twice(ctx,v_small(2u),v_small(1u));uint rejected=transition_v_twice(ctx,v_small(2u),v_zero());ctx.s.cols[0]=v_small(9u);uint trap=transition_v_bad(ctx,v_small(2u));atomicAdd(violations,ok);atomicAdd(committed,rejected);atomicAdd(reverted,trap);atomicAdd(effects,ctx.s.cols[0].w[0]+ctx.s.effect_count);}"#);
    if let Some(actual) = gpu_host::dispatch(&code) {
        assert_eq!(actual, [1, 0, 3, 12]);
    }
}
#[test]
fn real_wgsl_monitors_call_local_old_and_effects() {
    let p = checked(SOURCE);
    let mut code = generate_wgsl(&p, 2).unwrap();
    code.push_str(r#"@compute @workgroup_size(1) fn probe(){var ctx:Ctx;vera_init(&ctx);ctx.s.occ[0]=1u;ctx.s.gen[0]=1u;ctx.s.cols[0]=v_small(3u);let ok=transition_v_twice(&ctx,v_small(2u),v_small(1u));let rejected=transition_v_twice(&ctx,v_small(2u),v_zero());ctx.s.cols[0]=v_small(9u);let trap=transition_v_bad(&ctx,v_small(2u));atomicStore(&output.violations,ok);atomicStore(&output.committed,rejected);atomicStore(&output.reverted,trap);atomicStore(&output.effects,ctx.s.cols[0].w[0]+ctx.s.effect_count);}"#);
    let module = naga::front::wgsl::parse_str(&code)
        .unwrap_or_else(|e| panic!("{}", e.emit_to_string(&code)));
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .unwrap();
    let options = naga::back::glsl::Options {
        version: naga::back::glsl::Version::Desktop(460),
        writer_flags: naga::back::glsl::WriterFlags::empty(),
        binding_map: [(
            naga::ResourceBinding {
                group: 0,
                binding: 0,
            },
            0,
        )]
        .into_iter()
        .collect(),
        zero_initialize_workgroup_memory: true,
    };
    let pipeline = naga::back::glsl::PipelineOptions {
        shader_stage: naga::ShaderStage::Compute,
        entry_point: "probe".into(),
        multiview: None,
    };
    let mut glsl = String::new();
    naga::back::glsl::Writer::new(
        &mut glsl,
        &module,
        &info,
        &options,
        &pipeline,
        naga::proc::BoundsCheckPolicies::default(),
    )
    .unwrap()
    .write()
    .unwrap();
    if let Some(actual) = gpu_host::dispatch(&glsl) {
        assert_eq!(actual, [1, 0, 3, 12]);
    }
}

#[test]
fn gpu_operation_monitor_failures_roll_back_the_enclosing_transaction() {
    for source in [
        SOURCE.replace(
            "ensure count:eq(effect_count(),1);",
            "ensure count:eq(effect_count(),2);",
        ),
        SOURCE.replace("forall k in T:le(T[k].v,10)", "forall k in T:le(T[k].v,4)"),
    ] {
        let p = checked(&source);
        let probe = r#"int main(){Ctx ctx;vera_init(&ctx);ctx.s.occ[0]=1;ctx.s.gen[0]=1;ctx.s.cols[0]=v_small(3);assert(transition_v_twice(&ctx,v_small(2),v_small(1))==3);assert(ctx.s.cols[0].w[0]==3);assert(ctx.s.effect_count==0);}"#;
        binary(
            &format!(
                "#define VERA_GPU_HOST\n#include <cassert>\n{}\n{}",
                generate_cuda(&p, 2).unwrap(),
                probe
            ),
            "cpp",
            "g++",
            &["-std=c++17", "-O1"],
        );
    }
}

#[test]
fn operation_effect_frames_preserve_structured_payloads_at_nonzero_offsets() {
    let source = r#"
    effect Lead();effect Blob(bytes:bytes<4>,flag:bool);
    component Emitter owns [] {
      operation send(bytes:bytes<4>) reads [] writes [] effects [Blob]
      ensure count:eq(effect_count(),1);
      ensure value:eq(effect_arg(Blob,0,bytes),bytes);
      ensure flag:eq(effect_arg(Blob,0,flag),true);
      body {emit Blob(bytes,true);commit;}
    }
    transition send() reads [] writes []
    ensure count:eq(effect_count(),3);
    ensure last:eq(effect_arg(Blob,2,bytes),hex<4>("0203"));
    body {emit Lead();call Emitter.send(hex<4>("01"));call Emitter.send(hex<4>("0203"));commit;}
    "#;
    let p = checked(source);
    let probe = r#"int main(){Ctx ctx;vera_init(&ctx);assert(transition_v_send(&ctx)==1);assert(ctx.s.effect_count==3);assert(ctx.emitted_e1_p0[1].len==1);assert(ctx.emitted_e1_p0[2].len==2);assert(ctx.emitted_e1_p0[2].data[1]==3);}"#;
    binary(
        &format!(
            "#define VERA_GPU_HOST\n#include <cassert>\n{}\n{}",
            generate_cuda(&p, 2).unwrap(),
            probe
        ),
        "cpp",
        "g++",
        &["-std=c++17", "-O1"],
    );
}

#[test]
fn gpu_operation_helpers_do_not_cross_multiply_unrelated_event_layouts() {
    let source = r#"effect Wide(a:u8,b:u8,c:u8);effect Ping();
component Emitter owns [] {
 operation wide() reads [] writes [] effects [Wide]
 ensure payload:eq(effect_arg(Wide,0,c),3);
 body {emit Wide(1,2,3);commit;}
 operation silent() reads [] writes [] effects [Ping] body {emit Ping();commit;}
}
transition one() reads [] writes [] body {call Emitter.wide();commit;}
transition many() reads [] writes [] ensure count:eq(effect_count(),100);
body {let %n:u64=fold_bounded<100>(a:u64=0,i in [0..99]) {emit Ping();yield add(a,1);};commit;}
transition quiet() reads [] writes [] body {call Emitter.silent();commit;}
"#;
    let p = checked(source);
    let code = generate_cuda(&p, 2).unwrap();
    assert!(
        code.contains("V emitted_args[3]"),
        "unrelated narrow event count must not multiply wide payload storage"
    );
    binary(&format!("#define VERA_GPU_HOST\n#include <cassert>\n{code}\nint main(){{Ctx ctx;vera_init(&ctx);assert(transition_v_one(&ctx)==1);assert(transition_v_many(&ctx)==1);assert(transition_v_quiet(&ctx)==1);assert(ctx.s.effect_count==102);}}"),"cpp","g++",&["-std=c++17","-O1"]);
}

#[test]
fn operation_calls_in_folds_and_ordered_arguments_use_call_entry_snapshots() {
    let combine = r#"
operation combine(k:id64<T>,a:u8,b:u8) reads [T[k].v] writes [T[k].v] effects [Tick]
require live:table_exists(T,k);
require bounds:and(le(a,4),le(b,5));
require current:eq(table_get(T,k,v),b);
ensure entry:eq(old(T[k].v),b);
ensure result:eq(table_get(T,k,v),add(a,b));
ensure count:eq(effect_count(),1);
body {table_set(T,k,v,add(a,b));emit Tick(add(a,b));commit;}
}
transition twice"#;
    let mut source = SOURCE.replace("}\ntransition twice", combine);
    source.push_str(
        r#"
transition folded(k:id64<T>) reads [T[k].v] writes [T[k].v]
require live:table_exists(T,k);
require room:le(table_get(T,k,v),7);
ensure end:eq(table_get(T,k,v),add(old(T[k].v),3));
ensure count:eq(effect_count(),3);
body {let %n:u64=fold_bounded<3>(n:u64=0,i in [0..2]) {call Counter.inc(k);yield add(n,1);};commit;}
transition ordered(k:id64<T>) reads [T[k].v] writes [T[k].v]
require live:table_exists(T,k);
require start:eq(table_get(T,k,v),1);
ensure end:eq(table_get(T,k,v),5);
ensure count:eq(effect_count(),3);
body {call Counter.combine(k,
fold_bounded<1>(n:u8=0,i in [0..0]) {call Counter.inc(k);yield table_get(T,k,v);},
fold_bounded<1>(n:u8=0,i in [0..0]) {call Counter.inc(k);yield table_get(T,k,v);});commit;}
"#,
    );
    let p = checked(&source);
    let args = [("k".into(), Value::Int(2u8.into()))].into();
    for (transition, result) in [("folded", 4u8), ("ordered", 5u8)] {
        let mut m = Machine::with_state(&p, seeded(&p, 1)).unwrap();
        m.call(transition, &args).unwrap();
        assert_eq!(m.state.tables[0].columns[0][0], Value::Int(result.into()));
        assert_eq!(m.state.outbox.len(), 4);
    }
    let code = generate_cuda(&p, 2).unwrap();
    binary(&format!("#define VERA_GPU_HOST\n#include <cassert>\n{code}\nint main(){{Ctx ctx;vera_init(&ctx);ctx.s.occ[0]=1;ctx.s.gen[0]=1;ctx.s.cols[0]=v_small(1);assert(transition_v_folded(&ctx,v_small(2))==1);assert(ctx.s.cols[0].w[0]==4);ctx.s.cols[0]=v_small(1);assert(transition_v_ordered(&ctx,v_small(2))==1);assert(ctx.s.cols[0].w[0]==5);assert(ctx.s.effect_count==6);}}"),"cpp","g++",&["-std=c++17","-O1"]);
    let code = generate_rust(
        &p,
        RustOptions {
            use_rt: false,
            include_main: false,
        },
    )
    .unwrap();
    binary(
        &(code
            + r#"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_v[0]=1;folded(&mut s,2).unwrap();assert_eq!(s.t.col_v[0],4);s.t.col_v[0]=1;ordered(&mut s,2).unwrap();assert_eq!(s.t.col_v[0],5);assert_eq!(s.outbox.len(),6);}"#),
        "rs",
        "rustc",
        &["--edition=2021", "-O"],
    );
}
