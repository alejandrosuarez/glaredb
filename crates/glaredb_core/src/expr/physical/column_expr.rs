use std::fmt;

use glaredb_error::Result;

use super::ExpressionState;
use crate::arrays::array::Array;
use crate::arrays::array::selection::Selection;
use crate::arrays::batch::Batch;
use crate::arrays::cache::NopCache;
use crate::arrays::datatype::DataType;
use crate::buffer::buffer_manager::DefaultBufferManager;

#[derive(Debug, Clone)]
pub struct PhysicalColumnExpr {
    pub idx: usize,
    pub datatype: DataType,
}

impl From<(usize, DataType)> for PhysicalColumnExpr {
    fn from((idx, datatype): (usize, DataType)) -> Self {
        Self::new(idx, datatype)
    }
}

impl PhysicalColumnExpr {
    pub(crate) fn new(idx: usize, datatype: DataType) -> Self {
        PhysicalColumnExpr { idx, datatype }
    }

    pub(crate) fn create_state(&self, _batch_size: usize) -> Result<ExpressionState> {
        Ok(ExpressionState::empty())
    }

    pub fn datatype(&self) -> DataType {
        self.datatype.clone()
    }

    pub(crate) fn eval(
        &self,
        input: &mut Batch,
        _: &mut ExpressionState,
        sel: Selection,
        output: &mut Array,
    ) -> Result<()> {
        let col = &mut input.arrays_mut()[self.idx];
        // Caching will never be useful here, since evaling this expression is
        // always going to try to clone the input array.
        output.clone_from_other(col, &mut NopCache)?;

        if !sel.is_linear() || sel.len() != input.num_rows() {
            output.select(&DefaultBufferManager, sel.iter())?;
        }

        Ok(())
    }
}

impl fmt::Display for PhysicalColumnExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}", self.idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrays::datatype::DataType;
    use crate::testutil::arrays::assert_arrays_eq;
    use crate::util::iter::TryFromExactSizeIterator;

    #[test]
    fn column_expr_eval() {
        let mut input = Batch::from_arrays([
            Array::try_from_iter(["a", "b", "c", "d"]).unwrap(),
            Array::try_from_iter([1, 2, 3, 4]).unwrap(),
        ])
        .unwrap();

        let expr = PhysicalColumnExpr {
            idx: 1,
            datatype: DataType::int32(),
        };
        let mut out = Array::new(&DefaultBufferManager, DataType::int32(), 4).unwrap();
        let sel = Selection::linear(0, 4);

        expr.eval(&mut input, &mut ExpressionState::empty(), sel, &mut out)
            .unwrap();

        let expected = Array::try_from_iter([1, 2, 3, 4]).unwrap();
        assert_arrays_eq(&expected, &out);
    }

    #[test]
    fn column_expr_eval_with_selection() {
        let mut input = Batch::from_arrays([
            Array::try_from_iter(["a", "b", "c", "d"]).unwrap(),
            Array::try_from_iter([1, 2, 3, 4]).unwrap(),
        ])
        .unwrap();

        let expr = PhysicalColumnExpr {
            idx: 1,
            datatype: DataType::int32(),
        };
        let mut state = expr.create_state(4).unwrap();
        let mut out = Array::new(&DefaultBufferManager, DataType::int32(), 4).unwrap();
        let sel = Selection::slice(&[1, 3]);

        expr.eval(&mut input, &mut state, sel, &mut out).unwrap();

        let expected = Array::try_from_iter([2, 4]).unwrap();
        assert_arrays_eq(&expected, &out);
    }
}
