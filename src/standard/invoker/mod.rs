mod routines;

use super::{Config, MergeableRuntimeState, TransactGasometer};
use crate::call_create::{CallCreateTrapData, CallTrapData, CreateScheme, CreateTrapData};
use crate::{
	Capture, Context, ExitError, ExitException, ExitResult, ExitSucceed, GasedMachine,
	Gasometer as GasometerT, Invoker as InvokerT, MergeStrategy, Opcode, RuntimeBackend,
	RuntimeEnvironment, RuntimeState, TransactionContext, TransactionalBackend, Transfer,
};
use alloc::rc::Rc;
use core::cmp::min;
use core::convert::Infallible;
use primitive_types::{H160, H256, U256};
use sha3::{Digest, Keccak256};

pub trait IntoCallCreateTrap {
	type Interrupt;

	fn into_call_create_trap(self) -> Result<Opcode, Self::Interrupt>;
}

impl IntoCallCreateTrap for Opcode {
	type Interrupt = Infallible;

	fn into_call_create_trap(self) -> Result<Opcode, Infallible> {
		Ok(self)
	}
}

pub enum SubstackInvoke {
	Call { trap: CallTrapData },
	Create { trap: CreateTrapData, address: H160 },
}

pub struct TransactInvoke {
	pub create_address: Option<H160>,
	pub gas_fee: U256,
	pub gas_price: U256,
	pub caller: H160,
}

#[derive(Clone, Debug)]
pub enum TransactArgs {
	Call {
		caller: H160,
		address: H160,
		value: U256,
		data: Vec<u8>,
		gas_limit: U256,
		gas_price: U256,
		access_list: Vec<(H160, Vec<H256>)>,
	},
	Create {
		caller: H160,
		value: U256,
		init_code: Vec<u8>,
		salt: Option<H256>, // Some for CREATE2
		gas_limit: U256,
		gas_price: U256,
		access_list: Vec<(H160, Vec<H256>)>,
	},
}

impl TransactArgs {
	pub fn gas_limit(&self) -> U256 {
		match self {
			Self::Call { gas_limit, .. } => *gas_limit,
			Self::Create { gas_limit, .. } => *gas_limit,
		}
	}

	pub fn gas_price(&self) -> U256 {
		match self {
			Self::Call { gas_price, .. } => *gas_price,
			Self::Create { gas_price, .. } => *gas_price,
		}
	}

	pub fn access_list(&self) -> &Vec<(H160, Vec<H256>)> {
		match self {
			Self::Call { access_list, .. } => access_list,
			Self::Create { access_list, .. } => access_list,
		}
	}

	pub fn caller(&self) -> H160 {
		match self {
			Self::Call { caller, .. } => *caller,
			Self::Create { caller, .. } => *caller,
		}
	}

	pub fn value(&self) -> U256 {
		match self {
			Self::Call { value, .. } => *value,
			Self::Create { value, .. } => *value,
		}
	}
}

pub struct Invoker<'config> {
	config: &'config Config,
}

impl<'config> Invoker<'config> {
	pub fn new(config: &'config Config) -> Self {
		Self { config }
	}
}

impl<'config, S, G, H, Tr> InvokerT<S, G, H, Tr> for Invoker<'config>
where
	S: MergeableRuntimeState,
	G: GasometerT<S, H> + TransactGasometer<'config, S>,
	H: RuntimeEnvironment + RuntimeBackend + TransactionalBackend,
	Tr: IntoCallCreateTrap,
{
	type Interrupt = Tr::Interrupt;
	type TransactArgs = TransactArgs;
	type TransactInvoke = TransactInvoke;
	type TransactValue = (ExitSucceed, Option<H160>);
	type SubstackInvoke = SubstackInvoke;

	fn new_transact(
		&self,
		args: TransactArgs,
		handler: &mut H,
	) -> Result<(TransactInvoke, GasedMachine<S, G>), ExitError> {
		let caller = args.caller();
		let gas_price = args.gas_price();

		let gas_fee = args.gas_limit().saturating_mul(gas_price);
		handler.withdrawal(caller, gas_fee)?;

		let address = match &args {
			TransactArgs::Call { address, .. } => *address,
			TransactArgs::Create {
				caller,
				salt,
				init_code,
				..
			} => match salt {
				Some(salt) => {
					let scheme = CreateScheme::Create2 {
						caller: *caller,
						code_hash: H256::from_slice(Keccak256::digest(&init_code).as_slice()),
						salt: *salt,
					};
					scheme.address(handler)
				}
				None => {
					let scheme = CreateScheme::Legacy { caller: *caller };
					scheme.address(handler)
				}
			},
		};
		let value = args.value();

		let invoke = TransactInvoke {
			gas_fee,
			gas_price: args.gas_price(),
			caller: args.caller(),
			create_address: match &args {
				TransactArgs::Call { .. } => None,
				TransactArgs::Create { .. } => Some(address),
			},
		};

		handler.push_substate();

		let context = Context {
			caller,
			address,
			apparent_value: value,
		};
		let transaction_context = TransactionContext {
			origin: caller,
			gas_price,
		};
		let transfer = Transfer {
			source: caller,
			target: address,
			value,
		};

		let work = || -> Result<(TransactInvoke, GasedMachine<S, G>), ExitError> {
			match args {
				TransactArgs::Call {
					caller,
					address,
					data,
					gas_limit,
					access_list,
					..
				} => {
					for (address, keys) in &access_list {
						handler.mark_hot(*address, None)?;
						for key in keys {
							handler.mark_hot(*address, Some(*key))?;
						}
					}

					let code = handler.code(address);

					let gasometer =
						G::new_transact_call(gas_limit, &code, &data, &access_list, self.config)?;

					let machine = routines::make_enter_call_machine(
						self,
						code,
						data,
						false, // is_static
						Some(transfer),
						S::new_transact_call(RuntimeState {
							context,
							transaction_context: Rc::new(transaction_context),
							retbuf: Vec::new(),
							gas: U256::from(gas_limit),
						}),
						gasometer,
						handler,
					)?;

					if self.config.increase_state_access_gas {
						if self.config.warm_coinbase_address {
							let coinbase = handler.block_coinbase();
							handler.mark_hot(coinbase, None)?;
						}
						handler.mark_hot(caller, None)?;
						handler.mark_hot(address, None)?;
					}

					handler.inc_nonce(caller)?;

					Ok((invoke, machine))
				}
				TransactArgs::Create {
					caller,
					init_code,
					gas_limit,
					access_list,
					..
				} => {
					let gasometer =
						G::new_transact_create(gas_limit, &init_code, &access_list, self.config)?;

					let machine = routines::make_enter_create_machine(
						self,
						caller,
						init_code,
						false, // is_static
						transfer,
						S::new_transact_create(RuntimeState {
							context,
							transaction_context: Rc::new(transaction_context),
							retbuf: Vec::new(),
							gas: U256::from(gas_limit),
						}),
						gasometer,
						handler,
					)?;

					Ok((invoke, machine))
				}
			}
		};

		match work() {
			Ok(ret) => Ok(ret),
			Err(err) => {
				handler.pop_substate(MergeStrategy::Discard);
				Err(err)
			}
		}
	}

	fn finalize_transact(
		&self,
		invoke: &TransactInvoke,
		result: ExitResult,
		mut machine: GasedMachine<S, G>,
		handler: &mut H,
	) -> Result<Self::TransactValue, ExitError> {
		let left_gas = machine.gasometer.gas();

		let work = || -> Result<Self::TransactValue, ExitError> {
			if result.is_ok() {
				if let Some(address) = invoke.create_address {
					let retbuf = machine.machine.into_retbuf();

					routines::deploy_create_code(
						self,
						address,
						&retbuf,
						&mut machine.gasometer,
						handler,
					)?;
				}
			}

			result.map(|s| (s, invoke.create_address))
		};

		let result = work();

		let refunded_gas = match result {
			Ok(_) | Err(ExitError::Reverted) => left_gas,
			Err(_) => U256::zero(),
		};
		let refunded_fee = refunded_gas.saturating_mul(invoke.gas_price);
		let coinbase_reward = invoke.gas_fee.saturating_sub(refunded_fee);

		handler.deposit(invoke.caller, refunded_fee);
		handler.deposit(handler.block_coinbase(), coinbase_reward);

		match result {
			Ok(exit) => {
				handler.pop_substate(MergeStrategy::Commit);
				Ok(exit)
			}
			Err(err) => {
				handler.pop_substate(MergeStrategy::Discard);
				Err(err)
			}
		}
	}

	fn enter_substack(
		&self,
		trap: Tr,
		machine: &mut GasedMachine<S, G>,
		handler: &mut H,
		depth: usize,
	) -> Capture<Result<(SubstackInvoke, GasedMachine<S, G>), ExitError>, Tr::Interrupt> {
		fn l64(gas: U256) -> U256 {
			gas - gas / U256::from(64)
		}

		let opcode = match trap.into_call_create_trap() {
			Ok(opcode) => opcode,
			Err(interrupt) => return Capture::Trap(interrupt),
		};

		if depth >= self.config.call_stack_limit {
			return Capture::Exit(Err(ExitException::CallTooDeep.into()));
		}

		let trap_data = match CallCreateTrapData::new_from(opcode, &mut machine.machine) {
			Ok(trap_data) => trap_data,
			Err(err) => return Capture::Exit(Err(err)),
		};

		let after_gas = if self.config.call_l64_after_gas {
			l64(machine.gasometer.gas())
		} else {
			machine.gasometer.gas()
		};
		let target_gas = trap_data.target_gas().unwrap_or(after_gas);
		let mut gas_limit = min(after_gas, target_gas);

		match &trap_data {
			CallCreateTrapData::Call(call) if call.has_value() => {
				gas_limit = gas_limit.saturating_add(U256::from(self.config.call_stipend));
			}
			_ => (),
		}

		let is_static = if machine.is_static {
			true
		} else {
			match &trap_data {
				CallCreateTrapData::Call(CallTrapData { is_static, .. }) => *is_static,
				_ => false,
			}
		};

		let transaction_context = machine.machine.state.as_ref().transaction_context.clone();

		let code = trap_data.code(handler);
		let submeter = match machine.gasometer.submeter(gas_limit, &code) {
			Ok(submeter) => submeter,
			Err(err) => return Capture::Exit(Err(err)),
		};

		match trap_data {
			CallCreateTrapData::Call(call_trap_data) => {
				let substate = machine.machine.state.substate(RuntimeState {
					context: call_trap_data.context.clone(),
					transaction_context,
					retbuf: Vec::new(),
					gas: U256::from(gas_limit),
				});

				Capture::Exit(routines::enter_call_substack(
					self,
					code,
					call_trap_data,
					is_static,
					substate,
					submeter,
					handler,
				))
			}
			CallCreateTrapData::Create(create_trap_data) => {
				let caller = create_trap_data.scheme.caller();
				let address = create_trap_data.scheme.address(handler);
				let substate = machine.machine.state.substate(RuntimeState {
					context: Context {
						address,
						caller,
						apparent_value: create_trap_data.value,
					},
					transaction_context,
					retbuf: Vec::new(),
					gas: U256::from(gas_limit),
				});

				Capture::Exit(routines::enter_create_substack(
					self,
					code,
					create_trap_data,
					is_static,
					substate,
					submeter,
					handler,
				))
			}
		}
	}

	fn exit_substack(
		&self,
		result: ExitResult,
		child: GasedMachine<S, G>,
		trap_data: SubstackInvoke,
		parent: &mut GasedMachine<S, G>,
		handler: &mut H,
	) -> Result<(), ExitError> {
		let strategy = match &result {
			Ok(_) => MergeStrategy::Commit,
			Err(ExitError::Reverted) => MergeStrategy::Revert,
			Err(_) => MergeStrategy::Discard,
		};

		match trap_data {
			SubstackInvoke::Create { address, trap } => {
				let retbuf = child.machine.memory.into_data();
				parent.machine.state.merge(child.machine.state, strategy);

				let mut child_gasometer = child.gasometer;
				let result = result.and_then(|_| {
					routines::deploy_create_code(
						self,
						address,
						&retbuf,
						&mut child_gasometer,
						handler,
					)?;

					Ok(address)
				});

				handler.pop_substate(strategy);
				GasometerT::<S, H>::merge(&mut parent.gasometer, child_gasometer, strategy);

				trap.feedback(result, retbuf, &mut parent.machine)?;

				Ok(())
			}
			SubstackInvoke::Call { trap } => {
				parent.machine.state.merge(child.machine.state, strategy);

				let retbuf = child.machine.memory.into_data();

				handler.pop_substate(strategy);
				GasometerT::<S, H>::merge(&mut parent.gasometer, child.gasometer, strategy);

				trap.feedback(result, retbuf, &mut parent.machine)?;

				Ok(())
			}
		}
	}
}
