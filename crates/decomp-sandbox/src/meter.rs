//! Rewrites a job module so that it counts its own instructions.
//!
//! Metering has to be identical on every node, so it cannot depend on the engine
//! that runs the module. Instead the module is rewritten before it runs: a counter
//! is subtracted from as the code executes, and when it runs out the module calls
//! the host, which is where compute caps are enforced and checkpoints are taken.
//! Any WebAssembly engine that runs the rewritten module gets the same count.
//!
//! What the rewrite adds:
//!
//! - a global holding the instructions left before the next call to the host,
//!   exported as [`COUNTER_EXPORT`] so the host can read the final value;
//! - an import, `decomp.meter`, called with the counter when it goes negative; it
//!   returns the counter's new value;
//! - at the start of every straight-line run of code, a subtraction of that run's
//!   length. Runs that begin a function or a loop body also check the counter,
//!   which is enough to bound how far past zero it can go: all unbounded
//!   execution passes through a function entry or a loop header;
//! - before each instruction whose work grows with an operand (copying or filling
//!   memory, growing memory or a table), a subtraction of that operand.

use std::convert::Infallible;

use wasm_encoder::reencode::{Error, Reencode};
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, DataCountSection, DataSection, ElementSection, EntityType,
    ExportKind, ExportSection, Function, FunctionSection, GlobalSection, GlobalType, ImportSection,
    Instruction, MemorySection, Module, StartSection, TableSection, TypeSection, ValType,
};
use wasmparser::{Operator, Parser, Payload, TypeRef};

/// The module the host functions are imported from.
pub const HOST_MODULE: &str = "decomp";
/// The host function a rewritten module calls when its counter runs out.
pub const METER_IMPORT: &str = "meter";
/// The name the counter is exported under.
pub const COUNTER_EXPORT: &str = "decomp.counter";

/// Instructions charged for each 64 KiB page a module adds to its memory.
const INSTRUCTIONS_PER_PAGE_GROWN: i64 = 65_536;

/// Why a module could not be rewritten.
#[derive(Debug, thiserror::Error)]
pub enum MeterError {
    #[error("{0}")]
    Malformed(#[from] wasmparser::BinaryReaderError),
    #[error("it uses a name the sandbox reserves: {0}")]
    ReservedName(&'static str),
    #[error("it uses a WebAssembly feature the sandbox does not support")]
    Unsupported,
}

impl From<Error<Infallible>> for MeterError {
    fn from(error: Error<Infallible>) -> Self {
        match error {
            Error::ParseError(error) => Self::Malformed(error),
            _ => Self::Unsupported,
        }
    }
}

/// What the rewrite needs to know about a module before it changes anything.
struct Layout {
    /// How many parameters each type takes, by type index.
    parameters: Vec<u32>,
    imported_functions: u32,
    /// The type index of each function the module defines, in order.
    defined_function_types: Vec<u32>,
    globals: u32,
}

impl Layout {
    fn of(module: &[u8]) -> Result<Self, MeterError> {
        let mut layout = Self {
            parameters: Vec::new(),
            imported_functions: 0,
            defined_function_types: Vec::new(),
            globals: 0,
        };
        for payload in Parser::new(0).parse_all(module) {
            match payload? {
                Payload::TypeSection(section) => {
                    for group in section {
                        for ty in group?.into_types() {
                            let parameters = match &ty.composite_type.inner {
                                wasmparser::CompositeInnerType::Func(function) => {
                                    function.params().len() as u32
                                }
                                _ => 0,
                            };
                            layout.parameters.push(parameters);
                        }
                    }
                }
                Payload::ImportSection(section) => {
                    for import in section.into_imports() {
                        let import = import?;
                        if import.module == HOST_MODULE && import.name == METER_IMPORT {
                            return Err(MeterError::ReservedName("decomp.meter"));
                        }
                        match import.ty {
                            TypeRef::Func(_) | TypeRef::FuncExact(_) => {
                                layout.imported_functions += 1
                            }
                            TypeRef::Global(_) => layout.globals += 1,
                            _ => {}
                        }
                    }
                }
                Payload::FunctionSection(section) => {
                    for type_index in section {
                        layout.defined_function_types.push(type_index?);
                    }
                }
                Payload::GlobalSection(section) => layout.globals += section.count(),
                Payload::ExportSection(section) => {
                    for export in section {
                        if export?.name == COUNTER_EXPORT {
                            return Err(MeterError::ReservedName(COUNTER_EXPORT));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(layout)
    }
}

/// Copies a module while moving function indexes up by one past the imports, to
/// make room for the meter import.
struct Shift {
    imported_functions: u32,
}

impl Reencode for Shift {
    type Error = Infallible;

    fn function_index(&mut self, function: u32) -> Result<u32, Error<Infallible>> {
        Ok(if function >= self.imported_functions {
            function + 1
        } else {
            function
        })
    }
}

/// Returns `module` rewritten to count its own instructions.
pub fn add_metering(module: &[u8]) -> Result<Vec<u8>, MeterError> {
    let layout = Layout::of(module)?;
    let meter_type = layout.parameters.len() as u32;
    let meter = layout.imported_functions;
    let counter = layout.globals;
    let mut shift = Shift {
        imported_functions: layout.imported_functions,
    };

    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();
    let mut functions = FunctionSection::new();
    let mut tables = TableSection::new();
    let mut memories = MemorySection::new();
    let mut globals = GlobalSection::new();
    let mut exports = ExportSection::new();
    let mut start = None;
    let mut elements = ElementSection::new();
    let mut data_count = None;
    let mut code = CodeSection::new();
    let mut data = DataSection::new();
    let mut defined_function = 0;

    for payload in Parser::new(0).parse_all(module) {
        match payload? {
            Payload::TypeSection(section) => shift.parse_type_section(&mut types, section)?,
            Payload::ImportSection(section) => shift.parse_import_section(&mut imports, section)?,
            Payload::FunctionSection(section) => {
                shift.parse_function_section(&mut functions, section)?
            }
            Payload::TableSection(section) => shift.parse_table_section(&mut tables, section)?,
            Payload::MemorySection(section) => {
                shift.parse_memory_section(&mut memories, section)?
            }
            Payload::GlobalSection(section) => shift.parse_global_section(&mut globals, section)?,
            Payload::ExportSection(section) => shift.parse_export_section(&mut exports, section)?,
            Payload::StartSection { func, .. } => start = Some(shift.function_index(func)?),
            Payload::ElementSection(section) => {
                shift.parse_element_section(&mut elements, section)?
            }
            Payload::DataCountSection { count, .. } => data_count = Some(count),
            Payload::CodeSectionEntry(body) => {
                let type_index = layout.defined_function_types[defined_function];
                let parameters = layout.parameters[type_index as usize];
                defined_function += 1;
                code.function(&metered(&mut shift, body, parameters, counter, meter)?);
            }
            Payload::DataSection(section) => shift.parse_data_section(&mut data, section)?,
            // Custom sections only carry names and debug data, which the shifted
            // function indexes would make wrong.
            Payload::CustomSection(_)
            | Payload::Version { .. }
            | Payload::CodeSectionStart { .. }
            | Payload::End(_) => {}
            _ => return Err(MeterError::Unsupported),
        }
    }

    types.ty().function([ValType::I64], [ValType::I64]);
    imports.import(HOST_MODULE, METER_IMPORT, EntityType::Function(meter_type));
    let counter_type = GlobalType {
        val_type: ValType::I64,
        mutable: true,
        shared: false,
    };
    globals.global(counter_type, &ConstExpr::i64_const(0));
    exports.export(COUNTER_EXPORT, ExportKind::Global, counter);

    let mut rewritten = Module::new();
    rewritten
        .section(&types)
        .section(&imports)
        .section(&functions);
    if !tables.is_empty() {
        rewritten.section(&tables);
    }
    if !memories.is_empty() {
        rewritten.section(&memories);
    }
    rewritten.section(&globals).section(&exports);
    if let Some(function_index) = start {
        rewritten.section(&StartSection { function_index });
    }
    if !elements.is_empty() {
        rewritten.section(&elements);
    }
    if let Some(count) = data_count {
        rewritten.section(&DataCountSection { count });
    }
    rewritten.section(&code);
    if !data.is_empty() {
        rewritten.section(&data);
    }
    Ok(rewritten.finish())
}

/// One function body with the counting added.
fn metered(
    shift: &mut Shift,
    body: wasmparser::FunctionBody<'_>,
    parameters: u32,
    counter: u32,
    meter: u32,
) -> Result<Function, MeterError> {
    let mut locals = Vec::new();
    let mut declared = 0;
    for local in body.get_locals_reader()? {
        let (count, ty) = local?;
        declared += count;
        locals.push((count, shift.val_type(ty)?));
    }
    // One more local, to hold an operand while it is being charged for.
    let scratch = parameters + declared;
    locals.push((1, ValType::I32));
    let mut function = Function::new(locals);

    // A run is straight-line code: once its first instruction executes, they all
    // do. It is charged for in one subtraction placed before it.
    let mut run: Vec<Instruction<'_>> = Vec::new();
    let mut run_length: i64 = 0;
    let mut run_is_checked = true;
    let mut reader = body.get_operators_reader()?;
    while !reader.eof() {
        let operator = reader.read()?;
        let ends_run = ends_run(&operator);
        let begins_loop = matches!(operator, Operator::Loop { .. });
        if let Some(per_unit) = work_per_unit_of_operand(&operator) {
            charge_for_operand(&mut run, counter, scratch, per_unit);
        }
        run.push(shift.instruction(operator)?);
        run_length += 1;

        if ends_run {
            charge_for_run(&mut function, counter, meter, run_length, run_is_checked);
            for instruction in run.drain(..) {
                function.instruction(&instruction);
            }
            run_length = 0;
            run_is_checked = begins_loop;
        }
    }
    // A function body always ends with `end`, so nothing is left in the run.
    Ok(function)
}

/// Whether the code after `operator` can be reached without running the code
/// before it, or the code before it can run without the code after.
fn ends_run(operator: &Operator<'_>) -> bool {
    matches!(
        operator,
        Operator::Block { .. }
            | Operator::Loop { .. }
            | Operator::If { .. }
            | Operator::Else
            | Operator::End
            | Operator::Br { .. }
            | Operator::BrIf { .. }
            | Operator::BrTable { .. }
            | Operator::Return
            | Operator::Unreachable
    )
}

/// For an instruction whose work grows with the operand on top of the stack, the
/// instructions to charge per unit of that operand.
fn work_per_unit_of_operand(operator: &Operator<'_>) -> Option<i64> {
    match operator {
        Operator::MemoryCopy { .. }
        | Operator::MemoryFill { .. }
        | Operator::MemoryInit { .. }
        | Operator::TableCopy { .. }
        | Operator::TableFill { .. }
        | Operator::TableInit { .. }
        | Operator::TableGrow { .. } => Some(1),
        Operator::MemoryGrow { .. } => Some(INSTRUCTIONS_PER_PAGE_GROWN),
        _ => None,
    }
}

fn charge_for_run(function: &mut Function, counter: u32, meter: u32, length: i64, checked: bool) {
    function
        .instruction(&Instruction::GlobalGet(counter))
        .instruction(&Instruction::I64Const(length))
        .instruction(&Instruction::I64Sub)
        .instruction(&Instruction::GlobalSet(counter));
    if checked {
        function
            .instruction(&Instruction::GlobalGet(counter))
            .instruction(&Instruction::I64Const(0))
            .instruction(&Instruction::I64LtS)
            .instruction(&Instruction::If(BlockType::Empty))
            .instruction(&Instruction::GlobalGet(counter))
            .instruction(&Instruction::Call(meter))
            .instruction(&Instruction::GlobalSet(counter))
            .instruction(&Instruction::End);
    }
}

/// Subtracts the 32-bit operand on top of the stack, times `per_unit`, from the
/// counter, leaving the operand where it was.
fn charge_for_operand(run: &mut Vec<Instruction<'_>>, counter: u32, scratch: u32, per_unit: i64) {
    run.extend([
        Instruction::LocalSet(scratch),
        Instruction::GlobalGet(counter),
        Instruction::LocalGet(scratch),
        Instruction::I64ExtendI32U,
        Instruction::I64Const(per_unit),
        Instruction::I64Mul,
        Instruction::I64Sub,
        Instruction::GlobalSet(counter),
        Instruction::LocalGet(scratch),
    ]);
}
