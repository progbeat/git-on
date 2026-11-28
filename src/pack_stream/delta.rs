//! Decode Git's delta instructions directly into a new object buffer.
use anyhow::{ensure, Context, Result};
use std::ops::Range;

pub(super) struct Delta<'a> {
    instructions: &'a [u8],
    base_size: usize,
    target_size: usize,
}

impl<'a> Delta<'a> {
    pub(super) fn parse(mut bytes: &'a [u8]) -> Result<Self> {
        let base_size = number(&mut bytes)?;
        let target_size = number(&mut bytes)?;
        Ok(Self {
            instructions: bytes,
            base_size,
            target_size,
        })
    }

    pub(super) fn target_size(&self) -> usize {
        self.target_size
    }

    pub(super) fn apply(
        self,
        base_size: usize,
        mut emit: impl FnMut(Part<'_>) -> Result<()>,
    ) -> Result<()> {
        ensure!(base_size == self.base_size, "Git delta base size mismatch");
        let mut instructions = self.instructions;
        let mut remaining = self.target_size;
        while !instructions.is_empty() {
            let opcode = next(&mut instructions)?;
            if opcode & 0x80 != 0 {
                let offset = parameter(&mut instructions, opcode & 15, 4)?;
                let mut length = parameter(&mut instructions, (opcode >> 4) & 7, 3)?;
                if length == 0 {
                    length = 65_536;
                }
                ensure!(length <= remaining, "Git delta exceeded its output size");
                let end = offset
                    .checked_add(length)
                    .context("Git delta copy overflow")?;
                ensure!(end <= base_size, "Git delta copy is outside its base");
                emit(Part::Copy(offset..end))?;
                remaining -= length;
            } else {
                let length = usize::from(opcode);
                ensure!(
                    length != 0 && length <= remaining,
                    "invalid Git delta literal size"
                );
                let (literal, rest) = instructions
                    .split_at_checked(length)
                    .context("truncated Git delta literal")?;
                instructions = rest;
                emit(Part::Bytes(literal))?;
                remaining -= length;
            }
        }
        ensure!(remaining == 0, "Git delta produced too few bytes");
        Ok(())
    }
}

pub(super) enum Part<'a> {
    Copy(Range<usize>),
    Bytes(&'a [u8]),
}

fn next(bytes: &mut &[u8]) -> Result<u8> {
    let (&byte, rest) = bytes.split_first().context("truncated Git delta")?;
    *bytes = rest;
    Ok(byte)
}

fn number(bytes: &mut &[u8]) -> Result<usize> {
    let mut value = 0;
    for shift in (0..usize::BITS).step_by(7) {
        let byte = next(bytes)?;
        let part = usize::from(byte & 127);
        ensure!(part <= usize::MAX >> shift, "Git delta size overflow");
        value |= part << shift;
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    anyhow::bail!("Git delta size overflow")
}

fn parameter(bytes: &mut &[u8], flags: u8, count: u32) -> Result<usize> {
    let mut value = 0;
    for index in 0..count {
        if flags & (1 << index) != 0 {
            value |= usize::from(next(bytes)?) << (8 * index);
        }
    }
    Ok(value)
}
