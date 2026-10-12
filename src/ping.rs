use std::time::Duration;

use crate::{Context, Error, user_channel};

// Généré avec Piper (fr_FR-upmc-medium, locuteur pierre, phonèmes « pˈɔŋɡə, ɡʁˈo fˈis də pˈyt! »), -2 demi-tons + compression, 48 kHz mono
static PONG: &[u8] = include_bytes!("../assets/pong.wav");

/// Dit « pong » à voix haute dans ton salon vocal
#[poise::command(slash_command, guild_only)]
pub async fn ping(ctx: Context<'_>) -> Result<(), Error> {
    let Some(channel_id) = user_channel(ctx) else {
        ctx.say("pong fdp").await?;
        return Ok(());
    };
    ctx.say("🏓").await?;

    let guild_id = ctx.guild_id().unwrap();
    let manager = songbird::get(ctx.serenity_context()).await.ok_or("Songbird non initialisé")?;
    // Déjà en vocal (musique) : on dit pong par-dessus sans bouger ni couper la file
    if let Some(call) = manager.get(guild_id) {
        call.lock().await.play_input(PONG.into());
        return Ok(());
    }

    let call = manager.join(guild_id, channel_id).await?;
    let track = call.lock().await.play_input(PONG.into());
    // Le clip dure ~1,7 s ; le timeout évite de rester bloqué en vocal si la piste ne finit jamais
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while track.get_info().await.is_ok_and(|s| !s.playing.is_done()) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    manager.remove(guild_id).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use songbird::input::{Input, codecs};

    #[tokio::test]
    async fn pong_wav_is_playable() {
        let input: Input = super::PONG.into();
        assert!(input.make_playable_async(codecs::get_codec_registry(), codecs::get_probe()).await.is_ok());
    }
}
