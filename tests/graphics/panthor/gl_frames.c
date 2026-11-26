/* SPDX-License-Identifier: BSD-2-Clause */
/*
 * gl_frames: frames on the GPU with no display, to time the render path by itself. A GBM device
 * on a node, EGL and a surfaceless GLES 2 context, an 800x600 renderbuffer; each frame clears it,
 * draws a triangle (and, with "upload", first uploads a new 256x256 texture and draws with it, as
 * a compositor does with a client's shared-memory buffer), then waits for the GPU (glFinish).
 * Every 100 frames it prints the mean time per frame, so a path that slows down shows.
 *
 * cc -O2 -o gl_frames gl_frames.c -I/usr/local/include -L/usr/local/lib -lgbm -lEGL -lGLESv2
 * usage: gl_frames [node [frames [upload]]]   (default /dev/dri/renderD128, 1000)
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <fcntl.h>
#include <gbm.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static double
now_ms(void)
{
	struct timespec t;

	clock_gettime(CLOCK_MONOTONIC, &t);
	return (t.tv_sec * 1e3 + t.tv_nsec / 1e6);
}

static GLuint
shader(GLenum type, const char *src)
{
	GLuint s = glCreateShader(type);

	glShaderSource(s, 1, &src, NULL);
	glCompileShader(s);
	return (s);
}

int
main(int argc, char **argv)
{
	const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
	int frames = argc > 2 ? atoi(argv[2]) : 1000;
	int upload = argc > 3 && strcmp(argv[3], "upload") == 0;
	int fd = open(node, O_RDWR | O_CLOEXEC);

	if (fd < 0)
		return (perror(node), 1);
	struct gbm_device *gbm = gbm_create_device(fd);
	PFNEGLGETPLATFORMDISPLAYEXTPROC gpd =
	    (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
	EGLDisplay dpy = gpd(EGL_PLATFORM_GBM_MESA, gbm, NULL);
	EGLint major, minor;
	if (!eglInitialize(dpy, &major, &minor))
		return (printf("gl_frames: FAIL: eglInitialize %#x\n", eglGetError()), 1);
	eglBindAPI(EGL_OPENGL_ES_API);
	static const EGLint ca[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
	EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, ca);
	if (ctx == EGL_NO_CONTEXT ||
	    !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx))
		return (printf("gl_frames: FAIL: context %#x\n", eglGetError()), 1);
	printf("gl_frames: %s, %s%s\n", glGetString(GL_RENDERER), node,
	    upload ? ", a texture uploaded per frame" : "");

	GLuint rb, fb;
	glGenRenderbuffers(1, &rb);
	glBindRenderbuffer(GL_RENDERBUFFER, rb);
	glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8_OES, 800, 600);
	glGenFramebuffers(1, &fb);
	glBindFramebuffer(GL_FRAMEBUFFER, fb);
	glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb);
	glViewport(0, 0, 800, 600);

	GLuint p = glCreateProgram();
	glAttachShader(p, shader(GL_VERTEX_SHADER,
	    "attribute vec2 v; varying vec2 t;"
	    "void main() { t = v * 0.5 + 0.5; gl_Position = vec4(v, 0.0, 1.0); }"));
	glAttachShader(p, shader(GL_FRAGMENT_SHADER,
	    "precision mediump float; varying vec2 t; uniform sampler2D s;"
	    "void main() { gl_FragColor = texture2D(s, t) + vec4(t, 0.0, 1.0); }"));
	glBindAttribLocation(p, 0, "v");
	glLinkProgram(p);
	glUseProgram(p);
	static const GLfloat tri[] = { -1, -1, 1, -1, -1, 1 };
	glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, tri);
	glEnableVertexAttribArray(0);

	static unsigned char pixels[256 * 256 * 4];
	GLuint tex = 0;
	double t0 = now_ms(), start = t0;
	for (int i = 1; i <= frames; i++) {
		if (upload) {
			memset(pixels, i & 0xff, sizeof(pixels));
			glDeleteTextures(1, &tex);
			glGenTextures(1, &tex);
			glBindTexture(GL_TEXTURE_2D, tex);
			glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
			glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 256, 256, 0, GL_RGBA,
			    GL_UNSIGNED_BYTE, pixels);
		}
		glClearColor((i & 1) ? 1 : 0, 0, 0, 1);
		glClear(GL_COLOR_BUFFER_BIT);
		glDrawArrays(GL_TRIANGLES, 0, 3);
		glFinish();
		if (i % 100 == 0) {
			double t = now_ms();
			printf("gl_frames: frames %d-%d: %.3f ms each\n", i - 99, i,
			    (t - t0) / 100);
			fflush(stdout);
			t0 = t;
		}
	}
	printf("gl_frames: %d frames in %.1f ms\n", frames, now_ms() - start);
	return (0);
}
