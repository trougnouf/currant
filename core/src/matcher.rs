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
//!             | field ":" value      // field prefix (see below)
//!             | "#" genre            // genre shorthand
//!             | "*" rating           // rating shorthand
//!             | "~" duration         // duration shorthand (5m, 180s)
//!
//! Field prefixes (short and long forms):
//!   t / title        ar / artist      al / album
//!   g / genre        c / comment      y / year
//!   d / dur / duration / length
//!   * / r / rating   p / plays / play_count / playcount / count
//!
//! Value may carry a leading operator: `>=`, `<=`, `!=`, `!`, `>`, `<`, `=`.
//! The default operator is `contains` for text fields and `eq` for numeric ones.
//! `!` means not-contains (`t:!love`); `!=` means not-equals.
//!
//! A leading `-` on a free-text term negates it (`-pink`). Inside a field
//! value a dash is literal (`t:-love` matches titles containing `-love`).
//! A fully quoted term (`"kind of blue"`) is matched literally: it carries
//! no field prefix, shorthand, or operator. Quotes and backslash escapes are
//! consumed by the tokenizer anywhere inside a term.

use crate::model::{CmpOp, Field, SortPreset};
use crate::text;

/// A bound parameter value produced by the SQL compiler.
#[derive(Debug, Clone, PartialEq)]
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
                    frag.where_clause.push_str(
                        "(title_fold LIKE ? ESCAPE '\\' OR artist_fold LIKE ? ESCAPE '\\' OR album_fold LIKE ? ESCAPE '\\' OR comment_fold LIKE ? ESCAPE '\\')",
                    );
                    let p = format!("%{}%", escape_like(&text::fold(val)));
                    frag.params.push(SqlParam::Text(p.clone()));
                    frag.params.push(SqlParam::Text(p.clone()));
                    frag.params.push(SqlParam::Text(p.clone()));
                    frag.params.push(SqlParam::Text(p));
                }
                Field::Year | Field::Duration | Field::Rating | Field::PlayCount => {
                    if let Ok(n) = val.parse::<i64>() {
                        write_numeric(frag, field.column(), *op, n);
                    }
                }
                f => write_text(frag, f.fold_column(), *op, val),
            },
            SearchExpr::Num(field, op, val) => {
                write_numeric(frag, field.column(), *op, *val);
            }
            SearchExpr::And(a, b) => {
                frag.where_clause.push('(');
                a.write_sql(frag);
                frag.where_clause.push_str(" AND ");
                b.write_sql(frag);
                frag.where_clause.push(')');
            }
            SearchExpr::Or(a, b) => {
                frag.where_clause.push('(');
                a.write_sql(frag);
                frag.where_clause.push_str(" OR ");
                b.write_sql(frag);
                frag.where_clause.push(')');
            }
            SearchExpr::Not(a) => {
                frag.where_clause.push_str("NOT (");
                a.write_sql(frag);
                frag.where_clause.push(')');
            }
        }
    }
}

/// Escape LIKE wildcards so user text is matched literally: `%` and `_` are
/// wildcards by default, and `\` is the escape character declared by the
/// `ESCAPE '\'` clause on every LIKE.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("\\%"),
            '_' => out.push_str("\\_"),
            _ => out.push(c),
        }
    }
    out
}

fn write_text(frag: &mut SqlFragment, col: &str, op: CmpOp, val: &str) {
    let fv = text::fold(val);
    match op {
        CmpOp::Contains => {
            frag.where_clause
                .push_str(&format!("{col} LIKE ? ESCAPE '\\'"));
            frag.params
                .push(SqlParam::Text(format!("%{}%", escape_like(&fv))));
        }
        CmpOp::NotContains => {
            frag.where_clause
                .push_str(&format!("{col} NOT LIKE ? ESCAPE '\\'"));
            frag.params
                .push(SqlParam::Text(format!("%{}%", escape_like(&fv))));
        }
        CmpOp::Eq => {
            frag.where_clause.push_str(&format!("{col} = ?"));
            frag.params.push(SqlParam::Text(fv));
        }
        CmpOp::NotEq => {
            frag.where_clause.push_str(&format!("{col} != ?"));
            frag.params.push(SqlParam::Text(fv));
        }
        CmpOp::Gt | CmpOp::Ge | CmpOp::Lt | CmpOp::Le => {
            let sym = match op {
                CmpOp::Gt => ">",
                CmpOp::Ge => ">=",
                CmpOp::Lt => "<",
                CmpOp::Le => "<=",
                _ => unreachable!(),
            };
            frag.where_clause.push_str(&format!("{col} {sym} ?"));
            frag.params.push(SqlParam::Text(fv));
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
    frag.where_clause.push_str(&format!("{col} {sym} ?"));
    frag.params.push(SqlParam::Int(val));
}

/// Compile a sort preset into a SQL ORDER BY clause.
pub fn sort_to_order_by(sort: SortPreset) -> &'static str {
    match sort {
        SortPreset::ArtistAlbumTrack => "album_artist_fold, album_fold, track_number, title_fold",
        SortPreset::YearDesc => "year DESC, album_artist_fold, album_fold, track_number",
        SortPreset::MostPlayed => "play_count DESC, album_artist_fold, album_fold, track_number",
        SortPreset::HighestRated => "rating DESC, album_artist_fold, album_fold, track_number",
        SortPreset::Random => "RANDOM()",
        // Random album is handled by the controller (it picks an album first);
        // here it degrades to a random track order.
        SortPreset::RandomAlbum | SortPreset::RandomAlbumUniform => "RANDOM()",
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
    let s = render_node(expr, RenderCtx::Top);
    if s.is_empty() { None } else { Some(s) }
}

/// The node a rendered expression is embedded in, so the renderer can add
/// parentheses where the grammar would otherwise bind differently.
#[derive(Clone, Copy, PartialEq)]
enum RenderCtx {
    Top,
    And,
    Or,
    Not,
}

fn render_node(expr: &SearchExpr, ctx: RenderCtx) -> String {
    let s = match expr {
        SearchExpr::Str(field, op, val) => render_str(*field, *op, val),
        SearchExpr::Num(field, op, val) => render_num(*field, *op, *val),
        SearchExpr::And(a, b) => {
            let l = render_node(a, RenderCtx::And);
            let r = render_node(b, RenderCtx::And);
            if l.is_empty() {
                r
            } else if r.is_empty() {
                l
            } else {
                format!("{l} {r}")
            }
        }
        SearchExpr::Or(a, b) => {
            format!(
                "{} | {}",
                render_node(a, RenderCtx::Or),
                render_node(b, RenderCtx::Or)
            )
        }
        SearchExpr::Not(a) => format!("-{}", render_node(a, RenderCtx::Not)),
    };
    // OR inside AND/NOT and AND inside NOT would re-associate on re-parse.
    let needs_parens = matches!(
        (expr, ctx),
        (SearchExpr::Or(..), RenderCtx::And | RenderCtx::Not)
            | (SearchExpr::And(..), RenderCtx::Not)
    );
    if needs_parens { format!("({s})") } else { s }
}

fn render_str(field: Field, op: CmpOp, val: &str) -> String {
    match field {
        Field::All => render_value(op, val, true),
        Field::Genre => format!("#{}", render_value(op, val, false)),
        Field::Rating => {
            let n: i64 = val.parse().unwrap_or(0);
            format!("*{}{}", render_op(op), n)
        }
        Field::Duration => {
            let n: i64 = val.parse().unwrap_or(0);
            format!("~{}{}s", render_op(op), n)
        }
        f => format!("{}:{}", field_alias(f), render_value(op, val, false)),
    }
}

fn render_num(field: Field, op: CmpOp, val: i64) -> String {
    match field {
        Field::Rating => format!("*{}{}", render_op(op), val),
        Field::Duration => format!("~{}{}s", render_op(op), val),
        f => format!("{}:{}{}", field_alias(f), render_op(op), val),
    }
}

/// Render a text value, quoting it when it contains characters the tokenizer
/// would otherwise split or reinterpret. `free_text` adds the rules that only
/// apply to un-prefixed terms (shorthand prefixes and field prefixes).
fn render_value(op: CmpOp, val: &str, free_text: bool) -> String {
    let body = if needs_quoting(val, free_text) {
        format!("\"{}\"", escape_quoted(val))
    } else {
        val.to_string()
    };
    format!("{}{body}", render_op(op))
}

fn needs_quoting(val: &str, free_text: bool) -> bool {
    val.chars()
        .any(|c| matches!(c, ' ' | '\t' | '(' | ')' | '|' | '"' | '\\'))
        || (free_text && (val.starts_with(['#', '*', '~', '-']) || val.contains(':')))
}

/// Escape a value rendered inside double quotes.
fn escape_quoted(val: &str) -> String {
    let mut out = String::with_capacity(val.len());
    for c in val.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

fn render_op(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Contains => "",
        CmpOp::NotContains => "!",
        CmpOp::Eq => "=",
        CmpOp::NotEq => "!=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
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
    /// A term. The second field is true when the term was fully quoted
    /// (started with `"`), which makes it a literal free-text value.
    Text(String, bool),
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
                    // A dash at the end of the input or before a token
                    // boundary is a literal term.
                    None | Some(' ') | Some('\t') | Some(')') | Some('|') => {
                        tokens.push(Token::Text("-".into(), false))
                    }
                    // Otherwise it negates the following term, including
                    // `-(a b)` and `-"a b"`.
                    Some(_) => tokens.push(Token::NotPrefix),
                }
            }
            _ => {
                let mut term = String::new();
                let mut quoted = false;
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
                        // A quote opening an otherwise empty term marks it
                        // as a fully quoted (literal) term.
                        if term.is_empty() && !in_quote {
                            quoted = true;
                        }
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
                    tokens.push(Token::Text(term, quoted));
                }
            }
        }
    }
    tokens
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
            Some(Token::Text(t, quoted)) => {
                let term = t.clone();
                let quoted = *quoted;
                self.advance();
                Self::parse_term(term, quoted)
            }
            _ => SearchExpr::Str(Field::All, CmpOp::Contains, String::new()),
        }
    }

    fn parse_term(term: String, quoted: bool) -> SearchExpr {
        // A fully quoted term is a literal free-text value: no field prefix,
        // no shorthand, no operator.
        if quoted {
            return SearchExpr::Str(Field::All, CmpOp::Contains, term);
        }
        // Genre shorthand: #jazz  /  #!jazz
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

/// Split a text value into operator + value, where a bare value means
/// `contains`. `!=` (not-equals) must be checked before `!` (not-contains);
/// a leading `!` otherwise means not-contains.
fn split_text_op(value: &str) -> (CmpOp, &str) {
    if let Some(rest) = value.strip_prefix("!=") {
        return (CmpOp::NotEq, rest);
    }
    if let Some(rest) = value.strip_prefix('!') {
        return (CmpOp::NotContains, rest);
    }
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
        assert!(sql.where_clause.contains("artist_fold LIKE ?"));
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
    fn parses_named_field_aliases() {
        let e = parse_query("rating:>=4");
        assert!(e.to_sql().where_clause.contains("rating >= ?"));

        let e = parse_query("title:love");
        assert!(e.to_sql().where_clause.contains("title_fold LIKE ?"));

        let e = parse_query("artist:pink");
        assert!(e.to_sql().where_clause.contains("artist_fold LIKE ?"));

        let e = parse_query("album:kind");
        assert!(e.to_sql().where_clause.contains("album_fold LIKE ?"));

        let e = parse_query("genre:jazz");
        assert!(e.to_sql().where_clause.contains("genre_fold LIKE ?"));

        let e = parse_query("playcount:0");
        assert!(e.to_sql().where_clause.contains("play_count = ?"));

        let e = parse_query("length:>5m");
        assert!(e.to_sql().where_clause.contains("duration_secs > ?"));
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
        assert!(sql.where_clause.contains("album_fold = ?"));
    }

    #[test]
    fn escapes_like_wildcards() {
        // `%` and `_` in user text must be matched literally.
        let e = parse_query("c:100%");
        match &e.to_sql().params[0] {
            SqlParam::Text(t) => assert_eq!(t, "%100\\%%"),
            _ => panic!("expected text param"),
        }

        let e = parse_query("c:under_score");
        match &e.to_sql().params[0] {
            SqlParam::Text(t) => assert_eq!(t, "%under\\_score%"),
            _ => panic!("expected text param"),
        }
    }

    #[test]
    fn parses_not_contains() {
        let e = parse_query("t:!love");
        let sql = e.to_sql();
        assert!(sql.where_clause.contains("title_fold NOT LIKE ?"));

        let e = parse_query("t:-love");
        let sql = e.to_sql();
        // A dash inside a field value is literal, not a negation.
        assert!(sql.where_clause.contains("title_fold LIKE ?"));
        match &sql.params[0] {
            SqlParam::Text(t) => assert_eq!(t, "%-love%"),
            _ => panic!("expected text param"),
        }
    }

    #[test]
    fn parses_quoted_term_as_literal() {
        // A fully quoted term defeats the genre shorthand.
        let e = parse_query("\"#jazz\"");
        let sql = e.to_sql();
        assert!(sql.where_clause.contains("title_fold LIKE ?"));
        match &sql.params[0] {
            SqlParam::Text(t) => assert_eq!(t, "%#jazz%"),
            _ => panic!("expected text param"),
        }

        // And it defeats the negation prefix.
        let e = parse_query("-\"a b\"");
        let sql = e.to_sql();
        assert!(sql.where_clause.contains("NOT"));
        match &sql.params[0] {
            SqlParam::Text(t) => assert_eq!(t, "%a b%"),
            _ => panic!("expected text param"),
        }
    }

    /// `expr_to_query` must be lossless: parsing a rendered expression back
    /// must produce an equivalent SQL fragment.
    #[test]
    fn query_round_trip() {
        let queries = [
            "pink",
            "ar:pink",
            "ar:kind of blue",
            "al:=kind of blue",
            "al:!=kind of blue",
            "t:!love",
            "t:-love",
            "year:>=1990",
            "*>=4",
            "~>5m",
            "p:0",
            "-pink",
            "jazz -pink | classical",
            "(a b) c",
            "a | (b c)",
            "\"quoted text\"",
            "c:a\"b",
            "c:100% pure",
            "c:under_score",
            "#jazz",
            "#!jazz",
            "-(a b)",
            "-\"a b\"",
            "a|b",
        ];
        for q in queries {
            let e1 = parse_query(q);
            let s1 = e1.to_sql();
            let rendered = expr_to_query(&e1).unwrap_or_default();
            let s2 = parse_query(&rendered).to_sql();
            assert_eq!(
                s1.where_clause, s2.where_clause,
                "where_clause mismatch for {q:?} -> {rendered:?}"
            );
            assert_eq!(
                s1.params, s2.params,
                "params mismatch for {q:?} -> {rendered:?}"
            );
        }
    }
}
