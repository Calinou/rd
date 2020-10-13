#![allow(non_camel_case_types)]

use crate::{
    arch::{Architecture, NativeArch},
    bindings::{kernel, kernel::sock_filter},
    kernel_abi::Ptr,
};

#[repr(C)]
pub struct robust_list<Arch: Architecture> {
    pub next: Ptr<Arch::unsigned_word, robust_list<Arch>>,
}

/// Had to manually derive Copy and Clone
/// Would not work otherwise
impl<Arch: Architecture> Clone for robust_list<Arch> {
    fn clone(&self) -> Self {
        robust_list { next: self.next }
    }
}

impl<Arch: Architecture> Copy for robust_list<Arch> {}

assert_eq_size!(kernel::robust_list, robust_list<NativeArch>);
assert_eq_align!(kernel::robust_list, robust_list<NativeArch>);

#[repr(C)]
pub struct robust_list_head<Arch: Architecture> {
    pub list: robust_list<Arch>,
    pub futex_offset: Arch::signed_long,
    pub list_op_pending: Ptr<Arch::unsigned_word, robust_list<Arch>>,
}

/// Had to manually derive Copy and Clone
/// Would not work otherwise
impl<Arch: Architecture> Clone for robust_list_head<Arch> {
    fn clone(&self) -> Self {
        robust_list_head {
            list: self.list,
            futex_offset: self.futex_offset,
            list_op_pending: self.list_op_pending,
        }
    }
}

impl<Arch: Architecture> Copy for robust_list_head<Arch> {}

assert_eq_size!(kernel::robust_list_head, robust_list_head<NativeArch>);
assert_eq_align!(kernel::robust_list_head, robust_list_head<NativeArch>);

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct sock_fprog<Arch: Architecture> {
    pub len: u16,
    pub _padding: Arch::FPROG_PAD_ARR,
    pub filter: Ptr<Arch::unsigned_word, sock_filter>,
}

assert_eq_size!(kernel::sock_fprog, sock_fprog<NativeArch>);
assert_eq_align!(kernel::sock_fprog, sock_fprog<NativeArch>);

#[repr(C)]
#[derive(Copy, Clone, Default)]
/// @TODO Any align and size asserts?
pub struct kernel_sigaction<Arch: Architecture> {
    pub k_sa_handler: Ptr<Arch::unsigned_word, u8>,
    pub sa_flags: Arch::unsigned_long,
    pub sa_restorer: Ptr<Arch::unsigned_word, u8>,
    /// This is what it is for x86 and x64 to make things simple
    /// Might this definition cause problems elsewhere e.g. for AArch64?
    pub sa_mask: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
/// @TODO Any align and size asserts?
pub struct mmap_args<Arch: Architecture> {
    pub addr: Ptr<Arch::unsigned_word, u8>,
    pub len: Arch::size_t,
    pub prot: i32,
    pub flags: i32,
    pub fd: i32,
    pub __pad: Arch::STD_PAD_ARR,
    pub offset: Arch::off_t,
}
