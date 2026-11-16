//! Agenda do player: em que horário desenhar cada quadro, real ou gerado (ver `framegen`).
//!
//! Cada par (quadro atual -> próximo) chega com os horários dos quadros gerados e o do
//! real. A agenda segue esses horários e nunca deixa o atraso crescer: se a tela não dá
//! conta do ritmo (monitor mais lento, placa ocupada, espera do vsync), os quadros
//! gerados que já passaram da hora ficam de fora e o real aparece na hora dele. Antes,
//! todo quadro gerado era desenhado mesmo atrasado; cada par levava mais que o intervalo
//! da stream, o atraso ia somando e a imagem andava em câmera lenta até pular para frente.
//!
//! Genérica no quadro (`F`) e no movimento (`P`) para os testes simularem sem placa.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub enum Entry<F, P> {
    /// Quadro normal: substitui o mostrado.
    Frame(F),
    /// Quadros gerados entre o mostrado e `next` (horário e fração de cada um), depois
    /// `next` em `next_at`.
    Pair { next: F, mid: P, steps: Vec<(Instant, f32)>, next_at: Instant },
}

pub enum Action<F, P> {
    /// Nada na hora.
    Idle,
    /// Desenhar: Some(t) = quadro gerado na fração t; None = o real preparado (que passa a
    /// ser o mostrado antes de desenhar).
    Draw(Option<f32>),
    /// Mostrar este quadro normal.
    Show(F),
    /// Preparar o par (enviar o próximo e o movimento) e avisar com `prepared`.
    Prepare(F, P),
    /// O real preparado passa a ser o mostrado, sem desenhar (pulo para o mais novo).
    Advance,
}

/// Mais que isso na fila: muito atrasado, pula para o quadro mais novo.
const MAX_QUEUE: usize = 3;

pub struct Agenda<F, P> {
    /// Próximos desenhos com horário: Some(t) = gerado; None = o real.
    schedule: VecDeque<(Instant, Option<f32>)>,
    /// O que chegou enquanto a agenda do par anterior não terminou. Na rede os quadros
    /// chegam em grupos; descartar o real que esperava a vez dava engasgo a cada grupo.
    queue: VecDeque<Entry<F, P>>,
    /// Horários do par sendo preparado.
    plan: Option<(Vec<(Instant, f32)>, Instant)>,
    /// Testes: o comportamento antigo (desenha tudo, desloca sem limite).
    #[cfg(test)]
    legacy: bool,
}

impl<F, P> Agenda<F, P> {
    pub fn new() -> Self {
        Self {
            schedule: VecDeque::new(),
            queue: VecDeque::new(),
            plan: None,
            #[cfg(test)]
            legacy: false,
        }
    }

    fn legacy(&self) -> bool {
        #[cfg(test)]
        return self.legacy;
        #[cfg(not(test))]
        false
    }

    pub fn push(&mut self, e: Entry<F, P>) {
        self.queue.push_back(e);
    }

    /// Quanto dá para esperar por mensagens novas (None = até chegar uma).
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        match (self.schedule.front(), self.queue.is_empty()) {
            (Some((at, _)), _) => Some(at.saturating_duration_since(now)),
            (None, false) => Some(Duration::ZERO),
            (None, true) => None,
        }
    }

    pub fn next(&mut self, now: Instant) -> Action<F, P> {
        // Muito atrasado: pula para o mais novo (gerar entre quadros que não são vizinhos
        // daria errado, então ele entra como quadro normal).
        if self.queue.len() > MAX_QUEUE {
            let had_real = self.schedule.iter().any(|(_, step)| step.is_none());
            self.schedule.clear();
            self.plan = None;
            let newest = self.queue.pop_back().unwrap();
            self.queue.clear();
            self.queue.push_back(match newest {
                Entry::Pair { next, .. } => Entry::Frame(next),
                e => e,
            });
            if had_real {
                return Action::Advance;
            }
        }
        if !self.legacy() {
            // A tela ficou para trás: quadro gerado cujo seguinte também já está na hora
            // fica de fora (o real nunca).
            while self.schedule.len() >= 2 && self.schedule[0].1.is_some() && self.schedule[1].0 <= now {
                self.schedule.pop_front();
            }
        }
        if let Some(&(at, step)) = self.schedule.front() {
            if at > now {
                return Action::Idle;
            }
            self.schedule.pop_front();
            return Action::Draw(step);
        }
        match self.queue.pop_front() {
            Some(Entry::Frame(f)) => Action::Show(f),
            Some(Entry::Pair { next, mid, steps, next_at }) => {
                self.plan = Some((steps, next_at));
                Action::Prepare(next, mid)
            }
            None => Action::Idle,
        }
    }

    /// Resultado de `Action::Prepare` (false: não deu para interpolar e o quadro já foi
    /// mostrado como normal).
    pub fn prepared(&mut self, ok: bool, now: Instant) {
        let Some((steps, next_at)) = self.plan.take() else { return };
        if !ok {
            return;
        }
        // Chegou antes de o par anterior terminar (chegadas em grupo): desloca a agenda,
        // mantendo o espaçamento. No máximo até o horário do real; o que passar disso já
        // está atrasado e os gerados atrasados ficam de fora.
        let first = steps.first().map(|s| s.0).unwrap_or(next_at);
        let mut shift = now.saturating_duration_since(first);
        if !self.legacy() {
            shift = shift.min(next_at.saturating_duration_since(first));
        }
        self.schedule.extend(steps.into_iter().map(|(at, t)| (at + shift, Some(t))));
        self.schedule.push_back((next_at + shift, None));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Run {
        /// Atraso máximo do quadro real em relação ao horário dele (ms).
        max_lag: f64,
        /// Atraso médio do real no último segundo (ms): cresce = câmera lenta.
        lag_end: f64,
        /// Reais que nunca apareceram (pulos para frente).
        dropped: usize,
        /// Quadros na tela por segundo.
        fps: f64,
    }

    /// Simula a thread de desenho num relógio virtual: um par a cada `gap_ms`, `n` quadros
    /// por intervalo, cada desenho levando `render_ms`. `early`: (a cada quantos, ms) um
    /// quadro chega adiantado (rede em grupos).
    fn simulate(legacy: bool, gap_ms: f64, n: u32, render_ms: f64, early: Option<(usize, f64)>, secs: f64) -> Run {
        let ms = |x: f64| Duration::from_secs_f64(x / 1000.0);
        let t0 = Instant::now();
        let step = gap_ms / n as f64;
        let total = (secs * 1000.0 / gap_ms) as usize;
        // Horário em que cada par chega e o horário certo do real dele.
        let arrive: Vec<Instant> = (0..total)
            .map(|k| {
                let at = t0 + ms(k as f64 * gap_ms);
                match early {
                    Some((every, e)) if k % every == 1 => at - ms(e),
                    _ => at,
                }
            })
            .collect();
        let due: Vec<Instant> = (0..total).map(|k| t0 + ms(k as f64 * gap_ms + step * (n - 1) as f64)).collect();

        let mut a: Agenda<usize, ()> = Agenda::new();
        a.legacy = legacy;
        let (mut t, mut k) = (t0, 0usize);
        let mut prepared: VecDeque<usize> = VecDeque::new();
        let mut lags: Vec<(Instant, f64)> = vec![];
        let mut draws = 0usize;
        let end = t0 + ms(secs * 1000.0);
        while t < end {
            while k < total && arrive[k] <= t {
                let at = arrive[k];
                let steps = (1..n).map(|j| (at + ms(step * (j - 1) as f64), j as f32 / n as f32)).collect();
                a.push(Entry::Pair { next: k, mid: (), steps, next_at: at + ms(step * (n - 1) as f64) });
                k += 1;
            }
            let mut real = |id: usize, t: Instant| lags.push((t, t.saturating_duration_since(due[id]).as_secs_f64() * 1000.0));
            match a.next(t) {
                Action::Draw(None) => {
                    real(prepared.pop_front().unwrap(), t);
                    draws += 1;
                    t += ms(render_ms);
                }
                Action::Draw(Some(_)) => {
                    draws += 1;
                    t += ms(render_ms);
                }
                Action::Show(id) => {
                    real(id, t);
                    draws += 1;
                    t += ms(render_ms);
                }
                Action::Prepare(id, ()) => {
                    a.prepared(true, t);
                    prepared.push_back(id);
                }
                Action::Advance => {
                    prepared.pop_front();
                }
                Action::Idle => {
                    let next_arrival = arrive.get(k).copied().unwrap_or(end);
                    let wake = a.wait(t).map(|w| t + w).unwrap_or(end);
                    t = next_arrival.min(wake).max(t + ms(0.01));
                }
            }
        }
        let shown = lags.len();
        let in_flight = k - shown;
        let last: Vec<f64> = lags.iter().filter(|(at, _)| *at + ms(1000.0) >= end).map(|x| x.1).collect();
        Run {
            max_lag: lags.iter().map(|x| x.1).fold(0.0, f64::max),
            lag_end: last.iter().sum::<f64>() / last.len().max(1) as f64,
            dropped: in_flight.saturating_sub(MAX_QUEUE + 1),
            fps: draws as f64 / secs,
        }
    }

    fn show(name: &str, r: &Run) {
        println!("{name}: atraso do real máx {:.1} ms, no fim {:.1} ms | {} reais pulados | {:.0} quadros/s", r.max_lag, r.lag_end, r.dropped, r.fps);
    }

    /// Stream a 60, assistindo a 120, mas a tela só dá ~100 quadros/s (cada desenho leva
    /// 10 ms). Era o "câmera lenta": cada par levava 20 ms para um intervalo de 16,7.
    #[test]
    fn slow_screen_does_not_slow_the_stream_down() {
        let old = simulate(true, 1000.0 / 60.0, 2, 10.0, None, 10.0);
        let new = simulate(false, 1000.0 / 60.0, 2, 10.0, None, 10.0);
        show("antes", &old);
        show("agora", &new);
        assert!(old.max_lag > 40.0, "a simulação devia reproduzir o atraso crescendo");
        assert!(new.max_lag < 1000.0 / 60.0, "real atrasou {:.1} ms", new.max_lag);
        assert_eq!(new.dropped, 0);
        // Ainda gera alguns quadros (não volta simplesmente para 60).
        assert!(new.fps > 60.0, "{:.0} quadros/s", new.fps);
    }

    /// Tela rápida: tudo na hora, todos os gerados aparecem.
    #[test]
    fn fast_screen_shows_every_frame_on_time() {
        let r = simulate(false, 1000.0 / 30.0, 4, 1.0, None, 10.0);
        show("30 -> 120", &r);
        assert!(r.max_lag < 2.0);
        assert_eq!(r.dropped, 0);
        assert!(r.fps > 118.0, "{:.0} quadros/s", r.fps);
    }

    /// Chegadas em grupo (um em três 14 ms adiantado): continua sem acumular atraso e sem
    /// perder quadros reais.
    #[test]
    fn bunched_arrivals_keep_the_rhythm() {
        let r = simulate(false, 1000.0 / 30.0, 4, 1.0, Some((3, 14.0)), 10.0);
        show("30 -> 120 em grupos", &r);
        assert!(r.max_lag < 1000.0 / 30.0, "real atrasou {:.1} ms", r.max_lag);
        assert!(r.lag_end < 5.0, "atraso no fim {:.1} ms", r.lag_end);
        assert_eq!(r.dropped, 0);
        assert!(r.fps > 110.0, "{:.0} quadros/s", r.fps);
    }
}
