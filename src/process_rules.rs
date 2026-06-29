use sysinfo::{ProcessesToUpdate, System};

use crate::{config::AppRule, devices::PollingRate};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveRule {
    pub exe: String,
    pub rate: PollingRate,
    pub restore: Option<PollingRate>,
}

pub struct ProcessScanner {
    system: System,
}

impl ProcessScanner {
    pub fn new() -> Self {
        Self {
            system: System::new(),
        }
    }

    pub fn active_rule(&mut self, rules: &[AppRule]) -> Option<ActiveRule> {
        self.system.refresh_processes(ProcessesToUpdate::All, true);
        for rule in rules {
            if self.is_running(&rule.exe) {
                return Some(ActiveRule {
                    exe: rule.exe.clone(),
                    rate: rule.rate,
                    restore: rule.restore,
                });
            }
        }
        None
    }

    fn is_running(&self, exe: &str) -> bool {
        let target = normalize_exe(exe);
        self.system.processes().values().any(|process| {
            process
                .name()
                .to_str()
                .map(normalize_exe)
                .is_some_and(|process_name| process_name == target)
        })
    }
}

impl Default for ProcessScanner {
    fn default() -> Self {
        Self::new()
    }
}

pub fn normalize_exe(raw: &str) -> String {
    let name = raw.trim().trim_matches('"').to_ascii_lowercase();
    name.strip_suffix(".exe")
        .map(ToOwned::to_owned)
        .unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_exe_names() {
        assert_eq!(normalize_exe("Game.EXE"), "game");
        assert_eq!(normalize_exe("\"game\""), "game");
        assert_eq!(normalize_exe(" game.exe "), "game");
    }
}
