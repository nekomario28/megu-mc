// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
use crate::{Error, SECTION_CELLS, STATE_COUNT};

#[allow(dead_code, clippy::all)]
mod owner {
    include!(concat!(env!("OUT_DIR"), "/palette.rs"));
}

/// A checked protocol-767 section. Opacity/hidden facts are outside this interface.
pub struct PalettedSection(owner::PalettedSection);

impl PalettedSection {
    pub fn new_single(state: u32) -> Result<Self, Error> {
        check_state(state)?;
        Ok(Self(owner::PalettedSection::single(state, false, false)))
    }

    pub fn get(&self, index: usize) -> Result<u32, Error> {
        check_index(index)?;
        Ok(self.0.get(index))
    }

    pub fn set(&mut self, index: usize, state: u32) -> Result<(), Error> {
        check_index(index)?;
        check_state(state)?;
        self.0.set(index, state, &|_| false, &|_| false);
        Ok(())
    }

    pub fn non_air(&self) -> u16 {
        self.0.non_air()
    }

    pub fn is_uniform(&self) -> bool {
        self.0.is_uniform().is_some()
    }

    /// The state of a section in the 0-bit representation.
    pub fn uniform_state(&self) -> Option<u32> {
        self.0.uniform_state()
    }

    pub fn palette_bits(&self) -> u8 {
        self.0.palette_bits()
    }

    pub fn unpack_into(&self, output: &mut [u32; SECTION_CELLS]) {
        self.0.unpack_into(output);
    }

    /// Append canonical protocol-767 block-state container bytes.
    pub fn write_block_states_wire(&self, output: &mut Vec<u8>) {
        if self.0.is_clean_wire() {
            self.0.write_block_states_wire(output);
        } else {
            let mut states = [0; SECTION_CELLS];
            self.unpack_into(&mut states);
            owner::PalettedSection::pack(&states, &|_| false, &|_| false)
                .write_block_states_wire(output);
        }
    }
}

fn check_index(index: usize) -> Result<(), Error> {
    if index < SECTION_CELLS {
        Ok(())
    } else {
        Err(Error::OutOfRange)
    }
}

fn check_state(state: u32) -> Result<(), Error> {
    if state < STATE_COUNT {
        Ok(())
    } else {
        Err(Error::InvalidState)
    }
}
