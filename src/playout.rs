//! Buffer de reprodução do espectador: mostra cada quadro no horário certo, pelo
//! timestamp RTP, com um pequeno atraso que se adapta à variação da rede.
//!
//! Sem isso, quadros que chegam "juntos" (depois de um pacote atrasado ou de um soluço
//! na rede) aparecem juntos na tela e o vídeo engasga. É o que o jitter buffer do
//! navegador faz no WebRTC.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Atraso máximo que o buffer aceita adicionar.
const MAX_DELAY_MS: f64 = 80.0;
/// Janela usada para medir a variação da rede.
const WINDOW_MS: f64 = 3000.0;

pub struct Playout {
    start: Instant,
    last_ts: Option<u32>,
    ext_ts: i64,
    /// (horário local em ms, trânsito = local − mídia)
    window: VecDeque<(f64, f64)>,
    delay: f64,
}

impl Playout {
    pub fn new() -> Self {
        Self { start: Instant::now(), last_ts: None, ext_ts: 0, window: VecDeque::new(), delay: 0.0 }
    }

    /// Atraso atual do buffer (ms).
    pub fn delay_ms(&self) -> f64 {
        self.delay
    }

    /// Horário em que o quadro com este timestamp RTP deve aparecer.
    pub fn schedule(&mut self, rtp_ts: u32) -> Instant {
        // Timestamp estendido (sem a virada de 32 bits), em ms (90 kHz).
        match self.last_ts {
            None => self.ext_ts = 0,
            Some(prev) => self.ext_ts += rtp_ts.wrapping_sub(prev) as i32 as i64,
        }
        self.last_ts = Some(rtp_ts);
        let media_ms = self.ext_ts as f64 / 90.0;
        let local_ms = self.start.elapsed().as_secs_f64() * 1000.0;
        let transit = local_ms - media_ms;

        // Pulo grande (troca de fonte, pausa longa): recomeça a referência.
        if let Some(&(_, last)) = self.window.back() {
            if (transit - last).abs() > 1000.0 {
                self.window.clear();
            }
        }
        self.window.push_back((local_ms, transit));
        while self.window.front().is_some_and(|&(t, _)| local_ms - t > WINDOW_MS) {
            self.window.pop_front();
        }

        // O trânsito mínimo é o caminho "sem atraso"; a variação acima dele é o jitter.
        let min = self.window.iter().map(|&(_, x)| x).fold(f64::INFINITY, f64::min);
        let mut jitters: Vec<f64> = self.window.iter().map(|&(_, x)| x - min).collect();
        jitters.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p95 = jitters[((jitters.len() as f64 * 0.95) as usize).min(jitters.len() - 1)];
        let target = p95.min(MAX_DELAY_MS);
        // Sobe em passos pequenos (dilui o ajuste em vários quadros em vez de um engasgo)
        // e desce devagar (sem oscilar).
        self.delay = if target > self.delay { (self.delay + 4.0).min(target) } else { self.delay * 0.98 + target * 0.02 };

        let show_ms = media_ms + min + self.delay;
        let at = self.start + Duration::from_secs_f64(show_ms.max(0.0) / 1000.0);
        at.max(Instant::now())
    }
}

/// Dorme até `t` com precisão de ~0,1 ms (dorme o grosso e espera ativamente o final,
/// como o `os_sleepto_ns` do OBS).
pub fn sleep_until(t: Instant) {
    loop {
        let now = Instant::now();
        if now >= t {
            return;
        }
        let left = t - now;
        if left > Duration::from_millis(2) {
            std::thread::sleep(left - Duration::from_micros(1500));
        } else {
            std::hint::spin_loop();
        }
    }
}
