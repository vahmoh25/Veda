//! Starfall for the voice agent: what the game shows (the title screen and
//! the high score; a game's score, level, wave, shield, hull and guns; the
//! game over panel) and what it does (starting, restarting, pausing,
//! resuming and leaving games), through the same code paths as the keyboard
//! and the pause menu. The user flies: the agent has no controls for the
//! ship.
//!
//! [`serve`] answers the agent between frames; [`Link`] registers the game
//! with the agent service.

use alloc::format;
use alloc::string::String;
use alloc::vec;

use v3d::app::AppState;
use vui::agent::{self, Action, AgentServer, AppAgentInfo, Value, object};

pub use vui::agent::Registration as Link;

use crate::{Phase, Starfall};

/// The keys, for questions such as "how do I shoot?".
const CONTROLS: &str = "Arrow keys or WASD fly the ship, Space fires, Esc pauses";

/// What the game offers the agent.
pub fn info() -> AppAgentInfo {
    agent::info(
        "Starfall, a 3D space shooter against waves of enemy ships and asteroids: it starts, pauses, resumes and \
         ends games, while the user flies the ship with the keyboard.",
        vec![
            Action::new(
                "new_game",
                "Starts a game from the title screen or after a game over. Does not abandon a game in progress \
                 (restart_game does)",
            )
            .build(),
            Action::new(
                "restart_game",
                "Abandons the game in progress, losing its score (the high score stays), and starts a new one",
            )
            .build(),
            Action::new("pause", "Pauses the game in progress").build(),
            Action::new("resume", "Resumes the paused game; Starfall's window must be in front").build(),
            Action::new("quit_to_title", "Goes back to the title screen, abandoning any game in progress").build(),
        ],
    )
}

/// What the game shows: the screen and the game's numbers.
pub fn state(g: &Starfall, in_front: bool) -> Value {
    let screen = match g.phase {
        Phase::Title => "title",
        Phase::Playing if g.paused => "paused",
        Phase::Playing => "playing",
        Phase::GameOver => "game_over",
    };
    let mut v = object! { "screen" => screen };
    match g.phase {
        Phase::Title => {}
        Phase::Playing => {
            v.set("score", g.score);
            v.set("level", g.level);
            v.set("wave", g.wave.max(1));
            v.set("shield_percent", g.ship.shield.max(0.0) as u32);
            v.set("hull_percent", g.ship.hull.max(0.0) as u32);
            v.set("guns", g.ship.weapon);
            v.set("score_multiplier", g.multiplier());
            if let Some((message, _)) = &g.message {
                v.set("message", message.as_str());
            }
        }
        Phase::GameOver => {
            v.set("final_score", g.score);
            v.set("level", g.level);
            v.set("new_high_score", g.score >= g.high && g.score > 0);
        }
    }
    v.set("high_score", g.high);
    v.set("window_in_front", in_front);
    v.set("controls", CONTROLS);
    v
}

impl Starfall {
    /// The game in progress in a few words ("level 2, wave 3, 12400 points").
    fn game_summary(&self) -> String {
        let paused = if self.paused { ", paused" } else { "" };
        format!("level {}, wave {}, {} points{paused}", self.level, self.wave.max(1), self.score)
    }

    /// Where the game is, for actions that do not apply there.
    fn screen_name(&self) -> &'static str {
        match self.phase {
            Phase::Title => "on the title screen",
            Phase::GameOver => "showing the game over panel",
            Phase::Playing => "playing",
        }
    }

    /// The answer to an action that started a game.
    fn started(&self, in_front: bool) -> Value {
        let mut v = object! { "started" => true, "high_score" => self.high };
        if !in_front {
            v.set("note", "Starfall's window is not in front, so the game waits, paused, until it is");
        }
        v
    }

    pub(crate) fn agent_invoke(&mut self, action: &str, in_front: bool) -> Result<Value, String> {
        let playing = self.phase == Phase::Playing;
        match action {
            "new_game" => {
                if playing {
                    return Err(format!(
                        "A game is in progress ({}): restart_game abandons it and starts again, resume continues it",
                        self.game_summary()
                    ));
                }
                self.reset();
                Ok(self.started(in_front))
            }
            "restart_game" => {
                let abandoned = playing.then_some(self.score);
                self.reset();
                let mut v = self.started(in_front);
                if let Some(score) = abandoned {
                    v.set("abandoned_score", score);
                }
                Ok(v)
            }
            "pause" => {
                if !playing {
                    return Err(format!("There is no game to pause: Starfall is {}", self.screen_name()));
                }
                let mut v = object! { "paused" => true };
                if self.paused {
                    v.set("note", "the game was already paused");
                } else {
                    self.pause();
                }
                Ok(v)
            }
            "resume" => {
                if !playing {
                    return Err(format!("There is no game to resume: Starfall is {}", self.screen_name()));
                }
                if !self.paused {
                    return Ok(object! { "paused" => false, "note" => "the game was not paused" });
                }
                if !in_front {
                    return Err("Starfall's window is not in front, and a game pauses whenever it is not: \
                                bring the window to the front first"
                        .into());
                }
                self.paused = false;
                Ok(object! { "paused" => false })
            }
            "quit_to_title" => {
                let abandoned = playing.then_some(self.score);
                if self.phase != Phase::Title {
                    self.quit_to_title();
                }
                Ok(object! { "screen" => "title", "abandoned_score" => abandoned })
            }
            other => Err(format!("Starfall has no action called {other}")),
        }
    }
}

/// The game and its window during one frame, as the agent sees them.
struct View<'a> {
    game: &'a mut Starfall,
    /// The window has the keyboard (a game pauses when it loses it).
    in_front: bool,
}

impl AgentServer for View<'_> {
    fn agent_info(&self) -> AppAgentInfo {
        info()
    }

    fn agent_state(&self) -> Value {
        state(self.game, self.in_front)
    }

    fn agent_invoke(&mut self, action: &str, _args: &Value) -> Result<Value, String> {
        self.game.agent_invoke(action, self.in_front)
    }
}

/// Answers the agent's requests, between frames (never waits).
pub fn serve(game: &mut Starfall, app: &AppState) {
    if let Some(mut link) = game.agent.take() {
        link.serve(&mut View { game, in_front: app.focused });
        game.agent = Some(link);
    }
}
