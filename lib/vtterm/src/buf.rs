// SPDX-License-Identifier: BSD-2-Clause
//! The screen buffer (`vt_buf`): a ring of history rows, the screen
//! being the last `size.row` of them. A scroll of the whole screen
//! moves the ring instead of the cells, and the rows it uncovers are
//! the oldest history, which `teken` then blanks. In scroll mode the
//! view stays where it was while output goes on below it.
//!
//! Damage is kept per screen row, as the columns changed since the last
//! [`Buffer::take_damage`].
//!
//! Behavior reference: sys/dev/vt/vt_buf.c.

use alloc::vec;
use alloc::vec::Vec;

use crate::{Attr, Cell, Pos, Rect};

/// `VBF_DEFAULT_HISTORY_SIZE`: rows kept, the screen's among them.
pub const DEFAULT_HISTORY: u32 = 500;

/// Where [`Buffer::seek`] counts from (`VHS_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seek {
    /// From the oldest row.
    Set,
    /// From the view.
    Cur,
    /// To the screen.
    End,
}

/// The screen and its history (`struct vt_buf`).
#[derive(Clone, Debug)]
pub struct Buffer {
    size: Pos,
    history_size: u32,
    /// `history_size` rows of `size.col` cells.
    cells: Vec<Cell>,
    /// The ring row the screen starts at.
    curroffset: u32,
    /// The ring row the view starts at.
    roffset: u32,
    history_full: bool,
    cursor: Pos,
    cursor_visible: bool,
    scroll_mode: bool,
    /// Per screen row, the columns changed: `begin..end`, empty if none.
    damage: Vec<(u32, u32)>,
}

impl Buffer {
    /// A buffer of `size` with `history` rows, blank in `a`'s colors
    /// (`vtbuf_init`).
    #[must_use]
    pub fn new(size: Pos, history: u32, a: &Attr) -> Self {
        let size = Pos::new(size.row.max(1), size.col.max(1));
        let history_size = history.max(size.row);
        let mut b = Self {
            size,
            history_size,
            cells: vec![0; ix(history_size) * ix(size.col)],
            curroffset: 0,
            roffset: 0,
            history_full: false,
            cursor: Pos::default(),
            cursor_visible: true,
            scroll_mode: false,
            damage: vec![(0, 0); ix(size.row)],
        };
        b.cells.fill(space(a));
        b
    }

    #[must_use]
    pub fn size(&self) -> Pos {
        self.size
    }

    #[must_use]
    pub fn history_size(&self) -> u32 {
        self.history_size
    }

    #[must_use]
    pub fn cursor(&self) -> Pos {
        self.cursor
    }

    #[must_use]
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    #[must_use]
    pub fn scroll_mode(&self) -> bool {
        self.scroll_mode
    }

    /// Whether the cursor is drawn at `p` of the view (`vtbuf_iscursor`):
    /// not while scrolled back.
    #[must_use]
    pub fn is_cursor(&self, p: Pos) -> bool {
        self.cursor_visible && !self.scroll_mode && self.cursor == p
    }

    fn row_start(&self, ring_row: u32) -> usize {
        ix(ring_row % self.history_size) * ix(self.size.col)
    }

    /// Row `r` of the screen.
    #[must_use]
    pub fn screen_row(&self, r: u32) -> &[Cell] {
        let s = self.row_start(self.curroffset + r);
        &self.cells[s..s + ix(self.size.col)]
    }

    fn screen_row_mut(&mut self, r: u32) -> &mut [Cell] {
        let s = self.row_start(self.curroffset + r);
        let cols = ix(self.size.col);
        &mut self.cells[s..s + cols]
    }

    /// Row `r` of the view, which is the screen unless scrolled back
    /// (`VTBUF_GET_ROW`).
    #[must_use]
    pub fn view_row(&self, r: u32) -> &[Cell] {
        let s = self.row_start(self.roffset + r);
        &self.cells[s..s + ix(self.size.col)]
    }

    /// Whether the view is scrolled back from the screen.
    #[must_use]
    pub fn scrolled_back(&self) -> bool {
        self.roffset != self.curroffset
    }

    /// Marks `r` (screen coordinates) changed (`vtbuf_dirty`).
    pub fn dirty(&mut self, r: Rect) {
        let end_row = r.end.row.min(self.size.row);
        let end_col = r.end.col.min(self.size.col);
        for row in r.begin.row..end_row {
            let d = &mut self.damage[ix(row)];
            if d.0 >= d.1 {
                *d = (r.begin.col, end_col);
            } else {
                *d = (d.0.min(r.begin.col), d.1.max(end_col));
            }
        }
    }

    /// Marks the whole screen changed.
    pub fn dirty_all(&mut self) {
        self.damage.fill((0, self.size.col));
    }

    fn dirty_cell(&mut self, p: Pos) {
        if p.row < self.size.row && p.col < self.size.col {
            self.dirty(Rect::new(p, Pos::new(p.row + 1, p.col + 1)));
        }
    }

    /// Calls `f` with each changed row and its columns, and forgets them
    /// (`vtbuf_undirty`).
    pub fn take_damage(&mut self, mut f: impl FnMut(u32, u32, u32)) {
        for (row, d) in (0u32..).zip(self.damage.iter_mut()) {
            if d.0 < d.1 {
                f(row, d.0, d.1);
            }
            *d = (0, 0);
        }
    }

    /// Whether anything changed since the last [`Buffer::take_damage`].
    #[must_use]
    pub fn damaged(&self) -> bool {
        self.damage.iter().any(|d| d.0 < d.1)
    }

    /// Writes one cell (`vtbuf_putchar`).
    pub fn putchar(&mut self, p: Pos, c: Cell) {
        if p.row >= self.size.row || p.col >= self.size.col {
            return;
        }
        let cell = &mut self.screen_row_mut(p.row)[ix(p.col)];
        if *cell != c {
            *cell = c;
            self.dirty_cell(p);
        }
    }

    /// Fills `r` with `c` (`vtbuf_fill`).
    pub fn fill(&mut self, r: Rect, c: Cell) {
        let r = self.clamp(r);
        for row in r.begin.row..r.end.row {
            self.screen_row_mut(row)[ix(r.begin.col)..ix(r.end.col)].fill(c);
        }
        self.dirty(r);
    }

    fn clamp(&self, r: Rect) -> Rect {
        let end = Pos::new(r.end.row.min(self.size.row), r.end.col.min(self.size.col));
        Rect::new(
            Pos::new(r.begin.row.min(end.row), r.begin.col.min(end.col)),
            end,
        )
    }

    /// Copies `r` to `p` (`vtbuf_copy`). Copying the whole screen up
    /// moves the ring, adding the rows to history.
    pub fn copy(&mut self, r: Rect, p: Pos) {
        let r = self.clamp(r);
        if p.row >= self.size.row || p.col >= self.size.col {
            return;
        }
        let rows = (r.end.row - r.begin.row).min(self.size.row - p.row);
        let cols = (r.end.col - r.begin.col).min(self.size.col - p.col);
        if rows == 0 || cols == 0 {
            return;
        }
        if r.begin.row > p.row
            && r.begin.col == 0
            && r.end.col == self.size.col
            && rows + (r.begin.row - p.row) == self.size.row
        {
            self.add_lines(r.begin.row - p.row);
        } else {
            let (from_col, to_col, n) = (ix(r.begin.col), ix(p.col), ix(cols));
            let move_row = |b: &mut Self, i: u32| {
                let from = b.row_start(b.curroffset + r.begin.row + i) + from_col;
                let to = b.row_start(b.curroffset + p.row + i) + to_col;
                b.cells.copy_within(from..from + n, to);
            };
            if p.row < r.begin.row {
                for i in 0..rows {
                    move_row(self, i);
                }
            } else {
                for i in (0..rows).rev() {
                    move_row(self, i);
                }
            }
        }
        self.dirty(Rect::new(p, Pos::new(p.row + rows, p.col + cols)));
    }

    /// Moves the screen down the ring by `n` rows (`vthistory_addlines`).
    fn add_lines(&mut self, n: u32) {
        self.curroffset += n;
        if self.curroffset + self.size.row >= self.history_size {
            self.history_full = true;
            self.curroffset %= self.history_size;
        }
        if !self.scroll_mode {
            self.roffset = self.curroffset;
        }
    }

    /// Moves the cursor (`vtbuf_cursor_position`).
    pub fn cursor_position(&mut self, p: Pos) {
        if self.cursor_visible {
            self.dirty_cell(self.cursor);
            self.cursor = p;
            self.dirty_cell(p);
        } else {
            self.cursor = p;
        }
    }

    /// Shows or hides the cursor (`vtbuf_cursor_visibility`).
    pub fn cursor_visibility(&mut self, yes: bool) {
        if self.cursor_visible != yes {
            self.cursor_visible = yes;
            self.dirty_cell(self.cursor);
        }
    }

    /// Enters or leaves scroll mode (`vtbuf_scroll_mode`).
    pub fn set_scroll_mode(&mut self, yes: bool) {
        if self.scroll_mode != yes {
            self.scroll_mode = yes;
            self.dirty_cell(self.cursor);
        }
    }

    /// Moves the view (`vthistory_seek`); whether it moved. Outside
    /// scroll mode the view goes back to the screen.
    pub fn seek(&mut self, offset: i32, whence: Seek) -> bool {
        if !self.scroll_mode {
            if self.roffset != self.curroffset {
                self.roffset = self.curroffset;
                return true;
            }
            return false;
        }
        let hs = i64::from(self.history_size);
        let bottom = i64::from(self.curroffset);
        // The oldest row, which may come before ring row 0.
        let top = if self.history_full {
            bottom + i64::from(self.size.row) - hs
        } else {
            0
        };
        let roffset = match whence {
            Seek::Set => top + i64::from(offset.max(0)),
            Seek::Cur => {
                let mut r = i64::from(self.roffset);
                if r >= bottom + i64::from(self.size.row) {
                    r -= hs;
                }
                let mut r = (r + i64::from(offset)).clamp(top, bottom);
                if r < 0 {
                    r += hs;
                }
                r
            }
            Seek::End => bottom,
        };
        let roffset = u32::try_from(roffset.rem_euclid(hs)).unwrap_or(0);
        let moved = self.roffset != roffset;
        self.roffset = roffset;
        moved
    }

    /// The view's ring row (`vthistory_getpos`).
    #[must_use]
    pub fn view_offset(&self) -> u32 {
        self.roffset
    }

    /// Blanks the history and puts the screen at its start
    /// (`vtbuf_clearhistory`).
    pub fn clear_history(&mut self, a: &Attr) {
        self.cells.fill(space(a));
        self.roffset = 0;
        self.curroffset = 0;
        self.history_full = false;
        self.dirty_all();
    }

    /// Resizes the screen to `size` with `history` rows, keeping the
    /// newest rows (`vtbuf_grow`); new cells are blanks in `a`'s colors.
    pub fn grow(&mut self, size: Pos, history: u32, a: &Attr) {
        let size = Pos::new(size.row.max(1), size.col.max(1));
        let history_size = history.max(size.row);
        let blank = space(a);
        let (w, h, old_hs) = (self.size.col, self.size.row, self.history_size);
        let full = self.history_full || self.curroffset + h >= history_size;
        let keep = ix(size.col.min(w));
        let mut cells = vec![blank; ix(history_size) * ix(size.col)];
        let copy_rows = history_size.min(old_hs);
        // Grown, the old rows come first; shrunk, the oldest are dropped.
        let skip = old_hs.saturating_sub(history_size);
        for r in 0..copy_rows {
            let old = if full {
                (self.curroffset + h + r + skip) % old_hs
            } else {
                r
            };
            let from = self.row_start(old);
            let to = ix(r) * ix(size.col);
            cells[to..to + keep].copy_from_slice(&self.cells[from..from + keep]);
        }
        self.cells = cells;
        self.history_size = history_size;
        self.size = size;
        if history_size > old_hs {
            self.history_full = false;
            if full {
                self.curroffset = old_hs - h;
            }
        } else if full {
            self.curroffset = history_size - h.min(history_size);
            self.history_full = true;
        }
        self.roffset = self.curroffset;
        if self.cursor.col > size.col - 1 {
            self.cursor.col = size.col - 1;
        }
        if self.curroffset > 0 || self.cursor.row > size.row - 1 {
            self.cursor.row = size.row - 1;
        }
        self.damage = vec![(0, size.col); ix(size.row)];
    }
}

/// A blank in `a`'s colors, without its format (`VTBUF_SPACE_CHAR`).
fn space(a: &Attr) -> Cell {
    u32::from(b' ') | (u32::from(a.fg & 7) << 26) | (u32::from(a.bg & 7) << 29)
}

fn ix(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
