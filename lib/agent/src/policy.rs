//! When the agent must ask the user first.
//!
//! * [`Risk::Routine`] actions run without asking.
//! * [`Risk::Sensitive`] actions ask, unless the user chose "always allow"
//!   for that action ([`Permissions`]).
//! * [`Risk::Destructive`] actions always ask.
//!
//! The decision is the agent service's, enforced before anything runs; the
//! language model cannot skip it.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vjson::{Value, object};

use crate::tools::Risk;

/// The permission key of an action: `"files.write"`, `"editor.save"`.
pub fn key(app: &str, action: &str) -> String {
    alloc::format!("{app}.{action}")
}

/// Actions the user always allows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Permissions {
    /// Key to label ("Files: replace files").
    always: BTreeMap<String, String>,
}

impl Permissions {
    pub fn new() -> Permissions {
        Permissions::default()
    }

    /// Whether running the action needs the user's approval.
    pub fn needs_approval(&self, key: &str, risk: Risk) -> bool {
        match risk {
            Risk::Routine => false,
            Risk::Sensitive => !self.always.contains_key(key),
            Risk::Destructive => true,
        }
    }

    /// Remembers "always allow" (only sensitive actions can be).
    pub fn allow_always(&mut self, key: &str, label: &str, risk: Risk) -> bool {
        if risk != Risk::Sensitive || key.is_empty() || key.len() > 120 {
            return false;
        }
        self.always.insert(key.to_string(), label.chars().take(120).collect());
        true
    }

    pub fn revoke(&mut self, key: &str) -> bool {
        self.always.remove(key).is_some()
    }

    /// (key, label) of every permission.
    pub fn list(&self) -> Vec<(String, String)> {
        self.always.iter().map(|(k, l)| (k.clone(), l.clone())).collect()
    }

    pub fn to_json(&self) -> Value {
        let items: Vec<Value> =
            self.always.iter().map(|(k, l)| object! { "key" => k.as_str(), "label" => l.as_str() }).collect();
        object! { "always" => items }
    }

    pub fn from_json(text: &str) -> Permissions {
        let mut p = Permissions::new();
        if let Ok(v) = vjson::parse(text)
            && let Some(items) = v["always"].as_array()
        {
            for i in items {
                if let Some(k) = i.str("key") {
                    p.allow_always(k, i.str("label").unwrap_or(k), Risk::Sensitive);
                }
            }
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn decides_by_risk_and_permission() {
        let mut p = Permissions::new();
        let k = key("files", "write");
        assert!(!p.needs_approval(&k, Risk::Routine));
        assert!(p.needs_approval(&k, Risk::Sensitive));
        assert!(p.needs_approval(&k, Risk::Destructive));
        assert!(p.allow_always(&k, "Files: replace files", Risk::Sensitive));
        assert!(!p.needs_approval(&k, Risk::Sensitive));
        // Destructive actions can never be allowed for good.
        assert!(!p.allow_always("files.delete", "x", Risk::Destructive));
        assert!(p.needs_approval("files.delete", Risk::Destructive));
        let back = Permissions::from_json(&p.to_json().to_string());
        assert_eq!(back, p);
        assert!(p.revoke(&k));
        assert!(p.needs_approval(&k, Risk::Sensitive));
        assert_eq!(Permissions::from_json("nonsense"), Permissions::new());
    }
}
