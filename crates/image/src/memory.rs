//! Host memory manager: Core's memory subsystem backed by the system allocator.

use core::ptr::NonNull;
use memory::{CapabilityFlags, Layout, ManagerTrait};
use std::{
    alloc::{GlobalAlloc, System},
    sync::atomic::{AtomicUsize, Ordering},
};

/// Xila memory manager for host tools, backed by the system allocator.
///
/// It serves ordinary allocations and [`CapabilityFlags::AlwaysAccessible`],
/// which host memory always satisfies. Host-only capabilities it cannot
/// provide, [`CapabilityFlags::Executable`] and
/// [`CapabilityFlags::DirectMemoryAccess`], are rejected: such allocations
/// fail instead of returning ordinary memory. Zero-sized allocations and
/// reallocations fail too, as the system allocator does not accept them.
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

impl HostMemoryManager {
    /// Capabilities that host memory provides.
    pub const SUPPORTED_CAPABILITIES: CapabilityFlags = CapabilityFlags::AlwaysAccessible;
}

impl ManagerTrait for HostMemoryManager {
    unsafe fn allocate(
        &self,
        capabilities: CapabilityFlags,
        layout: Layout,
    ) -> Option<NonNull<u8>> {
        // `GlobalAlloc::alloc` has undefined behaviour for zero-sized layouts.
        if layout.size() == 0 || !Self::SUPPORTED_CAPABILITIES.contains(capabilities) {
            return None;
        }
        // SAFETY: the layout has a non-zero size.
        let pointer = NonNull::new(unsafe { System.alloc(layout) })?;
        self.used.fetch_add(layout.size(), Ordering::Relaxed);
        Some(pointer)
    }

    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        // `allocate` never returns zero-sized blocks, so there is nothing to
        // release.
        if layout.size() == 0 {
            return;
        }
        // SAFETY: the pointer was returned by `allocate` with this layout.
        unsafe { System.dealloc(pointer.as_ptr(), layout) };
        self.used.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn reallocate(
        &self,
        old_pointer: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Option<NonNull<u8>> {
        // A zero-sized target fails and leaves the original block valid, as
        // the trait requires on failure.
        if new_layout.size() == 0 {
            return None;
        }
        // SAFETY: `new_layout` has a non-zero size.
        let new_pointer = unsafe { self.allocate(CapabilityFlags::empty(), new_layout) }?;
        let length = old_layout.size().min(new_layout.size());
        // SAFETY: the caller guarantees `old_pointer` is a live block of
        // `old_layout`, and the new block is distinct and at least `length`
        // bytes long.
        unsafe {
            core::ptr::copy_nonoverlapping(old_pointer.as_ptr(), new_pointer.as_ptr(), length);
            self.deallocate(old_pointer, old_layout);
        }
        Some(new_pointer)
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

    #[test]
    fn zero_sized_requests_fail_without_reaching_the_system_allocator() {
        let manager = HostMemoryManager::new();
        let empty = Layout::from_size_align(0, 8).unwrap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        // SAFETY: every pointer is used within its layout and released once.
        unsafe {
            assert!(manager.allocate(CapabilityFlags::empty(), empty).is_none());
            // Deallocating a zero-sized block is a no-op.
            manager.deallocate(NonNull::dangling(), empty);
            assert_eq!(manager.get_used(), 0);

            let pointer = manager.allocate(CapabilityFlags::empty(), layout).unwrap();
            pointer.as_ptr().write_bytes(0x5A, 64);
            // Shrinking to zero fails and keeps the original block valid.
            assert!(manager.reallocate(pointer, layout, empty).is_none());
            assert_eq!(manager.get_used(), 64);
            assert_eq!(pointer.as_ptr().add(63).read(), 0x5A);

            // Growing from a zero-sized block allocates without copying.
            let grown = manager
                .reallocate(NonNull::dangling(), empty, layout)
                .unwrap();
            manager.deallocate(grown, layout);

            let larger = Layout::from_size_align(128, 8).unwrap();
            let pointer = manager.reallocate(pointer, layout, larger).unwrap();
            assert_eq!(manager.get_used(), 128);
            assert_eq!(pointer.as_ptr().add(63).read(), 0x5A);
            manager.deallocate(pointer, larger);
        }
        assert_eq!(manager.get_used(), 0);
    }

    #[test]
    fn unsupported_capabilities_are_rejected() {
        let manager = HostMemoryManager::new();
        let layout = Layout::from_size_align(32, 8).unwrap();
        // SAFETY: the only successful allocation is released with its layout.
        unsafe {
            for capabilities in [
                CapabilityFlags::Executable,
                CapabilityFlags::DirectMemoryAccess,
                CapabilityFlags::Executable | CapabilityFlags::AlwaysAccessible,
            ] {
                assert!(manager.allocate(capabilities, layout).is_none());
            }
            let pointer = manager
                .allocate(CapabilityFlags::AlwaysAccessible, layout)
                .unwrap();
            manager.deallocate(pointer, layout);
        }
        assert_eq!(manager.get_used(), 0);
    }
}
