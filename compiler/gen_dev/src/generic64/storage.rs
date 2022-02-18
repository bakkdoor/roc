use crate::{
    generic64::{Assembler, CallConv, RegTrait},
    single_register_floats, single_register_integers, single_register_layouts, Env,
};
use bumpalo::collections::Vec;
use roc_builtins::bitcode::{FloatWidth, IntWidth};
use roc_collections::all::{MutMap, MutSet};
use roc_error_macros::internal_error;
use roc_module::symbol::Symbol;
use roc_mono::layout::{Builtin, Layout};
use roc_target::TargetInfo;
use std::cmp::max;
use std::marker::PhantomData;
use std::rc::Rc;

use RegStorage::*;
use StackStorage::*;
use Storage::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RegStorage<GeneralReg: RegTrait, FloatReg: RegTrait> {
    General(GeneralReg),
    Float(FloatReg),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum StackStorage<GeneralReg: RegTrait, FloatReg: RegTrait> {
    // Primitives are 8 bytes or less. That generally live in registers but can move stored on the stack.
    // Their data must always be 8 byte aligned and will be moved as a block.
    // They are never part of a struct, union, or more complex value.
    // The rest of the bytes should be zero due to how these are loaded.
    Primitive {
        // Offset from the base pointer in bytes.
        base_offset: i32,
        // Optional register also holding the value.
        reg: Option<RegStorage<GeneralReg, FloatReg>>,
    },
    // Referenced Primitives are primitives within a complex structure.
    // They have no guarentees about alignment or zeroed bits.
    // When they are loaded, they should be aligned and zeroed.
    // After loading, they should just be stored in a register.
    ReferencedPrimitive {
        // Offset from the base pointer in bytes.
        base_offset: i32,
        // Size on the stack in bytes.
        size: u32,
    },
    // Complex data (lists, unions, structs, str) stored on the stack.
    // Note, this is also used for referencing a value within a struct/union.
    // It has no alignment guarantees.
    // When a primitive value is being loaded from this, it should be moved into a register.
    // To start, the primitive can just be loaded as a ReferencePrimitive.
    Complex {
        // Offset from the base pointer in bytes.
        base_offset: i32,
        // Size on the stack in bytes.
        size: u32,
        // TODO: investigate if storing a reg here for special values is worth it.
        // For example, the ptr in list.get/list.set
        // Instead, it would probably be better to change the incoming IR to load the pointer once and then use it multiple times.
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Storage<GeneralReg: RegTrait, FloatReg: RegTrait> {
    Reg(RegStorage<GeneralReg, FloatReg>),
    Stack(StackStorage<GeneralReg, FloatReg>),
    NoData,
}

pub struct StorageManager<
    'a,
    GeneralReg: RegTrait,
    FloatReg: RegTrait,
    ASM: Assembler<GeneralReg, FloatReg>,
    CC: CallConv<GeneralReg, FloatReg, ASM>,
> {
    phantom_cc: PhantomData<CC>,
    phantom_asm: PhantomData<ASM>,
    env: &'a Env<'a>,
    target_info: TargetInfo,
    // Data about where each symbol is stored.
    symbol_storage_map: MutMap<Symbol, Storage<GeneralReg, FloatReg>>,

    // A map from symbol to its owning allocation.
    // This is only used for complex data on the stack and its references.
    // In the case that subdata is still referenced from an overall structure,
    // We can't free the entire structure until the subdata is no longer needed.
    // If a symbol has only one reference, we can free it.
    allocation_map: MutMap<Symbol, Rc<(i32, u32)>>,

    // This should probably be smarter than a vec.
    // There are certain registers we should always use first. With pushing and popping, this could get mixed.
    general_free_regs: Vec<'a, GeneralReg>,
    float_free_regs: Vec<'a, FloatReg>,

    // The last major thing we need is a way to decide what reg to free when all of them are full.
    // Theoretically we want a basic lru cache for the currently loaded symbols.
    // For now just a vec of used registers and the symbols they contain.
    general_used_regs: Vec<'a, (GeneralReg, Symbol)>,
    float_used_regs: Vec<'a, (FloatReg, Symbol)>,

    // TODO: it probably would be faster to make these a list that linearly scans rather than hashing.
    // used callee saved regs must be tracked for pushing and popping at the beginning/end of the function.
    general_used_callee_saved_regs: MutSet<GeneralReg>,
    float_used_callee_saved_regs: MutSet<FloatReg>,

    free_stack_chunks: Vec<'a, (i32, u32)>,
    stack_size: u32,

    // The amount of extra stack space needed to pass args for function calling.
    fn_call_stack_size: u32,
}

pub fn new_storage_manager<
    'a,
    GeneralReg: RegTrait,
    FloatReg: RegTrait,
    ASM: Assembler<GeneralReg, FloatReg>,
    CC: CallConv<GeneralReg, FloatReg, ASM>,
>(
    env: &'a Env,
    target_info: TargetInfo,
) -> StorageManager<'a, GeneralReg, FloatReg, ASM, CC> {
    StorageManager {
        phantom_asm: PhantomData,
        phantom_cc: PhantomData,
        env,
        target_info,
        symbol_storage_map: MutMap::default(),
        allocation_map: MutMap::default(),
        general_free_regs: bumpalo::vec![in env.arena],
        general_used_regs: bumpalo::vec![in env.arena],
        general_used_callee_saved_regs: MutSet::default(),
        float_free_regs: bumpalo::vec![in env.arena],
        float_used_regs: bumpalo::vec![in env.arena],
        float_used_callee_saved_regs: MutSet::default(),
        free_stack_chunks: bumpalo::vec![in env.arena],
        stack_size: 0,
        fn_call_stack_size: 0,
    }
}

impl<
        'a,
        FloatReg: RegTrait,
        GeneralReg: RegTrait,
        ASM: Assembler<GeneralReg, FloatReg>,
        CC: CallConv<GeneralReg, FloatReg, ASM>,
    > StorageManager<'a, GeneralReg, FloatReg, ASM, CC>
{
    pub fn reset(&mut self) {
        self.symbol_storage_map.clear();
        self.allocation_map.clear();
        self.general_used_callee_saved_regs.clear();
        self.general_free_regs.clear();
        self.general_used_regs.clear();
        self.general_free_regs
            .extend_from_slice(CC::GENERAL_DEFAULT_FREE_REGS);
        self.float_used_callee_saved_regs.clear();
        self.float_free_regs.clear();
        self.float_used_regs.clear();
        self.float_free_regs
            .extend_from_slice(CC::FLOAT_DEFAULT_FREE_REGS);
        self.free_stack_chunks.clear();
        self.stack_size = 0;
        self.fn_call_stack_size = 0;
    }

    // Returns true if the symbol is storing a primitive value.
    pub fn is_stored_primitive(&self, sym: &Symbol) -> bool {
        matches!(
            self.get_storage_for_sym(sym),
            Reg(_) | Stack(Primitive { .. } | ReferencedPrimitive { .. })
        )
    }

    // Get a general register from the free list.
    // Will free data to the stack if necessary to get the register.
    fn get_general_reg(&mut self, buf: &mut Vec<'a, u8>) -> GeneralReg {
        if let Some(reg) = self.general_free_regs.pop() {
            if CC::general_callee_saved(&reg) {
                self.general_used_callee_saved_regs.insert(reg);
            }
            reg
        } else if !self.general_used_regs.is_empty() {
            let (reg, sym) = self.general_used_regs.remove(0);
            self.free_to_stack(buf, &sym, General(reg));
            reg
        } else {
            internal_error!("completely out of general purpose registers");
        }
    }

    // Get a float register from the free list.
    // Will free data to the stack if necessary to get the register.
    fn get_float_reg(&mut self, buf: &mut Vec<'a, u8>) -> FloatReg {
        if let Some(reg) = self.float_free_regs.pop() {
            if CC::float_callee_saved(&reg) {
                self.float_used_callee_saved_regs.insert(reg);
            }
            reg
        } else if !self.float_used_regs.is_empty() {
            let (reg, sym) = self.float_used_regs.remove(0);
            self.free_to_stack(buf, &sym, Float(reg));
            reg
        } else {
            internal_error!("completely out of general purpose registers");
        }
    }

    // Claims a general reg for a specific symbol.
    // They symbol should not already have storage.
    pub fn claim_general_reg(&mut self, buf: &mut Vec<'a, u8>, sym: &Symbol) -> GeneralReg {
        debug_assert_eq!(self.symbol_storage_map.get(sym), None);
        let reg = self.get_general_reg(buf);
        self.general_used_regs.push((reg, *sym));
        self.symbol_storage_map.insert(*sym, Reg(General(reg)));
        reg
    }

    // Claims a float reg for a specific symbol.
    // They symbol should not already have storage.
    pub fn claim_float_reg(&mut self, buf: &mut Vec<'a, u8>, sym: &Symbol) -> FloatReg {
        debug_assert_eq!(self.symbol_storage_map.get(sym), None);
        let reg = self.get_float_reg(buf);
        self.float_used_regs.push((reg, *sym));
        self.symbol_storage_map.insert(*sym, Reg(Float(reg)));
        reg
    }

    // This claims a temporary general register and enables is used in the passed in function.
    // Temporary registers are not safe across call instructions.
    pub fn with_tmp_general_reg<F: FnOnce(&mut Self, &mut Vec<'a, u8>, GeneralReg)>(
        &mut self,
        buf: &mut Vec<'a, u8>,
        callback: F,
    ) {
        let reg = self.get_general_reg(buf);
        callback(self, buf, reg);
        self.general_free_regs.push(reg);
    }

    // This claims a temporary float register and enables is used in the passed in function.
    // Temporary registers are not safe across call instructions.
    pub fn with_tmp_float_reg<F: FnOnce(&mut Self, &mut Vec<'a, u8>, FloatReg)>(
        &mut self,
        buf: &mut Vec<'a, u8>,
        callback: F,
    ) {
        let reg = self.get_float_reg(buf);
        callback(self, buf, reg);
        self.float_free_regs.push(reg);
    }

    // Loads a symbol into a general reg and returns that register.
    // The symbol must already be stored somewhere.
    // Will fail on values stored in float regs.
    // Will fail for values that don't fit in a single register.
    pub fn load_to_general_reg(&mut self, buf: &mut Vec<'a, u8>, sym: &Symbol) -> GeneralReg {
        let storage = self.remove_storage_for_sym(sym);
        match storage {
            Reg(General(reg))
            | Stack(Primitive {
                reg: Some(General(reg)),
                ..
            }) => {
                self.symbol_storage_map.insert(*sym, storage);
                reg
            }
            Reg(Float(_))
            | Stack(Primitive {
                reg: Some(Float(_)),
                ..
            }) => {
                internal_error!("Cannot load floating point symbol into GeneralReg: {}", sym)
            }
            Stack(Primitive {
                reg: None,
                base_offset,
            }) => {
                debug_assert_eq!(base_offset % 8, 0);
                let reg = self.get_general_reg(buf);
                ASM::mov_reg64_base32(buf, reg, base_offset);
                self.general_used_regs.push((reg, *sym));
                self.symbol_storage_map.insert(
                    *sym,
                    Stack(Primitive {
                        base_offset,
                        reg: Some(General(reg)),
                    }),
                );
                reg
            }
            Stack(ReferencedPrimitive { base_offset, size })
                if base_offset % 8 == 0 && size == 8 =>
            {
                // The primitive is aligned and the data is exactly 8 bytes, treat it like regular stack.
                let reg = self.get_general_reg(buf);
                ASM::mov_reg64_base32(buf, reg, base_offset);
                self.general_used_regs.push((reg, *sym));
                self.symbol_storage_map.insert(*sym, Reg(General(reg)));
                self.free_reference(sym);
                reg
            }
            Stack(ReferencedPrimitive { .. }) => {
                todo!("loading referenced primitives")
            }
            Stack(Complex { .. }) => {
                internal_error!("Cannot load large values into general registers: {}", sym)
            }
            NoData => {
                internal_error!("Cannot load no data into general registers: {}", sym)
            }
        }
    }

    // Loads a symbol into a float reg and returns that register.
    // The symbol must already be stored somewhere.
    // Will fail on values stored in general regs.
    // Will fail for values that don't fit in a single register.
    pub fn load_to_float_reg(&mut self, buf: &mut Vec<'a, u8>, sym: &Symbol) -> FloatReg {
        let storage = self.remove_storage_for_sym(sym);
        match storage {
            Reg(Float(reg))
            | Stack(Primitive {
                reg: Some(Float(reg)),
                ..
            }) => {
                self.symbol_storage_map.insert(*sym, storage);
                reg
            }
            Reg(General(_))
            | Stack(Primitive {
                reg: Some(General(_)),
                ..
            }) => {
                internal_error!("Cannot load general symbol into FloatReg: {}", sym)
            }
            Stack(Primitive {
                reg: None,
                base_offset,
            }) => {
                debug_assert_eq!(base_offset % 8, 0);
                let reg = self.get_float_reg(buf);
                ASM::mov_freg64_base32(buf, reg, base_offset);
                self.float_used_regs.push((reg, *sym));
                self.symbol_storage_map.insert(
                    *sym,
                    Stack(Primitive {
                        base_offset,
                        reg: Some(Float(reg)),
                    }),
                );
                reg
            }
            Stack(ReferencedPrimitive { base_offset, size })
                if base_offset % 8 == 0 && size == 8 =>
            {
                // The primitive is aligned and the data is exactly 8 bytes, treat it like regular stack.
                let reg = self.get_float_reg(buf);
                ASM::mov_freg64_base32(buf, reg, base_offset);
                self.float_used_regs.push((reg, *sym));
                self.symbol_storage_map.insert(*sym, Reg(Float(reg)));
                self.free_reference(sym);
                reg
            }
            Stack(ReferencedPrimitive { .. }) => {
                todo!("loading referenced primitives")
            }
            Stack(Complex { .. }) => {
                internal_error!("Cannot load large values into float registers: {}", sym)
            }
            NoData => {
                internal_error!("Cannot load no data into general registers: {}", sym)
            }
        }
    }

    // Loads the symbol to the specified register.
    // It will fail if the symbol is stored in a float register.
    // This is only made to be used in special cases where exact regs are needed (function args and returns).
    // It will not try to free the register first.
    // This will not track the symbol change (it makes no assumptions about the new reg).
    pub fn load_to_specified_general_reg(
        &self,
        buf: &mut Vec<'a, u8>,
        sym: &Symbol,
        reg: GeneralReg,
    ) {
        match self.get_storage_for_sym(sym) {
            Reg(General(old_reg))
            | Stack(Primitive {
                reg: Some(General(old_reg)),
                ..
            }) => {
                if *old_reg == reg {
                    return;
                }
                ASM::mov_reg64_reg64(buf, reg, *old_reg);
            }
            Reg(Float(_))
            | Stack(Primitive {
                reg: Some(Float(_)),
                ..
            }) => {
                internal_error!("Cannot load floating point symbol into GeneralReg: {}", sym)
            }
            Stack(Primitive {
                reg: None,
                base_offset,
            }) => {
                debug_assert_eq!(base_offset % 8, 0);
                ASM::mov_reg64_base32(buf, reg, *base_offset);
            }
            Stack(ReferencedPrimitive { base_offset, size })
                if base_offset % 8 == 0 && *size == 8 =>
            {
                // The primitive is aligned and the data is exactly 8 bytes, treat it like regular stack.
                ASM::mov_reg64_base32(buf, reg, *base_offset);
            }
            Stack(ReferencedPrimitive { .. }) => {
                todo!("loading referenced primitives")
            }
            Stack(Complex { .. }) => {
                internal_error!("Cannot load large values into general registers: {}", sym)
            }
            NoData => {
                internal_error!("Cannot load no data into general registers: {}", sym)
            }
        }
    }

    // Loads the symbol to the specified register.
    // It will fail if the symbol is stored in a general register.
    // This is only made to be used in special cases where exact regs are needed (function args and returns).
    // It will not try to free the register first.
    // This will not track the symbol change (it makes no assumptions about the new reg).
    pub fn load_to_specified_float_reg(&self, buf: &mut Vec<'a, u8>, sym: &Symbol, reg: FloatReg) {
        match self.get_storage_for_sym(sym) {
            Reg(Float(old_reg))
            | Stack(Primitive {
                reg: Some(Float(old_reg)),
                ..
            }) => {
                if *old_reg == reg {
                    return;
                }
                ASM::mov_freg64_freg64(buf, reg, *old_reg);
            }
            Reg(General(_))
            | Stack(Primitive {
                reg: Some(General(_)),
                ..
            }) => {
                internal_error!("Cannot load general symbol into FloatReg: {}", sym)
            }
            Stack(Primitive {
                reg: None,
                base_offset,
            }) => {
                debug_assert_eq!(base_offset % 8, 0);
                ASM::mov_freg64_base32(buf, reg, *base_offset);
            }
            Stack(ReferencedPrimitive { base_offset, size })
                if base_offset % 8 == 0 && *size == 8 =>
            {
                // The primitive is aligned and the data is exactly 8 bytes, treat it like regular stack.
                ASM::mov_freg64_base32(buf, reg, *base_offset);
            }
            Stack(ReferencedPrimitive { .. }) => {
                todo!("loading referenced primitives")
            }
            Stack(Complex { .. }) => {
                internal_error!("Cannot load large values into float registers: {}", sym)
            }
            NoData => {
                internal_error!("Cannot load no data into general registers: {}", sym)
            }
        }
    }

    // Loads a field from a struct or tag union.
    // This is lazy by default. It will not copy anything around.
    pub fn load_field_at_index(
        &mut self,
        sym: &Symbol,
        structure: &Symbol,
        index: u64,
        field_layouts: &'a [Layout<'a>],
    ) {
        debug_assert!(index < field_layouts.len() as u64);
        // This must be removed and reinserted for ownership and mutability reasons.
        let owned_data = if let Some(owned_data) = self.allocation_map.remove(structure) {
            owned_data
        } else {
            internal_error!("Unknown symbol: {}", structure);
        };
        self.allocation_map
            .insert(*structure, Rc::clone(&owned_data));
        match self.get_storage_for_sym(structure) {
            Stack(Complex { base_offset, size }) => {
                let (base_offset, size) = (*base_offset, *size);
                let mut data_offset = base_offset;
                for layout in field_layouts.iter().take(index as usize) {
                    let field_size = layout.stack_size(self.target_info);
                    data_offset += field_size as i32;
                }
                debug_assert!(data_offset < base_offset + size as i32);
                self.allocation_map.insert(*sym, owned_data);
                let layout = field_layouts[index as usize];
                let size = layout.stack_size(self.target_info);
                self.symbol_storage_map.insert(
                    *sym,
                    Stack(if is_primitive(&layout) {
                        ReferencedPrimitive {
                            base_offset: data_offset,
                            size,
                        }
                    } else {
                        Complex {
                            base_offset: data_offset,
                            size,
                        }
                    }),
                );
            }
            storage => {
                internal_error!(
                    "Cannot load field from data with storage type: {:?}",
                    storage
                );
            }
        }
    }

    // Creates a struct on the stack, moving the data in fields into the struct.
    pub fn create_struct(
        &mut self,
        buf: &mut Vec<'a, u8>,
        sym: &Symbol,
        layout: &Layout<'a>,
        fields: &'a [Symbol],
    ) {
        let struct_size = layout.stack_size(self.target_info);
        if struct_size == 0 {
            self.symbol_storage_map.insert(*sym, NoData);
            return;
        }
        let base_offset = self.claim_stack_area(sym, struct_size);

        if let Layout::Struct(field_layouts) = layout {
            let mut current_offset = base_offset;
            for (field, field_layout) in fields.iter().zip(field_layouts.iter()) {
                self.copy_symbol_to_stack_offset(buf, current_offset, field, field_layout);
                let field_size = field_layout.stack_size(self.target_info);
                current_offset += field_size as i32;
            }
        } else {
            // This is a single element struct. Just copy the single field to the stack.
            debug_assert_eq!(fields.len(), 1);
            self.copy_symbol_to_stack_offset(buf, base_offset, &fields[0], layout);
        }
    }

    // Copies a symbol to the specified stack offset. This is used for things like filling structs.
    // The offset is not guarenteed to be perfectly aligned, it follows Roc's alignment plan.
    // This means that, for example 2 I32s might be back to back on the stack.
    // Always interact with the stack using aligned 64bit movement.
    fn copy_symbol_to_stack_offset(
        &mut self,
        buf: &mut Vec<'a, u8>,
        to_offset: i32,
        sym: &Symbol,
        layout: &Layout<'a>,
    ) {
        match layout {
            Layout::Builtin(Builtin::Int(IntWidth::I64 | IntWidth::U64)) => {
                debug_assert_eq!(to_offset % 8, 0);
                let reg = self.load_to_general_reg(buf, sym);
                ASM::mov_base32_reg64(buf, to_offset, reg);
            }
            Layout::Builtin(Builtin::Float(FloatWidth::F64)) => {
                debug_assert_eq!(to_offset % 8, 0);
                let reg = self.load_to_float_reg(buf, sym);
                ASM::mov_base32_freg64(buf, to_offset, reg);
            }
            // Layout::Struct(_) if layout.safe_to_memcpy() => {
            //     // self.storage_manager.with_tmp_float_reg(&mut self.buf, |buf, storage, )
            //     // if let Some(SymbolStorage::Base {
            //     //     offset: from_offset,
            //     //     size,
            //     //     ..
            //     // }) = self.symbol_storage_map.get(sym)
            //     // {
            //     //     debug_assert_eq!(
            //     //         *size,
            //     //         layout.stack_size(self.target_info),
            //     //         "expected struct to have same size as data being stored in it"
            //     //     );
            //     //     for i in 0..layout.stack_size(self.target_info) as i32 {
            //     //         ASM::mov_reg64_base32(&mut self.buf, tmp_reg, from_offset + i);
            //     //         ASM::mov_base32_reg64(&mut self.buf, to_offset + i, tmp_reg);
            //     //     }
            //     todo!()
            //     } else {
            //         internal_error!("unknown struct: {:?}", sym);
            //     }
            // }
            x => todo!("copying data to the stack with layout, {:?}", x),
        }
    }

    // Frees `wanted_reg` which is currently owned by `sym` by making sure the value is loaded on the stack.
    fn free_to_stack(
        &mut self,
        buf: &mut Vec<'a, u8>,
        sym: &Symbol,
        wanted_reg: RegStorage<GeneralReg, FloatReg>,
    ) {
        match self.remove_storage_for_sym(sym) {
            Reg(reg_storage) => {
                debug_assert_eq!(reg_storage, wanted_reg);
                let base_offset = self.claim_stack_size(8);
                match reg_storage {
                    General(reg) => ASM::mov_base32_reg64(buf, base_offset, reg),
                    Float(reg) => ASM::mov_base32_freg64(buf, base_offset, reg),
                }
                self.symbol_storage_map.insert(
                    *sym,
                    Stack(Primitive {
                        base_offset,
                        reg: None,
                    }),
                );
            }
            Stack(Primitive {
                reg: Some(reg_storage),
                base_offset,
            }) => {
                debug_assert_eq!(reg_storage, wanted_reg);
                self.symbol_storage_map.insert(
                    *sym,
                    Stack(Primitive {
                        base_offset,
                        reg: None,
                    }),
                );
            }
            NoData
            | Stack(Complex { .. } | Primitive { reg: None, .. } | ReferencedPrimitive { .. }) => {
                internal_error!("Cannot free reg from symbol without a reg: {}", sym)
            }
        }
    }

    // gets the stack offset and size of the specified symbol.
    // the symbol must already be stored on the stack.
    pub fn stack_offset_and_size(&self, sym: &Symbol) -> (i32, u32) {
        match self.get_storage_for_sym(sym) {
            Stack(Primitive { base_offset, .. }) => (*base_offset, 8),
            Stack(ReferencedPrimitive { base_offset, size } | Complex { base_offset, size }) => {
                (*base_offset, *size)
            }
            storage => {
                internal_error!(
                    "Data not on the stack for sym ({}) with storage ({:?})",
                    sym,
                    storage
                )
            }
        }
    }

    // Specifies a symbol is loaded at the specified general register.
    pub fn general_reg_arg(&mut self, sym: &Symbol, reg: GeneralReg) {
        self.symbol_storage_map.insert(*sym, Reg(General(reg)));
        self.general_free_regs.retain(|r| *r != reg);
        self.general_used_regs.push((reg, *sym));
    }

    // Specifies a symbol is loaded at the specified float register.
    pub fn float_reg_arg(&mut self, sym: &Symbol, reg: FloatReg) {
        self.symbol_storage_map.insert(*sym, Reg(Float(reg)));
        self.float_free_regs.retain(|r| *r != reg);
        self.float_used_regs.push((reg, *sym));
    }

    // Specifies a primitive is loaded at the specific base offset.
    pub fn primitive_stack_arg(&mut self, sym: &Symbol, base_offset: i32) {
        self.symbol_storage_map.insert(
            *sym,
            Stack(Primitive {
                base_offset,
                reg: None,
            }),
        );
    }

    // Loads the arg pointer symbol to the specified general reg.
    pub fn ret_pionter_arg(&mut self, reg: GeneralReg) {
        self.symbol_storage_map
            .insert(Symbol::RET_POINTER, Reg(General(reg)));
    }

    // updates the function call stack size to the max of its current value and the size need for this call.
    pub fn update_fn_call_stack_size(&mut self, tmp_size: u32) {
        self.fn_call_stack_size = max(self.fn_call_stack_size, tmp_size);
    }

    /// claim_stack_area is the public wrapper around claim_stack_size.
    /// It also deals with updating symbol storage.
    /// It returns the base offset of the stack area.
    /// It should only be used for complex data and not primitives.
    pub fn claim_stack_area(&mut self, sym: &Symbol, size: u32) -> i32 {
        let base_offset = self.claim_stack_size(size);
        self.symbol_storage_map
            .insert(*sym, Stack(Complex { base_offset, size }));
        self.allocation_map
            .insert(*sym, Rc::new((base_offset, size)));
        base_offset
    }

    /// claim_stack_size claims `amount` bytes from the stack alignind to 8.
    /// This may be free space in the stack or result in increasing the stack size.
    /// It returns base pointer relative offset of the new data.
    fn claim_stack_size(&mut self, amount: u32) -> i32 {
        debug_assert!(amount > 0);
        // round value to 8 byte alignment.
        let amount = if amount % 8 != 0 {
            amount + 8 - (amount % 8)
        } else {
            amount
        };
        if let Some(fitting_chunk) = self
            .free_stack_chunks
            .iter()
            .enumerate()
            .filter(|(_, (_, size))| *size >= amount)
            .min_by_key(|(_, (_, size))| size)
        {
            let (pos, (offset, size)) = fitting_chunk;
            let (offset, size) = (*offset, *size);
            if size == amount {
                self.free_stack_chunks.remove(pos);
                offset
            } else {
                let (prev_offset, prev_size) = self.free_stack_chunks[pos];
                self.free_stack_chunks[pos] = (prev_offset + amount as i32, prev_size - amount);
                prev_offset
            }
        } else if let Some(new_size) = self.stack_size.checked_add(amount) {
            // Since stack size is u32, but the max offset is i32, if we pass i32 max, we have overflowed.
            if new_size > i32::MAX as u32 {
                internal_error!("Ran out of stack space");
            } else {
                self.stack_size = new_size;
                -(self.stack_size as i32)
            }
        } else {
            internal_error!("Ran out of stack space");
        }
    }

    pub fn free_symbol(&mut self, sym: &Symbol) {
        match self.remove_storage_for_sym(sym) {
            // Free stack chunck if this is the last reference to the chunk.
            Stack(Primitive { base_offset, .. }) => {
                self.free_stack_chunk(base_offset, 8);
            }
            Stack(Complex { .. } | ReferencedPrimitive { .. }) => {
                self.free_reference(sym);
            }
            _ => {}
        }
        for i in 0..self.general_used_regs.len() {
            let (reg, saved_sym) = self.general_used_regs[i];
            if saved_sym == *sym {
                self.general_free_regs.push(reg);
                self.general_used_regs.remove(i);
                break;
            }
        }
        for i in 0..self.float_used_regs.len() {
            let (reg, saved_sym) = self.float_used_regs[i];
            if saved_sym == *sym {
                self.float_free_regs.push(reg);
                self.float_used_regs.remove(i);
                break;
            }
        }
    }

    // Frees an reference and release an allocation if it is no longer used.
    fn free_reference(&mut self, sym: &Symbol) {
        let owned_data = if let Some(owned_data) = self.allocation_map.remove(sym) {
            owned_data
        } else {
            internal_error!("Unknown symbol: {}", sym);
        };
        if Rc::strong_count(&owned_data) == 1 {
            self.free_stack_chunk(owned_data.0, owned_data.1);
        }
    }

    fn free_stack_chunk(&mut self, base_offset: i32, size: u32) {
        let loc = (base_offset, size);
        // Note: this position current points to the offset following the specified location.
        // If loc was inserted at this position, it would shift the data at this position over by 1.
        let pos = self
            .free_stack_chunks
            .binary_search(&loc)
            .unwrap_or_else(|e| e);

        // Check for overlap with previous and next free chunk.
        let merge_with_prev = if pos > 0 {
            if let Some((prev_offset, prev_size)) = self.free_stack_chunks.get(pos - 1) {
                let prev_end = *prev_offset + *prev_size as i32;
                if prev_end > base_offset {
                    internal_error!("Double free? A previously freed stack location overlaps with the currently freed stack location.");
                }
                prev_end == base_offset
            } else {
                false
            }
        } else {
            false
        };
        let merge_with_next = if let Some((next_offset, _)) = self.free_stack_chunks.get(pos) {
            let current_end = base_offset + size as i32;
            if current_end > *next_offset {
                internal_error!("Double free? A previously freed stack location overlaps with the currently freed stack location.");
            }
            current_end == *next_offset
        } else {
            false
        };

        match (merge_with_prev, merge_with_next) {
            (true, true) => {
                let (prev_offset, prev_size) = self.free_stack_chunks[pos - 1];
                let (_, next_size) = self.free_stack_chunks[pos];
                self.free_stack_chunks[pos - 1] = (prev_offset, prev_size + size + next_size);
                self.free_stack_chunks.remove(pos);
            }
            (true, false) => {
                let (prev_offset, prev_size) = self.free_stack_chunks[pos - 1];
                self.free_stack_chunks[pos - 1] = (prev_offset, prev_size + size);
            }
            (false, true) => {
                let (_, next_size) = self.free_stack_chunks[pos];
                self.free_stack_chunks[pos] = (base_offset, next_size + size);
            }
            (false, false) => self.free_stack_chunks.insert(pos, loc),
        }
    }

    pub fn push_used_caller_saved_regs_to_stack(&mut self, buf: &mut Vec<'a, u8>) {
        let old_general_used_regs = std::mem::replace(
            &mut self.general_used_regs,
            bumpalo::vec![in self.env.arena],
        );
        for (reg, saved_sym) in old_general_used_regs.into_iter() {
            if CC::general_caller_saved(&reg) {
                self.general_free_regs.push(reg);
                self.free_to_stack(buf, &saved_sym, General(reg));
            } else {
                self.general_used_regs.push((reg, saved_sym));
            }
        }
        let old_float_used_regs =
            std::mem::replace(&mut self.float_used_regs, bumpalo::vec![in self.env.arena]);
        for (reg, saved_sym) in old_float_used_regs.into_iter() {
            if CC::float_caller_saved(&reg) {
                self.float_free_regs.push(reg);
                self.free_to_stack(buf, &saved_sym, Float(reg));
            } else {
                self.float_used_regs.push((reg, saved_sym));
            }
        }
    }

    /// Gets a value from storage. They index symbol must be defined.
    fn get_storage_for_sym(&self, sym: &Symbol) -> &Storage<GeneralReg, FloatReg> {
        if let Some(storage) = self.symbol_storage_map.get(sym) {
            storage
        } else {
            internal_error!("Unknown symbol: {}", sym);
        }
    }

    /// Removes and returns a value from storage. They index symbol must be defined.
    fn remove_storage_for_sym(&mut self, sym: &Symbol) -> Storage<GeneralReg, FloatReg> {
        if let Some(storage) = self.symbol_storage_map.remove(sym) {
            storage
        } else {
            internal_error!("Unknown symbol: {}", sym);
        }
    }
}

fn is_primitive(layout: &Layout<'_>) -> bool {
    matches!(layout, single_register_layouts!())
}
