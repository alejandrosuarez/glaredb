mod hash_table;

use std::fmt;
use std::task::Context;

use glaredb_error::{DbError, Result};
use hash_table::drain::{HashTablePartitionDrainState, needs_drain};
use hash_table::scan::HashTablePartitionScanState;
use hash_table::{HashTableBuildPartitionState, HashTableOperatorState, JoinHashTable};
use parking_lot::Mutex;

use super::util::delayed_count::DelayedPartitionCount;
use super::util::partition_wakers::PartitionWakers;
use super::{
    BaseOperator,
    ExecuteOperator,
    ExecutionProperties,
    PollExecute,
    PollFinalize,
    PollPush,
    PushOperator,
};
use crate::arrays::batch::Batch;
use crate::arrays::datatype::DataType;
use crate::explain::explainable::{EntryBuilder, ExplainConfig, ExplainEntry, Explainable};
use crate::expr::comparison_expr::ComparisonOperator;
use crate::expr::physical::PhysicalScalarExpression;
use crate::logical::logical_join::JoinType;

/// Join condition between left and right batches.
#[derive(Debug, Clone)]
pub struct HashJoinCondition {
    /// Expression for the left side.
    pub left: PhysicalScalarExpression,
    /// Expression for the right side.
    pub right: PhysicalScalarExpression,
    /// The comparison operator.
    pub op: ComparisonOperator,
}

impl fmt::Display for HashJoinCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(left).{} {} (right).{}", self.left, self.op, self.right)
    }
}

#[derive(Debug)]
pub struct HashJoinOperatorState {
    /// Global table we're inserting into.
    table: JoinHashTable,
    /// Operator state for the table.
    table_state: HashTableOperatorState,
    shared: Mutex<SharedState>,
}

#[derive(Debug)]
struct SharedState {
    /// If we're ready to begin inserting hashes.
    hash_inserts_ready: bool,
    /// If we've collected everything from the build side and have the directory
    /// initialized.
    scan_ready: bool,
    /// If all probers have completed probing.
    drain_ready: bool,
    /// Number of partitions still inserting hashes.
    ///
    /// Once zero, we can begin scanning.
    remaining_hash_inserters: DelayedPartitionCount,
    /// Number of partitions still probing.
    remaining_probers: DelayedPartitionCount,
    /// Partition wakers for build-side partitions that have completed inserting
    /// into the table, and are waiting for the directory to be initialized to
    /// begin inserting hashes.
    pending_hash_inserters: PartitionWakers,
    /// Partition wakers for the probe side if the scan isn't ready.
    pending_probers: PartitionWakers,
    /// Partition wakers on the probe side waiting to drain.
    pending_drainers: PartitionWakers,
}

#[derive(Debug)]
pub struct HashJoinPartitionBuildState {
    /// Which phase we're in for finalizing the build.
    finalize_phase: BuildFinalizePhase,
    /// State used for inserting into the hash table.
    build_state: HashTableBuildPartitionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildFinalizePhase {
    Collecting,
    InsertingHashes,
}

#[derive(Debug)]
pub enum HashJoinPartitionExecuteState {
    /// Right side is probing.
    Probing {
        /// If we're actually ready for scanning.
        ///
        /// If false, must check shared state before continuing.
        scan_ready: bool,
        /// Indicator if we should probe for RHS.
        ///
        /// A single probe may produce outputs that are larger than our output
        /// batch, requiring multiple scans. In such cases, multiple polls will be
        /// used for a single probe.
        rhs_needs_probe: bool,
        /// Scan state for the hash table.
        scan_state: Box<HashTablePartitionScanState>,
    },
    /// Right side is draining.
    ///
    /// Only applicable to some join types.
    Draining {
        /// If draining is ready (all probers completed).
        ///
        /// If false, must check shared state before continuing.
        drain_ready: bool,
        /// Drain state for the hash table.
        drain_state: HashTablePartitionDrainState,
    },
}

#[derive(Debug)]
pub struct PhysicalHashJoin {
    /// Join type.
    pub(crate) join_type: JoinType,
    /// Types from the left side of the join.
    pub(crate) left_types: Vec<DataType>,
    /// Types from the right side of the join.
    pub(crate) right_types: Vec<DataType>,
    /// Output types for the join, dependent on the join type.
    pub(crate) output_types: Vec<DataType>,
    /// Join conditions.
    pub(crate) conditions: Vec<HashJoinCondition>,
}

impl PhysicalHashJoin {
    pub fn new(
        join_type: JoinType,
        left_types: impl IntoIterator<Item = DataType>,
        right_types: impl IntoIterator<Item = DataType>,
        conditions: impl IntoIterator<Item = HashJoinCondition>,
    ) -> Result<Self> {
        let left_types: Vec<_> = left_types.into_iter().collect();
        let right_types: Vec<_> = right_types.into_iter().collect();

        let output_types = match join_type {
            JoinType::LeftSemi | JoinType::LeftAnti => left_types.clone(),
            JoinType::Right | JoinType::Full | JoinType::Left | JoinType::Inner => left_types
                .iter()
                .cloned()
                .chain(right_types.iter().cloned())
                .collect(),
            JoinType::LeftMark { .. } => {
                let mut types = left_types.clone();
                types.push(DataType::boolean());
                types
            }
        };

        Ok(PhysicalHashJoin {
            join_type,
            left_types,
            right_types,
            output_types,
            conditions: conditions.into_iter().collect(),
        })
    }
}

impl BaseOperator for PhysicalHashJoin {
    const OPERATOR_NAME: &str = "HashJoin";

    type OperatorState = HashJoinOperatorState;

    fn create_operator_state(&self, props: ExecutionProperties) -> Result<Self::OperatorState> {
        let table = JoinHashTable::try_new(
            self.join_type,
            self.left_types.clone(),
            self.right_types.clone(),
            self.conditions.clone(),
            props.batch_size,
        )?;

        let table_state = table.create_operator_state()?;

        Ok(HashJoinOperatorState {
            table,
            table_state,
            shared: Mutex::new(SharedState {
                hash_inserts_ready: false,
                scan_ready: false,
                drain_ready: false,
                remaining_hash_inserters: DelayedPartitionCount::uninit(),
                remaining_probers: DelayedPartitionCount::uninit(),
                pending_hash_inserters: PartitionWakers::empty(),
                pending_probers: PartitionWakers::empty(),
                pending_drainers: PartitionWakers::empty(),
            }),
        })
    }

    fn output_types(&self) -> &[DataType] {
        &self.output_types
    }
}

impl PushOperator for PhysicalHashJoin {
    type PartitionPushState = HashJoinPartitionBuildState;

    fn create_partition_push_states(
        &self,
        operator_state: &Self::OperatorState,
        _props: ExecutionProperties,
        partitions: usize,
    ) -> Result<Vec<Self::PartitionPushState>> {
        let mut shared = operator_state.shared.lock();
        shared.remaining_hash_inserters.set(partitions)?;
        shared.remaining_probers.set(partitions)?;
        shared
            .pending_hash_inserters
            .init_for_partitions(partitions);
        shared.pending_drainers.init_for_partitions(partitions);

        let table = &operator_state.table;
        let table_state = &operator_state.table_state;

        let states = table.create_build_partition_states(table_state, partitions)?;
        let states = states
            .into_iter()
            .map(|state| HashJoinPartitionBuildState {
                finalize_phase: BuildFinalizePhase::Collecting,
                build_state: state,
            })
            .collect();

        Ok(states)
    }

    fn poll_push(
        &self,
        _cx: &mut Context,
        operator_state: &Self::OperatorState,
        state: &mut Self::PartitionPushState,
        input: &mut Batch,
    ) -> Result<PollPush> {
        let table = &operator_state.table;
        let table_state = &operator_state.table_state;

        table.collect_build(table_state, &mut state.build_state, input)?;

        Ok(PollPush::NeedsMore)
    }

    fn poll_finalize_push(
        &self,
        cx: &mut Context,
        operator_state: &Self::OperatorState,
        state: &mut Self::PartitionPushState,
    ) -> Result<PollFinalize> {
        let table = &operator_state.table;
        let table_state = &operator_state.table_state;

        loop {
            match state.finalize_phase {
                BuildFinalizePhase::Collecting => {
                    // We're finalizing our build.
                    let is_last = table.finish_build(table_state, &mut state.build_state)?;

                    // Next call to finalize will be for inserting the hashes.
                    state.finalize_phase = BuildFinalizePhase::InsertingHashes;

                    if is_last {
                        // We're the last partition. Init the directory.
                        unsafe { table.init_directory(table_state)? };

                        // Now wake up all pending hash inserters.
                        let mut shared = operator_state.shared.lock();
                        shared.hash_inserts_ready = true;
                        shared.pending_hash_inserters.wake_all();

                        // Jump directly to inserting.
                        continue;
                    } else {
                        // Other partitions still building, we'll need to wait until we
                        // can insert the hashes.
                        let mut shared = operator_state.shared.lock();
                        if shared.hash_inserts_ready {
                            // We raced with the last partition. Directory was
                            // initialize before we could actually insert our
                            // waker.
                            //
                            // Continue to inserting hashes.
                            continue;
                        }

                        shared
                            .pending_hash_inserters
                            .store(cx.waker(), state.build_state.partition_idx);

                        return Ok(PollFinalize::Pending);
                    }
                }
                BuildFinalizePhase::InsertingHashes => {
                    // We're in the hash inserting phase.
                    let mut shared = operator_state.shared.lock();
                    if !shared.hash_inserts_ready {
                        // We're not ready to insert the hashes (directory not
                        // ready). Come back later.
                        shared
                            .pending_hash_inserters
                            .store(cx.waker(), state.build_state.partition_idx);
                        return Ok(PollFinalize::Pending);
                    }
                    std::mem::drop(shared);

                    // SAFETY: We've indicated that hash inserts are ready, so we should
                    // have the directory available.
                    //
                    // Parallel inserts from multiple partitions is ok.
                    unsafe { table.process_hashes(table_state, &mut state.build_state)? }

                    let mut shared = operator_state.shared.lock();
                    let remaining = shared.remaining_hash_inserters.dec_by_one()?;
                    if remaining == 0 {
                        // We're the last partition to complete inserting hashes.
                        // Wake up all pending probers.
                        shared.scan_ready = true;
                        shared.pending_probers.wake_all();
                        // If right side never actually produces batches, it's
                        // possible that all partitions finalize (and jump to
                        // the drain state) before the left side completes.
                        //
                        // Go ahead and wake up drainers.
                        shared.pending_drainers.wake_all();
                    }

                    return Ok(PollFinalize::Finalized);
                }
            }
        }
    }
}

impl ExecuteOperator for PhysicalHashJoin {
    type PartitionExecuteState = HashJoinPartitionExecuteState;

    fn create_partition_execute_states(
        &self,
        operator_state: &Self::OperatorState,
        _props: ExecutionProperties,
        partitions: usize,
    ) -> Result<Vec<Self::PartitionExecuteState>> {
        let mut shared = operator_state.shared.lock();
        shared.pending_probers.init_for_partitions(partitions);

        let table = &operator_state.table;
        let table_state = &operator_state.table_state;

        let states = table.create_probe_partition_states(table_state, partitions)?;
        let states = states
            .into_iter()
            .map(|state| HashJoinPartitionExecuteState::Probing {
                scan_ready: false,
                rhs_needs_probe: true,
                scan_state: Box::new(state),
            })
            .collect();

        Ok(states)
    }

    fn poll_execute(
        &self,
        cx: &mut Context,
        operator_state: &Self::OperatorState,
        state: &mut Self::PartitionExecuteState,
        input: &mut Batch,
        output: &mut Batch,
    ) -> Result<PollExecute> {
        let table = &operator_state.table;
        let table_state = &operator_state.table_state;

        match state {
            HashJoinPartitionExecuteState::Probing {
                scan_ready,
                rhs_needs_probe,
                scan_state,
            } => {
                if !*scan_ready {
                    // Check global state to see if we're actually ready for scanning.
                    let mut shared = operator_state.shared.lock();
                    if !shared.scan_ready {
                        // Come back later.
                        shared
                            .pending_probers
                            .store(cx.waker(), scan_state.partition_idx);
                        return Ok(PollExecute::Pending);
                    }

                    // We're ready, continue on...
                    *scan_ready = true;
                }

                if *rhs_needs_probe {
                    // New RHS batch, refresh scan state.
                    table.probe(table_state, scan_state, input)?;
                    *rhs_needs_probe = false;
                    // Continue...
                }

                // Scan it...
                scan_state.scan_next(table, table_state, input, output)?;

                if output.num_rows() == 0 {
                    // We scanned nothing. Either no matches or we've completely drained
                    // the state. Indicate we need a new RHS.
                    //
                    // Next RHS will trigger a probe.
                    *rhs_needs_probe = true;
                    return Ok(PollExecute::NeedsMore);
                }

                // Otherwise we produced output. Keep polling with the same RHS as we
                // may have more matches to drain.
                Ok(PollExecute::HasMore)
            }
            HashJoinPartitionExecuteState::Draining {
                drain_ready,
                drain_state,
            } => {
                if !*drain_ready {
                    // Check global state to see if we're ready to drain.
                    let mut shared = operator_state.shared.lock();
                    if !(shared.drain_ready && shared.scan_ready) {
                        // `drain_ready` gets set by right side, `scan_ready`
                        // gets set by left. Both must be true before we can
                        // actually drain, as that indicates the hash table has
                        // been completely build _and_ probed.
                        //
                        // If either is false, come back later.
                        //
                        // TODO: Might be worthwile to make the finalize pending
                        // if the left side hasn't been built yet.
                        shared
                            .pending_drainers
                            .store(cx.waker(), drain_state.partition_idx);
                        return Ok(PollExecute::Pending);
                    }

                    // We're read, continue on...
                    *drain_ready = true;
                }

                drain_state.drain_next(table, table_state, output)?;
                if output.num_rows() == 0 {
                    // We're done.
                    return Ok(PollExecute::Exhausted);
                }

                // Keep draining.
                Ok(PollExecute::HasMore)
            }
        }
    }

    fn poll_finalize_execute(
        &self,
        _cx: &mut Context,
        operator_state: &Self::OperatorState,
        state: &mut Self::PartitionExecuteState,
    ) -> Result<PollFinalize> {
        let table = &operator_state.table;
        let table_state = &operator_state.table_state;

        if needs_drain(table.join_type) {
            // Need to drain, update our state.
            let scan_state = match state {
                HashJoinPartitionExecuteState::Probing { scan_state, .. } => scan_state,
                _ => return Err(DbError::new("Prober in unexpected state")),
            };

            let drain_state = table.create_drain_state_from_scan_state(table_state, scan_state)?;

            *state = HashJoinPartitionExecuteState::Draining {
                drain_ready: false,
                drain_state,
            };

            let mut shared = operator_state.shared.lock();
            let remaining = shared.remaining_probers.dec_by_one()?;
            if remaining == 0 {
                // We're the last partition to finish probing, wake up all
                // drainers.
                shared.drain_ready = true;
                shared.pending_drainers.wake_all();
            }

            Ok(PollFinalize::NeedsDrain)
        } else {
            // We're done.
            Ok(PollFinalize::Finalized)
        }
    }
}

impl Explainable for PhysicalHashJoin {
    fn explain_entry(&self, conf: ExplainConfig) -> ExplainEntry {
        EntryBuilder::new(Self::OPERATOR_NAME, conf)
            .with_value("join_type", self.join_type)
            .with_values("conditions", &self.conditions)
            .build()
    }
}
