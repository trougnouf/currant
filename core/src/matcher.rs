// ./core/src/matcher.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! The query language and its compiler to SQL.
//!
//! Grammar (informal):
//!   expr     := or
//!   or       := and ( "|" and )*
//!   and      := unary+              // implicit AND on whitespace
//!   unary    := "-"? primary
//!   primary  := "(" expr ")" | term
//!   term     := free_text
//!             | field ":" value      // field: title/ar/al/g/c/y/d/*/r/p
//!             | "#" genre            // genre shorthand
//!             | "*" rating           // rating shorthand
//!             | "~" duration         // duration shorthand (5m, 180s)
//!
//! Value may carry a leading operator: `>=`, `<=`, `!=`, `>`, `<`, `=`.
//! The default operator is `contains` for text fields and `eq` for numeric ones.

use crate::model::{CmpOp, Field, SortPreset};

/// A bound parameter value produced by the SQL compiler.
#[derive(Debug, Clone)]
pub enum SqlParam {
    Text(String),
    Int(i64),
}

/// The compiled WHERE fragment plus its bound parameters.
#[derive(Debug, Clone, Default)]
pub struct SqlFragment {
    pub where_clause: String,
    pub params: Vec<SqlParam>,
}

impl SqlFragment {
    fn push(&mut self, sql: &str) {
        if !self.where_clause.is_empty() {
            self.where_clause.push_str(" AND ");
        }
        self.where_clause.push_str(sql);
    }
}

/// A compiled query: WHERE fragment and the ORDER BY clause.
#[derive(Debug, Clone)]
pub struct CompiledQuery {
    pub where_clause: String,
    pub params: Vec<SqlParam>,
    pub order_by: String,
}

/// The query AST.
#[derive(Debug, Clone)]
pub enum SearchExpr {
    /// Text field comparison (title/artist/album/genre/comment, and `All`).
    Str(Field, CmpOp, String),
    /// Numeric field comparison (year/duration/rating/play_count).
    Num(Field, CmpOp, i64),
    And(Box<SearchExpr>, Box<SearchExpr>),
    Or(Box<SearchExpr>, Box<SearchExpr>),
    Not(Box<SearchExpr>),
}

impl SearchExpr {
    /// Compile into a SQL WHERE fragment for the `tracks` table.
    pub fn to_sql(&self) -> SqlFragment {
        let mut frag = SqlFragment::default();
        self.write_sql(&mut frag);
        frag
    }

    fn write_sql(&self, frag: &mut SqlFragment) {
        match self {
            SearchExpr::Str(field, op, val) => match field {
                Field::All => {
                    let like = "(title LIKE ? OR artist LIKE ? OR album LIKE ?)".to_string();
                    frag.push(&like);
                    let p = format!("%{val}%");
                    frag.params.push(SqlParam::Text(p.clone()));
                    frag.params.push(SqlParam::Text(p.clone()));
                    frag.params.push(SqlParam::Text(p));
                }
                Field::Year | Field::Duration | Field::Rating | Field::PlayCount => {
                    // A text op applied to a numeric column: coerce if possible.
                    if let Ok(n) = val.parse::<i64>() {
                        write_numeric(frag, field.column(), *op, n);
                    }
                }
                f => write_text(frag, f.column(), *op, val),
            },
            SearchExpr::Num(field, op, val) => {
                write_numeric(frag, field.column(), *op, *val);
            }
            SearchExpr::And(a, b) => {
                frag.push("(");
                a.write_sql(frag);
                frag.where_clause.push_str(" AND ");
                b.write_sql(frag);
                frag.where_clause.push(')');
            }
            SearchExpr::Or(a, b) => {
                frag.push("(");
                a.write_sql(frag);
                frag.where_clause.push_str(" OR ");
                b.write_sql(frag);
                frag.where_clause.push(')');
            }
            SearchExpr::Not(a) => {
                frag.push("NOT (");
                a.write_sql(frag);
                frag.where_clause.push(')');
            }
        }
    }
}

fn write_text(frag: &mut SqlFragment, col: &str, op: CmpOp, val: &str) {
    match op {
        CmpOp::Contains => {
            frag.push(&format!("{col} LIKE ?"));
            frag.params.push(SqlParam::Text(format!("%{val}%")));
        }
        CmpOp::NotContains => {
            frag.push(&format!("{col} NOT LIKE ?"));
            frag.params.push(SqlParam::Text(format!("%{val}%")));
        }
        CmpOp::Eq => {
            frag.push(&format!("lower({col}) = lower(?)"));
            frag.params.push(SqlParam::Text(val.to_string()));
        }
        CmpOp::NotEq => {
            frag.push(&format!("lower({col}) != lower(?)"));
            frag.params.push(SqlParam::Text(val.to_string()));
        }
        CmpOp::Gt | CmpOp::Ge | CmpOp::Lt | CmpOp::Le => {
            // Lexicographic comparison on a text column.
            let sym = match op {
                CmpOp::Gt => ">",
                CmpOp::Ge => ">=",
                CmpOp::Lt => "<",
                CmpOp::Le => "<=",
                _ => unreachable!(),
            };
            frag.push(&format!("{col} {sym} ?"));
            frag.params.push(SqlParam::Text(val.to_string()));
        }
    }
}

fn write_numeric(frag: &mut SqlFragment, col: &str, op: CmpOp, val: i64) {
    let sym = match op {
        CmpOp::Eq => "=",
        CmpOp::NotEq => "!=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Contains => "=",
        CmpOp::NotContains => "!=",
    };
    frag.push(&format!("{col} {sym} ?"));
    frag.params.push(SqlParam::Int(val));
}

/// Compile a sort preset into a SQL ORDER BY clause.
pub fn sort_to_order_by(sort: SortPreset) -> &'static str {
    match sort {
        SortPreset::ArtistAlbumTrack => "artist, album, track_number, title",
        SortPreset::YearDesc => "year DESC, artist, album, track_number",
        SortPreset::MostPlayed => "play_count DESC, artist, album, track_number",
        SortPreset::HighestRated => "rating DESC, artist, album, track_number",
        SortPreset::Random => "RANDOM()",
        // Random album is handled by the controller (it picks an album first);
        // here it degrades to a random track order.
        SortPreset::RandomAlbum => "RANDOM()",
        SortPreset::Path => "path, title",
    }
}

/// Parse a query string into an AST. An empty input matches everything.
pub fn parse_query(input: &str) -> SearchExpr {
    let tokens = tokenize(input);
    let mut parser = Parser::new(tokens);
    parser.parse()
}

/// True when the expression is the trivial "match all" form.
pub fn is_empty(expr: &SearchExpr) -> bool {
    matches!(expr, SearchExpr::Str(Field::All, CmpOp::Contains, s) if s.is_empty())
}

/// Serialize a `SearchExpr` back into a query string. Returns `None` for the
/// trivial match-all expression (so an empty string is stored).
pub fn expr_to_query(expr: &SearchExpr) -> Option<String> {
    let s = render_expr(expr);
    if s.is_empty() { None } else { Some(s) }
}

fn render_expr(expr: &SearchExpr) -> String {
    match expr {
        SearchExpr::Str(field, op, val) => match field {
            Field::All => {
                if val.is_empty() {
                    String::new()
                } else {
                    val.clone()
                }
            }
            Field::Genre => format!("#{}", render_text_op(*op, val)),
            Field::Rating => {
                let n: i64 = val.parse().unwrap_or(0);
                format!("*{}{}", render_op(*op), n)
            }
            Field::Duration => {
                let n: i64 = val.parse().unwrap_or(0);
                format!("~{}{}s", render_op(*op), n)
            }
            f => format!("{}:{}", field_alias(*f), render_text_op(*op, val)),
        },
        SearchExpr::Num(field, op, val) => match field {
            Field::Rating => format!("*{}{}", render_op(*op), val),
            Field::Duration => format!("~{}{}s", render_op(*op), val),
            f => format!("{}:{}{}", field_alias(*f), render_op(*op), val),
        },
        SearchExpr::And(a, b) => {
            let l = render_expr(a);
            let r = render_expr(b);
            if l.is_empty() {
                r
            } else if r.is_empty() {
                l
            } else {
                format!("{l} {r}")
            }
        }
        SearchExpr::Or(a, b) => format!("{} | {}", render_expr(a), render_expr(b)),
        SearchExpr::Not(a) => format!("-{}", render_expr(a)),
    }
}

fn render_op(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Contains => "",
        CmpOp::NotContains => "!=",
        CmpOp::Eq => "=",
        CmpOp::NotEq => "!=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
    }
}

fn render_text_op(op: CmpOp, val: &str) -> String {
    match op {
        CmpOp::Contains => val.to_string(),
        CmpOp::NotContains => format!("!{val}"),
        _ => format!("{}{}", render_op(op), val),
    }
}

fn field_alias(field: Field) -> &'static str {
    match field {
        Field::Title => "t",
        Field::Artist => "ar",
        Field::Album => "al",
        Field::Genre => "g",
        Field::Comment => "c",
        Field::Year => "year",
        Field::Duration => "d",
        Field::Rating => "*",
        Field::PlayCount => "p",
        Field::All => "",
    }
}

#[derive(Debug, PartialEq, Clone)]
enum Token {
    Text(String),
    Or,
    LParen,
    RParen,
    NotPrefix,
}

fn is_delim(c: char) -> bool {
    c == ' ' || c == '\t' || c == '(' || c == ')' || c == '|'
}

fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' => {
                chars.next();
            }
            '(' => {
                tokens.push(Token::LParen);
                chars.next();
            }
            ')' => {
                tokens.push(Token::RParen);
                chars.next();
            }
            '|' => {
                tokens.push(Token::Or);
                chars.next();
            }
            '-' => {
                chars.next();
                match chars.peek() {
                    None => tokens.push(Token::Text("-".into())),
                    Some(&n) if is_delim(n) || n == '"' => tokens.push(Token::Text("-".into())),
                    // A leading dash before any other token is a negation.
                    Some(_) => tokens.push(Token::NotPrefix),
                }
            }
            _ => {
                let mut term = String::new();
                let mut in_quote = false;
                let mut escaped = false;
                while let Some(&c) = chars.peek() {
                    if escaped {
                        term.push(c);
                        chars.next();
                        escaped = false;
                    } else if c == '\\' {
                        chars.next();
                        escaped = true;
                    } else if c == '"' {
                        in_quote = !in_quote;
                        chars.next();
                    } else if !in_quote && is_delim(c) {
                        break;
                    } else {
                        term.push(c);
                        chars.next();
                    }
                }
                if !term.is_empty() {
                    tokens.push(Token::Text(term));
                }
            }
        }
    }
    tokens
}

/// A term counts as a "field term" if it starts with a recognised prefix or
/// shorthand. Kept for documentation; negation now applies to any token.
#[allow(dead_code)]
fn is_field_term(term: &str) -> bool {
    term == term.trim() && {
        term.starts_with('#')
            || term.starts_with('*')
            || term.starts_with('~')
            || term
                .split_once(':')
                .is_some_and(|(p, _)| Field::from_alias(p.trim()).is_some())
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn parse(&mut self) -> SearchExpr {
        if self.tokens.is_empty() {
            return SearchExpr::Str(Field::All, CmpOp::Contains, String::new());
        }
        let expr = self.parse_or();
        if self.pos < self.tokens.len() {
            // Leftover tokens: fold the rest as an implicit AND (forgiving parser).
            let rest = self.parse_or();
            return SearchExpr::And(Box::new(expr), Box::new(rest));
        }
        expr
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }
    fn advance(&mut self) {
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
    }

    fn parse_or(&mut self) -> SearchExpr {
        let mut left = self.parse_and();
        while let Some(Token::Or) = self.peek() {
            self.advance();
            left = SearchExpr::Or(Box::new(left), Box::new(self.parse_and()));
        }
        left
    }

    fn parse_and(&mut self) -> SearchExpr {
        let mut left = self.parse_unary();
        while let Some(token) = self.peek() {
            if matches!(token, Token::Or | Token::RParen) {
                break;
            }
            left = SearchExpr::And(Box::new(left), Box::new(self.parse_unary()));
        }
        left
    }

    fn parse_unary(&mut self) -> SearchExpr {
        if let Some(Token::NotPrefix) = self.peek() {
            self.advance();
            return SearchExpr::Not(Box::new(self.parse_primary()));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> SearchExpr {
        match self.peek() {
            Some(Token::LParen) => {
                self.advance();
                let expr = self.parse_or();
                if let Some(Token::RParen) = self.peek() {
                    self.advance();
                }
                expr
            }
            Some(Token::Text(t)) => {
                let term = t.clone();
                self.advance();
                Self::parse_term(term)
            }
            _ => SearchExpr::Str(Field::All, CmpOp::Contains, String::new()),
        }
    }

    fn parse_term(term: String) -> SearchExpr {
        // Genre shorthand: #jazz  /  #-jazz
        if let Some(stripped) = term.strip_prefix('#') {
            let (op, val) = split_text_op(stripped);
            return SearchExpr::Str(Field::Genre, op, val.to_string());
        }
        // Rating shorthand: *>=3
        if let Some(stripped) = term.strip_prefix('*') {
            let (op, val) = CmpOp::split_prefix(stripped);
            let n = val.parse::<i64>().unwrap_or(0);
            return SearchExpr::Num(Field::Rating, op, n);
        }
        // Duration shorthand: ~>5m  /  ~180s
        if let Some(stripped) = term.strip_prefix('~') {
            let (op, rest) = CmpOp::split_prefix(stripped);
            let secs = parse_duration(rest);
            return SearchExpr::Num(Field::Duration, op, secs);
        }
        // Prefixed fields: ar:foo, year:>=1990
        if let Some((prefix, value)) = term.split_once(':')
            && let Some(field) = Field::from_alias(prefix.trim())
        {
            return Self::parse_field_value(field, value);
        }
        // Free text.
        SearchExpr::Str(Field::All, CmpOp::Contains, term)
    }

    fn parse_field_value(field: Field, value: &str) -> SearchExpr {
        if field.is_text() {
            let (op, val) = split_text_op(value);
            SearchExpr::Str(field, op, val.to_string())
        } else if field == Field::Duration {
            let (op, rest) = CmpOp::split_prefix(value);
            SearchExpr::Num(field, op, parse_duration(rest))
        } else {
            let (op, rest) = CmpOp::split_prefix(value);
            let n = rest.parse::<i64>().unwrap_or(0);
            SearchExpr::Num(field, op, n)
        }
    }
}

/// Split a text value into operator + value, where a bare value means `contains`.
fn split_text_op(value: &str) -> (CmpOp, &str) {
    let (op, rest) = CmpOp::split_prefix(value);
    (op, rest)
}

/// Parse a duration shorthand: `5m`, `180s`, `1h30m`.
fn parse_duration(s: &str) -> i64 {
    let mut total: i64 = 0;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: i64 = num.parse().unwrap_or(0);
            num.clear();
            total += match c {
                'h' => n * 3600,
                'm' => n * 60,
                's' => n,
                _ => 0,
            };
        }
    }
    if !num.is_empty() {
        // A trailing bare number counts as seconds.
        total += num.parse::<i64>().unwrap_or(0);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_as_match_all() {
        let e = parse_query("");
        assert!(is_empty(&e));
    }

    #[test]
    fn parses_field_contains() {
        let e = parse_query("ar:pink");
        let sql = e.to_sql();
        assert!(sql.where_clause.contains("artist LIKE ?"));
        assert_eq!(sql.params.len(), 1);
        match &sql.params[0] {
            SqlParam::Text(t) => assert_eq!(t, "%pink%"),
            _ => panic!("expected text param"),
        }
    }

    #[test]
    fn parses_year_inequality() {
        let e = parse_query("year:>=1990");
        let sql = e.to_sql();
        assert!(sql.where_clause.contains("year >= ?"));
    }

    #[test]
    fn parses_rating_shorthand() {
        let e = parse_query("*>=4");
        let sql = e.to_sql();
        assert!(sql.where_clause.contains("rating >= ?"));
    }

    #[test]
    fn parses_duration_shorthand() {
        let e = parse_query("~>5m");
        let sql = e.to_sql();
        assert!(sql.where_clause.contains("duration_secs > ?"));
        match &sql.params[0] {
            SqlParam::Int(n) => assert_eq!(*n, 300),
            _ => panic!("expected int param"),
        }
    }

    #[test]
    fn parses_not_and_or() {
        let e = parse_query("jazz -pink | classical");
        let sql = e.to_sql();
        assert!(sql.where_clause.contains("NOT"));
        assert!(sql.where_clause.contains("OR"));
    }

    #[test]
    fn parses_equals_and_not_equals() {
        let e = parse_query("al:=kind of blue");
        let sql = e.to_sql();
        assert!(sql.where_clause.contains("lower(album) = lower(?)"));
    }
}
