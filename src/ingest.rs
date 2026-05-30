//! Mapping `DecodedSwap` (P2) -> ligne `pending_tx` persistable.
//!
//! Logique **pure** (zéro network, zéro DB) : on prend ce que le décodeur de P2
//! a produit + le contexte de la tx, et on en fait une ligne plate prête à
//! insérer. Pur = testable trivialement (cf. tests en bas).

use alloy::primitives::{Address, B256, U256};
use eth_mempool_watcher::decode::DecodedSwap;
use eth_mempool_watcher::routers::Router;

/// Contexte d'une transaction pending, extrait du body reçu par WebSocket.
///
/// Découplé d'alloy côté types de tx pour rester testable sans construire une
/// vraie transaction signée.
#[derive(Clone, Debug)]
pub struct TxContext {
    pub hash: B256,
    pub from: Address,
    /// Router cible (le `to` de la tx).
    pub to: Address,
    /// `tx.value` — sert de montant d'entrée pour les swaps V2 `ETHForTokens`
    /// (où l'`amount_in` du calldata vaut 0).
    pub value: U256,
    pub max_fee_per_gas: u128,
    /// Calldata brut (on en tire le selector et la taille).
    pub input: Vec<u8>,
}

/// Une ligne plate de la table `pending_tx`. Tous les montants/adresses sont
/// déjà sérialisés en `String` (hex pour adresses/hash, décimal pour U256).
// Pas de `Eq` : `max_fee_gwei` est un `f64` (pas totalement ordonnable).
#[derive(Clone, Debug, PartialEq)]
pub struct PendingTxRow {
    pub hash: String,
    pub seen_at_ms: i64,
    pub from_addr: String,
    pub to_addr: String,
    pub router: String,
    pub kind: String,
    pub protocol: Option<String>,
    pub selector: String,
    pub token_in: Option<String>,
    pub token_out: Option<String>,
    pub amount_in_wei: Option<String>,
    pub amount_out_min: Option<String>,
    pub fee_pips: Option<i64>,
    pub max_fee_gwei: f64,
    pub input_bytes: i64,
}

/// Selector 4-byte du calldata, formaté `0x........` (ou `0x` si trop court).
fn selector_hex(input: &[u8]) -> String {
    if input.len() >= 4 {
        format!(
            "0x{:02x}{:02x}{:02x}{:02x}",
            input[0], input[1], input[2], input[3]
        )
    } else {
        "0x".to_string()
    }
}

/// Construit la ligne `pending_tx` à partir du swap décodé + contexte tx.
///
/// `seen_at_ms` est injecté (epoch millis) plutôt que lu ici, pour garder la
/// fonction pure et testable de façon déterministe.
pub fn pending_row(
    router: Router,
    decoded: &DecodedSwap,
    ctx: &TxContext,
    seen_at_ms: i64,
) -> PendingTxRow {
    let base = |kind: &str, protocol: Option<&str>| PendingTxRow {
        hash: format!("{:#x}", ctx.hash),
        seen_at_ms,
        from_addr: format!("{:#x}", ctx.from),
        to_addr: format!("{:#x}", ctx.to),
        router: router.name().to_string(),
        kind: kind.to_string(),
        protocol: protocol.map(ToString::to_string),
        selector: selector_hex(&ctx.input),
        token_in: None,
        token_out: None,
        amount_in_wei: None,
        amount_out_min: None,
        fee_pips: None,
        max_fee_gwei: ctx.max_fee_per_gas as f64 / 1e9,
        input_bytes: ctx.input.len() as i64,
    };

    match decoded {
        DecodedSwap::ExactInput {
            protocol,
            token_in,
            token_out,
            amount_in,
            amount_out_min,
            fee_pips,
            ..
        } => PendingTxRow {
            token_in: Some(format!("{token_in:#x}")),
            token_out: Some(format!("{token_out:#x}")),
            amount_in_wei: Some(amount_in.to_string()),
            amount_out_min: Some(amount_out_min.to_string()),
            fee_pips: fee_pips.map(i64::from),
            ..base("ExactInput", Some(protocol))
        },
        DecodedSwap::ExactInputPath {
            protocol,
            path,
            amount_in,
            amount_out_min,
            ..
        } => {
            // V2 `swapExactETHForTokens` : amount_in du calldata = 0 -> tx.value.
            let amount_in_eff = if *amount_in == U256::ZERO {
                ctx.value
            } else {
                *amount_in
            };
            PendingTxRow {
                token_in: path.first().map(|a| format!("{a:#x}")),
                token_out: path.last().map(|a| format!("{a:#x}")),
                amount_in_wei: Some(amount_in_eff.to_string()),
                amount_out_min: Some(amount_out_min.to_string()),
                ..base("ExactInputPath", Some(protocol))
            }
        }
        DecodedSwap::UniversalRouterEnvelope { .. } => {
            base("UniversalRouterEnvelope", Some("UniversalRouter"))
        }
        DecodedSwap::Multicall { protocol, .. } => base("Multicall", Some(protocol)),
        DecodedSwap::Unknown { .. } => base("Unknown", None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn ctx_with_input(input: Vec<u8>) -> TxContext {
        TxContext {
            hash: B256::repeat_byte(0xab),
            from: address!("1111111111111111111111111111111111111111"),
            to: address!("2222222222222222222222222222222222222222"),
            value: U256::ZERO,
            max_fee_per_gas: 25_000_000_000, // 25 gwei
            input,
        }
    }

    #[test]
    fn selector_extracted_and_max_fee_to_gwei() {
        let ctx = ctx_with_input(vec![0x12, 0x34, 0x56, 0x78, 0x00]);
        let row = pending_row(
            Router::UniswapV2Router02,
            &DecodedSwap::Unknown {
                selector: [0x12, 0x34, 0x56, 0x78],
            },
            &ctx,
            1_700_000_000_000,
        );
        assert_eq!(row.selector, "0x12345678");
        assert_eq!(row.kind, "Unknown");
        assert_eq!(row.router, "Uniswap V2 Router02");
        assert!((row.max_fee_gwei - 25.0).abs() < 1e-9);
        assert_eq!(row.input_bytes, 5);
        assert!(row.token_in.is_none());
    }

    #[test]
    fn exact_input_fills_token_fields() {
        let token_in = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"); // WETH
        let token_out = address!("dac17f958d2ee523a2206206994597c13d831ec7"); // USDT
        let decoded = DecodedSwap::ExactInput {
            protocol: "UniV3",
            token_in,
            token_out,
            amount_in: U256::from(1_000u64),
            amount_out_min: U256::from(900u64),
            fee_pips: Some(3000),
            recipient: token_in,
        };
        let row = pending_row(
            Router::UniswapV3SwapRouter02,
            &decoded,
            &ctx_with_input(vec![0x04, 0xe4, 0x5a, 0xaf]),
            42,
        );
        assert_eq!(row.kind, "ExactInput");
        assert_eq!(row.protocol.as_deref(), Some("UniV3"));
        assert_eq!(
            row.token_in.as_deref(),
            Some("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
        );
        assert_eq!(
            row.token_out.as_deref(),
            Some("0xdac17f958d2ee523a2206206994597c13d831ec7")
        );
        assert_eq!(row.amount_in_wei.as_deref(), Some("1000"));
        assert_eq!(row.amount_out_min.as_deref(), Some("900"));
        assert_eq!(row.fee_pips, Some(3000));
    }

    #[test]
    fn exact_input_path_uses_tx_value_when_amount_in_zero() {
        let weth = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let memecoin = address!("7c58fffd3d36c19b58b75009f0d1c6f5b8e5e5e5");
        let decoded = DecodedSwap::ExactInputPath {
            protocol: "UniV2",
            path: vec![weth, memecoin],
            amount_in: U256::ZERO, // swapExactETHForTokens
            amount_out_min: U256::from(1u64),
            recipient: weth,
        };
        let mut ctx = ctx_with_input(vec![0x7f, 0xf3, 0x6a, 0xb5]);
        ctx.value = U256::from(500_000_000_000_000_000u64); // 0.5 ETH
        let row = pending_row(Router::UniswapV2Router02, &decoded, &ctx, 7);
        assert_eq!(row.kind, "ExactInputPath");
        assert_eq!(
            row.token_in.as_deref(),
            Some("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
        );
        assert_eq!(
            row.token_out.as_deref(),
            Some("0x7c58fffd3d36c19b58b75009f0d1c6f5b8e5e5e5")
        );
        // amount_in pris depuis tx.value car calldata = 0.
        assert_eq!(row.amount_in_wei.as_deref(), Some("500000000000000000"));
    }
}
