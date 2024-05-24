use core::fmt::Display;

use crate::primitives::{EVMError, HaltReason, InvalidTransaction};

use super::OptimismChainSpec;

/// Optimism transaction validation error.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum InvalidOptimismTransaction {
    Base(InvalidTransaction),
    /// System transactions are not supported post-regolith hardfork.
    ///
    /// Before the Regolith hardfork, there was a special field in the `Deposit` transaction
    /// type that differentiated between `system` and `user` deposit transactions. This field
    /// was deprecated in the Regolith hardfork, and this error is thrown if a `Deposit` transaction
    /// is found with this field set to `true` after the hardfork activation.
    ///
    /// In addition, this error is internal, and bubbles up into a [HaltReason::FailedDeposit] error
    /// in the `revm` handler for the consumer to easily handle. This is due to a state transition
    /// rule on OP Stack chains where, if for any reason a deposit transaction fails, the transaction
    /// must still be included in the block, the sender nonce is bumped, the `mint` value persists, and
    /// special gas accounting rules are applied. Normally on L1, [EVMError::Transaction] errors
    /// are cause for non-inclusion, so a special [HaltReason] variant was introduced to handle this
    /// case for failed deposit transactions.
    DepositSystemTxPostRegolith,
    /// Deposit transaction haults bubble up to the global main return handler, wiping state and
    /// only increasing the nonce + persisting the mint value.
    ///
    /// This is a catch-all error for any deposit transaction that is results in a [HaltReason] error
    /// post-regolith hardfork. This allows for a consumer to easily handle special cases where
    /// a deposit transaction fails during validation, but must still be included in the block.
    ///
    /// In addition, this error is internal, and bubbles up into a [HaltReason::FailedDeposit] error
    /// in the `revm` handler for the consumer to easily handle. This is due to a state transition
    /// rule on OP Stack chains where, if for any reason a deposit transaction fails, the transaction
    /// must still be included in the block, the sender nonce is bumped, the `mint` value persists, and
    /// special gas accounting rules are applied. Normally on L1, [EVMError::Transaction] errors
    /// are cause for non-inclusion, so a special [HaltReason] variant was introduced to handle this
    /// case for failed deposit transactions.
    HaltedDepositPostRegolith,
    /// L1 block info is missing for a non-deposit transaction.
    MissingL1BlockInfo,
    /// L1 block info is provided for a deposit transaction.
    UnexpectedL1BlockInfo,
}

impl Display for InvalidOptimismTransaction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Base(error) => error.fmt(f),
            Self::DepositSystemTxPostRegolith => {
                write!(
                    f,
                    "deposit system transactions post regolith hardfork are not supported"
                )
            }
            Self::HaltedDepositPostRegolith => {
                write!(
                    f,
                    "deposit transaction halted post-regolith; error will be bubbled up to main return handler"
                )
            }
            Self::MissingL1BlockInfo => {
                write!(f, "non-deposit transaction is missing L1 block info")
            }
            Self::UnexpectedL1BlockInfo => {
                write!(f, "deposit transaction has unexpected L1 block info")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for InvalidOptimismTransaction {}

impl From<InvalidTransaction> for InvalidOptimismTransaction {
    fn from(value: InvalidTransaction) -> Self {
        Self::Base(value)
    }
}

impl<DBError> From<InvalidOptimismTransaction> for EVMError<OptimismChainSpec, DBError> {
    fn from(value: InvalidOptimismTransaction) -> Self {
        Self::Transaction(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum OptimismHaltReason {
    Base(HaltReason),
    FailedDeposit,
}

impl From<HaltReason> for OptimismHaltReason {
    fn from(value: HaltReason) -> Self {
        Self::Base(value)
    }
}
