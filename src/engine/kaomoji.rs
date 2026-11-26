//! Dynamic Emotional Engine & Kaomoji Matrix.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const RECENT_HISTORY_CAPACITY: usize = 5;

static SEED_COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

/// Companion emotional spectrum categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmotionSpectrum {
    TsunderePout,
    SmugSuperior,
    AngryChastise,
    FlusteredBlush,
    SystemPanic,
    MelancholyFail,
    MelodicChill,
    DrowsyIdle,
    CuriousHacker,
    ProtectiveCare,
}

const TSUNDERE_POUT_PALETTE: &[&str] = &[
    "( ￣^￣ )",
    "( `ε´ )",
    "( ˘︹˘ )",
    "( `ー´ )",
    "( ￣ヘ￣)",
    "(¬_¬ )",
    "( ˘^˘ )",
    "( ￣ω￣;)",
    "( `д´ )",
    "( ¯ヘ¯ )",
    "( ˘_˘ )",
];

const SMUG_SUPERIOR_PALETTE: &[&str] = &[
    "( •̀ ω •́ )✧",
    "( ˘ω˘ )",
    "( ¬‿¬ )",
    "( ‾́ ◡ ‾́ )",
    "( ´罒`*)",
    "( ✧ω✧ )",
    "( ¯▽¯ )",
    "( ˘▾˘ )",
    "( ≖‿≖ )",
    "( ¯ω¯ )",
    "( ˙꒳˙ )",
];

const ANGRY_CHASTISE_PALETTE: &[&str] = &[
    "( •ˋ _ ˊ•)",
    "( `Д´ )ﾉ",
    "(눈_눈)",
    "( º言º )",
    "( ╬◣д◢)",
    "( #`Д´)",
    "( `皿´ )",
    "( ಠ_ಠ )",
    "( ▀̿Ĺ̯▀̿ ̿)",
    "( `•_•´ )",
];

const FLUSTERED_BLUSH_PALETTE: &[&str] = &[
    "(⁄ ⁄•⁄ω⁄•⁄ ⁄)",
    "( > ﹏ < )",
    "(*/ω＼*)",
    "(⁄ ⁄>⁄ ▽ ⁄<⁄ ⁄)",
    "( •́ ̯•̀ )",
    "(〃▽〃)",
    "(*/_＼)",
    "( ˘///˘ )",
    "( >///< )",
];

const SYSTEM_PANIC_PALETTE: &[&str] = &[
    "( ；´Д｀)",
    "( ; ° Д °)",
    "( º □ º )",
    "( ᵒ̌▱ᵒ̌ )",
    "( > o < )",
    "( ; ˘•ω•˘ )",
    "( º д º ;)",
    "( ° △ ° ;)",
];

const MELANCHOLY_FAIL_PALETTE: &[&str] = &[
    "(╥﹏╥)",
    "( ; _ ; )",
    "( ´•︵•` )",
    "( . _ . )",
    "( ◞‸◟ )",
    "( ╯︵╰ )",
    "( ´;ω;` )",
    "( ಥ_ಥ )",
];

const MELODIC_CHILL_PALETTE: &[&str] = &[
    "( ˘ ▽ ˘ ) ♫",
    "( ˘͈ ᵕ ˘͈ )",
    "( ＾◡＾)",
    "( ´ ▽ ` )",
    "( ˘ω˘ )♬",
    "( ˙ᵕ˙ )",
    "( ˘⌣˘ )",
];

const DROWSY_IDLE_PALETTE: &[&str] = &[
    "( - _ - ) zZz",
    "( ´ д ` )",
    "( ¯ . ¯ )",
    "( ˘o˘ )",
    "( ∪｡∪)｡｡zZ",
    "( -ω- )",
    "( ˘~˘ )",
];

const CURIOUS_HACKER_PALETTE: &[&str] = &[
    "( ✧Д✧ )",
    "( •̀ ᴗ •́ )و",
    "( ᐛ )و",
    "( *°▽°*)",
    "( ⚆ᗝ⚆ )",
    "( `･ω･´ )",
];

const PROTECTIVE_CARE_PALETTE: &[&str] = &[
    "( •̀ - •́ )",
    "( ˘•ω•˘ )",
    "( ´•_•` )",
    "( `･д･´ )",
    "( ¯ . ¯ ;)",
];

impl EmotionSpectrum {
    pub const ALL: [EmotionSpectrum; 10] = [
        EmotionSpectrum::TsunderePout,
        EmotionSpectrum::SmugSuperior,
        EmotionSpectrum::AngryChastise,
        EmotionSpectrum::FlusteredBlush,
        EmotionSpectrum::SystemPanic,
        EmotionSpectrum::MelancholyFail,
        EmotionSpectrum::MelodicChill,
        EmotionSpectrum::DrowsyIdle,
        EmotionSpectrum::CuriousHacker,
        EmotionSpectrum::ProtectiveCare,
    ];

    pub fn palette(self) -> &'static [&'static str] {
        match self {
            EmotionSpectrum::TsunderePout => TSUNDERE_POUT_PALETTE,
            EmotionSpectrum::SmugSuperior => SMUG_SUPERIOR_PALETTE,
            EmotionSpectrum::AngryChastise => ANGRY_CHASTISE_PALETTE,
            EmotionSpectrum::FlusteredBlush => FLUSTERED_BLUSH_PALETTE,
            EmotionSpectrum::SystemPanic => SYSTEM_PANIC_PALETTE,
            EmotionSpectrum::MelancholyFail => MELANCHOLY_FAIL_PALETTE,
            EmotionSpectrum::MelodicChill => MELODIC_CHILL_PALETTE,
            EmotionSpectrum::DrowsyIdle => DROWSY_IDLE_PALETTE,
            EmotionSpectrum::CuriousHacker => CURIOUS_HACKER_PALETTE,
            EmotionSpectrum::ProtectiveCare => PROTECTIVE_CARE_PALETTE,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EmotionSpectrum::TsunderePout => "TsunderePout (cemberut/gengsi)",
            EmotionSpectrum::SmugSuperior => "SmugSuperior (bangga/sukses)",
            EmotionSpectrum::AngryChastise => "AngryChastise (tegas/memarahi)",
            EmotionSpectrum::FlusteredBlush => "FlusteredBlush (malu/dipuji)",
            EmotionSpectrum::SystemPanic => "SystemPanic (panik/error kritis)",
            EmotionSpectrum::MelancholyFail => "MelancholyFail (sedih/gagal)",
            EmotionSpectrum::MelodicChill => "MelodicChill (santai/musik)",
            EmotionSpectrum::DrowsyIdle => "DrowsyIdle (mengantuk/malam/idle)",
            EmotionSpectrum::CuriousHacker => "CuriousHacker (antusias/oprek)",
            EmotionSpectrum::ProtectiveCare => "ProtectiveCare (peduli/mengingatkan)",
        }
    }
}

/// Dynamic kaomoji selector backed by a 5-entry anti-repetition ring buffer.
#[derive(Debug, Clone)]
pub struct KaomojiEngine {
    pub recent_history: VecDeque<&'static str>,
    rng_state: u64,
    last_emotion: EmotionSpectrum,
    last_kaomoji: &'static str,
}

impl Default for KaomojiEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl KaomojiEngine {
    pub fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0xCAFE_BABE_DEAD_BEEF);
        let salt = SEED_COUNTER.fetch_add(0xBF58_476D_1CE4_E5B9, Ordering::Relaxed);
        let pid = u64::from(std::process::id()).rotate_left(17);
        let seed = (nanos ^ salt ^ pid).max(1);
        Self::with_seed(seed)
    }

    pub fn with_seed(seed: u64) -> Self {
        Self {
            recent_history: VecDeque::with_capacity(RECENT_HISTORY_CAPACITY + 1),
            rng_state: if seed == 0 {
                0xA076_1D64_78BD_642F
            } else {
                seed
            },
            last_emotion: EmotionSpectrum::SmugSuperior,
            last_kaomoji: SMUG_SUPERIOR_PALETTE[0],
        }
    }

    pub fn pick(&mut self, emotion: EmotionSpectrum) -> &'static str {
        let palette = emotion.palette();
        let candidates: Vec<&'static str> = palette
            .iter()
            .copied()
            .filter(|k| !self.recent_history.contains(k))
            .collect();

        let chosen = if !candidates.is_empty() {
            let idx = self.next_usize(candidates.len());
            candidates[idx]
        } else {
            let idx = self.next_usize(palette.len());
            palette[idx]
        };

        self.record_pick(emotion, chosen);
        chosen
    }

    pub fn pick_from_group(&mut self, emotions: &[EmotionSpectrum]) -> &'static str {
        if emotions.is_empty() {
            return self.pick(EmotionSpectrum::SmugSuperior);
        }
        let chosen_emotion = if emotions.len() == 1 {
            emotions[0]
        } else {
            let idx = self.next_usize(emotions.len());
            emotions[idx]
        };
        self.pick(chosen_emotion)
    }

    pub fn current_kaomoji(&self) -> &'static str {
        self.last_kaomoji
    }

    pub fn current_emotion(&self) -> EmotionSpectrum {
        self.last_emotion
    }

    fn record_pick(&mut self, emotion: EmotionSpectrum, kaomoji: &'static str) {
        self.recent_history.push_back(kaomoji);
        while self.recent_history.len() > RECENT_HISTORY_CAPACITY {
            self.recent_history.pop_front();
        }
        self.last_emotion = emotion;
        self.last_kaomoji = kaomoji;
    }

    /// XorShift64* PRNG.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.rng_state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng_state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_usize(&mut self, upper_exclusive: usize) -> usize {
        if upper_exclusive <= 1 {
            return 0;
        }
        (self.next_u64() as usize) % upper_exclusive
    }
}

pub fn is_sweet_or_compliment_input(input: &str) -> bool {
    let lower = input.trim().to_ascii_lowercase();
    if lower.is_empty() || lower.starts_with('/') {
        return false;
    }

    const SWEET_PHRASES: &[&str] = &[
        "makasih",
        "terima kasih",
        "trims",
        "tq",
        "thanks",
        "thank you",
        "arigatou",
        "arigato",
        "sankyu",
        "pintar",
        "pinter",
        "hebat",
        "keren banget",
        "jago banget",
        "lucu",
        "imut",
        "cantik",
        "manis",
        "kawaii",
        "sayang",
        "good job",
        "good girl",
        "nice job",
        "mantap",
        "terbaik",
        "best companion",
        "love you",
        "lopyu",
        "baik banget",
        "gemes",
    ];

    SWEET_PHRASES.iter().any(|phrase| lower.contains(phrase))
}

pub fn is_late_night_hour(hour: u32) -> bool {
    !(5..23).contains(&hour)
}

pub fn render_system_prompt_matrix() -> String {
    let mut lines = Vec::with_capacity(EmotionSpectrum::ALL.len());
    for spectrum in EmotionSpectrum::ALL {
        let preview = spectrum
            .palette()
            .iter()
            .take(5)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("- {}: {}", spectrum.label(), preview));
    }
    lines.join("\n")
}

#[cfg(test)]
pub fn total_kaomoji_count() -> usize {
    EmotionSpectrum::ALL
        .iter()
        .map(|e| e.palette().len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ui::display_width;
    use crate::engine::client::sanitize_terminal_text;
    use std::collections::HashSet;

    #[test]
    fn palette_contains_at_least_80_valid_pure_text_kaomoji_without_graphic_emoji() {
        assert!(
            total_kaomoji_count() >= 80,
            "expected >= 80 kaomoji across 10 spectra, got {}",
            total_kaomoji_count()
        );

        for spectrum in EmotionSpectrum::ALL {
            let pal = spectrum.palette();
            assert!(
                pal.len() >= 5,
                "spectrum {:?} should have at least 5 entries",
                spectrum
            );
            for &k in pal {
                assert!(!k.trim().is_empty());
                assert!(
                    !k.contains('\n') && !k.contains('\r') && !k.contains('\x1b'),
                    "kaomoji must not contain control/ANSI escapes: {k:?}"
                );
                assert!(
                    display_width(k) > 0,
                    "kaomoji must have positive display width: {k:?}"
                );
                assert_eq!(
                    sanitize_terminal_text(k),
                    k,
                    "sanitizer must preserve pure-text kaomoji {k:?} intact"
                );
            }
        }
    }

    #[test]
    fn ring_buffer_prevents_consecutive_repetition_and_caps_history_at_5() {
        let mut engine = KaomojiEngine::with_seed(42);

        // Untuk spektrum dengan >= 6 elemen, 6 pick beruntun tidak pernah mengulang item
        // yang sedang ada di recent_history (kapasitas 5), sehingga 2 pick berturut-turut mustahil sama.
        for spectrum in [
            EmotionSpectrum::TsunderePout,
            EmotionSpectrum::SmugSuperior,
            EmotionSpectrum::AngryChastise,
            EmotionSpectrum::FlusteredBlush,
            EmotionSpectrum::SystemPanic,
            EmotionSpectrum::MelancholyFail,
            EmotionSpectrum::MelodicChill,
            EmotionSpectrum::DrowsyIdle,
            EmotionSpectrum::CuriousHacker,
        ] {
            let mut local_eng = KaomojiEngine::with_seed(1337);
            let mut seen_window = HashSet::new();
            let mut prev = "";
            for step in 0..12 {
                let picked = local_eng.pick(spectrum);
                assert!(
                    local_eng.recent_history.len() <= RECENT_HISTORY_CAPACITY,
                    "recent_history exceeded capacity 5"
                );
                assert_ne!(
                    picked, prev,
                    "consecutive duplicate detected for {:?} at step {step}: {picked}",
                    spectrum
                );
                if step < 6 {
                    assert!(
                        seen_window.insert(picked),
                        "duplicate within first 6 picks for {:?}: {picked}",
                        spectrum
                    );
                }
                prev = picked;
            }
        }

        // Untuk ProtectiveCare (tepat 5 elemen), 5 pick pertama seluruhnya unik,
        // lalu pick ke-6 melakukan fallback aman tanpa panic.
        let mut first_five = HashSet::new();
        for _ in 0..5 {
            first_five.insert(engine.pick(EmotionSpectrum::ProtectiveCare));
        }
        assert_eq!(first_five.len(), 5);
        let fallback_pick = engine.pick(EmotionSpectrum::ProtectiveCare);
        assert!(EmotionSpectrum::ProtectiveCare
            .palette()
            .contains(&fallback_pick));
        assert_eq!(engine.recent_history.len(), RECENT_HISTORY_CAPACITY);
    }

    #[test]
    fn detects_sweet_compliments_and_late_night_hours() {
        assert!(is_sweet_or_compliment_input("makasih banyak alice!"));
        assert!(is_sweet_or_compliment_input("wah kamu pintar banget"));
        assert!(is_sweet_or_compliment_input("good job, keren banget"));
        assert!(!is_sweet_or_compliment_input("install neovim dong"));
        assert!(!is_sweet_or_compliment_input("/telemetry"));

        assert!(is_late_night_hour(23));
        assert!(is_late_night_hour(0));
        assert!(is_late_night_hour(4));
        assert!(!is_late_night_hour(5));
        assert!(!is_late_night_hour(14));
    }
}
