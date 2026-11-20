//! Independently written, FerrumC-inspired bit-packed palette with free-slot reuse.
//! No third-party source, tables, or registry IDs are used.

use crate::wire::CELLS;
use std::mem::size_of;

#[derive(Clone)]
enum Storage {
    Single(u32),
    Indirect {
        bits: u8,
        entries: Vec<u32>,
        references: Vec<u16>,
        free: Vec<usize>,
        words: Vec<u64>,
    },
    Direct(Vec<u64>),
}

fn read_word(words: &[u64], bits: u8, cell: usize) -> u32 {
    let per_word = 64 / bits as usize;
    let shift = cell % per_word * bits as usize;
    ((words[cell / per_word] >> shift) & ((1u64 << bits) - 1)) as u32
}

fn write_word(words: &mut [u64], bits: u8, cell: usize, value: u32) {
    let per_word = 64 / bits as usize;
    let shift = cell % per_word * bits as usize;
    let mask = ((1u64 << bits) - 1) << shift;
    let word = &mut words[cell / per_word];
    *word = (*word & !mask) | (u64::from(value) << shift & mask);
}

fn pack_words(values: impl IntoIterator<Item = u32>, bits: u8) -> Vec<u64> {
    let mut words = vec![0; CELLS.div_ceil(64 / bits as usize)];
    for (cell, value) in values.into_iter().enumerate() {
        write_word(&mut words, bits, cell, value);
    }
    words
}

#[derive(Clone)]
pub struct PrototypeSection {
    storage: Storage,
    non_air: u16,
    non_opaque: u16,
    hidden: u16,
    edits: usize,
    clean_wire: bool,
}

impl PrototypeSection {
    pub fn from_cells(cells: &[u32; CELLS]) -> Self {
        let non_air = cells.iter().filter(|&&state| !crate::is_air(state)).count() as u16;
        let non_opaque = cells.iter().filter(|&&state| !crate::opaque(state)).count() as u16;
        let hidden = cells.iter().filter(|&&state| crate::hidden(state)).count() as u16;
        let first = cells[0];
        if cells.iter().all(|&state| state == first) {
            return Self {
                storage: Storage::Single(first),
                non_air,
                non_opaque,
                hidden,
                edits: 0,
                clean_wire: true,
            };
        }

        let mut entries = Vec::new();
        let mut references = Vec::<u16>::new();
        let mut indices = Vec::with_capacity(CELLS);
        for &state in cells {
            if let Some(index) = entries.iter().position(|&item| item == state) {
                references[index] += 1;
                indices.push(index as u32);
            } else if entries.len() < 256 {
                indices.push(entries.len() as u32);
                entries.push(state);
                references.push(1);
            } else {
                return Self {
                    storage: Storage::Direct(pack_words(cells.iter().copied(), 15)),
                    non_air,
                    non_opaque,
                    hidden,
                    edits: 0,
                    clean_wire: true,
                };
            }
        }
        let bits = ((usize::BITS - (entries.len() - 1).leading_zeros()) as u8).max(4);
        Self {
            storage: Storage::Indirect {
                bits,
                entries,
                references,
                free: Vec::new(),
                words: pack_words(indices, bits),
            },
            non_air,
            non_opaque,
            hidden,
            edits: 0,
            clean_wire: true,
        }
    }

    pub fn get(&self, cell: usize) -> u32 {
        assert!(cell < CELLS);
        match &self.storage {
            Storage::Single(state) => *state,
            Storage::Indirect {
                bits,
                entries,
                words,
                ..
            } => entries[read_word(words, *bits, cell) as usize],
            Storage::Direct(words) => read_word(words, 15, cell),
        }
    }

    pub fn set(&mut self, cell: usize, state: u32) {
        let old = self.get(cell);
        if old == state {
            return;
        }
        self.clean_wire = false;
        self.non_air = (i32::from(self.non_air) + i32::from(crate::is_air(old))
            - i32::from(crate::is_air(state))) as u16;
        self.non_opaque = (i32::from(self.non_opaque) + i32::from(!crate::opaque(state))
            - i32::from(!crate::opaque(old))) as u16;
        self.hidden = (i32::from(self.hidden) + i32::from(crate::hidden(state))
            - i32::from(crate::hidden(old))) as u16;

        let mut needs_growth = false;
        match &mut self.storage {
            Storage::Single(prior) => {
                let words = pack_words((0..CELLS).map(|i| u32::from(i == cell)), 4);
                self.storage = Storage::Indirect {
                    bits: 4,
                    entries: vec![*prior, state],
                    references: vec![(CELLS - 1) as u16, 1],
                    free: Vec::new(),
                    words,
                };
            }
            Storage::Indirect {
                bits,
                entries,
                references,
                free,
                words,
            } => {
                let old_index = read_word(words, *bits, cell) as usize;
                references[old_index] -= 1;
                if references[old_index] == 0 {
                    free.push(old_index);
                }
                let new_index = references
                    .iter()
                    .enumerate()
                    .find(|&(index, &count)| count != 0 && entries[index] == state)
                    .map(|(index, _)| index)
                    .or_else(|| {
                        if let Some(index) = free.pop() {
                            entries[index] = state;
                            Some(index)
                        } else if entries.len() < 1usize << *bits {
                            entries.push(state);
                            references.push(0);
                            Some(entries.len() - 1)
                        } else {
                            None
                        }
                    });
                if let Some(index) = new_index {
                    references[index] += 1;
                    write_word(words, *bits, cell, index as u32);
                } else {
                    needs_growth = true;
                }
            }
            Storage::Direct(words) => write_word(words, 15, cell, state),
        }
        if needs_growth {
            let mut cells = self.cells();
            cells[cell] = state;
            self.storage = Self::from_cells(&cells).storage;
        }
        self.edits += 1;
        // Sample periodically to amortize the 4,096-cell scan. Retire unused
        // slots and optionally demote Direct/Indirect only if resident bytes fall.
        if self.edits % 128 == 0 {
            let compact = Self::from_cells(&self.cells());
            if compact.approx_bytes() < self.approx_bytes() {
                self.storage = compact.storage;
                self.clean_wire = true;
            }
        }
    }

    pub fn cells(&self) -> [u32; CELLS] {
        std::array::from_fn(|cell| self.get(cell))
    }

    pub fn bits(&self) -> u8 {
        match &self.storage {
            Storage::Single(_) => 0,
            Storage::Indirect { bits, .. } => *bits,
            Storage::Direct(_) => 15,
        }
    }

    pub fn non_air(&self) -> u16 {
        self.non_air
    }

    pub fn write_wire(&self, biomes: &[i64; 64], out: &mut Vec<u8>) {
        if !self.clean_wire {
            crate::wire::encode(&self.cells(), biomes, out);
            return;
        }
        out.extend_from_slice(&self.non_air.to_be_bytes());
        match &self.storage {
            Storage::Single(state) => {
                out.push(0);
                crate::wire::put_varint(out, *state as u64);
                crate::wire::put_varint(out, 0);
            }
            Storage::Indirect {
                bits,
                entries,
                words,
                ..
            } => {
                out.push(*bits);
                crate::wire::put_varint(out, entries.len() as u64);
                for &entry in entries {
                    crate::wire::put_varint(out, entry as u64);
                }
                crate::wire::put_varint(out, words.len() as u64);
                for &word in words {
                    out.extend_from_slice(&word.to_be_bytes());
                }
            }
            Storage::Direct(words) => {
                out.push(15);
                crate::wire::put_varint(out, words.len() as u64);
                for &word in words {
                    out.extend_from_slice(&word.to_be_bytes());
                }
            }
        }
        crate::wire::write_biomes(out, biomes);
    }

    pub fn approx_bytes(&self) -> usize {
        size_of::<Self>()
            + match &self.storage {
                Storage::Single(_) => 0,
                Storage::Indirect {
                    entries,
                    references,
                    free,
                    words,
                    ..
                } => {
                    entries.capacity() * size_of::<u32>()
                        + references.capacity() * size_of::<u16>()
                        + free.capacity() * size_of::<usize>()
                        + words.capacity() * size_of::<u64>()
                }
                Storage::Direct(words) => words.capacity() * size_of::<u64>(),
            }
    }

    pub fn assert_metadata(&self, cells: &[u32; CELLS]) {
        assert_eq!(
            self.non_air as usize,
            cells.iter().filter(|&&s| !crate::is_air(s)).count()
        );
        assert_eq!(
            self.non_opaque as usize,
            cells.iter().filter(|&&s| !crate::opaque(s)).count()
        );
        assert_eq!(
            self.hidden as usize,
            cells.iter().filter(|&&s| crate::hidden(s)).count()
        );
    }
}
