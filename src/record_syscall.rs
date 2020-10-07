use crate::{
    arch::Architecture,
    bindings::prctl::{
        PR_CAPBSET_DROP,
        PR_CAPBSET_READ,
        PR_CAP_AMBIENT,
        PR_GET_CHILD_SUBREAPER,
        PR_GET_DUMPABLE,
        PR_GET_ENDIAN,
        PR_GET_FPEMU,
        PR_GET_FPEXC,
        PR_GET_KEEPCAPS,
        PR_GET_NAME,
        PR_GET_NO_NEW_PRIVS,
        PR_GET_PDEATHSIG,
        PR_GET_SECCOMP,
        PR_GET_SPECULATION_CTRL,
        PR_GET_TIMERSLACK,
        PR_GET_TSC,
        PR_GET_UNALIGN,
        PR_MCE_KILL,
        PR_MCE_KILL_GET,
        PR_SET_CHILD_SUBREAPER,
        PR_SET_DUMPABLE,
        PR_SET_KEEPCAPS,
        PR_SET_NAME,
        PR_SET_NO_NEW_PRIVS,
        PR_SET_PDEATHSIG,
        PR_SET_PTRACER,
        PR_SET_SECCOMP,
        PR_SET_SPECULATION_CTRL,
        PR_SET_TIMERSLACK,
        PR_SET_TSC,
        PR_TSC_ENABLE,
        PR_TSC_SIGSEGV,
    },
    event::Switchable,
    kernel_abi::SupportedArch,
    kernel_metadata::{is_sigreturn, syscall_name},
    log::{LogDebug, LogWarn},
    registers::{with_converted_registers, Registers},
    remote_ptr::{RemotePtr, Void},
    seccomp_filter_rewriter::SECCOMP_MAGIC_SKIP_ORIGINAL_SYSCALLNO,
    session::task::{
        record_task::RecordTask,
        task_common::{read_mem, read_val_mem, write_mem, write_val_mem},
        Task,
        TaskSharedWeakPtr,
    },
    taskish_uid::TaskUid,
    trace::trace_task_event::TraceTaskEvent,
};
use libc::{EINVAL, ENOSYS, SECCOMP_MODE_FILTER, SECCOMP_MODE_STRICT};
use std::{
    cell::RefCell,
    cmp::{max, min},
    convert::TryInto,
    mem::size_of,
    rc::Rc,
};

/// Prepare |t| to enter its current syscall event.  Return ALLOW_SWITCH if
/// a context-switch is allowed for |t|, PREVENT_SWITCH if not.
pub fn rec_prepare_syscall(t: &mut RecordTask) -> Switchable {
    if t.syscall_state.is_none() {
        let mut new_ts = TaskSyscallState::new(t.tuid());
        new_ts.init(t);
        t.syscall_state = Some(Rc::new(RefCell::new(new_ts)));
    }

    let s = rec_prepare_syscall_internal(t);
    let syscallno = t.ev().syscall_event().number;
    if is_sigreturn(syscallno, t.ev().syscall_event().arch()) {
        // There isn't going to be an exit event for this syscall, so remove
        // syscall_state now.
        t.syscall_state = None;
        return s;
    }

    t.syscall_state_unwrap().borrow_mut().done_preparing(t, s)
}

/// DIFF NOTE: Does not take separate TaskSyscallState param
/// as that can be gotten from t directly
fn rec_prepare_syscall_internal(t: &mut RecordTask) -> Switchable {
    let arch: SupportedArch = t.ev().syscall_event().arch();
    let regs = t.regs_ref().clone();
    with_converted_registers(&regs, arch, |converted_regs| {
        rd_arch_function_selfless!(rec_prepare_syscall_arch, arch, t, converted_regs)
    })
}

/// DIFF NOTE: Does not take separate TaskSyscallState param
/// as that can be gotten from t directly
fn rec_prepare_syscall_arch<Arch: Architecture>(
    t: &mut RecordTask,
    regs: &Registers,
) -> Switchable {
    let syscallno = t.ev().syscall_event().number;

    if t.regs_ref().original_syscallno() == SECCOMP_MAGIC_SKIP_ORIGINAL_SYSCALLNO {
        // rd vetoed this syscall. Don't do any pre-processing.
        return Switchable::PreventSwitch;
    }

    t.syscall_state_unwrap()
        .borrow_mut()
        .syscall_entry_registers = regs.clone();

    if !t.desched_rec().is_null() {
        // |t| was descheduled while in a buffered syscall.  We normally don't
        // use scratch memory for the call, because the syscallbuf itself
        // is serving that purpose. More importantly, we *can't* set up
        // scratch for |t|, because it's already in the syscall. Instead, we will
        // record the syscallbuf memory in rec_process_syscall_arch.
        //
        // However there is one case where we use scratch memory: when
        // sys_read's block-cloning path is interrupted. In that case, record
        // the scratch memory.
        if syscallno == Arch::READ && regs.arg2() == t.scratch_ptr.as_usize() {
            t.syscall_state_unwrap()
                .borrow_mut()
                .reg_parameter_with_size(
                    2,
                    ParamSize::from_syscall_result_with_size::<Arch::ssize_t>(regs.arg3()),
                    Some(ArgMode::InOutNoScratch),
                    None,
                );
        }

        return Switchable::AllowSwitch;
    }

    if syscallno < 0 {
        // Invalid syscall. Don't let it accidentally match a
        // syscall number below that's for an undefined syscall.
        t.syscall_state_unwrap().borrow_mut().expect_errno = ENOSYS;
        return Switchable::PreventSwitch;
    }

    // int prctl(int option, unsigned long arg2, unsigned long arg3, unsigned
    // long arg4, unsigned long arg5);
    if syscallno == Arch::PRCTL {
        // @TODO This is a arg1_signed() as i32 in rr
        match regs.arg1() as u32 {
            PR_GET_CHILD_SUBREAPER
            | PR_GET_ENDIAN
            | PR_GET_FPEMU
            | PR_GET_FPEXC
            | PR_GET_PDEATHSIG
            | PR_GET_UNALIGN => {
                t.syscall_state_unwrap()
                    .borrow_mut()
                    .reg_parameter::<i32>(2, None, None);
            }

            PR_GET_KEEPCAPS
            | PR_GET_NO_NEW_PRIVS
            | PR_GET_TIMERSLACK
            | PR_MCE_KILL
            | PR_MCE_KILL_GET
            | PR_SET_CHILD_SUBREAPER
            | PR_SET_KEEPCAPS
            | PR_SET_NAME
            | PR_SET_PDEATHSIG
            | PR_SET_TIMERSLACK
            | PR_CAP_AMBIENT
            | PR_CAPBSET_DROP
            | PR_CAPBSET_READ
            | PR_GET_SPECULATION_CTRL
            | PR_SET_SPECULATION_CTRL => (),

            PR_SET_DUMPABLE => {
                if regs.arg2() == 0 {
                    // Don't let processes make themselves undumpable. If a process
                    // becomes undumpable, calling perf_event_open on it fails.
                    let mut r: Registers = regs.clone();
                    r.set_arg1_signed(-1);
                    t.set_regs(&r);
                    t.syscall_state_unwrap().borrow_mut().emulate_result(0);
                    t.thread_group_mut().dumpable = false;
                } else if regs.arg2() == 1 {
                    t.thread_group_mut().dumpable = true;
                }
            }

            PR_GET_DUMPABLE => {
                t.syscall_state_unwrap()
                    .borrow_mut()
                    .emulate_result(if t.thread_group().dumpable { 1 } else { 0 });
            }

            PR_GET_SECCOMP => {
                t.syscall_state_unwrap()
                    .borrow_mut()
                    .emulate_result(t.prctl_seccomp_status as usize);
            }

            PR_GET_TSC => {
                // Prevent the actual GET_TSC call and return our emulated state.
                let mut r: Registers = regs.clone();
                r.set_arg1_signed(-1);
                t.set_regs(&r);
                t.syscall_state_unwrap().borrow_mut().emulate_result(0);
                let child_addr = t.syscall_state_unwrap().borrow_mut().reg_parameter::<i32>(
                    2,
                    Some(ArgMode::InOutNoScratch),
                    None,
                );
                let tsc_mode = t.tsc_mode;
                write_val_mem(t, child_addr, &tsc_mode, None);
            }

            PR_SET_TSC => {
                // Prevent the actual SET_TSC call.
                let mut r: Registers = regs.clone();
                r.set_arg1_signed(-1);
                t.set_regs(&r);
                let val = regs.arg2() as i32;
                if val != PR_TSC_ENABLE as i32 && val != PR_TSC_SIGSEGV as i32 {
                    t.syscall_state_unwrap()
                        .borrow_mut()
                        .emulate_result_signed(-EINVAL as isize);
                } else {
                    t.syscall_state_unwrap().borrow_mut().emulate_result(0);
                    t.tsc_mode = val;
                }
            }

            PR_GET_NAME => {
                t.syscall_state_unwrap()
                    .borrow_mut()
                    .reg_parameter_with_size(2, ParamSize::from(16), None, None);
            }

            PR_SET_NO_NEW_PRIVS => {
                // @TODO in rr there is a cast to unsigned long
                if regs.arg2() != 1 {
                    t.syscall_state_unwrap().borrow_mut().expect_errno = EINVAL;
                }
            }

            PR_SET_SECCOMP => {
                // Allow all known seccomp calls. We must allow the seccomp call
                // that rr triggers when spawning the initial tracee.
                match regs.arg2() as u32 {
                    SECCOMP_MODE_STRICT => (),
                    SECCOMP_MODE_FILTER => {
                        // If we're bootstrapping then this must be rr's own syscall
                        // filter, so just install it normally now.
                        if t.session().done_initial_exec() {
                            // Prevent the actual prctl call. We'll fix this up afterwards.
                            let mut r: Registers = regs.clone();
                            r.set_arg1_signed(-1);
                            t.set_regs(&r);
                        }
                    }
                    _ => {
                        t.syscall_state_unwrap().borrow_mut().expect_errno = EINVAL;
                    }
                }
            }

            PR_SET_PTRACER => {
                // Prevent any PR_SET_PTRACER call, but pretend it succeeded, since
                // we don't want any interference with our ptracing.
                let mut r: Registers = regs.clone();
                r.set_arg1_signed(-1);
                t.set_regs(&r);
                t.syscall_state_unwrap().borrow_mut().emulate_result(0);
            }

            _ => {
                t.syscall_state_unwrap().borrow_mut().expect_errno = EINVAL;
            }
        }

        return Switchable::PreventSwitch;
    }

    log!(
        LogDebug,
        "=====> Preparing {}",
        syscall_name(syscallno, Arch::arch())
    );

    unimplemented!()
}

pub fn rec_prepare_restart_syscall(_t: &RecordTask) {
    unimplemented!()
}

pub fn rec_process_syscall(_t: &RecordTask) {
    unimplemented!()
}

type AfterSyscallAction = Box<dyn Fn(&mut RecordTask) -> ()>;
type ArgMutator = Box<dyn Fn(&mut RecordTask, RemotePtr<Void>, Option<&mut [u8]>) -> bool>;

/// When tasks enter syscalls that may block and so must be
/// prepared for a context-switch, and the syscall params
/// include (in)outparams that point to buffers, we need to
/// redirect those arguments to scratch memory.  This allows rd
/// to serialize execution of what may be multiple blocked
/// syscalls completing "simultaneously" (from rd's
/// perspective).  After the syscall exits, we restore the data
/// saved in scratch memory to the original buffers.
///
/// Then during replay, we simply restore the saved data to the
/// tracee's passed-in buffer args and continue on.
///
/// This is implemented by having rec_prepare_syscall_arch set up
/// a record in param_list for syscall in-memory  parameter (whether
/// "in" or "out"). Then done_preparing is called, which does the actual
/// scratch setup. process_syscall_results is called when the syscall is
/// done, to write back scratch results to the real parameters and
/// clean everything up.
///
/// ... a fly in this ointment is may-block buffered syscalls.
/// If a task blocks in one of those, it will look like it just
/// entered a syscall that needs a scratch buffer.  However,
/// it's too late at that point to fudge the syscall args,
/// because processing of the syscall has already begun in the
/// kernel.  But that's OK: the syscallbuf code has already
/// swapped out the original buffer-pointers for pointers into
/// the syscallbuf (which acts as its own scratch memory).  We
/// just have to worry about setting things up properly for
/// replay.
///
/// The descheduled syscall will "abort" its commit into the
/// syscallbuf, so the outparam data won't actually be saved
/// there (and thus, won't be restored during replay).  During
/// replay, we have to restore them like we restore the
/// non-buffered-syscall scratch data. This is done by recording
/// the relevant syscallbuf record data in rec_process_syscall_arch.
///
/// DIFF NOTE: The struct is pub
pub struct TaskSyscallState {
    /// DIFF NOTE: In rr a pointer to the RecordTask is stored
    tuid: TaskUid,

    param_list: Vec<MemoryParam>,
    /// Tracks the position in t's scratch_ptr buffer where we should allocate
    /// the next scratch area.
    scratch: RemotePtr<Void>,

    after_syscall_actions: Vec<AfterSyscallAction>,

    /// DIFF NOTE: Made into an Option<>
    exec_saved_event: Option<Box<TraceTaskEvent>>,
    /// DIFF NOTE: Made into an Option<>
    emulate_wait_for_child: Option<TaskSharedWeakPtr>,

    /// Saved syscall-entry registers, used by code paths that modify the
    /// registers temporarily.
    syscall_entry_registers: Registers,

    /// When nonzero, syscall is expected to return the given errno and we should
    /// die if it does not. This is set when we detect an error condition during
    /// syscall-enter preparation.
    expect_errno: i32,

    /// When should_emulate_result is true, syscall result should be adjusted to
    /// be emulated_result.
    should_emulate_result: bool,
    /// DIFF NOTE: In rr this is a u64
    emulated_result: usize,

    /// Records whether the syscall is switchable. Only valid when
    /// preparation_done is true.
    switchable: Switchable,

    /// Whether we should write back the syscall results from scratch. Only
    /// valid when preparation_done is true.
    write_back: WriteBack,

    /// When true, this syscall has already been prepared and should not
    /// be set up again.
    preparation_done: bool,

    /// When true, the scratch area is enabled, otherwise we're letting
    /// syscall outputs be written directly to their destinations.
    /// Only valid when preparation_done is true.
    scratch_enabled: bool,

    /// Miscellaneous saved data that can be used by particular syscalls
    saved_data: Vec<u8>,
}

impl TaskSyscallState {
    // DIFF NOTE: Unlike rr, you need to specify `t` (but as a tuid) right from the beginning
    pub fn new(tuid: TaskUid) -> Self {
        Self {
            tuid,
            param_list: Default::default(),
            scratch: Default::default(),
            after_syscall_actions: Default::default(),
            exec_saved_event: Default::default(),
            emulate_wait_for_child: Default::default(),
            syscall_entry_registers: Default::default(),
            expect_errno: 0,
            should_emulate_result: false,
            emulated_result: 0,
            // Arbitrarily chosen
            switchable: Switchable::PreventSwitch,
            // Arbitrarily chosen
            write_back: WriteBack::NoWriteBack,
            preparation_done: false,
            scratch_enabled: false,
            saved_data: Default::default(),
        }
    }

    pub fn init(&mut self, t: &RecordTask) {
        assert!(self.tuid == t.tuid());

        if self.preparation_done {
            return;
        }

        self.scratch = t.scratch_ptr;
    }

    /// Identify a syscall memory parameter whose address is in register 'arg'
    /// with type T.
    /// Returns a RemotePtr to the data in the child (before scratch relocation)
    /// or null if parameters have already been prepared (the syscall is
    /// resuming).
    fn reg_parameter<T>(
        &mut self,
        arg: usize,
        maybe_mode: Option<ArgMode>,
        maybe_mutator: Option<ArgMutator>,
    ) -> RemotePtr<T> {
        RemotePtr::<T>::cast(self.reg_parameter_with_size(
            arg,
            ParamSize::from(size_of::<T>()),
            maybe_mode,
            maybe_mutator,
        ))
    }

    /// Identify a syscall memory parameter whose address is in register 'arg'
    /// with size 'size'.
    /// Returns a RemotePtr to the data in the child (before scratch relocation)
    /// or null if parameters have already been prepared (the syscall is
    /// resuming).
    fn reg_parameter_with_size(
        &mut self,
        arg: usize,
        param_size: ParamSize,
        maybe_mode: Option<ArgMode>,
        maybe_mutator: Option<ArgMutator>,
    ) -> RemotePtr<Void> {
        let mode = maybe_mode.unwrap_or(ArgMode::Out);
        if self.preparation_done {
            return RemotePtr::null();
        }

        let mut param = MemoryParam::default();
        let dest = RemotePtr::from(self.syscall_entry_registers.arg(arg));
        if dest.is_null() {
            return RemotePtr::null();
        }

        param.dest = dest;
        param.num_bytes = param_size;
        param.mode = mode;
        param.maybe_mutator = maybe_mutator;
        assert!(param.maybe_mutator.is_none() || mode == ArgMode::In);

        if mode != ArgMode::InOutNoScratch {
            param.scratch = self.scratch;
            self.scratch += param.num_bytes.incoming_size;
            align_scratch(&mut self.scratch, None);
            param.ptr_in_reg = arg;
        }

        self.param_list.push(param);

        dest
    }

    /// Identify a syscall memory parameter whose address is in memory at
    /// location 'addr_of_buf_ptr' with type T.
    /// Returns a RemotePtr to the data in the child (before scratch relocation)
    /// or null if parameters have already been prepared (the syscall is
    /// resuming).
    /// addr_of_buf_ptr must be in a buffer identified by some init_..._parameter
    /// call.
    ///
    /// DIFF NOTE: Takes t as param
    fn mem_ptr_parameter<T>(
        &mut self,
        t: &mut RecordTask,
        addr_of_buf_ptr: RemotePtr<Void>,
        maybe_mode: Option<ArgMode>,
        maybe_mutator: Option<ArgMutator>,
    ) -> RemotePtr<T> {
        RemotePtr::<T>::cast(self.mem_ptr_parameter_with_size(
            t,
            addr_of_buf_ptr,
            ParamSize::from(size_of::<T>()),
            maybe_mode,
            maybe_mutator,
        ))
    }

    /// Identify a syscall memory parameter whose address is in memory at
    /// location 'addr_of_buf_ptr' with type T.
    /// Returns a RemotePtr to the data in the child (before scratch relocation)
    /// or null if parameters have already been prepared (the syscall is
    /// resuming).
    /// addr_of_buf_ptr must be in a buffer identified by some init_..._parameter
    /// call.
    ///
    /// DIFF NOTE: Take t as param
    fn mem_ptr_parameter_inferred<Arch: Architecture, T>(
        &mut self,
        t: &mut RecordTask,
        addr_of_buf_ptr: RemotePtr<Arch::ptr<T>>,
        maybe_mode: Option<ArgMode>,
        maybe_mutator: Option<ArgMutator>,
    ) -> RemotePtr<T> {
        RemotePtr::<T>::cast(self.mem_ptr_parameter_with_size(
            t,
            RemotePtr::<Void>::cast(addr_of_buf_ptr),
            ParamSize::from(size_of::<T>()),
            maybe_mode,
            maybe_mutator,
        ))
    }

    /// Identify a syscall memory parameter whose address is in memory at
    /// location 'addr_of_buf_ptr' with size 'size'.
    /// Returns a RemotePtr to the data in the child (before scratch relocation)
    /// or null if parameters have already been prepared (the syscall is
    /// resuming).
    /// addr_of_buf_ptr must be in a buffer identified by some init_..._parameter
    /// call.
    ///
    /// DIFF NOTE: Take t as param
    fn mem_ptr_parameter_with_size(
        &mut self,
        t: &mut RecordTask,
        addr_of_buf_ptr: RemotePtr<Void>,
        param_size: ParamSize,
        maybe_mode: Option<ArgMode>,
        maybe_mutator: Option<ArgMutator>,
    ) -> RemotePtr<Void> {
        assert!(self.tuid == t.tuid());

        let mode = maybe_mode.unwrap_or(ArgMode::Out);
        if self.preparation_done || addr_of_buf_ptr.is_null() {
            return RemotePtr::null();
        }

        let mut param = MemoryParam::default();
        let dest = get_remote_ptr(t, addr_of_buf_ptr);
        if dest.is_null() {
            return RemotePtr::null();
        }

        param.dest = dest;
        param.num_bytes = param_size;
        param.mode = mode;
        param.maybe_mutator = maybe_mutator;
        ed_assert!(t, param.maybe_mutator.is_none() || mode == ArgMode::In);
        if mode != ArgMode::InOutNoScratch {
            param.scratch = self.scratch;
            self.scratch += param.num_bytes.incoming_size;
            align_scratch(&mut self.scratch, None);
            param.ptr_in_memory = addr_of_buf_ptr;
        }
        self.param_list.push(param);

        dest
    }

    fn after_syscall_action(&mut self, action: AfterSyscallAction) {
        self.after_syscall_actions.push(action)
    }

    fn emulate_result(&mut self, result: usize) {
        assert!(!self.preparation_done);
        assert!(!self.should_emulate_result);
        self.should_emulate_result = true;
        self.emulated_result = result;
    }

    fn emulate_result_signed(&mut self, result: isize) {
        assert!(!self.preparation_done);
        assert!(!self.should_emulate_result);
        self.should_emulate_result = true;
        self.emulated_result = result as usize;
    }

    /// Internal method that takes 'ptr', an address within some memory parameter,
    /// and relocates it to the parameter's location in scratch memory.
    fn relocate_pointer_to_scratch(&self, ptr: RemotePtr<Void>) -> RemotePtr<Void> {
        let mut num_relocations: usize = 0;
        let mut result = RemotePtr::<Void>::null();
        for param in &self.param_list {
            if param.dest <= ptr && ptr < param.dest + param.num_bytes.incoming_size {
                result = param.scratch + (ptr - param.dest);
                num_relocations += 1;
            }
        }
        // DIFF NOTE: These are debug_asserts in rr
        assert!(
            num_relocations > 0,
            "Pointer in non-scratch memory being updated to point to scratch?"
        );

        assert!(
            num_relocations <= 1,
            "Overlapping buffers containing relocated pointer?"
        );

        result
    }

    /// Internal method that takes the index of a MemoryParam and a vector
    /// containing the actual sizes assigned to each param < i, and
    /// computes the actual size to use for parameter param_index.
    ///
    /// DIFF NOTE: Takes t as param
    fn eval_param_size(
        &self,
        t: &mut RecordTask,
        i: usize,
        actual_sizes: &mut Vec<usize>,
    ) -> usize {
        assert_eq!(actual_sizes.len(), i);
        assert!(self.tuid == t.tuid());

        let mut already_consumed: usize = 0;
        for j in 0usize..i {
            if self.param_list[j]
                .num_bytes
                .is_same_source(&self.param_list[i].num_bytes)
            {
                already_consumed += actual_sizes[j];
            }
        }

        let size: usize = self.param_list[i].num_bytes.eval(t, already_consumed);

        actual_sizes.push(size);

        size
    }

    /// Called when all memory parameters have been identified. If 'sw' is
    /// Switchable::AllowSwitch, sets up scratch memory and updates registers etc as
    /// necessary.
    /// If scratch can't be used for some reason, returns Switchable::PreventSwitch,
    /// otherwise returns 'sw'.
    ///
    /// DIFF NOTE: Takes t as param
    fn done_preparing(&mut self, t: &mut RecordTask, mut sw: Switchable) -> Switchable {
        assert!(self.tuid == t.tuid());

        if self.preparation_done {
            return self.switchable;
        }

        sw = self.done_preparing_internal(t, sw);
        ed_assert_eq!(t, sw, self.switchable);

        // Step 3: Execute mutators. This must run even if the scratch steps do not.
        for param in &mut self.param_list {
            if param.maybe_mutator.is_some() {
                // Mutated parameters must be IN. If we have scratch space, we don't need
                // to save anything.
                let mut saved_data_loc: Option<&mut [u8]> = None;
                if !self.scratch_enabled {
                    let prev_size = self.saved_data.len();
                    self.saved_data
                        .resize(prev_size + param.num_bytes.incoming_size, 0);
                    saved_data_loc = Some(
                        &mut self.saved_data[prev_size..prev_size + param.num_bytes.incoming_size],
                    );
                }
                if !param.maybe_mutator.as_ref().unwrap()(
                    t,
                    if self.scratch_enabled {
                        param.scratch
                    } else {
                        param.dest
                    },
                    saved_data_loc,
                ) {
                    // Nothing was modified, no need to clean up when we unwind.
                    param.maybe_mutator = None;
                    if !self.scratch_enabled {
                        self.saved_data
                            .resize(self.saved_data.len() - param.num_bytes.incoming_size, 0);
                    }
                }
            }
        }

        self.switchable
    }

    /// DIFF NOTE: Takes t as param
    fn done_preparing_internal(&mut self, t: &mut RecordTask, sw: Switchable) -> Switchable {
        ed_assert!(t, !self.preparation_done);

        self.preparation_done = true;
        self.write_back = WriteBack::WriteBack;
        self.switchable = sw;

        if t.scratch_ptr.is_null() {
            return self.switchable;
        }

        ed_assert!(t, self.scratch >= t.scratch_ptr);

        if sw == Switchable::AllowSwitch && self.scratch > t.scratch_ptr + t.usable_scratch_size() {
            log!(LogWarn,
         "`{}' needed a scratch buffer of size {}, but only {} was available.  Disabling context switching: deadlock may follow.",
             t.ev().syscall_event().syscall_name(),
        self.scratch.as_usize() - t.scratch_ptr.as_usize(),
        t.usable_scratch_size());

            self.switchable = Switchable::PreventSwitch;
        }
        if self.switchable == Switchable::PreventSwitch || self.param_list.is_empty() {
            return self.switchable;
        }

        self.scratch_enabled = true;

        // Step 1: Copy all IN/IN_OUT parameters to their scratch areas
        for param in &self.param_list {
            if param.mode == ArgMode::InOut || param.mode == ArgMode::In {
                // Initialize scratch buffer with input data
                let buf = read_mem(t, param.dest, param.num_bytes.incoming_size, None);
                write_mem(t, param.scratch, &buf, None);
            }
        }
        // Step 2: Update pointers in registers/memory to point to scratch areas
        {
            let mut r: Registers = t.regs_ref().clone();
            let mut to_adjust = Vec::<(usize, RemotePtr<Void>)>::new();
            for (i, param) in self.param_list.iter().enumerate() {
                if param.ptr_in_reg != 0 {
                    r.set_arg(param.ptr_in_reg, param.scratch.as_usize());
                }
                if !param.ptr_in_memory.is_null() {
                    // Pointers being relocated must themselves be in scratch memory.
                    // We don't want to modify non-scratch memory. Find the pointer's
                    // location
                    // in scratch memory.
                    let p = self.relocate_pointer_to_scratch(param.ptr_in_memory);
                    // Update pointer to point to scratch.
                    // Note that this can only happen after step 1 is complete and all
                    // parameter data has been copied to scratch memory.
                    set_remote_ptr(t, p, param.scratch);
                }
                // If the number of bytes to record is coming from a memory location,
                // update that location to scratch.
                if !param.num_bytes.mem_ptr.is_null() {
                    to_adjust.push((i, self.relocate_pointer_to_scratch(param.num_bytes.mem_ptr)));
                }
            }

            for (i, rptr) in to_adjust {
                self.param_list[i].num_bytes.mem_ptr = rptr;
            }

            t.set_regs(&r);
        }

        self.switchable
    }

    /// Called when a syscall exits to copy results from scratch memory to their
    /// original destinations, update registers, etc.
    ///
    /// DIFF NOTE: Takes t as param
    fn process_syscall_results(&mut self, t: &mut RecordTask) {
        assert!(self.tuid == t.tuid());
        ed_assert!(t, self.preparation_done);

        // XXX what's the best way to handle failed syscalls? Currently we just
        // record everything as if it succeeded. That handles failed syscalls that
        // wrote partial results, but doesn't handle syscalls that failed with
        // EFAULT.
        let mut actual_sizes: Vec<usize> = Vec::new();
        if self.scratch_enabled {
            let scratch_num_bytes: usize = self.scratch - t.scratch_ptr;
            let child_addr = RemotePtr::<u8>::cast(t.scratch_ptr);
            let data = read_mem(t, child_addr, scratch_num_bytes, None);
            let mut r: Registers = t.regs_ref().clone();
            // Step 1: compute actual sizes of all buffers and copy outputs
            // from scratch back to their origin
            for (i, param) in self.param_list.iter().enumerate() {
                let size: usize = self.eval_param_size(t, i, &mut actual_sizes);
                if self.write_back == WriteBack::WriteBack
                    && (param.mode == ArgMode::InOut || param.mode == ArgMode::Out)
                {
                    let offset = param.scratch.as_usize() - t.scratch_ptr.as_usize();
                    let d = &data[offset..offset + size];
                    write_mem(t, param.dest, d, None);
                }
            }

            let mut memory_cleaned_up: bool = false;
            // Step 2: restore modified in-memory pointers and registers
            for param in &self.param_list {
                if param.ptr_in_reg > 0 {
                    r.set_arg(param.ptr_in_reg, param.dest.as_usize());
                }
                if !param.ptr_in_memory.is_null() {
                    memory_cleaned_up = true;
                    set_remote_ptr(t, param.ptr_in_memory, param.dest);
                }
            }
            if self.write_back == WriteBack::WriteBack {
                // Step 3: record all output memory areas
                for (i, param) in self.param_list.iter().enumerate() {
                    let size: usize = actual_sizes[i];
                    if param.mode == ArgMode::InOutNoScratch {
                        t.record_remote(param.dest, size);
                    } else if param.mode == ArgMode::InOut || param.mode == ArgMode::Out {
                        // If pointers in memory were fixed up in step 2, then record
                        // from tracee memory to ensure we record such fixes. Otherwise we
                        // can record from our local data.
                        // XXX This optimization can be improved if necessary...
                        if memory_cleaned_up {
                            t.record_remote(param.dest, size);
                        } else {
                            let offset = param.scratch.as_usize() - t.scratch_ptr.as_usize();
                            let d = &data[offset..offset + size];
                            t.record_local(param.dest, d);
                        }
                    }
                }
            }
            t.set_regs(&r);
        } else {
            // Step 1: restore all mutated memory
            for param in &self.param_list {
                if param.maybe_mutator.is_some() {
                    let size: usize = param.num_bytes.incoming_size;
                    ed_assert!(t, self.saved_data.len() >= size);
                    write_mem(t, param.dest, &self.saved_data[0..size], None);
                    self.saved_data.drain(0..size);
                }
            }

            ed_assert!(t, self.saved_data.is_empty());
            // Step 2: record all output memory areas
            for (i, param) in self.param_list.iter().enumerate() {
                let size: usize = self.eval_param_size(t, i, &mut actual_sizes);
                t.record_remote(param.dest, size);
            }
        }

        if self.should_emulate_result {
            let mut r: Registers = t.regs_ref().clone();
            r.set_syscall_result(self.emulated_result);
            t.set_regs(&r);
        }

        for action in &self.after_syscall_actions {
            action(t);
        }
    }

    /// Called when a syscall has been completely aborted to undo any changes we
    /// made.
    ///
    /// DIFF NOTE: Takes t as param
    fn abort_syscall_results(&mut self, t: &mut RecordTask) {
        assert!(self.tuid == t.tuid());
        ed_assert!(t, self.preparation_done);

        if self.scratch_enabled {
            let mut r: Registers = t.regs_ref().clone();
            // restore modified in-memory pointers and registers
            for param in &self.param_list {
                if param.ptr_in_reg != 0 {
                    r.set_arg(param.ptr_in_reg, param.dest.as_usize());
                }
                if !param.ptr_in_memory.is_null() {
                    set_remote_ptr(t, param.ptr_in_memory, param.dest);
                }
            }
            t.set_regs(&r);
        } else {
            for param in &self.param_list {
                if param.maybe_mutator.is_some() {
                    let size: usize = param.num_bytes.incoming_size;
                    ed_assert!(t, self.saved_data.len() >= size);
                    write_mem(t, param.dest, &self.saved_data[0..size], None);
                    self.saved_data.drain(0..size);
                }
            }
        }
    }
}

/// Upon successful syscall completion, each RestoreAndRecordScratch record
/// in param_list consumes num_bytes from the t->scratch_ptr
/// buffer, copying the data to remote_dest and recording the data at
/// remote_dest. If ptr_in_reg is greater than zero, updates the task's
/// ptr_in_reg register with 'remote_dest'. If ptr_in_memory is non-null,
/// updates the ptr_in_memory location with the value 'remote_dest'.
#[derive(Default)]
struct MemoryParam {
    dest: RemotePtr<Void>,
    scratch: RemotePtr<Void>,
    num_bytes: ParamSize,
    ptr_in_memory: RemotePtr<Void>,
    /// DIFF NOTE: This is an i32 in rr
    ptr_in_reg: usize,
    mode: ArgMode,
    maybe_mutator: Option<ArgMutator>,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum WriteBack {
    WriteBack,
    NoWriteBack,
}

/// Specifies how to determine the size of a syscall memory
/// parameter. There is usually an incoming size determined before the syscall
/// executes (which we need in order to allocate scratch memory), combined
/// with an optional final size taken from the syscall result or a specific
/// memory location after the syscall has executed. The minimum of the incoming
/// and final sizes is used, if both are present.
#[derive(Default, Copy, Clone)]
struct ParamSize {
    incoming_size: usize,
    /// If non-null, the size is limited by the value at this location after
    /// the syscall.
    mem_ptr: RemotePtr<Void>,
    /// Size of the value at mem_ptr or in the syscall result register.
    read_size: usize,
    /// If true, the size is limited by the value of the syscall result.
    from_syscall: bool,
}

impl From<usize> for ParamSize {
    fn from(siz: usize) -> Self {
        ParamSize {
            incoming_size: min(i32::MAX as usize, siz),
            mem_ptr: 0usize.into(),
            read_size: 0,
            from_syscall: false,
        }
    }
}

impl ParamSize {
    /// p points to a tracee location that is already initialized with a
    /// "maximum buffer size" passed in by the tracee, and which will be filled
    /// in with the size of the data by the kernel when the syscall exits.
    fn from_initialized_mem<T>(t: &mut dyn Task, p: RemotePtr<T>) -> ParamSize {
        let mut r = ParamSize::from(if p.is_null() {
            0
        } else {
            match size_of::<T>() {
                4 => read_val_mem(t, RemotePtr::<u32>::cast(p), None) as usize,
                8 => read_val_mem(t, RemotePtr::<u64>::cast(p), None)
                    .try_into()
                    .unwrap(),
                _ => {
                    ed_assert!(t, false, "Unknown read_size");
                    0
                }
            }
        });
        r.mem_ptr = RemotePtr::cast(p);
        r.read_size = size_of::<T>();

        r
    }

    /// p points to a tracee location which will be filled in with the size of
    /// the data by the kernel when the syscall exits, but the location
    /// is uninitialized before the syscall.
    fn from_mem<T>(p: RemotePtr<T>) -> ParamSize {
        let mut r = ParamSize::default();
        r.mem_ptr = RemotePtr::cast(p);
        r.read_size = size_of::<T>();

        r
    }

    /// When the syscall exits, the syscall result will be of type T and contain
    /// the size of the data. 'incoming_size', if present, is a bound on the size
    /// of the data.
    fn from_syscall_result<T>() -> ParamSize {
        let mut r = ParamSize::default();
        r.from_syscall = true;
        r.read_size = size_of::<T>();
        r
    }

    fn from_syscall_result_with_size<T>(incoming_size: usize) -> ParamSize {
        let mut r = ParamSize::from(incoming_size);
        r.from_syscall = true;
        r.read_size = size_of::<T>();
        r
    }

    /// Indicate that the size will be at most 'max'.
    fn limit_size(&self, max: usize) -> ParamSize {
        let mut r = self.clone();
        r.incoming_size = min(r.incoming_size, max);

        r
    }

    fn eval(&self, t: &mut dyn Task, already_consumed: usize) -> usize {
        let mut s: usize = self.incoming_size;
        if !self.mem_ptr.is_null() {
            let mem_size: usize;
            match self.read_size {
                4 => {
                    mem_size = read_val_mem(t, RemotePtr::<u32>::cast(self.mem_ptr), None) as usize
                }
                8 => {
                    mem_size = read_val_mem(t, RemotePtr::<u64>::cast(self.mem_ptr), None)
                        .try_into()
                        .unwrap();
                }
                _ => {
                    ed_assert!(t, false, "Unknown read_size");
                    return 0;
                }
            }

            ed_assert!(t, already_consumed <= mem_size);
            s = min(s, mem_size - already_consumed);
        }

        if self.from_syscall {
            let mut syscall_size: usize =
                max(0isize, t.regs_ref().syscall_result_signed()) as usize;
            syscall_size = match self.read_size {
                // @TODO Is this what we want?
                4 => syscall_size as u32 as usize,
                // @TODO Is this what we want?
                8 => syscall_size as u64 as usize,
                _ => {
                    ed_assert!(t, false, "Unknown read_size");
                    return 0;
                }
            };

            ed_assert!(t, already_consumed <= syscall_size);
            s = min(s, syscall_size - already_consumed);
        }

        s
    }

    /// Return true if 'other' takes its dynamic size from the same source as
    /// this.
    /// When multiple syscall memory parameters take their dynamic size from the
    /// same source, the source size is distributed among them, with the first
    /// registered parameter taking up to its max_size bytes, followed by the next,
    /// etc. This lets us efficiently record iovec buffers.
    fn is_same_source(&self, other: &ParamSize) -> bool {
        ((!self.mem_ptr.is_null() && other.mem_ptr == self.mem_ptr)
            || (self.from_syscall && other.from_syscall))
            && (self.read_size == other.read_size)
    }
}

/// Modes used to register syscall memory parameter with TaskSyscallState.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum ArgMode {
    /// Syscall memory parameter is an in-parameter only.
    /// This is only important when we want to move the buffer to scratch memory
    /// so we can modify it without making the modifications potentially visible
    /// to user code. Otherwise, such parameters can be ignored.
    In,
    /// Syscall memory parameter is out-parameter only.
    Out,
    /// Syscall memory parameter is an in-out parameter.
    InOut,
    /// Syscall memory parameter is an in-out parameter but we must not use
    /// scratch (e.g. for futexes, we must use the actual memory word).
    InOutNoScratch,
}

impl Default for ArgMode {
    fn default() -> Self {
        Self::Out
    }
}

fn set_remote_ptr_arch<Arch: Architecture>(
    t: &mut dyn Task,
    addr: RemotePtr<Void>,
    value: RemotePtr<Void>,
) {
    let typed_addr = RemotePtr::<Arch::unsigned_word>::cast(addr);
    write_val_mem(
        t,
        typed_addr,
        &Arch::as_unsigned_word(value.as_usize()),
        None,
    );
}

fn set_remote_ptr(t: &mut dyn Task, addr: RemotePtr<Void>, value: RemotePtr<Void>) {
    let arch = t.arch();
    rd_arch_function_selfless!(set_remote_ptr_arch, arch, t, addr, value);
}

fn get_remote_ptr_arch<Arch: Architecture>(
    t: &mut dyn Task,
    addr: RemotePtr<Void>,
) -> RemotePtr<Void> {
    let typed_addr = RemotePtr::<Arch::unsigned_word>::cast(addr);
    let old = read_val_mem(t, typed_addr, None);
    RemotePtr::from(old.try_into().unwrap())
}

fn get_remote_ptr(t: &mut dyn Task, addr: RemotePtr<Void>) -> RemotePtr<Void> {
    let arch = t.arch();
    rd_arch_function_selfless!(get_remote_ptr_arch, arch, t, addr)
}

fn align_scratch(scratch: &mut RemotePtr<Void>, maybe_amount: Option<usize>) {
    let amount = maybe_amount.unwrap_or(8);
    *scratch = RemotePtr::from((scratch.as_usize() + amount - 1) & !(amount - 1));
}
