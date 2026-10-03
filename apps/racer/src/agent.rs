//! Velocity for the voice agent: what the game shows (the main menu with
//! the settings of the next race; a race's position, laps, times and
//! standings; the results) and what it does (starting, restarting, pausing,
//! resuming and leaving races, changing the track, laps and opponents),
//! through the same code paths as the keyboard and the menus. The user
//! drives: the agent has no controls for the car.
//!
//! [`serve`] answers the agent between frames; [`Link`] registers the game
//! with the agent service.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use v3d::app::AppState;
use vui::agent::{self, Action, AgentServer, AppAgentInfo, Value, arg_opt_int, arg_opt_str, object};

pub use vui::agent::Registration as Link;

use crate::hud::fmt_time;
use crate::track::TRACK_NAMES;
use crate::{MAX_LAPS, MAX_OPPONENTS, NAMES, Phase, Racer};

/// The keys, for questions such as "how do I brake?".
const CONTROLS: &str = "Arrow keys or WASD drive (down brakes, then reverses), Space is the handbrake, Esc pauses";

/// What the game offers the agent.
pub fn info() -> AppAgentInfo {
    agent::info(
        "Velocity, a 3D arcade racing game against computer drivers: it starts, pauses and resumes races and \
         changes the track, laps and opponents, while the user drives with the keyboard.",
        vec![
            Action::new(
                "start_race",
                "Starts a race from the main menu or the results, with the current settings unless others are \
                 given. Does not abandon a race in progress (restart_race does)",
            )
            .choice("track", "The circuit (the current one by default)", false, &TRACK_NAMES)
            .param("laps", "integer", "Laps, 1 to 9 (the current number by default)", false)
            .param("opponents", "integer", "Computer drivers, 1 to 5 (the current number by default)", false)
            .build(),
            Action::new("restart_race", "Abandons the race in progress and starts it again from the grid").build(),
            Action::new("pause", "Pauses the race in progress").build(),
            Action::new("resume", "Resumes the paused race; Velocity's window must be in front").build(),
            Action::new("quit_to_menu", "Goes back to the main menu, abandoning any race in progress").build(),
            Action::new("change_settings", "Changes the track, laps or opponents of the next race (between races)")
                .choice("track", "The circuit", false, &TRACK_NAMES)
                .param("laps", "integer", "Laps, 1 to 9", false)
                .param("opponents", "integer", "Computer drivers, 1 to 5", false)
                .build(),
        ],
    )
}

/// What the game shows: the screen, the settings, and the race or its
/// results.
pub fn state(g: &Racer, in_front: bool) -> Value {
    let racing = matches!(g.phase, Phase::Countdown | Phase::Racing);
    let screen = match g.phase {
        Phase::Title => "menu",
        _ if racing && g.paused => "paused",
        Phase::Countdown => "countdown",
        Phase::Racing => "racing",
        Phase::Results => "results",
    };
    let mut v = object! { "screen" => screen };
    g.add_settings(&mut v);
    if racing {
        v.set("race", race(g));
    } else if g.phase == Phase::Results {
        v.set("your_place", place(&g.order()));
        v.set("results", results(g));
    }
    v.set("window_in_front", in_front);
    v.set("controls", CONTROLS);
    v
}

/// The player's place in `order` (from 1).
fn place(order: &[usize]) -> usize {
    order.iter().position(|&i| i == 0).unwrap_or(0) + 1
}

/// "1st", "2nd", "3rd", "4th"...
fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, x) if x != 11 => "st",
        (2, x) if x != 12 => "nd",
        (3, x) if x != 13 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// The player's race, as the race screen shows it.
fn race(g: &Racer) -> Value {
    let order = g.order();
    let me = &g.progress[0];
    let counting = g.phase == Phase::Countdown;
    let time = if counting { 0.0 } else { me.finish.unwrap_or(g.race_time) };
    let mut v = object! {
        "position" => place(&order),
        "cars" => g.cars.len(),
        "lap" => me.lap.clamp(1, g.laps),
        "time" => fmt_time(time),
        "best_lap" => me.best.map(fmt_time),
        "speed_kmh" => (g.cars[0].speed().abs() * 3.6) as u32,
        "standings" => order.iter().map(|&i| NAMES[i]).collect::<Vec<_>>(),
    };
    if counting {
        // The start lights go out after three seconds.
        v.set("starts_in_seconds", 3u32.saturating_sub(g.phase_time as u32).max(1));
    } else if me.finish.is_some() {
        v.set("finished", true);
    } else {
        v.set("lap_time", fmt_time((g.race_time - me.lap_start).max(0.0)));
    }
    if g.phase == Phase::Racing && g.wrong_way > 1.2 {
        v.set("wrong_way", true);
    }
    v
}

/// The results table, as the results screen shows it.
fn results(g: &Racer) -> Value {
    g.order()
        .iter()
        .enumerate()
        .map(|(k, &i)| {
            let p = &g.progress[i];
            object! {
                "place" => k + 1,
                "driver" => NAMES[i],
                "time" => p.finish.map(fmt_time).unwrap_or_else(|| String::from("still racing")),
                "best_lap" => p.best.map(fmt_time),
            }
        })
        .collect()
}

/// Settings for the next race, from an action's arguments.
#[derive(Default)]
struct Settings {
    track: Option<usize>,
    laps: Option<u32>,
    opponents: Option<usize>,
}

impl Settings {
    fn from_args(args: &Value) -> Result<Settings, String> {
        let track = arg_opt_str(args, "track").map(find_track).transpose()?;
        let laps = match arg_opt_int(args, "laps")? {
            Some(n) if !(1..=MAX_LAPS as i64).contains(&n) => {
                return Err(format!("A race has 1 to {MAX_LAPS} laps"));
            }
            n => n.map(|n| n as u32),
        };
        let opponents = match arg_opt_int(args, "opponents")? {
            Some(n) if !(1..=MAX_OPPONENTS as i64).contains(&n) => {
                return Err(format!("Velocity races against 1 to {MAX_OPPONENTS} computer drivers"));
            }
            n => n.map(|n| n as usize),
        };
        Ok(Settings { track, laps, opponents })
    }

    fn is_empty(&self) -> bool {
        self.track.is_none() && self.laps.is_none() && self.opponents.is_none()
    }
}

/// The circuit called `name`, or with `name` in its name.
fn find_track(name: &str) -> Result<usize, String> {
    let want = name.trim().to_lowercase();
    let names = || TRACK_NAMES.iter().map(|t| t.to_lowercase());
    names()
        .position(|t| t == want)
        .or_else(|| names().position(|t| !want.is_empty() && (t.contains(&want) || want.contains(&t))))
        .ok_or_else(|| format!("Velocity has no track called {name}; its tracks are {}", TRACK_NAMES.join(", ")))
}

impl Racer {
    /// The circuit of the next race (or of the race in progress).
    fn track_name(&self) -> &'static str {
        TRACK_NAMES[self.pending_track.unwrap_or(self.variant) % TRACK_NAMES.len()]
    }

    /// Adds the track, laps and opponents to `v`.
    fn add_settings(&self, v: &mut Value) {
        v.set("track", self.track_name());
        v.set("laps", self.laps);
        v.set("opponents", self.opponents);
        if self.pending_track.is_some() {
            v.set("generating_track", true);
        }
    }

    /// A race is on that the player has not finished.
    fn race_in_progress(&self) -> bool {
        matches!(self.phase, Phase::Countdown | Phase::Racing) && self.progress[0].finish.is_none()
    }

    /// The race in progress in a few words ("lap 2 of 3, 2nd of 4").
    fn race_summary(&self) -> String {
        if self.phase == Phase::Countdown {
            return String::from("on the starting grid");
        }
        let lap = self.progress[0].lap.clamp(1, self.laps);
        format!("lap {lap} of {}, {} of {}", self.laps, ordinal(place(&self.order())), self.cars.len())
    }

    /// Where the game is, for actions that do not apply there.
    fn screen_name(&self) -> &'static str {
        match self.phase {
            Phase::Title => "on the main menu",
            Phase::Results => "showing the results",
            Phase::Countdown | Phase::Racing => "racing",
        }
    }

    /// Changes the settings as the main menu's items do.
    fn apply(&mut self, s: &Settings) {
        if let Some(n) = s.laps {
            self.laps = n;
        }
        if let Some(n) = s.opponents
            && n != self.opponents
        {
            self.set_opponents(n);
        }
        if let Some(v) = s.track {
            // The menu shows that it is being generated (see `Racer::step`).
            self.pending_track = (v != self.variant).then_some(v);
        }
    }

    /// Starts a race as the menus do: at once, or on the new circuit as soon
    /// as it has been generated.
    fn begin_race(&mut self) {
        if self.pending_track.is_some() {
            if self.phase != Phase::Title {
                self.quit_to_menu();
            }
            self.start_when_ready = true;
        } else {
            self.start_race();
        }
    }

    /// The answer to an action that started a race.
    fn started(&self, in_front: bool) -> Value {
        let mut v = if self.start_when_ready {
            object! { "started" => "as soon as the track has been generated" }
        } else {
            object! { "started" => true }
        };
        self.add_settings(&mut v);
        if !in_front {
            v.set("note", "Velocity's window is not in front, so the race waits, paused, until it is");
        }
        v
    }

    pub(crate) fn agent_invoke(&mut self, action: &str, args: &Value, in_front: bool) -> Result<Value, String> {
        let racing = matches!(self.phase, Phase::Countdown | Phase::Racing);
        match action {
            "start_race" => {
                let settings = Settings::from_args(args)?;
                if self.race_in_progress() {
                    return Err(format!(
                        "A race is in progress ({}): restart_race starts it again, quit_to_menu abandons it",
                        self.race_summary()
                    ));
                }
                // The settings are on the main menu.
                if self.phase != Phase::Title {
                    self.quit_to_menu();
                }
                self.apply(&settings);
                self.begin_race();
                Ok(self.started(in_front))
            }
            "restart_race" => {
                self.begin_race();
                Ok(self.started(in_front))
            }
            "pause" => {
                if !racing {
                    return Err(format!("There is no race to pause: Velocity is {}", self.screen_name()));
                }
                let mut v = object! { "paused" => true };
                if self.paused {
                    v.set("note", "the race was already paused");
                } else {
                    self.pause();
                }
                Ok(v)
            }
            "resume" => {
                if !racing {
                    return Err(format!("There is no race to resume: Velocity is {}", self.screen_name()));
                }
                if !self.paused {
                    return Ok(object! { "paused" => false, "note" => "the race was not paused" });
                }
                if !in_front {
                    return Err("Velocity's window is not in front, and a race pauses whenever it is not: \
                                bring the window to the front first"
                        .into());
                }
                self.paused = false;
                Ok(object! { "paused" => false })
            }
            "quit_to_menu" => {
                let abandoned = self.race_in_progress();
                self.start_when_ready = false;
                if self.phase != Phase::Title {
                    self.quit_to_menu();
                }
                Ok(object! { "screen" => "menu", "abandoned_race" => abandoned })
            }
            "change_settings" => {
                let settings = Settings::from_args(args)?;
                if settings.is_empty() {
                    return Err("Say what to change: the track, the laps or the number of opponents".into());
                }
                if self.race_in_progress() {
                    return Err(format!(
                        "The settings change between races, and a race is in progress ({}): quit_to_menu \
                         abandons it",
                        self.race_summary()
                    ));
                }
                if self.phase != Phase::Title {
                    self.quit_to_menu();
                }
                self.apply(&settings);
                let mut v = Value::object();
                self.add_settings(&mut v);
                Ok(v)
            }
            other => Err(format!("Velocity has no action called {other}")),
        }
    }
}

/// The game and its window during one frame, as the agent sees them.
struct View<'a> {
    game: &'a mut Racer,
    /// The window has the keyboard (a race pauses when it loses it).
    in_front: bool,
}

impl AgentServer for View<'_> {
    fn agent_info(&self) -> AppAgentInfo {
        info()
    }

    fn agent_state(&self) -> Value {
        state(self.game, self.in_front)
    }

    fn agent_invoke(&mut self, action: &str, args: &Value) -> Result<Value, String> {
        self.game.agent_invoke(action, args, self.in_front)
    }
}

/// Answers the agent's requests, between frames (never waits).
pub fn serve(game: &mut Racer, app: &AppState) {
    if let Some(mut link) = game.agent.take() {
        link.serve(&mut View { game, in_front: app.focused });
        game.agent = Some(link);
    }
}
