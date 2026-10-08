// HexDB Core SQL
//
// A read-only SQL dialect over tessellations, for tools that speak SQL (BI
// tools, the ODBC driver). A statement is parsed with sqlparser and translated
// into the query or aggregation the REST API already runs, so indexes, row
// filters and field masks apply exactly as they do there.
//
//   SELECT customer, COUNT(*) AS orders, SUM(total) AS spent
//   FROM orders
//   WHERE status IN ('paid', 'shipped') AND placed_at >= '2026-01-01'
//   GROUP BY customer
//   HAVING SUM(total) > 100
//   ORDER BY spent DESC
//   LIMIT 10
//
// Supported:
// - SELECT [DISTINCT] with *, fields (dotted paths into objects), literals,
//   and COUNT(*), COUNT(x), COUNT(DISTINCT x), SUM, AVG, MIN, MAX;
// - FROM one tessellation, with an optional alias; SELECT without FROM for
//   literals (`SELECT 1`);
// - WHERE: comparisons between a field and a value, AND, OR, NOT, IN,
//   BETWEEN, LIKE (exact, prefix, suffix or substring patterns),
//   IS [NOT] NULL, IS [NOT] TRUE/FALSE, and a bare boolean field;
// - GROUP BY fields, HAVING, ORDER BY (fields, aliases, aggregates or
//   positions), LIMIT, OFFSET, FETCH FIRST and TOP;
// - parameters: `?` (numbered in order of appearance) or `$1`, `$2`, ...
//
// Comparisons follow SQL's NULL rules: a comparison with NULL matches nothing,
// and `x <> 1`, `NOT x = 1` and `x NOT IN (...)` don't match documents where x
// is missing or null. Identifiers are case-sensitive, like HexDB field names.
//
// Not supported: joins, subqueries, UNION, expressions over fields (`total *
// 2`, `UPPER(name)`), comparing two fields, and statements other than SELECT.

use crate::{
    aggregate::{AggregateOp, AggregateSpec, Aggregation, MAX_GROUPS},
    engine::{DocumentQuery, EngineError, HexDBEngine},
    filter::{compare, Filter, SortKey},
};
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Map, Number, Value};
use sqlparser::ast::{
    self as ast, BinaryOperator, Distinct, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, LimitClause,
    ObjectName, OrderByKind, OrderBySort, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Statement, TableFactor,
    UnaryOperator,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer};
use std::cmp::Ordering;
use std::collections::HashMap;

/// Rows per response unless the request asks for another page size.
pub const DEFAULT_SQL_PAGE_SIZE: usize = 1000;
/// Most rows one response may hold.
pub const MAX_SQL_PAGE_SIZE: usize = 10_000;
/// Longest statement accepted.
pub const MAX_SQL_LENGTH: usize = 100_000;

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(format!("sql: {}", message.into())).into()
}

fn unsupported(what: impl std::fmt::Display) -> anyhow::Error {
    invalid(format!("{} isn't supported.", what))
}

// ---------------------------------------------------------------------------
// Parsed statements
// ---------------------------------------------------------------------------

/// A translated SELECT statement, ready to run.
#[derive(Debug, Clone)]
pub struct SqlQuery {
    tessellation: Option<String>,
    body: Body,
    offset: usize,
    limit: Option<usize>,
}

#[derive(Debug, Clone)]
enum Body {
    /// SELECT without FROM: one row of literals.
    Constant(Vec<(String, Value)>),
    Documents {
        filter: Value,
        sort: Vec<SortKey>,
        columns: Vec<DocColumn>,
    },
    Aggregate {
        filter: Value,
        group_by: Vec<String>,
        aggregates: Vec<AggregateSpec>,
        sort: Vec<SortKey>,
        having: Option<(Having, String)>,
        /// Output columns: a key of the aggregation's rows, or a literal.
        columns: Vec<(String, Source)>,
        /// Only COUNT(*) without GROUP BY or HAVING: answered by a count.
        count_only: bool,
        /// (SUM, COUNT of the same field): SQL's SUM of no values is NULL, not 0.
        sum_guards: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone)]
enum DocColumn {
    /// `*`: `id` and every field, in the order they first appear (without
    /// metadata such as `_schema` and `_expires_at`).
    All,
    Field { name: String, path: Vec<String> },
    Constant { name: String, value: Value },
}

#[derive(Debug, Clone)]
enum Source {
    Key(String),
    Constant(Value),
}

/// A HAVING condition, evaluated per group with SQL's three-valued logic.
#[derive(Debug, Clone)]
enum Having {
    And(Vec<Having>),
    Or(Vec<Having>),
    Not(Box<Having>),
    Compare(Operand, BinaryOperator, Operand),
    IsNull(Operand),
    In(Operand, Vec<Value>),
    Constant(Option<bool>),
}

#[derive(Debug, Clone)]
enum Operand {
    Key(String),
    Value(Value),
}

/// A WHERE condition being translated: everything, nothing, or a filter.
enum Cond {
    All,
    Nothing,
    Filter(Value),
}

impl Cond {
    fn constant(value: Option<bool>) -> Cond {
        if value == Some(true) {
            Cond::All
        } else {
            Cond::Nothing
        }
    }

    fn combine(and: bool, parts: Vec<Cond>) -> Cond {
        let mut filters = Vec::new();
        for part in parts {
            match (part, and) {
                (Cond::All, true) | (Cond::Nothing, false) => {}
                (Cond::All, false) => return Cond::All,
                (Cond::Nothing, true) => return Cond::Nothing,
                (Cond::Filter(f), _) => {
                    let key = if and { "$and" } else { "$or" };
                    match f {
                        // Flatten nested groups of the same kind.
                        Value::Object(mut map) if map.len() == 1 && map.contains_key(key) => {
                            if let Some(Value::Array(items)) = map.remove(key) {
                                filters.extend(items);
                            }
                        }
                        other => filters.push(other),
                    }
                }
            }
        }
        match filters.len() {
            0 if and => Cond::All,
            0 => Cond::Nothing,
            1 => Cond::Filter(filters.pop().unwrap_or(Value::Null)),
            _ => Cond::Filter(json!({ (if and { "$and" } else { "$or" }): filters })),
        }
    }

    fn into_json(self) -> Value {
        match self {
            Cond::All => json!({}),
            Cond::Nothing => json!({ "$or": [] }),
            Cond::Filter(f) => f,
        }
    }
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// Statement parameters: `?` placeholders are numbered by where they appear.
struct Params<'a> {
    values: &'a [Value],
    /// (line, column) of each `?`, in order.
    positions: HashMap<(u64, u64), usize>,
    used: Vec<bool>,
}

impl<'a> Params<'a> {
    fn new(text: &str, values: &'a [Value]) -> Result<Params<'a>> {
        let tokens = Tokenizer::new(&GenericDialect {}, text).tokenize_with_location().map_err(|e| invalid(e.to_string()))?;
        let mut positions = HashMap::new();
        for token in tokens {
            if matches!(&token.token, Token::Placeholder(p) if p == "?") {
                let index = positions.len();
                positions.insert((token.span.start.line, token.span.start.column), index);
            }
        }
        Ok(Params { values, positions, used: vec![false; values.len()] })
    }

    fn get(&mut self, name: &str, at: (u64, u64)) -> Result<Value> {
        let index = if name == "?" {
            *self.positions.get(&at).ok_or_else(|| invalid("couldn't place a ? parameter."))?
        } else {
            let number: usize = name
                .strip_prefix('$')
                .and_then(|n| n.parse().ok())
                .filter(|n| *n >= 1)
                .ok_or_else(|| invalid(format!("unknown parameter {}: use ? or $1, $2, ...", name)))?;
            number - 1
        };
        let value = self
            .values
            .get(index)
            .ok_or_else(|| invalid(format!("parameter {} has no value: {} given.", index + 1, self.values.len())))?;
        self.used[index] = true;
        Ok(value.clone())
    }

    fn check_all_used(&self) -> Result<()> {
        match self.used.iter().position(|u| !u) {
            Some(i) => Err(invalid(format!("parameter {} isn't used by the statement.", i + 1))),
            None => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

impl SqlQuery {
    /// Parse and translate one SELECT statement. `params` fill its `?` / `$n` placeholders.
    pub fn parse(text: &str, params: &[Value]) -> Result<SqlQuery> {
        if text.len() > MAX_SQL_LENGTH {
            return Err(invalid(format!("the statement is longer than {} bytes.", MAX_SQL_LENGTH)));
        }
        let statements = Parser::parse_sql(&GenericDialect {}, text).map_err(|e| invalid(e.to_string()))?;
        let statement = match statements.as_slice() {
            [one] => one,
            [] => return Err(invalid("no statement given.")),
            _ => return Err(invalid("send one statement at a time.")),
        };
        let Statement::Query(query) = statement else {
            return Err(invalid("only SELECT statements are supported; HexDB's SQL is read-only."));
        };
        let mut translator = Translator { params: Params::new(text, params)?, table: None, alias: None };
        let result = translator.query(query)?;
        translator.params.check_all_used()?;
        Ok(result)
    }

    /// The tessellation the statement reads (`None` for SELECT without FROM).
    pub fn tessellation(&self) -> Option<&str> {
        self.tessellation.as_deref()
    }

    /// What the statement was translated into, for the response and for debugging.
    pub fn describe(&self) -> Value {
        let mut out = Map::new();
        if let Some(t) = &self.tessellation {
            out.insert("tessellation".into(), json!(t));
        }
        match &self.body {
            Body::Constant(_) => {
                out.insert("constant".into(), json!(true));
            }
            Body::Documents { filter, sort, .. } => {
                out.insert("query".into(), json!({ "filter": filter, "sort": sort }));
            }
            Body::Aggregate { filter, group_by, aggregates, sort, having, count_only, .. } => {
                if *count_only {
                    out.insert("count".into(), json!({ "filter": filter }));
                } else {
                    let aggregates: Map<String, Value> = aggregates
                        .iter()
                        .map(|a| (a.name.clone(), json!({ op_name(a.op): a.field.clone().unwrap_or_else(|| "*".into()) })))
                        .collect();
                    out.insert(
                        "aggregate".into(),
                        json!({ "filter": filter, "group_by": group_by, "aggregates": aggregates, "sort": sort }),
                    );
                    if let Some((_, text)) = having {
                        out.insert("having".into(), json!(text));
                    }
                }
            }
        }
        if self.offset > 0 {
            out.insert("offset".into(), json!(self.offset));
        }
        if let Some(limit) = self.limit {
            out.insert("limit".into(), json!(limit));
        }
        Value::Object(out)
    }
}

fn op_name(op: AggregateOp) -> &'static str {
    match op {
        AggregateOp::Count => "$count",
        AggregateOp::CountDistinct => "$countDistinct",
        AggregateOp::Sum => "$sum",
        AggregateOp::Avg => "$avg",
        AggregateOp::Min => "$min",
        AggregateOp::Max => "$max",
    }
}

/// One SELECT list entry.
enum Item<'q> {
    Wildcard,
    Expr { expr: &'q Expr, name: String },
}

struct Translator<'a> {
    params: Params<'a>,
    table: Option<String>,
    alias: Option<String>,
}

impl<'a> Translator<'a> {
    fn query(&mut self, query: &ast::Query) -> Result<SqlQuery> {
        if query.with.is_some() {
            return Err(unsupported("WITH"));
        }
        if !query.locks.is_empty() || query.for_clause.is_some() || query.settings.is_some() || query.format_clause.is_some()
            || !query.pipe_operators.is_empty()
        {
            return Err(unsupported("that clause"));
        }
        let select = match query.body.as_ref() {
            SetExpr::Select(select) => select,
            SetExpr::SetOperation { op, .. } => return Err(unsupported(format!("{}", op).to_uppercase())),
            SetExpr::Query(_) => return Err(unsupported("a parenthesized query")),
            SetExpr::Values(_) => return Err(unsupported("VALUES")),
            other => return Err(unsupported(format!("'{}'", other))),
        };
        if select.flavor != ast::SelectFlavor::Standard {
            return Err(unsupported("FROM-first syntax"));
        }
        if select.into.is_some()
            || !select.lateral_views.is_empty()
            || select.prewhere.is_some()
            || !select.connect_by.is_empty()
            || !select.cluster_by.is_empty()
            || !select.distribute_by.is_empty()
            || !select.sort_by.is_empty()
            || !select.named_window.is_empty()
            || select.qualify.is_some()
            || select.value_table_mode.is_some()
            || select.exclude.is_some()
        {
            return Err(unsupported("that clause"));
        }

        // FROM
        match select.from.as_slice() {
            [] => {}
            [from] => {
                if !from.joins.is_empty() {
                    return Err(unsupported("JOIN"));
                }
                let TableFactor::Table { name, alias, args, .. } = &from.relation else {
                    return Err(unsupported("a subquery or table function in FROM"));
                };
                if args.is_some() {
                    return Err(unsupported("a table function in FROM"));
                }
                self.table = Some(single_name(name)?);
                if let Some(alias) = alias {
                    if !alias.columns.is_empty() {
                        return Err(unsupported("column aliases on a table"));
                    }
                    self.alias = Some(alias.name.value.clone());
                }
            }
            _ => return Err(unsupported("more than one table in FROM (joins)")),
        }

        // LIMIT / OFFSET / FETCH / TOP
        let (mut limit, mut offset) = (None, 0);
        match &query.limit_clause {
            None => {}
            Some(LimitClause::LimitOffset { limit: l, offset: o, limit_by }) => {
                if !limit_by.is_empty() {
                    return Err(unsupported("LIMIT BY"));
                }
                if let Some(l) = l {
                    limit = self.count(l, "LIMIT")?;
                }
                if let Some(o) = o {
                    offset = self.count(&o.value, "OFFSET")?.unwrap_or(0);
                }
            }
            Some(LimitClause::OffsetCommaLimit { offset: o, limit: l }) => {
                offset = self.count(o, "OFFSET")?.unwrap_or(0);
                limit = self.count(l, "LIMIT")?;
            }
        }
        if let Some(fetch) = &query.fetch {
            if fetch.with_ties || fetch.percent {
                return Err(unsupported("FETCH ... WITH TIES or PERCENT"));
            }
            let n = match &fetch.quantity {
                Some(q) => self.count(q, "FETCH")?,
                None => Some(1),
            };
            limit = min_option(limit, n);
        }
        if let Some(top) = &select.top {
            if top.with_ties || top.percent {
                return Err(unsupported("TOP ... WITH TIES or PERCENT"));
            }
            let n = match &top.quantity {
                Some(ast::TopQuantity::Constant(n)) => Some(*n as usize),
                Some(ast::TopQuantity::Expr(e)) => self.count(e, "TOP")?,
                None => None,
            };
            limit = min_option(limit, n);
        }

        // SELECT list
        let mut items = Vec::new();
        for item in &select.projection {
            items.push(match item {
                SelectItem::Wildcard(options) => {
                    check_wildcard(options)?;
                    Item::Wildcard
                }
                SelectItem::QualifiedWildcard(kind, options) => {
                    check_wildcard(options)?;
                    let qualifier = match kind {
                        SelectItemQualifiedWildcardKind::ObjectName(name) => single_name(name)?,
                        SelectItemQualifiedWildcardKind::Expr(e) => e.to_string(),
                    };
                    if !self.is_table(&qualifier) {
                        return Err(invalid(format!("'{}' isn't the table in FROM.", qualifier)));
                    }
                    Item::Wildcard
                }
                SelectItem::UnnamedExpr(expr) => Item::Expr { expr, name: self.default_name(expr) },
                SelectItem::ExprWithAlias { expr, alias } => Item::Expr { expr, name: alias.value.clone() },
                SelectItem::ExprWithAliases { .. } => return Err(unsupported("several aliases for one column")),
            });
        }

        let Some(tessellation) = self.table.clone() else {
            return self.constant_select(select, query, &items, offset, limit);
        };

        let filter = match &select.selection {
            Some(expr) => self.cond(expr, false)?.into_json(),
            None => json!({}),
        };
        // Check the filter now, so a bad one is reported as a SQL error.
        Filter::parse(&filter)?;

        let group_exprs = match &select.group_by {
            GroupByExpr::All(_) => return Err(unsupported("GROUP BY ALL")),
            GroupByExpr::Expressions(exprs, modifiers) => {
                if !modifiers.is_empty() {
                    return Err(unsupported("GROUP BY modifiers (ROLLUP, CUBE, ...)"));
                }
                exprs.as_slice()
            }
        };
        let order_by: &[ast::OrderByExpr] = match &query.order_by {
            None => &[],
            Some(order) => {
                if order.interpolate.is_some() {
                    return Err(unsupported("INTERPOLATE"));
                }
                match &order.kind {
                    OrderByKind::All(_) => return Err(unsupported("ORDER BY ALL")),
                    OrderByKind::Expressions(exprs) => exprs,
                }
            }
        };
        let distinct = match &select.distinct {
            None | Some(Distinct::All) => false,
            Some(Distinct::Distinct) => true,
            Some(Distinct::On(_)) => return Err(unsupported("DISTINCT ON")),
        };

        let aggregated = distinct
            || !group_exprs.is_empty()
            || select.having.is_some()
            || items.iter().any(|i| matches!(i, Item::Expr { expr, .. } if has_aggregate(expr)))
            || order_by.iter().any(|o| has_aggregate(&o.expr));

        let body = if aggregated {
            self.aggregate_body(filter, &items, group_exprs, select.having.as_ref(), order_by, distinct)?
        } else {
            self.documents_body(filter, &items, order_by)?
        };
        Ok(SqlQuery { tessellation: Some(tessellation), body, offset, limit })
    }

    fn constant_select(
        &mut self,
        select: &ast::Select,
        query: &ast::Query,
        items: &[Item],
        offset: usize,
        limit: Option<usize>,
    ) -> Result<SqlQuery> {
        if select.selection.is_some() || select.having.is_some() || query.order_by.is_some() || select.distinct.is_some() {
            return Err(unsupported("WHERE, HAVING, ORDER BY or DISTINCT without FROM"));
        }
        if matches!(&select.group_by, GroupByExpr::Expressions(e, _) if !e.is_empty()) {
            return Err(unsupported("GROUP BY without FROM"));
        }
        let mut row = Vec::new();
        for item in items {
            let Item::Expr { expr, name } = item else {
                return Err(invalid("SELECT * needs a FROM clause."));
            };
            let value = self
                .literal(expr)?
                .ok_or_else(|| invalid(format!("'{}' needs a FROM clause; without one, SELECT only takes values.", expr)))?;
            row.push((name.clone(), value));
        }
        Ok(SqlQuery { tessellation: None, body: Body::Constant(row), offset, limit })
    }

    fn documents_body(&mut self, filter: Value, items: &[Item], order_by: &[ast::OrderByExpr]) -> Result<Body> {
        let mut columns = Vec::new();
        for item in items {
            columns.push(match item {
                Item::Wildcard => DocColumn::All,
                Item::Expr { expr, name } => {
                    if let Some(path) = self.field(expr)? {
                        DocColumn::Field { name: name.clone(), path: split(&path) }
                    } else if let Some(value) = self.literal(expr)? {
                        DocColumn::Constant { name: name.clone(), value }
                    } else {
                        return Err(select_error(expr));
                    }
                }
            });
        }
        let mut sort = Vec::new();
        for order in order_by {
            let descending = order_direction(order)?;
            let field = if let Some(position) = position(&order.expr, items.len())? {
                match &items[position] {
                    Item::Expr { expr, .. } => self.field(expr)?,
                    Item::Wildcard => None,
                }
            } else if let Some(c) = alias_of(&order.expr).and_then(|a| {
                columns.iter().find_map(|c| match c {
                    DocColumn::Field { name, path } if name == a => Some(path.join(".")),
                    _ => None,
                })
            }) {
                Some(c)
            } else {
                self.field(&order.expr)?
            };
            let field = field.ok_or_else(|| invalid(format!("can't sort by '{}': sort by a field.", order.expr)))?;
            sort.push(SortKey { field, descending });
        }
        Ok(Body::Documents { filter, sort, columns })
    }

    #[allow(clippy::too_many_arguments)]
    fn aggregate_body(
        &mut self,
        filter: Value,
        items: &[Item],
        group_exprs: &[Expr],
        having: Option<&Expr>,
        order_by: &[ast::OrderByExpr],
        distinct: bool,
    ) -> Result<Body> {
        // GROUP BY fields: named directly, by a select alias, or by position.
        let mut group_by: Vec<String> = Vec::new();
        for expr in group_exprs {
            // A position, then a select alias (`address.city AS city ... GROUP BY city`), then a field.
            let target = if let Some(p) = position(expr, items.len())? {
                match &items[p] {
                    Item::Expr { expr, .. } => *expr,
                    Item::Wildcard => return Err(group_error(expr)),
                }
            } else {
                alias_of(expr).and_then(|a| find_item(items, a)).unwrap_or(expr)
            };
            let field = self.field(target)?.ok_or_else(|| group_error(expr))?;
            if !group_by.contains(&field) {
                group_by.push(field);
            }
        }
        if distinct {
            if !group_by.is_empty() {
                return Err(unsupported("SELECT DISTINCT with GROUP BY"));
            }
            for item in items {
                match item {
                    Item::Wildcard => return Err(unsupported("SELECT DISTINCT *")),
                    Item::Expr { expr, .. } => {
                        if has_aggregate(expr) {
                            return Err(unsupported("SELECT DISTINCT with aggregates"));
                        }
                        if let Some(field) = self.field(expr)? {
                            if !group_by.contains(&field) {
                                group_by.push(field);
                            }
                        }
                    }
                }
            }
        }

        let mut aggregates: Vec<AggregateSpec> = Vec::new();
        let mut columns = Vec::new();
        for item in items {
            let Item::Expr { expr, name } = item else {
                return Err(invalid("SELECT * can't be combined with GROUP BY or aggregates."));
            };
            let source = self.operand(expr, &group_by, &mut aggregates, items)?;
            columns.push((
                name.clone(),
                match source {
                    Operand::Key(k) => Source::Key(k),
                    Operand::Value(v) => Source::Constant(v),
                },
            ));
        }

        let having = match having {
            Some(expr) => Some((self.having(expr, &group_by, &mut aggregates, items)?, expr.to_string())),
            None => None,
        };

        let mut sort = Vec::new();
        for order in order_by {
            let descending = order_direction(order)?;
            let key = if let Some(p) = position(&order.expr, items.len())? {
                match &columns[p].1 {
                    Source::Key(k) => k.clone(),
                    Source::Constant(_) => continue, // sorting by a constant changes nothing
                }
            } else {
                match self.operand(&order.expr, &group_by, &mut aggregates, items)? {
                    Operand::Key(k) => k,
                    Operand::Value(_) => continue,
                }
            };
            sort.push(SortKey { field: key, descending });
        }

        let count_only = group_by.is_empty()
            && having.is_none()
            && !aggregates.is_empty()
            && aggregates.iter().all(|a| a.op == AggregateOp::Count && a.field.is_none());
        let mut sum_guards = Vec::new();
        for sum in aggregates.clone().iter().filter(|a| a.op == AggregateOp::Sum) {
            let count = match aggregates.iter().find(|a| a.op == AggregateOp::Count && a.field == sum.field) {
                Some(existing) => existing.name.clone(),
                None => {
                    let name = format!("$agg{}", aggregates.len());
                    aggregates.push(AggregateSpec { name: name.clone(), op: AggregateOp::Count, field: sum.field.clone() });
                    name
                }
            };
            sum_guards.push((sum.name.clone(), count));
        }
        // Validate names and sort keys the way the engine will.
        Aggregation::new(Filter::all(), group_by.clone(), aggregates.clone(), sort.clone(), 0, 0)?;
        Ok(Body::Aggregate { filter, group_by, aggregates, sort, having, columns, count_only, sum_guards })
    }

    /// A value in an aggregate query: a group field, an aggregate, a select alias, or a literal.
    fn operand(&mut self, expr: &Expr, group_by: &[String], aggregates: &mut Vec<AggregateSpec>, items: &[Item]) -> Result<Operand> {
        if let Expr::Nested(inner) = expr {
            return self.operand(inner, group_by, aggregates, items);
        }
        if let Expr::Function(function) = expr {
            if let Some(spec) = self.aggregate(function)? {
                let existing = aggregates.iter().find(|a| a.op == spec.0 && a.field == spec.1);
                let name = match existing {
                    Some(a) => a.name.clone(),
                    None => {
                        let name = format!("$agg{}", aggregates.len());
                        aggregates.push(AggregateSpec { name: name.clone(), op: spec.0, field: spec.1 });
                        name
                    }
                };
                return Ok(Operand::Key(name));
            }
        }
        if let Some(value) = self.literal(expr)? {
            return Ok(Operand::Value(value));
        }
        if let Some(field) = self.field(expr)? {
            if group_by.contains(&field) {
                return Ok(Operand::Key(field));
            }
            // An alias of another select item, unless it names a field itself.
            if let Some(item_expr) = alias_of(expr).and_then(|a| find_item(items, a)) {
                if !std::ptr::eq(item_expr, expr) {
                    return self.operand(item_expr, group_by, aggregates, items);
                }
            }
            return Err(invalid(format!("'{}' must appear in GROUP BY or be used in an aggregate function.", field)));
        }
        Err(invalid(format!(
            "'{}' isn't supported here: use fields from GROUP BY, aggregates (COUNT, SUM, AVG, MIN, MAX) and values.",
            expr
        )))
    }

    /// COUNT/SUM/AVG/MIN/MAX, as (operator, field); `None` for other functions.
    fn aggregate(&mut self, function: &ast::Function) -> Result<Option<(AggregateOp, Option<String>)>> {
        let name = single_name(&function.name)?.to_ascii_lowercase();
        let op = match name.as_str() {
            "count" => AggregateOp::Count,
            "sum" => AggregateOp::Sum,
            "avg" => AggregateOp::Avg,
            "min" => AggregateOp::Min,
            "max" => AggregateOp::Max,
            _ => return Ok(None),
        };
        if function.filter.is_some() || function.over.is_some() || !function.within_group.is_empty() || function.null_treatment.is_some() {
            return Err(unsupported(format!("FILTER, OVER or WITHIN GROUP on {}", name.to_uppercase())));
        }
        if !matches!(function.parameters, FunctionArguments::None) {
            return Err(unsupported(format!("parameters on {}", name.to_uppercase())));
        }
        let FunctionArguments::List(list) = &function.args else {
            return Err(invalid(format!("{} takes one argument.", name.to_uppercase())));
        };
        if !list.clauses.is_empty() {
            return Err(unsupported(format!("clauses inside {}", name.to_uppercase())));
        }
        let distinct = matches!(list.duplicate_treatment, Some(ast::DuplicateTreatment::Distinct));
        let [arg] = list.args.as_slice() else {
            return Err(invalid(format!("{} takes one argument.", name.to_uppercase())));
        };
        let FunctionArg::Unnamed(arg) = arg else {
            return Err(invalid(format!("{} takes one argument.", name.to_uppercase())));
        };
        let field = match arg {
            FunctionArgExpr::Wildcard if op == AggregateOp::Count && !distinct => None,
            FunctionArgExpr::Expr(expr) => match self.field(expr)? {
                Some(f) => Some(f),
                // COUNT(1) counts rows like COUNT(*).
                None if op == AggregateOp::Count && !distinct && matches!(self.literal(expr)?, Some(v) if !v.is_null()) => None,
                None => return Err(invalid(format!("{} takes a field: '{}' isn't one.", name.to_uppercase(), expr))),
            },
            _ => return Err(invalid(format!("{} takes a field.", name.to_uppercase()))),
        };
        let op = match (op, distinct) {
            (AggregateOp::Count, true) => AggregateOp::CountDistinct,
            (op, false) => op,
            (_, true) => return Err(unsupported(format!("DISTINCT inside {}", name.to_uppercase()))),
        };
        Ok(Some((op, field)))
    }

    fn having(&mut self, expr: &Expr, group_by: &[String], aggregates: &mut Vec<AggregateSpec>, items: &[Item]) -> Result<Having> {
        Ok(match expr {
            Expr::Nested(e) => self.having(e, group_by, aggregates, items)?,
            Expr::UnaryOp { op: UnaryOperator::Not, expr } => Having::Not(Box::new(self.having(expr, group_by, aggregates, items)?)),
            Expr::BinaryOp { left, op: op @ (BinaryOperator::And | BinaryOperator::Or), right } => {
                let parts = vec![self.having(left, group_by, aggregates, items)?, self.having(right, group_by, aggregates, items)?];
                if *op == BinaryOperator::And {
                    Having::And(parts)
                } else {
                    Having::Or(parts)
                }
            }
            Expr::BinaryOp { left, op, right } if comparison(op) => Having::Compare(
                self.operand(left, group_by, aggregates, items)?,
                op.clone(),
                self.operand(right, group_by, aggregates, items)?,
            ),
            Expr::IsNull(e) => Having::IsNull(self.operand(e, group_by, aggregates, items)?),
            Expr::IsNotNull(e) => Having::Not(Box::new(Having::IsNull(self.operand(e, group_by, aggregates, items)?))),
            Expr::InList { expr, list, negated } => {
                let operand = self.operand(expr, group_by, aggregates, items)?;
                let values = list.iter().map(|e| self.value(e)).collect::<Result<Vec<_>>>()?;
                let test = Having::In(operand, values);
                if *negated {
                    Having::Not(Box::new(test))
                } else {
                    test
                }
            }
            Expr::Between { expr, negated, low, high } => {
                let test = Having::And(vec![
                    Having::Compare(self.operand(expr, group_by, aggregates, items)?, BinaryOperator::GtEq, Operand::Value(self.value(low)?)),
                    Having::Compare(self.operand(expr, group_by, aggregates, items)?, BinaryOperator::LtEq, Operand::Value(self.value(high)?)),
                ]);
                if *negated {
                    Having::Not(Box::new(test))
                } else {
                    test
                }
            }
            Expr::Value(_) => Having::Constant(match self.value(expr)? {
                Value::Bool(b) => Some(b),
                Value::Null => None,
                other => return Err(invalid(format!("HAVING {} isn't a condition.", other))),
            }),
            other => return Err(unsupported(format!("'{}' in HAVING", other))),
        })
    }

    // -- WHERE ---------------------------------------------------------------

    /// Translate a WHERE condition; `negated` pushes a NOT down to each comparison.
    fn cond(&mut self, expr: &Expr, negated: bool) -> Result<Cond> {
        Ok(match expr {
            Expr::Nested(e) => self.cond(e, negated)?,
            Expr::UnaryOp { op: UnaryOperator::Not, expr } => self.cond(expr, !negated)?,
            Expr::BinaryOp { left, op: op @ (BinaryOperator::And | BinaryOperator::Or), right } => {
                let and = (*op == BinaryOperator::And) != negated;
                let parts = vec![self.cond(left, negated)?, self.cond(right, negated)?];
                Cond::combine(and, parts)
            }
            Expr::BinaryOp { left, op, right } if comparison(op) => match (self.field(left)?, self.field(right)?) {
                (Some(_), Some(_)) => return Err(unsupported(format!("comparing two fields ('{}')", expr))),
                (Some(field), None) => {
                    let value = self.operand_value(right, expr)?;
                    field_compare(&field, op.clone(), value, negated)
                }
                (None, Some(field)) => {
                    let value = self.operand_value(left, expr)?;
                    field_compare(&field, flip(op), value, negated)
                }
                (None, None) => {
                    let (l, r) = (self.operand_value(left, expr)?, self.operand_value(right, expr)?);
                    Cond::constant(compare_values(&l, op, &r).map(|b| b != negated))
                }
            },
            Expr::IsNull(e) | Expr::IsNotNull(e) => {
                let field = self.require_field(e, expr)?;
                let is_null = matches!(expr, Expr::IsNull(_)) != negated;
                Cond::Filter(json!({ field: { (if is_null { "$eq" } else { "$ne" }): null } }))
            }
            Expr::IsTrue(e) | Expr::IsNotTrue(e) | Expr::IsFalse(e) | Expr::IsNotFalse(e) => {
                let field = self.require_field(e, expr)?;
                let target = matches!(expr, Expr::IsTrue(_) | Expr::IsNotTrue(_));
                let equal = matches!(expr, Expr::IsTrue(_) | Expr::IsFalse(_)) != negated;
                Cond::Filter(json!({ field: { (if equal { "$eq" } else { "$ne" }): target } }))
            }
            Expr::InList { expr: e, list, negated: not_in } => {
                let field = self.require_field(e, expr)?;
                let values = list.iter().map(|v| self.value(v)).collect::<Result<Vec<_>>>()?;
                in_list(&field, values, *not_in != negated)
            }
            Expr::Between { expr: e, negated: not_between, low, high } => {
                let field = self.require_field(e, expr)?;
                let (low, high) = (self.value(low)?, self.value(high)?);
                if *not_between != negated {
                    Cond::combine(
                        false,
                        vec![field_compare(&field, BinaryOperator::Lt, low, false), field_compare(&field, BinaryOperator::Gt, high, false)],
                    )
                } else {
                    Cond::combine(
                        true,
                        vec![field_compare(&field, BinaryOperator::GtEq, low, false), field_compare(&field, BinaryOperator::LtEq, high, false)],
                    )
                }
            }
            Expr::Like { negated: not_like, any, expr: e, pattern, escape_char } => {
                if *any {
                    return Err(unsupported("LIKE ANY"));
                }
                let field = self.require_field(e, expr)?;
                let pattern = self.value(pattern)?;
                let escape = match escape_char {
                    None => None,
                    Some(c) => match self.value(c)? {
                        Value::String(s) if s.chars().count() == 1 => s.chars().next(),
                        _ => return Err(invalid("ESCAPE takes one character.")),
                    },
                };
                like(&field, &pattern, escape, *not_like != negated)?
            }
            Expr::ILike { .. } => return Err(unsupported("ILIKE (HexDB's string matching is case-sensitive; use LIKE)")),
            Expr::InSubquery { .. } | Expr::Exists { .. } | Expr::Subquery(_) => return Err(unsupported("subqueries")),
            Expr::Function(f) => return Err(unsupported(format!("the function {} in WHERE", f.name))),
            _ => {
                if let Some(field) = self.field(expr)? {
                    // A bare boolean field.
                    Cond::Filter(json!({ field: { "$eq": !negated } }))
                } else if let Some(value) = self.literal(expr)? {
                    Cond::constant(match value {
                        Value::Bool(b) => Some(b != negated),
                        Value::Null => None,
                        other => return Err(invalid(format!("WHERE {} isn't a condition.", other))),
                    })
                } else {
                    return Err(unsupported(format!("'{}' in WHERE", expr)));
                }
            }
        })
    }

    fn require_field(&mut self, expr: &Expr, context: &Expr) -> Result<String> {
        self.field(expr)?
            .ok_or_else(|| invalid(format!("'{}': the left side must be a field, not '{}'.", context, expr)))
    }

    fn operand_value(&mut self, expr: &Expr, context: &Expr) -> Result<Value> {
        self.literal(expr)?.ok_or_else(|| {
            invalid(format!("'{}': compare a field with a value; expressions like '{}' aren't supported.", context, expr))
        })
    }

    fn value(&mut self, expr: &Expr) -> Result<Value> {
        self.literal(expr)?.ok_or_else(|| invalid(format!("'{}' must be a value.", expr)))
    }

    // -- Names and values ----------------------------------------------------

    fn is_table(&self, name: &str) -> bool {
        self.alias.as_deref() == Some(name) || (self.alias.is_none() && self.table.as_deref() == Some(name))
    }

    /// A field reference as a dotted path, or `None` if `expr` isn't one.
    /// A leading table name or alias is dropped (`o.total` is `total`).
    fn field(&self, expr: &Expr) -> Result<Option<String>> {
        Ok(match expr {
            Expr::Nested(e) => self.field(e)?,
            Expr::Identifier(id) => Some(id.value.clone()),
            Expr::CompoundIdentifier(parts) => {
                let mut names: Vec<&str> = parts.iter().map(|p| p.value.as_str()).collect();
                if names.len() > 1 && self.is_table(names[0]) {
                    names.remove(0);
                }
                Some(names.join("."))
            }
            _ => None,
        })
    }

    /// A value: a literal, a parameter, or a negated number. `None` if `expr` isn't one.
    fn literal(&mut self, expr: &Expr) -> Result<Option<Value>> {
        Ok(match expr {
            Expr::Nested(e) => self.literal(e)?,
            Expr::Value(v) => Some(match &v.value {
                ast::Value::Number(text, _) => Value::Number(parse_number(text)?),
                ast::Value::SingleQuotedString(s)
                | ast::Value::EscapedStringLiteral(s)
                | ast::Value::NationalStringLiteral(s)
                | ast::Value::UnicodeStringLiteral(s)
                | ast::Value::TripleSingleQuotedString(s) => Value::String(s.clone()),
                ast::Value::Boolean(b) => Value::Bool(*b),
                ast::Value::Null => Value::Null,
                ast::Value::Placeholder(name) => self.params.get(name, (v.span.start.line, v.span.start.column))?,
                other => return Err(unsupported(format!("the value {}", other))),
            }),
            Expr::UnaryOp { op: op @ (UnaryOperator::Minus | UnaryOperator::Plus), expr: inner } => match self.literal(inner)? {
                Some(Value::Number(n)) if *op == UnaryOperator::Plus => Some(Value::Number(n)),
                Some(Value::Number(n)) => Some(Value::Number(negate(&n)?)),
                Some(other) => return Err(invalid(format!("can't negate {}.", other))),
                None => None,
            },
            // DATE '2026-01-01', TIMESTAMP '...': HexDB stores dates as strings.
            Expr::TypedString(typed) => match &typed.value.value {
                ast::Value::SingleQuotedString(s) => Some(Value::String(s.clone())),
                other => return Err(unsupported(format!("the value {}", other))),
            },
            _ => None,
        })
    }

    /// LIMIT / OFFSET / FETCH / TOP: a non-negative integer, or NULL (no limit).
    fn count(&mut self, expr: &Expr, clause: &str) -> Result<Option<usize>> {
        match self.value(expr)? {
            Value::Null => Ok(None),
            Value::Number(n) if n.as_u64().is_some() => Ok(Some(n.as_u64().unwrap_or(0) as usize)),
            other => Err(invalid(format!("{} must be a non-negative integer, not {}.", clause, other))),
        }
    }

    fn default_name(&self, expr: &Expr) -> String {
        match self.field(expr) {
            Ok(Some(path)) => path,
            _ => expr.to_string(),
        }
    }
}

fn check_wildcard(options: &ast::WildcardAdditionalOptions) -> Result<()> {
    if options.opt_ilike.is_some()
        || options.opt_exclude.is_some()
        || options.opt_except.is_some()
        || options.opt_replace.is_some()
        || options.opt_rename.is_some()
    {
        return Err(unsupported("options on *"));
    }
    Ok(())
}

fn single_name(name: &ObjectName) -> Result<String> {
    match name.0.as_slice() {
        [part] => part
            .as_ident()
            .map(|id| id.value.clone())
            .ok_or_else(|| invalid(format!("'{}' isn't a name.", name))),
        _ => Err(invalid(format!("'{}': name a tessellation directly, without a schema or database.", name))),
    }
}

fn min_option(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn split(path: &str) -> Vec<String> {
    path.split('.').map(String::from).collect()
}

fn select_error(expr: &Expr) -> anyhow::Error {
    invalid(format!(
        "'{}' isn't supported in SELECT: use fields, values and aggregates (COUNT, SUM, AVG, MIN, MAX).",
        expr
    ))
}

fn group_error(expr: &Expr) -> anyhow::Error {
    invalid(format!("GROUP BY '{}': group by fields.", expr))
}

/// `ORDER BY 2` / `GROUP BY 1`: a 0-based select item index.
fn position(expr: &Expr, items: usize) -> Result<Option<usize>> {
    let Expr::Value(v) = expr else { return Ok(None) };
    let ast::Value::Number(text, _) = &v.value else { return Ok(None) };
    match text.parse::<usize>() {
        Ok(n) if n >= 1 && n <= items => Ok(Some(n - 1)),
        _ => Err(invalid(format!("position {} is out of range: the SELECT list has {} items.", text, items))),
    }
}

/// The name an unqualified identifier gives, to match select aliases.
fn alias_of(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Identifier(id) => Some(id.value.as_str()),
        _ => None,
    }
}

fn find_item<'q>(items: &'q [Item], alias: &str) -> Option<&'q Expr> {
    items.iter().find_map(|i| match i {
        Item::Expr { expr, name } if name == alias => Some(*expr),
        _ => None,
    })
}

fn order_direction(order: &ast::OrderByExpr) -> Result<bool> {
    if order.options.nulls_first.is_some() || order.with_fill.is_some() {
        return Err(unsupported("NULLS FIRST / LAST and WITH FILL"));
    }
    match &order.options.sort {
        None | Some(OrderBySort::Asc) => Ok(false),
        Some(OrderBySort::Desc) => Ok(true),
        Some(other) => Err(unsupported(format!("ORDER BY ... {:?}", other))),
    }
}

fn has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function(f) => matches!(
            f.name.0.as_slice(),
            [part] if part.as_ident().is_some_and(|id| ["count", "sum", "avg", "min", "max"].contains(&id.value.to_ascii_lowercase().as_str()))
        ),
        Expr::Nested(e) | Expr::UnaryOp { expr: e, .. } | Expr::IsNull(e) | Expr::IsNotNull(e) => has_aggregate(e),
        Expr::BinaryOp { left, right, .. } => has_aggregate(left) || has_aggregate(right),
        _ => false,
    }
}

fn comparison(op: &BinaryOperator) -> bool {
    matches!(
        op,
        BinaryOperator::Eq | BinaryOperator::NotEq | BinaryOperator::Lt | BinaryOperator::LtEq | BinaryOperator::Gt | BinaryOperator::GtEq
    )
}

/// `5 < x` is `x > 5`.
fn flip(op: &BinaryOperator) -> BinaryOperator {
    match op {
        BinaryOperator::Lt => BinaryOperator::Gt,
        BinaryOperator::LtEq => BinaryOperator::GtEq,
        BinaryOperator::Gt => BinaryOperator::Lt,
        BinaryOperator::GtEq => BinaryOperator::LtEq,
        other => other.clone(),
    }
}

/// `NOT (x < 5)` is `x >= 5` (both are false when x is null).
fn negate_op(op: &BinaryOperator) -> BinaryOperator {
    match op {
        BinaryOperator::Eq => BinaryOperator::NotEq,
        BinaryOperator::NotEq => BinaryOperator::Eq,
        BinaryOperator::Lt => BinaryOperator::GtEq,
        BinaryOperator::LtEq => BinaryOperator::Gt,
        BinaryOperator::Gt => BinaryOperator::LtEq,
        BinaryOperator::GtEq => BinaryOperator::Lt,
        other => other.clone(),
    }
}

/// `field op value` as a filter, with SQL's NULL rules.
fn field_compare(field: &str, op: BinaryOperator, value: Value, negated: bool) -> Cond {
    if value.is_null() {
        return Cond::Nothing; // a comparison with NULL is never true
    }
    let op = if negated { negate_op(&op) } else { op };
    Cond::Filter(match op {
        BinaryOperator::Eq => json!({ field: { "$eq": value } }),
        // Missing and null fields don't match `<>` in SQL.
        BinaryOperator::NotEq => json!({ field: { "$nin": [value, null] } }),
        BinaryOperator::Lt => json!({ field: { "$lt": value } }),
        BinaryOperator::LtEq => json!({ field: { "$lte": value } }),
        BinaryOperator::Gt => json!({ field: { "$gt": value } }),
        _ => json!({ field: { "$gte": value } }),
    })
}

fn in_list(field: &str, values: Vec<Value>, negated: bool) -> Cond {
    if negated {
        // `x NOT IN (1, NULL)` is never true.
        if values.iter().any(Value::is_null) {
            return Cond::Nothing;
        }
        let mut values = values;
        values.push(Value::Null);
        Cond::Filter(json!({ field: { "$nin": values } }))
    } else {
        let values: Vec<Value> = values.into_iter().filter(|v| !v.is_null()).collect();
        if values.is_empty() {
            return Cond::Nothing;
        }
        Cond::Filter(json!({ field: { "$in": values } }))
    }
}

/// LIKE with `%` at the start, the end, both, or neither; `_` isn't supported.
fn like(field: &str, pattern: &Value, escape: Option<char>, negated: bool) -> Result<Cond> {
    let pattern = match pattern {
        Value::Null => return Ok(Cond::Nothing),
        Value::String(s) => s,
        other => return Err(invalid(format!("LIKE needs a string pattern, not {}.", other))),
    };
    // Split into literal text and unescaped % positions.
    let mut text = String::new();
    let mut leading = false;
    let mut trailing = false;
    let mut chars = pattern.chars().peekable();
    let mut at_start = true;
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            match chars.next() {
                Some(next) => text.push(next),
                None => return Err(invalid("LIKE pattern ends with the escape character.")),
            }
        } else if c == '%' {
            if at_start {
                leading = true;
            } else if chars.peek().is_none() || chars.clone().all(|c| c == '%') {
                trailing = true;
                break;
            } else {
                return Err(unsupported(format!(
                    "the LIKE pattern '{}': use % only at the start or end (prefix, suffix or substring matches)",
                    pattern
                )));
            }
            continue; // stays at_start for '%%'
        } else if c == '_' {
            return Err(unsupported(format!(
                "_ in the LIKE pattern '{}' (escape it to match a literal _: LIKE 'a\\_%' ESCAPE '\\')",
                pattern
            )));
        } else {
            text.push(c);
        }
        at_start = false;
    }
    let op = match (leading, trailing) {
        (false, false) => return Ok(field_compare(field, BinaryOperator::Eq, Value::String(text), negated)),
        _ if text.is_empty() => {
            // '%' matches any string.
            return Ok(if negated {
                Cond::Nothing
            } else {
                Cond::Filter(json!({ field: { "$ne": null } }))
            });
        }
        (false, true) => "$startsWith",
        (true, false) => "$endsWith",
        (true, true) => "$contains",
    };
    Ok(Cond::Filter(if negated {
        json!({ field: { "$not": { op: text }, "$ne": null } })
    } else {
        json!({ field: { op: text } })
    }))
}

fn parse_number(text: &str) -> Result<Number> {
    if let Ok(i) = text.parse::<i64>() {
        return Ok(Number::from(i));
    }
    if let Ok(u) = text.parse::<u64>() {
        return Ok(Number::from(u));
    }
    text.parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .ok_or_else(|| invalid(format!("'{}' isn't a number HexDB can store.", text)))
}

fn negate(n: &Number) -> Result<Number> {
    if let Some(i) = n.as_i64() {
        if let Some(neg) = i.checked_neg() {
            return Ok(Number::from(neg));
        }
    }
    n.as_f64()
        .and_then(|f| Number::from_f64(-f))
        .ok_or_else(|| invalid(format!("can't negate {}.", n)))
}

/// SQL comparison of two values: `None` when either is null or they can't be compared.
fn compare_values(left: &Value, op: &BinaryOperator, right: &Value) -> Option<bool> {
    if left.is_null() || right.is_null() {
        return None;
    }
    let ord = compare(left, right).or_else(|| if left == right { Some(Ordering::Equal) } else { None });
    Some(match op {
        BinaryOperator::Eq => ord == Some(Ordering::Equal),
        BinaryOperator::NotEq => ord != Some(Ordering::Equal),
        BinaryOperator::Lt => ord? == Ordering::Less,
        BinaryOperator::LtEq => ord? != Ordering::Greater,
        BinaryOperator::Gt => ord? == Ordering::Greater,
        BinaryOperator::GtEq => ord? != Ordering::Less,
        _ => return None,
    })
}

impl Having {
    fn eval(&self, row: &Map<String, Value>) -> Option<bool> {
        let get = |o: &Operand| -> Value {
            match o {
                Operand::Key(k) => row.get(k).cloned().unwrap_or(Value::Null),
                Operand::Value(v) => v.clone(),
            }
        };
        match self {
            Having::Constant(v) => *v,
            Having::Not(inner) => inner.eval(row).map(|b| !b),
            Having::And(parts) => {
                let mut result = Some(true);
                for p in parts {
                    match p.eval(row) {
                        Some(false) => return Some(false),
                        None => result = None,
                        Some(true) => {}
                    }
                }
                result
            }
            Having::Or(parts) => {
                let mut result = Some(false);
                for p in parts {
                    match p.eval(row) {
                        Some(true) => return Some(true),
                        None => result = None,
                        Some(false) => {}
                    }
                }
                result
            }
            Having::Compare(l, op, r) => compare_values(&get(l), op, &get(r)),
            Having::IsNull(o) => Some(get(o).is_null()),
            Having::In(o, values) => {
                let v = get(o);
                if v.is_null() {
                    return None;
                }
                let mut result = Some(false);
                for candidate in values {
                    match compare_values(&v, &BinaryOperator::Eq, candidate) {
                        Some(true) => return Some(true),
                        None => result = None,
                        Some(false) => {}
                    }
                }
                result
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Which page of a statement's rows to return.
#[derive(Debug, Clone, Default)]
pub struct SqlPage {
    /// Rows per page (`DEFAULT_SQL_PAGE_SIZE` if 0).
    pub size: usize,
    /// The `next` value from the previous page.
    pub cursor: Option<String>,
}

/// One result column. `type` is inferred from the page's values: boolean,
/// integer, number, string, json (objects, arrays or mixed types) or null.
#[derive(Debug, Clone, Serialize)]
pub struct SqlColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// A page of results.
#[derive(Debug, Clone, Serialize)]
pub struct SqlResult {
    pub columns: Vec<SqlColumn>,
    pub rows: Vec<Vec<Value>>,
    /// Pass back as `cursor` for the next page; `None` on the last page.
    pub next: Option<String>,
}

/// Where a page starts: rows already returned, and the document ID to continue after.
struct Cursor {
    returned: usize,
    after: Option<ulid::Ulid>,
}

impl Cursor {
    fn parse(text: Option<&str>) -> Result<Cursor> {
        let bad = || invalid("cursor isn't a value this endpoint returned as next.");
        let Some(text) = text else { return Ok(Cursor { returned: 0, after: None }) };
        if let Some(rest) = text.strip_prefix('o') {
            return Ok(Cursor { returned: rest.parse().map_err(|_| bad())?, after: None });
        }
        if let Some(rest) = text.strip_prefix('a') {
            let (n, id) = rest.split_once(':').ok_or_else(bad)?;
            return Ok(Cursor { returned: n.parse().map_err(|_| bad())?, after: Some(ulid::Ulid::from_string(id).map_err(|_| bad())?) });
        }
        Err(bad())
    }
}

impl HexDBEngine {
    /// Run a translated statement and return one page of rows. The caller's
    /// read permission on the tessellation must already have been checked.
    pub async fn sql(&self, query: &SqlQuery, page: &SqlPage) -> Result<SqlResult> {
        let size = match page.size {
            0 => DEFAULT_SQL_PAGE_SIZE,
            n => n.min(MAX_SQL_PAGE_SIZE),
        };
        let cursor = Cursor::parse(page.cursor.as_deref())?;
        // Rows still allowed by LIMIT, and how many this page may hold.
        let remaining = query.limit.map(|l| l.saturating_sub(cursor.returned));
        let want = remaining.map_or(size, |r| r.min(size));
        let start = query.offset.saturating_add(cursor.returned);
        let tess = query.tessellation.as_deref().unwrap_or_default();

        let (names, rows, more, after): (Vec<String>, Vec<Vec<Value>>, bool, Option<ulid::Ulid>) = match &query.body {
            Body::Constant(values) => {
                let names = values.iter().map(|(n, _)| n.clone()).collect();
                let row: Vec<Value> = values.iter().map(|(_, v)| v.clone()).collect();
                let rows = if start == 0 && want > 0 { vec![row] } else { Vec::new() };
                (names, rows, false, None)
            }
            Body::Documents { filter, sort, columns } => {
                if want == 0 {
                    (doc_names(columns, &[]), Vec::new(), false, None)
                } else {
                    let request = DocumentQuery {
                        filter: Filter::parse(filter)?,
                        sort: sort.clone(),
                        offset: if cursor.after.is_some() { 0 } else { start },
                        limit: want,
                        after: cursor.after,
                        with_total: false,
                    };
                    let result = self.query_documents(tess, &request).await?;
                    let documents: Vec<Value> = result.documents.iter().map(|d| d.to_api_json()).collect();
                    let names = doc_names(columns, &documents);
                    let rows = documents.iter().map(|d| doc_row(columns, &names, d)).collect::<Vec<_>>();
                    let more = rows.len() == want && remaining != Some(want);
                    (names, rows, more, if sort.is_empty() { result.next } else { None })
                }
            }
            Body::Aggregate { filter, group_by, aggregates, sort, having, columns, count_only, sum_guards } => {
                let guard = |mut rows: Vec<Map<String, Value>>| {
                    for row in &mut rows {
                        for (sum, count) in sum_guards {
                            if row.get(count).and_then(Value::as_u64) == Some(0) {
                                row.insert(sum.clone(), Value::Null);
                            }
                        }
                    }
                    rows
                };
                let names: Vec<String> = columns.iter().map(|(n, _)| n.clone()).collect();
                let filter = Filter::parse(filter)?;
                let (groups, more): (Vec<Map<String, Value>>, bool) = if want == 0 {
                    (Vec::new(), false)
                } else if *count_only {
                    let count = self.count_matching(tess, &filter).await?;
                    let row: Map<String, Value> = aggregates.iter().map(|a| (a.name.clone(), json!(count))).collect();
                    (if start == 0 { vec![row] } else { Vec::new() }, false)
                } else if let Some((having, _)) = having {
                    let all = Aggregation::new(filter, group_by.clone(), aggregates.clone(), sort.clone(), 0, MAX_GROUPS)?;
                    let result = self.aggregate(tess, &all).await?;
                    let kept: Vec<_> = guard(result.rows).into_iter().filter(|row| having.eval(row) == Some(true)).collect();
                    let more = kept.len() > start.saturating_add(want);
                    (kept.into_iter().skip(start).take(want).collect(), more)
                } else {
                    let aggregation = Aggregation::new(filter, group_by.clone(), aggregates.clone(), sort.clone(), start, want)?;
                    let result = self.aggregate(tess, &aggregation).await?;
                    let more = result.total_groups > start.saturating_add(result.rows.len());
                    (guard(result.rows), more)
                };
                let rows = groups
                    .iter()
                    .map(|g| {
                        columns
                            .iter()
                            .map(|(_, source)| match source {
                                Source::Key(k) => g.get(k).cloned().unwrap_or(Value::Null),
                                Source::Constant(v) => v.clone(),
                            })
                            .collect()
                    })
                    .collect::<Vec<Vec<Value>>>();
                let more = more && rows.len() == want && remaining != Some(want);
                (names, rows, more, None)
            }
        };

        let returned = cursor.returned + rows.len();
        let next = match (more, after) {
            (false, _) => None,
            (true, Some(id)) => Some(format!("a{}:{}", returned, id)),
            (true, None) => Some(format!("o{}", returned)),
        };
        let columns = names
            .into_iter()
            .enumerate()
            .map(|(i, name)| SqlColumn { name, kind: column_type(rows.iter().map(|r| &r[i])) })
            .collect();
        Ok(SqlResult { columns, rows, next })
    }
}

/// A column of a tessellation, for catalog queries (ODBC's SQLColumns).
#[derive(Debug, Clone, Serialize)]
pub struct SqlColumnInfo {
    pub name: String,
    /// boolean, integer, number, string or json (see `SqlColumn`).
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub nullable: bool,
    /// Where the column came from: `schema`, or `sample` (fields seen in documents).
    pub source: &'static str,
}

/// Documents read to find the fields of a tessellation without a schema.
pub const COLUMN_SAMPLE_SIZE: usize = 100;

impl HexDBEngine {
    /// The columns a SELECT * on `tess` returns, as the caller sees them: `id`,
    /// the current schema's fields, then (if the schema allows other fields,
    /// or there is none) the fields found in the first documents. Hidden
    /// fields of the caller's role are left out.
    pub async fn sql_columns(&self, tess: &str) -> Result<Vec<SqlColumnInfo>> {
        let scope = self.caller_scope(tess, crate::auth::Action::Read);
        let visible = |name: &str| !scope.hidden.iter().any(|h| h == name || name.starts_with(&format!("{}.", h)));
        let mut columns = vec![SqlColumnInfo { name: "id".into(), kind: "string", nullable: false, source: "schema" }];
        let schema = self.schemas(tess).into_iter().max_by_key(|s| s.version);
        if let Some(schema) = &schema {
            for (name, rule) in &schema.fields {
                // A nested rule (`dims.width`) makes its top-level field a JSON column.
                if let Some((top, _)) = name.split_once('.') {
                    if visible(top) && !schema.fields.contains_key(top) && !columns.iter().any(|c| c.name == top) {
                        columns.push(SqlColumnInfo { name: top.to_string(), kind: "json", nullable: true, source: "schema" });
                    }
                    continue;
                }
                if !visible(name) {
                    continue;
                }
                let kind = match rule.kind {
                    crate::schema::FieldType::String => "string",
                    crate::schema::FieldType::Number => "number",
                    crate::schema::FieldType::Integer => "integer",
                    crate::schema::FieldType::Boolean => "boolean",
                    _ => "json",
                };
                columns.push(SqlColumnInfo { name: name.clone(), kind, nullable: !rule.required || rule.nullable, source: "schema" });
            }
        }
        if schema.as_ref().is_none_or(|s| s.additional_fields) {
            let query = DocumentQuery { limit: COLUMN_SAMPLE_SIZE, with_total: false, ..DocumentQuery::default() };
            let documents: Vec<Value> = self.query_documents(tess, &query).await?.documents.iter().map(|d| d.to_api_json()).collect();
            let names = doc_names(&[DocColumn::All], &documents);
            for name in names.into_iter().skip(1) {
                if columns.iter().any(|c| c.name == name) || !visible(&name) {
                    continue;
                }
                let kind = match column_type(documents.iter().map(|d| d.get(&name).unwrap_or(&Value::Null))) {
                    "null" => "string",
                    k => k,
                };
                columns.push(SqlColumnInfo { name, kind, nullable: true, source: "sample" });
            }
        }
        Ok(columns)
    }
}

/// Column names for a page of documents (`*` expands to the fields present).
fn doc_names(columns: &[DocColumn], documents: &[Value]) -> Vec<String> {
    let mut names = Vec::new();
    for column in columns {
        match column {
            DocColumn::Field { name, .. } | DocColumn::Constant { name, .. } => names.push(name.clone()),
            DocColumn::All => {
                let start = names.len();
                names.push("id".to_string());
                for doc in documents {
                    if let Value::Object(map) = doc {
                        for key in map.keys() {
                            if !key.starts_with('_') && !names[start..].contains(key) {
                                names.push(key.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    names
}

fn doc_row(columns: &[DocColumn], names: &[String], doc: &Value) -> Vec<Value> {
    let mut row = Vec::with_capacity(names.len());
    for column in columns {
        match column {
            DocColumn::Field { path, .. } => row.push(lookup(doc, path)),
            DocColumn::Constant { value, .. } => row.push(value.clone()),
            DocColumn::All => {
                // The names `*` expanded to follow this column's position.
                let start = row.len();
                let width = names.len() - columns.len() + 1;
                for name in &names[start..start + width] {
                    row.push(doc.get(name).cloned().unwrap_or(Value::Null));
                }
            }
        }
    }
    row
}

/// The value at a dotted path, or null.
fn lookup(doc: &Value, path: &[String]) -> Value {
    let mut current = doc;
    for part in path {
        match current.get(part) {
            Some(v) => current = v,
            None => return Value::Null,
        }
    }
    current.clone()
}

fn column_type<'v>(values: impl Iterator<Item = &'v Value>) -> &'static str {
    let mut kind = "null";
    for value in values {
        let this = match value {
            Value::Null => continue,
            Value::Bool(_) => "boolean",
            Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) | Value::Object(_) => "json",
        };
        kind = match (kind, this) {
            ("null", t) => t,
            (k, t) if k == t => k,
            ("integer", "number") | ("number", "integer") => "number",
            _ => "json",
        };
    }
    kind
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(sql: &str) -> Value {
        SqlQuery::parse(sql, &[]).unwrap_or_else(|e| panic!("{}: {:#}", sql, e)).describe()
    }

    fn filter(sql: &str) -> Value {
        translate(sql)["query"]["filter"].clone()
    }

    fn error(sql: &str) -> String {
        format!("{:#}", SqlQuery::parse(sql, &[]).expect_err(sql))
    }

    #[test]
    fn where_clauses_become_filters() {
        assert_eq!(filter("SELECT * FROM t WHERE a = 1"), json!({ "a": { "$eq": 1 } }));
        assert_eq!(filter("SELECT * FROM t WHERE 5 < a"), json!({ "a": { "$gt": 5 } }));
        assert_eq!(filter("SELECT * FROM t WHERE a <> 'x'"), json!({ "a": { "$nin": ["x", null] } }));
        assert_eq!(
            filter("SELECT * FROM t WHERE a = 1 AND (b = 2 AND c >= -3.5)"),
            json!({ "$and": [{ "a": { "$eq": 1 } }, { "b": { "$eq": 2 } }, { "c": { "$gte": -3.5 } }] })
        );
        assert_eq!(
            filter("SELECT * FROM t WHERE NOT (a = 1 OR b < 2)"),
            json!({ "$and": [{ "a": { "$nin": [1, null] } }, { "b": { "$gte": 2 } }] })
        );
        assert_eq!(filter("SELECT * FROM t WHERE a IS NULL"), json!({ "a": { "$eq": null } }));
        assert_eq!(filter("SELECT * FROM t WHERE NOT a IS NULL"), json!({ "a": { "$ne": null } }));
        assert_eq!(filter("SELECT * FROM t WHERE a IN (1, 2, NULL)"), json!({ "a": { "$in": [1, 2] } }));
        assert_eq!(filter("SELECT * FROM t WHERE a NOT IN (1, 2)"), json!({ "a": { "$nin": [1, 2, null] } }));
        assert_eq!(filter("SELECT * FROM t WHERE a NOT IN (1, NULL)"), json!({ "$or": [] }));
        assert_eq!(filter("SELECT * FROM t WHERE a = NULL"), json!({ "$or": [] }));
        assert_eq!(
            filter("SELECT * FROM t WHERE a BETWEEN 1 AND 5"),
            json!({ "$and": [{ "a": { "$gte": 1 } }, { "a": { "$lte": 5 } }] })
        );
        assert_eq!(
            filter("SELECT * FROM t WHERE a NOT BETWEEN 1 AND 5"),
            json!({ "$or": [{ "a": { "$lt": 1 } }, { "a": { "$gt": 5 } }] })
        );
        assert_eq!(filter("SELECT * FROM t WHERE name LIKE 'Ad%'"), json!({ "name": { "$startsWith": "Ad" } }));
        assert_eq!(filter("SELECT * FROM t WHERE name LIKE '%a'"), json!({ "name": { "$endsWith": "a" } }));
        assert_eq!(filter("SELECT * FROM t WHERE name LIKE '%d%'"), json!({ "name": { "$contains": "d" } }));
        assert_eq!(filter("SELECT * FROM t WHERE name LIKE 'Ada'"), json!({ "name": { "$eq": "Ada" } }));
        assert_eq!(
            filter("SELECT * FROM t WHERE name NOT LIKE 'a!_%' ESCAPE '!'"),
            json!({ "name": { "$not": { "$startsWith": "a_" }, "$ne": null } })
        );
        assert_eq!(filter("SELECT * FROM t WHERE active"), json!({ "active": { "$eq": true } }));
        assert_eq!(filter("SELECT * FROM t WHERE NOT active"), json!({ "active": { "$eq": false } }));
        assert_eq!(filter("SELECT * FROM t WHERE active IS NOT TRUE"), json!({ "active": { "$ne": true } }));
        assert_eq!(filter("SELECT * FROM t WHERE 1 = 1 AND a = 2"), json!({ "a": { "$eq": 2 } }));
        assert_eq!(filter("SELECT * FROM t WHERE 1 = 0 OR a = 2"), json!({ "a": { "$eq": 2 } }));
        assert_eq!(filter("SELECT * FROM t WHERE 1 = 0"), json!({ "$or": [] }));
        assert_eq!(filter("SELECT * FROM t o WHERE o.author.name = 'Ada'"), json!({ "author.name": { "$eq": "Ada" } }));
        assert_eq!(filter("SELECT * FROM t WHERE placed >= DATE '2026-01-01'"), json!({ "placed": { "$gte": "2026-01-01" } }));
    }

    #[test]
    fn parameters_fill_in_order() {
        let params = [json!("ada"), json!(10), json!(3)];
        let q = SqlQuery::parse("SELECT * FROM t WHERE name = ? AND total > ? LIMIT ?", &params).unwrap();
        let d = q.describe();
        assert_eq!(d["query"]["filter"], json!({ "$and": [{ "name": { "$eq": "ada" } }, { "total": { "$gt": 10 } }] }));
        assert_eq!(d["limit"], json!(3));
        let q = SqlQuery::parse("SELECT * FROM t WHERE total > $2 AND name = $1", &[json!("x"), json!(1)]).unwrap();
        assert_eq!(q.describe()["query"]["filter"], json!({ "$and": [{ "total": { "$gt": 1 } }, { "name": { "$eq": "x" } }] }));
        assert!(format!("{:#}", SqlQuery::parse("SELECT * FROM t WHERE a = ?", &[]).unwrap_err()).contains("no value"));
        assert!(format!("{:#}", SqlQuery::parse("SELECT * FROM t", &[json!(1)]).unwrap_err()).contains("isn't used"));
    }

    #[test]
    fn aggregates_translate() {
        let d = translate(
            "SELECT customer, COUNT(*) AS n, SUM(total) FROM orders WHERE paid GROUP BY customer HAVING COUNT(*) > 2 ORDER BY 3 DESC LIMIT 5",
        );
        assert_eq!(d["aggregate"]["group_by"], json!(["customer"]));
        assert_eq!(d["aggregate"]["aggregates"], json!({ "$agg0": { "$count": "*" }, "$agg1": { "$sum": "total" }, "$agg2": { "$count": "total" } }));
        assert_eq!(d["aggregate"]["sort"], json!([{ "field": "$agg1", "descending": true }]));
        assert_eq!(d["having"], json!("COUNT(*) > 2"));
        assert_eq!(d["limit"], json!(5));
        assert_eq!(translate("SELECT COUNT(*) FROM t WHERE a = 1")["count"]["filter"], json!({ "a": { "$eq": 1 } }));
        assert_eq!(translate("SELECT DISTINCT status FROM t")["aggregate"]["group_by"], json!(["status"]));
        let d = translate("SELECT address.city AS city, COUNT(*) FROM t GROUP BY city ORDER BY city");
        assert_eq!(d["aggregate"]["group_by"], json!(["address.city"]));
        assert_eq!(d["aggregate"]["sort"], json!([{ "field": "address.city", "descending": false }]));
        assert_eq!(
            translate("SELECT COUNT(DISTINCT tag) FROM t")["aggregate"]["aggregates"],
            json!({ "$agg0": { "$countDistinct": "tag" } })
        );
        assert_eq!(translate("SELECT TOP 3 * FROM t")["limit"], json!(3));
        assert_eq!(translate("SELECT * FROM t FETCH FIRST 2 ROWS ONLY")["limit"], json!(2));
    }

    #[test]
    fn unsupported_sql_is_refused_with_a_reason() {
        assert!(error("DELETE FROM t").contains("read-only"));
        assert!(error("SELECT * FROM a JOIN b ON a.x = b.x").contains("JOIN"));
        assert!(error("SELECT * FROM t WHERE a = b").contains("comparing two fields"));
        assert!(error("SELECT * FROM t WHERE UPPER(a) = 'X'").contains("UPPER"));
        assert!(error("SELECT * FROM t WHERE a LIKE 'a_b'").contains("_ in the LIKE"));
        assert!(error("SELECT * FROM t WHERE a LIKE 'a%b'").contains("% only at the start or end"));
        assert!(error("SELECT name, COUNT(*) FROM t").contains("GROUP BY"));
        assert!(error("SELECT total * 2 FROM t").contains("isn't supported in SELECT"));
        assert!(error("SELECT * FROM (SELECT * FROM t) x").contains("subquery"));
        assert!(error("SELECT * FROM t UNION SELECT * FROM u").contains("UNION"));
        assert!(error("SELECT 1; SELECT 2").contains("one statement"));
    }

    #[test]
    fn having_uses_three_valued_logic() {
        let row: Map<String, Value> = serde_json::from_value(json!({ "n": 3, "s": null })).unwrap();
        let gt = Having::Compare(Operand::Key("n".into()), BinaryOperator::Gt, Operand::Value(json!(2)));
        let null = Having::Compare(Operand::Key("s".into()), BinaryOperator::Gt, Operand::Value(json!(2)));
        assert_eq!(gt.eval(&row), Some(true));
        assert_eq!(null.eval(&row), None);
        assert_eq!(Having::Not(Box::new(null.clone())).eval(&row), None);
        assert_eq!(Having::Or(vec![null.clone(), gt.clone()]).eval(&row), Some(true));
        assert_eq!(Having::And(vec![null, gt]).eval(&row), None);
    }

    #[test]
    fn column_types_are_inferred() {
        assert_eq!(column_type([json!(1), json!(null), json!(2)].iter()), "integer");
        assert_eq!(column_type([json!(1), json!(2.5)].iter()), "number");
        assert_eq!(column_type([json!("a"), json!(1)].iter()), "json");
        assert_eq!(column_type([json!(null)].iter()), "null");
    }
}
