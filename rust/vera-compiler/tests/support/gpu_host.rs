//! Execute compute shaders through the project's real surfaceless EGL host.
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn required() -> bool {
    std::env::var("VERA_REQUIRE_GPU_TESTS").is_ok_and(|value| value == "1")
}

fn unavailable(reason: &str) {
    assert!(!required(), "required GPU execution unavailable: {reason}");
    eprintln!("GPU execution skipped: {reason}");
}

fn host() -> Option<&'static PathBuf> {
    static HOST: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOST.get_or_init(|| {
        if let Some(path) = std::env::var_os("VERA_GLSL_HOST") {
            return Some(PathBuf::from(path));
        }
        let folder = std::env::temp_dir().join(format!("vera-gpu-host-{}", std::process::id()));
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
            Err(error) => panic!("failed to start GPU host compiler: {error}"),
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

struct ShaderFile(PathBuf);
impl Drop for ShaderFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Compile and dispatch one compute workgroup, then read its four-u32 output.
pub fn dispatch(code: &str) -> Option<[u32; 4]> {
    let host = host()?;
    let source = ShaderFile(std::env::temp_dir().join(format!(
        "vera-gpu-{}-{}.comp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )));
    fs::write(&source.0, code).unwrap();
    match Command::new("glslangValidator").arg("--version").output() {
        Ok(version) => {
            assert!(version.status.success(), "glslangValidator is not runnable");
            let compiled = Command::new("glslangValidator")
                .arg(&source.0)
                .output()
                .unwrap();
            assert!(
                compiled.status.success(),
                "{}{}",
                String::from_utf8_lossy(&compiled.stdout),
                String::from_utf8_lossy(&compiled.stderr)
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required() => {}
        Err(error) => panic!("required shader validator unavailable: {error}"),
    }
    let result = Command::new(host)
        .arg(&source.0)
        .arg("1")
        .env("EGL_PLATFORM", "surfaceless")
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .env("MESA_GL_VERSION_OVERRIDE", "4.6")
        .env("MESA_GLSL_VERSION_OVERRIDE", "460")
        .output()
        .unwrap();
    if result.status.code() == Some(77) {
        unavailable(&String::from_utf8_lossy(&result.stderr));
        return None;
    }
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8(result.stdout).unwrap();
    let counters = stdout
        .lines()
        .find(|line| line.starts_with("violations="))
        .expect("GPU host must return its compute output");
    let mut result = [0; 4];
    let mut fields = counters.split_whitespace();
    for (destination, name) in
        result
            .iter_mut()
            .zip(["violations", "committed", "reverted", "effects"])
    {
        *destination = fields
            .next()
            .and_then(|field| field.strip_prefix(&format!("{name}=")))
            .expect("GPU host counter names and order must match the shader ABI")
            .parse()
            .expect("GPU counters must be unsigned 32-bit values");
    }
    assert!(
        fields.next().is_none(),
        "unexpected GPU host output: {counters}"
    );
    Some(result)
}
