use glaredb_error::Result;

use super::binder::bind_context::BindContext;
use super::binder::table_list::TableRef;
use super::operator::{LogicalNode, Node};
use crate::arrays::datatype::DataType;
use crate::explain::explainable::{EntryBuilder, ExplainConfig, ExplainEntry, Explainable};
use crate::expr::Expression;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalExpressionList {
    /// Table ref for the output of this list.
    pub table_ref: TableRef,
    /// Types for each "column" in the expression list.
    pub types: Vec<DataType>,
    /// "Rows" in the expression list.
    ///
    /// All rows should have the same column type.
    pub rows: Vec<Vec<Expression>>,
}

impl LogicalNode for Node<LogicalExpressionList> {
    fn name(&self) -> &'static str {
        "ExpressionList"
    }

    fn get_output_table_refs(&self, _bind_context: &BindContext) -> Vec<TableRef> {
        vec![self.node.table_ref]
    }

    fn for_each_expr<'a, F>(&'a self, mut func: F) -> Result<()>
    where
        F: FnMut(&'a Expression) -> Result<()>,
    {
        for row in &self.node.rows {
            for expr in row {
                func(expr)?;
            }
        }
        Ok(())
    }

    fn for_each_expr_mut<'a, F>(&'a mut self, mut func: F) -> Result<()>
    where
        F: FnMut(&'a mut Expression) -> Result<()>,
    {
        for row in &mut self.node.rows {
            for expr in row {
                func(expr)?;
            }
        }
        Ok(())
    }
}

impl Explainable for LogicalExpressionList {
    fn explain_entry(&self, conf: ExplainConfig) -> ExplainEntry {
        EntryBuilder::new("ExpressionList", conf)
            .with_value("num_rows", self.rows.len())
            .with_values("datatypes", &self.types)
            .with_value_if_verbose("table_ref", self.table_ref)
            .build()
    }
}
