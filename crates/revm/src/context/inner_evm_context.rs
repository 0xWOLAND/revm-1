use crate::{
    db::Database,
    interpreter::{
        analysis::{to_analysed, validate_eof},
        gas, return_ok, Contract, CreateInputs, EOFCreateInputs, EOFCreateKind, Gas,
        InstructionResult, Interpreter, InterpreterResult, LoadAccountResult, SStoreResult,
        SelfDestructResult, MAX_CODE_SIZE,
    },
    journaled_state::JournaledState,
    primitives::{
        keccak256, Account, Address, AnalysisKind, Bytecode, Bytes, CreateScheme, EVMError, Env,
        Eof, HashSet, Spec,
        SpecId::{self, *},
        B256, EOF_MAGIC_BYTES, EOF_MAGIC_HASH, U256,
    },
    FrameOrResult, JournalCheckpoint, CALL_STACK_LIMIT,
};
use std::{boxed::Box, sync::Arc};

/// EVM contexts contains data that EVM needs for execution.
#[derive(Debug)]
pub struct InnerEvmContext<DB: Database> {
    /// EVM Environment contains all the information about config, block and transaction that
    /// evm needs.
    pub env: Box<Env>,
    /// EVM State with journaling support.
    pub journaled_state: JournaledState,
    /// Database to load data from.
    pub db: DB,
    /// Error that happened during execution.
    pub error: Result<(), EVMError<DB::Error>>,
    /// Used as temporary value holder to store L1 block info.
    #[cfg(feature = "optimism")]
    pub l1_block_info: Option<crate::optimism::L1BlockInfo>,
}

impl<DB: Database + Clone> Clone for InnerEvmContext<DB>
where
    DB::Error: Clone,
{
    fn clone(&self) -> Self {
        Self {
            env: self.env.clone(),
            journaled_state: self.journaled_state.clone(),
            db: self.db.clone(),
            error: self.error.clone(),
            #[cfg(feature = "optimism")]
            l1_block_info: self.l1_block_info.clone(),
        }
    }
}

impl<DB: Database> InnerEvmContext<DB> {
    pub fn new(db: DB) -> Self {
        Self {
            env: Box::default(),
            journaled_state: JournaledState::new(SpecId::LATEST, HashSet::new()),
            db,
            error: Ok(()),
            #[cfg(feature = "optimism")]
            l1_block_info: None,
        }
    }

    /// Creates a new context with the given environment and database.
    #[inline]
    pub fn new_with_env(db: DB, env: Box<Env>) -> Self {
        Self {
            env,
            journaled_state: JournaledState::new(SpecId::LATEST, HashSet::new()),
            db,
            error: Ok(()),
            #[cfg(feature = "optimism")]
            l1_block_info: None,
        }
    }

    /// Sets the database.
    ///
    /// Note that this will ignore the previous `error` if set.
    #[inline]
    pub fn with_db<ODB: Database>(self, db: ODB) -> InnerEvmContext<ODB> {
        InnerEvmContext {
            env: self.env,
            journaled_state: self.journaled_state,
            db,
            error: Ok(()),
            #[cfg(feature = "optimism")]
            l1_block_info: self.l1_block_info,
        }
    }

    /// Returns the configured EVM spec ID.
    #[inline]
    pub const fn spec_id(&self) -> SpecId {
        self.journaled_state.spec
    }

    /// Load access list for berlin hard fork.
    ///
    /// Loading of accounts/storages is needed to make them warm.
    #[inline]
    pub fn load_access_list(&mut self) -> Result<(), EVMError<DB::Error>> {
        for (address, slots) in self.env.tx.access_list.iter() {
            self.journaled_state
                .initial_account_load(*address, slots, &mut self.db)?;
        }
        Ok(())
    }

    /// Return environment.
    #[inline]
    pub fn env(&mut self) -> &mut Env {
        &mut self.env
    }

    /// Returns the error by replacing it with `Ok(())`, if any.
    #[inline]
    pub fn take_error(&mut self) -> Result<(), EVMError<DB::Error>> {
        core::mem::replace(&mut self.error, Ok(()))
    }

    /// Fetch block hash from database.
    #[inline]
    pub fn block_hash(&mut self, number: U256) -> Result<B256, EVMError<DB::Error>> {
        self.db.block_hash(number).map_err(EVMError::Database)
    }

    /// Mark account as touched as only touched accounts will be added to state.
    #[inline]
    pub fn touch(&mut self, address: &Address) {
        self.journaled_state.touch(address);
    }

    /// Loads an account into memory. Returns `true` if it is cold accessed.
    #[inline]
    pub fn load_account(
        &mut self,
        address: Address,
    ) -> Result<(&mut Account, bool), EVMError<DB::Error>> {
        self.journaled_state.load_account(address, &mut self.db)
    }

    /// Load account from database to JournaledState.
    ///
    /// Return boolean pair where first is `is_cold` second bool `exists`.
    #[inline]
    pub fn load_account_exist(
        &mut self,
        address: Address,
    ) -> Result<LoadAccountResult, EVMError<DB::Error>> {
        self.journaled_state
            .load_account_exist(address, &mut self.db)
    }

    /// Return account balance and is_cold flag.
    #[inline]
    pub fn balance(&mut self, address: Address) -> Result<(U256, bool), EVMError<DB::Error>> {
        self.journaled_state
            .load_account(address, &mut self.db)
            .map(|(acc, is_cold)| (acc.info.balance, is_cold))
    }

    /// Return account code bytes and if address is cold loaded.
    ///
    /// In case of EOF account it will return `EOF_MAGIC` (0xEF00) as code.
    #[inline]
    pub fn code(&mut self, address: Address) -> Result<(Bytes, bool), EVMError<DB::Error>> {
        self.journaled_state
            .load_code(address, &mut self.db)
            .map(|(a, is_cold)| {
                // SAFETY: safe to unwrap as load_code will insert code if it is empty.
                let code = a.info.code.as_ref().unwrap();
                if code.is_eof() {
                    (EOF_MAGIC_BYTES.clone(), is_cold)
                } else {
                    (code.original_bytes().clone(), is_cold)
                }
            })
    }

    /// Get code hash of address.
    ///
    /// In case of EOF account it will return `EOF_MAGIC_HASH`
    /// (the hash of `0xEF00`).
    #[inline]
    pub fn code_hash(&mut self, address: Address) -> Result<(B256, bool), EVMError<DB::Error>> {
        let (acc, is_cold) = self.journaled_state.load_code(address, &mut self.db)?;
        if acc.is_empty() {
            return Ok((B256::ZERO, is_cold));
        }
        if let Some(true) = acc.info.code.as_ref().map(|code| code.is_eof()) {
            return Ok((EOF_MAGIC_HASH, is_cold));
        }
        Ok((acc.info.code_hash, is_cold))
    }

    /// Load storage slot, if storage is not present inside the account then it will be loaded from database.
    #[inline]
    pub fn sload(
        &mut self,
        address: Address,
        index: U256,
    ) -> Result<(U256, bool), EVMError<DB::Error>> {
        // account is always warm. reference on that statement https://eips.ethereum.org/EIPS/eip-2929 see `Note 2:`
        self.journaled_state.sload(address, index, &mut self.db)
    }

    /// Storage change of storage slot, before storing `sload` will be called for that slot.
    #[inline]
    pub fn sstore(
        &mut self,
        address: Address,
        index: U256,
        value: U256,
    ) -> Result<SStoreResult, EVMError<DB::Error>> {
        self.journaled_state
            .sstore(address, index, value, &mut self.db)
    }

    /// Returns transient storage value.
    #[inline]
    pub fn tload(&mut self, address: Address, index: U256) -> U256 {
        self.journaled_state.tload(address, index)
    }

    /// Stores transient storage value.
    #[inline]
    pub fn tstore(&mut self, address: Address, index: U256, value: U256) {
        self.journaled_state.tstore(address, index, value)
    }

    /// Selfdestructs the account.
    #[inline]
    pub fn selfdestruct(
        &mut self,
        address: Address,
        target: Address,
    ) -> Result<SelfDestructResult, EVMError<DB::Error>> {
        self.journaled_state
            .selfdestruct(address, target, &mut self.db)
    }

    /// Make create frame.
    #[inline]
    pub fn make_eofcreate_frame(
        &mut self,
        spec_id: SpecId,
        inputs: &EOFCreateInputs,
    ) -> Result<FrameOrResult, EVMError<DB::Error>> {
        let return_error = |e| {
            Ok(FrameOrResult::new_eofcreate_result(
                InterpreterResult {
                    result: e,
                    gas: Gas::new(inputs.gas_limit),
                    output: Bytes::new(),
                },
                None,
            ))
        };

        let (input, initcode, created_address) = match &inputs.kind {
            EOFCreateKind::Opcode {
                initcode,
                input,
                created_address,
            } => (input.clone(), initcode.clone(), *created_address),
            EOFCreateKind::Tx { initdata } => {
                // get nonce from tx (if set) or from account (if not).
                // Nonce for call is bumped in deduct_caller while
                // for CREATE it is not (it is done inside exec handlers).
                let nonce = self.env.tx.nonce.unwrap_or_else(|| {
                    let caller = self.env.tx.caller;
                    self.load_account(caller)
                        .map(|(a, _)| a.info.nonce)
                        .unwrap_or_default()
                });

                // decode eof and init code.
                let Ok((eof, input)) = Eof::decode_dangling(initdata.clone()) else {
                    return return_error(InstructionResult::InvalidEOFInitCode);
                };

                if validate_eof(&eof).is_err() {
                    // TODO (EOF) new error type.
                    return return_error(InstructionResult::InvalidEOFInitCode);
                }

                (input, eof, self.env.tx.caller.create(nonce))
            }
        };

        // Check depth
        if self.journaled_state.depth() > CALL_STACK_LIMIT {
            return return_error(InstructionResult::CallTooDeep);
        }

        // Fetch balance of caller.
        let (caller_balance, _) = self.balance(inputs.caller)?;

        // Check if caller has enough balance to send to the created contract.
        if caller_balance < inputs.value {
            return return_error(InstructionResult::OutOfFunds);
        }

        // Increase nonce of caller and check if it overflows
        if self.journaled_state.inc_nonce(inputs.caller).is_none() {
            // can't happen on mainnet.
            return return_error(InstructionResult::Return);
        }

        // Load account so it needs to be marked as warm for access list.
        self.journaled_state
            .load_account(created_address, &mut self.db)?;

        // create account, transfer funds and make the journal checkpoint.
        let checkpoint = match self.journaled_state.create_account_checkpoint(
            inputs.caller,
            created_address,
            inputs.value,
            spec_id,
        ) {
            Ok(checkpoint) => checkpoint,
            Err(e) => {
                return return_error(e);
            }
        };

        let contract = Contract::new(
            input.clone(),
            // fine to clone as it is Bytes.
            Bytecode::Eof(Arc::new(initcode.clone())),
            None,
            created_address,
            inputs.caller,
            inputs.value,
        );

        let mut interpreter = Interpreter::new(contract, inputs.gas_limit, false);
        // EOF init will enable RETURNCONTRACT opcode.
        interpreter.set_is_eof_init();

        Ok(FrameOrResult::new_eofcreate_frame(
            created_address,
            checkpoint,
            interpreter,
        ))
    }

    /// If error is present revert changes, otherwise save EOF bytecode.
    pub fn eofcreate_return<SPEC: Spec>(
        &mut self,
        interpreter_result: &mut InterpreterResult,
        address: Address,
        journal_checkpoint: JournalCheckpoint,
    ) {
        // Note we still execute RETURN opcode and return the bytes.
        // In EOF those opcodes should abort execution.
        //
        // In RETURN gas is still protecting us from ddos and in oog,
        // behaviour will be same as if it failed on return.
        //
        // Bytes of RETURN will drained in `insert_eofcreate_outcome`.
        if interpreter_result.result != InstructionResult::ReturnContract {
            self.journaled_state.checkpoint_revert(journal_checkpoint);
            return;
        }

        if interpreter_result.output.len() > MAX_CODE_SIZE {
            self.journaled_state.checkpoint_revert(journal_checkpoint);
            interpreter_result.result = InstructionResult::CreateContractSizeLimit;
            return;
        }

        // deduct gas for code deployment.
        let gas_for_code = interpreter_result.output.len() as u64 * gas::CODEDEPOSIT;
        if !interpreter_result.gas.record_cost(gas_for_code) {
            self.journaled_state.checkpoint_revert(journal_checkpoint);
            interpreter_result.result = InstructionResult::OutOfGas;
            return;
        }

        // commit changes reduces depth by -1.
        self.journaled_state.checkpoint_commit();

        // decode bytecode has a performance hit, but it has reasonable restrains.
        let bytecode =
            Eof::decode(interpreter_result.output.clone()).expect("Eof is already verified");

        // eof bytecode is going to be hashed.
        self.journaled_state
            .set_code(address, Bytecode::Eof(Arc::new(bytecode)));
    }

    /// Make create frame.
    #[inline]
    pub fn make_create_frame(
        &mut self,
        spec_id: SpecId,
        inputs: &CreateInputs,
    ) -> Result<FrameOrResult, EVMError<DB::Error>> {
        let return_error = |e| {
            Ok(FrameOrResult::new_create_result(
                InterpreterResult {
                    result: e,
                    gas: Gas::new(inputs.gas_limit),
                    output: Bytes::new(),
                },
                None,
            ))
        };

        // Check depth
        if self.journaled_state.depth() > CALL_STACK_LIMIT {
            return return_error(InstructionResult::CallTooDeep);
        }

        // Prague EOF
        if spec_id.is_enabled_in(PRAGUE) && inputs.init_code.get(..2) == Some(&[0xEF, 00]) {
            return return_error(InstructionResult::CreateInitCodeStartingEF00);
        }

        // Fetch balance of caller.
        let (caller_balance, _) = self.balance(inputs.caller)?;

        // Check if caller has enough balance to send to the created contract.
        if caller_balance < inputs.value {
            return return_error(InstructionResult::OutOfFunds);
        }

        // Increase nonce of caller and check if it overflows
        let old_nonce;
        if let Some(nonce) = self.journaled_state.inc_nonce(inputs.caller) {
            old_nonce = nonce - 1;
        } else {
            return return_error(InstructionResult::Return);
        }

        // Create address
        let mut init_code_hash = B256::ZERO;
        let created_address = match inputs.scheme {
            CreateScheme::Create => inputs.caller.create(old_nonce),
            CreateScheme::Create2 { salt } => {
                init_code_hash = keccak256(&inputs.init_code);
                inputs.caller.create2(salt.to_be_bytes(), init_code_hash)
            }
        };

        // Load account so it needs to be marked as warm for access list.
        self.journaled_state
            .load_account(created_address, &mut self.db)?;

        // create account, transfer funds and make the journal checkpoint.
        let checkpoint = match self.journaled_state.create_account_checkpoint(
            inputs.caller,
            created_address,
            inputs.value,
            spec_id,
        ) {
            Ok(checkpoint) => checkpoint,
            Err(e) => {
                return return_error(e);
            }
        };

        let bytecode = Bytecode::new_raw(inputs.init_code.clone());

        let contract = Contract::new(
            Bytes::new(),
            bytecode,
            Some(init_code_hash),
            created_address,
            inputs.caller,
            inputs.value,
        );

        Ok(FrameOrResult::new_create_frame(
            created_address,
            checkpoint,
            Interpreter::new(contract, inputs.gas_limit, false),
        ))
    }

    /// Handles call return.
    #[inline]
    pub fn call_return(
        &mut self,
        interpreter_result: &InterpreterResult,
        journal_checkpoint: JournalCheckpoint,
    ) {
        // revert changes or not.
        if matches!(interpreter_result.result, return_ok!()) {
            self.journaled_state.checkpoint_commit();
        } else {
            self.journaled_state.checkpoint_revert(journal_checkpoint);
        }
    }

    /// Handles create return.
    #[inline]
    pub fn create_return<SPEC: Spec>(
        &mut self,
        interpreter_result: &mut InterpreterResult,
        address: Address,
        journal_checkpoint: JournalCheckpoint,
    ) {
        // if return is not ok revert and return.
        if !matches!(interpreter_result.result, return_ok!()) {
            self.journaled_state.checkpoint_revert(journal_checkpoint);
            return;
        }
        // Host error if present on execution
        // if ok, check contract creation limit and calculate gas deduction on output len.
        //
        // EIP-3541: Reject new contract code starting with the 0xEF byte
        if SPEC::enabled(LONDON)
            && !interpreter_result.output.is_empty()
            && interpreter_result.output.first() == Some(&0xEF)
        {
            self.journaled_state.checkpoint_revert(journal_checkpoint);
            interpreter_result.result = InstructionResult::CreateContractStartingWithEF;
            return;
        }

        // EIP-170: Contract code size limit
        // By default limit is 0x6000 (~25kb)
        if SPEC::enabled(SPURIOUS_DRAGON)
            && interpreter_result.output.len()
                > self
                    .env
                    .cfg
                    .limit_contract_code_size
                    .unwrap_or(MAX_CODE_SIZE)
        {
            self.journaled_state.checkpoint_revert(journal_checkpoint);
            interpreter_result.result = InstructionResult::CreateContractSizeLimit;
            return;
        }
        let gas_for_code = interpreter_result.output.len() as u64 * gas::CODEDEPOSIT;
        if !interpreter_result.gas.record_cost(gas_for_code) {
            // record code deposit gas cost and check if we are out of gas.
            // EIP-2 point 3: If contract creation does not have enough gas to pay for the
            // final gas fee for adding the contract code to the state, the contract
            //  creation fails (i.e. goes out-of-gas) rather than leaving an empty contract.
            if SPEC::enabled(HOMESTEAD) {
                self.journaled_state.checkpoint_revert(journal_checkpoint);
                interpreter_result.result = InstructionResult::OutOfGas;
                return;
            } else {
                interpreter_result.output = Bytes::new();
            }
        }
        // if we have enough gas we can commit changes.
        self.journaled_state.checkpoint_commit();

        // Do analysis of bytecode straight away.
        let bytecode = match self.env.cfg.perf_analyse_created_bytecodes {
            AnalysisKind::Raw => Bytecode::new_raw(interpreter_result.output.clone()),
            AnalysisKind::Analyse => {
                to_analysed(Bytecode::new_raw(interpreter_result.output.clone()))
            }
        };

        // set code
        self.journaled_state.set_code(address, bytecode);

        interpreter_result.result = InstructionResult::Return;
    }
}
