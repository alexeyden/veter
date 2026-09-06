use crate::term::BufWrite as _;

#[derive(Clone, Debug)]
pub struct Row {
    cells: Vec<crate::Cell>,
    wrapped: bool,
}

impl Row {
    pub fn new(cols: u16) -> Self {
        Self {
            cells: vec![crate::Cell::new(); usize::from(cols)],
            wrapped: false,
        }
    }

    fn cols(&self) -> u16 {
        self.cells
            .len()
            .try_into()
            // we limit the number of cols to a u16 (see Size)
            .unwrap()
    }

    pub fn clear(&mut self, attrs: crate::attrs::Attrs) {
        for cell in &mut self.cells {
            cell.clear(attrs);
        }
        self.wrapped = false;
    }

    fn cells(&self) -> impl Iterator<Item = &crate::Cell> {
        self.cells.iter()
    }

    /// This row's cells as a slice, for consumers that read a row end
    /// to end and want O(1) indexing rather than one `get` per cell.
    pub fn cells_slice(&self) -> &[crate::Cell] {
        &self.cells
    }

    pub fn get(&self, col: u16) -> Option<&crate::Cell> {
        self.cells.get(usize::from(col))
    }

    pub fn get_mut(&mut self, col: u16) -> Option<&mut crate::Cell> {
        self.cells.get_mut(usize::from(col))
    }

    pub fn insert(&mut self, i: u16, cell: crate::Cell) {
        self.cells.insert(usize::from(i), cell);
        self.wrapped = false;
    }

    pub fn remove(&mut self, i: u16) {
        self.clear_wide(i);
        self.cells.remove(usize::from(i));
        self.wrapped = false;
    }

    pub fn erase(&mut self, i: u16, attrs: crate::attrs::Attrs) {
        let wide = self.cells[usize::from(i)].is_wide();
        self.clear_wide(i);
        self.cells[usize::from(i)].clear(attrs);
        if i == self.cols() - if wide { 2 } else { 1 } {
            self.wrapped = false;
        }
    }

    pub fn truncate(&mut self, len: u16) {
        self.cells.truncate(usize::from(len));
        self.wrapped = false;
        self.repair_trailing_wide();
    }

    pub fn resize(&mut self, len: u16, cell: crate::Cell) {
        self.cells.resize(usize::from(len), cell);
        self.wrapped = false;
        self.repair_trailing_wide();
    }

    /// Clear a wide character's head if it ended up in the last column
    /// with its continuation cut off.
    ///
    /// A width shrink truncates every row, and the cut can land between
    /// the two halves of a wide character. The rest of the grid assumes
    /// a head always has a cell after it — [`Self::clear_wide`] and the
    /// wide-write path in `Screen::text` both step to `col + 1` — so an
    /// orphaned head is a latent panic reachable from ordinary program
    /// output after a window resize. `truncate` has always repaired it;
    /// `resize`, which is what `Grid::set_size` actually calls, did not.
    fn repair_trailing_wide(&mut self) {
        if let Some(last) = self.cells.last_mut() {
            if last.is_wide() {
                last.clear(*last.attrs());
            }
        }
    }

    pub fn wrap(&mut self, wrap: bool) {
        self.wrapped = wrap;
    }

    pub fn wrapped(&self) -> bool {
        self.wrapped
    }

    pub(crate) fn serialize_binary(&self, w: &mut crate::snapshot::Writer) {
        w.varu(self.cells.len() as u64);
        for c in &self.cells {
            c.serialize_binary(w);
        }
        w.bool(self.wrapped);
    }

    pub(crate) fn deserialize_binary(
        r: &mut crate::snapshot::Reader,
    ) -> Result<Self, crate::snapshot::SnapshotError> {
        let n = r.varu()? as usize;
        // Cap at u16::MAX since Row::cols() is u16 elsewhere, and at
        // what is left of the payload: a cell is at least one byte on
        // the wire, so a count past that can only be corruption. The
        // second bound is what keeps `with_capacity` from aborting the
        // process on an absurd length — an abort no `Result` can catch.
        if n > u16::MAX as usize || n > r.remaining() {
            return Err(crate::snapshot::SnapshotError::bad_payload(
                "implausible row cell count",
            ));
        }
        let mut cells = Vec::with_capacity(n);
        for _ in 0..n {
            cells.push(crate::cell::Cell::deserialize_binary(r)?);
        }
        let wrapped = r.bool()?;
        let mut row = Self { cells, wrapped };
        // A row whose last cell is a wide head has no continuation to
        // go with it. Repair rather than reject: the sender may simply
        // be a build whose resize path predates
        // `repair_trailing_wide`, and one unrepresentable glyph is not
        // a reason to throw away a whole session's state.
        row.repair_trailing_wide();
        Ok(row)
    }

    /// Clear the other half of the wide character at `col`, if there is
    /// one.
    ///
    /// Every index here is bounds-checked. [`Self::repair_trailing_wide`]
    /// keeps a head from being orphaned in the last column in the first
    /// place, but this is also reached from a restored snapshot, whose
    /// bytes come off the wire from a possibly different build — and the
    /// cost of being wrong is the whole terminal process, every portal
    /// in it included.
    pub fn clear_wide(&mut self, col: u16) {
        let idx = usize::from(col);
        let (wide, continuation) = match self.cells.get(idx) {
            Some(cell) => (cell.is_wide(), cell.is_wide_continuation()),
            None => return,
        };
        let other_idx = if wide {
            idx + 1
        } else if continuation {
            match idx.checked_sub(1) {
                Some(i) => i,
                None => return,
            }
        } else {
            return;
        };
        if let Some(other) = self.cells.get_mut(other_idx) {
            other.clear(*other.attrs());
        }
    }

    pub fn write_contents(
        &self,
        contents: &mut String,
        start: u16,
        width: u16,
        wrapping: bool,
    ) {
        let mut prev_was_wide = false;

        let mut prev_col = start;
        for (col, cell) in self
            .cells()
            .enumerate()
            .skip(usize::from(start))
            .take(usize::from(width))
        {
            if prev_was_wide {
                prev_was_wide = false;
                continue;
            }
            prev_was_wide = cell.is_wide();

            // we limit the number of cols to a u16 (see Size)
            let col: u16 = col.try_into().unwrap();
            if cell.has_contents() {
                for _ in 0..(col - prev_col) {
                    contents.push(' ');
                }
                prev_col += col - prev_col;

                contents.push_str(cell.contents());
                prev_col += if cell.is_wide() { 2 } else { 1 };
            }
        }
        if prev_col == start && wrapping {
            contents.push('\n');
        }
    }

    pub fn write_contents_formatted(
        &self,
        contents: &mut Vec<u8>,
        start: u16,
        width: u16,
        row: u16,
        wrapping: bool,
        prev_pos: Option<crate::grid::Pos>,
        prev_attrs: Option<crate::attrs::Attrs>,
    ) -> (crate::grid::Pos, crate::attrs::Attrs) {
        let mut prev_was_wide = false;
        let default_cell = crate::Cell::new();

        let mut prev_pos = prev_pos.unwrap_or_else(|| {
            if wrapping {
                crate::grid::Pos {
                    row: row - 1,
                    col: self.cols(),
                }
            } else {
                crate::grid::Pos { row, col: start }
            }
        });
        let mut prev_attrs = prev_attrs.unwrap_or_default();

        let first_cell = &self.cells[usize::from(start)];
        if wrapping && first_cell == &default_cell {
            let default_attrs = default_cell.attrs();
            if &prev_attrs != default_attrs {
                default_attrs.write_escape_code_diff(contents, &prev_attrs);
                prev_attrs = *default_attrs;
            }
            contents.push(b' ');
            crate::term::Backspace.write_buf(contents);
            crate::term::EraseChar::new(1).write_buf(contents);
            prev_pos = crate::grid::Pos { row, col: 0 };
        }

        let mut erase: Option<(u16, &crate::attrs::Attrs)> = None;
        for (col, cell) in self
            .cells()
            .enumerate()
            .skip(usize::from(start))
            .take(usize::from(width))
        {
            if prev_was_wide {
                prev_was_wide = false;
                continue;
            }
            prev_was_wide = cell.is_wide();

            // we limit the number of cols to a u16 (see Size)
            let col: u16 = col.try_into().unwrap();
            let pos = crate::grid::Pos { row, col };

            if let Some((prev_col, attrs)) = erase {
                if cell.has_contents() || cell.attrs() != attrs {
                    let new_pos = crate::grid::Pos { row, col: prev_col };
                    if wrapping
                        && prev_pos.row + 1 == new_pos.row
                        && prev_pos.col >= self.cols()
                    {
                        if new_pos.col > 0 {
                            contents.extend(
                                " ".repeat(usize::from(new_pos.col))
                                    .as_bytes(),
                            );
                        } else {
                            contents.extend(b" ");
                            crate::term::Backspace.write_buf(contents);
                        }
                    } else {
                        crate::term::MoveFromTo::new(prev_pos, new_pos)
                            .write_buf(contents);
                    }
                    prev_pos = new_pos;
                    if &prev_attrs != attrs {
                        attrs.write_escape_code_diff(contents, &prev_attrs);
                        prev_attrs = *attrs;
                    }
                    crate::term::EraseChar::new(pos.col - prev_col)
                        .write_buf(contents);
                    erase = None;
                }
            }

            if cell != &default_cell {
                let attrs = cell.attrs();
                if cell.has_contents() {
                    if pos != prev_pos {
                        if !wrapping
                            || prev_pos.row + 1 != pos.row
                            || prev_pos.col
                                < self.cols() - u16::from(cell.is_wide())
                            || pos.col != 0
                        {
                            crate::term::MoveFromTo::new(prev_pos, pos)
                                .write_buf(contents);
                        }
                        prev_pos = pos;
                    }

                    if &prev_attrs != attrs {
                        attrs.write_escape_code_diff(contents, &prev_attrs);
                        prev_attrs = *attrs;
                    }

                    prev_pos.col += if cell.is_wide() { 2 } else { 1 };
                    let cell_contents = cell.contents();
                    contents.extend(cell_contents.as_bytes());
                } else if erase.is_none() {
                    erase = Some((pos.col, attrs));
                }
            }
        }
        if let Some((prev_col, attrs)) = erase {
            let new_pos = crate::grid::Pos { row, col: prev_col };
            if wrapping
                && prev_pos.row + 1 == new_pos.row
                && prev_pos.col >= self.cols()
            {
                if new_pos.col > 0 {
                    contents.extend(
                        " ".repeat(usize::from(new_pos.col)).as_bytes(),
                    );
                } else {
                    contents.extend(b" ");
                    crate::term::Backspace.write_buf(contents);
                }
            } else {
                crate::term::MoveFromTo::new(prev_pos, new_pos)
                    .write_buf(contents);
            }
            prev_pos = new_pos;
            if &prev_attrs != attrs {
                attrs.write_escape_code_diff(contents, &prev_attrs);
                prev_attrs = *attrs;
            }
            crate::term::ClearRowForward.write_buf(contents);
        }

        (prev_pos, prev_attrs)
    }

    // while it's true that most of the logic in this is identical to
    // write_contents_formatted, i can't figure out how to break out the
    // common parts without making things noticeably slower.
    pub fn write_contents_diff(
        &self,
        contents: &mut Vec<u8>,
        prev: &Self,
        start: u16,
        width: u16,
        row: u16,
        wrapping: bool,
        prev_wrapping: bool,
        mut prev_pos: crate::grid::Pos,
        mut prev_attrs: crate::attrs::Attrs,
    ) -> (crate::grid::Pos, crate::attrs::Attrs) {
        let mut prev_was_wide = false;

        let first_cell = &self.cells[usize::from(start)];
        let prev_first_cell = &prev.cells[usize::from(start)];
        if wrapping
            && !prev_wrapping
            && first_cell == prev_first_cell
            && prev_pos.row + 1 == row
            && prev_pos.col
                >= self.cols() - u16::from(prev_first_cell.is_wide())
        {
            let first_cell_attrs = first_cell.attrs();
            if &prev_attrs != first_cell_attrs {
                first_cell_attrs
                    .write_escape_code_diff(contents, &prev_attrs);
                prev_attrs = *first_cell_attrs;
            }
            let mut cell_contents = prev_first_cell.contents();
            let need_erase = if cell_contents.is_empty() {
                cell_contents = " ";
                true
            } else {
                false
            };
            contents.extend(cell_contents.as_bytes());
            crate::term::Backspace.write_buf(contents);
            if prev_first_cell.is_wide() {
                crate::term::Backspace.write_buf(contents);
            }
            if need_erase {
                crate::term::EraseChar::new(1).write_buf(contents);
            }
            prev_pos = crate::grid::Pos { row, col: 0 };
        }

        let mut erase: Option<(u16, &crate::attrs::Attrs)> = None;
        for (col, (cell, prev_cell)) in self
            .cells()
            .zip(prev.cells())
            .enumerate()
            .skip(usize::from(start))
            .take(usize::from(width))
        {
            if prev_was_wide {
                prev_was_wide = false;
                continue;
            }
            prev_was_wide = cell.is_wide();

            // we limit the number of cols to a u16 (see Size)
            let col: u16 = col.try_into().unwrap();
            let pos = crate::grid::Pos { row, col };

            if let Some((prev_col, attrs)) = erase {
                if cell.has_contents() || cell.attrs() != attrs {
                    let new_pos = crate::grid::Pos { row, col: prev_col };
                    if wrapping
                        && prev_pos.row + 1 == new_pos.row
                        && prev_pos.col >= self.cols()
                    {
                        if new_pos.col > 0 {
                            contents.extend(
                                " ".repeat(usize::from(new_pos.col))
                                    .as_bytes(),
                            );
                        } else {
                            contents.extend(b" ");
                            crate::term::Backspace.write_buf(contents);
                        }
                    } else {
                        crate::term::MoveFromTo::new(prev_pos, new_pos)
                            .write_buf(contents);
                    }
                    prev_pos = new_pos;
                    if &prev_attrs != attrs {
                        attrs.write_escape_code_diff(contents, &prev_attrs);
                        prev_attrs = *attrs;
                    }
                    crate::term::EraseChar::new(pos.col - prev_col)
                        .write_buf(contents);
                    erase = None;
                }
            }

            if cell != prev_cell {
                let attrs = cell.attrs();
                if cell.has_contents() {
                    if pos != prev_pos {
                        if !wrapping
                            || prev_pos.row + 1 != pos.row
                            || prev_pos.col
                                < self.cols() - u16::from(cell.is_wide())
                            || pos.col != 0
                        {
                            crate::term::MoveFromTo::new(prev_pos, pos)
                                .write_buf(contents);
                        }
                        prev_pos = pos;
                    }

                    if &prev_attrs != attrs {
                        attrs.write_escape_code_diff(contents, &prev_attrs);
                        prev_attrs = *attrs;
                    }

                    prev_pos.col += if cell.is_wide() { 2 } else { 1 };
                    contents.extend(cell.contents().as_bytes());
                } else if erase.is_none() {
                    erase = Some((pos.col, attrs));
                }
            }
        }
        if let Some((prev_col, attrs)) = erase {
            let new_pos = crate::grid::Pos { row, col: prev_col };
            if wrapping
                && prev_pos.row + 1 == new_pos.row
                && prev_pos.col >= self.cols()
            {
                if new_pos.col > 0 {
                    contents.extend(
                        " ".repeat(usize::from(new_pos.col)).as_bytes(),
                    );
                } else {
                    contents.extend(b" ");
                    crate::term::Backspace.write_buf(contents);
                }
            } else {
                crate::term::MoveFromTo::new(prev_pos, new_pos)
                    .write_buf(contents);
            }
            prev_pos = new_pos;
            if &prev_attrs != attrs {
                attrs.write_escape_code_diff(contents, &prev_attrs);
                prev_attrs = *attrs;
            }
            crate::term::ClearRowForward.write_buf(contents);
        }

        // if this row is going from wrapped to not wrapped, we need to erase
        // and redraw the last character to break wrapping. if this row is
        // wrapped, we need to redraw the last character without erasing it to
        // position the cursor after the end of the line correctly so that
        // drawing the next line can just start writing and be wrapped.
        if (!self.wrapped && prev.wrapped) || (!prev.wrapped && self.wrapped)
        {
            let end_pos = if self.cells[usize::from(self.cols() - 1)]
                .is_wide_continuation()
            {
                crate::grid::Pos {
                    row,
                    col: self.cols() - 2,
                }
            } else {
                crate::grid::Pos {
                    row,
                    col: self.cols() - 1,
                }
            };
            crate::term::MoveFromTo::new(prev_pos, end_pos)
                .write_buf(contents);
            prev_pos = end_pos;
            if !self.wrapped {
                crate::term::EraseChar::new(1).write_buf(contents);
            }
            let end_cell = &self.cells[usize::from(end_pos.col)];
            if end_cell.has_contents() {
                let attrs = end_cell.attrs();
                if &prev_attrs != attrs {
                    attrs.write_escape_code_diff(contents, &prev_attrs);
                    prev_attrs = *attrs;
                }
                contents.extend(end_cell.contents().as_bytes());
                prev_pos.col += if end_cell.is_wide() { 2 } else { 1 };
            }
        }

        (prev_pos, prev_attrs)
    }
}
