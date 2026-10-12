//! Reconnaît « Ratus » dans une transcription, y compris quand Whisper le déforme.

/// « Ratus » et ce que Whisper en fait (Lératus, Rattuuse, Ratu, Diratu, Atus, rat us…),
/// sans attraper « rat », « tu », « gratuit », « rature »…
pub fn contient_ratus(texte: &str) -> bool {
    let mots = mots(texte);
    let mot_ok = |m: &String| m.starts_with("atus") || m.starts_with("ratous") || ratu_isole(m);
    // Deux mots collés (« rat us ») : seulement « ratus » complet, sinon « rat tu » passerait
    let paire_ok = |w: &[String]| ["ratus", "rattus"].iter().any(|v| w.concat().contains(v));
    mots.iter().any(mot_ok) || mots.windows(2).any(paire_ok)
}

/// Phrases que Whisper invente sur du bruit, de l'écho ou un silence (héritage de ses sous-titres d'entraînement)
pub fn est_une_hallucination(texte: &str) -> bool {
    let t = mots(texte).join(" ");
    ["merci", "merci beaucoup", "merci d avoir regarde"].contains(&t.as_str())
        || ["sous titr", "radio canada", "amara org"].iter().any(|h| t.contains(h))
}

/// Minuscules, sans accents ni ponctuation, découpé en mots
fn mots(texte: &str) -> Vec<String> {
    texte
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'à' | 'â' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'ô' | 'ö' => 'o',
            'û' | 'ü' | 'ù' => 'u',
            c if c.is_alphanumeric() => c,
            _ => ' ',
        })
        .collect::<String>()
        .split_whitespace()
        .map(String::from)
        .collect()
}

/// « ratu » / « rattu » / « retu » (« Diretu » pour « Dis Ratus ») suivi de rien ou d'autre chose que i/r (exclut grat-ui-t, rat-ur-e)
fn ratu_isole(mot: &str) -> bool {
    ["rattu", "ratu", "retu"].iter().any(|motif| {
        mot.match_indices(motif).any(|(i, _)| !matches!(mot[i + motif.len()..].chars().next(), Some('i' | 'r')))
    })
}

#[cfg(test)]
mod tests {
    use super::{contient_ratus, est_une_hallucination};

    #[test]
    fn reconnait_ratus_et_ses_deformations() {
        for oui in [
            "Ratus, raconte une blague",
            "Salut, moi c'est Lératus",
            "Hé Rattus !",
            "rat us tu dors",
            "RATUSSE",
            "Atus, raconte-mongue-blague",
            "Rattuuse, c'est quoi",
            "Ratu c'est quoi",
            "Diratu, on mange quoi",
            "Diretu's, on mange quoi",
        ] {
            assert!(contient_ratus(oui), "{oui}");
        }
        for non in ["Le rat est dans la cuisine", "T'as vu le match ?", "rat tu vois", "saturation", "c'est gratuit", "une rature", ""] {
            assert!(!contient_ratus(non), "{non}");
        }
    }

    #[test]
    fn ecarte_les_hallucinations_de_whisper() {
        for h in ["Merci.", "Sous-titrage Société Radio-Canada", "Sous-titres réalisés par la communauté d'Amara.org", "Merci d'avoir regardé !"] {
            assert!(est_une_hallucination(h), "{h}");
        }
        for vrai in ["Merci Ratus, t'es un pote", "Ratus, raconte une blague", "Oui"] {
            assert!(!est_une_hallucination(vrai), "{vrai}");
        }
    }
}
