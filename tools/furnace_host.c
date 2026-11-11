/* VERA furnace host: EGL surfaceless + OpenGL 4.6 compute.
 *
 * Usage:  furnace_host <shader.comp> [workgroups]
 *
 * Creates a headless GL 4.6 context, compiles the generated compute shader,
 * dispatches it across the GPU, and reads back the violation / commit counters.
 */
#define GL_GLEXT_PROTOTYPES
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>
#include <GL/glext.h>

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static char *read_file(const char *path, long *out_len) {
    FILE *f = fopen(path, "rb");
    if (!f) { fprintf(stderr, "cannot open %s\n", path); exit(2); }
    fseek(f, 0, SEEK_END);
    long len = ftell(f);
    fseek(f, 0, SEEK_SET);
    char *buf = (char *)malloc((size_t)len + 1);
    if (fread(buf, 1, (size_t)len, f) != (size_t)len) { fprintf(stderr, "short read\n"); exit(2); }
    buf[len] = 0;
    fclose(f);
    *out_len = len;
    return buf;
}

#define CHECK_GL(msg) do { \
    GLenum e = glGetError(); \
    if (e != GL_NO_ERROR) { fprintf(stderr, "GL error 0x%x: %s\n", e, msg); exit(3); } \
} while (0)

int main(int argc, char **argv) {
    if (argc < 2) { fprintf(stderr, "usage: %s <shader.comp> [workgroups]\n", argv[0]); return 2; }
    long src_len = 0;
    char *src = read_file(argv[1], &src_len);
    int workgroups = argc > 2 ? atoi(argv[2]) : 64;

    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (dpy == EGL_NO_DISPLAY) { fprintf(stderr, "no EGL display\n"); return 1; }
    EGLint major = 0, minor = 0;
    if (!eglInitialize(dpy, &major, &minor)) { fprintf(stderr, "eglInitialize failed\n"); return 1; }
    printf("EGL %d.%d, vendor=%s\n", major, minor, eglQueryString(dpy, EGL_VENDOR));

    EGLint cfg_attribs[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig cfg;
    EGLint num_cfg = 0;
    if (!eglChooseConfig(dpy, cfg_attribs, &cfg, 1, &num_cfg) || num_cfg < 1) {
        fprintf(stderr, "no EGL config\n"); return 1;
    }
    eglBindAPI(EGL_OPENGL_API);
    EGLint ctx_attribs[] = {
        EGL_CONTEXT_MAJOR_VERSION, 4,
        EGL_CONTEXT_MINOR_VERSION, 6,
        EGL_CONTEXT_OPENGL_PROFILE_MASK, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
        EGL_NONE,
    };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attribs);
    if (ctx == EGL_NO_CONTEXT) { fprintf(stderr, "eglCreateContext failed\n"); return 1; }
    if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
        fprintf(stderr, "eglMakeCurrent (surfaceless) failed\n"); return 1;
    }
    printf("GL renderer: %s\n", (const char *)glGetString(GL_RENDERER));
    printf("GL version : %s\n", (const char *)glGetString(GL_VERSION));

    GLuint shader = glCreateShader(GL_COMPUTE_SHADER);
    glShaderSource(shader, 1, (const GLchar **)&src, NULL);
    glCompileShader(shader);
    GLint ok = 0;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[8192];
        glGetShaderInfoLog(shader, sizeof(log), NULL, log);
        fprintf(stderr, "compute shader compile error:\n%s\n", log);
        return 1;
    }
    GLuint program = glCreateProgram();
    glAttachShader(program, shader);
    glLinkProgram(program);
    glGetProgramiv(program, GL_LINK_STATUS, &ok);
    if (!ok) {
        char log[8192];
        glGetProgramInfoLog(program, sizeof(log), NULL, log);
        fprintf(stderr, "program link error:\n%s\n", log);
        return 1;
    }
    CHECK_GL("program");

    GLuint ssbo = 0;
    glGenBuffers(1, &ssbo);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, ssbo);
    uint32_t out[4] = {0, 0, 0, 0};
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof(out), out, GL_DYNAMIC_COPY);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, ssbo);
    CHECK_GL("ssbo");

    glUseProgram(program);
    glDispatchCompute((GLuint)workgroups, 1, 1);
    CHECK_GL("dispatch");
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
    CHECK_GL("barrier");

    uint32_t *res = (uint32_t *)glMapBufferRange(GL_SHADER_STORAGE_BUFFER, 0, sizeof(out), GL_MAP_READ_BIT);
    if (!res) { fprintf(stderr, "map failed\n"); return 1; }
    printf("workgroups=%d threads=%d\n", workgroups, workgroups * 32);
    printf("violations=%u committed=%u\n", res[0], res[1]);
    glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);

    glDeleteProgram(program);
    glDeleteShader(shader);
    glDeleteBuffers(1, &ssbo);
    eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroyContext(dpy, ctx);
    eglTerminate(dpy);
    free(src);
    return 0;
}
