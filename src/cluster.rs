//! Clustering d'addresses par co-occurrence — étage "entity" (P3.3).
//!
//! Heuristique : deux addresses appartiennent probablement au même opérateur si
//! elles frappent **les mêmes tokens cibles** dans **la même fenêtre temporelle**
//! (ex: deux wallets qui snipent le même memecoin au même bloc). Un seul token
//! partagé est un signal faible (deux bots indépendants peuvent viser un token
//! chaud) ; on n'unit deux addresses que si elles co-occurrent sur **≥
//! `min_shared_tokens` tokens distincts** — la coïncidence devient implausible.
//!
//! Algo (logique pure, testable sans DB) :
//! 1. par token, on trie les observations et on relie les paires d'addresses
//!    distinctes vues à moins de `window_ms` l'une de l'autre ;
//! 2. le poids d'une paire = nombre de tokens distincts sur lesquels elle
//!    co-occurre ;
//! 3. union-find sur les paires de poids ≥ `min_shared_tokens` ;
//! 4. composantes connexes de taille ≥ 2 = opérateurs.
//!
//! Limite assumée : c'est conservateur mais pas infaillible (orderflow privé
//! invisible, bots qui partagent un token chaud par hasard). Les clusters sont
//! des **hypothèses à valider**, pas une vérité — d'où la trace des tokens-preuve.

use std::collections::{BTreeSet, HashMap};

/// Une observation de swap : qui (`from`) a visé quel token (`token_out`) et quand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub from: String,
    pub token_out: String,
    pub seen_at_ms: i64,
}

/// Un cluster d'addresses attribué à un même opérateur présumé.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cluster {
    /// Addresses du cluster (triées, ≥ 2).
    pub addresses: Vec<String>,
    /// Tokens distincts qui lient le cluster (la preuve), triés.
    pub shared_tokens: Vec<String>,
    /// 1re et dernière activité observée parmi les membres.
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

// --- Union-Find -------------------------------------------------------------

fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]]; // path halving
        x = parent[x];
    }
    x
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[ra] = rb;
    }
}

/// Regroupe les addresses co-occurrentes en clusters d'opérateurs.
///
/// `window_ms` : fenêtre de co-occurrence (deux swaps sur le même token vus à
/// moins de ça = "ensemble"). `min_shared_tokens` : nombre de tokens distincts
/// partagés requis pour unir deux addresses (≥ 2 recommandé, conservateur).
pub fn cluster_addresses(
    obs: &[Observation],
    window_ms: i64,
    min_shared_tokens: usize,
) -> Vec<Cluster> {
    // 1. Indexe les addresses (string -> usize).
    let mut idx_of: HashMap<String, usize> = HashMap::new();
    let mut addr_of: Vec<String> = Vec::new();
    let mut by_token: HashMap<String, Vec<(usize, i64)>> = HashMap::new();
    for o in obs {
        // Index stable de l'address (insère si nouvelle).
        let next = addr_of.len();
        let ai = *idx_of.entry(o.from.clone()).or_insert(next);
        if ai == next {
            addr_of.push(o.from.clone());
        }
        by_token
            .entry(o.token_out.clone())
            .or_default()
            .push((ai, o.seen_at_ms));
    }

    // 2. Paires d'addresses distinctes co-occurrentes -> ensemble de tokens.
    let mut pair_tokens: HashMap<(usize, usize), BTreeSet<String>> = HashMap::new();
    for (token, mut list) in by_token {
        list.sort_by_key(|&(_, t)| t);
        for i in 0..list.len() {
            let (ai, ti) = list[i];
            for &(aj, tj) in &list[i + 1..] {
                if tj - ti > window_ms {
                    break; // trié par temps : au-delà, plus rien dans la fenêtre
                }
                if ai == aj {
                    continue; // pas d'arête d'une address vers elle-même
                }
                let key = if ai < aj { (ai, aj) } else { (aj, ai) };
                pair_tokens.entry(key).or_default().insert(token.clone());
            }
        }
    }

    // 3. Union-find sur les paires de poids suffisant.
    let n = addr_of.len();
    let mut parent: Vec<usize> = (0..n).collect();
    for (&(a, b), toks) in &pair_tokens {
        if toks.len() >= min_shared_tokens {
            union(&mut parent, a, b);
        }
    }

    // 4. Preuves (tokens) et span temporel agrégés par racine.
    let mut evidence: HashMap<usize, BTreeSet<String>> = HashMap::new();
    for (&(a, _), toks) in &pair_tokens {
        if toks.len() >= min_shared_tokens {
            // a et b ont été unis, donc même racine : on agrège la preuve via a.
            let root = find(&mut parent, a);
            evidence
                .entry(root)
                .or_default()
                .extend(toks.iter().cloned());
        }
    }
    let mut span: HashMap<usize, (i64, i64)> = HashMap::new();
    for o in obs {
        if let Some(&i) = idx_of.get(&o.from) {
            let root = find(&mut parent, i);
            let e = span.entry(root).or_insert((o.seen_at_ms, o.seen_at_ms));
            e.0 = e.0.min(o.seen_at_ms);
            e.1 = e.1.max(o.seen_at_ms);
        }
    }

    // 5. Composantes de taille >= 2 -> clusters.
    let mut members: HashMap<usize, Vec<String>> = HashMap::new();
    for (i, addr) in addr_of.iter().enumerate() {
        let root = find(&mut parent, i);
        members.entry(root).or_default().push(addr.clone());
    }

    let mut clusters: Vec<Cluster> = members
        .into_iter()
        .filter(|(_, addrs)| addrs.len() >= 2)
        .map(|(root, mut addresses)| {
            addresses.sort();
            let shared: Vec<String> = evidence
                .get(&root)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            let (first, last) = span.get(&root).copied().unwrap_or((0, 0));
            Cluster {
                addresses,
                shared_tokens: shared,
                first_seen_ms: first,
                last_seen_ms: last,
            }
        })
        .collect();

    // Ordre déterministe et utile : plus gros / mieux étayés d'abord.
    clusters.sort_by(|a, b| {
        b.addresses
            .len()
            .cmp(&a.addresses.len())
            .then_with(|| b.shared_tokens.len().cmp(&a.shared_tokens.len()))
            .then_with(|| a.addresses.cmp(&b.addresses))
    });
    clusters
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(from: &str, token: &str, t: i64) -> Observation {
        Observation {
            from: from.to_string(),
            token_out: token.to_string(),
            seen_at_ms: t,
        }
    }

    #[test]
    fn two_addresses_sharing_two_tokens_cluster() {
        // A et B frappent token T1 puis T2, à chaque fois proches dans le temps.
        let observations = [
            obs("A", "T1", 1_000),
            obs("B", "T1", 1_500),
            obs("A", "T2", 5_000),
            obs("B", "T2", 5_400),
        ];
        let clusters = cluster_addresses(&observations, 30_000, 2);
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].addresses,
            vec!["A".to_string(), "B".to_string()]
        );
        assert_eq!(
            clusters[0].shared_tokens,
            vec!["T1".to_string(), "T2".to_string()]
        );
        assert_eq!(clusters[0].first_seen_ms, 1_000);
        assert_eq!(clusters[0].last_seen_ms, 5_400);
    }

    #[test]
    fn single_shared_token_is_not_enough() {
        // A et B ne partagent qu'UN token -> signal trop faible, pas de cluster.
        let observations = [obs("A", "T1", 1_000), obs("B", "T1", 1_200)];
        assert!(cluster_addresses(&observations, 30_000, 2).is_empty());
    }

    #[test]
    fn co_occurrence_outside_window_is_ignored() {
        // Mêmes tokens mais espacés au-delà de la fenêtre -> aucune arête.
        let observations = [
            obs("A", "T1", 1_000),
            obs("B", "T1", 100_000),
            obs("A", "T2", 2_000),
            obs("B", "T2", 200_000),
        ];
        assert!(cluster_addresses(&observations, 30_000, 2).is_empty());
    }

    #[test]
    fn transitive_union_merges_three_addresses() {
        // A~B (T1,T2) et B~C (T3,T4) -> {A,B,C} par transitivité.
        let observations = [
            obs("A", "T1", 1_000),
            obs("B", "T1", 1_100),
            obs("A", "T2", 2_000),
            obs("B", "T2", 2_100),
            obs("B", "T3", 3_000),
            obs("C", "T3", 3_100),
            obs("B", "T4", 4_000),
            obs("C", "T4", 4_100),
        ];
        let clusters = cluster_addresses(&observations, 30_000, 2);
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].addresses,
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
        assert_eq!(clusters[0].shared_tokens.len(), 4);
    }

    #[test]
    fn same_address_repeated_does_not_self_cluster() {
        // Une seule address, même répétée sur plusieurs tokens -> pas de cluster.
        let observations = [
            obs("A", "T1", 1_000),
            obs("A", "T2", 2_000),
            obs("A", "T1", 3_000),
        ];
        assert!(cluster_addresses(&observations, 30_000, 2).is_empty());
    }
}
