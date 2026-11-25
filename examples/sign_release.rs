//! Outil de signature des versions publiées (utilisé par `scripts/release.ps1`).
//!
//!   cargo run --release --example sign_release -- keygen <clé>
//!   cargo run --release --example sign_release -- pubkey <clé>
//!   cargo run --release --example sign_release -- sign <clé> <fichier> <version>
//!
//! `<clé>` : fichier de la clé privée (64 caractères hexadécimaux), à garder hors du dépôt
//! et à sauvegarder : sans elle, les versions installées refusent les mises à jour.
//! `sign` écrit `<fichier>.sig` à côté du fichier.

#[path = "../src/release_sig.rs"]
mod release_sig;

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

fn load_key(path: &Path) -> Result<SigningKey, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("clé illisible ({}) : {e}", path.display()))?;
    let seed = release_sig::from_hex::<32>(&text).ok_or("clé privée invalide")?;
    Ok(SigningKey::from_bytes(&seed))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(release_sig::to_hex(&h.finalize()))
}

fn run(args: &[String]) -> Result<(), String> {
    match args {
        [cmd, key] if cmd == "keygen" => {
            let path = PathBuf::from(key);
            if path.exists() {
                return Err(format!("{} existe déjà : jamais écrasée", path.display()));
            }
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).map_err(|e| e.to_string())?;
            if let Some(d) = path.parent() {
                std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
            }
            std::fs::write(&path, release_sig::to_hex(&seed)).map_err(|e| e.to_string())?;
            let k = SigningKey::from_bytes(&seed);
            println!("{}", release_sig::to_hex(k.verifying_key().as_bytes()));
            Ok(())
        }
        [cmd, key] if cmd == "pubkey" => {
            let k = load_key(Path::new(key))?;
            println!("{}", release_sig::to_hex(k.verifying_key().as_bytes()));
            Ok(())
        }
        [cmd, key, file, version] if cmd == "sign" => {
            let k = load_key(Path::new(key))?;
            let public = release_sig::to_hex(k.verifying_key().as_bytes());
            if public != release_sig::PUBLIC_KEY_HEX {
                return Err(
                    "cette clé ne correspond pas à la clé publique de l'application (src/release_sig.rs)"
                        .into(),
                );
            }
            let file = PathBuf::from(file);
            let name = file
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("nom de fichier invalide")?;
            let sha = sha256_file(&file)?;
            let sig = k.sign(release_sig::message(name, version, &sha).as_bytes());
            let sig_hex = release_sig::to_hex(&sig.to_bytes());
            // Relecture immédiate avec la clé publique de l'application.
            release_sig::verify(name, version, &sha, &sig_hex)?;
            let mut out = file.clone().into_os_string();
            out.push(".sig");
            std::fs::write(&out, format!("{sig_hex}\n")).map_err(|e| e.to_string())?;
            println!("{}", PathBuf::from(out).display());
            Ok(())
        }
        _ => Err("usage : keygen <clé> | pubkey <clé> | sign <clé> <fichier> <version>".into()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("sign_release : {e}");
        std::process::exit(1);
    }
}
