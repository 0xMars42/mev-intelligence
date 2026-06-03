//! Scan d'un bloc on-chain → sandwiches, via les Transfer logs des receipts.
//!
//! Pont entre alloy (réseau) et [`crate::tokenflow`] (logique pure) : récupère
//! les receipts d'un bloc, parse les events `Transfer` ERC-20, reconstruit les
//! swaps et détecte les sandwiches. Partagé par le daemon (`main.rs`) et le
//! binaire de backfill/test (`bin/scan_range.rs`).

use crate::tokenflow::{self, Sandwich, Transfer};
use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Log;
use eyre::Result;

/// Topic0 de l'event ERC-20 `Transfer(address,address,uint256)`.
pub const TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// Parse un log en `Transfer` ERC-20, ou `None` si ce n'est pas un Transfer
/// standard (topic0 ≠, ou < 3 topics = from/to non indexés).
pub fn parse_transfer_log(log: &Log) -> Option<Transfer> {
    let topics = log.topics();
    if topics.len() < 3 || format!("{:#x}", topics[0]) != TRANSFER_TOPIC {
        return None;
    }
    let from = Address::from_word(topics[1]);
    let to = Address::from_word(topics[2]);
    let data = log.data().data.as_ref();
    if data.len() < 32 {
        return None;
    }
    let value = U256::from_be_slice(&data[..32]).saturating_to::<u128>();
    Some(Transfer {
        token: format!("{:#x}", log.address()),
        from: format!("{from:#x}"),
        to: format!("{to:#x}"),
        value,
    })
}

/// Scanne un bloc : reconstruit tous les swaps depuis les Transfer logs et
/// détecte les sandwiches. Retourne `(swaps_reconstruits, sandwiches)`.
pub async fn sandwiches_in_block<P: Provider>(
    provider: &P,
    block_num: u64,
) -> Result<(usize, Vec<Sandwich>)> {
    let receipts = provider
        .get_block_receipts(BlockId::from(block_num))
        .await?;
    let Some(receipts) = receipts else {
        return Ok((0, Vec::new()));
    };

    let mut all_swaps = Vec::new();
    for r in &receipts {
        let tx_hash = format!("{:#x}", r.transaction_hash);
        let tx_from = format!("{:#x}", r.from);
        let tx_to = r.to.map(|a| format!("{a:#x}")).unwrap_or_default();
        let tx_index = r.transaction_index.unwrap_or(0) as i64;

        let transfers: Vec<Transfer> = r.logs().iter().filter_map(parse_transfer_log).collect();
        if transfers.is_empty() {
            continue;
        }
        all_swaps.extend(tokenflow::extract_swaps(
            tx_index, &tx_hash, &tx_from, &tx_to, &transfers,
        ));
    }

    let n_swaps = all_swaps.len();
    Ok((n_swaps, tokenflow::detect_sandwiches(&all_swaps)))
}

/// Convertit un [`Sandwich`] (tokenflow) en ligne persistable.
pub fn to_candidate(
    s: &Sandwich,
    block_number: i64,
    detected_at_ms: i64,
) -> crate::db::SandwichCandidate {
    crate::db::SandwichCandidate {
        block_number,
        frontrun_hash: s.frontrun_hash.clone(),
        victim_hash: s.victim_hash.clone(),
        backrun_hash: s.backrun_hash.clone(),
        attacker_addr: s.attacker.clone(),
        target_token: Some(s.token.clone()),
        pool: Some(s.pool.clone()),
        front_amount: Some(s.front_amount.to_string()),
        back_amount: Some(s.back_amount.to_string()),
        frontrun_idx: s.frontrun_idx,
        victim_idx: s.victim_idx,
        backrun_idx: s.backrun_idx,
        detected_at_ms,
    }
}
