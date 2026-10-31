fn main() {
    println!("cargo:rerun-if-changed=assets/percepta.ico");
    winresource::WindowsResource::new()
        .set_icon("assets/percepta.ico")
        .compile()
        .expect("embedding the exe icon needs the Windows SDK's resource compiler");
}
