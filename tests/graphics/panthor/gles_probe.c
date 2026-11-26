/* SPDX-License-Identifier: BSD-2-Clause */
/*
 * gles_probe: the path Sway's GLES renderer takes on the Orange Pi, without Sway. Opens the
 * display card (komeda), makes a GBM device on it (Mesa's render-only layer then finds the GPU's
 * render node by driver name), brings up EGL and a surfaceless GLES 2 context, prints the
 * renderer, clears a 64x64 renderbuffer and draws a triangle into it, and checks the pixels read
 * back.
 *
 * cc -O2 -o gles_probe gles_probe.c -I/usr/local/include -L/usr/local/lib -lgbm -lEGL -lGLESv2
 * usage: gles_probe [card]   (default /dev/dri/card1)
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
#include <unistd.h>

#define W 64
#define H 64

static int
fail(const char *what)
{
	printf("gles_probe: FAIL: %s (egl %#x, gl %#x)\n", what, eglGetError(),
	    glGetError());
	return (1);
}

static GLuint
shader(GLenum type, const char *src)
{
	GLuint s = glCreateShader(type);
	GLint ok = 0;
	char log[512];

	glShaderSource(s, 1, &src, NULL);
	glCompileShader(s);
	glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
	if (!ok) {
		glGetShaderInfoLog(s, sizeof(log), NULL, log);
		printf("gles_probe: shader: %s\n", log);
	}
	return (s);
}

int
main(int argc, char **argv)
{
	const char *card = argc > 1 ? argv[1] : "/dev/dri/card1";
	int fd = open(card, O_RDWR | O_CLOEXEC);
	if (fd < 0)
		return (fail("open the card"));
	struct gbm_device *gbm = gbm_create_device(fd);
	if (gbm == NULL)
		return (fail("gbm_create_device"));
	printf("gles_probe: GBM backend %s\n", gbm_device_get_backend_name(gbm));
	PFNEGLGETPLATFORMDISPLAYEXTPROC get_display =
	    (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress(
		"eglGetPlatformDisplayEXT");
	EGLDisplay dpy = get_display(EGL_PLATFORM_GBM_KHR, gbm, NULL);
	EGLint major, minor;
	if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, &major, &minor))
		return (fail("eglInitialize"));
	printf("gles_probe: EGL %d.%d %s\n", major, minor,
	    eglQueryString(dpy, EGL_VENDOR));
	eglBindAPI(EGL_OPENGL_ES_API);
	EGLint cattr[] = { EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
		EGL_SURFACE_TYPE, 0, EGL_NONE };
	EGLConfig cfg;
	EGLint n = 0;
	if (!eglChooseConfig(dpy, cattr, &cfg, 1, &n) || n == 0)
		return (fail("eglChooseConfig"));
	EGLint xattr[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
	EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, xattr);
	if (ctx == EGL_NO_CONTEXT)
		return (fail("eglCreateContext"));
	if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx))
		return (fail("eglMakeCurrent"));
	printf("gles_probe: GL_RENDERER %s\n", glGetString(GL_RENDERER));
	printf("gles_probe: GL_VERSION %s\n", glGetString(GL_VERSION));

	GLuint rb, fb;
	glGenRenderbuffers(1, &rb);
	glBindRenderbuffer(GL_RENDERBUFFER, rb);
	glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8_OES, W, H);
	glGenFramebuffers(1, &fb);
	glBindFramebuffer(GL_FRAMEBUFFER, fb);
	glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
	    GL_RENDERBUFFER, rb);
	if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
		return (fail("framebuffer incomplete"));
	glViewport(0, 0, W, H);

	/* 1. A clear. */
	glClearColor(1, 0, 0, 1);
	glClear(GL_COLOR_BUFFER_BIT);
	unsigned char px[W * H * 4];
	memset(px, 0, sizeof(px));
	glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
	printf("gles_probe: clear reads %02x%02x%02x%02x\n", px[0], px[1],
	    px[2], px[3]);
	if (px[0] != 0xff || px[1] != 0 || px[2] != 0)
		return (fail("the clear did not land"));

	/* 2. A triangle over the whole viewport's lower left half, green. */
	const char *vs = "attribute vec2 p; void main() { gl_Position = vec4(p, 0.0, 1.0); }";
	const char *fs = "precision mediump float; void main() { gl_FragColor = vec4(0.0, 1.0, 0.0, 1.0); }";
	GLuint prog = glCreateProgram();
	glAttachShader(prog, shader(GL_VERTEX_SHADER, vs));
	glAttachShader(prog, shader(GL_FRAGMENT_SHADER, fs));
	glBindAttribLocation(prog, 0, "p");
	glLinkProgram(prog);
	glUseProgram(prog);
	GLfloat tri[] = { -1, -1, 1, -1, -1, 1 };
	glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, tri);
	glEnableVertexAttribArray(0);
	glDrawArrays(GL_TRIANGLES, 0, 3);
	glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
	unsigned char *ll = &px[(2 * W + 2) * 4];		/* lower left */
	unsigned char *ur = &px[((H - 3) * W + (W - 3)) * 4];	/* upper right */
	printf("gles_probe: triangle reads %02x%02x%02x lower left, %02x%02x%02x upper right\n",
	    ll[0], ll[1], ll[2], ur[0], ur[1], ur[2]);
	if (ll[1] != 0xff || ll[0] != 0 || ur[0] != 0xff || ur[1] != 0)
		return (fail("the triangle is not where it was drawn"));
	printf("gles_probe: PASS\n");
	return (0);
}
