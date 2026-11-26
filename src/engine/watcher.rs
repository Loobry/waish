//! Emotive Hardware Watcher & Battery Promise State Machine.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Timelike;
use tokio::sync::mpsc;

use super::kaomoji::{is_late_night_hour, EmotionSpectrum, KaomojiEngine};
use super::telemetry::HardwareSnapshot;
use crate::cli::ui::Kaomoji;
use crate::tools::desktop::{export_waybar_status_with_kaomoji, WaybarState};

pub const WATCHER_POLL_INTERVAL_SECS: u64 = 30;
pub const POSTPONE_DURATION_SECS: u64 = 45 * 60;
pub const LOW_BATTERY_THRESHOLD: u8 = 20;
pub const CRITICAL_BATTERY_THRESHOLD: u8 = 7;
pub const BROKEN_PROMISE_DROP_PCT: u8 = 3;
pub const RAM_SPIKE_THRESHOLD: u8 = 90;
pub const CPU_TEMP_THRESHOLD_C: f32 = 85.0;

/// User commitment state machine for battery charging reminders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatteryPromiseState {
    None,
    Postponed { until: Instant },
    PromisedToCharge { noted_capacity: u8 },
}

/// In-memory runtime state for the Hardware Telemetry Watcher.
#[derive(Debug, Clone)]
pub struct HardwareWatcherState {
    pub last_battery_pct: u8,
    pub last_status: String,
    pub promise_state: BatteryPromiseState,
    pub ram_spike_counter: u8,
    pub low_battery_warned: bool,
    pub critical_battery_warned: bool,
    pub broken_promise_triggered: bool,
    pub ram_warned: bool,
    pub thermal_warned: bool,
}

impl Default for HardwareWatcherState {
    fn default() -> Self {
        Self {
            last_battery_pct: 100,
            last_status: "Unknown".to_string(),
            promise_state: BatteryPromiseState::None,
            ram_spike_counter: 0,
            low_battery_warned: false,
            critical_battery_warned: false,
            broken_promise_triggered: false,
            ram_warned: false,
            thermal_warned: false,
        }
    }
}

/// Proactive hardware alert category emitted by the watcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProactiveEventKind {
    LowBattery,
    CriticalBattery,
    BrokenPromise,
    RamSpike,
    HighTemperature,
}

impl ProactiveEventKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::LowBattery => "Low Battery",
            Self::CriticalBattery => "Critical Battery",
            Self::BrokenPromise => "Broken Promise",
            Self::RamSpike => "RAM Spike",
            Self::HighTemperature => "CPU Overheat",
        }
    }
}

/// Proactive event payload sent from the watcher task to the REPL over `mpsc`.
#[derive(Debug, Clone)]
pub struct ProactiveEvent {
    pub kind: ProactiveEventKind,
    pub mood: Kaomoji,
    pub system_instruction: String,
    pub fallback_message: String,
}

impl HardwareWatcherState {
    pub fn postpone_battery_warning(&mut self, now: Instant) {
        self.promise_state = BatteryPromiseState::Postponed {
            until: now + Duration::from_secs(POSTPONE_DURATION_SECS),
        };
        self.low_battery_warned = true;
    }

    pub fn record_charge_promise(&mut self) {
        self.promise_state = BatteryPromiseState::PromisedToCharge {
            noted_capacity: self.last_battery_pct,
        };
        self.broken_promise_triggered = false;
    }

    pub fn clear_battery_promise(&mut self) {
        self.promise_state = BatteryPromiseState::None;
        self.low_battery_warned = false;
        self.critical_battery_warned = false;
        self.broken_promise_triggered = false;
    }

    pub fn observe_user_reply(&mut self, user_input: &str, now: Instant) -> Option<&'static str> {
        let is_discharging_low = self.last_status.eq_ignore_ascii_case("discharging")
            && (self.last_battery_pct <= LOW_BATTERY_THRESHOLD || self.low_battery_warned);

        if !is_discharging_low {
            return None;
        }

        let lower = user_input.trim().to_ascii_lowercase();
        let postpone_markers = [
            "nanti aja",
            "nanti dulu",
            "bentar lagi",
            "lagi di luar",
            "di luar",
            "gak bawa charger",
            "ga bawa charger",
            "belum ada colokan",
            "postpone",
            "later",
        ];
        if postpone_markers.iter().any(|m| lower.contains(m)) {
            self.postpone_battery_warning(now);
            return Some("postponed_45m");
        }

        let promise_markers = [
            "mau di-charge",
            "mau dicharge",
            "mau charge",
            "mau ngecas",
            "colok sekarang",
            "aku colok",
            "gue colok",
            "cas sekarang",
            "otw charge",
            "otw cas",
            "plugging in",
            "charging now",
        ];
        if promise_markers.iter().any(|m| lower.contains(m)) {
            self.record_charge_promise();
            return Some("promised_to_charge");
        }

        None
    }

    pub fn evaluate(&mut self, snapshot: &HardwareSnapshot, now: Instant) -> Vec<ProactiveEvent> {
        let mut events = Vec::new();

        if let Some(bat) = &snapshot.battery {
            self.last_battery_pct = bat.capacity;
            self.last_status = bat.status.clone();

            let discharging = bat.status.eq_ignore_ascii_case("discharging");
            let plugged_in = bat.status.eq_ignore_ascii_case("charging")
                || bat.status.eq_ignore_ascii_case("full");

            if plugged_in {
                self.clear_battery_promise();
            } else if discharging {
                self.evaluate_discharging_battery(bat.capacity, now, &mut events);
            }
        }

        if let Some(ram_pct) = snapshot.ram_used_pct {
            if ram_pct > RAM_SPIKE_THRESHOLD {
                self.ram_spike_counter = self.ram_spike_counter.saturating_add(1);
                if self.ram_spike_counter >= 2 && !self.ram_warned {
                    self.ram_warned = true;
                    events.push(ProactiveEvent {
                        kind: ProactiveEventKind::RamSpike,
                        mood: Kaomoji::Warn,
                        system_instruction: format!(
                            "[SYSTEM EVENT: RAM_SPIKE]\n\
                             Penggunaan RAM sistem berada di {ram_pct}% selama 2 interval berturut-turut.\n\
                             Ingatkan user dengan gaya companion kamu dan tawarkan untuk menganalisa 5 proses pemakan RAM terbesar (`ps -eo comm,%mem --sort=-%mem | head -n 5`)."
                        ),
                        fallback_message: format!(
                            "Eh, RAM kamu sudah tembus {ram_pct}% terus-terusan nih! Mau aku cek 5 proses terberat pakai `ps -eo comm,%mem --sort=-%mem | head -n 5`?"
                        ),
                    });
                }
            } else if ram_pct <= 85 {
                self.ram_spike_counter = 0;
                self.ram_warned = false;
            }
        }

        if let Some(temp_c) = snapshot.cpu_temp_c {
            if temp_c > CPU_TEMP_THRESHOLD_C && !self.thermal_warned {
                self.thermal_warned = true;
                events.push(ProactiveEvent {
                    kind: ProactiveEventKind::HighTemperature,
                    mood: Kaomoji::Warn,
                    system_instruction: format!(
                        "[SYSTEM EVENT: CPU_OVERHEAT]\n\
                         Suhu CPU laptop saat ini mencapai {temp_c:.1}°C (di atas ambang 85°C).\n\
                         Peringatkan user dengan nada perhatian/protektif agar mengecek beban proses atau ventilasi laptop."
                    ),
                    fallback_message: format!(
                        "Waduh, suhu CPU kamu naik sampai {temp_c:.1} C nih! Coba cek ventilasi atau proses berat yang lagi jalan yaa."
                    ),
                });
            } else if temp_c <= 78.0 {
                self.thermal_warned = false;
            }
        }

        events
    }

    fn evaluate_discharging_battery(
        &mut self,
        current_pct: u8,
        now: Instant,
        events: &mut Vec<ProactiveEvent>,
    ) {
        match self.promise_state.clone() {
            BatteryPromiseState::PromisedToCharge { noted_capacity } => {
                if noted_capacity.saturating_sub(current_pct) >= BROKEN_PROMISE_DROP_PCT
                    && !self.broken_promise_triggered
                {
                    self.broken_promise_triggered = true;
                    events.push(ProactiveEvent {
                        kind: ProactiveEventKind::BrokenPromise,
                        mood: Kaomoji::Angry,
                        system_instruction: format!(
                            "[SYSTEM EVENT: USER_BROKE_PROMISE]\n\
                             User sebelumnya berjanji akan segera men-charge laptop (saat itu {noted_capacity}%), tetapi baterai saat ini turun ke {current_pct}% dan laptop belum dicolok.\n\
                             Tegur user dengan nada kesal/tsundere khas kamu, tanyakan kenapa belum dicolok juga!"
                        ),
                        fallback_message: format!(
                            "Heh! Tadi katanya mau langsung dicolok pas {noted_capacity}%, sekarang udah turun ke {current_pct}% tapi belum dicolok juga?! Colok charger-nya sekarang dong!"
                        ),
                    });
                }
            }
            BatteryPromiseState::Postponed { until } => {
                if current_pct < CRITICAL_BATTERY_THRESHOLD && !self.critical_battery_warned {
                    self.critical_battery_warned = true;
                    events.push(ProactiveEvent {
                        kind: ProactiveEventKind::CriticalBattery,
                        mood: Kaomoji::Warn,
                        system_instruction: format!(
                            "[SYSTEM EVENT: BATTERY_CRITICAL]\n\
                             Baterai sudah mencapai level kritis {current_pct}% (di bawah 7%) meskipun sebelumnya user menunda.\n\
                             Segera peringatkan user dengan panik/protektif agar langsung simpan pekerjaan dan colok charger!"
                        ),
                        fallback_message: format!(
                            "Gawat! Baterai tinggal {current_pct}% lagi nih, bentar lagi mati! Simpan kerjaan kamu dan cari colokan sekarang yaa!"
                        ),
                    });
                } else if now >= until {
                    self.promise_state = BatteryPromiseState::None;
                    self.low_battery_warned = false;
                    if current_pct < LOW_BATTERY_THRESHOLD {
                        self.low_battery_warned = true;
                        events.push(build_low_battery_event(current_pct));
                    }
                }
            }
            BatteryPromiseState::None => {
                if current_pct < LOW_BATTERY_THRESHOLD && !self.low_battery_warned {
                    self.low_battery_warned = true;
                    events.push(build_low_battery_event(current_pct));
                }
            }
        }
    }
}

fn build_low_battery_event(current_pct: u8) -> ProactiveEvent {
    ProactiveEvent {
        kind: ProactiveEventKind::LowBattery,
        mood: Kaomoji::Warn,
        system_instruction: format!(
            "[SYSTEM EVENT: BATTERY_LOW]\n\
             Baterai laptop pengguna turun ke {current_pct}% dan sedang tidak di-charge (Discharging).\n\
             Mulai obrolan peringatan santai bergaya companion untuk mengingatkan user agar men-charge laptop."
        ),
        fallback_message: format!(
            "Psst, baterai laptop kamu tinggal {current_pct}% lho dan belum dicolok. Mau di-charge sekarang atau lagi di luar?"
        ),
    }
}

pub fn spawn_hardware_watcher(
    state: Arc<Mutex<HardwareWatcherState>>,
    kaomoji_engine: Arc<Mutex<KaomojiEngine>>,
    tx: mpsc::Sender<ProactiveEvent>,
    bot_name: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(WATCHER_POLL_INTERVAL_SECS));
        interval.tick().await;
        let mut was_media_playing = false;

        loop {
            interval.tick().await;

            let snapshot = tokio::task::spawn_blocking(HardwareSnapshot::collect)
                .await
                .unwrap_or_else(|_| HardwareSnapshot::collect());

            let events = match state.lock() {
                Ok(mut guard) => guard.evaluate(&snapshot, Instant::now()),
                Err(_) => Vec::new(),
            };

            let had_warning = !events.is_empty();
            for ev in events {
                let alert_emotions: &[EmotionSpectrum] = match ev.kind {
                    ProactiveEventKind::BrokenPromise => &[EmotionSpectrum::AngryChastise],
                    ProactiveEventKind::CriticalBattery | ProactiveEventKind::HighTemperature => {
                        &[EmotionSpectrum::AngryChastise, EmotionSpectrum::SystemPanic]
                    }
                    ProactiveEventKind::LowBattery | ProactiveEventKind::RamSpike => &[
                        EmotionSpectrum::AngryChastise,
                        EmotionSpectrum::ProtectiveCare,
                    ],
                };
                let face = kaomoji_engine
                    .lock()
                    .map(|mut eng| eng.pick_from_group(alert_emotions))
                    .unwrap_or_else(|_| ev.mood.as_str());
                let tooltip = format!("{bot_name} [{}]: {}", ev.kind.label(), ev.fallback_message);
                export_waybar_status_with_kaomoji(
                    WaybarState::Warning,
                    &bot_name,
                    face,
                    Some(&tooltip),
                );
                if tx.send(ev).await.is_err() {
                    return;
                }
            }

            if !had_warning {
                if let Some(media) = crate::tools::media::query_mpris_snapshot(None).await {
                    if media.is_playing {
                        let face = kaomoji_engine
                            .lock()
                            .map(|mut eng| eng.pick(EmotionSpectrum::MelodicChill))
                            .unwrap_or(Kaomoji::Music.as_str());
                        crate::tools::desktop::sync_waybar_for_media_status_with_kaomoji(
                            &bot_name,
                            true,
                            Some(face),
                            media.track_info.as_deref(),
                        );
                        was_media_playing = true;
                    } else if was_media_playing {
                        let idle_face = pick_idle_waybar_face(&kaomoji_engine);
                        crate::tools::desktop::sync_waybar_for_media_status_with_kaomoji(
                            &bot_name,
                            false,
                            Some(idle_face),
                            None,
                        );
                        was_media_playing = false;
                    }
                } else if was_media_playing {
                    let idle_face = pick_idle_waybar_face(&kaomoji_engine);
                    crate::tools::desktop::sync_waybar_for_media_status_with_kaomoji(
                        &bot_name,
                        false,
                        Some(idle_face),
                        None,
                    );
                    was_media_playing = false;
                }
            }
        }
    })
}

fn pick_idle_waybar_face(kaomoji_engine: &Arc<Mutex<KaomojiEngine>>) -> &'static str {
    let late_night = is_late_night_hour(chrono::Local::now().hour());
    kaomoji_engine
        .lock()
        .map(|mut eng| {
            if late_night {
                eng.pick(EmotionSpectrum::DrowsyIdle)
            } else {
                eng.current_kaomoji()
            }
        })
        .unwrap_or(Kaomoji::Greet.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::BatteryInfo;

    fn snap_bat(pct: u8, status: &str) -> HardwareSnapshot {
        HardwareSnapshot {
            battery: Some(BatteryInfo {
                capacity: pct,
                status: status.to_string(),
            }),
            ram_used_pct: Some(40),
            ram_used_mib: Some(4000),
            ram_total_mib: Some(10000),
            cpu_temp_c: Some(50.0),
        }
    }

    #[test]
    fn battery_postpone_suppresses_45_minutes_unless_critical() {
        let mut st = HardwareWatcherState::default();
        let t0 = Instant::now();

        // Turun ke 18% -> memicu LowBattery
        let ev1 = st.evaluate(&snap_bat(18, "Discharging"), t0);
        assert_eq!(ev1.len(), 1);
        assert_eq!(ev1[0].kind, ProactiveEventKind::LowBattery);

        // User menunda ("lagi di luar") -> Postponed 45 menit
        assert_eq!(st.observe_user_reply("nanti aja, lagi di luar", t0), Some("postponed_45m"));
        assert!(matches!(st.promise_state, BatteryPromiseState::Postponed { .. }));

        // 10 menit kemudian di 12% -> diam (tidak spam)
        let t10 = t0 + Duration::from_secs(10 * 60);
        assert!(st.evaluate(&snap_bat(12, "Discharging"), t10).is_empty());

        // Jika anjlok ke 6% (< 7%) -> tembus mode Postponed, picu CriticalBattery
        let ev_crit = st.evaluate(&snap_bat(6, "Discharging"), t10);
        assert_eq!(ev_crit.len(), 1);
        assert_eq!(ev_crit[0].kind, ProactiveEventKind::CriticalBattery);

        // Setelah 46 menit berlalu di 14% -> kembali memperingatkan
        let mut st2 = HardwareWatcherState::default();
        st2.evaluate(&snap_bat(19, "Discharging"), t0);
        st2.postpone_battery_warning(t0);
        let t46 = t0 + Duration::from_secs(46 * 60);
        let ev_after = st2.evaluate(&snap_bat(14, "Discharging"), t46);
        assert_eq!(ev_after.len(), 1);
        assert_eq!(ev_after[0].kind, ProactiveEventKind::LowBattery);
    }

    #[test]
    fn broken_promise_triggers_when_battery_drops_3_percent() {
        let mut st = HardwareWatcherState::default();
        let t0 = Instant::now();

        st.evaluate(&snap_bat(18, "Discharging"), t0);
        assert_eq!(
            st.observe_user_reply("iya ini mau di-charge sekarang", t0),
            Some("promised_to_charge")
        );
        assert_eq!(
            st.promise_state,
            BatteryPromiseState::PromisedToCharge { noted_capacity: 18 }
        );

        // Turun 2% (16%) -> belum melanggar ambang >= 3%
        assert!(st.evaluate(&snap_bat(16, "Discharging"), t0).is_empty());

        // Turun 3% (15%) -> BROKEN PROMISE!
        let ev = st.evaluate(&snap_bat(15, "Discharging"), t0);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, ProactiveEventKind::BrokenPromise);
        assert_eq!(ev[0].mood, Kaomoji::Angry);
        assert!(ev[0].system_instruction.contains("[SYSTEM EVENT: USER_BROKE_PROMISE]"));

        // Begitu dicolok (Charging) -> reset state janji
        assert!(st.evaluate(&snap_bat(16, "Charging"), t0).is_empty());
        assert_eq!(st.promise_state, BatteryPromiseState::None);
    }

    #[test]
    fn ram_spike_requires_two_intervals_and_thermal_triggers_above_85c() {
        let mut st = HardwareWatcherState::default();
        let t0 = Instant::now();

        let high_ram = HardwareSnapshot {
            battery: None,
            ram_used_pct: Some(93),
            ram_used_mib: Some(15000),
            ram_total_mib: Some(16000),
            cpu_temp_c: Some(70.0),
        };
        // Interval 1: belum trigger
        assert!(st.evaluate(&high_ram, t0).is_empty());
        // Interval 2: trigger RamSpike
        let ev = st.evaluate(&high_ram, t0);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, ProactiveEventKind::RamSpike);

        // CPU overheat > 85°C
        let hot = HardwareSnapshot {
            battery: None,
            ram_used_pct: Some(50),
            ram_used_mib: Some(8000),
            ram_total_mib: Some(16000),
            cpu_temp_c: Some(87.2),
        };
        let ev_hot = st.evaluate(&hot, t0);
        assert_eq!(ev_hot.len(), 1);
        assert_eq!(ev_hot[0].kind, ProactiveEventKind::HighTemperature);
    }
}
