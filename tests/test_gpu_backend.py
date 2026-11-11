"""OpenGL 4.6 fuzzing furnace (Target B) tests."""

import os
import shutil
import subprocess

import pytest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def shutil_which(name):
    return shutil.which(name)


glslang = shutil_which("glslangValidator")
gcc = shutil_which("gcc")


def _generate(tmp_path, spec, name):
    from vera.parser import parse_file
    from vera.semantic import analyze
    from vera.codegen_glsl import generate_glsl
    prog = analyze(parse_file(os.path.join(ROOT, spec)))
    path = tmp_path / name
    path.write_text(generate_glsl(prog))
    return str(path)


def test_glsl_shader_compiles(tmp_path):
    if glslang is None:
        pytest.skip("glslangValidator not available")
    shader = _generate(tmp_path, "examples/bank.vera", "bank.comp")
    proc = subprocess.run([glslang, shader], capture_output=True, text=True)
    assert proc.returncode == 0, proc.stderr


def test_furnace_runs_on_gpu(tmp_path):
    """End-to-end: compile the EGL host, run the shader, check the counters."""
    if glslang is None or gcc is None:
        pytest.skip("glslangValidator/gcc not available")
    if shutil_which("glxinfo") is None:
        pytest.skip("no GL stack")
    shader = _generate(tmp_path, "examples/bank.vera", "bank.comp")
    host = tmp_path / "furnace_host"
    proc = subprocess.run([gcc, "-O2", os.path.join(ROOT, "tools", "furnace_host.c"),
                           "-o", str(host), "-lEGL", "-lGL"],
                          capture_output=True, text=True)
    assert proc.returncode == 0, proc.stderr
    run = subprocess.run([str(host), shader, "8"], capture_output=True, text=True,
                         timeout=120)
    assert run.returncode == 0, run.stderr
    assert "AMD" in run.stdout or "Radeon" in run.stdout or "OpenGL" in run.stdout
    assert "violations=0" in run.stdout


def test_furnace_detects_violation(tmp_path):
    """A spec whose transfer can breach the bound invariant must be flagged."""
    if glslang is None or gcc is None:
        pytest.skip("glslangValidator/gcc not available")
    src = open(os.path.join(ROOT, "examples", "bank.vera")).read()
    buggy = src.replace(
        "  require no_bound_breach : le(amount, sub(1000000000, Account[to].balance));\n", "")
    buggy_path = tmp_path / "bound_buggy.vera"
    buggy_path.write_text(buggy)
    from vera.parser import parse_source
    from vera.semantic import analyze
    from vera.codegen_glsl import generate_glsl
    shader = tmp_path / "bound_buggy.comp"
    shader.write_text(generate_glsl(analyze(parse_source(buggy))))
    host = tmp_path / "furnace_host"
    subprocess.run([gcc, "-O2", os.path.join(ROOT, "tools", "furnace_host.c"),
                    "-o", str(host), "-lEGL", "-lGL"], capture_output=True, text=True)
    run = subprocess.run([str(host), str(shader), "64"], capture_output=True, text=True,
                         timeout=180)
    assert run.returncode == 0, run.stderr
    assert "violations=0" not in run.stdout
