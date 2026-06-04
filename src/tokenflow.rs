//! Détection de sandwich par **flux de tokens** (Transfer logs) — Phase P3.8.
//!
//! Contrairement au décodage de calldata (limité aux routers publics whitelistés,
//! ~2 % d'un bloc), cette approche reconstruit les swaps depuis les events
//! `Transfer(from, to, value)` ERC-20 présents dans les receipts. Elle capte donc
//! les sandwicheurs MEV qui appellent les pools via leur **contrat custom** —
//! invisibles au décodeur calldata.
//!
//! Principe :
//! 1. Un **pool** de swap est une adresse qui, dans une même tx, REÇOIT un token
//!    et en ENVOIE un autre (≠). C'est la signature on-chain d'un échange.
//! 2. Le **trader** = la contrepartie du pool. S'il reçoit le token sujet (non-quote)
//!    → il ACHÈTE ; s'il l'envoie → il VEND.
//! 3. **Sandwich** = un même trader (attaquant) ACHÈTE le token T sur le pool P
//!    (tx i), une victime ACHÈTE T sur P (tx j, i<j<k), l'attaquant VEND T sur P
//!    (tx k) — le tout dans le même bloc.
//!
//! Logique PURE (zéro réseau, zéro DB) → testable de façon déterministe (cf. bas).

use std::collections::HashMap;

/// Quote tokens L1 (WETH/USDC/USDT/DAI). Le "token sujet" d'un swap est le côté
/// NON-quote — celui qu'un sandwicheur accumule puis revend.
pub const QUOTE_TOKENS: [&str; 4] = [
    "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // WETH
    "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // USDC
    "0xdac17f958d2ee523a2206206994597c13d831ec7", // USDT
    "0x6b175474e89094c44da98b954eedeac495271d0f", // DAI
];

pub fn is_quote(addr: &str) -> bool {
    QUOTE_TOKENS.contains(&addr)
}

/// Un event `Transfer(from, to, value)` ERC-20 (adresses en hex lowercase).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transfer {
    pub token: String,
    pub from: String,
    pub to: String,
    pub value: u128,
}

/// Un swap reconstruit : `trader` échange `token` (non-quote) contre du quote
/// via `pool`. `is_buy = true` ⇒ le trader REÇOIT le token (achète).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapAction {
    pub tx_index: i64,
    pub tx_hash: String,
    pub trader: String,
    pub pool: String,
    pub token: String,
    pub is_buy: bool,
    pub amount: u128, // quantité du token sujet qui transite
}

/// Un sandwich détecté : attaquant `attacker` encadre `victim` sur `(pool, token)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sandwich {
    pub pool: String,
    pub token: String,
    pub attacker: String,
    pub frontrun_hash: String,
    pub victim_hash: String,
    pub backrun_hash: String,
    pub frontrun_idx: i64,
    pub victim_idx: i64,
    pub backrun_idx: i64,
    /// Tokens achetés au frontrun puis revendus au backrun.
    pub front_amount: u128,
    pub back_amount: u128,
    /// Profit BRUT estimé en quote (bilan net WETH/stable de l'attaquant sur
    /// front+back, hors gas). Rempli par [`crate::scan`] via [`estimate_profit`] ;
    /// `detect_sandwiches` le laisse à `(0, None)` (il n'a pas les transfers).
    pub gross_profit: i128,
    pub profit_token: Option<String>,
}

/// Reconstruit les swaps d'UNE tx depuis ses Transfer logs.
///
/// Clé : distinguer le **trader** du **pool**. Les deux "reçoivent un token et en
/// envoient un autre" — symétriques au niveau des transfers bruts. On lève
/// l'ambiguïté avec `tx_from`/`tx_to` (fournis par le receipt) : l'initiateur de
/// la tx (l'EOA `tx_from` et son contrat `tx_to`, ex. le contrat MEV custom du
/// sandwicheur) forme le **groupe trader** ; le pool est la contrepartie externe.
///
/// Pour chaque token non-quote dont le groupe a un delta net non nul, on émet une
/// `SwapAction` : `is_buy` si le groupe en a net reçu (achat), sinon vente.
/// L'identité du trader (pour matcher front/back) est `tx_from` (EOA, stable).
///
/// Angle mort assumé (v1) : si le trader appelle le pool en direct (`tx_to` = pool),
/// le pool entre dans le groupe et le swap n'est pas isolé. Les sandwicheurs pros
/// passent par un contrat (`tx_to` ≠ pool), donc ce cas est marginal.
pub fn extract_swaps(
    tx_index: i64,
    tx_hash: &str,
    tx_from: &str,
    tx_to: &str,
    transfers: &[Transfer],
) -> Vec<SwapAction> {
    // Par token non-quote : montant net reçu/envoyé par le groupe + contrepartie.
    struct Flow {
        recv: u128,
        sent: u128,
        pool_buy: Option<String>,
        pool_sell: Option<String>,
    }

    let in_group = |a: &str| a == tx_from || a == tx_to;
    let mut flows: HashMap<&str, Flow> = HashMap::new();

    for t in transfers {
        if is_quote(&t.token) {
            continue;
        }
        let from_in = in_group(&t.from);
        let to_in = in_group(&t.to);
        // On ignore les transfers internes au groupe et ceux qui ne le touchent pas.
        if from_in == to_in {
            continue;
        }

        let f = flows.entry(t.token.as_str()).or_insert(Flow {
            recv: 0,
            sent: 0,
            pool_buy: None,
            pool_sell: None,
        });
        if to_in {
            // Token entre dans le groupe : achat. Contrepartie = l'expéditeur (pool).
            f.recv = f.recv.saturating_add(t.value);
            f.pool_buy.get_or_insert_with(|| t.from.clone());
        } else {
            // Token sort du groupe : vente. Contrepartie = le destinataire (pool).
            f.sent = f.sent.saturating_add(t.value);
            f.pool_sell.get_or_insert_with(|| t.to.clone());
        }
    }

    let mut out = Vec::new();
    for (token, f) in flows {
        if f.recv > f.sent {
            if let Some(pool) = f.pool_buy {
                out.push(SwapAction {
                    tx_index,
                    tx_hash: tx_hash.to_string(),
                    trader: tx_from.to_string(),
                    pool,
                    token: token.to_string(),
                    is_buy: true,
                    amount: f.recv - f.sent,
                });
            }
        } else if f.sent > f.recv
            && let Some(pool) = f.pool_sell
        {
            out.push(SwapAction {
                tx_index,
                tx_hash: tx_hash.to_string(),
                trader: tx_from.to_string(),
                pool,
                token: token.to_string(),
                is_buy: false,
                amount: f.sent - f.recv,
            });
        }
    }
    out
}

/// Détecte les sandwiches parmi tous les swaps d'un bloc.
///
/// Pour chaque (pool, token), cherche un attaquant qui ACHÈTE (idx i) puis VEND
/// (idx k>i) avec une VICTIME distincte qui ACHÈTE entre les deux (i<j<k).
pub fn detect_sandwiches(swaps: &[SwapAction]) -> Vec<Sandwich> {
    // Indexe les swaps par (pool, token).
    let mut groups: HashMap<(&str, &str), Vec<&SwapAction>> = HashMap::new();
    for s in swaps {
        groups
            .entry((s.pool.as_str(), s.token.as_str()))
            .or_default()
            .push(s);
    }

    let mut sandwiches = Vec::new();
    for ((pool, token), mut acts) in groups {
        if acts.len() < 3 {
            continue;
        } // besoin d'au moins front + victim + back
        acts.sort_by_key(|s| s.tx_index);

        // Pour chaque attaquant potentiel : un BUY (i) puis un SELL (k) plus loin.
        for fi in 0..acts.len() {
            let front = acts[fi];
            if !front.is_buy {
                continue;
            }
            for ki in (fi + 1)..acts.len() {
                let back = acts[ki];
                if back.is_buy {
                    continue;
                }
                if back.trader != front.trader {
                    continue;
                }
                let attacker = &front.trader;

                // Victime = un BUY d'une AUTRE adresse, strictement entre les deux.
                let victim = acts[(fi + 1)..ki].iter().find(|s| {
                    s.is_buy
                        && &s.trader != attacker
                        && s.tx_index > front.tx_index
                        && s.tx_index < back.tx_index
                });
                let Some(victim) = victim else { continue };

                sandwiches.push(Sandwich {
                    pool: pool.to_string(),
                    token: token.to_string(),
                    attacker: attacker.clone(),
                    frontrun_hash: front.tx_hash.clone(),
                    victim_hash: victim.tx_hash.clone(),
                    backrun_hash: back.tx_hash.clone(),
                    frontrun_idx: front.tx_index,
                    victim_idx: victim.tx_index,
                    backrun_idx: back.tx_index,
                    front_amount: front.amount,
                    back_amount: back.amount,
                    // Profit rempli ensuite par scan::sandwiches_in_block (besoin des transfers).
                    gross_profit: 0,
                    profit_token: None,
                });
                break; // un sandwich par (front, attaquant) suffit
            }
        }
    }
    sandwiches
}

#[cfg(test)]
mod tests {
    use super::*;

    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const POOL: &str = "0x1111111111111111111111111111111111111111";
    const SHIT: &str = "0x2222222222222222222222222222222222222222"; // token sujet
    // EOA + contrat MEV de l'attaquant (tx_from / tx_to d'un sandwicheur custom).
    const ATK_EOA: &str = "0xaaaa000000000000000000000000000000000000";
    const ATK_BOT: &str = "0xaabb000000000000000000000000000000000000";
    // EOA + contrat (routeur) de la victime.
    const VIC_EOA: &str = "0xbbbb000000000000000000000000000000000000";
    const VIC_RTR: &str = "0xbbcc000000000000000000000000000000000000";

    fn tr(token: &str, from: &str, to: &str, v: u128) -> Transfer {
        Transfer {
            token: token.into(),
            from: from.into(),
            to: to.into(),
            value: v,
        }
    }

    #[test]
    fn extract_buy_from_pool() {
        // Le contrat du bot envoie WETH au pool, reçoit SHIT → achat de SHIT.
        let transfers = vec![tr(WETH, ATK_BOT, POOL, 1_000), tr(SHIT, POOL, ATK_BOT, 500)];
        let swaps = extract_swaps(0, "0xtx", ATK_EOA, ATK_BOT, &transfers);
        assert_eq!(swaps.len(), 1);
        assert_eq!(swaps[0].token, SHIT);
        assert!(swaps[0].is_buy);
        assert_eq!(swaps[0].trader, ATK_EOA);
        assert_eq!(swaps[0].pool, POOL);
        assert_eq!(swaps[0].amount, 500);
    }

    #[test]
    fn extract_sell_to_pool() {
        // Le contrat du bot envoie SHIT au pool, reçoit WETH → vente de SHIT.
        let transfers = vec![tr(SHIT, ATK_BOT, POOL, 500), tr(WETH, POOL, ATK_BOT, 900)];
        let swaps = extract_swaps(0, "0xtx", ATK_EOA, ATK_BOT, &transfers);
        assert_eq!(swaps.len(), 1);
        assert_eq!(swaps[0].token, SHIT);
        assert!(!swaps[0].is_buy);
        assert_eq!(swaps[0].trader, ATK_EOA);
    }

    // Le profit est désormais calculé par `crate::profit` (reconstruction du
    // périmètre de l'attaquant) — testé dans ce module-là.

    /// Helpers : un swap achat/vente via (eoa, bot) contre un pool.
    fn buy(idx: i64, hash: &str, eoa: &str, bot: &str, pool: &str, amt: u128) -> Vec<SwapAction> {
        extract_swaps(
            idx,
            hash,
            eoa,
            bot,
            &[tr(WETH, bot, pool, amt * 2), tr(SHIT, pool, bot, amt)],
        )
    }
    fn sell(idx: i64, hash: &str, eoa: &str, bot: &str, pool: &str, amt: u128) -> Vec<SwapAction> {
        extract_swaps(
            idx,
            hash,
            eoa,
            bot,
            &[tr(SHIT, bot, pool, amt), tr(WETH, pool, bot, amt * 2)],
        )
    }

    #[test]
    fn detects_classic_sandwich() {
        let mut swaps = Vec::new();
        swaps.extend(buy(0, "0xfront", ATK_EOA, ATK_BOT, POOL, 500));
        swaps.extend(buy(1, "0xvictim", VIC_EOA, VIC_RTR, POOL, 800));
        swaps.extend(sell(2, "0xback", ATK_EOA, ATK_BOT, POOL, 500));

        let sw = detect_sandwiches(&swaps);
        assert_eq!(sw.len(), 1, "doit détecter exactement 1 sandwich");
        let s = &sw[0];
        assert_eq!(s.attacker, ATK_EOA);
        assert_eq!(s.token, SHIT);
        assert_eq!(s.frontrun_hash, "0xfront");
        assert_eq!(s.victim_hash, "0xvictim");
        assert_eq!(s.backrun_hash, "0xback");
        assert_eq!((s.frontrun_idx, s.victim_idx, s.backrun_idx), (0, 1, 2));
    }

    #[test]
    fn no_sandwich_without_victim() {
        let mut swaps = Vec::new();
        swaps.extend(buy(0, "0xfront", ATK_EOA, ATK_BOT, POOL, 500));
        swaps.extend(sell(1, "0xback", ATK_EOA, ATK_BOT, POOL, 500));
        assert!(detect_sandwiches(&swaps).is_empty());
    }

    #[test]
    fn no_sandwich_if_victim_is_attacker() {
        // Le "milieu" est encore l'attaquant → pas de victime distincte.
        let mut swaps = Vec::new();
        swaps.extend(buy(0, "0xfront", ATK_EOA, ATK_BOT, POOL, 500));
        swaps.extend(buy(1, "0xmid", ATK_EOA, ATK_BOT, POOL, 500));
        swaps.extend(sell(2, "0xback", ATK_EOA, ATK_BOT, POOL, 500));
        assert!(detect_sandwiches(&swaps).is_empty());
    }

    #[test]
    fn different_pool_is_not_sandwiched() {
        let other_pool = "0x9999999999999999999999999999999999999999";
        let mut swaps = Vec::new();
        swaps.extend(buy(0, "0xfront", ATK_EOA, ATK_BOT, POOL, 500));
        swaps.extend(buy(1, "0xvictim", VIC_EOA, VIC_RTR, other_pool, 800));
        swaps.extend(sell(2, "0xback", ATK_EOA, ATK_BOT, POOL, 500));
        assert!(detect_sandwiches(&swaps).is_empty());
    }
}
