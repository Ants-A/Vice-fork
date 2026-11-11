//! Remember the game present in the recent replay, independently of focus
//! changes while saving. Never carry an old game beyond the requested clip.
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct GameContext {
    recent: Option<(String, Instant)>,
}
impl GameContext {
    pub fn observe(&mut self, game: Option<&str>, now: Instant) {
        if let Some(game) = game.filter(|game| !game.trim().is_empty()) {
            self.recent = Some((game.to_owned(), now));
        }
    }
    pub fn game(&self, now: Instant, lookback: Duration) -> Option<String> {
        self.recent
            .as_ref()
            .filter(|(_, observed)| now.saturating_duration_since(*observed) <= lookback)
            .map(|(game, _)| game.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workspace_switch_does_not_erase_game_in_the_replay() {
        let now = Instant::now();
        let mut context = GameContext::default();
        context.observe(Some("Cyberpunk 2077"), now);
        context.observe(None, now + Duration::from_secs(2));
        assert_eq!(
            context.game(now + Duration::from_secs(3), Duration::from_secs(20)),
            Some("Cyberpunk 2077".into())
        );
        let captured = context.game(now + Duration::from_secs(3), Duration::from_secs(20));
        context.observe(Some("Another game"), now + Duration::from_secs(4));
        assert_eq!(
            captured,
            Some("Cyberpunk 2077".into()),
            "an in-flight save keeps its original game"
        );
        assert_eq!(
            context.game(now + Duration::from_secs(5), Duration::from_secs(20)),
            Some("Another game".into())
        );
    }
    #[test]
    fn a_game_outside_the_selected_clip_is_not_reused() {
        let now = Instant::now();
        let mut context = GameContext::default();
        context.observe(Some("Cyberpunk 2077"), now);
        context.observe(None, now + Duration::from_secs(30));
        assert_eq!(
            context.game(now + Duration::from_secs(30), Duration::from_secs(20)),
            None
        );
        assert_eq!(
            context.game(now + Duration::from_secs(30), Duration::from_secs(60)),
            Some("Cyberpunk 2077".into())
        );
        assert_eq!(
            GameContext::default().game(now, Duration::from_secs(60)),
            None
        );
    }
}
