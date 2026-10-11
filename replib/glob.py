from pathlib import Path as _GlobPath


# Validate spelling before Path normalization can erase forbidden components.
def _glob_check_path(value: str) -> None:
    # Monty's Path treats backslashes as separators even on POSIX hosts.
    normalized = value.replace("\\", "/")
    if normalized.startswith("/") or ".." in normalized.split("/"):
        raise ValueError("paths must be relative and must not contain '..': " + value)


# Keep bracket expressions together; an unclosed '[' is a literal character.
def _glob_tokens(pattern: str) -> list[str]:
    tokens: list[str] = []
    index = 0
    while index < len(pattern):
        end = index + 1
        if pattern[index] == "[":
            if end < len(pattern) and pattern[end] == "!":
                end += 1
            if end < len(pattern) and pattern[end] == "]":
                end += 1
            while end < len(pattern) and pattern[end] != "]":
                end += 1
            if end < len(pattern):
                tokens.append(pattern[index:end + 1])
                index = end + 1
                continue
        tokens.append(pattern[index])
        index += 1
    return tokens


# Match shell bracket ranges without translating them into a different regex dialect.
def _glob_character(token: str, character: str) -> bool:
    if token == "?":
        return True
    if len(token) == 1:
        return token == character
    members = token[1:-1]
    negate = members.startswith("!")
    if negate:
        members = members[1:]
    found = False
    index = 0
    while index < len(members):
        if index + 2 < len(members) and members[index + 1] == "-":
            if members[index] <= character <= members[index + 2]:
                found = True
            index += 3
        else:
            if members[index] == character:
                found = True
            index += 1
    return not found if negate else found


# Dynamic programming bounds wildcard matching without recursive backtracking.
def _glob_match(pattern: str, name: str) -> bool:
    previous = [True] + [False] * len(name)
    for token in _glob_tokens(pattern):
        current = [False] * (len(name) + 1)
        if token == "*":
            current[0] = previous[0]
            for index in range(len(name)):
                current[index + 1] = previous[index + 1] or current[index]
        else:
            for index in range(len(name)):
                current[index + 1] = previous[index] and _glob_character(token, name[index])
        previous = current
    return previous[len(name)]


# Expand one component at a time so a narrow pattern does not read unrelated trees.
def glob(
    pattern: str,
    path: str | None = None,
    exclude: tuple[str, ...] = (),
) -> list[str]:
    """Return sorted paths matching pattern beneath path (default '.').

    Results keep the supplied relative root, so they can be opened directly.
    Absolute paths and '..' are rejected, so /store and /library are out of reach;
    use pathlib or the direct tools there. '*', '?', and [...] match within one
    name; '**' matches any number of directories and does not enter directory
    symlinks. Hidden names are included. A trailing '/' selects directories.
    exclude is a tuple of entry names skipped at every level. Nothing is skipped by
    default, so '**' from the root also walks .git and target unless you pass
    exclude=('.git', 'target'). A missing path raises FileNotFoundError; other
    filesystem errors propagate.
    """
    # A str would silently turn the entry-name test below into a substring test.
    if isinstance(exclude, str):
        raise TypeError("exclude must be a tuple of entry names, not a str: " + repr(exclude))
    root = "." if path is None else path
    _glob_check_path(root)
    _glob_check_path(pattern)
    # Match the separator normalization used by Monty's Path for the root.
    pattern = pattern.replace("\\", "/")
    # An unusable root raises instead of returning [], which would read as "nothing
    # matched" and hide a mistyped path. A root through a directory symlink is walked.
    start = _GlobPath(root)
    if not start.is_dir():
        if start.exists():
            raise NotADirectoryError("path is not a directory: " + repr(root))
        raise FileNotFoundError("path does not exist: " + repr(root))
    parts = [part for part in pattern.split("/") if part not in ("", ".")]
    directories_only = pattern.endswith("/")
    pending: list[tuple[str, int]] = [(str(start), 0)]
    matches: set[str] = set()
    visited: set[tuple[str, int]] = set()
    while pending:
        current, index = pending.pop()
        state = (current, index)
        if state in visited:
            continue
        visited.add(state)
        entry = _GlobPath(current)
        directory = entry.is_dir()
        if index == len(parts):
            if (directory or entry.exists()) and (not directories_only or directory):
                matches.add(current)
            continue
        if not directory:
            continue
        component = parts[index]
        if component == "**":
            pending.append((current, index + 1))
        for child in entry.iterdir():
            if child.name in exclude:
                continue
            if component == "**":
                # '**' is the only component that can revisit a directory without
                # consuming a pattern part, so keeping it out of directory symlinks is
                # what prevents cycles; other components may pass through a symlink.
                child_directory = child.is_dir()
                if child_directory and not child.is_symlink():
                    pending.append((str(child), index))
                elif not child_directory and index + 1 == len(parts):
                    # A trailing '**' also matches files. Before a later part, a file
                    # could never match, so it is not queued.
                    pending.append((str(child), index + 1))
            elif _glob_match(component, child.name):
                pending.append((str(child), index + 1))
    return sorted(matches)
