const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = 0xEDB88320 ^ (crc >> 1);
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

pub struct Crc32 {
    value: u32,
}

impl Crc32 {
    pub fn new() -> Self {
        Self { value: !0u32 }
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut crc = self.value;
        for &byte in data {
            let idx = ((crc as u8) ^ byte) as usize;
            crc = CRC32_TABLE[idx] ^ (crc >> 8);
        }
        self.value = crc;
    }

    pub fn finalize(self) -> u32 {
        !self.value
    }

    pub fn finalize_hex(self) -> String {
        format!("{:08x}", self.finalize())
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(data);
    crc.finalize()
}

pub fn crc32_hex(data: &[u8]) -> String {
    format!("{:08x}", crc32(data))
}
