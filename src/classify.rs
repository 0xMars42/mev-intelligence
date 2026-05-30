//! Classification comportementale des addresses → taxonomie de bots (P3.4).
//!
//! À partir de features agrégées (calculées depuis `pending_tx` + `tx_outcome`),
//! on attribue un **type** à chaque address via des règles heuristiques. C'est
//! volontairement simple et explicable — une 1re taxonomie, pas du ML.
//!
//! Honnêteté : certains types MEV (sandwich, JIT) demandent l'ordre intra-bloc
//! ou le décodage des inner-calls Universal Router qu'on n'a pas encore — donc
//! on ne prétend PAS les distinguer ici. Les classes ci-dessous sont celles que
//! les features disponibles séparent de façon défendable.

/// Features agrégées d'une address (counts bruts ; les ratios sont dérivés).
#[derive(Clone, Debug, PartialEq)]
pub struct AddressFeatures {
    pub address: String,
    /// Nombre total de swaps observés (router-hits).
    pub tx_count: u64,
    /// Tokens *sujets* distincts (côté non-quote : `token_out` pour un achat,
    /// `token_in` pour une vente ; WETH/USDC/USDT/DAI exclus). 0 si que de
    /// l'opaque (Universal Router / 1inch) ou que des quote tokens.
    pub distinct_tokens_out: u64,
    /// Swaps dont `token_in` == WETH (= achat de token avec de l'ETH).
    pub weth_in_count: u64,
    /// Swaps dont `token_in` est connu (dénominateur du ratio WETH-in).
    pub token_in_count: u64,
    /// `max_fee_per_gas` moyen (gwei) — proxy d'agressivité.
    pub avg_max_fee_gwei: f64,
    /// Swaps ayant un outcome figé (dénominateur des taux).
    pub validated: u64,
    pub mined_success: u64,
    pub mined_reverted: u64,
    pub not_mined: u64,
    /// Volume d'entrée en ETH (somme des `amount_in` des swaps WETH-in). Proxy
    /// de taille d'activité — PAS une valorisation des tokens reçus.
    pub weth_volume_eth: f64,
    /// 1re et dernière activité observée.
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

impl AddressFeatures {
    /// Part de tokens distincts par swap : ~1 = spray de tokens frais (sniper),
    /// ~0 = concentré sur 1-2 tokens (market maker / accumulateur).
    pub fn token_diversity(&self) -> f64 {
        if self.tx_count == 0 {
            0.0
        } else {
            self.distinct_tokens_out as f64 / self.tx_count as f64
        }
    }

    /// Part de swaps qui achètent un token avec du WETH.
    pub fn weth_in_ratio(&self) -> f64 {
        if self.token_in_count == 0 {
            0.0
        } else {
            self.weth_in_count as f64 / self.token_in_count as f64
        }
    }

    /// Part de swaps minés mais revertés (gas brûlé = races MEV perdues).
    pub fn revert_rate(&self) -> f64 {
        if self.validated == 0 {
            0.0
        } else {
            self.mined_reverted as f64 / self.validated as f64
        }
    }

    /// Part de swaps minés avec succès parmi ceux validés.
    pub fn success_rate(&self) -> f64 {
        if self.validated == 0 {
            0.0
        } else {
            self.mined_success as f64 / self.validated as f64
        }
    }
}

/// Type de bot attribué (taxonomie v1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BotClass {
    /// Trop peu d'activité pour conclure.
    LowActivity,
    /// Spray de tokens frais, achète (WETH-in), paie le gas — sniper de memecoins.
    Sniper,
    /// Concentré sur 1-2 tokens — market maker / accumulateur sur une paire.
    SingleTarget,
    /// Agressif (gas haut) + beaucoup de reverts — perd des races MEV (snipe/sandwich).
    Racer,
    /// Activité réelle mais sans signature nette.
    Generic,
}

impl BotClass {
    pub const fn label(self) -> &'static str {
        match self {
            Self::LowActivity => "LowActivity",
            Self::Sniper => "Sniper",
            Self::SingleTarget => "SingleTarget",
            Self::Racer => "Racer",
            Self::Generic => "Generic",
        }
    }
}

// --- Seuils (documentés, ajustables) ----------------------------------------
const MIN_TX: u64 = 3;
const MIN_VALIDATED_FOR_RACER: u64 = 3;
const HIGH_GAS_GWEI: f64 = 20.0;
const SNIPER_DIVERSITY: f64 = 0.7;
const SNIPER_WETH_IN: f64 = 0.6;
const SINGLE_TARGET_DIVERSITY: f64 = 0.34;
const RACER_REVERT_RATE: f64 = 0.3;

/// Attribue un [`BotClass`] à une address depuis ses features. Règles testées,
/// en ordre de spécificité décroissante.
pub fn classify(f: &AddressFeatures) -> BotClass {
    if f.tx_count < MIN_TX {
        return BotClass::LowActivity;
    }
    // Aucun token décodé (ex: que des enveloppes Universal Router) : on ne peut
    // pas profiler le comportement token → Generic plutôt que faux SingleTarget.
    if f.distinct_tokens_out == 0 {
        return BotClass::Generic;
    }
    // Racer : agressif ET perd beaucoup de races (besoin d'assez d'outcomes).
    if f.validated >= MIN_VALIDATED_FOR_RACER
        && f.revert_rate() >= RACER_REVERT_RATE
        && f.avg_max_fee_gwei >= HIGH_GAS_GWEI
    {
        return BotClass::Racer;
    }
    // Sniper : beaucoup de tokens frais distincts, achète, paie le gas.
    if f.token_diversity() >= SNIPER_DIVERSITY
        && f.weth_in_ratio() >= SNIPER_WETH_IN
        && f.avg_max_fee_gwei >= HIGH_GAS_GWEI
    {
        return BotClass::Sniper;
    }
    // SingleTarget : très concentré sur peu de tokens.
    if f.token_diversity() <= SINGLE_TARGET_DIVERSITY {
        return BotClass::SingleTarget;
    }
    BotClass::Generic
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feat(address: &str) -> AddressFeatures {
        AddressFeatures {
            address: address.to_string(),
            tx_count: 10,
            distinct_tokens_out: 5,
            weth_in_count: 5,
            token_in_count: 10,
            avg_max_fee_gwei: 10.0,
            validated: 0,
            mined_success: 0,
            mined_reverted: 0,
            not_mined: 0,
            weth_volume_eth: 0.0,
            first_seen_ms: 0,
            last_seen_ms: 0,
        }
    }

    #[test]
    fn too_few_tx_is_low_activity() {
        let mut f = feat("a");
        f.tx_count = 2;
        assert_eq!(classify(&f), BotClass::LowActivity);
    }

    #[test]
    fn no_decoded_tokens_is_generic() {
        let mut f = feat("ur");
        f.distinct_tokens_out = 0;
        assert_eq!(classify(&f), BotClass::Generic);
    }

    #[test]
    fn high_diversity_buying_high_gas_is_sniper() {
        let mut f = feat("sniper");
        f.distinct_tokens_out = 9; // diversity 0.9
        f.weth_in_count = 10; // 100% WETH-in
        f.avg_max_fee_gwei = 50.0;
        assert_eq!(classify(&f), BotClass::Sniper);
    }

    #[test]
    fn concentrated_on_one_token_is_single_target() {
        let mut f = feat("mm");
        f.distinct_tokens_out = 1; // diversity 0.1
        assert_eq!(classify(&f), BotClass::SingleTarget);
    }

    #[test]
    fn aggressive_with_many_reverts_is_racer() {
        let mut f = feat("racer");
        f.distinct_tokens_out = 5; // diversity 0.5 (ni sniper ni single)
        f.avg_max_fee_gwei = 40.0;
        f.validated = 8;
        f.mined_reverted = 5; // revert_rate 0.625
        f.mined_success = 3;
        assert_eq!(classify(&f), BotClass::Racer);
    }

    #[test]
    fn ordinary_activity_is_generic() {
        // diversity 0.5, gas bas, pas d'outcomes -> rien ne matche.
        assert_eq!(classify(&feat("x")), BotClass::Generic);
    }

    #[test]
    fn success_and_revert_rates() {
        let mut f = feat("r");
        f.validated = 4;
        f.mined_success = 3;
        f.mined_reverted = 1;
        assert!((f.success_rate() - 0.75).abs() < 1e-9);
        assert!((f.revert_rate() - 0.25).abs() < 1e-9);
        // Sans outcome validé, les taux retombent à 0 (pas de division par 0).
        assert_eq!(feat("z").success_rate(), 0.0);
    }
}
