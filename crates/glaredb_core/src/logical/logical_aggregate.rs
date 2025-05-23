use std::collections::BTreeSet;

use glaredb_error::Result;

use super::binder::bind_context::BindContext;
use super::binder::table_list::TableRef;
use super::operator::{LogicalNode, Node};
use crate::explain::explainable::{EntryBuilder, ExplainConfig, ExplainEntry, Explainable};
use crate::expr::Expression;

/// An instance of a GROUPING function.
///
/// A GROUPING function returns a i64 value denoting the null bitmask for that
/// group.
///
/// The rationale for returning an i64 is mostly for its higher compatability
/// with casting to other types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupingFunction {
    /// Indices pointing to expressions in the GROUP BY.
    pub group_exprs: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalAggregate {
    /// Table ref that represents output of aggregate expressions.
    pub aggregates_table: TableRef,
    /// Aggregate expressions.
    pub aggregates: Vec<Expression>,
    /// Table ref representing output of the group by expressions.
    ///
    /// Will be None if this aggregate does not have an associated GROUP BY.
    pub group_table: Option<TableRef>,
    /// Expressions to group on.
    pub group_exprs: Vec<Expression>,
    /// Grouping set referencing group exprs.
    pub grouping_sets: Option<Vec<BTreeSet<usize>>>,
    /// Table reference for getting the GROUPING value for a group.
    ///
    /// This is Some if there's an explicit GROUPING function call in the query.
    /// Internally, the hash aggregate produces a group id based on null
    /// bitmaps, and that id is stored with the group. This let's us
    /// disambiguate NULL values from NULLs in the column vs NULLs produced by
    /// the null bitmap.
    ///
    /// Follows postgres semantics.
    /// See: <https://www.postgresql.org/docs/current/functions-aggregate.html#FUNCTIONS-GROUPING-TABLE>
    pub grouping_functions_table: Option<TableRef>,
    /// Grouping function calls.
    ///
    /// Empty if `grouping_set_table` is None.
    pub grouping_functions: Vec<GroupingFunction>,
}

impl Explainable for LogicalAggregate {
    fn explain_entry(&self, conf: ExplainConfig) -> ExplainEntry {
        EntryBuilder::new("Aggregate", conf)
            .with_contextual_values("aggregates", &self.aggregates)
            .with_value_if_verbose("table_ref", self.aggregates_table)
            .with_value_opt("grouping_set_table_ref", self.grouping_functions_table)
            .with_contextual_values_opt("group_expressions", {
                self.group_table.map(|_| &self.group_exprs)
            })
            .with_value_opt("group_table_ref", self.group_table)
            .build()
    }
}

impl LogicalNode for Node<LogicalAggregate> {
    fn name(&self) -> &'static str {
        "Aggregate"
    }

    fn get_output_table_refs(&self, _bind_context: &BindContext) -> Vec<TableRef> {
        // Order of refs here need to match physical output ordering of the
        // aggregate operators.
        //
        // Grouped aggregates: [GROUPS, AGG_RESULTS, GROUPING_FUNCS]
        let mut refs = Vec::new();
        if let Some(group_table) = self.node.group_table {
            refs.push(group_table);
        }
        refs.push(self.node.aggregates_table);
        if let Some(grouping_set_table) = self.node.grouping_functions_table {
            refs.push(grouping_set_table);
        }
        refs
    }

    fn for_each_expr<'a, F>(&'a self, mut func: F) -> Result<()>
    where
        F: FnMut(&'a Expression) -> Result<()>,
    {
        for expr in &self.node.aggregates {
            func(expr)?;
        }
        for expr in &self.node.group_exprs {
            func(expr)?;
        }
        Ok(())
    }

    fn for_each_expr_mut<'a, F>(&'a mut self, mut func: F) -> Result<()>
    where
        F: FnMut(&'a mut Expression) -> Result<()>,
    {
        for expr in &mut self.node.aggregates {
            func(expr)?;
        }
        for expr in &mut self.node.group_exprs {
            func(expr)?;
        }
        Ok(())
    }
}
