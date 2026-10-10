//! Chapter 5: the tape — 4096 wrapping byte cells.

/// How many cells the tape owns.
pub const CELLS: usize = 4096;

/// The tape state: cells plus a pointer.
#[derive(Debug)]
pub struct Tape {
    cells: Vec<u8>,
    pointer: usize,
}

/// Ways a move can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapeError {
    Overflow,
    Underflow,
}

impl Tape {
    /// A fresh tape: pointer at cell zero, every cell zero.
    pub fn new() -> Self {
        Self {
            cells: vec![0; CELLS],
            pointer: 0,
        }
    }

    /// The pointer position, zero-based.
    pub fn pointer(&self) -> usize {
        self.pointer
    }

    /// The byte under the pointer.
    pub fn value(&self) -> u8 {
        self.cells[self.pointer]
    }

    /// Moves right; the last cell has no right neighbor.
    pub fn next(&mut self) -> Result<(), TapeError> {
        if self.pointer + 1 >= CELLS {
            return Err(TapeError::Overflow);
        }
        self.pointer += 1;
        Ok(())
    }

    /// Moves left; cell zero has no left neighbor.
    pub fn prev(&mut self) -> Result<(), TapeError> {
        if self.pointer == 0 {
            return Err(TapeError::Underflow);
        }
        self.pointer -= 1;
        Ok(())
    }

    /// Increments the current cell, wrapping modulo 256.
    pub fn inc(&mut self) {
        let cell = &mut self.cells[self.pointer];
        *cell = cell.wrapping_add(1);
    }

    /// Decrements the current cell, wrapping modulo 256.
    pub fn dec(&mut self) {
        let cell = &mut self.cells[self.pointer];
        *cell = cell.wrapping_sub(1);
    }

    /// Overwrites the current cell.
    pub fn write_value(&mut self, byte: u8) {
        self.cells[self.pointer] = byte;
    }
}

impl Default for Tape {
    fn default() -> Self {
        Self::new()
    }
}
