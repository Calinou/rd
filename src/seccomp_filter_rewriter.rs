use crate::{
    arch::Architecture,
    arch_structs::sock_fprog,
    auto_remote_syscalls::{AutoRemoteSyscalls, AutoRestoreMem},
    bindings::kernel::{sock_filter, BPF_K, BPF_RET},
    kernel_abi::is_seccomp_syscall,
    kernel_supplement::{
        SECCOMP_FILTER_FLAG_TSYNC, SECCOMP_RET_ALLOW, SECCOMP_RET_DATA, SECCOMP_RET_TRACE,
    },
    log::LogDebug,
    registers::Registers,
    remote_ptr::RemotePtr,
    seccomp_bpf::SeccompFilter,
    session::{
        address_space::{address_space::AddressSpace, Privileged},
        task::{
            record_task::RecordTask,
            task_common::{read_mem, read_val_mem, write_mem, write_val_mem},
        },
    },
};
use std::{collections::HashMap, convert::TryInto, mem::size_of};

/// When seccomp decides not to execute a syscall the kernel returns to userspace
/// without modifying the registers. There is no negative return value to
/// indicate that whatever side effects the syscall would happen did not take
/// place. This is a problem for rd, because for syscalls that require special
/// handling, we'll be performing that handling even though the syscall didn't
/// actually happen.
///
/// To get around this we can use the same mechanism that is used to skip the
/// syscall in the kernel to skip it ourselves: original_syscallno. We can't
/// use the traditional value of -1 though, because the kernel initializes
/// original_syscallno to -1 when delivering signals, and exiting sigreturn
/// will restore that. Not recording the side effects of sigreturn would be
/// bad. Instead we use -2, which still causes skipping the syscall when
/// given to the kernel as original_syscallno, but is never generated by the
/// kernel itself.
pub const SECCOMP_MAGIC_SKIP_ORIGINAL_SYSCALLNO: isize = -2;

/// Start numbering custom data values from here. This avoids overlapping
/// values that might be returned from a PTRACE_EVENT_EXIT, so we can
/// distinguish unexpected exits from real results of PTRACE_GETEVENTMSG.
pub const BASE_CUSTOM_DATA: u32 = 0x100;

#[derive(Default)]
pub struct SeccompFilterRewriter {
    /// Seccomp filters can return 32-bit result values. We need to map all of
    /// them into a single 16 bit data field. Fortunately (so far) all the
    /// filters we've seen return constants, so there aren't too many distinct
    /// values we need to deal with. For each constant value that gets returned,
    /// we'll add it as the key in `result_to_index`, with the corresponding value
    /// being the 16-bit data value that our rewritten filter returns.
    result_to_index: HashMap<u32, u16>,
    index_to_result: Vec<u32>,
}

impl SeccompFilterRewriter {
    /// Assuming `t` is set up for a prctl or seccomp syscall that
    /// installs a seccomp-bpf filter, patch the filter to signal the tracer
    /// instead of silently delivering an errno, and install it.
    pub fn install_patched_seccomp_filter(&mut self, t: &RecordTask) {
        let arch = t.arch();
        rd_arch_function_selfless!(
            install_patched_seccomp_filter_arch,
            arch,
            t,
            &mut self.result_to_index,
            &mut self.index_to_result
        )
    }

    /// Returns false if the input value is not valid. In this case a
    /// PTRACE_EVENT_EXIT probably got in the way.
    pub fn map_filter_data_to_real_result(
        &self,
        t: &RecordTask,
        value: u16,
        result: &mut u32,
    ) -> bool {
        if (value as u32) < BASE_CUSTOM_DATA {
            return false;
        }
        ed_assert!(
            t,
            (value as usize) < (BASE_CUSTOM_DATA as usize) + self.index_to_result.len()
        );

        *result = self.index_to_result[value as usize - BASE_CUSTOM_DATA as usize];

        true
    }
}

#[allow(non_snake_case)]
const fn BPF_CLASS(code: u16) -> u32 {
    code as u32 & 0x07
}

#[allow(non_snake_case)]
const fn BPF_RVAL(code: u16) -> u32 {
    code as u32 & 0x18
}

fn install_patched_seccomp_filter_arch<Arch: Architecture>(
    t: &RecordTask,
    result_to_index: &mut HashMap<u32, u16>,
    index_to_result: &mut Vec<u32>,
) {
    // Take advantage of the fact that the filter program is arg3() in both
    // prctl and seccomp syscalls.
    let mut ok = true;
    let child_addr = RemotePtr::<sock_fprog<Arch>>::from(t.regs_ref().arg3());
    let mut prog = read_val_mem(t, child_addr, Some(&mut ok));
    if !ok {
        // We'll probably return EFAULT but a kernel that doesn't support
        // seccomp(2) should return ENOSYS instead, so just run the original
        // system call to get the correct error.
        pass_through_seccomp_filter(t);
        return;
    }

    // Assuming struct sock_filter is architecture independent
    let mut code: Vec<sock_filter> = read_mem(
        t,
        Arch::as_rptr(prog.filter),
        prog.len as usize,
        Some(&mut ok),
    );

    if !ok {
        pass_through_seccomp_filter(t);
        return;
    }
    // Convert all returns to TRACE returns so that rd can handle them.
    // See handle_ptrace_event in RecordSession.
    for u in &mut code {
        if BPF_CLASS(u.code) == BPF_RET {
            ed_assert_eq!(
                t,
                BPF_RVAL(u.code),
                BPF_K,
                "seccomp-bpf program uses BPF_RET with A/X register, not supported"
            );
            if u.k != SECCOMP_RET_ALLOW {
                if result_to_index.get(&u.k).is_none() {
                    ed_assert!(
                        t,
                        BASE_CUSTOM_DATA as usize + index_to_result.len()
                            < SECCOMP_RET_DATA as usize,
                        "Too many distinct constants used in seccomp-bpf programs"
                    );
                    result_to_index.insert(u.k, index_to_result.len().try_into().unwrap());
                    index_to_result.push(u.k);
                }
                u.k = (BASE_CUSTOM_DATA + result_to_index[&u.k] as u32) | SECCOMP_RET_TRACE;
            }
        }
    }

    let mut f = SeccompFilter::new();
    for e in AddressSpace::rd_page_syscalls() {
        if e.privileged == Privileged::Privileged {
            let ip = AddressSpace::rd_page_syscall_exit_point(e.traced, e.privileged, e.enabled);
            f.allow_syscalls_from_callsite(ip);
        }
    }
    f.filters.extend_from_slice(&code);

    let orig_syscallno = t.regs_ref().original_syscallno().try_into().unwrap();
    let arg2 = t.regs_ref().arg2();
    let ret: isize;
    {
        let arg1 = t.regs_ref().arg1();
        let mut remote = AutoRemoteSyscalls::new(t);
        let mut mem = AutoRestoreMem::new(
            &mut remote,
            None,
            size_of::<sock_fprog<Arch>>() + f.filters.len() * size_of::<sock_filter>(),
        );
        let code_ptr: RemotePtr<sock_filter> = RemotePtr::cast(mem.get().unwrap());

        write_mem(mem.task(), code_ptr, &f.filters, None);

        prog.len = f.filters.len().try_into().unwrap();
        prog.filter = Arch::from_remote_ptr(code_ptr);
        let prog_ptr = RemotePtr::<sock_fprog<Arch>>::cast(code_ptr + f.filters.len());
        write_val_mem(mem.task(), prog_ptr, &prog, None);

        log!(LogDebug, "About to install seccomp filter");
        ret = mem.syscall(orig_syscallno, &[arg1, arg2, prog_ptr.as_usize()]);
    }

    set_syscall_result(t, ret);

    if !t.regs_ref().syscall_failed() {
        t.prctl_seccomp_status.set(2);
        if is_seccomp_syscall(orig_syscallno, t.arch())
            && (arg2 & SECCOMP_FILTER_FLAG_TSYNC as usize != 0)
        {
            for tt in t
                .thread_group()
                .borrow()
                .task_set()
                .iter_except(t.weak_self_clone())
            {
                tt.as_rec_unwrap().prctl_seccomp_status.set(2);
            }
        }
    }
}

fn set_syscall_result(t: &RecordTask, ret: isize) {
    let mut r: Registers = t.regs_ref().clone();
    r.set_syscall_result_signed(ret);
    t.set_regs(&r);
}

fn pass_through_seccomp_filter(t: &RecordTask) {
    let ret: isize;
    {
        let arg1 = t.regs_ref().arg1();
        let arg2 = t.regs_ref().arg2();
        let arg3 = t.regs_ref().arg3();
        let orig_syscallno: i32 = t.regs_ref().original_syscallno().try_into().unwrap();
        let mut remote = AutoRemoteSyscalls::new(t);
        ret = remote.syscall(orig_syscallno, &[arg1, arg2, arg3]);
    }
    set_syscall_result(t, ret);
    ed_assert!(t, t.regs_ref().syscall_failed());
}
