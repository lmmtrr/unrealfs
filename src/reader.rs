pub type Result<T> = std::result::Result<T, String>;

const MAX_NESTING: usize = 64;

pub struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
    depth: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0, depth: 0 }
    }

    pub fn nested<T>(&mut self, read: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.depth >= MAX_NESTING {
            return Err(format!("data is nested too deeply at {}", self.pos));
        }
        self.depth += 1;
        let result = read(self);
        self.depth -= 1;
        result
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let Some(s) = self.pos.checked_add(n).and_then(|end| self.data.get(self.pos..end)) else {
            return Err(format!(
                "read of {} bytes at {} exceeds length {}",
                n,
                self.pos,
                self.data.len()
            ));
        };
        self.pos += n;
        Ok(s)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }

    pub fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    pub fn bool32(&mut self) -> Result<bool> {
        let at = self.pos;
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(format!("invalid bool value {} at {}", v, at)),
        }
    }

    pub fn count(&mut self, min_elem_size: usize) -> Result<usize> {
        let at = self.pos;
        let n = self.i32()?;
        if n < 0 || (n as usize).saturating_mul(min_elem_size.max(1)) > self.remaining() {
            return Err(format!("invalid array count {} at {}", n, at));
        }
        Ok(n as usize)
    }

    pub fn fstring(&mut self) -> Result<String> {
        let at = self.pos;
        let n = self.i32()?;
        if n == 0 {
            return Ok(String::new());
        }
        if n < 0 {
            let len = n.unsigned_abs() as usize;
            if len * 2 > self.remaining() {
                return Err(format!("invalid FString length {} at {}", n, at));
            }
            let raw = self.bytes(len * 2)?;
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take(len - 1)
                .collect();
            Ok(String::from_utf16_lossy(&units))
        } else {
            let len = n as usize;
            if len > self.remaining() {
                return Err(format!("invalid FString length {} at {}", n, at));
            }
            let raw = self.bytes(len)?;
            Ok(raw[..len - 1].iter().map(|&b| b as char).collect())
        }
    }

    pub fn bulk_array(&mut self) -> Result<(usize, usize, &'a [u8])> {
        let at = self.pos;
        let elem_size = self.i32()?;
        let count = self.i32()?;
        if elem_size < 0 || count < 0 {
            return Err(format!("invalid bulk array header at {}", at));
        }
        let total = (elem_size as usize)
            .checked_mul(count as usize)
            .ok_or_else(|| format!("bulk array overflow at {}", at))?;
        let data = self.bytes(total)?;
        Ok((elem_size as usize, count as usize, data))
    }
}
