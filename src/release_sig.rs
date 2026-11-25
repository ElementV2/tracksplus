//! Signature des versions publiées (Ed25519, comme minisign / signify) : chaque installeur
//! est accompagné de `<installeur>.sig`, signé avec la clé privée de l'éditeur, gardée hors
//! du dépôt. L'application refuse d'installer toute version dont la signature ne
//! correspond pas à la clé publique ci-dessous : même un compte GitHub piraté ne peut pas
//! faire installer un faux installeur, ni republier un ancien sous un numéro plus récent
//! (nom du fichier et version font partie de ce qui est signé).
//!
//! Partagé avec l'outil de signature (`examples/sign_release.rs`).

/// Clé publique de l'éditeur (hexadécimal).
pub const PUBLIC_KEY_HEX: &str = "7cbb23b23f5f74c87a3c9d97b588615ca3c57a8094ccb98ee99b544dbf8d30ec";

/// Texte signé pour un fichier publié : nom du fichier, version de la release, empreinte
/// SHA-256 (hexadécimal minuscule).
pub fn message(file_name: &str, version: &str, sha256_hex: &str) -> String {
    format!(
        "TracksPlus release v1\n{}\n{}\n{}\n",
        file_name.to_lowercase(),
        version.trim_start_matches(['v', 'V']),
        sha256_hex.to_lowercase()
    )
}

/// Octets d'une chaîne hexadécimale de longueur fixe.
pub fn from_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    if s.len() != 2 * N || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[allow(dead_code)] // (outil de signature, tests)
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Vérifie la signature `sig_hex` d'un fichier publié avec la clé `public_hex`.
pub fn verify_with(
    public_hex: &str,
    file_name: &str,
    version: &str,
    sha256_hex: &str,
    sig_hex: &str,
) -> Result<(), String> {
    use ed25519_dalek::{Signature, VerifyingKey};
    let key = from_hex::<32>(public_hex)
        .and_then(|k| VerifyingKey::from_bytes(&k).ok())
        .ok_or("clé publique de l'éditeur invalide")?;
    let sig = from_hex::<64>(sig_hex).ok_or("signature illisible")?;
    key.verify_strict(
        message(file_name, version, sha256_hex).as_bytes(),
        &Signature::from_bytes(&sig),
    )
    .map_err(|_| "signature de l'éditeur invalide : ce fichier ne vient pas de TracksPlus".into())
}

/// Vérifie un fichier publié avec la clé de l'éditeur.
#[allow(dead_code)]
pub fn verify(
    file_name: &str,
    version: &str,
    sha256_hex: &str,
    sig_hex: &str,
) -> Result<(), String> {
    verify_with(PUBLIC_KEY_HEX, file_name, version, sha256_hex, sig_hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn publisher_key_is_valid() {
        let k = from_hex::<32>(PUBLIC_KEY_HEX).expect("clé publique hexadécimale");
        assert!(ed25519_dalek::VerifyingKey::from_bytes(&k).is_ok());
    }

    /// Une signature ne vaut que pour son fichier, sa version et son contenu.
    #[test]
    fn signature_binds_name_version_and_content() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let public = to_hex(key.verifying_key().as_bytes());
        let sha = "ab".repeat(32);
        let name = "TracksPlus-Setup-0.6.0.exe";
        let sig = to_hex(&key.sign(message(name, "0.6.0", &sha).as_bytes()).to_bytes());
        assert!(verify_with(&public, name, "0.6.0", &sha, &sig).is_ok());
        assert!(
            verify_with(
                &public,
                name,
                "v0.6.0",
                &sha,
                &format!(
                    "{sig}
"
                )
            )
            .is_ok()
        );
        // Ancien installeur republié sous une version plus récente : refusé.
        assert!(verify_with(&public, name, "9.0.0", &sha, &sig).is_err());
        assert!(verify_with(&public, "TracksPlus-Setup-9.0.0.exe", "0.6.0", &sha, &sig).is_err());
        assert!(verify_with(&public, name, "0.6.0", &"cd".repeat(32), &sig).is_err());
        // Autre clé (pas celle de l'éditeur) : refusé.
        assert!(verify(name, "0.6.0", &sha, &sig).is_err());
        assert!(verify_with(&public, name, "0.6.0", &sha, "zz").is_err());
    }
}
