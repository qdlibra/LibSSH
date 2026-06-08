use std::sync::atomic::{AtomicU8, Ordering};

const ZH: u8 = 0;
const EN: u8 = 1;

static LANG: AtomicU8 = AtomicU8::new(ZH);

pub fn set_language(code: &str) {
    let en = code.eq_ignore_ascii_case("en");
    LANG.store(if en { EN } else { ZH }, Ordering::Relaxed);
    apply_to_slint();
}

pub fn apply_to_slint() {
    let lang = if is_en() { "en" } else { "zh" };
    let _ = slint::select_bundled_translation(lang);
}

pub fn current_code() -> &'static str {
    if is_en() {
        "en"
    } else {
        "zh"
    }
}

pub fn is_en() -> bool {
    LANG.load(Ordering::Relaxed) == EN
}

pub fn t(zh: &'static str, en: &'static str) -> &'static str {
    if is_en() {
        en
    } else {
        zh
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn set_language_accepts_en_case_insensitively() {
        let _guard = test_lock();
        set_language("EN");

        assert!(is_en());
        assert_eq!(current_code(), "en");
        assert_eq!(t("中文", "English"), "English");
    }

    #[test]
    fn unknown_language_defaults_to_zh() {
        let _guard = test_lock();
        set_language("fr");

        assert!(!is_en());
        assert_eq!(current_code(), "zh");
        assert_eq!(t("中文", "English"), "中文");
    }
}
