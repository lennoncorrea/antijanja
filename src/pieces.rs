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
        let mut out = self.source();
        for b in 0..self.blocks.len() {
            let kb = self.header.block_pieces(b as u32);
            let extra = ((kb as f32 * overhead).ceil() as u32).max(1);
            out.extend(self.repair_block(b, extra));
        }
        out
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
        self.stats.pieces += 1;
        self.expire(now);
        let Some(h) = Header::read(piece) else {
            self.stats.invalid += 1;
            return None;
        };
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
            self.pending.insert(key, Pending { header: h, decoders, parts: vec![None; n], got: vec![0; n], first_at: now });
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
        Some(Frame { kind: h.kind, rendition: h.rendition, frame: h.frame, ts: h.ts, keyframe: h.keyframe(), data })
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
