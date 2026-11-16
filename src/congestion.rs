//! Bitrate adaptativo: quanto a rede entre o host e cada espectador aguenta.
//!
//! Quem assiste manda um relatório a cada ~500 ms pelo canal de dados (taxa recebida,
//! perda de pacotes e atraso de fila). O host roda um controle AIMD parecido com o GCC
//! do WebRTC: sobe devagar enquanto a rede está folgada, corta rápido quando aparece
//! perda ou fila crescendo. O encoder é um só para todos, então o alvo é o do espectador
//! com a pior conexão (sem passar do que quem transmite escolheu).
//!
//! Com pouca banda para a resolução, também cai para 30 fps (cada quadro fica com o
//! dobro de bits); quem assiste pode ligar a geração de quadros para voltar a 60.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Intervalo entre relatórios do espectador.
pub const REPORT_INTERVAL: Duration = Duration::from_millis(500);
/// Sem relatório há esse tempo: o espectador não entra na conta (travou ou saiu).
pub const REPORT_STALE: Duration = Duration::from_secs(4);
/// Piso do bitrate adaptado.
pub const MIN_BPS: u32 = 300_000;

/// Perda ou fila acima disso: a rede está sobrecarregada, corta.
const LOSS_OVERUSE: f32 = 0.10;
const QUEUE_OVERUSE_MS: f32 = 150.0;
/// Acima disso (e abaixo da sobrecarga): segura onde está.
const LOSS_HOLD: f32 = 0.02;
const QUEUE_HOLD_MS: f32 = 60.0;
/// Corte: um pouco abaixo do que está chegando de fato.
const DECREASE: f64 = 0.85;
/// Subida por segundo com a rede folgada.
const INCREASE_PER_SEC: f64 = 1.15;
/// Depois de um corte: sem subir por esse tempo, sem cortar de novo antes disso.
const HOLD_AFTER_DECREASE: Duration = Duration::from_secs(2);
const MIN_BETWEEN_DECREASES: Duration = Duration::from_secs(1);

/// Relatório do espectador (vai como JSON `{"t":"rr", ...}`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// Taxa de vídeo recebida (kb/s).
    pub kbps: u32,
    /// Fração de pacotes perdidos no intervalo (0..1).
    pub loss: f32,
    /// Atraso de fila estimado (ms): o quanto os quadros estão chegando mais
    /// atrasados que o mínimo recente.
    pub qd: f32,
}

/// Estimativa de banda de um espectador.
pub struct RateController {
    estimate: f64,
    last_report: Option<Instant>,
    last_decrease: Option<Instant>,
}

impl RateController {
    /// Começa na metade do máximo (pelo menos 3 Mb/s) e sobe se a rede deixar.
    pub fn new(max_bps: u32) -> Self {
        let max = max_bps.max(MIN_BPS) as f64;
        Self { estimate: (max * 0.5).max(3_000_000f64.min(max)), last_report: None, last_decrease: None }
    }

    pub fn estimate(&self) -> u32 {
        self.estimate as u32
    }

    pub fn last_report(&self) -> Option<Instant> {
        self.last_report
    }

    /// Aplica um relatório. Devolve a nova estimativa (b/s).
    pub fn on_report(&mut self, r: &Report, max_bps: u32, now: Instant) -> u32 {
        let max = max_bps.max(MIN_BPS) as f64;
        let dt = self.last_report.map(|t| now.saturating_duration_since(t)).unwrap_or(REPORT_INTERVAL);
        let dt = dt.clamp(Duration::from_millis(100), Duration::from_secs(2)).as_secs_f64();
        self.last_report = Some(now);
        let received = r.kbps as f64 * 1000.0;

        if r.loss > LOSS_OVERUSE || r.qd > QUEUE_OVERUSE_MS {
            if self.last_decrease.is_none_or(|t| now.saturating_duration_since(t) >= MIN_BETWEEN_DECREASES) {
                let base = if received > 0.0 { received.min(self.estimate) } else { self.estimate };
                self.estimate = base * DECREASE;
                self.last_decrease = Some(now);
            }
        } else if r.loss > LOSS_HOLD || r.qd > QUEUE_HOLD_MS {
            // Perto do limite: segura.
        } else if self.last_decrease.is_none_or(|t| now.saturating_duration_since(t) >= HOLD_AFTER_DECREASE) {
            // Folga. Só sobe se a banda liberada está sendo usada: com a tela parada o
            // encoder manda bem menos que o alvo e não dá para saber se caberia mais.
            if received >= self.estimate * 0.6 {
                self.estimate *= INCREASE_PER_SEC.powf(dt);
            }
        }
        self.estimate = self.estimate.clamp(MIN_BPS as f64, max);
        self.estimate()
    }
}

/// Alvo do encoder: a pior conexão entre os espectadores com relatório recente.
pub fn combined_target<'a>(controllers: impl Iterator<Item = &'a RateController>, now: Instant) -> Option<u32> {
    controllers
        .filter(|c| c.last_report().is_some_and(|t| now.saturating_duration_since(t) < REPORT_STALE))
        .map(|c| c.estimate())
        .min()
}

/// Versões menores que o host pode mandar, além da principal (alturas).
pub const EXTRA_HEIGHTS: [u32; 2] = [720, 480];

/// Bitrate de uma versão menor: proporcional à área, com um pouco mais de bits por pixel
/// (imagem menor comprime pior). 1080p a 12 Mb/s -> 720p ~6,5 e 480p ~3,6.
pub fn extra_bitrate(main_bps: u32, main_height: u32, height: u32) -> u32 {
    let ratio = (height as f64 / main_height.max(1) as f64).clamp(0.0, 1.0);
    ((main_bps as f64 * ratio.powf(1.5)) as u32).max(MIN_BPS)
}

/// Versão que o "Auto" escolhe para quem tem `estimate` b/s: a melhor que cabe com
/// folga. `options` = (chave, bitrate), da melhor para a pior.
pub fn pick_rendition(options: &[(u32, u32)], estimate: u32) -> u32 {
    options.iter().find(|(_, bps)| estimate as f64 >= *bps as f64 * 0.8).or(options.last()).map(|(k, _)| *k).unwrap_or(0)
}

/// Bits por pixel por quadro abaixo disso: cai para 30 fps. Acima de `FPS_RESTORE_BPP`
/// (medido no fps escolhido por quem transmite): volta. A folga entre os dois evita
/// ficar trocando.
const FPS_DROP_BPP: f64 = 0.02;
const FPS_RESTORE_BPP: f64 = 0.04;
/// fps usado quando falta banda.
pub const LOW_FPS: u32 = 30;

/// Decide o fps com a banda disponível. `current` = limite de fps atual (None = sem limite).
pub fn adapt_fps(user_fps: u32, width: u32, height: u32, cap_bps: Option<u32>, current: Option<u32>) -> Option<u32> {
    let Some(cap) = cap_bps else { return None };
    if user_fps <= LOW_FPS {
        return None;
    }
    let bpp = cap as f64 / (width.max(1) as f64 * height.max(1) as f64 * user_fps as f64);
    match current {
        None if bpp < FPS_DROP_BPP => Some(LOW_FPS),
        Some(_) if bpp > FPS_RESTORE_BPP => None,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rr(kbps: u32, loss: f32, qd: f32) -> Report {
        Report { kbps, loss, qd }
    }

    /// Simula uma rede com `link` kb/s: o que passar disso vira fila e perda.
    fn run(c: &mut RateController, max: u32, link_kbps: u32, secs: u32, t: &mut Instant) {
        for _ in 0..secs * 2 {
            *t += REPORT_INTERVAL;
            let sending = c.estimate() / 1000;
            let r = if sending > link_kbps {
                let over = (sending - link_kbps) as f32 / sending as f32;
                rr(link_kbps, over, 40.0 + over * 1000.0)
            } else {
                rr(sending, 0.0, 5.0)
            };
            c.on_report(&r, max, *t);
        }
    }

    #[test]
    fn converges_below_a_slow_link_and_stays() {
        let mut t = Instant::now();
        let mut c = RateController::new(12_000_000);
        run(&mut c, 12_000_000, 4_000, 20, &mut t);
        let mut seen = vec![];
        for _ in 0..20 {
            run(&mut c, 12_000_000, 4_000, 1, &mut t);
            seen.push(c.estimate());
        }
        let (lo, hi) = (*seen.iter().min().unwrap(), *seen.iter().max().unwrap());
        assert!(hi <= 4_300_000, "passou muito do link: {hi}");
        assert!(lo >= 2_500_000, "caiu demais: {lo}");
    }

    #[test]
    fn climbs_to_the_max_on_a_fast_link() {
        let mut t = Instant::now();
        let mut c = RateController::new(12_000_000);
        run(&mut c, 12_000_000, 100_000, 15, &mut t);
        assert_eq!(c.estimate(), 12_000_000);
    }

    #[test]
    fn recovers_after_the_link_improves() {
        let mut t = Instant::now();
        let mut c = RateController::new(12_000_000);
        run(&mut c, 12_000_000, 2_000, 15, &mut t);
        assert!(c.estimate() < 2_200_000);
        run(&mut c, 12_000_000, 100_000, 20, &mut t);
        assert_eq!(c.estimate(), 12_000_000);
    }

    #[test]
    fn does_not_climb_while_the_encoder_sends_little() {
        // Tela parada: chega bem menos que a estimativa, sem perda.
        let mut t = Instant::now();
        let mut c = RateController::new(20_000_000);
        let start = c.estimate();
        for _ in 0..20 {
            t += REPORT_INTERVAL;
            c.on_report(&rr(1_500, 0.0, 3.0), 20_000_000, t);
        }
        assert_eq!(c.estimate(), start);
    }

    #[test]
    fn respects_the_users_max_and_the_floor() {
        let mut t = Instant::now();
        let mut c = RateController::new(5_000_000);
        run(&mut c, 5_000_000, 100_000, 20, &mut t);
        assert_eq!(c.estimate(), 5_000_000);
        for _ in 0..40 {
            t += REPORT_INTERVAL;
            c.on_report(&rr(50, 0.5, 900.0), 5_000_000, t);
        }
        assert_eq!(c.estimate(), MIN_BPS);
    }

    #[test]
    fn weakest_recent_viewer_wins() {
        let now = Instant::now();
        let mut a = RateController::new(12_000_000);
        let mut b = RateController::new(12_000_000);
        let stale = RateController::new(12_000_000); // nunca mandou relatório
        a.on_report(&rr(6_000, 0.0, 5.0), 12_000_000, now);
        b.on_report(&rr(2_000, 0.3, 300.0), 12_000_000, now);
        let target = combined_target([&a, &b, &stale].into_iter(), now).unwrap();
        assert_eq!(target, b.estimate());
        assert!(combined_target([&a].into_iter(), now + REPORT_STALE).is_none());
    }

    #[test]
    fn renditions_scale_with_area_and_auto_picks_what_fits() {
        assert_eq!(extra_bitrate(12_000_000, 1080, 1080), 12_000_000);
        let b720 = extra_bitrate(12_000_000, 1080, 720);
        let b480 = extra_bitrate(12_000_000, 1080, 480);
        assert!((6_000_000..7_000_000).contains(&b720), "{b720}");
        assert!((3_000_000..4_000_000).contains(&b480), "{b480}");
        let opts = [(0, 12_000_000), (720, b720), (480, b480)];
        assert_eq!(pick_rendition(&opts, 20_000_000), 0);
        assert_eq!(pick_rendition(&opts, 10_000_000), 0, "80% do alvo já serve");
        assert_eq!(pick_rendition(&opts, 7_000_000), 720);
        assert_eq!(pick_rendition(&opts, 3_000_000), 480);
        assert_eq!(pick_rendition(&opts, 500_000), 480, "nada cabe: a menor");
    }

    #[test]
    fn fps_drops_with_little_bandwidth_and_comes_back_with_margin() {
        // 1080p60: cai abaixo de ~2,5 Mb/s, volta acima de ~5 Mb/s.
        let (w, h) = (1920, 1080);
        assert_eq!(adapt_fps(60, w, h, Some(2_000_000), None), Some(30));
        assert_eq!(adapt_fps(60, w, h, Some(4_000_000), Some(30)), Some(30), "não volta logo");
        assert_eq!(adapt_fps(60, w, h, Some(6_000_000), Some(30)), None);
        assert_eq!(adapt_fps(60, w, h, Some(4_000_000), None), None, "não cai sem precisar");
        assert_eq!(adapt_fps(30, w, h, Some(500_000), None), None, "já está em 30");
        assert_eq!(adapt_fps(60, w, h, None, Some(30)), None);
    }
}
