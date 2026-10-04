//! Slash commands: one table used both to run them and to list them as completions,
//! so the two can't drift apart.

#[derive(Clone, Copy)]
pub enum Action {
    Exit,
    ToggleToolReasoning,
    ShowTools,
    Rename,
}

pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// One line, shown in the completion list.
    pub description: &'static str,
    pub action: Action,
    /// Takes the text after the name as its argument (`/rename <name>`). Other
    /// commands don't match input with text after the name.
    pub takes_argument: bool,
}

/// In completion-list order. `/exit` is last, so `/` then Enter can't quit by accident.
const COMMANDS: &[Command] = &[
    Command {
        name: "/rename",
        aliases: &[],
        description: "Rename this session",
        action: Action::Rename,
        takes_argument: true,
    },
    Command {
        name: "/tool-reasoning",
        aliases: &[],
        description: "Turn tool reasoning on or off",
        action: Action::ToggleToolReasoning,
        takes_argument: false,
    },
    Command {
        name: "/tools",
        aliases: &[],
        description: "Show tool descriptions",
        action: Action::ShowTools,
        takes_argument: false,
    },
    Command {
        name: "/exit",
        aliases: &["/quit"],
        description: "Exit",
        action: Action::Exit,
        takes_argument: false,
    },
];

/// The command `input` names, by name or alias, and its argument: the text after the
/// first whitespace, trimmed (empty if none).
pub fn find(input: &str) -> Option<(Action, &str)> {
    let (word, argument) = match input.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (input, ""),
    };
    let command = COMMANDS
        .iter()
        .find(|command| command.name == word || command.aliases.contains(&word))?;
    (command.takes_argument || argument.is_empty()).then_some((command.action, argument))
}

/// A completion: the spelling shown and inserted, and its command.
pub struct Completion {
    pub spelling: &'static str,
    pub command: &'static Command,
}

/// Commands whose name or an alias starts with `input`, each once: by its name if
/// that matches, otherwise by the first matching alias (so `/q` offers `/quit`).
pub fn completions(input: &str) -> Vec<Completion> {
    COMMANDS
        .iter()
        .filter_map(|command| {
            let spelling = std::iter::once(&command.name)
                .chain(command.aliases)
                .find(|spelling| spelling.starts_with(input))?;
            Some(Completion { spelling, command })
        })
        .collect()
}
