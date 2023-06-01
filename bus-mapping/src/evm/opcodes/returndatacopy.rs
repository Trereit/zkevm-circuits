use crate::{
    circuit_input_builder::{
        CircuitInputStateRef, CopyDataType, CopyEvent, ExecStep, NumberOrHash,
    },
    evm::Opcode,
    operation::{CallContextField, MemoryOp, RW},
    Error,
};
use eth_types::GethExecStep;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Returndatacopy;

impl Opcode for Returndatacopy {
    fn gen_associated_ops(
        state: &mut CircuitInputStateRef,
        geth_steps: &[GethExecStep],
    ) -> Result<Vec<ExecStep>, Error> {
        let geth_step = &geth_steps[0];
        let mut exec_steps = vec![gen_returndatacopy_step(state, geth_step)?];

        // reconstruction
        let geth_step = &geth_steps[0];
        let dst_offset = geth_step.stack.nth_last(0)?;
        let src_offset = geth_step.stack.nth_last(1)?;
        let length = geth_step.stack.nth_last(2)?;

        // can we reduce this clone?
        let call_ctx = state.call_ctx_mut()?;
        let memory = &mut call_ctx.memory;

        memory.copy_from(dst_offset, src_offset, length, &call_ctx.return_data);

        let copy_event = gen_copy_event(state, geth_step)?;
        state.push_copy(&mut exec_steps[0], copy_event);
        Ok(exec_steps)
    }
}

fn gen_returndatacopy_step(
    state: &mut CircuitInputStateRef,
    geth_step: &GethExecStep,
) -> Result<ExecStep, Error> {
    let mut exec_step = state.new_step(geth_step)?;
    let memory_offset = geth_step.stack.nth_last(0)?;
    let data_offset = geth_step.stack.nth_last(1)?;
    let length = geth_step.stack.nth_last(2)?;

    state.stack_read(
        &mut exec_step,
        geth_step.stack.nth_last_filled(0),
        memory_offset,
    )?;
    state.stack_read(
        &mut exec_step,
        geth_step.stack.nth_last_filled(1),
        data_offset,
    )?;
    state.stack_read(&mut exec_step, geth_step.stack.nth_last_filled(2), length)?;

    let call_id = state.call()?.call_id;
    let call_ctx = state.call_ctx()?;
    let return_data_len = call_ctx.return_data.len();
    let last_callee_id = state.call()?.last_callee_id;
    let last_callee_return_data_offset = state.call()?.last_callee_return_data_offset;
    let last_callee_return_data_length = state.call()?.last_callee_return_data_length;

    assert_eq!(
        last_callee_return_data_length as usize, return_data_len,
        "callee return data size should be correct"
    );

    // read last callee info
    for (field, value) in [
        (CallContextField::LastCalleeId, last_callee_id.into()),
        (
            CallContextField::LastCalleeReturnDataOffset,
            last_callee_return_data_offset.into(),
        ),
        (
            CallContextField::LastCalleeReturnDataLength,
            return_data_len.into(),
        ),
    ] {
        state.call_context_read(&mut exec_step, call_id, field, value);
    }
    Ok(exec_step)
}

fn gen_copy_event(
    state: &mut CircuitInputStateRef,
    geth_step: &GethExecStep,
) -> Result<CopyEvent, Error> {
    let rw_counter_start = state.block_ctx.rwc;

    // Get low Uint64 of offset.
    let dst_addr = geth_step.stack.nth_last(0)?.low_u64();
    let data_offset = geth_step.stack.nth_last(1)?.as_u64();
    let length = geth_step.stack.nth_last(2)?.as_u64();

    let last_callee_return_data_offset = state.call()?.last_callee_return_data_offset;
    let last_callee_return_data_length = state.call()?.last_callee_return_data_length;
    let (src_addr, src_addr_end) = (
        last_callee_return_data_offset + data_offset,
        last_callee_return_data_offset + last_callee_return_data_length,
    );

    let mut exec_step = state.new_step(geth_step)?;

    let (read_steps, write_steps) = state.gen_copy_steps_for_return_data(
        &mut exec_step,
        src_addr,
        src_addr_end,
        dst_addr,
        length,
    )?;

    Ok(CopyEvent {
        src_type: CopyDataType::Memory,
        src_id: NumberOrHash::Number(state.call()?.last_callee_id),
        src_addr,
        src_addr_end,
        dst_type: CopyDataType::Memory,
        dst_id: NumberOrHash::Number(state.call()?.call_id),
        dst_addr,
        log_id: None,
        rw_counter_start,
        bytes: read_steps,
        aux_bytes: Some(write_steps),
    })
}

#[cfg(test)]
mod return_tests {
    use crate::mock::BlockData;
    use eth_types::{bytecode, geth_types::GethData, word};
    use mock::{
        test_ctx::helpers::{account_0_code_account_1_no_code, tx_from_1_to_0},
        TestContext,
    };

    #[test]
    fn test_ok() {
        // // deployed contract
        // PUSH1 0x20
        // PUSH1 0
        // PUSH1 0
        // CALLDATACOPY
        // PUSH1 0x20
        // PUSH1 0
        // RETURN
        //
        // bytecode: 0x6020600060003760206000F3
        //
        // // constructor
        // PUSH12 0x6020600060003760206000F3
        // PUSH1 0
        // MSTORE
        // PUSH1 0xC
        // PUSH1 0x14
        // RETURN
        //
        // bytecode: 0x6B6020600060003760206000F3600052600C6014F3
        let code = bytecode! {
            PUSH21(word!("6B6020600060003760206000F3600052600C6014F3"))
            PUSH1(0)
            MSTORE

            PUSH1 (0x15)
            PUSH1 (0xB)
            PUSH1 (0)
            CREATE

            PUSH1 (0x20)
            PUSH1 (0x20)
            PUSH1 (0x20)
            PUSH1 (0)
            PUSH1 (0)
            DUP6
            PUSH2 (0xFFFF)
            CALL

            PUSH1 (0x20)
            PUSH1 (0)
            PUSH1 (0x40)
            RETURNDATACOPY

            STOP
        };
        // Get the execution steps from the external tracer
        let block: GethData = TestContext::<2, 1>::new(
            None,
            account_0_code_account_1_no_code(code),
            tx_from_1_to_0,
            |block, _tx| block.number(0xcafeu64),
        )
        .unwrap()
        .into();

        let mut builder = BlockData::new_from_geth_data(block.clone()).new_circuit_input_builder();
        builder
            .handle_block(&block.eth_block, &block.geth_traces)
            .unwrap();
        let copy_events = builder.block.copy_events.clone();
    }
}
