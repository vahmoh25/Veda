//! Installed applications, described by `apps/<id>.app` files in the system
//! image:
//!
//! ```text
//! name=Text Editor
//! exe=/system/bin/editor.exe
//! icon=editor
//! category=Accessories
//! description=Edit plain-text documents
//! pinned=true
//! ```

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vproto::init::AppInfo;

pub fn parse(id: &str, text: &str) -> AppInfo {
    let mut app = AppInfo {
        id: id.to_string(),
        name: id.to_string(),
        exe: alloc::format!("/system/bin/{id}.exe"),
        icon: id.to_string(),
        category: "Applications".to_string(),
        description: String::new(),
        pinned: false,
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim().to_string();
        match k.trim() {
            "name" => app.name = v,
            "exe" => app.exe = v,
            "icon" => app.icon = v,
            "category" => app.category = v,
            "description" => app.description = v,
            "pinned" => app.pinned = v == "true",
            _ => {}
        }
    }
    app
}

/// Reads every application manifest from the initrd.
pub fn load(archive: &initrd::Archive) -> Vec<AppInfo> {
    let mut apps: Vec<AppInfo> = archive
        .files()
        .filter_map(|f| {
            let id = f.path.strip_prefix("apps/")?.strip_suffix(".app")?;
            Some(parse(id, core::str::from_utf8(f.data).ok()?))
        })
        .collect();
    apps.sort_by(|a, b| a.name.cmp(&b.name));
    apps
}
