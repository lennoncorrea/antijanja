//! Malha P2P em rede (etapa 2; docs/malha-p2p.md): conexões entre espectadores,
//! repasse das faixas e a ponte entre o coordenador (`mesh`) e os espectadores.
//!
//! - `MeshLink`: conexão WebRTC entre dois espectadores, só com o canal dos pedaços ("aj-m",
//!   fora de ordem e sem reenvio). Quem repassa (pai) faz a oferta; a troca de oferta e
//!   resposta passa pelo canal "aj" de cada um com o host.
//! - `ViewerMesh` (espectador): segue a rota que o host manda, abre e fecha as ligações,
//!   repassa na hora os pedaços das faixas em que é interno, avisa o host quando passa a
//!   receber uma faixa de um vizinho ("fed") e quando o vizinho para ("starve").
//! - `MeshHub` (host): liga o `Coordinator` aos espectadores conectados.

use crate::mesh::{Coordinator, Out, Route};
use crate::transport::{build_pc, err, spawn, wait_gathering, Handler, HostPeer, Res};
use bytes::BytesMut;
use rtc::peer_connection::sdp::RTCSessionDescription;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
use webrtc::peer_connection::PeerConnection;
use webrtc::runtime::channel;

const LINK_LABEL: &str = "aj-m";
/// Vizinho que mandava uma faixa e parou por esse tempo (com o resto chegando): a faixa
/// volta a vir do host.
const STARVE_AFTER: Duration = Duration::from_millis(400);
/// Ligação nova que não entregou nada nesse tempo: não deu certo (NAT, por exemplo).
const LINK_TIMEOUT: Duration = Duration::from_secs(5);
/// Rota trocada: a antiga continua valendo para repassar por esse tempo (fazer antes de
/// desfazer). Sem isso, numa troca de várias faixas ao mesmo tempo os pais antigos paravam
/// de repassar antes de os novos começarem, faltava mais que a folga cobre e um quadro se
/// perdia.
const ROUTE_GRACE: Duration = Duration::from_millis(500);
/// "O resto está chegando": algum pedaço nesse intervalo (senão o problema é a própria
/// conexão, ou a transmissão parou, e trocar de vizinho não adianta).
const ALIVE: Duration = Duration::from_millis(150);

/* ================================================================== ligação */

/// Conexão entre dois espectadores (só pedaços).
pub struct MeshLink {
    pub pc: Arc<dyn PeerConnection>,
    dc: Arc<Mutex<Option<Arc<dyn DataChannel>>>>,
    seq: AtomicU32,
}

impl MeshLink {
    /// Pai: cria a oferta (e o canal).
    pub async fn offer(stun: bool) -> Res<(Arc<MeshLink>, String)> {
        let (gtx, mut grx) = channel::<()>(1);
        let handler = Arc::new(Handler { gathered: gtx, on_state: Arc::new(|_| {}), on_track: None, on_dc: None });
        let pc = build_pc(handler, stun).await?;
        let init = RTCDataChannelInit { ordered: false, max_retransmits: Some(0), ..Default::default() };
        let dc = pc.create_data_channel(LINK_LABEL, Some(init)).await.map_err(err)?;
        // Nada chega por ele no pai; só esvazia os eventos.
        let d = dc.clone();
        spawn(async move { while let Some(ev) = d.poll().await { if matches!(ev, DataChannelEvent::OnClose) { break; } } });
        let offer = pc.create_offer(None).await.map_err(err)?;
        pc.set_local_description(offer).await.map_err(err)?;
        wait_gathering(&mut grx).await;
        let sdp = pc.local_description().await.ok_or("sem descrição local")?.sdp;
        Ok((Arc::new(MeshLink { pc, dc: Arc::new(Mutex::new(Some(dc))), seq: AtomicU32::new(0) }), sdp))
    }

    /// Filho: responde à oferta; cada mensagem que chegar vai para `on_piece`.
    pub async fn answer(offer: String, stun: bool, on_piece: Arc<dyn Fn(Vec<u8>) + Send + Sync>) -> Res<(Arc<MeshLink>, String)> {
        let (gtx, mut grx) = channel::<()>(1);
        let slot: Arc<Mutex<Option<Arc<dyn DataChannel>>>> = Arc::new(Mutex::new(None));
        let s = slot.clone();
        let on_dc: Arc<dyn Fn(Arc<dyn DataChannel>) + Send + Sync> = Arc::new(move |dc: Arc<dyn DataChannel>| {
            dbg_mesh("filho: canal da ligação chegou");
            *s.lock().unwrap() = Some(dc.clone());
            let on_piece = on_piece.clone();
            spawn(async move {
                while let Some(ev) = dc.poll().await {
                    match ev {
                        DataChannelEvent::OnMessage(m) => on_piece(m.data.to_vec()),
                        DataChannelEvent::OnClose => break,
                        _ => {}
                    }
                }
            });
        });
        let handler = Arc::new(Handler { gathered: gtx, on_state: Arc::new(|_| {}), on_track: None, on_dc: Some(on_dc) });
        let pc = build_pc(handler, stun).await?;
        pc.set_remote_description(RTCSessionDescription::offer(offer).map_err(err)?).await.map_err(err)?;
        let answer = pc.create_answer(None).await.map_err(err)?;
        pc.set_local_description(answer).await.map_err(err)?;
        wait_gathering(&mut grx).await;
        let sdp = pc.local_description().await.ok_or("sem descrição local")?.sdp;
        Ok((Arc::new(MeshLink { pc, dc: slot, seq: AtomicU32::new(0) }), sdp))
    }

    pub async fn accept_answer(&self, sdp: String) -> Res<()> {
        self.pc.set_remote_description(RTCSessionDescription::answer(sdp).map_err(err)?).await.map_err(err)
    }

    /// Repassa um pedaço (com o número sequencial desta ligação na frente).
    pub async fn send(&self, piece: &[u8]) {
        let dc = self.dc.lock().unwrap().clone();
        let Some(dc) = dc else { return };
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut b = BytesMut::with_capacity(4 + piece.len());
        b.extend_from_slice(&seq.to_le_bytes());
        b.extend_from_slice(piece);
        let _ = dc.send(b).await;
    }

    pub fn close(self: &Arc<Self>) {
        let pc = self.pc.clone();
        spawn(async move {
            let _ = pc.close().await;
        });
    }
}

/* ============================================================== espectador */

#[derive(Clone, Default)]
struct LaneState {
    /// Já avisou o host que recebe esta faixa do pai.
    fed: bool,
    last_parent: Option<Instant>,
    /// Já reclamou do pai desta faixa (espera rota nova).
    starved: bool,
    /// Desde quando espera o pai começar a mandar (rota nova ou depois de uma queda da
    /// própria conexão).
    waiting_since: Option<Instant>,
}

/// Ligação com um filho: `nonce` identifica a oferta (a resposta tem que ser dela);
/// `link` fica None enquanto a oferta está sendo criada.
struct ChildLink {
    nonce: u64,
    link: Option<Arc<MeshLink>>,
}

#[derive(Default)]
struct Inner {
    route: Option<Route>,
    /// Ligações em que este espectador é o pai (pelo id do filho).
    children: HashMap<u64, ChildLink>,
    /// Ligações com os pais (pelo id do pai).
    parents: HashMap<u64, Arc<MeshLink>>,
    lanes: Vec<LaneState>,
    route_at: Option<Instant>,
    last_any: Option<Instant>,
    /// Rota anterior, que ainda vale para repassar até `ROUTE_GRACE` depois da troca.
    old_route: Option<Route>,
    /// Ligações com filhos que saíram da rota: fecham depois do período de graça.
    retiring: Vec<(u64, Arc<MeshLink>, Instant)>,
    /// Vizinhos já denunciados por quadro adulterado (e quando).
    reported: HashMap<u64, Instant>,
}

/// A parte da malha de um espectador.
pub struct ViewerMesh {
    stun: bool,
    inner: Mutex<Inner>,
    to_host: Arc<OnceLock<Arc<dyn DataChannel>>>,
    /// Pedaços que chegam dos vizinhos vão para o mesmo receptor dos pedaços do host.
    pieces_tx: tokio::sync::mpsc::Sender<(u64, Vec<u8>)>,
}

impl ViewerMesh {
    pub fn new(stun: bool, to_host: Arc<OnceLock<Arc<dyn DataChannel>>>, pieces_tx: tokio::sync::mpsc::Sender<(u64, Vec<u8>)>) -> Arc<Self> {
        Arc::new(Self { stun, inner: Mutex::new(Inner::default()), to_host, pieces_tx })
    }

    fn send_host(&self, msg: Value) {
        if let Some(dc) = self.to_host.get().cloned() {
            let text = msg.to_string();
            spawn(async move { crate::transport::send_reliably(&dc, &text).await });
        }
    }

    /// Mensagem do host. true = era da malha (já tratada).
    pub fn on_host_message(self: &Arc<Self>, v: &Value) -> bool {
        match v["t"].as_str() {
            Some("route") => {
                if let Some(r) = parse_route(v) {
                    self.set_route(r);
                }
                true
            }
            Some("signal") => {
                let (Some(from), Some(sdp)) = (v["from"].as_u64(), v["sdp"].as_str()) else { return true };
                match v["kind"].as_str() {
                    Some("offer") => self.accept_offer(from, sdp.to_string(), v["link"].as_u64().unwrap_or(0)),
                    Some("answer") => {
                        let nonce = v["link"].as_u64();
                        let link = self.inner.lock().unwrap().children.get(&from).filter(|c| Some(c.nonce) == nonce).and_then(|c| c.link.clone());
                        if let Some(link) = link {
                            let sdp = sdp.to_string();
                            spawn(async move {
                                let _ = link.accept_answer(sdp).await;
                            });
                        }
                    }
                    _ => {}
                }
                true
            }
            _ => false,
        }
    }

    fn set_route(self: &Arc<Self>, r: Route) {
        let mut new_children = vec![];
        {
            let mut g = self.inner.lock().unwrap();
            let lanes = r.parent.len();
            if g.lanes.len() != lanes {
                g.lanes = vec![LaneState::default(); lanes];
            }
            let old_route = g.route.clone();
            let now = Instant::now();
            for (l, st) in g.lanes.iter_mut().enumerate() {
                let old = old_route.as_ref().and_then(|o| o.parent.get(l).copied().flatten());
                if old != r.parent[l] {
                    *st = LaneState { waiting_since: Some(now), ..Default::default() };
                }
            }
            let want_children: std::collections::HashSet<u64> = r.children.iter().flatten().copied().collect();
            let want_parents: std::collections::HashSet<u64> = r.parent.iter().flatten().copied().collect();
            let now = Instant::now();
            let mut retiring = vec![];
            g.children.retain(|id, c| {
                let keep = want_children.contains(id);
                if !keep {
                    if let Some(l) = &c.link {
                        retiring.push((*id, l.clone(), now));
                    }
                }
                keep
            });
            g.retiring.extend(retiring);
            g.parents.retain(|id, link| {
                let keep = want_parents.contains(id);
                if !keep {
                    link.close();
                }
                keep
            });
            for c in want_children {
                if !g.children.contains_key(&c) {
                    // Registra já (sem a ligação): outra rota chegando enquanto a oferta é
                    // criada não abre uma segunda ligação para o mesmo filho.
                    let mut b = [0u8; 8];
                    let _ = getrandom::fill(&mut b);
                    let nonce = u64::from_le_bytes(b) >> 12; // cabe num número do JSON
                    g.children.insert(c, ChildLink { nonce, link: None });
                    new_children.push((c, nonce));
                }
            }
            g.old_route = g.route.take();
            g.route = Some(r);
            g.route_at = Some(Instant::now());
        }
        // Pai: oferece a ligação a cada filho novo (pelo host).
        for (child, nonce) in new_children {
            let me = self.clone();
            spawn(async move {
                match MeshLink::offer(me.stun).await {
                    Ok((link, sdp)) => {
                        dbg_mesh(&format!("pai: oferta pronta para {child}"));
                        let current = {
                            let mut g = me.inner.lock().unwrap();
                            match g.children.get_mut(&child) {
                                Some(c) if c.nonce == nonce => {
                                    c.link = Some(link.clone());
                                    true
                                }
                                _ => false,
                            }
                        };
                        if current {
                            me.send_host(json!({ "t": "mesh", "ev": "signal", "to": child, "kind": "offer", "link": nonce, "sdp": sdp }));
                        } else {
                            link.close(); // a rota mudou enquanto a oferta era criada
                        }
                    }
                    Err(e) => eprintln!("malha: oferta para {child}: {e}"),
                }
            });
        }
    }

    fn accept_offer(self: &Arc<Self>, from: u64, sdp: String, nonce: u64) {
        let me = self.clone();
        let tx = self.pieces_tx.clone();
        spawn(async move {
            let on_piece: Arc<dyn Fn(Vec<u8>) + Send + Sync> = Arc::new(move |d| {
                let _ = tx.try_send((from, d));
            });
            match MeshLink::answer(sdp, me.stun, on_piece).await {
                Ok((link, answer)) => {
                    dbg_mesh(&format!("filho: resposta pronta para {from}"));
                    if let Some(old) = me.inner.lock().unwrap().parents.insert(from, link) {
                        old.close();
                    }
                    me.send_host(json!({ "t": "mesh", "ev": "signal", "to": from, "kind": "answer", "link": nonce, "sdp": answer }));
                }
                Err(e) => eprintln!("malha: resposta para {from}: {e}"),
            }
        });
    }

    /// Pedaço que chegou (`src` = 0: do host; senão o id do vizinho). Repassa se este
    /// espectador é interno na faixa, e registra de quem a faixa está vindo.
    pub async fn on_piece(&self, src: u64, piece: &[u8], now: Instant) {
        let Some(lane) = crate::pieces::lane_of(piece).map(|l| l as usize) else { return };
        let (targets, fed_now) = {
            let mut g = self.inner.lock().unwrap();
            g.last_any = Some(now);
            let Some(route) = g.route.clone() else { return };
            let Some(parent) = route.parent.get(lane).copied() else { return };
            let mut fed_now = false;
            if let (Some(p), Some(st)) = (parent, g.lanes.get_mut(lane)) {
                if p == src {
                    st.last_parent = Some(now);
                    if !st.fed {
                        st.fed = true;
                        fed_now = true;
                    }
                }
            }
            // Só repassa o que veio de quem devia mandar esta faixa (o host, se interno).
            let mut targets: Vec<Arc<MeshLink>> = if parent.unwrap_or(0) == src {
                route.children[lane].iter().filter_map(|c| g.children.get(c).and_then(|x| x.link.clone())).collect()
            } else {
                vec![]
            };
            // Período de graça: continua repassando como na rota anterior.
            let in_grace = g.route_at.is_some_and(|t| now.saturating_duration_since(t) < ROUTE_GRACE);
            if let (true, Some(old)) = (in_grace, g.old_route.as_ref()) {
                if old.parent.get(lane).copied().flatten().unwrap_or(0) == src {
                    for c in old.children.get(lane).into_iter().flatten() {
                        let link = g.children.get(c).and_then(|x| x.link.clone()).or_else(|| g.retiring.iter().find(|r| r.0 == *c).map(|r| r.1.clone()));
                        if let Some(l) = link {
                            if !targets.iter().any(|t| Arc::ptr_eq(t, &l)) {
                                targets.push(l);
                            }
                        }
                    }
                }
            }
            (targets, fed_now)
        };
        if fed_now {
            dbg_mesh(&format!("recebendo a faixa {lane} de {src}"));
            self.send_host(json!({ "t": "mesh", "ev": "fed", "lane": lane, "from": src }));
        }
        for t in targets {
            t.send(piece).await;
        }
    }

    /// Chamar de tempos em tempos: reclama dos vizinhos que pararam de mandar.
    pub fn tick(&self, now: Instant) {
        let mut starve: Vec<(usize, u64)> = vec![];
        let mut own = false;
        {
            let mut g = self.inner.lock().unwrap();
            // Ligações aposentadas: fecham depois do período de graça.
            g.retiring.retain(|(_, l, at)| {
                let keep = now.saturating_duration_since(*at) < ROUTE_GRACE;
                if !keep {
                    l.close();
                }
                keep
            });
            let alive = g.last_any.is_some_and(|t| now.saturating_duration_since(t) < ALIVE);
            let Some(route) = g.route.clone() else { return };
            let mut stopped_lanes = vec![];
            for (l, st) in g.lanes.iter_mut().enumerate() {
                if route.parent[l].is_none() || st.starved || !alive {
                    continue;
                }
                let stopped = st.fed && st.last_parent.is_some_and(|t| now.saturating_duration_since(t) > STARVE_AFTER);
                let never = !st.fed && st.waiting_since.is_some_and(|t| now.saturating_duration_since(t) > LINK_TIMEOUT);
                if stopped {
                    stopped_lanes.push(l);
                } else if never {
                    st.starved = true;
                    starve.push((l, route.parent[l].unwrap_or(0)));
                }
            }
            // Duas ou mais faixas de vizinhos diferentes pararam juntas: o problema é a
            // conexão deste espectador, não os vizinhos. A faixa volta do host sem culpar
            // ninguém, e quando o vizinho voltar a entregar ela sai do host de novo.
            let parents: HashSet<u64> = stopped_lanes.iter().filter_map(|&l| route.parent[l]).collect();
            own |= parents.len() >= 2;
            for l in stopped_lanes {
                let st = &mut g.lanes[l];
                if own {
                    *st = LaneState { waiting_since: Some(now), ..Default::default() };
                } else {
                    st.starved = true;
                }
                starve.push((l, route.parent[l].unwrap_or(0)));
            }
        }
        for (l, from) in starve {
            dbg_mesh(&format!("a faixa {l} parou de vir de {from}{}", if own { " (conexão própria)" } else { "" }));
            self.send_host(json!({ "t": "mesh", "ev": "starve", "lane": l, "from": from, "own": own }));
        }
    }

    /// Quadros que não conferiram com a assinatura tinham pedaços destes vizinhos: avisa o
    /// host (cada um no máximo a cada 5 s).
    pub fn report_forged(&self, suspects: &[u64]) {
        let now = Instant::now();
        let mut send = vec![];
        {
            let mut g = self.inner.lock().unwrap();
            for &s in suspects.iter().filter(|&&s| s != 0) {
                if g.reported.get(&s).is_none_or(|t| now.saturating_duration_since(*t) > Duration::from_secs(5)) {
                    g.reported.insert(s, now);
                    send.push(s);
                }
            }
        }
        for s in send {
            dbg_mesh(&format!("quadro adulterado com pedaços de {s}"));
            self.send_host(json!({ "t": "mesh", "ev": "forged", "from": s }));
        }
    }

    pub fn close(&self) {
        let mut g = self.inner.lock().unwrap();
        let mut links: Vec<Arc<MeshLink>> = g.children.drain().filter_map(|(_, c)| c.link).collect();
        links.extend(g.retiring.drain(..).map(|(_, l, _)| l));
        links.extend(g.parents.drain().map(|(_, l)| l));
        drop(g);
        for l in links {
            l.close();
        }
    }
}

fn dbg_mesh(m: &str) {
    if std::env::var_os("AJ_TRACE").is_some() {
        eprintln!("[malha] {m}");
    }
}

fn parse_route(v: &Value) -> Option<Route> {
    let parent = v["parents"].as_array()?.iter().map(|p| p.as_u64().filter(|&id| id != 0)).collect::<Vec<_>>();
    let children = v["children"]
        .as_array()?
        .iter()
        .map(|c| c.as_array().map(|a| a.iter().filter_map(|x| x.as_u64()).collect()).unwrap_or_default())
        .collect::<Vec<_>>();
    (parent.len() == children.len()).then_some(Route { parent, children })
}

fn route_json(r: &Route) -> Value {
    json!({
        "t": "route",
        "parents": r.parent.iter().map(|p| p.unwrap_or(0)).collect::<Vec<_>>(),
        "children": r.children,
    })
}

/* ==================================================================== host */

/// O coordenador da malha ligado aos espectadores conectados.
pub struct MeshHub {
    coord: Mutex<Coordinator>,
    peers: Mutex<HashMap<u64, Arc<HostPeer>>>,
}

impl MeshHub {
    pub fn new(lanes: usize) -> Arc<Self> {
        Arc::new(Self { coord: Mutex::new(Coordinator::new(lanes)), peers: Mutex::new(HashMap::new()) })
    }

    pub fn contains(&self, id: u64) -> bool {
        self.peers.lock().unwrap().contains_key(&id)
    }

    /// Ids dos espectadores na malha.
    pub fn ids(&self) -> Vec<u64> {
        self.peers.lock().unwrap().keys().copied().collect()
    }

    pub fn join(&self, id: u64, peer: Arc<HostPeer>) {
        self.peers.lock().unwrap().insert(id, peer);
        let outs = self.coord.lock().unwrap().join(id, true);
        self.apply(outs);
    }

    pub fn leave(&self, id: u64) {
        if let Some(p) = self.peers.lock().unwrap().remove(&id) {
            // Continua conectado (ex.: foi para outra versão do vídeo): tudo direto.
            p.set_direct_lanes(u32::MAX);
        }
        let outs = self.coord.lock().unwrap().leave(id);
        self.apply(outs);
    }

    /// Diagnóstico: (id, faixas mandadas direto, rota) de cada espectador na malha.
    pub fn snapshot(&self) -> Vec<(u64, u32, Option<Route>)> {
        let c = self.coord.lock().unwrap();
        let ids: Vec<u64> = self.peers.lock().unwrap().keys().copied().collect();
        let mut v: Vec<_> = ids.into_iter().map(|id| (id, c.direct_mask(id), c.route(id).cloned())).collect();
        v.sort_by_key(|x| x.0);
        v
    }

    /// Chamar de tempos em tempos (perdoa ligações, devolve rebaixados).
    pub fn tick(&self) {
        let outs = self.coord.lock().unwrap().expire(Instant::now());
        self.apply(outs);
    }

    /// Mensagem de um espectador. true = era da malha.
    pub fn on_message(&self, from: u64, v: &Value) -> bool {
        if v["t"] != "mesh" {
            return false;
        }
        match v["ev"].as_str() {
            Some("fed") => {
                if let (Some(l), Some(p)) = (v["lane"].as_u64(), v["from"].as_u64()) {
                    let outs = self.coord.lock().unwrap().fed(from, l as usize, p);
                    self.apply(outs);
                }
            }
            Some("starve") => {
                if let (Some(l), Some(p)) = (v["lane"].as_u64(), v["from"].as_u64()) {
                    let own = v["own"].as_bool().unwrap_or(false);
                    let outs = self.coord.lock().unwrap().starve(from, l as usize, p, own, Instant::now());
                    self.apply(outs);
                }
            }
            Some("forged") => {
                if let Some(p) = v["from"].as_u64() {
                    let outs = self.coord.lock().unwrap().forged(from, p, Instant::now());
                    self.apply(outs);
                }
            }
            Some("signal") => {
                let to = v["to"].as_u64().unwrap_or(0);
                let target = self.peers.lock().unwrap().get(&to).cloned();
                if let Some(t) = target {
                    let msg = json!({ "t": "signal", "from": from, "kind": v["kind"], "link": v["link"], "sdp": v["sdp"] }).to_string();
                    spawn(async move { t.send_message(&msg).await });
                }
            }
            _ => {}
        }
        true
    }

    fn apply(&self, outs: Vec<Out>) {
        let peers = self.peers.lock().unwrap();
        for o in outs {
            match o {
                Out::Route { to, route } => {
                    if let Some(p) = peers.get(&to).cloned() {
                        let msg = route_json(&route).to_string();
                        spawn(async move { p.send_message(&msg).await });
                    }
                }
                Out::Direct { to, mask } => {
                    if let Some(p) = peers.get(&to) {
                        p.set_direct_lanes(mask);
                    }
                }
            }
        }
    }
}

/// Para os testes e o diagnóstico: a ligação está com o canal aberto?
#[allow(dead_code)]
pub fn link_ready(l: &MeshLink) -> bool {
    l.dc.lock().unwrap().is_some()
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{block_on, ViewerSession, ViewerSinks};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};

    /// Quadro sintético `idx`: começo de NAL H.264 (5 = keyframe, 1 = normal), o número do
    /// quadro e um enchimento que depende dele (dá para conferir que chegou intacto).
    fn frame(idx: u32) -> Vec<u8> {
        frame_kf(idx, idx % 60 == 0)
    }

    fn frame_kf(idx: u32, key: bool) -> Vec<u8> {
        let len = if key { 60_000 } else { 3_000 + (idx as usize * 7919) % 17_000 };
        let mut d = vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }];
        d.extend_from_slice(&idx.to_le_bytes());
        d.extend((d.len()..len).map(|i| (i as u32).wrapping_mul(idx + 1) as u8));
        d
    }

    fn intact(d: &[u8]) -> Option<u32> {
        if d.len() < 9 {
            return None;
        }
        let idx = u32::from_le_bytes([d[5], d[6], d[7], d[8]]);
        (frame_kf(idx, d[4] == 0x65) == d).then_some(idx)
    }

    struct Viewer {
        session: Arc<ViewerSession>,
        peer: Arc<HostPeer>,
        got: Arc<Mutex<Vec<u32>>>,
        corrupt: Arc<AtomicUsize>,
    }

    /// Host e 4 espectadores na mesma máquina, malha ligada. Confere que todos recebem os
    /// quadros intactos, mede o upload do host e derruba um repassador no meio.
    /// `cargo test mesh_four_viewers -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn mesh_four_viewers() {
        const N: u64 = 4;
        let hub = MeshHub::new(crate::pieces::LANES as usize);
        // Pedido de keyframe de qualquer espectador: o próximo quadro sai como keyframe
        // (como o app faz, no máximo 1 por segundo).
        let want_kf = Arc::new(AtomicBool::new(false));
        let mut viewers: Vec<(u64, Viewer)> = vec![];
        for id in 1..=N {
            let got = Arc::new(Mutex::new(Vec::new()));
            let corrupt = Arc::new(AtomicUsize::new(0));
            let (g, c) = (got.clone(), corrupt.clone());
            let sinks = ViewerSinks {
                on_video: Arc::new(move |d, _| match intact(&d) {
                    Some(i) => g.lock().unwrap().push(i),
                    None => {
                        c.fetch_add(1, Ordering::SeqCst);
                    }
                }),
                on_audio: Arc::new(|_| {}),
                on_state: Arc::new(|_| {}),
                on_message: Arc::new(|_| {}),
            };
            let (session, offer) = block_on(ViewerSession::offer(sinks, false)).expect("oferta");
            let h = hub.clone();
            let wk = want_kf.clone();
            let (ctx, crx) = std::sync::mpsc::channel();
            let (peer, answer) = block_on(HostPeer::answer(
                offer,
                false,
                Arc::new(move |e| {
                    if e == crate::transport::PeerEvent::Connected {
                        let _ = ctx.send(());
                    }
                }),
                Arc::new(move |m| {
                    if let Ok(v) = serde_json::from_str::<Value>(&m) {
                        if v["t"] == "kf" {
                            wk.store(true, Ordering::SeqCst);
                        }
                        h.on_message(id, &v);
                    }
                }),
            ))
            .expect("resposta");
            block_on(session.accept_answer(answer)).unwrap();
            crx.recv_timeout(Duration::from_secs(10)).expect("não conectou");
            for _ in 0..100 {
                if peer.has_pieces() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(peer.has_pieces());
            viewers.push((id, Viewer { session, peer, got, corrupt }));
        }
        // Chave que assina os quadros: vai para cada um antes de entrar na malha (como o host).
        let signer = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);
        let pk: String = signer.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        for (_, v) in &viewers {
            block_on(v.peer.send_message(&json!({ "t": "stream-key", "pk": pk }).to_string()));
        }
        for (id, v) in &viewers {
            hub.join(*id, v.peer.clone());
        }

        let mut source = crate::pieces::PieceSource::new(crate::transport::PIECE_OVERHEAD);
        source.survive_lane = true;
        source.signer = Some(signer);
        let (secs, drop_at) = (12u32, 7u32);
        let start = Instant::now();
        let dropped = AtomicBool::new(false);
        let mut last_kf = Instant::now() - Duration::from_secs(5);
        let mut forced = 0u32;
        let (mut naive, mut steady_naive, mut steady_from, mut steady_to) = (0u64, 0u64, 0u64, 0u64);
        let host_sent = |vs: &Vec<(u64, Viewer)>| vs.iter().map(|(_, v)| v.peer.pieces_sent.load(Ordering::Relaxed)).sum::<u64>();
        for idx in 0..60 * secs {
            let at = start + Duration::from_micros(idx as u64 * 16_667);
            if let Some(w) = at.checked_duration_since(Instant::now()) {
                std::thread::sleep(w);
            }
            let mut key = idx % 60 == 0;
            if want_kf.swap(false, Ordering::SeqCst) && last_kf.elapsed() >= Duration::from_secs(1) && idx > 0 {
                key = true;
                forced += 1;
            }
            if key {
                last_kf = Instant::now();
            }
            let f = source.encode(0, &frame_kf(idx, key), Duration::from_micros(16_667)).unwrap();
            let alive = viewers.iter().filter(|(id, _)| !(dropped.load(Ordering::SeqCst) && *id == 1)).count() as u64;
            naive += f.pieces.len() as u64 * alive;
            for (id, v) in &viewers {
                if dropped.load(Ordering::SeqCst) && *id == 1 {
                    continue;
                }
                v.peer.send_pieces(&f);
            }
            // Medição em regime: do 4º ao 6º segundo (malha montada, antes da queda).
            if idx == 60 * 4 {
                steady_from = host_sent(&viewers);
                steady_naive = naive;
            }
            if idx == 60 * 5 {
                for (id, mask, route) in hub.snapshot() {
                    println!("  {id}: direto {mask:04b}, rota {route:?}");
                }
            }
            if idx == 60 * 6 {
                steady_to = host_sent(&viewers);
                steady_naive = naive - steady_naive;
            }
            // O espectador 1 (repassador da faixa 0) some sem avisar; o host percebe depois.
            if idx == 60 * drop_at {
                dropped.store(true, Ordering::SeqCst);
                let (s, p) = (viewers[0].1.session.clone(), viewers[0].1.peer.clone());
                s.mesh.close();
                let _ = block_on(s.pc.close());
                let _ = block_on(p.pc.close());
            }
            if idx == 60 * drop_at + 30 {
                hub.leave(1);
            }
        }
        std::thread::sleep(Duration::from_millis(600));
        let total = 60 * secs;
        println!("keyframes pedidos e atendidos: {forced}");
        let host_steady = steady_to - steady_from;
        println!(
            "upload do host em regime: {host_steady} pedaços (mandando tudo para todos seriam {steady_naive}: {:.0}%)",
            host_steady as f64 * 100.0 / steady_naive as f64
        );
        for (id, v) in &viewers {
            let got = v.got.lock().unwrap();
            let after_drop = got.iter().filter(|&&i| i >= 60 * drop_at).count();
            let mut sorted = got.clone();
            sorted.dedup();
            println!(
                "espectador {id}: {} de {total} quadros, {} corrompidos, depois da queda {after_drop} de {}",
                got.len(), v.corrupt.load(Ordering::SeqCst), total - 60 * drop_at
            );
            assert_eq!(v.corrupt.load(Ordering::SeqCst), 0);
            assert!(got.windows(2).all(|w| w[0] < w[1]), "fora de ordem ou repetido");
            if *id != 1 {
                assert!(got.len() as u32 >= total * 85 / 100, "espectador {id} perdeu quadros demais");
                assert!(after_drop as u32 >= (total - 60 * drop_at) * 80 / 100, "espectador {id} não se recuperou da queda");
            }
        }
        assert!(host_steady * 100 / steady_naive.max(1) <= 50, "o host devia mandar bem menos que tudo para todos");
        let _ = AtomicU64::new(0);
    }
}
