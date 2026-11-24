// SPDX-License-Identifier: LGPL-3.0-or-later
use crate::{Error, Registry, SECTION_CELLS, STATE_COUNT};

#[allow(dead_code, clippy::all)]
mod owner {
    include!(concat!(env!("OUT_DIR"), "/palette.rs"));
}

/// A checked protocol-767 section. Opacity/hidden facts are outside this interface.
///
/// Every section carries the state count of the registry its IDs belong to: the
/// 1.21.1 constructors pin `STATE_COUNT`, the `_bounded` forms accept any
/// registry's `state_count()` so packs with more (or fewer) states still decode.
pub struct PalettedSection {
    inner: owner::PalettedSection,
    state_limit: u32,
}

impl PalettedSection {
    /// A section bound to the pinned 1.21.1 state count.
    pub fn new_single(state: u32) -> Result<Self, Error> {
        Self::new_single_bounded(state, STATE_COUNT as usize)
    }

    /// A section bound to `state_count`, such as `registry.state_count()` of a
    /// pack opened with `Registry::from_parts` or `Registry::from_version_dir`.
    pub fn new_single_bounded(state: u32, state_count: usize) -> Result<Self, Error> {
        let state_limit = u32::try_from(state_count).map_err(|_| Error::OutOfRange)?;
        check_state(state, state_limit)?;
        Ok(Self {
            inner: owner::PalettedSection::single(state, false, false),
            state_limit,
        })
    }

    /// A section bound to `registry`'s state count.
    pub fn new_single_in(state: u32, registry: &Registry) -> Result<Self, Error> {
        Self::new_single_bounded(state, registry.state_count())
    }

    /// The exclusive state bound this section validates IDs against.
    pub fn state_limit(&self) -> u32 {
        self.state_limit
    }

    pub fn get(&self, index: usize) -> Result<u32, Error> {
        check_index(index)?;
        Ok(self.inner.get(index))
    }

    pub fn set(&mut self, index: usize, state: u32) -> Result<(), Error> {
        check_index(index)?;
        check_state(state, self.state_limit)?;
        self.inner.set(index, state, &|_| false, &|_| false);
        Ok(())
    }

    pub fn non_air(&self) -> u16 {
        self.inner.non_air()
    }

    pub fn is_uniform(&self) -> bool {
        self.inner.is_uniform().is_some()
    }

    /// The state of a section in the 0-bit representation.
    pub fn uniform_state(&self) -> Option<u32> {
        self.inner.uniform_state()
    }

    pub fn palette_bits(&self) -> u8 {
        self.inner.palette_bits()
    }

    pub fn unpack_into(&self, output: &mut [u32; SECTION_CELLS]) {
        self.inner.unpack_into(output);
    }

    /// Decode one protocol-767 block-state container and return the bytes consumed.
    /// Accepts the 0-bit single, 4-8-bit indirect and 15-bit direct encodings.
    /// IDs are validated against the pinned 1.21.1 state count.
    pub fn decode_block_states_wire(input: &[u8]) -> Result<(Self, usize), Error> {
        Self::decode_block_states_wire_bounded(input, STATE_COUNT as usize)
    }

    /// `decode_block_states_wire` with IDs validated against `state_count`, such
    /// as `registry.state_count()` of the pack the chunk was encoded with.
    pub fn decode_block_states_wire_bounded(
        input: &[u8],
        state_count: usize,
    ) -> Result<(Self, usize), Error> {
        let state_limit = u32::try_from(state_count).map_err(|_| Error::OutOfRange)?;
        let mut reader = Wire {
            input,
            at: 0,
            state_limit,
        };
        let bits = reader.byte()?;
        let mut palette = Vec::new();
        match bits {
            0 => {
                let state = reader.state()?;
                if reader.varint()? != 0 {
                    return Err(Error::InvalidWire);
                }
                return Ok((Self::new_single_bounded(state, state_count)?, reader.at));
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
                check_state(value, state_limit)?;
                value
            } else {
                *palette.get(value as usize).ok_or(Error::InvalidWire)?
            };
        }
        let section = owner::PalettedSection::pack(&states, &|_| false, &|_| false);
        Ok((
            Self {
                inner: section,
                state_limit,
            },
            reader.at,
        ))
    }

    /// Append canonical protocol-767 block-state container bytes.
    pub fn write_block_states_wire(&self, output: &mut Vec<u8>) {
        if self.inner.is_clean_wire() {
            self.inner.write_block_states_wire(output);
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

fn check_state(state: u32, state_limit: u32) -> Result<(), Error> {
    if state < state_limit {
        Ok(())
    } else {
        Err(Error::InvalidState)
    }
}

struct Wire<'a> {
    input: &'a [u8],
    at: usize,
    state_limit: u32,
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
        check_state(state, self.state_limit)?;
        Ok(state)
    }
}
