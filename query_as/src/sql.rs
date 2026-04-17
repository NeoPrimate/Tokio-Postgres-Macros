use sqlparser::ast::{
    JoinConstraint, JoinOperator, Query, Select, SetExpr, Statement, TableFactor,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlparser::ast::Expr;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JoinKind {
    From,
    Inner,
    Left,
}

#[derive(Debug)]
pub struct TableRef {
    pub alias: String,
    pub real_name: String,
    pub join_kind: JoinKind,
}

#[derive(Debug)]
pub struct SqlAnalysis {
    pub table_refs: Vec<TableRef>,
    /// (table_alias, col_name) pairs referenced in JOIN ON conditions.
    /// These columns are join keys and don't need to appear in the SELECT.
    pub join_condition_columns: Vec<(String, String)>,
}

pub fn analyze(sql: &str) -> Result<SqlAnalysis, String> {
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|e| format!("SQL parse error: {e}"))?;

    let stmt = statements
        .first()
        .ok_or_else(|| "Empty SQL".to_string())?;

    match stmt {
        Statement::Query(q) => {
            let select = extract_select(q)?;
            let table_refs = extract_table_refs(&select)?;
            let join_condition_columns = extract_join_condition_columns(&select);
            Ok(SqlAnalysis { table_refs, join_condition_columns })
        }

        // INSERT ... RETURNING — single table, no JOINs.
        Statement::Insert(insert) => {
            let table_name = extract_table_object_name(&insert.table)?;
            Ok(SqlAnalysis {
                table_refs: vec![TableRef {
                    alias: table_name.clone(),
                    real_name: table_name,
                    join_kind: JoinKind::From,
                }],
                join_condition_columns: vec![],
            })
        }

        // UPDATE ... RETURNING — single table, no JOINs.
        Statement::Update(update) => {
            let table_name = extract_table_factor_name(&update.table.relation)?;
            Ok(SqlAnalysis {
                table_refs: vec![TableRef {
                    alias: table_name.clone(),
                    real_name: table_name,
                    join_kind: JoinKind::From,
                }],
                join_condition_columns: vec![],
            })
        }

        _ => Err("Expected SELECT, INSERT ... RETURNING, or UPDATE ... RETURNING".into()),
    }
}

fn extract_select(query: &Query) -> Result<&Select, String> {
    match query.body.as_ref() {
        SetExpr::Select(s) => Ok(s.as_ref()),
        _ => Err("Only simple SELECT queries are supported (no UNION, INTERSECT, etc.)".into()),
    }
}

fn extract_table_refs(select: &Select) -> Result<Vec<TableRef>, String> {
    let mut refs = Vec::new();

    for twj in &select.from {
        add_table_factor(&twj.relation, JoinKind::From, &mut refs)?;

        for join in &twj.joins {
            let kind = match &join.join_operator {
                JoinOperator::Join(_) | JoinOperator::Inner(_) | JoinOperator::CrossJoin(_) => JoinKind::Inner,
                JoinOperator::LeftOuter(_) | JoinOperator::LeftSemi(_)
                | JoinOperator::LeftAnti(_) => JoinKind::Left,
                other => {
                    return Err(format!("Unsupported join type: {other:?}"));
                }
            };
            add_table_factor(&join.relation, kind, &mut refs)?;
        }
    }

    Ok(refs)
}

fn add_table_factor(
    factor: &TableFactor,
    kind: JoinKind,
    refs: &mut Vec<TableRef>,
) -> Result<(), String> {
    match factor {
        TableFactor::Table { name, alias, .. } => {
            let real_name = name
                .0
                .last()
                .and_then(|i| i.as_ident())
                .map(|i| i.value.clone())
                .unwrap_or_default();
            let alias_name = alias
                .as_ref()
                .map(|a| a.name.value.clone())
                .unwrap_or_else(|| real_name.clone());
            refs.push(TableRef {
                alias: alias_name,
                real_name,
                join_kind: kind,
            });
            Ok(())
        }
        _ => Err("Only simple table references are supported (no subqueries in FROM)".into()),
    }
}

/// Collect (table_alias, col_name) for every column referenced in JOIN ON conditions.
fn extract_join_condition_columns(select: &Select) -> Vec<(String, String)> {
    let mut cols = Vec::new();
    for twj in &select.from {
        for join in &twj.joins {
            if let JoinOperator::Join(JoinConstraint::On(expr))
            | JoinOperator::Inner(JoinConstraint::On(expr))
            | JoinOperator::LeftOuter(JoinConstraint::On(expr)) = &join.join_operator
            {
                collect_col_refs(expr, &mut cols);
            }
        }
    }
    cols
}

/// Walk an expression and collect (table_alias, col_name) from qualified column refs.
fn collect_col_refs(expr: &Expr, out: &mut Vec<(String, String)>) {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            out.push((parts[0].value.clone(), parts[1].value.clone()));
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_col_refs(left, out);
            collect_col_refs(right, out);
        }
        _ => {}
    }
}

/// Extract table name from an INSERT's `TableObject`.
fn extract_table_object_name(table: &sqlparser::ast::TableObject) -> Result<String, String> {
    match table {
        sqlparser::ast::TableObject::TableName(name) => {
            name.0.last()
                .and_then(|i| i.as_ident())
                .map(|i| i.value.clone())
                .ok_or_else(|| "Could not extract table name from INSERT".into())
        }
        _ => Err("INSERT with function target not supported".into()),
    }
}

/// Extract table name from a `TableFactor`.
fn extract_table_factor_name(factor: &TableFactor) -> Result<String, String> {
    match factor {
        TableFactor::Table { name, .. } => {
            name.0.last()
                .and_then(|i| i.as_ident())
                .map(|i| i.value.clone())
                .ok_or_else(|| "Could not extract table name from UPDATE".into())
        }
        _ => Err("UPDATE with non-table target not supported".into()),
    }
}
