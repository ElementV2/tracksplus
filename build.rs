//! Ressources Windows de l'exécutable : icône (Explorateur, barre des tâches) et
//! informations de version (Propriétés → Détails).

fn main() {
    println!("cargo:rerun-if-changed=assets/icon/tracksplus.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon/tracksplus.ico")
        .set("ProductName", "TracksPlus")
        .set("FileDescription", "TracksPlus — multipiste et soundboard")
        .set("OriginalFilename", "TracksPlus.exe")
        .set("LegalCopyright", "© TracksPlus");
    if let Err(e) = res.compile() {
        // Sans compilateur de ressources, l'application reste fonctionnelle (sans icône).
        println!("cargo:warning=Ressources Windows non intégrées : {e}");
    }
}
