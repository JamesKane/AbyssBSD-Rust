/* SPDX-License-Identifier: BSD-2-Clause */
/* Sets the mode, flips between two buffers a few times, optionally holds the picture
 * (second argument: seconds), turns the controller off. Usage: kmsflip [node [hold]] */
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

struct res { uint64_t fb, crtc, conn, enc; uint32_t nfb, ncrtc, nconn, nenc, minw, maxw, minh, maxh; };
struct mode { uint32_t clock; uint16_t hd, hss, hse, ht, hskew, vd, vss, vse, vt, vscan; uint32_t vrefresh, flags, type; char name[32]; };
struct conn { uint64_t enc, modes, props, vals; uint32_t nmodes, nprops, nenc, encid, id, type, typeid, connection, mmw, mmh, subpixel, pad; };
struct crtc { uint64_t conns; uint32_t nconns, id, fb, x, y, gamma, valid; struct mode mode; };
struct dumb { uint32_t h, w, bpp, flags, handle, pitch; uint64_t size; };
struct mapd { uint32_t handle, pad; uint64_t offset; };
struct fb2 { uint32_t id, w, h, fmt, flags, handles[4], pitches[4], offsets[4]; uint64_t mod[4]; };
struct flip { uint32_t crtc, fb, flags, reserved; uint64_t ud; };

#define GETRES _IOWR('d', 0xA0, struct res)
#define SETCRTC _IOWR('d', 0xA2, struct crtc)
#define GETCONN _IOWR('d', 0xA7, struct conn)
#define PFLIP _IOWR('d', 0xB0, struct flip)
#define CDUMB _IOWR('d', 0xB2, struct dumb)
#define MDUMB _IOWR('d', 0xB3, struct mapd)
#define ADDFB2 _IOWR('d', 0xB8, struct fb2)

static double now(void) {
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return t.tv_sec + t.tv_nsec / 1e9;
}

static uint32_t make(int fd, uint32_t w, uint32_t h, int variant) {
	struct dumb d = { .h = h, .w = w, .bpp = 32 };
	if (ioctl(fd, CDUMB, &d)) { perror("create dumb"); exit(1); }
	struct mapd m = { .handle = d.handle };
	if (ioctl(fd, MDUMB, &m)) { perror("map dumb"); exit(1); }
	uint32_t *p = mmap(0, d.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, m.offset);
	if (p == MAP_FAILED) { perror("mmap"); exit(1); }
	for (uint32_t y = 0; y < h; y++)
		for (uint32_t x = 0; x < w; x++) {
			uint32_t v;
			if (variant == 0) /* a diagonal gradient with a blue frame */
				v = (x < 6 || y < 6 || x >= w - 6 || y >= h - 6) ? 0x000000ff : ((x * 255 / w) << 16 | (y * 255 / h) << 8 | 0x40);
			else /* vertical bars and a red frame */
				v = (x < 6 || y < 6 || x >= w - 6 || y >= h - 6) ? 0x00ff0000 : (((x * 8 / w) & 1) ? 0x00ffffff : 0x00206020);
			p[y * (d.pitch / 4) + x] = v;
		}
	struct fb2 f = { .w = w, .h = h, .fmt = 0x34325258 /* XR24 */ };
	f.handles[0] = d.handle; f.pitches[0] = d.pitch;
	if (ioctl(fd, ADDFB2, &f)) { perror("addfb2"); exit(1); }
	return f.id;
}

int main(int argc, char **argv) {
	int fd = open(argc > 1 ? argv[1] : "/dev/dri/card1", O_RDWR);
	if (fd < 0) { perror("open"); return 1; }
	struct res r = {0};
	ioctl(fd, GETRES, &r);
	uint32_t crtcs[8], conns[8], fbs[8], encs[8];
	r.crtc = (uintptr_t)crtcs; r.conn = (uintptr_t)conns; r.fb = (uintptr_t)fbs; r.enc = (uintptr_t)encs;
	if (ioctl(fd, GETRES, &r)) { perror("getres"); return 1; }
	printf("%u crtc, %u connector, %u encoder\n", r.ncrtc, r.nconn, r.nenc);
	struct conn c = { .id = conns[0] };
	ioctl(fd, GETCONN, &c);
	struct mode modes[8];
	uint32_t props[16], vals[16], cenc[8];
	c.modes = (uintptr_t)modes; c.props = (uintptr_t)props; c.vals = (uintptr_t)vals; c.enc = (uintptr_t)cenc;
	if (c.nmodes > 8) c.nmodes = 8;
	if (ioctl(fd, GETCONN, &c) || c.nmodes == 0) { perror("getconn"); return 1; }
	struct mode m = modes[0];
	printf("mode %ux%u, clock %u kHz, refresh %u\n", m.hd, m.vd, m.clock, m.vrefresh);
	uint32_t a = make(fd, m.hd, m.vd, 0), b = make(fd, m.hd, m.vd, 1);
	struct crtc s = { .conns = (uintptr_t)conns, .nconns = 1, .id = crtcs[0], .fb = a, .valid = 1, .mode = m };
	double t0 = now();
	if (ioctl(fd, SETCRTC, &s)) { perror("setcrtc"); return 1; }
	printf("set crtc: %.1f ms\n", (now() - t0) * 1e3);
	sleep(2);
	uint32_t cur = a;
	for (int i = 0; i < 6; i++) {
		cur = (cur == a) ? b : a;
		struct flip f = { .crtc = crtcs[0], .fb = cur };
		t0 = now();
		if (ioctl(fd, PFLIP, &f)) { perror("flip"); return 1; }
		printf("flip %d: %.1f ms\n", i, (now() - t0) * 1e3);
		sleep(1);
	}
	if (argc > 2) {
		printf("holding %s s\n", argv[2]);
		sleep(atoi(argv[2]));
	}
	struct crtc off = { .id = crtcs[0] };
	ioctl(fd, SETCRTC, &off);
	printf("off\n");
	return 0;
}
