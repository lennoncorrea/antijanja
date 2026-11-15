fn main() {
    // Interface (arquivos .slint) compilada para Rust.
    slint_build::compile("ui/app.slint").expect("falha ao compilar a interface");
    // Ícone e dados de versão do .exe.
    println!("cargo:rerun-if-changed=assets/antijanja.rc");
    let _ = embed_resource::compile("assets/antijanja.rc", embed_resource::NONE);
}
