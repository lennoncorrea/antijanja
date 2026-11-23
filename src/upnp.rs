//! Porta aberta no roteador pelo próprio app (UPnP), quando o roteador deixa.
//!
//! Atrás de um roteador, só entram pacotes de quem nós já chamamos: por isso a conexão
//! direta depende de os dois lados mandarem ao mesmo tempo (o "furo"), que falha quando o
//! roteador muda a porta a cada destino (NAT simétrico). Com a porta aberta (mapeada) no
//! roteador, qualquer um alcança o app nesse endereço, sem furo.
//!
//! - A porta da fofoca é aberta ao iniciar e renovada antes de vencer: o endereço aberto
//!   vai no ticket do encontro direto e serve de endereço público.
//! - Cada conexão WebRTC abre as portas dela e acrescenta o endereço aberto à própria
//!   oferta/resposta como mais um candidato (`augment_sdp`): o outro lado tenta também
//!   esse caminho, que passa mesmo com NAT simétrico.
//!
//! Só aceitamos aberturas com prazo (`LEASE`): roteador que só aceita porta aberta para
//! sempre fica sem (não deixamos portas abertas esquecidas). Nenhum servidor envolvido: é
//! uma conversa com o roteador da casa.

use igd_next::{Gateway, PortMappingProtocol, SearchOptions};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Prazo de cada abertura; a da fofoca é renovada na metade.
const LEASE: u32 = 60 * 60;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(3);
/// Uma nova busca pelo roteador depois de uma que não achou (a rede pode ter mudado).
const RESEARCH_AFTER: Duration = Duration::from_secs(10 * 60);

fn log(msg: impl AsRef<str>) {
    crate::connlog::log(format!("upnp: {}", msg.as_ref()));
}

struct St {
    gateway: Option<Gateway>,
    searched: Option<Instant>,
    /// Endereço da fofoca aberto no roteador.
    gossip: Option<SocketAddrV4>,
}

fn st() -> &'static Mutex<St> {
    static ST: OnceLock<Mutex<St>> = OnceLock::new();
    ST.get_or_init(|| Mutex::new(St { gateway: None, searched: None, gossip: None }))
}

/// Desligado nos testes e com `AJ_NO_UPNP` (não mexe no roteador de quem roda os testes).
fn enabled() -> bool {
    !cfg!(test) && std::env::var_os("AJ_NO_UPNP").is_none()
}

/// O roteador com UPnP (procura uma vez; de novo só depois de um tempo, se não achou).
fn gateway() -> Option<Gateway> {
    if !enabled() {
        return None;
    }
    {
        let s = st().lock().unwrap();
        if s.gateway.is_some() || s.searched.is_some_and(|t| t.elapsed() < RESEARCH_AFTER) {
            return s.gateway.clone();
        }
    }
    let t = Instant::now();
    let mut opts = SearchOptions::default();
    opts.timeout = Some(SEARCH_TIMEOUT);
    opts.single_search_timeout = Some(SEARCH_TIMEOUT);
    let found = igd_next::search_gateway(opts);
    let mut s = st().lock().unwrap();
    s.searched = Some(Instant::now());
    match found {
        Ok(g) => {
            log(format!("roteador com UPnP achado em {} ms ({})", t.elapsed().as_millis(), g.addr));
            s.gateway = Some(g);
        }
        Err(e) => log(format!("sem UPnP no roteador ({e})")),
    }
    s.gateway.clone()
}

/// IP desta máquina na rede do roteador.
fn lan_ipv4() -> Option<Ipv4Addr> {
    match crate::gossip::lan_ip()? {
        IpAddr::V4(ip) => Some(ip),
        _ => None,
    }
}

/// Abre no roteador a porta UDP local `port` (de preferência com o mesmo número por fora).
/// Devolve o endereço público aberto.
fn map(gw: &Gateway, local: SocketAddrV4, what: &str) -> Option<SocketAddrV4> {
    let ext_ip = match gw.get_external_ip() {
        Ok(IpAddr::V4(ip)) => ip,
        _ => return None,
    };
    // Roteador atrás de outro (ou CGNAT): o "público" que ele vê é privado e não adianta.
    if ext_ip.is_private() || ext_ip.is_loopback() || ext_ip.is_unspecified() || (ext_ip.octets()[0] == 100 && (ext_ip.octets()[1] & 0xC0) == 64) {
        log(format!("o roteador não tem IP público ({ext_ip}): abrir porta não adianta"));
        return None;
    }
    let desc = format!("AntiJanja {what}");
    let port = match gw.add_port(PortMappingProtocol::UDP, local.port(), SocketAddr::V4(local), LEASE, &desc) {
        Ok(()) => local.port(),
        Err(_) => match gw.add_any_port(PortMappingProtocol::UDP, SocketAddr::V4(local), LEASE, &desc) {
            Ok(p) => p,
            Err(e) => {
                log(format!("não abriu a porta {} ({what}): {e}", local.port()));
                return None;
            }
        },
    };
    Some(SocketAddrV4::new(ext_ip, port))
}

/// Abre a porta da fofoca e a mantém aberta (em segundo plano).
pub fn start_gossip(local_port: u16) {
    if !enabled() {
        return;
    }
    thread::Builder::new()
        .name("aj-upnp".into())
        .spawn(move || loop {
            let opened = gateway().zip(lan_ipv4()).and_then(|(gw, ip)| map(&gw, SocketAddrV4::new(ip, local_port), "fofoca"));
            let changed = {
                let mut s = st().lock().unwrap();
                let changed = s.gossip != opened;
                s.gossip = opened;
                changed
            };
            if changed {
                if let Some(a) = opened {
                    log(format!("porta da fofoca aberta: {a}"));
                }
            }
            // Renova na metade do prazo; sem roteador, tenta de novo bem mais tarde.
            thread::sleep(if opened.is_some() { Duration::from_secs(LEASE as u64 / 2) } else { RESEARCH_AFTER });
        })
        .ok();
}

/// Endereço público da fofoca aberto no roteador, se houver.
pub fn gossip_addr() -> Option<SocketAddrV4> {
    st().lock().unwrap().gossip
}

/// Acrescenta à oferta/resposta, para cada candidato local IPv4 desta máquina, o endereço
/// aberto no roteador para a mesma porta (como candidato "srflx"). Sem UPnP, devolve o
/// SDP como veio. Bloqueia (conversa com o roteador): chamar fora do runtime async.
pub fn augment_sdp(sdp: &str) -> String {
    // Só o roteador já achado (a busca é feita em segundo plano, por `start_gossip`): a
    // conexão não espera uma busca.
    let Some(gw) = st().lock().unwrap().gateway.clone().filter(|_| enabled()) else { return sdp.to_string() };
    let Some(lan) = lan_ipv4() else { return sdp.to_string() };
    let mut out = String::with_capacity(sdp.len() + 256);
    let mut opened = 0;
    for line in sdp.split_inclusive('\n') {
        out.push_str(line);
        if let Some(extra) = host_candidate(line.trim_end(), lan).and_then(|(c, port)| map(&gw, SocketAddrV4::new(lan, port), "conexão").map(|ext| (c, port, ext))).map(|(c, port, ext)| srflx_line(&c, lan, port, ext)) {
            out.push_str(&extra);
            out.push_str("\r\n");
            opened += 1;
        }
    }
    if opened > 0 {
        log(format!("{opened} porta(s) da conexão aberta(s) no roteador"));
    }
    out
}

/// Partes de um candidato: fundação, componente, prioridade.
struct Cand {
    foundation: String,
    component: String,
    priority: u32,
}

/// Candidato "host" UDP com o IP local `lan`: (partes, porta).
fn host_candidate(line: &str, lan: Ipv4Addr) -> Option<(Cand, u16)> {
    let rest = line.strip_prefix("a=candidate:")?;
    let f: Vec<&str> = rest.split(' ').collect();
    // fundação componente protocolo prioridade ip porta typ host ...
    if f.len() < 8 || !f[2].eq_ignore_ascii_case("udp") || f[6] != "typ" || f[7] != "host" || f[4].parse::<Ipv4Addr>().ok()? != lan {
        return None;
    }
    Some((Cand { foundation: f[0].to_string(), component: f[1].to_string(), priority: f[3].parse().ok()? }, f[5].parse().ok()?))
}

fn srflx_line(c: &Cand, lan: Ipv4Addr, port: u16, ext: SocketAddrV4) -> String {
    // Prioridade um pouco abaixo do candidato local (preferência de tipo "srflx").
    let priority = (100u32 << 24) | (c.priority & 0x00ff_ffff);
    format!("a=candidate:{}u {} udp {priority} {} {} typ srflx raddr {lan} rport {port}", c.foundation, c.component, ext.ip(), ext.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_srflx_from_a_host_candidate() {
        let lan: Ipv4Addr = "192.168.0.10".parse().unwrap();
        let line = "a=candidate:1234 1 udp 2130706431 192.168.0.10 50000 typ host";
        let (c, port) = host_candidate(line, lan).unwrap();
        assert_eq!(port, 50000);
        let s = srflx_line(&c, lan, port, "203.0.113.5:50000".parse().unwrap());
        assert_eq!(s, format!("a=candidate:1234u 1 udp {} 203.0.113.5 50000 typ srflx raddr 192.168.0.10 rport 50000", (100u32 << 24) | (2130706431 & 0x00ff_ffff)));
        // Outros IPs, IPv6, TCP e candidatos que não são "host" ficam de fora.
        assert!(host_candidate("a=candidate:1 1 udp 2130706431 192.168.0.11 50000 typ host", lan).is_none());
        assert!(host_candidate("a=candidate:1 1 udp 2130706431 2804::1 50000 typ host", lan).is_none());
        assert!(host_candidate("a=candidate:1 1 tcp 2130706431 192.168.0.10 50000 typ host", lan).is_none());
        assert!(host_candidate("a=candidate:1 1 udp 1694498815 192.168.0.10 50000 typ srflx raddr 0.0.0.0 rport 0", lan).is_none());
        // Nos testes não há UPnP: o SDP volta igual.
        assert_eq!(augment_sdp(line), line);
    }

    /// Só leitura: acha o roteador e pergunta o IP externo (não abre porta nenhuma).
    /// `cargo test router_discovery -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn router_discovery() {
        let mut opts = SearchOptions::default();
        opts.timeout = Some(SEARCH_TIMEOUT);
        match igd_next::search_gateway(opts) {
            Ok(gw) => println!("roteador {}; IP externo {:?}", gw.addr, gw.get_external_ip()),
            Err(e) => println!("sem UPnP: {e}"),
        }
    }
}
