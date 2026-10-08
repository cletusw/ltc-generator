use std::sync::Mutex;

#[derive(Default)]
pub(crate) struct StatusText {
    ntp: String,
    audio: Option<String>,
}

impl StatusText {
    pub(crate) fn new(ntp: impl Into<String>) -> Self {
        Self {
            ntp: ntp.into(),
            audio: None,
        }
    }

    pub(crate) fn combined(&self) -> String {
        match &self.audio {
            Some(audio) => format!("{} | Audio output: {audio}", self.ntp),
            None => self.ntp.clone(),
        }
    }
}

pub(crate) fn set_ntp_status(status_text: &Mutex<StatusText>, message: &str) {
    if let Ok(mut status) = status_text.lock() {
        status.ntp = message.to_owned();
    }
}

pub(crate) fn set_audio_status(status_text: &Mutex<StatusText>, message: &str) {
    if let Ok(mut status) = status_text.lock() {
        status.audio = Some(message.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_sync_and_audio_enable_status_reads_as_expected() {
        let status_text = Mutex::new(StatusText::new("NTP: Syncing"));

        set_ntp_status(&status_text, "NTP: Locked");
        set_audio_status(&status_text, "Audio output stream is playing");

        assert_eq!(
            status_text.lock().unwrap().combined(),
            "NTP: Locked | Audio output: Audio output stream is playing"
        );
    }
}
