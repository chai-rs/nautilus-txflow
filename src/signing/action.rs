//! `/exchange` actions signed with the agent key.
//!
//! The node hashes the MessagePack of the action without its `type` key, with named keys in
//! the node's field order. The field order of each struct here IS that order (taken from the
//! "Action fields" list of each docs page): do not reorder fields. Null optionals are left
//! out, because an extra key changes the hash.

use serde::Serialize;

use crate::common::enums::TxflowMarginMode;

/// One signed action. JSON carries `type` first; [`Action::msgpack`] leaves it out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Action {
    Order(OrderAction),
    Cancel(CancelAction),
    CancelByCloid(CancelByCloidAction),
    Modify2(ModifyAction),
    UpdateLeverage(UpdateLeverage),
    UpdateMarginMode(UpdateMarginMode),
}

impl Action {
    /// MessagePack bytes the node hashes: the action body without `type`.
    ///
    /// # Errors
    ///
    /// Returns an error if encoding fails.
    pub fn msgpack(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        match self {
            Self::Order(action) => rmp_serde::to_vec_named(action),
            Self::Cancel(action) => rmp_serde::to_vec_named(action),
            Self::CancelByCloid(action) => rmp_serde::to_vec_named(action),
            Self::Modify2(action) => rmp_serde::to_vec_named(action),
            Self::UpdateLeverage(action) => rmp_serde::to_vec_named(action),
            Self::UpdateMarginMode(action) => rmp_serde::to_vec_named(action),
        }
    }
}

/// `order`: up to 50 orders. `builder` is not supported and never sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderAction {
    pub grouping: Grouping,
    pub orders: Vec<OrderBody>,
}

/// Order grouping. Trigger orders are rejected under `na`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Grouping {
    Na,
    NormalTrigger,
}

/// One order of an `order` action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderBody {
    /// Asset: `perpMeta.index`.
    pub a: u32,
    /// Buy.
    pub b: bool,
    /// Limit price, or the literal `bbo`.
    pub p: String,
    /// Size in base units.
    pub s: String,
    /// Reduce-only. Optional on the wire; always sent, as in the docs' test vector.
    pub r: bool,
    pub t: OrderKind,
    /// Client order id: at most 64 bytes, never starting with `0x`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub c: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub m: Option<TxflowMarginMode>,
    /// Expiry in milliseconds; required for `Gtd`.
    #[serde(rename = "expiresAt", skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

/// `t`: `{"limit":{"tif":..}}` or `{"trigger":{..}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OrderKind {
    Limit {
        tif: Tif,
    },
    #[serde(rename_all = "camelCase")]
    Trigger {
        is_market: bool,
        trigger_px: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tpsl: Option<Tpsl>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Tif {
    Gtc,
    Ioc,
    /// Post-only.
    Alo,
    Gtd,
    /// Market order with a price cap.
    FrontendMarket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tpsl {
    Tp,
    Sl,
}

/// Order family a cancel refers to. Left out for plain orders (`na`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CancelGrouping {
    Trigger,
}

/// `cancel`: up to 200 cancels by order id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CancelAction {
    pub cancels: Vec<CancelBody>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CancelBody {
    pub a: u32,
    /// Order id, hashed as an integer.
    pub o: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grouping: Option<CancelGrouping>,
}

/// `cancelByCloid`: up to 200 cancels by client order id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CancelByCloidAction {
    pub cancels: Vec<CancelByCloidBody>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CancelByCloidBody {
    pub asset: u32,
    pub cloid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grouping: Option<CancelGrouping>,
}

/// `modify2`: new price and new TOTAL size of a resting order. `tpsl` is never sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModifyAction {
    pub order: ModifyBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModifyBody {
    pub o: u64,
    /// `bbo` is not accepted here.
    pub p: String,
    pub s: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateLeverage {
    pub asset: u32,
    pub leverage: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin_mode: Option<TxflowMarginMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateMarginMode {
    pub asset: u32,
    pub is_cross: bool,
}

#[cfg(test)]
pub(crate) mod tests {
    use rstest::rstest;
    use serde_json::{Value, json};

    use super::*;

    /// The `order` action of the docs' signing vector.
    pub fn vector_order() -> Action {
        Action::Order(OrderAction {
            grouping: Grouping::Na,
            orders: vec![OrderBody {
                a: 1,
                b: true,
                p: "50000.0".to_owned(),
                s: "0.01".to_owned(),
                r: false,
                t: OrderKind::Limit { tif: Tif::Gtc },
                c: None,
                m: None,
                expires_at: None,
            }],
        })
    }

    fn trigger_order() -> Action {
        Action::Order(OrderAction {
            grouping: Grouping::NormalTrigger,
            orders: vec![OrderBody {
                a: 2,
                b: false,
                p: "1900.0".to_owned(),
                s: "0.5".to_owned(),
                r: true,
                t: OrderKind::Trigger {
                    is_market: true,
                    trigger_px: "1950.0".to_owned(),
                    tpsl: Some(Tpsl::Sl),
                },
                c: Some("O-1".to_owned()),
                m: Some(TxflowMarginMode::Cross),
                expires_at: Some(1_800_000_000_000),
            }],
        })
    }

    #[rstest]
    fn order_msgpack_matches_the_docs_vector() {
        // docs.txflow.com, Signing, "Test vectors" (agent flow)
        let expected = "82a867726f7570696e67a26e61a66f72646572739186a16101a162c3a170a735303030302e30a173a4302e3031a172c2a17481a56c696d697481a3746966a3477463";
        assert_eq!(
            alloy_primitives::hex::encode(vector_order().msgpack().unwrap()),
            expected
        );
    }

    /// JSON bodies as written in the docs' request examples.
    #[rstest]
    #[case(vector_order(), json!({"type":"order","grouping":"na","orders":[{"a":1,"b":true,"p":"50000.0","s":"0.01","r":false,"t":{"limit":{"tif":"Gtc"}}}]}))]
    #[case(trigger_order(), json!({"type":"order","grouping":"normalTrigger","orders":[{"a":2,"b":false,"p":"1900.0","s":"0.5","r":true,"t":{"trigger":{"isMarket":true,"triggerPx":"1950.0","tpsl":"sl"}},"c":"O-1","m":"cross","expiresAt":1_800_000_000_000_u64}]}))]
    #[case(
        Action::Cancel(CancelAction { cancels: vec![CancelBody { a: 1, o: 123_456_789, grouping: None }] }),
        json!({"type":"cancel","cancels":[{"a":1,"o":123_456_789}]})
    )]
    #[case(
        Action::Cancel(CancelAction { cancels: vec![CancelBody { a: 1, o: 7, grouping: Some(CancelGrouping::Trigger) }] }),
        json!({"type":"cancel","cancels":[{"a":1,"o":7,"grouping":"trigger"}]})
    )]
    #[case(
        Action::CancelByCloid(CancelByCloidAction { cancels: vec![CancelByCloidBody { asset: 1, cloid: "my-order-001".to_owned(), grouping: None }] }),
        json!({"type":"cancelByCloid","cancels":[{"asset":1,"cloid":"my-order-001"}]})
    )]
    #[case(
        Action::Modify2(ModifyAction { order: ModifyBody { o: 444_555_666, p: "52000.0".to_owned(), s: "0.03".to_owned() } }),
        json!({"type":"modify2","order":{"o":444_555_666,"p":"52000.0","s":"0.03"}})
    )]
    #[case(
        Action::UpdateLeverage(UpdateLeverage { asset: 1, leverage: 20, margin_mode: Some(TxflowMarginMode::Cross) }),
        json!({"type":"updateLeverage","asset":1,"leverage":20,"marginMode":"cross"})
    )]
    #[case(
        Action::UpdateMarginMode(UpdateMarginMode { asset: 1, is_cross: true }),
        json!({"type":"updateMarginMode","asset":1,"isCross":true})
    )]
    fn json_and_msgpack_agree(#[case] action: Action, #[case] expected: Value) {
        assert_eq!(serde_json::to_value(&action).unwrap(), expected);

        // The hashed bytes decode to the same body minus `type`
        let decoded: Value = rmp_serde::from_slice(&action.msgpack().unwrap()).unwrap();
        let mut body = expected;
        body.as_object_mut().unwrap().remove("type");
        assert_eq!(decoded, body);
    }

    /// Key order in the hashed bytes (each key as a MessagePack fixstr).
    #[rstest]
    #[case(trigger_order(), &["grouping", "orders", "a", "b", "p", "s", "r", "t", "trigger", "isMarket", "triggerPx", "tpsl", "c", "m", "expiresAt"])]
    #[case(
        Action::Cancel(CancelAction { cancels: vec![CancelBody { a: 1, o: 7, grouping: Some(CancelGrouping::Trigger) }] }),
        &["cancels", "a", "o", "grouping"]
    )]
    #[case(
        Action::CancelByCloid(CancelByCloidAction { cancels: vec![CancelByCloidBody { asset: 1, cloid: "x".to_owned(), grouping: None }] }),
        &["cancels", "asset", "cloid"]
    )]
    #[case(
        Action::Modify2(ModifyAction { order: ModifyBody { o: 1, p: "2".to_owned(), s: "3".to_owned() } }),
        &["order", "o", "p", "s"]
    )]
    #[case(
        Action::UpdateLeverage(UpdateLeverage { asset: 1, leverage: 20, margin_mode: Some(TxflowMarginMode::Isolated) }),
        &["asset", "leverage", "marginMode"]
    )]
    #[case(Action::UpdateMarginMode(UpdateMarginMode { asset: 1, is_cross: false }), &["asset", "isCross"])]
    fn msgpack_keys_keep_the_node_order(#[case] action: Action, #[case] keys: &[&str]) {
        let bytes = action.msgpack().unwrap();
        let mut from = 0;
        for key in keys {
            let mut needle = vec![0xa0 | key.len() as u8];
            needle.extend_from_slice(key.as_bytes());
            let at = bytes[from..]
                .windows(needle.len())
                .position(|window| window == needle)
                .unwrap_or_else(|| panic!("key {key} missing or out of order"));
            from += at + needle.len();
        }
    }
}
