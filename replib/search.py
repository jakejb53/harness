from pathlib import Path as _SearchPath
import re as _search_re


# search's public 'glob' parameter shadows the function; retain a private callable.
_glob_paths = glob


# Use the same traversal as glob so exclusions and symlink handling stay consistent.
def search(
    pattern: str,
    path: str | None = None,
    glob: str | None = None,
    exclude: tuple[str, ...] = (),
) -> list[dict[str, str | int]]:
    """Find regex matches in files beneath path (default '.'). Not re.search.

    Return one {'path', 'line', 'text'} record per matching line, ordered by path,
    then by 1-based line number. glob is a glob() pattern relative to path (default
    '**/*'): '*.rs' matches only top-level files, '**/*.rs' matches every level.
    exclude works as in glob(). Files containing a NUL byte are skipped as binary,
    as the grep tool skips them; other files that are not UTF-8 raise
    UnicodeDecodeError.
    """
    paths = _glob_paths("**/*" if glob is None else glob, path, exclude)
    expression = _search_re.compile(pattern)
    results: list[dict[str, str | int]] = []
    for name in paths:
        entry = _SearchPath(name)
        if entry.is_file():
            data = entry.read_bytes()
            # A NUL byte marks a binary file, the grep tool's rule, so a search from
            # the root does not fail on files such as .git/index. Every other file
            # must decode as UTF-8, so non-UTF-8 text raises instead of vanishing.
            if b"\x00" in data:
                continue
            for number, text in enumerate(data.decode("utf-8").splitlines(), 1):
                if expression.search(text) is not None:
                    results.append({"path": name, "line": number, "text": text})
    return results
