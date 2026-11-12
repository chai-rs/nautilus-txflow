//! EIP-712 signing for TxFlow `/exchange` requests.
//!
//! Two flows, both over the domain `EIP712Domain(name, version, chainId, verifyingContract)`
//! with name `TxFlow-Mainnet`/`TxFlow-Testnet`, version `1` and the zero address:
//!
//! - agent key: `Agent(txflowNetwork, chainId, apiVersion, connectionId)` where
//!   `connectionId = keccak256(msgpack(action without type) ‖ nonce BE8 ‖ vault flag)`;
//! - account key: `ApproveAgent(...)`, signed over its own fields. This crate can build that
//!   signature but never sends it: approving an agent is done outside the adapter.
//!
//! Nothing here does I/O or reads keys; the caller supplies the key.

pub mod action;
pub mod nonce;

use std::{borrow::Cow, fmt, str::FromStr};

use alloy::{
    signers::{SignerSync, local::PrivateKeySigner},
    sol_types::{Eip712Domain, SolStruct},
};
use alloy_primitives::{Address, B256, Keccak256, U256};
use serde::Serialize;
use serde_json::{Value, json};

use self::action::Action;
use crate::common::{consts::SIGNING_API_VERSION, enums::TxflowEnvironment};

alloy::sol! {
    #[derive(Debug)]
    struct Agent {
        string txflowNetwork;
        uint32 chainId;
        uint32 apiVersion;
        bytes32 connectionId;
    }

    #[derive(Debug)]
    struct ApproveAgent {
        string txflowNetwork;
        uint32 chainId;
        uint32 apiVersion;
        address agentAddress;
        string agentName;
        uint64 nonce;
    }
}

/// Errors from building or signing a payload.
#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("invalid private key")]
    InvalidKey,
    #[error("MessagePack encoding failed: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("signer error: {0}")]
    Signer(#[from] alloy::signers::Error),
}

/// `signature` object of the `/exchange` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Signature {
    pub r: String,
    pub s: String,
    /// 27 or 28.
    pub v: u8,
}

/// EIP-712 domain of `environment`.
#[must_use]
pub fn domain(environment: TxflowEnvironment) -> Eip712Domain {
    Eip712Domain::new(
        Some(Cow::Borrowed(environment.network())),
        Some(Cow::Borrowed("1")),
        Some(U256::from(environment.chain_id())),
        Some(Address::ZERO),
        None,
    )
}

/// Hash of an agent-signed action: `keccak256(action ‖ nonce BE8 ‖ vault flag)`, where the
/// vault flag is `0x00`, or `0x01` followed by the 20 address bytes.
#[must_use]
pub fn connection_id(action_msgpack: &[u8], nonce: u64, vault: Option<Address>) -> B256 {
    let mut hasher = Keccak256::new();
    hasher.update(action_msgpack);
    hasher.update(nonce.to_be_bytes());
    match vault {
        Some(vault) => {
            hasher.update([1u8]);
            hasher.update(vault.as_slice());
        }
        None => hasher.update([0u8]),
    }
    hasher.finalize()
}

/// A private key bound to one TxFlow environment.
pub struct TxflowSigner {
    key: PrivateKeySigner,
    environment: TxflowEnvironment,
    domain: Eip712Domain,
}

impl TxflowSigner {
    /// `private_key` is 32 bytes of hex, with or without `0x`.
    ///
    /// # Errors
    ///
    /// Returns an error if the key is not a valid secp256k1 key. The error never carries the
    /// key text.
    pub fn new(private_key: &str, environment: TxflowEnvironment) -> Result<Self, SigningError> {
        let key =
            PrivateKeySigner::from_str(private_key.trim()).map_err(|_| SigningError::InvalidKey)?;
        Ok(Self {
            key,
            environment,
            domain: domain(environment),
        })
    }

    /// Address of the wrapped key.
    #[must_use]
    pub fn address(&self) -> Address {
        self.key.address()
    }

    /// EIP-712 digest of an agent-signed action.
    ///
    /// # Errors
    ///
    /// Returns an error if the action cannot be encoded.
    pub fn action_digest(
        &self,
        action: &Action,
        nonce: u64,
        vault: Option<Address>,
    ) -> Result<B256, SigningError> {
        let agent = Agent {
            txflowNetwork: self.environment.network().to_owned(),
            chainId: self.environment.chain_id(),
            apiVersion: SIGNING_API_VERSION,
            connectionId: connection_id(&action.msgpack()?, nonce, vault),
        };
        Ok(agent.eip712_signing_hash(&self.domain))
    }

    /// Signs an action with the agent key.
    ///
    /// # Errors
    ///
    /// Returns an error if encoding or signing fails.
    pub fn sign_action(
        &self,
        action: &Action,
        nonce: u64,
        vault: Option<Address>,
    ) -> Result<Signature, SigningError> {
        self.sign_digest(&self.action_digest(action, nonce, vault)?)
    }

    /// The signed `/exchange` body for `action`.
    ///
    /// `confirm: executed` makes the venue answer with the execution result instead of
    /// "accepted by consensus", so a failed order is known from the response.
    ///
    /// # Errors
    ///
    /// Returns an error if encoding or signing fails.
    pub fn envelope(&self, action: &Action, nonce: u64) -> Result<Value, SigningError> {
        let signature = self.sign_action(action, nonce, None)?;
        Ok(json!({
            "action": action,
            "signature": signature,
            "nonce": nonce,
            "vaultAddress": null,
            "signatureChainId": null,
            "confirm": "executed",
        }))
    }

    /// EIP-712 digest of `approveAgent`. A missing agent name is hashed as the empty string.
    #[must_use]
    pub fn approve_agent_digest(&self, agent: Address, name: Option<&str>, nonce: u64) -> B256 {
        ApproveAgent {
            txflowNetwork: self.environment.network().to_owned(),
            chainId: self.environment.chain_id(),
            apiVersion: SIGNING_API_VERSION,
            agentAddress: agent,
            agentName: name.unwrap_or_default().to_owned(),
            nonce,
        }
        .eip712_signing_hash(&self.domain)
    }

    /// Signs `approveAgent` with the ACCOUNT key. The adapter never sends this action.
    ///
    /// # Errors
    ///
    /// Returns an error if signing fails.
    pub fn sign_approve_agent(
        &self,
        agent: Address,
        name: Option<&str>,
        nonce: u64,
    ) -> Result<Signature, SigningError> {
        self.sign_digest(&self.approve_agent_digest(agent, name, nonce))
    }

    fn sign_digest(&self, digest: &B256) -> Result<Signature, SigningError> {
        let signature = self.key.sign_hash_sync(digest)?;
        Ok(Signature {
            r: format!("0x{:064x}", signature.r()),
            s: format!("0x{:064x}", signature.s()),
            v: 27 + u8::from(signature.v()),
        })
    }
}

// Manual impl so the key can never reach logs, whatever the inner type prints.
impl fmt::Debug for TxflowSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TxflowSigner")
            .field("address", &self.key.address())
            .field("environment", &self.environment)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use alloy_primitives::{address, b256};
    use rstest::rstest;
    use serde::Serialize;

    use super::{action::tests::vector_order, *};

    /// Throwaway key of the TxFlow docs' signing vectors (32 bytes of `0xab`): public, never
    /// funded.
    pub const DOCS_KEY: &str = "0xabababababababababababababababababababababababababababababababab";

    fn signer() -> TxflowSigner {
        TxflowSigner::new(DOCS_KEY, TxflowEnvironment::Testnet).unwrap()
    }

    #[rstest]
    fn docs_key_has_the_documented_address() {
        assert_eq!(
            signer().address(),
            address!("0xe239cdc5fbe977a8a141b72194d3cf8c41bc5bc6")
        );
    }

    #[rstest]
    #[case(
        TxflowEnvironment::Mainnet,
        b256!("0x878a21f7c464d592bad925771b5ab6f52897c4321b30eb6cfdf4ea55c749ad8d")
    )]
    #[case(
        TxflowEnvironment::Testnet,
        b256!("0x839dd579adc05b1545a37213db5dd3dc6928532ddf73dfa1b53f647421060a8f")
    )]
    fn domain_separators_match_the_docs(
        #[case] environment: TxflowEnvironment,
        #[case] expected: B256,
    ) {
        assert_eq!(domain(environment).separator(), expected);
    }

    /// docs.txflow.com, Signing, test vector of the agent flow.
    #[rstest]
    fn order_action_matches_the_docs_vector() {
        let (action, nonce) = (vector_order(), 1_700_000_000_000);
        assert_eq!(
            connection_id(&action.msgpack().unwrap(), nonce, None),
            b256!("0x6546d1c3acf430a28a0b2552c66e985f6543646660874734732ae9e9daf4d806")
        );
        assert_eq!(
            signer().action_digest(&action, nonce, None).unwrap(),
            b256!("0xeae1ae63709636fb9567accd33bc5dd2717bfac4d97e9a853f91c7fb391e1cc6")
        );
        assert_eq!(
            signer().sign_action(&action, nonce, None).unwrap(),
            Signature {
                r: "0x4064848dd881985c52dacdc00f7046f1010f7f370e34620d36fcf3b2d1ecb3e0".to_owned(),
                s: "0x25c45ad67b9506ac653fc3658f47aa8b6002bfa86a31ae619b334d71aa8b4f6f".to_owned(),
                v: 27,
            }
        );
    }

    /// docs.txflow.com, Signing, test vector of the account flow.
    #[rstest]
    fn approve_agent_matches_the_docs_vector() {
        let agent = address!("0x0202020202020202020202020202020202020202");
        assert_eq!(
            signer().approve_agent_digest(agent, Some("my-trading-bot"), 2000),
            b256!("0x1df22801b48ae1402ad36b244aa245fd6e842fa5fef2c40819b1e6c661cd2212")
        );
        assert_eq!(
            signer()
                .sign_approve_agent(agent, Some("my-trading-bot"), 2000)
                .unwrap(),
            Signature {
                r: "0x4431d4afb9fcd0dcd619afd55a7044cc26cb0ee1d436a1b06d836da217842ead".to_owned(),
                s: "0x54a0755ee3d8c241235a1828a2be732d72e9c152b5de25fc7c169966de4210f7".to_owned(),
                v: 27,
            }
        );
    }

    /// TxFlow gives no vector with a vault. The hash layout (action ‖ nonce BE8 ‖ 0x01 ‖
    /// vault) is the one of Hyperliquid, so this replays the reference vector of
    /// `nautilus-hyperliquid` 0.67.0 (`signing/signers.rs`) through [`connection_id`].
    #[rstest]
    fn connection_id_with_a_vault_matches_the_hyperliquid_reference() {
        #[derive(Serialize)]
        struct Tif {
            tif: &'static str,
        }
        #[derive(Serialize)]
        struct Kind {
            limit: Tif,
        }
        #[derive(Serialize)]
        struct Order {
            a: u32,
            b: bool,
            p: &'static str,
            s: &'static str,
            r: bool,
            t: Kind,
        }
        #[derive(Serialize)]
        struct HyperliquidOrderAction {
            r#type: &'static str,
            orders: Vec<Order>,
            grouping: &'static str,
        }

        let action = HyperliquidOrderAction {
            r#type: "order",
            orders: vec![Order {
                a: 0,
                b: true,
                p: "50000",
                s: "0.1",
                r: false,
                t: Kind {
                    limit: Tif { tif: "Gtc" },
                },
            }],
            grouping: "na",
        };
        let bytes = rmp_serde::to_vec_named(&action).unwrap();
        assert_eq!(
            connection_id(
                &bytes,
                1_640_995_200_000,
                Some(address!("0xAbCdEf0123456789AbCdEf0123456789AbCdEf01"))
            ),
            b256!("0xedc33e36cec99166e20ea113da7e7b028cb94efda22813f814752d719a272757")
        );
    }

    #[rstest]
    fn envelope_has_the_documented_shape() {
        let envelope = signer()
            .envelope(&vector_order(), 1_700_000_000_000)
            .unwrap();
        assert_eq!(envelope["action"]["type"], "order");
        assert_eq!(envelope["nonce"], 1_700_000_000_000_u64);
        assert_eq!(envelope["signature"]["v"], 27);
        assert!(envelope["vaultAddress"].is_null() && envelope["signatureChainId"].is_null());
        assert_eq!(envelope["confirm"], "executed");
    }

    #[rstest]
    #[case("")]
    #[case("0x1234")]
    #[case("not a key")]
    #[case("0x0000000000000000000000000000000000000000000000000000000000000000")]
    fn bad_keys_are_rejected_without_echoing_them(#[case] key: &str) {
        let error = TxflowSigner::new(key, TxflowEnvironment::Mainnet).unwrap_err();
        assert_eq!(error.to_string(), "invalid private key");
    }

    #[rstest]
    fn debug_output_hides_the_key() {
        let text = format!("{:?}", signer());
        assert!(!text.contains("abab"));
        assert!(text.contains("address"));
    }
}
