use super::basic_block::BasicBlockId;
use super::dfg::DataFlowGraph;
use super::instruction::Instruction;
use super::map::{Id, SecondaryMap};
use super::types::Type;

use noirc_errors::Location;

/// A function holds a list of instructions.
/// These instructions are further grouped into Basic blocks
///
/// Like Crane-lift all functions outside of the current function is seen as external.
/// To reference external functions, one must first import the function signature
/// into the current function's context.
#[derive(Debug)]
pub(crate) struct Function {
    /// Maps instructions to source locations
    source_locations: SecondaryMap<Instruction, Location>,

    /// The first basic block in the function
    entry_block: BasicBlockId,

    pub(crate) dfg: DataFlowGraph,
}

impl Function {
    /// Creates a new function with an automatically inserted entry block.
    ///
    /// Note that any parameters to the function must be manually added later.
    pub(crate) fn new() -> Self {
        let mut dfg = DataFlowGraph::default();
        let entry_block = dfg.new_block();
        Self { source_locations: SecondaryMap::new(), entry_block, dfg }
    }

    pub(crate) fn entry_block(&self) -> BasicBlockId {
        self.entry_block
    }
}

/// FunctionId is a reference for a function
pub(crate) type FunctionId = Id<Function>;

#[derive(Debug, Default, Clone)]
pub(crate) struct Signature {
    pub(crate) params: Vec<Type>,
    pub(crate) returns: Vec<Type>,
}

#[test]
fn sign_smoke() {
    let mut signature = Signature::default();

    signature.params.push(Type::Numeric(super::types::NumericType::NativeField));
    signature.returns.push(Type::Numeric(super::types::NumericType::Unsigned { bit_size: 32 }));
}
