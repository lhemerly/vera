"""OpenGL target, using the same exact limb compiler as CUDA and WGSL."""

from .codegen_gpu import GpuGenerator


def generate_glsl(prog, fuzz_cap=None, depth=64):
    """Emit complete declared state; the compatibility fuzz_cap never truncates it."""
    return GpuGenerator(prog, "glsl", depth).emit()
