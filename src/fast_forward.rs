use crate::{
    registers::Registers,
    session::task::{task_inner::ResumeRequest, Task},
};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FastForwardStatus {
    pub did_fast_forward: bool,
    pub incomplete_fast_forward: bool,
}

impl Default for FastForwardStatus {
    fn default() -> Self {
        Self::new()
    }
}

impl FastForwardStatus {
    pub fn new() -> FastForwardStatus {
        FastForwardStatus {
            did_fast_forward: false,
            incomplete_fast_forward: false,
        }
    }
}

/// Return true if the instruction at t.ip() is a string instruction
pub fn at_x86_string_instruction<T: Task>(_t: &mut T) -> bool {
    unimplemented!()
}

/// Perform one or more synchronous singlesteps of |t|. Usually just does
/// one singlestep, except when a singlestep leaves the IP unchanged (i.e. a
/// single instruction represents a loop, such as an x86 REP-prefixed string
/// instruction).
///
/// |how| must be either RESUME_SINGLESTEP or RESUME_SYSEMU_SINGLESTEP.
///
/// We always perform at least one singlestep. We stop after a singlestep if
/// one of the following is true, or will be true after one more singlestep:
/// -- Any breakpoint or watchpoint has been triggered
/// -- IP has advanced to the next instruction
/// -- One of the register states in |states| (a null-terminated list)
/// has been reached.
///
/// Spurious returns after any singlestep are also allowed.
///
/// This will not add more than one tick to t->tick_count().
///
/// Returns true if we did a fast-forward, false if we just did one regular
/// singlestep.
pub fn fast_forward_through_instruction(
    _t: &dyn Task,
    _how: ResumeRequest,
    _states: &[&Registers],
) -> FastForwardStatus {
    unimplemented!()
}

/// Return true if the instruction at t->ip(), or the instruction immediately
/// before t->ip(), could be a REP-prefixed string instruction. It's OK to
/// return true if it's not really a string instruction (though for performance
/// reasons, this should be rare).
pub fn maybe_at_or_after_x86_string_instruction(_t: &dyn Task) -> bool {
    unimplemented!()
}
