use core::fmt::Display;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalAddr(usize);

impl PhysicalAddr {
    pub const fn new(addr: usize) -> Self {
        Self(addr)
    }

    pub const fn as_u64(self) -> u64 {
        self.0 as u64
    }

    pub const fn as_usize(self) -> usize {
        self.0
    }

    pub const fn add(self, offset: usize) -> PhysicalAddr {
        PhysicalAddr(self.0 + offset)
    }

    pub const fn align_up(self, align: usize) -> PhysicalAddr {
        assert!(align.is_power_of_two());
        PhysicalAddr((self.0 + align - 1) & !(align - 1))
    }

    pub fn to_virtual(self, map: &impl DirectMap) -> VirtualAddr {
        map.p2v(self)
    }
}

impl Display for PhysicalAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VirtualAddr(usize);

impl VirtualAddr {
    pub const fn new(addr: usize) -> Self {
        Self(addr)
    }

    pub const fn as_u64(self) -> u64 {
        self.0 as u64
    }

    pub const fn as_usize(self) -> usize {
        self.0
    }

    pub const fn add(self, offset: usize) -> VirtualAddr {
        VirtualAddr(self.0 + offset)
    }

    pub fn to_physical(self, map: &impl DirectMap) -> Option<PhysicalAddr> {
        map.v2p(self)
    }

    pub const fn as_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }

    pub unsafe fn as_ref_mut<'i, T>(self) -> &'i mut T {
        debug_assert!(self.0 % core::mem::align_of::<T>() == 0);
        unsafe { &mut *self.as_ptr() }
    }
}

impl Display for VirtualAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

pub trait DirectMap {
    fn p2v(&self, paddr: PhysicalAddr) -> VirtualAddr;
    fn v2p(&self, vaddr: VirtualAddr) -> Option<PhysicalAddr>;
}
