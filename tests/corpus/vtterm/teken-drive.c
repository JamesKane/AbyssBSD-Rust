/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * Runs pseudo-random byte streams through FreeBSD's teken and prints a
 * digest of each final screen and the cursor: teken-digests.txt, which
 * lib/vtterm's teken_differential test compares with its own emulator
 * on the same streams.
 *
 * To regenerate, on FreeBSD with sys/teken copied beside this file:
 *	awk -f gensequences sequences > teken_state.h
 *	cc -O1 -o drive teken-drive.c teken.c && ./drive > teken-digests.txt
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "teken.h"

static uint32_t *cells;
static int rows, cols;
static teken_pos_t cur;

static uint8_t to16(uint8_t c) { return c >= 16 ? teken_256to16(c) : c; }
static uint32_t pack(teken_char_t c, const teken_attr_t *a)
{
	uint8_t fg = to16(a->ta_fgcolor), bg = to16(a->ta_bgcolor);
	return (c & 0x1fffff) | ((uint32_t)(a->ta_format & 0x1f) << 21) |
	    ((uint32_t)(fg & 7) << 26) | ((uint32_t)(fg & 8) << 18) |
	    ((uint32_t)(bg & 7) << 29) | ((uint32_t)(bg & 8) << 20);
}
static void t_bell(void *s) { (void)s; }
static void t_cursor(void *s, const teken_pos_t *p) { (void)s; cur = *p; }
static void t_putchar(void *s, const teken_pos_t *p, teken_char_t c, const teken_attr_t *a)
{ (void)s; cells[p->tp_row * cols + p->tp_col] = pack(c, a); }
static void t_fill(void *s, const teken_rect_t *r, teken_char_t c, const teken_attr_t *a)
{
	(void)s;
	for (int i = r->tr_begin.tp_row; i < r->tr_end.tp_row; i++)
		for (int j = r->tr_begin.tp_col; j < r->tr_end.tp_col; j++)
			cells[i * cols + j] = pack(c, a);
}
static void t_copy(void *s, const teken_rect_t *r, const teken_pos_t *p)
{
	(void)s;
	int n = r->tr_end.tp_row - r->tr_begin.tp_row;
	int w = r->tr_end.tp_col - r->tr_begin.tp_col;
	if (p->tp_row < r->tr_begin.tp_row) {
		for (int i = 0; i < n; i++)
			memmove(&cells[(p->tp_row + i) * cols + p->tp_col],
			    &cells[(r->tr_begin.tp_row + i) * cols + r->tr_begin.tp_col], w * 4);
	} else {
		for (int i = n - 1; i >= 0; i--)
			memmove(&cells[(p->tp_row + i) * cols + p->tp_col],
			    &cells[(r->tr_begin.tp_row + i) * cols + r->tr_begin.tp_col], w * 4);
	}
}
static void t_param(void *s, int c, unsigned int v) { (void)s; (void)c; (void)v; }
static void t_respond(void *s, const void *b, size_t l) { (void)s; (void)b; (void)l; }
static const teken_funcs_t funcs = { t_bell, t_cursor, t_putchar, t_fill, t_copy,
    NULL, NULL, t_param, t_respond };

static uint64_t x = 0x0123456789abcdefULL;
static uint64_t rnd(void) { x ^= x << 13; x ^= x >> 7; x ^= x << 17; return x; }

static const char *pieces[] = {
	"\x1b[", "\x1b[?", "\x1b[=", "\x1b[>", "\x1b[ ", "\x1b#", "\x1b(", "\x1b)", "\x1b",
	";", "1", "2", "3", "4", "7", "25", "0", "99", "\r", "\n", "\t", "\b", "\x0e", "\x0f",
	"\xe6\xbc\xa2", "\xcc\x81", "\xe2", "\x80", "\xff", "\x1b]", "\x07",
	"m", "H", "J", "K", "r", "h", "l", "@", "P", "L", "M", "X", "b", "S", "T", "Z", "I",
	"g", "d", "G", "A", "B", "C", "D", "E", "F", "s", "u", "7", "8", "c", "x",
};
static const char letters[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz@`~#q";

int main(void)
{
	int npieces = sizeof(pieces) / sizeof(pieces[0]);
	for (int round = 0; round < 1000; round++) {
		teken_t t;
		rows = rnd() % 30 + 1;
		cols = rnd() % 98 + 3;
		cells = calloc(rows * cols, 4);
		teken_attr_t normal = { 0, TC_WHITE, TC_BLACK };
		teken_init(&t, &funcs, NULL);
		teken_set_defattr(&t, &normal);
		teken_pos_t sz = { rows, cols };
		teken_set_winsize(&t, &sz);
		cur.tp_row = cur.tp_col = 0;
		for (int i = 0; i < rows * cols; i++)
			cells[i] = pack(' ', &normal);
		int len = rnd() % 3000;
		char *b = malloc(len * 4 + 16);
		int n = 0;
		for (int i = 0; i < len; i++) {
			if (rnd() % 2 == 0) {
				const char *p = pieces[rnd() % npieces];
				size_t l = strlen(p);
				memcpy(b + n, p, l);
				n += l;
			} else
				b[n++] = letters[rnd() % (sizeof(letters) - 1)];
		}
		teken_input(&t, b, n);
		uint64_t h = 0xcbf29ce484222325ULL;
		for (int i = 0; i < rows * cols; i++)
			for (int k = 0; k < 4; k++) {
				h ^= (cells[i] >> (8 * k)) & 0xff;
				h *= 0x100000001b3ULL;
			}
		const teken_pos_t *c = teken_get_cursor(&t);
		printf("%d %d %d %u %u %016llx\n", round, rows, cols, c->tp_row,
		    c->tp_col, (unsigned long long)h);
		free(b);
		free(cells);
	}
	return 0;
}
