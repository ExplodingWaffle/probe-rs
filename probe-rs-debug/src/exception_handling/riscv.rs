use std::ops::ControlFlow;

use crate::{
    DebugError, DebugInfo, DebugRegisters, StackFrame, get_object_reference,
    unwind_pc_without_debuginfo, unwind_program_counter_register,
};

use gimli::{RegisterRule, UnwindTableRow};
use probe_rs::{InstructionSet, MemoryInterface, RegisterRole, RegisterValue};

use super::{ExceptionInfo, ExceptionInterface};

pub struct RiscvExceptionHandler;

impl RiscvExceptionHandler {
    fn unwind_registers(
        &self,
        memory: &mut dyn MemoryInterface,
        unwind_registers: &mut DebugRegisters,
    ) -> Result<(), DebugError> {
        // Current register values.
        let sp = unwind_registers.get_register_value_by_role(&RegisterRole::StackPointer)?;

        if sp < 8 {
            // Stack pointer is too low, cannot unwind.
            return Err(DebugError::Other(
                "Stack pointer is too low to unwind".to_string(),
            ));
        }

        let mut stack_frame = [0; 2];
        memory.read_32(sp - 8, &mut stack_frame)?;

        let [caller_sp, return_addr] = stack_frame;

        // TODO: use an architecture-appropriate value?
        if (caller_sp as u64).saturating_sub(sp) > 0x1000_0000 {
            // Stack pointer is too far away from the current stack pointer.
            return Err(DebugError::Other(
                "Stack pointer is too far away to unwind".to_string(),
            ));
        }

        // TODO: unwind other registers as well.
        let regs_from_current_frame = [
            (RegisterRole::ReturnAddress, return_addr),
            (RegisterRole::StackPointer, caller_sp),
        ];

        for (role, value) in regs_from_current_frame {
            let reg = unwind_registers.get_register_mut_by_role(&role).unwrap();
            reg.value = Some(RegisterValue::from(value));
        }

        Ok(())
    }
}

impl ExceptionInterface for RiscvExceptionHandler {
    fn unwind_without_debuginfo(
        &self,
        unwind_registers: &mut DebugRegisters,
        frame_pc: u64,
        _stack_frames: &[StackFrame],
        instruction_set: Option<probe_rs::InstructionSet>,
        memory: &mut dyn MemoryInterface,
    ) -> ControlFlow<Option<DebugError>> {
        // Use the default method to unwind PC.
        unwind_pc_without_debuginfo(unwind_registers, frame_pc, instruction_set)?;

        // TODO: this should be automatically handled by the caller.
        match self.unwind_registers(memory, unwind_registers) {
            Ok(_) => ControlFlow::Continue(()),
            Err(error) => ControlFlow::Break(Some(error)),
        }
    }
    fn exception_details(
        &self,
        memory: &mut dyn MemoryInterface,
        stackframe_registers: &DebugRegisters,
        debug_info: &DebugInfo,
    ) -> Result<Option<ExceptionInfo>, DebugError> {
        // how to figure out if we are in an exception handler?
        // check register rule for lr=xtvec
        let ra = stackframe_registers.get_return_address().unwrap();
        println!("ra={ra:?}");

        let mut unwind_context = Box::new(gimli::UnwindContext::new());
        let frame_pc = stackframe_registers.get_program_counter().unwrap();

        let frame_pc: u64 = frame_pc.value.unwrap().try_into().unwrap();

        let unwind_info: &UnwindTableRow<usize> = crate::debug_info::get_unwind_info(
            &mut unwind_context,
            &debug_info.frame_section,
            frame_pc,
        )
        .unwrap();

        let register_rule = ra
            .dwarf_id
            .map(|register_position| unwind_info.register(gimli::Register(register_position)))
            .unwrap_or(RegisterRule::Undefined);

        println!("rule for ra: {register_rule:?}");

        // find RegisterRule::Register(reg)
        if let RegisterRule::Register(gimli::Register(0x1341)) = register_rule {
            // ra is stored in mtvec, so we are in an exception handler
            let raw_exception = self.raw_exception(stackframe_registers)?; //TODO
            let description = self.exception_description(raw_exception, memory)?; //TODO
            let registers =
                self.calling_frame_registers(memory, stackframe_registers, raw_exception)?; //TODO?!

            let exception_frame_pc =
                registers.get_register_value_by_role(&RegisterRole::ProgramCounter)?;

            let handler_frame = StackFrame {
                id: get_object_reference(),
                function_name: description.clone(),
                source_location: None,
                registers,
                pc: RegisterValue::U32(exception_frame_pc as u32),
                frame_base: None,
                is_inlined: false,
                local_variables: None,
                canonical_frame_address: None,
            };

            //TODO update SP as in v6m+v7m?

            Ok(Some(ExceptionInfo {
                raw_exception,
                description,
                handler_frame,
            }))
        } else {
            Ok(None)
        }
    }

    fn calling_frame_registers(
        &self,
        memory: &mut dyn MemoryInterface,
        stackframe_registers: &crate::DebugRegisters,
        _raw_exception: u32,
    ) -> Result<crate::DebugRegisters, DebugError> {
        //todo: eliminate this clone by passing a mutable reference and updating in place??
        let mut unwind_registers = stackframe_registers.clone();
        let program_counter = unwind_registers.get_program_counter_mut().unwrap();
        let unwound_return_address = stackframe_registers
            .get_register_by_role(&RegisterRole::Other("mepc"))
            .ok()
            .and_then(|reg| reg.value);

        let mut register_rule_string = "PC=(unwound MEPC) (dwarf Undefined)".to_string();
        let current_pc = stackframe_registers
            .get_program_counter()
            .unwrap()
            .value
            .unwrap()
            .try_into()
            .unwrap();
        let instruction_set = Some(InstructionSet::RV32C); // TODO: figure out how to get the instruction set for RISC-V

        program_counter.value = unwound_return_address.and_then(|return_address| {
            unwind_program_counter_register(
                return_address,
                current_pc,
                instruction_set,
                &mut register_rule_string,
            )
        });

        // self.unwind_registers(memory, &mut unwind_registers)?;

        Ok(unwind_registers)
    }

    fn raw_exception(
        &self,
        stackframe_registers: &crate::DebugRegisters,
    ) -> Result<u32, DebugError> {
        Ok(stackframe_registers.get_register_value_by_role(&RegisterRole::Other("mcause"))? as u32)
    }

    fn exception_description(
        &self,
        raw_exception: u32,
        _memory: &mut dyn MemoryInterface,
    ) -> Result<String, DebugError> {
        let reason = TrapReason::from(raw_exception);
        // just debug print for now
        match reason {
            TrapReason::Interrupt(interrupt) => Ok(format!("Interrupt: {interrupt:?}")),
            TrapReason::Exception(exception) => Ok(format!("Exception: {exception:?}")),
        }
        // Ok(format!("RISC-V Exception: mcause={:#x}", raw_exception))
        // Ok("Exception".to_string())
        // Err(DebugError::NotImplemented("exception description"))
    }
}

#[derive(Debug)]
enum TrapReason {
    Interrupt(InterruptReason),
    Exception(ExceptionReason),
}

//riscv priv table 46, includes H extension additions
#[derive(Debug)]
enum InterruptReason {
    SupervisorSoftware,
    VirtualSupervisorSoftware,
    MachineSoftware,
    SupervisorTimer,
    VirtualSupervisorTimer,
    MachineTimer,
    SupervisorExternal,
    VirtualSupervisorExternal,
    MachineExternal,
    SupervisorGuestExternal,
    CounterOverflow,
    Platform(u32),
    Reserved(u32),
}

#[derive(Debug)]
enum ExceptionReason {
    InstructionAddressMisaligned,
    InstructionAccessFault,
    IllegalInstruction,
    Breakpoint,
    LoadAddressMisaligned,
    LoadAccessFault,
    StoreAMOAddressMisaligned,
    StoreAMOAccessFault,
    EnvironmentCallFromUMode,
    EnvironmentCallFromHSMode,
    EnvironmentCallFromSMode,
    EnvironmentCallFromMMode,
    InstructionPageFault,
    LoadPageFault,
    StoreAMOPageFault,
    DoubleTrap,
    SoftwareCheck,
    HardwareError,
    InstructionGuestPageFault,
    LoadGuestPageFault,
    VirtualInstruction,
    StoreAMOGuestPageFault,
    Custom(u32),
    Reserved(u32),
}

impl From<u32> for TrapReason {
    fn from(value: u32) -> Self {
        // The most significant bit indicates whether it's an interrupt (1) or exception (0).
        if (value & 0x8000_0000) == 1 {
            let interrupt_code = value & 0x7FFF_FFFF;
            TrapReason::Interrupt(match interrupt_code {
                1 => InterruptReason::SupervisorSoftware,
                2 => InterruptReason::VirtualSupervisorSoftware,
                3 => InterruptReason::MachineSoftware,
                5 => InterruptReason::SupervisorTimer,
                6 => InterruptReason::VirtualSupervisorTimer,
                7 => InterruptReason::MachineTimer,
                9 => InterruptReason::SupervisorExternal,
                10 => InterruptReason::VirtualSupervisorExternal,
                11 => InterruptReason::MachineExternal,
                12 => InterruptReason::SupervisorGuestExternal,
                13 => InterruptReason::CounterOverflow,
                code @ 16.. => InterruptReason::Platform(code),
                code => InterruptReason::Reserved(code),
            })
        } else {
            let exception_code = value & 0x7FFF_FFFF;
            TrapReason::Exception(match exception_code {
                0 => ExceptionReason::InstructionAddressMisaligned,
                1 => ExceptionReason::InstructionAccessFault,
                2 => ExceptionReason::IllegalInstruction,
                3 => ExceptionReason::Breakpoint,
                4 => ExceptionReason::LoadAddressMisaligned,
                5 => ExceptionReason::LoadAccessFault,
                6 => ExceptionReason::StoreAMOAddressMisaligned,
                7 => ExceptionReason::StoreAMOAccessFault,
                8 => ExceptionReason::EnvironmentCallFromUMode,
                9 => ExceptionReason::EnvironmentCallFromHSMode,
                10 => ExceptionReason::EnvironmentCallFromSMode,
                11 => ExceptionReason::EnvironmentCallFromMMode,
                12 => ExceptionReason::InstructionPageFault,
                13 => ExceptionReason::LoadPageFault,
                15 => ExceptionReason::StoreAMOPageFault,
                16 => ExceptionReason::DoubleTrap,
                18 => ExceptionReason::SoftwareCheck,
                19 => ExceptionReason::HardwareError,
                20 => ExceptionReason::InstructionGuestPageFault,
                21 => ExceptionReason::LoadGuestPageFault,
                22 => ExceptionReason::VirtualInstruction,
                23 => ExceptionReason::StoreAMOGuestPageFault,
                code @ 24..=31 | code @ 48..=63 => ExceptionReason::Custom(code),
                code => ExceptionReason::Reserved(code),
            })
        }
    }
}
