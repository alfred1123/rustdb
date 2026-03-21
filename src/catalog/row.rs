use crate::error::{Error, Result};

/// Per-field overhead: u64 LE length prefix (8 bytes).
pub const LENGTH_PREFIX_SIZE: usize = 8;

/// Minimum serialized bytes per column: length prefix + 1 byte data
/// (smallest possible value is CHAR(1)).  Used to derive the dynamic
/// column-count limit from the page size.
pub const MIN_COLUMN_BYTES: usize = LENGTH_PREFIX_SIZE + 1;

/// Reads length-prefixed fields from a binary row.
///
/// Wire format per field: [u64 LE byte_length][value_bytes]
pub struct RowReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> RowReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn read_field(&mut self) -> Result<&'a [u8]> {
        if self.pos + 8 > self.data.len() {
            return Err(Error::Corruption("unexpected end of row".into()));
        }
        let len = u64::from_le_bytes(
            self.data[self.pos..self.pos + 8].try_into().unwrap(),
        ) as usize;
        self.pos += 8;
        if self.pos + len > self.data.len() {
            return Err(Error::Corruption("field exceeds row boundary".into()));
        }
        let val = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(val)
    }

    pub fn read_i16(&mut self) -> Result<i16> {
        let b = self.read_field()?;
        let arr: [u8; 2] = b
            .try_into()
            .map_err(|_| Error::Corruption("invalid SMALLINT".into()))?;
        Ok(i16::from_le_bytes(arr))
    }

    pub fn read_i32(&mut self) -> Result<i32> {
        let b = self.read_field()?;
        let arr: [u8; 4] = b
            .try_into()
            .map_err(|_| Error::Corruption("invalid INTEGER".into()))?;
        Ok(i32::from_le_bytes(arr))
    }

    pub fn read_string(&mut self) -> Result<String> {
        let b = self.read_field()?;
        String::from_utf8(b.to_vec())
            .map_err(|e| Error::Corruption(format!("invalid UTF-8: {e}")))
    }

    pub fn read_bool(&mut self) -> Result<bool> {
        let b = self.read_field()?;
        match b {
            [b'Y'] => Ok(true),
            [b'N'] => Ok(false),
            _ => Err(Error::Corruption("expected Y/N flag".into())),
        }
    }
}

/// Writes length-prefixed fields to a binary row buffer.
pub struct RowWriter {
    buf: Vec<u8>,
}

impl RowWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn write_field(&mut self, data: &[u8]) {
        self.buf
            .extend_from_slice(&(data.len() as u64).to_le_bytes());
        self.buf.extend_from_slice(data);
    }

    pub fn write_i16(&mut self, val: i16) {
        self.write_field(&val.to_le_bytes());
    }

    pub fn write_i32(&mut self, val: i32) {
        self.write_field(&val.to_le_bytes());
    }

    pub fn write_string(&mut self, val: &str) {
        self.write_field(val.as_bytes());
    }

    pub fn write_bool(&mut self, val: bool) {
        self.write_field(if val { b"Y" } else { b"N" });
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_fields() {
        let mut w = RowWriter::new();
        w.write_i16(42);
        w.write_string("HELLO");
        w.write_bool(true);
        w.write_i32(-1);
        let data = w.finish();

        let mut r = RowReader::new(&data);
        assert_eq!(r.read_i16().unwrap(), 42);
        assert_eq!(r.read_string().unwrap(), "HELLO");
        assert_eq!(r.read_bool().unwrap(), true);
        assert_eq!(r.read_i32().unwrap(), -1);
    }
}
