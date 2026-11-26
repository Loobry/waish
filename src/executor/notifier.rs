//! Desktop Notification Wrapper via FreeDesktop D-Bus.

use std::time::Duration;

use notify_rust::{Notification, Timeout};

use crate::cli::ui::Kaomoji;
use crate::config::settings::{Settings, DEFAULT_BOT_NAME};

pub fn format_notification_payload(
    bot_name: &str,
    intent: &str,
    succeeded: bool,
) -> (String, String) {
    let name = match bot_name.trim() {
        "" => DEFAULT_BOT_NAME,
        trimmed => trimmed,
    };
    let clean_intent = match intent.trim() {
        "" => "Tugas sistem",
        trimmed => trimmed,
    };

    let summary = format!("[{name}]");
    let body = if succeeded {
        format!("{clean_intent} sudah beres! {}", Kaomoji::Ready.as_str())
    } else {
        format!("{clean_intent} gagal dijalankan {}", Kaomoji::Fail.as_str())
    };

    (summary, body)
}

pub fn notify_if_long_task(
    settings: &Settings,
    intent: &str,
    elapsed: Duration,
    succeeded: bool,
) {
    if !settings.behavior.notify_on_long_task {
        return;
    }

    let threshold = Duration::from_secs(settings.behavior.long_task_threshold_secs);
    if elapsed < threshold {
        return;
    }

    let (summary, body) =
        format_notification_payload(&settings.general.bot_name, intent, succeeded);

    let _ = Notification::new()
        .appname("waish")
        .summary(&summary)
        .body(&body)
        .timeout(Timeout::Milliseconds(5000))
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_notification_summary_and_body_without_graphic_emoji() {
        let (summary, body) = format_notification_payload("alice", "Pasang paket neovim", true);
        assert_eq!(summary, "[alice]");
        assert_eq!(body, "Pasang paket neovim sudah beres! ( •̀ ω •́ )✧");

        let (summary_fail, body_fail) =
            format_notification_payload("alice", "Kompilasi kernel", false);
        assert_eq!(summary_fail, "[alice]");
        assert_eq!(body_fail, "Kompilasi kernel gagal dijalankan (╥﹏╥)");
    }
}
