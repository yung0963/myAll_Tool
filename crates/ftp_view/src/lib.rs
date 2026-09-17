//! FTP/FTPS connection form and view support.

rust_i18n::i18n!("locales", fallback = "en");

mod form_window;
mod view;

use rust_i18n::t;

pub use form_window::{
    FtpFormPostSaveAction, FtpFormSavedCallback, FtpFormWindow, FtpFormWindowConfig,
    validate_ftp_params,
};
pub use view::FtpView;

/// Keep FTP translations in sync with the application's selected language.
pub fn set_locale(locale: &str) {
    rust_i18n::set_locale(locale);
}

/// Return the localized title used by the standalone FTP form window.
pub fn form_title(is_editing: bool) -> String {
    if is_editing {
        t!("FTP.edit").to_string()
    } else {
        t!("FTP.new").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ftp_locale_resolves_form_and_common_labels() {
        assert_eq!("Connection Name", t!("FTP.name", locale = "en").to_string());
        assert_eq!("連線名稱", t!("FTP.name", locale = "zh-HK").to_string());
        assert_eq!("Refresh", t!("Common.refresh", locale = "en").to_string());
        assert_eq!("無", t!("Common.none", locale = "zh-HK").to_string());
    }
}
