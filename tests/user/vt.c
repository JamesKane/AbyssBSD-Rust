/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * vt: checks the requests on a virtual terminal (ttyv2) that vt
 * answers: its index, the console's version and information, the
 * display mode, a keyboard mode and lock state kept per window, the
 * switching mode (VT_SETMODE), the history size, the screen map, the
 * mouse cursor, a font of the window's own (PIO_VFONT), which sizes it,
 * releasing the console's keyboard and taking it back (CONS_RELKBD,
 * CONS_SETKBD), I/O port access (KDENABIO, KDDISABIO), the frame
 * buffer's description (FBIO_*) and the frame buffer mapped through
 * the window (as X's scfb maps it), the bell (KDMKTONE), and switching (VT_ACTIVATE, VT_WAITACTIVE) to the window and back,
 * which only an open window allows.
 *
 * Runs on FreeBSD too (as root, under vt, with ttyv2 not in use); the
 * screen comes back to the window that was visible. Without vt (no
 * framebuffer) it skips.
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/param.h>
#include <sys/consio.h>
#include <sys/fbio.h>
#include <sys/ioctl.h>
#include <sys/kbio.h>
#include <sys/mman.h>
#include <sys/wait.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <termios.h>
#include <unistd.h>

static int failed;

/* An 8x8 font: glyph 0 blank, glyph 1 a box for the space. */
static uint8_t glyphs[2 * 8] = {
	0, 0, 0, 0, 0, 0, 0, 0,
	0xff, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0xff,
};
static vfnt_map_t maps[2] = {
	{ .vfm_src = 0x20, .vfm_dst = 1, .vfm_len = 0 },
	{ .vfm_src = 0x10, .vfm_dst = 0, .vfm_len = 0 },
};

static void
font8x8(vfnt_t *f)
{
	memset(f, 0, sizeof(*f));
	f->map[VFNT_MAP_NORMAL] = maps;
	f->map_count[VFNT_MAP_NORMAL] = 1;
	f->glyphs = glyphs;
	f->glyph_count = 2;
	f->width = 8;
	f->height = 8;
}

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("vt: line %d: %s: ", __LINE__, #cond);		\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

#ifdef __amd64__
/* Reads port 0x61 (the speaker and NMI status), which is harmless. */
static int
inb61(void)
{
	unsigned char v;

	__asm __volatile("inb $0x61, %0" : "=a" (v));
	return (v);
}
#endif

/*
 * Runs f in a child; its wait status. A plain function pointer keeps
 * the child to what f does.
 */
static int
in_child(int (*f)(int), int fd)
{
	pid_t pid;
	int status;

	pid = fork();
	if (pid == 0)
		_exit(f(fd));
	if (pid < 0 || waitpid(pid, &status, 0) != pid)
		return (-1);
	return (status);
}

/* Not root, KDENABIO is refused: exits 0 if it is, with EPERM. */
static int
enabio_unprivileged(int fd)
{
	if (setuid(65534) != 0)
		return (2);
	return (ioctl(fd, KDENABIO) == -1 && errno == EPERM ? 0 : 1);
}

#ifdef __amd64__
/* After KDDISABIO, a port read faults. */
static int
port_read(int fd)
{
	(void)fd;
	inb61();
	return (0);
}
#endif

/* A request with an int argument passed by value (_IOWINT). */
static int
req(int fd, unsigned long r, int v)
{
	return (ioctl(fd, r, v));
}

int
main(void)
{
	struct vt_mode vm;
	struct winsize ws, ws8;
	vid_info_t vi;
	vfnt_t f;
	scrmap_t sm;
	mouse_info_t mi;
	keyboard_info_t ki;
	struct fbtype fbt;
	struct fb_rgboffs rgb;
	video_display_start_t ds;
	u_int line;
	volatile uint32_t *fb;
	uint32_t px;
	int rofd;
	video_info_t mode;
	int fd, active, v, i, closed;

	fd = open("/dev/ttyv2", O_RDWR | O_NONBLOCK);
	if (fd < 0 && errno == ENOENT) {
		/* No framebuffer, so no vt. */
		printf("vt: no /dev/ttyv2, skipped\n");
		return (0);
	}
	CHECK(fd >= 0, "ttyv2: %s", strerror(errno));
	CHECK(ioctl(fd, VT_GETINDEX, &v) == 0 && v == 3, "index %d", v);
	CHECK(ioctl(fd, VT_GETACTIVE, &active) == 0 && active >= 1 &&
	    active <= 12, "active %d", active);
	CHECK(active != 3, "ttyv2 is visible: run this from another window");
	CHECK(ioctl(fd, CONS_GETVERS, &v) == 0 && v == 0x200, "version %#x",
	    v);
	CHECK(ioctl(fd, CONS_GET, &v) == 0 && v == M_CG640x480, "mode %d", v);
	CHECK(ioctl(fd, TIOCGWINSZ, &ws) == 0 && ws.ws_row > 0 &&
	    ws.ws_col > 0, "%d rows %d cols", ws.ws_row, ws.ws_col);

	/* Display mode. */
	CHECK(ioctl(fd, KDGETMODE, &v) == 0 && v == KD_TEXT, "mode %d", v);
	CHECK(req(fd, KDSETMODE, KD_GRAPHICS) == 0 &&
	    ioctl(fd, KDGETMODE, &v) == 0 && v == KD_GRAPHICS, "graphics %d",
	    v);
	CHECK(req(fd, KDSETMODE, KD_TEXT) == 0 &&
	    ioctl(fd, KDGETMODE, &v) == 0 && v == KD_TEXT, "text %d", v);

	/* The keyboard as this hidden window keeps it. */
	CHECK(ioctl(fd, KDGKBMODE, &v) == 0 && v == K_XLATE, "kbmode %d", v);
	CHECK(req(fd, KDSKBMODE, K_RAW) == 0 &&
	    ioctl(fd, KDGKBMODE, &v) == 0 && v == K_RAW, "raw %d", v);
	CHECK(req(fd, KDSKBMODE, K_XLATE) == 0, "%s", strerror(errno));
	CHECK(req(fd, KDSKBMODE, 7) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	CHECK(req(fd, KDSKBSTATE, 0x100) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	CHECK(req(fd, KDSETLED, 8) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	CHECK(req(fd, KDSKBSTATE, CLKED) == 0 &&
	    ioctl(fd, KDGKBSTATE, &v) == 0 && v == CLKED, "state %#x", v);
	CHECK(req(fd, KDSKBSTATE, 0) == 0, "%s", strerror(errno));
	CHECK(ioctl(fd, KDGKBTYPE, &v) == 0, "%s", strerror(errno));
	CHECK(ioctl(fd, CONS_MODEINFO, &mode) == 0, "%s", strerror(errno));
	/* The bell, and a tone of 1 kHz for 10 ms: off, they only report. */
	CHECK(req(fd, KDMKTONE, 0) == 0, "%s", strerror(errno));
	CHECK(req(fd, KDMKTONE, 10 << 16 | 1193) == 0, "%s", strerror(errno));

	/* The frame buffer, the size TIOCGWINSZ gives in pixels. */
	CHECK(ioctl(fd, TIOCGWINSZ, &ws) == 0, "%s", strerror(errno));
	memset(&fbt, 0xff, sizeof(fbt));
	CHECK(ioctl(fd, FBIOGTYPE, &fbt) == 0 && fbt.fb_width == ws.ws_xpixel &&
	    fbt.fb_height == ws.ws_ypixel && fbt.fb_depth > 0 &&
	    fbt.fb_cmsize == 16, "fbtype %d %dx%d depth %d cmsize %d",
	    fbt.fb_type, fbt.fb_width, fbt.fb_height, fbt.fb_depth,
	    fbt.fb_cmsize);
	CHECK(ioctl(fd, FBIO_GETLINEWIDTH, &line) == 0 &&
	    line >= (u_int)fbt.fb_width * 4 &&
	    fbt.fb_size == (int)line * fbt.fb_height, "line %u size %d", line,
	    fbt.fb_size);
	line = 7;
	CHECK(ioctl(fd, FBIO_GETWINORG, &line) == 0 && line == 0, "origin %u",
	    line);
	ds.x = ds.y = 7;
	CHECK(ioctl(fd, FBIO_GETDISPSTART, &ds) == 0 && ds.x == 0 && ds.y == 0,
	    "start %d, %d", ds.x, ds.y);
	CHECK(ioctl(fd, FBIO_GETRGBOFFS, &rgb) == 0 && rgb.green == 8 &&
	    rgb.red + rgb.blue == 16, "offsets %d %d %d", rgb.red, rgb.green,
	    rgb.blue);

	/*
	 * The frame buffer, mapped shared through the window; a pixel is
	 * read and written back unchanged. No private mapping, none past
	 * its end, and writing needs the window open for writing.
	 */
	fb = mmap(NULL, fbt.fb_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	CHECK(fb != MAP_FAILED, "mmap: %s", strerror(errno));
	if (fb != MAP_FAILED) {
		px = fb[fbt.fb_size / 4 - 1];
		fb[fbt.fb_size / 4 - 1] = px;
		CHECK(munmap((void *)fb, fbt.fb_size) == 0, "%s", strerror(errno));
	}
	CHECK(mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0) == MAP_FAILED &&
	    errno == EINVAL, "private: %s", strerror(errno));
	CHECK(mmap(NULL, 4096, PROT_READ, MAP_SHARED, fd,
	    roundup2(fbt.fb_size, 4096)) == MAP_FAILED && errno == EINVAL,
	    "past the end: %s", strerror(errno));
	rofd = open("/dev/ttyv2", O_RDONLY | O_NONBLOCK);
	CHECK(rofd >= 0, "%s", strerror(errno));
	CHECK(mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, rofd, 0) ==
	    MAP_FAILED && errno == EACCES, "read-only: %s", strerror(errno));
	close(rofd);

	/* I/O port access: root only; on amd64, the ports open and close. */
	v = in_child(enabio_unprivileged, fd);
	CHECK(WIFEXITED(v) && WEXITSTATUS(v) == 0, "status %#x", v);
	CHECK(ioctl(fd, KDENABIO) == 0, "%s", strerror(errno));
#ifdef __amd64__
	inb61();
#endif
	CHECK(ioctl(fd, KDDISABIO) == 0, "%s", strerror(errno));
#ifdef __amd64__
	v = in_child(port_read, fd);
	CHECK(WIFSIGNALED(v) && WTERMSIG(v) == SIGBUS, "status %#x", v);
#endif

	/* The console's keyboard: released, and taken back. */
	CHECK(ioctl(fd, KDGKBINFO, &ki) == 0, "%s", strerror(errno));
	CHECK(req(fd, CONS_SETKBD, ki.kb_index) == 0, "%s", strerror(errno));
	CHECK(req(fd, CONS_SETKBD, 1000) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	CHECK(req(fd, CONS_SETKBD, -1) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	CHECK(ioctl(fd, CONS_RELKBD) == 0, "%s", strerror(errno));
	/* Without one, the keyboard's requests do nothing. */
	i = ki.kb_index;
	memset(&ki, 0xff, sizeof(ki));
	CHECK(ioctl(fd, KDGKBINFO, &ki) == 0 && ki.kb_index == 0 &&
	    ki.kb_flags == 0, "index %d", ki.kb_index);
	v = -1;
	CHECK(ioctl(fd, KDGKBTYPE, &v) == 0 && v == 0, "type %d", v);
	CHECK(req(fd, KDSKBSTATE, NLKED) == 0 &&
	    ioctl(fd, KDGKBSTATE, &v) == 0 && v == NLKED, "state %#x", v);
	CHECK(req(fd, KDSKBSTATE, 0) == 0, "%s", strerror(errno));
	CHECK(ioctl(fd, CONS_RELKBD) == 0, "%s", strerror(errno));
	CHECK(req(fd, CONS_SETKBD, i) == 0, "%s", strerror(errno));
	CHECK(ioctl(fd, KDGKBINFO, &ki) == 0 && ki.kb_index == i,
	    "index %d", ki.kb_index);

	/* The switching mode. */
	memset(&vm, 0xff, sizeof(vm));
	CHECK(ioctl(fd, VT_GETMODE, &vm) == 0 && vm.mode == VT_AUTO,
	    "mode %d", vm.mode);
	memset(&vm, 0, sizeof(vm));
	vm.mode = VT_PROCESS;
	CHECK(ioctl(fd, VT_SETMODE, &vm) == -1 && errno == EINVAL,
	    "no signals: %s", strerror(errno));
	vm.relsig = SIGUSR1;
	vm.acqsig = SIGUSR2;
	CHECK(ioctl(fd, VT_SETMODE, &vm) == 0, "%s", strerror(errno));
	memset(&vm, 0, sizeof(vm));
	CHECK(ioctl(fd, VT_GETMODE, &vm) == 0 && vm.mode == VT_PROCESS &&
	    vm.relsig == SIGUSR1 && vm.acqsig == SIGUSR2, "mode %d", vm.mode);
	CHECK(req(fd, VT_RELDISP, VT_TRUE) == -1 && errno == EINVAL,
	    "not visible: %s", strerror(errno));
	vm.mode = VT_AUTO;
	CHECK(ioctl(fd, VT_SETMODE, &vm) == 0, "%s", strerror(errno));
	vm.mode = 5;
	CHECK(ioctl(fd, VT_SETMODE, &vm) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	v = 3;
	CHECK(ioctl(fd, VT_LOCKSWITCH, &v) == -1 && errno == EINVAL, "%s",
	    strerror(errno));

	/* The console's information. */
	memset(&vi, 0, sizeof(vi));
	vi.size = sizeof(vi);
	CHECK(ioctl(fd, CONS_GETINFO, &vi) == 0 && vi.m_num == active &&
	    vi.mv_rsz == ws.ws_row && vi.mv_csz == ws.ws_col &&
	    vi.mv_row < vi.mv_rsz && vi.mv_col < vi.mv_csz, "m_num %d %dx%d",
	    vi.m_num, vi.mv_rsz, vi.mv_csz);
	vi.size = 10;
	CHECK(ioctl(fd, CONS_GETINFO, &vi) == -1 && errno == EINVAL, "%s",
	    strerror(errno));

	/* History, screen map, mouse cursor. */
	v = -1;
	CHECK(ioctl(fd, CONS_HISTORY, &v) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	v = 1000;
	CHECK(ioctl(fd, CONS_HISTORY, &v) == 0, "%s", strerror(errno));
	v = 500;
	CHECK(ioctl(fd, CONS_HISTORY, &v) == 0, "%s", strerror(errno));
	CHECK(ioctl(fd, CONS_CLRHIST) == 0, "%s", strerror(errno));
	CHECK(ioctl(fd, GIO_SCRNMAP, &sm) == 0, "%s", strerror(errno));
	for (i = 0; i < 256; i++)
		if ((u_char)sm.scrmap[i] != i)
			break;
	CHECK(i == 256, "scrmap[%d] = %d", i, (u_char)sm.scrmap[i]);
	memset(&mi, 0, sizeof(mi));
	mi.operation = MOUSE_MOVEABS;
	CHECK(ioctl(fd, CONS_MOUSECTL, &mi) == -1 && errno == EINVAL, "%s",
	    strerror(errno));

	/* A font of the window's own; the window takes its size. */
	font8x8(&f);
	f.width = 0;
	CHECK(ioctl(fd, PIO_VFONT, &f) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	f.width = 129;
	CHECK(ioctl(fd, PIO_VFONT, &f) == -1 && errno == E2BIG, "%s",
	    strerror(errno));
	font8x8(&f);
	f.glyph_count = 0;
	CHECK(ioctl(fd, PIO_VFONT, &f) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	font8x8(&f);
	f.map_count[VFNT_MAP_NORMAL] = 2;	/* 0x10 after 0x20 */
	CHECK(ioctl(fd, PIO_VFONT, &f) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	font8x8(&f);
	maps[0].vfm_dst = 2;			/* past the glyphs */
	CHECK(ioctl(fd, PIO_VFONT, &f) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	maps[0].vfm_dst = 1;
	f.glyphs = (void *)8;
	CHECK(ioctl(fd, PIO_VFONT, &f) == -1 && errno == EFAULT, "%s",
	    strerror(errno));
	font8x8(&f);
	CHECK(ioctl(fd, PIO_VFONT, &f) == 0, "%s", strerror(errno));
	memset(&ws8, 0, sizeof(ws8));
	CHECK(ioctl(fd, TIOCGWINSZ, &ws8) == 0 &&
	    ws8.ws_xpixel == ws.ws_xpixel && ws8.ws_ypixel == ws.ws_ypixel &&
	    ws8.ws_row == MIN(ws.ws_ypixel / 8, 150) &&
	    ws8.ws_col == MIN(ws.ws_xpixel / 8, 512),
	    "8x8: %d rows %d cols of %dx%d", ws8.ws_row, ws8.ws_col,
	    ws8.ws_xpixel, ws8.ws_ypixel);
	memset(&vi, 0, sizeof(vi));
	vi.size = sizeof(vi);
	CHECK(ioctl(fd, CONS_GETINFO, &vi) == 0 && vi.mv_rsz == ws8.ws_row &&
	    vi.mv_csz == ws8.ws_col, "%dx%d", vi.mv_rsz, vi.mv_csz);
	CHECK(ioctl(fd, PIO_VFONT_DEFAULT) == 0, "%s", strerror(errno));
	memset(&ws8, 0, sizeof(ws8));
	CHECK(ioctl(fd, TIOCGWINSZ, &ws8) == 0 && ws8.ws_row == ws.ws_row &&
	    ws8.ws_col == ws.ws_col, "default: %d rows %d cols", ws8.ws_row,
	    ws8.ws_col);

	/* Switching: not out of range, not to a window nobody has open. */
	CHECK(req(fd, VT_ACTIVATE, 0) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	CHECK(req(fd, VT_ACTIVATE, 13) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	closed = 0;
	CHECK(ioctl(fd, VT_OPENQRY, &closed) == 0 && closed >= 1 &&
	    closed <= 12 && closed != 3, "openqry %d", closed);
	if (closed > 1)
		CHECK(req(fd, VT_ACTIVATE, closed) == -1 && errno == EINVAL,
		    "to closed ttyv%x: %s", closed - 1, strerror(errno));
	CHECK(req(fd, VT_ACTIVATE, 3) == 0, "%s", strerror(errno));
	CHECK(req(fd, VT_WAITACTIVE, 3) == 0, "%s", strerror(errno));
	CHECK(ioctl(fd, VT_GETACTIVE, &v) == 0 && v == 3, "active %d", v);
	/*
	 * (Not KDGKBMODE here: vt wakes VT_WAITACTIVE before it gives the
	 * keyboard the window's mode, and on a visible window KDGKBMODE
	 * keeps what the keyboard says.)
	 */
	CHECK(ioctl(fd, KDGETMODE, &v) == 0 && v == KD_TEXT, "mode %d", v);
	CHECK(req(fd, VT_ACTIVATE, active) == 0, "%s", strerror(errno));
	CHECK(req(fd, VT_WAITACTIVE, active) == 0, "%s", strerror(errno));
	CHECK(ioctl(fd, VT_GETACTIVE, &v) == 0 && v == active, "back %d", v);

	close(fd);
	return (failed);
}
