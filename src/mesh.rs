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
use std::time::{Duration, Instant};

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

/// Mensagem do coordenador para um espectador.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// A rota dele mudou (de quem recebe e para quem repassa cada faixa).
    Route { to: u64, route: Route },
    /// As faixas que o host manda direto para ele mudaram.
    Direct { to: u64, mask: u32 },
}

/// Ligação que falhou fica fora das rotas por esse tempo (depois tenta de novo: pode ter
/// sido um tropeço passageiro).
pub const BAD_FORGIVE: Duration = Duration::from_secs(30);
/// Janela das reclamações contra um repassador.
pub const STRIKE_WINDOW: Duration = Duration::from_secs(60);
/// Reclamações (de espectadores diferentes) que tiram alguém de repassador.
pub const STRIKES_TO_DEMOTE: usize = 2;
/// Quanto tempo quem foi rebaixado fica só recebendo.
pub const DEMOTE_FOR: Duration = Duration::from_secs(60);

/// Mantém as rotas da malha no host. Regra de segurança: o host só para de mandar uma
/// faixa direto depois que o espectador avisa que já recebe essa faixa do vizinho
/// (`fed`); se o vizinho parar de mandar (`starve`), o host volta a mandar e evita aquela
/// ligação por um tempo. Repassador que atrapalha espectadores diferentes (ou manda quadro
/// adulterado) deixa de repassar por um tempo.
pub struct Coordinator {
    lanes: usize,
    /// Quem está na malha e se pode repassar (capacidade informada).
    members: BTreeMap<u64, bool>,
    routes: BTreeMap<u64, Route>,
    fed: HashSet<(u64, usize)>,
    /// Ligações (pai, filho) fora das rotas, e desde quando.
    bad: BTreeMap<(u64, u64), Instant>,
    masks: BTreeMap<u64, u32>,
    /// Reclamações contra cada repassador: (quando, quem reclamou, peso).
    strikes: BTreeMap<u64, Vec<(Instant, u64, usize)>>,
    /// Rebaixados (só recebem) até quando.
    demoted: BTreeMap<u64, Instant>,
}

impl Coordinator {
    pub fn new(lanes: usize) -> Self {
        Self {
            lanes,
            members: BTreeMap::new(),
            routes: BTreeMap::new(),
            fed: HashSet::new(),
            bad: BTreeMap::new(),
            masks: BTreeMap::new(),
            strikes: BTreeMap::new(),
            demoted: BTreeMap::new(),
        }
    }

    pub fn route(&self, id: u64) -> Option<&Route> {
        self.routes.get(&id)
    }

    /// Faixas que o host manda direto para `id` (todas, se ele não está na malha).
    pub fn direct_mask(&self, id: u64) -> u32 {
        self.masks.get(&id).copied().unwrap_or(u32::MAX)
    }

    pub fn is_demoted(&self, id: u64) -> bool {
        self.demoted.contains_key(&id)
    }

    pub fn join(&mut self, id: u64, relay: bool) -> Vec<Out> {
        self.members.insert(id, relay);
        self.recompute()
    }

    pub fn leave(&mut self, id: u64) -> Vec<Out> {
        self.members.remove(&id);
        self.masks.remove(&id);
        self.fed.retain(|&(c, _)| c != id);
        self.bad.retain(|&(a, b), _| a != id && b != id);
        self.strikes.remove(&id);
        self.demoted.remove(&id);
        self.recompute()
    }

    /// `child` já recebe a faixa `lane` do pai `from`: o host pode parar de mandá-la
    /// direto. Ignorado se `from` não é mais o pai dessa faixa (aviso de uma rota antiga).
    pub fn fed(&mut self, child: u64, lane: usize, from: u64) -> Vec<Out> {
        if self.parent_of(child, lane) != Some(from) || !self.fed.insert((child, lane)) {
            return vec![];
        }
        self.update_masks()
    }

    /// `child` parou de receber a faixa `lane` do pai `from`: volta a receber do host.
    /// `own`: várias faixas pararam juntas no `child` (a conexão dele é que está ruim): o
    /// vizinho não leva a culpa e a rota fica. Senão a ligação sai das rotas por um tempo e
    /// o vizinho leva uma reclamação. Ignorado se `from` não é mais o pai dessa faixa.
    pub fn starve(&mut self, child: u64, lane: usize, from: u64, own: bool, now: Instant) -> Vec<Out> {
        if self.parent_of(child, lane) != Some(from) {
            return vec![];
        }
        if own {
            self.fed.remove(&(child, lane));
            return self.update_masks();
        }
        self.bad.insert((from, child), now);
        self.strike(from, child, 1, now);
        self.recompute()
    }

    /// Um quadro com pedaços de `from` não conferiu com a assinatura: a ligação sai das
    /// rotas e `from` leva reclamação dobrada (adulterar é pior que falhar).
    pub fn forged(&mut self, child: u64, from: u64, now: Instant) -> Vec<Out> {
        if !self.members.contains_key(&from) || from == child {
            return vec![];
        }
        self.bad.insert((from, child), now);
        self.strike(from, child, STRIKES_TO_DEMOTE, now);
        self.recompute()
    }

    fn strike(&mut self, parent: u64, child: u64, weight: usize, now: Instant) {
        let list = self.strikes.entry(parent).or_default();
        list.retain(|(t, _, _)| now.saturating_duration_since(*t) < STRIKE_WINDOW);
        list.push((now, child, weight));
        // Conta por quem reclamou (um espectador com a conexão ruim não derruba ninguém).
        let mut by_child: BTreeMap<u64, usize> = BTreeMap::new();
        for (_, c, w) in list.iter() {
            let e = by_child.entry(*c).or_default();
            *e = (*e).max(*w);
        }
        if by_child.values().sum::<usize>() >= STRIKES_TO_DEMOTE {
            self.demoted.insert(parent, now + DEMOTE_FOR);
            self.strikes.remove(&parent);
        }
    }

    /// Chamar de tempos em tempos: perdoa ligações e devolve quem foi rebaixado.
    pub fn expire(&mut self, now: Instant) -> Vec<Out> {
        let (b0, d0) = (self.bad.len(), self.demoted.len());
        self.bad.retain(|_, t| now.saturating_duration_since(*t) < BAD_FORGIVE);
        self.demoted.retain(|_, until| now < *until);
        if self.bad.len() != b0 || self.demoted.len() != d0 {
            self.recompute()
        } else {
            vec![]
        }
    }

    fn parent_of(&self, child: u64, lane: usize) -> Option<u64> {
        self.routes.get(&child).and_then(|r| r.parent.get(lane).copied().flatten())
    }

    fn recompute(&mut self) -> Vec<Out> {
        let members: Vec<Member> =
            self.members.iter().map(|(&id, &relay)| Member { id, relay: relay && !self.demoted.contains_key(&id) }).collect();
        let bad: HashSet<(u64, u64)> = self.bad.keys().copied().collect();
        let new = routes(&members, self.lanes, &bad);
        let mut out = vec![];
        for (id, r) in &new {
            if self.routes.get(id) != Some(r) {
                out.push(Out::Route { to: *id, route: r.clone() });
            }
            // Pai novo numa faixa: até ele confirmar, a faixa volta a vir do host.
            let old = self.routes.get(id);
            for lane in 0..self.lanes {
                if old.map(|o| o.parent[lane]) != Some(r.parent[lane]) {
                    self.fed.remove(&(*id, lane));
                }
            }
        }
        self.routes = new;
        out.extend(self.update_masks());
        out
    }

    fn update_masks(&mut self) -> Vec<Out> {
        let mut out = vec![];
        for (id, r) in &self.routes {
            let mut mask = r.direct_mask();
            for lane in 0..self.lanes {
                if r.parent[lane].is_some() && !self.fed.contains(&(*id, lane)) {
                    mask |= 1 << lane;
                }
            }
            if self.masks.get(id) != Some(&mask) {
                self.masks.insert(*id, mask);
                out.push(Out::Direct { to: *id, mask });
            }
        }
        out
    }
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

    /// Todos confirmam todas as faixas que recebem de um pai.
    fn feed_all(c: &mut Coordinator, ids: std::ops::RangeInclusive<u64>) {
        for id in ids {
            for l in 0..4 {
                if let Some(p) = c.route(id).unwrap().parent[l] {
                    c.fed(id, l, p);
                }
            }
        }
    }

    fn masks(out: &[Out]) -> BTreeMap<u64, u32> {
        out.iter().filter_map(|o| match o { Out::Direct { to, mask } => Some((*to, *mask)), _ => None }).collect()
    }

    #[test]
    fn coordinator_keeps_sending_direct_until_the_neighbor_delivers() {
        let mut c = Coordinator::new(4);
        for id in 1..=4 {
            c.join(id, true);
        }
        // Rotas prontas, mas ninguém confirmou: o host ainda manda tudo para todos.
        for id in 1..=4 {
            assert_eq!(c.direct_mask(id), 0b1111);
        }
        let r2 = c.route(2).unwrap().clone();
        let lane = (0..4).find(|&l| r2.parent[l].is_some()).unwrap();
        let parent = r2.parent[lane].unwrap();
        assert!(c.fed(2, lane, parent + 100).is_empty(), "aviso de quem não é o pai: ignorado");
        let out = c.fed(2, lane, parent);
        assert_eq!(masks(&out).get(&2), Some(&(0b1111 & !(1 << lane))), "só essa faixa deixa de vir direto");
        // Confirmando tudo, o host manda a cada um só a faixa que ele repassa: 1 stream no total.
        feed_all(&mut c, 1..=4);
        let total: u32 = (1..=4).map(|id| c.direct_mask(id).count_ones()).sum();
        assert_eq!(total, 4);
    }

    #[test]
    fn coordinator_reroutes_when_a_neighbor_stops_or_leaves() {
        let mut c = Coordinator::new(4);
        for id in 1..=8 {
            c.join(id, true);
        }
        feed_all(&mut c, 1..=8);
        let parent = c.route(6).unwrap().parent[0].unwrap();
        // O pai parou de mandar a faixa 0 para o 6: rota nova sem essa ligação, e a faixa
        // volta direto até o pai novo confirmar.
        let out = c.starve(6, 0, parent, false, Instant::now());
        assert!(out.iter().any(|o| matches!(o, Out::Route { to: 6, .. })));
        assert_ne!(c.route(6).unwrap().parent[0], Some(parent));
        assert!(c.direct_mask(6) & 1 != 0);
        // Um repassador sai: quem dependia dele volta a receber direto aquela faixa.
        let leaving = 1u64;
        let deps: Vec<u64> = (2..=8).filter(|&id| c.route(id).unwrap().parent.contains(&Some(leaving))).collect();
        assert!(!deps.is_empty());
        c.leave(leaving);
        for id in deps {
            let r = c.route(id).unwrap();
            for l in 0..4 {
                assert_ne!(r.parent[l], Some(leaving));
            }
        }
        assert!(c.route(leaving).is_none());
    }

    #[test]
    fn coordinator_ignores_confirmations_for_lanes_without_a_parent() {
        let mut c = Coordinator::new(4);
        c.join(1, true);
        assert!(c.fed(1, 0, 2).is_empty(), "sozinho, tudo vem do host");
        assert_eq!(c.direct_mask(1), 0b1111);
        assert!(c.starve(1, 0, 2, false, Instant::now()).is_empty());
    }

    #[test]
    fn coordinator_forgives_bad_links_after_a_while() {
        let mut c = Coordinator::new(4);
        for id in 1..=8 {
            c.join(id, true);
        }
        feed_all(&mut c, 1..=8);
        let t0 = Instant::now();
        let parent = c.route(6).unwrap().parent[0].unwrap();
        c.starve(6, 0, parent, false, t0);
        assert_ne!(c.route(6).unwrap().parent[0], Some(parent));
        assert!(c.expire(t0 + BAD_FORGIVE / 2).is_empty(), "ainda fora");
        let out = c.expire(t0 + BAD_FORGIVE);
        assert!(!out.is_empty());
        assert_eq!(c.route(6).unwrap().parent[0], Some(parent), "perdoada: volta à rota original");
    }

    #[test]
    fn a_relay_that_fails_different_viewers_stops_relaying_for_a_while() {
        let mut c = Coordinator::new(4);
        for id in 1..=8 {
            c.join(id, true);
        }
        feed_all(&mut c, 1..=8);
        let t0 = Instant::now();
        // O 1 repassa a faixa 0. Falha para um filho: só a ligação sai.
        let kids: Vec<u64> = (2..=8).filter(|&id| c.route(id).unwrap().parent[0] == Some(1)).collect();
        assert!(kids.len() >= 2);
        c.starve(kids[0], 0, 1, false, t0);
        assert!(!c.is_demoted(1));
        // Falha para outro filho: rebaixado, ninguém mais recebe dele.
        let p = c.route(kids[1]).unwrap().parent[0];
        if p == Some(1) {
            c.starve(kids[1], 0, 1, false, t0);
        } else {
            c.forged(kids[1], 1, t0);
        }
        assert!(c.is_demoted(1));
        for id in 2..=8 {
            assert!(!c.route(id).unwrap().parent.contains(&Some(1)), "{id} ainda recebe do rebaixado");
        }
        assert!(c.route(1).unwrap().children.iter().all(|x| x.is_empty()));
        // Depois de um tempo volta a repassar.
        c.expire(t0 + DEMOTE_FOR);
        assert!(!c.is_demoted(1));
        assert!(c.route(1).unwrap().children.iter().any(|x| !x.is_empty()));
    }

    #[test]
    fn the_same_viewer_complaining_twice_does_not_demote_anyone() {
        let mut c = Coordinator::new(4);
        for id in 1..=8 {
            c.join(id, true);
        }
        feed_all(&mut c, 1..=8);
        let t0 = Instant::now();
        let parent = c.route(6).unwrap().parent[0].unwrap();
        c.starve(6, 0, parent, false, t0);
        c.expire(t0 + BAD_FORGIVE);
        feed_all(&mut c, 1..=8);
        let again = c.route(6).unwrap().parent[0].unwrap();
        c.starve(6, 0, again, false, t0 + BAD_FORGIVE);
        assert!(!c.is_demoted(parent) && !c.is_demoted(again), "a conexão do 6 é que é ruim");
    }

    #[test]
    fn own_connection_trouble_returns_the_lane_without_blaming_the_neighbor() {
        let mut c = Coordinator::new(4);
        for id in 1..=4 {
            c.join(id, true);
        }
        feed_all(&mut c, 1..=4);
        let r = c.route(2).unwrap().clone();
        let lane = (0..4).find(|&l| r.parent[l].is_some()).unwrap();
        let out = c.starve(2, lane, r.parent[lane].unwrap(), true, Instant::now());
        assert!(c.direct_mask(2) & (1 << lane) != 0, "a faixa volta a vir do host");
        assert_eq!(c.route(2).unwrap(), &r, "a rota fica");
        assert!(out.iter().all(|o| !matches!(o, Out::Route { .. })));
        // Quando o vizinho volta a entregar, o host para de novo.
        c.fed(2, lane, r.parent[lane].unwrap());
        assert!(c.direct_mask(2) & (1 << lane) == 0);
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
