//! Compile and execute GLSL using the same EGL host shipped with VERA.
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
};
use vera_compiler::{codegen_gpu::generate_glsl, parse_source, semantic::analyze};
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
fn required() -> bool {
    std::env::var("VERA_REQUIRE_GPU_TESTS").is_ok_and(|v| v == "1")
}
fn unavailable(reason: &str) {
    assert!(!required(), "required GLSL execution unavailable: {reason}");
    eprintln!("GLSL execution skipped: {reason}");
}
fn host() -> Option<&'static PathBuf> {
    static HOST: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOST.get_or_init(|| {
        if let Some(path) = std::env::var_os("VERA_GLSL_HOST") {
            return Some(PathBuf::from(path));
        }
        let folder = std::env::temp_dir().join(format!("vera-glsl-host-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let executable = folder.join("furnace_host");
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/furnace_host.c");
        let result = match Command::new("cc")
            .arg(source)
            .arg("-o")
            .arg(&executable)
            .args(["-lEGL", "-lGL"])
            .output()
        {
            Ok(result) => result,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                unavailable("C compiler is not installed");
                return None;
            }
            Err(error) => panic!("failed to start GLSL host compiler: {error}"),
        };
        if !result.status.success() {
            let error = String::from_utf8_lossy(&result.stderr);
            if error.contains("No such file or directory") || error.contains("cannot find -l") {
                unavailable(&format!(
                    "EGL/OpenGL development dependencies are missing: {error}"
                ));
                return None;
            }
            panic!("EGL host compilation failed: {error}");
        }
        Some(executable)
    })
    .as_ref()
}
fn run(source: &str, probe: &str, expected: &str) {
    let Some(host) = host() else { return };
    let program = analyze(parse_source(source).unwrap()).unwrap();
    let mut code = generate_glsl(&program, 2).unwrap();
    if !probe.is_empty() {
        code = code.replacen("void main(){", "void vera_furnace_main(){", 1);
        code.push_str(probe);
    }
    let path = std::env::temp_dir().join(format!(
        "vera-glsl-{}-{}.comp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, &code).unwrap();
    if let Ok(version) = Command::new("glslangValidator").arg("--version").output() {
        assert!(version.status.success(), "glslangValidator is not runnable");
        let compiled = Command::new("glslangValidator")
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "{}{}",
            String::from_utf8_lossy(&compiled.stdout),
            String::from_utf8_lossy(&compiled.stderr)
        );
    } else if required() {
        panic!("VERA_REQUIRE_GPU_TESTS=1 requires glslangValidator");
    }
    let result = Command::new(host)
        .arg(&path)
        .arg("1")
        .env("EGL_PLATFORM", "surfaceless")
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .env("MESA_GL_VERSION_OVERRIDE", "4.6")
        .env("MESA_GLSL_VERSION_OVERRIDE", "460")
        .output()
        .unwrap();
    let _ = fs::remove_file(path);
    if result.status.code() == Some(77) {
        unavailable(&String::from_utf8_lossy(&result.stderr));
        return;
    }
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        stdout.contains(expected),
        "expected {expected}, actual output: {stdout}"
    );
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
