fn main() {
    println!("cargo:rerun-if-changed=assets/icon.png");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let icon = image::open("assets/icon.png").expect("load app icon");
    let icon_path =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("orpheus.ico");
    icon.resize_exact(256, 256, image::imageops::FilterType::Lanczos3)
        .save_with_format(&icon_path, image::ImageFormat::Ico)
        .expect("encode Windows app icon");
    winresource::WindowsResource::new()
        .set_icon(icon_path.to_str().unwrap())
        .set("ProductName", "Orpheus")
        .set("FileDescription", "Orpheus mouse polling control")
        .compile()
        .expect("compile Windows app resources");
}
