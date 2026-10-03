//! The functions the language model may call, and how much each can affect
//! the user.
//!
//! The list is fixed for a conversation, and applications are reached
//! through it: `use_app` runs any action an application offers (its
//! actions are listed in the prompt and by `app_actions`), so applications
//! started later, or written later, need no new functions.
//!
//! [`definitions`] gives the Deepgram function list. [`risk`] classifies a
//! call of a system function; actions of applications carry their own risk.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vjson::{Value, object};

/// How much an action can affect the user (the same scale as
/// `vproto::agent::Risk`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Reading, or routine and easy to undo: done without asking.
    Routine,
    /// Matters, but is not destructive: asks unless always allowed.
    Sensitive,
    /// Deletes data, discards work or affects the whole system: always asks.
    Destructive,
}

/// A parameter for [`function`].
pub struct P<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub description: &'a str,
    pub required: bool,
    pub choices: &'a [&'a str],
}

/// A required parameter.
pub const fn req<'a>(name: &'a str, kind: &'a str, description: &'a str) -> P<'a> {
    P { name, kind, description, required: true, choices: &[] }
}

/// An optional parameter.
pub const fn opt<'a>(name: &'a str, kind: &'a str, description: &'a str) -> P<'a> {
    P { name, kind, description, required: false, choices: &[] }
}

/// A string parameter with fixed choices.
pub const fn choice<'a>(name: &'a str, description: &'a str, required: bool, choices: &'a [&'a str]) -> P<'a> {
    P { name, kind: "string", description, required, choices }
}

/// The JSON schema of a list of parameters.
pub fn schema(params: &[P]) -> Value {
    let mut props = vjson::Map::new();
    let mut required = Vec::new();
    for p in params {
        let kind = match p.kind {
            "integer" | "number" | "boolean" | "object" | "array" => p.kind,
            _ => "string",
        };
        let mut s = object! { "type" => kind, "description" => p.description };
        // Lists are lists of text (file names, paths).
        if kind == "array" {
            s.set("items", object! { "type" => "string" });
        }
        if !p.choices.is_empty() {
            s.set("enum", p.choices.iter().map(|c| Value::from(*c)).collect::<Vec<_>>());
        }
        props.insert(p.name, s);
        if p.required {
            required.push(Value::from(p.name));
        }
    }
    object! { "type" => "object", "properties" => props, "required" => required }
}

/// One function definition. `defer` holds the call until the user has
/// finished speaking (for actions with effects).
pub fn function(name: &str, description: &str, params: &[P], defer: bool) -> Value {
    let mut f = object! { "name" => name, "description" => description, "parameters" => schema(params) };
    if defer {
        f.set("defer_until_eot", true);
    }
    f
}

/// Names of the system functions.
pub mod names {
    pub const GET_STATUS: &str = "get_status";
    pub const LIST_APPS: &str = "list_apps";
    pub const OPEN_APP: &str = "open_app";
    pub const APP_ACTIONS: &str = "app_actions";
    pub const USE_APP: &str = "use_app";
    pub const READ_APP: &str = "read_app";
    pub const WINDOW: &str = "window";
    pub const FILES: &str = "files";
    pub const VOLUME: &str = "volume";
    pub const WIFI: &str = "wifi";
    pub const WALLPAPER: &str = "wallpaper";
    pub const NOTIFY: &str = "notify";
    pub const TIMER: &str = "timer";
    pub const MEMORY: &str = "memory";
    pub const SYSTEM: &str = "system";
    pub const TASKS: &str = "tasks";
    pub const END_CONVERSATION: &str = "end_conversation";
}

/// The functions offered to the language model.
pub fn definitions() -> Vec<Value> {
    use names::*;
    alloc::vec![
        function(
            GET_STATUS,
            "The current date and time, the open windows and which one is in front, volume, and network/Wi-Fi status.",
            &[],
            false,
        ),
        function(LIST_APPS, "The installed applications, whether they are running, and what each can do.", &[], false),
        function(
            OPEN_APP,
            "Opens an application, or brings it to the front if it is already open. Optionally opens a file in it.",
            &[
                req("app", "string", "Application id or name, e.g. \"editor\" or \"Music\""),
                opt("file", "string", "A file to open, e.g. \"~/Documents/notes.txt\"")
            ],
            true,
        ),
        function(
            APP_ACTIONS,
            "Lists the actions an application offers, with their parameters (starts the app if needed).",
            &[req("app", "string", "Application id or name")],
            false,
        ),
        function(
            USE_APP,
            "Runs an action of an application (see the application list in your instructions or app_actions). Starts the app if needed.",
            &[
                req("app", "string", "Application id or name"),
                req("action", "string", "The action's name"),
                opt("arguments", "object", "The action's arguments as an object"),
            ],
            true,
        ),
        function(
            READ_APP,
            "Describes what an open application shows right now (document, selection, playing track, page...).",
            &[req("app", "string", "Application id or name")],
            false,
        ),
        function(
            WINDOW,
            "Arranges windows: lists them, brings one to the front, minimises, maximises, restores, closes, snaps it to half of the screen, or shows the desktop.",
            &[
                choice(
                    "operation",
                    "What to do",
                    true,
                    &[
                        "list",
                        "focus",
                        "minimize",
                        "maximize",
                        "restore",
                        "close",
                        "snap_left",
                        "snap_right",
                        "show_desktop"
                    ]
                ),
                opt("window", "string", "The window's application or title (not needed for list and show_desktop)"),
            ],
            true,
        ),
        function(
            FILES,
            "Works with the user's files under ~ (the home folder: Documents, Pictures, Music, Desktop). Paths may start with ~.",
            &[
                choice(
                    "operation",
                    "What to do",
                    true,
                    &[
                        "list",
                        "find",
                        "info",
                        "read",
                        "write",
                        "append",
                        "create_folder",
                        "copy",
                        "move",
                        "rename",
                        "delete"
                    ]
                ),
                req("path", "string", "The file or folder (for find: the folder to search, default ~)"),
                opt("to", "string", "Destination for copy, move and rename"),
                opt("text", "string", "Content for write and append; what to look for with find"),
            ],
            true,
        ),
        function(
            VOLUME,
            "Changes or reports the sound volume.",
            &[
                opt("level", "integer", "Volume 0-100"),
                opt("change", "integer", "Change by this many points, e.g. 10 or -10"),
                opt("mute", "boolean", "Mute or unmute")
            ],
            true,
        ),
        function(
            WIFI,
            "Wi-Fi: status, networks in range, joining or leaving a network, turning the radio on or off.",
            &[
                choice("operation", "What to do", true, &["status", "scan", "connect", "disconnect", "on", "off"]),
                opt("network", "string", "Network name for connect"),
                opt("password", "string", "Password for connect, if the user gave one"),
            ],
            true,
        ),
        function(
            WALLPAPER,
            "Changes the desktop wallpaper: lists the pictures, sets one, or switches to the next.",
            &[
                choice("operation", "What to do", true, &["list", "set", "next"]),
                opt("picture", "string", "Picture name or path for set")
            ],
            true,
        ),
        function(
            NOTIFY,
            "Shows a notification on the screen (for things the user should see later, not for normal replies).",
            &[req("title", "string", "Short title"), req("text", "string", "The message")],
            true,
        ),
        function(
            TIMER,
            "Timers and reminders: when one is due you will be told and should tell the user.",
            &[
                choice("operation", "What to do", true, &["set", "list", "cancel"]),
                opt("seconds", "integer", "For set: due in this many seconds"),
                opt("at", "string", "For set: due at this local time today or tomorrow, \"HH:MM\" (24-hour)"),
                opt("label", "string", "What it is for"),
                opt("id", "integer", "For cancel"),
            ],
            false,
        ),
        function(
            MEMORY,
            "Your long-term memory of the user. Remember only what the user plainly told you about themselves \
             (their name, people in their life, plans, likes and dislikes, how they want things done) or asked you \
             to remember — never your own guesses or impressions, and nothing about this conversation itself \
             (thanks, goodbyes, how long answers were). Also search it, and forget on request.",
            &[
                choice("operation", "What to do", true, &["remember", "search", "forget", "forget_all"]),
                opt("text", "string", "What to remember, in the user's terms, or what to search for or forget"),
                choice("kind", "For remember", false, &["fact", "preference", "habit"]),
                opt("id", "integer", "For forget: the memory's id"),
            ],
            false,
        ),
        function(
            SYSTEM,
            "System information, restarting or shutting down the computer.",
            &[choice("operation", "What to do", true, &["info", "restart", "shutdown"])],
            true,
        ),
        function(
            TASKS,
            "Running programs: list them with their memory and CPU use, or end one that misbehaves.",
            &[
                choice("operation", "What to do", true, &["list", "end"]),
                opt("name", "string", "For end: the program's name"),
                opt("id", "integer", "For end: the program's id from list, when several have the name")
            ],
            true,
        ),
        function(
            END_CONVERSATION,
            "Ends the conversation after your reply, when the user says goodbye or clearly has finished. You stay available: the user wakes you by saying your name.",
            &[],
            false,
        ),
    ]
}

/// The risk of a call of a system function (`exists` tells whether a path
/// exists, to tell creating a file from overwriting one).
pub fn risk(name: &str, args: &Value, exists: &dyn Fn(&str) -> bool) -> Risk {
    let op = args.str("operation").unwrap_or("");
    match name {
        names::FILES => match op {
            "delete" => Risk::Destructive,
            "write" if args.str("path").is_some_and(exists) => Risk::Sensitive,
            "move" | "rename" if args.str("to").is_some_and(exists) => Risk::Destructive,
            _ => Risk::Routine,
        },
        names::WIFI if matches!(op, "disconnect" | "off") => Risk::Sensitive,
        names::MEMORY if op == "forget_all" => Risk::Destructive,
        names::SYSTEM if matches!(op, "restart" | "shutdown") => Risk::Destructive,
        names::TASKS if op == "end" => Risk::Destructive,
        _ => Risk::Routine,
    }
}

/// How an action reads in an approval request: (action, detail).
pub fn describe(name: &str, args: &Value) -> (String, String) {
    let op = args.str("operation").unwrap_or("");
    let path = args.str("path").unwrap_or("");
    let to = args.str("to").unwrap_or("");
    let quoted = |p: &str| alloc::format!("\u{201c}{}\u{201d}", p.rsplit('/').next().unwrap_or(p));
    match (name, op) {
        (names::FILES, "delete") => {
            (alloc::format!("Delete {}", quoted(path)), alloc::format!("{path} will be deleted for good."))
        }
        (names::FILES, "write") => (
            alloc::format!("Replace {}", quoted(path)),
            alloc::format!("The current contents of {path} will be overwritten."),
        ),
        (names::FILES, "move" | "rename") => (
            alloc::format!("Replace {} with {}", quoted(to), quoted(path)),
            alloc::format!("{to} already exists and will be replaced."),
        ),
        (names::WIFI, "off") => {
            ("Turn Wi-Fi off".into(), "The computer may lose its Internet connection, and the agent with it.".into())
        }
        (names::WIFI, "disconnect") => (
            "Disconnect from Wi-Fi".into(),
            "The computer may lose its Internet connection, and the agent with it.".into(),
        ),
        (names::MEMORY, "forget_all") => {
            ("Forget everything".into(), "Everything the agent remembers about you will be erased.".into())
        }
        (names::SYSTEM, "restart") => {
            ("Restart the computer".into(), "Unsaved work in open applications will be lost.".into())
        }
        (names::SYSTEM, "shutdown") => {
            ("Shut down the computer".into(), "Unsaved work in open applications will be lost.".into())
        }
        (names::TASKS, "end") => (
            match (args.str("name"), args["id"].as_u64()) {
                (Some(n), Some(id)) => alloc::format!("End {n} (id {id})"),
                (Some(n), None) => alloc::format!("End {n}"),
                (None, Some(id)) => alloc::format!("End program {id}"),
                (None, None) => "End a program".into(),
            },
            "The program stops at once; unsaved work in it is lost.".into(),
        ),
        _ => (name.replace('_', " "), args.to_string()),
    }
}

/// The JSON schema of an application action's parameters.
pub fn action_schema(params: &[(String, String, String, bool, Vec<String>)]) -> Value {
    let p: Vec<P> = params
        .iter()
        .map(|(n, k, d, r, _)| P { name: n, kind: k, description: d, required: *r, choices: &[] })
        .collect();
    let mut s = schema(&p);
    for (n, _, _, _, choices) in params {
        if !choices.is_empty()
            && let Some(prop) = s.get("properties").and_then(|p| p.get(n)).cloned()
        {
            let mut prop = prop;
            prop.set("enum", choices.iter().map(|c| Value::from(c.as_str())).collect::<Vec<_>>());
            if let Some(Value::Object(props)) = s.as_object_mut().and_then(|o| o.get_mut("properties")) {
                props.insert(n.as_str(), prop);
            }
        }
    }
    s
}

/// A one-line signature of an action for the prompt: `open_file(path, [line])`.
pub fn signature(name: &str, params: &[(String, bool)]) -> String {
    let args: Vec<String> =
        params.iter().map(|(n, req)| if *req { n.clone() } else { alloc::format!("[{n}]") }).collect();
    alloc::format!("{}({})", name, args.join(", "))
}

/// Whether `name` is one of the system functions.
pub fn is_system(name: &str) -> bool {
    definitions().iter().any(|d| d.str("name") == Some(name))
}

/// A string for the language model saying a call failed.
pub fn error(message: &str) -> String {
    object! { "ok" => false, "error" => message }.to_string()
}

/// A string for the language model with a successful result.
pub fn ok(result: Value) -> String {
    let mut v = object! { "ok" => true };
    if !result.is_null() {
        v.set("result", result);
    }
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn definitions_are_valid_function_schemas() {
        let defs = definitions();
        assert_eq!(defs.len(), 17);
        let mut names: Vec<&str> = defs.iter().filter_map(|d| d.str("name")).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 17, "names must be unique");
        for d in &defs {
            assert!(d.str("description").is_some_and(|s| s.len() > 20));
            assert_eq!(d.pointer("/parameters/type").and_then(Value::as_str), Some("object"));
            let required = d.pointer("/parameters/required").and_then(Value::as_array).unwrap();
            for r in required {
                assert!(d.pointer(&alloc::format!("/parameters/properties/{}", r.as_str().unwrap())).is_some());
            }
        }
        let files = defs.iter().find(|d| d.str("name") == Some("files")).unwrap();
        assert_eq!(files["defer_until_eot"].as_bool(), Some(true));
        assert!(files.pointer("/parameters/properties/operation/enum").and_then(Value::as_array).unwrap().len() > 5);
        assert!(is_system("files") && !is_system("open_file"));
    }

    #[test]
    fn classifies_risk() {
        let exists = |p: &str| p == "~/Documents/a.txt";
        let r = |name: &str, args: Value| risk(name, &args, &exists);
        assert_eq!(r("files", object! { "operation" => "read", "path" => "~/Documents/a.txt" }), Risk::Routine);
        assert_eq!(r("files", object! { "operation" => "delete", "path" => "x" }), Risk::Destructive);
        assert_eq!(r("files", object! { "operation" => "write", "path" => "~/Documents/new.txt" }), Risk::Routine);
        assert_eq!(r("files", object! { "operation" => "write", "path" => "~/Documents/a.txt" }), Risk::Sensitive);
        assert_eq!(
            r("files", object! { "operation" => "rename", "path" => "b", "to" => "~/Documents/a.txt" }),
            Risk::Destructive
        );
        assert_eq!(r("system", object! { "operation" => "shutdown" }), Risk::Destructive);
        assert_eq!(r("system", object! { "operation" => "info" }), Risk::Routine);
        assert_eq!(r("wifi", object! { "operation" => "off" }), Risk::Sensitive);
        assert_eq!(r("memory", object! { "operation" => "forget_all" }), Risk::Destructive);
        assert_eq!(r("open_app", object! { "app" => "editor" }), Risk::Routine);
        let (action, detail) =
            describe("files", &object! { "operation" => "delete", "path" => "~/Documents/report.txt" });
        assert_eq!(action, "Delete \u{201c}report.txt\u{201d}");
        assert!(detail.contains("~/Documents/report.txt"));
    }

    #[test]
    fn builds_action_schemas_and_signatures() {
        let params = vec![
            ("path".to_string(), "string".to_string(), "The file".to_string(), true, vec![]),
            (
                "mode".to_string(),
                "string".to_string(),
                "How".to_string(),
                false,
                vec!["a".to_string(), "b".to_string()],
            ),
        ];
        let s = action_schema(&params);
        assert_eq!(s.pointer("/required/0").and_then(Value::as_str), Some("path"));
        assert_eq!(s.pointer("/properties/mode/enum/1").and_then(Value::as_str), Some("b"));
        assert_eq!(signature("open_file", &[("path".into(), true), ("line".into(), false)]), "open_file(path, [line])");
        assert_eq!(ok(Value::Null), r#"{"ok":true}"#);
        assert_eq!(error("no"), r#"{"ok":false,"error":"no"}"#);
    }
}
