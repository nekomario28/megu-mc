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

    /// Decode one protocol-767 block-state container and return the bytes consumed.
    /// Accepts the 0-bit single, 4-8-bit indirect and 15-bit direct encodings.
    pub fn decode_block_states_wire(input: &[u8]) -> Result<(Self, usize), Error> {
        let mut reader = Wire { input, at: 0 };
        let bits = reader.byte()?;
        let mut palette = Vec::new();
        match bits {
            0 => {
                let state = reader.state()?;
                if reader.varint()? != 0 {
                    return Err(Error::InvalidWire);
                }
                return Ok((Self::new_single(state)?, reader.at));
            }
            4..=8 => {
                let len = reader.varint()? as usize;
                if len == 0 || len > 1 << bits {
                    return Err(Error::InvalidWire);
                }
                for _ in 0..len {
                    palette.push(reader.state()?);
                }
            }
            15 => {}
            _ => return Err(Error::InvalidWire),
        }
        let per_word = 64 / usize::from(bits);
        if reader.varint()? as usize != SECTION_CELLS.div_ceil(per_word) {
            return Err(Error::InvalidWire);
        }
        let mut states = [0; SECTION_CELLS];
        let mut word = 0;
        for (i, state) in states.iter_mut().enumerate() {
            if i % per_word == 0 {
                word = reader.word()?;
            }
            let value = ((word >> (i % per_word * usize::from(bits))) & ((1 << bits) - 1)) as u32;
            *state = if bits == 15 {
                check_state(value)?;
                value
            } else {
                *palette.get(value as usize).ok_or(Error::InvalidWire)?
            };
        }
        let section = owner::PalettedSection::pack(&states, &|_| false, &|_| false);
        Ok((Self(section), reader.at))
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

struct Wire<'a> {
    input: &'a [u8],
    at: usize,
}

impl Wire<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let bytes = self
            .input
            .get(self.at..self.at + N)
            .ok_or(Error::InvalidWire)?;
        self.at += N;
        Ok(bytes.try_into().expect("slice length"))
    }

    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take::<1>()?[0])
    }

    fn word(&mut self) -> Result<u64, Error> {
        Ok(u64::from_be_bytes(self.take()?))
    }

    /// A non-negative protocol VarInt of at most five bytes.
    fn varint(&mut self) -> Result<u32, Error> {
        let mut value = 0u32;
        for shift in (0..35).step_by(7) {
            let byte = self.byte()?;
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return if value > i32::MAX as u32 {
                    Err(Error::InvalidWire)
                } else {
                    Ok(value)
                };
            }
        }
        Err(Error::InvalidWire)
    }

    fn state(&mut self) -> Result<u32, Error> {
        let state = self.varint()?;
        check_state(state)?;
        Ok(state)
    }
}
