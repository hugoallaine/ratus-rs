use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use poise::serenity_prelude as serenity;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serenity::{ChannelId, GuildId, MessageId};
use songbird::input::metadata::YoutubeDlOutput;
use songbird::input::{AuxMetadata, HlsRequest, HttpRequest, Input, YoutubeDl};
use songbird::tracks::{PlayMode, Track};
use songbird::{Event, EventContext, EventHandler, Songbird, TrackEvent};
use tokio::sync::Notify;

use crate::{Context, Data, Error, user_channel};

const VOLUME: f32 = 0.5; // musique à 50 % par défaut : YouTube est mixé fort, la voix de Ratus reste à 100 %
const RAFRAICHISSEMENT: Duration = Duration::from_secs(10); // Discord limite les modifications de message
const A_SUIVRE: usize = 5;

/// Lecteur interactif d'un serveur : un message dans le salon textuel, tenu à jour par une tâche de fond
pub struct Lecteur {
    salon: ChannelId,
    message: Option<MessageId>,
    volume: f32,
    reveil: Arc<Notify>, // force une mise à jour immédiate (changement de morceau, bouton)
}
pub type Lecteurs = Arc<Mutex<HashMap<GuildId, Lecteur>>>;

/// Commandes du lecteur de musique
pub fn commands() -> Vec<poise::Command<Data, Error>> {
    vec![play(), skip(), stop(), leave()]
}

/// Réponse visible seulement par l'auteur : le lecteur montre déjà tout au salon
async fn dire(ctx: Context<'_>, texte: impl Into<String>) -> Result<(), Error> {
    ctx.send(poise::CreateReply::default().content(texte).ephemeral(true)).await?;
    Ok(())
}

/// Appel vocal du serveur, ou `None` après avoir prévenu l'utilisateur (pas une erreur, donc pas de log)
async fn call(ctx: Context<'_>) -> Result<Option<Arc<tokio::sync::Mutex<songbird::Call>>>, Error> {
    let guild_id = ctx.guild_id().ok_or("Commande à utiliser sur un serveur")?;
    let manager = songbird::get(ctx.serenity_context()).await.ok_or("Songbird non initialisé")?;
    let call = manager.get(guild_id);
    if call.is_none() {
        dire(ctx, "Je ne suis pas en vocal").await?;
    }
    Ok(call)
}

fn reveiller(lecteurs: &Lecteurs, guild: GuildId) {
    if let Some(l) = lecteurs.lock().unwrap().get(&guild) {
        l.reveil.notify_one();
    }
}

/// Source paresseuse sur l'URL donnée par yt-dlp : la connexion ne s'ouvre qu'au moment de jouer.
/// shortcut: les URL YouTube expirent après ~6 h, un son resté plus longtemps en file échouera ; relancer yt-dlp si ça arrive
fn flux(client: reqwest::Client, out: YoutubeDlOutput) -> Input {
    let headers: HeaderMap = (out.http_headers.iter().flatten())
        .filter_map(|(k, v)| Some((HeaderName::from_bytes(k.as_bytes()).ok()?, HeaderValue::from_str(v).ok()?)))
        .collect();
    match out.protocol.as_deref() {
        Some("m3u8_native") => HlsRequest::new_with_headers(client, out.url, headers).into(),
        _ => HttpRequest { client, request: out.url, headers, content_length: out.filesize }.into(),
    }
}

fn label(meta: &AuxMetadata) -> String {
    let title = meta.title.as_deref().unwrap_or("Sans titre");
    match &meta.source_url {
        Some(url) => format!("[{title}](<{url}>)"),
        None => title.to_string(),
    }
}

/// Joue une URL YouTube ou une recherche, ou l'ajoute à la file
#[poise::command(slash_command, guild_only)]
async fn play(ctx: Context<'_>, #[rest] recherche: String) -> Result<(), Error> {
    let guild_id = ctx.guild_id().unwrap();
    let Some(channel_id) = user_channel(ctx) else {
        dire(ctx, "Rejoins un salon vocal d'abord").await?;
        return Ok(());
    };
    ctx.defer_ephemeral().await?;

    let http = ctx.data().http.clone();
    let mut src = if recherche.starts_with("http") {
        YoutubeDl::new(http.clone(), recherche)
    } else {
        YoutubeDl::new_search(http.clone(), recherche)
    };
    // Un seul passage de yt-dlp : titre ET URL du flux (YoutubeDl relancerait yt-dlp au moment de jouer)
    let out = src.query(1).await?.swap_remove(0);
    let meta = out.as_aux_metadata();
    let msg = format!("Ajouté : {}", label(&meta));
    let input = flux(http, out);

    let manager = songbird::get(ctx.serenity_context()).await.ok_or("Songbird non initialisé")?;
    let call = manager.join(guild_id, channel_id).await?;
    let (volume, reveil) = lecteur(ctx, &manager, guild_id);
    let track = call.lock().await.enqueue(Track::new_with_data(input, Arc::new(meta)).volume(volume)).await;
    // Début et fin de chaque morceau → mise à jour immédiate du lecteur
    for evenement in [TrackEvent::Play, TrackEvent::End] {
        track.add_event(Event::Track(evenement), Reveil(reveil.clone()))?;
    }
    reveil.notify_one();
    dire(ctx, msg).await
}

/// Lecteur du serveur (créé au premier /play, dans le salon de la commande) : (volume, réveil)
fn lecteur(ctx: Context<'_>, manager: &Arc<Songbird>, guild: GuildId) -> (f32, Arc<Notify>) {
    let lecteurs = ctx.data().lecteurs.clone();
    let mut map = lecteurs.lock().unwrap();
    if let Some(l) = map.get(&guild) {
        return (l.volume, l.reveil.clone());
    }
    let reveil = Arc::new(Notify::new());
    map.insert(guild, Lecteur { salon: ctx.channel_id(), message: None, volume: VOLUME, reveil: reveil.clone() });
    tokio::spawn(animer(ctx.serenity_context().http.clone(), manager.clone(), lecteurs.clone(), guild));
    (VOLUME, reveil)
}

struct Reveil(Arc<Notify>);

#[serenity::async_trait]
impl EventHandler for Reveil {
    async fn act(&self, _: &EventContext<'_>) -> Option<Event> {
        self.0.notify_one();
        None
    }
}

/// Tâche de fond d'un lecteur : met le message à jour jusqu'à ce que la file soit vide ou le bot parti
async fn animer(http: Arc<serenity::Http>, manager: Arc<Songbird>, lecteurs: Lecteurs, guild: GuildId) {
    // Créée juste avant l'ajout du 1er morceau : on attend le signal de /play, sinon la file paraît vide et on s'arrêterait
    let reveil = lecteurs.lock().unwrap().get(&guild).map(|l| l.reveil.clone());
    if let Some(reveil) = reveil {
        let _ = tokio::time::timeout(Duration::from_secs(5), reveil.notified()).await;
    }
    loop {
        let Some((salon, message, volume, reveil)) =
            lecteurs.lock().unwrap().get(&guild).map(|l| (l.salon, l.message, l.volume, l.reveil.clone()))
        else {
            return;
        };
        let rendu = match manager.get(guild) {
            Some(call) => rendu(&call, volume).await,
            None => None,
        };
        let Some((embed, boutons)) = rendu else {
            // File vide ou bot parti : le lecteur disparaît du salon
            if let Some(m) = message {
                let _ = salon.delete_message(&http, m).await; // déjà supprimé à la main : rien à faire
            }
            lecteurs.lock().unwrap().remove(&guild);
            return;
        };
        let modif = serenity::EditMessage::new().embed(embed.clone()).components(boutons.clone());
        let edite = match message {
            Some(m) => salon.edit_message(&http, m, modif).await.is_ok(),
            None => false,
        };
        // Premier affichage, ou message supprimé entre-temps : on en poste un nouveau
        if !edite {
            match salon.send_message(&http, serenity::CreateMessage::new().embed(embed).components(boutons)).await {
                Ok(m) => {
                    if let Some(l) = lecteurs.lock().unwrap().get_mut(&guild) {
                        l.message = Some(m.id);
                    }
                },
                Err(e) => tracing::warn!("Lecteur musique : envoi impossible dans {salon} ({e})"),
            }
        }
        tokio::select! {
            _ = reveil.notified() => {},
            _ = tokio::time::sleep(RAFRAICHISSEMENT) => {},
        }
    }
}

/// Contenu du lecteur, ou `None` s'il n'y a plus rien à jouer
async fn rendu(
    call: &tokio::sync::Mutex<songbird::Call>,
    volume: f32,
) -> Option<(serenity::CreateEmbed, Vec<serenity::CreateActionRow>)> {
    let file = call.lock().await.queue().current_queue();
    let courant = file.first()?;
    let etat = courant.get_info().await.ok()?;
    let meta = courant.data::<AuxMetadata>();
    let en_pause = matches!(etat.playing, PlayMode::Pause);

    let suite: Vec<String> =
        file.iter().skip(1).take(A_SUIVRE).enumerate().map(|(i, t)| format!("{}. {}", i + 1, label(&t.data::<AuxMetadata>()))).collect();
    let reste = file.len().saturating_sub(1 + A_SUIVRE);
    let mut a_suivre = if suite.is_empty() { "Rien, ajoute avec `/play`".to_string() } else { suite.join("\n") };
    if reste > 0 {
        a_suivre += &format!("\n… et {reste} autre(s)");
    }

    let mut embed = serenity::CreateEmbed::new()
        .title(if en_pause { "⏸️ En pause" } else { "🎶 En cours" })
        .description(format!("**{}**\n{}", label(&meta), barre(etat.position, meta.duration)))
        .field("À suivre", a_suivre, false)
        .footer(serenity::CreateEmbedFooter::new(format!("Volume {} % · {} en file", (volume * 100.0).round(), file.len())))
        .color(if en_pause { 0x99_AA_B5 } else { 0x1D_B9_54 });
    if let Some(miniature) = &meta.thumbnail {
        embed = embed.thumbnail(miniature);
    }
    let bouton = |id: &str, emoji: char| {
        serenity::CreateButton::new(format!("ratus:{id}")).emoji(emoji).style(serenity::ButtonStyle::Secondary)
    };
    let boutons = serenity::CreateActionRow::Buttons(vec![
        bouton("pause", if en_pause { '▶' } else { '⏸' }),
        bouton("suivant", '⏭'),
        bouton("stop", '⏹'),
        bouton("moins", '🔉'),
        bouton("plus", '🔊'),
    ]);
    Some((embed, vec![boutons]))
}

/// `1:23 ▬▬▬▬🔘▬▬▬▬▬▬ 3:45`
fn barre(position: Duration, duree: Option<Duration>) -> String {
    let temps = |d: Duration| format!("{}:{:02}", d.as_secs() / 60, d.as_secs() % 60);
    let Some(duree) = duree.filter(|d| !d.is_zero()) else { return temps(position) };
    const LARGEUR: usize = 14;
    let curseur = ((position.as_secs_f32() / duree.as_secs_f32()) * LARGEUR as f32).min(LARGEUR as f32) as usize;
    let piste: String = (0..=LARGEUR).map(|i| if i == curseur { "🔘" } else { "▬" }).collect();
    format!("`{}` {piste} `{}`", temps(position), temps(duree))
}

/// Passe au son suivant
#[poise::command(slash_command, guild_only)]
async fn skip(ctx: Context<'_>) -> Result<(), Error> {
    let Some(call) = call(ctx).await? else { return Ok(()) };
    call.lock().await.queue().skip()?;
    dire(ctx, "Suivant ⏭️").await
}

/// Arrête la lecture et vide la file
#[poise::command(slash_command, guild_only)]
async fn stop(ctx: Context<'_>) -> Result<(), Error> {
    let Some(call) = call(ctx).await? else { return Ok(()) };
    call.lock().await.queue().stop();
    reveiller(&ctx.data().lecteurs, ctx.guild_id().unwrap());
    dire(ctx, "Stop, file vidée ⏹️").await
}

/// Quitte le vocal
#[poise::command(slash_command, guild_only)]
async fn leave(ctx: Context<'_>) -> Result<(), Error> {
    let Some(call) = call(ctx).await? else { return Ok(()) };
    call.lock().await.queue().stop();
    let manager = songbird::get(ctx.serenity_context()).await.ok_or("Songbird non initialisé")?;
    manager.remove(ctx.guild_id().unwrap()).await?;
    reveiller(&ctx.data().lecteurs, ctx.guild_id().unwrap());
    ctx.say("Ciao 👋").await?;
    Ok(())
}

pub async fn on_event(fw: poise::FrameworkContext<'_, Data, Error>, event: &serenity::FullEvent) -> Result<(), Error> {
    match event {
        // Bot déconnecté sans /leave (kick, clic droit, salon supprimé) : songbird garde la file en mémoire, on la libère
        serenity::FullEvent::VoiceStateUpdate { new, .. }
            if new.user_id == fw.bot_id() && new.channel_id.is_none() =>
        {
            let Some(guild_id) = new.guild_id else { return Ok(()) };
            let manager = songbird::get(fw.serenity_context).await.ok_or("Songbird non initialisé")?;
            if let Some(call) = manager.get(guild_id) {
                call.lock().await.queue().stop();
                manager.remove(guild_id).await?;
                tracing::info!("Déconnecté du vocal sur {guild_id}, file vidée");
            }
            reveiller(&fw.user_data.lecteurs, guild_id);
        },
        serenity::FullEvent::InteractionCreate { interaction: serenity::Interaction::Component(clic) }
            if clic.data.custom_id.starts_with("ratus:") =>
        {
            // Accusé de réception d'abord : Discord laisse 3 s
            clic.create_response(&fw.serenity_context.http, serenity::CreateInteractionResponse::Acknowledge).await?;
            let Some(guild_id) = clic.guild_id else { return Ok(()) };
            bouton(fw, guild_id, &clic.data.custom_id).await?;
            reveiller(&fw.user_data.lecteurs, guild_id);
        },
        _ => {},
    }
    Ok(())
}

async fn bouton(fw: poise::FrameworkContext<'_, Data, Error>, guild_id: GuildId, id: &str) -> Result<(), Error> {
    let manager = songbird::get(fw.serenity_context).await.ok_or("Songbird non initialisé")?;
    let Some(call) = manager.get(guild_id) else { return Ok(()) };
    let file = call.lock().await.queue().clone();
    match id {
        "ratus:pause" => {
            let en_pause = match file.current() {
                Some(t) => matches!(t.get_info().await?.playing, PlayMode::Pause),
                None => return Ok(()),
            };
            if en_pause { file.resume()? } else { file.pause()? }
        },
        "ratus:suivant" => file.skip()?,
        "ratus:stop" => file.stop(),
        "ratus:moins" | "ratus:plus" => {
            let pas = if id == "ratus:plus" { 0.1 } else { -0.1 };
            let volume = match fw.user_data.lecteurs.lock().unwrap().get_mut(&guild_id) {
                Some(l) => {
                    l.volume = (l.volume + pas).clamp(0.0, 1.0);
                    l.volume
                },
                None => return Ok(()),
            };
            // Morceau en cours et morceaux déjà en file
            for t in file.current_queue() {
                let _ = t.set_volume(volume);
            }
        },
        _ => {},
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use songbird::input::{YoutubeDl, codecs};

    #[test]
    fn barre_de_progression() {
        let b = super::barre(Duration::from_secs(83), Some(Duration::from_secs(225)));
        assert!(b.starts_with("`1:23`") && b.ends_with("`3:45`") && b.contains('🔘'), "{b}");
        assert_eq!(super::barre(Duration::from_secs(5), None), "0:05"); // live : pas de durée connue
        assert!(super::barre(Duration::from_secs(999), Some(Duration::from_secs(10))).contains('🔘')); // jamais hors piste
    }

    // Réseau + yt-dlp requis : cargo test -- --ignored
    #[tokio::test]
    #[ignore]
    async fn flux_youtube_est_lisible() {
        let http = reqwest::Client::new();
        let out = YoutubeDl::new_search(http.clone(), "never gonna give you up".to_string())
            .query(1)
            .await
            .unwrap()
            .swap_remove(0);
        assert!(out.as_aux_metadata().title.is_some());
        let input = super::flux(http, out);
        assert!(input.make_playable_async(codecs::get_codec_registry(), codecs::get_probe()).await.is_ok());
    }
}
