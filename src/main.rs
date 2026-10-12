mod assistant;
mod music;
mod nom;
mod ping;

use poise::serenity_prelude as serenity;
use songbird::SerenityInit;

struct Data {
    http: reqwest::Client,
    assistant: Option<std::sync::Arc<assistant::Config>>,
    lecteurs: music::Lecteurs,
}
type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

/// Salon vocal où se trouve l'auteur de la commande
fn user_channel(ctx: Context<'_>) -> Option<serenity::ChannelId> {
    ctx.guild()?.voice_states.get(&ctx.author().id)?.channel_id
}

#[tokio::main]
async fn main() {
    // .env facultatif (dev) ; en Docker les variables viennent de l'environnement
    dotenvy::dotenv().ok();
    // RUST_LOG surcharge le niveau, ex. RUST_LOG=songbird=debug ; udp_rx=error masque les « RTCP decryption failed » (statistiques, pas l'audio)
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "warn,ratus=info,songbird::driver::tasks::udp_rx=error".into());
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let token = std::env::var("DISCORD_TOKEN").expect("variable DISCORD_TOKEN manquante");
    let intents = serenity::GatewayIntents::non_privileged();
    let assistant = assistant::Config::from_env().map(std::sync::Arc::new);
    if assistant.is_none() {
        tracing::warn!("GROQ_API_KEY absente : /ecoute désactivé");
    }

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: music::commands().into_iter().chain([ping::ping(), assistant::ecoute()]).collect(),
            event_handler: |fw, event| Box::pin(music::on_event(fw, event)),
            ..Default::default()
        })
        .setup(|ctx, ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                tracing::info!(
                    "✅ {} connecté sur {} serveur(s), {} commandes enregistrées",
                    ready.user.name,
                    ready.guilds.len(),
                    framework.options().commands.len()
                );
                Ok(Data { http: reqwest::Client::new(), assistant, lecteurs: Default::default() })
            })
        })
        .build();

    let mut client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .register_songbird()
        .await
        .expect("création du client");

    // Ctrl+C ou `docker stop` : on ferme la connexion Discord proprement, start() rend la main et le processus sort en 0
    let shards = client.shard_manager.clone();
    tokio::spawn(async move {
        signal_arret().await;
        tracing::info!("Arrêt demandé, déconnexion…");
        shards.shutdown_all().await;
    });

    client.start().await.expect("client arrêté");
    tracing::info!("👋 Bot arrêté");
}

async fn signal_arret() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("écoute de SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.expect("écoute de Ctrl+C");
}
