pub use ethereum_types::{H160, H256, U256};
use {crate::util::bytes::Bytes, derive_more::From, web3::types::AccessList};

/// A contract address.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContractAddress(pub H160);

/// An ERC20 token address.
///
/// https://eips.ethereum.org/EIPS/eip-20
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TokenAddress(pub H160);

impl From<H160> for TokenAddress {
    fn from(inner: H160) -> Self {
        Self(inner)
    }
}

/// The WETH token (or equivalent) for the EVM compatible network.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WethAddress(pub H160);

/// An asset on the Ethereum blockchain. Represents a particular amount of a
/// particular token.
#[derive(Debug, Clone, Copy)]
pub struct Asset {
    pub amount: U256,
    pub token: TokenAddress,
}

/// An Ether amount in wei.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Ether(pub U256);

/// A token amount in wei, always representing the sell token of an order.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, From)]
pub struct SellTokenAmount(pub U256);

/// Like [`Gas`] but can be negative to encode a gas discount.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SignedGas(i64);

impl From<i64> for SignedGas {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

/// Gas amount.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gas(pub U256);

impl std::ops::Add<SignedGas> for Gas {
    type Output = Self;

    fn add(self, rhs: SignedGas) -> Self::Output {
        if rhs.0.is_positive() {
            Self(self.0.saturating_add(rhs.0.into()))
        } else {
            Self(self.0.saturating_sub(rhs.0.abs().into()))
        }
    }
}

/// A 256-bit rational type.
pub type Rational = num::rational::Ratio<U256>;

/// An address. Can be an EOA or a smart contract address.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address(pub H160);

/// An onchain transaction.
#[derive(Debug, Clone)]
pub struct Tx {
    pub from: Address,
    pub to: Address,
    pub value: Ether,
    pub input: Bytes<Vec<u8>>,
    pub access_list: AccessList,
}

/// An arbitrary ethereum interaction that is required for the settlement
/// execution.
#[derive(Debug)]
pub struct Interaction {
    pub target: Address,
    pub value: Ether,
    pub calldata: Vec<u8>,
}
