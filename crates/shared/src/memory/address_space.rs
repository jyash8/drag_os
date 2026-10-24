use crate::memory::page::{PageTable, PhysPage};

#[derive(Clone, Copy, Debug)]
pub struct AddressSpace {
    root: PhysPage,
}

impl AddressSpace {
    pub fn new(root: PhysPage) -> Self {
        Self { root }
    }

    pub fn root(&self) -> PhysPage {
        self.root
    }

    pub unsafe fn root_table_mut(&self) -> &mut PageTable {
        unsafe { &mut *self.root.start().as_mut_ptr::<PageTable>() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::address::PhysAddr;

    #[test]
    fn wraps_phys_page() {
        let p = PhysPage::containing(PhysAddr(0x3000));
        let aspace = AddressSpace::new(p);
        assert_eq!(aspace.root(), p);
    }
}
