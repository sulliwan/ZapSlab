//! Safe stand-in for Boop's `EvalJavascript.js`.
//!
//! The Boop script runs the text through JavaScript's `eval`, then appends the
//! result as a `// Result:` block. Executing arbitrary code is not an option
//! here, so the "script" is parsed as a small arithmetic expression instead.
//! Only decimal numbers, `+ - * / %`, parentheses, and unary signs are
//! accepted; identifiers, calls, string/array/object literals, `Math`, and
//! every other JS construct are rejected with an error rather than executed.
//! The output shape is unchanged: the original expression text (minus any stale
//! result block) followed by the same `// Result:` block.

use super::{Scope, Transform, TransformResult};

pub const TRANSFORMS: &[Transform] = &[Transform {
    name: "Eval Javascript",
    description: "Evaluate a numeric expression (+ - * / %, parentheses, unary signs) and append a // Result: block; identifiers, calls, and literals are rejected.",
    tags: "js,script,run",
    scope: Scope::SelectionOrFullText,
    apply: eval_script,
}];

/// Boop strips `/\n\n\/\/ Result:[\s\S]*$/` before evaluating. Because
/// `[\s\S]*` runs greedily to the end of the string, the first occurrence of
/// the marker is where the strip takes effect.
const RESULT_MARKER: &str = "\n\n// Result:";

fn eval_script(input: &str) -> TransformResult {
    let script = match input.find(RESULT_MARKER) {
        Some(index) => &input[..index],
        None => input,
    };

    let value = match evaluate(script) {
        Ok(value) => value,
        Err(message) => return TransformResult::error(message),
    };
    let rendered = match format_result(value) {
        Some(rendered) => rendered,
        None => return TransformResult::error("Result is not a finite number"),
    };

    TransformResult::replace(format!("{script}\n\n// Result:\n\n{rendered}"))
}

/// Renders a finite result. Rust's `Display` for `f64` already omits the
/// decimal point on integral values (`3`, not `3.0`) and otherwise prints the
/// shortest representation that round-trips; `-0.0` is normalized to `0` the
/// way JS `String(-0)` does.
fn format_result(value: f64) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    if value == 0.0 {
        return Some("0".to_string());
    }
    Some(value.to_string())
}

fn evaluate(expression: &str) -> Result<f64, String> {
    let mut parser = Parser::new(expression);
    let value = parser.parse_expression()?;
    parser.skip_whitespace();
    if parser.at_end() {
        Ok(value)
    } else {
        Err(format!(
            "Unexpected character '{}'",
            parser.current_char().unwrap_or('?')
        ))
    }
}

/// Recursive-descent parser for
/// `expression := term (("+" | "-") term)*`,
/// `term := unary (("*" | "/" | "%") unary)*`,
/// `unary := ("+" | "-") unary | primary`,
/// `primary := number | "(" expression ")"`.
struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            position: 0,
        }
    }

    fn at_end(&self) -> bool {
        self.position >= self.bytes.len()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn current_char(&self) -> Option<char> {
        self.input[self.position..].chars().next()
    }

    fn skip_whitespace(&mut self) {
        while let Some(byte) = self.peek() {
            if byte.is_ascii_whitespace() {
                self.position += 1;
            } else {
                break;
            }
        }
    }

    fn parse_expression(&mut self) -> Result<f64, String> {
        let mut value = self.parse_term()?;
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'+') => {
                    self.position += 1;
                    value += self.parse_term()?;
                }
                Some(b'-') => {
                    self.position += 1;
                    value -= self.parse_term()?;
                }
                _ => return Ok(value),
            }
        }
    }

    fn parse_term(&mut self) -> Result<f64, String> {
        let mut value = self.parse_unary()?;
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'*') => {
                    self.position += 1;
                    value *= self.parse_unary()?;
                }
                Some(b'/') => {
                    self.position += 1;
                    let divisor = self.parse_unary()?;
                    if divisor == 0.0 {
                        return Err("Division by zero".to_string());
                    }
                    value /= divisor;
                }
                Some(b'%') => {
                    self.position += 1;
                    let divisor = self.parse_unary()?;
                    if divisor == 0.0 {
                        return Err("Modulo by zero".to_string());
                    }
                    value %= divisor;
                }
                _ => return Ok(value),
            }
        }
    }

    fn parse_unary(&mut self) -> Result<f64, String> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'+') => {
                self.position += 1;
                self.parse_unary()
            }
            Some(b'-') => {
                self.position += 1;
                Ok(-self.parse_unary()?)
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<f64, String> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'(') => {
                self.position += 1;
                let value = self.parse_expression()?;
                self.skip_whitespace();
                if self.peek() == Some(b')') {
                    self.position += 1;
                    Ok(value)
                } else {
                    Err("Unbalanced parentheses: expected ')'".to_string())
                }
            }
            Some(byte) if byte.is_ascii_digit() || byte == b'.' => self.parse_number(),
            Some(_) => Err(format!(
                "Unexpected character '{}'",
                self.current_char().unwrap_or('?')
            )),
            None => Err("Unexpected end of expression".to_string()),
        }
    }

    /// Decimal literals only: digits with an optional fractional part. JS also
    /// accepts exponents, hex, `Infinity`, and `NaN`; those are out of scope
    /// for the safe evaluator.
    fn parse_number(&mut self) -> Result<f64, String> {
        let start = self.position;
        let mut has_digit = false;

        while let Some(byte) = self.peek() {
            if byte.is_ascii_digit() {
                has_digit = true;
                self.position += 1;
            } else {
                break;
            }
        }

        if self.peek() == Some(b'.') {
            self.position += 1;
            while let Some(byte) = self.peek() {
                if byte.is_ascii_digit() {
                    has_digit = true;
                    self.position += 1;
                } else {
                    break;
                }
            }
        }

        if !has_digit {
            return Err("Expected a number".to_string());
        }

        self.input[start..self.position]
            .parse::<f64>()
            .map_err(|_| "Invalid number".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(result: TransformResult) -> String {
        match result {
            TransformResult::Replace(text) => text,
            _ => panic!("expected a replacement"),
        }
    }

    fn error(result: TransformResult) -> String {
        match result {
            TransformResult::Error(message) => message,
            _ => panic!("expected an error"),
        }
    }

    fn result_of(expression: &str) -> String {
        let output = text(eval_script(expression));
        match output.split_once("\n\n// Result:\n\n") {
            Some((_, result)) => result.to_string(),
            None => panic!("missing result block in {output:?}"),
        }
    }

    #[test]
    fn appends_the_script_and_result_block() {
        assert_eq!(text(eval_script("1 + 2")), "1 + 2\n\n// Result:\n\n3");
    }

    #[test]
    fn preserves_the_script_verbatim() {
        assert_eq!(
            text(eval_script("  1 + 1  ")),
            "  1 + 1  \n\n// Result:\n\n2"
        );
    }

    #[test]
    fn strips_a_previous_result_block_before_evaluating() {
        let stale = "3 * 3\n\n// Result:\n\n999";
        assert_eq!(text(eval_script(stale)), "3 * 3\n\n// Result:\n\n9");
        // Running it twice is idempotent.
        let once = text(eval_script("2 * 3"));
        assert_eq!(text(eval_script(&once)), once);
    }

    #[test]
    fn honours_precedence() {
        assert_eq!(result_of("2 + 3 * 4"), "14");
        assert_eq!(result_of("2 * 3 + 4"), "10");
        assert_eq!(result_of("2 - 3 - 4"), "-5");
        assert_eq!(result_of("8 / 4 / 2"), "1");
        assert_eq!(result_of("10 % 4 * 2"), "4");
    }

    #[test]
    fn honours_parentheses() {
        assert_eq!(result_of("(2 + 3) * 4"), "20");
        assert_eq!(result_of("2 * (3 + (4 - 1))"), "12");
        assert_eq!(result_of("((7))"), "7");
    }

    #[test]
    fn honours_unary_signs() {
        assert_eq!(result_of("-5 + 2"), "-3");
        assert_eq!(result_of("-(2 + 3)"), "-5");
        assert_eq!(result_of("+4"), "4");
        assert_eq!(result_of("3 * -2"), "-6");
        assert_eq!(result_of("-2 * -3"), "6");
    }

    #[test]
    fn handles_decimals() {
        assert_eq!(result_of("7 / 2"), "3.5");
        assert_eq!(result_of("6 / 3"), "2");
        assert_eq!(result_of("0.1 + 0.2"), "0.30000000000000004");
        assert_eq!(result_of(".5 + 5."), "5.5");
        assert_eq!(result_of("1 / 3"), "0.3333333333333333");
    }

    #[test]
    fn normalizes_negative_zero() {
        assert_eq!(result_of("0 * -1"), "0");
    }

    #[test]
    fn tolerates_whitespace_and_newlines() {
        assert_eq!(result_of(" ( 1 +\n\t2 ) * 3 "), "9");
    }

    #[test]
    fn reports_division_and_modulo_by_zero() {
        assert_eq!(error(eval_script("1 / 0")), "Division by zero");
        assert_eq!(error(eval_script("1 % 0")), "Modulo by zero");
    }

    #[test]
    fn reports_parse_errors() {
        assert_eq!(error(eval_script("")), "Unexpected end of expression");
        assert_eq!(error(eval_script("1 +")), "Unexpected end of expression");
        assert_eq!(error(eval_script("1 + * 2")), "Unexpected character '*'");
        assert_eq!(error(eval_script("(1 + 2")), "Unbalanced parentheses: expected ')'");
        assert_eq!(error(eval_script("1 2")), "Unexpected character '2'");
        assert_eq!(error(eval_script("1)")), "Unexpected character ')'");
        assert_eq!(error(eval_script("abc")), "Unexpected character 'a'");
        assert_eq!(error(eval_script(".")), "Expected a number");
    }

    #[test]
    fn rejects_non_finite_results() {
        let huge = "9".repeat(400);
        assert_eq!(error(eval_script(&huge)), "Result is not a finite number");
    }
}
