//! Control commands: tests (and people) change the simulated environment
//! while Vindows runs, over a line-based TCP connection.
//!
//! Every command is one line; the answer is zero or more lines followed by
//! `ok` or `error: <why>`.
//!
//! ```text
//! list                              the access points
//! status                            the guest radio, the wired side, counters
//! ap <name> on|off                  switch an access point on (restarted) or off
//! ap <name> signal <dBm>            signal level at the guest (-92 and below: out of range)
//! ap <name> loss <percent>          frame loss in each direction
//! ap <name> deauth [<reason>]       disconnect its stations
//! ap <name> rekey                   send new group keys
//! ap <name> password <password>     change the password (restarts it)
//! ap <name> restart                 restart it (stations must join again)
//! wired up|down                     the wired network behind the access points
//! dhcp on|off                       DHCP messages pass or are dropped
//! dns normal|unanswered|servfail    how DNS queries are treated
//! radio drop [<seconds>]            disconnect the guest's radio for a while (default 3 s)
//! ```

use crate::world::{DnsMode, World};

/// Runs one command line. Returns the answer without the final `ok`, or
/// the error.
pub fn run(world: &mut World, line: &str, now: u64) -> Result<String, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        [] => Ok(String::new()),
        ["help"] => Ok(HELP.into()),
        ["list"] => Ok(world.describe_networks()),
        ["status"] => Ok(world.describe_status()),
        ["wired", state] => {
            world.wired.up = on_off(state, "up", "down")?;
            Ok(String::new())
        }
        ["dhcp", state] => {
            world.wired.dhcp = on_off(state, "on", "off")?;
            Ok(String::new())
        }
        ["radio", "drop", rest @ ..] => {
            let secs: u64 = match rest {
                [] => 3,
                [s] => s.parse().ok().filter(|&s| s <= 600).ok_or_else(|| format!("bad duration {s:?}"))?,
                _ => return Err("usage: radio drop [<seconds>]".into()),
            };
            world.radio_drop = Some(secs);
            Ok(String::new())
        }
        ["dns", mode] => {
            world.wired.dns = match *mode {
                "normal" => DnsMode::Normal,
                "unanswered" => DnsMode::Unanswered,
                "servfail" => DnsMode::ServFail,
                _ => return Err(format!("unknown DNS mode {mode:?} (normal, unanswered, servfail)")),
            };
            Ok(String::new())
        }
        ["ap", name, rest @ ..] => {
            let i = world.find(name).ok_or_else(|| format!("no access point named {name:?}"))?;
            match rest {
                ["on" | "off"] => world.set_on(i, rest[0] == "on", now),
                ["signal", dbm] => {
                    let dbm: i8 = dbm.parse().map_err(|_| format!("bad signal level {dbm:?}"))?;
                    if dbm > -10 {
                        return Err("the signal level must be below -10 dBm".into());
                    }
                    world.networks[i].signal_dbm = dbm;
                }
                ["loss", pct] => {
                    let pct: u8 = pct.trim_end_matches('%').parse().map_err(|_| format!("bad percentage {pct:?}"))?;
                    if pct > 100 {
                        return Err("the loss must be 0 to 100 percent".into());
                    }
                    world.networks[i].loss_pct = pct;
                }
                ["deauth"] => world.deauthenticate(i, vwlan::frame::reason::UNSPECIFIED, now),
                ["deauth", code] => {
                    let code: u16 = code.parse().map_err(|_| format!("bad reason code {code:?}"))?;
                    world.deauthenticate(i, code, now);
                }
                ["rekey"] => world.rekey(i, now),
                ["password", password] => {
                    if world.networks[i].ap.cfg.passphrase.is_none() {
                        return Err("an open network has no password".into());
                    }
                    if !(8..=63).contains(&password.len()) {
                        return Err("the password must be 8 to 63 characters".into());
                    }
                    world.set_password(i, password, now);
                }
                ["restart"] => world.restart(i, now),
                _ => return Err(format!("unknown access point command {:?}", rest.join(" "))),
            }
            Ok(String::new())
        }
        _ => Err(format!("unknown command {line:?} (try help)")),
    }
}

fn on_off(word: &str, on: &str, off: &str) -> Result<bool, String> {
    match word {
        w if w == on => Ok(true),
        w if w == off => Ok(false),
        _ => Err(format!("expected {on} or {off}, not {word:?}")),
    }
}

const HELP: &str = "\
list | status
ap <name> on|off | signal <dBm> | loss <percent> | deauth [<reason>] | rekey | password <password> | restart
wired up|down
dhcp on|off
dns normal|unanswered|servfail
radio drop [<seconds>]
";
