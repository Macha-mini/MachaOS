use core::slice;
use core::str;

pub const MAGIC: u32 = 0x2BADB002;

pub struct MultibootInfo {
    addr: usize,
}

impl MultibootInfo {
    pub const fn new(addr: usize) -> Self {
        Self { addr }
    }

    pub fn flags(&self) -> u32 {
        self.read_u32(0)
    }

    pub fn memory_lower_kb(&self) -> Option<u32> {
        (self.flags() & 1 != 0).then(|| self.read_u32(4))
    }

    pub fn memory_upper_kb(&self) -> Option<u32> {
        (self.flags() & 1 != 0).then(|| self.read_u32(8))
    }

    pub fn cmdline(&self) -> Option<&'static str> {
        if self.flags() & (1 << 2) == 0 {
            return None;
        }
        Some(cstr(self.read_u32(16) as usize))
    }

    pub fn boot_loader_name(&self) -> Option<&'static str> {
        if self.flags() & (1 << 9) == 0 {
            return None;
        }
        Some(cstr(self.read_u32(64) as usize))
    }

    pub fn memory_map(&self) -> MemoryMapIter {
        if self.flags() & (1 << 6) == 0 {
            return MemoryMapIter::empty();
        }
        let length = self.read_u32(44) as usize;
        let start = self.read_u32(48) as usize;
        MemoryMapIter {
            current: start,
            end: start + length,
        }
    }

    pub fn framebuffer(&self) -> Option<FramebufferInfo> {
        if self.flags() & (1 << 12) == 0 {
            return None;
        }
        let addr = self.read_u64(88);
        let pitch = self.read_u32(96);
        let width = self.read_u32(100);
        let height = self.read_u32(104);
        let bpp = self.read_u8(108);
        if addr == 0 || width == 0 || height == 0 || bpp == 0 {
            return None;
        }
        Some(FramebufferInfo {
            addr,
            pitch,
            width,
            height,
            bpp,
        })
    }

    fn read_u32(&self, offset: usize) -> u32 {
        unsafe { core::ptr::read_unaligned((self.addr + offset) as *const u32) }
    }

    fn read_u64(&self, offset: usize) -> u64 {
        unsafe { core::ptr::read_unaligned((self.addr + offset) as *const u64) }
    }

    fn read_u8(&self, offset: usize) -> u8 {
        unsafe { core::ptr::read_unaligned((self.addr + offset) as *const u8) }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FramebufferInfo {
    pub addr: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub bpp: u8,
}

fn cstr(addr: usize) -> &'static str {
    let mut end = addr;
    while unsafe { core::ptr::read_unaligned(end as *const u8) } != 0 {
        end += 1;
    }
    let bytes = unsafe { slice::from_raw_parts(addr as *const u8, end - addr) };
    unsafe { str::from_utf8_unchecked(bytes) }
}

#[derive(Clone, Copy, Debug)]
pub struct MemoryRegion {
    pub base: u64,
    pub length: u64,
    pub region_type: u32,
}

impl MemoryRegion {
    pub fn type_name(&self) -> &'static str {
        match self.region_type {
            1 => "usable",
            2 => "reserved",
            3 => "ACPI reclaimable",
            4 => "ACPI NVS",
            5 => "bad RAM",
            _ => "unknown",
        }
    }
}

pub struct MemoryMapIter {
    current: usize,
    end: usize,
}

impl MemoryMapIter {
    pub fn empty() -> Self {
        Self { current: 0, end: 0 }
    }
}

impl Iterator for MemoryMapIter {
    type Item = MemoryRegion;

    fn next(&mut self) -> Option<MemoryRegion> {
        if self.current >= self.end {
            return None;
        }
        unsafe {
            let size = core::ptr::read_unaligned(self.current as *const u32) as usize;
            let base = core::ptr::read_unaligned((self.current + 4) as *const u64);
            let length = core::ptr::read_unaligned((self.current + 12) as *const u64);
            let region_type = core::ptr::read_unaligned((self.current + 20) as *const u32);
            self.current = (self.current + size + 4 + 7) & !7;
            Some(MemoryRegion {
                base,
                length,
                region_type,
            })
        }
    }
}

static mut INFO_ADDR: usize = 0;

pub fn set_info(addr: usize) {
    unsafe { INFO_ADDR = addr }
}

pub fn info() -> MultibootInfo {
    MultibootInfo::new(unsafe { INFO_ADDR })
}
