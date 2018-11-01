//! Weld's runtime functions.
//!
//! These are functions that are accessed from generated code.

extern crate libc;
extern crate fnv;

use std::alloc::System as Allocator;
use libc::{c_char, int32_t, int64_t, uint64_t};

use fnv::FnvHashMap;

use std::ffi::CStr;
use std::fmt;
use std::ptr;
use std::sync::{Once, ONCE_INIT};

use std::alloc::{Layout, GlobalAlloc};

pub type Ptr = *mut u8;
pub type WeldRuntimeContextRef = *mut WeldRuntimeContext;

/// Initialize the Weld runtime only once.
static ONCE: Once = ONCE_INIT;
static mut INITIALIZE_FAILED: bool = false;

const DEFAULT_ALIGN: usize = 8;

/// An errno set by the runtime but also used by the Weld API.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd)]
#[repr(u64)]
pub enum WeldRuntimeErrno {
    /// Indicates success.
    ///
    /// This will always be 0.  
    Success = 0,
    /// Invalid configuration.
    ConfigurationError,
    /// Dynamic library load error.
    LoadLibraryError,
    /// Weld compilation error.
    CompileError,
    /// Array out-of-bounds error.
    ArrayOutOfBounds,
    /// A Weld iterator was invalid.
    BadIteratorLength,
    /// Mismatched Zip error.
    ///
    /// This error is thrown if the vectors in a Zip have different lengths.
    MismatchedZipSize,
    /// Out of memory error.
    ///
    /// This error is thrown if the amount of memory allocated by the runtime exceeds the limit set
    /// by the configuration.
    OutOfMemory,
    RunNotFound,
    /// An unknown error.
    Unknown,
    /// A deserialization error.
    ///
    /// This error occurs if a buffer being deserialized has an invalid length.
    DeserializationError,
    /// A key was not found in a dictionary.
    KeyNotFoundError,
    /// Maximum errno value.
    ///
    /// All errors will have a value less than this value and greater than 0.
    ErrnoMax,
}

impl fmt::Display for WeldRuntimeErrno {
    /// Just return the errno name.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}


/// Maintains information about a single Weld run.
#[derive(Debug,PartialEq)]
pub struct WeldRuntimeContext {
    /// Maps pointers to allocation size in bytes.
    allocations: FnvHashMap<Ptr, Layout>,
    /// An error code set for the context.
    errno: WeldRuntimeErrno,
    /// A result pointer set by the runtime.
    result: Ptr,
    /// The number of worker threads.
    nworkers: i32,
    /// A memory limit.
    memlimit: usize,
    /// Number of allocated bytes so far.
    ///
    /// This will always be equal to `allocations.values().sum()`.
    allocated: usize,
}

/// Private API used by the FFI.
impl WeldRuntimeContext {
    unsafe fn malloc(&mut self, size: i64) -> Ptr {
        if size == 0 {
            trace!("Alloc'd 0-size pointer (null)");
            return ptr::null_mut();
        }

        let size = size as usize;

        if self.allocated + size > self.memlimit {
            self.errno = WeldRuntimeErrno::OutOfMemory;
            panic!("Weld run ran out of memory (limit={}, attempted to allocate {}",
                   self.memlimit,
                   self.allocated + size);
        }
        let layout = Layout::from_size_align_unchecked(size as usize, DEFAULT_ALIGN);
        let mem = Allocator.alloc(layout.clone());

        self.allocated += layout.size();
        trace!("Alloc'd pointer {:?} ({} bytes)", mem, layout.size());

        self.allocations.insert(mem.clone(), layout);
        mem
    }

    unsafe fn realloc(&mut self, pointer: Ptr, size: i64) -> Ptr {

        if pointer == ptr::null_mut() {
            return self.malloc(size)
        }

        let size = size as usize;
        let old_layout = self.allocations.remove(&pointer).unwrap();
        if self.allocated - old_layout.size() + size > self.memlimit {
            self.errno = WeldRuntimeErrno::OutOfMemory;
            panic!("Weld run ran out of memory (limit={}, attempted to allocate {}",
                   self.memlimit,
                   self.allocated - old_layout.size() + size);
        }

        // Must pass *old* layout to realloc!
        let mem = Allocator.realloc(pointer, old_layout.clone(), size);
        let new_layout = Layout::from_size_align_unchecked(size, DEFAULT_ALIGN);

        self.allocated -= old_layout.size();
        self.allocated += new_layout.size();

        self.allocations.insert(mem, new_layout);
        mem
    }

    fn set_errno(&mut self, errno: WeldRuntimeErrno) {
        self.errno = errno;
        panic!("Weld runtime threw error: {}", self.errno)
    }

    fn set_result(&mut self, result: Ptr) {
        self.result = result;
    }

    fn result(&self) -> Ptr {
        self.result
    }

}

// Public API.
impl WeldRuntimeContext {
    /// Construct a new `WeldRuntimeContext`.
    pub fn new(nworkers: i32, memlimit: i64) -> WeldRuntimeContext {
        WeldRuntimeContext {
            allocations: FnvHashMap::default(),
            errno: WeldRuntimeErrno::Success,
            result: ptr::null_mut(),
            nworkers: nworkers,
            memlimit: memlimit as usize,
            allocated: 0,
        }
    }

    /// Free an allocated data value.
    ///
    /// Panics if the passed value was not allocated by the Weld runtime.
    pub unsafe fn free(&mut self, pointer: Ptr) {
        if pointer == ptr::null_mut() {
            trace!("Freed null pointer (no-op) in runst_free()");
            return;
        }

        let layout = self.allocations.remove(&pointer).unwrap();

        trace!("Freeing pointer {:?} ({} bytes) in runst_free()", pointer, layout.size());

        Allocator.dealloc(pointer, layout.clone());
        self.allocated -= layout.size();
    }

    /// Returns the number of bytes allocated by this Weld run.
    pub fn memory_usage(&self) -> i64 {
        self.allocated as i64
    }

    /// Returns a 64-bit ID identifying this run.
    pub fn run_id(&self) -> i64 {
        0
    }

    /// Returns the error code of this run.
    fn errno(&self) -> WeldRuntimeErrno {
        self.errno
    }

    /// Returns the number of worker threads set for this run.
    pub fn threads(&self) -> i32 {
        self.nworkers
    }

    /// Returns the memory limit of this run.
    pub fn memory_limit(&self) -> i64 {
        self.memlimit as i64
    }
}

impl Drop for WeldRuntimeContext {
    fn drop(&mut self) {
        // Free memory allocated by the run.
        trace!("Allocations: {}", self.allocations.len());
        unsafe {
            for (pointer, layout) in self.allocations.iter() {
                trace!("Freeing pointer {:?} ({} bytes) in drop()", *pointer, layout.size());
                Allocator.dealloc(*pointer, layout.clone());
            }
        }
    }
}

unsafe fn initialize() {
    // Hack to prevent symbols from being compiled out in a Rust binary.
    let mut x =  weld_runst_init as i64;
    x += weld_runst_set_result as i64;
    x += weld_runst_get_result as i64;
    x += weld_runst_malloc as i64;
    x += weld_runst_realloc as i64;
    x += weld_runst_free as i64;
    x += weld_runst_get_errno as i64;
    x += weld_runst_set_errno as i64;

    x += weld_runst_print_int as i64;
    x += weld_runst_print as i64;

    trace!("Runtime initialized with hashed values {}", x);
}

#[no_mangle]
/// Initialize the runtime.
///
/// This call currently circumvents an awkward problem with binaries using Rust, where the FFI
/// below used by the Weld runtime is compiled out.
pub unsafe extern "C" fn weld_init() {
    ONCE.call_once(|| initialize());
    if INITIALIZE_FAILED {
        panic!("Weld runtime initialization failed")
    }
}

#[no_mangle]
/// Initialize the runtime if necessary, and then initialize run-specific information.
pub unsafe extern "C" fn weld_runst_init(nworkers: int32_t, memlimit: int64_t) -> WeldRuntimeContextRef {
    Box::into_raw(Box::new(WeldRuntimeContext::new(nworkers, memlimit)))
}

#[no_mangle]
/// Allocate memory using `libc` `malloc`, tracking memory usage information.
pub unsafe extern "C" fn weld_runst_malloc(run: WeldRuntimeContextRef, size: int64_t) -> Ptr {
    let run = &mut *run;
    run.malloc(size)
}

#[no_mangle]
/// Allocate memory using `libc` `remalloc`, tracking memory usage information.
pub unsafe extern "C" fn weld_runst_realloc(run: WeldRuntimeContextRef,
                                     ptr: Ptr,
                                     newsize: int64_t) -> Ptr {
    let run = &mut *run;
    run.realloc(ptr, newsize)
}

#[no_mangle]
/// Free memory using `libc` `free`, tracking memory usage information.
pub unsafe extern "C" fn weld_runst_free(run: WeldRuntimeContextRef, ptr: Ptr) {
    let run = &mut *run;
    run.free(ptr)
}

#[no_mangle]
/// Set the result pointer.
pub unsafe extern "C" fn weld_runst_set_result(run: WeldRuntimeContextRef, ptr: Ptr) {
    let run = &mut *run;
    run.set_result(ptr)
}

#[no_mangle]
/// Set the errno value.
pub unsafe extern "C" fn weld_runst_set_errno(run: WeldRuntimeContextRef,
                                              errno: WeldRuntimeErrno) {
    let run = &mut *run;
    run.set_errno(errno)
}

#[no_mangle]
/// Get the errno value.
pub unsafe extern "C" fn weld_runst_get_errno(run: WeldRuntimeContextRef) -> WeldRuntimeErrno {
    let run = &mut *run;
    run.errno()
}

#[no_mangle]
/// Get the result pointer.
pub unsafe extern "C" fn weld_runst_get_result(run: WeldRuntimeContextRef) -> Ptr {
    let run = &mut *run;
    run.result()
}

#[no_mangle]
/// Print a value from generated code.
pub unsafe extern "C" fn weld_runst_print(_run: WeldRuntimeContextRef, string: *const c_char) {
    let string = CStr::from_ptr(string).to_str().unwrap();
    println!("{} ", string);
}

#[no_mangle]
/// Print a value from generated code.
pub unsafe extern "C" fn weld_runst_print_int(_run: WeldRuntimeContextRef, i: uint64_t) {
    println!("{}", i);
}