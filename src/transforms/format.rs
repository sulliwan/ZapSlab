//! XML and SQL format/minify transforms, ported from Boop's `FormatXML.js`,
//! `MinifyXML.js`, `FormatSQL.js` and `MinifySQL.js`.
//!
//! Boop implements all four with `vkBeautify`. CSS variants are intentionally
//! omitted: there is no lightweight, reliable CSS formatter among the project's
//! dependencies.

use std::iter::Peekable;
use std::str::Chars;

use xmltree::{Element, EmitterConfig, XMLNode};

use super::{Scope, Transform, TransformResult};

pub const TRANSFORMS: &[Transform] = &[
    Transform {
        name: "Format XML",
        description: "Cleans and format XML/HTML documents.",
        tags: "html,prettify,clean,indent",
        scope: Scope::SelectionOrFullText,
        apply: format_xml,
    },
    Transform {
        name: "Minify XML",
        description: "Cleans and minifies XML/HTML documents.",
        tags: "html,minify,clean,indent",
        scope: Scope::SelectionOrFullText,
        apply: minify_xml,
    },
    Transform {
        name: "Format SQL",
        description: "Cleans and format SQL queries.",
        tags: "mysql,sql,prettify,clean,indent",
        scope: Scope::SelectionOrFullText,
        apply: format_sql,
    },
    Transform {
        name: "Minify SQL",
        description: "Cleans and minifies SQL queries.",
        tags: "mysql,sql,minify,clean,indent",
        scope: Scope::SelectionOrFullText,
        apply: minify_sql_transform,
    },
];

/// Parse the first root element, turning any failure into a user-facing error.
///
/// `Element::parse` panics via an internal `unreachable!()` when the input has
/// no root element, so we use `parse_all` and pick the element ourselves.
fn parse_xml(input: &str) -> Result<Element, String> {
    let nodes = Element::parse_all(input.as_bytes()).map_err(|err| err.to_string())?;
    nodes
        .into_iter()
        .find_map(|node| match node {
            XMLNode::Element(element) => Some(element),
            _ => None,
        })
        .ok_or_else(|| "No XML root element found".to_string())
}

fn format_xml(input: &str) -> TransformResult {
    let element = match parse_xml(input) {
        Ok(element) => element,
        Err(message) => return TransformResult::error(message),
    };

    // `perform_indent` uses xml-rs's default indent string of two spaces.
    // Boop's output has no XML declaration, so suppress it.
    let config = EmitterConfig::new()
        .perform_indent(true)
        .write_document_declaration(false)
        .pad_self_closing(false);
    emit_xml(&element, config)
}

fn minify_xml(input: &str) -> TransformResult {
    let mut element = match parse_xml(input) {
        Ok(element) => element,
        Err(message) => return TransformResult::error(message),
    };
    strip_whitespace_nodes(&mut element);

    let config = EmitterConfig::new()
        .perform_indent(false)
        .write_document_declaration(false)
        .pad_self_closing(false);
    emit_xml(&element, config)
}

/// Drop text nodes that contain only whitespace. xmltree already discards
/// `Whitespace` events while parsing, but this also covers any text nodes that
/// consist solely of spaces/tabs/newlines.
fn strip_whitespace_nodes(element: &mut Element) {
    element
        .children
        .retain(|node| !matches!(node, XMLNode::Text(text) if text.trim().is_empty()));

    for node in &mut element.children {
        if let XMLNode::Element(child) = node {
            strip_whitespace_nodes(child);
        }
    }
}

fn emit_xml(element: &Element, config: EmitterConfig) -> TransformResult {
    let mut output = Vec::new();
    match element.write_with_config(&mut output, config) {
        Ok(()) => TransformResult::replace(String::from_utf8_lossy(&output).into_owned()),
        Err(err) => TransformResult::error(err.to_string()),
    }
}

fn format_sql(input: &str) -> TransformResult {
    // sqlformat 0.5 takes the options by reference.
    let formatted = sqlformat::format(
        input,
        &sqlformat::QueryParams::None,
        &sqlformat::FormatOptions::default(),
    );
    TransformResult::replace(formatted)
}

fn minify_sql_transform(input: &str) -> TransformResult {
    TransformResult::replace(minify_sql(input))
}

/// Heuristic SQL minifier: `sqlformat` has no minify mode, and Boop's
/// `vkBeautify` implementation is likewise a regex-based whitespace collapser.
///
/// - Runs of whitespace collapse to at most one space.
/// - Spaces around tight punctuation (`(`, `)`, `[`, `]`, `{`, `}`, `,`, `;`,
///   `.`) are removed.
/// - Quoted strings (`'...'`, `"..."`, `` `...` ``), line comments and block
///   comments are copied verbatim so their contents are never altered.
/// - Arithmetic operators are deliberately left spaced: collapsing `a - -1`
///   into `a--1` would turn it into a line comment.
fn minify_sql(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\'' || c == '"' || c == '`' {
            push_quoted(&mut output, &mut chars, c);
            continue;
        }

        if c == '-' && chars.peek() == Some(&'-') {
            output.push('-');
            // Keep the newline: dropping it would comment out the rest of the
            // statement.
            while let Some(next) = chars.next() {
                output.push(next);
                if next == '\n' {
                    break;
                }
            }
            continue;
        }

        if c == '/' && chars.peek() == Some(&'*') {
            output.push('/');
            let mut previous = '\0';
            while let Some(next) = chars.next() {
                output.push(next);
                if previous == '*' && next == '/' {
                    break;
                }
                previous = next;
            }
            continue;
        }

        if c.is_whitespace() {
            while chars.peek().is_some_and(|next| next.is_whitespace()) {
                chars.next();
            }
            let previous = output.chars().last();
            let next = chars.peek().copied();
            if let (Some(previous), Some(next)) = (previous, next) {
                if !is_tight(previous) && !is_tight(next) {
                    output.push(' ');
                }
            }
            continue;
        }

        output.push(c);
    }

    output
}

/// Copy a quoted SQL token verbatim, honoring doubled-quote and backslash
/// escapes.
fn push_quoted(output: &mut String, chars: &mut Peekable<Chars<'_>>, quote: char) {
    output.push(quote);
    while let Some(c) = chars.next() {
        output.push(c);

        if c == '\\' {
            if let Some(escaped) = chars.next() {
                output.push(escaped);
            }
            continue;
        }

        if c == quote {
            // A doubled quote is an escaped quote, not the end of the token.
            if chars.peek() == Some(&quote) {
                if let Some(escaped) = chars.next() {
                    output.push(escaped);
                }
                continue;
            }
            return;
        }
    }
}

fn is_tight(c: char) -> bool {
    matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replaced(result: TransformResult) -> String {
        match result {
            TransformResult::Replace(text) => text,
            _ => panic!("expected TransformResult::Replace"),
        }
    }

    #[test]
    fn format_xml_indents_with_two_spaces() {
        assert_eq!(
            replaced(format_xml("<a><b>x</b></a>")),
            "<a>\n  <b>x</b>\n</a>"
        );
    }

    #[test]
    fn format_xml_omits_document_declaration() {
        let output = replaced(format_xml("<a/>"));
        assert_eq!(output, "<a/>");
        assert!(!output.contains("<?xml"));
    }

    #[test]
    fn minify_xml_removes_inter_element_whitespace() {
        assert_eq!(
            replaced(minify_xml("<a>\n  <b>x</b>\n  <c/>\n</a>")),
            "<a><b>x</b><c/></a>"
        );
    }

    #[test]
    fn minify_xml_preserves_significant_text() {
        assert_eq!(
            replaced(minify_xml("<a>\n  <b> x </b>\n</a>")),
            "<a><b> x </b></a>"
        );
    }

    #[test]
    fn invalid_xml_reports_an_error() {
        match format_xml("<a>") {
            TransformResult::Error(message) => assert!(!message.is_empty()),
            _ => panic!("expected TransformResult::Error"),
        }
    }

    #[test]
    fn minify_sql_collapses_whitespace_and_tightens_punctuation() {
        assert_eq!(
            minify_sql("SELECT  a , b\nFROM   t WHERE x = 'a  b'"),
            "SELECT a,b FROM t WHERE x = 'a  b'"
        );
    }

    #[test]
    fn minify_sql_does_not_create_line_comments() {
        assert_eq!(minify_sql("a - -1"), "a - -1");
    }

    #[test]
    fn minify_sql_preserves_quoted_comment_lookalikes() {
        assert_eq!(
            minify_sql("SELECT '--not a comment'  FROM t"),
            "SELECT '--not a comment' FROM t"
        );
    }

    #[test]
    fn format_sql_produces_output() {
        let output = replaced(format_sql("select * from users where id = 1"));
        assert!(output.contains("select"));
        assert!(output.contains("from"));
        assert!(output.contains('\n'));
    }
}
