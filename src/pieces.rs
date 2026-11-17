//! Pedaços codificados (RaptorQ, RFC 6330) para a malha P2P (ver docs/malha-p2p.md).
//!
//! Cada quadro vira `k` pedaços originais e quantos pedaços extras se quiser: quaisquer `k`
//! (raramente `k`+1 ou `k`+2) pedaços, de qualquer origem e em qualquer ordem, refazem o
//! quadro. Um pedaço cabe num pacote (sem fragmentar no SCTP) e leva no cabeçalho tudo o
//! que o montador precisa, então pedaços de quadros diferentes podem chegar misturados.
//!
//! Quadros grandes (keyframes) são divididos em blocos de até `BLOCK` pedaços originais,
//! cada um codificado à parte: o RaptorQ precisa de um "plano" para cada número de
//! pedaços, que custa ~65 ms na primeira vez para k=273 (um keyframe de 300 KB) e ~1 ms
//! para k≤64. Com blocos, existem no máximo 64 planos e nenhum keyframe espera.
//!
//! Os pedaços vão passar por outros espectadores: o montador desconfia de tudo (tamanhos,
//! campos incoerentes, dados malformados) e nunca trava nem derruba o app por isso.

use raptorq::{EncodingPacket, ObjectTransmissionInformation, PayloadId, SourceBlockDecoder, SourceBlockEncoder};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// Maior símbolo (dados de um pedaço): cabeçalho + símbolo + SCTP/DTLS/UDP/IP cabem em
/// ~1.200 bytes, abaixo do MTU comum.
pub const MAX_SYMBOL: usize = 1100;
/// Menor símbolo: quadros pequenos ainda viram alguns pedaços (dá para repartir entre
/// vizinhos), sem pedaços minúsculos.
const MIN_SYMBOL: usize = 64;
/// Quadros pequenos viram pelo menos tantos pedaços originais (se o símbolo mínimo deixar).
const MIN_PIECES: usize = 8;
/// Maior quadro aceito (um keyframe 4K com bitrate alto fica bem abaixo disso).
pub const MAX_FRAME: usize = 4 << 20;
/// Pedaços originais por bloco.
pub const BLOCK: usize = 64;
const VERSION: u8 = 1;
pub const HEADER: usize = 25;

/// Tipo do conteúdo do pedaço.
pub const KIND_VIDEO: u8 = 0;
pub const KIND_AUDIO: u8 = 1;
/// Assinatura de um quadro (Ed25519 do host): vai num "pedaço" próprio, um por faixa
/// (chega a todos pela malha), antes dos pedaços do quadro.
pub const KIND_SIG: u8 = 2;
const SIG_LEN: usize = 64;
/// Marcador: o quadro é um keyframe.
pub const FLAG_KEYFRAME: u8 = 1;

/// Cabeçalho de um pedaço (little-endian, 25 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: u8,
    /// Versão do vídeo: 0 = principal, senão a altura (720, 480).
    pub rendition: u16,
    /// Número do quadro (sequencial em cada tipo e versão).
    pub frame: u32,
    /// Timestamp RTP (90 kHz para vídeo).
    pub ts: u32,
    /// Tamanho do quadro em bytes.
    pub len: u32,
    /// Tamanho do símbolo (o mesmo em todos os pedaços do quadro).
    pub symbol: u16,
    pub flags: u8,
    /// Faixa da malha por onde o pedaço anda.
    pub lane: u8,
    /// Id do pedaço no RaptorQ dentro do bloco (os primeiros são os originais). Até 2^24.
    pub esi: u32,
    /// Bloco do quadro a que o pedaço pertence.
    pub block: u8,
}

impl Header {
    fn write(&self, out: &mut Vec<u8>) {
        out.push(VERSION);
        out.push(self.kind);
        out.extend_from_slice(&self.rendition.to_le_bytes());
        out.extend_from_slice(&self.frame.to_le_bytes());
        out.extend_from_slice(&self.ts.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.symbol.to_le_bytes());
        out.push(self.flags);
        out.push(self.lane);
        out.extend_from_slice(&self.esi.to_le_bytes());
        out.push(self.block);
    }

    /// Lê e valida o cabeçalho. None = pedaço inválido.
    pub fn read(b: &[u8]) -> Option<Header> {
        if b.len() < HEADER || b[0] != VERSION {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let h = Header {
            kind: b[1],
            rendition: u16_at(2),
            frame: u32_at(4),
            ts: u32_at(8),
            len: u32_at(12),
            symbol: u16_at(16),
            flags: b[18],
            lane: b[19],
            esi: u32_at(20),
            block: b[24],
        };
        if h.kind == KIND_SIG {
            let ok = h.len == SIG_LEN as u32 && h.symbol == SIG_LEN as u16 && h.esi == 0 && h.block == 0 && b.len() == HEADER + SIG_LEN;
            return ok.then_some(h);
        }
        let ok = h.kind <= KIND_AUDIO
            && (1..=MAX_FRAME as u32).contains(&h.len)
            && (1..=MAX_SYMBOL as u16).contains(&h.symbol)
            && h.esi < (1 << 24)
            && b.len() == HEADER + h.symbol as usize
            && (h.block as u32) < h.blocks();
        ok.then_some(h)
    }

    pub fn keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    /// Quantos pedaços originais o quadro tem.
    pub fn source_pieces(&self) -> u32 {
        (self.len as usize).div_ceil(self.symbol as usize) as u32
    }

    pub fn blocks(&self) -> u32 {
        self.source_pieces().div_ceil(BLOCK as u32)
    }

    /// Pedaços originais do bloco `b`.
    pub fn block_pieces(&self, b: u32) -> u32 {
        (self.source_pieces() - b * BLOCK as u32).min(BLOCK as u32)
    }
}

/// Tamanho do símbolo para um quadro de `len` bytes.
fn symbol_for(len: usize) -> usize {
    len.div_ceil(MIN_PIECES).clamp(MIN_SYMBOL, MAX_SYMBOL)
}

fn oti(len: usize, symbol: usize) -> ObjectTransmissionInformation {
    ObjectTransmissionInformation::new(len as u64, symbol as u16, 1, 1, 1)
}

/// Gera os pedaços de um quadro.
pub struct FrameEncoder {
    blocks: Vec<SourceBlockEncoder>,
    header: Header,
    k: u32,
    lanes: u8,
    /// Próximo pedaço extra de cada bloco (os extras não têm fim: dá para pedir mais depois).
    next_repair: Vec<u32>,
}

impl FrameEncoder {
    /// `lanes`: em quantas faixas os pedaços são repartidos.
    pub fn new(kind: u8, rendition: u16, frame: u32, ts: u32, data: &[u8], keyframe: bool, lanes: u8) -> Option<FrameEncoder> {
        if data.is_empty() || data.len() > MAX_FRAME {
            return None;
        }
        let symbol = symbol_for(data.len());
        let header = Header {
            kind,
            rendition,
            frame,
            ts,
            len: data.len() as u32,
            symbol: symbol as u16,
            flags: if keyframe { FLAG_KEYFRAME } else { 0 },
            lane: 0,
            esi: 0,
            block: 0,
        };
        let blocks = (0..header.blocks())
            .map(|b| {
                let start = b as usize * BLOCK * symbol;
                let end = (start + BLOCK * symbol).min(data.len());
                // Último símbolo completado com zeros (o montador corta pelo tamanho do quadro).
                let mut chunk = data[start..end].to_vec();
                chunk.resize(header.block_pieces(b) as usize * symbol, 0);
                SourceBlockEncoder::new(b as u8, &oti(chunk.len(), symbol), &chunk)
            })
            .collect::<Vec<_>>();
        let k = header.source_pieces();
        let n = blocks.len();
        Some(FrameEncoder { blocks, header, k, lanes: lanes.max(1), next_repair: vec![0; n] })
    }

    pub fn header(&self) -> Header {
        self.header
    }

    /// Pedaços originais (todos os blocos).
    pub fn k(&self) -> u32 {
        self.k
    }

    fn serialize(&self, block: u8, p: &EncodingPacket) -> Vec<u8> {
        let esi = p.payload_id().encoding_symbol_id();
        // Espalha os blocos entre as faixas (o pedaço 0 de cada bloco numa faixa diferente).
        let lane = ((esi + block as u32) % self.lanes as u32) as u8;
        let h = Header { esi, lane, block, ..self.header };
        let mut out = Vec::with_capacity(HEADER + p.data().len());
        h.write(&mut out);
        out.extend_from_slice(p.data());
        out
    }

    /// Os pedaços originais.
    pub fn source(&self) -> Vec<Vec<u8>> {
        let mut out = vec![];
        for (b, enc) in self.blocks.iter().enumerate() {
            out.extend(enc.source_packets().iter().map(|p| self.serialize(b as u8, p)));
        }
        out
    }

    /// Os próximos `n` pedaços extras do bloco `b` (cada chamada continua de onde parou).
    pub fn repair_block(&mut self, b: usize, n: u32) -> Vec<Vec<u8>> {
        let Some(enc) = self.blocks.get(b) else { return vec![] };
        let out = enc.repair_packets(self.next_repair[b], n).iter().map(|p| self.serialize(b as u8, p)).collect();
        self.next_repair[b] += n;
        out
    }

    /// Os próximos `n` pedaços extras de cada bloco.
    pub fn repair(&mut self, n: u32) -> Vec<Vec<u8>> {
        (0..self.blocks.len()).flat_map(|b| self.repair_block(b, n)).collect()
    }

    /// Originais + extras com `overhead` de folga em cada bloco (0,4 = 40% a mais, pelo
    /// menos 1 extra por bloco).
    pub fn pieces(&mut self, overhead: f32) -> Vec<Vec<u8>> {
        self.pieces_with(overhead, false)
    }

    /// Como `pieces`; com `survive_lane`, a folga também garante que perder uma faixa
    /// inteira (um vizinho da malha que caiu) ainda deixa pedaços para fechar cada bloco,
    /// com 1 de sobra. Com 4 faixas e quadros pequenos, isso pede ~+40% em vez de +25%.
    pub fn pieces_with(&mut self, overhead: f32, survive_lane: bool) -> Vec<Vec<u8>> {
        let mut out = self.source();
        let lanes = self.lanes as u32;
        for b in 0..self.blocks.len() {
            let kb = self.header.block_pieces(b as u32);
            let mut extra = ((kb as f32 * overhead).ceil() as u32).max(1);
            if survive_lane && lanes > 1 {
                // Os pedaços se repartem igualmente entre as faixas: perder a maior parte
                // (ceil(n/faixas)) ainda deixa kb+1.
                while (kb + extra) - (kb + extra).div_ceil(lanes) < kb + 1 {
                    extra += 1;
                }
            }
            out.extend(self.repair_block(b, extra));
        }
        out
    }
}

/// Faixas da malha: os pedaços de cada quadro são repartidos entre elas.
pub const LANES: u8 = 4;

/// Faixa de um pedaço já serializado (sem validar o resto).
pub fn lane_of(piece: &[u8]) -> Option<u8> {
    piece.get(19).copied()
}

/// Um quadro já em pedaços, o mesmo para todos os espectadores (na malha, um pedaço
/// repassado por alguém tem que servir no montador de qualquer outro).
pub struct PieceFrame {
    pub pieces: Vec<Vec<u8>>,
    pub keyframe: bool,
}

/// Numera e codifica os quadros de cada versão do vídeo, uma vez só para todos.
pub struct PieceSource {
    /// Por versão: (próximo quadro, timestamp de 90 kHz).
    state: std::collections::HashMap<u32, (u32, u32)>,
    seed: u32,
    pub overhead: f32,
    /// Malha: folga suficiente para perder uma faixa inteira.
    pub survive_lane: bool,
    /// Malha: assina cada quadro (os pedaços passam por outros espectadores).
    pub signer: Option<ed25519_dalek::SigningKey>,
    /// Codificadores dos quadros recentes: geram pedaços extras quando alguém pede.
    recent: VecDeque<(u32, u32, FrameEncoder)>,
}

/// Quadros recentes que ainda dá para reparar (~1,5 s a 60 fps).
const KEEP_FOR_REPAIR: usize = 90;
/// Mais pedaços extras que isso por bloco num pedido: pedido estranho.
const MAX_REPAIR: u32 = 64;

impl PieceSource {
    pub fn new(overhead: f32) -> Self {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        Self { state: Default::default(), seed: u32::from_le_bytes(b), overhead, survive_lane: false, signer: None, recent: VecDeque::new() }
    }

    /// `key`: versão (0 = principal, senão a altura). None só se o quadro for inválido;
    /// mesmo assim o número do quadro avança (quem recebe vê o buraco e pede keyframe).
    pub fn encode(&mut self, key: u32, data: &[u8], duration: Duration) -> Option<std::sync::Arc<PieceFrame>> {
        let samples = (duration.as_secs_f64() * 90000.0).round() as u32;
        let seed = self.seed;
        let st = self.state.entry(key).or_insert((0, seed));
        let (frame, ts) = *st;
        *st = (frame.wrapping_add(1), ts.wrapping_add(samples));
        let keyframe = crate::media::has_nal(data, 5);
        let mut enc = FrameEncoder::new(KIND_VIDEO, key.min(u16::MAX as u32) as u16, frame, ts, data, keyframe, LANES)?;
        let mut pieces = enc.pieces_with(overhead_for(keyframe, self.overhead), self.survive_lane);
        if let Some(sk) = &self.signer {
            // A assinatura vai na frente: quando o quadro fecha, ela já chegou.
            let mut signed = sig_pieces(sk, &enc.header(), data);
            signed.append(&mut pieces);
            pieces = signed;
        }
        self.recent.push_back((key, frame, enc));
        while self.recent.len() > KEEP_FOR_REPAIR {
            self.recent.pop_front();
        }
        Some(std::sync::Arc::new(PieceFrame { pieces, keyframe }))
    }

    /// Pedaços extras novos para um quadro recente: `blocks` = (bloco, quantos). Vazio se
    /// o quadro já saiu da memória.
    pub fn more(&mut self, key: u32, frame: u32, blocks: &[(u8, u32)]) -> Vec<Vec<u8>> {
        let Some((_, _, enc)) = self.recent.iter_mut().find(|(k, f, _)| *k == key && *f == frame) else { return vec![] };
        blocks.iter().flat_map(|&(b, n)| enc.repair_block(b as usize, n.min(MAX_REPAIR))).collect()
    }
}

/// O que a assinatura cobre: tudo o que descreve o quadro e os dados.
fn digest(kind: u8, rendition: u16, frame: u32, ts: u32, data: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"antijanja quadro v1");
    h.update(&[kind]);
    h.update(&rendition.to_le_bytes());
    h.update(&frame.to_le_bytes());
    h.update(&ts.to_le_bytes());
    h.update(&(data.len() as u32).to_le_bytes());
    h.update(data);
    *h.finalize().as_bytes()
}

/// Um "pedaço" de assinatura por faixa.
fn sig_pieces(sk: &ed25519_dalek::SigningKey, h: &Header, data: &[u8]) -> Vec<Vec<u8>> {
    use ed25519_dalek::Signer;
    let sig = sk.sign(&digest(h.kind, h.rendition, h.frame, h.ts, data)).to_bytes();
    (0..LANES)
        .map(|lane| {
            let sh = Header { kind: KIND_SIG, len: SIG_LEN as u32, symbol: SIG_LEN as u16, esi: 0, block: 0, lane, ..*h };
            let mut out = Vec::with_capacity(HEADER + SIG_LEN);
            sh.write(&mut out);
            out.extend_from_slice(&sig);
            out
        })
        .collect()
}

/// Quanto um quadro refeito espera pela assinatura dele antes de ser descartado.
pub const SIG_WAIT: Duration = Duration::from_millis(100);

/// Confere a assinatura de cada quadro refeito (malha: os pedaços passaram por outros
/// espectadores, que poderiam alterá-los). Quadro sem assinatura válida não vai para o
/// decoder.
pub struct Verifier {
    key: ed25519_dalek::VerifyingKey,
    sigs: std::collections::HashMap<(u16, u32), [u8; SIG_LEN]>,
    order: VecDeque<(u16, u32)>,
    waiting: Vec<(Frame, Instant)>,
    /// Quadros descartados: assinatura errada / não chegou a tempo.
    pub forged: u64,
    pub unsigned: u64,
    /// De quem vieram os pedaços dos quadros que não conferiram (quem chama esvazia).
    pub suspects: Vec<u64>,
}

impl Verifier {
    pub fn new(key: ed25519_dalek::VerifyingKey) -> Self {
        Self { key, sigs: Default::default(), order: VecDeque::new(), waiting: vec![], forged: 0, unsigned: 0, suspects: vec![] }
    }

    /// Pedaço de assinatura. Devolve quadros que esperavam por ela e agora conferem.
    pub fn on_sig(&mut self, piece: &[u8], now: Instant) -> Vec<Frame> {
        let Some(h) = Header::read(piece).filter(|h| h.kind == KIND_SIG) else { return vec![] };
        let key = (h.rendition, h.frame);
        if !self.sigs.contains_key(&key) {
            let mut sig = [0u8; SIG_LEN];
            sig.copy_from_slice(&piece[HEADER..]);
            self.sigs.insert(key, sig);
            self.order.push_back(key);
            while self.order.len() > REMEMBER_DONE {
                if let Some(old) = self.order.pop_front() {
                    self.sigs.remove(&old);
                }
            }
        }
        let waiting = std::mem::take(&mut self.waiting);
        let mut out = vec![];
        for (f, at) in waiting {
            match self.check_one(&f) {
                Some(true) => out.push(f),
                Some(false) => self.reject(&f),
                None => self.waiting.push((f, at)),
            }
        }
        out.extend(self.expire(now));
        out
    }

    /// Quadro refeito. Devolve os que já conferem (este e/ou outros).
    pub fn push(&mut self, f: Frame, now: Instant) -> Vec<Frame> {
        let mut out = vec![];
        match self.check_one(&f) {
            Some(true) => out.push(f),
            Some(false) => self.reject(&f),
            None => self.waiting.push((f, now)),
        }
        out.extend(self.expire(now));
        out
    }

    fn reject(&mut self, f: &Frame) {
        self.forged += 1;
        for s in &f.sources {
            if !self.suspects.contains(s) {
                self.suspects.push(*s);
            }
        }
    }

    /// Chamar de tempos em tempos (descarta quem esperou demais pela assinatura).
    pub fn tick(&mut self, now: Instant) -> Vec<Frame> {
        self.expire(now)
    }

    fn expire(&mut self, now: Instant) -> Vec<Frame> {
        let before = self.waiting.len();
        self.waiting.retain(|(_, at)| now.saturating_duration_since(*at) < SIG_WAIT);
        self.unsigned += (before - self.waiting.len()) as u64;
        vec![]
    }

    /// Some(true) = confere, Some(false) = não confere, None = assinatura ainda não chegou.
    fn check_one(&self, f: &Frame) -> Option<bool> {
        let sig = self.sigs.get(&(f.rendition, f.frame))?;
        let sig = ed25519_dalek::Signature::from_bytes(sig);
        Some(self.key.verify_strict(&digest(f.kind, f.rendition, f.frame, f.ts, &f.data), &sig).is_ok())
    }
}

/// Folga de pedaços extras por tipo de quadro: keyframes são grandes e, se um se perde, a
/// imagem fica estragada até o próximo.
pub fn overhead_for(keyframe: bool, base: f32) -> f32 {
    if keyframe { (base * 2.0).max(0.5) } else { base }
}

/// Quadro refeito pelo montador.
#[derive(Debug, PartialEq)]
pub struct Frame {
    pub kind: u8,
    pub rendition: u16,
    pub frame: u32,
    pub ts: u32,
    pub keyframe: bool,
    pub data: Vec<u8>,
    /// De quem vieram os pedaços usados (0 = host): se o quadro não conferir com a
    /// assinatura, um deles adulterou.
    pub sources: Vec<u64>,
}

/// Contadores do montador (para relatórios e diagnóstico).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    pub pieces: u64,
    /// Pedaços de quadros já refeitos (chegaram tarde ou sobraram) ou repetidos.
    pub surplus: u64,
    pub invalid: u64,
    pub frames: u64,
    /// Quadros que nunca fecharam (faltaram pedaços até o prazo).
    pub incomplete: u64,
    /// Pedaços a mais que os originais usados para fechar quadros.
    pub extra_used: u64,
}

struct Pending {
    header: Header,
    /// Um montador por bloco; None depois que o bloco fechou.
    decoders: Vec<Option<SourceBlockDecoder>>,
    /// Dados de cada bloco já fechado.
    parts: Vec<Option<Vec<u8>>>,
    /// Pedaços recebidos em cada bloco.
    got: Vec<u32>,
    first_at: Instant,
    sources: Vec<u64>,
}

/// Quanto tempo um quadro incompleto espera por pedaços antes de desistir.
pub const FRAME_DEADLINE: Duration = Duration::from_millis(500);
/// Quadros incompletos guardados ao mesmo tempo (limita a memória com pedaços estranhos).
const MAX_PENDING: usize = 64;
/// Quadros refeitos lembrados (para ignorar os pedaços que ainda chegarem deles).
const REMEMBER_DONE: usize = 512;

type Key = (u8, u16, u32);

/// Junta pedaços de vários quadros (misturados, fora de ordem) e devolve cada quadro assim
/// que dá para refazê-lo.
pub struct Assembler {
    pending: BTreeMap<Key, Pending>,
    done: VecDeque<Key>,
    pub stats: Stats,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    pub fn new() -> Self {
        Self { pending: BTreeMap::new(), done: VecDeque::new(), stats: Stats::default() }
    }

    pub fn push(&mut self, piece: &[u8], now: Instant) -> Option<Frame> {
        self.push_from(piece, now, 0)
    }

    /// Como `push`, anotando de quem veio o pedaço (0 = host).
    pub fn push_from(&mut self, piece: &[u8], now: Instant, src: u64) -> Option<Frame> {
        self.stats.pieces += 1;
        self.expire(now);
        let Some(h) = Header::read(piece) else {
            self.stats.invalid += 1;
            return None;
        };
        if h.kind == KIND_SIG {
            return None; // tratadas pelo `Verifier`
        }
        let key = (h.kind, h.rendition, h.frame);
        if self.done.contains(&key) {
            self.stats.surplus += 1;
            return None;
        }
        if !self.pending.contains_key(&key) {
            if self.pending.len() >= MAX_PENDING {
                // Cheio: desiste do quadro esperando há mais tempo.
                if let Some(oldest) = self.pending.iter().min_by_key(|(_, p)| p.first_at).map(|(k, _)| *k) {
                    self.pending.remove(&oldest);
                    self.stats.incomplete += 1;
                }
            }
            let n = h.blocks() as usize;
            let decoders = (0..n as u32)
                .map(|b| {
                    let bytes = h.block_pieces(b) as usize * h.symbol as usize;
                    Some(SourceBlockDecoder::new(b as u8, &oti(bytes, h.symbol as usize), bytes as u64))
                })
                .collect();
            self.pending.insert(key, Pending { header: h, decoders, parts: vec![None; n], got: vec![0; n], first_at: now, sources: vec![] });
        }
        let p = self.pending.get_mut(&key).unwrap();
        // Todos os pedaços de um quadro têm que concordar no que descreve o quadro.
        let same = Header { esi: p.header.esi, lane: p.header.lane, block: p.header.block, ..h } == p.header;
        if !same {
            self.stats.invalid += 1;
            return None;
        }
        let b = h.block as usize;
        let Some(decoder) = p.decoders[b].as_mut() else {
            self.stats.surplus += 1; // bloco já fechado
            return None;
        };
        p.got[b] += 1;
        if !p.sources.contains(&src) {
            p.sources.push(src);
        }
        let packet = EncodingPacket::new(PayloadId::new(h.block, h.esi), piece[HEADER..].to_vec());
        // Defesa extra: um pedaço malformado não pode derrubar o app.
        let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decoder.decode(std::iter::once(packet))));
        match decoded {
            Ok(Some(d)) => {
                p.decoders[b] = None;
                p.parts[b] = Some(d);
            }
            Ok(None) => return None,
            Err(_) => {
                self.pending.remove(&key);
                self.stats.invalid += 1;
                return None;
            }
        }
        if p.parts.iter().any(|x| x.is_none()) {
            return None;
        }
        let p = self.pending.remove(&key).unwrap();
        self.remember(key);
        self.stats.frames += 1;
        let used: u32 = p.got.iter().sum();
        self.stats.extra_used += used.saturating_sub(p.header.source_pieces()) as u64;
        let mut data: Vec<u8> = p.parts.into_iter().flatten().flatten().collect();
        data.truncate(p.header.len as usize);
        Some(Frame { kind: h.kind, rendition: h.rendition, frame: h.frame, ts: h.ts, keyframe: h.keyframe(), data, sources: p.sources })
    }

    fn remember(&mut self, key: Key) {
        self.done.push_back(key);
        while self.done.len() > REMEMBER_DONE {
            self.done.pop_front();
        }
    }

    /// Desiste dos quadros que passaram do prazo.
    fn expire(&mut self, now: Instant) {
        let before = self.pending.len();
        self.pending.retain(|_, p| now.saturating_duration_since(p.first_at) < FRAME_DEADLINE);
        self.stats.incomplete += (before - self.pending.len()) as u64;
    }

    /// Blocos ainda abertos de um quadro pendente e quantos pedaços faltam (pelo menos) em
    /// cada um.
    pub fn missing_blocks(&self, kind: u8, rendition: u16, frame: u32) -> Option<Vec<(u8, u32)>> {
        let p = self.pending.get(&(kind, rendition, frame))?;
        Some(
            (0..p.parts.len())
                .filter(|&b| p.parts[b].is_none())
                .map(|b| (b as u8, p.header.block_pieces(b as u32).saturating_sub(p.got[b]).max(1)))
                .collect(),
        )
    }

    /// Quantos pedaços ainda faltam (pelo menos) para fechar o quadro, se ele está pendente.
    pub fn missing(&self, kind: u8, rendition: u16, frame: u32) -> Option<u32> {
        self.pending.get(&(kind, rendition, frame)).map(|p| {
            (0..p.parts.len())
                .filter(|&b| p.parts[b].is_none())
                .map(|b| p.header.block_pieces(b as u32).saturating_sub(p.got[b]))
                .sum()
        })
    }
}

/// Quanto um quadro que falta pode demorar (os seguintes esperando) antes de ser dado como
/// perdido. Pedaços chegam fora de ordem: o quadro 11 pode fechar antes do 10.
pub const ORDER_WAIT: Duration = Duration::from_millis(60);

/// Entrega os quadros de vídeo na ordem para o decoder. Um quadro perdido estraga os
/// seguintes (eles dependem dele): aí descarta até o próximo keyframe e avisa que precisa
/// de um (as mesmas regras do caminho RTP).
pub struct Ordered {
    next: Option<u32>,
    ready: BTreeMap<u32, Frame>,
    /// Faltou um quadro: descarta até o próximo keyframe. Começa assim (espera o primeiro).
    broken: bool,
    waiting_since: Option<Instant>,
    /// Quadros dados como perdidos.
    pub lost: u64,
}

impl Default for Ordered {
    fn default() -> Self {
        Self::new()
    }
}

impl Ordered {
    pub fn new() -> Self {
        Self { next: None, ready: BTreeMap::new(), broken: true, waiting_since: None, lost: 0 }
    }

    pub fn needs_keyframe(&self) -> bool {
        self.broken
    }

    /// O quadro que está faltando enquanto os seguintes esperam (vale pedir reparo dele).
    pub fn waiting_for(&self) -> Option<u32> {
        self.waiting_since.and(self.next)
    }

    /// Quadro refeito pelo montador. Devolve os que já podem ir para o decoder, na ordem.
    pub fn push(&mut self, f: Frame, now: Instant) -> Vec<Frame> {
        let next = *self.next.get_or_insert(f.frame);
        if f.frame >= next {
            self.ready.insert(f.frame, f);
        }
        self.drain(now)
    }

    /// Chamar de tempos em tempos: desiste de quadros que não chegaram a tempo.
    pub fn tick(&mut self, now: Instant) -> Vec<Frame> {
        self.drain(now)
    }

    fn drain(&mut self, now: Instant) -> Vec<Frame> {
        let mut out = vec![];
        loop {
            let Some(next) = self.next else { break };
            if let Some(f) = self.ready.remove(&next) {
                self.next = Some(next.wrapping_add(1));
                self.waiting_since = None;
                if f.keyframe {
                    self.broken = false;
                }
                if !self.broken {
                    out.push(f);
                }
                continue;
            }
            let Some((&first, _)) = self.ready.iter().next() else { break };
            // Falta `next` e há quadros depois dele. Um keyframe adiante resolve na hora
            // (os que faltam antes dele não fazem mais diferença); senão espera um pouco.
            let keyframe_ahead = self.ready.iter().find(|(_, f)| f.keyframe).map(|(&n, _)| n);
            let since = *self.waiting_since.get_or_insert(now);
            let to = match keyframe_ahead {
                Some(k) => k,
                None if now.saturating_duration_since(since) >= ORDER_WAIT => first,
                None => break,
            };
            self.lost += to.wrapping_sub(next) as u64;
            // Quadros completos antes do keyframe não servem mais (dependem dos perdidos).
            self.ready.retain(|&n, _| n >= to);
            if keyframe_ahead.is_none() {
                self.broken = true;
            }
            self.next = Some(to);
            self.waiting_since = None;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2654435761).max(1);
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    /// Embaralha com um gerador simples (determinístico).
    fn shuffle<T>(v: &mut [T], seed: u32) {
        let mut x = seed.max(1);
        for i in (1..v.len()).rev() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            v.swap(i, x as usize % (i + 1));
        }
    }

    fn vf(n: u32, keyframe: bool) -> Frame {
        Frame { kind: KIND_VIDEO, rendition: 0, frame: n, ts: n * 1500, keyframe, data: vec![n as u8], sources: vec![] }
    }

    fn nums(v: &[Frame]) -> Vec<u32> {
        v.iter().map(|f| f.frame).collect()
    }

    #[test]
    fn ordered_waits_for_the_first_keyframe_then_delivers_in_order() {
        let mut o = Ordered::new();
        let t = Instant::now();
        assert!(o.push(vf(0, false), t).is_empty(), "sem keyframe ainda: descarta");
        assert!(o.needs_keyframe());
        assert_eq!(nums(&o.push(vf(1, true), t)), vec![1]);
        assert!(!o.needs_keyframe());
        // 3 fecha antes do 2: segura o 3 até o 2 chegar.
        assert!(o.push(vf(3, false), t).is_empty());
        assert_eq!(nums(&o.push(vf(2, false), t)), vec![2, 3]);
    }

    #[test]
    fn ordered_gives_up_on_a_missing_frame_and_waits_for_a_keyframe() {
        let mut o = Ordered::new();
        let t = Instant::now();
        o.push(vf(0, true), t);
        assert!(o.push(vf(2, false), t).is_empty(), "o 1 ainda pode chegar");
        assert!(o.tick(t + ORDER_WAIT / 2).is_empty());
        // Passou o prazo: o 1 se perdeu; o 2 e os seguintes dependem dele.
        assert!(o.tick(t + ORDER_WAIT).is_empty());
        assert!(o.needs_keyframe());
        assert_eq!(o.lost, 1);
        assert!(o.push(vf(3, false), t + ORDER_WAIT).is_empty());
        assert_eq!(nums(&o.push(vf(4, true), t + ORDER_WAIT)), vec![4]);
        assert!(!o.needs_keyframe());
    }

    #[test]
    fn ordered_jumps_to_a_keyframe_without_waiting() {
        let mut o = Ordered::new();
        let t = Instant::now();
        o.push(vf(0, true), t);
        // 1 e 2 sumiram, o keyframe 3 chegou: não precisa esperar pelos que faltam.
        assert_eq!(nums(&o.push(vf(3, true), t)), vec![3]);
        assert_eq!(o.lost, 2);
        assert!(!o.needs_keyframe());
        // Um atrasado de antes do keyframe é ignorado.
        assert!(o.push(vf(1, false), t).is_empty());
        assert_eq!(nums(&o.push(vf(4, false), t)), vec![4]);
    }

    #[test]
    fn all_source_pieces_rebuild_the_frame() {
        for len in [1usize, 63, 64, 500, 1100, 1101, 40_000, 300_000] {
            let d = data(len, len as u32);
            let enc = FrameEncoder::new(KIND_VIDEO, 0, 7, 1234, &d, false, 1).unwrap();
            let mut a = Assembler::new();
            let now = Instant::now();
            let mut out = None;
            for p in enc.source() {
                assert!(out.is_none(), "fechou antes de todos os originais ({len} bytes)");
                out = a.push(&p, now);
            }
            let f = out.unwrap_or_else(|| panic!("não fechou ({len} bytes)"));
            assert_eq!((f.frame, f.ts, f.data.len()), (7, 1234, len));
            assert!(f.data == d, "dados diferentes ({len} bytes)");
        }
    }

    #[test]
    fn any_k_plus_two_pieces_rebuild_and_k_almost_always() {
        let d = data(40_000, 9);
        let (mut with_k, mut trials) = (0, 0);
        for seed in 1..=200u32 {
            let mut enc = FrameEncoder::new(KIND_VIDEO, 0, seed, 0, &d, false, 4).unwrap();
            let k = enc.k() as usize;
            let mut pieces = enc.pieces(0.4);
            shuffle(&mut pieces, seed);
            // Fica com k+2 pedaços quaisquer (perdeu o resto, de qualquer vizinho).
            let mut a = Assembler::new();
            let now = Instant::now();
            let f = pieces[..k + 2].iter().find_map(|p| a.push(p, now));
            assert!(f.is_some_and(|f| f.data == d), "k+2 pedaços não fecharam (semente {seed})");
            // Só k: o RaptorQ fecha em ~99% das vezes.
            let mut a = Assembler::new();
            trials += 1;
            if pieces[..k].iter().find_map(|p| a.push(p, now)).is_some() {
                with_k += 1;
            }
        }
        assert!(with_k * 100 >= trials * 95, "com exatamente k pedaços fechou só {with_k} de {trials}");
    }

    #[test]
    fn frames_mixed_and_out_of_order() {
        let frames: Vec<Vec<u8>> = (0..10).map(|i| data(3_000 + i * 900, i as u32 + 1)).collect();
        let mut all = vec![];
        for (i, d) in frames.iter().enumerate() {
            let mut enc = FrameEncoder::new(KIND_VIDEO, 0, i as u32, i as u32 * 3000, d, i == 0, 2).unwrap();
            all.extend(enc.pieces(0.5));
        }
        shuffle(&mut all, 77);
        let mut a = Assembler::new();
        let now = Instant::now();
        let mut got: Vec<Frame> = all.iter().filter_map(|p| a.push(p, now)).collect();
        got.sort_by_key(|f| f.frame);
        assert_eq!(got.len(), 10);
        for (i, f) in got.iter().enumerate() {
            assert!(f.data == frames[i]);
            assert_eq!(f.keyframe, i == 0);
        }
        assert!(a.stats.surplus > 0, "os pedaços que sobram depois de fechar são ignorados");
        assert_eq!(a.stats.incomplete, 0);
    }

    #[test]
    fn asking_for_more_pieces_later_closes_the_frame() {
        let d = data(20_000, 3);
        let mut enc = FrameEncoder::new(KIND_VIDEO, 0, 1, 0, &d, false, 1).unwrap();
        let k = enc.k() as usize;
        let src = enc.source();
        let mut a = Assembler::new();
        let now = Instant::now();
        // Chegaram só k-3 originais.
        for p in &src[..k - 3] {
            assert!(a.push(p, now).is_none());
        }
        assert_eq!(a.missing(KIND_VIDEO, 0, 1), Some(3));
        // Pede "mais 4" (sem dizer quais faltaram): pedaços novos fecham o quadro.
        let more = enc.repair(4);
        let f = more.iter().find_map(|p| a.push(p, now));
        assert!(f.is_some_and(|f| f.data == d));
    }

    #[test]
    fn stale_frames_are_dropped_and_counted() {
        let d = data(10_000, 5);
        let enc = FrameEncoder::new(KIND_VIDEO, 0, 1, 0, &d, false, 1).unwrap();
        let src = enc.source();
        let mut a = Assembler::new();
        let t0 = Instant::now();
        a.push(&src[0], t0);
        // O resto chega depois do prazo: o quadro já foi descartado; recomeça do zero e
        // não fecha só com os que faltavam.
        let late = t0 + FRAME_DEADLINE + Duration::from_millis(1);
        let f = src[1..].iter().find_map(|p| a.push(p, late));
        assert!(f.is_none());
        assert_eq!(a.stats.incomplete, 1);
    }

    /// Keyframe grande (vários blocos) com 15% dos pedaços perdidos, espalhados.
    #[test]
    fn big_keyframe_in_blocks_survives_scattered_loss() {
        let d = data(300_000, 21);
        let mut enc = FrameEncoder::new(KIND_VIDEO, 0, 1, 0, &d, true, 4).unwrap();
        assert!(enc.header().blocks() > 1);
        let mut pieces = enc.pieces(overhead_for(true, 0.3));
        shuffle(&mut pieces, 5);
        let keep = pieces.len() * 85 / 100;
        let mut a = Assembler::new();
        let now = Instant::now();
        let f = pieces[..keep].iter().find_map(|p| a.push(p, now));
        assert!(f.is_some_and(|f| f.keyframe && f.data == d));
    }

    fn signing_key(seed: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
    }

    /// Quadros assinados passam; adulterados, de outra chave ou sem assinatura, não.
    #[test]
    fn verifier_accepts_only_frames_signed_by_the_host() {
        let sk = signing_key(7);
        let mut src = PieceSource::new(0.25);
        src.signer = Some(sk.clone());
        let mut v = Verifier::new(sk.verifying_key());
        let mut a = Assembler::new();
        let now = Instant::now();
        let run = |pieces: &[Vec<u8>], a: &mut Assembler, v: &mut Verifier, now: Instant| -> Vec<Frame> {
            let mut out = vec![];
            for p in pieces {
                if Header::read(p).is_some_and(|h| h.kind == KIND_SIG) {
                    out.extend(v.on_sig(p, now));
                } else if let Some(f) = a.push(p, now) {
                    out.extend(v.push(f, now));
                }
            }
            out
        };
        // Normal: confere.
        let d = data(9_000, 1);
        let f = src.encode(0, &d, Duration::from_millis(16)).unwrap();
        let ok = run(&f.pieces, &mut a, &mut v, now);
        assert_eq!(ok.len(), 1);
        assert!(ok[0].data == d);
        // Alguém no caminho trocou um byte de todos os pedaços originais: refaz um quadro
        // diferente, que não confere.
        let d2 = data(9_000, 2);
        let f2 = src.encode(0, &d2, Duration::from_millis(16)).unwrap();
        let tampered: Vec<Vec<u8>> = f2
            .pieces
            .iter()
            .map(|p| {
                let mut p = p.clone();
                if Header::read(&p).is_some_and(|h| h.kind == KIND_VIDEO) {
                    p[HEADER] ^= 0xff;
                }
                p
            })
            .collect();
        assert!(run(&tampered, &mut a, &mut v, now).is_empty());
        assert_eq!(v.forged, 1);
        assert_eq!(v.suspects, vec![0], "os pedaços vieram do host (fonte 0) neste teste");
        // Assinado por outra chave (alguém se passando pelo host): não confere.
        let mut evil = PieceSource::new(0.25);
        evil.signer = Some(signing_key(9));
        let mut a2 = Assembler::new();
        let fe = evil.encode(0, &data(5_000, 3), Duration::from_millis(16)).unwrap();
        assert!(run(&fe.pieces, &mut a2, &mut v, now).is_empty());
        // Sem assinatura: espera um pouco e descarta.
        let mut plain = PieceSource::new(0.25);
        let mut a3 = Assembler::new();
        // (outra versão do vídeo: nenhuma assinatura com esse número chegou)
        let fp = plain.encode(480, &data(5_000, 4), Duration::from_millis(16)).unwrap();
        assert!(run(&fp.pieces, &mut a3, &mut v, now).is_empty());
        v.tick(now + SIG_WAIT);
        assert_eq!(v.unsigned, 1);
    }

    /// Quem recebeu pedaços de menos pede mais ao host e fecha o quadro.
    #[test]
    fn asking_the_source_for_more_closes_a_frame() {
        let mut src = PieceSource::new(0.25);
        let d = data(30_000, 8);
        let f = src.encode(0, &d, Duration::from_millis(16)).unwrap();
        let mut a = Assembler::new();
        let now = Instant::now();
        // Chegou só metade.
        for p in &f.pieces[..f.pieces.len() / 2] {
            assert!(a.push(p, now).is_none());
        }
        let h = Header::read(&f.pieces[0]).unwrap();
        let blocks = a.missing_blocks(KIND_VIDEO, 0, h.frame).unwrap();
        let ask: Vec<(u8, u32)> = blocks.iter().map(|&(b, n)| (b, n + 1)).collect();
        let more = src.more(0, h.frame, &ask);
        assert!(!more.is_empty());
        let got = more.iter().find_map(|p| a.push(p, now));
        assert!(got.is_some_and(|g| g.data == d));
        // Quadro que já saiu da memória: nada.
        assert!(src.more(0, h.frame + 1000, &ask).is_empty());
    }

    /// A assinatura pode chegar depois do quadro (por outro vizinho): ele espera um pouco.
    #[test]
    fn verifier_waits_briefly_for_a_late_signature() {
        let sk = signing_key(5);
        let mut src = PieceSource::new(0.25);
        src.signer = Some(sk.clone());
        let mut v = Verifier::new(sk.verifying_key());
        let mut a = Assembler::new();
        let now = Instant::now();
        let f = src.encode(0, &data(4_000, 1), Duration::from_millis(16)).unwrap();
        let (sigs, rest): (Vec<_>, Vec<_>) = f.pieces.iter().partition(|p| Header::read(p).is_some_and(|h| h.kind == KIND_SIG));
        let frame = rest.iter().find_map(|p| a.push(p, now)).unwrap();
        assert!(v.push(frame, now).is_empty(), "sem assinatura ainda: espera");
        let out = v.on_sig(sigs[2], now + Duration::from_millis(30));
        assert_eq!(out.len(), 1);
    }

    /// Na malha, perder uma faixa inteira (qualquer uma) não pode impedir nenhum quadro.
    #[test]
    fn losing_any_whole_lane_still_rebuilds_every_frame() {
        for len in [500usize, 3_000, 9_000, 40_000, 300_000] {
            let d = data(len, len as u32);
            for lost in 0..LANES {
                let mut enc = FrameEncoder::new(KIND_VIDEO, 0, 1, 0, &d, len > 100_000, LANES).unwrap();
                let pieces = enc.pieces_with(0.25, true);
                let mut a = Assembler::new();
                let now = Instant::now();
                let f = pieces.iter().filter(|p| lane_of(p) != Some(lost)).find_map(|p| a.push(p, now));
                assert!(f.is_some_and(|f| f.data == d), "{len} bytes sem a faixa {lost}");
            }
        }
    }

    #[test]
    fn keyframes_get_more_slack() {
        assert!(overhead_for(true, 0.4) > overhead_for(false, 0.4));
        assert!(overhead_for(true, 0.1) >= 0.5);
    }

    #[test]
    fn garbage_never_panics_or_completes() {
        let mut a = Assembler::new();
        let now = Instant::now();
        let d = data(5_000, 11);
        let enc = FrameEncoder::new(KIND_VIDEO, 0, 1, 0, &d, false, 1).unwrap();
        let good = enc.source();
        let mut evil: Vec<Vec<u8>> = vec![vec![], vec![0; 5], vec![VERSION; HEADER], vec![2; 200]];
        // Cabeçalho válido, tamanho do quadro diferente do primeiro pedaço do mesmo quadro.
        let mut liar = good[1].clone();
        liar[12..16].copy_from_slice(&999u32.to_le_bytes());
        evil.push(liar);
        // Símbolo com tamanho errado.
        let mut short = good[2].clone();
        short.truncate(short.len() - 1);
        evil.push(short);
        // ESI enorme.
        let mut big = good[3].clone();
        big[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        evil.push(big);
        for x in 0..2000u32 {
            evil.push(data(HEADER + (x as usize % 300), x));
        }
        a.push(&good[0], now);
        for e in &evil {
            assert!(a.push(e, now).is_none());
        }
        // Os pedaços bons ainda fecham o quadro.
        let f = good[1..].iter().find_map(|p| a.push(p, now));
        assert!(f.is_some_and(|f| f.data == d));
        assert!(a.stats.invalid >= 7);
    }

    /// Custo de codificar e montar (o host codifica cada quadro de cada versão).
    #[test]
    #[ignore]
    fn coding_cost() {
        for len in [40_000usize, 300_000, 301_000, 302_000] {
            let d = data(len, 1);
            for round in 0..2 {
                let t = Instant::now();
                let mut enc = FrameEncoder::new(KIND_VIDEO, 0, 1, 0, &d, true, 4).unwrap();
                let pieces = enc.pieces(0.4);
                let e = t.elapsed().as_secs_f64() * 1000.0;
                let k = enc.k() as usize;
                let mut a = Assembler::new();
                let t = Instant::now();
                let now = Instant::now();
                // Um terço dos originais perdido: fecha com os extras.
                let got = pieces.iter().enumerate().filter(|(i, _)| i % 3 != 0 || *i >= k).find_map(|(_, p)| a.push(p, now));
                let m = t.elapsed().as_secs_f64() * 1000.0;
                println!("{len} bytes, k={k}, rodada {round}: codificar {e:.2} ms, montar {m:.2} ms, fechou {}", got.is_some());
            }
        }
    }
}
