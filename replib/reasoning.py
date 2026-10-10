# A no-op. The app never runs this body for the model's own calls: a call that is
# exactly `reasoning("...")` on one plain string literal is answered by the app with
# `(no output)`, without approval and without reaching the REPL (see
# `is_reasoning_call` in src/tool_reasoning.rs). Any other code that calls it
# (`reasoning(x)`, or alongside other statements) runs this body; other operations
# retain their normal permission checks.
#
# This definition still matters: `help()` lists it, so the docstring is what the model
# reads about the function. Edit the docstring freely, but keep the body a no-op that
# prints nothing and returns None, or the app's `(no output)` answer becomes a lie.


# Keep the interpreted form equivalent to the app's exact-call shortcut.
def reasoning(text: str) -> None:
    """Record your reasoning as part of the conversation. Has no other effect; returns None."""
    pass
