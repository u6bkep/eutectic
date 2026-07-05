//! The tokenizer / quote-aware comment stripping and the generic nested block-tree
//! parse+serialize (over `ir::Block`/`ir::Node`). Self-contained: strings and the IR
//! data model in, strings and the block forest out.

use super::*;

/// The code part of a line: everything before the first `#` that is **not** inside a
/// double-quoted string. Quote-aware so a text label may contain `#` (`text "A#1" …`).
pub(crate) fn strip_comment(raw: &str) -> &str {
    let mut in_str = false;
    for (i, c) in raw.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &raw[..i],
            _ => {}
        }
    }
    raw
}

// ----------------------------------------------------------------------------
// Block tree (nested-block grammar infrastructure — Phase 0)
// ----------------------------------------------------------------------------
//
// The base grammar is one directive per line. A directive line may additionally
// *open a block* by ending with a trailing `{`; the block closes with a `}` alone
// on its own line, and blocks nest to arbitrary depth. This module owns only the
// generic nested representation and its (de)serialization — no directive here
// consumes a block yet (see [`keyword_takes_block`]). The Decision-20 layout tree
// (`row`/`column`) and Decision-21 `def` bodies are the consumers-to-be: they will
// walk a [`Block`]'s header + children ([`Node`]) without re-tokenizing.
//
// A block body is a [`Node`] sequence: child directives interleaved with the comment
// and blank lines between them. Trivia is preserved *inside* blocks (Decision 21's
// mixed-authorship `def` bodies must round-trip) but dropped at the top level, where
// the flat path has always dropped it.

// `Block` and `Node` — the block-tree DATA model — now live in `crate::ir` (the
// common downward dependency of `text` and `elaborate`). Re-exported here so
// every existing `crate::text::{Block, Node}` path keeps working; the parsing and
// serialization over them stays in this module.
pub use crate::ir::{Block, Node};

impl Block {
    /// The full header line as it feeds the flat directive path: `keyword` then
    /// `rest` (when non-empty). This is what [`parse_line`] receives for a leaf.
    pub(crate) fn header_line(&self) -> String {
        if self.rest.is_empty() {
            self.keyword.clone()
        } else {
            format!("{} {}", self.keyword, self.rest)
        }
    }
}

/// The per-keyword block allowlist. No existing directive accepts a block, so this is
/// `false` for every keyword today: a block opened on any current keyword is a parse
/// error, leaving all existing documents unchanged.
///
/// A Phase-1/2 consumer (the Decision-20 layout containers, Decision-21 `def`) enables
/// its keyword by wiring **three** things — the block tree is already built for every
/// keyword, but nothing walks a block body yet, so opting in is not a one-liner:
///
/// 1. return `true` here for the keyword;
/// 2. add a children-aware arm in [`parse_forest`] *before* the `parse_line`
///    fallthrough, which walks [`Block::children`] (recursing into nested
///    [`Node::Block`]s) and lowers the body into its own tier-1 representation;
/// 3. add storage for that representation in [`Parsed`].
///
/// The recursion path in [`parse_forest`] is exercised end-to-end by a `cfg(test)`
/// block-accepting keyword (see the `testblock` tests), so Phase 1 inherits a tested
/// descent rather than a latent one.
pub(crate) fn keyword_takes_block(keyword: &str) -> bool {
    // A test-only sentinel keyword that opts into blocks, so the `parse_forest` descent
    // path is covered before any real consumer lands (finding 3). Never reachable in a
    // non-test build; real keywords are added to this match when their consumer lands.
    #[cfg(test)]
    if keyword == TEST_BLOCK_KEYWORD {
        return true;
    }
    // Decision 20 layout tree: the `schematic` block and its nested `row`/`column`
    // containers accept block bodies. `sym` leaves do not (they are single-line
    // directives inside a container).
    // Decision 21a: a `def` opens a block body (its sub-circuit); `port` is a leaf
    // directive inside it.
    matches!(keyword, "schematic" | "row" | "column" | "def")
}

/// A `cfg(test)` sentinel keyword that accepts a block, used to exercise the
/// `parse_forest` descent end-to-end. Chosen not to collide with any real directive.
#[cfg(test)]
pub(crate) const TEST_BLOCK_KEYWORD: &str = "testblock";

/// Split a block-tree header into its tokens, keeping quoted runs intact. The leading
/// token is the keyword; `rest` is the original header with the keyword and one run of
/// separating whitespace removed (so coordinate/quote-sensitive per-directive parsers
/// see exactly what the flat path gives them today).
pub(crate) fn split_header(header: &str) -> (String, Vec<String>, String) {
    let tokens = split_ws_quoted(header);
    let keyword = tokens.first().cloned().unwrap_or_default();
    let rest = match header.trim_start().split_once(char::is_whitespace) {
        Some((_, r)) => r.trim().to_string(),
        None => String::new(),
    };
    (keyword, tokens, rest)
}

/// Detect a block-opening trailing `{` on an already comment-stripped line. Returns
/// `(header, opened_block)`: for an opener the trailing `{` is removed and `header`
/// is the directive part; otherwise `header` is the line unchanged. A `{` only opens
/// a block when it is the final non-whitespace character *outside* a quoted string —
/// a brace inside a quoted value (`text "a{b}"`) is literal. The scan is quote-aware
/// and runs on the unquoted remainder after comment stripping, so a `{` after a `#`
/// comment never opens a block either (the comment is already gone).
pub(crate) fn split_block_open(line: &str) -> (&str, bool) {
    let trimmed = line.trim_end();
    // The last character must be `{` *and* lie outside any quoted run to open a block.
    if !trimmed.ends_with('{') {
        return (line, false);
    }
    let mut in_str = false;
    let brace_at = trimmed.len() - 1; // `{` is ASCII, one byte.
    for (i, c) in trimmed.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '{' if !in_str && i == brace_at => {
                return (trimmed[..i].trim_end(), true);
            }
            _ => {}
        }
    }
    // The trailing `{` was inside a quoted string — literal, not a block opener.
    (line, false)
}

/// Is this comment-stripped, trimmed line a lone block close (`}`)? A `}` only closes
/// a block when it stands alone on its line; a `}` embedded in a directive or inside a
/// quoted value is not a close (quoted values are handled by the tokenizer downstream).
pub(crate) fn is_block_close(line: &str) -> bool {
    line == "}"
}

/// The comment text of a raw line whose code part (quote-aware) is empty — i.e. a
/// whole-line comment. Returns the text after the first unquoted `#`, trimmed, or
/// `None` if the line carries no comment. (A blank line has no `#` and returns `None`.)
pub(crate) fn whole_line_comment(raw: &str) -> Option<&str> {
    let mut in_str = false;
    for (i, c) in raw.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return Some(raw[i + 1..].trim()),
            _ => {}
        }
    }
    None
}

/// Parse text into a forest of [`Block`]s: the nested-block grammar's generic
/// representation. Comment stripping is quote-aware (a `#` inside a quoted value is
/// literal) and happens *before* brace detection on the unquoted remainder. Comment and
/// blank lines *inside a block* are preserved as [`Node`] trivia (Decision 21 mixed
/// authorship); at the top level they are dropped, as the flat path has always done.
/// Errors — an unbalanced `{` (a block left open at end of input), a `}` with no open
/// block, and an empty-keyword block opener (a lone `{`) — are `E_BLOCK` diagnostics
/// located by line number, collected in the house *collect-all* style. On any error the
/// whole parse fails and no partial tree escapes.
pub fn parse_blocks(text: &str) -> Result<Vec<Block>, Vec<Diagnostic>> {
    // A stack of (open-block header, its accumulating body). The bottom frame is the
    // synthetic top-level forest, whose opener is `None` and is never read.
    let mut stack: Vec<(Option<Block>, Vec<Node>)> = vec![(None, Vec::new())];
    let mut errors: Vec<Diagnostic> = Vec::new();
    // Whether the innermost frame is a real (non-bottom) block: trivia is preserved only
    // there, so top-level comments/blanks stay dropped exactly as before.
    let in_block = |stack: &Vec<(Option<Block>, Vec<Node>)>| stack.len() > 1;

    for (i, raw) in text.lines().enumerate() {
        let lineno = (i + 1) as u32;
        let stripped = strip_comment(raw).trim();
        if stripped.is_empty() {
            // A trivia line (blank or whole-line comment). Preserve it inside a block;
            // drop it at the top level (unchanged flat behavior).
            if in_block(&stack) {
                let node = match whole_line_comment(raw) {
                    Some(text) => Node::Comment(text.to_string()),
                    None => Node::Blank,
                };
                stack.last_mut().unwrap().1.push(node);
            }
            continue;
        }
        if is_block_close(stripped) {
            if !in_block(&stack) {
                errors.push(Diagnostic::error(
                    "E_BLOCK",
                    "`}` with no open block".to_string(),
                    Location::Span {
                        line: lineno,
                        col: 1,
                    },
                ));
                continue;
            }
            // Pop the frame, attach its finished body, and push the completed block onto
            // its parent as a `Node::Block`.
            let (opener, children) = stack.pop().expect("checked len > 1");
            let mut block = opener.expect("non-bottom frame always carries an opener");
            block.children = children;
            stack.last_mut().unwrap().1.push(Node::Block(block));
            continue;
        }
        let (header, opened) = split_block_open(stripped);
        let (keyword, tokens, rest) = split_header(header);
        if opened && keyword.is_empty() {
            // A `{` with no directive in front (e.g. a lone `{`). Rejected here in the
            // public API so a malformed opener never reaches a consumer or serializes to
            // a leading-space line (finding 4). No frame is opened.
            errors.push(Diagnostic::error(
                "E_BLOCK",
                "block opener has no directive before `{`".to_string(),
                Location::Span {
                    line: lineno,
                    col: 1,
                },
            ));
            continue;
        }
        let block = Block {
            keyword,
            tokens,
            rest,
            opened_block: opened,
            children: Vec::new(),
            line: lineno,
        };
        if opened {
            // Open a new frame; its body accumulates until the matching `}`.
            stack.push((Some(block), Vec::new()));
        } else {
            stack.last_mut().unwrap().1.push(Node::Block(block));
        }
    }

    // Any frame still open at end of input is an unbalanced `{`. Report each, located
    // at its opener's line.
    while stack.len() > 1 {
        let (opener, _) = stack.pop().expect("checked len > 1");
        let opener = opener.expect("non-bottom frame always carries an opener");
        let header = opener.header_line();
        errors.push(Diagnostic::error(
            "E_BLOCK",
            format!("unbalanced `{{`: block opened by `{header}` is never closed"),
            Location::Span {
                line: opener.line,
                col: 1,
            },
        ));
    }

    if errors.is_empty() {
        // Top-level trivia was never pushed (the bottom frame is not "in a block"), so
        // the bottom frame holds only `Node::Block`; unwrap to the `Vec<Block>` forest.
        let top = stack.pop().expect("bottom frame is always present").1;
        Ok(top
            .into_iter()
            .map(|n| match n {
                Node::Block(b) => b,
                _ => unreachable!("top-level trivia is dropped, never pushed"),
            })
            .collect())
    } else {
        // Report errors in source order (the opener pop order above is innermost-first).
        errors.sort_by_key(|d| match d.location {
            Location::Span { line, .. } => line,
            _ => 0,
        });
        Err(errors)
    }
}

/// The canonical indent for a nested block: two spaces per depth level. Matches the
/// existing serializer's flat, space-based style (it emits no tabs anywhere).
pub(crate) const BLOCK_INDENT: &str = "  ";

/// Serialize a forest of [`Block`]s back to canonical block-grammar text, deterministic
/// and round-tripping ([`parse_blocks`] of the output reproduces the forest). Each
/// directive renders as its header line; a block opener appends ` {`, its body renders
/// indented one level deeper (nested directives, and the comment/blank trivia between
/// them), and a `}` closes at the opener's indent. A comment renders as `# <text>` (or
/// bare `#` when empty), a blank as an empty line. This is the emission half consumers
/// reuse once their keyword opts into blocks; the flat [`serialize`] on a `Doc` is
/// unchanged (routeless/blockless docs stay byte-identical).
pub fn serialize_blocks(blocks: &[Block]) -> String {
    let mut out = String::new();
    // `indent` grows/shrinks by one level per depth so ancestors are never re-indented
    // (emission is O(total output), not O(depth·output)).
    let mut indent = String::new();
    emit_block_seq(blocks, &mut indent, &mut out);
    out
}

/// Emit a sequence of block *directives* (no trivia — the top-level forest and the
/// caller's convenience over a `&[Block]`), wrapping each into a `Node::Block` view.
pub(crate) fn emit_block_seq(blocks: &[Block], indent: &mut String, out: &mut String) {
    for b in blocks {
        emit_block(b, indent, out);
    }
}

/// Emit a block body: nested directives interleaved with preserved trivia.
pub(crate) fn emit_nodes(nodes: &[Node], indent: &mut String, out: &mut String) {
    for n in nodes {
        match n {
            Node::Block(b) => emit_block(b, indent, out),
            Node::Comment(text) => {
                out.push_str(indent);
                if text.is_empty() {
                    out.push_str("#\n");
                } else {
                    out.push_str("# ");
                    out.push_str(text);
                    out.push('\n');
                }
            }
            Node::Blank => out.push('\n'),
        }
    }
}

pub(crate) fn emit_block(b: &Block, indent: &mut String, out: &mut String) {
    out.push_str(indent);
    out.push_str(&b.header_line());
    if b.opened_block {
        out.push_str(" {\n");
        indent.push_str(BLOCK_INDENT);
        emit_nodes(&b.children, indent, out);
        indent.truncate(indent.len() - BLOCK_INDENT.len());
        out.push_str(indent);
        out.push_str("}\n");
    } else {
        out.push('\n');
    }
}
