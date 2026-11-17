//! Sincronia entre áudio e vídeo de quem assiste (malha e pedaços; docs/malha-p2p.md).
//!
//! O host carimba cada quadro e cada pacote de áudio com o instante da captura, no mesmo
//! relógio (90 kHz desde o começo da transmissão). Aqui, os dois tocam numa agenda única:
//! o que foi capturado no instante X toca em `X + base + atraso`, para os dois. Assim a
//! sincronia não depende do caminho de cada um (a malha pode atrasar o vídeo, mudar a
//! rota, perder um vizinho): muda só o atraso, igual para os dois.
//!
//! - `base`: o menor trânsito visto (chegada − captura) nos dois, a referência de "sem
//!   fila" (inclui a diferença entre os relógios do host e daqui).
//! - `atraso`: o que o mais lento dos dois precisa (o p95 do trânsito dele acima da base
//!   mais uma margem de processamento). Desliza devagar, nunca pula: o vídeo absorve sem
//!   se notar e o áudio acompanha corrigindo a velocidade de leve (ver `AudioTimeline`).
//!   Antes de começar a tocar, mede a rede por um tempo (`WARMUP`) e começa com folga,
//!   para quase nunca precisar aumentar depois.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Taxa do relógio de mídia (a do vídeo H.264 no RTP).
pub const MEDIA_HZ: f64 = 90_000.0;
/// Taxa do áudio.
pub const AUDIO_HZ: f64 = 48_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Video = 0,
    Audio = 1,
}

/// Margem de processamento depois de chegar: vídeo (decodificar e desenhar) e áudio
/// (decodificar um pacote de 20 ms e passar pelo dispositivo).
const MARGIN_MS: [f64; 2] = [8.0, 30.0];
/// Folga extra no começo (a rede costuma variar mais do que o primeiro meio segundo mostra).
const START_HEADROOM_MS: f64 = 30.0;
/// Medição antes de começar a tocar.
pub const WARMUP: Duration = Duration::from_millis(500);
/// Janela das medições (a base, o menor trânsito, olha 10 s; a necessidade de cada tipo,
/// os últimos 5 s: um engarrafamento que passou deixa de pesar logo).
const WINDOW_MS: f64 = 10_000.0;
const NEED_WINDOW_MS: f64 = 5_000.0;
/// Um tipo que não chega há esse tempo para de contar (ex.: transmissão sem áudio).
const STALE_MS: f64 = 2_000.0;
/// O atraso desliza no máximo isso por segundo (1% = 10 ms/s) para cima; para baixo, bem
/// mais devagar, e só depois de sobrar por um tempo.
const SLEW_UP: f64 = 0.010;
/// Para baixo: 0,5% (5 ms/s; para o áudio, uma mudança de velocidade que não se ouve),
/// depois de sobrar por 5 s.
const SLEW_DOWN: f64 = 0.005;
const DOWN_AFTER_MS: f64 = 5_000.0;
/// Limites do atraso.
const MIN_DELAY_MS: f64 = 20.0;
const MAX_DELAY_MS: f64 = 600.0;

struct Inner {
    start: Instant,
    /// Desenrola o timestamp de 32 bits: (último visto, valor estendido dele).
    ref_ts: Option<(u32, i64)>,
    /// (horário local ms, tipo, trânsito ms)
    window: VecDeque<(f64, Stream, f64)>,
    base: f64,
    delay: Option<f64>,
    /// Último ajuste do atraso (ms locais) e desde quando ele sobra.
    last_update: f64,
    surplus_since: Option<f64>,
    first_seen: Option<f64>,
    /// Áudio tocando: (instante em que uma amostra sai no alto-falante, o instante de mídia
    /// dela em ms, quando foi informado). É o relógio mestre do vídeo.
    audio_ref: Option<(Instant, f64, Instant)>,
}

/// O relógio do áudio vale para o vídeo enquanto o tocador informar a posição com essa
/// frequência (pede a cada ~10 ms).
const AUDIO_REF_FRESH: Duration = Duration::from_millis(200);

/// O relógio comum de áudio e vídeo de uma stream (compartilhado entre as threads).
pub struct SyncClock {
    inner: Mutex<Inner>,
}

impl Default for SyncClock {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl Inner {
    fn ms(&self, t: Instant) -> f64 {
        t.saturating_duration_since(self.start).as_secs_f64() * 1000.0
    }

    /// Timestamp de 90 kHz estendido (sem a virada dos 32 bits), em ms de mídia.
    fn media_ms(&mut self, ts: u32) -> f64 {
        let ext = match self.ref_ts {
            None => ts as i64,
            Some((r, e)) => e + ts.wrapping_sub(r) as i32 as i64,
        };
        // A referência acompanha o mais novo (o i32 cobre ±6 h em volta dela).
        if self.ref_ts.is_none_or(|(_, e)| ext > e) {
            self.ref_ts = Some((ts, ext));
        }
        ext as f64 / MEDIA_HZ * 1000.0
    }

    fn media_ms_peek(&self, ts: u32) -> f64 {
        let ext = match self.ref_ts {
            None => ts as i64,
            Some((r, e)) => e + ts.wrapping_sub(r) as i32 as i64,
        };
        ext as f64 / MEDIA_HZ * 1000.0
    }

    /// O atraso que o mais lento precisa agora.
    fn target(&self, now_ms: f64) -> f64 {
        let mut need: f64 = MIN_DELAY_MS;
        for s in [Stream::Video, Stream::Audio] {
            let mut v: Vec<f64> =
                self.window.iter().filter(|x| x.1 == s && now_ms - x.0 <= NEED_WINDOW_MS).map(|x| x.2 - self.base).collect();
            let fresh = self.window.iter().rev().find(|x| x.1 == s).is_some_and(|x| now_ms - x.0 < STALE_MS);
            if v.is_empty() || !fresh {
                continue;
            }
            v.sort_by(|a, b| a.total_cmp(b));
            let p95 = v[((v.len() as f64 * 0.95) as usize).min(v.len() - 1)];
            need = need.max(p95 + MARGIN_MS[s as usize]);
        }
        need.min(MAX_DELAY_MS)
    }

    fn update(&mut self, now_ms: f64) {
        let target = self.target(now_ms);
        let dt = (now_ms - self.last_update).max(0.0);
        self.last_update = now_ms;
        let Some(d) = self.delay else {
            // Começa depois do aquecimento, com folga.
            if self.first_seen.is_some_and(|f| now_ms - f >= WARMUP.as_secs_f64() * 1000.0) {
                self.delay = Some((target + START_HEADROOM_MS).min(MAX_DELAY_MS));
            }
            return;
        };
        let next = if target > d {
            self.surplus_since = None;
            (d + dt * SLEW_UP).min(target)
        } else {
            let since = *self.surplus_since.get_or_insert(now_ms);
            if now_ms - since >= DOWN_AFTER_MS {
                (d - dt * SLEW_DOWN).max(target)
            } else {
                d
            }
        };
        self.delay = Some(next.clamp(MIN_DELAY_MS, MAX_DELAY_MS));
    }
}

impl SyncClock {
    pub fn new(now: Instant) -> Self {
        Self {
            inner: Mutex::new(Inner {
                start: now,
                ref_ts: None,
                window: VecDeque::new(),
                base: f64::INFINITY,
                delay: None,
                last_update: 0.0,
                surplus_since: None,
                first_seen: None,
                audio_ref: None,
            }),
        }
    }

    /// Chegou um quadro ou pacote com este timestamp (relógio de mídia de 90 kHz).
    pub fn observe(&self, s: Stream, ts: u32, arrival: Instant) {
        let mut g = self.inner.lock().unwrap();
        let now_ms = g.ms(arrival);
        let media = g.media_ms(ts);
        let transit = now_ms - media;
        g.first_seen.get_or_insert(now_ms);
        // Pulo enorme (o host recomeçou o relógio): recomeça as medições.
        if g.window.back().is_some_and(|x| (transit - x.2).abs() > 5_000.0) {
            g.window.clear();
            g.base = f64::INFINITY;
        }
        g.window.push_back((now_ms, s, transit));
        while g.window.front().is_some_and(|x| now_ms - x.0 > WINDOW_MS) {
            g.window.pop_front();
        }
        g.base = g.window.iter().map(|x| x.2).fold(f64::INFINITY, f64::min);
        g.update(now_ms);
    }

    /// Já dá para tocar (passou o aquecimento)?
    pub fn ready(&self) -> bool {
        self.inner.lock().unwrap().delay.is_some()
    }

    /// Diagnóstico: base e, por tipo, os percentis 50/90/95/máx do trânsito acima dela.
    pub fn debug(&self) -> String {
        let g = self.inner.lock().unwrap();
        let mut parts = vec![format!("atraso {:.0} ms", g.delay.unwrap_or(0.0))];
        for (name, s) in [("vídeo", Stream::Video), ("áudio", Stream::Audio)] {
            let mut v: Vec<f64> = g.window.iter().filter(|x| x.1 == s).map(|x| x.2 - g.base).collect();
            if v.is_empty() {
                continue;
            }
            v.sort_by(|a, b| a.total_cmp(b));
            let q = |p: f64| v[((v.len() as f64 * p) as usize).min(v.len() - 1)];
            parts.push(format!("{name} p50 {:.0} p90 {:.0} p95 {:.0} máx {:.0}", q(0.5), q(0.9), q(0.95), v[v.len() - 1]));
        }
        parts.join(", ")
    }

    /// Atraso atual (ms), para o diagnóstico.
    pub fn delay_ms(&self) -> f64 {
        self.inner.lock().unwrap().delay.unwrap_or(0.0)
    }

    /// Quando tocar o que tem este timestamp. None durante o aquecimento.
    ///
    /// Com áudio tocando, o áudio é o relógio mestre: o horário sai da posição que o
    /// tocador informou (o que está saindo no alto-falante agora). Assim a sincronia é
    /// exata mesmo enquanto o áudio corrige a velocidade aos poucos (deriva da placa de
    /// som, atraso deslizando): o vídeo acompanha o que de fato se ouve.
    pub fn play_at(&self, ts: u32) -> Option<Instant> {
        let g = self.inner.lock().unwrap();
        let d = g.delay?;
        let media = g.media_ms_peek(ts);
        if let Some((heard, at_ms, updated)) = g.audio_ref {
            if updated.elapsed() < AUDIO_REF_FRESH {
                let ahead = media - at_ms;
                return Some(if ahead >= 0.0 {
                    heard + Duration::from_secs_f64(ahead / 1000.0)
                } else {
                    heard.checked_sub(Duration::from_secs_f64(-ahead / 1000.0)).unwrap_or(heard)
                });
            }
        }
        let ms = media + g.base + d;
        Some(g.start + Duration::from_secs_f64(ms.max(0.0) / 1000.0))
    }

    /// O tocador de áudio informa: a amostra com o instante de mídia `media_ms` sai no
    /// alto-falante em `heard_at`.
    pub fn set_audio_position(&self, heard_at: Instant, media_ms: f64) {
        self.inner.lock().unwrap().audio_ref = Some((heard_at, media_ms, Instant::now()));
    }

    /// O instante de mídia (ms) que deve estar tocando no horário local `t` (o inverso de
    /// `play_at`; o áudio usa para saber que amostra mandar ao dispositivo).
    pub fn media_ms_at(&self, t: Instant) -> Option<f64> {
        let mut g = self.inner.lock().unwrap();
        let now_ms = g.ms(t);
        // Sem chegadas, o atraso também desliza (o áudio pede a cada ~10 ms).
        g.update(now_ms);
        let d = g.delay?;
        Some(now_ms - g.base - d)
    }

    /// Converte um timestamp de 90 kHz em ms de mídia (mesma régua de `media_ms_at`).
    pub fn media_ms(&self, ts: u32) -> f64 {
        self.inner.lock().unwrap().media_ms_peek(ts)
    }
}

/* =================================================================== áudio */

/// Erro (amostras) acima do qual o áudio pula direto para o horário certo, em vez de
/// corrigir a velocidade aos poucos (só acontece no começo ou depois de um buraco grande).
const SNAP_SAMPLES: f64 = AUDIO_HZ * 0.060;
/// Correção máxima de velocidade (2%): segue o atraso deslizando a 1%/s com folga.
const MAX_SPEED_ADJ: f64 = 0.02;
/// Ganho da correção: 20 ms de erro corrigem em ~2 s (1% de velocidade).
const SPEED_GAIN: f64 = 0.01 / (AUDIO_HZ * 0.020);
/// Áudio guardado à frente no máximo (mais que isso é lixo ou o relógio pulou).
const MAX_AHEAD_SAMPLES: i64 = (AUDIO_HZ * 3.0) as i64;

/// O áudio decodificado numa linha do tempo de mídia (amostras de 48 kHz desde o começo).
/// O dispositivo pede blocos; cada bloco sai do ponto da linha do tempo que deve estar
/// tocando naquele instante. Pequenas diferenças (deriva entre a placa de som e o relógio
/// do host, o atraso deslizando) são corrigidas variando a velocidade de leve, com
/// interpolação (sem cortes nem estalos).
pub struct AudioTimeline {
    /// Índice (amostra de mídia) da primeira posição de `buf`.
    start: i64,
    /// Quadros estéreo; None = não chegou (sai silêncio).
    buf: VecDeque<Option<[f32; 2]>>,
    /// Posição de leitura (amostra de mídia, fracionária).
    pos: Option<f64>,
    /// Diagnóstico: amostras que saíram em silêncio por falta de dado, pulos.
    pub missing: u64,
    pub snaps: u64,
}

impl Default for AudioTimeline {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioTimeline {
    pub fn new() -> Self {
        Self { start: 0, buf: VecDeque::new(), pos: None, missing: 0, snaps: 0 }
    }

    /// Áudio decodificado (estéreo intercalado) que começa na amostra de mídia `at`.
    pub fn push(&mut self, at: i64, pcm: &[f32]) {
        let frames = pcm.len() / 2;
        if frames == 0 {
            return;
        }
        if self.buf.is_empty() {
            self.start = at;
        }
        // Muito à frente ou muito atrás do que temos: recomeça a linha do tempo.
        let end = self.start + self.buf.len() as i64;
        if at > end + MAX_AHEAD_SAMPLES || at + (frames as i64) < self.start - MAX_AHEAD_SAMPLES {
            self.buf.clear();
            self.start = at;
        }
        // Antes do começo (já tocou): só a parte que ainda não passou.
        let skip = (self.start - at).max(0) as usize;
        if skip >= frames {
            return;
        }
        let first = at + skip as i64;
        let need = first + (frames - skip) as i64 - self.start;
        while (self.buf.len() as i64) < need {
            self.buf.push_back(None);
        }
        for i in skip..frames {
            let idx = (at + i as i64 - self.start) as usize;
            self.buf[idx] = Some([pcm[i * 2], pcm[i * 2 + 1]]);
        }
    }

    /// Até onde há áudio contínuo à frente da leitura (amostras de mídia), para saber se
    /// vale decodificar mais.
    pub fn buffered_until(&self) -> i64 {
        self.start + self.buf.len() as i64
    }

    /// `frames` quadros estéreo para o dispositivo. `target`: a amostra de mídia que deve
    /// estar tocando no primeiro deles (None = ainda não sabe: silêncio).
    pub fn pull(&mut self, frames: usize, target: Option<f64>) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames * 2);
        let Some(target) = target else {
            out.resize(frames * 2, 0.0);
            return out;
        };
        let pos = *self.pos.get_or_insert(target);
        let err = target - pos;
        let mut pos = if err.abs() > SNAP_SAMPLES {
            if self.missing > 0 || self.buf.iter().any(|x| x.is_some()) {
                self.snaps += 1;
            }
            target
        } else {
            pos
        };
        let speed = if err.abs() > SNAP_SAMPLES { 1.0 } else { 1.0 + (err * SPEED_GAIN).clamp(-MAX_SPEED_ADJ, MAX_SPEED_ADJ) };
        for _ in 0..frames {
            let i = pos.floor() as i64;
            let frac = (pos - i as f64) as f32;
            let a = self.sample(i);
            let b = self.sample(i + 1);
            match (a, b) {
                (Some(a), Some(b)) => {
                    out.push(a[0] + (b[0] - a[0]) * frac);
                    out.push(a[1] + (b[1] - a[1]) * frac);
                }
                (Some(a), None) => {
                    out.push(a[0]);
                    out.push(a[1]);
                }
                _ => {
                    self.missing += 1;
                    out.push(0.0);
                    out.push(0.0);
                }
            }
            pos += speed;
        }
        self.pos = Some(pos);
        // Descarta o que já tocou.
        let keep_from = pos.floor() as i64 - 1;
        while self.start < keep_from && !self.buf.is_empty() {
            self.buf.pop_front();
            self.start += 1;
        }
        if self.buf.is_empty() {
            self.start = self.start.max(keep_from);
        }
        out
    }

    fn sample(&self, i: i64) -> Option<[f32; 2]> {
        if i < self.start {
            return None;
        }
        self.buf.get((i - self.start) as usize).copied().flatten()
    }

    /// Posição de leitura atual (amostra de mídia), para testes e diagnóstico.
    pub fn position(&self) -> Option<f64> {
        self.pos
    }
}

/// Um bloco para o dispositivo de áudio: decodifica o que chegou, tira da linha do tempo o
/// trecho que deve sair em `heard_at` (quando a primeira amostra do bloco sai no
/// alto-falante) e informa a posição ao relógio (o áudio é o mestre do vídeo).
pub fn render_block(clock: &SyncClock, state: &mut AudioState, now: Instant, heard_at: Instant, frames: usize) -> Vec<f32> {
    let AudioState { rx, tl } = state;
    rx.run(clock, now, tl);
    let target = clock.media_ms_at(heard_at).map(|ms| ms * AUDIO_HZ / 1000.0);
    let pcm = tl.pull(frames, target);
    // Posição depois do bloco = a amostra que sai `frames` depois de `heard_at`.
    if let (Some(_), Some(pos)) = (target, tl.position()) {
        let end = heard_at + Duration::from_secs_f64(frames as f64 / AUDIO_HZ);
        clock.set_audio_position(end, pos * 1000.0 / AUDIO_HZ);
    }
    pcm
}

/// Amostra de mídia (48 kHz) de um timestamp em ms de mídia.
pub fn audio_sample_at(media_ms: f64) -> i64 {
    (media_ms * AUDIO_HZ / 1000.0).round() as i64
}

/* ============================================================ pacotes de áudio */

/// O áudio de uma stream em pedaços: o receptor (ordem, FEC, PLC) e a linha do tempo.
pub struct AudioState {
    pub rx: AudioReceiver,
    pub tl: AudioTimeline,
}

impl AudioState {
    pub fn new() -> Result<Self, String> {
        Ok(Self { rx: AudioReceiver::new()?, tl: AudioTimeline::new() })
    }
}

/// Duração de um pacote de áudio (20 ms) em ticks de 90 kHz e em amostras.
pub const PACKET_TICKS: u32 = 1_800;
pub const PACKET_SAMPLES: usize = 960;
/// Pacote que falta: espera até esse tempo antes da hora de tocar e então disfarça a
/// falta (correção de erro do próximo pacote, ou o preenchimento do Opus).
const CONCEAL_LEAD: Duration = Duration::from_millis(40);
/// Disfarces seguidos sem nenhum pacote depois (3 = 60 ms; o Opus vai suavizando até o
/// silêncio, sem estalo).
const MAX_BLIND: u32 = 3;

/// Recebe os pacotes Opus (fora de ordem, repetidos: chegam por duas faixas da malha),
/// decodifica na ordem e põe na linha do tempo, cada um no horário da captura. Pacote
/// perdido não vira silêncio: se o seguinte chegou, a correção de erro dele (Opus FEC)
/// refaz o que faltou; senão o Opus preenche a falta de forma suave (PLC).
pub struct AudioReceiver {
    dec: opus::Decoder,
    /// Pacotes à espera, por número: (timestamp 90 kHz, dados).
    packets: std::collections::BTreeMap<u32, (u32, Vec<u8>)>,
    next: Option<u32>,
    /// Timestamp do último pacote decodificado ou disfarçado (o próximo deve ser +20 ms).
    last_ts: Option<u32>,
    /// Disfarces seguidos sem nenhum pacote depois (a falta pode ser só atraso, ou o host
    /// em pausa: quando nada toca lá, ele não manda pacotes).
    blind: u32,
    /// Números já tocados (a segunda cópia, pela outra faixa, conta como repetida e não
    /// como atrasada).
    played: VecDeque<u32>,
    pcm: Vec<f32>,
    /// Diagnóstico.
    pub decoded: u64,
    pub fec: u64,
    pub plc: u64,
    pub duplicate: u64,
    pub late: u64,
}

impl AudioReceiver {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            dec: opus::Decoder::new(48_000, opus::Channels::Stereo).map_err(|e| e.to_string())?,
            packets: Default::default(),
            next: None,
            last_ts: None,
            blind: 0,
            played: VecDeque::new(),
            pcm: vec![0.0; 5760 * 2],
            decoded: 0,
            fec: 0,
            plc: 0,
            duplicate: 0,
            late: 0,
        })
    }

    /// Pacote que chegou (`seq` = número do pacote, `ts` = captura em 90 kHz).
    pub fn push(&mut self, seq: u32, ts: u32, data: Vec<u8>) {
        if self.next.is_some_and(|n| seq < n) {
            if self.played.contains(&seq) {
                self.duplicate += 1; // a cópia da outra faixa
            } else {
                self.late += 1; // chegou depois da hora (foi disfarçado)
            }
            return;
        }
        // O pacote esperado chegou depois de disfarces "às cegas": se o horário dele já
        // foi coberto, chegou tarde; se é depois (o host estava em pausa), vale normal.
        if self.blind > 0 && self.next == Some(seq) {
            let covered = self.last_ts.is_some_and(|t| (ts.wrapping_sub(t) as i32) <= 0);
            self.blind = 0;
            if covered {
                self.late += 1;
                self.next = Some(seq + 1);
                return;
            }
        }
        if self.packets.contains_key(&seq) {
            self.duplicate += 1;
            return;
        }
        // Proteção contra lixo: no máximo 3 s de pacotes à espera.
        if self.packets.len() >= 150 {
            if let Some((&first, _)) = self.packets.iter().next() {
                self.packets.remove(&first);
            }
        }
        self.packets.insert(seq, (ts, data));
    }

    /// Decodifica o que dá e põe na linha do tempo; disfarça pacotes que faltam quando a
    /// hora deles está chegando. Chamar a cada pacote e de tempos em tempos.
    pub fn run(&mut self, clock: &SyncClock, now: Instant, tl: &mut AudioTimeline) {
        loop {
            let Some(n) = self.next.or_else(|| self.packets.keys().next().copied()) else { return };
            if self.next.is_none() {
                self.next = Some(n);
            }
            if let Some((ts, data)) = self.packets.remove(&n) {
                let out = self.decode(&data, false);
                self.emit(ts, out, clock, tl);
                self.decoded += 1;
                self.blind = 0;
                self.played.push_back(n);
                if self.played.len() > 128 {
                    self.played.pop_front();
                }
                self.next = Some(n + 1);
                continue;
            }
            // Falta o `n` e nada chegou depois dele: na hora de tocar, preenche (até 60 ms)
            // em vez de deixar sair silêncio, sem dar o `n` por perdido (pode ser só atraso,
            // ou o host em pausa).
            let Some((&after, _)) = self.packets.iter().next() else {
                let Some(last) = self.last_ts else { return };
                let expected = last.wrapping_add(PACKET_TICKS);
                let due = clock.play_at(expected).is_some_and(|at| now + CONCEAL_LEAD / 2 >= at);
                if due && self.blind < MAX_BLIND {
                    self.blind += 1;
                    self.plc += 1;
                    let out = self.decode(&[], false);
                    self.emit(expected, out, clock, tl);
                }
                return;
            };
            let expected = self.last_ts.map(|t| t.wrapping_add(PACKET_TICKS)).unwrap_or_else(|| {
                // Sem referência: estima pelo seguinte.
                self.packets[&after].0.wrapping_sub(PACKET_TICKS * (after - n))
            });
            let due = clock.play_at(expected).is_some_and(|at| now + CONCEAL_LEAD >= at);
            if !due {
                return;
            }
            let out = match self.packets.get(&(n + 1)) {
                Some((_, next_data)) => {
                    self.fec += 1;
                    let next_data = next_data.clone();
                    self.decode(&next_data, true)
                }
                None => {
                    self.plc += 1;
                    self.decode(&[], false)
                }
            };
            self.emit(expected, out, clock, tl);
            self.next = Some(n + 1);
        }
    }

    fn decode(&mut self, data: &[u8], fec: bool) -> Vec<f32> {
        // FEC e PLC precisam saber o tamanho do pacote que falta: 20 ms.
        let max = if fec || data.is_empty() { PACKET_SAMPLES * 2 } else { self.pcm.len() };
        match self.dec.decode_float(data, &mut self.pcm[..max], fec) {
            Ok(n) => self.pcm[..n * 2].to_vec(),
            Err(_) => vec![0.0; PACKET_SAMPLES * 2],
        }
    }

    fn emit(&mut self, ts: u32, pcm: Vec<f32>, clock: &SyncClock, tl: &mut AudioTimeline) {
        self.last_ts = Some(ts);
        tl.push(audio_sample_at(clock.media_ms(ts)), &pcm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts_at(ms: f64) -> u32 {
        ((ms * MEDIA_HZ / 1000.0).round() as i64) as u32
    }

    /// Áudio direto (rápido, pouca variação) e vídeo pela malha (40 ms mais lento, com
    /// variação): os dois tocam na mesma agenda, com o atraso do mais lento.
    /// Um tom de 440 Hz codificado em Opus, pacote a pacote (20 ms).
    fn opus_packets(n: usize) -> Vec<Vec<u8>> {
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        enc.set_inband_fec(true).unwrap();
        enc.set_packet_loss_perc(10).unwrap();
        let mut out = vec![0u8; 4000];
        (0..n)
            .map(|k| {
                let pcm: Vec<i16> = (0..PACKET_SAMPLES)
                    .flat_map(|i| {
                        let t = (k * PACKET_SAMPLES + i) as f32 / 48_000.0;
                        let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 12_000.0) as i16;
                        [v, v]
                    })
                    .collect();
                let len = enc.encode(&pcm, &mut out).unwrap();
                out[..len].to_vec()
            })
            .collect()
    }

    /// Recebe pacotes (perdendo alguns), toca pela linha do tempo e devolve o áudio de
    /// saída mais o receptor.
    fn play(lost: &[usize], dup_and_shuffle: bool) -> (Vec<f32>, AudioReceiver, AudioTimeline) {
        let t0 = Instant::now();
        let clock = SyncClock::new(t0);
        let mut rx = AudioReceiver::new().unwrap();
        let mut tl = AudioTimeline::new();
        let packets = opus_packets(150);
        let mut out = vec![];
        let block = 480usize;
        let mut sent = 0usize;
        for step in 0..300usize {
            let now_ms = step as f64 * 10.0;
            let now = t0 + Duration::from_secs_f64(now_ms / 1000.0);
            // Pacote k capturado em k*20 ms, chega 15 ms depois (a cada 20 ms).
            while sent < packets.len() && sent as f64 * 20.0 + 15.0 <= now_ms {
                let k = sent;
                sent += 1;
                if lost.contains(&k) {
                    continue;
                }
                let ts = (k as u32) * PACKET_TICKS;
                clock.observe(Stream::Audio, ts, now);
                if dup_and_shuffle && k % 3 == 1 && k + 1 < packets.len() && !lost.contains(&(k + 1)) {
                    // Este chega depois do seguinte, e repetido.
                    continue;
                }
                rx.push(k as u32, ts, packets[k].clone());
                if dup_and_shuffle && k % 3 == 2 {
                    rx.push(k as u32 - 1, (k as u32 - 1) * PACKET_TICKS, packets[k - 1].clone());
                    rx.push(k as u32 - 1, (k as u32 - 1) * PACKET_TICKS, packets[k - 1].clone());
                }
            }
            rx.run(&clock, now, &mut tl);
            let target = clock.media_ms_at(now).map(|ms| ms * AUDIO_HZ / 1000.0);
            if target.is_some() {
                out.extend(tl.pull(block, target));
            }
        }
        (out, rx, tl)
    }

    /// Trechos de silêncio (corte audível) no meio do áudio tocado.
    fn silent_runs(out: &[f32]) -> usize {
        // Ignora o começo e o fim (antes do primeiro e depois do último pacote).
        let body = &out[48_000..out.len().saturating_sub(48_000)];
        let mut runs = 0;
        let mut zeros = 0;
        for f in body.chunks_exact(2) {
            if f[0].abs() < 1e-4 {
                zeros += 1;
                if zeros == 96 {
                    runs += 1; // 2 ms seguidos sem som
                }
            } else {
                zeros = 0;
            }
        }
        runs
    }

    /// O host para de mandar (nada tocando lá) e volta: o receptor preenche um pouco e
    /// depois aceita normalmente o pacote que chega depois da pausa (não é "atrasado").
    #[test]
    fn a_pause_on_the_host_does_not_drop_the_audio_that_comes_back() {
        let t0 = Instant::now();
        let clock = SyncClock::new(t0);
        let mut rx = AudioReceiver::new().unwrap();
        let mut tl = AudioTimeline::new();
        let packets = opus_packets(80);
        let mut out = vec![];
        for step in 0..400usize {
            let now_ms = step as f64 * 10.0;
            let now = t0 + Duration::from_secs_f64(now_ms / 1000.0);
            // Pacotes 0..39 em seguida; pausa de 1 s; 40..79 com os horários depois da pausa.
            for k in 0..80usize {
                let cap = if k < 40 { k as f64 * 20.0 } else { k as f64 * 20.0 + 1000.0 };
                let arrival = cap + 15.0;
                if arrival > now_ms - 10.0 && arrival <= now_ms {
                    let ts = (cap * 90.0) as u32;
                    clock.observe(Stream::Audio, ts, now);
                    rx.push(k as u32, ts, packets[k].clone());
                }
            }
            rx.run(&clock, now, &mut tl);
            let target = clock.media_ms_at(now).map(|ms| ms * AUDIO_HZ / 1000.0);
            if target.is_some() {
                out.extend(tl.pull(480, target));
            }
        }
        assert_eq!(rx.late, 0, "o áudio depois da pausa foi descartado");
        // Até MAX_BLIND na pausa e outros tantos no fim (os pacotes acabam de vez).
        assert!(rx.plc >= 1 && rx.plc <= 2 * MAX_BLIND as u64, "preenche só um pouco na pausa: plc {} fec {} decoded {} late {} dup {}", rx.plc, rx.fec, rx.decoded, rx.late, rx.duplicate);
        assert_eq!(rx.decoded, 80);
    }

    #[test]
    fn audio_without_loss_plays_continuously() {
        let (out, rx, tl) = play(&[], false);
        assert_eq!(rx.fec + rx.plc, 0);
        assert_eq!(silent_runs(&out), 0, "corte no áudio");
        assert_eq!(tl.snaps, 0);
    }

    /// Um pacote perdido aqui e ali: o seguinte refaz o que faltou, sem corte.
    #[test]
    fn single_losses_are_rebuilt_from_the_next_packet() {
        let (out, rx, _) = play(&[40, 77, 101], false);
        assert_eq!(rx.fec, 3, "FEC {}, PLC {}", rx.fec, rx.plc);
        assert_eq!(silent_runs(&out), 0, "corte no áudio");
    }

    /// Dois seguidos: o primeiro é preenchido pelo Opus, o segundo refeito pelo próximo.
    #[test]
    fn a_burst_loss_is_concealed_without_silence() {
        let (out, rx, _) = play(&[60, 61], false);
        assert_eq!((rx.plc, rx.fec), (1, 1));
        assert_eq!(silent_runs(&out), 0, "corte no áudio");
    }

    /// Fora de ordem e repetidos (duas faixas): sai igual, sem disfarce nenhum.
    #[test]
    fn duplicates_and_reordering_are_harmless() {
        let (out, rx, _) = play(&[], true);
        assert_eq!(rx.fec + rx.plc, 0);
        assert!(rx.duplicate > 0);
        assert_eq!(silent_runs(&out), 0);
    }

    #[test]
    fn audio_and_video_play_on_the_same_schedule() {
        let t0 = Instant::now();
        let c = SyncClock::new(t0);
        let mut rng = 7u32;
        let mut jitter = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            (rng % 1000) as f64 / 1000.0
        };
        // O relógio do host está 12345 ms "à frente" do daqui (não importa).
        let host_offset = 12_345.0;
        for i in 0..300 {
            let capture_ms = i as f64 * 16.667;
            let ts = ts_at(capture_ms + host_offset);
            let video_arrival = capture_ms + 50.0 + jitter() * 20.0; // malha: 50–70 ms
            let audio_arrival = capture_ms + 10.0 + jitter() * 4.0; // direto: 10–14 ms
            c.observe(Stream::Audio, ts, t0 + Duration::from_secs_f64(audio_arrival / 1000.0));
            c.observe(Stream::Video, ts, t0 + Duration::from_secs_f64(video_arrival / 1000.0));
        }
        assert!(c.ready());
        // Mesmo instante de captura: mesmo horário de tocar, para os dois (é uma agenda só).
        let ts = ts_at(4_000.0 + host_offset);
        let at = c.play_at(ts).unwrap();
        let media_then = c.media_ms_at(at).unwrap();
        assert!((media_then - c.media_ms(ts)).abs() < 0.01, "o áudio toca a mídia {media_then} quando o vídeo mostra {}", c.media_ms(ts));
        // O atraso cobre o vídeo mais lento (base vem do áudio, ~10 ms).
        let d = c.delay_ms();
        assert!(d >= 60.0 + 8.0 - 10.0 && d <= 120.0, "atraso {d}");
        // Todo quadro de vídeo chega antes do horário dele.
        let at_ms = at.duration_since(t0).as_secs_f64() * 1000.0;
        assert!(at_ms > 4_000.0 + 70.0, "toca depois do pior caso de chegada do vídeo: {at_ms}");
    }

    #[test]
    fn nothing_plays_during_warmup() {
        let t0 = Instant::now();
        let c = SyncClock::new(t0);
        c.observe(Stream::Audio, ts_at(0.0), t0);
        assert!(!c.ready());
        assert!(c.play_at(ts_at(0.0)).is_none());
        c.observe(Stream::Audio, ts_at(600.0), t0 + Duration::from_millis(600));
        assert!(c.ready());
    }

    /// A rede piora no meio: o atraso sobe deslizando (nunca pula), no máximo 1% por segundo.
    #[test]
    fn delay_slides_and_never_jumps() {
        let t0 = Instant::now();
        let c = SyncClock::new(t0);
        let mut last = None::<f64>;
        let mut max_step = 0.0f64;
        for i in 0..1200 {
            let capture = i as f64 * 20.0;
            let lat = if i < 600 { 10.0 } else { 90.0 }; // aos 12 s, +80 ms de fila
            let arrival = t0 + Duration::from_secs_f64((capture + lat) / 1000.0);
            c.observe(Stream::Audio, ts_at(capture), arrival);
            if c.ready() {
                let d = c.delay_ms();
                if let Some(l) = last {
                    max_step = max_step.max((d - l).abs());
                }
                last = Some(d);
            }
        }
        // 20 ms entre chegadas a 1%/s = 0,2 ms por passo.
        assert!(max_step <= 0.21, "o atraso pulou {max_step} ms de uma vez");
        assert!(c.delay_ms() > 60.0, "subiu para cobrir a fila: {}", c.delay_ms());
    }

    /// Engarrafamento de 5 s e depois volta ao normal: o atraso sobe para cobrir e depois
    /// volta perto do necessário em menos de ~40 s (antes levava minutos).
    #[test]
    fn delay_comes_back_down_after_congestion() {
        let t0 = Instant::now();
        let c = SyncClock::new(t0);
        let mut peak = 0.0f64;
        for i in 0..4000 {
            let capture = i as f64 * 20.0;
            let lat = if (500..750).contains(&i) { 150.0 } else { 10.0 };
            c.observe(Stream::Audio, ts_at(capture), t0 + Duration::from_secs_f64((capture + lat) / 1000.0));
            peak = peak.max(c.delay_ms());
        }
        assert!(peak > 100.0, "devia ter subido no engarrafamento: {peak}");
        assert!(c.delay_ms() < 50.0, "devia ter voltado (80 s depois): {}", c.delay_ms());
    }

    fn tone(at: i64, frames: usize) -> Vec<f32> {
        // Rampa: o valor da amostra diz qual amostra de mídia ela é (dá para conferir o
        // horário de cada uma na saída).
        (0..frames).flat_map(|i| {
            let v = ((at + i as i64) % 10_000) as f32 / 10_000.0;
            [v, v]
        }).collect()
    }

    /// A placa de som anda 0,1% mais rápida que o relógio do host: a linha do tempo corrige
    /// aos poucos, sem pular nem ficar em silêncio, e fica presa no horário.
    #[test]
    fn timeline_follows_the_clock_despite_device_drift() {
        let mut tl = AudioTimeline::new();
        let mut produced = 0i64;
        let block = 480; // 10 ms
        let mut max_err = 0.0f64;
        for k in 0..6000 {
            // O host manda 20 ms a cada 20 ms (de mídia).
            while produced < (k as i64 + 20) * block as i64 {
                tl.push(produced, &tone(produced, 960));
                produced += 960;
            }
            // O dispositivo pede 10 ms do tempo dele, que corre 0,1% mais rápido.
            let device_t = k as f64 * block as f64 * 1.001;
            let out = tl.pull(block, Some(device_t));
            assert_eq!(out.len(), block * 2);
            if k > 300 {
                max_err = max_err.max((tl.position().unwrap() - (device_t + block as f64 * 1.001)).abs());
            }
        }
        assert_eq!(tl.snaps, 0, "não pode pular");
        assert_eq!(tl.missing, 0, "não pode faltar áudio");
        assert!(max_err < AUDIO_HZ * 0.003, "erro máximo {:.1} ms", max_err / 48.0);
    }

    /// Pacote que nunca chega vira silêncio só no lugar dele (o resto fica no horário).
    #[test]
    fn a_hole_in_the_timeline_stays_in_place() {
        let mut tl = AudioTimeline::new();
        tl.push(0, &tone(0, 960));
        // 960..1920 não chegou
        tl.push(1920, &tone(1920, 960));
        let out = tl.pull(2880, Some(0.0));
        assert!(out[2 * 959] > 0.0);
        assert_eq!(out[2 * 1000], 0.0, "o buraco sai em silêncio");
        let v = out[2 * 2000];
        assert!((v - 2000.0 / 10_000.0).abs() < 1e-4, "depois do buraco, a amostra certa: {v}");
        assert_eq!(tl.snaps, 0);
    }

    /// Começo e salto grande: pula direto para o horário certo.
    #[test]
    fn timeline_snaps_only_on_big_errors() {
        let mut tl = AudioTimeline::new();
        tl.push(0, &tone(0, 48_000));
        tl.pull(480, Some(0.0));
        // 5 ms de erro: corrige aos poucos.
        tl.pull(480, Some(480.0 + 240.0));
        assert_eq!(tl.snaps, 0);
        // 200 ms: pula.
        tl.pull(480, Some(9_600.0 + 960.0));
        assert_eq!(tl.snaps, 1);
    }
}
