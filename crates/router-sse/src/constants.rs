//! Fixed constants: the default thinking signatures and the Claude system
//! prompt.
//!
//! The signature blobs are copied verbatim rather than retyped: a single
//! altered base64 character turns a thinking placeholder into a 400 from the
//! upstream ("Corrupted thought signature"), and that failure is invisible
//! until a real request hits the provider.

/// The Claude Code system prompt. `openai-to-claude` prepends it so a Claude
/// upstream sees the Claude Code system prompt.
pub const CLAUDE_SYSTEM_PROMPT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// `DEFAULT_THINKING_TEXT`: the placeholder body for a thinking block with no
/// real content.
pub const DEFAULT_THINKING_TEXT: &str = "...";

/// `DEFAULT_THINKING_CLAUDE_SIGNATURE`.
pub const DEFAULT_THINKING_CLAUDE_SIGNATURE: &str = "EpwGCkYIChgCKkCzVUuRrg7CcglSUWEef4rH6o35g9UYS8ZPe0/VomQTBsFx6sttYNj5l8GqgW6ejuHyYqpFToxIbZl0bw17l5dJEgzCnqDO0Z8fRlMrNgsaDLS1cnCjC53KBqE0CCIwAADQdo1eO+7qPAmo8J4WR3JPmr92S97kmvr5K1iPMiOpkZNj8mEXW8uzBoOJs/9ZKoMFiqHJ3UObwaJDqFOW70E9oCwDoc6jesaWVAEdN5vWfKMpIkjFJjECdjIdkxyJNJ8Ib8yXVal3qwE7uThoPRqSZDdHB5mmwPEjWE/90cSYCbtX2YsJki1265CabBb8/QEkODXg4kgRrL+c8e8rRXz/dr1RswvaPuzEdGKHRNi9UooNUeOK4/ebx1KkP9YZttyohN9GWqlts36kOoW0Cfie/ABDgF9g534BPth/sstxDM6d79QlRmh6NxizyTF74DXJI34u0M4tTRchqE5pAq85SgdJaa+dix1yJPMji8m6nZkwJbscJb9rdc2MKyKWjz8QL2+rTSSuZ2F1k1qSsW0xNcI7qLcI12Vncfn/VqY6YOIZy/saZBR0ezXvN6g+UYbuIdyVg7AyIFZt3nbrO7/kmOEb2VKzygwklHGEIJHfFgMpH3JSrAzbZIowVHOF7VaJ+KXRFDCFin7hHTOiOsdg+1ij1mML9Z/x/9CP4b7OUcaQm1llDZPSHc6rZMNL3DdB+fW5YfmNgKU35S+7AMtA10nVILzDAk1UV4T2K9Do09JlI6rjOs9UuULlIN2Z0eE8YTlANR6uQcw7lMcdfqYE8tke4rDKc2dDiaS5vVe45VewICNpdXGN11yw8QqH7p27CR1HtN30e0tHXOR3bIwWk/Yb6O5fTaKG6Ri8e5ZCPvdD9HqepVi188nM0iTjJqL58F3ni04ECIhcbyaQWnuTes1Kw4CMwiZDLQkk8Hgz7HkUOf1btQTF/0nhD7ry0n0hAEg2PaDM3V6TjOjf4hEldRmeqERcQF1PfgKb6ZM12rlIIfUqKACczWJSzTV158+47HX36o0cgux6nFlv/DE+sEiRVxgB";
