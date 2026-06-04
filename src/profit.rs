//! Estimation robuste du profit d'un sandwich par reconstruction du bilan net de
//! **l'ensemble d'adresses de l'attaquant** (EOA + ses contrats) — Phase P3.9.
//!
//! Le bilan net naïf sur `{tx_from, tx_to}` échoue pour les bots multi-contrats :
//! le profit s'accumule sur un contrat de collecte hors de ce couple, et on ne
//! voit qu'une patte du flux (artefacts ±0.2-0.5 WETH observés). Cette version
//! identifie correctement le périmètre de l'attaquant.
//!
//! Idée clé pour distinguer **contrat de l'attaquant** d'un **pool**, sans liste :
//! - un **pool** est touché par PLUSIEURS signers (EOA) différents dans le bloc
//!   (plein de gens tradent contre lui) ;
//! - un **contrat de l'attaquant** n'est touché que par l'EOA de l'attaquant.
//!
//! L'ensemble attaquant = `{EOA} ∪ {adresses des transfers front/back touchées
//! UNIQUEMENT par cet EOA dans tout le bloc}`. Le profit = bilan net des quote
//! tokens de cet ensemble sur front+back.
//!
//! **Flash loans** : le lender (Aave/Balancer/pool) est multi-signer → externe.
//! L'emprunt (entrée) et le remboursement (sortie) traversent donc la frontière
//! de l'ensemble et s'annulent : il ne reste que le fee (un coût réel). Correct.
//!
//! Logique PURE (zéro réseau) → testable. Cf. tests en bas (sandwich multi-contrat
//! + flash loan).

use crate::tokenflow::{Transfer, is_quote};
use std::collections::{HashMap, HashSet};

/// Estime le profit net d'un sandwich.
///
/// - `tx_transfers` : transfers de CHAQUE tx du bloc (clé = tx_index).
/// - `tx_signer`    : signer (EOA `tx.from`) de chaque tx du bloc.
/// - `attacker`     : EOA de l'attaquant (signer du front = du back).
/// - `front_idx` / `back_idx` : positions des deux pattes dans le bloc.
///
/// Retourne `(profit_net, quote_token)` dans le quote DOMINANT (plus grand |net|),
/// ou `(0, None)` si aucun quote ne traverse la frontière de l'ensemble attaquant.
#[allow(clippy::implicit_hasher)]
pub fn estimate_sandwich_profit(
    tx_transfers: &HashMap<i64, Vec<Transfer>>,
    tx_signer: &HashMap<i64, String>,
    attacker: &str,
    front_idx: i64,
    back_idx: i64,
) -> (i128, Option<String>) {
    // 1) Pour chaque adresse, l'ensemble des signers des tx où elle apparaît.
    //    Un pool ⇒ ≥ 2 signers distincts.
    let mut addr_signers: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (idx, transfers) in tx_transfers {
        let Some(signer) = tx_signer.get(idx) else {
            continue;
        };
        for t in transfers {
            addr_signers
                .entry(t.from.as_str())
                .or_default()
                .insert(signer);
            addr_signers
                .entry(t.to.as_str())
                .or_default()
                .insert(signer);
        }
    }

    // 2) Ensemble attaquant = {EOA} ∪ {adresses des transfers front/back touchées
    //    UNIQUEMENT par cet EOA (mono-signer = l'attaquant) — ses contrats}.
    let mut group: HashSet<&str> = HashSet::new();
    group.insert(attacker);
    for idx in [front_idx, back_idx] {
        let Some(transfers) = tx_transfers.get(&idx) else {
            continue;
        };
        for t in transfers {
            for addr in [t.from.as_str(), t.to.as_str()] {
                if addr == attacker {
                    continue;
                }
                if let Some(signers) = addr_signers.get(addr) {
                    // Contrat du bot : touché seulement par l'attaquant.
                    if signers.len() == 1 && signers.contains(attacker) {
                        group.insert(addr);
                    }
                }
            }
        }
    }

    // 3) Bilan net des quote tokens de l'ensemble sur front+back.
    let mut net: HashMap<&str, i128> = HashMap::new();
    for idx in [front_idx, back_idx] {
        let Some(transfers) = tx_transfers.get(&idx) else {
            continue;
        };
        for t in transfers {
            if !is_quote(&t.token) {
                continue;
            }
            let from_in = group.contains(t.from.as_str());
            let to_in = group.contains(t.to.as_str());
            if from_in == to_in {
                continue; // interne au groupe, ou totalement externe
            }
            let e = net.entry(t.token.as_str()).or_insert(0);
            if to_in {
                *e += t.value as i128; // entré dans le périmètre attaquant
            } else {
                *e -= t.value as i128; // sorti
            }
        }
    }

    net.into_iter()
        .max_by_key(|(_, v)| v.abs())
        .map_or((0, None), |(k, v)| (v, Some(k.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const SHIT: &str = "0x2222222222222222222222222222222222222222";
    const ATK_EOA: &str = "0xaaaa000000000000000000000000000000000000";
    const ATK_BOT: &str = "0xaabb000000000000000000000000000000000000"; // contrat du bot
    const POOL: &str = "0x1111111111111111111111111111111111111111";
    const VICTIM: &str = "0xbbbb000000000000000000000000000000000000";
    const LENDER: &str = "0xcccc000000000000000000000000000000000000"; // flash-loan lender

    fn tr(token: &str, from: &str, to: &str, v: u128) -> Transfer {
        Transfer {
            token: token.into(),
            from: from.into(),
            to: to.into(),
            value: v,
        }
    }

    #[test]
    fn profit_via_attacker_contract() {
        // Le profit transite par le CONTRAT du bot (ATK_BOT), pas l'EOA.
        // Front (idx 0, signé ATK_EOA) : BOT dépense 1.0 WETH au pool, reçoit SHIT.
        // Victime (idx 1, signé VICTIM) : achète SHIT sur le même pool.
        // Back  (idx 2, signé ATK_EOA) : BOT revend SHIT, reçoit 1.2 WETH.
        let mut txt = HashMap::new();
        let mut sig = HashMap::new();
        txt.insert(
            0,
            vec![
                tr(WETH, ATK_BOT, POOL, 1_000_000),
                tr(SHIT, POOL, ATK_BOT, 500),
            ],
        );
        sig.insert(0, ATK_EOA.to_string());
        txt.insert(
            1,
            vec![
                tr(WETH, VICTIM, POOL, 2_000_000),
                tr(SHIT, POOL, VICTIM, 800),
            ],
        );
        sig.insert(1, VICTIM.to_string());
        txt.insert(
            2,
            vec![
                tr(SHIT, ATK_BOT, POOL, 500),
                tr(WETH, POOL, ATK_BOT, 1_200_000),
            ],
        );
        sig.insert(2, ATK_EOA.to_string());

        let (profit, token) = estimate_sandwich_profit(&txt, &sig, ATK_EOA, 0, 2);
        assert_eq!(token.as_deref(), Some(WETH));
        // Le pool est touché par ATK_EOA et VICTIM (2 signers) → externe.
        // Net du périmètre {ATK_EOA, ATK_BOT} : -1.0 (front) +1.2 (back) = +0.2 WETH.
        assert_eq!(profit, 200_000);
    }

    #[test]
    fn flash_loan_is_netted_out() {
        // Le bot emprunte 10 WETH au LENDER (externe, multi-tx normalement), s'en
        // sert pour acheter, puis rembourse 10 WETH + garde le profit.
        // Front : LENDER -> BOT 10 WETH (emprunt) ; BOT -> POOL 10 WETH ; POOL -> BOT SHIT.
        // Back  : BOT -> POOL SHIT ; POOL -> BOT 10.3 WETH ; BOT -> LENDER 10 WETH (remb).
        // Profit réel = 10.3 - 10 (achat) - 10 (remb) + 10 (emprunt) = +0.3 WETH.
        let mut txt = HashMap::new();
        let mut sig = HashMap::new();
        // LENDER touché aussi par une autre tx du bloc → multi-signer → externe.
        txt.insert(
            0,
            vec![
                tr(WETH, LENDER, ATK_BOT, 10_000_000),
                tr(WETH, ATK_BOT, POOL, 10_000_000),
                tr(SHIT, POOL, ATK_BOT, 5_000),
            ],
        );
        sig.insert(0, ATK_EOA.to_string());
        // tx tierce qui touche aussi LENDER et POOL (les rend multi-signer = pools/externes)
        txt.insert(
            1,
            vec![
                tr(WETH, VICTIM, LENDER, 1),
                tr(WETH, VICTIM, POOL, 2_000_000),
                tr(SHIT, POOL, VICTIM, 700),
            ],
        );
        sig.insert(1, VICTIM.to_string());
        txt.insert(
            2,
            vec![
                tr(SHIT, ATK_BOT, POOL, 5_000),
                tr(WETH, POOL, ATK_BOT, 10_300_000),
                tr(WETH, ATK_BOT, LENDER, 10_000_000),
            ],
        );
        sig.insert(2, ATK_EOA.to_string());

        let (profit, token) = estimate_sandwich_profit(&txt, &sig, ATK_EOA, 0, 2);
        assert_eq!(token.as_deref(), Some(WETH));
        // {ATK_EOA, ATK_BOT}. Front: +10 (emprunt) -10 (achat) = 0. Back: +10.3 -10 (remb) = +0.3.
        assert_eq!(profit, 300_000);
    }

    #[test]
    fn no_profit_when_nothing_crosses() {
        // Aucun quote ne touche le périmètre.
        let mut txt = HashMap::new();
        let mut sig = HashMap::new();
        txt.insert(0, vec![tr(SHIT, POOL, ATK_BOT, 500)]);
        sig.insert(0, ATK_EOA.to_string());
        txt.insert(2, vec![tr(SHIT, ATK_BOT, POOL, 500)]);
        sig.insert(2, ATK_EOA.to_string());
        let (profit, token) = estimate_sandwich_profit(&txt, &sig, ATK_EOA, 0, 2);
        assert_eq!(profit, 0);
        assert_eq!(token, None);
    }
}
