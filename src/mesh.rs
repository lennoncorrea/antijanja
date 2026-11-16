//! Rotas da malha P2P (ver docs/malha-p2p.md): quem repassa cada faixa para quem.
//!
//! Inspirado no SplitStream: cada espectador que pode repassar é "interno" em uma faixa só
//! (a de número `posição % faixas`). O host manda cada faixa para os internos dela, e cada
//! interno repassa a faixa para uma parte dos outros espectadores. Assim cada espectador
//! recebe as faixas de vizinhos diferentes (e a dele, do host) e repassa uma só: o
//! trabalho fica repartido, e perder um vizinho custa uma faixa, que a folga de pedaços
//! extras cobre enquanto a rota é refeita.
//!
//! Upload do host: cada faixa (1/faixas da stream) vai para os internos dela, então cerca
//! de espectadores/faixas streams (4x menos que hoje com 4 faixas), mais as faixas que
//! precisam ir direto (ligação entre espectadores que não funcionou).

use std::collections::{BTreeMap, HashSet};

/// Mais filhos que isso numa faixa, os excedentes recebem a faixa direto do host.
pub const MAX_CHILDREN: usize = 6;

/// Um espectador que pode entrar na malha.
#[derive(Clone, Debug)]
pub struct Member {
    pub id: u64,
    /// Pode repassar (upload sobrando, conexão estável).
    pub relay: bool,
}

/// O que cada espectador faz em cada faixa.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Route {
    /// De quem vem cada faixa (None = do host).
    pub parent: Vec<Option<u64>>,
    /// Para quem repassar cada faixa.
    pub children: Vec<Vec<u64>>,
}

impl Route {
    fn new(lanes: usize) -> Self {
        Self { parent: vec![None; lanes], children: vec![vec![]; lanes] }
    }

    /// Faixas que o host manda direto (bit `l` = faixa `l`).
    pub fn direct_mask(&self) -> u32 {
        self.parent.iter().enumerate().filter(|(_, p)| p.is_none()).fold(0, |m, (l, _)| m | (1 << l))
    }
}

/// Calcula as rotas. `bad`: ligações (pai, filho) que falharam e não devem ser usadas.
/// Determinístico: os mesmos espectadores dão as mesmas rotas.
pub fn routes(members: &[Member], lanes: usize, bad: &HashSet<(u64, u64)>) -> BTreeMap<u64, Route> {
    let mut out: BTreeMap<u64, Route> = members.iter().map(|m| (m.id, Route::new(lanes))).collect();
    let mut relays: Vec<u64> = members.iter().filter(|m| m.relay).map(|m| m.id).collect();
    relays.sort_unstable();
    // Com um só espectador (ou ninguém para repassar) não há malha: tudo direto.
    if members.len() < 2 || relays.is_empty() || lanes == 0 {
        return out;
    }
    let mut everyone: Vec<u64> = members.iter().map(|m| m.id).collect();
    everyone.sort_unstable();
    for lane in 0..lanes {
        let interior: Vec<u64> = relays.iter().enumerate().filter(|(i, _)| i % lanes == lane).map(|(_, id)| *id).collect();
        if interior.is_empty() {
            continue; // menos repassadores que faixas: esta faixa vai direto para todos
        }
        let mut load = vec![0usize; interior.len()];
        let others = everyone.iter().filter(|id| !interior.contains(id));
        for (n, &child) in others.enumerate() {
            // Começa num interno diferente para cada filho (reparte) e pula ligações ruins
            // e internos lotados.
            let pick = (0..interior.len())
                .map(|j| (n + j) % interior.len())
                .find(|&j| load[j] < MAX_CHILDREN && !bad.contains(&(interior[j], child)));
            let Some(j) = pick else { continue }; // sem pai possível: direto do host
            load[j] += 1;
            let parent = interior[j];
            out.get_mut(&child).unwrap().parent[lane] = Some(parent);
            out.get_mut(&parent).unwrap().children[lane].push(child);
        }
    }
    out
}

/// Quantas "faixas" o host manda no total (custo de upload do host, em faixas).
pub fn host_lanes(routes: &BTreeMap<u64, Route>) -> usize {
    routes.values().map(|r| r.direct_mask().count_ones() as usize).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members(n: u64) -> Vec<Member> {
        (1..=n).map(|id| Member { id, relay: true }).collect()
    }

    /// Cada espectador recebe cada faixa exatamente uma vez (do host ou de um pai, que tem
    /// ele como filho), ninguém é pai de si mesmo e só repassadores têm filhos.
    fn check(m: &[Member], r: &BTreeMap<u64, Route>, lanes: usize) {
        for mem in m {
            let route = &r[&mem.id];
            for lane in 0..lanes {
                if let Some(p) = route.parent[lane] {
                    assert_ne!(p, mem.id, "pai de si mesmo");
                    assert!(r[&p].children[lane].contains(&mem.id), "pai {p} não tem {} como filho na faixa {lane}", mem.id);
                    assert!(r[&p].parent[lane].is_none(), "o pai recebe a faixa direto (dois níveis)");
                }
                for c in &route.children[lane] {
                    assert_eq!(r[c].parent[lane], Some(mem.id));
                }
            }
            if !mem.relay {
                assert!(route.children.iter().all(|c| c.is_empty()), "{} não repassa e tem filhos", mem.id);
            }
        }
    }

    #[test]
    fn one_viewer_gets_everything_direct() {
        let m = members(1);
        let r = routes(&m, 4, &HashSet::new());
        assert_eq!(r[&1].direct_mask(), 0b1111);
        assert_eq!(host_lanes(&r), 4, "uma stream inteira");
    }

    #[test]
    fn four_viewers_each_relays_one_lane_and_the_host_sends_one_stream() {
        let m = members(4);
        let r = routes(&m, 4, &HashSet::new());
        check(&m, &r, 4);
        for (i, id) in (1..=4u64).enumerate() {
            assert_eq!(r[&id].direct_mask(), 1 << i, "{id} recebe direto só a faixa que repassa");
            assert_eq!(r[&id].children[i].len(), 3, "repassa a faixa dele para os outros 3");
        }
        assert_eq!(host_lanes(&r), 4, "o host manda 1 stream, não 4");
    }

    #[test]
    fn more_viewers_split_the_children_and_the_host_cost_grows_slowly() {
        let m = members(12);
        let r = routes(&m, 4, &HashSet::new());
        check(&m, &r, 4);
        // 12 espectadores, 3 internos por faixa: o host manda 12 faixas = 3 streams (era 12).
        assert_eq!(host_lanes(&r), 12);
        let max_children = r.values().flat_map(|x| x.children.iter().map(|c| c.len())).max().unwrap();
        assert!(max_children <= 3, "9 filhos por faixa repartidos entre 3 internos: {max_children}");
    }

    #[test]
    fn viewers_that_cannot_relay_only_receive() {
        let mut m = members(6);
        m[1].relay = false;
        m[4].relay = false;
        let r = routes(&m, 4, &HashSet::new());
        check(&m, &r, 4);
    }

    #[test]
    fn a_bad_link_is_avoided_or_falls_back_to_the_host() {
        let m = members(8);
        let first = routes(&m, 4, &HashSet::new());
        // O filho 6 recebia a faixa 0 de um pai p (os internos da faixa 0 são 1 e 5): essa
        // ligação falhou.
        let (child, lane) = (6u64, 0usize);
        let p = first[&child].parent[lane].expect("tinha pai");
        let bad: HashSet<(u64, u64)> = [(p, child)].into();
        let r = routes(&m, 4, &bad);
        check(&m, &r, 4);
        assert_ne!(r[&child].parent[lane], Some(p), "não usa a ligação ruim");
        // Faixa com um interno só e a ligação com ele ruim: vem do host.
        let m4 = members(4);
        let r4 = routes(&m4, 4, &[(1, 2)].into());
        assert_eq!(r4[&2].parent[0], None);
        assert!(r4[&2].direct_mask() & 1 != 0);
    }

    #[test]
    fn the_same_viewers_give_the_same_routes() {
        let m = members(9);
        let mut shuffled = m.clone();
        shuffled.reverse();
        assert_eq!(routes(&m, 4, &HashSet::new()), routes(&shuffled, 4, &HashSet::new()));
    }

    #[test]
    fn children_per_lane_are_capped() {
        let m: Vec<Member> = (1..=30).map(|id| Member { id, relay: id <= 4 }).collect();
        let r = routes(&m, 4, &HashSet::new());
        check(&m, &r, 4);
        for x in r.values() {
            assert!(x.children.iter().all(|c| c.len() <= MAX_CHILDREN));
        }
        // 26 que só recebem, 1 interno por faixa com no máximo 6 filhos: o resto vem do host.
        assert!(r.values().any(|x| x.parent.iter().any(|p| p.is_none()) && x.children.iter().all(|c| c.is_empty())));
    }
}
