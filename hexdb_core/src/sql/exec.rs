// HexDB Core SQL: the general executor
//
// Statements the translator in `sql.rs` can't turn into one query or
// aggregation run here: joins, subqueries, expressions over fields, CASE,
// CAST and functions, UNION / INTERSECT / EXCEPT, and WITH.
//
// A statement is compiled into a small plan, then run in two phases:
//
// 1. Each table reference is read through the engine's ordinary query path
//    (so the caller's row filters and hidden fields apply, and indexes are
//    used). WHERE and inner-join conditions that compare one table's field
//    with a value are pushed into that read. Values compared for equality
//    with a tokenized field are replaced by their tokens first (`Tokens`). A read may return at most
//    `limits.sql_max_rows` documents; more is an error, never a silently
//    short answer.
// 2. Everything else runs in memory: hash joins on equality conditions
//    (nested loops otherwise), filters, grouping and aggregates, HAVING,
//    DISTINCT, ORDER BY, LIMIT and set operations. Uncorrelated subqueries
//    and CTEs run once; correlated subqueries run per outer row. A work
//    counter stops a statement that would compare too many rows.
//
// Names: `alias.field` reads a table's field (a dotted path reads into
// objects); an unqualified name reads the one table in FROM, or, with
// several, the table whose document has that field. Outer queries' tables
// are reached by their alias.

use super::{column_type, compare_values, field_compare, in_list, invalid, like, negate, parse_number, unsupported, Cond, Params};
use crate::engine::{DocumentQuery, HexDBEngine};
use crate::filter::{compare, sort_order, Filter};
use anyhow::Result;
use serde_json::{json, Map, Number, Value};
use sqlparser::ast::{
    self, BinaryOperator, Distinct, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, JoinConstraint, JoinOperator,
    LimitClause, OrderByKind, OrderBySort, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, SetOperator, SetQuantifier,
    TableFactor, UnaryOperator,
};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Default for `limits.sql_max_rows`.
pub const DEFAULT_SQL_MAX_ROWS: usize = 100_000;
/// Deepest nesting of subqueries and set operations.
const MAX_DEPTH: usize = 32;
/// Work units (rows read, compared or produced) per allowed row.
const WORK_PER_ROW: u64 = 100;

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// A compiled statement for the general executor.
#[derive(Debug, Clone)]
pub struct General {
    root: QueryNode,
    subqueries: Vec<SubPlan>,
    ctes: Vec<QueryNode>,
    scans: Vec<ScanSpec>,
    tables: Vec<String>,
}

#[derive(Debug, Clone)]
struct SubPlan {
    query: QueryNode,
    correlated: bool,
}

/// One read of a tessellation: the filter pushed into it.
#[derive(Debug, Clone)]
struct ScanSpec {
    table: String,
    filter: Value,
}

#[derive(Debug, Clone)]
struct QueryNode {
    body: SetNode,
    /// ORDER BY on the output columns (set operations), as (column, descending, nulls first).
    order: Vec<(usize, bool, Option<bool>)>,
    offset: usize,
    limit: Option<usize>,
}

#[derive(Debug, Clone)]
enum SetNode {
    Select(Box<SelectNode>),
    Set { op: SetOperator, all: bool, left: Box<SetNode>, right: Box<SetNode> },
    Query(Box<QueryNode>),
}

#[derive(Debug, Clone)]
struct SelectNode {
    /// Binding (table) names in this scope; a row holds one value per binding.
    bindings: Vec<String>,
    from: Option<FromNode>,
    filter: Option<Ex>,
    grouped: bool,
    group_keys: Vec<Ex>,
    aggregates: Vec<AggSpec>,
    having: Option<Ex>,
    items: Vec<OutItem>,
    distinct: bool,
    /// ORDER BY keys evaluated per row: (expression, descending, nulls first).
    order: Vec<(Ex, bool, Option<bool>)>,
}

#[derive(Debug, Clone)]
enum OutItem {
    /// `*` (None) or `t.*`.
    Wildcard(Option<usize>),
    Expr(Ex, String),
}

#[derive(Debug, Clone)]
enum FromNode {
    Source { binding: usize, source: Source },
    Join { kind: JoinKind, left: Box<FromNode>, right: Box<FromNode>, on: Option<Ex>, left_bindings: Vec<usize>, right_bindings: Vec<usize> },
}

#[derive(Debug, Clone)]
enum Source {
    Scan(usize),
    Cte(usize, Vec<String>),
    Derived(Box<QueryNode>, Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
struct AggSpec {
    func: AggFunc,
    distinct: bool,
    arg: Option<Ex>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// A column reference: `depth` scopes out, a binding (or any binding of
/// that scope that has the field, when `None`), and a path into it.
#[derive(Debug, Clone, PartialEq)]
struct ColRef {
    depth: usize,
    binding: Option<usize>,
    path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Ex {
    Lit(Value),
    Col(ColRef),
    Neg(Box<Ex>),
    Not(Box<Ex>),
    Arith(Box<Ex>, Arith, Box<Ex>),
    Cmp(Box<Ex>, BinaryOperator, Box<Ex>),
    And(Box<Ex>, Box<Ex>),
    Or(Box<Ex>, Box<Ex>),
    Concat(Box<Ex>, Box<Ex>),
    IsNull(Box<Ex>, bool),
    /// IS [NOT] TRUE / FALSE: (expr, target, negated).
    IsBool(Box<Ex>, bool, bool),
    /// IS [NOT] DISTINCT FROM.
    DistinctFrom(Box<Ex>, Box<Ex>, bool),
    InList(Box<Ex>, Vec<Ex>, bool),
    InSub(Box<Ex>, usize, bool),
    /// `x op ANY (subquery)` (false) or `x op ALL (subquery)` (true).
    Quantified(Box<Ex>, BinaryOperator, usize, bool),
    Exists(usize, bool),
    Scalar(usize),
    Like { expr: Box<Ex>, pattern: Box<Ex>, escape: Option<char>, negated: bool, ci: bool },
    Case { operand: Option<Box<Ex>>, whens: Vec<(Ex, Ex)>, otherwise: Option<Box<Ex>> },
    Cast(Box<Ex>, CastTo, bool),
    Func(Func, Vec<Ex>),
    Extract(DatePart, Box<Ex>),
    Agg(usize),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Arith {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CastTo {
    Integer,
    Number,
    String,
    Boolean,
    Date,
    Timestamp,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DatePart {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    DayOfWeek,
    DayOfYear,
    Hour,
    Minute,
    Second,
    Epoch,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Func {
    Upper,
    Lower,
    Length,
    Substring,
    Trim,
    LTrim,
    RTrim,
    Concat,
    Replace,
    Left,
    Right,
    Position,
    Coalesce,
    NullIf,
    Abs,
    Round,
    Floor,
    Ceil,
    Mod,
    Power,
    Sqrt,
    Sign,
    Now,
    CurrentDate,
    CurrentTime,
    Locate,
    Exp,
    Ln,
    Log10,
    Pi,
    Truncate,
    DayName,
    MonthName,
}

impl Func {
    fn by_name(name: &str) -> Option<(Func, usize, usize)> {
        // (function, fewest arguments, most arguments)
        Some(match name {
            "upper" | "ucase" => (Func::Upper, 1, 1),
            "lower" | "lcase" => (Func::Lower, 1, 1),
            "length" | "char_length" | "character_length" | "len" => (Func::Length, 1, 1),
            "substring" | "substr" => (Func::Substring, 2, 3),
            "trim" | "btrim" => (Func::Trim, 1, 1),
            "ltrim" => (Func::LTrim, 1, 1),
            "rtrim" => (Func::RTrim, 1, 1),
            "concat" => (Func::Concat, 1, usize::MAX),
            "replace" => (Func::Replace, 3, 3),
            "left" => (Func::Left, 2, 2),
            "right" => (Func::Right, 2, 2),
            "strpos" | "instr" => (Func::Position, 2, 2),
            "coalesce" | "ifnull" | "nvl" => (Func::Coalesce, 1, usize::MAX),
            "nullif" => (Func::NullIf, 2, 2),
            "abs" => (Func::Abs, 1, 1),
            "round" => (Func::Round, 1, 2),
            "floor" => (Func::Floor, 1, 1),
            "ceil" | "ceiling" => (Func::Ceil, 1, 1),
            "mod" => (Func::Mod, 2, 2),
            "power" | "pow" => (Func::Power, 2, 2),
            "sqrt" => (Func::Sqrt, 1, 1),
            "sign" => (Func::Sign, 1, 1),
            "now" | "current_timestamp" | "localtimestamp" => (Func::Now, 0, 0),
            "current_date" | "curdate" => (Func::CurrentDate, 0, 0),
            "current_time" | "curtime" | "localtime" => (Func::CurrentTime, 0, 0),
            "locate" => (Func::Locate, 2, 3),
            "exp" => (Func::Exp, 1, 1),
            // LOG is the natural logarithm, as in ODBC.
            "ln" | "log" => (Func::Ln, 1, 1),
            "log10" => (Func::Log10, 1, 1),
            "pi" => (Func::Pi, 0, 0),
            "truncate" | "trunc" => (Func::Truncate, 1, 2),
            "dayname" => (Func::DayName, 1, 1),
            "monthname" => (Func::MonthName, 1, 1),
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Compiling
// ---------------------------------------------------------------------------

/// Columns a CTE or derived table produces, known once it has run.
#[derive(Debug, Clone)]
struct CteDef {
    name: String,
    index: usize,
    columns: Vec<String>,
}

struct Compiler<'p> {
    params: Params<'p>,
    /// Binding names of each open scope, outermost first.
    scopes: Vec<Vec<String>>,
    /// For each subquery being compiled: the scope it starts at, and whether it reads an outer one.
    correlation: Vec<(usize, bool)>,
    /// CTEs visible by name, innermost last.
    ctes_in_scope: Vec<CteDef>,
    subqueries: Vec<SubPlan>,
    ctes: Vec<QueryNode>,
    scans: Vec<ScanSpec>,
    tables: Vec<String>,
    depth: usize,
}

/// Aggregates collected while compiling one SELECT's items, HAVING and ORDER BY.
struct AggCollector {
    specs: Vec<AggSpec>,
}

impl General {
    pub(super) fn compile(query: &ast::Query, params: Params<'_>) -> Result<General> {
        let mut compiler = Compiler {
            params,
            scopes: Vec::new(),
            correlation: Vec::new(),
            ctes_in_scope: Vec::new(),
            subqueries: Vec::new(),
            ctes: Vec::new(),
            scans: Vec::new(),
            tables: Vec::new(),
            depth: 0,
        };
        let root = compiler.query(query)?;
        compiler.params.check_all_used()?;
        Ok(General { root, subqueries: compiler.subqueries, ctes: compiler.ctes, scans: compiler.scans, tables: compiler.tables })
    }

    /// Every tessellation the statement reads.
    pub fn tables(&self) -> &[String] {
        &self.tables
    }

    pub fn describe(&self) -> Value {
        json!({
            "executor": "general",
            "reads": self.scans.iter().map(|s| json!({ "tessellation": s.table, "filter": s.filter })).collect::<Vec<_>>(),
        })
    }
}

fn ident_name(name: &ast::ObjectName) -> Result<String> {
    super::single_name(name)
}

impl<'p> Compiler<'p> {
    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(invalid(format!("the statement nests more than {} queries deep.", MAX_DEPTH)));
        }
        Ok(())
    }

    fn query(&mut self, query: &ast::Query) -> Result<QueryNode> {
        self.enter()?;
        let ctes_before = self.ctes_in_scope.len();
        if let Some(with) = &query.with {
            if with.recursive {
                return Err(unsupported("WITH RECURSIVE"));
            }
            for cte in &with.cte_tables {
                // A CTE sees the CTEs before it, not the enclosing query's tables.
                let saved = std::mem::take(&mut self.scopes);
                let saved_correlation = std::mem::take(&mut self.correlation);
                let body = self.query(&cte.query);
                self.scopes = saved;
                self.correlation = saved_correlation;
                let body = body?;
                let columns = cte.alias.columns.iter().map(|c| c.name.value.clone()).collect();
                self.ctes.push(body);
                self.ctes_in_scope.push(CteDef { name: cte.alias.name.value.clone(), index: self.ctes.len() - 1, columns });
            }
        }
        if !query.locks.is_empty() || query.for_clause.is_some() || query.settings.is_some() || query.format_clause.is_some()
            || !query.pipe_operators.is_empty()
        {
            return Err(unsupported("that clause"));
        }

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
            limit = super::min_option(limit, n);
        }

        let order_by: Vec<ast::OrderByExpr> = match &query.order_by {
            None => Vec::new(),
            Some(order) => {
                if order.interpolate.is_some() {
                    return Err(unsupported("INTERPOLATE"));
                }
                match &order.kind {
                    OrderByKind::All(_) => return Err(unsupported("ORDER BY ALL")),
                    OrderByKind::Expressions(exprs) => exprs.clone(),
                }
            }
        };

        let (body, order) = match query.body.as_ref() {
            SetExpr::Select(select) => {
                let (node, top) = self.select(select, &order_by)?;
                if let Some(n) = top {
                    limit = super::min_option(limit, n);
                }
                (SetNode::Select(Box::new(node)), Vec::new())
            }
            other => {
                let body = self.set_expr(other)?;
                let names = output_names(&body);
                let mut order = Vec::new();
                for key in &order_by {
                    let (descending, nulls_first) = order_options(key)?;
                    let column = match &key.expr {
                        Expr::Value(v) if matches!(v.value, ast::Value::Number(..)) => {
                            super::position(&key.expr, names.as_ref().map_or(usize::MAX, |n| n.len()))?.ok_or_else(|| invalid("bad ORDER BY position."))?
                        }
                        Expr::Identifier(id) => names
                            .as_ref()
                            .and_then(|n| n.iter().position(|c| *c == id.value))
                            .ok_or_else(|| invalid(format!("ORDER BY {}: name a column of the result.", id.value)))?,
                        other => return Err(invalid(format!("ORDER BY '{}': after UNION, INTERSECT or EXCEPT, order by result columns.", other))),
                    };
                    order.push((column, descending, nulls_first));
                }
                (body, order)
            }
        };
        self.ctes_in_scope.truncate(ctes_before);
        self.depth -= 1;
        Ok(QueryNode { body, order, offset, limit })
    }

    fn set_expr(&mut self, expr: &SetExpr) -> Result<SetNode> {
        self.enter()?;
        let node = match expr {
            SetExpr::Select(select) => {
                let (node, top) = self.select(select, &[])?;
                if top.is_some() {
                    return Err(unsupported("TOP inside UNION, INTERSECT or EXCEPT"));
                }
                SetNode::Select(Box::new(node))
            }
            SetExpr::Query(q) => SetNode::Query(Box::new(self.query(q)?)),
            SetExpr::SetOperation { op, set_quantifier, left, right } => {
                let all = match set_quantifier {
                    SetQuantifier::All => true,
                    SetQuantifier::Distinct | SetQuantifier::None => false,
                    other => return Err(unsupported(format!("{} {}", op, other))),
                };
                let op = match op {
                    SetOperator::Minus => SetOperator::Except,
                    other => *other,
                };
                SetNode::Set { op, all, left: Box::new(self.set_expr(left)?), right: Box::new(self.set_expr(right)?) }
            }
            SetExpr::Values(_) => return Err(unsupported("VALUES")),
            other => return Err(unsupported(format!("'{}'", other))),
        };
        self.depth -= 1;
        Ok(node)
    }

    /// Compile one SELECT. Returns the plan and its TOP count.
    fn select(&mut self, select: &ast::Select, order_by: &[ast::OrderByExpr]) -> Result<(SelectNode, Option<Option<usize>>)> {
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
        let top = match &select.top {
            None => None,
            Some(top) => {
                if top.with_ties || top.percent {
                    return Err(unsupported("TOP ... WITH TIES or PERCENT"));
                }
                Some(match &top.quantity {
                    Some(ast::TopQuantity::Constant(n)) => Some(*n as usize),
                    Some(ast::TopQuantity::Expr(e)) => self.count(e, "TOP")?,
                    None => None,
                })
            }
        };

        // FROM: the scope's bindings, then the join tree (whose ON conditions
        // see every binding of the scope, like WHERE).
        let mut bindings = Vec::new();
        for table in &select.from {
            collect_bindings(&table.relation, &mut bindings)?;
            for join in &table.joins {
                collect_bindings(&join.relation, &mut bindings)?;
            }
        }
        let mut seen = HashSet::new();
        for b in &bindings {
            if !seen.insert(b.clone()) {
                return Err(invalid(format!("the table name or alias '{}' appears twice in FROM; give one an alias.", b)));
            }
        }
        self.scopes.push(bindings.clone());
        let result = self.select_in_scope(select, order_by, &bindings);
        self.scopes.pop();
        Ok((result?, top))
    }

    fn select_in_scope(&mut self, select: &ast::Select, order_by: &[ast::OrderByExpr], bindings: &[String]) -> Result<SelectNode> {
        let mut from: Option<FromNode> = None;
        let mut next = 0usize;
        // Bindings on the null-filled side of an outer join (no pushdown there).
        let mut nullable: HashSet<usize> = HashSet::new();
        for table in &select.from {
            let node = self.joined(table, &mut next, &mut nullable)?;
            from = Some(match from {
                None => node,
                Some(left) => {
                    let left_bindings = node_bindings(&left);
                    let right_bindings = node_bindings(&node);
                    FromNode::Join { kind: JoinKind::Cross, left: Box::new(left), right: Box::new(node), on: None, left_bindings, right_bindings }
                }
            });
        }

        let mut filter = match &select.selection {
            Some(expr) => Some(self.expr(expr, None)?),
            None => None,
        };

        // SELECT list.
        let mut aggs = AggCollector { specs: Vec::new() };
        let mut items = Vec::new();
        for item in &select.projection {
            items.push(match item {
                SelectItem::Wildcard(options) => {
                    super::check_wildcard(options)?;
                    if bindings.is_empty() {
                        return Err(invalid("SELECT * needs a FROM clause."));
                    }
                    OutItem::Wildcard(None)
                }
                SelectItem::QualifiedWildcard(kind, options) => {
                    super::check_wildcard(options)?;
                    let qualifier = match kind {
                        SelectItemQualifiedWildcardKind::ObjectName(name) => ident_name(name)?,
                        SelectItemQualifiedWildcardKind::Expr(e) => e.to_string(),
                    };
                    let index = bindings
                        .iter()
                        .position(|b| *b == qualifier)
                        .ok_or_else(|| invalid(format!("'{}' isn't a table or alias in FROM.", qualifier)))?;
                    OutItem::Wildcard(Some(index))
                }
                SelectItem::UnnamedExpr(expr) => OutItem::Expr(self.expr(expr, Some(&mut aggs))?, self.default_name(expr)),
                SelectItem::ExprWithAlias { expr, alias } => OutItem::Expr(self.expr(expr, Some(&mut aggs))?, alias.value.clone()),
                SelectItem::ExprWithAliases { .. } => return Err(unsupported("several aliases for one column")),
            });
        }

        // GROUP BY: expressions, select positions or select aliases.
        let group_exprs = match &select.group_by {
            GroupByExpr::All(_) => return Err(unsupported("GROUP BY ALL")),
            GroupByExpr::Expressions(exprs, modifiers) => {
                if !modifiers.is_empty() {
                    return Err(unsupported("GROUP BY modifiers (ROLLUP, CUBE, ...)"));
                }
                exprs.clone()
            }
        };
        let mut group_keys = Vec::new();
        for expr in &group_exprs {
            let key = if let Some(i) = super::position(expr, items.len())? {
                match &items[i] {
                    OutItem::Expr(e, _) => e.clone(),
                    OutItem::Wildcard(_) => return Err(invalid("GROUP BY can't refer to *.")),
                }
            } else if let Some(e) = alias_item(expr, &items) {
                e
            } else {
                self.expr(expr, None)?
            };
            if contains_agg(&key) {
                return Err(invalid("GROUP BY can't contain an aggregate."));
            }
            group_keys.push(key);
        }

        let having = match &select.having {
            Some(expr) => Some(self.expr_with_aliases(expr, &items, &mut aggs)?),
            None => None,
        };

        // ORDER BY: positions, select aliases, or expressions over the row.
        let mut order = Vec::new();
        for key in order_by {
            let (descending, nulls_first) = order_options(key)?;
            let ex = if let Some(i) = super::position(&key.expr, items.len())? {
                match &items[i] {
                    OutItem::Expr(e, _) => e.clone(),
                    OutItem::Wildcard(_) => return Err(invalid("ORDER BY can't refer to *.")),
                }
            } else {
                self.expr_with_aliases(&key.expr, &items, &mut aggs)?
            };
            order.push((ex, descending, nulls_first));
        }

        let distinct = match &select.distinct {
            None | Some(Distinct::All) => false,
            Some(Distinct::Distinct) => true,
            Some(Distinct::On(_)) => return Err(unsupported("DISTINCT ON")),
        };
        let grouped = !group_keys.is_empty() || !aggs.specs.is_empty() || having.is_some();
        if grouped {
            for item in &items {
                match item {
                    OutItem::Wildcard(_) => return Err(invalid("SELECT * can't be combined with GROUP BY or aggregates.")),
                    OutItem::Expr(e, name) => check_grouped(e, &group_keys).map_err(|_| not_grouped(name))?,
                }
            }
            if let Some(h) = &having {
                check_grouped(h, &group_keys).map_err(|_| invalid("HAVING may only use grouped columns and aggregates."))?;
            }
            for (e, _, _) in &order {
                check_grouped(e, &group_keys).map_err(|_| invalid("ORDER BY may only use grouped columns and aggregates here."))?;
            }
        }
        if let Some(f) = &filter {
            if contains_agg(f) {
                return Err(invalid("aggregates aren't allowed in WHERE; use HAVING."));
            }
        }

        // Pushdown: WHERE conjuncts that compare one table's field with a value
        // go into that table's read. Inner and cross joins also take WHERE
        // conditions that connect their two sides (so `FROM a, b WHERE a.x =
        // b.y` is a hash join).
        if let Some(node) = from.as_mut() {
            if let Some(f) = filter.take() {
                let mut rest = Vec::new();
                for conjunct in conjuncts(f) {
                    if !attach_to_join(node, &conjunct) {
                        rest.push(conjunct);
                    }
                }
                filter = and_all(rest);
            }
            let mut pushed: HashMap<usize, Vec<Ex>> = HashMap::new();
            if let Some(f) = &filter {
                for c in conjuncts(f.clone()) {
                    if let Some(b) = single_binding(&c) {
                        if !nullable.contains(&b) {
                            pushed.entry(b).or_default().push(c);
                        }
                    }
                }
            }
            collect_join_pushdown(node, &nullable, &mut pushed);
            self.apply_pushdown(node, &pushed);
        }

        Ok(SelectNode {
            bindings: bindings.to_vec(),
            from,
            filter,
            grouped,
            group_keys,
            aggregates: aggs.specs,
            having,
            items,
            distinct,
            order,
        })
    }

    /// A FROM item and its joins, as one tree.
    fn joined(&mut self, table: &ast::TableWithJoins, next: &mut usize, nullable: &mut HashSet<usize>) -> Result<FromNode> {
        let mut node = self.source(&table.relation, next, nullable)?;
        for join in &table.joins {
            let right = self.source(&join.relation, next, nullable)?;
            let (kind, constraint) = match &join.join_operator {
                JoinOperator::Join(c) | JoinOperator::Inner(c) => (JoinKind::Inner, Some(c)),
                JoinOperator::Left(c) | JoinOperator::LeftOuter(c) => (JoinKind::Left, Some(c)),
                JoinOperator::Right(c) | JoinOperator::RightOuter(c) => (JoinKind::Right, Some(c)),
                JoinOperator::FullOuter(c) => (JoinKind::Full, Some(c)),
                JoinOperator::CrossJoin(c) => (JoinKind::Cross, Some(c)),
                other => return Err(unsupported(format!("{:?} joins", other).split('(').next().unwrap_or("that join").to_string())),
            };
            let left_bindings = node_bindings(&node);
            let right_bindings = node_bindings(&right);
            let on = match constraint {
                Some(JoinConstraint::On(expr)) => Some(self.expr(expr, None)?),
                Some(JoinConstraint::Using(names)) => {
                    let mut on: Option<Ex> = None;
                    for name in names {
                        let column = ident_name(name)?;
                        let (Some(&l), Some(&r)) = (left_bindings.last(), right_bindings.first()) else {
                            return Err(invalid("USING needs a table on each side."));
                        };
                        let eq = Ex::Cmp(
                            Box::new(Ex::Col(ColRef { depth: 0, binding: Some(l), path: vec![column.clone()] })),
                            BinaryOperator::Eq,
                            Box::new(Ex::Col(ColRef { depth: 0, binding: Some(r), path: vec![column] })),
                        );
                        on = Some(match on {
                            None => eq,
                            Some(prev) => Ex::And(Box::new(prev), Box::new(eq)),
                        });
                    }
                    on
                }
                Some(JoinConstraint::Natural) => return Err(unsupported("NATURAL joins (name the columns with ON or USING)")),
                Some(JoinConstraint::None) | None => {
                    if kind != JoinKind::Cross {
                        return Err(invalid("this join needs ON or USING."));
                    }
                    None
                }
            };
            match kind {
                JoinKind::Left => nullable.extend(right_bindings.iter().copied()),
                JoinKind::Right => nullable.extend(left_bindings.iter().copied()),
                JoinKind::Full => {
                    nullable.extend(left_bindings.iter().copied());
                    nullable.extend(right_bindings.iter().copied());
                }
                _ => {}
            }
            node = FromNode::Join { kind, left: Box::new(node), right: Box::new(right), on, left_bindings, right_bindings };
        }
        Ok(node)
    }

    /// Give each scan in the tree the filter for what can be pushed to it.
    fn apply_pushdown(&mut self, node: &FromNode, pushed: &HashMap<usize, Vec<Ex>>) {
        match node {
            FromNode::Source { binding, source: Source::Scan(scan) } => {
                let Some(conds) = pushed.get(binding) else { return };
                let parts: Vec<Cond> = conds.iter().filter_map(to_cond).collect();
                if parts.is_empty() {
                    return;
                }
                let combined = Cond::combine(true, parts).into_json();
                if Filter::parse(&combined).is_ok() {
                    self.scans[*scan].filter = combined;
                }
            }
            FromNode::Source { .. } => {}
            FromNode::Join { left, right, .. } => {
                self.apply_pushdown(left, pushed);
                self.apply_pushdown(right, pushed);
            }
        }
    }

    /// A table reference: a tessellation, a CTE, or a derived table.
    fn source(&mut self, factor: &TableFactor, next: &mut usize, nullable: &mut HashSet<usize>) -> Result<FromNode> {
        if let TableFactor::NestedJoin { table_with_joins, alias: None } = factor {
            return self.joined(table_with_joins, next, nullable);
        }
        let binding = *next;
        *next += 1;
        let source = match factor {
            TableFactor::Table { name, alias, args, .. } => {
                if args.is_some() {
                    return Err(unsupported("a table function in FROM"));
                }
                if alias.as_ref().is_some_and(|a| !a.columns.is_empty()) {
                    return Err(unsupported("column aliases on a table"));
                }
                let table = ident_name(name)?;
                if let Some(cte) = self.ctes_in_scope.iter().rev().find(|c| c.name == table) {
                    Source::Cte(cte.index, cte.columns.clone())
                } else {
                    if !self.tables.contains(&table) {
                        self.tables.push(table.clone());
                    }
                    self.scans.push(ScanSpec { table, filter: json!({}) });
                    Source::Scan(self.scans.len() - 1)
                }
            }
            TableFactor::Derived { lateral, subquery, alias, .. } => {
                if *lateral {
                    return Err(unsupported("LATERAL"));
                }
                if alias.is_none() {
                    return Err(invalid("a subquery in FROM needs an alias: (SELECT ...) AS name."));
                }
                // A derived table doesn't see the tables beside it.
                let saved = std::mem::take(&mut self.scopes);
                let saved_correlation = std::mem::take(&mut self.correlation);
                let query = self.query(subquery);
                self.scopes = saved;
                self.correlation = saved_correlation;
                let columns = alias.as_ref().map(|a| a.columns.iter().map(|c| c.name.value.clone()).collect()).unwrap_or_default();
                Source::Derived(Box::new(query?), columns)
            }
            TableFactor::NestedJoin { .. } => return Err(unsupported("an alias on parenthesized joins")),
            other => return Err(unsupported(format!("'{}' in FROM", other))),
        };
        Ok(FromNode::Source { binding, source })
    }

    // -- Expressions ---------------------------------------------------------

    /// An expression where select aliases may stand for their expressions
    /// (HAVING and ORDER BY), unless the name is also a table's field.
    fn expr_with_aliases(&mut self, expr: &Expr, items: &[OutItem], aggs: &mut AggCollector) -> Result<Ex> {
        if let Some(e) = alias_item(expr, items) {
            return Ok(e);
        }
        self.expr(expr, Some(aggs))
    }

    fn sub(&mut self, query: &ast::Query) -> Result<usize> {
        self.correlation.push((self.scopes.len(), false));
        let compiled = self.query(query);
        let (_, correlated) = self.correlation.pop().unwrap_or((0, true));
        self.subqueries.push(SubPlan { query: compiled?, correlated });
        Ok(self.subqueries.len() - 1)
    }

    fn expr(&mut self, expr: &Expr, mut aggs: Option<&mut AggCollector>) -> Result<Ex> {
        self.enter()?;
        let result = self.expr_inner(expr, &mut aggs);
        self.depth -= 1;
        result
    }

    fn expr_inner(&mut self, expr: &Expr, aggs: &mut Option<&mut AggCollector>) -> Result<Ex> {
        macro_rules! e {
            ($x:expr) => {
                Box::new(self.expr($x, aggs.as_deref_mut())?)
            };
        }
        Ok(match expr {
            Expr::Nested(inner) => self.expr(inner, aggs.as_deref_mut())?,
            Expr::Identifier(id) => Ex::Col(self.column(std::slice::from_ref(&id.value))?),
            Expr::CompoundIdentifier(parts) => {
                let names: Vec<String> = parts.iter().map(|p| p.value.clone()).collect();
                Ex::Col(self.column(&names)?)
            }
            Expr::Value(_) | Expr::TypedString(_) => Ex::Lit(self.literal(expr)?),
            Expr::UnaryOp { op, expr: inner } => match op {
                UnaryOperator::Not => Ex::Not(e!(inner)),
                UnaryOperator::Minus => match self.expr(inner, aggs.as_deref_mut())? {
                    Ex::Lit(Value::Number(n)) => Ex::Lit(Value::Number(negate(&n)?)),
                    other => Ex::Neg(Box::new(other)),
                },
                UnaryOperator::Plus => self.expr(inner, aggs.as_deref_mut())?,
                other => return Err(unsupported(format!("the operator {}", other))),
            },
            Expr::BinaryOp { left, op, right } => {
                let (l, r) = (e!(left), e!(right));
                match op {
                    BinaryOperator::And => Ex::And(l, r),
                    BinaryOperator::Or => Ex::Or(l, r),
                    BinaryOperator::Plus => Ex::Arith(l, Arith::Add, r),
                    BinaryOperator::Minus => Ex::Arith(l, Arith::Sub, r),
                    BinaryOperator::Multiply => Ex::Arith(l, Arith::Mul, r),
                    BinaryOperator::Divide => Ex::Arith(l, Arith::Div, r),
                    BinaryOperator::Modulo => Ex::Arith(l, Arith::Mod, r),
                    BinaryOperator::StringConcat => Ex::Concat(l, r),
                    op if super::comparison(op) => Ex::Cmp(l, op.clone(), r),
                    other => return Err(unsupported(format!("the operator {}", other))),
                }
            }
            Expr::IsNull(inner) => Ex::IsNull(e!(inner), false),
            Expr::IsNotNull(inner) => Ex::IsNull(e!(inner), true),
            Expr::IsTrue(inner) => Ex::IsBool(e!(inner), true, false),
            Expr::IsNotTrue(inner) => Ex::IsBool(e!(inner), true, true),
            Expr::IsFalse(inner) => Ex::IsBool(e!(inner), false, false),
            Expr::IsNotFalse(inner) => Ex::IsBool(e!(inner), false, true),
            Expr::IsDistinctFrom(a, b) => Ex::DistinctFrom(e!(a), e!(b), false),
            Expr::IsNotDistinctFrom(a, b) => Ex::DistinctFrom(e!(a), e!(b), true),
            Expr::InList { expr: inner, list, negated } => {
                let target = e!(inner);
                let mut values = Vec::with_capacity(list.len());
                for item in list {
                    values.push(self.expr(item, aggs.as_deref_mut())?);
                }
                Ex::InList(target, values, *negated)
            }
            Expr::InSubquery { expr: inner, subquery, negated } => {
                let target = e!(inner);
                Ex::InSub(target, self.sub(subquery)?, *negated)
            }
            Expr::Exists { subquery, negated } => Ex::Exists(self.sub(subquery)?, *negated),
            Expr::AnyOp { left, compare_op, right, .. } | Expr::AllOp { left, compare_op, right } => {
                let Expr::Subquery(q) = right.as_ref() else {
                    return Err(unsupported("ANY / ALL over something other than a subquery"));
                };
                if !super::comparison(compare_op) {
                    return Err(invalid(format!("ANY / ALL needs a comparison, not {}.", compare_op)));
                }
                let target = e!(left);
                Ex::Quantified(target, compare_op.clone(), self.sub(q)?, matches!(expr, Expr::AllOp { .. }))
            }
            Expr::Subquery(q) => Ex::Scalar(self.sub(q)?),
            Expr::Between { expr: inner, negated, low, high } => {
                let (target, low, high) = (self.expr(inner, aggs.as_deref_mut())?, self.expr(low, aggs.as_deref_mut())?, self.expr(high, aggs.as_deref_mut())?);
                let test = Ex::And(
                    Box::new(Ex::Cmp(Box::new(target.clone()), BinaryOperator::GtEq, Box::new(low))),
                    Box::new(Ex::Cmp(Box::new(target), BinaryOperator::LtEq, Box::new(high))),
                );
                if *negated {
                    Ex::Not(Box::new(test))
                } else {
                    test
                }
            }
            Expr::Like { negated, any, expr: inner, pattern, escape_char } | Expr::ILike { negated, any, expr: inner, pattern, escape_char } => {
                if *any {
                    return Err(unsupported("LIKE ANY"));
                }
                let escape = match escape_char {
                    None => None,
                    Some(c) => match self.literal(c)? {
                        Value::String(s) if s.chars().count() == 1 => s.chars().next(),
                        _ => return Err(invalid("ESCAPE takes one character.")),
                    },
                };
                Ex::Like { expr: e!(inner), pattern: e!(pattern), escape, negated: *negated, ci: matches!(expr, Expr::ILike { .. }) }
            }
            Expr::Case { operand, conditions, else_result, .. } => {
                let operand = match operand {
                    Some(o) => Some(e!(o)),
                    None => None,
                };
                let mut whens = Vec::new();
                for when in conditions {
                    whens.push((self.expr(&when.condition, aggs.as_deref_mut())?, self.expr(&when.result, aggs.as_deref_mut())?));
                }
                let otherwise = match else_result {
                    Some(e) => Some(e!(e)),
                    None => None,
                };
                Ex::Case { operand, whens, otherwise }
            }
            Expr::Cast { kind, expr: inner, data_type, format, .. } => {
                if format.is_some() {
                    return Err(unsupported("CAST ... FORMAT"));
                }
                let lenient = !matches!(kind, ast::CastKind::Cast | ast::CastKind::DoubleColon);
                Ex::Cast(e!(inner), cast_target(data_type)?, lenient)
            }
            Expr::Extract { field, expr: inner, .. } => Ex::Extract(date_part(field)?, e!(inner)),
            Expr::Substring { expr: inner, substring_from, substring_for, .. } => {
                let mut args = vec![self.expr(inner, aggs.as_deref_mut())?];
                args.push(match substring_from {
                    Some(f) => self.expr(f, aggs.as_deref_mut())?,
                    None => Ex::Lit(json!(1)),
                });
                if let Some(f) = substring_for {
                    args.push(self.expr(f, aggs.as_deref_mut())?);
                }
                Ex::Func(Func::Substring, args)
            }
            Expr::Trim { expr: inner, trim_where, trim_what, trim_characters } => {
                if trim_what.is_some() || trim_characters.is_some() {
                    return Err(unsupported("TRIM of characters other than spaces"));
                }
                let func = match trim_where {
                    None | Some(ast::TrimWhereField::Both) => Func::Trim,
                    Some(ast::TrimWhereField::Leading) => Func::LTrim,
                    Some(ast::TrimWhereField::Trailing) => Func::RTrim,
                };
                Ex::Func(func, vec![self.expr(inner, aggs.as_deref_mut())?])
            }
            Expr::Position { expr: needle, r#in } => {
                // POSITION(a IN b) is STRPOS(b, a).
                let (haystack, needle) = (self.expr(r#in, aggs.as_deref_mut())?, self.expr(needle, aggs.as_deref_mut())?);
                Ex::Func(Func::Position, vec![haystack, needle])
            }
            Expr::Ceil { expr: inner, field } | Expr::Floor { expr: inner, field } => {
                if !matches!(field, ast::CeilFloorKind::DateTimeField(ast::DateTimeField::NoDateTime)) {
                    return Err(unsupported("CEIL / FLOOR with a scale or date part"));
                }
                let func = if matches!(expr, Expr::Ceil { .. }) { Func::Ceil } else { Func::Floor };
                Ex::Func(func, vec![self.expr(inner, aggs.as_deref_mut())?])
            }
            Expr::Function(function) => self.function(function, aggs)?,
            other => return Err(unsupported(format!("'{}'", other))),
        })
    }

    fn function(&mut self, function: &ast::Function, aggs: &mut Option<&mut AggCollector>) -> Result<Ex> {
        let name = ident_name(&function.name)?.to_ascii_lowercase();
        if function.over.is_some() {
            return Err(unsupported(format!("window functions ({} OVER ...)", name.to_uppercase())));
        }
        if function.filter.is_some() || !function.within_group.is_empty() || function.null_treatment.is_some() {
            return Err(unsupported(format!("FILTER or WITHIN GROUP on {}", name.to_uppercase())));
        }
        if !matches!(function.parameters, FunctionArguments::None) {
            return Err(unsupported(format!("parameters on {}", name.to_uppercase())));
        }
        let (list, distinct): (Vec<&FunctionArg>, bool) = match &function.args {
            FunctionArguments::None => (Vec::new(), false),
            FunctionArguments::List(list) => {
                if !list.clauses.is_empty() {
                    return Err(unsupported(format!("clauses inside {}", name.to_uppercase())));
                }
                (list.args.iter().collect(), matches!(list.duplicate_treatment, Some(ast::DuplicateTreatment::Distinct)))
            }
            FunctionArguments::Subquery(_) => return Err(unsupported(format!("a subquery as the argument of {}", name.to_uppercase()))),
        };

        let agg = match name.as_str() {
            "count" => Some(AggFunc::Count),
            "sum" => Some(AggFunc::Sum),
            "avg" => Some(AggFunc::Avg),
            "min" => Some(AggFunc::Min),
            "max" => Some(AggFunc::Max),
            _ => None,
        };
        if let Some(func) = agg {
            let Some(collector) = aggs.as_deref_mut() else {
                return Err(invalid(format!("{} isn't allowed here (aggregates go in SELECT, HAVING and ORDER BY).", name.to_uppercase())));
            };
            let [FunctionArg::Unnamed(arg)] = list.as_slice() else {
                return Err(invalid(format!("{} takes one argument.", name.to_uppercase())));
            };
            let arg = match arg {
                FunctionArgExpr::Wildcard if func == AggFunc::Count && !distinct => None,
                FunctionArgExpr::Expr(e) => {
                    // No aggregates inside an aggregate.
                    let compiled = self.expr(e, None)?;
                    Some(compiled)
                }
                _ => return Err(invalid(format!("{} takes one expression.", name.to_uppercase()))),
            };
            let spec = AggSpec { func, distinct, arg };
            let index = match collector.specs.iter().position(|s| *s == spec) {
                Some(i) => i,
                None => {
                    collector.specs.push(spec);
                    collector.specs.len() - 1
                }
            };
            return Ok(Ex::Agg(index));
        }

        if distinct {
            return Err(invalid(format!("DISTINCT only goes inside an aggregate, not {}.", name.to_uppercase())));
        }
        let part = match name.as_str() {
            "year" => Some(DatePart::Year),
            "quarter" => Some(DatePart::Quarter),
            "month" => Some(DatePart::Month),
            "week" => Some(DatePart::Week),
            "day" | "dayofmonth" => Some(DatePart::Day),
            "dayofweek" => Some(DatePart::DayOfWeek),
            "dayofyear" => Some(DatePart::DayOfYear),
            "hour" => Some(DatePart::Hour),
            "minute" => Some(DatePart::Minute),
            "second" => Some(DatePart::Second),
            _ => None,
        };
        if let Some(part) = part {
            let [FunctionArg::Unnamed(FunctionArgExpr::Expr(arg))] = list.as_slice() else {
                return Err(invalid(format!("{} takes one argument.", name.to_uppercase())));
            };
            let extracted = Ex::Extract(part, Box::new(self.expr(arg, aggs.as_deref_mut())?));
            // ODBC's DAYOFWEEK counts from 1 (Sunday); EXTRACT(DOW) from 0.
            return Ok(if part == DatePart::DayOfWeek { Ex::Arith(Box::new(extracted), Arith::Add, Box::new(Ex::Lit(json!(1)))) } else { extracted });
        }
        let Some((func, min, max)) = Func::by_name(&name) else {
            return Err(unsupported(format!("the function {}", name.to_uppercase())));
        };
        let mut args = Vec::with_capacity(list.len());
        for arg in list {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg else {
                return Err(invalid(format!("{} takes plain arguments.", name.to_uppercase())));
            };
            args.push(self.expr(e, aggs.as_deref_mut())?);
        }
        if args.len() < min || args.len() > max {
            return Err(invalid(format!("{} takes {} argument(s), not {}.", name.to_uppercase(), arg_range(min, max), args.len())));
        }
        Ok(Ex::Func(func, args))
    }

    /// Resolve a (possibly qualified) name to a column reference.
    fn column(&mut self, names: &[String]) -> Result<ColRef> {
        if names.len() > 1 {
            for (s, scope) in self.scopes.iter().enumerate().rev() {
                if let Some(b) = scope.iter().position(|b| *b == names[0]) {
                    self.note_scope(s);
                    return Ok(ColRef { depth: self.scopes.len() - 1 - s, binding: Some(b), path: names[1..].to_vec() });
                }
            }
        }
        // An unqualified name (or a dotted path into an object) reads the current scope.
        let Some(scope) = self.scopes.last() else {
            return Err(invalid(format!("'{}' needs a FROM clause.", names.join("."))));
        };
        if scope.is_empty() {
            return Err(invalid(format!("'{}' needs a FROM clause.", names.join("."))));
        }
        let binding = if scope.len() == 1 { Some(0) } else { None };
        Ok(ColRef { depth: 0, binding, path: names.to_vec() })
    }

    /// A reference to scope `s`: subqueries that started inside it are correlated.
    fn note_scope(&mut self, s: usize) {
        for (start, correlated) in self.correlation.iter_mut() {
            if s < *start {
                *correlated = true;
            }
        }
    }

    fn literal(&mut self, expr: &Expr) -> Result<Value> {
        Ok(match expr {
            Expr::Value(v) => match &v.value {
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
            },
            Expr::TypedString(typed) => match &typed.value.value {
                ast::Value::SingleQuotedString(s) => Value::String(s.clone()),
                other => return Err(unsupported(format!("the value {}", other))),
            },
            other => return Err(invalid(format!("'{}' must be a value.", other))),
        })
    }

    fn count(&mut self, expr: &Expr, clause: &str) -> Result<Option<usize>> {
        let value = match expr {
            Expr::Value(_) => self.literal(expr)?,
            other => return Err(invalid(format!("{} must be a number, not '{}'.", clause, other))),
        };
        match value {
            Value::Null => Ok(None),
            Value::Number(n) if n.as_u64().is_some() => Ok(Some(n.as_u64().unwrap_or(0) as usize)),
            other => Err(invalid(format!("{} must be a non-negative integer, not {}.", clause, other))),
        }
    }

    fn default_name(&self, expr: &Expr) -> String {
        match expr {
            Expr::Nested(e) => self.default_name(e),
            Expr::Identifier(id) => id.value.clone(),
            Expr::CompoundIdentifier(parts) => {
                let names: Vec<&str> = parts.iter().map(|p| p.value.as_str()).collect();
                let qualified = names.len() > 1 && self.scopes.iter().any(|s| s.iter().any(|b| b == names[0]));
                if qualified {
                    names[1..].join(".")
                } else {
                    names.join(".")
                }
            }
            other => other.to_string(),
        }
    }
}

fn arg_range(min: usize, max: usize) -> String {
    if min == max {
        min.to_string()
    } else if max == usize::MAX {
        format!("{} or more", min)
    } else {
        format!("{} to {}", min, max)
    }
}

fn not_grouped(name: &str) -> anyhow::Error {
    invalid(format!("'{}' must be in GROUP BY or inside an aggregate.", name))
}

fn collect_bindings(factor: &TableFactor, out: &mut Vec<String>) -> Result<()> {
    match factor {
        TableFactor::Table { name, alias, .. } => out.push(match alias {
            Some(a) => a.name.value.clone(),
            None => ident_name(name)?,
        }),
        TableFactor::Derived { alias: Some(a), .. } => out.push(a.name.value.clone()),
        TableFactor::Derived { alias: None, .. } => return Err(invalid("a subquery in FROM needs an alias: (SELECT ...) AS name.")),
        TableFactor::NestedJoin { table_with_joins, alias: None } => {
            collect_bindings(&table_with_joins.relation, out)?;
            for join in &table_with_joins.joins {
                collect_bindings(&join.relation, out)?;
            }
        }
        TableFactor::NestedJoin { alias: Some(_), .. } => return Err(unsupported("an alias on parenthesized joins")),
        other => return Err(unsupported(format!("'{}' in FROM", other))),
    }
    Ok(())
}

fn node_bindings(node: &FromNode) -> Vec<usize> {
    match node {
        FromNode::Source { binding, .. } => vec![*binding],
        FromNode::Join { left_bindings, right_bindings, .. } => left_bindings.iter().chain(right_bindings).copied().collect(),
    }
}

/// The select item an unqualified name aliases, if any.
fn alias_item(expr: &Expr, items: &[OutItem]) -> Option<Ex> {
    let Expr::Identifier(id) = expr else { return None };
    items.iter().find_map(|i| match i {
        OutItem::Expr(e, name) if *name == id.value && !matches!(e, Ex::Col(c) if c.path == [id.value.clone()]) => Some(e.clone()),
        _ => None,
    })
}

fn order_options(key: &ast::OrderByExpr) -> Result<(bool, Option<bool>)> {
    if key.with_fill.is_some() {
        return Err(unsupported("WITH FILL"));
    }
    let descending = match &key.options.sort {
        None | Some(OrderBySort::Asc) => false,
        Some(OrderBySort::Desc) => true,
        Some(other) => return Err(unsupported(format!("ORDER BY ... {:?}", other))),
    };
    Ok((descending, key.options.nulls_first))
}

fn cast_target(data_type: &ast::DataType) -> Result<CastTo> {
    let text = data_type.to_string().to_ascii_uppercase();
    let base = text.split(['(', ' ']).next().unwrap_or("");
    Ok(match base {
        "INT" | "INTEGER" | "BIGINT" | "SMALLINT" | "TINYINT" | "INT2" | "INT4" | "INT8" | "INT64" | "SIGNED" | "UNSIGNED" => CastTo::Integer,
        "REAL" | "FLOAT" | "FLOAT4" | "FLOAT8" | "FLOAT64" | "DOUBLE" | "DECIMAL" | "DEC" | "NUMERIC" | "NUMBER" => CastTo::Number,
        "CHAR" | "VARCHAR" | "NVARCHAR" | "NCHAR" | "TEXT" | "STRING" | "CHARACTER" | "CLOB" => CastTo::String,
        "BOOLEAN" | "BOOL" | "BIT" => CastTo::Boolean,
        "DATE" => CastTo::Date,
        "TIMESTAMP" | "DATETIME" | "TIMESTAMPTZ" => CastTo::Timestamp,
        "JSON" | "JSONB" => CastTo::Json,
        _ => return Err(unsupported(format!("CAST to {}", text))),
    })
}

fn date_part(field: &ast::DateTimeField) -> Result<DatePart> {
    use ast::DateTimeField as F;
    Ok(match field {
        F::Year | F::Years => DatePart::Year,
        F::Quarter => DatePart::Quarter,
        F::Month | F::Months => DatePart::Month,
        F::Week(_) | F::Weeks => DatePart::Week,
        F::Day | F::Days => DatePart::Day,
        F::DayOfWeek | F::Dow => DatePart::DayOfWeek,
        F::DayOfYear | F::Doy => DatePart::DayOfYear,
        F::Hour | F::Hours => DatePart::Hour,
        F::Minute | F::Minutes => DatePart::Minute,
        F::Second | F::Seconds => DatePart::Second,
        F::Epoch => DatePart::Epoch,
        other => return Err(unsupported(format!("EXTRACT({})", other))),
    })
}

// -- Plan analysis -----------------------------------------------------------

fn conjuncts(ex: Ex) -> Vec<Ex> {
    match ex {
        Ex::And(a, b) => {
            let mut out = conjuncts(*a);
            out.extend(conjuncts(*b));
            out
        }
        other => vec![other],
    }
}

fn and_all(parts: Vec<Ex>) -> Option<Ex> {
    parts.into_iter().reduce(|a, b| Ex::And(Box::new(a), Box::new(b)))
}

/// Visit every column reference of an expression (not inside subqueries),
/// with the depth adjusted to the expression's own scope.
fn columns_of<'e>(ex: &'e Ex, out: &mut Vec<&'e ColRef>) -> bool {
    // Returns false when the expression has subqueries or aggregates.
    match ex {
        Ex::Lit(_) => true,
        Ex::Col(c) => {
            out.push(c);
            true
        }
        Ex::Neg(a) | Ex::Not(a) | Ex::IsNull(a, _) | Ex::IsBool(a, _, _) | Ex::Cast(a, _, _) | Ex::Extract(_, a) => columns_of(a, out),
        Ex::Arith(a, _, b) | Ex::Cmp(a, _, b) | Ex::And(a, b) | Ex::Or(a, b) | Ex::Concat(a, b) | Ex::DistinctFrom(a, b, _) => {
            columns_of(a, out) && columns_of(b, out)
        }
        Ex::InList(a, list, _) => columns_of(a, out) && list.iter().all(|e| columns_of(e, out)),
        Ex::Like { expr, pattern, .. } => columns_of(expr, out) && columns_of(pattern, out),
        Ex::Case { operand, whens, otherwise } => {
            operand.as_ref().is_none_or(|o| columns_of(o, out))
                && whens.iter().all(|(w, t)| columns_of(w, out) && columns_of(t, out))
                && otherwise.as_ref().is_none_or(|o| columns_of(o, out))
        }
        Ex::Func(_, args) => args.iter().all(|e| columns_of(e, out)),
        Ex::InSub(..) | Ex::Quantified(..) | Ex::Exists(..) | Ex::Scalar(_) | Ex::Agg(_) => false,
    }
}

/// The one binding of this scope an expression reads, if it reads exactly one
/// (by an explicit reference) and nothing else.
fn single_binding(ex: &Ex) -> Option<usize> {
    let mut cols = Vec::new();
    if !columns_of(ex, &mut cols) {
        return None;
    }
    let mut found = None;
    for c in cols {
        if c.depth != 0 {
            return None;
        }
        let b = c.binding?;
        match found {
            None => found = Some(b),
            Some(f) if f == b => {}
            Some(_) => return None,
        }
    }
    found
}

/// The bindings an expression reads in this scope, if all references are explicit and local.
fn bindings_read(ex: &Ex) -> Option<HashSet<usize>> {
    let mut cols = Vec::new();
    if !columns_of(ex, &mut cols) {
        return None;
    }
    let mut out = HashSet::new();
    for c in cols {
        if c.depth != 0 {
            return None;
        }
        out.insert(c.binding?);
    }
    Some(out)
}

/// Move a WHERE conjunct into the innermost inner or cross join whose two
/// sides it connects. Returns false if no join took it.
fn attach_to_join(node: &mut FromNode, conjunct: &Ex) -> bool {
    let Some(read) = bindings_read(conjunct) else { return false };
    if read.len() < 2 {
        return false;
    }
    attach_inner(node, conjunct, &read)
}

fn attach_inner(node: &mut FromNode, conjunct: &Ex, read: &HashSet<usize>) -> bool {
    let FromNode::Join { kind, left, right, on, left_bindings, right_bindings } = node else { return false };
    let all: HashSet<usize> = left_bindings.iter().chain(right_bindings.iter()).copied().collect();
    if !read.is_subset(&all) {
        return false;
    }
    // A deeper join covering everything takes it first.
    if attach_inner(left, conjunct, read) || attach_inner(right, conjunct, read) {
        return true;
    }
    if !matches!(kind, JoinKind::Inner | JoinKind::Cross) {
        return false;
    }
    *kind = JoinKind::Inner;
    *on = Some(match on.take() {
        None => conjunct.clone(),
        Some(prev) => Ex::And(Box::new(prev), Box::new(conjunct.clone())),
    });
    true
}

/// ON conditions of inner joins that read one table can be pushed to it.
fn collect_join_pushdown(node: &FromNode, nullable: &HashSet<usize>, pushed: &mut HashMap<usize, Vec<Ex>>) {
    if let FromNode::Join { kind, left, right, on, right_bindings, left_bindings } = node {
        if let Some(on) = on {
            for c in conjuncts(on.clone()) {
                if let Some(b) = single_binding(&c) {
                    // A LEFT join's ON may filter its right side's read (unmatched
                    // left rows still come out), and vice versa for RIGHT.
                    let ok = match kind {
                        JoinKind::Inner | JoinKind::Cross => !nullable.contains(&b),
                        JoinKind::Left => right_bindings.contains(&b) && right_bindings.len() == 1,
                        JoinKind::Right => left_bindings.contains(&b) && left_bindings.len() == 1,
                        JoinKind::Full => false,
                    };
                    if ok {
                        pushed.entry(b).or_default().push(c);
                    }
                }
            }
        }
        collect_join_pushdown(left, nullable, pushed);
        collect_join_pushdown(right, nullable, pushed);
    }
}

/// A conjunct as a HexDB filter on one table, when it has that form.
fn to_cond(ex: &Ex) -> Option<Cond> {
    let field = |e: &Ex| match e {
        Ex::Col(c) if c.depth == 0 && c.binding.is_some() && !c.path.is_empty() => Some(c.path.join(".")),
        _ => None,
    };
    let lit = |e: &Ex| match e {
        Ex::Lit(v) => Some(v.clone()),
        _ => None,
    };
    match ex {
        Ex::Cmp(a, op, b) => match (field(a), lit(b), lit(a), field(b)) {
            (Some(f), Some(v), _, _) => Some(field_compare(&f, op.clone(), v, false)),
            (_, _, Some(v), Some(f)) => Some(field_compare(&f, super::flip(op), v, false)),
            _ => None,
        },
        Ex::IsNull(a, negated) => {
            let f = field(a)?;
            Some(Cond::Filter(json!({ f: { (if *negated { "$ne" } else { "$eq" }): null } })))
        }
        Ex::InList(a, list, negated) => {
            let f = field(a)?;
            let values = list.iter().map(lit).collect::<Option<Vec<_>>>()?;
            Some(in_list(&f, values, *negated))
        }
        Ex::Like { expr, pattern, escape, negated, ci: false } => {
            let f = field(expr)?;
            let p = lit(pattern)?;
            like(&f, &p, *escape, *negated).ok()
        }
        Ex::And(a, b) => Some(Cond::combine(true, vec![to_cond(a)?, to_cond(b)?])),
        _ => None,
    }
}

fn contains_agg(ex: &Ex) -> bool {
    match ex {
        Ex::Agg(_) => true,
        Ex::Lit(_) | Ex::Col(_) | Ex::Exists(..) | Ex::Scalar(_) => false,
        Ex::InSub(a, _, _) | Ex::Quantified(a, _, _, _) | Ex::Neg(a) | Ex::Not(a) | Ex::IsNull(a, _) | Ex::IsBool(a, _, _) | Ex::Cast(a, _, _) | Ex::Extract(_, a) => {
            contains_agg(a)
        }
        Ex::Arith(a, _, b) | Ex::Cmp(a, _, b) | Ex::And(a, b) | Ex::Or(a, b) | Ex::Concat(a, b) | Ex::DistinctFrom(a, b, _) => {
            contains_agg(a) || contains_agg(b)
        }
        Ex::InList(a, list, _) => contains_agg(a) || list.iter().any(contains_agg),
        Ex::Like { expr, pattern, .. } => contains_agg(expr) || contains_agg(pattern),
        Ex::Case { operand, whens, otherwise } => {
            operand.as_ref().is_some_and(|o| contains_agg(o))
                || whens.iter().any(|(w, t)| contains_agg(w) || contains_agg(t))
                || otherwise.as_ref().is_some_and(|o| contains_agg(o))
        }
        Ex::Func(_, args) => args.iter().any(contains_agg),
    }
}

/// In a grouped SELECT, an expression may read only group keys and aggregates.
fn check_grouped(ex: &Ex, keys: &[Ex]) -> std::result::Result<(), ()> {
    if keys.contains(ex) {
        return Ok(());
    }
    match ex {
        Ex::Lit(_) | Ex::Agg(_) | Ex::Exists(..) | Ex::Scalar(_) => Ok(()),
        // Outer columns are constant within a group.
        Ex::Col(c) if c.depth > 0 => Ok(()),
        Ex::Col(_) => Err(()),
        Ex::InSub(a, _, _) | Ex::Quantified(a, _, _, _) | Ex::Neg(a) | Ex::Not(a) | Ex::IsNull(a, _) | Ex::IsBool(a, _, _) | Ex::Cast(a, _, _) | Ex::Extract(_, a) => {
            check_grouped(a, keys)
        }
        Ex::Arith(a, _, b) | Ex::Cmp(a, _, b) | Ex::And(a, b) | Ex::Or(a, b) | Ex::Concat(a, b) | Ex::DistinctFrom(a, b, _) => {
            check_grouped(a, keys)?;
            check_grouped(b, keys)
        }
        Ex::InList(a, list, _) => {
            check_grouped(a, keys)?;
            list.iter().try_for_each(|e| check_grouped(e, keys))
        }
        Ex::Like { expr, pattern, .. } => {
            check_grouped(expr, keys)?;
            check_grouped(pattern, keys)
        }
        Ex::Case { operand, whens, otherwise } => {
            if let Some(o) = operand {
                check_grouped(o, keys)?;
            }
            for (w, t) in whens {
                check_grouped(w, keys)?;
                check_grouped(t, keys)?;
            }
            match otherwise {
                Some(o) => check_grouped(o, keys),
                None => Ok(()),
            }
        }
        Ex::Func(_, args) => args.iter().try_for_each(|e| check_grouped(e, keys)),
    }
}

/// Output column names of a set expression, when they're known before running it.
fn output_names(node: &SetNode) -> Option<Vec<String>> {
    match node {
        SetNode::Select(s) => {
            let mut names = Vec::new();
            for item in &s.items {
                match item {
                    OutItem::Expr(_, name) => names.push(name.clone()),
                    OutItem::Wildcard(_) => return None,
                }
            }
            Some(names)
        }
        SetNode::Set { left, .. } => output_names(left),
        SetNode::Query(q) => output_names(&q.body),
    }
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// A loaded table: its documents and the column order `*` gives.
struct Loaded {
    rows: Vec<Arc<Value>>,
    columns: Vec<String>,
}

/// A query's result: column names and rows.
#[derive(Clone)]
pub(super) struct Output {
    pub names: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

type Row = Vec<Arc<Value>>;

struct Ctx<'g> {
    plan: &'g General,
    scans: Vec<Loaded>,
    sub_cache: RefCell<HashMap<usize, Arc<Output>>>,
    cte_cache: RefCell<HashMap<usize, Arc<Output>>>,
    work: Cell<u64>,
    work_limit: u64,
    now: chrono::DateTime<chrono::Utc>,
    null: Arc<Value>,
}

/// What an expression is evaluated against: a row of this scope, the
/// aggregates of its group (in a grouped SELECT), and the enclosing scope.
struct Env<'a> {
    row: &'a [Arc<Value>],
    aggs: Option<&'a [Value]>,
    parent: Option<&'a Env<'a>>,
}

impl HexDBEngine {
    /// Run a statement compiled for the general executor: every row of its result.
    pub(super) async fn run_general(&self, plan: &General) -> Result<Output> {
        let tokenized: HashMap<String, Vec<String>> = plan
            .tables
            .iter()
            .map(|t| (t.clone(), self.tokenized_fields(t).into_iter().map(|(path, _)| path).collect::<Vec<_>>()))
            .filter(|(_, paths)| !paths.is_empty())
            .collect();
        let rewritten;
        let plan = if tokenized.is_empty() {
            plan
        } else {
            let mut copy = plan.clone();
            Tokens { engine: self, fields: tokenized, seen: RefCell::new(HashSet::new()) }.plan(&mut copy);
            rewritten = copy;
            &rewritten
        };
        let max_rows = self.config.limits.sql_max_rows.max(1);
        let mut scans = Vec::with_capacity(plan.scans.len());
        let mut loaded: HashMap<(String, String), usize> = HashMap::new();
        for spec in &plan.scans {
            let key = (spec.table.clone(), spec.filter.to_string());
            if let Some(&i) = loaded.get(&key) {
                let same: &Loaded = &scans[i];
                scans.push(Loaded { rows: same.rows.clone(), columns: same.columns.clone() });
                continue;
            }
            if self.is_system_tessellation(&spec.table) {
                return Err(invalid(format!("'{}' is a system tessellation.", spec.table)));
            }
            if !self.tessellation_exists(&spec.table) {
                return Err(crate::engine::EngineError::NotFound(format!("Tessellation '{}' not found.", spec.table)).into());
            }
            let request = DocumentQuery {
                filter: Filter::parse(&spec.filter)?,
                sort: Vec::new(),
                offset: 0,
                limit: max_rows + 1,
                after: None,
                with_total: false,
            };
            let page = self.query_documents(&spec.table, &request).await?;
            if page.documents.len() > max_rows {
                return Err(invalid(format!(
                    "reading '{}' would return more than {} documents (limits.sql_max_rows). Narrow it with WHERE.",
                    spec.table, max_rows
                )));
            }
            let rows: Vec<Arc<Value>> = page.documents.iter().map(|d| Arc::new(d.to_api_json())).collect();
            let mut columns = vec!["id".to_string()];
            for row in &rows {
                if let Value::Object(map) = row.as_ref() {
                    for key in map.keys() {
                        if !key.starts_with('_') && !columns.contains(key) {
                            columns.push(key.clone());
                        }
                    }
                }
            }
            if rows.is_empty() {
                columns = self.sql_columns(&spec.table).await?.into_iter().map(|c| c.name).collect();
            }
            loaded.insert(key, scans.len());
            scans.push(Loaded { rows, columns });
        }
        let ctx = Ctx {
            plan,
            scans,
            sub_cache: RefCell::new(HashMap::new()),
            cte_cache: RefCell::new(HashMap::new()),
            work: Cell::new(0),
            work_limit: (max_rows as u64).saturating_mul(WORK_PER_ROW),
            now: chrono::Utc::now(),
            null: Arc::new(Value::Null),
        };
        ctx.query(&plan.root, None)
    }
}

/// Rewrites a plan so values compared for equality with a tokenized field
/// (`=`, `<>`, `IN`, `IS [NOT] DISTINCT FROM`) are compared as tokens, which
/// is what the rows hold. Pushed-down reads get the same rewrite from the
/// engine (`prepare_filter`); this covers what's checked in memory.
struct Tokens<'e> {
    engine: &'e HexDBEngine,
    /// Table -> its tokenized field paths.
    fields: HashMap<String, Vec<String>>,
    /// Subqueries already rewritten.
    seen: RefCell<HashSet<usize>>,
}

/// Per binding of one scope, the table it scans (None: a CTE or derived table).
type Scope = Vec<Option<String>>;

impl Tokens<'_> {
    fn plan(&self, plan: &mut General) {
        let tables: Vec<String> = plan.scans.iter().map(|s| s.table.clone()).collect();
        let mut subs: Vec<Option<SubPlan>> = std::mem::take(&mut plan.subqueries).into_iter().map(Some).collect();
        self.query(&mut plan.root, &tables, &mut Vec::new(), &mut subs);
        for cte in &mut plan.ctes {
            self.query(cte, &tables, &mut Vec::new(), &mut subs);
        }
        // Subqueries are reached from where they're used; any left see no outer scope.
        for i in 0..subs.len() {
            self.sub(i, &tables, &mut Vec::new(), &mut subs);
        }
        plan.subqueries = subs.into_iter().map(|s| s.expect("every subquery is put back")).collect();
    }

    fn sub(&self, index: usize, tables: &[String], scopes: &mut Vec<Scope>, subs: &mut [Option<SubPlan>]) {
        if !self.seen.borrow_mut().insert(index) {
            return;
        }
        if let Some(mut sub) = subs[index].take() {
            self.query(&mut sub.query, tables, scopes, subs);
            subs[index] = Some(sub);
        }
    }

    fn query(&self, node: &mut QueryNode, tables: &[String], scopes: &mut Vec<Scope>, subs: &mut [Option<SubPlan>]) {
        self.set(&mut node.body, tables, scopes, subs);
    }

    fn set(&self, node: &mut SetNode, tables: &[String], scopes: &mut Vec<Scope>, subs: &mut [Option<SubPlan>]) {
        match node {
            SetNode::Select(select) => self.select(select, tables, scopes, subs),
            SetNode::Set { left, right, .. } => {
                self.set(left, tables, scopes, subs);
                self.set(right, tables, scopes, subs);
            }
            SetNode::Query(query) => self.query(query, tables, scopes, subs),
        }
    }

    fn select(&self, s: &mut SelectNode, tables: &[String], scopes: &mut Vec<Scope>, subs: &mut [Option<SubPlan>]) {
        let mut scope: Scope = vec![None; s.bindings.len()];
        if let Some(from) = &mut s.from {
            self.scanned_tables(from, tables, &mut scope, subs);
        }
        scopes.push(scope);
        if let Some(from) = &mut s.from {
            self.join_conditions(from, tables, scopes, subs);
        }
        let exprs = s
            .filter
            .iter_mut()
            .chain(s.having.iter_mut())
            .chain(s.group_keys.iter_mut())
            .chain(s.aggregates.iter_mut().filter_map(|a| a.arg.as_mut()))
            .chain(s.items.iter_mut().filter_map(|i| match i {
                OutItem::Expr(e, _) => Some(e),
                OutItem::Wildcard(_) => None,
            }))
            .chain(s.order.iter_mut().map(|(e, _, _)| e));
        for e in exprs {
            self.ex(e, tables, scopes, subs);
        }
        scopes.pop();
    }

    /// Note the table each binding scans; derived tables see no outer scope.
    fn scanned_tables(&self, node: &mut FromNode, tables: &[String], scope: &mut Scope, subs: &mut [Option<SubPlan>]) {
        match node {
            FromNode::Source { binding, source } => match source {
                Source::Scan(i) => scope[*binding] = tables.get(*i).cloned(),
                Source::Derived(query, _) => self.query(query, tables, &mut Vec::new(), subs),
                Source::Cte(..) => {}
            },
            FromNode::Join { left, right, .. } => {
                self.scanned_tables(left, tables, scope, subs);
                self.scanned_tables(right, tables, scope, subs);
            }
        }
    }

    fn join_conditions(&self, node: &mut FromNode, tables: &[String], scopes: &mut Vec<Scope>, subs: &mut [Option<SubPlan>]) {
        if let FromNode::Join { left, right, on, .. } = node {
            self.join_conditions(left, tables, scopes, subs);
            self.join_conditions(right, tables, scopes, subs);
            if let Some(on) = on {
                self.ex(on, tables, scopes, subs);
            }
        }
    }

    fn ex(&self, e: &mut Ex, tables: &[String], scopes: &mut Vec<Scope>, subs: &mut [Option<SubPlan>]) {
        match e {
            Ex::Cmp(a, BinaryOperator::Eq | BinaryOperator::NotEq, b) | Ex::DistinctFrom(a, b, _) => {
                if self.tokenized(a, scopes) {
                    self.literal(b);
                } else if self.tokenized(b, scopes) {
                    self.literal(a);
                }
            }
            Ex::InList(a, list, _) if self.tokenized(a, scopes) => list.iter_mut().for_each(|v| self.literal(v)),
            _ => {}
        }
        match e {
            Ex::Lit(_) | Ex::Col(_) | Ex::Agg(_) => {}
            Ex::Neg(a) | Ex::Not(a) | Ex::IsNull(a, _) | Ex::IsBool(a, _, _) | Ex::Cast(a, _, _) | Ex::Extract(_, a) => self.ex(a, tables, scopes, subs),
            Ex::Arith(a, _, b) | Ex::Cmp(a, _, b) | Ex::And(a, b) | Ex::Or(a, b) | Ex::Concat(a, b) | Ex::DistinctFrom(a, b, _) => {
                self.ex(a, tables, scopes, subs);
                self.ex(b, tables, scopes, subs);
            }
            Ex::InList(a, list, _) => {
                self.ex(a, tables, scopes, subs);
                list.iter_mut().for_each(|v| self.ex(v, tables, scopes, subs));
            }
            Ex::InSub(a, i, _) | Ex::Quantified(a, _, i, _) => {
                self.ex(a, tables, scopes, subs);
                self.sub(*i, tables, scopes, subs);
            }
            Ex::Exists(i, _) | Ex::Scalar(i) => self.sub(*i, tables, scopes, subs),
            Ex::Like { expr, pattern, .. } => {
                self.ex(expr, tables, scopes, subs);
                self.ex(pattern, tables, scopes, subs);
            }
            Ex::Case { operand, whens, otherwise } => {
                for part in operand.iter_mut().chain(otherwise.iter_mut()) {
                    self.ex(part, tables, scopes, subs);
                }
                for (when, then) in whens {
                    self.ex(when, tables, scopes, subs);
                    self.ex(then, tables, scopes, subs);
                }
            }
            Ex::Func(_, args) => args.iter_mut().for_each(|a| self.ex(a, tables, scopes, subs)),
        }
    }

    /// Whether an expression is a column of a tokenized field.
    fn tokenized(&self, e: &Ex, scopes: &[Scope]) -> bool {
        let Ex::Col(c) = e else { return false };
        let Some(scope) = scopes.len().checked_sub(c.depth + 1).and_then(|i| scopes.get(i)) else { return false };
        let path = c.path.join(".");
        let tokenizes = |table: &Option<String>| table.as_ref().and_then(|t| self.fields.get(t)).is_some_and(|paths| paths.contains(&path));
        match c.binding {
            Some(b) => scope.get(b).is_some_and(tokenizes),
            // Unqualified: the one table in scope, or the one that tokenizes it.
            None if scope.len() == 1 => tokenizes(&scope[0]),
            None => scope.iter().filter(|t| tokenizes(t)).count() == 1,
        }
    }

    fn literal(&self, e: &mut Ex) {
        if let Ex::Lit(value) = e {
            if let Some(token) = self.engine.token_for_filter(value) {
                *value = token;
            }
        }
    }
}

impl<'g> Ctx<'g> {
    fn tick(&self, n: usize) -> Result<()> {
        let total = self.work.get().saturating_add(n as u64);
        self.work.set(total);
        if total > self.work_limit {
            return Err(invalid(format!(
                "the statement compares too many rows (over {} units of work; limits.sql_max_rows sets the scale). Narrow it with WHERE or join on equal fields.",
                self.work_limit
            )));
        }
        Ok(())
    }

    fn query(&self, node: &QueryNode, outer: Option<&Env>) -> Result<Output> {
        let mut out = match &node.body {
            SetNode::Select(select) => self.select(select, outer)?,
            other => self.set(other, outer)?,
        };
        if !node.order.is_empty() {
            let keys = &node.order;
            out.rows.sort_by(|a, b| {
                for (column, descending, nulls_first) in keys {
                    let ord = order_values(&a[*column], &b[*column], *descending, *nulls_first);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                Ordering::Equal
            });
        }
        if node.offset > 0 || node.limit.is_some() {
            let rows = std::mem::take(&mut out.rows);
            out.rows = rows.into_iter().skip(node.offset).take(node.limit.unwrap_or(usize::MAX)).collect();
        }
        Ok(out)
    }

    fn set(&self, node: &SetNode, outer: Option<&Env>) -> Result<Output> {
        match node {
            SetNode::Select(select) => self.select(select, outer),
            SetNode::Query(q) => self.query(q, outer),
            SetNode::Set { op, all, left, right } => {
                let left = self.set(left, outer)?;
                let right = self.set(right, outer)?;
                if left.names.len() != right.names.len() {
                    return Err(invalid(format!(
                        "both sides of {} need the same number of columns ({} and {}).",
                        op,
                        left.names.len(),
                        right.names.len()
                    )));
                }
                self.tick(left.rows.len() + right.rows.len())?;
                let key = |row: &Vec<Value>| row.iter().map(canonical).collect::<Vec<_>>().join("\u{1}");
                let rows = match (op, all) {
                    (SetOperator::Union, true) => left.rows.into_iter().chain(right.rows).collect(),
                    (SetOperator::Union, false) => dedupe(left.rows.into_iter().chain(right.rows).collect()),
                    (SetOperator::Intersect, _) => {
                        let mut counts: HashMap<String, usize> = HashMap::new();
                        for row in &right.rows {
                            *counts.entry(key(row)).or_default() += 1;
                        }
                        let mut rows = Vec::new();
                        for row in left.rows {
                            if let Some(n) = counts.get_mut(&key(&row)) {
                                if *n > 0 {
                                    if *all {
                                        *n -= 1;
                                    }
                                    rows.push(row);
                                }
                            }
                        }
                        if *all {
                            rows
                        } else {
                            dedupe(rows)
                        }
                    }
                    _ => {
                        let mut counts: HashMap<String, usize> = HashMap::new();
                        for row in &right.rows {
                            *counts.entry(key(row)).or_default() += 1;
                        }
                        let mut rows = Vec::new();
                        for row in left.rows {
                            match counts.get_mut(&key(&row)) {
                                Some(n) if *n > 0 => {
                                    if *all {
                                        *n -= 1;
                                    }
                                }
                                _ => rows.push(row),
                            }
                        }
                        if *all {
                            rows
                        } else {
                            dedupe(rows)
                        }
                    }
                };
                Ok(Output { names: left.names, rows })
            }
        }
    }

    // -- FROM ----------------------------------------------------------------

    /// The rows of a FROM tree, each as wide as the scope's bindings.
    fn from(&self, node: &FromNode, width: usize, outer: Option<&Env>) -> Result<(Vec<Row>, Vec<Vec<String>>)> {
        match node {
            FromNode::Source { binding, source } => {
                let (values, columns): (Vec<Arc<Value>>, Vec<String>) = match source {
                    Source::Scan(i) => {
                        let loaded = &self.scans[*i];
                        (loaded.rows.clone(), loaded.columns.clone())
                    }
                    Source::Cte(i, rename) => {
                        let out = self.cte(*i)?;
                        objects(&out, rename)?
                    }
                    Source::Derived(query, rename) => {
                        let out = self.query(query, None)?;
                        objects(&out, rename)?
                    }
                };
                self.tick(values.len())?;
                let mut column_sets = vec![Vec::new(); width];
                column_sets[*binding] = columns;
                let rows = values
                    .into_iter()
                    .map(|v| {
                        let mut row = vec![self.null.clone(); width];
                        row[*binding] = v;
                        row
                    })
                    .collect();
                Ok((rows, column_sets))
            }
            FromNode::Join { kind, left, right, on, left_bindings, right_bindings } => {
                let (left_rows, mut columns) = self.from(left, width, outer)?;
                let (right_rows, right_columns) = self.from(right, width, outer)?;
                for (i, c) in right_columns.into_iter().enumerate() {
                    if !c.is_empty() {
                        columns[i] = c;
                    }
                }
                let rows = self.join(*kind, left_rows, right_rows, on.as_ref(), left_bindings, right_bindings, outer)?;
                Ok((rows, columns))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn join(
        &self,
        kind: JoinKind,
        left: Vec<Row>,
        right: Vec<Row>,
        on: Option<&Ex>,
        left_bindings: &[usize],
        right_bindings: &[usize],
        outer: Option<&Env>,
    ) -> Result<Vec<Row>> {
        let merge = |l: &Row, r: &Row| -> Row {
            let mut row = l.clone();
            for &b in right_bindings {
                row[b] = r[b].clone();
            }
            row
        };
        // Equality conditions between the two sides become a hash join.
        let (mut keys, mut residual): (Vec<(Ex, Ex)>, Vec<Ex>) = (Vec::new(), Vec::new());
        if let Some(on) = on {
            let lset: HashSet<usize> = left_bindings.iter().copied().collect();
            let rset: HashSet<usize> = right_bindings.iter().copied().collect();
            for c in conjuncts(on.clone()) {
                if let Ex::Cmp(a, BinaryOperator::Eq, b) = &c {
                    let (ra, rb) = (bindings_read(a), bindings_read(b));
                    if let (Some(ra), Some(rb)) = (ra, rb) {
                        if !ra.is_empty() && !rb.is_empty() {
                            if ra.is_subset(&lset) && rb.is_subset(&rset) {
                                keys.push(((**a).clone(), (**b).clone()));
                                continue;
                            }
                            if ra.is_subset(&rset) && rb.is_subset(&lset) {
                                keys.push(((**b).clone(), (**a).clone()));
                                continue;
                            }
                        }
                    }
                }
                residual.push(c);
            }
        }
        let residual = and_all(residual);
        let matches = |l: &Row, r: &Row| -> Result<Option<Row>> {
            let row = merge(l, r);
            match &residual {
                None => Ok(Some(row)),
                Some(cond) => {
                    let env = Env { row: &row, aggs: None, parent: outer };
                    Ok(if truth(&self.eval(cond, &env)?)? == Some(true) { Some(row) } else { None })
                }
            }
        };

        let mut out = Vec::new();
        let mut right_matched = vec![false; right.len()];
        if keys.is_empty() {
            self.tick(left.len().saturating_mul(right.len().max(1)))?;
            for l in &left {
                let mut any = false;
                for (j, r) in right.iter().enumerate() {
                    if let Some(row) = matches(l, r)? {
                        any = true;
                        right_matched[j] = true;
                        out.push(row);
                    }
                }
                if !any && matches!(kind, JoinKind::Left | JoinKind::Full) {
                    out.push(l.clone());
                }
            }
        } else {
            let key_of = |row: &Row, exprs: &mut dyn Iterator<Item = &Ex>| -> Result<Option<String>> {
                let env = Env { row, aggs: None, parent: outer };
                let mut parts = Vec::new();
                for e in exprs {
                    let v = self.eval(e, &env)?;
                    if v.is_null() {
                        return Ok(None);
                    }
                    parts.push(canonical(&v));
                }
                Ok(Some(parts.join("\u{1}")))
            };
            let mut table: HashMap<String, Vec<usize>> = HashMap::new();
            for (j, r) in right.iter().enumerate() {
                if let Some(k) = key_of(r, &mut keys.iter().map(|(_, b)| b))? {
                    table.entry(k).or_default().push(j);
                }
            }
            self.tick(left.len() + right.len())?;
            for l in &left {
                let mut any = false;
                if let Some(k) = key_of(l, &mut keys.iter().map(|(a, _)| a))? {
                    if let Some(candidates) = table.get(&k) {
                        self.tick(candidates.len())?;
                        for &j in candidates {
                            if let Some(row) = matches(l, &right[j])? {
                                any = true;
                                right_matched[j] = true;
                                out.push(row);
                            }
                        }
                    }
                }
                if !any && matches!(kind, JoinKind::Left | JoinKind::Full) {
                    out.push(l.clone());
                }
            }
        }
        if matches!(kind, JoinKind::Right | JoinKind::Full) {
            for (j, r) in right.iter().enumerate() {
                if !right_matched[j] {
                    let mut row = r.clone();
                    for &b in left_bindings {
                        row[b] = self.null.clone();
                    }
                    out.push(row);
                }
            }
        }
        self.tick(out.len())?;
        Ok(out)
    }

    fn cte(&self, index: usize) -> Result<Arc<Output>> {
        if let Some(out) = self.cte_cache.borrow().get(&index) {
            return Ok(out.clone());
        }
        let out = Arc::new(self.query(&self.plan.ctes[index], None)?);
        self.cte_cache.borrow_mut().insert(index, out.clone());
        Ok(out)
    }

    // -- SELECT --------------------------------------------------------------

    fn select(&self, s: &SelectNode, outer: Option<&Env>) -> Result<Output> {
        let width = s.bindings.len();
        let (rows, columns) = match &s.from {
            Some(from) => self.from(from, width, outer)?,
            None => (vec![Vec::new()], Vec::new()),
        };

        // WHERE
        let rows: Vec<Row> = match &s.filter {
            None => rows,
            Some(cond) => {
                let mut kept = Vec::with_capacity(rows.len());
                for row in rows {
                    let env = Env { row: &row, aggs: None, parent: outer };
                    if truth(&self.eval(cond, &env)?)? == Some(true) {
                        kept.push(row);
                    }
                }
                kept
            }
        };
        self.tick(rows.len())?;

        // Output names (`*` expands to each table's columns).
        let mut names = Vec::new();
        for item in &s.items {
            match item {
                OutItem::Expr(_, name) => names.push(name.clone()),
                OutItem::Wildcard(None) => {
                    for c in &columns {
                        names.extend(c.iter().cloned());
                    }
                }
                OutItem::Wildcard(Some(b)) => names.extend(columns.get(*b).cloned().unwrap_or_default()),
            }
        }

        // Each output row with its ORDER BY keys.
        let mut produced: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
        let project = |env: &Env| -> Result<(Vec<Value>, Vec<Value>)> {
            let mut values = Vec::with_capacity(names.len());
            for item in &s.items {
                match item {
                    OutItem::Expr(e, _) => values.push(self.eval(e, env)?),
                    OutItem::Wildcard(which) => {
                        for (b, cols) in columns.iter().enumerate() {
                            if which.is_some_and(|w| w != b) {
                                continue;
                            }
                            for c in cols {
                                values.push(env.row.get(b).and_then(|v| v.get(c)).cloned().unwrap_or(Value::Null));
                            }
                        }
                    }
                }
            }
            let mut keys = Vec::with_capacity(s.order.len());
            for (e, _, _) in &s.order {
                keys.push(self.eval(e, env)?);
            }
            Ok((values, keys))
        };

        if s.grouped {
            let mut groups: Vec<(Row, Vec<Accumulator>)> = Vec::new();
            let mut index: HashMap<String, usize> = HashMap::new();
            for row in &rows {
                let env = Env { row, aggs: None, parent: outer };
                let mut key = String::new();
                for k in &s.group_keys {
                    key.push_str(&canonical(&self.eval(k, &env)?));
                    key.push('\u{1}');
                }
                let g = match index.get(&key) {
                    Some(&g) => g,
                    None => {
                        groups.push((row.clone(), s.aggregates.iter().map(Accumulator::new).collect()));
                        index.insert(key, groups.len() - 1);
                        groups.len() - 1
                    }
                };
                for (spec, acc) in s.aggregates.iter().zip(groups[g].1.iter_mut()) {
                    let value = match &spec.arg {
                        None => Value::Bool(true),
                        Some(e) => self.eval(e, &env)?,
                    };
                    acc.add(value)?;
                }
            }
            // No GROUP BY: one group, even over no rows.
            if groups.is_empty() && s.group_keys.is_empty() {
                groups.push((vec![self.null.clone(); width], s.aggregates.iter().map(Accumulator::new).collect()));
            }
            self.tick(groups.len())?;
            for (row, accs) in groups {
                let aggs: Vec<Value> = accs.into_iter().map(Accumulator::finish).collect();
                let env = Env { row: &row, aggs: Some(&aggs), parent: outer };
                if let Some(h) = &s.having {
                    if truth(&self.eval(h, &env)?)? != Some(true) {
                        continue;
                    }
                }
                produced.push(project(&env)?);
            }
        } else {
            for row in &rows {
                let env = Env { row, aggs: None, parent: outer };
                produced.push(project(&env)?);
            }
        }

        if s.distinct {
            let mut seen = HashSet::new();
            produced.retain(|(values, _)| seen.insert(values.iter().map(canonical).collect::<Vec<_>>().join("\u{1}")));
        }
        if !s.order.is_empty() {
            produced.sort_by(|(_, a), (_, b)| {
                for (i, (_, descending, nulls_first)) in s.order.iter().enumerate() {
                    let ord = order_values(&a[i], &b[i], *descending, *nulls_first);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                Ordering::Equal
            });
        }
        Ok(Output { names, rows: produced.into_iter().map(|(v, _)| v).collect() })
    }

    // -- Subqueries ----------------------------------------------------------

    fn subquery(&self, index: usize, env: &Env) -> Result<Arc<Output>> {
        let sub = &self.plan.subqueries[index];
        if !sub.correlated {
            if let Some(out) = self.sub_cache.borrow().get(&index) {
                return Ok(out.clone());
            }
            let out = Arc::new(self.query(&sub.query, Some(env))?);
            self.sub_cache.borrow_mut().insert(index, out.clone());
            return Ok(out);
        }
        Ok(Arc::new(self.query(&sub.query, Some(env))?))
    }

    // -- Expressions ---------------------------------------------------------

    fn column(&self, c: &ColRef, env: &Env) -> Result<Value> {
        let mut scope = env;
        for _ in 0..c.depth {
            scope = scope.parent.ok_or_else(|| invalid("a column refers to a query that isn't there."))?;
        }
        let base: &Value = match c.binding {
            Some(b) => scope.row.get(b).map(|v| v.as_ref()).unwrap_or(&Value::Null),
            None => {
                // The one table whose document has the field.
                let first = &c.path[0];
                let mut found: Option<&Value> = None;
                for v in scope.row {
                    if v.get(first).is_some() {
                        if found.is_some() {
                            return Err(invalid(format!("'{}' is ambiguous: more than one table has it. Qualify it with a table name or alias.", first)));
                        }
                        found = Some(v.as_ref());
                    }
                }
                match found {
                    Some(v) => v,
                    None => return Ok(Value::Null),
                }
            }
        };
        Ok(super::lookup(base, &c.path))
    }

    fn eval(&self, ex: &Ex, env: &Env) -> Result<Value> {
        Ok(match ex {
            Ex::Lit(v) => v.clone(),
            Ex::Col(c) => self.column(c, env)?,
            Ex::Agg(i) => env.aggs.and_then(|a| a.get(*i)).cloned().unwrap_or(Value::Null),
            Ex::Neg(a) => match self.eval(a, env)? {
                Value::Null => Value::Null,
                Value::Number(n) => Value::Number(negate(&n)?),
                other => return Err(invalid(format!("can't negate {}.", other))),
            },
            Ex::Not(a) => match truth(&self.eval(a, env)?)? {
                None => Value::Null,
                Some(b) => Value::Bool(!b),
            },
            Ex::And(a, b) => {
                let l = truth(&self.eval(a, env)?)?;
                if l == Some(false) {
                    return Ok(Value::Bool(false));
                }
                match (l, truth(&self.eval(b, env)?)?) {
                    (_, Some(false)) => Value::Bool(false),
                    (Some(true), Some(true)) => Value::Bool(true),
                    _ => Value::Null,
                }
            }
            Ex::Or(a, b) => {
                let l = truth(&self.eval(a, env)?)?;
                if l == Some(true) {
                    return Ok(Value::Bool(true));
                }
                match (l, truth(&self.eval(b, env)?)?) {
                    (_, Some(true)) => Value::Bool(true),
                    (Some(false), Some(false)) => Value::Bool(false),
                    _ => Value::Null,
                }
            }
            Ex::Cmp(a, op, b) => {
                let (l, r) = (self.eval(a, env)?, self.eval(b, env)?);
                compare_values(&l, op, &r).map_or(Value::Null, Value::Bool)
            }
            Ex::Arith(a, op, b) => arith(&self.eval(a, env)?, *op, &self.eval(b, env)?)?,
            Ex::Concat(a, b) => match (self.eval(a, env)?, self.eval(b, env)?) {
                (Value::Null, _) | (_, Value::Null) => Value::Null,
                (l, r) => Value::String(format!("{}{}", text(&l), text(&r))),
            },
            Ex::IsNull(a, negated) => Value::Bool(self.eval(a, env)?.is_null() != *negated),
            Ex::IsBool(a, target, negated) => {
                let v = truth(&self.eval(a, env)?)?;
                Value::Bool((v == Some(*target)) != *negated)
            }
            Ex::DistinctFrom(a, b, negated) => {
                let (l, r) = (self.eval(a, env)?, self.eval(b, env)?);
                let same = match (l.is_null(), r.is_null()) {
                    (true, true) => true,
                    (false, false) => compare_values(&l, &BinaryOperator::Eq, &r) == Some(true),
                    _ => false,
                };
                Value::Bool(same == *negated)
            }
            Ex::InList(a, list, negated) => {
                let target = self.eval(a, env)?;
                let mut values = Vec::with_capacity(list.len());
                for e in list {
                    values.push(self.eval(e, env)?);
                }
                in_values(&target, values.iter(), *negated)
            }
            Ex::InSub(a, sub, negated) => {
                let target = self.eval(a, env)?;
                let out = self.subquery(*sub, env)?;
                if out.names.len() != 1 {
                    return Err(invalid("a subquery after IN must return one column."));
                }
                self.tick(out.rows.len())?;
                in_values(&target, out.rows.iter().map(|r| &r[0]), *negated)
            }
            Ex::Exists(sub, negated) => Value::Bool(self.subquery(*sub, env)?.rows.is_empty() == *negated),
            Ex::Quantified(a, op, sub, all) => {
                let target = self.eval(a, env)?;
                let out = self.subquery(*sub, env)?;
                if out.names.len() != 1 {
                    return Err(invalid("a subquery after ANY or ALL must return one column."));
                }
                self.tick(out.rows.len())?;
                let mut unknown = false;
                for row in out.rows.iter() {
                    match compare_values(&target, op, &row[0]) {
                        // ALL fails on one false; ANY succeeds on one true.
                        Some(hit) if hit != *all => return Ok(Value::Bool(hit)),
                        Some(_) => {}
                        None => unknown = true,
                    }
                }
                if unknown {
                    Value::Null
                } else {
                    Value::Bool(*all)
                }
            }
            Ex::Scalar(sub) => {
                let out = self.subquery(*sub, env)?;
                if out.names.len() != 1 {
                    return Err(invalid("a subquery used as a value must return one column."));
                }
                match out.rows.len() {
                    0 => Value::Null,
                    1 => out.rows[0][0].clone(),
                    _ => return Err(invalid("a subquery used as a value returned more than one row.")),
                }
            }
            Ex::Like { expr, pattern, escape, negated, ci } => {
                let (value, pattern) = (self.eval(expr, env)?, self.eval(pattern, env)?);
                match (value, pattern) {
                    (Value::Null, _) | (_, Value::Null) => Value::Null,
                    (v, Value::String(p)) => {
                        let v = text(&v);
                        let matched = if *ci { like_match(&v.to_lowercase(), &p.to_lowercase(), *escape) } else { like_match(&v, &p, *escape) };
                        Value::Bool(matched != *negated)
                    }
                    (_, other) => return Err(invalid(format!("LIKE needs a string pattern, not {}.", other))),
                }
            }
            Ex::Case { operand, whens, otherwise } => {
                let subject = match operand {
                    Some(o) => Some(self.eval(o, env)?),
                    None => None,
                };
                for (when, then) in whens {
                    let hit = match &subject {
                        Some(s) => compare_values(s, &BinaryOperator::Eq, &self.eval(when, env)?) == Some(true),
                        None => truth(&self.eval(when, env)?)? == Some(true),
                    };
                    if hit {
                        return self.eval(then, env);
                    }
                }
                match otherwise {
                    Some(o) => self.eval(o, env)?,
                    None => Value::Null,
                }
            }
            Ex::Cast(a, to, lenient) => {
                let v = self.eval(a, env)?;
                match cast(&v, *to) {
                    Ok(v) => v,
                    Err(_) if *lenient => Value::Null,
                    Err(e) => return Err(e),
                }
            }
            Ex::Extract(part, a) => extract(*part, &self.eval(a, env)?)?,
            Ex::Func(func, args) => {
                if *func == Func::Coalesce {
                    for a in args {
                        let v = self.eval(a, env)?;
                        if !v.is_null() {
                            return Ok(v);
                        }
                    }
                    return Ok(Value::Null);
                }
                let mut values = Vec::with_capacity(args.len());
                for a in args {
                    values.push(self.eval(a, env)?);
                }
                call(*func, &values, self.now)?
            }
        })
    }
}

/// Rows of an output as objects (for a derived table or CTE), with optional renamed columns.
fn objects(out: &Output, rename: &[String]) -> Result<(Vec<Arc<Value>>, Vec<String>)> {
    let names: Vec<String> = if rename.is_empty() {
        out.names.clone()
    } else {
        if rename.len() != out.names.len() {
            return Err(invalid(format!("{} column names given for {} columns.", rename.len(), out.names.len())));
        }
        rename.to_vec()
    };
    let rows = out
        .rows
        .iter()
        .map(|r| {
            let map: Map<String, Value> = names.iter().cloned().zip(r.iter().cloned()).collect();
            Arc::new(Value::Object(map))
        })
        .collect();
    Ok((rows, names))
}

fn dedupe(rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let mut seen = HashSet::new();
    rows.into_iter().filter(|r| seen.insert(r.iter().map(canonical).collect::<Vec<_>>().join("\u{1}"))).collect()
}

/// A key for equality: 1 and 1.0 are the same; types stay apart.
fn canonical(v: &Value) -> String {
    match v {
        Value::Null => "n".into(),
        Value::Bool(b) => format!("b{}", b),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => format!("i{}", f as i64),
            Some(f) => format!("f{}", f),
            None => format!("f{}", n),
        },
        Value::String(s) => format!("s{}", s),
        other => format!("j{}", other),
    }
}

fn order_values(a: &Value, b: &Value, descending: bool, nulls_first: Option<bool>) -> Ordering {
    let (an, bn) = (a.is_null(), b.is_null());
    if an || bn {
        if an && bn {
            return Ordering::Equal;
        }
        // Default: nulls first ascending, last descending (as in plain queries).
        let first = nulls_first.unwrap_or(!descending);
        return if an == first { Ordering::Less } else { Ordering::Greater };
    }
    let ord = sort_order(Some(a), Some(b));
    if descending {
        ord.reverse()
    } else {
        ord
    }
}

/// A condition's value: true, false, or unknown (NULL).
fn truth(v: &Value) -> Result<Option<bool>> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        other => Err(invalid(format!("{} isn't a condition (true, false or NULL).", other))),
    }
}

fn in_values<'v>(target: &Value, values: impl Iterator<Item = &'v Value>, negated: bool) -> Value {
    if target.is_null() {
        return Value::Null;
    }
    let mut unknown = false;
    for v in values {
        match compare_values(target, &BinaryOperator::Eq, v) {
            Some(true) => return Value::Bool(!negated),
            None => unknown = true,
            Some(false) => {}
        }
    }
    if unknown {
        Value::Null
    } else {
        Value::Bool(negated)
    }
}

/// A value as text, for concatenation and string functions.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn number(v: &Value, what: &str) -> Result<Option<f64>> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => Ok(n.as_f64()),
        other => Err(invalid(format!("{} needs a number, not {}.", what, other))),
    }
}

fn float(f: f64) -> Value {
    Number::from_f64(f).map_or(Value::Null, Value::Number)
}

fn arith(l: &Value, op: Arith, r: &Value) -> Result<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    let (Value::Number(a), Value::Number(b)) = (l, r) else {
        return Err(invalid(format!("arithmetic needs numbers, not {} and {}.", l, r)));
    };
    if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) {
        let exact = match op {
            Arith::Add => x.checked_add(y),
            Arith::Sub => x.checked_sub(y),
            Arith::Mul => x.checked_mul(y),
            Arith::Mod if y != 0 => x.checked_rem(y),
            // Division gives an exact result when there's one (7 / 2 = 3.5).
            Arith::Div if y != 0 && x % y == 0 => x.checked_div(y),
            _ => None,
        };
        if let Some(v) = exact {
            return Ok(json!(v));
        }
    }
    let (x, y) = (a.as_f64().unwrap_or(f64::NAN), b.as_f64().unwrap_or(f64::NAN));
    if matches!(op, Arith::Div | Arith::Mod) && y == 0.0 {
        return Err(invalid("division by zero."));
    }
    Ok(float(match op {
        Arith::Add => x + y,
        Arith::Sub => x - y,
        Arith::Mul => x * y,
        Arith::Div => x / y,
        Arith::Mod => x % y,
    }))
}

/// SQL LIKE: `%` any run, `_` one character, `escape` makes the next one literal.
fn like_match(value: &str, pattern: &str, escape: Option<char>) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum P {
        Any,
        One,
        Lit(char),
    }
    let mut parts = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            if let Some(n) = chars.next() {
                parts.push(P::Lit(n));
            }
        } else if c == '%' {
            parts.push(P::Any);
        } else if c == '_' {
            parts.push(P::One);
        } else {
            parts.push(P::Lit(c));
        }
    }
    let text: Vec<char> = value.chars().collect();
    // Two-pointer match with backtracking to the last %.
    let (mut t, mut p) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while t < text.len() {
        match parts.get(p) {
            Some(P::One) => {
                t += 1;
                p += 1;
            }
            Some(P::Lit(c)) if *c == text[t] => {
                t += 1;
                p += 1;
            }
            Some(P::Any) => {
                star = Some(p);
                mark = t;
                p += 1;
            }
            _ => match star {
                Some(s) => {
                    p = s + 1;
                    mark += 1;
                    t = mark;
                }
                None => return false,
            },
        }
    }
    parts[p..].iter().all(|x| *x == P::Any)
}

fn parse_datetime(v: &Value) -> Result<Option<chrono::NaiveDateTime>> {
    let s = match v {
        Value::Null => return Ok(None),
        Value::String(s) => s.trim(),
        other => return Err(invalid(format!("{} isn't a date or time.", other))),
    };
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(Some(dt.with_timezone(&chrono::Utc).naive_utc()));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, format) {
            return Ok(Some(dt));
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0));
    }
    Err(invalid(format!("'{}' isn't a date or time HexDB can read (use ISO 8601, like 2026-10-19 or 2026-10-19T14:30:00Z).", s)))
}

fn extract(part: DatePart, v: &Value) -> Result<Value> {
    use chrono::{Datelike, Timelike};
    let Some(dt) = parse_datetime(v)? else { return Ok(Value::Null) };
    Ok(match part {
        DatePart::Year => json!(dt.year()),
        DatePart::Quarter => json!((dt.month() - 1) / 3 + 1),
        DatePart::Month => json!(dt.month()),
        DatePart::Week => json!(dt.iso_week().week()),
        DatePart::Day => json!(dt.day()),
        DatePart::DayOfWeek => json!(dt.weekday().num_days_from_sunday()),
        DatePart::DayOfYear => json!(dt.ordinal()),
        DatePart::Hour => json!(dt.hour()),
        DatePart::Minute => json!(dt.minute()),
        DatePart::Second => {
            let s = dt.second() as f64 + dt.nanosecond() as f64 / 1e9;
            if s.fract() == 0.0 {
                json!(dt.second())
            } else {
                float(s)
            }
        }
        DatePart::Epoch => json!(dt.and_utc().timestamp()),
    })
}

fn cast(v: &Value, to: CastTo) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let fail = || invalid(format!("can't convert {} to {:?}.", v, to).to_lowercase());
    Ok(match to {
        CastTo::Json => v.clone(),
        CastTo::String => Value::String(text(v)),
        CastTo::Integer => match v {
            Value::Number(n) => match n.as_i64() {
                Some(i) => json!(i),
                None => json!(n.as_f64().ok_or_else(fail)?.trunc() as i64),
            },
            Value::String(s) => match s.trim().parse::<i64>() {
                Ok(i) => json!(i),
                Err(_) => json!(s.trim().parse::<f64>().map_err(|_| fail())?.trunc() as i64),
            },
            Value::Bool(b) => json!(*b as i64),
            _ => return Err(fail()),
        },
        CastTo::Number => match v {
            Value::Number(_) => v.clone(),
            Value::String(s) => Value::Number(parse_number(s.trim()).map_err(|_| fail())?),
            Value::Bool(b) => json!(*b as i64),
            _ => return Err(fail()),
        },
        CastTo::Boolean => match v {
            Value::Bool(_) => v.clone(),
            Value::Number(n) => Value::Bool(n.as_f64() != Some(0.0)),
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "yes" | "y" | "1" => Value::Bool(true),
                "false" | "f" | "no" | "n" | "0" => Value::Bool(false),
                _ => return Err(fail()),
            },
            _ => return Err(fail()),
        },
        CastTo::Date => match parse_datetime(v)? {
            Some(dt) => Value::String(dt.format("%Y-%m-%d").to_string()),
            None => Value::Null,
        },
        CastTo::Timestamp => match parse_datetime(v)? {
            Some(dt) => Value::String(dt.format("%Y-%m-%dT%H:%M:%S%.fZ").to_string()),
            None => Value::Null,
        },
    })
}

fn call(func: Func, args: &[Value], now: chrono::DateTime<chrono::Utc>) -> Result<Value> {
    let arg = |i: usize| args.get(i).unwrap_or(&Value::Null);
    let any_null = args.iter().any(Value::is_null);
    let int_arg = |i: usize, what: &str| -> Result<i64> {
        match arg(i) {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).ok_or_else(|| invalid(format!("{} needs an integer.", what))),
            other => Err(invalid(format!("{} needs an integer, not {}.", what, other))),
        }
    };
    Ok(match func {
        Func::Now => Value::String(now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
        Func::CurrentDate => Value::String(now.format("%Y-%m-%d").to_string()),
        Func::CurrentTime => Value::String(now.format("%H:%M:%S").to_string()),
        Func::Pi => float(std::f64::consts::PI),
        Func::Concat => Value::String(args.iter().map(text).collect()),
        Func::Coalesce => args.iter().find(|v| !v.is_null()).cloned().unwrap_or(Value::Null),
        Func::NullIf => {
            if compare_values(arg(0), &BinaryOperator::Eq, arg(1)) == Some(true) {
                Value::Null
            } else {
                arg(0).clone()
            }
        }
        _ if any_null => Value::Null,
        Func::Upper => Value::String(text(arg(0)).to_uppercase()),
        Func::Lower => Value::String(text(arg(0)).to_lowercase()),
        Func::Length => json!(text(arg(0)).chars().count()),
        Func::Trim => Value::String(text(arg(0)).trim().to_string()),
        Func::LTrim => Value::String(text(arg(0)).trim_start().to_string()),
        Func::RTrim => Value::String(text(arg(0)).trim_end().to_string()),
        Func::Replace => Value::String(text(arg(0)).replace(&text(arg(1)), &text(arg(2)))),
        Func::Substring => {
            let s: Vec<char> = text(arg(0)).chars().collect();
            // 1-based start; a start before 1 eats into the length, as in SQL.
            let start = int_arg(1, "SUBSTRING")?;
            let end = if args.len() > 2 {
                let len = int_arg(2, "SUBSTRING")?;
                if len < 0 {
                    return Err(invalid("SUBSTRING's length can't be negative."));
                }
                start.saturating_add(len)
            } else {
                i64::MAX
            };
            let from = (start.max(1) - 1) as usize;
            let to = (end.max(1) - 1).min(s.len() as i64) as usize;
            Value::String(if from < to { s[from..to].iter().collect() } else { String::new() })
        }
        Func::Left | Func::Right => {
            let s: Vec<char> = text(arg(0)).chars().collect();
            let n = int_arg(1, "LEFT / RIGHT")?.max(0) as usize;
            let n = n.min(s.len());
            Value::String(if func == Func::Left { s[..n].iter().collect() } else { s[s.len() - n..].iter().collect() })
        }
        Func::Position => {
            let (haystack, needle) = (text(arg(0)), text(arg(1)));
            json!(haystack.find(&needle).map_or(0, |byte| haystack[..byte].chars().count() + 1))
        }
        Func::Abs => match arg(0) {
            Value::Number(n) if n.is_i64() => json!(n.as_i64().unwrap_or(0).unsigned_abs()),
            other => float(number(other, "ABS")?.unwrap_or(0.0).abs()),
        },
        Func::Sign => float(number(arg(0), "SIGN")?.map_or(0.0, |f| if f > 0.0 { 1.0 } else if f < 0.0 { -1.0 } else { 0.0 })),
        Func::Floor | Func::Ceil => match arg(0) {
            Value::Number(n) if n.is_i64() || n.is_u64() => arg(0).clone(),
            other => {
                let f = number(other, "FLOOR / CEIL")?.unwrap_or(0.0);
                let r = if func == Func::Floor { f.floor() } else { f.ceil() };
                if r.abs() < 9.0e15 {
                    json!(r as i64)
                } else {
                    float(r)
                }
            }
        },
        Func::Round => {
            let f = number(arg(0), "ROUND")?.unwrap_or(0.0);
            let digits = if args.len() > 1 { int_arg(1, "ROUND")? } else { 0 };
            if digits <= 0 && arg(0).as_i64().is_some() && digits == 0 {
                arg(0).clone()
            } else {
                let scale = 10f64.powi(digits.clamp(-15, 15) as i32);
                let r = (f * scale).round() / scale;
                if digits <= 0 && r.abs() < 9.0e15 {
                    json!(r as i64)
                } else {
                    float(r)
                }
            }
        }
        Func::Mod => arith(arg(0), Arith::Mod, arg(1))?,
        Func::Locate => {
            // LOCATE(needle, haystack [, start]): 1-based, 0 if absent.
            let (needle, haystack) = (text(arg(0)), text(arg(1)));
            let chars: Vec<char> = haystack.chars().collect();
            let start = if args.len() > 2 { (int_arg(2, "LOCATE")?.max(1) - 1) as usize } else { 0 };
            if start > chars.len() {
                json!(0)
            } else {
                let rest: String = chars[start..].iter().collect();
                json!(rest.find(&needle).map_or(0, |byte| start + rest[..byte].chars().count() + 1))
            }
        }
        Func::Exp => float(number(arg(0), "EXP")?.unwrap_or(0.0).exp()),
        Func::Ln | Func::Log10 => {
            let f = number(arg(0), "LOG")?.unwrap_or(0.0);
            if f <= 0.0 {
                return Err(invalid("the logarithm of a number that isn't positive."));
            }
            float(if func == Func::Ln { f.ln() } else { f.log10() })
        }
        Func::Truncate => {
            let f = number(arg(0), "TRUNCATE")?.unwrap_or(0.0);
            let digits = if args.len() > 1 { int_arg(1, "TRUNCATE")? } else { 0 };
            let scale = 10f64.powi(digits.clamp(-15, 15) as i32);
            let r = (f * scale).trunc() / scale;
            if digits <= 0 && r.abs() < 9.0e15 {
                json!(r as i64)
            } else {
                float(r)
            }
        }
        Func::DayName | Func::MonthName => match parse_datetime(arg(0))? {
            None => Value::Null,
            Some(dt) => Value::String(dt.format(if func == Func::DayName { "%A" } else { "%B" }).to_string()),
        },
        Func::Power => float(number(arg(0), "POWER")?.unwrap_or(0.0).powf(number(arg(1), "POWER")?.unwrap_or(0.0))),
        Func::Sqrt => {
            let f = number(arg(0), "SQRT")?.unwrap_or(0.0);
            if f < 0.0 {
                return Err(invalid("SQRT of a negative number."));
            }
            float(f.sqrt())
        }
    })
}

// -- Aggregates --------------------------------------------------------------

struct Accumulator {
    func: AggFunc,
    distinct: Option<HashSet<String>>,
    count: u64,
    int_sum: Option<i64>,
    float_sum: f64,
    floats: bool,
    best: Option<Value>,
}

impl Accumulator {
    fn new(spec: &AggSpec) -> Accumulator {
        Accumulator {
            func: spec.func,
            distinct: if spec.distinct { Some(HashSet::new()) } else { None },
            count: 0,
            int_sum: Some(0),
            float_sum: 0.0,
            floats: false,
            best: None,
        }
    }

    fn add(&mut self, value: Value) -> Result<()> {
        if value.is_null() {
            return Ok(());
        }
        if let Some(seen) = &mut self.distinct {
            if !seen.insert(canonical(&value)) {
                return Ok(());
            }
        }
        self.count += 1;
        match self.func {
            AggFunc::Count => {}
            AggFunc::Sum | AggFunc::Avg => {
                let Value::Number(n) = &value else {
                    return Err(invalid(format!("SUM and AVG need numbers, not {}.", value)));
                };
                match (n.as_i64(), self.int_sum) {
                    (Some(i), Some(sum)) if !self.floats => match sum.checked_add(i) {
                        Some(s) => self.int_sum = Some(s),
                        None => {
                            self.floats = true;
                            self.float_sum = sum as f64 + i as f64;
                        }
                    },
                    _ => {
                        if !self.floats {
                            self.floats = true;
                            self.float_sum = self.int_sum.unwrap_or(0) as f64;
                        }
                        self.float_sum += n.as_f64().unwrap_or(0.0);
                    }
                }
            }
            AggFunc::Min | AggFunc::Max => {
                let better = match &self.best {
                    None => true,
                    Some(b) => match compare(&value, b) {
                        Some(Ordering::Less) => self.func == AggFunc::Min,
                        Some(Ordering::Greater) => self.func == AggFunc::Max,
                        _ => false,
                    },
                };
                if better {
                    self.best = Some(value);
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Value {
        match self.func {
            AggFunc::Count => json!(self.count),
            _ if self.count == 0 => Value::Null,
            AggFunc::Sum => {
                if self.floats {
                    float(self.float_sum)
                } else {
                    json!(self.int_sum.unwrap_or(0))
                }
            }
            AggFunc::Avg => {
                let total = if self.floats { self.float_sum } else { self.int_sum.unwrap_or(0) as f64 };
                float(total / self.count as f64)
            }
            AggFunc::Min | AggFunc::Max => self.best.unwrap_or(Value::Null),
        }
    }
}

/// Column types of an output (for the response).
pub(super) fn output_columns(out: &Output) -> Vec<super::SqlColumn> {
    out.names
        .iter()
        .enumerate()
        .map(|(i, name)| super::SqlColumn { name: name.clone(), kind: column_type(out.rows.iter().map(|r| &r[i])) })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns() {
        assert!(like_match("hello", "h%o", None));
        assert!(like_match("hello", "h_llo", None));
        assert!(like_match("hello", "%", None));
        assert!(!like_match("hello", "h_lo", None));
        assert!(like_match("a_b", "a\\_b", Some('\\')));
        assert!(!like_match("axb", "a\\_b", Some('\\')));
        assert!(like_match("abcabc", "%bc%bc", None));
        assert!(!like_match("abc", "abcd%", None));
    }

    #[test]
    fn arithmetic_keeps_integers_exact() {
        assert_eq!(arith(&json!(7), Arith::Div, &json!(2)).unwrap(), json!(3.5));
        assert_eq!(arith(&json!(8), Arith::Div, &json!(2)).unwrap(), json!(4));
        assert_eq!(arith(&json!(2), Arith::Mul, &json!(3)).unwrap(), json!(6));
        assert_eq!(arith(&json!(i64::MAX), Arith::Add, &json!(1)).unwrap(), json!(i64::MAX as f64 + 1.0));
        assert!(arith(&json!(1), Arith::Div, &json!(0)).is_err());
        assert_eq!(arith(&json!(null), Arith::Add, &json!(1)).unwrap(), json!(null));
    }

    #[test]
    fn dates_extract_and_cast() {
        assert_eq!(extract(DatePart::Year, &json!("2026-10-19T14:30:00Z")).unwrap(), json!(2026));
        assert_eq!(extract(DatePart::Quarter, &json!("2026-10-19")).unwrap(), json!(4));
        assert_eq!(extract(DatePart::DayOfWeek, &json!("2026-10-19")).unwrap(), json!(1));
        assert_eq!(cast(&json!("2026-10-19T14:30:00+02:00"), CastTo::Date).unwrap(), json!("2026-10-19"));
        assert_eq!(cast(&json!("42"), CastTo::Integer).unwrap(), json!(42));
        assert_eq!(cast(&json!(" 2.5 "), CastTo::Number).unwrap(), json!(2.5));
        assert!(cast(&json!("x"), CastTo::Integer).is_err());
    }

    #[test]
    fn canonical_keys_join_numbers() {
        assert_eq!(canonical(&json!(1)), canonical(&json!(1.0)));
        assert_ne!(canonical(&json!(1)), canonical(&json!("1")));
    }
}
