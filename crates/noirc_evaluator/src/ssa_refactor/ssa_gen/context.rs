use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Mutex, RwLock};

use acvm::FieldElement;
use iter_extended::{try_vecmap, vecmap};
use noirc_errors::Location;
use noirc_frontend::monomorphization::ast::{self, LocalId, Parameters};
use noirc_frontend::monomorphization::ast::{FuncId, Program};
use noirc_frontend::Signedness;

use crate::errors::InternalError;
use crate::ssa_refactor::ir::dfg::DataFlowGraph;
use crate::ssa_refactor::ir::function::FunctionId as IrFunctionId;
use crate::ssa_refactor::ir::function::{Function, RuntimeType};
use crate::ssa_refactor::ir::instruction::BinaryOp;
use crate::ssa_refactor::ir::map::AtomicCounter;
use crate::ssa_refactor::ir::types::{NumericType, Type};
use crate::ssa_refactor::ir::value::ValueId;
use crate::ssa_refactor::ssa_builder::FunctionBuilder;

use super::value::{Tree, Value, Values};

/// The FunctionContext is the main context object for translating a
/// function into SSA form during the SSA-gen pass.
///
/// This context can be used to build any amount of functions,
/// so long as it is cleared out in between each function via
/// calling self.new_function().
///
/// If compiling many functions across multiple threads, there should
/// be a separate FunctionContext for each thread. Each FunctionContext
/// can communicate via the SharedContext field which as its name suggests
/// is the only part of the context that needs to be shared between threads.
pub(super) struct FunctionContext<'a> {
    definitions: HashMap<LocalId, Values>,

    pub(super) builder: FunctionBuilder,
    shared_context: &'a SharedContext,
}

/// Shared context for all functions during ssa codegen. This is the only
/// object that is shared across all threads when generating ssa in multiple threads.
///
/// The main job of the SharedContext is to remember which functions are already
/// compiled, what their IDs are, and keep a queue of which functions still need to
/// be compiled.
///
/// SSA can be generated by continuously popping from this function_queue and using
/// FunctionContext to generate from the popped function id. Once the queue is empty,
/// no other functions are reachable and the SSA generation is finished.
pub(super) struct SharedContext {
    /// All currently known functions which have already been assigned function ids.
    /// These functions are all either currently having their SSA generated or are
    /// already finished.
    functions: RwLock<HashMap<FuncId, IrFunctionId>>,

    /// Queue of which functions still need to be compiled.
    ///
    /// The queue is currently Last-in First-out (LIFO) but this is an
    /// implementation detail that can be trivially changed and should
    /// not impact the resulting SSA besides changing which IDs are assigned
    /// to which functions.
    function_queue: Mutex<FunctionQueue>,

    /// Shared counter used to assign the ID of the next function
    function_counter: AtomicCounter<Function>,

    /// The entire monomorphized source program
    pub(super) program: Program,
}

/// The queue of functions remaining to compile
type FunctionQueue = Vec<(ast::FuncId, IrFunctionId)>;

impl<'a> FunctionContext<'a> {
    /// Create a new FunctionContext to compile the first function in the shared_context's
    /// function queue.
    ///
    /// This will pop from the function queue, so it is expected the shared_context's function
    /// queue is non-empty at the time of calling this function. This can be ensured by calling
    /// `shared_context.get_or_queue_function(function_to_queue)` before calling this constructor.
    ///
    /// `function_name` and `parameters` are expected to be the name and parameters of the function
    /// this constructor will pop from the function queue.
    pub(super) fn new(
        function_name: String,
        parameters: &Parameters,
        runtime: RuntimeType,
        shared_context: &'a SharedContext,
    ) -> Result<Self, InternalError> {
        let function_id = shared_context
            .pop_next_function_in_queue()
            .expect("No function in queue for the FunctionContext to compile")
            .1;

        let builder = FunctionBuilder::new(function_name, function_id, runtime);
        let mut this = Self { definitions: HashMap::new(), builder, shared_context };
        this.add_parameters_to_scope(parameters)?;
        Ok(this)
    }

    /// Finish building the current function and switch to building a new function with the
    /// given name, id, and parameters.
    ///
    /// Note that the previous function cannot be resumed after calling this. Developers should
    /// avoid calling new_function until the previous function is completely finished with ssa-gen.
    pub(super) fn new_function(
        &mut self,
        id: IrFunctionId,
        func: &ast::Function,
    ) -> Result<(), InternalError> {
        self.definitions.clear();
        if func.unconstrained {
            self.builder.new_brillig_function(func.name.clone(), id);
        } else {
            self.builder.new_function(func.name.clone(), id);
        }
        self.add_parameters_to_scope(&func.parameters)?;
        Ok(())
    }

    /// Add each parameter to the current scope, and return the list of parameter types.
    ///
    /// The returned parameter type list will be flattened, so any struct parameters will
    /// be returned as one entry for each field (recursively).
    fn add_parameters_to_scope(&mut self, parameters: &Parameters) -> Result<(), InternalError> {
        for (id, mutable, _, typ) in parameters {
            self.add_parameter_to_scope(*id, typ, *mutable)?;
        }
        Ok(())
    }

    /// Adds a "single" parameter to scope.
    ///
    /// Single is in quotes here because in the case of tuple parameters, the tuple is flattened
    /// into a new parameter for each field recursively.
    fn add_parameter_to_scope(
        &mut self,
        parameter_id: LocalId,
        parameter_type: &ast::Type,
        mutable: bool,
    ) -> Result<(), InternalError> {
        // Add a separate parameter for each field type in 'parameter_type'
        let parameter_value = Self::try_map_type(parameter_type, |typ| {
            let value = self.builder.add_parameter(typ);
            if mutable {
                self.new_mutable_variable(value)
            } else {
                Ok(value.into())
            }
        })?;

        self.definitions.insert(parameter_id, parameter_value);
        Ok(())
    }

    /// Allocate a single slot of memory and store into it the given initial value of the variable.
    /// Always returns a Value::Mutable wrapping the allocate instruction.
    pub(super) fn new_mutable_variable(
        &mut self,
        value_to_store: ValueId,
    ) -> Result<Value, InternalError> {
        let alloc = self.builder.insert_allocate()?;
        self.builder.insert_store(alloc, value_to_store)?;
        let typ = self.builder.type_of_value(value_to_store);
        Ok(Value::Mutable(alloc, typ))
    }

    /// Maps the given type to a Tree of the result type.
    ///
    /// This can be used to (for example) flatten a tuple type, creating
    /// and returning a new parameter for each field type.
    pub(super) fn map_type<T>(typ: &ast::Type, mut f: impl FnMut(Type) -> T) -> Tree<T> {
        Self::map_type_helper(typ, &mut f)
    }

    // This helper is needed because we need to take f by mutable reference,
    // otherwise we cannot move it multiple times each loop of vecmap.
    fn map_type_helper<T>(typ: &ast::Type, f: &mut dyn FnMut(Type) -> T) -> Tree<T> {
        match typ {
            ast::Type::Tuple(fields) => {
                Tree::Branch(vecmap(fields, |field| Self::map_type_helper(field, f)))
            }
            ast::Type::Unit => Tree::empty(),
            // A mutable reference wraps each element into a reference.
            // This can be multiple values if the element type is a tuple.
            ast::Type::MutableReference(element) => {
                Self::map_type_helper(element, &mut |_| f(Type::Reference))
            }
            ast::Type::FmtString(len, fields) => {
                // A format string is represented by multiple values
                // The message string, the number of fields to be formatted, and
                // then the encapsulated fields themselves
                let final_fmt_str_fields =
                    vec![ast::Type::String(*len), ast::Type::Field, *fields.clone()];
                let fmt_str_tuple = ast::Type::Tuple(final_fmt_str_fields);
                Self::map_type_helper(&fmt_str_tuple, f)
            }
            other => Tree::Leaf(f(Self::convert_non_tuple_type(other))),
        }
    }

    /// Maps the given type to a Tree of the result type.
    ///
    /// This can be used to (for example) flatten a tuple type, creating
    /// and returning a new parameter for each field type.
    pub(super) fn try_map_type<T, E>(
        typ: &ast::Type,
        mut f: impl FnMut(Type) -> Result<T, E>,
    ) -> Result<Tree<T>, E> {
        Self::try_map_type_helper(typ, &mut f)
    }

    // This helper is needed because we need to take f by mutable reference,
    // otherwise we cannot move it multiple times each loop of vecmap.
    fn try_map_type_helper<T, E>(
        typ: &ast::Type,
        f: &mut dyn FnMut(Type) -> Result<T, E>,
    ) -> Result<Tree<T>, E> {
        match typ {
            ast::Type::Tuple(fields) => {
                Ok(Tree::Branch(try_vecmap(fields, |field| Self::try_map_type_helper(field, f))?))
            }
            ast::Type::Unit => Ok(Tree::empty()),
            // A mutable reference wraps each element into a reference.
            // This can be multiple values if the element type is a tuple.
            ast::Type::MutableReference(element) => {
                Self::try_map_type_helper(element, &mut |_| f(Type::Reference))
            }
            other => Ok(Tree::Leaf(f(Self::convert_non_tuple_type(other))?)),
        }
    }

    /// Convert a monomorphized type to an SSA type, preserving the structure
    /// of any tuples within.
    pub(super) fn convert_type(typ: &ast::Type) -> Tree<Type> {
        // Do nothing in the closure here - map_type_helper already calls
        // convert_non_tuple_type internally.
        Self::map_type_helper(typ, &mut |x| x)
    }

    /// Converts a non-tuple type into an SSA type. Panics if a tuple type is passed.
    ///
    /// This function is needed since this SSA IR has no concept of tuples and thus no type for
    /// them. Use `convert_type` if tuple types need to be handled correctly.
    pub(super) fn convert_non_tuple_type(typ: &ast::Type) -> Type {
        match typ {
            ast::Type::Field => Type::field(),
            ast::Type::Array(len, element) => {
                let element_types = Self::convert_type(element).flatten();
                Type::Array(Rc::new(element_types), *len as usize)
            }
            ast::Type::Integer(Signedness::Signed, bits) => Type::signed(*bits),
            ast::Type::Integer(Signedness::Unsigned, bits) => Type::unsigned(*bits),
            ast::Type::Bool => Type::unsigned(1),
            ast::Type::String(len) => Type::Array(Rc::new(vec![Type::char()]), *len as usize),
            ast::Type::FmtString(_, _) => {
                panic!("convert_non_tuple_type called on a fmt string: {typ}")
            }
            ast::Type::Unit => panic!("convert_non_tuple_type called on a unit type"),
            ast::Type::Tuple(_) => panic!("convert_non_tuple_type called on a tuple: {typ}"),
            ast::Type::Function(_, _) => Type::Function,
            ast::Type::Slice(element) => {
                let element_types = Self::convert_type(element).flatten();
                Type::Slice(Rc::new(element_types))
            }
            ast::Type::MutableReference(element) => {
                // Recursive call to panic if element is a tuple
                Self::convert_non_tuple_type(element);
                Type::Reference
            }
        }
    }

    /// Returns the unit value, represented as an empty tree of values
    pub(super) fn unit_value() -> Values {
        Values::empty()
    }

    /// Insert a binary instruction at the end of the current block.
    /// Converts the form of the binary instruction as necessary
    /// (e.g. swapping arguments, inserting a not) to represent it in the IR.
    /// For example, (a <= b) is represented as !(b < a)
    pub(super) fn insert_binary(
        &mut self,
        mut lhs: ValueId,
        operator: noirc_frontend::BinaryOpKind,
        mut rhs: ValueId,
        location: Location,
    ) -> Result<Values, InternalError> {
        let op = convert_operator(operator);

        if op == BinaryOp::Eq && matches!(self.builder.type_of_value(lhs), Type::Array(..)) {
            return self.insert_array_equality(lhs, operator, rhs, location);
        }

        if operator_requires_swapped_operands(operator) {
            std::mem::swap(&mut lhs, &mut rhs);
        }

        let mut result = self.builder.set_location(location).insert_binary(lhs, op, rhs)?;

        if let Some(max_bit_size) = operator_result_max_bit_size_to_truncate(
            operator,
            lhs,
            rhs,
            &self.builder.current_function.dfg,
        ) {
            let result_type = self.builder.current_function.dfg.type_of_value(result);
            let bit_size = match result_type {
                Type::Numeric(NumericType::Signed { bit_size })
                | Type::Numeric(NumericType::Unsigned { bit_size }) => bit_size,
                _ => {
                    unreachable!("ICE: Truncation attempted on non-integer");
                }
            };
            result = self.builder.insert_truncate(result, bit_size, max_bit_size)?;
        }

        if operator_requires_not(operator) {
            result = self.builder.insert_not(result)?;
        }
        Ok(result.into())
    }

    /// The frontend claims to support equality (==) on arrays, so we must support it in SSA here.
    /// The actual BinaryOp::Eq in SSA is meant only for primitive numeric types so we encode an
    /// entire equality loop on each array element. The generated IR is as follows:
    ///
    ///   ...
    ///   result_alloc = allocate
    ///   store u1 1 in result_alloc
    ///   jmp loop_start(0)
    /// loop_start(i: Field):
    ///   v0 = lt i, array_len
    ///   jmpif v0, then: loop_body, else: loop_end
    /// loop_body():
    ///   v1 = array_get lhs, index i
    ///   v2 = array_get rhs, index i
    ///   v3 = eq v1, v2
    ///   v4 = load result_alloc
    ///   v5 = and v4, v3
    ///   store v5 in result_alloc
    ///   v6 = add i, Field 1
    ///   jmp loop_start(v6)
    /// loop_end():
    ///   result = load result_alloc
    fn insert_array_equality(
        &mut self,
        lhs: ValueId,
        operator: noirc_frontend::BinaryOpKind,
        rhs: ValueId,
        location: Location,
    ) -> Result<Values, InternalError> {
        let lhs_type = self.builder.type_of_value(lhs);
        let rhs_type = self.builder.type_of_value(rhs);

        let (array_length, element_type) = match (lhs_type, rhs_type) {
            (
                Type::Array(lhs_composite_type, lhs_length),
                Type::Array(rhs_composite_type, rhs_length),
            ) => {
                assert!(
                    lhs_composite_type.len() == 1 && rhs_composite_type.len() == 1,
                    "== is unimplemented for arrays of structs"
                );
                assert_eq!(lhs_composite_type[0], rhs_composite_type[0]);
                assert_eq!(lhs_length, rhs_length, "Expected two arrays of equal length");
                (lhs_length, lhs_composite_type[0].clone())
            }
            _ => unreachable!("Expected two array values"),
        };

        let loop_start = self.builder.insert_block();
        let loop_body = self.builder.insert_block();
        let loop_end = self.builder.insert_block();

        // pre-loop
        let result_alloc = self.builder.set_location(location).insert_allocate()?;
        let true_value = self.builder.numeric_constant(1u128, Type::bool());
        self.builder.insert_store(result_alloc, true_value)?;
        let zero = self.builder.field_constant(0u128);
        self.builder.terminate_with_jmp(loop_start, vec![zero]);

        // loop_start
        self.builder.switch_to_block(loop_start);
        let i = self.builder.add_block_parameter(loop_start, Type::field());
        let array_length = self.builder.field_constant(array_length as u128);
        let v0 = self.builder.insert_binary(i, BinaryOp::Lt, array_length)?;
        self.builder.terminate_with_jmpif(v0, loop_body, loop_end);

        // loop body
        self.builder.switch_to_block(loop_body);
        let v1 = self.builder.insert_array_get(lhs, i, element_type.clone())?;
        let v2 = self.builder.insert_array_get(rhs, i, element_type)?;
        let v3 = self.builder.insert_binary(v1, BinaryOp::Eq, v2)?;
        let v4 = self.builder.insert_load(result_alloc, Type::bool())?;
        let v5 = self.builder.insert_binary(v4, BinaryOp::And, v3)?;
        self.builder.insert_store(result_alloc, v5)?;
        let one = self.builder.field_constant(1u128);
        let v6 = self.builder.insert_binary(i, BinaryOp::Add, one)?;
        self.builder.terminate_with_jmp(loop_start, vec![v6]);

        // loop end
        self.builder.switch_to_block(loop_end);
        let mut result = self.builder.insert_load(result_alloc, Type::bool())?;

        if operator_requires_not(operator) {
            result = self.builder.insert_not(result)?;
        }
        Ok(result.into())
    }

    /// Inserts a call instruction at the end of the current block and returns the results
    /// of the call.
    ///
    /// Compared to self.builder.insert_call, this version will reshape the returned Vec<ValueId>
    /// back into a Values tree of the proper shape.
    pub(super) fn insert_call(
        &mut self,
        function: ValueId,
        arguments: Vec<ValueId>,
        result_type: &ast::Type,
        location: Location,
    ) -> Result<Values, InternalError> {
        let result_types = Self::convert_type(result_type).flatten();
        let results =
            self.builder.set_location(location).insert_call(function, arguments, result_types)?;

        let mut i = 0;
        let reshaped_return_values = Self::map_type(result_type, |_| {
            let result = results[i].into();
            i += 1;
            result
        });
        assert_eq!(i, results.len());
        Ok(reshaped_return_values)
    }

    /// Create a const offset of an address for an array load or store
    pub(super) fn make_offset(
        &mut self,
        mut address: ValueId,
        offset: u128,
    ) -> Result<ValueId, InternalError> {
        if offset != 0 {
            let offset = self.builder.field_constant(offset);
            address = self.builder.insert_binary(address, BinaryOp::Add, offset)?;
        }
        Ok(address)
    }

    /// Define a local variable to be some Values that can later be retrieved
    /// by calling self.lookup(id)
    pub(super) fn define(&mut self, id: LocalId, value: Values) {
        let existing = self.definitions.insert(id, value);
        assert!(existing.is_none(), "Variable {id:?} was defined twice in ssa-gen pass");
    }

    /// Looks up the value of a given local variable. Expects the variable to have
    /// been previously defined or panics otherwise.
    pub(super) fn lookup(&self, id: LocalId) -> Values {
        self.definitions.get(&id).expect("lookup: variable not defined").clone()
    }

    /// Extract the given field of the tuple. Panics if the given Values is not
    /// a Tree::Branch or does not have enough fields.
    pub(super) fn get_field(tuple: Values, field_index: usize) -> Values {
        match tuple {
            Tree::Branch(mut trees) => trees.remove(field_index),
            Tree::Leaf(value) => {
                unreachable!("Tried to extract tuple index {field_index} from non-tuple {value:?}")
            }
        }
    }

    /// Extract the given field of the tuple by reference. Panics if the given Values is not
    /// a Tree::Branch or does not have enough fields.
    pub(super) fn get_field_ref(tuple: &Values, field_index: usize) -> &Values {
        match tuple {
            Tree::Branch(trees) => &trees[field_index],
            Tree::Leaf(value) => {
                unreachable!("Tried to extract tuple index {field_index} from non-tuple {value:?}")
            }
        }
    }

    /// Replace the given field of the tuple with a new one. Panics if the given Values is not
    /// a Tree::Branch or does not have enough fields.
    pub(super) fn replace_field(tuple: Values, field_index: usize, new_value: Values) -> Values {
        match tuple {
            Tree::Branch(mut trees) => {
                trees[field_index] = new_value;
                Tree::Branch(trees)
            }
            Tree::Leaf(value) => {
                unreachable!("Tried to extract tuple index {field_index} from non-tuple {value:?}")
            }
        }
    }

    /// Retrieves the given function, adding it to the function queue
    /// if it is not yet compiled.
    pub(super) fn get_or_queue_function(&mut self, id: FuncId) -> Values {
        let function = self.shared_context.get_or_queue_function(id);
        self.builder.import_function(function).into()
    }

    /// Extracts the current value out of an LValue.
    ///
    /// Goal: Handle the case of assigning to nested expressions such as `foo.bar[i1].baz[i2] = e`
    ///       while also noting that assigning to arrays will create a new array rather than mutate
    ///       the original.
    ///
    /// Method: First `extract_current_value` must recurse on the lvalue to extract the current
    ///         value contained:
    ///
    /// v0 = foo.bar                 ; allocate instruction for bar
    /// v1 = load v0                 ; loading the bar array
    /// v2 = add i1, baz_index       ; field offset for index i1, field baz
    /// v3 = array_get v1, index v2  ; foo.bar[i1].baz
    ///
    /// Method (part 2): Then, `assign_new_value` will recurse in the opposite direction to
    ///                  construct the larger value as needed until we can `store` to the nearest
    ///                  allocation.
    ///
    /// v4 = array_set v3, index i2, e   ; finally create a new array setting the desired value
    /// v5 = array_set v1, index v2, v4  ; now must also create the new bar array
    /// store v5 in v0                   ; and store the result in the only mutable reference
    ///
    /// The returned `LValueRef` tracks the current value at each step of the lvalue.
    /// This is later used by `assign_new_value` to construct a new updated value that
    /// can be assigned to an allocation within the LValueRef::Ident.
    ///
    /// This is operationally equivalent to extract_current_value_recursive, but splitting these
    /// into two separate functions avoids cloning the outermost `Values` returned by the recursive
    /// version, as it is only needed for recursion.
    pub(super) fn extract_current_value(
        &mut self,
        lvalue: &ast::LValue,
    ) -> Result<LValue, InternalError> {
        match lvalue {
            ast::LValue::Ident(ident) => Ok(LValue::Ident(self.ident_lvalue(ident))),
            ast::LValue::Index { array, index, .. } => Ok(self.index_lvalue(array, index)?.2),
            ast::LValue::MemberAccess { object, field_index } => {
                let (old_object, object_lvalue) = self.extract_current_value_recursive(object)?;
                let object_lvalue = Box::new(object_lvalue);
                Ok(LValue::MemberAccess { old_object, object_lvalue, index: *field_index })
            }
            ast::LValue::Dereference { reference, .. } => {
                let (reference, _) = self.extract_current_value_recursive(reference)?;
                Ok(LValue::Dereference { reference })
            }
        }
    }

    pub(super) fn dereference(
        &mut self,
        values: &Values,
        element_type: &ast::Type,
    ) -> Result<Values, InternalError> {
        let element_types = Self::convert_type(element_type);
        values.try_map_both(element_types, |value, element_type| {
            let reference = value.eval(self)?;
            Ok(self.builder.insert_load(reference, element_type)?.into())
        })
    }

    /// Compile the given identifier as a reference - ie. avoid calling .eval()
    fn ident_lvalue(&self, ident: &ast::Ident) -> Values {
        match &ident.definition {
            ast::Definition::Local(id) => self.lookup(*id),
            other => panic!("Unexpected definition found for mutable value: {other}"),
        }
    }

    /// Compile the given `array[index]` expression as a reference.
    /// This will return a triple of (array, index, lvalue_ref) where the lvalue_ref records the
    /// structure of the lvalue expression for use by `assign_new_value`.
    fn index_lvalue(
        &mut self,
        array: &ast::LValue,
        index: &ast::Expression,
    ) -> Result<(ValueId, ValueId, LValue), InternalError> {
        let (old_array, array_lvalue) = self.extract_current_value_recursive(array)?;
        let old_array = old_array.into_leaf().eval(self)?;
        let array_lvalue = Box::new(array_lvalue);
        let index = self.codegen_non_tuple_expression(index)?;
        Ok((old_array, index, LValue::Index { old_array, index, array_lvalue }))
    }

    fn extract_current_value_recursive(
        &mut self,
        lvalue: &ast::LValue,
    ) -> Result<(Values, LValue), InternalError> {
        match lvalue {
            ast::LValue::Ident(ident) => {
                let variable = self.ident_lvalue(ident);
                Ok((variable.clone(), LValue::Ident(variable)))
            }
            ast::LValue::Index { array, index, element_type, location } => {
                let (old_array, index, index_lvalue) = self.index_lvalue(array, index)?;
                let element =
                    self.codegen_array_index(old_array, index, element_type, *location)?;
                Ok((element, index_lvalue))
            }
            ast::LValue::MemberAccess { object, field_index: index } => {
                let (old_object, object_lvalue) = self.extract_current_value_recursive(object)?;
                let object_lvalue = Box::new(object_lvalue);
                let element = Self::get_field_ref(&old_object, *index).clone();
                Ok((element, LValue::MemberAccess { old_object, object_lvalue, index: *index }))
            }
            ast::LValue::Dereference { reference, element_type } => {
                let (reference, _) = self.extract_current_value_recursive(reference)?;
                let dereferenced = self.dereference(&reference, element_type)?;
                Ok((dereferenced, LValue::Dereference { reference }))
            }
        }
    }

    /// Assigns a new value to the given LValue.
    /// The LValue can be created via a previous call to extract_current_value.
    /// This method recurs on the given LValue to create a new value to assign an allocation
    /// instruction within an LValue::Ident or LValue::Dereference - see the comment on
    /// `extract_current_value` for more details.
    pub(super) fn assign_new_value(
        &mut self,
        lvalue: LValue,
        new_value: Values,
    ) -> Result<(), InternalError> {
        match lvalue {
            LValue::Ident(references) => self.assign(references, new_value),
            LValue::Index { old_array: mut array, index, array_lvalue } => {
                let element_size = self.builder.field_constant(self.element_size(array));

                // The actual base index is the user's index * the array element type's size
                let mut index = self.builder.insert_binary(index, BinaryOp::Mul, element_size)?;
                let one = self.builder.field_constant(FieldElement::one());

                new_value.try_for_each(|value| {
                    let value = value.eval(self)?;
                    array = self.builder.insert_array_set(array, index, value)?;
                    index = self.builder.insert_binary(index, BinaryOp::Add, one)?;
                    Ok::<(), InternalError>(())
                })?;

                self.assign_new_value(*array_lvalue, array.into())?;
                Ok(())
            }
            LValue::MemberAccess { old_object, index, object_lvalue } => {
                let new_object = Self::replace_field(old_object, index, new_value);
                self.assign_new_value(*object_lvalue, new_object)?;
                Ok(())
            }
            LValue::Dereference { reference } => {
                self.assign(reference, new_value)?;
                Ok(())
            }
        }
    }

    fn element_size(&self, array: ValueId) -> FieldElement {
        match self.builder.type_of_value(array) {
            Type::Array(elements, _) | Type::Slice(elements) => (elements.len() as u128).into(),
            t => panic!("Uncaught type error: tried to take element size of non-array type {t}"),
        }
    }

    /// Given an lhs containing only references, create a store instruction to store each value of
    /// rhs into its corresponding value in lhs.
    fn assign(&mut self, lhs: Values, rhs: Values) -> Result<(), InternalError> {
        match (lhs, rhs) {
            (Tree::Branch(lhs_branches), Tree::Branch(rhs_branches)) => {
                assert_eq!(lhs_branches.len(), rhs_branches.len());

                for (lhs, rhs) in lhs_branches.into_iter().zip(rhs_branches) {
                    self.assign(lhs, rhs)?;
                }
                Ok(())
            }
            (Tree::Leaf(lhs), Tree::Leaf(rhs)) => {
                let (lhs, rhs) = (lhs.eval_reference(), rhs.eval(self)?);
                self.builder.insert_store(lhs, rhs)?;
                Ok(())
            }
            (lhs, rhs) => {
                unreachable!(
                    "assign: Expected lhs and rhs values to match but found {lhs:?} and {rhs:?}"
                )
            }
        }
    }
}

/// True if the given operator cannot be encoded directly and needs
/// to be represented as !(some other operator)
fn operator_requires_not(op: noirc_frontend::BinaryOpKind) -> bool {
    use noirc_frontend::BinaryOpKind::*;
    matches!(op, NotEqual | LessEqual | GreaterEqual)
}

/// True if the given operator cannot be encoded directly and needs
/// to have its lhs and rhs swapped to be represented with another operator.
/// Example: (a > b) needs to be represented as (b < a)
fn operator_requires_swapped_operands(op: noirc_frontend::BinaryOpKind) -> bool {
    use noirc_frontend::BinaryOpKind::*;
    matches!(op, Greater | LessEqual)
}

/// If the operation requires its result to be truncated because it is an integer, the maximum
/// number of bits that result may occupy is returned.
fn operator_result_max_bit_size_to_truncate(
    op: noirc_frontend::BinaryOpKind,
    lhs: ValueId,
    rhs: ValueId,
    dfg: &DataFlowGraph,
) -> Option<u32> {
    let lhs_type = dfg.type_of_value(lhs);
    let rhs_type = dfg.type_of_value(rhs);

    let get_bit_size = |typ| match typ {
        Type::Numeric(NumericType::Signed { bit_size } | NumericType::Unsigned { bit_size }) => {
            Some(bit_size)
        }
        _ => None,
    };

    let lhs_bit_size = get_bit_size(lhs_type)?;
    let rhs_bit_size = get_bit_size(rhs_type)?;
    use noirc_frontend::BinaryOpKind::*;
    match op {
        Add => Some(std::cmp::max(lhs_bit_size, rhs_bit_size) + 1),
        Subtract => Some(std::cmp::max(lhs_bit_size, rhs_bit_size) + 1),
        Multiply => Some(lhs_bit_size + rhs_bit_size),
        ShiftLeft => {
            if let Some(rhs_constant) = dfg.get_numeric_constant(rhs) {
                // Happy case is that we know precisely by how many bits the the integer will
                // increase: lhs_bit_size + rhs
                return Some(lhs_bit_size + (rhs_constant.to_u128() as u32));
            }
            // Unhappy case is that we don't yet know the rhs value, (even though it will
            // eventually have to resolve to a constant). The best we can is assume the value of
            // rhs to be the maximum value of it's numeric type. If that turns out to be larger
            // than the native field's bit size, we full back to using that.

            // The formula for calculating the max bit size of a left shift is:
            // lhs_bit_size + 2^{rhs_bit_size} - 1
            // Inferring the max bit size of left shift from its operands can result in huge
            // number, that might not only be larger than the native field's max bit size, but
            // furthermore might not be representable as a u32. Hence we use overflow checks and
            // fallback to the native field's max bits.
            let field_max_bits = FieldElement::max_num_bits();
            let (rhs_bit_size_pow_2, overflows) = 2_u32.overflowing_pow(rhs_bit_size);
            if overflows {
                return Some(field_max_bits);
            }
            let (max_bits_plus_1, overflows) = rhs_bit_size_pow_2.overflowing_add(lhs_bit_size);
            if overflows {
                return Some(field_max_bits);
            }
            let max_bit_size = std::cmp::min(max_bits_plus_1 - 1, field_max_bits);
            Some(max_bit_size)
        }
        _ => None,
    }
}

/// Converts the given operator to the appropriate BinaryOp.
/// Take care when using this to insert a binary instruction: this requires
/// checking operator_requires_not and operator_requires_swapped_operands
/// to represent the full operation correctly.
fn convert_operator(op: noirc_frontend::BinaryOpKind) -> BinaryOp {
    use noirc_frontend::BinaryOpKind;
    match op {
        BinaryOpKind::Add => BinaryOp::Add,
        BinaryOpKind::Subtract => BinaryOp::Sub,
        BinaryOpKind::Multiply => BinaryOp::Mul,
        BinaryOpKind::Divide => BinaryOp::Div,
        BinaryOpKind::Modulo => BinaryOp::Mod,
        BinaryOpKind::Equal => BinaryOp::Eq,
        BinaryOpKind::NotEqual => BinaryOp::Eq, // Requires not
        BinaryOpKind::Less => BinaryOp::Lt,
        BinaryOpKind::Greater => BinaryOp::Lt, // Requires operand swap
        BinaryOpKind::LessEqual => BinaryOp::Lt, // Requires operand swap and not
        BinaryOpKind::GreaterEqual => BinaryOp::Lt, // Requires not
        BinaryOpKind::And => BinaryOp::And,
        BinaryOpKind::Or => BinaryOp::Or,
        BinaryOpKind::Xor => BinaryOp::Xor,
        BinaryOpKind::ShiftRight => BinaryOp::Shr,
        BinaryOpKind::ShiftLeft => BinaryOp::Shl,
    }
}

impl SharedContext {
    /// Create a new SharedContext for the given monomorphized program.
    pub(super) fn new(program: Program) -> Self {
        Self {
            functions: Default::default(),
            function_queue: Default::default(),
            function_counter: Default::default(),
            program,
        }
    }

    /// Pops the next function from the shared function queue, returning None if the queue is empty.
    pub(super) fn pop_next_function_in_queue(&self) -> Option<(ast::FuncId, IrFunctionId)> {
        self.function_queue.lock().expect("Failed to lock function_queue").pop()
    }

    /// Return the matching id for the given function if known. If it is not known this
    /// will add the function to the queue of functions to compile, assign it a new id,
    /// and return this new id.
    pub(super) fn get_or_queue_function(&self, id: ast::FuncId) -> IrFunctionId {
        // Start a new block to guarantee the destructor for the map lock is released
        // before map needs to be aquired again in self.functions.write() below
        {
            let map = self.functions.read().expect("Failed to read self.functions");
            if let Some(existing_id) = map.get(&id) {
                return *existing_id;
            }
        }

        let next_id = self.function_counter.next();

        let mut queue = self.function_queue.lock().expect("Failed to lock function queue");
        queue.push((id, next_id));

        self.functions.write().expect("Failed to write to self.functions").insert(id, next_id);

        next_id
    }
}

/// Used to remember the results of each step of extracting a value from an ast::LValue
#[derive(Debug)]
pub(super) enum LValue {
    Ident(Values),
    Index { old_array: ValueId, index: ValueId, array_lvalue: Box<LValue> },
    MemberAccess { old_object: Values, index: usize, object_lvalue: Box<LValue> },
    Dereference { reference: Values },
}
