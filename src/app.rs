slint::include_modules!();

use slint::ComponentHandle;

pub fn run() -> anyhow::Result<()> {
    let store = crate::config::ConfigStore::load()?;
    crate::i18n::set_language(store.language());

    let window = AppWindow::new()?;
    crate::i18n::apply_to_slint();
    window.window().set_size(slint::LogicalSize::new(1200.0, 760.0));
    window.run()?;
    Ok(())
}
