/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * vtools: runs FreeBSD's kbdcontrol and vidcontrol on a virtual
 * terminal (ttyv2) as rc and a user would, and checks what they did
 * through the requests themselves: vidcontrol's history size, a font
 * loaded from a file (which sizes the window) and the default font back,
 * and colors; kbdcontrol's keyboard information, repeat rate, and a
 * keymap loaded from a file and dumped. What they change is put back.
 *
 * Runs on FreeBSD too (as root, under vt, with ttyv2 not in use), with
 * base's kbdcontrol and vidcontrol; AbyssBSD's memory disk has them
 * built static from FreeBSD's sources until dynamic linking. Without vt
 * or the tools it skips.
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/param.h>
#include <sys/consio.h>
#include <sys/ioctl.h>
#include <sys/kbio.h>
#include <sys/wait.h>

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <termios.h>
#include <unistd.h>

#define	KBDCONTROL	"/usr/sbin/kbdcontrol"
#define	VIDCONTROL	"/usr/sbin/vidcontrol"
#define	FONT		"/tmp/vtools-8x8.fnt"
#define	KEYMAP		"/tmp/vtools.kbd"

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("vtools: line %d: %s: ", __LINE__, #cond);	\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

/*
 * Runs argv with ttyv2 as its standard input and output (the tools act
 * on standard input); its exit status, or -1 if it did not exit.
 */
static int
run(char *const argv[])
{
	pid_t pid;
	int fd, status;

	pid = fork();
	if (pid == 0) {
		fd = open("/dev/ttyv2", O_RDWR);
		if (fd < 0 || dup2(fd, 0) < 0 || dup2(fd, 1) < 0)
			_exit(126);
		execv(argv[0], argv);
		_exit(127);
	}
	if (pid < 0 || waitpid(pid, &status, 0) != pid || !WIFEXITED(status))
		return (-1);
	return (WEXITSTATUS(status));
}

static void
put_be32(uint8_t *p, uint32_t v)
{
	p[0] = v >> 24;
	p[1] = v >> 16;
	p[2] = v >> 8;
	p[3] = v;
}

/*
 * An 8x8 font in vtfontcvt's format (sys/sys/font.h, big-endian): two
 * glyphs, a box and a bar, and a space mapped to the bar.
 */
static int
write_font(void)
{
	uint8_t f[32 + 16 + 8];
	FILE *fp;

	memset(f, 0, sizeof(f));
	memcpy(f, "VFNT0002", 8);
	f[8] = 8;			/* width */
	f[9] = 8;			/* height */
	put_be32(f + 12, 2);		/* glyphs */
	put_be32(f + 16, 1);		/* normal map entries */
	memset(f + 32, 0xff, 8);	/* glyph 0 */
	memset(f + 40, 0x18, 8);	/* glyph 1 */
	put_be32(f + 48, ' ');		/* src */
	f[52] = 0;			/* dst (big-endian 1) */
	f[53] = 1;
	fp = fopen(FONT, "w");
	if (fp == NULL)
		return (-1);
	if (fwrite(f, sizeof(f), 1, fp) != 1) {
		fclose(fp);
		return (-1);
	}
	return (fclose(fp));
}

/* A keymap of three keys, in kbdcontrol's text form (keymap(5)). */
static const char keymap_text[] =
"#                                                         alt\n"
"# scan                       cntrl          alt    alt   cntrl lock\n"
"# code  base   shift  cntrl  shift  alt    shift  cntrl  shift state\n"
"# ------------------------------------------------------------------\n"
"  000   nop    nop    nop    nop    nop    nop    nop    nop     O\n"
"  001   esc    esc    esc    esc    esc    esc    debug  esc     O\n"
"  002   '1'    '!'    nop    nop    '1'    '!'    nop    nop     O\n";

int
main(void)
{
	static keymap_t saved_map, map;
	struct winsize ws, ws8;
	int fd, saved_rep[2], rep[2];
	FILE *fp;

	if (access(KBDCONTROL, X_OK) != 0 || access(VIDCONTROL, X_OK) != 0) {
		printf("vtools: no kbdcontrol or vidcontrol, skipped\n");
		return (0);
	}
	fd = open("/dev/ttyv2", O_RDWR | O_NONBLOCK);
	if (fd < 0 && errno == ENOENT) {
		printf("vtools: no /dev/ttyv2, skipped\n");
		return (0);
	}
	CHECK(fd >= 0, "ttyv2: %s", strerror(errno));
	if (fd < 0)
		return (1);

	/* vidcontrol: the history size, colors. */
	CHECK(run((char *[]){ VIDCONTROL, "-h", "300", NULL }) == 0, "-h");
	CHECK(run((char *[]){ VIDCONTROL, "white", "blue", NULL }) == 0,
	    "colors");
	CHECK(run((char *[]){ VIDCONTROL, "white", "black", NULL }) == 0,
	    "colors back");

	/* A font from a file: the window takes its cells' size. */
	CHECK(ioctl(fd, TIOCGWINSZ, &ws) == 0, "%s", strerror(errno));
	CHECK(write_font() == 0, "%s: %s", FONT, strerror(errno));
	CHECK(run((char *[]){ VIDCONTROL, "-f", FONT, NULL }) == 0, "-f");
	CHECK(ioctl(fd, TIOCGWINSZ, &ws8) == 0 &&
	    ws8.ws_row == MIN(ws.ws_ypixel / 8, 150) &&
	    ws8.ws_col == MIN(ws.ws_xpixel / 8, 512),
	    "%dx%d after the 8x8 font on %dx%d pixels", ws8.ws_col,
	    ws8.ws_row, ws.ws_xpixel, ws.ws_ypixel);
	CHECK(run((char *[]){ VIDCONTROL, "-f", NULL }) == 0, "-f default");
	CHECK(ioctl(fd, TIOCGWINSZ, &ws8) == 0 && ws8.ws_row == ws.ws_row &&
	    ws8.ws_col == ws.ws_col, "%dx%d after the default font",
	    ws8.ws_col, ws8.ws_row);
	unlink(FONT);

	/* kbdcontrol: the keyboard's information. */
	CHECK(run((char *[]){ KBDCONTROL, "-i", NULL }) == 0, "-i");

	/* The repeat rate, in kbdmux's steps (500 ms, 126 ms). */
	CHECK(ioctl(fd, KDGETREPEAT, saved_rep) == 0, "%s", strerror(errno));
	CHECK(run((char *[]){ KBDCONTROL, "-r", "500.126", NULL }) == 0, "-r");
	CHECK(ioctl(fd, KDGETREPEAT, rep) == 0 && rep[0] == 500 &&
	    rep[1] == 126, "repeat %d.%d", rep[0], rep[1]);
	CHECK(ioctl(fd, KDSETREPEAT, saved_rep) == 0, "%s", strerror(errno));

	/* A keymap from a file, then dumped; the old one put back. */
	CHECK(ioctl(fd, GIO_KEYMAP, &saved_map) == 0, "%s", strerror(errno));
	fp = fopen(KEYMAP, "w");
	CHECK(fp != NULL && fputs(keymap_text, fp) >= 0 && fclose(fp) == 0,
	    "%s: %s", KEYMAP, strerror(errno));
	CHECK(run((char *[]){ KBDCONTROL, "-l", KEYMAP, NULL }) == 0, "-l");
	memset(&map, 0, sizeof(map));
	CHECK(ioctl(fd, GIO_KEYMAP, &map) == 0 && map.n_keys == 3 &&
	    map.key[2].map[0] == '1' && map.key[2].map[1] == '!' &&
	    map.key[1].map[0] == 0x1b, "%d keys, key 2 %#x %#x", map.n_keys,
	    map.key[2].map[0], map.key[2].map[1]);
	CHECK(run((char *[]){ KBDCONTROL, "-d", NULL }) == 0, "-d");
	CHECK(ioctl(fd, PIO_KEYMAP, &saved_map) == 0, "%s", strerror(errno));
	unlink(KEYMAP);

	close(fd);
	return (failed);
}
