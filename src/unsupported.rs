//! Python names agents commonly reach for that the REPL does not support, each with the
//! supported alternative. This table is the one source of help()'s `Use instead` lines
//! (`help_lines`) and of the line added after a REPL call's final exception when the
//! table lists that exception (`pointer`). Nothing here executes or rewrites code.
//!
//! Each message is the exact text the pinned Monty 1.0.0 raises for the name, so a Monty
//! upgrade must recheck them. Each alternative uses only names the REPL supports and
//! relative paths. The glob entry names the `glob()` helper shipped in `replib/glob.py`.

use monty_types::{ExcType, MontyException};

/// One help() line: unsupported names that share one alternative.
struct Entry {
    /// The unsupported names, comma-separated in help() and named singly by `pointer`.
    names: &'static [Name],
    /// What to use instead, worded to follow "use " and carrying no final period.
    alternative: &'static str,
}

/// One unsupported name and the exceptions Monty raises when code uses it.
struct Name {
    /// The name as help() and the pointer print it.
    name: &'static str,
    /// Exact `(type, message)` pairs; empty where Monty raises nothing specific to the
    /// name, so it is listed by help() but never pointed to. A pair appears only once in
    /// the whole table, so a match names exactly one name.
    errors: &'static [(ExcType, &'static str)],
}

/// The table, in help() order.
const TABLE: &[Entry] = &[
    Entry {
        names: &[Name {
            name: "os.path",
            errors: &[
                // `os.path.join(...)`: reading a missing module attribute names no module,
                // unlike calling one (`os.walk(...)` below).
                (
                    ExcType::AttributeError,
                    "'module' object has no attribute 'path'",
                ),
                // `import os.path` and `from os.path import join`.
                (ExcType::ModuleNotFoundError, "No module named 'os.path'"),
                // `from os import path`.
                (
                    ExcType::ImportError,
                    "cannot import name 'path' from 'os' (unknown location)",
                ),
            ],
        }],
        alternative: "pathlib.Path (Path(a) / b for join, p.exists(), p.is_file(), p.is_dir(), p.stat().st_size for getsize, p.stat().st_mtime for getmtime, p.name for basename, p.parent for dirname, p.with_suffix(\"\") and p.suffix for splitext)",
    },
    Entry {
        names: &[
            Name {
                name: "os.walk",
                errors: &[
                    (
                        ExcType::AttributeError,
                        "module 'os' has no attribute 'walk'",
                    ),
                    (
                        ExcType::ImportError,
                        "cannot import name 'walk' from 'os' (unknown location)",
                    ),
                ],
            },
            Name {
                name: "os.scandir",
                errors: &[
                    (
                        ExcType::AttributeError,
                        "module 'os' has no attribute 'scandir'",
                    ),
                    (
                        ExcType::ImportError,
                        "cannot import name 'scandir' from 'os' (unknown location)",
                    ),
                ],
            },
            Name {
                name: "Path.walk",
                errors: &[(
                    ExcType::AttributeError,
                    "'PosixPath' object has no attribute 'walk'",
                )],
            },
        ],
        alternative: "glob(\"**/*\", \"src\") for a whole tree, Path(d).iterdir() or os.listdir(d) for one directory",
    },
    Entry {
        names: &[
            Name {
                name: "import subprocess",
                errors: &[(ExcType::ModuleNotFoundError, "No module named 'subprocess'")],
            },
            Name {
                name: "os.system",
                errors: &[
                    (
                        ExcType::AttributeError,
                        "module 'os' has no attribute 'system'",
                    ),
                    (
                        ExcType::ImportError,
                        "cannot import name 'system' from 'os' (unknown location)",
                    ),
                ],
            },
            Name {
                name: "os.popen",
                errors: &[
                    (
                        ExcType::AttributeError,
                        "module 'os' has no attribute 'popen'",
                    ),
                    (
                        ExcType::ImportError,
                        "cannot import name 'popen' from 'os' (unknown location)",
                    ),
                ],
            },
        ],
        alternative: "run([\"git\", \"status\"]), which returns a dict with exit_code, stdout, and stderr",
    },
    Entry {
        names: &[
            Name {
                name: "import glob",
                errors: &[(ExcType::ModuleNotFoundError, "No module named 'glob'")],
            },
            Name {
                name: "Path.glob",
                errors: &[(
                    ExcType::AttributeError,
                    "'PosixPath' object has no attribute 'glob'",
                )],
            },
            Name {
                name: "Path.rglob",
                errors: &[(
                    ExcType::AttributeError,
                    "'PosixPath' object has no attribute 'rglob'",
                )],
            },
        ],
        alternative: "the glob(pattern, path) helper: glob(\"*.py\", \"src\") matches one level, glob(\"**/*.py\", \"src\") every level",
    },
    Entry {
        names: &[Name {
            name: "datetime.datetime.fromtimestamp",
            errors: &[(
                ExcType::AttributeError,
                "type object 'datetime.datetime' has no attribute 'fromtimestamp'",
            )],
        }],
        // `int(t)`: Monty's timedelta components take integers only.
        alternative: "datetime.datetime(1970, 1, 1, tzinfo=datetime.timezone.utc) + datetime.timedelta(seconds=int(t))",
    },
    Entry {
        // Monty's bytes() takes no iterable of ints; a generator expression arrives as
        // a list, so it raises the list message.
        names: &[
            Name {
                name: "bytes(range(...))",
                errors: &[(ExcType::TypeError, "cannot convert 'range' object to bytes")],
            },
            Name {
                name: "bytes([...])",
                errors: &[(ExcType::TypeError, "cannot convert 'list' object to bytes")],
            },
        ],
        alternative: "bytes.fromhex(\"\".join([f\"{i:02x}\" for i in ints]))",
    },
    Entry {
        names: &[Name {
            name: "for line in f (open file)",
            errors: &[
                (
                    ExcType::TypeError,
                    "'_io.TextIOWrapper' object is not iterable",
                ),
                (
                    ExcType::TypeError,
                    "'_io.BufferedReader' object is not iterable",
                ),
            ],
        }],
        alternative: "f.readlines() or p.read_text().splitlines()",
    },
    Entry {
        names: &[Name {
            name: "Path.touch",
            errors: &[(
                ExcType::AttributeError,
                "'PosixPath' object has no attribute 'touch'",
            )],
        }],
        alternative: "p.append_text(\"\"), which creates a missing file and leaves an existing one unchanged",
    },
    Entry {
        names: &[Name {
            name: "Path.relative_to",
            errors: &[(
                ExcType::AttributeError,
                "'PosixPath' object has no attribute 'relative_to'",
            )],
        }],
        alternative: "Path(*p.parts[len(base.parts):])",
    },
    Entry {
        names: &[Name {
            name: "Path.home",
            errors: &[
                // `Path.home()` on the class, and on an instance.
                (
                    ExcType::AttributeError,
                    "type object 'PosixPath' has no attribute 'home'",
                ),
                (
                    ExcType::AttributeError,
                    "'PosixPath' object has no attribute 'home'",
                ),
            ],
        }],
        alternative: "paths relative to the repository root (the REPL has no home directory)",
    },
    Entry {
        names: &[
            Name {
                name: "Path.read_text(encoding=...)",
                errors: &[(ExcType::TypeError, "read_text() takes no keyword arguments")],
            },
            // Monty reports any extra write_text argument by count, not by keyword, so
            // that message would also match calls unrelated to encoding=.
            Name {
                name: "Path.write_text(text, encoding=...)",
                errors: &[],
            },
        ],
        alternative: "the call without encoding= (text is always UTF-8)",
    },
    Entry {
        names: &[Name {
            name: "Path.unlink(missing_ok=True)",
            errors: &[(ExcType::TypeError, "unlink() takes no keyword arguments")],
        }],
        alternative: "if p.exists(): p.unlink()",
    },
];

/// help()'s `Use instead` lines: one `<names>: <alternative>` line per entry, in table
/// order, the names comma-separated.
pub fn help_lines() -> String {
    TABLE
        .iter()
        .map(|entry| {
            let names: Vec<&str> = entry.names.iter().map(|name| name.name).collect();
            format!("{}: {}\n", names.join(", "), entry.alternative)
        })
        .collect()
}

/// The line to add after `exception`, a REPL call's final exception, when the table lists
/// its exact type and message: `<name> is not available in this REPL; use <alternative>.`
/// `None` for any other exception, including a listed message under another type.
pub fn pointer(exception: &MontyException) -> Option<String> {
    let message = exception.message()?;
    TABLE.iter().find_map(|entry| {
        entry
            .names
            .iter()
            .find(|name| {
                name.errors
                    .iter()
                    .any(|(kind, text)| *kind == exception.exc_type() && *text == message)
            })
            .map(|name| {
                format!(
                    "{} is not available in this REPL; use {}.",
                    name.name, entry.alternative
                )
            })
    })
}
