slint::include_modules!();

use slint::ComponentHandle;

pub fn run() -> anyhow::Result<()> {
    let window = AppWindow::new()?;
    window.window().set_size(slint::LogicalSize::new(1200.0, 760.0));
    window.run()?;
    Ok(())
}
