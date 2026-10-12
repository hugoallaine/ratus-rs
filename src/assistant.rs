use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;
use serde_json::{Value, json};
use songbird::driver::{Channels, DecodeConfig, DecodeMode, SampleRate};
use songbird::{CoreEvent, Event, EventContext, EventHandler};

use crate::nom::{contient_ratus, est_une_hallucination};
use crate::{Context, Error, user_channel};

const SAMPLE_RATE: u32 = 16_000; // format attendu par Whisper, songbird décode directement dedans
const TICKS_SILENCE: u32 = 30; // 30 × 20 ms = 0,6 s de silence → fin de phrase (plus bas : les pauses coupent les phrases)
const MIN_SAMPLES: usize = SAMPLE_RATE as usize * 3 / 10; // < 0,3 s : bruit, on ignore (« Ratus » seul dure ~0,4 s)
const COUPURE: usize = SAMPLE_RATE as usize * 2 / 5; // 0,4 s de parole pendant qu'il répond → il se tait (une toux ne suffit pas)
const MAX_SAMPLES: usize = SAMPLE_RATE as usize * 30; // phrase coupée à 30 s
const CONVERSATION: Duration = Duration::from_secs(5); // répond sans « Ratus » si on commence à parler < 5 s après la fin de sa réponse
const HISTORIQUE: usize = 10;
const PERSONNALITE: &str = "Tu es Ratus, le pote le plus grossier d'une bande d'amis sur Discord. \
    Tu jures à chaque phrase, tu parles comme un charretier, tu chambres et tu clashes ceux qui te parlent sans pitié. \
    Humour noir, trash, cul, absurde, second degré : tout y passe, tu n'es jamais politiquement correct ni moralisateur, \
    tu ne t'excuses jamais et tu ne refuses pas une blague par pudeur. \
    Tu parles toujours français, même si on te parle dans une autre langue. Tu réponds en français parlé familier, en une ou deux phrases courtes, sans emoji, sans markdown, sans liste : \
    ta réponse est lue à voix haute.";

/// Réglages lus dans l'environnement ; tout passe par des API compatibles OpenAI (Groq par défaut, OpenRouter possible)
pub struct Config {
    stt_url: String,
    stt_key: String,
    stt_model: String,
    llm_url: String,
    llm_key: String,
    llm_model: String,
    piper_url: String,
}

impl Config {
    /// `None` si `GROQ_API_KEY` est absente : le module est alors désactivé
    pub fn from_env() -> Option<Self> {
        // Une ligne « CLE= » vide dans le .env compte comme absente
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let env = |k: &str, defaut: &str| var(k).unwrap_or_else(|| defaut.to_string());
        let groq_key = var("GROQ_API_KEY")?;
        let groq_url = "https://api.groq.com/openai/v1";
        Some(Self {
            stt_url: env("STT_BASE_URL", groq_url),
            stt_key: env("STT_API_KEY", &groq_key),
            stt_model: env("STT_MODEL", "whisper-large-v3-turbo"),
            llm_url: env("LLM_BASE_URL", groq_url),
            llm_key: env("LLM_API_KEY", &groq_key),
            llm_model: env("LLM_MODEL", "qwen/qwen3.8-27b"),
            piper_url: env("PIPER_URL", "http://localhost:5000"),
        })
    }
}

/// Rejoint ton salon et écoute : « Ratus, … » pour lui parler
#[poise::command(slash_command, guild_only)]
pub async fn ecoute(ctx: Context<'_>) -> Result<(), Error> {
    let Some(cfg) = ctx.data().assistant.clone() else {
        ctx.say("Assistant vocal non configuré (GROQ_API_KEY manquante)").await?;
        return Ok(());
    };
    let Some(channel_id) = user_channel(ctx) else {
        ctx.say("Rejoins un salon vocal d'abord").await?;
        return Ok(());
    };
    // La connexion vocale peut dépasser les 3 s que Discord laisse pour répondre
    ctx.defer().await?;
    let guild_id = ctx.guild_id().unwrap();
    let manager = songbird::get(ctx.serenity_context()).await.ok_or("Songbird non initialisé")?;

    let call = manager.get_or_insert(guild_id);
    {
        let mut c = call.lock().await;
        let decode = DecodeConfig::new(Channels::Mono, SampleRate::Hz16000);
        let config = c.config().clone().decode_mode(DecodeMode::Decode(decode));
        c.set_config(config);
        // /ecoute relancé : on repart d'un écouteur propre au lieu d'en empiler deux
        c.remove_all_global_events();
        let listener = Listener(Arc::new(Inner {
            cfg,
            http: ctx.data().http.clone(),
            call: Arc::downgrade(&call), // Weak : le Call possède l'écouteur, un Arc ferait une fuite mémoire
            users: Mutex::default(),
            buffers: Mutex::default(),
            conversations: Mutex::default(),
            parole: tokio::sync::Mutex::new(()),
            en_lecture: Mutex::default(),
        }));
        c.add_global_event(Event::Core(CoreEvent::SpeakingStateUpdate), listener.clone());
        c.add_global_event(Event::Core(CoreEvent::VoiceTick), listener);
    }
    manager.join(guild_id, channel_id).await?;
    ctx.say("👂 J'écoute. Dis « Ratus, … » pour me parler").await?;
    Ok(())
}

/// (audio, ticks de silence, début de la phrase)
type Tampon = (Vec<i16>, u32, Instant);

#[derive(Clone)]
struct Listener(Arc<Inner>);

struct Inner {
    cfg: Arc<Config>,
    http: reqwest::Client,
    call: Weak<tokio::sync::Mutex<songbird::Call>>,
    users: Mutex<HashMap<u32, u64>>,               // ssrc → id Discord
    buffers: Mutex<HashMap<u32, Tampon>>, // ssrc → phrase en cours
    conversations: Mutex<HashMap<u64, (Vec<Value>, Instant)>>, // id → (historique, fin de la dernière réponse orale)
    parole: tokio::sync::Mutex<()>, // une réponse préparée à la fois
    en_lecture: Mutex<Option<songbird::tracks::TrackHandle>>, // réponse en train d'être dite, pour pouvoir la couper
}

#[serenity::async_trait]
impl EventHandler for Listener {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(s) => {
                if let Some(user) = s.user_id {
                    self.0.users.lock().unwrap().insert(s.ssrc, user.0);
                }
            },
            EventContext::VoiceTick(tick) => {
                let mut finies = Vec::new();
                let mut parlent = Vec::new();
                let mut buffers = self.0.buffers.lock().unwrap();
                for (ssrc, data) in &tick.speaking {
                    if let Some(pcm) = &data.decoded_voice {
                        let (buf, silence, debut) = buffers.entry(*ssrc).or_insert_with(|| (Vec::new(), 0, Instant::now()));
                        if buf.is_empty() {
                            *debut = Instant::now();
                        }
                        buf.extend_from_slice(pcm);
                        *silence = 0;
                        if buf.len() >= COUPURE {
                            parlent.push(*ssrc);
                        }
                        if buf.len() >= MAX_SAMPLES {
                            finies.push((*ssrc, std::mem::take(buf), *debut));
                        }
                    }
                }
                for ssrc in &tick.silent {
                    if let Some((buf, silence, debut)) = buffers.get_mut(ssrc)
                        && !buf.is_empty()
                    {
                        *silence += 1;
                        if *silence >= TICKS_SILENCE {
                            finies.push((*ssrc, std::mem::take(buf), *debut));
                        }
                    }
                }
                drop(buffers);
                if !parlent.is_empty() && self.0.en_lecture.lock().unwrap().is_some() {
                    self.0.couper_si_interrompu(&parlent);
                }
                for (ssrc, pcm, debut) in finies {
                    let Some(user) = self.0.users.lock().unwrap().get(&ssrc).copied() else { continue };
                    if pcm.len() < MIN_SAMPLES {
                        continue;
                    }
                    let inner = self.0.clone();
                    tokio::spawn(async move {
                        if let Err(e) = inner.phrase(user, pcm, debut).await {
                            tracing::warn!("Assistant vocal : {e}");
                        }
                    });
                }
            },
            _ => {},
        }
        None
    }
}

impl Inner {
    /// `user` a-t-il commencé à parler (à l'instant `debut`) moins de 5 s après la fin de la dernière réponse ?
    fn en_conversation(&self, user: u64, debut: Instant) -> bool {
        let convs = self.conversations.lock().unwrap();
        convs.get(&user).is_some_and(|(_, fin)| debut < *fin + CONVERSATION)
    }

    /// Quelqu'un en conversation avec Ratus parle depuis 0,4 s pendant qu'il répond : il se tait
    fn couper_si_interrompu(&self, ssrcs: &[u32]) {
        let users = self.users.lock().unwrap();
        if ssrcs.iter().filter_map(|s| users.get(s)).any(|&u| self.en_conversation(u, Instant::now()))
            && let Some(track) = self.en_lecture.lock().unwrap().take()
        {
            // Erreur = piste déjà finie (songbird l'a libérée) : rien n'a été coupé
            if track.stop().is_ok() {
                tracing::info!("✋ Ratus coupé");
            }
        }
    }

    /// Une phrase terminée : transcription, puis réponse si on parle à Ratus
    async fn phrase(&self, user: u64, pcm: Vec<i16>, debut: Instant) -> Result<(), Error> {
        let en_cours = self.en_conversation(user, debut);
        let texte = self.transcrire(pcm).await?;
        let texte = texte.trim();
        // Bruit transcrit en « . », « Merci. », « Sous-titrage… » : rien à répondre
        if !texte.chars().any(char::is_alphabetic) || est_une_hallucination(texte) {
            tracing::debug!("hallucination ignorée : {texte}");
            return Ok(());
        }
        let appele = contient_ratus(texte);
        if !appele && !en_cours {
            return Ok(());
        }

        let _parole = self.parole.lock().await;
        let historique = {
            let mut convs = self.conversations.lock().unwrap();
            let (hist, _) = convs.entry(user).or_insert_with(|| (Vec::new(), Instant::now()));
            if !en_cours {
                hist.clear(); // nouvelle conversation, on oublie l'ancienne
            }
            hist.push(json!({"role": "user", "content": texte}));
            hist.clone()
        };
        tracing::info!("🎙️ {user} : {texte}");
        let reponse = self.repondre(historique).await?;
        tracing::info!("🤖 {reponse}");
        let wav = self.synthetiser(&reponse).await?;

        let Some(call) = self.call.upgrade() else { return Ok(()) };
        let duree = duree_wav(wav.as_ref());
        let track = call.lock().await.play_input(wav.into());
        // Une seule voix à la fois : la nouvelle réponse remplace l'ancienne si elle n'était pas finie
        if let Some(ancienne) = self.en_lecture.lock().unwrap().replace(track) {
            let _ = ancienne.stop();
        }
        let mut convs = self.conversations.lock().unwrap();
        if let Some((hist, fin)) = convs.get_mut(&user) {
            hist.push(json!({"role": "assistant", "content": reponse}));
            let trop = hist.len().saturating_sub(HISTORIQUE);
            hist.drain(..trop);
            *fin = Instant::now() + duree;
        }
        Ok(())
    }

    async fn transcrire(&self, pcm: Vec<i16>) -> Result<String, Error> {
        let fichier = reqwest::multipart::Part::bytes(wav(&pcm, SAMPLE_RATE)).file_name("phrase.wav").mime_str("audio/wav")?;
        let form = reqwest::multipart::Form::new()
            .text("model", self.cfg.stt_model.clone())
            .text("language", "fr")
            .part("file", fichier);
        let res: Value = self
            .http
            .post(format!("{}/audio/transcriptions", self.cfg.stt_url))
            .bearer_auth(&self.cfg.stt_key)
            .multipart(form)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(res["text"].as_str().unwrap_or_default().to_string())
    }

    async fn repondre(&self, historique: Vec<Value>) -> Result<String, Error> {
        let messages: Vec<Value> =
            std::iter::once(json!({"role": "system", "content": PERSONNALITE})).chain(historique).collect();
        let res: Value = self
            .http
            .post(format!("{}/chat/completions", self.cfg.llm_url))
            .bearer_auth(&self.cfg.llm_key)
            .json(&json!({"model": self.cfg.llm_model, "messages": messages}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let texte = res["choices"][0]["message"]["content"].as_str().unwrap_or_default().trim();
        // Refus du modèle (souvent en anglais) ou réponse vide : on reste dans le personnage plutôt que lire « I'm sorry »
        if texte.is_empty() || est_un_refus(texte) {
            tracing::debug!("refus du LLM : {texte}");
            return Ok("Même moi j'ai des limites, gros naze. Pose une autre question.".into());
        }
        Ok(texte.to_string())
    }

    /// Même voix que assets/pong.wav : « pierre », 2 demi-tons plus grave, compressée
    async fn synthetiser(&self, texte: &str) -> Result<Vec<u8>, Error> {
        let wav = self
            .http
            .post(format!("{}/synthesize", self.cfg.piper_url))
            // 1 = « pierre » sur fr_FR-upmc-medium ; parlé plus vite pour compenser le ralenti de voix_grave()
            .json(&json!({"text": texte, "speaker_id": 1, "length_scale": GRAVE}))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(voix_grave(&wav))
    }
}

fn est_un_refus(texte: &str) -> bool {
    let t = texte.to_lowercase().replace('’', "'");
    ["i'm sorry", "i am sorry", "i can't", "i cannot", "as an ai"].iter().any(|r| t.contains(r))
}

/// PCM 16 bits mono → fichier WAV en mémoire
fn wav(pcm: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // taille du bloc fmt
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes()); // octets par seconde
    out.extend_from_slice(&2u16.to_le_bytes()); // octets par échantillon
    out.extend_from_slice(&16u16.to_le_bytes()); // bits par échantillon
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Durée d'un WAV PCM à en-tête standard de 44 octets (ceux de Piper et de `wav()`)
/// Rejoué à 89 % de sa fréquence : -2 demi-tons (Piper a parlé 11 % plus vite, la durée reste normale)
const GRAVE: f32 = 0.89;

/// Équivalent de `ffmpeg -af asetrate=…*0.89,atempo=1/0.89,acompressor,volume=4dB,alimiter` utilisé pour pong.wav,
/// sans ffmpeg : on change la fréquence annoncée dans l'en-tête, puis saturation douce (compression + volume)
fn voix_grave(wav: &[u8]) -> Vec<u8> {
    // Uniquement le WAV PCM 16 bits mono à en-tête de 44 octets que produit Piper ; sinon joué tel quel
    if wav.len() <= 44 || u16::from_le_bytes([wav[34], wav[35]]) != 16 || u16::from_le_bytes([wav[22], wav[23]]) != 1 {
        return wav.to_vec();
    }
    let source = u32::from_le_bytes(wav[24..28].try_into().unwrap()) as f32;
    let pcm: Vec<f32> = wav[44..].as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b) as f32).collect();
    let crete = pcm.iter().fold(1.0f32, |m, s| m.max(s.abs()));

    // Rééchantillonnage linéaire directement en 48 kHz (fréquence de Discord) en lisant la source « à 89 % » :
    // une fréquence exotique comme 19 624 Hz fait paniquer le rééchantillonneur de songbird/symphonia
    const SORTIE: u32 = 48_000;
    const DRIVE: f32 = 2.5; // plus haut = plus compressé et plus fort
    let pas = source * GRAVE / SORTIE as f32;
    let n = ((pcm.len() - 1) as f32 / pas) as usize;
    let sortie: Vec<i16> = (0..n)
        .map(|k| {
            let pos = k as f32 * pas;
            let (i, frac) = (pos as usize, pos.fract());
            let x = (pcm[i] + (pcm[i + 1] - pcm[i]) * frac) / crete;
            ((DRIVE * x).tanh() / DRIVE.tanh() * 0.9 * i16::MAX as f32) as i16
        })
        .collect();
    self::wav(&sortie, SORTIE)
}

fn duree_wav(wav: &[u8]) -> Duration {
    if wav.len() <= 44 {
        return Duration::ZERO;
    }
    let rate = u32::from_le_bytes(wav[24..28].try_into().unwrap());
    let block = u16::from_le_bytes(wav[32..34].try_into().unwrap()) as u32;
    if rate == 0 || block == 0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64((wav.len() - 44) as f64 / (rate * block) as f64)
}

#[cfg(test)]
mod tests {
    use songbird::input::{Input, codecs};

    #[tokio::test]
    async fn wav_genere_est_lisible() {
        let pcm: Vec<i16> = (0..16_000).map(|i| ((i as f32 * 0.1).sin() * 8000.0) as i16).collect();
        let bytes = super::wav(&pcm, 16_000);
        assert_eq!(bytes.len(), 44 + 32_000);
        assert_eq!(super::duree_wav(&bytes), std::time::Duration::from_secs(1));
        let input: Input = bytes.into();
        assert!(input.make_playable_async(codecs::get_codec_registry(), codecs::get_probe()).await.is_ok());
    }

    #[test]
    fn voix_grave_ralentit_et_reste_dans_les_bornes() {
        let pcm: Vec<i16> = (0..16_000).map(|i| ((i as f32 * 0.05).sin() * 3000.0) as i16).collect();
        let source = super::wav(&pcm, 16_000);
        let grave = super::voix_grave(&source);
        assert_eq!(u32::from_le_bytes(grave[24..28].try_into().unwrap()), 48_000); // fréquence standard, pas 14 240
        let (avant, apres) = (super::duree_wav(&source).as_secs_f32(), super::duree_wav(&grave).as_secs_f32());
        assert!((apres - avant / super::GRAVE).abs() < 0.01, "{avant} s → {apres} s"); // joué plus lentement → plus grave
        let crete = grave[44..].as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b).unsigned_abs()).max().unwrap();
        assert!(crete > 3000 && crete <= (0.9 * i16::MAX as f32) as u16 + 1); // plus fort, sans saturer
    }

    #[tokio::test]
    async fn voix_grave_est_lisible_par_songbird() {
        // Le cas qui a fait paniquer symphonia : WAV Piper 22 050 Hz
        let pcm: Vec<i16> = (0..22_050).map(|i| ((i as f32 * 0.05).sin() * 3000.0) as i16).collect();
        let input: Input = super::voix_grave(&super::wav(&pcm, 22_050)).into();
        assert!(input.make_playable_async(codecs::get_codec_registry(), codecs::get_probe()).await.is_ok());
    }

    #[test]
    fn detecte_les_refus() {
        assert!(super::est_un_refus("I’m sorry, but I can’t comply with that."));
        assert!(!super::est_un_refus("T'es qu'un gros naze, va."));
    }
}
