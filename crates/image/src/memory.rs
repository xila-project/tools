//! Host memory manager: Core's memory subsystem backed by the system allocator.

use core::ptr::NonNull;
use memory::{CapabilityFlags, Layout, ManagerTrait};
use std::{
    alloc::{GlobalAlloc, System},
    sync::atomic::{AtomicUsize, Ordering},
};

/// Xila memory manager for host tools: every capability is served by the
/// system allocator.
pub struct HostMemoryManager {
    used: AtomicUsize,
}

impl Default for HostMemoryManager {
    fn default() -> Self {
        Self::new()
    }
}

impl HostMemoryManager {
    pub const fn new() -> Self {
        Self {
            used: AtomicUsize::new(0),
        }
    }
}

impl ManagerTrait for HostMemoryManager {
    unsafe fn allocate(&self, _: CapabilityFlags, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: the caller upholds the `GlobalAlloc::alloc` contract.
        let pointer = NonNull::new(unsafe { System.alloc(layout) })?;
        self.used.fetch_add(layout.size(), Ordering::Relaxed);
        Some(pointer)
    }

    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        // SAFETY: the pointer was returned by `allocate` with this layout.
        unsafe { System.dealloc(pointer.as_ptr(), layout) };
        self.used.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    fn get_used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    fn get_free(&self) -> usize {
        (isize::MAX as usize).saturating_sub(self.get_used())
    }
}

/// Pull the ABI memory exports into the executable.
///
/// LittleFS calls these C ABI symbols from its allocator hooks, so they must
/// be linked even though no Rust code calls them. [`crate::build`] calls this
/// function; code that uses LittleFS directly should call it too.
pub fn link_abi_memory_exports() {
    std::hint::black_box(abi_definitions::xila_memory_allocate as *const ());
    std::hint::black_box(abi_definitions::xila_memory_deallocate as *const ());
    std::hint::black_box(&abi_definitions::XILA_MEMORY_CAPABILITIES_NONE as *const _ as usize);
}

/// Install [`HostMemoryManager`] as the global allocator and Xila memory
/// manager. Invoke it once, in the executable (or test) that links the image
/// library.
#[macro_export]
macro_rules! instantiate_global_allocator {
    () => {
        static __XILA_TOOLS_MEMORY_MANAGER: $crate::HostMemoryManager =
            $crate::HostMemoryManager::new();
        $crate::memory_crate::instantiate_global_allocator!(&__XILA_TOOLS_MEMORY_MANAGER);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocations_are_counted_and_released() {
        let manager = HostMemoryManager::default();
        let layout = Layout::from_size_align(128, 16).unwrap();
        // SAFETY: the layout has a non-zero size and the pointer is released
        // with the same layout.
        unsafe {
            let pointer = manager.allocate(CapabilityFlags::empty(), layout).unwrap();
            assert_eq!(pointer.as_ptr() as usize % 16, 0);
            assert_eq!(manager.get_used(), 128);
            assert!(manager.get_free() > 0);
            manager.deallocate(pointer, layout);
        }
        assert_eq!(manager.get_used(), 0);
    }
}
