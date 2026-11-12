//! Revokes one agent (API wallet) of a TxFlow account: use it when leaving the adapter or
//! when the agent key may have leaked. The execution client cannot trade with that agent
//! afterwards. Open orders are not cancelled: run `txflow-flatten` first.
//!
//! The venue stores agents by label. The request is `approveAgent` with the zero address and
//! the label, signed by the ACCOUNT key (the wallet), not by the agent. Nothing is sent before
//! Enter is pressed.
//!
//! Arguments: `[testnet] [LABEL]`. `testnet` as the first argv targets testnet; defaults to
//! mainnet. Without a label the unnamed agent is revoked.
//!
//! Environment variable (read at run time only, never printed):
//! - `TXFLOW_ACCOUNT_PRIVATE_KEY`: the wallet key of the account
//!   (testnet: `TXFLOW_TESTNET_ACCOUNT_PRIVATE_KEY`).
//!
//! Run with:
//!
//! ```bash
//! cargo run --bin txflow-agent-revoke -- my-trading-bot           # mainnet
//! cargo run --bin txflow-agent-revoke -- testnet my-trading-bot   # testnet
//! ```

use std::{
    env,
    io::{BufRead, Write},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::Address;
use anyhow::Context;
use nautilus_txflow::{
    common::{
        consts::{HTTP_TIMEOUT_SECS, SIGNING_API_VERSION},
        enums::TxflowEnvironment,
    },
    execution::order::{Verdict, verdicts},
    http::client::TxflowHttpClient,
    signing::TxflowSigner,
};
use serde::Deserialize;
use serde_json::{Value, json};

const ENV_ACCOUNT_PRIVATE_KEY: &str = "TXFLOW_ACCOUNT_PRIVATE_KEY";
const ENV_TESTNET_ACCOUNT_PRIVATE_KEY: &str = "TXFLOW_TESTNET_ACCOUNT_PRIVATE_KEY";

/// `/info approvedAgents` payload.
// unverified: shape from the docs' example; the endpoint was never read live.
#[derive(Debug, Deserialize)]
struct ApprovedAgents {
    agents: Vec<ApprovedAgent>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApprovedAgent {
    agent_address: String,
    #[serde(default)]
    agent_name: String,
}

/// The environment and the label, from the arguments after the program name.
fn parse_args(args: impl Iterator<Item = String>) -> (TxflowEnvironment, String) {
    let mut args = args.peekable();
    let environment = if args.next_if(|arg| arg == "testnet").is_some() {
        TxflowEnvironment::Testnet
    } else {
        TxflowEnvironment::Mainnet
    };
    (environment, args.next().unwrap_or_default())
}

/// The address stored under `label`.
///
/// # Errors
///
/// Returns an error that lists the labels of the account if `label` is not one of them: the
/// venue refuses such a revocation.
fn agent_address<'a>(agents: &'a ApprovedAgents, label: &str) -> anyhow::Result<&'a str> {
    let agent = agents.agents.iter().find(|agent| agent.agent_name == label);
    agent
        .map(|agent| agent.agent_address.as_str())
        .with_context(|| {
            let labels: Vec<_> = agents
                .agents
                .iter()
                .map(|agent| &agent.agent_name)
                .collect();
            format!("the account has no agent with the label {label:?}; its labels: {labels:?}")
        })
}

/// The signed `/exchange` body that revokes the agent stored under `label`.
///
/// # Errors
///
/// Returns an error if signing fails.
// unverified: the docs describe this body ("Revoking an agent") without a signed example. The
// signature is the one of `approveAgent`, which has a vector for another address.
fn revoke_body(
    signer: &TxflowSigner,
    environment: TxflowEnvironment,
    label: &str,
    nonce: u64,
) -> anyhow::Result<Value> {
    // The docs promise the empty label for a null name, not for an empty string
    let name = (!label.is_empty()).then_some(label);
    let signature = signer.sign_approve_agent(Address::ZERO, name, nonce)?;
    Ok(json!({
        "action": {
            "type": "approveAgent",
            "txflowNetwork": environment.network(),
            "chainId": environment.chain_id(),
            "apiVersion": SIGNING_API_VERSION,
            "agentAddress": format!("{:#x}", Address::ZERO),
            "agentName": name,
            "nonce": nonce,
        },
        "signature": signature,
        "nonce": nonce,
        "vaultAddress": null,
        "signatureChainId": null,
        "confirm": "executed",
    }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    nautilus_common::logging::ensure_logging_initialized();

    let (environment, label) = parse_args(env::args().skip(1));
    let key_var = match environment {
        TxflowEnvironment::Mainnet => ENV_ACCOUNT_PRIVATE_KEY,
        TxflowEnvironment::Testnet => ENV_TESTNET_ACCOUNT_PRIVATE_KEY,
    };
    let key = env::var(key_var).with_context(|| format!("{key_var} is not set"))?;
    let signer = TxflowSigner::new(&key, environment)?;
    let account = format!("{:#x}", signer.address());

    let http = TxflowHttpClient::new(environment, None, HTTP_TIMEOUT_SECS, None)?;
    let agents = http
        .info::<ApprovedAgents>(&json!({"type": "approvedAgents", "user": account}))
        .await;
    let agent = match &agents {
        Ok(agents) => agent_address(agents, &label)?,
        // A list the adapter cannot read must not stop the revocation of a leaked key
        Err(e) => {
            println!("The agents of the account could not be listed ({e})");
            "not listed"
        }
    };

    println!();
    println!("TxFlow agent revocation");
    println!("  environment : {environment:?}");
    println!("  account     : {account}");
    println!("  label       : {label:?}");
    println!("  agent       : {agent}");
    println!();
    println!("Orders signed by this agent are refused once it is revoked.");
    print!("\nProceed? [Enter to continue, Ctrl+C to abort] ");
    std::io::stdout().flush()?;
    std::io::stdin().lock().read_line(&mut String::new())?;

    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let body = revoke_body(&signer, environment, &label, nonce)?;
    match verdicts(&http.exchange(&body).await, 1).pop() {
        Some(Verdict::Sent) => println!("Agent {label:?} ({agent}) revoked."),
        Some(Verdict::Rejected(reason)) => anyhow::bail!("refused: {reason}"),
        Some(Verdict::Unknown(reason)) => {
            anyhow::bail!("no clear answer: {reason}; run again to see the agents of the account")
        }
        None => unreachable!("one verdict per item"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// Key of the TxFlow docs' signing vectors: public test key, never funded.
    const DOCS_KEY: &str = "0xabababababababababababababababababababababababababababababababab";

    fn signer() -> TxflowSigner {
        TxflowSigner::new(DOCS_KEY, TxflowEnvironment::Testnet).unwrap()
    }

    fn args(args: &[&str]) -> (TxflowEnvironment, String) {
        parse_args(args.iter().map(ToString::to_string))
    }

    #[rstest]
    #[case(&[], TxflowEnvironment::Mainnet, "")]
    #[case(&["testnet"], TxflowEnvironment::Testnet, "")]
    #[case(&["my-trading-bot"], TxflowEnvironment::Mainnet, "my-trading-bot")]
    #[case(&["testnet", "my-trading-bot"], TxflowEnvironment::Testnet, "my-trading-bot")]
    fn arguments_select_the_environment_and_the_label(
        #[case] given: &[&str],
        #[case] environment: TxflowEnvironment,
        #[case] label: &str,
    ) {
        assert_eq!(args(given), (environment, label.to_string()));
    }

    /// The action of the docs ("Revoking an agent") inside the documented envelope.
    #[rstest]
    fn revoke_body_is_approve_agent_with_the_zero_address() {
        let nonce = 1_757_992_743_994;
        let body = revoke_body(
            &signer(),
            TxflowEnvironment::Testnet,
            "my-trading-bot",
            nonce,
        )
        .unwrap();

        let signature = signer()
            .sign_approve_agent(Address::ZERO, Some("my-trading-bot"), nonce)
            .unwrap();
        assert_eq!(
            body,
            json!({
                "action": {
                    "type": "approveAgent",
                    "txflowNetwork": "TxFlow-Testnet",
                    "chainId": 2511,
                    "apiVersion": 1,
                    "agentAddress": "0x0000000000000000000000000000000000000000",
                    "agentName": "my-trading-bot",
                    "nonce": 1_757_992_743_994_u64,
                },
                "signature": signature,
                "nonce": 1_757_992_743_994_u64,
                "vaultAddress": null,
                "signatureChainId": null,
                "confirm": "executed",
            })
        );
        assert_ne!(
            signature,
            signer()
                .sign_approve_agent(Address::ZERO, Some("other"), nonce)
                .unwrap()
        );
    }

    #[rstest]
    fn unnamed_agent_is_revoked_with_a_null_name() {
        let body = revoke_body(&signer(), TxflowEnvironment::Mainnet, "", 2000).unwrap();
        assert!(body["action"]["agentName"].is_null());
        assert_eq!(body["action"]["txflowNetwork"], "TxFlow-Mainnet");
        assert_eq!(
            serde_json::to_value(
                signer()
                    .sign_approve_agent(Address::ZERO, None, 2000)
                    .unwrap()
            )
            .unwrap(),
            body["signature"]
        );
    }

    /// Response example of the docs' `approvedAgents` page, plus one named agent.
    #[rstest]
    fn label_must_be_one_of_the_account() {
        let agents: ApprovedAgents = serde_json::from_value(json!({
            "agents": [
                {"agentAddress": "0x0000000000000000000000000000000000000000", "agentName": ""},
                {"agentAddress": "0x0202020202020202020202020202020202020202", "agentName": "my-trading-bot"},
            ],
            "user": "0x0000000000000000000000000000000000000000",
        }))
        .unwrap();

        assert_eq!(
            agent_address(&agents, "my-trading-bot").unwrap(),
            "0x0202020202020202020202020202020202020202"
        );
        assert!(agent_address(&agents, "").is_ok());
        let error = agent_address(&agents, "gone").unwrap_err().to_string();
        assert!(error.contains(r#"["", "my-trading-bot"]"#), "{error}");
    }
}
