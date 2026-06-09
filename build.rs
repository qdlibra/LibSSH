fn main() {
    // Bundle gettext `.po` translations under `lang/` so Slint `@tr(...)`
    // strings can switch language at runtime via select_bundled_translation.
    // The blueprint's implementation uses English msgids; zh.po translates
    // English to Chinese and en.po remains identity.
    println!("cargo:rerun-if-changed=lang");

    // 注入构建日期，供「关于」对话框显示「发版日期」。clean build 取当天日期；
    // 增量构建沿用上次值（发版走 clean build，故约等于发版日期）。
    let build_date = chrono::Local::now().format("%Y-%m-%d").to_string();
    println!("cargo:rustc-env=LIBSSH_BUILD_DATE={build_date}");

    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .with_style("fluent".into())
            .with_bundled_translations("lang")
            .with_default_translation_context(slint_build::DefaultTranslationContext::None),
    )
    .expect("Slint build failed");

    // Embed the application icon into the Windows executable so it shows up in
    // Explorer, the taskbar and shortcuts. No-op on non-Windows targets.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/LibSSH.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/LibSSH.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed Windows icon: {e}");
        }
    }
}
